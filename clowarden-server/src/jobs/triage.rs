//! Asynchronous webhook triage worker with per-installation fairness and
//! bounded cross-organization concurrency.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::Duration,
};

use anyhow::{Error, Result};
use octorust::types::{ChecksCreateRequestConclusion, JobStatus};
use tokio::{
    sync::mpsc,
    task::{Id as TaskId, JoinError, JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, instrument, warn};

use clowarden_core::{
    cfg::Organization,
    github::{is_cancelled_error, is_rate_limit_error},
};

use crate::github::{self, AssociatedPullRequest, Ctx, DynGH, PullRequestEventAction};

use super::{
    CheckRunRerequestedInput, Job, JobsContext, MAX_VALIDATE_RATE_LIMIT_RETRIES, PullRequestTriageInput,
    TriageInput, WEBHOOK_TRIAGE_QUEUE_CAPACITY, bounded_rate_limit_delay,
    checks::{CheckRunPublisher, CheckRunTarget},
    deadline_after, rate_limit_retry_after, sleep_until_or_cancel,
    worker::INTERRUPTED_VALIDATION_MSG,
};

/// Message used when a pull request does not update configuration files.
pub(super) const NO_CONFIG_CHANGES_MSG: &str = "No CLOWarden configuration changes detected";

/// Maximum concurrent webhook triage tasks across organizations.
const TRIAGE_MAX_CONCURRENCY: usize = 4;

/// Shared runtime dependencies used by asynchronous webhook triage tasks.
#[derive(Clone)]
pub(crate) struct TriageContext {
    /// Cancellation token interrupting in-flight triage tasks when drain times out.
    pub(super) cancel_jobs: CancellationToken,
    /// Shared check-run publisher and cancellation/retry handles.
    pub(super) checks: CheckRunPublisher,
    /// Sender for enqueueing PR Validate and Reconcile jobs.
    pub(super) pr_jobs_tx: mpsc::UnboundedSender<Job>,
}

impl TriageContext {
    /// Process a `check_run.rerequested` or `check_suite.rerequested` triage
    /// item by resolving matching open pull requests for `head_sha` (supporting
    /// both same-repo and fork PRs) and running standard pull request
    /// validation triage. Rate-limited lookups are deferred into
    /// `deferred_retries` so only unresolved lookups are retried without
    /// occupying the installation's active triage slot.
    #[instrument(fields(org = input.org.name, head_sha = input.head_sha), skip_all)]
    pub(super) async fn handle_check_run_rerequested(&self, input: CheckRunRerequestedInput) {
        // Resolve and triage matching PRs immediately when quota allows
        let Some((initial_unresolved, initial_delay)) =
            self.resolve_and_triage_check_run_rerequested(&input).await
        else {
            return;
        };

        // Skip scheduling deferred retries once intake is closing
        if self.checks.stop_intake.is_cancelled() {
            return;
        }

        // Defer only the unresolved rate-limited lookup(s) without holding a triage slot
        let triage_ctx = self.clone();
        let mut retry_input = CheckRunRerequestedInput {
            pr_numbers: initial_unresolved,
            ..input
        };
        let handle = tokio::spawn(async move {
            let mut delay = initial_delay;
            for attempt in 1..=MAX_VALIDATE_RATE_LIMIT_RETRIES {
                warn!(
                    org = %retry_input.org.name,
                    head_sha = %retry_input.head_sha,
                    retry_attempt = attempt,
                    ?delay,
                    "deferring rate-limited check rerun PR lookup"
                );
                if sleep_until_or_cancel(deadline_after(delay), &triage_ctx.checks.stop_intake).await {
                    return;
                }
                let Some((unresolved, next_delay)) =
                    triage_ctx.resolve_and_triage_check_run_rerequested(&retry_input).await
                else {
                    return;
                };
                retry_input.pr_numbers = unresolved;
                delay = next_delay;
            }
        });
        self.checks.deferred_retries.push(handle);
    }

    /// Process a single pull request triage item: inspect changed files, create
    /// initial check runs, and enqueue Validate or Reconcile jobs as needed.
    #[instrument(fields(org = input.org.name, pr_number = input.pr_number), skip_all)]
    pub(super) async fn handle_pr_triage(&self, input: PullRequestTriageInput) {
        let is_validation_event = input.action.is_validation_action();

        // Skip stale validation events already superseded by a newer commit
        if is_validation_event
            && self
                .checks
                .latest_pr_heads
                .is_superseded(&input.org.name, input.pr_number, &input.pr_head_sha)
        {
            debug!(head_sha = %input.pr_head_sha, "skipping superseded pull request triage");
            return;
        }

        // Check if the PR updates any managed configuration file
        let ctx = Ctx::from_org_with_cancel(&input.org, self.cancel_jobs.clone());
        let status = match pr_updates_config(&self.checks.gh, &ctx, &input.org, input.pr_number).await {
            Ok(status) => status,
            Err(err) => {
                self.handle_pr_triage_config_error(input, is_validation_event, &err);
                return;
            }
        };

        // Re-check supersession after the live PR file list call so we never
        // stamp a stale commit with a green no-config-changes verdict from a newer head
        if is_validation_event
            && self
                .checks
                .latest_pr_heads
                .is_superseded(&input.org.name, input.pr_number, &input.pr_head_sha)
        {
            debug!(head_sha = %input.pr_head_sha, "skipping check run for superseded pull request commit");
            return;
        }

        // Dispatch check run updates and jobs based on whether config changed
        match status {
            PullRequestConfigChanges::Changed | PullRequestConfigChanges::Truncated => {
                self.handle_pr_triage_changed(&ctx, input, status).await;
            }
            PullRequestConfigChanges::Unchanged => {
                if is_validation_event {
                    self.handle_pr_triage_unchanged(input).await;
                }
            }
        }
    }

    /// Handle a pull request that modifies (or may modify) managed config files.
    async fn handle_pr_triage_changed(
        &self,
        ctx: &Ctx,
        input: PullRequestTriageInput,
        status: PullRequestConfigChanges,
    ) {
        if status == PullRequestConfigChanges::Truncated {
            warn!("pr file list hit github truncation cap; treating as potential config change");
        }

        match input.action {
            PullRequestEventAction::Edited
            | PullRequestEventAction::Opened
            | PullRequestEventAction::Reopened
            | PullRequestEventAction::Synchronize => {
                // Avoid starting a new in-progress check once shutdown drain has expired
                if self.cancel_jobs.is_cancelled() {
                    return;
                }

                // Create validation in-progress check run
                let attempt_generation = self.checks.latest_pr_heads.current_generation(
                    &input.org.name,
                    input.pr_number,
                    &input.pr_head_sha,
                );
                let check_body = github::new_checks_create_request(
                    input.pr_head_sha.clone(),
                    Some(JobStatus::InProgress),
                    None,
                    "Validating configuration changes",
                );
                let in_progress_created = match self.checks.gh.create_check_run(ctx, &check_body).await {
                    Ok(()) => {
                        debug!(
                            latency_ms = input.received_at.elapsed().as_millis(),
                            "created in-progress validation check run"
                        );
                        true
                    }
                    Err(err) if is_cancelled_error(&err) => return,
                    Err(err) => {
                        error!(?err, "error creating validation in-progress check run");
                        false
                    }
                };

                // If cancel_jobs fired while the in-progress POST was in flight,
                // or if the worker queue is already closed, fail-closed the check
                // immediately using transport_cancel instead of leaving it stuck
                let failed_handoff = if self.cancel_jobs.is_cancelled() {
                    Some((input.org, input.pr_number, input.pr_head_sha))
                } else if let Err(mpsc::error::SendError(Job::Validate(validate_input))) =
                    self.pr_jobs_tx.send(Job::Validate(input.into_validate_input()))
                {
                    Some((
                        validate_input.org,
                        validate_input.pr_number,
                        validate_input.pr_head_sha,
                    ))
                } else {
                    None
                };
                if let Some((org, pr_number, head_sha)) = failed_handoff
                    && in_progress_created
                {
                    let target = CheckRunTarget {
                        head_sha: &head_sha,
                        org: &org,
                        pr_number,
                        attempt_generation,
                    };
                    self.checks.complete_interrupted(&target, INTERRUPTED_VALIDATION_MSG).await;
                }
            }
            PullRequestEventAction::Closed => {
                if self.cancel_jobs.is_cancelled() {
                    debug!("skipping merged pr reconcile enqueue due to shutdown cancellation");
                    return;
                }
                // Enqueue reconcile job (unmerged closed PRs are dropped in handler)
                _ = self.pr_jobs_tx.send(Job::Reconcile(input.into_reconcile_input()));
            }
            PullRequestEventAction::Other => {}
        }
    }

    /// Handle a failure inspecting pull request files during triage, falling
    /// back defensively for merged PRs and rate-limited validation events.
    fn handle_pr_triage_config_error(
        &self,
        input: PullRequestTriageInput,
        is_validation_event: bool,
        err: &Error,
    ) {
        if is_cancelled_error(err) || self.cancel_jobs.is_cancelled() {
            debug!(?err, "stopping pull request triage due to cancellation");
            return;
        }
        if input.action == PullRequestEventAction::Closed {
            // Never drop a merged PR's reconciliation due to a transient
            // file-list error; a no-op reconcile is safe
            warn!(
                ?err,
                "error checking if merged pr updates config; enqueueing reconcile defensively"
            );
            _ = self.pr_jobs_tx.send(Job::Reconcile(input.into_reconcile_input()));
        } else if is_validation_event && is_rate_limit_error(err) {
            // Rate-limited validation-event triage falls back to SHA-pinned
            // validation so its deferred retry logic recovers after reset
            if !self.checks.latest_pr_heads.is_superseded(
                &input.org.name,
                input.pr_number,
                &input.pr_head_sha,
            ) {
                warn!(
                    ?err,
                    "rate-limited checking pr files; enqueueing validation defensively"
                );
                _ = self.pr_jobs_tx.send(Job::Validate(input.into_validate_input()));
            }
        } else {
            error!(?err, "error checking if pr updates config");
        }
    }

    /// Publish the passing check run for a validation-triggering PR event that
    /// does not update any managed configuration files, because branch
    /// protection may require a check.
    async fn handle_pr_triage_unchanged(&self, input: PullRequestTriageInput) {
        // Prepare the passing check run bound to the current attempt generation
        let attempt_generation = self.checks.latest_pr_heads.current_generation(
            &input.org.name,
            input.pr_number,
            &input.pr_head_sha,
        );
        let check_body = github::new_checks_create_request(
            input.pr_head_sha.clone(),
            Some(JobStatus::Completed),
            Some(ChecksCreateRequestConclusion::Success),
            NO_CONFIG_CHANGES_MSG,
        );
        let target = CheckRunTarget {
            head_sha: &input.pr_head_sha,
            org: &input.org,
            pr_number: input.pr_number,
            attempt_generation,
        };

        // Publish the passing check, deferring publication on rate limits
        if let Err(err) = self.checks.publish_with_retry(&target, check_body).await {
            error!(
                ?err,
                org = %input.org.name,
                repo = %input.org.repository,
                head_sha = %input.pr_head_sha,
                pr_number = input.pr_number,
                "error creating no configuration changes check run"
            );
        } else {
            debug!(
                latency_ms = input.received_at.elapsed().as_millis(),
                "created no-config-changes check run"
            );
        }
    }

    /// Resolve associated PRs for a re-requested check run/suite and triage all
    /// resolved open matches. Returns `Some((unresolved_pr_numbers, delay))`
    /// when one or more lookups hit a rate limit and should be retried later.
    async fn resolve_and_triage_check_run_rerequested(
        &self,
        input: &CheckRunRerequestedInput,
    ) -> Option<(Vec<i64>, Duration)> {
        let ctx = Ctx::from_org_with_cancel(&input.org, self.cancel_jobs.clone());

        // Fetch authoritative PR metadata (embedded numbers or fork-PR commit lookup)
        let resolved = self.resolve_associated_prs(&ctx, input).await?;

        // Re-triage resolved open PRs whose current head matches the check run SHA
        for pr in resolved.associated_prs {
            if self.cancel_jobs.is_cancelled() {
                debug!("stopping check rerun triage due to cancellation");
                return None;
            }
            if !pr.is_open || pr.base_ref != input.org.branch || pr.head_sha != input.head_sha {
                continue;
            }
            self.checks.latest_pr_heads.record(
                &input.org.name,
                pr.number,
                pr.head_sha.clone(),
                pr.updated_at,
                None,
            );
            let pr_input =
                PullRequestTriageInput::from_associated_pr(input.org.clone(), pr, input.received_at);
            self.handle_pr_triage(pr_input).await;
        }

        // Return any rate-limited lookups along with a bounded retry delay
        resolved.deferred_pr_numbers.map(|unresolved| {
            let delay = bounded_rate_limit_delay(resolved.deferred_delay);
            (unresolved, delay)
        })
    }

    /// Fetch authoritative pull request metadata for a re-requested check run,
    /// returning `None` when a non-rate-limit commit lookup fails or shutdown
    /// cancellation interrupts the lookup.
    async fn resolve_associated_prs(
        &self,
        ctx: &Ctx,
        input: &CheckRunRerequestedInput,
    ) -> Option<ResolvedCheckRunPrs> {
        let mut associated_prs = Vec::new();
        let mut deferred_pr_numbers: Option<Vec<i64>> = None;
        let mut deferred_delay: Option<Duration> = None;

        if input.pr_numbers.is_empty() {
            // Fall back to commit->PR lookup for fork PRs where GitHub omits
            // pull request numbers from check_run/check_suite payloads
            match self.checks.gh.list_commit_prs(ctx, &input.head_sha).await {
                Ok(prs) => associated_prs = prs,
                Err(err) if is_cancelled_error(&err) => {
                    debug!(?err, "stopping check rerun commit lookup due to cancellation");
                    return None;
                }
                Err(err) if is_rate_limit_error(&err) => {
                    deferred_pr_numbers = Some(vec![]);
                    deferred_delay = rate_limit_retry_after(&err);
                }
                Err(err) => {
                    error!(?err, "error listing pull requests for re-requested check run");
                    return None;
                }
            }
        } else {
            // Fetch each embedded pull request directly, tracking rate-limited
            // numbers so only unresolved lookups are deferred
            let mut unresolved = Vec::new();
            for pr_number in &input.pr_numbers {
                match self.checks.gh.get_pr(ctx, *pr_number).await {
                    Ok(pr) => associated_prs.push(pr),
                    Err(err) if is_cancelled_error(&err) => {
                        debug!(
                            ?err,
                            pr_number, "stopping check rerun pr lookup due to cancellation"
                        );
                        return None;
                    }
                    Err(err) if is_rate_limit_error(&err) => {
                        unresolved.push(*pr_number);
                        let delay = rate_limit_retry_after(&err);
                        deferred_delay = match (deferred_delay, delay) {
                            (Some(a), Some(b)) => Some(a.max(b)),
                            (Some(a), None) => Some(a),
                            (None, Some(b)) => Some(b),
                            (None, None) => None,
                        };
                    }
                    Err(err) => {
                        error!(
                            ?err,
                            pr_number, "error fetching pull request for re-requested check run"
                        );
                    }
                }
            }
            if !unresolved.is_empty() {
                deferred_pr_numbers = Some(unresolved);
            }
        }

        Some(ResolvedCheckRunPrs {
            associated_prs,
            deferred_delay,
            deferred_pr_numbers,
        })
    }
}

impl From<&JobsContext> for TriageContext {
    fn from(ctx: &JobsContext) -> Self {
        Self {
            cancel_jobs: ctx.cancel_jobs.clone(),
            checks: CheckRunPublisher::from(ctx),
            pr_jobs_tx: ctx.pr_jobs_tx.clone(),
        }
    }
}

/// Asynchronous webhook triage worker with per-installation fairness and
/// bounded cross-org concurrency so one rate-limited installation can occupy
/// at most one triage slot and never stalls triage for healthy installations,
/// while bounding total buffered items to preserve webhook 503 backpressure.
pub(crate) fn triage_worker(
    ctx: TriageContext,
    mut triage_rx: mpsc::Receiver<TriageInput>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut queue = TriageQueue::new();
        let mut cancel_jobs_handled = false;

        loop {
            // Fill available cross-org concurrency slots from ready installations
            queue.dispatch_ready(&ctx);

            if queue.is_drained() {
                break;
            }

            // Stop pulling from triage_rx once buffered + active work reaches
            // WEBHOOK_TRIAGE_QUEUE_CAPACITY so HTTP handlers apply 503 backpressure
            let can_accept_more = queue.can_accept_more();
            let has_in_flight = queue.has_in_flight();
            let intake_open = !queue.is_intake_closed();

            tokio::select! {
                biased;

                // Abort remaining triage tasks immediately on final transport cancel
                () = ctx.checks.transport_cancel.cancelled() => {
                    queue.abort_all();
                    break;
                }

                // When drain times out, drop unstarted items and let in-flight
                // triage tasks unwind cooperatively during the cleanup grace window
                () = ctx.cancel_jobs.cancelled(), if !cancel_jobs_handled => {
                    queue.cancel_pending();
                    cancel_jobs_handled = true;
                }

                // Release installation slot on both normal completion and panic
                Some(res) = queue.join_next(), if has_in_flight => {
                    queue.complete_task(res);
                }

                // Pull next delivery when buffer capacity allows
                maybe_input = triage_rx.recv(), if can_accept_more => {
                    match maybe_input {
                        Some(input) => queue.enqueue(input),
                        None => queue.close_intake(),
                    }
                }

                // On shutdown, drain any items already buffered in triage_rx
                // before closing intake so acknowledged webhooks are processed
                () = ctx.checks.stop_intake.cancelled(), if intake_open => {
                    queue.close_intake_and_drain(&mut triage_rx);
                }
            }
        }
    })
}

/// Configuration change status for a pull request evaluated during triage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PullRequestConfigChanges {
    /// The pull request updates CLOWarden configuration files.
    Changed,
    /// The pull request file list hit GitHub's 3,000-file truncation cap
    /// without matching a config file, so changes cannot be ruled out.
    Truncated,
    /// The pull request does not update configuration files.
    Unchanged,
}

/// Pull request lookup result for a re-requested check run, including any
/// rate-limited lookups that must be retried later.
struct ResolvedCheckRunPrs {
    /// Pull requests resolved during this attempt.
    associated_prs: Vec<AssociatedPullRequest>,

    /// Suggested retry delay for rate-limited lookups, when present.
    deferred_delay: Option<Duration>,
    /// Unresolved PR numbers (or empty vec for commit lookup) to retry later.
    deferred_pr_numbers: Option<Vec<i64>>,
}

/// Per-installation fair queue and bounded concurrency tracker for webhook
/// triage tasks.
struct TriageQueue {
    /// Installations that currently have an active triage task running.
    active_installations: HashSet<i64>,
    /// Active triage tasks.
    in_flight: JoinSet<()>,
    /// Whether upstream webhook intake has been closed.
    intake_closed: bool,
    /// Per-installation FIFO queues of buffered triage items.
    pending: HashMap<i64, VecDeque<TriageInput>>,
    /// Total number of items currently buffered across `pending`.
    pending_count: usize,
    /// Round-robin queue of installations with buffered items and no active task.
    ready: VecDeque<i64>,
    /// Mapping from spawned task id back to its installation id.
    task_installations: HashMap<TaskId, i64>,
}

impl TriageQueue {
    /// Create an empty triage queue.
    fn new() -> Self {
        Self {
            active_installations: HashSet::new(),
            in_flight: JoinSet::new(),
            intake_closed: false,
            pending: HashMap::new(),
            pending_count: 0,
            ready: VecDeque::new(),
            task_installations: HashMap::new(),
        }
    }

    /// Abort all currently running triage tasks.
    fn abort_all(&mut self) {
        self.in_flight.abort_all();
    }

    /// Return true when intake is still open and total buffered plus active
    /// work is below [`WEBHOOK_TRIAGE_QUEUE_CAPACITY`].
    fn can_accept_more(&self) -> bool {
        !self.intake_closed && self.pending_count + self.in_flight.len() < WEBHOOK_TRIAGE_QUEUE_CAPACITY
    }

    /// Close intake and drop unstarted buffered items while allowing already
    /// running triage tasks to finish unwinding cooperatively.
    fn cancel_pending(&mut self) {
        self.intake_closed = true;
        self.pending.clear();
        self.ready.clear();
        self.pending_count = 0;
    }

    /// Mark upstream webhook intake as closed.
    fn close_intake(&mut self) {
        self.intake_closed = true;
    }

    /// Drain any deliveries already buffered in `triage_rx` and close intake.
    fn close_intake_and_drain(&mut self, triage_rx: &mut mpsc::Receiver<TriageInput>) {
        while let Ok(input) = triage_rx.try_recv() {
            self.enqueue(input);
        }
        self.intake_closed = true;
    }

    /// Release an installation slot when its triage task finishes or panics,
    /// re-queuing the installation if more deliveries are pending for it.
    fn complete_task(&mut self, res: Result<(TaskId, ()), JoinError>) {
        let task_id = match res {
            Ok((id, ())) => id,
            Err(err) => {
                error!(?err, "webhook triage task failed");
                err.id()
            }
        };
        if let Some(inst_id) = self.task_installations.remove(&task_id) {
            self.active_installations.remove(&inst_id);
            if self.pending.contains_key(&inst_id) {
                self.ready.push_back(inst_id);
            }
        }
    }

    /// Dispatch up to [`TRIAGE_MAX_CONCURRENCY`] installations with at most one
    /// concurrent triage task per installation.
    fn dispatch_ready(&mut self, ctx: &TriageContext) {
        while self.in_flight.len() < TRIAGE_MAX_CONCURRENCY {
            // Pop the next ready installation and its oldest buffered delivery
            let Some(inst_id) = self.ready.pop_front() else {
                break;
            };
            let Some(item) = self.pending.get_mut(&inst_id).and_then(VecDeque::pop_front) else {
                continue;
            };
            self.pending_count = self.pending_count.saturating_sub(1);
            if self.pending.get(&inst_id).is_some_and(VecDeque::is_empty) {
                self.pending.remove(&inst_id);
            }

            // Mark the installation active and spawn its triage task
            self.active_installations.insert(inst_id);
            let task_ctx = ctx.clone();
            let handle = self.in_flight.spawn(async move {
                match item {
                    TriageInput::CheckRunRerequested(input) => {
                        task_ctx.handle_check_run_rerequested(input).await;
                    }
                    TriageInput::PullRequest(input) => {
                        task_ctx.handle_pr_triage(input).await;
                    }
                }
            });
            self.task_installations.insert(handle.id(), inst_id);
        }
    }

    /// Enqueue an item into its installation's FIFO queue and mark the
    /// installation ready when it has no task currently running.
    fn enqueue(&mut self, item: TriageInput) {
        let inst_id = item.installation_id();
        let queue = self.pending.entry(inst_id).or_default();
        let was_empty = queue.is_empty();
        queue.push_back(item);
        self.pending_count += 1;
        if was_empty && !self.active_installations.contains(&inst_id) {
            self.ready.push_back(inst_id);
        }
    }

    /// Return true when at least one triage task is currently running.
    fn has_in_flight(&self) -> bool {
        !self.in_flight.is_empty()
    }

    /// Return true once intake is closed and no buffered or in-flight work remains.
    fn is_drained(&self) -> bool {
        self.intake_closed && self.pending.is_empty() && self.in_flight.is_empty()
    }

    /// Return true once upstream webhook intake has been closed.
    fn is_intake_closed(&self) -> bool {
        self.intake_closed
    }

    /// Await the next completed in-flight triage task and return its id and result.
    async fn join_next(&mut self) -> Option<Result<(TaskId, ()), JoinError>> {
        self.in_flight.join_next_with_id().await
    }
}

// Helpers.

/// Check whether the pull request modifies any managed configuration files.
async fn pr_updates_config(
    gh: &DynGH,
    ctx: &Ctx,
    org: &Organization,
    pr_number: i64,
) -> Result<PullRequestConfigChanges> {
    if org.legacy.enabled {
        let mut legacy_cfg_files = vec![&org.legacy.sheriff_permissions_path];
        if let Some(cncf_people_path) = &org.legacy.cncf_people_path {
            legacy_cfg_files.push(cncf_people_path);
        }
        let pr_files = gh.list_pr_files(ctx, pr_number).await?;
        for filename in &pr_files.files {
            if legacy_cfg_files.contains(&filename) {
                return Ok(PullRequestConfigChanges::Changed);
            }
        }
        if pr_files.is_truncated {
            return Ok(PullRequestConfigChanges::Truncated);
        }
    }

    Ok(PullRequestConfigChanges::Unchanged)
}
