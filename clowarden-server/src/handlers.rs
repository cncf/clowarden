//! This module defines the handlers used to process HTTP requests to the
//! supported endpoints.

use std::{fmt::Display, path::Path};

use anyhow::{Error, Result, bail, format_err};
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
use sha2::Sha256;
use tokio::sync::mpsc::{self, error::TrySendError};
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
    github::{
        CHECK_RUN_NAME, CheckRunEvent, CheckRunEventAction, CheckRunPullRequest, CheckSuiteEvent, Event,
        EventError, PullRequestEvent, PullRequestEventAction,
    },
    jobs::{CheckRunRerequestedInput, LatestPrHeads, PullRequestTriageInput, TriageInput},
};

#[cfg(test)]
mod tests;

/// Audit index HTML document cache duration.
const AUDIT_INDEX_CACHE_MAX_AGE: usize = 300;

/// Default cache duration for some API endpoints.
const DEFAULT_API_MAX_AGE: usize = 300;

/// Maximum number of changes that can be requested in a single search.
const MAX_SEARCH_LIMIT: usize = 100;

/// Static files cache duration.
const STATIC_CACHE_MAX_AGE: usize = 365 * 24 * 60 * 60;

/// Header representing the kind of the event received.
const GITHUB_EVENT_HEADER: &str = "X-GitHub-Event";

/// Header representing the event payload signature.
const GITHUB_SIGNATURE_HEADER: &str = "X-Hub-Signature-256";

/// Header that indicates the number of items available for pagination purposes.
const PAGINATION_TOTAL_COUNT: &str = "pagination-total-count";

/// Message returned when a search query cannot be parsed.
const INVALID_SEARCH_QUERY_MSG: &str = "invalid search query";

/// Router's state.
#[derive(Clone, FromRef)]
struct RouterState {
    db: DynDB,
    latest_pr_heads: LatestPrHeads,
    orgs: Vec<Organization>,
    triage_tx: mpsc::Sender<TriageInput>,
    webhook_secret: String,

    webhook_secret_fallback: Option<String>,
}

/// Setup HTTP server router.
pub(crate) fn setup_router(
    cfg: &Config,
    db: DynDB,
    latest_pr_heads: LatestPrHeads,
    triage_tx: mpsc::Sender<TriageInput>,
) -> Result<Router> {
    // Setup some paths
    let static_path = cfg.server.static_path.clone();
    let root_index_path = Path::new(&static_path).join("index.html");
    let audit_path = Path::new(&static_path).join("audit");
    let audit_index_path = audit_path.join("index.html");

    // Setup static files cache header, used only on successful and not modified responses
    let static_cache_control = HeaderValue::try_from(format!("max-age={STATIC_CACHE_MAX_AGE}"))?;
    let static_cache_header = move |response: &Response<_>| {
        let status = response.status();
        (status.is_success() || status == StatusCode::NOT_MODIFIED).then(|| static_cache_control.clone())
    };

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
                static_cache_header.clone(),
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
                static_cache_header,
            )),
        )
        .fallback_service(get_service(ServeFile::new(&root_index_path)))
        .layer(ServiceBuilder::new().layer(TraceLayer::new_for_http()))
        .with_state(RouterState {
            db,
            latest_pr_heads,
            orgs: cfg.organizations.clone().unwrap_or_default(),
            triage_tx,
            webhook_secret: cfg.server.github_app.webhook_secret.clone(),
            webhook_secret_fallback: cfg.server.github_app.webhook_secret_fallback.clone(),
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
#[allow(clippy::unused_async)]
#[instrument(skip_all, err(Debug))]
async fn event(
    State(webhook_secret): State<String>,
    State(webhook_secret_fallback): State<Option<String>>,
    State(latest_pr_heads): State<LatestPrHeads>,
    State(triage_tx): State<mpsc::Sender<TriageInput>>,
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

    // Enqueue actionable events for asynchronous triage and respond quickly
    match event {
        Event::CheckRun(event) => {
            enqueue_check_run_event(&event, &orgs, &triage_tx)?;
        }
        Event::CheckSuite(event) => {
            enqueue_check_suite_event(&event, &orgs, &triage_tx)?;
        }
        Event::PullRequest(event) => {
            enqueue_pull_request_event(*event, &orgs, &latest_pr_heads, &triage_tx)?;
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
    // Parse and validate search input
    let query = query.unwrap_or_default();
    let Ok(input) = serde_qs::from_str::<SearchChangesInput>(&query) else {
        return Ok((StatusCode::BAD_REQUEST, INVALID_SEARCH_QUERY_MSG).into_response());
    };
    if let Err(err) = validate_search_pagination(&input) {
        return Ok((StatusCode::BAD_REQUEST, err.to_string()).into_response());
    }

    // Search changes in database
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

/// Enqueue a `check_run.rerequested` webhook event for matching organizations.
fn enqueue_check_run_event(
    event: &CheckRunEvent,
    orgs: &[Organization],
    triage_tx: &mpsc::Sender<TriageInput>,
) -> std::result::Result<(), (StatusCode, String)> {
    // Ignore non-rerequested actions and check runs belonging to other apps
    if event.action != CheckRunEventAction::Rerequested || event.check_run.name != CHECK_RUN_NAME {
        return Ok(());
    }
    enqueue_rerequested_check(
        event.organization.as_ref().map(|org| org.login.as_str()),
        &event.repository.name,
        &event.check_run.head_sha,
        &event.check_run.pull_requests,
        orgs,
        triage_tx,
    )
}

/// Enqueue a `check_suite.rerequested` webhook event ("Re-run all checks") for
/// matching organizations.
fn enqueue_check_suite_event(
    event: &CheckSuiteEvent,
    orgs: &[Organization],
    triage_tx: &mpsc::Sender<TriageInput>,
) -> std::result::Result<(), (StatusCode, String)> {
    // Ignore actions other than rerequested
    if event.action != CheckRunEventAction::Rerequested {
        return Ok(());
    }
    enqueue_rerequested_check(
        event.organization.as_ref().map(|org| org.login.as_str()),
        &event.repository.name,
        &event.check_suite.head_sha,
        &event.check_suite.pull_requests,
        orgs,
        triage_tx,
    )
}

/// Enqueue a re-requested check run or check suite for every configured
/// organization matching `(organization_login, repository_name)`.
fn enqueue_rerequested_check(
    organization_login: Option<&str>,
    repository_name: &str,
    head_sha: &str,
    pull_requests: &[CheckRunPullRequest],
    orgs: &[Organization],
    triage_tx: &mpsc::Sender<TriageInput>,
) -> std::result::Result<(), (StatusCode, String)> {
    // Match registered organizations owning the repository
    let Some(org_login) = organization_login else {
        return Ok(());
    };
    let matching_orgs: Vec<Organization> = orgs
        .iter()
        .filter(|org| org.name == org_login && org.repository == repository_name)
        .cloned()
        .collect();
    if matching_orgs.is_empty() {
        return Ok(());
    }

    // Reserve a triage slot for each matching target branch
    let received_at = tokio::time::Instant::now();
    for org in matching_orgs {
        let pr_numbers: Vec<i64> =
            pull_requests.iter().filter(|pr| pr.base.ref_ == org.branch).map(|pr| pr.number).collect();
        if !pull_requests.is_empty() && pr_numbers.is_empty() {
            continue;
        }
        let permit = reserve_triage_slot(triage_tx)?;
        permit.send(TriageInput::CheckRunRerequested(CheckRunRerequestedInput {
            head_sha: head_sha.to_string(),
            org,
            pr_numbers,
            received_at,
        }));
    }
    Ok(())
}

/// Enqueue an actionable pull request webhook event after updating latest head
/// state (ignoring stale out-of-order or redelivered webhooks).
fn enqueue_pull_request_event(
    event: PullRequestEvent,
    orgs: &[Organization],
    latest_pr_heads: &LatestPrHeads,
    triage_tx: &mpsc::Sender<TriageInput>,
) -> std::result::Result<(), (StatusCode, String)> {
    // Check event comes from a registered organization target
    let Some(gh_org) = &event.organization else {
        return Ok(());
    };
    let Some(org) = find_target_org(
        orgs,
        &gh_org.login,
        &event.repository.name,
        &event.pull_request.base.ref_,
    )
    .cloned() else {
        return Ok(());
    };

    // Ignore unsupported actions, non-base edits, and closed-unmerged PRs before triage
    if !is_actionable_pr_event(&event) {
        return Ok(());
    }

    // Reserve a slot atomically and record the latest PR head SHA before
    // publishing so triage cannot observe a stale SHA
    let is_validation_event = event.action.is_validation_action();
    let org_name = org.name.clone();
    let pr_number = event.pull_request.number;
    let head_sha = event.pull_request.head.sha.clone();
    let triage_input = PullRequestTriageInput::new(org, event.action, event.pull_request, event.before);
    let updated_at = triage_input.pr_updated_at;
    let before_sha = triage_input.pr_before_sha.clone();
    let permit = reserve_triage_slot(triage_tx)?;
    if is_validation_event
        && latest_pr_heads.record(&org_name, pr_number, head_sha, updated_at, before_sha).is_none()
    {
        return Ok(());
    }
    permit.send(TriageInput::PullRequest(triage_input));
    Ok(())
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

/// Reserve a slot in the webhook triage queue, translating backpressure into
/// `503 Service Unavailable`.
fn reserve_triage_slot(
    triage_tx: &mpsc::Sender<TriageInput>,
) -> std::result::Result<mpsc::Permit<'_, TriageInput>, (StatusCode, String)> {
    match triage_tx.try_reserve() {
        Ok(permit) => Ok(permit),
        Err(TrySendError::Full(())) => {
            error!("webhook triage queue is full; rejecting delivery");
            Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "webhook triage queue is full".to_string(),
            ))
        }
        Err(TrySendError::Closed(())) => {
            error!("webhook triage queue is closed; rejecting delivery");
            Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "webhook triage queue is unavailable".to_string(),
            ))
        }
    }
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

/// Check if the pull request event requires triage. Closed pull requests that
/// were not merged and `edited` events that did not retarget the base branch
/// of an open pull request are ignored before querying GitHub.
fn is_actionable_pr_event(event: &PullRequestEvent) -> bool {
    match event.action {
        PullRequestEventAction::Opened
        | PullRequestEventAction::Reopened
        | PullRequestEventAction::Synchronize => true,
        PullRequestEventAction::Edited => {
            event.pull_request.state == octorust::types::State::Open
                && event.changes.as_ref().is_some_and(|changes| changes.base.is_some())
        }
        PullRequestEventAction::Closed => event.pull_request.merged,
        PullRequestEventAction::Other => false,
    }
}

/// Validate the pagination values in the search input provided. The database
/// reads them as 32-bit integers, and the limit is capped to bound the size of
/// the response.
fn validate_search_pagination(input: &SearchChangesInput) -> Result<()> {
    if input.limit.is_some_and(|limit| limit > MAX_SEARCH_LIMIT) {
        bail!("limit must not be greater than {MAX_SEARCH_LIMIT}");
    }
    if input.offset.is_some_and(|offset| i32::try_from(offset).is_err()) {
        bail!("offset must not be greater than {}", i32::MAX);
    }

    Ok(())
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
