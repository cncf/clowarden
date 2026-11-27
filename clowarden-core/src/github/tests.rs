use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::Engine as _;
use serde_json::json;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::cfg::GitHubApp;

use super::{
    CachingGH, ClientPool, GH, GHApi, RateLimitPolicy, RequestKind, Source, b64,
    http_fake::{FakeGitHub, Route, generate_app_private_key},
    is_cancelled_error, is_rate_limit_error,
};

const CONTENT: &str = "teams:\n  - name: team1\n    maintainers:\n      - user1\n      - user2\n";
const CONTENT_PATH: &str = "/repos/fork-owner/fork-repo/contents/config/perms.yaml";
const FILE_PATH: &str = "config/perms.yaml";
const INSTALLATION_ID: i64 = 1;
const OTHER_INSTALLATION_ID: i64 = 2;

#[tokio::test]
async fn test_caching_gh_deduplicates_reads_and_does_not_cache_errors() {
    // Setup fake failing once then serving file content
    let body = content_file_body(&wrap_base64(&b64.encode(CONTENT)));
    let fake = FakeGitHub::start(vec![
        Route::once("GET", CONTENT_PATH, 404, "{}"),
        Route::new("GET", CONTENT_PATH, 200, body),
    ]);
    let gh = Arc::new(setup_gh(&fake));
    let cached = CachingGH::new(gh, CancellationToken::new());

    // First call fails and must not be cached
    assert!(cached.get_file_content(&sample_source(), FILE_PATH).await.is_err());

    // Second call succeeds and third hits the per-job cache
    assert_eq!(
        cached.get_file_content(&sample_source(), FILE_PATH).await.unwrap(),
        CONTENT
    );
    assert_eq!(
        cached.get_file_content(&sample_source(), FILE_PATH).await.unwrap(),
        CONTENT
    );
    assert_eq!(fake.requests().len(), 2);
}

#[tokio::test]
async fn test_caching_gh_respects_cancellation() {
    // Setup a pre-cancelled per-job cache
    let fake = FakeGitHub::start(vec![]);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let cached = CachingGH::new(Arc::new(setup_gh(&fake)), cancel);

    // Attempt to read a file
    let err = cached.get_file_content(&sample_source(), FILE_PATH).await.unwrap_err();

    // Check cancellation stopped the call before hitting the fake
    assert!(is_cancelled_error(&err));
    assert_eq!(fake.requests().len(), 0);
}

#[tokio::test]
async fn test_client_pool_appends_per_page_only_on_paginated_list_endpoints() {
    // Setup fake serving both a paginated list endpoint and a file content endpoint
    let repos_path = "/orgs/org1/repos";
    let body = content_file_body(&wrap_base64(&b64.encode(CONTENT)));
    let fake = FakeGitHub::start(vec![
        Route::new("GET", repos_path, 200, "[]"),
        Route::new("GET", CONTENT_PATH, 200, body),
    ]);
    let pool = fake.client_pool(fast_policy());
    let cancel = CancellationToken::new();

    // Call paginated list endpoint
    let _: Vec<octorust::types::MinimalRepository> = pool
        .execute(
            Some(INSTALLATION_ID),
            RequestKind::Read,
            &cancel,
            |client| async move {
                client
                    .repos()
                    .list_all_for_org(
                        "org1",
                        octorust::types::ReposListOrgType::All,
                        octorust::types::ReposListOrgSort::FullName,
                        octorust::types::Order::Asc,
                    )
                    .await
            },
        )
        .await
        .unwrap();

    // Call non-list content endpoint
    let gh = GHApi::new(pool);
    gh.get_file_content(&sample_source(), FILE_PATH).await.unwrap();

    // Check per_page=100 was appended only to the list endpoint
    let requests = fake.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].query_param("per_page"), Some("100"));
    assert_eq!(requests[1].query_param("per_page"), None);
}

#[tokio::test]
async fn test_client_pool_blocks_other_installation_during_global_secondary_cooldown() {
    // Setup fake where installation 1 receives a 429 with a long Retry-After
    let body = content_file_body(&wrap_base64(&b64.encode(CONTENT)));
    let fake = FakeGitHub::start(vec![
        Route::once("GET", CONTENT_PATH, 429, r#"{"message":"secondary limit"}"#)
            .with_header("Retry-After", "60"),
        Route::new("GET", CONTENT_PATH, 200, body),
    ]);
    let mut policy = fast_policy();
    policy.max_wait = Duration::from_secs(5);
    let gh = GHApi::new(fake.client_pool(policy));

    // Installation 1 hits a 60s secondary rate limit and fails fast
    let src_a = Source {
        inst_id: Some(INSTALLATION_ID),
        ..sample_source()
    };
    let err_a = gh.get_file_content(&src_a, FILE_PATH).await.unwrap_err();
    assert!(is_rate_limit_error(&err_a));

    // Installation 2 is also blocked by the global secondary cooldown
    let src_b = Source {
        inst_id: Some(OTHER_INSTALLATION_ID),
        ..sample_source()
    };
    let err_b = gh.get_file_content(&src_b, FILE_PATH).await.unwrap_err();
    assert!(is_rate_limit_error(&err_b));
    assert_eq!(fake.requests().len(), 1);
}

#[tokio::test]
async fn test_client_pool_cancels_in_flight_read_promptly() {
    // Setup fake with a slow read endpoint
    let body = content_file_body(&wrap_base64(&b64.encode(CONTENT)));
    let fake = FakeGitHub::start(vec![
        Route::new("GET", CONTENT_PATH, 200, body).with_delay(Duration::from_secs(5)),
    ]);
    let pool = fake.client_pool(fast_policy());
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(40)).await;
        trigger.cancel();
    });

    // Execute the read and ensure it aborts promptly when cancelled mid-flight
    let start = Instant::now();
    let err = pool
        .execute(
            Some(INSTALLATION_ID),
            RequestKind::Read,
            &cancel,
            |client| async move {
                client.repos().get_content_file("fork-owner", "fork-repo", FILE_PATH, "head-branch").await
            },
        )
        .await
        .unwrap_err();

    assert!(is_cancelled_error(&err));
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "expected prompt read cancellation, elapsed={:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn test_client_pool_cancels_mid_cooldown_wait() {
    // Setup a pool with a long active secondary cooldown
    let fake = FakeGitHub::start(vec![]);
    let mut policy = fast_policy();
    policy.max_wait = Duration::from_secs(60);
    let pool = fake.client_pool(policy);
    pool.governor()
        .record_secondary_rate_limit(Some(INSTALLATION_ID), Some(Duration::from_secs(30)));

    // Cancel the operation shortly after it enters the cooldown sleep
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        trigger.cancel();
    });

    // Execute a read under the pool
    let err = pool
        .execute(
            Some(INSTALLATION_ID),
            RequestKind::Read,
            &cancel,
            |client| async move { client.teams().list_all("org1").await },
        )
        .await
        .unwrap_err();

    // Check cancellation aborted the wait without hitting the fake
    assert!(is_cancelled_error(&err));
    assert_eq!(fake.requests().len(), 0);
}

#[tokio::test]
async fn test_client_pool_clamps_huge_retry_after_without_overflow_or_poisoning() {
    // Setup fake returning u64::MAX in Retry-After (would overflow Instant::add
    // if unclamped) followed by a normal response on another call
    let fake = FakeGitHub::start(vec![
        Route::once("GET", CONTENT_PATH, 429, "{}").with_header("Retry-After", u64::MAX.to_string()),
    ]);
    let mut policy = fast_policy();
    policy.max_wait = Duration::from_secs(5);
    let pool = fake.client_pool(policy);
    let gh = GHApi::new(pool.clone());

    // First call hits the huge Retry-After and fails fast without panicking
    let err = gh.get_file_content(&sample_source(), FILE_PATH).await.unwrap_err();
    assert!(is_rate_limit_error(&err));

    // Governor lock remains healthy and reports a clamped active cooldown
    let (remaining, is_secondary) = pool.governor().active_cooldown(None).unwrap();
    assert!(is_secondary);
    assert!(remaining <= Duration::from_hours(24));
}

#[tokio::test]
async fn test_client_pool_does_not_retry_genuine_permission_403() {
    // Setup fake returning a permission 403 without rate-limit markers
    let fake = FakeGitHub::start(vec![Route::new(
        "GET",
        CONTENT_PATH,
        403,
        r#"{"message":"Resource not accessible by integration"}"#,
    )]);
    let gh = GHApi::new(fake.client_pool(fast_policy()));

    // Fetch the file
    let err = gh.get_file_content(&sample_source(), FILE_PATH).await.unwrap_err();

    // Check the error was returned immediately without retry
    assert!(!is_rate_limit_error(&err));
    assert_eq!(fake.requests().len(), 1);
}

#[tokio::test]
async fn test_client_pool_does_not_retry_non_idempotent_post_on_5xx() {
    // Setup fake returning 502 with exhausted rate-limit headers to verify
    // octorust's RateLimited mapping never replays a non-idempotent POST
    let comments_path = "/repos/org1/repo1/issues/1/comments";
    let reset = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 1;
    let fake = FakeGitHub::start(vec![
        Route::once("POST", comments_path, 502, "bad gateway")
            .with_header("x-ratelimit-remaining", "0")
            .with_header("x-ratelimit-reset", reset.to_string()),
        Route::new(
            "POST",
            comments_path,
            201,
            r#"{"id":1,"author_association":"NONE"}"#,
        ),
    ]);
    let pool = fake.client_pool(fast_policy());
    let cancel = CancellationToken::new();

    // Send a non-idempotent POST
    let result = pool
        .execute(
            Some(INSTALLATION_ID),
            RequestKind::NonIdempotentWrite,
            &cancel,
            |client| async move {
                client
                    .issues()
                    .create_comment(
                        "org1",
                        "repo1",
                        1,
                        &octorust::types::PullsUpdateReviewRequest {
                            body: "hello".to_string(),
                        },
                    )
                    .await
            },
        )
        .await;

    // Check the POST was not replayed
    assert!(result.is_err());
    assert_eq!(fake.requests().len(), 1);
}

#[tokio::test]
async fn test_client_pool_does_not_retry_pat_client_on_401() {
    // Setup fake returning 401 for a PAT-authenticated client
    let fake = FakeGitHub::start(vec![Route::new(
        "GET",
        CONTENT_PATH,
        401,
        r#"{"message":"Bad credentials"}"#,
    )]);
    let pool = fake.client_pool(fast_policy());
    let cancel = CancellationToken::new();

    // Execute a read; PAT clients cannot refresh tokens and must fail immediately
    let err = pool
        .execute(
            Some(INSTALLATION_ID),
            RequestKind::Read,
            &cancel,
            |client| async move {
                client.repos().get_content_file("fork-owner", "fork-repo", FILE_PATH, "head-branch").await
            },
        )
        .await
        .unwrap_err();

    // Check the 401 was returned immediately without retry
    assert!(super::is_unauthorized_error(&err));
    assert_eq!(fake.requests().len(), 1);
}

#[tokio::test]
async fn test_client_pool_enforces_write_pacing_between_mutations() {
    let path = "/repos/org1/repo1/teams";
    let fake = FakeGitHub::start(vec![Route::new("DELETE", "/orgs/org1/teams/team1", 204, "")]);
    let mut policy = fast_policy();
    policy.mutation_interval = Duration::from_millis(80);
    let pool = fake.client_pool(policy);
    let cancel = CancellationToken::new();

    let start = Instant::now();
    for _ in 0..2 {
        pool.execute(
            Some(INSTALLATION_ID),
            RequestKind::IdempotentWrite,
            &cancel,
            |client| async move { client.teams().delete_in_org("org1", "team1").await },
        )
        .await
        .unwrap();
    }

    assert!(
        start.elapsed() >= Duration::from_millis(70),
        "expected mutation spacing, elapsed={:?}, path={path}",
        start.elapsed()
    );
    assert_eq!(fake.requests().len(), 2);
}

#[tokio::test]
async fn test_client_pool_follows_link_pagination_and_pauses_on_exhausted_page_one() {
    // Setup two-page Link sequence where page 1 exhausts primary quota with a
    // long reset, so page 2 must be blocked before hitting GitHub
    let teams_path = "/orgs/org1/teams";
    let future_reset = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 600;
    let fake = FakeGitHub::start(vec![
        Route::once("GET", teams_path, 200, r#"[{"id":1,"name":"T1","slug":"t1"}]"#)
            .with_header("x-ratelimit-remaining", "0")
            .with_header("x-ratelimit-reset", future_reset.to_string())
            .with_header(
                "Link",
                format!(r#"<http://127.0.0.1:1{teams_path}?per_page=100&page=2>; rel="next""#),
            ),
        Route::new("GET", teams_path, 200, r#"[{"id":2,"name":"T2","slug":"t2"}]"#),
    ]);
    let mut policy = fast_policy();
    policy.max_wait = Duration::from_secs(5);
    let pool = fake.client_pool(policy);
    let cancel = CancellationToken::new();

    // Run paginated list; page 1 exhausts quota so page 2 is blocked by middleware
    let err = pool
        .execute(
            Some(INSTALLATION_ID),
            RequestKind::Read,
            &cancel,
            |client| async move { client.teams().list_all("org1").await },
        )
        .await
        .unwrap_err();

    // Check page 1 was sent with per_page=100 and page 2 was never dispatched
    assert!(is_rate_limit_error(&err));
    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].query_param("per_page"), Some("100"));
}

#[tokio::test]
async fn test_client_pool_isolates_primary_rate_limit_to_affected_installation() {
    let future_reset = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 600;
    let body = content_file_body(&wrap_base64(&b64.encode(CONTENT)));
    let fake = FakeGitHub::start(vec![
        Route::once(
            "GET",
            CONTENT_PATH,
            403,
            r#"{"message":"API rate limit exceeded"}"#,
        )
        .with_header("x-ratelimit-remaining", "0")
        .with_header("x-ratelimit-reset", future_reset.to_string()),
        Route::new("GET", CONTENT_PATH, 200, body),
    ]);
    let mut policy = fast_policy();
    policy.max_wait = Duration::from_secs(5);
    let gh = GHApi::new(fake.client_pool(policy));

    // Installation 1 hits a 10-minute primary limit and fails fast
    let src_a = Source {
        inst_id: Some(INSTALLATION_ID),
        ..sample_source()
    };
    let err = gh.get_file_content(&src_a, FILE_PATH).await.unwrap_err();
    assert!(is_rate_limit_error(&err));

    // Installation 2 is unaffected and succeeds immediately
    let src_b = Source {
        inst_id: Some(OTHER_INSTALLATION_ID),
        ..sample_source()
    };
    let content = gh.get_file_content(&src_b, FILE_PATH).await.unwrap();
    assert_eq!(content, CONTENT);
}

#[tokio::test]
async fn test_client_pool_paces_http_mutations_across_token_exchange() {
    // Setup fake serving installation token exchanges (POST) immediately with
    // zero delay followed by DELETE mutations for two concurrent installations
    let token_path_1 = format!("/app/installations/{INSTALLATION_ID}/access_tokens");
    let token_path_2 = format!("/app/installations/{OTHER_INSTALLATION_ID}/access_tokens");
    let token_body = json!({
        "token": "ghs_test_token",
        "expires_at": "2099-01-01T00:00:00Z"
    })
    .to_string();
    let fake = FakeGitHub::start(vec![
        Route::new("POST", &token_path_1, 201, token_body.clone()),
        Route::new("POST", &token_path_2, 201, token_body),
        Route::new("DELETE", "/orgs/org1/teams/team1", 204, ""),
        Route::new("DELETE", "/orgs/org2/teams/team1", 204, ""),
    ]);

    let mut policy = fast_policy();
    policy.mutation_interval = Duration::from_millis(120);
    let pool = Arc::new(
        ClientPool::new_with_app_creds_for_testing(
            &sample_github_app(&generate_app_private_key()),
            fake.base_url(),
            policy,
            CancellationToken::new(),
        )
        .unwrap(),
    );
    let cancel = CancellationToken::new();

    // Dispatch concurrent writes for two installations, each requiring a POST
    // token exchange before its DELETE mutation (4 total HTTP mutations)
    let (res1, res2) = tokio::join!(
        pool.execute(
            Some(INSTALLATION_ID),
            RequestKind::IdempotentWrite,
            &cancel,
            |client| async move { client.teams().delete_in_org("org1", "team1").await },
        ),
        pool.execute(
            Some(OTHER_INSTALLATION_ID),
            RequestKind::IdempotentWrite,
            &cancel,
            |client| async move { client.teams().delete_in_org("org2", "team1").await },
        ),
    );
    res1.unwrap();
    res2.unwrap();

    // Check every consecutive mutative HTTP dispatch (both token POSTs and
    // DELETEs) was spaced by the governor even though responses were immediate
    let requests = fake.requests();
    assert_eq!(requests.len(), 4);
    for pair in requests.windows(2) {
        let gap = pair[1].received_at.saturating_duration_since(pair[0].received_at);
        assert!(
            gap >= Duration::from_millis(100),
            "expected >= 100ms spacing between {} {} and {} {}, got {gap:?}",
            pair[0].method,
            pair[0].path,
            pair[1].method,
            pair[1].path
        );
    }
}

#[tokio::test]
async fn test_client_pool_preserves_wait_budget_across_401_refresh() {
    // Setup fake where a pre-401 cooldown (70ms) and the post-401 write-pacing
    // wait (90ms) each fit independently inside max_wait (130ms), while their
    // combined wait (160ms) exceeds max_wait so a budget reset would succeed
    let token_path = format!("/app/installations/{INSTALLATION_ID}/access_tokens");
    let comments_path = "/repos/org1/repo1/issues/1/comments";
    let body = content_file_body(&wrap_base64(&b64.encode(CONTENT)));
    let fake = FakeGitHub::start(vec![
        Route::new(
            "POST",
            &token_path,
            201,
            json!({"token": "ghs_token", "expires_at": "2099-01-01T00:00:00Z"}).to_string(),
        ),
        Route::once("GET", CONTENT_PATH, 200, body),
        Route::once("POST", comments_path, 401, r#"{"message":"Bad credentials"}"#),
        Route::new(
            "POST",
            comments_path,
            201,
            r#"{"id":1,"author_association":"NONE"}"#,
        ),
    ]);
    let mut policy = fast_policy();
    policy.max_wait = Duration::from_millis(130);
    policy.mutation_interval = Duration::from_millis(90);
    let pool = Arc::new(
        ClientPool::new_with_app_creds_for_testing(
            &sample_github_app(&generate_app_private_key()),
            fake.base_url(),
            policy,
            CancellationToken::new(),
        )
        .unwrap(),
    );

    // Warm the cached installation client so initial token POST doesn't count
    // toward the test call's budget, then let its write slot expire
    let cancel = CancellationToken::new();
    let gh = GHApi::new(pool.clone());
    let src = Source {
        inst_id: Some(INSTALLATION_ID),
        ..sample_source()
    };
    assert_eq!(gh.get_file_content(&src, FILE_PATH).await.unwrap(), CONTENT);
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Record a 70ms cooldown before the POST: 70ms fits in 130ms, and the
    // 90ms post-401 write-pacing wait fits on its own too, but 70+90 > 130
    pool.governor()
        .record_primary_rate_limit(Some(INSTALLATION_ID), Duration::from_millis(70));
    let err = pool
        .execute(
            Some(INSTALLATION_ID),
            RequestKind::NonIdempotentWrite,
            &cancel,
            |client| async move {
                client
                    .issues()
                    .create_comment(
                        "org1",
                        "repo1",
                        1,
                        &octorust::types::PullsUpdateReviewRequest {
                            body: "hello".to_string(),
                        },
                    )
                    .await
            },
        )
        .await
        .unwrap_err();

    // Check the retry failed fast on the shared budget rather than resetting it
    assert!(is_rate_limit_error(&err));
    assert_eq!(
        fake.requests().iter().filter(|r| r.path == comments_path).count(),
        1
    );
}

#[tokio::test]
async fn test_client_pool_refreshes_401_with_exhausted_rate_limit_headers() {
    // Setup fake returning 401 with x-ratelimit-remaining: 0 and a far-future
    // reset (simulating the unauthenticated 60/hr IP bucket) on a POST
    let token_path = format!("/app/installations/{INSTALLATION_ID}/access_tokens");
    let comments_path = "/repos/org1/repo1/issues/1/comments";
    let reset = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 3_600;
    let fake = FakeGitHub::start(vec![
        Route::once(
            "POST",
            &token_path,
            201,
            json!({"token": "ghs_stale", "expires_at": "2099-01-01T00:00:00Z"}).to_string(),
        ),
        Route::once(
            "POST",
            &token_path,
            201,
            json!({"token": "ghs_fresh", "expires_at": "2099-01-01T00:00:00Z"}).to_string(),
        ),
        Route::once("POST", comments_path, 401, r#"{"message":"Bad credentials"}"#)
            .with_header("x-ratelimit-remaining", "0")
            .with_header("x-ratelimit-reset", reset.to_string()),
        Route::new(
            "POST",
            comments_path,
            201,
            r#"{"id":1,"author_association":"NONE"}"#,
        ),
    ]);
    let pool = Arc::new(
        ClientPool::new_with_app_creds_for_testing(
            &sample_github_app(&generate_app_private_key()),
            fake.base_url(),
            fast_policy(),
            CancellationToken::new(),
        )
        .unwrap(),
    );

    // Execute the POST; middleware must not poison the installation cooldown
    // with the 401's unauthenticated rate-limit headers and must refresh once
    let cancel = CancellationToken::new();
    let comment = pool
        .execute(
            Some(INSTALLATION_ID),
            RequestKind::NonIdempotentWrite,
            &cancel,
            |client| async move {
                client
                    .issues()
                    .create_comment(
                        "org1",
                        "repo1",
                        1,
                        &octorust::types::PullsUpdateReviewRequest {
                            body: "hello".to_string(),
                        },
                    )
                    .await
            },
        )
        .await
        .unwrap();

    // Check credentials were refreshed immediately and no cooldown was recorded
    assert_eq!(comment.id, 1);
    assert!(pool.governor().active_cooldown(Some(INSTALLATION_ID)).is_none());
    let comment_requests: Vec<_> = fake.requests().into_iter().filter(|r| r.path == comments_path).collect();
    assert_eq!(comment_requests.len(), 2);
    assert_eq!(
        comment_requests[0].header("authorization"),
        Some("token ghs_stale")
    );
    assert_eq!(
        comment_requests[1].header("authorization"),
        Some("token ghs_fresh")
    );
}

#[tokio::test]
async fn test_client_pool_refreshes_installation_token_once_on_401() {
    // Setup fake serving an initial token that later returns 401, followed by a
    // fresh token that succeeds on retry
    let token_path = format!("/app/installations/{INSTALLATION_ID}/access_tokens");
    let body = content_file_body(&wrap_base64(&b64.encode(CONTENT)));
    let fake = FakeGitHub::start(vec![
        Route::once(
            "POST",
            &token_path,
            201,
            json!({"token": "ghs_stale", "expires_at": "2099-01-01T00:00:00Z"}).to_string(),
        ),
        Route::once(
            "POST",
            &token_path,
            201,
            json!({"token": "ghs_fresh", "expires_at": "2099-01-01T00:00:00Z"}).to_string(),
        ),
        Route::once("GET", CONTENT_PATH, 200, body.clone()),
        Route::once("GET", CONTENT_PATH, 401, r#"{"message":"Bad credentials"}"#),
        Route::new("GET", CONTENT_PATH, 200, body),
    ]);

    let pool = Arc::new(
        ClientPool::new_with_app_creds_for_testing(
            &sample_github_app(&generate_app_private_key()),
            fake.base_url(),
            fast_policy(),
            CancellationToken::new(),
        )
        .unwrap(),
    );
    let cancel = CancellationToken::new();

    // Warm the cached installation client with ghs_stale
    let gh = GHApi::new(pool.clone());
    let src = Source {
        inst_id: Some(INSTALLATION_ID),
        ..sample_source()
    };
    assert_eq!(gh.get_file_content(&src, FILE_PATH).await.unwrap(), CONTENT);

    // Next call receives 401, refreshes the installation client, and succeeds
    assert_eq!(gh.get_file_content(&src, FILE_PATH).await.unwrap(), CONTENT);

    // Verify token exchange happened twice and the retry used the fresh token
    let requests = fake.requests();
    let token_requests: Vec<_> = requests.iter().filter(|r| r.path == token_path).collect();
    let content_requests: Vec<_> = requests.iter().filter(|r| r.path == CONTENT_PATH).collect();
    assert_eq!(token_requests.len(), 2);
    assert_eq!(content_requests.len(), 3);
    assert_eq!(
        content_requests[0].header("authorization"),
        Some("token ghs_stale")
    );
    assert_eq!(
        content_requests[1].header("authorization"),
        Some("token ghs_stale")
    );
    assert_eq!(
        content_requests[2].header("authorization"),
        Some("token ghs_fresh")
    );

    // A persistent 401 after refresh fails without looping indefinitely
    let persistent_fake = FakeGitHub::start(vec![
        Route::new(
            "POST",
            &token_path,
            201,
            json!({"token": "ghs_bad", "expires_at": "2099-01-01T00:00:00Z"}).to_string(),
        ),
        Route::new("GET", CONTENT_PATH, 401, r#"{"message":"Bad credentials"}"#),
    ]);
    let persistent_pool = Arc::new(
        ClientPool::new_with_app_creds_for_testing(
            &sample_github_app(&generate_app_private_key()),
            persistent_fake.base_url(),
            fast_policy(),
            CancellationToken::new(),
        )
        .unwrap(),
    );
    let err = persistent_pool
        .execute(
            Some(INSTALLATION_ID),
            RequestKind::Read,
            &cancel,
            |client| async move {
                client.repos().get_content_file("fork-owner", "fork-repo", FILE_PATH, "head-branch").await
            },
        )
        .await
        .unwrap_err();
    assert!(super::is_unauthorized_error(&err));
    assert_eq!(
        persistent_fake.requests().iter().filter(|r| r.path == CONTENT_PATH).count(),
        2
    );
}

#[tokio::test]
async fn test_client_pool_retries_429_with_retry_after_and_succeeds() {
    let body = content_file_body(&wrap_base64(&b64.encode(CONTENT)));
    let fake = FakeGitHub::start(vec![
        Route::once("GET", CONTENT_PATH, 429, r#"{"message":"rate limited"}"#)
            .with_header("Retry-After", "0"),
        Route::new("GET", CONTENT_PATH, 200, body),
    ]);
    let gh = GHApi::new(fake.client_pool(fast_policy()));

    let content = gh.get_file_content(&sample_source(), FILE_PATH).await.unwrap();

    assert_eq!(content, CONTENT);
    assert_eq!(fake.requests().len(), 2);
}

#[tokio::test]
async fn test_client_pool_retries_body_only_403_secondary_rate_limit() {
    let body = content_file_body(&wrap_base64(&b64.encode(CONTENT)));
    let fake = FakeGitHub::start(vec![
        Route::once(
            "GET",
            CONTENT_PATH,
            403,
            r#"{"message":"You have exceeded a secondary rate limit. Please wait a few minutes."}"#,
        ),
        Route::new("GET", CONTENT_PATH, 200, body),
    ]);
    let gh = GHApi::new(fake.client_pool(fast_policy()));

    let content = gh.get_file_content(&sample_source(), FILE_PATH).await.unwrap();

    assert_eq!(content, CONTENT);
    assert_eq!(fake.requests().len(), 2);
}

#[tokio::test]
async fn test_client_pool_retries_read_on_5xx_and_succeeds() {
    // Setup fake failing once with 502 then returning content
    let body = content_file_body(&wrap_base64(&b64.encode(CONTENT)));
    let fake = FakeGitHub::start(vec![
        Route::once("GET", CONTENT_PATH, 502, "bad gateway"),
        Route::new("GET", CONTENT_PATH, 200, body),
    ]);
    let gh = GHApi::new(fake.client_pool(fast_policy()));

    // Fetch the file
    let content = gh.get_file_content(&sample_source(), FILE_PATH).await.unwrap();

    // Check the read retried and succeeded
    assert_eq!(content, CONTENT);
    assert_eq!(fake.requests().len(), 2);
}

#[tokio::test]
async fn test_get_file_content_decodes_wrapped_base64_from_given_ref() {
    // Setup file served by the fake
    let body = content_file_body(&wrap_base64(&b64.encode(CONTENT)));
    let fake = FakeGitHub::start(vec![Route::new("GET", CONTENT_PATH, 200, body)]);
    let gh = setup_gh(&fake);

    // Fetch the file
    let content = gh.get_file_content(&sample_source(), FILE_PATH).await.unwrap();

    // Check the content was decoded and read from the right reference
    assert_eq!(content, CONTENT);
    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, CONTENT_PATH);
    assert_eq!(requests[0].query_param("ref"), Some("head-branch"));
}

#[tokio::test]
async fn test_get_file_content_rejects_invalid_base64() {
    // Setup file served by the fake
    let body = content_file_body("not*base64");
    let fake = FakeGitHub::start(vec![Route::new("GET", CONTENT_PATH, 200, body)]);
    let gh = setup_gh(&fake);

    // Fetch the file
    let err = gh.get_file_content(&sample_source(), FILE_PATH).await.unwrap_err();

    // Check the decoding error is returned
    assert!(err.downcast_ref::<base64::DecodeError>().is_some());
}

#[tokio::test]
async fn test_get_file_content_rejects_non_utf8_content() {
    // Setup file served by the fake
    let body = content_file_body(&b64.encode([0xff, 0xfe]));
    let fake = FakeGitHub::start(vec![Route::new("GET", CONTENT_PATH, 200, body)]);
    let gh = setup_gh(&fake);

    // Fetch the file
    let err = gh.get_file_content(&sample_source(), FILE_PATH).await.unwrap_err();

    // Check the conversion error is returned
    assert!(err.downcast_ref::<std::string::FromUtf8Error>().is_some());
}

#[test]
fn test_new_with_app_creds_allows_getting_installation_client() {
    // Build the instance
    let gh = GHApi::new_with_app_creds(&sample_github_app(&generate_app_private_key())).unwrap();

    // Check a client is available for the installation
    assert!(gh.pool().get_client(Some(INSTALLATION_ID)).is_ok());
}

#[test]
fn test_new_with_app_creds_rejects_malformed_pem() {
    // Build the instance
    let result = GHApi::new_with_app_creds(&sample_github_app("not a pem"));

    // Check the parsing error is returned
    assert!(result.err().unwrap().downcast_ref::<pem::PemError>().is_some());
}

#[test]
fn test_new_with_token_allows_getting_client_without_installation() {
    // Build the instance
    let gh = GHApi::new_with_token("token".to_string()).unwrap();

    // Check a client is available when no installation is involved
    assert!(gh.pool().get_client(None).is_ok());
}

// Helpers.

/// Build the body of a contents API response with the content provided.
fn content_file_body(content: &str) -> String {
    json!({ "_links": {}, "content": content, "encoding": "base64" }).to_string()
}

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

/// Return a GitHub application using the private key provided.
fn sample_github_app(private_key: &str) -> GitHubApp {
    GitHubApp {
        app_id: 1,
        private_key: private_key.to_string(),
        ..Default::default()
    }
}

/// Return a source pointing to a pull request's head in a fork.
fn sample_source() -> Source {
    Source {
        inst_id: None,
        owner: "fork-owner".to_string(),
        repo: "fork-repo".to_string(),
        ref_: "head-branch".to_string(),
    }
}

/// Setup a GHApi instance that sends all requests to the fake provided.
fn setup_gh(fake: &FakeGitHub) -> GHApi {
    let pool = Arc::new(ClientPool::new_with_token("token".to_string()).unwrap());
    pool.insert_client(None, Arc::new(fake.client()));
    GHApi::new(pool)
}

/// Split the base64 content provided in lines, as the contents API does.
fn wrap_base64(content: &str) -> String {
    let lines: Vec<&str> =
        content.as_bytes().chunks(60).map(|line| std::str::from_utf8(line).unwrap()).collect();
    format!("{}\n", lines.join("\n"))
}
