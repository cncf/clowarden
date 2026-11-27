//! This module defines an abstraction layer over the GitHub API.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use axum::http::HeaderValue;
#[cfg(test)]
use mockall::automock;
use octorust::types::{
    ChecksCreateRequest, ChecksCreateRequestConclusion, ChecksCreateRequestOutput, DiffEntry, JobStatus,
    OrganizationSimple, PullRequestData, PullRequestSimple, PullsUpdateReviewRequest, Repository, SimpleUser,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

use clowarden_core::{
    cfg::Organization,
    github::{ClientPool, RequestKind},
};

#[cfg(test)]
mod tests;

/// Name used for the check run in GitHub.
pub(crate) const CHECK_RUN_NAME: &str = "CLOWarden";

/// Maximum number of files returned by GitHub's list pull request files
/// endpoint before results are truncated.
pub(crate) const MAX_PR_FILES: usize = 3_000;

/// Trait that defines some operations a GH implementation must support.
#[async_trait]
#[cfg_attr(test, automock)]
pub(crate) trait GH {
    /// Create a check run.
    async fn create_check_run(&self, ctx: &Ctx, body: &ChecksCreateRequest) -> Result<()>;

    /// Fetch pull request metadata by number.
    async fn get_pr(&self, ctx: &Ctx, pr_number: i64) -> Result<AssociatedPullRequest>;

    /// List pull requests associated with a commit SHA.
    async fn list_commit_prs(&self, ctx: &Ctx, commit_sha: &str) -> Result<Vec<AssociatedPullRequest>>;

    /// List pull request files, including the previous name of renamed files
    /// and whether the GitHub response reached its truncation limit.
    async fn list_pr_files(&self, ctx: &Ctx, pr_number: i64) -> Result<PrFiles>;

    /// Post the comment provided in the repository's pull request given.
    async fn post_comment(&self, ctx: &Ctx, pr_number: i64, body: &str) -> Result<CommentId>;
}

/// Type alias to represent a GH trait object.
pub(crate) type DynGH = Arc<dyn GH + Send + Sync>;

/// Type alias to represent a comment id.
type CommentId = i64;

/// Type alias to represent a filename.
type FileName = String;

/// Compact pull request metadata needed to validate a re-requested check run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AssociatedPullRequest {
    /// Target base branch name.
    pub base_ref: String,
    /// Current head branch name.
    pub head_ref: String,
    /// Current head commit SHA.
    pub head_sha: String,
    /// Whether the pull request is currently open.
    pub is_open: bool,
    /// Pull request number.
    pub number: i64,

    /// Owner login of the head repository (differs for fork PRs).
    pub head_owner: Option<String>,
    /// Name of the head repository (differs for fork PRs).
    pub head_repo: Option<String>,
    /// Last updated timestamp of the pull request, when available.
    pub updated_at: Option<OffsetDateTime>,
}

impl From<PullRequestData> for AssociatedPullRequest {
    fn from(pr: PullRequestData) -> Self {
        Self {
            base_ref: pr.base.ref_,
            head_ref: pr.head.ref_,
            head_sha: pr.head.sha,
            is_open: pr.state == octorust::types::State::Open,
            number: pr.number,
            head_owner: pr.head.repo.as_ref().map(|r| r.owner.login.clone()),
            head_repo: pr.head.repo.map(|r| r.name),
            updated_at: pr.updated_at.and_then(|dt| OffsetDateTime::from_unix_timestamp(dt.timestamp()).ok()),
        }
    }
}

impl From<PullRequestSimple> for AssociatedPullRequest {
    fn from(pr: PullRequestSimple) -> Self {
        Self {
            base_ref: pr.base.ref_,
            head_ref: pr.head.ref_,
            head_sha: pr.head.sha,
            is_open: pr.state.eq_ignore_ascii_case("open"),
            number: pr.number,
            head_owner: pr.head.repo.as_ref().and_then(|r| r.owner.as_ref()).map(|o| o.login.clone()),
            head_repo: pr.head.repo.map(|r| r.name),
            updated_at: pr.updated_at.and_then(|dt| OffsetDateTime::from_unix_timestamp(dt.timestamp()).ok()),
        }
    }
}

/// Files touched by a pull request and whether the listing may be truncated.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PrFiles {
    /// File paths touched by the pull request, including previous names of
    /// renamed files.
    pub files: Vec<FileName>,
    /// True when the response reached GitHub's 3,000-file cap.
    pub is_truncated: bool,
}

/// GH implementation backed by the GitHub API.
pub(crate) struct GHApi {
    /// Shared GitHub API client pool and rate-limit governor.
    pool: Arc<ClientPool>,
}

impl GHApi {
    /// Create a new GHApi instance backed by the shared [`ClientPool`].
    pub(crate) fn new(pool: Arc<ClientPool>) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl GH for GHApi {
    /// [`GH::create_check_run`].
    async fn create_check_run(&self, ctx: &Ctx, body: &ChecksCreateRequest) -> Result<()> {
        self.pool
            .execute(
                Some(ctx.inst_id),
                RequestKind::NonIdempotentWrite,
                &ctx.cancel_token,
                |client| {
                    let body = body.clone();
                    async move { client.checks().create(&ctx.owner, &ctx.repo, &body).await }
                },
            )
            .await?;
        Ok(())
    }

    /// [`GH::get_pr`].
    async fn get_pr(&self, ctx: &Ctx, pr_number: i64) -> Result<AssociatedPullRequest> {
        let pr = self
            .pool
            .execute(
                Some(ctx.inst_id),
                RequestKind::Read,
                &ctx.cancel_token,
                |client| async move { client.pulls().get(&ctx.owner, &ctx.repo, pr_number).await },
            )
            .await?;
        Ok(pr.into())
    }

    /// [`GH::list_commit_prs`].
    async fn list_commit_prs(&self, ctx: &Ctx, commit_sha: &str) -> Result<Vec<AssociatedPullRequest>> {
        let commit_sha = commit_sha.to_string();
        let prs = self
            .pool
            .execute(
                Some(ctx.inst_id),
                RequestKind::Read,
                &ctx.cancel_token,
                |client| {
                    let commit_sha = commit_sha.clone();
                    async move {
                        client
                            .repos()
                            .list_all_pull_requests_associated_with_commit(&ctx.owner, &ctx.repo, &commit_sha)
                            .await
                    }
                },
            )
            .await?;
        Ok(prs.into_iter().map(Into::into).collect())
    }

    /// [`GH::list_pr_files`].
    async fn list_pr_files(&self, ctx: &Ctx, pr_number: i64) -> Result<PrFiles> {
        let entries = self
            .pool
            .execute(
                Some(ctx.inst_id),
                RequestKind::Read,
                &ctx.cancel_token,
                |client| async move { client.pulls().list_all_files(&ctx.owner, &ctx.repo, pr_number).await },
            )
            .await?;
        let is_truncated = entries.len() >= MAX_PR_FILES;
        Ok(PrFiles {
            files: diff_entries_file_names(&entries),
            is_truncated,
        })
    }

    /// [`GH::post_comment`].
    async fn post_comment(&self, ctx: &Ctx, pr_number: i64, body: &str) -> Result<CommentId> {
        let body = PullsUpdateReviewRequest {
            body: body.to_string(),
        };
        let comment =
            self.pool
                .execute(
                    Some(ctx.inst_id),
                    RequestKind::NonIdempotentWrite,
                    &ctx.cancel_token,
                    |client| {
                        let body = body.clone();
                        async move {
                            client.issues().create_comment(&ctx.owner, &ctx.repo, pr_number, &body).await
                        }
                    },
                )
                .await?;
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
    CheckRun(Box<CheckRunEvent>),
    CheckSuite(Box<CheckSuiteEvent>),
    PullRequest(Box<PullRequestEvent>),
}

impl TryFrom<(&EventHeader, &EventPayload)> for Event {
    type Error = EventError;

    fn try_from((event_name, event_body): (&EventHeader, &EventPayload)) -> Result<Self, Self::Error> {
        match event_name {
            Some(event_name) => match event_name.as_bytes() {
                b"check_run" => {
                    let event: CheckRunEvent = serde_json::from_slice(event_body)
                        .map_err(|err| EventError::InvalidBody(err.to_string()))?;
                    Ok(Event::CheckRun(Box::new(event)))
                }
                b"check_suite" => {
                    let event: CheckSuiteEvent = serde_json::from_slice(event_body)
                        .map_err(|err| EventError::InvalidBody(err.to_string()))?;
                    Ok(Event::CheckSuite(Box::new(event)))
                }
                b"pull_request" => {
                    let event: PullRequestEvent = serde_json::from_slice(event_body)
                        .map_err(|err| EventError::InvalidBody(err.to_string()))?;
                    Ok(Event::PullRequest(Box::new(event)))
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

/// Check run event payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CheckRunEvent {
    pub action: CheckRunEventAction,
    pub check_run: CheckRunPayload,
    pub repository: Repository,

    pub organization: Option<OrganizationSimple>,
}

/// Check run event action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CheckRunEventAction {
    Rerequested,
    #[serde(other)]
    Other,
}

/// Minimal check run fields needed to handle `check_run.rerequested`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CheckRunPayload {
    pub head_sha: String,
    pub name: String,
    #[serde(default)]
    pub pull_requests: Vec<CheckRunPullRequest>,
}

/// Minimal pull request reference embedded in a `check_run` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CheckRunPullRequest {
    pub base: CheckRunPullRequestRef,
    pub head: CheckRunPullRequestRef,
    pub number: i64,
}

/// Minimal git ref embedded in a `check_run` pull request reference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CheckRunPullRequestRef {
    #[serde(rename = "ref")]
    pub ref_: String,
    pub sha: String,
}

/// Check suite event payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CheckSuiteEvent {
    pub action: CheckRunEventAction,
    pub check_suite: CheckSuitePayload,
    pub repository: Repository,

    pub organization: Option<OrganizationSimple>,
}

/// Minimal check suite fields needed to handle `check_suite.rerequested`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CheckSuitePayload {
    pub head_sha: String,
    #[serde(default)]
    pub pull_requests: Vec<CheckRunPullRequest>,
}

/// Pull request event payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PullRequestEvent {
    pub action: PullRequestEventAction,
    pub number: i64,
    pub pull_request: PullRequestData,
    pub repository: Repository,
    pub sender: SimpleUser,

    #[serde(default)]
    pub before: Option<String>,
    #[serde(default)]
    pub changes: Option<PullRequestEventChanges>,
    pub organization: Option<OrganizationSimple>,
}

/// Fields changed in a `pull_request.edited` webhook payload.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct PullRequestEventChanges {
    /// Present when the pull request's target base branch was edited.
    #[serde(default)]
    pub base: Option<serde_json::Value>,
}

/// Pull request event action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PullRequestEventAction {
    Closed,
    Edited,
    Opened,
    Reopened,
    Synchronize,
    #[serde(other)]
    Other,
}

impl PullRequestEventAction {
    /// Return true if this pull request action triggers configuration validation.
    pub(crate) fn is_validation_action(&self) -> bool {
        matches!(
            self,
            Self::Edited | Self::Opened | Self::Reopened | Self::Synchronize
        )
    }
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

/// Return the names of the files in the diff entries provided. Renamed files
/// are reported by both names, so that moving a file away counts as changing it.
fn diff_entries_file_names(entries: &[DiffEntry]) -> Vec<FileName> {
    entries
        .iter()
        .flat_map(|e| [&e.filename, &e.previous_filename])
        .filter(|filename| !filename.is_empty())
        .cloned()
        .collect()
}

/// Information about the target of a GitHub API request.
#[derive(Clone, Default)]
pub struct Ctx {
    /// Cancellation token interrupting rate-limit waits before dispatch.
    pub cancel_token: CancellationToken,
    /// GitHub App installation identifier.
    pub inst_id: i64,
    /// Repository owner login.
    pub owner: String,
    /// Repository name.
    pub repo: String,
}

impl Ctx {
    /// Create a context bound to `org` and `cancel_token`.
    #[must_use]
    pub fn from_org_with_cancel(org: &Organization, cancel_token: CancellationToken) -> Self {
        Self {
            cancel_token,
            inst_id: org.installation_id,
            owner: org.name.clone(),
            repo: org.repository.clone(),
        }
    }
}

impl From<&Organization> for Ctx {
    fn from(org: &Organization) -> Self {
        Self::from_org_with_cancel(org, CancellationToken::new())
    }
}
