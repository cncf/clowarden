use std::time::Duration;

use clowarden_core::github::{
    RateLimitPolicy,
    http_fake::{FakeGitHub, Request, Route},
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::*;

const CHECK_RUNS_PATH: &str = "/repos/Octocoders/Hello-World/check-runs";
/// Closed pull request event recorded by GitHub, as published in the
/// `payload-examples` directory of the `octokit/webhooks` repository.
const CLOSED_EVENT: &str = include_str!("../testdata/webhooks/pull-request-closed.json");
const COMMENTS_PATH: &str = "/repos/Octocoders/Hello-World/issues/2/comments";
const HEAD_SHA: &str = "ec26c3e57ca3a959ca5aad62de7213c562f8c821";
const INSTALLATION_ID: i64 = 1;
/// Labeled pull request event recorded by GitHub, from the same source.
const LABELED_EVENT: &str = include_str!("../testdata/webhooks/pull-request-labeled.json");
/// Opened pull request event recorded by GitHub, from the same source.
const OPENED_EVENT: &str = include_str!("../testdata/webhooks/pull-request-opened.json");
const ORG: &str = "Octocoders";
const PR_FILES_PATH: &str = "/repos/Octocoders/Hello-World/pulls/2/files";
const PR_NUMBER: i64 = 2;
const REPO: &str = "Hello-World";

#[test]
fn test_diff_entries_file_names_includes_previous_name_of_renamed_files() {
    // Setup diff entries as reported by GitHub
    let entries: Vec<DiffEntry> = serde_json::from_value(json!([
        {
            "filename": "old-config.yaml",
            "previous_filename": "config.yaml",
            "status": "renamed",
        },
        {
            "filename": "docs/README.md",
            "status": "modified",
        },
    ]))
    .unwrap();

    // Extract the file names
    let file_names = diff_entries_file_names(&entries);

    // Check both names of the renamed file are present, and no empty names
    assert_eq!(
        file_names,
        vec!["old-config.yaml", "config.yaml", "docs/README.md"]
    );
}

#[test]
fn test_event_try_from_accepts_absent_organization() {
    // Setup event from a repository owned by a user
    let mut payload = recorded_event(OPENED_EVENT);
    payload.as_object_mut().unwrap().remove("organization");

    // Parse the event
    let event = parse_pull_request_event(&payload);

    // Check the event was parsed without an organization
    assert_eq!(event.organization, None);
}

#[test]
fn test_event_try_from_maps_unknown_action_to_other() {
    // Parse the event
    let event = parse_pull_request_event(&recorded_event(LABELED_EVENT));

    // Check the action was not treated as an error
    assert_eq!(event.action, PullRequestEventAction::Other);
}

#[test]
fn test_event_try_from_parses_check_run_rerequested_event() {
    let payload = json!({
        "action": "rerequested",
        "check_run": {
            "name": CHECK_RUN_NAME,
            "head_sha": HEAD_SHA,
            "pull_requests": [
                {
                    "number": PR_NUMBER,
                    "base": { "ref": "master", "sha": "base-sha" },
                    "head": { "ref": "feature", "sha": HEAD_SHA }
                }
            ]
        },
        "organization": {
            "id": 1,
            "login": ORG,
            "node_id": "node",
            "url": "",
            "repos_url": "",
            "events_url": "",
            "hooks_url": "",
            "issues_url": "",
            "members_url": "",
            "public_members_url": "",
            "avatar_url": ""
        },
        "repository": recorded_event(OPENED_EVENT)["repository"]
    });
    let header = Some(HeaderValue::from_static("check_run"));
    let body = serde_json::to_vec(&payload).unwrap();

    let Event::CheckRun(event) = Event::try_from((&header, &body[..])).unwrap() else {
        panic!("expected CheckRun event");
    };
    assert_eq!(event.action, CheckRunEventAction::Rerequested);
    assert_eq!(event.check_run.name, CHECK_RUN_NAME);
    assert_eq!(event.check_run.head_sha, HEAD_SHA);
    assert_eq!(event.check_run.pull_requests.len(), 1);
    assert_eq!(event.check_run.pull_requests[0].number, PR_NUMBER);
}

#[test]
fn test_event_try_from_parses_check_suite_rerequested_event() {
    let payload = json!({
        "action": "rerequested",
        "check_suite": {
            "head_sha": HEAD_SHA,
            "pull_requests": [
                {
                    "number": PR_NUMBER,
                    "base": { "ref": "master", "sha": "base-sha" },
                    "head": { "ref": "feature", "sha": HEAD_SHA }
                }
            ]
        },
        "organization": {
            "id": 1,
            "login": ORG,
            "node_id": "node",
            "url": "",
            "repos_url": "",
            "events_url": "",
            "hooks_url": "",
            "issues_url": "",
            "members_url": "",
            "public_members_url": "",
            "avatar_url": ""
        },
        "repository": recorded_event(OPENED_EVENT)["repository"]
    });
    let header = Some(HeaderValue::from_static("check_suite"));
    let body = serde_json::to_vec(&payload).unwrap();

    let Event::CheckSuite(event) = Event::try_from((&header, &body[..])).unwrap() else {
        panic!("expected CheckSuite event");
    };
    assert_eq!(event.action, CheckRunEventAction::Rerequested);
    assert_eq!(event.check_suite.head_sha, HEAD_SHA);
    assert_eq!(event.check_suite.pull_requests.len(), 1);
    assert_eq!(event.check_suite.pull_requests[0].number, PR_NUMBER);
}

#[test]
fn test_event_try_from_parses_closed_event() {
    // Parse the event
    let event = parse_pull_request_event(&recorded_event(CLOSED_EVENT));

    // Check the event fields used to decide on the action to take
    assert_eq!(event.action, PullRequestEventAction::Closed);
    assert_eq!(event.number, PR_NUMBER);
    assert!(!event.pull_request.merged);
    assert_eq!(event.pull_request.merged_by, None);
}

#[test]
fn test_event_try_from_parses_edited_and_reopened_events() {
    // Parse edited event with base branch change
    let mut edited = recorded_event(OPENED_EVENT);
    edited["action"] = json!("edited");
    edited["changes"] = json!({
        "base": {
            "ref": { "from": "develop" },
            "sha": { "from": "0000000" }
        }
    });
    let event = parse_pull_request_event(&edited);
    assert_eq!(event.action, PullRequestEventAction::Edited);
    assert!(event.changes.unwrap().base.is_some());

    // Parse reopened event
    let mut reopened = recorded_event(OPENED_EVENT);
    reopened["action"] = json!("reopened");
    let event = parse_pull_request_event(&reopened);
    assert_eq!(event.action, PullRequestEventAction::Reopened);
}

#[test]
fn test_event_try_from_parses_opened_event() {
    // Parse the event
    let event = parse_pull_request_event(&recorded_event(OPENED_EVENT));

    // Check the event fields used to decide on the action to take
    assert_eq!(event.action, PullRequestEventAction::Opened);
    assert_eq!(event.number, PR_NUMBER);
    assert_eq!(event.organization.unwrap().login, ORG);
    assert_eq!(event.pull_request.base.ref_, "master");
    assert_eq!(event.pull_request.head.sha, HEAD_SHA);
    assert!(!event.pull_request.merged);
    assert_eq!(event.repository.name, "Hello-World");
}

#[test]
fn test_event_try_from_rejects_invalid_body() {
    // Setup event header
    let header = Some(HeaderValue::from_static("pull_request"));

    // Parse the event
    let result = Event::try_from((&header, &b"not json"[..]));

    // Check the parsing error is reported
    assert!(matches!(result, Err(EventError::InvalidBody(_))));
}

#[tokio::test]
async fn test_gh_api_create_check_run_posts_check_run() {
    // Setup GitHub API stand-in and check run
    let fake = FakeGitHub::start(vec![Route::new(
        "POST",
        CHECK_RUNS_PATH,
        201,
        r#"{"output": {}}"#,
    )]);
    let gh = setup_gh_api(&fake);
    let body = new_checks_create_request(
        HEAD_SHA.to_string(),
        Some(JobStatus::Completed),
        Some(ChecksCreateRequestConclusion::Success),
        "check message",
    );

    // Create the check run
    gh.create_check_run(&setup_ctx(), &body).await.unwrap();

    // Check the request sent
    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, CHECK_RUNS_PATH);
    assert!(requests[0].query.is_empty());
    let sent_body = requests[0].body.as_ref().unwrap();
    assert_eq!(sent_body["conclusion"], "success");
    assert_eq!(sent_body["head_sha"], HEAD_SHA);
    assert_eq!(sent_body["name"], CHECK_RUN_NAME);
    assert_eq!(sent_body["output"]["summary"], "check message");
    assert_eq!(sent_body["output"]["title"], "check message");
    assert_eq!(sent_body["status"], "completed");
}

#[tokio::test]
async fn test_gh_api_list_pr_files_marks_truncated_at_3000_entries() {
    // Setup GitHub API stand-in returning 3,000 diff entries
    let entries: Vec<Value> = (0..MAX_PR_FILES)
        .map(|i| json!({"filename": format!("file-{i}.txt"), "status": "modified"}))
        .collect();
    let fake = FakeGitHub::start(vec![Route::new(
        "GET",
        PR_FILES_PATH,
        200,
        serde_json::to_string(&entries).unwrap(),
    )]);
    let gh = setup_gh_api(&fake);

    // List the pull request files
    let pr_files = gh.list_pr_files(&setup_ctx(), PR_NUMBER).await.unwrap();

    // Check truncation flag and per_page=100 query parameter
    assert!(pr_files.is_truncated);
    assert_eq!(pr_files.files.len(), MAX_PR_FILES);
    assert_eq!(fake.requests()[0].query_param("per_page"), Some("100"));
}

#[tokio::test]
async fn test_gh_api_list_pr_files_returns_both_names_of_renamed_files() {
    // Setup GitHub API stand-in with a renamed file
    let fake = FakeGitHub::start(vec![Route::new(
        "GET",
        PR_FILES_PATH,
        200,
        r#"[{"filename": "new-config.yaml", "previous_filename": "config.yaml", "status": "renamed"}]"#,
    )]);
    let gh = setup_gh_api(&fake);

    // List the pull request files
    let pr_files = gh.list_pr_files(&setup_ctx(), PR_NUMBER).await.unwrap();

    // Check the request sent (with per_page=100) and the file names returned
    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path, PR_FILES_PATH);
    assert_eq!(requests[0].query_param("per_page"), Some("100"));
    assert!(!pr_files.is_truncated);
    assert_eq!(pr_files.files, vec!["new-config.yaml", "config.yaml"]);
}

#[tokio::test]
async fn test_gh_api_post_comment_returns_comment_id() {
    // Setup GitHub API stand-in
    let fake = FakeGitHub::start(vec![Route::new(
        "POST",
        COMMENTS_PATH,
        201,
        r#"{"author_association": "NONE", "id": 1234}"#,
    )]);
    let gh = setup_gh_api(&fake);

    // Post the comment
    let comment_id = gh.post_comment(&setup_ctx(), PR_NUMBER, "comment body").await.unwrap();

    // Check the request sent and the comment id returned
    assert_eq!(
        fake.requests(),
        vec![Request::new(
            "POST",
            COMMENTS_PATH,
            Some(json!({"body": "comment body"}))
        )]
    );
    assert_eq!(comment_id, 1234);
}

// Helpers.

/// Parses the payload provided as a pull request webhook event.
fn parse_pull_request_event(payload: &Value) -> PullRequestEvent {
    let header = Some(HeaderValue::from_static("pull_request"));
    let body = serde_json::to_vec(payload).unwrap();
    let Event::PullRequest(event) = Event::try_from((&header, &body[..])).unwrap() else {
        panic!("expected PullRequest event");
    };
    *event
}

/// Returns the recorded event provided as a JSON value.
fn recorded_event(event: &str) -> Value {
    serde_json::from_str(event).unwrap()
}

/// Returns the context of the requests sent in the tests.
fn setup_ctx() -> Ctx {
    Ctx {
        cancel_token: CancellationToken::new(),
        inst_id: INSTALLATION_ID,
        owner: ORG.to_string(),
        repo: REPO.to_string(),
    }
}

/// Returns a GHApi instance backed by a production-middleware ClientPool
/// pointed at the fake provided.
fn setup_gh_api(fake: &FakeGitHub) -> GHApi {
    let policy = RateLimitPolicy {
        jitter_max: Duration::ZERO,
        mutation_interval: Duration::ZERO,
        secondary_base_backoff: Duration::from_millis(10),
        transient_base_backoff: Duration::from_millis(10),
        ..RateLimitPolicy::default()
    };
    GHApi::new(fake.client_pool(policy))
}
