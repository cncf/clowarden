use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use anyhow::{Error, format_err};
use octorust::ClientError;
use reqwest::StatusCode;
use time::{OffsetDateTime, format_description::well_known::Rfc2822};
use tokio::{sync::Notify, time::Instant};
use tokio_util::sync::CancellationToken;

use crate::multierror::MultiError;

use super::*;

const INSTALLATION_ID: i64 = 1;
const OTHER_INSTALLATION_ID: i64 = 2;

#[tokio::test]
async fn test_governor_blocks_all_installations_during_global_secondary_cooldown() {
    // Setup governor with a short wait budget and an active secondary cooldown
    let policy = RateLimitPolicy {
        max_wait: Duration::from_secs(5),
        ..fast_policy()
    };
    let governor = RateLimitGovernor::new(policy);
    governor.record_secondary_rate_limit(Some(INSTALLATION_ID), Some(Duration::from_secs(30)));

    // Execute a read for a DIFFERENT installation; it must fail fast without running
    let calls = AtomicUsize::new(0);
    let cancel = CancellationToken::new();
    let shutdown = CancellationToken::new();
    let err = governor
        .execute(
            Some(OTHER_INSTALLATION_ID),
            RequestKind::Read,
            &shutdown,
            &cancel,
            || {
                calls.fetch_add(1, Ordering::Relaxed);
                async { Ok::<(), ClientError>(()) }
            },
        )
        .await
        .unwrap_err();

    // Check the global secondary cooldown blocked the other installation
    assert!(is_rate_limit_error(&err));
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

#[test]
fn test_governor_counts_burst_of_secondary_limits_as_single_strike() {
    // Setup governor with 10s base backoff and zero jitter
    let policy = RateLimitPolicy {
        jitter_max: Duration::ZERO,
        secondary_base_backoff: Duration::from_secs(10),
        ..RateLimitPolicy::default()
    };
    let governor = RateLimitGovernor::new(policy);

    // Record four secondary hits in the same active cooldown episode
    let first = governor.record_secondary_rate_limit(Some(INSTALLATION_ID), None);
    let second = governor.record_secondary_rate_limit(Some(INSTALLATION_ID), None);
    let third = governor.record_secondary_rate_limit(Some(INSTALLATION_ID), None);
    let fourth = governor.record_secondary_rate_limit(Some(INSTALLATION_ID), None);

    // Check strikes did not escalate 10s -> 20s -> 40s -> 80s within one burst
    assert_eq!(first, Duration::from_secs(10));
    assert!(second <= Duration::from_secs(10));
    assert!(third <= Duration::from_secs(10));
    assert!(fourth <= Duration::from_secs(10));
}

#[tokio::test]
async fn test_governor_enforces_max_in_flight_and_fails_when_permit_wait_exceeds_budget() {
    // Setup governor allowing only 1 in-flight operation and a short wait budget
    let policy = RateLimitPolicy {
        max_in_flight: 1,
        max_wait: Duration::from_millis(60),
        ..fast_policy()
    };
    let governor = Arc::new(RateLimitGovernor::new(policy));
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());

    // Start first operation holding the sole in-flight permit
    let first_gov = governor.clone();
    let first_entered = entered.clone();
    let first_release = release.clone();
    let first_task = tokio::spawn(async move {
        let cancel = CancellationToken::new();
        let shutdown = CancellationToken::new();
        first_gov
            .execute(
                Some(INSTALLATION_ID),
                RequestKind::Read,
                &shutdown,
                &cancel,
                || {
                    let entered = first_entered.clone();
                    let release = first_release.clone();
                    async move {
                        entered.notify_one();
                        release.notified().await;
                        Ok::<(), ClientError>(())
                    }
                },
            )
            .await
    });
    entered.notified().await;

    // Attempt a second concurrent operation while the permit is held
    let second_calls = AtomicUsize::new(0);
    let cancel = CancellationToken::new();
    let shutdown = CancellationToken::new();
    let err = governor
        .execute(
            Some(OTHER_INSTALLATION_ID),
            RequestKind::Read,
            &shutdown,
            &cancel,
            || {
                second_calls.fetch_add(1, Ordering::Relaxed);
                async { Ok::<(), ClientError>(()) }
            },
        )
        .await
        .unwrap_err();

    // Check the second operation timed out waiting for the permit without executing
    assert!(is_rate_limit_error(&err));
    assert_eq!(second_calls.load(Ordering::Relaxed), 0);

    // Release the first operation and verify it completes cleanly
    release.notify_one();
    first_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn test_governor_exhausts_rate_limit_retries_after_max_attempts() {
    // Setup governor allowing 2 rate-limit retries with minimal backoff
    let policy = RateLimitPolicy {
        max_rate_limit_retries: 2,
        secondary_base_backoff: Duration::from_millis(5),
        ..fast_policy()
    };
    let governor = RateLimitGovernor::new(policy);

    // Always fail with a secondary rate-limit 403
    let calls = AtomicUsize::new(0);
    let cancel = CancellationToken::new();
    let shutdown = CancellationToken::new();
    let err = governor
        .execute(
            Some(INSTALLATION_ID),
            RequestKind::Read,
            &shutdown,
            &cancel,
            || {
                calls.fetch_add(1, Ordering::Relaxed);
                async {
                    Err::<(), ClientError>(ClientError::HttpError {
                        status: StatusCode::FORBIDDEN,
                        error: "You have exceeded a secondary rate limit".to_string(),
                    })
                }
            },
        )
        .await
        .unwrap_err();

    // Check 1 initial attempt + 2 retries ran before returning RateLimitError
    assert!(is_rate_limit_error(&err));
    assert_eq!(calls.load(Ordering::Relaxed), 3);
}

#[tokio::test]
async fn test_governor_exhausts_transient_retries_after_max_attempts() {
    // Setup governor allowing 2 transient retries with minimal backoff
    let policy = RateLimitPolicy {
        max_transient_retries: 2,
        transient_base_backoff: Duration::from_millis(5),
        ..fast_policy()
    };
    let governor = RateLimitGovernor::new(policy);

    // Always fail with a transient 502 error
    let calls = AtomicUsize::new(0);
    let cancel = CancellationToken::new();
    let shutdown = CancellationToken::new();
    let err = governor
        .execute(
            Some(INSTALLATION_ID),
            RequestKind::Read,
            &shutdown,
            &cancel,
            || {
                calls.fetch_add(1, Ordering::Relaxed);
                async {
                    Err::<(), ClientError>(ClientError::HttpError {
                        status: StatusCode::BAD_GATEWAY,
                        error: "bad gateway".to_string(),
                    })
                }
            },
        )
        .await
        .unwrap_err();

    // Check 1 initial attempt + 2 retries ran before returning the 502 error
    assert!(!is_rate_limit_error(&err));
    assert_eq!(calls.load(Ordering::Relaxed), 3);
}

#[tokio::test]
async fn test_governor_pre_cooldown_success_does_not_reset_secondary_strikes() {
    // Setup governor with short base backoff, zero jitter, and no auto-retry
    let policy = RateLimitPolicy {
        jitter_max: Duration::ZERO,
        max_rate_limit_retries: 0,
        secondary_base_backoff: Duration::from_millis(40),
        ..RateLimitPolicy::default()
    };
    let governor = RateLimitGovernor::new(policy);

    // Record a pre-cooldown dispatch timestamp, then hit a secondary limit
    let pre_cooldown_dispatch = Instant::now();
    let first_wait = governor.record_secondary_rate_limit(Some(INSTALLATION_ID), None);
    assert_eq!(first_wait, Duration::from_millis(40));

    // Wait for cooldown to expire and admit a post-cooldown probe via execute
    tokio::time::sleep(Duration::from_millis(55)).await;
    let cancel = CancellationToken::new();
    let shutdown = CancellationToken::new();
    let _ = governor
        .execute(
            Some(INSTALLATION_ID),
            RequestKind::Read,
            &shutdown,
            &cancel,
            || async {
                // Simulate a late pre-cooldown request succeeding AFTER the probe
                // was admitted (while the probe itself has not succeeded yet)
                governor.record_success(pre_cooldown_dispatch);
                Err::<(), ClientError>(ClientError::HttpError {
                    status: StatusCode::FORBIDDEN,
                    error: "You have exceeded a secondary rate limit".to_string(),
                })
            },
        )
        .await;

    // A subsequent secondary hit must still see escalated strike history (2x)
    let (remaining, is_secondary) = governor.active_cooldown(Some(INSTALLATION_ID)).unwrap();
    assert!(is_secondary);
    assert!(
        remaining > Duration::from_millis(45),
        "expected doubled secondary backoff (>45ms), got {remaining:?}"
    );
}

#[tokio::test]
async fn test_governor_resets_secondary_strikes_after_post_cooldown_success() {
    // Setup governor with short base backoff and zero jitter
    let policy = RateLimitPolicy {
        jitter_max: Duration::ZERO,
        secondary_base_backoff: Duration::from_millis(30),
        ..fast_policy()
    };
    let governor = RateLimitGovernor::new(policy);

    // Trigger first secondary cooldown episode and wait for it to expire
    let first_wait = governor.record_secondary_rate_limit(Some(INSTALLATION_ID), None);
    assert_eq!(first_wait, Duration::from_millis(30));
    tokio::time::sleep(Duration::from_millis(45)).await;

    // Complete a successful call dispatched after the cooldown expired
    let cancel = CancellationToken::new();
    let shutdown = CancellationToken::new();
    governor
        .execute(
            Some(INSTALLATION_ID),
            RequestKind::Read,
            &shutdown,
            &cancel,
            || async { Ok::<(), ClientError>(()) },
        )
        .await
        .unwrap();

    // A later secondary hit must start again from the 1x base backoff
    let next_wait = governor.record_secondary_rate_limit(Some(INSTALLATION_ID), None);
    assert_eq!(next_wait, Duration::from_millis(30));
}

#[test]
fn test_is_cancelled_error_detects_direct_and_nested_multierror() {
    // Setup direct, contextualized, nested MultiError, and unrelated errors
    let direct = Error::new(CancelledError);
    let contextualized = Error::new(CancelledError).context("outer context");
    let mut inner_merr = MultiError::new(Some("inner".to_string()));
    inner_merr.push(format_err!("other problem"));
    inner_merr.push(Error::new(CancelledError));
    let mut outer_merr = MultiError::new(Some("outer".to_string()));
    outer_merr.push(Error::from(inner_merr));
    let unrelated = format_err!("not cancelled");

    // Check detection across all shapes
    assert!(is_cancelled_error(&direct));
    assert!(is_cancelled_error(&contextualized));
    assert!(is_cancelled_error(&Error::from(outer_merr)));
    assert!(!is_cancelled_error(&unrelated));
}

#[test]
fn test_is_paginated_list_path_matches_expected_routes() {
    assert!(is_paginated_list_path("/orgs/cncf/repos"));
    assert!(is_paginated_list_path("/orgs/cncf/members"));
    assert!(is_paginated_list_path("/orgs/cncf/teams"));
    assert!(is_paginated_list_path("/orgs/cncf/teams/t1/members"));
    assert!(is_paginated_list_path("/orgs/cncf/teams/t1/invitations"));
    assert!(is_paginated_list_path("/repos/cncf/r1/collaborators"));
    assert!(is_paginated_list_path("/repos/cncf/r1/invitations"));
    assert!(is_paginated_list_path("/repos/cncf/r1/teams"));
    assert!(is_paginated_list_path("/repos/cncf/r1/commits/abc123/pulls"));
    assert!(is_paginated_list_path("/repos/cncf/r1/pulls/42/files"));

    assert!(!is_paginated_list_path("/repos/cncf/r1/contents/teams"));
    assert!(!is_paginated_list_path("/users/alice"));
    assert!(!is_paginated_list_path("/orgs/cncf/teams/t1/memberships/alice"));
}

#[test]
fn test_is_rate_limit_error_detects_direct_and_nested_multierror() {
    // Setup direct, contextualized, nested MultiError, and unrelated errors
    let direct = Error::new(RateLimitError::primary(Some(Duration::from_secs(60)), "primary"));
    let contextualized = Error::new(RateLimitError::secondary(None, "secondary")).context("outer context");
    let mut inner_merr = MultiError::new(Some("inner".to_string()));
    inner_merr.push(format_err!("other problem"));
    inner_merr.push(Error::new(RateLimitError::secondary(None, "secondary")));
    let mut outer_merr = MultiError::new(Some("outer".to_string()));
    outer_merr.push(Error::from(inner_merr));
    let unrelated = format_err!("not rate limited");

    // Check detection across all shapes
    assert!(is_rate_limit_error(&direct));
    assert!(is_rate_limit_error(&contextualized));
    assert!(is_rate_limit_error(&Error::from(outer_merr)));
    assert!(!is_rate_limit_error(&unrelated));
}

#[test]
fn test_parse_retry_after_parses_seconds_http_date_and_clamps_overflow() {
    // Parse integer seconds and overflow clamping
    assert_eq!(parse_retry_after(" 120 "), Some(Duration::from_secs(120)));
    assert_eq!(parse_retry_after("0"), Some(Duration::ZERO));
    assert_eq!(
        parse_retry_after(&u64::MAX.to_string()),
        Some(MAX_PARSED_RATE_LIMIT_WAIT)
    );

    // Parse a future RFC 2822 HTTP-date
    let future_date = (OffsetDateTime::now_utc() + time::Duration::seconds(90)).format(&Rfc2822).unwrap();
    let parsed_future = parse_retry_after(&future_date).unwrap();
    assert!(
        parsed_future >= Duration::from_secs(85) && parsed_future <= Duration::from_secs(91),
        "unexpected future Retry-After duration: {parsed_future:?}"
    );

    // Parse a far-future HTTP-date (clamped) and a past HTTP-date (zero)
    let far_future = (OffsetDateTime::now_utc() + time::Duration::days(30)).format(&Rfc2822).unwrap();
    assert_eq!(parse_retry_after(&far_future), Some(MAX_PARSED_RATE_LIMIT_WAIT));
    let past_date = (OffsetDateTime::now_utc() - time::Duration::seconds(30)).format(&Rfc2822).unwrap();
    assert_eq!(parse_retry_after(&past_date), Some(Duration::ZERO));

    // Reject unparseable values
    assert_eq!(parse_retry_after("not-a-date"), None);
}

// Helpers.

/// Return a fast deterministic policy suitable for unit tests.
fn fast_policy() -> RateLimitPolicy {
    RateLimitPolicy {
        jitter_max: Duration::ZERO,
        max_wait: Duration::from_secs(5),
        mutation_interval: Duration::ZERO,
        secondary_base_backoff: Duration::from_millis(10),
        transient_base_backoff: Duration::from_millis(10),
        ..RateLimitPolicy::default()
    }
}
