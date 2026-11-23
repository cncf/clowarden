use std::sync::Arc;

use anyhow::format_err;
use axum::{
    body::to_bytes,
    http::{
        Method,
        header::{IF_MODIFIED_SINCE, LAST_MODIFIED, LOCATION},
    },
};
use deadpool_postgres::Config as Db;
use futures::future;
use hmac::digest::MacError;
use serde_json::{Value, json};
use time::OffsetDateTime;
use tower::ServiceExt;

use clowarden_core::cfg::{GitHubApp, Legacy, Service, Services};

use crate::{
    cfg::{BasicAuth, DEFAULT_RECONCILE_FREQUENCY, HttpServer, Log, LogFormat},
    db::MockDB,
};

use super::*;

const AUDIT_APP_JS: &str = "console.log(\"audit app\");\n";
const AUDIT_INDEX: &str = "<html><body>audit index</body></html>\n";
const AUDIT_INDEX_CACHE_CONTROL: &str = "max-age=300";
const BASE_REF: &str = "master";
const BASIC_AUTH_PASSWORD: &str = "password";
const BASIC_AUTH_USERNAME: &str = "username";
const ERROR: &str = "something went wrong";
const HEAD_SHA: &str = "ec26c3e57ca3a959ca5aad62de7213c562f8c821";
const INSTALLATION_ID: i64 = 1;
/// Pull request event recorded by GitHub, as published in the
/// `payload-examples` directory of the `octokit/webhooks` repository. The
/// organization, repository, branch and pull request details used in the
/// tests match the ones in this payload.
const OPENED_EVENT: &str = include_str!("../testdata/webhooks/pull-request-opened.json");
const ORG: &str = "Octocoders";
const PROTECTED_ROUTES: [&str; 5] = [
    "/audit/",
    "/audit/api/changes/search",
    "/audit/api/organizations",
    "/audit/some/route",
    "/audit/static/app.js",
];
const PR_CREATED_BY: &str = "Codertocat";
const PR_MERGED_AT: &str = "2019-05-15T15:20:33Z";
const PR_MERGED_AT_TIMESTAMP: i64 = 1_557_933_633;
const PR_MERGED_BY: &str = "merger";
const PR_NUMBER: i64 = 2;
const REPO: &str = "Hello-World";
const ROOT_INDEX: &str = "<html><body>root index</body></html>\n";
const SEARCH_RESULTS: &str = r#"[{"kind":"repository-added"}]"#;
const STATIC_CACHE_CONTROL: &str = "max-age=31536000";
const STATIC_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/testdata/static");
const WEBHOOK_SECRET: &str = "webhook-secret";
const WEBHOOK_SECRET_FALLBACK: &str = "webhook-secret-fallback";

#[tokio::test]
async fn test_event_accepts_fallback_secret_signature() {
    // Setup configuration
    let mut cfg = sample_config();
    cfg.server.github_app.webhook_secret_fallback = Some(WEBHOOK_SECRET_FALLBACK.to_string());

    // Run the request
    let body = serde_json::to_vec(&opened_event()).unwrap();
    let request = webhook_request("pull_request", body, WEBHOOK_SECRET_FALLBACK);
    let (response, triaged) = send_request(&cfg, idle_db(), request).await;

    // Check the event was accepted and enqueued for triage
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(triaged.len(), 1);
    assert_eq!(
        unwrap_pr_triage(&triaged[0]).action,
        PullRequestEventAction::Opened
    );
}

#[tokio::test]
async fn test_event_check_run_rerequested_enqueues_triage() {
    let payload = json!({
        "action": "rerequested",
        "check_run": {
            "name": CHECK_RUN_NAME,
            "head_sha": HEAD_SHA,
            "pull_requests": [
                {
                    "number": PR_NUMBER,
                    "base": { "ref": BASE_REF, "sha": "base-sha" },
                    "head": { "ref": "feature", "sha": HEAD_SHA }
                }
            ]
        },
        "organization": opened_event()["organization"],
        "repository": opened_event()["repository"]
    });
    let request = webhook_request("check_run", serde_json::to_vec(&payload).unwrap(), WEBHOOK_SECRET);
    let (response, triaged) = send_request(&sample_config(), idle_db(), request).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(triaged.len(), 1);
    let TriageInput::CheckRunRerequested(input) = &triaged[0] else {
        panic!("expected CheckRunRerequested triage input");
    };
    assert_eq!(input.org, sample_org());
    assert_eq!(input.head_sha, HEAD_SHA);
    assert_eq!(input.pr_numbers, vec![PR_NUMBER]);

    // Non-CLOWarden check runs and other actions are ignored
    let mut other_check = payload.clone();
    other_check["check_run"]["name"] = json!("OtherCI");
    let (_, other_triaged) = send_request(
        &sample_config(),
        idle_db(),
        webhook_request(
            "check_run",
            serde_json::to_vec(&other_check).unwrap(),
            WEBHOOK_SECRET,
        ),
    )
    .await;
    assert_eq!(other_triaged, vec![]);
}

#[tokio::test]
async fn test_event_check_suite_rerequested_enqueues_triage() {
    let payload = json!({
        "action": "rerequested",
        "check_suite": {
            "head_sha": HEAD_SHA,
            "pull_requests": [
                {
                    "number": PR_NUMBER,
                    "base": { "ref": BASE_REF, "sha": "base-sha" },
                    "head": { "ref": "feature", "sha": HEAD_SHA }
                }
            ]
        },
        "organization": opened_event()["organization"],
        "repository": opened_event()["repository"]
    });
    let request = webhook_request(
        "check_suite",
        serde_json::to_vec(&payload).unwrap(),
        WEBHOOK_SECRET,
    );
    let (response, triaged) = send_request(&sample_config(), idle_db(), request).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(triaged.len(), 1);
    let TriageInput::CheckRunRerequested(input) = &triaged[0] else {
        panic!("expected CheckRunRerequested triage input");
    };
    assert_eq!(input.org, sample_org());
    assert_eq!(input.head_sha, HEAD_SHA);
    assert_eq!(input.pr_numbers, vec![PR_NUMBER]);
}

#[tokio::test]
async fn test_event_closed_and_merged_enqueues_triage_with_merge_metadata() {
    // Run the request
    let request = pull_request_webhook_request(&merged_event());
    let (response, triaged) = send_request(&sample_config(), idle_db(), request).await;

    // Check triage input captured the merge metadata
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(triaged.len(), 1);
    let pr = unwrap_pr_triage(&triaged[0]);
    assert_eq!(pr.action, PullRequestEventAction::Closed);
    assert_eq!(pr.org, sample_org());
    assert_eq!(pr.pr_number, PR_NUMBER);
    assert_eq!(pr.pr_created_by.as_deref(), Some(PR_CREATED_BY));
    assert_eq!(pr.pr_merged_by.as_deref(), Some(PR_MERGED_BY));
    assert_eq!(
        pr.pr_merged_at,
        Some(OffsetDateTime::from_unix_timestamp(PR_MERGED_AT_TIMESTAMP).unwrap())
    );
}

#[tokio::test]
async fn test_event_closed_unmerged_is_ignored_without_queueing() {
    // Setup event for a pull request closed without merging
    let mut event = opened_event();
    event["action"] = json!("closed");

    // Run the request
    let request = pull_request_webhook_request(&event);
    let (response, triaged) = send_request(&sample_config(), idle_db(), request).await;

    // Check no triage work was queued
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(triaged, vec![]);
}

#[tokio::test]
async fn test_event_edited_base_change_enqueues_triage_and_ignores_non_base_or_closed_edits() {
    // Open PR retargeted to the configured base branch must be validated
    let mut base_changed = opened_event();
    base_changed["action"] = json!("edited");
    base_changed["changes"] = json!({
        "base": {
            "ref": { "from": "develop" },
            "sha": { "from": "0000000000000000000000000000000000000000" }
        }
    });
    let (response, triaged) = send_request(
        &sample_config(),
        idle_db(),
        pull_request_webhook_request(&base_changed),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(triaged.len(), 1);
    assert_eq!(
        unwrap_pr_triage(&triaged[0]).action,
        PullRequestEventAction::Edited
    );

    // Title-only edit on an open PR must be ignored
    let mut title_only = opened_event();
    title_only["action"] = json!("edited");
    title_only["changes"] = json!({ "title": { "from": "Old title" } });
    let (response, triaged) = send_request(
        &sample_config(),
        idle_db(),
        pull_request_webhook_request(&title_only),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(triaged, vec![]);

    // Base edit on a closed PR must be ignored
    let mut closed_edit = base_changed;
    closed_edit["pull_request"]["state"] = json!("closed");
    let (response, triaged) = send_request(
        &sample_config(),
        idle_db(),
        pull_request_webhook_request(&closed_edit),
    )
    .await;
    // Check base edit on a closed PR was ignored
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(triaged, vec![]);
}

#[tokio::test]
async fn test_event_edited_retarget_to_previously_seen_sha_with_newer_timestamp_enqueues_triage() {
    // Setup router with shared LatestPrHeads tracking A -> B on the managed branch
    let (triage_tx, mut triage_rx) = mpsc::channel(16);
    let latest_pr_heads = LatestPrHeads::new();
    let router = setup_router(
        &sample_config(),
        Arc::new(idle_db()),
        latest_pr_heads.clone(),
        triage_tx,
    )
    .unwrap();

    let first = opened_event();
    let mut second = opened_event();
    second["action"] = json!("synchronize");
    second["before"] = json!(HEAD_SHA);
    second["pull_request"]["head"]["sha"] = json!("newer-sha-b");
    second["pull_request"]["updated_at"] = json!("2019-05-15T15:25:00Z");

    // Retargeted back to managed branch at previously seen HEAD_SHA with strictly newer timestamp
    let mut retargeted = opened_event();
    retargeted["action"] = json!("edited");
    retargeted["changes"] = json!({ "base": { "ref": { "from": "develop" } } });
    retargeted["pull_request"]["updated_at"] = json!("2019-05-15T15:30:00Z");

    // Deliver opened (A), synchronize (B), then edited back to managed branch at A
    assert_eq!(
        router.clone().oneshot(pull_request_webhook_request(&first)).await.unwrap().status(),
        StatusCode::OK
    );
    assert_eq!(
        router.clone().oneshot(pull_request_webhook_request(&second)).await.unwrap().status(),
        StatusCode::OK
    );
    assert_eq!(
        router.oneshot(pull_request_webhook_request(&retargeted)).await.unwrap().status(),
        StatusCode::OK
    );

    // Check all three deliveries were queued and HEAD_SHA is now current
    assert!(!latest_pr_heads.is_superseded(ORG, PR_NUMBER, HEAD_SHA));
    let _ = triage_rx.try_recv().unwrap();
    let _ = triage_rx.try_recv().unwrap();
    let third = triage_rx.try_recv().unwrap();
    assert_eq!(unwrap_pr_triage(&third).action, PullRequestEventAction::Edited);
    assert_eq!(unwrap_pr_triage(&third).pr_head_sha, HEAD_SHA);
}

#[tokio::test]
async fn test_event_enqueues_fork_pr_triage_for_opened_and_synchronize() {
    for (action, expected_action) in [
        ("opened", PullRequestEventAction::Opened),
        ("synchronize", PullRequestEventAction::Synchronize),
    ] {
        // Setup event for a pull request created from a fork
        let mut event = opened_event();
        event["action"] = json!(action);
        event["pull_request"]["head"]["ref"] = json!("feature");
        event["pull_request"]["head"]["repo"]["name"] = json!("forked-repo");
        event["pull_request"]["head"]["repo"]["owner"]["login"] = json!("forker");

        // Run the request
        let request = pull_request_webhook_request(&event);
        let (response, triaged) = send_request(&sample_config(), idle_db(), request).await;

        // Check triage input was queued with fork coordinates and head SHA
        assert_eq!(response.status(), StatusCode::OK, "{action}");
        assert_eq!(triaged.len(), 1, "{action}");
        let pr = unwrap_pr_triage(&triaged[0]);
        assert_eq!(pr.action, expected_action, "{action}");
        assert_eq!(pr.org, sample_org(), "{action}");
        assert_eq!(pr.pr_number, PR_NUMBER, "{action}");
        assert_eq!(pr.pr_head_owner.as_deref(), Some("forker"), "{action}");
        assert_eq!(pr.pr_head_repo.as_deref(), Some("forked-repo"), "{action}");
        assert_eq!(pr.pr_head_ref, "feature", "{action}");
        assert_eq!(pr.pr_head_sha, HEAD_SHA, "{action}");
    }
}

#[tokio::test]
async fn test_event_redelivered_older_pr_webhook_does_not_overwrite_newer_head() {
    let (triage_tx, mut triage_rx) = mpsc::channel(16);
    let latest_pr_heads = LatestPrHeads::new();
    let router = setup_router(
        &sample_config(),
        Arc::new(idle_db()),
        latest_pr_heads.clone(),
        triage_tx,
    )
    .unwrap();

    // Newer synchronize delivery (head B, later updated_at) arrives first
    let mut newer = opened_event();
    newer["action"] = json!("synchronize");
    newer["before"] = json!(HEAD_SHA);
    newer["pull_request"]["head"]["sha"] = json!("newer-sha-b");
    newer["pull_request"]["updated_at"] = json!("2019-05-15T15:25:00Z");

    // Older opened delivery (head A, earlier updated_at) is redelivered afterward
    let older = opened_event();

    let resp_new = router.clone().oneshot(pull_request_webhook_request(&newer)).await.unwrap();
    let resp_old = router.oneshot(pull_request_webhook_request(&older)).await.unwrap();
    assert_eq!(resp_new.status(), StatusCode::OK);
    assert_eq!(resp_old.status(), StatusCode::OK);

    // Head B remains current (not superseded), head A is superseded and not queued
    assert!(!latest_pr_heads.is_superseded(ORG, PR_NUMBER, "newer-sha-b"));
    assert!(latest_pr_heads.is_superseded(ORG, PR_NUMBER, HEAD_SHA));
    let queued = triage_rx.try_recv().unwrap();
    assert_eq!(unwrap_pr_triage(&queued).pr_head_sha, "newer-sha-b");
    assert!(triage_rx.try_recv().is_err());
}

#[tokio::test]
async fn test_event_ignores_non_target_pull_requests() {
    // Setup events that must be ignored
    let cases: [(&str, AdjustEvent); 5] = [
        ("missing organization", |event| {
            event.as_object_mut().unwrap().remove("organization");
        }),
        ("unknown organization", |event| {
            event["organization"]["login"] = json!("other");
        }),
        ("unsupported action", |event| {
            event["action"] = json!("labeled");
        }),
        ("wrong base branch", |event| {
            event["pull_request"]["base"]["ref"] = json!("feature");
            event["pull_request"]["head"]["ref"] = json!(BASE_REF);
        }),
        ("wrong repository", |event| {
            event["repository"]["name"] = json!("other");
        }),
    ];

    for (name, adjust_event) in cases {
        // Setup event
        let mut event = opened_event();
        adjust_event(&mut event);

        // Run the request
        let request = pull_request_webhook_request(&event);
        let (response, triaged) = send_request(&sample_config(), idle_db(), request).await;

        // Check the event was ignored
        assert_eq!(response.status(), StatusCode::OK, "{name}");
        assert_eq!(triaged, vec![], "{name}");
    }
}

#[tokio::test]
async fn test_event_ignores_unsupported_event() {
    // Run the request
    let request = webhook_request("ping", b"{}".to_vec(), WEBHOOK_SECRET);
    let (response, triaged) = send_request(&sample_config(), idle_db(), request).await;

    // Check the event was ignored
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(triaged, vec![]);
}

#[tokio::test]
async fn test_event_rejects_invalid_signature_before_side_effects() {
    // Setup request signed with the wrong secret
    let body = serde_json::to_vec(&merged_event()).unwrap();
    let request = webhook_request("pull_request", body, "wrong-secret");

    // Run the request
    let (response, triaged) = send_request(&sample_config(), idle_db(), request).await;

    // Check the request was rejected without side effects
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_text(response).await, "no valid signature found");
    assert_eq!(triaged, vec![]);
}

#[tokio::test]
async fn test_event_rejects_malformed_body() {
    // Run the request
    let request = webhook_request("pull_request", b"not json".to_vec(), WEBHOOK_SECRET);
    let (response, triaged) = send_request(&sample_config(), idle_db(), request).await;

    // Check the request was rejected
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_text(response).await.starts_with("invalid body: "));
    assert_eq!(triaged, vec![]);
}

#[tokio::test]
async fn test_event_rejects_missing_event_header() {
    // Setup request without event header
    let body = serde_json::to_vec(&opened_event()).unwrap();
    let request = Request::builder()
        .method(Method::POST)
        .uri("/webhook/github")
        .header(GITHUB_SIGNATURE_HEADER, signature(&body, WEBHOOK_SECRET))
        .body(Body::from(body))
        .unwrap();

    // Run the request
    let (response, triaged) = send_request(&sample_config(), idle_db(), request).await;

    // Check the request was rejected
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_text(response).await, "event header missing");
    assert_eq!(triaged, vec![]);
}

#[tokio::test]
async fn test_event_reopened_enqueues_triage() {
    // Setup event for a reopened pull request
    let mut reopened = opened_event();
    reopened["action"] = json!("reopened");

    // Run the request
    let (response, triaged) = send_request(
        &sample_config(),
        idle_db(),
        pull_request_webhook_request(&reopened),
    )
    .await;

    // Check the reopened event was enqueued for validation triage
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(triaged.len(), 1);
    assert_eq!(
        unwrap_pr_triage(&triaged[0]).action,
        PullRequestEventAction::Reopened
    );
}

#[tokio::test]
async fn test_event_returns_service_unavailable_when_triage_queue_is_full() {
    // Setup router with a capacity-1 triage queue already full
    let (triage_tx, _triage_rx) = mpsc::channel(1);
    let router = setup_router(
        &sample_config(),
        Arc::new(idle_db()),
        LatestPrHeads::new(),
        triage_tx,
    )
    .unwrap();
    let first = pull_request_webhook_request(&opened_event());
    let second = pull_request_webhook_request(&opened_event());

    // First delivery fills the queue; second receives 503 backpressure
    let first_resp = router.clone().oneshot(first).await.unwrap();
    let second_resp = router.oneshot(second).await.unwrap();

    assert_eq!(first_resp.status(), StatusCode::OK);
    assert_eq!(second_resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[test]
fn test_find_target_org_matches_organization_repository_and_base_ref() {
    // Setup organizations
    let org = sample_org();
    let orgs = vec![org.clone()];

    // Run the target lookup
    let result = find_target_org(&orgs, ORG, REPO, BASE_REF);

    // Check matching organization target was found
    assert_eq!(result, Some(&org));
}

#[test]
fn test_find_target_org_rejects_wrong_base_ref() {
    // Setup organizations
    let orgs = vec![sample_org()];

    // Run the target lookup
    let result = find_target_org(&orgs, ORG, REPO, "feature");

    // Check no target was found
    assert_eq!(result, None);
}

#[test]
fn test_find_target_org_rejects_wrong_organization() {
    // Setup organizations
    let orgs = vec![sample_org()];

    // Run the target lookup
    let result = find_target_org(&orgs, "other", REPO, BASE_REF);

    // Check no target was found
    assert_eq!(result, None);
}

#[test]
fn test_find_target_org_rejects_wrong_repository() {
    // Setup organizations
    let orgs = vec![sample_org()];

    // Run the target lookup
    let result = find_target_org(&orgs, ORG, "other", BASE_REF);

    // Check no target was found
    assert_eq!(result, None);
}

#[test]
fn test_find_target_org_selects_matching_entry_among_several() {
    // Setup organizations
    let other_org = Organization {
        name: "other".to_string(),
        ..sample_org()
    };
    let org = sample_org();
    let orgs = vec![other_org, org.clone()];

    // Run the target lookup
    let result = find_target_org(&orgs, ORG, REPO, BASE_REF);

    // Check the matching organization target was selected
    assert_eq!(result, Some(&org));
}

#[tokio::test]
async fn test_list_organizations_returns_empty_list_without_organizations() {
    // Setup configuration without organizations
    let mut cfg = sample_config();
    cfg.organizations = None;

    // Run the request
    let request = get_request("/audit/api/organizations");
    let (response, _) = send_request(&cfg, idle_db(), request).await;

    // Check an empty list was returned
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_text(response).await, "[]");
}

#[tokio::test]
async fn test_list_organizations_returns_names_as_json() {
    // Run the request
    let request = get_request("/audit/api/organizations");
    let (response, _) = send_request(&sample_config(), idle_db(), request).await;

    // Check the response
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(cache_control(&response), Some("max-age=300"));
    assert_eq!(response.headers()[CONTENT_TYPE], "application/json");
    assert_eq!(body_text(response).await, format!(r#"["{ORG}"]"#));
}

#[tokio::test]
async fn test_search_changes_accepts_pagination_limits() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_search_changes()
        .withf(|input| input.limit == Some(100) && input.offset == Some(2_147_483_647))
        .times(1)
        .returning(|_| Box::pin(future::ready(Ok((0, "[]".to_string())))));

    // Run the request
    let request = get_request("/audit/api/changes/search?limit=100&offset=2147483647");
    let (response, _) = send_request(&sample_config(), db, request).await;

    // Check the search was accepted
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_search_changes_database_failure_returns_internal_error() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_search_changes()
        .times(1)
        .returning(|_| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Run the request
    let request = get_request("/audit/api/changes/search");
    let (response, _) = send_request(&sample_config(), db, request).await;

    // Check the error details are not exposed
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body_text(response).await, "");
}

#[tokio::test]
async fn test_search_changes_parses_web_query_and_returns_results() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_search_changes()
        .withf(|input| {
            *input
                == SearchChangesInput {
                    limit: Some(50),
                    offset: Some(100),
                    sort_by: Some("date".to_string()),
                    sort_direction: Some("desc".to_string()),
                    organization: Some("org1".to_string()),
                    service: Some(vec!["github".to_string()]),
                    kind: Some(vec!["repository-added".to_string(), "team-added".to_string()]),
                    applied_from: Some("2024-01-01 10:00:00+0000".to_string()),
                    applied_to: Some("2024-01-02 10:00:00+0000".to_string()),
                    pr_number: Some(vec![12]),
                    pr_merged_by: Some(vec!["user1".to_string()]),
                    applied_successfully: None,
                    ts_query_web: Some("hello".to_string()),
                }
        })
        .times(1)
        .returning(|_| Box::pin(future::ready(Ok((25, SEARCH_RESULTS.to_string())))));

    // Run the request using the query format built by the web application
    let request = get_request(
        "/audit/api/changes/search?limit=50&offset=100&sort_by=date&sort_direction=desc\
         &applied_from=2024-01-01%2010%3A00%3A00%2B0000&applied_to=2024-01-02%2010%3A00%3A00%2B0000\
         &organization=org1&ts_query_web=hello&service[0]=github&kind[0]=repository-added\
         &kind[1]=team-added&pr_number[0]=12&pr_merged_by[0]=user1",
    );
    let (response, _) = send_request(&sample_config(), db, request).await;

    // Check the response
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(cache_control(&response), Some("max-age=300"));
    assert_eq!(response.headers()[CONTENT_TYPE], "application/json");
    assert_eq!(response.headers()[PAGINATION_TOTAL_COUNT], "25");
    assert_eq!(body_text(response).await, SEARCH_RESULTS);
}

#[tokio::test]
async fn test_search_changes_rejects_invalid_query_before_searching() {
    // Setup queries that cannot be parsed, including numbers above usize::MAX
    let queries = [
        "limit=18446744073709551616",
        "limit=abc",
        "offset=18446744073709551616",
        "pr_number[0]=abc",
    ];

    for query in queries {
        // Run the request
        let request = get_request(&format!("/audit/api/changes/search?{query}"));
        let (response, _) = send_request(&sample_config(), idle_db(), request).await;

        // Check the request was rejected with a useful message
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
        assert_eq!(body_text(response).await, "invalid search query", "{query}");
    }
}

#[tokio::test]
async fn test_search_changes_rejects_pagination_out_of_range_before_searching() {
    // Setup queries that must be rejected and the expected error messages
    let cases = [
        ("limit=101", "limit must not be greater than 100"),
        ("limit=2147483648", "limit must not be greater than 100"),
        ("limit=18446744073709551615", "limit must not be greater than 100"),
        ("offset=2147483648", "offset must not be greater than 2147483647"),
        (
            "offset=18446744073709551615",
            "offset must not be greater than 2147483647",
        ),
    ];

    for (query, expected_message) in cases {
        // Run the request
        let request = get_request(&format!("/audit/api/changes/search?{query}"));
        let (response, _) = send_request(&sample_config(), idle_db(), request).await;

        // Check the request was rejected with a useful message
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
        assert_eq!(body_text(response).await, expected_message, "{query}");
    }
}

#[tokio::test]
async fn test_search_changes_uses_default_input_without_query() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_search_changes()
        .withf(|input| *input == SearchChangesInput::default())
        .times(1)
        .returning(|_| Box::pin(future::ready(Ok((0, "[]".to_string())))));

    // Run the request
    let request = get_request("/audit/api/changes/search");
    let (response, _) = send_request(&sample_config(), db, request).await;

    // Check the response
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[PAGINATION_TOTAL_COUNT], "0");
    assert_eq!(body_text(response).await, "[]");
}

#[tokio::test]
async fn test_setup_router_basic_auth_accepts_valid_credentials() {
    for route in PROTECTED_ROUTES {
        // Setup database expectations
        let mut db = MockDB::new();
        db.expect_search_changes()
            .times(usize::from(route == "/audit/api/changes/search"))
            .returning(|_| Box::pin(future::ready(Ok((0, "[]".to_string())))));

        // Run the request
        let mut request = get_request(route);
        let authorization = basic_authorization(BASIC_AUTH_USERNAME, BASIC_AUTH_PASSWORD);
        request.headers_mut().insert(AUTHORIZATION, authorization);
        let (response, _) = send_request(&basic_auth_config(true), db, request).await;

        // Check access was granted
        assert_eq!(response.status(), StatusCode::OK, "{route}");
    }
}

#[tokio::test]
async fn test_setup_router_basic_auth_disabled_leaves_audit_open() {
    // Setup configurations without basic auth
    let cfg_disabled = basic_auth_config(false);
    let cfg_not_set = sample_config();

    for cfg in [cfg_disabled, cfg_not_set] {
        // Run the request
        let request = get_request("/audit/api/organizations");
        let (response, _) = send_request(&cfg, idle_db(), request).await;

        // Check access was granted
        assert_eq!(response.status(), StatusCode::OK);
    }
}

#[tokio::test]
async fn test_setup_router_basic_auth_leaves_other_routes_open() {
    // Setup configuration with basic auth enabled
    let cfg = basic_auth_config(true);

    // Run the requests
    let health_check_request = get_request("/health-check");
    let (health_check_response, _) = send_request(&cfg, idle_db(), health_check_request).await;
    let webhook_request = webhook_request("ping", b"{}".to_vec(), WEBHOOK_SECRET);
    let (webhook_response, _) = send_request(&cfg, idle_db(), webhook_request).await;

    // Check credentials were not required
    assert_eq!(health_check_response.status(), StatusCode::OK);
    assert_eq!(webhook_response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_setup_router_basic_auth_rejects_missing_credentials() {
    for route in PROTECTED_ROUTES {
        // Run the request
        let request = get_request(route);
        let (response, _) = send_request(&basic_auth_config(true), idle_db(), request).await;

        // Check access was denied
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{route}");
        assert_eq!(response.headers()[WWW_AUTHENTICATE], "Basic", "{route}");
        assert_eq!(body_text(response).await, "", "{route}");
    }
}

#[tokio::test]
async fn test_setup_router_basic_auth_rejects_wrong_credentials() {
    for route in PROTECTED_ROUTES {
        // Run the request
        let mut request = get_request(route);
        let authorization = basic_authorization(BASIC_AUTH_USERNAME, "wrong-password");
        request.headers_mut().insert(AUTHORIZATION, authorization);
        let (response, _) = send_request(&basic_auth_config(true), idle_db(), request).await;

        // Check access was denied
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{route}");
        assert_eq!(response.headers()[WWW_AUTHENTICATE], "Basic", "{route}");
    }
}

#[tokio::test]
async fn test_setup_router_redirects_audit_to_trailing_slash() {
    // Run the request
    let request = get_request("/audit");
    let (response, _) = send_request(&sample_config(), idle_db(), request).await;

    // Check the redirection
    assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
    assert_eq!(response.headers()[LOCATION], "/audit/");
}

#[tokio::test]
async fn test_setup_router_rejects_get_on_webhook_route() {
    // Run the request
    let request = get_request("/webhook/github");
    let (response, _) = send_request(&sample_config(), idle_db(), request).await;

    // Check the method is not allowed
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn test_setup_router_serves_audit_index_with_short_cache() {
    for route in ["/audit/", "/audit/some/route"] {
        // Run the request
        let request = get_request(route);
        let (response, _) = send_request(&sample_config(), idle_db(), request).await;

        // Check the audit index was returned
        assert_eq!(response.status(), StatusCode::OK, "{route}");
        assert_eq!(
            cache_control(&response),
            Some(AUDIT_INDEX_CACHE_CONTROL),
            "{route}"
        );
        assert_eq!(body_text(response).await, AUDIT_INDEX, "{route}");
    }
}

#[tokio::test]
async fn test_setup_router_serves_root_index_without_cache_header() {
    for route in ["/", "/some/route"] {
        // Run the request
        let request = get_request(route);
        let (response, _) = send_request(&sample_config(), idle_db(), request).await;

        // Check the root index was returned
        assert_eq!(response.status(), StatusCode::OK, "{route}");
        assert_eq!(cache_control(&response), None, "{route}");
        assert_eq!(body_text(response).await, ROOT_INDEX, "{route}");
    }
}

#[tokio::test]
async fn test_setup_router_serves_static_files_with_long_cache() {
    // Setup the file expected from each static files mount
    let cases = [
        ("/audit/static/app.js", AUDIT_APP_JS),
        ("/static/audit/app.js", AUDIT_APP_JS),
        ("/static/index.html", ROOT_INDEX),
    ];

    for (route, expected_body) in cases {
        // Run the request
        let request = get_request(route);
        let (response, _) = send_request(&sample_config(), idle_db(), request).await;

        // Check the file was returned
        assert_eq!(response.status(), StatusCode::OK, "{route}");
        assert_eq!(cache_control(&response), Some(STATIC_CACHE_CONTROL), "{route}");
        assert_eq!(body_text(response).await, expected_body, "{route}");
    }
}

#[tokio::test]
async fn test_setup_router_static_file_not_found_is_not_cached() {
    for route in ["/audit/static/missing.js", "/static/missing.js"] {
        // Run the request
        let request = get_request(route);
        let (response, _) = send_request(&sample_config(), idle_db(), request).await;

        // Check the response does not carry the static files cache header
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{route}");
        assert_eq!(cache_control(&response), None, "{route}");
    }
}

#[tokio::test]
async fn test_setup_router_static_file_not_modified_keeps_long_cache() {
    for route in ["/audit/static/app.js", "/static/audit/app.js"] {
        // Get the file's last modification date
        let request = get_request(route);
        let (response, _) = send_request(&sample_config(), idle_db(), request).await;
        let last_modified = response.headers()[LAST_MODIFIED].clone();

        // Run the conditional request
        let mut request = get_request(route);
        request.headers_mut().insert(IF_MODIFIED_SINCE, last_modified);
        let (response, _) = send_request(&sample_config(), idle_db(), request).await;

        // Check the cached file can continue to be used
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED, "{route}");
        assert_eq!(cache_control(&response), Some(STATIC_CACHE_CONTROL), "{route}");
    }
}

#[test]
fn test_verify_signature_accepts_fallback_secret() {
    // Setup signature using the fallback secret
    let signature = signature_header(b"body", WEBHOOK_SECRET_FALLBACK);

    // Verify the signature
    let result = verify_signature(
        Some(&signature),
        WEBHOOK_SECRET.as_bytes(),
        Some(WEBHOOK_SECRET_FALLBACK.as_bytes()),
        b"body",
    );

    // Check the signature was accepted
    assert!(result.is_ok());
}

#[test]
fn test_verify_signature_accepts_primary_secret() {
    // Setup signature using the primary secret
    let signature = signature_header(b"body", WEBHOOK_SECRET);

    // Verify the signature with and without a fallback secret available
    let without_fallback = verify_signature(Some(&signature), WEBHOOK_SECRET.as_bytes(), None, b"body");
    let with_fallback = verify_signature(
        Some(&signature),
        WEBHOOK_SECRET.as_bytes(),
        Some(WEBHOOK_SECRET_FALLBACK.as_bytes()),
        b"body",
    );

    // Check the signature was accepted
    assert!(without_fallback.is_ok());
    assert!(with_fallback.is_ok());
}

#[test]
fn test_verify_signature_rejects_malformed_signatures() {
    // Setup signature values that cannot be decoded
    let valid_signature = signature(b"body", WEBHOOK_SECRET);
    let without_prefix = valid_signature.strip_prefix("sha256=").unwrap().to_string();
    let cases = [
        ("missing", None),
        ("non hex", Some("sha256=not-hex".to_string())),
        ("sha1 prefix", Some(format!("sha1={without_prefix}"))),
        ("without prefix", Some(without_prefix.clone())),
    ];

    for (name, signature) in cases {
        // Verify the signature
        let signature = signature.map(|value| HeaderValue::try_from(value).unwrap());
        let result = verify_signature(signature.as_ref(), WEBHOOK_SECRET.as_bytes(), None, b"body");

        // Check the signature was rejected
        assert_eq!(
            result.unwrap_err().to_string(),
            "no valid signature found",
            "{name}"
        );
    }
}

#[test]
fn test_verify_signature_rejects_mismatched_signatures() {
    // Setup signature truncated to 16 bytes
    let valid_signature = signature(b"body", WEBHOOK_SECRET);
    let truncated_signature = HeaderValue::try_from(&valid_signature[.."sha256=".len() + 32]).unwrap();

    // Setup scenarios that must be rejected
    let wrong_secret_signature = signature_header(b"body", "wrong-secret");
    let primary_signature = signature_header(b"body", WEBHOOK_SECRET);
    let fallback = Some(WEBHOOK_SECRET_FALLBACK.as_bytes());
    let cases = [
        ("tampered body", &primary_signature, None, &b"tampered"[..]),
        ("truncated", &truncated_signature, None, &b"body"[..]),
        (
            "truncated with fallback",
            &truncated_signature,
            fallback,
            &b"body"[..],
        ),
        ("wrong secret", &wrong_secret_signature, None, &b"body"[..]),
        (
            "wrong secret with fallback",
            &wrong_secret_signature,
            fallback,
            &b"body"[..],
        ),
    ];

    for (name, signature, secret_fallback, body) in cases {
        // Verify the signature
        let result = verify_signature(Some(signature), WEBHOOK_SECRET.as_bytes(), secret_fallback, body);

        // Check the signature was rejected
        assert!(result.unwrap_err().is::<MacError>(), "{name}");
    }
}

// Helpers.

/// Function that adjusts an event to represent a given scenario.
type AdjustEvent = fn(&mut Value);

/// Returns a configuration with basic auth set up in the state provided.
fn basic_auth_config(enabled: bool) -> Config {
    let mut cfg = sample_config();
    cfg.server.basic_auth = Some(BasicAuth {
        enabled,
        username: BASIC_AUTH_USERNAME.to_string(),
        password: BASIC_AUTH_PASSWORD.to_string(),
    });
    cfg
}

/// Returns the authorization header value for the credentials provided.
fn basic_authorization(username: &str, password: &str) -> HeaderValue {
    let credentials = base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
    HeaderValue::try_from(format!("Basic {credentials}")).unwrap()
}

/// Returns the response's body as text.
async fn body_text(response: Response<Body>) -> String {
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    String::from_utf8(body.to_vec()).unwrap()
}

/// Returns the response's cache control header value, if any.
fn cache_control(response: &Response<Body>) -> Option<&str> {
    response.headers().get(CACHE_CONTROL).map(|value| value.to_str().unwrap())
}

/// Returns a GET request for the uri provided.
fn get_request(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

/// Returns a database mock that does not expect any call.
fn idle_db() -> MockDB {
    let mut db = MockDB::new();
    db.expect_get_reconciliation_schedule_state().never();
    db.expect_register_reconciliation().never();
    db.expect_search_changes().never();
    db
}

/// Returns the recorded event adjusted to represent a merged pull request.
fn merged_event() -> Value {
    let mut event = opened_event();
    event["action"] = json!("closed");
    event["pull_request"]["merged"] = json!(true);
    event["pull_request"]["merged_at"] = json!(PR_MERGED_AT);
    event["pull_request"]["merged_by"] = event["pull_request"]["user"].clone();
    event["pull_request"]["merged_by"]["login"] = json!(PR_MERGED_BY);
    event
}

/// Returns the recorded opened pull request event.
fn opened_event() -> Value {
    serde_json::from_str(OPENED_EVENT).unwrap()
}

/// Returns a pull request webhook request for the event provided, signed with
/// the primary secret.
fn pull_request_webhook_request(event: &Value) -> Request<Body> {
    webhook_request("pull_request", serde_json::to_vec(event).unwrap(), WEBHOOK_SECRET)
}

/// Returns a configuration that uses the static files fixtures and manages the
/// organization under test.
fn sample_config() -> Config {
    Config {
        db: Db::default(),
        log: Log {
            format: LogFormat::Json,
        },
        server: HttpServer {
            addr: "127.0.0.1:9000".to_string(),
            github_app: GitHubApp {
                webhook_secret: WEBHOOK_SECRET.to_string(),
                ..GitHubApp::default()
            },
            reconcile_frequency: DEFAULT_RECONCILE_FREQUENCY,
            static_path: STATIC_PATH.into(),
            basic_auth: None,
        },
        services: Services {
            github: Service { enabled: true },
        },
        organizations: Some(vec![sample_org()]),
    }
}

/// Returns the organization under test.
fn sample_org() -> Organization {
    Organization {
        name: ORG.to_string(),
        installation_id: INSTALLATION_ID,
        repository: REPO.to_string(),
        branch: BASE_REF.to_string(),
        legacy: Legacy {
            enabled: true,
            sheriff_permissions_path: "config.yaml".to_string(),

            cncf_people_path: Some("people.yaml".to_string()),
        },
    }
}

/// Sends the request provided to a router set up with the configuration and
/// database mock provided, returning the response and any queued triage items.
async fn send_request(
    cfg: &Config,
    db: MockDB,
    request: Request<Body>,
) -> (Response<Body>, Vec<TriageInput>) {
    // Setup router
    let (triage_tx, mut triage_rx) = mpsc::channel(16);
    let router = setup_router(cfg, Arc::new(db), LatestPrHeads::new(), triage_tx).unwrap();

    // Run the request
    let response = router.oneshot(request).await.unwrap();

    // Collect queued triage inputs
    let mut triaged = vec![];
    while let Ok(item) = triage_rx.try_recv() {
        triaged.push(item);
    }

    (response, triaged)
}

/// Extract a [`PullRequestTriageInput`] from a [`TriageInput`] in tests.
fn unwrap_pr_triage(item: &TriageInput) -> &PullRequestTriageInput {
    match item {
        TriageInput::PullRequest(input) => input,
        TriageInput::CheckRunRerequested(_) => panic!("expected PullRequest triage input"),
    }
}

/// Returns the signature of the body provided using the secret given.
fn signature(body: &[u8], secret: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    format!("sha256={}", hex::encode(&mac.finalize().into_bytes()[..]))
}

/// Returns the signature of the body provided as a header value.
fn signature_header(body: &[u8], secret: &str) -> HeaderValue {
    HeaderValue::try_from(signature(body, secret)).unwrap()
}

/// Returns a webhook request for the event and body provided, signed with the
/// secret given.
fn webhook_request(event_name: &str, body: Vec<u8>, secret: &str) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri("/webhook/github")
        .header(GITHUB_EVENT_HEADER, event_name)
        .header(GITHUB_SIGNATURE_HEADER, signature(&body, secret))
        .body(Body::from(body))
        .unwrap()
}
