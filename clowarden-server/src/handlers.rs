//! This module defines the handlers used to process HTTP requests to the
//! supported endpoints.

use std::{fmt::Display, path::Path};

use anyhow::{Error, Result, format_err};
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{FromRef, RawQuery, State},
    http::{
        HeaderMap, HeaderValue, Request, Response, StatusCode,
        header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, WWW_AUTHENTICATE},
    },
    response::{IntoResponse, Redirect},
    routing::{get, get_service, post},
};
use base64::Engine;
use hmac::{Hmac, KeyInit, Mac};
use mime::APPLICATION_JSON;
use octorust::types::{ChecksCreateRequestConclusion, JobStatus};
use sha2::Sha256;
use tokio::sync::mpsc;
use tower::ServiceBuilder;
use tower_http::{
    services::{ServeDir, ServeFile},
    set_header::SetResponseHeader,
    trace::TraceLayer,
    validate_request::ValidateRequestHeaderLayer,
};
use tracing::{error, instrument, trace};

use clowarden_core::cfg::Organization;

use crate::{
    cfg::Config,
    db::{DynDB, SearchChangesInput},
    github::{self, Ctx, DynGH, Event, EventError, PullRequestEventAction},
    jobs::{Job, ReconcileInput, ValidateInput},
};

/// Audit index HTML document cache duration.
const AUDIT_INDEX_CACHE_MAX_AGE: usize = 300;

/// Default cache duration for some API endpoints.
const DEFAULT_API_MAX_AGE: usize = 300;

/// Static files cache duration.
const STATIC_CACHE_MAX_AGE: usize = 365 * 24 * 60 * 60;

/// Header representing the kind of the event received.
const GITHUB_EVENT_HEADER: &str = "X-GitHub-Event";

/// Header representing the event payload signature.
const GITHUB_SIGNATURE_HEADER: &str = "X-Hub-Signature-256";

/// Header that indicates the number of items available for pagination purposes.
const PAGINATION_TOTAL_COUNT: &str = "pagination-total-count";

/// Message used when a pull request does not update configuration files.
const NO_CONFIG_CHANGES_MSG: &str = "No CLOWarden configuration changes detected";

/// Router's state.
#[derive(Clone, FromRef)]
struct RouterState {
    db: DynDB,
    gh: DynGH,
    webhook_secret: String,
    webhook_secret_fallback: Option<String>,
    jobs_tx: mpsc::UnboundedSender<Job>,
    orgs: Vec<Organization>,
}

/// Setup HTTP server router.
pub(crate) fn setup_router(
    cfg: &Config,
    db: DynDB,
    gh: DynGH,
    jobs_tx: mpsc::UnboundedSender<Job>,
) -> Result<Router> {
    // Setup some paths
    let static_path = cfg.server.static_path.clone();
    let root_index_path = Path::new(&static_path).join("index.html");
    let audit_path = Path::new(&static_path).join("audit");
    let audit_index_path = audit_path.join("index.html");

    // Setup audit index handler
    let audit_index = SetResponseHeader::overriding(
        ServeFile::new(audit_index_path),
        CACHE_CONTROL,
        HeaderValue::try_from(format!("max-age={AUDIT_INDEX_CACHE_MAX_AGE}"))?,
    );

    // Setup audit router
    let mut audit_router = Router::new()
        .route("/api/organizations", get(list_organizations))
        .route("/api/changes/search", get(search_changes))
        .nest_service(
            "/static",
            get_service(SetResponseHeader::overriding(
                ServeDir::new(audit_path),
                CACHE_CONTROL,
                HeaderValue::try_from(format!("max-age={STATIC_CACHE_MAX_AGE}"))?,
            )),
        )
        .route("/", get_service(audit_index.clone()))
        .fallback_service(get_service(audit_index));

    // Setup basic auth
    if let Some(basic_auth) = &cfg.server.basic_auth
        && basic_auth.enabled
    {
        let basic_auth_value = HeaderValue::try_from(format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD
                .encode(format!("{}:{}", basic_auth.username, basic_auth.password))
        ))?;
        audit_router = audit_router.layer(ValidateRequestHeaderLayer::custom(
            #[allow(clippy::result_large_err)]
            move |request: &mut Request<Body>| match request.headers().get(AUTHORIZATION) {
                Some(value) if value == basic_auth_value => Ok(()),
                _ => {
                    let mut response = Response::new(Body::empty());
                    *response.status_mut() = StatusCode::UNAUTHORIZED;
                    response.headers_mut().insert(WWW_AUTHENTICATE, HeaderValue::from_static("Basic"));
                    Err(response)
                }
            },
        ));
    }

    // Setup main router
    let router = Router::new()
        .route("/webhook/github", post(event))
        .route("/health-check", get(health_check))
        .route("/audit", get(|| async { Redirect::permanent("/audit/") }))
        .route("/", get_service(ServeFile::new(&root_index_path)))
        .nest("/audit/", audit_router)
        .nest_service(
            "/static",
            get_service(SetResponseHeader::overriding(
                ServeDir::new(static_path),
                CACHE_CONTROL,
                HeaderValue::try_from(format!("max-age={STATIC_CACHE_MAX_AGE}"))?,
            )),
        )
        .fallback_service(get_service(ServeFile::new(&root_index_path)))
        .layer(ServiceBuilder::new().layer(TraceLayer::new_for_http()))
        .with_state(RouterState {
            db,
            gh,
            webhook_secret: cfg.server.github_app.webhook_secret.clone(),
            webhook_secret_fallback: cfg.server.github_app.webhook_secret_fallback.clone(),
            jobs_tx,
            orgs: cfg.organizations.clone().unwrap_or_default(),
        });

    Ok(router)
}

// Handlers.

/// Handler that takes care of health check requests.
#[allow(clippy::unused_async)]
async fn health_check() -> impl IntoResponse {
    ""
}

/// Handler that processes webhook events from GitHub.
#[allow(clippy::let_with_type_underscore)]
#[instrument(skip_all, err(Debug))]
async fn event(
    State(gh): State<DynGH>,
    State(webhook_secret): State<String>,
    State(webhook_secret_fallback): State<Option<String>>,
    State(jobs_tx): State<mpsc::UnboundedSender<Job>>,
    State(orgs): State<Vec<Organization>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    // Verify payload signature
    let webhook_secret = webhook_secret.as_bytes();
    let webhook_secret_fallback = webhook_secret_fallback.as_ref().map(String::as_bytes);
    if verify_signature(
        headers.get(GITHUB_SIGNATURE_HEADER),
        webhook_secret,
        webhook_secret_fallback,
        &body[..],
    )
    .is_err()
    {
        return Err((StatusCode::BAD_REQUEST, "no valid signature found".to_string()));
    }

    // Parse event
    let event_header = &headers.get(GITHUB_EVENT_HEADER).cloned();
    let event_payload = &body[..];
    let event = match Event::try_from((event_header, event_payload)) {
        Ok(event) => event,
        Err(err @ EventError::MissingHeader) => {
            return Err((StatusCode::BAD_REQUEST, err.to_string()));
        }
        Err(EventError::InvalidBody(err)) => {
            return Err((StatusCode::BAD_REQUEST, EventError::InvalidBody(err).to_string()));
        }
        Err(EventError::UnsupportedEvent) => return Ok(()),
    };
    trace!(?event, "webhook event received");

    // Take action on event when needed
    match event {
        Event::PullRequest(event) => {
            // Check event comes from a registered organization target
            let Some(gh_org) = &event.organization else {
                return Ok(());
            };
            let Some(org) = find_target_org(
                &orgs,
                &gh_org.login,
                &event.repository.name,
                &event.pull_request.base.ref_,
            )
            .cloned() else {
                return Ok(());
            };

            // Check if we are interested on the event's action
            if !is_supported_pr_action(&event.action) {
                return Ok(());
            }

            // Check if the PR updates the configuration files
            match pr_updates_config(
                gh.clone(),
                &org,
                &event.repository.name,
                &event.pull_request.base.ref_,
                event.pull_request.number,
            )
            .await
            {
                Ok(PullRequestConfigChanges::Changed) => {
                    // It does, go ahead processing event
                }
                Ok(PullRequestConfigChanges::Unchanged) => {
                    // It does not, report success when branch protection may require a check
                    if should_report_no_config_changes(&event.action) {
                        report_no_config_changes(
                            gh.clone(),
                            &org,
                            event.pull_request.number,
                            &event.pull_request.head.sha,
                        )
                        .await;
                    }
                    return Ok(());
                }
                Ok(PullRequestConfigChanges::NotTarget) => return Ok(()),
                Err(err) => {
                    error!(?err, "error checking if pr updates config");
                    return Ok(());
                }
            }

            // Take action on event
            match event.action {
                PullRequestEventAction::Opened | PullRequestEventAction::Synchronize => {
                    // Create validation in-progress check run
                    let ctx = Ctx::from(&org);
                    let check_body = github::new_checks_create_request(
                        event.pull_request.head.sha.clone(),
                        Some(JobStatus::InProgress),
                        None,
                        "Validating configuration changes",
                    );
                    if let Err(err) = gh.create_check_run(&ctx, &check_body).await {
                        error!(?err, "error creating validation in-progress check run");
                    }

                    // Enqueue validation job
                    let input = ValidateInput::new(org, event.pull_request);
                    _ = jobs_tx.send(Job::Validate(input));
                }
                PullRequestEventAction::Closed if event.pull_request.merged => {
                    // Enqueue reconcile job
                    let input = ReconcileInput::new(org, event.pull_request);
                    _ = jobs_tx.send(Job::Reconcile(input));
                }
                _ => {}
            }
        }
    }

    Ok(())
}

/// Handler that lists the registered organizations.
#[allow(clippy::unused_async)]
async fn list_organizations(State(orgs): State<Vec<Organization>>) -> impl IntoResponse {
    // Prepare organizations list
    let orgs_names: Vec<String> = orgs.iter().map(|o| o.name.clone()).collect();
    let orgs_names_json = serde_json::to_string(&orgs_names).map_err(internal_error)?;

    // Return organizations list as json
    Response::builder()
        .header(CACHE_CONTROL, format!("max-age={DEFAULT_API_MAX_AGE}"))
        .header(CONTENT_TYPE, APPLICATION_JSON.as_ref())
        .body(Body::from(orgs_names_json))
        .map_err(internal_error)
}

/// Handler that allows searching for changes.
async fn search_changes(State(db): State<DynDB>, RawQuery(query): RawQuery) -> impl IntoResponse {
    // Search changes in database
    let query = query.unwrap_or_default();
    let input: SearchChangesInput = serde_qs::from_str(&query).map_err(|_| StatusCode::BAD_REQUEST)?;
    let (count, changes) = db.search_changes(&input).await.map_err(internal_error)?;

    // Return search results as json
    Response::builder()
        .header(CACHE_CONTROL, format!("max-age={DEFAULT_API_MAX_AGE}"))
        .header(CONTENT_TYPE, APPLICATION_JSON.as_ref())
        .header(PAGINATION_TOTAL_COUNT, count.to_string())
        .body(Body::from(changes))
        .map_err(internal_error)
}

// Helpers.

/// Create a successful check run when a pull request does not update
/// configuration files.
async fn create_no_config_changes_check(gh: DynGH, org: &Organization, head_sha: &str) -> Result<()> {
    let ctx = Ctx::from(org);
    let check_body = github::new_checks_create_request(
        head_sha.to_string(),
        Some(JobStatus::Completed),
        Some(ChecksCreateRequestConclusion::Success),
        NO_CONFIG_CHANGES_MSG,
    );
    gh.create_check_run(&ctx, &check_body).await
}

/// Find the organization target for a pull request event.
fn find_target_org<'a>(
    orgs: &'a [Organization],
    organization_name: &str,
    repository_name: &str,
    base_ref: &str,
) -> Option<&'a Organization> {
    orgs.iter().find(|org| {
        org.name == organization_name && org.repository == repository_name && org.branch == base_ref
    })
}

/// Helper for mapping any error into a `500 Internal Server Error` response.
#[allow(clippy::needless_pass_by_value)]
fn internal_error<E>(err: E) -> StatusCode
where
    E: Into<Error> + Display,
{
    error!(%err);
    StatusCode::INTERNAL_SERVER_ERROR
}

/// Check if the pull request action is supported.
fn is_supported_pr_action(action: &PullRequestEventAction) -> bool {
    matches!(
        action,
        PullRequestEventAction::Closed | PullRequestEventAction::Opened | PullRequestEventAction::Synchronize
    )
}

/// Check if the pull request in the event provided updates any of the
/// organization configuration files.
async fn pr_updates_config(
    gh: DynGH,
    org: &Organization,
    repository_name: &str,
    base_ref: &str,
    pr_number: i64,
) -> Result<PullRequestConfigChanges> {
    // Check if repository in PR matches with config
    if org.repository != repository_name {
        return Ok(PullRequestConfigChanges::NotTarget);
    }

    // Check if base branch in PR matches with config
    if org.branch != base_ref {
        return Ok(PullRequestConfigChanges::NotTarget);
    }

    // Check if any of the configuration files is on the pr
    if org.legacy.enabled {
        let mut legacy_cfg_files = vec![&org.legacy.sheriff_permissions_path];
        if let Some(cncf_people_path) = &org.legacy.cncf_people_path {
            legacy_cfg_files.push(cncf_people_path);
        }
        let ctx = Ctx::from(org);
        for filename in gh.list_pr_files(&ctx, pr_number).await? {
            if legacy_cfg_files.contains(&&filename) {
                return Ok(PullRequestConfigChanges::Changed);
            }
        }
    }

    Ok(PullRequestConfigChanges::Unchanged)
}

/// Check if a no-configuration-changes report should be created for the pull
/// request action.
fn should_report_no_config_changes(action: &PullRequestEventAction) -> bool {
    matches!(
        action,
        PullRequestEventAction::Opened | PullRequestEventAction::Synchronize
    )
}

/// Report a successful no-configuration-changes check when possible.
async fn report_no_config_changes(gh: DynGH, org: &Organization, pr_number: i64, head_sha: &str) {
    if let Err(err) = create_no_config_changes_check(gh, org, head_sha).await {
        error!(
            ?err,
            org = %org.name,
            repo = %org.repository,
            %head_sha,
            pr_number,
            "error creating no configuration changes check run"
        );
    }
}

/// Verify that the signature provided is valid.
fn verify_signature(
    signature: Option<&HeaderValue>,
    secret: &[u8],
    secret_fallback: Option<&[u8]>,
    body: &[u8],
) -> Result<()> {
    if let Some(signature) = signature
        .and_then(|s| s.to_str().ok())
        .and_then(|s| s.strip_prefix("sha256="))
        .and_then(|s| hex::decode(s).ok())
    {
        // Try primary secret
        let mut mac = Hmac::<Sha256>::new_from_slice(secret)?;
        mac.update(body);
        let result = mac.verify_slice(&signature[..]);
        if result.is_ok() {
            return Ok(());
        }
        if secret_fallback.is_none() {
            return result.map_err(Error::new);
        }

        // Try fallback secret (if available)
        let mut mac = Hmac::<Sha256>::new_from_slice(secret_fallback.expect("secret should be set"))?;
        mac.update(body);
        mac.verify_slice(&signature[..]).map_err(Error::new)
    } else {
        Err(format_err!("no valid signature found"))
    }
}

// Types.

/// Configuration change status for a pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PullRequestConfigChanges {
    /// The pull request updates CLOWarden configuration files.
    Changed,

    /// The pull request targets a repository or branch not managed by this
    /// CLOWarden instance.
    NotTarget,

    /// The pull request targets this CLOWarden instance but does not update
    /// configuration files.
    Unchanged,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use anyhow::format_err;
    use futures::future;

    use clowarden_core::cfg::Legacy;

    use crate::github::MockGH;

    use super::*;

    const BASE_REF: &str = "main";
    const ERROR: &str = "something went wrong";
    const HEAD_SHA: &str = "abc123";
    const INSTALLATION_ID: i64 = 1;
    const ORG: &str = "cncf";
    const PR_NUMBER: i64 = 42;
    const REPO: &str = "clowarden-config";

    #[tokio::test]
    async fn create_no_config_changes_check_success() {
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
        let result = create_no_config_changes_check(Arc::new(gh), &setup_test_org(), HEAD_SHA).await;

        // Check check run was created
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn create_no_config_changes_check_failure() {
        // Setup GitHub mock
        let mut gh = MockGH::new();
        gh.expect_create_check_run()
            .times(1)
            .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));

        // Run the workflow
        let result = create_no_config_changes_check(Arc::new(gh), &setup_test_org(), HEAD_SHA).await;

        // Check error is propagated
        assert!(result.is_err());
    }

    #[test]
    fn find_target_org_matches_organization_repository_and_base_ref() {
        // Setup organizations
        let org = setup_test_org();
        let orgs = vec![org.clone()];

        // Run the target lookup
        let result = find_target_org(&orgs, ORG, REPO, BASE_REF);

        // Check matching organization target was found
        assert_eq!(result, Some(&org));
    }

    #[test]
    fn find_target_org_rejects_wrong_base_ref() {
        // Setup organizations
        let orgs = vec![setup_test_org()];

        // Run the target lookup
        let result = find_target_org(&orgs, ORG, REPO, "feature");

        // Check no target was found
        assert_eq!(result, None);
    }

    #[test]
    fn find_target_org_rejects_wrong_repository() {
        // Setup organizations
        let orgs = vec![setup_test_org()];

        // Run the target lookup
        let result = find_target_org(&orgs, ORG, "other", BASE_REF);

        // Check no target was found
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn pr_updates_config_legacy_file_changed() {
        // Setup GitHub mock
        let mut gh = MockGH::new();
        gh.expect_list_pr_files()
            .withf(|ctx, pr_number| {
                ctx.inst_id == INSTALLATION_ID
                    && ctx.owner == ORG
                    && ctx.repo == REPO
                    && *pr_number == PR_NUMBER
            })
            .times(1)
            .returning(|_, _| {
                Box::pin(future::ready(Ok(vec![
                    "docs/README.md".to_string(),
                    "config.yaml".to_string(),
                ])))
            });

        // Run the workflow
        let result = pr_updates_config(Arc::new(gh), &setup_test_org(), REPO, BASE_REF, PR_NUMBER).await;

        // Check configuration update was detected
        assert_eq!(result.unwrap(), PullRequestConfigChanges::Changed);
    }

    #[tokio::test]
    async fn pr_updates_config_legacy_file_not_changed() {
        // Setup GitHub mock
        let mut gh = MockGH::new();
        gh.expect_list_pr_files()
            .times(1)
            .returning(|_, _| Box::pin(future::ready(Ok(vec!["docs/README.md".to_string()]))));

        // Run the workflow
        let result = pr_updates_config(Arc::new(gh), &setup_test_org(), REPO, BASE_REF, PR_NUMBER).await;

        // Check configuration update was not detected
        assert_eq!(result.unwrap(), PullRequestConfigChanges::Unchanged);
    }

    #[tokio::test]
    async fn pr_updates_config_wrong_base_ref_skips_files() {
        // Setup GitHub mock
        let gh = MockGH::new();

        // Run the workflow
        let result = pr_updates_config(Arc::new(gh), &setup_test_org(), REPO, "feature", PR_NUMBER).await;

        // Check pull request was treated as a non-target
        assert_eq!(result.unwrap(), PullRequestConfigChanges::NotTarget);
    }

    #[tokio::test]
    async fn pr_updates_config_wrong_repository_skips_files() {
        // Setup GitHub mock
        let gh = MockGH::new();

        // Run the workflow
        let result = pr_updates_config(Arc::new(gh), &setup_test_org(), "other", BASE_REF, PR_NUMBER).await;

        // Check pull request was treated as a non-target
        assert_eq!(result.unwrap(), PullRequestConfigChanges::NotTarget);
    }

    #[tokio::test]
    async fn report_no_config_changes_ignores_check_failure() {
        // Setup GitHub mock
        let mut gh = MockGH::new();
        gh.expect_create_check_run()
            .times(1)
            .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));

        // Run the workflow
        report_no_config_changes(Arc::new(gh), &setup_test_org(), PR_NUMBER, HEAD_SHA).await;
    }

    // Helpers.

    fn setup_test_org() -> Organization {
        Organization {
            branch: BASE_REF.to_string(),
            installation_id: INSTALLATION_ID,
            legacy: Legacy {
                enabled: true,
                sheriff_permissions_path: "config.yaml".to_string(),

                cncf_people_path: Some("people.yaml".to_string()),
            },
            name: ORG.to_string(),
            repository: REPO.to_string(),
        }
    }
}
