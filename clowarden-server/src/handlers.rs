//! This module defines the handlers used to process HTTP requests to the
//! supported endpoints.

use std::{fmt::Display, path::Path};

use anyhow::{format_err, Error, Result};
use axum::{
    body::{Body, Bytes},
    extract::{FromRef, RawQuery, State},
    http::{
        header::{CACHE_CONTROL, CONTENT_TYPE},
        HeaderMap, HeaderValue, Response, StatusCode,
    },
    response::{IntoResponse, Redirect},
    routing::{get, get_service, post},
    Router,
};
use hmac::{Hmac, Mac};
use mime::APPLICATION_JSON;
use octorust::types::JobStatus;
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
    github::{self, Ctx, DynGH, Event, EventError, PullRequestEvent, PullRequestEventAction},
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
    if let Some(basic_auth) = &cfg.server.basic_auth {
        if basic_auth.enabled {
            audit_router = audit_router.layer(ValidateRequestHeaderLayer::basic(
                &basic_auth.username,
                &basic_auth.password,
            ));
        }
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
            return Err((StatusCode::BAD_REQUEST, EventError::InvalidBody(err).to_string()))
        }
        Err(EventError::UnsupportedEvent) => return Ok(()),
    };
    trace!(?event, "webhook event received");

    // Take action on event when needed
    match event {
        Event::PullRequest(event) => {
            // Check event comes from a registered organization
            let Some(gh_org) = &event.organization else {
                return Ok(());
            };
            let Some(org) = orgs.iter().find(|o| o.name == gh_org.login).cloned() else {
                return Ok(());
            };

            // Check if we are interested on the event's action
            if ![
                PullRequestEventAction::Closed,
                PullRequestEventAction::Opened,
                PullRequestEventAction::Synchronize,
            ]
            .contains(&event.action)
            {
                return Ok(());
            }

            // Check if the PR updates the configuration files
            match pr_updates_config(gh.clone(), &org, &event).await {
                Ok(true) => {
                    // It does, go ahead processing event
                }
                Ok(false) => {
                    // It does not, return
                    return Ok(());
                }
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

/// Check if the pull request in the event provided updates any of the
/// organization configuration files.
async fn pr_updates_config(gh: DynGH, org: &Organization, event: &PullRequestEvent) -> Result<bool> {
    // Check if repository in PR matches with config
    if org.repository != event.repository.name {
        return Ok(false);
    }

    // Check if base branch in PR matches with config
    if org.branch != event.pull_request.base.ref_ {
        return Ok(false);
    }

    // Check if any of the configuration files is on the pr
    if org.legacy.enabled {
        let mut legacy_cfg_files = vec![&org.legacy.sheriff_permissions_path];
        if let Some(cncf_people_path) = &org.legacy.cncf_people_path {
            legacy_cfg_files.push(cncf_people_path);
        }
        let ctx = Ctx::from(org);
        for filename in gh.list_pr_files(&ctx, event.pull_request.number).await? {
            if legacy_cfg_files.contains(&&filename) {
                return Ok(true);
            }
        }
    }

    Ok(false)
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
