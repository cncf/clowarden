//! This module provides GitHub API rate-limit governance, mutation pacing,
//! concurrency control, and HTTP middleware shared across installations.

use std::{
    collections::HashMap,
    error::Error as StdError,
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Error, Result};
use async_trait::async_trait;
use octorust::ClientError;
use reqwest::{Method, Request, Response, StatusCode, Url, header::HeaderMap};
use reqwest_middleware::{Error as MiddlewareError, Middleware, Next};
use task_local_extensions::Extensions;
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc2822};
use tokio::{
    sync::Semaphore,
    time::{Instant, sleep},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// Default page size appended to paginated GitHub REST list endpoints.
pub(crate) const DEFAULT_PER_PAGE: &str = "100";

/// Default maximum jitter added to secondary rate-limit backoffs.
const DEFAULT_JITTER_MAX: Duration = Duration::from_secs(2);

/// Default threshold below which remaining primary quota logs a warning.
const DEFAULT_LOW_REMAINING_THRESHOLD: u32 = 200;

/// Default maximum concurrent outbound GitHub API requests process-wide.
const DEFAULT_MAX_IN_FLIGHT: usize = 4;

/// Default maximum retries when a request hits a GitHub rate limit.
const DEFAULT_MAX_RATE_LIMIT_RETRIES: u32 = 3;

/// Default maximum retries when an idempotent request hits a transient error.
const DEFAULT_MAX_TRANSIENT_RETRIES: u32 = 2;

/// Default cumulative wait budget per GitHub API operation.
const DEFAULT_MAX_WAIT: Duration = Duration::from_mins(5);

/// Default minimum spacing between mutative GitHub API requests.
const DEFAULT_MUTATION_INTERVAL: Duration = Duration::from_secs(1);

/// Default base wait duration for secondary rate limits without `Retry-After`.
const DEFAULT_SECONDARY_BASE_BACKOFF: Duration = Duration::from_secs(60);

/// Default base wait duration for transient 5xx/network retries.
const DEFAULT_TRANSIENT_BASE_BACKOFF: Duration = Duration::from_secs(1);

/// Maximum parsed rate-limit wait accepted from response headers (caps bogus
/// or extreme `Retry-After` / `x-ratelimit-reset` values to avoid overflow).
const MAX_PARSED_RATE_LIMIT_WAIT: Duration = Duration::from_hours(24);

/// Safety margin added to `x-ratelimit-reset` timestamps.
const RESET_BUFFER: Duration = Duration::from_secs(1);

tokio::task_local! {
    /// Per-operation cancellation token and cumulative wait tracker propagated
    /// from [`RateLimitGovernor::execute`] to [`GitHubMiddleware::handle`].
    static CALL_CONTEXT: CallContext;
}

/// Per-operation context shared between [`RateLimitGovernor::execute`] and
/// [`GitHubMiddleware::handle`].
#[derive(Clone)]
struct CallContext {
    /// Caller cancellation token aborting pending waits.
    cancel: CancellationToken,
    /// Cumulative wait duration accrued by the current governed operation.
    cumulative_wait: Arc<Mutex<Duration>>,
}

/// Error returned when a GitHub API call is aborted due to cancellation.
#[derive(Debug, Error)]
#[error("github api request cancelled")]
pub struct CancelledError;

/// Error returned when a GitHub API call receives HTTP `401 Unauthorized`
/// alongside exhausted rate-limit headers that would otherwise be converted
/// into `octorust::ClientError::RateLimited`.
#[derive(Debug, Error)]
#[error("github api returned 401 unauthorized")]
pub struct UnauthorizedError;

/// Error returned when a GitHub primary or secondary rate limit prevents a
/// request from completing within the configured retry or wait budget.
#[derive(Debug, Clone, Error)]
#[error("github rate limit exceeded ({kind}): {message}")]
pub struct RateLimitError {
    /// Whether this error came from a secondary (abuse) rate limit.
    pub is_secondary: bool,
    /// Human-readable kind ("primary" or "secondary") used in Display.
    kind: &'static str,
    /// Diagnostic detail describing the rate-limit response or wait cap.
    pub message: String,

    /// Optional duration until the rate limit is expected to clear.
    pub retry_after: Option<Duration>,
}

impl RateLimitError {
    /// Create a new primary rate-limit error.
    #[must_use]
    pub fn primary(retry_after: Option<Duration>, message: impl Into<String>) -> Self {
        Self {
            is_secondary: false,
            kind: "primary",
            message: message.into(),
            retry_after,
        }
    }

    /// Create a new secondary rate-limit error.
    #[must_use]
    pub fn secondary(retry_after: Option<Duration>, message: impl Into<String>) -> Self {
        Self {
            is_secondary: true,
            kind: "secondary",
            message: message.into(),
            retry_after,
        }
    }
}

/// Classification of a GitHub API request for pacing and transient retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    /// Mutative request that is safe to retry on transient 5xx/network errors
    /// (for example PUT or DELETE).
    IdempotentWrite,
    /// Mutative request that must not be retried on transient 5xx/network
    /// errors (for example POST creating a resource or comment).
    NonIdempotentWrite,
    /// Read-only GET/HEAD request.
    Read,
}

impl RequestKind {
    /// Whether this request mutates state on GitHub and must obey write pacing.
    fn is_write(self) -> bool {
        matches!(self, Self::IdempotentWrite | Self::NonIdempotentWrite)
    }

    /// Whether this request may be retried after a transient 5xx/network error.
    fn retries_transient(self) -> bool {
        matches!(self, Self::IdempotentWrite | Self::Read)
    }
}

/// Tuning parameters for [`RateLimitGovernor`].
#[derive(Debug, Clone)]
pub struct RateLimitPolicy {
    /// Maximum jitter added to computed secondary rate-limit backoffs.
    pub jitter_max: Duration,
    /// Remaining primary quota below which a warning is logged.
    pub low_remaining_threshold: u32,
    /// Maximum concurrent outbound GitHub API requests across all orgs.
    pub max_in_flight: usize,
    /// Maximum retry attempts on GitHub rate-limit responses.
    pub max_rate_limit_retries: u32,
    /// Maximum retry attempts on transient 5xx/network errors.
    pub max_transient_retries: u32,
    /// Maximum cumulative wait time allowed for a single operation.
    pub max_wait: Duration,
    /// Minimum spacing enforced between mutative requests.
    pub mutation_interval: Duration,
    /// Base backoff duration for secondary rate limits without `Retry-After`.
    pub secondary_base_backoff: Duration,
    /// Base backoff duration for transient 5xx/network errors.
    pub transient_base_backoff: Duration,
}

impl Default for RateLimitPolicy {
    fn default() -> Self {
        Self {
            jitter_max: DEFAULT_JITTER_MAX,
            low_remaining_threshold: DEFAULT_LOW_REMAINING_THRESHOLD,
            max_in_flight: DEFAULT_MAX_IN_FLIGHT,
            max_rate_limit_retries: DEFAULT_MAX_RATE_LIMIT_RETRIES,
            max_transient_retries: DEFAULT_MAX_TRANSIENT_RETRIES,
            max_wait: DEFAULT_MAX_WAIT,
            mutation_interval: DEFAULT_MUTATION_INTERVAL,
            secondary_base_backoff: DEFAULT_SECONDARY_BASE_BACKOFF,
            transient_base_backoff: DEFAULT_TRANSIENT_BASE_BACKOFF,
        }
    }
}

/// Shared coordinator enforcing concurrency limits, mutation pacing, and
/// primary/secondary rate-limit cooldowns across all GitHub installations.
pub struct RateLimitGovernor {
    /// Global semaphore bounding concurrent outbound GitHub HTTP calls.
    in_flight: Arc<Semaphore>,
    /// Monotonic counter used to derive small deterministic-friendly jitter.
    jitter_counter: AtomicU64,
    /// Active governance policy.
    policy: RateLimitPolicy,
    /// Shared cooldown and write-pacing state.
    state: Mutex<GovernorState>,
}

impl RateLimitGovernor {
    /// Create a new governor using the policy provided.
    #[must_use]
    pub fn new(policy: RateLimitPolicy) -> Self {
        let max_in_flight = policy.max_in_flight.max(1);
        Self {
            in_flight: Arc::new(Semaphore::new(max_in_flight)),
            jitter_counter: AtomicU64::new(0),
            policy,
            state: Mutex::new(GovernorState::default()),
        }
    }

    /// Execute a GitHub API operation under rate-limit governance, retrying
    /// above `octorust` so installation tokens are refreshed after long waits.
    ///
    /// Both `shutdown` and `cancel` abort pending waits cleanly without
    /// spawning background watcher tasks.
    pub async fn execute<T, F, Fut>(
        &self,
        inst_id: Option<i64>,
        kind: RequestKind,
        shutdown: &CancellationToken,
        cancel: &CancellationToken,
        op: F,
    ) -> Result<T>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = std::result::Result<T, ClientError>>,
    {
        self.execute_with_auth_refresh(inst_id, kind, shutdown, cancel, op, || Ok(false)).await
    }

    /// Execute a GitHub API operation under rate-limit governance, invoking
    /// `on_unauthorized` at most once on HTTP `401 Unauthorized` to refresh
    /// cached credentials while preserving cumulative wait and retry budgets.
    pub(crate) async fn execute_with_auth_refresh<T, F, Fut, R>(
        &self,
        inst_id: Option<i64>,
        kind: RequestKind,
        shutdown: &CancellationToken,
        cancel: &CancellationToken,
        op: F,
        mut on_unauthorized: R,
    ) -> Result<T>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = std::result::Result<T, ClientError>>,
        R: FnMut() -> Result<bool>,
    {
        let cumulative_wait = Arc::new(Mutex::new(Duration::ZERO));
        let mut auth_refreshed = false;
        let mut rate_limit_attempts: u32 = 0;
        let mut transient_attempts: u32 = 0;

        loop {
            // Acquire admission (cooldowns, in-flight permit, and pre-dispatch write check)
            let permit = self
                .acquire_admission(inst_id, kind.is_write(), shutdown, cancel, &cumulative_wait)
                .await?;

            // Run the operation while holding only the in-flight permit, passing
            // per-call cancellation and wait budget to GitHubMiddleware
            let dispatched_at = Instant::now();
            let call_ctx = CallContext {
                cancel: cancel.clone(),
                cumulative_wait: cumulative_wait.clone(),
            };
            let result = CALL_CONTEXT
                .scope(call_ctx, async {
                    if kind == RequestKind::Read {
                        tokio::select! {
                            biased;

                            () = shutdown.cancelled() => Err(None),
                            () = cancel.cancelled() => Err(None),
                            res = op() => res.map_err(Some),
                        }
                    } else {
                        op().await.map_err(Some)
                    }
                })
                .await;
            drop(permit);

            match result {
                Ok(value) => {
                    // Reset secondary strike counter only if this request was
                    // dispatched after the last secondary cooldown ended
                    self.record_success(dispatched_at);
                    return Ok(value);
                }
                Err(None) => return Err(Error::new(CancelledError)),
                Err(Some(err)) => {
                    // Classify the failure into rate-limit, transient, unauthorized, or fatal
                    match self.classify_error(inst_id, kind, err) {
                        ClassifiedError::RateLimited(rl_err) => {
                            self.check_rate_limit_retry(
                                inst_id,
                                rl_err,
                                &mut rate_limit_attempts,
                                &cumulative_wait,
                            )?;
                        }
                        ClassifiedError::Transient(err) if kind.retries_transient() => {
                            self.wait_transient_retry(
                                inst_id,
                                err,
                                &mut transient_attempts,
                                shutdown,
                                cancel,
                                &cumulative_wait,
                            )
                            .await?;
                        }
                        ClassifiedError::Unauthorized(err) if !auth_refreshed => {
                            if on_unauthorized()? {
                                auth_refreshed = true;
                                continue;
                            }
                            return Err(err.into());
                        }
                        ClassifiedError::Fatal(err)
                        | ClassifiedError::Transient(err)
                        | ClassifiedError::Unauthorized(err) => {
                            return Err(err.into());
                        }
                    }
                }
            }
        }
    }

    /// Check rate-limit retry count and cumulative wait budget before retrying.
    fn check_rate_limit_retry(
        &self,
        inst_id: Option<i64>,
        rl_err: RateLimitError,
        rate_limit_attempts: &mut u32,
        cumulative_wait: &Mutex<Duration>,
    ) -> Result<()> {
        if *rate_limit_attempts >= self.policy.max_rate_limit_retries {
            warn!(
                ?inst_id,
                is_secondary = rl_err.is_secondary,
                attempts = *rate_limit_attempts,
                "github rate-limit retries exhausted"
            );
            return Err(Error::new(rl_err));
        }
        *rate_limit_attempts += 1;

        let planned_wait = rl_err.retry_after.unwrap_or(Duration::ZERO);
        let accrued = *cumulative_wait.lock().expect("cumulative wait lock not to be poisoned");
        if accrued.saturating_add(planned_wait) > self.policy.max_wait {
            warn!(
                ?inst_id,
                is_secondary = rl_err.is_secondary,
                ?planned_wait,
                cumulative_wait = ?accrued,
                max_wait = ?self.policy.max_wait,
                "github rate-limit wait exceeds budget; failing fast"
            );
            return Err(Error::new(rl_err));
        }

        debug!(
            ?inst_id,
            is_secondary = rl_err.is_secondary,
            attempt = *rate_limit_attempts,
            ?planned_wait,
            "retrying github request after rate limit"
        );
        Ok(())
    }

    /// Wait out transient backoff if retries and cumulative wait budget allow.
    async fn wait_transient_retry(
        &self,
        inst_id: Option<i64>,
        err: ClientError,
        transient_attempts: &mut u32,
        shutdown: &CancellationToken,
        cancel: &CancellationToken,
        cumulative_wait: &Mutex<Duration>,
    ) -> Result<()> {
        if *transient_attempts >= self.policy.max_transient_retries {
            return Err(err.into());
        }
        let backoff = self.transient_backoff(*transient_attempts);
        *transient_attempts += 1;

        let accrued = *cumulative_wait.lock().expect("cumulative wait lock not to be poisoned");
        if accrued.saturating_add(backoff) > self.policy.max_wait {
            return Err(err.into());
        }

        warn!(
            ?inst_id,
            attempt = *transient_attempts,
            ?backoff,
            %err,
            "retrying github request after transient error"
        );
        sleep_cancellable(backoff, shutdown, cancel).await?;
        add_cumulative_wait(cumulative_wait, backoff);
        Ok(())
    }

    /// Return any active primary or secondary cooldown wait for `inst_id`.
    #[must_use]
    pub fn active_cooldown(&self, inst_id: Option<i64>) -> Option<(Duration, bool)> {
        let now = Instant::now();
        let state = self.state.lock().expect("governor lock not to be poisoned");
        compute_pending_wait(&state, inst_id, false, now)
    }

    /// Return the configured policy.
    #[must_use]
    pub fn policy(&self) -> &RateLimitPolicy {
        &self.policy
    }

    /// Record a primary rate-limit deadline for the installation provided,
    /// extending (never shortening) any existing cooldown.
    pub fn record_primary_rate_limit(&self, inst_id: Option<i64>, wait: Duration) {
        let clamped_wait = wait.min(MAX_PARSED_RATE_LIMIT_WAIT);
        let Some(deadline) = Instant::now().checked_add(clamped_wait) else {
            return;
        };
        let mut state = self.state.lock().expect("governor lock not to be poisoned");
        state
            .installation_cooldowns
            .entry(inst_id)
            .and_modify(|existing| {
                if deadline > *existing {
                    *existing = deadline;
                }
            })
            .or_insert(deadline);
    }

    /// Record a secondary rate limit, updating the global cooldown with
    /// `max(existing, new)` and returning the computed wait duration.
    ///
    /// Concurrent responses belonging to the same active cooldown episode do
    /// not escalate `secondary_strikes` multiple times.
    pub fn record_secondary_rate_limit(
        &self,
        inst_id: Option<i64>,
        retry_after: Option<Duration>,
    ) -> Duration {
        // Compute jitter outside the lock
        let jitter = self.next_jitter();
        let now = Instant::now();

        // Advance the strike counter at most once per active cooldown episode
        let mut state = self.state.lock().expect("governor lock not to be poisoned");
        let active_remaining =
            state.global_cooldown_until.and_then(|deadline| deadline.checked_duration_since(now));
        let (strike, wait) = if let Some(remaining) = active_remaining {
            let strike = state.secondary_strikes;
            let wait = retry_after.map_or(remaining, |ra| ra.min(MAX_PARSED_RATE_LIMIT_WAIT).max(remaining));
            (strike, wait)
        } else {
            let strike = state.secondary_strikes;
            state.secondary_strikes = state.secondary_strikes.saturating_add(1);
            let wait = if let Some(retry_after) = retry_after {
                retry_after.min(MAX_PARSED_RATE_LIMIT_WAIT)
            } else {
                let exp_factor = 1u32.checked_shl(strike.min(4)).unwrap_or(16);
                self.policy
                    .secondary_base_backoff
                    .saturating_mul(exp_factor)
                    .saturating_add(jitter)
                    .min(MAX_PARSED_RATE_LIMIT_WAIT)
            };
            (strike, wait)
        };

        // Extend the global cooldown deadline without shortening or overflowing
        if let Some(deadline) = now.checked_add(wait) {
            match state.global_cooldown_until {
                Some(existing) if existing >= deadline => {}
                _ => state.global_cooldown_until = Some(deadline),
            }
        }

        warn!(
            ?inst_id,
            strike,
            ?wait,
            "github secondary rate limit recorded; activating global cooldown"
        );
        wait
    }

    /// Reset the consecutive secondary strike counter after a successful call
    /// dispatched after any prior secondary cooldown expired.
    pub fn record_success(&self, dispatched_at: Instant) {
        let mut state = self.state.lock().expect("governor lock not to be poisoned");
        if state.global_cooldown_until.is_none_or(|cooldown_until| dispatched_at >= cooldown_until) {
            state.secondary_strikes = 0;
        }
    }

    /// Wait for and atomically claim the next HTTP-level write pacing slot
    /// right before dispatching a mutative HTTP request (including `octorust`
    /// installation-token exchanges).
    pub(crate) async fn wait_for_write_slot(
        &self,
        shutdown: &CancellationToken,
        cancel: &CancellationToken,
        cumulative_wait: &Mutex<Duration>,
    ) -> Result<()> {
        if self.policy.mutation_interval.is_zero() {
            return Ok(());
        }

        loop {
            if shutdown.is_cancelled() || cancel.is_cancelled() {
                return Err(Error::new(CancelledError));
            }

            let wait = {
                let now = Instant::now();
                let mut state = self.state.lock().expect("governor lock not to be poisoned");
                if let Some(next_write) = state.next_write_at
                    && next_write > now
                {
                    Some(next_write - now)
                } else {
                    state.next_write_at = now.checked_add(self.policy.mutation_interval);
                    None
                }
            };

            let Some(wait) = wait else {
                return Ok(());
            };
            self.wait_within_budget(wait, true, shutdown, cancel, cumulative_wait).await?;
        }
    }

    /// Acquire an in-flight permit only after all cooldowns and write-pacing
    /// gates are clear immediately before dispatch.
    async fn acquire_admission(
        &self,
        inst_id: Option<i64>,
        is_write: bool,
        shutdown: &CancellationToken,
        cancel: &CancellationToken,
        cumulative_wait: &Mutex<Duration>,
    ) -> Result<tokio::sync::OwnedSemaphorePermit> {
        loop {
            if shutdown.is_cancelled() || cancel.is_cancelled() {
                return Err(Error::new(CancelledError));
            }

            // Wait out any currently published cooldown before taking a permit
            if let Some((wait, is_secondary)) = self.pending_wait(inst_id, is_write) {
                self.wait_within_budget(wait, is_secondary, shutdown, cancel, cumulative_wait).await?;
                continue;
            }

            // Acquire an in-flight permit bounded by the remaining wait budget
            let accrued = *cumulative_wait.lock().expect("cumulative wait lock not to be poisoned");
            let remaining_budget = self.policy.max_wait.saturating_sub(accrued);
            let wait_start = Instant::now();
            let permit = tokio::select! {
                biased;

                () = shutdown.cancelled() => return Err(Error::new(CancelledError)),
                () = cancel.cancelled() => return Err(Error::new(CancelledError)),
                result = tokio::time::timeout(remaining_budget, self.in_flight.clone().acquire_owned()) => {
                    match result {
                        Ok(permit) => permit.expect("in-flight semaphore not to be closed"),
                        Err(_) => {
                            return Err(Error::new(RateLimitError::secondary(
                                Some(remaining_budget),
                                format!(
                                    "waiting for in-flight permit exceeded remaining budget ({}s)",
                                    remaining_budget.as_secs()
                                ),
                            )));
                        }
                    }
                }
            };
            add_cumulative_wait(cumulative_wait, wait_start.elapsed());

            // Re-check cooldowns right before entering the operation
            match self.try_claim_dispatch_slot(inst_id, is_write) {
                Ok(()) => return Ok(permit),
                Err((wait, is_secondary)) => {
                    // Release the permit before sleeping so other orgs aren't starved
                    drop(permit);
                    self.wait_within_budget(wait, is_secondary, shutdown, cancel, cumulative_wait).await?;
                }
            }
        }
    }

    /// Classify an `octorust::ClientError` into rate-limit, transient,
    /// unauthorized, or fatal.
    fn classify_error(&self, inst_id: Option<i64>, kind: RequestKind, err: ClientError) -> ClassifiedError {
        // Check for a RateLimitError, CancelledError, or UnauthorizedError produced by GitHubMiddleware
        if let ClientError::ReqwestMiddleWareError(MiddlewareError::Middleware(ref inner)) = err {
            if let Some(rl_err) = inner.downcast_ref::<RateLimitError>() {
                return ClassifiedError::RateLimited(rl_err.clone());
            }
            if inner.downcast_ref::<UnauthorizedError>().is_some() {
                return ClassifiedError::Unauthorized(err);
            }
            if inner.downcast_ref::<CancelledError>().is_some() {
                return ClassifiedError::Fatal(err);
            }
        }

        // Octorust maps ANY non-2xx with remaining==0 (including 5xx) to
        // ClientError::RateLimited. Genuine 403/429 rate limits and 401s with
        // remaining==0 are already intercepted by GitHubMiddleware above, so
        // only retry here when the request kind allows transient retries
        // (never replay NonIdempotentWrite)
        if let ClientError::RateLimited { duration } = err {
            let wait = Duration::from_secs(duration).saturating_add(RESET_BUFFER);
            self.record_primary_rate_limit(inst_id, wait);
            if kind.retries_transient() {
                return ClassifiedError::RateLimited(RateLimitError::primary(
                    Some(wait),
                    format!("primary rate limit reached; reset in {}s", wait.as_secs()),
                ));
            }
            return ClassifiedError::Fatal(err);
        }

        // Check for 401 unauthorized, 403 body-only secondary rate limits, and 5xx/408
        if let ClientError::HttpError { status, ref error } = err {
            if status == StatusCode::UNAUTHORIZED {
                return ClassifiedError::Unauthorized(err);
            }
            if status == StatusCode::FORBIDDEN && is_secondary_rate_limit_body(error) {
                let wait = self.record_secondary_rate_limit(inst_id, None);
                return ClassifiedError::RateLimited(RateLimitError::secondary(Some(wait), error.clone()));
            }
            if status.is_server_error() || status == StatusCode::REQUEST_TIMEOUT {
                return ClassifiedError::Transient(err);
            }
            return ClassifiedError::Fatal(err);
        }

        // Check for transient reqwest transport errors
        if is_transient_client_error(&err) {
            return ClassifiedError::Transient(err);
        }

        ClassifiedError::Fatal(err)
    }

    /// Compute a small jitter duration bounded by `policy.jitter_max`.
    fn next_jitter(&self) -> Duration {
        let max_millis = u64::try_from(self.policy.jitter_max.as_millis()).unwrap_or(u64::MAX);
        if max_millis == 0 {
            return Duration::ZERO;
        }
        let seq = self.jitter_counter.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| u64::from(d.subsec_nanos()));
        let jitter_ms = nanos.wrapping_add(seq.wrapping_mul(397)) % max_millis;
        Duration::from_millis(jitter_ms)
    }

    /// Return the longest currently active wait (cooldown or write slot) if any.
    fn pending_wait(&self, inst_id: Option<i64>, is_write: bool) -> Option<(Duration, bool)> {
        let now = Instant::now();
        let state = self.state.lock().expect("governor lock not to be poisoned");
        compute_pending_wait(&state, inst_id, is_write, now)
    }

    /// Compute exponential backoff for transient 5xx/network retries.
    fn transient_backoff(&self, attempt: u32) -> Duration {
        let factor = 1u32.checked_shl(attempt.min(4)).unwrap_or(16);
        self.policy.transient_base_backoff.saturating_mul(factor)
    }

    /// Re-check all cooldowns and any active write-pacing window before
    /// entering the operation. Actual write-slot reservation happens at the
    /// HTTP dispatch boundary in [`GitHubMiddleware`] so token exchanges and
    /// subsequent writes are each paced accurately.
    fn try_claim_dispatch_slot(
        &self,
        inst_id: Option<i64>,
        is_write: bool,
    ) -> std::result::Result<(), (Duration, bool)> {
        let now = Instant::now();
        let mut state = self.state.lock().expect("governor lock not to be poisoned");

        // Retain `global_cooldown_until` even after expiry so `record_success`
        // can distinguish pre-cooldown in-flight successes from post-cooldown
        // probes; clean up expired per-installation cooldowns opportunistically
        if state.installation_cooldowns.get(&inst_id).is_some_and(|deadline| *deadline <= now) {
            state.installation_cooldowns.remove(&inst_id);
        }

        if let Some(wait) = compute_pending_wait(&state, inst_id, is_write, now) {
            return Err(wait);
        }

        Ok(())
    }

    /// Sleep for `wait` if it stays within the cumulative budget, else fail fast.
    async fn wait_within_budget(
        &self,
        wait: Duration,
        is_secondary: bool,
        shutdown: &CancellationToken,
        cancel: &CancellationToken,
        cumulative_wait: &Mutex<Duration>,
    ) -> Result<()> {
        // Reject waits that would exceed the operation's cumulative wait budget
        let accrued = *cumulative_wait.lock().expect("cumulative wait lock not to be poisoned");
        if accrued.saturating_add(wait) > self.policy.max_wait {
            let msg = format!(
                "required wait ({}s) exceeds remaining budget ({}s)",
                wait.as_secs(),
                self.policy.max_wait.saturating_sub(accrued).as_secs()
            );
            let err = if is_secondary {
                RateLimitError::secondary(Some(wait), msg)
            } else {
                RateLimitError::primary(Some(wait), msg)
            };
            return Err(Error::new(err));
        }

        // Sleep cancellably and account for elapsed wait
        sleep_cancellable(wait, shutdown, cancel).await?;
        add_cumulative_wait(cumulative_wait, wait);
        Ok(())
    }
}

/// Mutable state protected by [`RateLimitGovernor::state`].
#[derive(Default)]
struct GovernorState {
    /// Per-installation cooldown deadlines triggered by primary rate limits.
    installation_cooldowns: HashMap<Option<i64>, Instant>,
    /// Consecutive secondary rate-limit hits without an intervening success.
    secondary_strikes: u32,

    /// Global cooldown deadline triggered by secondary rate limits.
    global_cooldown_until: Option<Instant>,
    /// Earliest instant at which the next mutative request may be dispatched.
    next_write_at: Option<Instant>,
}

/// Reqwest middleware that appends `per_page=100` on paginated GitHub REST
/// endpoints and translates rate-limit response headers into governor state.
pub(crate) struct GitHubMiddleware {
    /// Shared governor updated when rate-limit headers are observed.
    governor: Arc<RateLimitGovernor>,
    /// Process-wide shutdown token checked before dispatching paginated follow-ups.
    shutdown_token: CancellationToken,

    /// Installation identifier associated with this client, when applicable.
    inst_id: Option<i64>,
}

impl GitHubMiddleware {
    /// Create a new middleware bound to the governor, shutdown token, and
    /// installation provided.
    pub(crate) fn new(
        governor: Arc<RateLimitGovernor>,
        shutdown_token: CancellationToken,
        inst_id: Option<i64>,
    ) -> Self {
        Self {
            governor,
            shutdown_token,
            inst_id,
        }
    }

    /// Reject dispatch if shutdown, per-call cancellation, or an active
    /// cooldown is currently in effect.
    fn check_dispatch_gates(&self) -> reqwest_middleware::Result<()> {
        let call_cancelled = CALL_CONTEXT.try_with(|ctx| ctx.cancel.is_cancelled()).unwrap_or(false);
        if self.shutdown_token.is_cancelled() || call_cancelled {
            return Err(MiddlewareError::Middleware(Error::new(CancelledError)));
        }
        if let Some((wait, is_secondary)) = self.governor.active_cooldown(self.inst_id) {
            let err = if is_secondary {
                RateLimitError::secondary(
                    Some(wait),
                    format!("active secondary cooldown ({}s remaining)", wait.as_secs()),
                )
            } else {
                RateLimitError::primary(
                    Some(wait),
                    format!("active primary cooldown ({}s remaining)", wait.as_secs()),
                )
            };
            return Err(MiddlewareError::Middleware(Error::new(err)));
        }
        Ok(())
    }

    /// Record cooldown state for a rate-limited HTTP response and build a typed
    /// [`RateLimitError`].
    fn handle_rate_limit_response(
        &self,
        status: StatusCode,
        rate_headers: &RateLimitHeaders,
        default_secondary: bool,
    ) -> RateLimitError {
        // Primary rate limit when remaining == 0 and reset is known
        if rate_headers.remaining == Some(0)
            && let Some(reset_wait) = rate_headers.reset_wait
        {
            let wait = rate_headers.retry_after.map_or(reset_wait, |retry_after| retry_after.max(reset_wait));
            warn!(
                inst_id = ?self.inst_id,
                status = %status,
                resource = ?rate_headers.resource,
                ?wait,
                "github primary rate limit hit"
            );
            self.governor.record_primary_rate_limit(self.inst_id, wait);
            return RateLimitError::primary(
                Some(wait),
                format!("HTTP {status}; primary rate limit resets in {}s", wait.as_secs()),
            );
        }

        // Secondary rate limit (Retry-After header or 429 without remaining=0)
        if rate_headers.retry_after.is_some() || default_secondary {
            let wait = self.governor.record_secondary_rate_limit(self.inst_id, rate_headers.retry_after);
            return RateLimitError::secondary(
                Some(wait),
                format!("HTTP {status}; secondary rate limit wait {}s", wait.as_secs()),
            );
        }

        let wait = self.governor.record_secondary_rate_limit(self.inst_id, None);
        RateLimitError::secondary(Some(wait), format!("HTTP {status}"))
    }
}

#[async_trait]
impl Middleware for GitHubMiddleware {
    async fn handle(
        &self,
        mut req: Request,
        extensions: &mut Extensions,
        next: Next<'_>,
    ) -> reqwest_middleware::Result<Response> {
        // Reject requests if shutdown or a cooldown became active before dispatch
        self.check_dispatch_gates()?;

        // Pace every mutative HTTP request right at the HTTP dispatch boundary
        // (including octorust installation-token POST exchanges)
        if is_mutative_method(req.method()) {
            let (call_cancel, call_wait) = CALL_CONTEXT
                .try_with(|ctx| (ctx.cancel.clone(), ctx.cumulative_wait.clone()))
                .unwrap_or_else(|_| (CancellationToken::new(), Arc::new(Mutex::new(Duration::ZERO))));
            self.governor
                .wait_for_write_slot(&self.shutdown_token, &call_cancel, &call_wait)
                .await
                .map_err(MiddlewareError::Middleware)?;

            // Re-check shutdown and cooldowns in case they activated during wait
            self.check_dispatch_gates()?;
        }

        // Append per_page=100 to paginated list endpoints when not already set
        if req.method() == Method::GET
            && is_paginated_list_path(req.url().path())
            && !has_query_param(req.url(), "per_page")
        {
            req.url_mut().query_pairs_mut().append_pair("per_page", DEFAULT_PER_PAGE);
        }

        // Send the HTTP request
        let response = next.run(req, extensions).await?;
        let status = response.status();
        let headers = response.headers();
        let rate_headers = parse_rate_limit_headers(headers);

        // Handle successful responses, proactively recording exhausted buckets
        if status.is_success() {
            if rate_headers.remaining == Some(0) {
                if let Some(wait) = rate_headers.reset_wait {
                    warn!(
                        inst_id = ?self.inst_id,
                        resource = ?rate_headers.resource,
                        ?wait,
                        "github primary rate limit exhausted on successful response"
                    );
                    self.governor.record_primary_rate_limit(self.inst_id, wait);
                }
            } else if let Some(remaining) = rate_headers.remaining
                && remaining < self.governor.policy().low_remaining_threshold
            {
                warn!(
                    inst_id = ?self.inst_id,
                    remaining,
                    resource = ?rate_headers.resource,
                    "github primary rate limit remaining is low"
                );
            }
            return Ok(response);
        }

        // Handle 429 Too Many Requests (primary or secondary rate limit)
        if status == StatusCode::TOO_MANY_REQUESTS {
            let rl_err = self.handle_rate_limit_response(status, &rate_headers, true);
            return Err(MiddlewareError::Middleware(Error::new(rl_err)));
        }

        // Handle 403 Forbidden only when headers prove a rate limit; leave
        // body-only secondary rate limits and genuine 403s for octorust/execute
        if status == StatusCode::FORBIDDEN
            && (rate_headers.retry_after.is_some()
                || (rate_headers.remaining == Some(0) && rate_headers.reset_wait.is_some()))
        {
            let rl_err = self.handle_rate_limit_response(status, &rate_headers, false);
            return Err(MiddlewareError::Middleware(Error::new(rl_err)));
        }

        // Never attribute unauthenticated 401 rate-limit headers (such as the
        // 60/hr IP bucket) to the installation's primary quota; when
        // remaining==0, intercept 401 before octorust maps it to
        // ClientError::RateLimited and discards the 401 status
        if status == StatusCode::UNAUTHORIZED {
            if rate_headers.remaining == Some(0) {
                return Err(MiddlewareError::Middleware(Error::new(UnauthorizedError)));
            }
            return Ok(response);
        }

        // Record exhausted primary quota even on non-403/429 error responses
        if rate_headers.remaining == Some(0)
            && let Some(wait) = rate_headers.reset_wait
        {
            self.governor.record_primary_rate_limit(self.inst_id, wait);
        }

        Ok(response)
    }
}

/// Internal classification of an `octorust::ClientError`.
enum ClassifiedError {
    /// Non-retryable error returned immediately to the caller.
    Fatal(ClientError),
    /// Primary or secondary rate limit error.
    RateLimited(RateLimitError),
    /// Transient server or network error retryable on idempotent requests.
    Transient(ClientError),
    /// HTTP 401 Unauthorized error eligible for a single credential refresh.
    Unauthorized(ClientError),
}

/// Parsed GitHub rate-limit headers from an HTTP response.
struct RateLimitHeaders {
    /// Remaining primary requests in the current window (`x-ratelimit-remaining`).
    remaining: Option<u32>,
    /// Computed wait until the primary window resets (`x-ratelimit-reset`).
    reset_wait: Option<Duration>,
    /// Rate-limit resource family (`x-ratelimit-resource`).
    resource: Option<String>,
    /// Server-requested wait duration (`retry-after`).
    retry_after: Option<Duration>,
}

/// Return true if the anyhow error chain (including nested [`MultiError`]s)
/// contains a [`CancelledError`].
#[must_use]
pub fn is_cancelled_error(err: &Error) -> bool {
    err.chain().any(|cause| {
        cause.is::<CancelledError>()
            || cause
                .downcast_ref::<crate::multierror::MultiError>()
                .is_some_and(|merr| merr.errors().iter().any(|inner| is_cancelled_error(inner)))
    })
}

/// Return true if the anyhow error chain (including nested [`MultiError`]s)
/// contains a [`RateLimitError`].
#[must_use]
pub fn is_rate_limit_error(err: &Error) -> bool {
    err.chain().any(|cause| {
        cause.is::<RateLimitError>()
            || cause
                .downcast_ref::<crate::multierror::MultiError>()
                .is_some_and(|merr| merr.errors().iter().any(|inner| is_rate_limit_error(inner)))
    })
}

/// Return true if `path` matches one of the paginated GitHub REST list
/// endpoints used by CLOWarden.
pub(crate) fn is_paginated_list_path(path: &str) -> bool {
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    matches!(
        segments.as_slice(),
        ["orgs", _, "members" | "repos" | "teams"]
            | ["orgs", _, "teams", _, "invitations" | "members"]
            | ["repos", _, _, "collaborators" | "invitations" | "teams"]
            | ["repos", _, _, "commits", _, "pulls"]
            | ["repos", _, _, "pulls", _, "files"]
    )
}

// Helpers.

/// Add `delta` to the shared cumulative wait duration.
fn add_cumulative_wait(cumulative_wait: &Mutex<Duration>, delta: Duration) {
    let mut guard = cumulative_wait.lock().expect("cumulative wait lock not to be poisoned");
    *guard = guard.saturating_add(delta);
}

/// Compute the longest active wait across global cooldown, installation
/// cooldown, and (for writes) mutation pacing.
fn compute_pending_wait(
    state: &GovernorState,
    inst_id: Option<i64>,
    is_write: bool,
    now: Instant,
) -> Option<(Duration, bool)> {
    let mut best: Option<(Duration, bool)> = None;

    // Global secondary rate-limit cooldown
    if let Some(deadline) = state.global_cooldown_until
        && deadline > now
    {
        best = Some((deadline - now, true));
    }

    // Per-installation primary rate-limit cooldown
    if let Some(&deadline) = state.installation_cooldowns.get(&inst_id)
        && deadline > now
    {
        let wait = deadline - now;
        if best.is_none_or(|(current, _)| wait > current) {
            best = Some((wait, false));
        }
    }

    // Mutative request spacing
    if is_write
        && let Some(next_write) = state.next_write_at
        && next_write > now
    {
        let wait = next_write - now;
        if best.is_none_or(|(current, _)| wait > current) {
            best = Some((wait, true));
        }
    }

    best
}

/// Return true if `url` already contains a query parameter named `name`.
fn has_query_param(url: &Url, name: &str) -> bool {
    url.query_pairs().any(|(k, _)| k == name)
}

/// Return true if an HTTP method mutates state on GitHub and must be spaced.
fn is_mutative_method(method: &Method) -> bool {
    matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    )
}

/// Return true if a 403 response body indicates a GitHub secondary rate limit.
fn is_secondary_rate_limit_body(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("secondary rate limit") || lower.contains("abuse detection")
}

/// Return true if a `ClientError` represents a transient transport failure.
fn is_transient_client_error(err: &ClientError) -> bool {
    // Extract the underlying reqwest error, if any
    let reqwest_err = match err {
        ClientError::ReqwestError(e) | ClientError::ReqwestMiddleWareError(MiddlewareError::Reqwest(e)) => {
            Some(e)
        }
        _ => None,
    };
    let Some(e) = reqwest_err else {
        return false;
    };

    // Treat connection and timeout failures as transient
    if e.is_timeout() || e.is_connect() {
        return true;
    }

    // Detect truncated hyper responses where the peer closed mid-body
    if e.is_request() {
        let mut source = e.source();
        while let Some(cause) = source {
            if cause.to_string().contains("connection closed before message completed") {
                return true;
            }
            source = cause.source();
        }
    }
    false
}

/// Parse GitHub's rate-limit headers from an HTTP response.
fn parse_rate_limit_headers(headers: &HeaderMap) -> RateLimitHeaders {
    // Parse remaining primary quota count
    let remaining = headers
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u32>().ok());

    // Convert reset UTC epoch seconds into a relative wait with safety buffer
    let reset_wait = headers
        .get("x-ratelimit-reset")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|reset_epoch| {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
            Duration::from_secs(reset_epoch.saturating_sub(now))
                .saturating_add(RESET_BUFFER)
                .min(MAX_PARSED_RATE_LIMIT_WAIT)
        });

    // Extract rate-limit resource family and optional Retry-After header
    let resource = headers.get("x-ratelimit-resource").and_then(|v| v.to_str().ok()).map(ToString::to_string);
    let retry_after = headers.get("retry-after").and_then(|v| v.to_str().ok()).and_then(parse_retry_after);

    RateLimitHeaders {
        remaining,
        reset_wait,
        resource,
        retry_after,
    }
}

/// Parse a `Retry-After` header value as either integer seconds or RFC 2822
/// HTTP-date, capped at [`MAX_PARSED_RATE_LIMIT_WAIT`].
fn parse_retry_after(raw: &str) -> Option<Duration> {
    let trimmed = raw.trim();
    if let Ok(seconds) = trimmed.parse::<u64>() {
        return Some(Duration::from_secs(seconds).min(MAX_PARSED_RATE_LIMIT_WAIT));
    }
    if let Ok(dt) = OffsetDateTime::parse(trimmed, &Rfc2822) {
        let now = OffsetDateTime::now_utc();
        let diff = dt - now;
        let secs = u64::try_from(diff.whole_seconds().max(0)).unwrap_or(0);
        return Some(Duration::from_secs(secs).min(MAX_PARSED_RATE_LIMIT_WAIT));
    }
    None
}

/// Sleep for `duration`, returning [`CancelledError`] immediately if either
/// `shutdown` or `cancel` fires.
async fn sleep_cancellable(
    duration: Duration,
    shutdown: &CancellationToken,
    cancel: &CancellationToken,
) -> Result<()> {
    if shutdown.is_cancelled() || cancel.is_cancelled() {
        return Err(Error::new(CancelledError));
    }
    if duration.is_zero() {
        return Ok(());
    }
    tokio::select! {
        biased;

        () = shutdown.cancelled() => Err(Error::new(CancelledError)),
        () = cancel.cancelled() => Err(Error::new(CancelledError)),
        () = sleep(duration) => Ok(()),
    }
}

#[cfg(test)]
mod tests;
