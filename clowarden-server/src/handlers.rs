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
    response::{IntoResponse, Redirect},
    routing::{get, get_service, post},
    Router,
};
use config::Config;
use hmac::{Hmac, Mac};
use mime::APPLICATION_JSON;
use octorust::types::JobStatus;
use sha2::Sha256;
use std::{fmt::Display, path::Path, sync::Arc};
use tokio::sync::mpsc;
use tower::ServiceBuilder;
use tower_http::{
    services::{ServeDir, ServeFile},
    set_header::SetResponseHeader,
    trace::TraceLayer,
    validate_request::ValidateRequestHeaderLayer,
};
use tracing::{error, instrument, trace};

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
) -> Result<Router> {
    // Setup some paths
    let static_path = cfg.get_string("server.staticPath").unwrap();
    let root_index_path = Path::new(&static_path).join("index.html");
    let audit_path = Path::new(&static_path).join("audit");
    let audit_index_path = audit_path.join("index.html");

    // Setup webhook secret
    let webhook_secret = cfg.get_string("server.githubApp.webhookSecret").unwrap();

    // Setup audit router
    let mut audit_router = Router::new()
        .route("/api/changes/search", get(search_changes))
        .nest_service(
            "/static",
            get_service(SetResponseHeader::overriding(
                ServeDir::new(audit_path),
                CACHE_CONTROL,
                HeaderValue::try_from(format!("max-age={STATIC_CACHE_MAX_AGE}"))?,
            )),
        )
        .route("/", get_service(ServeFile::new(&audit_index_path)))
        .fallback_service(get_service(ServeFile::new(&audit_index_path)));

    // Setup basic auth
    if cfg.get_bool("server.basicAuth.enabled").unwrap_or(false) {
        let username = cfg.get_string("server.basicAuth.username")?;
        let password = cfg.get_string("server.basicAuth.password")?;
        audit_router = audit_router.layer(ValidateRequestHeaderLayer::basic(&username, &password));
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
            cfg,
            db,
            gh,
            webhook_secret,
            jobs_tx,
        });

    Ok(router)
}

/// Handler that takes care of health check requests.
async fn health_check() -> impl IntoResponse {
    ""
}

/// Handler that processes webhook events from GitHub.
#[allow(clippy::let_with_type_underscore)]
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
    let cfg_repo = &cfg.get_string("server.config.repository").unwrap();
    if cfg_repo != &event.repository.name {
        return Ok(false);
    }

    // Check if base branch in PR matches with config
    let cfg_branch = &cfg.get_string("server.config.branch").unwrap();
    if cfg_branch != &event.pull_request.base.ref_ {
        return Ok(false);
    }

    // Check if any of the configuration files is on the pr
    if let Ok(true) = cfg.get_bool("server.config.legacy.enabled") {
        let mut legacy_cfg_files =
            vec![cfg.get_string("server.config.legacy.sheriff.permissionsPath").unwrap()];
        if let Ok(people_path) = cfg.get_string("server.config.legacy.cncf.peoplePath") {
            legacy_cfg_files.push(people_path);
        };
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
