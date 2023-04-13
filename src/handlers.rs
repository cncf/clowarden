use crate::{
    github::{self, DynGH, Event, EventError, PullRequestEvent, PullRequestEventAction},
    jobs::Job,
};
use anyhow::{format_err, Error, Result};
use axum::{
    body::Bytes,
    extract::{FromRef, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::post,
    Router,
};
use config::Config;
use hmac::{Hmac, Mac};
use octorust::types::JobStatus;
use sha2::Sha256;
use std::sync::Arc;
use tokio::sync::mpsc;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;
use tracing::{error, instrument, trace};

/// Router's state.
#[derive(Clone, FromRef)]
struct RouterState {
    cfg: Arc<Config>,
    gh: DynGH,
    webhook_secret: String,
    jobs_tx: mpsc::UnboundedSender<Job>,
}

/// Setup HTTP server router.
pub(crate) fn setup_router(cfg: Arc<Config>, gh: DynGH, jobs_tx: mpsc::UnboundedSender<Job>) -> Router {
    // Setup webhook secret
    let webhook_secret = cfg.get_string("githubApp.webhookSecret").unwrap();

    // Setup router
    Router::new()
        .route("/api/events", post(event))
        .layer(ServiceBuilder::new().layer(TraceLayer::new_for_http()))
        .with_state(RouterState {
            cfg,
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
        headers.get(github::SIGNATURE_HEADER),
        webhook_secret.as_bytes(),
        &body[..],
    )
    .is_err()
    {
        return Err((StatusCode::BAD_REQUEST, "no valid signature found".to_string()));
    };

    // Parse event
    let event_header = &headers.get(github::EVENT_HEADER).cloned();
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
                    _ = jobs_tx.send(Job::Validate(event.pull_request));
                }
                PullRequestEventAction::Closed if event.pull_request.merged => {
                    // Enqueue reconcile job
                    _ = jobs_tx.send(Job::Reconcile(Some(event.pull_request)));
                }
                _ => {}
            }
        }
    }

    Ok(())
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
