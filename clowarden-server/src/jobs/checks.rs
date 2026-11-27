//! Check-run publication with rate-limit retry deferral and supersession checks.

use std::time::Duration;

use anyhow::Result;
use octorust::types::{ChecksCreateRequest, ChecksCreateRequestConclusion, JobStatus};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

use clowarden_core::{cfg::Organization, github::is_rate_limit_error};

use crate::github::{self, Ctx, DynGH};

use super::{
    DeferredRetries, JobsContext, LatestPrHeads, bounded_rate_limit_delay, deadline_after,
    rate_limit_retry_after, sleep_until_or_cancel,
};

/// Maximum number of deferred retries when publishing a computed check run
/// hits a GitHub rate limit.
const MAX_CHECK_PUBLISH_RETRIES: u32 = 2;

/// Shared dependencies used to publish PR check runs with rate-limit retry
/// deferral and supersession tracking.
#[derive(Clone)]
pub(super) struct CheckRunPublisher {
    /// Tracker for deferred rate-limited check publication retries.
    pub(super) deferred_retries: DeferredRetries,
    /// Server-side GitHub API client.
    pub(super) gh: DynGH,
    /// Shared tracker of latest head SHAs per PR.
    pub(super) latest_pr_heads: LatestPrHeads,
    /// Cancellation token signalling intake stop.
    pub(super) stop_intake: CancellationToken,
    /// Cancellation token aborting underlying GitHub HTTP waits on final exit.
    pub(super) transport_cancel: CancellationToken,
}

impl CheckRunPublisher {
    /// Complete an interrupted validation check run as failed (fail-closed) so
    /// unvalidated changes cannot satisfy required checks, deferring
    /// publication if an active rate-limit cooldown prevents immediate delivery.
    pub(super) async fn complete_interrupted(&self, target: &CheckRunTarget<'_>, msg: &str) {
        let check_body = github::new_checks_create_request(
            target.head_sha.to_string(),
            Some(JobStatus::Completed),
            Some(ChecksCreateRequestConclusion::Failure),
            msg,
        );
        if let Err(check_err) = self.publish_with_retry(target, check_body).await {
            error!(?check_err, "error completing interrupted validation check run");
        }
    }

    /// Publish a computed check run, deferring publication on rate-limit errors
    /// so a finished validation verdict is not lost (without re-running
    /// validation or duplicating comments). Tracks `attempt_generation` so a
    /// delayed publication can never supersede a newer rerun on the same SHA.
    pub(super) async fn publish_with_retry(
        &self,
        target: &CheckRunTarget<'_>,
        check_body: ChecksCreateRequest,
    ) -> Result<()> {
        // Attempt immediate publication first, deferring only on active rate limits
        let ctx = Ctx::from_org_with_cancel(target.org, self.transport_cancel.clone());
        match self.gh.create_check_run(&ctx, &check_body).await {
            Ok(()) => Ok(()),
            Err(err) if is_rate_limit_error(&err) && !self.stop_intake.is_cancelled() => {
                let initial_delay = bounded_rate_limit_delay(rate_limit_retry_after(&err));
                warn!(
                    org = %target.org.name,
                    pr_number = target.pr_number,
                    head_sha = target.head_sha,
                    ?initial_delay,
                    %err,
                    "deferring rate-limited check run publication"
                );
                self.spawn_deferred_publish(target, check_body, initial_delay);
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    /// Spawn a background task to retry publishing a rate-limited check run
    /// after cooldowns unless superseded or interrupted by shutdown.
    fn spawn_deferred_publish(
        &self,
        target: &CheckRunTarget<'_>,
        check_body: ChecksCreateRequest,
        initial_delay: Duration,
    ) {
        // Capture owned target metadata and shared handles for the retry task
        let publisher = self.clone();
        let org = target.org.clone();
        let pr_number = target.pr_number;
        let head_sha = target.head_sha.to_string();
        let attempt_generation = target.attempt_generation;

        // Retry check publication after cooldowns unless superseded by a newer
        // commit, a newer rerun generation, or shutdown
        let handle = tokio::spawn(async move {
            let mut delay = initial_delay;
            for attempt in 1..=MAX_CHECK_PUBLISH_RETRIES {
                if sleep_until_or_cancel(deadline_after(delay), &publisher.stop_intake).await {
                    return;
                }
                if publisher.latest_pr_heads.is_superseded_attempt(
                    &org.name,
                    pr_number,
                    &head_sha,
                    attempt_generation,
                ) {
                    return;
                }
                let ctx = Ctx::from_org_with_cancel(&org, publisher.transport_cancel.clone());
                match publisher.gh.create_check_run(&ctx, &check_body).await {
                    Ok(()) => {
                        debug!(
                            org = %org.name,
                            pr_number,
                            head_sha,
                            attempt,
                            "published deferred check run"
                        );
                        return;
                    }
                    Err(retry_err) if is_rate_limit_error(&retry_err) => {
                        delay = bounded_rate_limit_delay(rate_limit_retry_after(&retry_err));
                        warn!(
                            org = %org.name,
                            pr_number,
                            head_sha,
                            attempt,
                            ?delay,
                            %retry_err,
                            "deferred check run publication still rate-limited"
                        );
                    }
                    Err(retry_err) => {
                        error!(
                            ?retry_err,
                            org = %org.name,
                            pr_number,
                            head_sha,
                            "error publishing deferred check run"
                        );
                        return;
                    }
                }
            }
        });
        self.deferred_retries.push(handle);
    }
}

impl From<&JobsContext> for CheckRunPublisher {
    fn from(ctx: &JobsContext) -> Self {
        Self {
            deferred_retries: ctx.deferred_retries.clone(),
            gh: ctx.gh.clone(),
            latest_pr_heads: ctx.latest_pr_heads.clone(),
            stop_intake: ctx.stop_intake.clone(),
            transport_cancel: ctx.transport_cancel.clone(),
        }
    }
}

/// Pull request commit target for a check run publication attempt.
pub(super) struct CheckRunTarget<'a> {
    /// Head commit SHA targeted by the check run.
    pub(super) head_sha: &'a str,
    /// Target organization configuration.
    pub(super) org: &'a Organization,
    /// Pull request number.
    pub(super) pr_number: i64,

    /// Attempt generation for supersession checks, when tracked.
    pub(super) attempt_generation: Option<u64>,
}
