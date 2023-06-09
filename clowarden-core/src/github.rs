use anyhow::{Context, Result};
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as b64, Engine as _};
use config::Config;
#[cfg(test)]
use mockall::automock;
use octorust::{
    auth::{Credentials, InstallationTokenGenerator, JWTCredentials},
    Client,
};
use std::sync::Arc;

/// Trait that defines some operations a GH implementation must support.
#[async_trait]
#[cfg_attr(test, automock)]
pub trait GH {
    /// Get file content.
    async fn get_file_content(&self, path: &str, ref_: Option<&str>) -> Result<String>;
}

/// Type alias to represent a GH trait object.
pub type DynGH = Arc<dyn GH + Send + Sync>;

/// GH implementation backed by the GitHub API.
pub struct GHApi {
    client: Client,
    org: String,
    repo: String,
    branch: String,
}

impl GHApi {
    /// Create a new GHApi instance.
    pub fn new(org: String, repo: String, branch: String, token: String) -> Result<Self> {
        // Setup GitHub API client
        let client = Client::new(
            format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
            Credentials::Token(token),
        )?;

        Ok(Self {
            client,
            org,
            repo,
            branch,
        })
    }

    /// Create a new GHApi instance from the configuration instance provided.
    pub fn new_from_config(cfg: Arc<Config>) -> Result<Self> {
        // Setup GitHub app credentials
        let app_id = cfg.get_int("server.githubApp.appId").unwrap();
        let app_private_key =
            pem::parse(cfg.get_string("server.githubApp.privateKey").unwrap())?.contents().to_owned();
        let credentials =
            JWTCredentials::new(app_id, app_private_key).context("error setting up credentials")?;

        // Setup GitHub API client
        let inst_id = cfg.get_int("server.githubApp.installationId").unwrap();
        let tg = InstallationTokenGenerator::new(inst_id, credentials);
        let client = Client::new(
            format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
            Credentials::InstallationToken(tg),
        )?;

        Ok(Self {
            client,
            org: cfg.get_string("server.config.organization").unwrap(),
            repo: cfg.get_string("server.config.repository").unwrap(),
            branch: cfg.get_string("server.config.branch").unwrap(),
        })
    }
}

#[async_trait]
impl GH for GHApi {
    /// [GH::get_file_content]
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
}
