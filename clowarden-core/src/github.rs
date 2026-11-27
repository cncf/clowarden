//! This module defines an abstraction layer over the GitHub API.

use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex as StdMutex, RwLock},
    time::Duration,
};

use anyhow::{Context, Error, Result, format_err};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as b64};
#[cfg(test)]
use mockall::automock;
use octorust::{
    Client, ClientError,
    auth::{Credentials, InstallationTokenGenerator, JWTCredentials},
};
#[cfg(test)]
use reqwest::StatusCode;
use reqwest_middleware::ClientBuilder;
use reqwest_tracing::TracingMiddleware;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::cfg::{GitHubApp, Organization};

mod governor;
#[cfg(any(test, feature = "testing"))]
pub mod http_fake;
#[cfg(test)]
mod tests;

pub use governor::{
    CancelledError, RateLimitError, RateLimitGovernor, RateLimitPolicy, RequestKind, is_cancelled_error,
    is_rate_limit_error,
};

use governor::GitHubMiddleware;
#[cfg(test)]
use governor::UnauthorizedError;

/// Default TCP connect timeout for outbound GitHub HTTP requests.
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Default per-request timeout for outbound GitHub HTTP requests.
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Trait that defines some operations a GH implementation must support.
#[async_trait]
#[allow(clippy::ref_option_ref)]
#[cfg_attr(test, automock)]
pub trait GH {
    /// Get file content.
    async fn get_file_content(&self, src: &Source, path: &str) -> Result<String>;
}

/// Type alias to represent a GH trait object.
pub type DynGH = Arc<dyn GH + Send + Sync>;

/// Shared pool of GitHub API clients and rate-limit governance state reused
/// across all GitHub API wrappers in a process.
pub struct ClientPool {
    /// Cached `octorust::Client` instances keyed by installation id.
    clients: RwLock<HashMap<Option<i64>, Arc<Client>>>,
    /// Shared rate-limit, concurrency, and mutation-pacing governor.
    governor: Arc<RateLimitGovernor>,
    /// Shared underlying reqwest client for connection pooling.
    http: reqwest::Client,
    /// Process-wide cancellation token interrupting waits during shutdown.
    shutdown_token: CancellationToken,

    /// GitHub App JWT credentials, when authenticating as a GitHub App.
    app_credentials: Option<JWTCredentials>,
    /// Optional base URL override used by tests targeting a local fake server.
    host_override: Option<String>,
    /// Personal access token, when authenticating without a GitHub App.
    token: Option<String>,
}

impl ClientPool {
    /// Create a new client pool pointed at a local test server using the real
    /// middleware and governor stack.
    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    pub fn new_for_testing(
        host_override: String,
        policy: RateLimitPolicy,
        shutdown_token: CancellationToken,
    ) -> Self {
        let http = reqwest::Client::builder().no_proxy().build().expect("http client to be built");

        Self {
            clients: RwLock::new(HashMap::new()),
            governor: Arc::new(RateLimitGovernor::new(policy)),
            http,
            shutdown_token,
            app_credentials: None,
            host_override: Some(host_override),
            token: Some("token".to_string()),
        }
    }

    /// Create a new client pool pointed at a local test server using GitHub
    /// App JWT credentials so installation token exchanges flow through the
    /// real middleware and governor stack.
    #[cfg(test)]
    pub(crate) fn new_with_app_creds_for_testing(
        gh_app: &GitHubApp,
        host_override: String,
        policy: RateLimitPolicy,
        shutdown_token: CancellationToken,
    ) -> Result<Self> {
        let private_key = pem::parse(&gh_app.private_key)?.contents().to_owned();
        let jwt_credentials =
            JWTCredentials::new(gh_app.app_id, private_key).context("error setting up credentials")?;
        let http = reqwest::Client::builder().no_proxy().build().expect("http client to be built");

        Ok(Self {
            clients: RwLock::new(HashMap::new()),
            governor: Arc::new(RateLimitGovernor::new(policy)),
            http,
            shutdown_token,
            app_credentials: Some(jwt_credentials),
            host_override: Some(host_override),
            token: None,
        })
    }

    /// Create a new client pool using the GitHub App credentials provided.
    pub fn new_with_app_creds(gh_app: &GitHubApp) -> Result<Self> {
        Self::new_with_app_creds_and_options(gh_app, RateLimitPolicy::default(), CancellationToken::new())
    }

    /// Create a new client pool using the GitHub App credentials, rate-limit
    /// policy, and shutdown cancellation token provided.
    pub fn new_with_app_creds_and_options(
        gh_app: &GitHubApp,
        policy: RateLimitPolicy,
        shutdown_token: CancellationToken,
    ) -> Result<Self> {
        // Parse the GitHub App private key and build JWT credentials
        let private_key = pem::parse(&gh_app.private_key)?.contents().to_owned();
        let jwt_credentials =
            JWTCredentials::new(gh_app.app_id, private_key).context("error setting up credentials")?;

        // Build shared reqwest HTTP client with explicit timeouts
        let http = reqwest::Client::builder()
            .connect_timeout(HTTP_CONNECT_TIMEOUT)
            .timeout(HTTP_REQUEST_TIMEOUT)
            .build()?;

        Ok(Self {
            clients: RwLock::new(HashMap::new()),
            governor: Arc::new(RateLimitGovernor::new(policy)),
            http,
            shutdown_token,
            app_credentials: Some(jwt_credentials),
            host_override: None,
            token: None,
        })
    }

    /// Create a new client pool using the personal access token provided.
    pub fn new_with_token(token: String) -> Result<Self> {
        Self::new_with_token_and_options(token, RateLimitPolicy::default(), CancellationToken::new())
    }

    /// Create a new client pool using the personal access token, rate-limit
    /// policy, and shutdown cancellation token provided.
    pub fn new_with_token_and_options(
        token: String,
        policy: RateLimitPolicy,
        shutdown_token: CancellationToken,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(HTTP_CONNECT_TIMEOUT)
            .timeout(HTTP_REQUEST_TIMEOUT)
            .build()?;

        Ok(Self {
            clients: RwLock::new(HashMap::new()),
            governor: Arc::new(RateLimitGovernor::new(policy)),
            http,
            shutdown_token,
            app_credentials: None,
            host_override: None,
            token: Some(token),
        })
    }

    /// Execute a GitHub API operation for `inst_id` under the shared governor.
    ///
    /// Both the caller's `cancel` token and the pool's `shutdown_token` abort
    /// any pending wait before dispatch without spawning background tasks. When
    /// using GitHub App installation credentials, an HTTP `401 Unauthorized`
    /// evicts the cached client (minting a fresh JWT and installation token)
    /// and retries the operation once.
    pub async fn execute<T, F, Fut>(
        &self,
        inst_id: Option<i64>,
        kind: RequestKind,
        cancel: &CancellationToken,
        op: F,
    ) -> Result<T>
    where
        F: Fn(Arc<Client>) -> Fut,
        Fut: Future<Output = std::result::Result<T, ClientError>>,
    {
        // Resolve the cached client for this installation and execute under
        // governance, recovering once from a stale or revoked installation
        // token (or sleep-staled JWT) while keeping shared wait/retry budgets
        let current_client = StdMutex::new(self.get_client(inst_id)?);
        self.governor
            .execute_with_auth_refresh(
                inst_id,
                kind,
                &self.shutdown_token,
                cancel,
                || {
                    let client = current_client.lock().expect("client lock not to be poisoned").clone();
                    op(client)
                },
                || {
                    if !self.can_refresh_installation_client(inst_id) {
                        return Ok(false);
                    }
                    warn!(
                        ?inst_id,
                        "github api returned 401 unauthorized; refreshing installation client and retrying once"
                    );
                    let mut guard = current_client.lock().expect("client lock not to be poisoned");
                    *guard = self.refresh_client(inst_id, &guard)?;
                    Ok(true)
                },
            )
            .await
    }

    /// Get the GitHub API client for the installation id provided (if any),
    /// reusing cached instances when available and creating a new one
    /// otherwise.
    pub fn get_client(&self, inst_id: Option<i64>) -> Result<Arc<Client>> {
        if let Some(client) =
            self.clients.read().expect("clients lock not to be poisoned").get(&inst_id).cloned()
        {
            return Ok(client);
        }

        let client = Arc::new(self.build_client(inst_id)?);
        let mut clients = self.clients.write().expect("clients lock not to be poisoned");
        let entry = clients.entry(inst_id).or_insert_with(|| client.clone());
        Ok(entry.clone())
    }

    /// Return the shared rate-limit governor.
    #[must_use]
    pub fn governor(&self) -> &Arc<RateLimitGovernor> {
        &self.governor
    }

    /// Insert a pre-built `octorust::Client` into the pool for testing.
    #[cfg(any(test, feature = "testing"))]
    pub fn insert_client(&self, inst_id: Option<i64>, client: Arc<Client>) {
        self.clients.write().expect("clients lock not to be poisoned").insert(inst_id, client);
    }

    /// Build a new `octorust::Client` bound to the shared `reqwest::Client`
    /// and a per-installation [`GitHubMiddleware`].
    fn build_client(&self, inst_id: Option<i64>) -> Result<Client> {
        // Build user agent string for identifying this service
        let user_agent = format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));

        // Determine which GitHub credentials to use, minting a fresh JWT
        // cache per built client so refreshed clients never reuse a stale JWT
        let credentials = if let Some(inst_id) = inst_id {
            if let Some(app_creds) = &self.app_credentials {
                let fresh_jwt = JWTCredentials::new(app_creds.app_id, app_creds.private_key.clone())
                    .context("error setting up credentials")?;
                Credentials::InstallationToken(InstallationTokenGenerator::new(inst_id, fresh_jwt))
            } else if let Some(token) = self.token.clone() {
                Credentials::Token(token)
            } else {
                return Err(format_err!(
                    "error setting up github client: app credentials not provided"
                ));
            }
        } else {
            let Some(token) = self.token.clone() else {
                return Err(format_err!("error setting up github client: token not provided"));
            };
            Credentials::Token(token)
        };

        // Wrap the shared reqwest client with tracing and GitHub middleware
        let middleware_client = ClientBuilder::new(self.http.clone())
            .with(TracingMiddleware::default())
            .with(GitHubMiddleware::new(
                self.governor.clone(),
                self.shutdown_token.clone(),
                inst_id,
            ))
            .build();

        let mut client = Client::custom(user_agent, credentials, middleware_client);
        if let Some(host_override) = &self.host_override {
            client.with_host_override(host_override.clone());
        }

        Ok(client)
    }

    /// Return true when `inst_id` authenticates via GitHub App installation
    /// tokens that can be refreshed by rebuilding the cached client.
    fn can_refresh_installation_client(&self, inst_id: Option<i64>) -> bool {
        inst_id.is_some() && self.app_credentials.is_some()
    }

    /// Replace the cached client for `inst_id` if it is still `failed_client`,
    /// returning the already-refreshed client when another concurrent caller
    /// beat us to the replacement.
    fn refresh_client(&self, inst_id: Option<i64>, failed_client: &Arc<Client>) -> Result<Arc<Client>> {
        let mut clients = self.clients.write().expect("clients lock not to be poisoned");
        if let Some(existing) = clients.get(&inst_id)
            && !Arc::ptr_eq(existing, failed_client)
        {
            return Ok(existing.clone());
        }

        let fresh = Arc::new(self.build_client(inst_id)?);
        clients.insert(inst_id, fresh.clone());
        Ok(fresh)
    }
}

/// Return true if the error chain contains a GitHub `401 Unauthorized` response.
#[cfg(test)]
fn is_unauthorized_error(err: &Error) -> bool {
    err.chain().any(|cause| {
        cause.is::<UnauthorizedError>()
            || matches!(
                cause.downcast_ref::<ClientError>(),
                Some(ClientError::HttpError {
                    status: StatusCode::UNAUTHORIZED,
                    ..
                })
            )
    })
}

/// Per-job caching wrapper around a [`DynGH`] that caches successful file
/// content reads for the lifetime of a single job and respects cancellation.
pub struct CachingGH {
    /// Cached file contents keyed by `(inst_id, owner, repo, ref_, path)`.
    cache: Mutex<HashMap<FileCacheKey, String>>,
    /// Cancellation token for the owning job.
    cancel_token: CancellationToken,
    /// Underlying GitHub file reader.
    inner: DynGH,
}

impl CachingGH {
    /// Create a new per-job caching wrapper around `inner`.
    #[must_use]
    pub fn new(inner: DynGH, cancel_token: CancellationToken) -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
            cancel_token,
            inner,
        }
    }
}

#[async_trait]
impl GH for CachingGH {
    /// [`GH::get_file_content`].
    async fn get_file_content(&self, src: &Source, path: &str) -> Result<String> {
        // Abort immediately when the owning job has been cancelled
        if self.cancel_token.is_cancelled() {
            return Err(Error::new(CancelledError));
        }

        // Build per-job lookup key
        let key = FileCacheKey {
            owner: src.owner.clone(),
            path: path.to_string(),
            ref_: src.ref_.clone(),
            repo: src.repo.clone(),
            inst_id: src.inst_id,
        };

        // Return cached content when already fetched in this job
        if let Some(content) = self.cache.lock().await.get(&key).cloned() {
            return Ok(content);
        }

        // Fetch from the underlying reader while respecting job cancellation
        let content = tokio::select! {
            biased;

            () = self.cancel_token.cancelled() => return Err(Error::new(CancelledError)),
            result = self.inner.get_file_content(src, path) => result?,
        };

        // Store successful read in the per-job cache
        self.cache.lock().await.insert(key, content.clone());
        Ok(content)
    }
}

/// Cache key identifying a specific file at a specific repository reference.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FileCacheKey {
    /// Repository owner login.
    owner: String,
    /// Repository-relative file path.
    path: String,
    /// Git reference (branch or commit SHA).
    ref_: String,
    /// Repository name.
    repo: String,

    /// Installation id associated with the read, if any.
    inst_id: Option<i64>,
}

/// GH implementation backed by the GitHub API.
pub struct GHApi {
    /// Shared GitHub API client pool and rate-limit governor.
    pool: Arc<ClientPool>,
}

impl GHApi {
    /// Create a new GHApi instance backed by the shared [`ClientPool`].
    #[must_use]
    pub fn new(pool: Arc<ClientPool>) -> Self {
        Self { pool }
    }

    /// Create a new GHApi instance using the app credentials provided in the
    /// configuration.
    pub fn new_with_app_creds(gh_app: &GitHubApp) -> Result<Self> {
        Ok(Self {
            pool: Arc::new(ClientPool::new_with_app_creds(gh_app)?),
        })
    }

    /// Create a new GHApi instance using the token provided.
    pub fn new_with_token(token: String) -> Result<Self> {
        Ok(Self {
            pool: Arc::new(ClientPool::new_with_token(token)?),
        })
    }

    /// Return the underlying client pool.
    #[must_use]
    pub fn pool(&self) -> &Arc<ClientPool> {
        &self.pool
    }
}

#[async_trait]
impl GH for GHApi {
    /// [`GH::get_file_content`].
    async fn get_file_content(&self, src: &Source, path: &str) -> Result<String> {
        let cancel = CancellationToken::new();
        let mut content = self
            .pool
            .execute(src.inst_id, RequestKind::Read, &cancel, |client| async move {
                client.repos().get_content_file(&src.owner, &src.repo, path, &src.ref_).await
            })
            .await?
            .content
            .as_bytes()
            .to_owned();
        content.retain(|b| !b" \n\t\r\x0b\x0c".contains(b));
        let decoded_content = String::from_utf8(b64.decode(content)?)?;
        Ok(decoded_content)
    }
}

/// Information about the origin of a file located in a GitHub repository.
pub struct Source {
    pub inst_id: Option<i64>,
    pub owner: String,
    pub repo: String,
    pub ref_: String,
}

impl From<&Organization> for Source {
    fn from(org: &Organization) -> Self {
        Source {
            inst_id: Some(org.installation_id),
            owner: org.name.clone(),
            repo: org.repository.clone(),
            ref_: org.branch.clone(),
        }
    }
}
