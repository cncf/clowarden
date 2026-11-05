use std::sync::Arc;

use base64::Engine as _;
use serde_json::json;

use crate::cfg::GitHubApp;

use super::{
    GH, GHApi, Source, b64,
    http_fake::{FakeGitHub, Route, generate_app_private_key},
};

const CONTENT: &str = "teams:\n  - name: team1\n    maintainers:\n      - user1\n      - user2\n";
const CONTENT_PATH: &str = "/repos/fork-owner/fork-repo/contents/config/perms.yaml";
const FILE_PATH: &str = "config/perms.yaml";
const INSTALLATION_ID: i64 = 1;

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
    assert!(gh.get_client(Some(INSTALLATION_ID)).is_ok());
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
    let gh = GHApi::new_with_token("token".to_string());

    // Check a client is available when no installation is involved
    assert!(gh.get_client(None).is_ok());
}

// Helpers.

/// Build the body of a contents API response with the content provided.
fn content_file_body(content: &str) -> String {
    json!({ "_links": {}, "content": content, "encoding": "base64" }).to_string()
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
    let gh = GHApi::default();
    gh.clients.write().unwrap().insert(None, Arc::new(fake.client()));
    gh
}

/// Split the base64 content provided in lines, as the contents API does.
fn wrap_base64(content: &str) -> String {
    let lines: Vec<&str> =
        content.as_bytes().chunks(60).map(|line| std::str::from_utf8(line).unwrap()).collect();
    format!("{}\n", lines.join("\n"))
}
