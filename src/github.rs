use anyhow::{Context, Result};
use async_trait::async_trait;
use axum::http::HeaderValue;
use base64::{engine::general_purpose::STANDARD as b64, Engine as _};
use config::Config;
#[cfg(test)]
use mockall::automock;
use octorust::{
    auth::{Credentials, InstallationTokenGenerator, JWTCredentials},
    types::{
        ChecksCreateRequest, ChecksCreateRequestConclusion, ChecksCreateRequestOutput, JobStatus,
        OrganizationSimple, PullRequestData, PullsUpdateReviewRequest, Repository, SimpleUser,
    },
    Client,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;

/// Type alias to represent a GH trait object.
pub(crate) type DynGH = Arc<dyn GH + Send + Sync>;

/// Type alias to represent a webhook event header.
type EventHeader = Option<HeaderValue>;

/// Type alias to represent a webhook event payload.
type EventPayload = [u8];

/// Type alias to represent a comment id.
type CommentId = i64;

/// Type alias to represent a filename.
type FileName = String;

/// Name used for the check run in GitHub.
const CHECK_RUN_NAME: &str = "CLOWarden";

/// Header representing the kind of the event received.
pub(crate) const EVENT_HEADER: &str = "X-GitHub-Event";

/// Header representing the event payload signature.
pub(crate) const SIGNATURE_HEADER: &str = "X-Hub-Signature-256";

/// Trait that defines some operations a GH implementation must support.
#[async_trait]
#[cfg_attr(test, automock)]
pub(crate) trait GH {
    /// Create a check run.
    async fn create_check_run(&self, body: &ChecksCreateRequest) -> Result<()>;

    /// Get file content.
    async fn get_file_content(&self, path: &str, ref_: Option<&str>) -> Result<String>;

    /// List pull request files.
    async fn list_pr_files(&self, pr_number: i64) -> Result<Vec<FileName>>;

    /// Post the comment provided in the repository's pull request given.
    async fn post_comment(&self, pr_number: i64, body: &str) -> Result<CommentId>;
}

/// GH implementation backed by the GitHub API.
pub(crate) struct GHApi {
    client: Client,
    org: String,
    repo: String,
    branch: String,
}

impl GHApi {
    /// Create a new GHApi instance.
    pub(crate) fn new(cfg: Arc<Config>) -> Result<Self> {
        // Setup GitHub app credentials
        let app_id = cfg.get_int("githubApp.appId").unwrap() as u64;
        let app_private_key = pem::parse(cfg.get_string("githubApp.privateKey").unwrap())?.contents;
        let credentials =
            JWTCredentials::new(app_id, app_private_key).context("error setting up credentials")?;

        // Setup GitHub API client
        let inst_id = cfg.get_int("githubApp.installationId").unwrap() as u64;
        let tg = InstallationTokenGenerator::new(inst_id, credentials);
        let client = Client::new(
            format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
            Credentials::InstallationToken(tg),
        )?;

        Ok(Self {
            client,
            org: cfg.get_string("config.organization").unwrap(),
            repo: cfg.get_string("config.repository").unwrap(),
            branch: cfg.get_string("config.branch").unwrap(),
        })
    }
}

#[async_trait]
impl GH for GHApi {
    async fn create_check_run(&self, body: &ChecksCreateRequest) -> Result<()> {
        _ = self.client.checks().create(&self.org, &self.repo, body).await?;
        Ok(())
    }

    async fn get_file_content(&self, path: &str, ref_: Option<&str>) -> Result<String> {
        let ref_ = ref_.unwrap_or(&self.branch);
        let mut content = self
            .client
            .repos()
            .get_content_file(&self.org, &self.repo, path, ref_)
            .await?
            .content
            .as_bytes()
            .to_owned();
        content.retain(|b| !b" \n\t\r\x0b\x0c".contains(b));
        let decoded_content = String::from_utf8(b64.decode(content)?)?;
        Ok(decoded_content)
    }

    async fn list_pr_files(&self, pr_number: i64) -> Result<Vec<FileName>> {
        let files = self
            .client
            .pulls()
            .list_all_files(&self.org, &self.repo, pr_number)
            .await?
            .iter()
            .map(|e| e.filename.clone())
            .collect();
        Ok(files)
    }

    async fn post_comment(&self, pr_number: i64, body: &str) -> Result<CommentId> {
        let body = &PullsUpdateReviewRequest {
            body: body.to_string(),
        };
        let comment = self
            .client
            .issues()
            .create_comment(&self.org, &self.repo, pr_number, body)
            .await?;
        Ok(comment.id)
    }
}

/// Represents a GitHub webhook event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum Event {
    PullRequest(PullRequestEvent),
}

impl TryFrom<(&EventHeader, &EventPayload)> for Event {
    type Error = EventError;

    fn try_from((event_name, event_body): (&EventHeader, &EventPayload)) -> Result<Self, Self::Error> {
        match event_name {
            Some(event_name) => match event_name.as_bytes() {
                b"pull_request" => {
                    let event: PullRequestEvent = serde_json::from_slice(event_body)
                        .map_err(|err| EventError::InvalidBody(err.to_string()))?;
                    Ok(Event::PullRequest(event))
                }
                _ => Err(EventError::UnsupportedEvent),
            },
            None => Err(EventError::MissingHeader),
        }
    }
}

/// Errors that may occur while creating a new webhook event instance.
#[derive(Error, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum EventError {
    #[error("event header missing")]
    MissingHeader,
    #[error("unsupported event")]
    UnsupportedEvent,
    #[error("invalid body: {0}")]
    InvalidBody(String),
}

/// Pull request event payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PullRequestEvent {
    pub action: PullRequestEventAction,
    pub number: i64,
    pub organization: Option<OrganizationSimple>,
    pub pull_request: PullRequestData,
    pub repository: Repository,
    pub sender: SimpleUser,
}

/// Pull request event action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PullRequestEventAction {
    Closed,
    Opened,
    Synchronize,
    #[serde(other)]
    Other,
}

/// Helper function to create a new ChecksCreateRequest instance.
pub(crate) fn new_checks_create_request(
    head_sha: String,
    status: Option<JobStatus>,
    conclusion: Option<ChecksCreateRequestConclusion>,
    msg: &str,
) -> ChecksCreateRequest {
    ChecksCreateRequest {
        actions: vec![],
        completed_at: None,
        conclusion,
        details_url: "".to_string(),
        external_id: "".to_string(),
        head_sha,
        name: CHECK_RUN_NAME.to_string(),
        output: Some(ChecksCreateRequestOutput {
            annotations: vec![],
            images: vec![],
            summary: msg.to_string(),
            text: "".to_string(),
            title: msg.to_string(),
        }),
        started_at: None,
        status,
    }
}
