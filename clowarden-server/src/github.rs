//! This module defines an abstraction layer over the GitHub API.

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use axum::http::HeaderValue;
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
use thiserror::Error;

use clowarden_core::cfg::{GitHubApp, Organization};

/// Name used for the check run in GitHub.
const CHECK_RUN_NAME: &str = "CLOWarden";

/// Trait that defines some operations a GH implementation must support.
#[async_trait]
#[cfg_attr(test, automock)]
pub(crate) trait GH {
    /// Create a check run.
    async fn create_check_run(&self, ctx: &Ctx, body: &ChecksCreateRequest) -> Result<()>;

    /// List pull request files.
    async fn list_pr_files(&self, ctx: &Ctx, pr_number: i64) -> Result<Vec<FileName>>;

    /// Post the comment provided in the repository's pull request given.
    async fn post_comment(&self, ctx: &Ctx, pr_number: i64, body: &str) -> Result<CommentId>;
}

/// Type alias to represent a GH trait object.
pub(crate) type DynGH = Arc<dyn GH + Send + Sync>;

/// Type alias to represent a comment id.
type CommentId = i64;

/// Type alias to represent a filename.
type FileName = String;

/// GH implementation backed by the GitHub API.
pub(crate) struct GHApi {
    app_credentials: JWTCredentials,
}

impl GHApi {
    /// Create a new GHApi instance.
    pub(crate) fn new(gh_app: &GitHubApp) -> Result<Self> {
        // Setup GitHub app credentials
        let private_key = pem::parse(&gh_app.private_key)?.contents().to_owned();
        let app_credentials =
            JWTCredentials::new(gh_app.app_id, private_key).context("error setting up credentials")?;

        Ok(Self { app_credentials })
    }

    /// Setup GitHub API client for the installation id provided.
    fn setup_client(&self, inst_id: i64) -> Result<Client> {
        let user_agent = format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
        let tg = InstallationTokenGenerator::new(inst_id, self.app_credentials.clone());
        let credentials = Credentials::InstallationToken(tg);

        Ok(Client::new(user_agent, credentials)?)
    }
}

#[async_trait]
impl GH for GHApi {
    /// [GH::create_check_run]
    async fn create_check_run(&self, ctx: &Ctx, body: &ChecksCreateRequest) -> Result<()> {
        let client = self.setup_client(ctx.inst_id)?;
        _ = client.checks().create(&ctx.owner, &ctx.repo, body).await?;
        Ok(())
    }

    /// [GH::list_pr_files]
    async fn list_pr_files(&self, ctx: &Ctx, pr_number: i64) -> Result<Vec<FileName>> {
        let client = self.setup_client(ctx.inst_id)?;
        let files = client
            .pulls()
            .list_all_files(&ctx.owner, &ctx.repo, pr_number)
            .await?
            .iter()
            .map(|e| e.filename.clone())
            .collect();
        Ok(files)
    }

    /// [GH::post_comment]
    async fn post_comment(&self, ctx: &Ctx, pr_number: i64, body: &str) -> Result<CommentId> {
        let body = &PullsUpdateReviewRequest {
            body: body.to_string(),
        };
        let client = self.setup_client(ctx.inst_id)?;
        let comment = client.issues().create_comment(&ctx.owner, &ctx.repo, pr_number, body).await?;
        Ok(comment.id)
    }
}

/// Type alias to represent a webhook event header.
type EventHeader = Option<HeaderValue>;

/// Type alias to represent a webhook event payload.
type EventPayload = [u8];

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
        details_url: String::new(),
        external_id: String::new(),
        head_sha,
        name: CHECK_RUN_NAME.to_string(),
        output: Some(ChecksCreateRequestOutput {
            annotations: vec![],
            images: vec![],
            summary: msg.to_string(),
            text: String::new(),
            title: msg.to_string(),
        }),
        started_at: None,
        status,
    }
}

/// Information about the target of a GitHub API request.
pub struct Ctx {
    pub inst_id: i64,
    pub owner: String,
    pub repo: String,
}

impl From<&Organization> for Ctx {
    fn from(org: &Organization) -> Self {
        Ctx {
            inst_id: org.installation_id,
            owner: org.name.clone(),
            repo: org.repository.clone(),
        }
    }
}
