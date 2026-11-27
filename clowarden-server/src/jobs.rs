//! This module defines the types and functionality needed to schedule and
//! process jobs.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};

use ::time::OffsetDateTime;
use anyhow::Error;
use octorust::types::PullRequestData;
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{Notify, mpsc, oneshot},
    task::JoinHandle,
    time::{Instant, sleep_until},
};
use tokio_util::sync::CancellationToken;
use tracing::error;

use clowarden_core::{
    self as core,
    cfg::Organization,
    github::RateLimitError,
    multierror::MultiError,
    services::{DynServiceHandler, ServiceName},
};

use crate::{
    db::{DynDB, OrgReconciliationStatus},
    github::{AssociatedPullRequest, DynGH, PullRequestEventAction},
};

mod checks;
mod scheduler;
#[cfg(test)]
mod tests;
mod triage;
mod worker;

pub(crate) use scheduler::scheduler;
pub(crate) use triage::{TriageContext, triage_worker};
pub(crate) use worker::handler;

/// Default delay when deferring a rate-limited retry whose error did not
/// include an explicit `Retry-After` or reset duration.
const DEFAULT_VALIDATE_DEFER_DELAY: Duration = Duration::from_secs(60);

/// Maximum number of previous head SHAs remembered per PR to reject multi-hop
/// out-of-order redeliveries within the same timestamp second.
const MAX_SEEN_HEAD_SHAS: usize = 16;

/// Maximum delay when deferring a rate-limited PR validation retry (covers a
/// full 60-minute GitHub primary rate-limit reset window plus buffer).
const MAX_VALIDATE_DEFER_DELAY: Duration = Duration::from_mins(65);

/// Maximum number of deferred retries for a PR validation that hits rate limits.
const MAX_VALIDATE_RATE_LIMIT_RETRIES: u32 = 2;

/// Capacity of the bounded channel decoupling webhook HTTP requests from
/// GitHub API triage calls.
pub(crate) const WEBHOOK_TRIAGE_QUEUE_CAPACITY: usize = 256;

/// Compact `check_run.rerequested` webhook payload queued for asynchronous
/// triage so the HTTP handler responds immediately.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CheckRunRerequestedInput {
    /// Head commit SHA targeted by the re-requested check run.
    pub head_sha: String,
    /// Target organization configuration matched by the webhook.
    pub org: Organization,
    /// Pull request numbers embedded in the `check_run` payload, if any.
    pub pr_numbers: Vec<i64>,
    /// Monotonic timestamp when the webhook arrived (for latency logging).
    pub received_at: Instant,
}

/// Shared in-memory record of the most recent reconciliation attempt and clean
/// completion timestamps per organization. Recording failed attempts as well
/// ensures a newer failure invalidates an earlier clean run instead of being
/// hidden behind `reconcile_frequency`, and notifies the periodic sweeper so
/// failed PR reconciliations are retried promptly.
#[derive(Clone, Default)]
pub(crate) struct CleanReconcileTracker {
    /// Notification signalled whenever a failed reconciliation is recorded.
    failure_notify: Arc<Notify>,
    /// Latest attempt and clean reconciliation status keyed by organization.
    statuses: Arc<Mutex<HashMap<String, OrgReconciliationStatus>>>,
}

impl CleanReconcileTracker {
    /// Create an empty reconciliation status tracker.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Return the most recent recorded status for `org`.
    pub(crate) fn get(&self, org: &str) -> Option<OrgReconciliationStatus> {
        self.statuses
            .lock()
            .expect("clean reconcile tracker lock not to be poisoned")
            .get(org)
            .cloned()
    }

    /// Record a reconciliation outcome for `org` at `completed_at`.
    pub(crate) fn record_outcome(&self, org: &str, completed_at: OffsetDateTime, is_clean: bool) {
        {
            let mut guard = self.statuses.lock().expect("clean reconcile tracker lock not to be poisoned");
            let entry = guard.entry(org.to_string()).or_default();
            if entry.last_attempt_at.is_none_or(|prev| completed_at > prev) {
                entry.last_attempt_at = Some(completed_at);
            }
            if is_clean && entry.last_clean_at.is_none_or(|prev| completed_at > prev) {
                entry.last_clean_at = Some(completed_at);
            }
        }
        if !is_clean {
            self.failure_notify.notify_one();
        }
    }

    /// Wait until a failed reconciliation outcome is recorded.
    pub(crate) async fn wait_for_failure(&self) {
        self.failure_notify.notified().await;
    }
}

/// Shared handle tracking spawned deferred PR validation and check-publication
/// retry tasks so graceful shutdown can drain or abort them cleanly.
#[derive(Clone, Default)]
pub(crate) struct DeferredRetries {
    /// Active deferred retry task handles.
    handles: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl DeferredRetries {
    /// Create an empty deferred-retries tracker.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Abort all currently tracked deferred retry tasks.
    pub(crate) fn abort_all(&self) {
        let handles = {
            let mut guard = self.handles.lock().expect("deferred retries lock not to be poisoned");
            std::mem::take(&mut *guard)
        };
        for handle in handles {
            handle.abort();
        }
    }

    /// Wait for all currently tracked deferred retry tasks to finish.
    ///
    /// Cancel-safe: if this future is dropped before all tasks finish (for
    /// example on a shutdown timeout), unfinished handles are returned to the
    /// tracker so a subsequent `join_all` or `abort_all` still observes them.
    pub(crate) async fn join_all(&self) {
        struct ReturnOnDrop<'a> {
            handles: &'a Mutex<Vec<JoinHandle<()>>>,
            pending: Vec<JoinHandle<()>>,
        }

        impl Drop for ReturnOnDrop<'_> {
            fn drop(&mut self) {
                if !self.pending.is_empty() {
                    let mut guard = self.handles.lock().expect("deferred retries lock not to be poisoned");
                    guard.extend(self.pending.drain(..));
                }
            }
        }

        loop {
            let pending = {
                let mut guard = self.handles.lock().expect("deferred retries lock not to be poisoned");
                std::mem::take(&mut *guard)
            };
            if pending.is_empty() {
                break;
            }
            let mut guard = ReturnOnDrop {
                handles: &self.handles,
                pending,
            };
            while let Some(handle) = guard.pending.last_mut() {
                let _ = handle.await;
                guard.pending.pop();
            }
        }
    }

    /// Track a newly spawned deferred retry task, pruning finished ones.
    pub(crate) fn push(&self, handle: JoinHandle<()>) {
        let mut guard = self.handles.lock().expect("deferred retries lock not to be poisoned");
        guard.retain(|existing| !existing.is_finished());
        guard.push(handle);
    }
}

/// Represents a job to be executed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Job {
    /// A reconcile job verifies if the desired state as described in the
    /// configuration files matches the actual state in the external services,
    /// applying the necessary changes. This work is delegated on services
    /// handlers, one for each of the external services. It can be triggered
    /// periodically or manually from a pull request. When it's triggered from
    /// a pull request, any feedback will be published to it in the form of
    /// comments.
    Reconcile(ReconcileInput),

    /// A validate job verifies that the proposed changes to the configuration
    /// files in a pull request are valid, providing feedback to address issues
    /// whenever possible, as well as a summary of changes to facilitate
    /// reviews.
    Validate(ValidateInput),
}

impl Job {
    /// Get the name of the organization this job is related to.
    pub(crate) fn org_name(&self) -> &str {
        match self {
            Job::Reconcile(input) => &input.org.name,
            Job::Validate(input) => &input.org.name,
        }
    }
}

/// Shared runtime dependencies passed into the jobs handler and workers.
#[derive(Clone)]
pub(crate) struct JobsContext {
    /// Cancellation token interrupting in-flight triage and PR jobs when the
    /// graceful drain deadline expires, while leaving `transport_cancel` open
    /// briefly for fail-closed check runs and partial reconciliation comments.
    pub cancel_jobs: CancellationToken,
    /// Shared tracker of recent clean reconciliations per org.
    pub clean_tracker: CleanReconcileTracker,
    /// Database handle for audit logging.
    pub db: DynDB,
    /// Tracker for deferred rate-limited PR validation retries.
    pub deferred_retries: DeferredRetries,
    /// Server-side GitHub API client (comments, checks, PR files).
    pub gh: DynGH,
    /// Core GitHub file content reader.
    pub ghc: core::github::DynGH,
    /// Shared tracker of latest head SHAs per PR.
    pub latest_pr_heads: LatestPrHeads,
    /// Sender for re-enqueueing deferred PR validations after rate limits.
    pub pr_jobs_tx: mpsc::UnboundedSender<Job>,
    /// Minimum clean interval between periodic reconciliations.
    pub reconcile_frequency: Duration,
    /// Registered external service handlers.
    pub services: HashMap<ServiceName, DynServiceHandler>,
    /// Cancellation token signalling intake stop (stops periodic sweeps).
    pub stop_intake: CancellationToken,
    /// Cancellation token signalling workers to exit after draining PR queues
    /// (cancelled only after triage and deferred retries have finished).
    pub stop_workers: CancellationToken,
    /// Cancellation token aborting underlying GitHub HTTP waits on final exit.
    pub transport_cancel: CancellationToken,
}

/// Shared tracker of the latest head commit SHA observed per `(org, pr_number)`
/// so queued triage or deferred validation retries never certify a superseded
/// commit, while ignoring out-of-order or redelivered older webhooks.
#[derive(Clone, Default)]
pub(crate) struct LatestPrHeads {
    /// Latest head commit record keyed by `(organization, pr_number)`.
    heads: Arc<Mutex<HashMap<(String, i64), LatestPrHead>>>,
}

impl LatestPrHeads {
    /// Create an empty PR head tracker.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Return the current attempt generation for `(org, pr_number)` if its
    /// tracked head matches `sha`.
    pub(crate) fn current_generation(&self, org: &str, pr_number: i64, sha: &str) -> Option<u64> {
        self.heads
            .lock()
            .expect("latest PR heads lock not to be poisoned")
            .get(&(org.to_string(), pr_number))
            .filter(|latest| latest.sha == sha)
            .map(|latest| latest.generation)
    }

    /// Return true when a newer commit SHA has been observed for `(org, pr_number)`.
    pub(crate) fn is_superseded(&self, org: &str, pr_number: i64, sha: &str) -> bool {
        self.is_superseded_attempt(org, pr_number, sha, None)
    }

    /// Return true when a newer commit SHA OR a newer rerun generation on the
    /// same SHA has been observed for `(org, pr_number)`.
    pub(crate) fn is_superseded_attempt(
        &self,
        org: &str,
        pr_number: i64,
        sha: &str,
        generation: Option<u64>,
    ) -> bool {
        self.heads
            .lock()
            .expect("latest PR heads lock not to be poisoned")
            .get(&(org.to_string(), pr_number))
            .is_some_and(|latest| {
                latest.sha != sha || generation.is_some_and(|attempt_gen| latest.generation != attempt_gen)
            })
    }

    /// Record `sha` as the latest observed head commit for `(org, pr_number)`
    /// unless the existing entry is strictly newer (by `updated_at` timestamp
    /// or prior-SHA history). Returns `Some(generation)` when accepted (bumping
    /// the generation counter for reruns on the same SHA) or `None` if rejected
    /// as a stale redelivery.
    pub(crate) fn record(
        &self,
        org: &str,
        pr_number: i64,
        sha: String,
        updated_at: Option<OffsetDateTime>,
        previous_sha: Option<String>,
    ) -> Option<u64> {
        let mut guard = self.heads.lock().expect("latest PR heads lock not to be poisoned");
        let key = (org.to_string(), pr_number);
        let mut next_generation = 1u64;
        let mut seen_shas = VecDeque::new();

        if let Some(existing) = guard.get(&key) {
            // Re-requests and redeliveries of the current head are always
            // accepted; keep the newer timestamp and bump generation so any
            // delayed publication from an earlier attempt is superseded
            if existing.sha == sha {
                let merged_ts = match (existing.updated_at, updated_at) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (Some(a), None) => Some(a),
                    (None, Some(b)) => Some(b),
                    (None, None) => None,
                };
                next_generation = existing.generation.saturating_add(1);
                let updated = LatestPrHead {
                    generation: next_generation,
                    seen_shas: existing.seen_shas.clone(),
                    sha,
                    previous_sha: previous_sha.or_else(|| existing.previous_sha.clone()),
                    updated_at: merged_ts,
                };
                guard.insert(key, updated);
                return Some(next_generation);
            }

            // Reject strictly older timestamps for a different SHA
            if let (Some(prev_ts), Some(new_ts)) = (existing.updated_at, updated_at)
                && new_ts < prev_ts
            {
                return None;
            }

            // On equal or missing timestamps, accept forward `before -> sha`
            // transitions and reject SHAs already seen earlier for this PR.
            // Strictly newer timestamps (for example a retargeted PR returning
            // to a previously seen head) are always accepted
            let is_strictly_newer = matches!(
                (existing.updated_at, updated_at),
                (Some(prev_ts), Some(new_ts)) if new_ts > prev_ts
            );
            let is_forward_transition = previous_sha.as_deref() == Some(existing.sha.as_str());
            let already_seen = existing.seen_shas.iter().any(|seen| seen == &sha)
                || existing.previous_sha.as_deref() == Some(sha.as_str());
            if !is_strictly_newer && !is_forward_transition && already_seen {
                return None;
            }

            // Carry forward prior SHA history (capped) and bump generation
            next_generation = existing.generation.saturating_add(1);
            seen_shas.clone_from(&existing.seen_shas);
            if !seen_shas.iter().any(|seen| seen == &existing.sha) {
                if seen_shas.len() >= MAX_SEEN_HEAD_SHAS {
                    seen_shas.pop_front();
                }
                seen_shas.push_back(existing.sha.clone());
            }
            if let Some(prev) = &existing.previous_sha
                && !seen_shas.iter().any(|seen| seen == prev)
            {
                if seen_shas.len() >= MAX_SEEN_HEAD_SHAS {
                    seen_shas.pop_front();
                }
                seen_shas.push_back(prev.clone());
            }
        }

        guard.insert(
            key,
            LatestPrHead {
                generation: next_generation,
                seen_shas,
                sha,
                previous_sha,
                updated_at,
            },
        );
        Some(next_generation)
    }
}

/// Result reported back to the periodic sweeper when an org worker finishes
/// (or skips) a periodic reconciliation job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PeriodicJobOutcome {
    /// Reconciliation ran and completed with no top-level or per-change errors.
    Clean,
    /// Reconciliation was deferred because the org worker was busy with PR work.
    DeferredBusy,
    /// Reconciliation ran and completed with one or more errors.
    Failed,
    /// Reconciliation was skipped because a recent clean PR run satisfied the
    /// frequency window.
    SkippedClean,
}

/// Compact pull request webhook payload queued for asynchronous triage so the
/// HTTP handler can acknowledge GitHub promptly even during rate-limit waits.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PullRequestTriageInput {
    /// Webhook pull request action (`opened`, `edited`, `reopened`,
    /// `synchronize`, or `closed`).
    pub action: PullRequestEventAction,
    /// Target organization configuration matched by the webhook.
    pub org: Organization,
    /// Head branch name reported by the webhook.
    pub pr_head_ref: String,
    /// Immutable head commit SHA reported by the webhook.
    pub pr_head_sha: String,
    /// Pull request number.
    pub pr_number: i64,
    /// Monotonic timestamp when the webhook arrived (for latency logging).
    pub received_at: Instant,

    /// Previous head SHA reported by `synchronize` webhooks, when present.
    pub pr_before_sha: Option<String>,
    /// GitHub login of the pull request author, when present.
    pub pr_created_by: Option<String>,
    /// Owner login of the head repository (differs for fork PRs).
    pub pr_head_owner: Option<String>,
    /// Name of the head repository (differs for fork PRs).
    pub pr_head_repo: Option<String>,
    /// Timestamp when the pull request was merged, when present.
    pub pr_merged_at: Option<OffsetDateTime>,
    /// GitHub login of the user who merged the pull request, when present.
    pub pr_merged_by: Option<String>,
    /// Last updated timestamp reported by the webhook, when present.
    pub pr_updated_at: Option<OffsetDateTime>,
}

impl PullRequestTriageInput {
    /// Create a compact triage item from a validated pull request webhook.
    pub(crate) fn new(
        org: Organization,
        action: PullRequestEventAction,
        pr: PullRequestData,
        before: Option<String>,
    ) -> Self {
        let mut pr_merged_at = None;
        if let Some(merged_at) = pr.merged_at {
            if let Ok(parsed) = OffsetDateTime::from_unix_timestamp(merged_at.timestamp()) {
                pr_merged_at = Some(parsed);
            } else {
                error!(pr.number, ?merged_at, "invalid merged_at value");
            }
        }
        let pr_updated_at =
            pr.updated_at.and_then(|dt| OffsetDateTime::from_unix_timestamp(dt.timestamp()).ok());

        Self {
            action,
            org,
            pr_head_ref: pr.head.ref_,
            pr_head_sha: pr.head.sha,
            pr_number: pr.number,
            received_at: Instant::now(),
            pr_before_sha: before,
            pr_created_by: pr.user.map(|u| u.login),
            pr_head_owner: pr.head.repo.as_ref().map(|r| r.owner.clone().login),
            pr_head_repo: pr.head.repo.map(|r| r.name),
            pr_merged_at,
            pr_merged_by: pr.merged_by.map(|u| u.login),
            pr_updated_at,
        }
    }

    /// Create a triage item for a re-requested check run on an associated PR.
    fn from_associated_pr(org: Organization, pr: AssociatedPullRequest, received_at: Instant) -> Self {
        Self {
            action: PullRequestEventAction::Synchronize,
            org,
            pr_head_ref: pr.head_ref,
            pr_head_sha: pr.head_sha,
            pr_number: pr.number,
            received_at,
            pr_before_sha: None,
            pr_created_by: None,
            pr_head_owner: pr.head_owner,
            pr_head_repo: pr.head_repo,
            pr_merged_at: None,
            pr_merged_by: None,
            pr_updated_at: pr.updated_at,
        }
    }

    /// Convert into a [`ReconcileInput`] for a merged pull request.
    fn into_reconcile_input(self) -> ReconcileInput {
        ReconcileInput {
            org: self.org,
            pr_number: Some(self.pr_number),
            pr_created_by: self.pr_created_by,
            pr_merged_by: self.pr_merged_by,
            pr_merged_at: self.pr_merged_at,
        }
    }

    /// Convert into a [`ValidateInput`] for a validation-triggering PR event.
    fn into_validate_input(self) -> ValidateInput {
        ValidateInput {
            org: self.org,
            pr_number: self.pr_number,
            pr_head_owner: self.pr_head_owner,
            pr_head_repo: self.pr_head_repo,
            pr_head_ref: self.pr_head_ref,
            pr_head_sha: self.pr_head_sha,
            retry_attempt: 0,
        }
    }
}

/// Wrapper around a periodic [`Job`] carrying an optional completion signal
/// awaited by the sequential periodic sweeper.
pub(crate) struct QueuedJob {
    /// Job to run.
    pub job: Job,

    /// Completion channel notified with the outcome once finished.
    pub done: Option<oneshot::Sender<PeriodicJobOutcome>>,
}

/// Information required to process a reconcile job.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct ReconcileInput {
    pub org: Organization,
    pub pr_number: Option<i64>,
    pub pr_created_by: Option<String>,
    pub pr_merged_by: Option<String>,
    pub pr_merged_at: Option<OffsetDateTime>,
}

/// Webhook triage item queued for asynchronous processing.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TriageInput {
    /// Re-requested CLOWarden check run.
    CheckRunRerequested(CheckRunRerequestedInput),
    /// Actionable pull request event (`opened`, `synchronize`, or `closed`).
    PullRequest(PullRequestTriageInput),
}

impl TriageInput {
    /// Return the GitHub App installation id associated with this item.
    fn installation_id(&self) -> i64 {
        match self {
            Self::CheckRunRerequested(input) => input.org.installation_id,
            Self::PullRequest(input) => input.org.installation_id,
        }
    }
}

/// Information required to process a validate job.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct ValidateInput {
    pub org: Organization,
    pub pr_number: i64,
    pub pr_head_owner: Option<String>,
    pub pr_head_repo: Option<String>,
    pub pr_head_ref: String,
    pub pr_head_sha: String,
    #[serde(default)]
    pub retry_attempt: u32,
}

/// Latest head commit metadata stored per `(org, pr_number)`.
struct LatestPrHead {
    /// Monotonic attempt generation incremented on each head update or rerun.
    generation: u64,
    /// Bounded history of earlier SHAs observed on this PR (for same-second tie breaking).
    seen_shas: VecDeque<String>,
    /// Head commit SHA.
    sha: String,

    /// Previous head SHA from `synchronize` webhooks, when present.
    previous_sha: Option<String>,
    /// Last updated timestamp from the webhook payload, when present.
    updated_at: Option<OffsetDateTime>,
}

// Helpers.

/// Normalize a rate-limit retry delay with a default fallback and upper cap.
fn bounded_rate_limit_delay(retry_after: Option<Duration>) -> Duration {
    retry_after.unwrap_or(DEFAULT_VALIDATE_DEFER_DELAY).min(MAX_VALIDATE_DEFER_DELAY)
}

/// Return `Instant::now() + duration` safely without panicking on overflow.
fn deadline_after(duration: Duration) -> Instant {
    deadline_from(Instant::now(), duration)
}

/// Return `base + duration` safely without panicking on overflow.
fn deadline_from(base: Instant, duration: Duration) -> Instant {
    base.checked_add(duration)
        .unwrap_or_else(|| base + Duration::from_secs(u64::from(u32::MAX)))
}

/// Return the remaining freshness window if `status`'s latest attempt itself
/// was clean (`last_clean_at >= last_attempt_at`) and within
/// `reconcile_frequency`.
fn fresh_clean_remaining(
    status: &OrgReconciliationStatus,
    now_utc: OffsetDateTime,
    reconcile_frequency: Duration,
) -> Option<Duration> {
    let last_clean_at = status.last_clean_at?;
    if status.last_attempt_at.is_some_and(|last_attempt_at| last_attempt_at > last_clean_at) {
        return None;
    }
    let elapsed = now_utc - last_clean_at;
    let elapsed_secs = u64::try_from(elapsed.whole_seconds().max(0)).unwrap_or(0);
    let elapsed_dur = Duration::from_secs(elapsed_secs);
    if elapsed_dur < reconcile_frequency {
        Some(reconcile_frequency.saturating_sub(elapsed_dur))
    } else {
        None
    }
}

/// Extract the suggested retry delay from a [`RateLimitError`] in `err`'s chain.
fn rate_limit_retry_after(err: &Error) -> Option<Duration> {
    for cause in err.chain() {
        if let Some(rl) = cause.downcast_ref::<RateLimitError>() {
            return rl.retry_after;
        }
        if let Some(merr) = cause.downcast_ref::<MultiError>() {
            for inner in merr.errors() {
                if let Some(delay) = rate_limit_retry_after(inner) {
                    return Some(delay);
                }
            }
        }
    }
    None
}

/// Sleep until `deadline`, returning `true` if `cancel_token` fired first.
async fn sleep_until_or_cancel(deadline: Instant, cancel_token: &CancellationToken) -> bool {
    if Instant::now() >= deadline {
        return cancel_token.is_cancelled();
    }
    tokio::select! {
        biased;

        () = cancel_token.cancelled() => true,
        () = sleep_until(deadline) => false,
    }
}
