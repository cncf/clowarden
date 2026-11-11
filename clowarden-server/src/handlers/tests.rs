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
use octorust::types::ChecksCreateRequest;
use serde_json::{Value, json};
use time::OffsetDateTime;
use tower::ServiceExt;

use clowarden_core::cfg::{GitHubApp, Legacy, Service, Services};

use crate::{
    cfg::{BasicAuth, HttpServer, Log, LogFormat},
    db::MockDB,
    github::MockGH,
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
async fn test_create_no_config_changes_check_failure() {
    // Setup GitHub mock
    let mut gh = MockGH::new();
    gh.expect_create_check_run()
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Run the workflow
    let result = create_no_config_changes_check(Arc::new(gh), &sample_org(), HEAD_SHA).await;

    // Check error is propagated
    assert_eq!(result.unwrap_err().to_string(), ERROR);
}

#[tokio::test]
async fn test_create_no_config_changes_check_success() {
    // Setup GitHub mock
    let mut gh = MockGH::new();
    gh.expect_create_check_run()
        .withf(|ctx, body| {
            ctx.inst_id == INSTALLATION_ID
                && ctx.owner == ORG
                && ctx.repo == REPO
                && body.conclusion == Some(ChecksCreateRequestConclusion::Success)
                && body.head_sha == HEAD_SHA
                && body.name == "CLOWarden"
                && body.output.as_ref().is_some_and(|output| {
                    output.summary == NO_CONFIG_CHANGES_MSG && output.title == NO_CONFIG_CHANGES_MSG
                })
                && body.status == Some(JobStatus::Completed)
        })
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(()))));

    // Run the workflow
    let result = create_no_config_changes_check(Arc::new(gh), &sample_org(), HEAD_SHA).await;

    // Check check run was created
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_event_accepts_fallback_secret_signature() {
    // Setup configuration and GitHub expectations
    let mut cfg = sample_config();
    cfg.server.github_app.webhook_secret_fallback = Some(WEBHOOK_SECRET_FALLBACK.to_string());
    let mut gh = MockGH::new();
    expect_pr_files(&mut gh, &["config.yaml"]);
    expect_check_run(&mut gh, is_in_progress_check, Ok(()));

    // Run the request
    let body = serde_json::to_vec(&opened_event()).unwrap();
    let request = webhook_request("pull_request", body, WEBHOOK_SECRET_FALLBACK);
    let (response, jobs) = send_request(&cfg, idle_db(), gh, request).await;

    // Check the event was processed
    assert_eq!(response.status(), StatusCode::OK);
    assert!(matches!(jobs.as_slice(), [Job::Validate(_)]));
}

#[tokio::test]
async fn test_event_closed_and_merged_config_change_enqueues_reconcile() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_pr_files(&mut gh, &["config.yaml"]);
    gh.expect_create_check_run().never();

    // Run the request
    let request = pull_request_webhook_request(&merged_event());
    let (response, jobs) = send_request(&sample_config(), idle_db(), gh, request).await;

    // Check a reconcile job with the pull request details was queued
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        jobs,
        vec![Job::Reconcile(ReconcileInput {
            org: sample_org(),
            pr_number: Some(PR_NUMBER),
            pr_created_by: Some(PR_CREATED_BY.to_string()),
            pr_merged_by: Some(PR_MERGED_BY.to_string()),
            pr_merged_at: Some(OffsetDateTime::from_unix_timestamp(PR_MERGED_AT_TIMESTAMP).unwrap()),
        })]
    );
}

#[tokio::test]
async fn test_event_closed_and_merged_without_config_change_does_nothing() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_pr_files(&mut gh, &["docs/README.md"]);
    gh.expect_create_check_run().never();

    // Run the request
    let request = pull_request_webhook_request(&merged_event());
    let (response, jobs) = send_request(&sample_config(), idle_db(), gh, request).await;

    // Check no job was queued
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(jobs, vec![]);
}

#[tokio::test]
async fn test_event_closed_unmerged_config_change_does_nothing() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_pr_files(&mut gh, &["config.yaml"]);
    gh.expect_create_check_run().never();

    // Setup event for a pull request closed without merging
    let mut event = opened_event();
    event["action"] = json!("closed");

    // Run the request
    let request = pull_request_webhook_request(&event);
    let (response, jobs) = send_request(&sample_config(), idle_db(), gh, request).await;

    // Check no job was queued
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(jobs, vec![]);
}

#[tokio::test]
async fn test_event_config_change_check_failure_still_enqueues_validate() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_pr_files(&mut gh, &["config.yaml"]);
    expect_check_run(&mut gh, is_in_progress_check, Err(ERROR));

    // Run the request
    let request = pull_request_webhook_request(&opened_event());
    let (response, jobs) = send_request(&sample_config(), idle_db(), gh, request).await;

    // Check the validation job was queued anyway
    assert_eq!(response.status(), StatusCode::OK);
    assert!(matches!(jobs.as_slice(), [Job::Validate(_)]));
}

#[tokio::test]
async fn test_event_config_change_enqueues_validate_with_in_progress_check() {
    for action in ["opened", "synchronize"] {
        // Setup GitHub expectations
        let mut gh = MockGH::new();
        expect_pr_files(&mut gh, &["docs/README.md", "config.yaml"]);
        expect_check_run(&mut gh, is_in_progress_check, Ok(()));

        // Setup event for a pull request created from a fork
        let mut event = opened_event();
        event["action"] = json!(action);
        event["pull_request"]["head"]["ref"] = json!("feature");
        event["pull_request"]["head"]["repo"]["name"] = json!("forked-repo");
        event["pull_request"]["head"]["repo"]["owner"]["login"] = json!("forker");

        // Run the request
        let request = pull_request_webhook_request(&event);
        let (response, jobs) = send_request(&sample_config(), idle_db(), gh, request).await;

        // Check a validate job with the pull request details was queued
        assert_eq!(response.status(), StatusCode::OK, "{action}");
        assert_eq!(
            jobs,
            vec![Job::Validate(ValidateInput {
                org: sample_org(),
                pr_number: PR_NUMBER,
                pr_head_owner: Some("forker".to_string()),
                pr_head_repo: Some("forked-repo".to_string()),
                pr_head_ref: "feature".to_string(),
                pr_head_sha: HEAD_SHA.to_string(),
            })],
            "{action}"
        );
    }
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
        let (response, jobs) = send_request(&sample_config(), idle_db(), idle_gh(), request).await;

        // Check the event was ignored
        assert_eq!(response.status(), StatusCode::OK, "{name}");
        assert_eq!(jobs, vec![], "{name}");
    }
}

#[tokio::test]
async fn test_event_ignores_unsupported_event() {
    // Run the request
    let request = webhook_request("ping", b"{}".to_vec(), WEBHOOK_SECRET);
    let (response, jobs) = send_request(&sample_config(), idle_db(), idle_gh(), request).await;

    // Check the event was ignored
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(jobs, vec![]);
}

#[tokio::test]
async fn test_event_list_files_failure_reports_nothing() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_list_pr_files()
        .withf(|ctx, pr_number| ctx.inst_id == INSTALLATION_ID && *pr_number == PR_NUMBER)
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    gh.expect_create_check_run().never();

    // Run the request
    let request = pull_request_webhook_request(&opened_event());
    let (response, jobs) = send_request(&sample_config(), idle_db(), gh, request).await;

    // Check no check run was posted on the pull request not inspected
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(jobs, vec![]);
}

#[tokio::test]
async fn test_event_no_config_change_check_failure_returns_ok_without_job() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_pr_files(&mut gh, &["docs/README.md"]);
    expect_check_run(&mut gh, is_no_config_changes_check, Err(ERROR));

    // Run the request
    let request = pull_request_webhook_request(&opened_event());
    let (response, jobs) = send_request(&sample_config(), idle_db(), gh, request).await;

    // Check the failure did not affect the response
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(jobs, vec![]);
}

#[tokio::test]
async fn test_event_no_config_change_reports_success_check_without_job() {
    for action in ["opened", "synchronize"] {
        // Setup GitHub expectations
        let mut gh = MockGH::new();
        expect_pr_files(&mut gh, &["docs/README.md"]);
        expect_check_run(&mut gh, is_no_config_changes_check, Ok(()));

        // Setup event
        let mut event = opened_event();
        event["action"] = json!(action);

        // Run the request
        let request = pull_request_webhook_request(&event);
        let (response, jobs) = send_request(&sample_config(), idle_db(), gh, request).await;

        // Check no job was queued
        assert_eq!(response.status(), StatusCode::OK, "{action}");
        assert_eq!(jobs, vec![], "{action}");
    }
}

#[tokio::test]
async fn test_event_rejects_invalid_signature_before_side_effects() {
    // Setup request signed with the wrong secret
    let body = serde_json::to_vec(&merged_event()).unwrap();
    let request = webhook_request("pull_request", body, "wrong-secret");

    // Run the request
    let (response, jobs) = send_request(&sample_config(), idle_db(), idle_gh(), request).await;

    // Check the request was rejected without side effects
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_text(response).await, "no valid signature found");
    assert_eq!(jobs, vec![]);
}

#[tokio::test]
async fn test_event_rejects_malformed_body() {
    // Run the request
    let request = webhook_request("pull_request", b"not json".to_vec(), WEBHOOK_SECRET);
    let (response, jobs) = send_request(&sample_config(), idle_db(), idle_gh(), request).await;

    // Check the request was rejected
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_text(response).await.starts_with("invalid body: "));
    assert_eq!(jobs, vec![]);
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
    let (response, jobs) = send_request(&sample_config(), idle_db(), idle_gh(), request).await;

    // Check the request was rejected
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_text(response).await, "event header missing");
    assert_eq!(jobs, vec![]);
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
    let (response, _) = send_request(&cfg, idle_db(), idle_gh(), request).await;

    // Check an empty list was returned
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_text(response).await, "[]");
}

#[tokio::test]
async fn test_list_organizations_returns_names_as_json() {
    // Run the request
    let request = get_request("/audit/api/organizations");
    let (response, _) = send_request(&sample_config(), idle_db(), idle_gh(), request).await;

    // Check the response
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(cache_control(&response), Some("max-age=300"));
    assert_eq!(response.headers()[CONTENT_TYPE], "application/json");
    assert_eq!(body_text(response).await, format!(r#"["{ORG}"]"#));
}

#[tokio::test]
async fn test_pr_updates_config_legacy_file_changed() {
    // Setup GitHub mock
    let mut gh = MockGH::new();
    gh.expect_list_pr_files()
        .withf(|ctx, pr_number| {
            ctx.inst_id == INSTALLATION_ID && ctx.owner == ORG && ctx.repo == REPO && *pr_number == PR_NUMBER
        })
        .times(1)
        .returning(|_, _| {
            Box::pin(future::ready(Ok(vec![
                "docs/README.md".to_string(),
                "config.yaml".to_string(),
            ])))
        });

    // Run the workflow
    let result = pr_updates_config(Arc::new(gh), &sample_org(), REPO, BASE_REF, PR_NUMBER).await;

    // Check configuration update was detected
    assert_eq!(result.unwrap(), PullRequestConfigChanges::Changed);
}

#[tokio::test]
async fn test_pr_updates_config_legacy_file_not_changed() {
    // Setup GitHub mock
    let mut gh = MockGH::new();
    gh.expect_list_pr_files()
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(vec!["docs/README.md".to_string()]))));

    // Run the workflow
    let result = pr_updates_config(Arc::new(gh), &sample_org(), REPO, BASE_REF, PR_NUMBER).await;

    // Check configuration update was not detected
    assert_eq!(result.unwrap(), PullRequestConfigChanges::Unchanged);
}

#[tokio::test]
async fn test_pr_updates_config_own_guard_rejects_wrong_base_ref() {
    // Setup GitHub mock
    let mut gh = MockGH::new();
    gh.expect_list_pr_files().never();

    // Run the workflow
    let result = pr_updates_config(Arc::new(gh), &sample_org(), REPO, "feature", PR_NUMBER).await;

    // Check pull request was treated as a non-target
    assert_eq!(result.unwrap(), PullRequestConfigChanges::NotTarget);
}

#[tokio::test]
async fn test_pr_updates_config_own_guard_rejects_wrong_repository() {
    // Setup GitHub mock
    let mut gh = MockGH::new();
    gh.expect_list_pr_files().never();

    // Run the workflow
    let result = pr_updates_config(Arc::new(gh), &sample_org(), "other", BASE_REF, PR_NUMBER).await;

    // Check pull request was treated as a non-target
    assert_eq!(result.unwrap(), PullRequestConfigChanges::NotTarget);
}

#[tokio::test]
async fn test_pr_updates_config_people_file_changed() {
    // Setup GitHub mock
    let mut gh = MockGH::new();
    expect_pr_files(&mut gh, &["people.yaml"]);

    // Run the workflow
    let result = pr_updates_config(Arc::new(gh), &sample_org(), REPO, BASE_REF, PR_NUMBER).await;

    // Check configuration update was detected
    assert_eq!(result.unwrap(), PullRequestConfigChanges::Changed);
}

#[tokio::test]
async fn test_pr_updates_config_people_file_ignored_without_people_path() {
    // Setup organization without people file
    let mut org = sample_org();
    org.legacy.cncf_people_path = None;

    // Setup GitHub mock
    let mut gh = MockGH::new();
    expect_pr_files(&mut gh, &["people.yaml"]);

    // Run the workflow
    let result = pr_updates_config(Arc::new(gh), &org, REPO, BASE_REF, PR_NUMBER).await;

    // Check configuration update was not detected
    assert_eq!(result.unwrap(), PullRequestConfigChanges::Unchanged);
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
    let (response, _) = send_request(&sample_config(), db, idle_gh(), request).await;

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
    let (response, _) = send_request(&sample_config(), db, idle_gh(), request).await;

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
    let (response, _) = send_request(&sample_config(), db, idle_gh(), request).await;

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
        let (response, _) = send_request(&sample_config(), idle_db(), idle_gh(), request).await;

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
        let (response, _) = send_request(&sample_config(), idle_db(), idle_gh(), request).await;

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
    let (response, _) = send_request(&sample_config(), db, idle_gh(), request).await;

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
        let (response, _) = send_request(&basic_auth_config(true), db, idle_gh(), request).await;

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
        let (response, _) = send_request(&cfg, idle_db(), idle_gh(), request).await;

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
    let (health_check_response, _) = send_request(&cfg, idle_db(), idle_gh(), health_check_request).await;
    let webhook_request = webhook_request("ping", b"{}".to_vec(), WEBHOOK_SECRET);
    let (webhook_response, _) = send_request(&cfg, idle_db(), idle_gh(), webhook_request).await;

    // Check credentials were not required
    assert_eq!(health_check_response.status(), StatusCode::OK);
    assert_eq!(webhook_response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_setup_router_basic_auth_rejects_missing_credentials() {
    for route in PROTECTED_ROUTES {
        // Run the request
        let request = get_request(route);
        let (response, _) = send_request(&basic_auth_config(true), idle_db(), idle_gh(), request).await;

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
        let (response, _) = send_request(&basic_auth_config(true), idle_db(), idle_gh(), request).await;

        // Check access was denied
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{route}");
        assert_eq!(response.headers()[WWW_AUTHENTICATE], "Basic", "{route}");
    }
}

#[tokio::test]
async fn test_setup_router_redirects_audit_to_trailing_slash() {
    // Run the request
    let request = get_request("/audit");
    let (response, _) = send_request(&sample_config(), idle_db(), idle_gh(), request).await;

    // Check the redirection
    assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
    assert_eq!(response.headers()[LOCATION], "/audit/");
}

#[tokio::test]
async fn test_setup_router_rejects_get_on_webhook_route() {
    // Run the request
    let request = get_request("/webhook/github");
    let (response, _) = send_request(&sample_config(), idle_db(), idle_gh(), request).await;

    // Check the method is not allowed
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn test_setup_router_serves_audit_index_with_short_cache() {
    for route in ["/audit/", "/audit/some/route"] {
        // Run the request
        let request = get_request(route);
        let (response, _) = send_request(&sample_config(), idle_db(), idle_gh(), request).await;

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
        let (response, _) = send_request(&sample_config(), idle_db(), idle_gh(), request).await;

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
        let (response, _) = send_request(&sample_config(), idle_db(), idle_gh(), request).await;

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
        let (response, _) = send_request(&sample_config(), idle_db(), idle_gh(), request).await;

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
        let (response, _) = send_request(&sample_config(), idle_db(), idle_gh(), request).await;
        let last_modified = response.headers()[LAST_MODIFIED].clone();

        // Run the conditional request
        let mut request = get_request(route);
        request.headers_mut().insert(IF_MODIFIED_SINCE, last_modified);
        let (response, _) = send_request(&sample_config(), idle_db(), idle_gh(), request).await;

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

/// Sets up the expectation of a single check run for the organization under
/// test that satisfies the predicate provided.
fn expect_check_run(
    gh: &mut MockGH,
    predicate: fn(&ChecksCreateRequest) -> bool,
    result: Result<(), &'static str>,
) {
    gh.expect_create_check_run()
        .withf(move |ctx, body| {
            ctx.inst_id == INSTALLATION_ID && ctx.owner == ORG && ctx.repo == REPO && predicate(body)
        })
        .times(1)
        .returning(move |_, _| Box::pin(future::ready(result.map_err(|err| format_err!(err)))));
}

/// Sets up the expectation of a single request for the files in the pull
/// request under test.
fn expect_pr_files(gh: &mut MockGH, files: &'static [&'static str]) {
    gh.expect_list_pr_files()
        .withf(|ctx, pr_number| {
            ctx.inst_id == INSTALLATION_ID && ctx.owner == ORG && ctx.repo == REPO && *pr_number == PR_NUMBER
        })
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(files.iter().map(ToString::to_string).collect()))));
}

/// Returns a GET request for the uri provided.
fn get_request(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

/// Returns a database mock that does not expect any call.
fn idle_db() -> MockDB {
    let mut db = MockDB::new();
    db.expect_register_reconciliation().never();
    db.expect_search_changes().never();
    db
}

/// Returns a GitHub mock that does not expect any call.
fn idle_gh() -> MockGH {
    let mut gh = MockGH::new();
    gh.expect_create_check_run().never();
    gh.expect_list_pr_files().never();
    gh.expect_post_comment().never();
    gh
}

/// Checks if the request provided describes the in-progress validation check.
fn is_in_progress_check(body: &ChecksCreateRequest) -> bool {
    body.conclusion.is_none()
        && body.head_sha == HEAD_SHA
        && body.name == "CLOWarden"
        && body.status == Some(JobStatus::InProgress)
}

/// Checks if the request provided describes the no-configuration-changes check.
fn is_no_config_changes_check(body: &ChecksCreateRequest) -> bool {
    body.conclusion == Some(ChecksCreateRequestConclusion::Success)
        && body.head_sha == HEAD_SHA
        && body.name == "CLOWarden"
        && body.status == Some(JobStatus::Completed)
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
            static_path: STATIC_PATH.into(),
            basic_auth: None,
            github_app: GitHubApp {
                webhook_secret: WEBHOOK_SECRET.to_string(),
                ..GitHubApp::default()
            },
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
/// mocks provided, returning the response and the jobs queued.
async fn send_request(
    cfg: &Config,
    db: MockDB,
    gh: MockGH,
    request: Request<Body>,
) -> (Response<Body>, Vec<Job>) {
    // Setup router
    let (jobs_tx, mut jobs_rx) = mpsc::unbounded_channel();
    let router = setup_router(cfg, Arc::new(db), Arc::new(gh), jobs_tx).unwrap();

    // Run the request
    let response = router.oneshot(request).await.unwrap();

    // Collect queued jobs
    let mut jobs = vec![];
    while let Ok(job) = jobs_rx.try_recv() {
        jobs.push(job);
    }

    (response, jobs)
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
