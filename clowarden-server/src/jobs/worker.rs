//! Organization job workers and PR/periodic job router.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use ::time::OffsetDateTime;
use anyhow::{Error, Result};
use askama::Template;
use octorust::types::{ChecksCreateRequestConclusion, JobStatus};
use tokio::{
    sync::mpsc,
    task::JoinHandle,
    time::{Instant, sleep, timeout},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, instrument, warn};

use clowarden_core::{
    cfg::Organization,
    directory::Directory,
    github::{Source, is_cancelled_error, is_rate_limit_error},
    multierror::MultiError,
    services::{BaseRefConfigStatus, ChangesApplied, ChangesSummary, JobContext, ServiceName},
};

use crate::{
    github::{self, Ctx},
    tmpl,
};

use super::{
    Job, JobsContext, MAX_VALIDATE_RATE_LIMIT_RETRIES, PeriodicJobOutcome, QueuedJob, ReconcileInput,
    ValidateInput, bounded_rate_limit_delay,
    checks::{CheckRunPublisher, CheckRunTarget},
    deadline_after, fresh_clean_remaining, rate_limit_retry_after, sleep_until_or_cancel,
};

/// Timeout for persisting a reconciliation record to PostgreSQL.
const DB_REGISTER_TIMEOUT: Duration = Duration::from_secs(30);

/// Message used when PR validation is interrupted before completion (for
/// example during server shutdown).
pub(super) const INTERRUPTED_VALIDATION_MSG: &str =
    "Validation was interrupted before completion; please re-run";

/// Additional grace period after the cooperative deadline before forcefully
/// aborting a wedged job future (in seconds).
pub(super) const JOB_HARD_TIMEOUT_GRACE: u64 = 60;

/// Maximum time allowed for handling a single job (in seconds).
pub(super) const JOB_TIMEOUT: u64 = 60 * 60; // 60 minutes

/// Message used when PR validation cannot complete because GitHub rate limits
/// persisted past retry limits.
pub(super) const RATE_LIMITED_VALIDATION_MSG: &str =
    "Validation could not complete due to GitHub API rate limits; please re-run";

/// A jobs handler is in charge of executing the received jobs. It creates one
/// worker per organization (ensuring per-org serialization while prioritizing
/// PR-triggered jobs ahead of periodic sweeps) plus a router task.
pub(crate) fn handler(
    ctx: &JobsContext,
    mut pr_jobs_rx: mpsc::UnboundedReceiver<Job>,
    mut periodic_jobs_rx: mpsc::Receiver<QueuedJob>,
    orgs: &[Organization],
) -> Vec<JoinHandle<()>> {
    let mut handles = Vec::with_capacity(orgs.len() + 1);
    let mut org_pr_channels = HashMap::new();
    let mut org_periodic_channels = HashMap::new();
    let mut org_activity = HashMap::new();
    let router_done = CancellationToken::new();

    // Create a worker for each organization
    for org in orgs {
        let (pr_tx, pr_rx) = mpsc::unbounded_channel();
        let (periodic_tx, periodic_rx) = mpsc::channel(1);
        let activity = OrgWorkerActivity::new();
        org_pr_channels.insert(org.name.clone(), pr_tx);
        org_periodic_channels.insert(org.name.clone(), periodic_tx);
        org_activity.insert(org.name.clone(), activity.clone());
        let org_worker = OrgWorker::new(ctx.clone());
        handles.push(org_worker.run(pr_rx, periodic_rx, router_done.clone(), activity));
    }

    // Create a router dispatching PR and periodic jobs to the matching org worker
    let stop_intake = ctx.stop_intake.clone();
    let stop_workers = ctx.stop_workers.clone();
    let jobs_router = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;

                // Prioritize interactive PR jobs over periodic sweeps
                Some(job) = pr_jobs_rx.recv() => {
                    let org_name = job.org_name().to_string();
                    if let Some(org_pr_tx) = org_pr_channels.get(&org_name) {
                        if let Some(activity) = org_activity.get(&org_name) {
                            activity.pending_pr_jobs.fetch_add(1, Ordering::AcqRel);
                        }
                        if org_pr_tx.send(job).is_err()
                            && let Some(activity) = org_activity.get(&org_name)
                        {
                            activity.pending_pr_jobs.fetch_sub(1, Ordering::AcqRel);
                        }
                    }
                }

                // Exit once upstream intake/triage finished and PR queue is empty
                () = stop_workers.cancelled() => break,

                // Route sequential periodic jobs only when target org worker is idle
                Some(queued) = periodic_jobs_rx.recv(), if !stop_intake.is_cancelled() => {
                    let org_name = queued.job.org_name();
                    if org_activity.get(org_name).is_some_and(OrgWorkerActivity::is_busy) {
                        if let Some(done) = queued.done {
                            let _ = done.send(PeriodicJobOutcome::DeferredBusy);
                        }
                        continue;
                    }
                    if let Some(org_periodic_tx) = org_periodic_channels.get(org_name) {
                        match org_periodic_tx.try_send(queued) {
                            Err(mpsc::error::TrySendError::Full(queued)) => {
                                if let Some(done) = queued.done {
                                    let _ = done.send(PeriodicJobOutcome::DeferredBusy);
                                }
                            }
                            Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
                        }
                    }
                }
            }
        }

        // Notify organization workers that no further PR jobs will be routed
        router_done.cancel();
    });
    handles.push(jobs_router);

    handles
}

/// An organization worker is in charge of processing jobs for a given
/// organization.
pub(super) struct OrgWorker {
    /// Shared runtime dependencies for job execution.
    ctx: JobsContext,
}

impl OrgWorker {
    /// Create a new organization worker instance.
    pub(super) fn new(ctx: JobsContext) -> Self {
        Self { ctx }
    }

    /// Run organization worker, prioritizing PR-triggered jobs ahead of
    /// periodic sweeps.
    pub(super) fn run(
        self,
        mut pr_jobs_rx: mpsc::UnboundedReceiver<Job>,
        mut periodic_jobs_rx: mpsc::Receiver<QueuedJob>,
        stop_worker: CancellationToken,
        activity: OrgWorkerActivity,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;

                    // Always pick high-priority PR jobs first (and drain on shutdown)
                    job = pr_jobs_rx.recv() => {
                        match job {
                            Some(job) => {
                                // Immediately defer any periodic job admitted just
                                // before this PR arrived so the global sweeper does
                                // not wait behind PR execution
                                while let Ok(queued) = periodic_jobs_rx.try_recv() {
                                    if let Some(done) = queued.done {
                                        let _ = done.send(PeriodicJobOutcome::DeferredBusy);
                                    }
                                }
                                activity.is_running.store(true, Ordering::Release);
                                self.run_job(job, false).await;
                                activity.is_running.store(false, Ordering::Release);
                                let _ = activity.pending_pr_jobs.try_update(
                                    Ordering::AcqRel,
                                    Ordering::Acquire,
                                    |n| Some(n.saturating_sub(1)),
                                );
                            }
                            None => break,
                        }
                    }

                    // Exit once router has finished forwarding PR jobs and queue is empty
                    () = stop_worker.cancelled() => break,

                    // Pick periodic reconcile when idle and intake is still open
                    Some(queued) = periodic_jobs_rx.recv(), if !self.ctx.stop_intake.is_cancelled() => {
                        let outcome = if !pr_jobs_rx.is_empty() {
                            PeriodicJobOutcome::DeferredBusy
                        } else if self.is_recently_clean(queued.job.org_name()) {
                            debug!(
                                org = %queued.job.org_name(),
                                "skipping periodic reconcile; recently reconciled cleanly by PR job"
                            );
                            PeriodicJobOutcome::SkippedClean
                        } else {
                            activity.is_running.store(true, Ordering::Release);
                            let clean = self.run_job(queued.job, true).await;
                            activity.is_running.store(false, Ordering::Release);
                            if clean {
                                PeriodicJobOutcome::Clean
                            } else {
                                PeriodicJobOutcome::Failed
                            }
                        };
                        if let Some(done) = queued.done {
                            let _ = done.send(outcome);
                        }
                    }
                }
            }
        })
    }

    /// Reconcile job handler. Returns `true` when every service finished
    /// cleanly without top-level or per-change errors.
    #[instrument(fields(org = input.org.name), skip_all)]
    pub(super) async fn handle_reconcile_job(
        &self,
        input: ReconcileInput,
        job_cancel: CancellationToken,
    ) -> Result<bool> {
        let mut changes_applied: HashMap<ServiceName, ChangesApplied> = HashMap::new();
        let mut errors: HashMap<ServiceName, Error> = HashMap::new();
        let job_ctx = JobContext::new(self.ctx.ghc.clone(), job_cancel);

        // Reconcile services state, preserving any partial changes on error
        for (service_name, service_handler) in &self.ctx.services {
            debug!(service_name, "reconciling state");
            let outcome = service_handler.reconcile(&input.org, &job_ctx).await;
            if !outcome.changes_applied.is_empty() || outcome.error.is_none() {
                changes_applied.insert(service_name, outcome.changes_applied);
            }
            if let Some(err) = outcome.error {
                errors.insert(service_name, err);
            }
        }

        // Determine whether reconciliation converged cleanly and record outcome
        // (recording failed attempts ensures they invalidate earlier clean runs)
        let is_clean = errors.is_empty()
            && changes_applied.values().all(|entries| entries.iter().all(|entry| entry.error.is_none()));
        self.ctx
            .clean_tracker
            .record_outcome(&input.org.name, OffsetDateTime::now_utc(), is_clean);

        // Register audit records in PostgreSQL and post the PR comment
        // concurrently so a slow database write cannot starve the shutdown
        // cleanup grace window for posting partial reconciliation comments
        let register_fut = async {
            match timeout(
                DB_REGISTER_TIMEOUT,
                self.ctx.db.register_reconciliation(&input, &changes_applied, &errors),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(err)) => error!(?err, "error registering reconciliation in database"),
                Err(_) => error!("timed out registering reconciliation in database"),
            }
        };
        let comment_fut = async {
            let Some(pr_number) = input.pr_number else {
                return Ok(());
            };
            let ctx = Ctx::from_org_with_cancel(&input.org, self.ctx.transport_cancel.clone());
            let comment_body = tmpl::ReconciliationCompleted::new(&changes_applied, &errors).render()?;
            if let Err(err) = self.ctx.gh.post_comment(&ctx, pr_number, &comment_body).await {
                error!(?err, "error posting reconciliation comment");
            }
            Ok::<(), Error>(())
        };
        let ((), comment_result) = tokio::join!(register_fut, comment_fut);
        comment_result?;

        // Log top-level service reconciliation errors (individual changes are
        // logged incrementally as each service applies them)
        for (service_name, error) in &errors {
            error!(?error, service = service_name, "reconciliation failed");
        }

        Ok(is_clean)
    }

    /// Validate job handler.
    #[cfg(test)]
    pub(super) async fn handle_validate_job(
        &self,
        input: ValidateInput,
        job_cancel: CancellationToken,
    ) -> Result<()> {
        self.execute_validate_job(input, job_cancel).await.map_err(ValidateError::into_inner)
    }

    /// Execute a pull request validation job, classifying failures so normal
    /// configuration mistakes and already-logged interruptions are not logged
    /// as server `ERROR`s while genuine operational failures are logged once.
    #[instrument(fields(org = input.org.name, pr_number = input.pr_number), skip_all)]
    pub(super) async fn execute_validate_job(
        &self,
        input: ValidateInput,
        job_cancel: CancellationToken,
    ) -> std::result::Result<(), ValidateError> {
        let started_at = Instant::now();
        let mut merr = MultiError::new(None);
        let job_ctx = JobContext::new(self.ctx.ghc.clone(), job_cancel);
        let attempt_generation =
            self.ctx
                .latest_pr_heads
                .current_generation(&input.org.name, input.pr_number, &input.pr_head_sha);

        // Pin head reads to the immutable commit SHA matching the check run
        let head_src = Source {
            inst_id: Some(input.org.installation_id),
            owner: input.pr_head_owner.clone().unwrap_or(input.org.name.clone()),
            repo: input.pr_head_repo.clone().unwrap_or(input.org.repository.clone()),
            ref_: input.pr_head_sha.clone(),
        };

        // Directory configuration validation (deferring or failing closed on rate limits)
        let directory_changes =
            match Directory::get_changes_summary(job_ctx.gh.clone(), &input.org, &head_src).await {
                Ok(changes) => changes,
                Err(err) if is_rate_limit_error(&err) || is_cancelled_error(&err) => {
                    return Err(ValidateError::interrupted(
                        self.handle_interrupted_validate(input, attempt_generation, err).await,
                    ));
                }
                Err(err) => {
                    merr.push(err);
                    ChangesSummary {
                        changes: vec![],
                        base_ref_config_status: BaseRefConfigStatus::Unknown,
                        warnings: vec![],
                    }
                }
            };

        // Services configuration validation (deferring or failing closed on rate limits)
        let mut services_changes: HashMap<ServiceName, ChangesSummary> = HashMap::new();
        if !merr.contains_errors() {
            for (service_name, service_handler) in &self.ctx.services {
                match service_handler.get_changes_summary(&input.org, &head_src, &job_ctx).await {
                    Ok(changes) => {
                        services_changes.insert(service_name, changes);
                    }
                    Err(err) if is_rate_limit_error(&err) || is_cancelled_error(&err) => {
                        return Err(ValidateError::interrupted(
                            self.handle_interrupted_validate(input, attempt_generation, err).await,
                        ));
                    }
                    Err(err) => merr.push(err),
                }
            }
        }

        // Publish the PR comment and completion check run, then classify outcome
        self.publish_validation_verdict(
            &input,
            attempt_generation,
            started_at,
            &directory_changes,
            &services_changes,
            merr,
        )
        .await
    }

    /// Post the validation comment and check run for `input` and classify the
    /// final outcome for logging.
    async fn publish_validation_verdict(
        &self,
        input: &ValidateInput,
        attempt_generation: Option<u64>,
        started_at: Instant,
        directory_changes: &ChangesSummary,
        services_changes: &HashMap<ServiceName, ChangesSummary>,
        merr: MultiError,
    ) -> std::result::Result<(), ValidateError> {
        // Post validation completed comment
        let errors_found = merr.contains_errors();
        let err = Error::from(merr);
        let ctx = Ctx::from_org_with_cancel(&input.org, self.ctx.transport_cancel.clone());
        let comment_body = if errors_found {
            tmpl::ValidationFailed::new(&err).render()
        } else {
            tmpl::ValidationSucceeded::new(directory_changes, services_changes).render()
        }
        .map_err(|render_err| ValidateError::operational(render_err.into()))?;
        let comment_result = self.ctx.gh.post_comment(&ctx, input.pr_number, &comment_body).await;

        // Create check run, which must be completed even if the comment can't
        // be posted. Its conclusion only depends on the validation outcome
        let (conclusion, verdict) = if errors_found {
            (
                ChecksCreateRequestConclusion::Failure,
                "The configuration changes proposed are not valid",
            )
        } else {
            (
                ChecksCreateRequestConclusion::Success,
                "The configuration changes proposed are valid",
            )
        };
        let check_msg = if let Err(err) = comment_result {
            error!(?err, "error posting validation comment");
            format!("{verdict} (the validation comment could not be posted)")
        } else {
            verdict.to_string()
        };
        let check_body = github::new_checks_create_request(
            input.pr_head_sha.clone(),
            Some(JobStatus::Completed),
            Some(conclusion),
            &check_msg,
        );
        let target = CheckRunTarget {
            head_sha: &input.pr_head_sha,
            org: &input.org,
            pr_number: input.pr_number,
            attempt_generation,
        };
        let check_result = CheckRunPublisher::from(&self.ctx).publish_with_retry(&target, check_body).await;

        // Validation errors take precedence, as the comment may not have been
        // posted and returning them is the only way of not losing them
        let duration_ms = started_at.elapsed().as_millis();
        if errors_found {
            if let Err(err) = check_result {
                error!(?err, "error creating validation check run");
            }
            if has_operational_error(&err) {
                info!(valid = false, duration_ms, "finished pull request validation");
                return Err(ValidateError::operational(err));
            }
            info!(
                valid = false,
                duration_ms,
                ?err,
                "finished pull request validation"
            );
            return Err(ValidateError::config_invalid(err));
        }
        info!(valid = true, duration_ms, "finished pull request validation");
        check_result.map_err(ValidateError::operational)
    }

    /// Handle a validation interrupted by rate limiting or cancellation by
    /// scheduling a deferred retry when retries remain, or completing the check
    /// run as failed so unvalidated changes cannot satisfy required checks.
    async fn handle_interrupted_validate(
        &self,
        input: ValidateInput,
        attempt_generation: Option<u64>,
        err: Error,
    ) -> Error {
        let is_rate_limit = is_rate_limit_error(&err);
        let msg = if is_rate_limit {
            RATE_LIMITED_VALIDATION_MSG
        } else {
            INTERRUPTED_VALIDATION_MSG
        };

        // Defer and retry on rate limits when retries remain and server is active
        if is_rate_limit
            && input.retry_attempt < MAX_VALIDATE_RATE_LIMIT_RETRIES
            && !self.ctx.stop_intake.is_cancelled()
        {
            let delay = bounded_rate_limit_delay(rate_limit_retry_after(&err));
            warn!(
                org = %input.org.name,
                pr_number = input.pr_number,
                retry_attempt = input.retry_attempt + 1,
                ?delay,
                %err,
                "deferring rate-limited validation for retry"
            );
            self.spawn_deferred_validate_retry(input, attempt_generation, delay);
            return err;
        }

        // Complete the check run as failure (fail-closed) so unvalidated PRs
        // cannot satisfy required checks and can be re-run cleanly
        warn!(
            org = %input.org.name,
            pr_number = input.pr_number,
            %err,
            "completing interrupted validation check run as failure"
        );
        let target = CheckRunTarget {
            head_sha: &input.pr_head_sha,
            org: &input.org,
            pr_number: input.pr_number,
            attempt_generation,
        };
        CheckRunPublisher::from(&self.ctx).complete_interrupted(&target, msg).await;
        err
    }

    /// Return true if `org`'s latest reconciliation attempt was clean and
    /// completed within `reconcile_frequency`.
    fn is_recently_clean(&self, org: &str) -> bool {
        let Some(status) = self.ctx.clean_tracker.get(org) else {
            return false;
        };
        fresh_clean_remaining(&status, OffsetDateTime::now_utc(), self.ctx.reconcile_frequency).is_some()
    }

    /// Execute a single job under a cooperative deadline token so timeouts
    /// preserve partial audit records instead of dropping the future mid-flight,
    /// backed by a hard safety timeout if a dependency wedges.
    ///
    /// Returns `true` when a reconcile job converged cleanly (or a validate job
    /// succeeded).
    async fn run_job(&self, job: Job, is_periodic: bool) -> bool {
        // Periodic jobs stop cooperatively on stop_intake; PR jobs drain until
        // cancel_jobs fires while keeping transport_cancel open during the
        // cleanup grace window for fail-closed check runs and PR comments
        let base_cancel = if is_periodic {
            &self.ctx.stop_intake
        } else {
            &self.ctx.cancel_jobs
        };
        let job_cancel = base_cancel.child_token();
        let deadline_cancel = job_cancel.clone();
        let deadline_task = tokio::spawn(async move {
            sleep(Duration::from_secs(JOB_TIMEOUT)).await;
            deadline_cancel.cancel();
        });
        let hard_timeout = Duration::from_secs(JOB_TIMEOUT + JOB_HARD_TIMEOUT_GRACE);

        // Run the job bounded by the hard safety timeout
        let succeeded = match job {
            Job::Reconcile(input) => {
                match timeout(hard_timeout, self.handle_reconcile_job(input.clone(), job_cancel)).await {
                    Ok(Ok(is_clean)) => is_clean,
                    Ok(Err(err)) => {
                        error!(?err, ?input, "error handling reconcile job");
                        false
                    }
                    Err(_) => {
                        error!(?input, "reconcile job hit hard timeout");
                        // Invalidate any earlier clean run and wake the sweeper
                        self.ctx.clean_tracker.record_outcome(
                            &input.org.name,
                            OffsetDateTime::now_utc(),
                            false,
                        );
                        false
                    }
                }
            }
            Job::Validate(input) => {
                let attempt_generation = self.ctx.latest_pr_heads.current_generation(
                    &input.org.name,
                    input.pr_number,
                    &input.pr_head_sha,
                );
                match timeout(hard_timeout, self.execute_validate_job(input.clone(), job_cancel)).await {
                    Ok(Ok(())) => true,
                    Ok(Err(validate_err)) => {
                        if validate_err.kind == ValidateErrorKind::Operational {
                            error!(err = ?validate_err.error, ?input, "error handling validate job");
                        }
                        false
                    }
                    Err(_) => {
                        error!(?input, "validate job hit hard timeout");
                        // Fail-closed the in-progress check if this attempt is
                        // still current
                        if !self.ctx.latest_pr_heads.is_superseded_attempt(
                            &input.org.name,
                            input.pr_number,
                            &input.pr_head_sha,
                            attempt_generation,
                        ) {
                            let target = CheckRunTarget {
                                head_sha: &input.pr_head_sha,
                                org: &input.org,
                                pr_number: input.pr_number,
                                attempt_generation,
                            };
                            CheckRunPublisher::from(&self.ctx)
                                .complete_interrupted(&target, INTERRUPTED_VALIDATION_MSG)
                                .await;
                        }
                        false
                    }
                }
            }
        };

        // Clean up the cooperative deadline timer
        deadline_task.abort();
        succeeded
    }

    /// Spawn a background task to re-enqueue a rate-limited validation after
    /// `delay`, failing closed on shutdown unless superseded.
    fn spawn_deferred_validate_retry(
        &self,
        input: ValidateInput,
        attempt_generation: Option<u64>,
        delay: Duration,
    ) {
        // Capture shared handles and bump the retry attempt counter
        let publisher = CheckRunPublisher::from(&self.ctx);
        let pr_jobs_tx = self.ctx.pr_jobs_tx.clone();
        let retry_input = ValidateInput {
            retry_attempt: input.retry_attempt + 1,
            ..input
        };

        // Sleep through the cooldown, failing closed on shutdown or re-enqueueing
        // the validation if this attempt is still current
        let handle = tokio::spawn(async move {
            if sleep_until_or_cancel(deadline_after(delay), &publisher.stop_intake).await {
                // Fail-closed the in-progress check on shutdown unless a newer
                // commit or rerun already superseded this attempt
                if !publisher.latest_pr_heads.is_superseded_attempt(
                    &retry_input.org.name,
                    retry_input.pr_number,
                    &retry_input.pr_head_sha,
                    attempt_generation,
                ) {
                    let target = CheckRunTarget {
                        head_sha: &retry_input.pr_head_sha,
                        org: &retry_input.org,
                        pr_number: retry_input.pr_number,
                        attempt_generation,
                    };
                    publisher.complete_interrupted(&target, INTERRUPTED_VALIDATION_MSG).await;
                }
                return;
            }
            if publisher.latest_pr_heads.is_superseded_attempt(
                &retry_input.org.name,
                retry_input.pr_number,
                &retry_input.pr_head_sha,
                attempt_generation,
            ) {
                return;
            }
            _ = pr_jobs_tx.send(Job::Validate(retry_input));
        });
        self.ctx.deferred_retries.push(handle);
    }
}

/// Nonblocking activity state for an organization worker, allowing the router
/// to immediately defer periodic sweeps while the worker is executing or has
/// queued PR work.
#[derive(Clone, Default)]
pub(super) struct OrgWorkerActivity {
    /// Whether the organization worker is currently running a job.
    is_running: Arc<AtomicBool>,
    /// Number of PR jobs routed to this worker that have not yet finished.
    pending_pr_jobs: Arc<AtomicUsize>,
}

impl OrgWorkerActivity {
    /// Create a new idle worker activity tracker.
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Return true when the worker is executing any job or has queued PR work.
    fn is_busy(&self) -> bool {
        self.is_running.load(Ordering::Acquire) || self.pending_pr_jobs.load(Ordering::Acquire) > 0
    }
}

/// Classification of pull request validation failures so server `ERROR` logs
/// are reserved for genuine operational issues rather than normal PR config
/// mistakes or already-logged interruptions.
#[derive(Debug)]
pub(super) struct ValidateError {
    /// Underlying validation or operational error.
    pub(super) error: Error,
    /// Category controlling how `run_job` logs the failure.
    pub(super) kind: ValidateErrorKind,
}

impl ValidateError {
    /// Wrap an ordinary configuration validation error already logged at info.
    fn config_invalid(error: Error) -> Self {
        Self {
            error,
            kind: ValidateErrorKind::ConfigInvalid,
        }
    }

    /// Wrap an interruption error already logged at warn.
    fn interrupted(error: Error) -> Self {
        Self {
            error,
            kind: ValidateErrorKind::Interrupted,
        }
    }

    /// Extract the underlying error.
    #[cfg(test)]
    fn into_inner(self) -> Error {
        self.error
    }

    /// Wrap an operational or GitHub API failure that should be logged at error.
    fn operational(error: Error) -> Self {
        Self {
            error,
            kind: ValidateErrorKind::Operational,
        }
    }
}

/// Category of a [`ValidateError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ValidateErrorKind {
    /// Ordinary configuration validation failure.
    ConfigInvalid,
    /// Rate-limit or cancellation interruption.
    Interrupted,
    /// Operational or GitHub API failure.
    Operational,
}

/// Return true if `err` (including nested [`MultiError`] chains) contains an
/// underlying GitHub API/transport client error rather than a pure config error.
fn has_operational_error(err: &Error) -> bool {
    err.chain().any(|cause| {
        cause.is::<octorust::ClientError>()
            || cause
                .downcast_ref::<MultiError>()
                .is_some_and(|merr| merr.errors().iter().any(|inner| has_operational_error(inner)))
    })
}
