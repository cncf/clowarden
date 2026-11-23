//! Sequential DB-backed periodic reconciliation sweeper.

use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use ::time::OffsetDateTime;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::{Instant, sleep_until, timeout},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

use clowarden_core::cfg::Organization;

use crate::db::{DynDB, OrgReconciliationStatus};

use super::{
    CleanReconcileTracker, Job, PeriodicJobOutcome, QueuedJob, ReconcileInput, deadline_after, deadline_from,
    fresh_clean_remaining, sleep_until_or_cancel,
};

/// Timeout for reading reconciliation schedule state from PostgreSQL.
const DB_SCHEDULE_QUERY_TIMEOUT: Duration = Duration::from_secs(30);

/// Base backoff duration before retrying an organization whose previous
/// reconciliation attempt failed (doubling up to `reconcile_frequency`).
const FAILURE_RETRY_BASE_DELAY: Duration = Duration::from_mins(15);

/// Minimum pause between sweep cycles after dispatching work or hitting a
/// database error (in seconds).
const MIN_SWEEP_RETRY_DELAY: u64 = 60;

/// Sequential DB-backed periodic sweeper. Orders organizations by oldest clean
/// reconciliation (persisted in PostgreSQL), skips organizations cleanly
/// reconciled within `reconcile_frequency`, applies exponential backoff on
/// persistent failures, defers orgs busy with PR work, wakes early when a PR
/// reconciliation fails, and runs at most one periodic reconciliation globally
/// at a time.
pub(crate) fn scheduler(
    db: DynDB,
    clean_tracker: CleanReconcileTracker,
    periodic_tx: mpsc::Sender<QueuedJob>,
    stop_intake: CancellationToken,
    orgs: &[Organization],
    reconcile_frequency: Duration,
) -> Vec<JoinHandle<()>> {
    let sweeper = PeriodicSweeper::new(
        db,
        clean_tracker,
        periodic_tx,
        stop_intake,
        orgs.to_vec(),
        reconcile_frequency,
    );
    vec![tokio::spawn(sweeper.run())]
}

/// Stateful sequential periodic sweeper across configured organizations.
struct PeriodicSweeper {
    /// Shared tracker of recent reconciliation attempts and clean completions.
    clean_tracker: CleanReconcileTracker,
    /// Database handle for loading persisted reconciliation timestamps.
    db: DynDB,
    /// Organizations deferred because their worker was busy with PR work.
    deferred_busy_orgs: HashSet<String>,
    /// Consecutive failed reconciliation counts keyed by organization name.
    failure_strikes: HashMap<String, u32>,
    /// Next paced start timestamp for a fresh periodic reconciliation slot.
    next_slot_start: Instant,
    /// Configured organizations managed by this sweeper.
    orgs: Vec<Organization>,
    /// Channel sending periodic reconciliation jobs to the router.
    periodic_tx: mpsc::Sender<QueuedJob>,
    /// Target minimum interval between clean reconciliations per organization.
    reconcile_frequency: Duration,
    /// Even target start spacing between fresh periodic slots across orgs.
    slot_spacing: Duration,
    /// Cancellation token signalling intake stop.
    stop_intake: CancellationToken,
}

impl PeriodicSweeper {
    /// Create a new periodic sweeper with evenly spaced initial pacing.
    fn new(
        db: DynDB,
        clean_tracker: CleanReconcileTracker,
        periodic_tx: mpsc::Sender<QueuedJob>,
        stop_intake: CancellationToken,
        orgs: Vec<Organization>,
        reconcile_frequency: Duration,
    ) -> Self {
        // Compute even target start spacing across the reconciliation window
        let org_count = u32::try_from(orgs.len()).unwrap_or(u32::MAX).max(1);
        let slot_spacing = reconcile_frequency / org_count;

        Self {
            clean_tracker,
            db,
            deferred_busy_orgs: HashSet::new(),
            failure_strikes: HashMap::new(),
            next_slot_start: Instant::now(),
            orgs,
            periodic_tx,
            reconcile_frequency,
            slot_spacing,
            stop_intake,
        }
    }

    /// Compute the sleep duration until the next sweep cycle should begin.
    fn compute_cycle_pause(&self, cycle: &SweepCycle, woke_early_for_failure: bool) -> Duration {
        if woke_early_for_failure {
            return Duration::ZERO;
        }

        // Re-check promptly if any orgs were deferred due to PR traffic;
        // otherwise sleep only until the earliest absolute due deadline
        let busy_pause =
            (!self.deferred_busy_orgs.is_empty()).then_some(Duration::from_secs(MIN_SWEEP_RETRY_DELAY));
        let due_pause = cycle
            .earliest_next_due
            .map(|due_at| due_at.saturating_duration_since(Instant::now()).max(Duration::from_secs(1)));
        [busy_pause, due_pause].into_iter().flatten().min().unwrap_or(self.reconcile_frequency)
    }

    /// Dispatch a single periodic reconcile job and wait for its outcome,
    /// returning `None` if shutdown is requested or the router closed.
    async fn dispatch_org(&self, org: &Organization) -> Option<PeriodicJobOutcome> {
        let (done_tx, done_rx) = oneshot::channel();
        let queued = QueuedJob {
            job: Job::Reconcile(ReconcileInput {
                org: org.clone(),
                ..Default::default()
            }),
            done: Some(done_tx),
        };

        // Enqueue the periodic job unless intake stops first
        tokio::select! {
            biased;

            () = self.stop_intake.cancelled() => return None,
            send_res = self.periodic_tx.send(queued) => {
                if send_res.is_err() {
                    return None;
                }
            }
        }

        // Await the worker's completion outcome
        tokio::select! {
            biased;

            () = self.stop_intake.cancelled() => None,
            res = done_rx => Some(res.unwrap_or(PeriodicJobOutcome::Failed)),
        }
    }

    /// Combine persisted DB timestamps with newer in-memory reconciliation
    /// updates for `org_name`.
    fn effective_status(
        &self,
        schedule_state: &HashMap<String, OrgReconciliationStatus>,
        org_name: &str,
    ) -> OrgReconciliationStatus {
        merge_reconciliation_status(schedule_state.get(org_name), self.clean_tracker.get(org_name))
    }

    /// Load the latest attempt and clean timestamps per org from PostgreSQL,
    /// retrying after a bounded pause on database errors or timeouts.
    async fn load_schedule_state(&self) -> Option<HashMap<String, OrgReconciliationStatus>> {
        loop {
            if self.stop_intake.is_cancelled() {
                return None;
            }

            // Query persisted reconciliation timestamps with cancellation and timeout
            let res = tokio::select! {
                biased;

                () = self.stop_intake.cancelled() => return None,
                res = timeout(DB_SCHEDULE_QUERY_TIMEOUT, self.db.get_reconciliation_schedule_state()) => res,
            };

            // Return on success or log the transient failure before retrying
            match res {
                Ok(Ok(map)) => return Some(map),
                Ok(Err(err)) => {
                    error!(?err, "error loading reconciliation schedule state from database");
                }
                Err(_) => {
                    error!("timed out loading reconciliation schedule state from database");
                }
            }

            // Pause before retrying the schedule query
            if sleep_until_or_cancel(
                deadline_after(Duration::from_secs(MIN_SWEEP_RETRY_DELAY)),
                &self.stop_intake,
            )
            .await
            {
                return None;
            }
        }
    }

    /// Update pacing, failure backoff strikes, deferred-busy state, and cycle
    /// counters after one organization's periodic attempt completes.
    fn record_outcome(
        &mut self,
        org_name: &str,
        actual_start: Instant,
        outcome: PeriodicJobOutcome,
        cycle: &mut SweepCycle,
    ) {
        // Advance the pacing slot only when a periodic slot actually executed
        match outcome {
            PeriodicJobOutcome::Clean => {
                if actual_start >= self.next_slot_start {
                    self.next_slot_start = deadline_from(actual_start, self.slot_spacing);
                }
                self.failure_strikes.remove(org_name);
                self.deferred_busy_orgs.remove(org_name);
                cycle.record_due(deadline_after(self.reconcile_frequency));
                cycle.dispatched += 1;
            }
            PeriodicJobOutcome::Failed => {
                if actual_start >= self.next_slot_start {
                    self.next_slot_start = deadline_from(actual_start, self.slot_spacing);
                }
                let strikes = self.failure_strikes.entry(org_name.to_string()).or_insert(0);
                *strikes = strikes.saturating_add(1);
                let backoff = failure_backoff(*strikes, self.reconcile_frequency);
                self.deferred_busy_orgs.remove(org_name);
                cycle.record_due(deadline_after(backoff));
                cycle.dispatched += 1;
            }
            PeriodicJobOutcome::SkippedClean => {
                self.failure_strikes.remove(org_name);
                self.deferred_busy_orgs.remove(org_name);
                cycle.record_due(deadline_after(self.reconcile_frequency));
            }
            PeriodicJobOutcome::DeferredBusy => {
                debug!(org = %org_name, "deferring periodic reconcile; org worker is busy with PR work");
                self.deferred_busy_orgs.insert(org_name.to_string());
                cycle.deferred_busy += 1;
            }
        }

        info!(
            org = %org_name,
            ?outcome,
            duration_secs = actual_start.elapsed().as_secs(),
            "finished periodic reconciliation attempt for organization"
        );
    }

    /// Run sweep cycles continuously until `stop_intake` is cancelled.
    async fn run(mut self) {
        // Exit immediately when no organizations are configured
        if self.orgs.is_empty() {
            return;
        }

        loop {
            // Load persisted schedule state with retry on transient DB errors
            let Some(schedule_state) = self.load_schedule_state().await else {
                break;
            };

            // Run one sequential sweep cycle and wait until the next due org
            // or until a failed PR reconciliation wakes the sweeper early
            let Some(pause) = self.run_sweep_cycle(&schedule_state).await else {
                break;
            };
            if sleep_until_or_wake(deadline_after(pause), &self.stop_intake, &self.clean_tracker).await {
                break;
            }
        }
    }

    /// Run one sequential sweep across configured organizations ordered
    /// oldest-first by clean reconciliation timestamp, respecting failure
    /// backoff, deferring busy organizations, and ending early when an earlier
    /// retry or deferred-busy recheck comes due before the next paced slot.
    async fn run_sweep_cycle(
        &mut self,
        schedule_state: &HashMap<String, OrgReconciliationStatus>,
    ) -> Option<Duration> {
        let cycle_start = Instant::now();
        let now_utc = OffsetDateTime::now_utc();
        let mut cycle = SweepCycle::default();

        // Partition organizations into skipped vs eligible and prioritize retries
        let eligible_orgs = self.select_eligible_orgs(schedule_state, now_utc, &mut cycle);

        // Dispatch eligible organizations sequentially under slot pacing
        let mut woke_early_for_failure = false;
        for (org, is_priority_retry) in eligible_orgs {
            if self.stop_intake.is_cancelled() {
                return None;
            }

            // Wait for the next paced start slot on fresh periodic runs
            if !is_priority_retry {
                match self.wait_for_pacing_slot(&cycle).await {
                    SlotWaitOutcome::Cancelled => return None,
                    SlotWaitOutcome::Ready => {}
                    SlotWaitOutcome::WokeEarlyForFailure => {
                        woke_early_for_failure = true;
                        break;
                    }
                    SlotWaitOutcome::YieldForEarlierDue => {
                        cycle.record_due(self.next_slot_start);
                        break;
                    }
                }
            }

            // Re-check freshness right before dispatch in case a clean PR
            // reconcile finished while waiting for this slot
            let refreshed_status = self.effective_status(schedule_state, &org.name);
            if let Some(remaining) = fresh_clean_remaining(
                &refreshed_status,
                OffsetDateTime::now_utc(),
                self.reconcile_frequency,
            ) {
                cycle.record_due(deadline_after(remaining));
                self.failure_strikes.remove(&org.name);
                self.deferred_busy_orgs.remove(&org.name);
                debug!(
                    org = %org.name,
                    "skipping periodic reconcile; became cleanly reconciled during slot wait"
                );
                continue;
            }

            // Dispatch the periodic reconcile and record its outcome
            let actual_start = Instant::now();
            let outcome = self.dispatch_org(&org).await?;
            self.record_outcome(&org.name, actual_start, outcome, &mut cycle);
        }

        info!(
            dispatched_count = cycle.dispatched,
            deferred_busy_count = cycle.deferred_busy,
            cycle_duration_secs = cycle_start.elapsed().as_secs(),
            "finished periodic reconciliation sweep cycle"
        );

        Some(self.compute_cycle_pause(&cycle, woke_early_for_failure))
    }

    /// Partition organizations into skipped (clean or backing off) and
    /// eligible, sort eligible orgs with priority retries first, and log the
    /// oldest clean reconciliation age.
    fn select_eligible_orgs(
        &mut self,
        schedule_state: &HashMap<String, OrgReconciliationStatus>,
        now_utc: OffsetDateTime,
        cycle: &mut SweepCycle,
    ) -> Vec<(Organization, bool)> {
        let mut eligible_orgs: Vec<(Organization, bool)> = Vec::new();
        let mut oldest_org: Option<(String, Option<OffsetDateTime>)> = None;

        for org in &self.orgs {
            let status = self.effective_status(schedule_state, &org.name);
            if oldest_org
                .as_ref()
                .is_none_or(|(name, ts)| (status.last_clean_at, org.name.as_str()) < (*ts, name.as_str()))
            {
                oldest_org = Some((org.name.clone(), status.last_clean_at));
            }

            // Skip only when the latest attempt itself was clean and within window
            if let Some(remaining) = fresh_clean_remaining(&status, now_utc, self.reconcile_frequency) {
                cycle.record_due(deadline_after(remaining));
                self.failure_strikes.remove(&org.name);
                self.deferred_busy_orgs.remove(&org.name);
                debug!(
                    org = %org.name,
                    remaining_secs = remaining.as_secs(),
                    "skipping periodic reconcile; recently reconciled cleanly"
                );
                continue;
            }

            // Apply exponential backoff when the latest attempt failed recently
            let latest_failed = status
                .last_attempt_at
                .is_some_and(|attempt| status.last_clean_at.is_none_or(|clean| attempt > clean));
            if latest_failed && let Some(last_attempt_at) = status.last_attempt_at {
                let strikes = self.failure_strikes.get(&org.name).copied().unwrap_or(1);
                let backoff = failure_backoff(strikes, self.reconcile_frequency);
                let elapsed = now_utc - last_attempt_at;
                let elapsed_secs = u64::try_from(elapsed.whole_seconds().max(0)).unwrap_or(0);
                let elapsed_dur = Duration::from_secs(elapsed_secs);
                if elapsed_dur < backoff {
                    let remaining = backoff.saturating_sub(elapsed_dur);
                    cycle.record_due(deadline_after(remaining));
                    self.deferred_busy_orgs.remove(&org.name);
                    debug!(
                        org = %org.name,
                        strikes,
                        backoff_secs = backoff.as_secs(),
                        remaining_secs = remaining.as_secs(),
                        "skipping periodic reconcile; backing off after recent failure"
                    );
                    continue;
                }
            }

            let is_priority_retry = self.deferred_busy_orgs.contains(&org.name)
                || latest_failed
                || self.failure_strikes.contains_key(&org.name);
            eligible_orgs.push((org.clone(), is_priority_retry));
        }

        // Order priority retries ahead of fresh periodic orgs, oldest-first
        // within each group
        eligible_orgs.sort_by(|(a, a_priority), (b, b_priority)| {
            let a_status = self.effective_status(schedule_state, &a.name);
            let b_status = self.effective_status(schedule_state, &b.name);
            (!*a_priority)
                .cmp(&!*b_priority)
                .then_with(|| a_status.last_clean_at.cmp(&b_status.last_clean_at))
                .then_with(|| a_status.last_attempt_at.cmp(&b_status.last_attempt_at))
                .then_with(|| a.name.cmp(&b.name))
        });

        // Log oldest clean reconciliation age for operational visibility
        if let Some((oldest_name, oldest_ts)) = oldest_org {
            let oldest_age_secs = oldest_ts.map(|ts| (now_utc - ts).whole_seconds().max(0));
            info!(
                org_count = self.orgs.len(),
                oldest_org = %oldest_name,
                ?oldest_age_secs,
                reconcile_frequency_secs = self.reconcile_frequency.as_secs(),
                "starting periodic reconciliation sweep cycle"
            );
        }

        eligible_orgs
    }

    /// Wait for `next_slot_start` unless an earlier failure retry or
    /// deferred-busy recheck is due first, a failure notification arrives, or
    /// shutdown is requested.
    async fn wait_for_pacing_slot(&self, cycle: &SweepCycle) -> SlotWaitOutcome {
        let now = Instant::now();
        if self.next_slot_start <= now {
            return SlotWaitOutcome::Ready;
        }

        // End the cycle early when a retry or busy recheck comes due before the slot
        let busy_due = (!self.deferred_busy_orgs.is_empty())
            .then(|| deadline_from(now, Duration::from_secs(MIN_SWEEP_RETRY_DELAY)));
        let early_wakeup = [cycle.earliest_next_due, busy_due].into_iter().flatten().min();
        if early_wakeup.is_some_and(|due| due < self.next_slot_start) {
            return SlotWaitOutcome::YieldForEarlierDue;
        }

        // Sleep until the pacing slot, waking immediately on cancel or failure
        if sleep_until_or_wake(self.next_slot_start, &self.stop_intake, &self.clean_tracker).await {
            return SlotWaitOutcome::Cancelled;
        }
        if Instant::now() < self.next_slot_start {
            return SlotWaitOutcome::WokeEarlyForFailure;
        }

        SlotWaitOutcome::Ready
    }
}

/// Outcome of waiting for the next paced periodic reconciliation slot.
enum SlotWaitOutcome {
    /// Shutdown cancellation fired while waiting.
    Cancelled,
    /// Pacing slot has arrived and dispatch can proceed.
    Ready,
    /// A failed reconciliation woke the sweeper before the pacing slot elapsed.
    WokeEarlyForFailure,
    /// An earlier retry or deferred-busy recheck is due before the pacing slot.
    YieldForEarlierDue,
}

/// Per-cycle counters and next-due deadline accumulated during a single sweep.
#[derive(Default)]
struct SweepCycle {
    /// Organizations deferred due to active PR work during this cycle.
    deferred_busy: usize,
    /// Organizations dispatched for periodic reconciliation during this cycle.
    dispatched: usize,

    /// Earliest absolute deadline when any organization becomes due again.
    earliest_next_due: Option<Instant>,
}

impl SweepCycle {
    /// Record `due_at` as the earliest known next-due deadline when earlier
    /// than the current value.
    fn record_due(&mut self, due_at: Instant) {
        self.earliest_next_due = Some(self.earliest_next_due.map_or(due_at, |cur| cur.min(due_at)));
    }
}

// Helpers.

/// Compute exponential backoff delay for an org with `strikes` consecutive
/// failed reconciliations, capped at `reconcile_frequency`.
fn failure_backoff(strikes: u32, reconcile_frequency: Duration) -> Duration {
    let shift = strikes.saturating_sub(1).min(6);
    let factor = 1u32.checked_shl(shift).unwrap_or(64);
    FAILURE_RETRY_BASE_DELAY.saturating_mul(factor).min(reconcile_frequency)
}

/// Merge persisted DB reconciliation timestamps with newer in-memory updates.
fn merge_reconciliation_status(
    db_status: Option<&OrgReconciliationStatus>,
    mem_status: Option<OrgReconciliationStatus>,
) -> OrgReconciliationStatus {
    let max_opt = |a: Option<OffsetDateTime>, b: Option<OffsetDateTime>| match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (None, None) => None,
    };
    OrgReconciliationStatus {
        last_attempt_at: max_opt(
            db_status.and_then(|s| s.last_attempt_at),
            mem_status.as_ref().and_then(|s| s.last_attempt_at),
        ),
        last_clean_at: max_opt(
            db_status.and_then(|s| s.last_clean_at),
            mem_status.and_then(|s| s.last_clean_at),
        ),
    }
}

/// Sleep until `deadline` or until a failed reconciliation notifies
/// `clean_tracker`, returning `true` only if `cancel_token` fired first.
async fn sleep_until_or_wake(
    deadline: Instant,
    cancel_token: &CancellationToken,
    clean_tracker: &CleanReconcileTracker,
) -> bool {
    if Instant::now() >= deadline {
        return cancel_token.is_cancelled();
    }
    tokio::select! {
        biased;

        () = cancel_token.cancelled() => true,
        () = clean_tracker.wait_for_failure() => false,
        () = sleep_until(deadline) => false,
    }
}
