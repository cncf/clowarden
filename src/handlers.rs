use crate::{
    db::{DynDB, SearchChangesInput},
    github::{self, DynGH, Event, EventError, PullRequestEvent, PullRequestEventAction},
    jobs::Job,
};
use anyhow::{format_err, Error, Result};
use axum::{
    body::{Bytes, Full},
    extract::{FromRef, RawQuery, State},
    http::{
        header::{CACHE_CONTROL, CONTENT_TYPE},
        HeaderMap, HeaderValue, Response, StatusCode,
    },
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use config::Config;
use hmac::{Hmac, Mac};
use mime::APPLICATION_JSON;
use octorust::types::JobStatus;
use sha2::Sha256;
use std::{fmt::Display, sync::Arc};
use tokio::sync::mpsc;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;
use tracing::{error, instrument, trace};

/// Default cache duration for some API endpoints.
const DEFAULT_API_MAX_AGE: usize = 300;

/// Header representing the kind of the event received.
const GITHUB_EVENT_HEADER: &str = "X-GitHub-Event";

/// Header representing the event payload signature.
const GITHUB_SIGNATURE_HEADER: &str = "X-Hub-Signature-256";

/// Header that indicates the number of items available for pagination purposes.
const PAGINATION_TOTAL_COUNT: &str = "pagination-total-count";

/// Router's state.
#[derive(Clone, FromRef)]
struct RouterState {
    cfg: Arc<Config>,
    db: DynDB,
    gh: DynGH,
    webhook_secret: String,
    jobs_tx: mpsc::UnboundedSender<Job>,
}

/// Setup HTTP server router.
pub(crate) fn setup_router(
    cfg: Arc<Config>,
    db: DynDB,
    gh: DynGH,
    jobs_tx: mpsc::UnboundedSender<Job>,
) -> Router {
    // Setup webhook secret
    let webhook_secret = cfg.get_string("githubApp.webhookSecret").unwrap();

    // Setup router
    Router::new()
        .route("/api/changes/search", get(search_changes))
        .route("/api/events", post(event))
        .layer(ServiceBuilder::new().layer(TraceLayer::new_for_http()))
        .with_state(RouterState {
            cfg,
            db,
            gh,
            webhook_secret,
            jobs_tx,
        })
}

/// Handler that processes webhook events from GitHub.
#[instrument(skip_all, err(Debug))]
async fn event(
    State(cfg): State<Arc<Config>>,
    State(gh): State<DynGH>,
    State(webhook_secret): State<String>,
    State(jobs_tx): State<mpsc::UnboundedSender<Job>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    // Verify payload signature
    if verify_signature(
        headers.get(GITHUB_SIGNATURE_HEADER),
        webhook_secret.as_bytes(),
        &body[..],
    )
    .is_err()
    {
        return Err((StatusCode::BAD_REQUEST, "no valid signature found".to_string()));
    };

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
            match pr_updates_config(cfg.clone(), gh.clone(), &event).await {
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
                    let check_body = github::new_checks_create_request(
                        event.pull_request.head.sha.clone(),
                        Some(JobStatus::InProgress),
                        None,
                        "Validating configuration changes",
                    );
                    if let Err(err) = gh.create_check_run(&check_body).await {
                        error!(?err, "error creating validation in-progress check run");
                    }

                    // Enqueue validation job
                    let input = event.pull_request.into();
                    _ = jobs_tx.send(Job::Validate(input));
                }
                PullRequestEventAction::Closed if event.pull_request.merged => {
                    // Enqueue reconcile job
                    let input = event.pull_request.into();
                    _ = jobs_tx.send(Job::Reconcile(input));
                }
                _ => {}
            }
        }
    }

    Ok(())
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
        .body(Full::from(changes))
        .map_err(internal_error)
}

/// Verify that the signature provided is valid.
fn verify_signature(signature: Option<&HeaderValue>, secret: &[u8], body: &[u8]) -> Result<()> {
    if let Some(signature) = signature
        .and_then(|s| s.to_str().ok())
        .and_then(|s| s.strip_prefix("sha256="))
        .and_then(|s| hex::decode(s).ok())
    {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret)?;
        mac.update(body);
        mac.verify_slice(&signature[..]).map_err(Error::new)
    } else {
        Err(format_err!("no valid signature found"))
    }
}

/// Check if the pull request in the event provided updates any of the
/// configuration files.
async fn pr_updates_config(cfg: Arc<Config>, gh: DynGH, event: &PullRequestEvent) -> Result<bool> {
    // Check if repository in PR matches with config
    let cfg_repo = &cfg.get_string("config.repository").unwrap();
    if cfg_repo != &event.repository.name {
        return Ok(false);
    }

    // Check if base branch in PR matches with config
    let cfg_branch = &cfg.get_string("config.branch").unwrap();
    if cfg_branch != &event.pull_request.base.ref_ {
        return Ok(false);
    }

    // Check if any of the configuration files is on the pr
    if let Ok(true) = cfg.get_bool("config.legacy.enabled") {
        let legacy_cfg_files = &[
            cfg.get_string("config.legacy.sheriff.permissionsPath").unwrap(),
            cfg.get_string("config.legacy.cncf.peoplePath").unwrap(),
        ];
        for filename in gh.list_pr_files(event.pull_request.number).await? {
            if legacy_cfg_files.contains(&filename) {
                return Ok(true);
            }
        }
    }

    Ok(false)
}

/// Helper for mapping any error into a `500 Internal Server Error` response.
fn internal_error<E>(err: E) -> StatusCode
where
    E: Into<Error> + Display,
{
    error!(%err);
    StatusCode::INTERNAL_SERVER_ERROR
}
