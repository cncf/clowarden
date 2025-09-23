//! This module defines an abstraction layer over the GitHub API.

use std::sync::Arc;

use anyhow::{Context, Result, format_err};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as b64};
#[cfg(test)]
use mockall::automock;
use octorust::{
    Client,
    auth::{Credentials, InstallationTokenGenerator, JWTCredentials},
};

use crate::cfg::{GitHubApp, Organization};

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

/// GH implementation backed by the GitHub API.
#[derive(Default)]
pub struct GHApi {
    app_credentials: Option<JWTCredentials>,
    token: Option<String>,
}

impl GHApi {
    /// Create a new GHApi instance using the token provided.
    #[must_use]
    pub fn new_with_token(token: String) -> Self {
        Self {
            token: Some(token),
            ..Default::default()
        }
    }

    /// Create a new GHApi instance using the app credentials provided in the
    /// configuration.
    pub fn new_with_app_creds(gh_app: &GitHubApp) -> Result<Self> {
        // Setup GitHub app credentials
        let private_key = pem::parse(&gh_app.private_key)?.contents().to_owned();
        let jwt_credentials =
            JWTCredentials::new(gh_app.app_id, private_key).context("error setting up credentials")?;

        Ok(Self {
            app_credentials: Some(jwt_credentials),
            ..Default::default()
        })
    }

    /// Setup GitHub API client for the installation id provided (if any).
    fn setup_client(&self, inst_id: Option<i64>) -> Result<Client> {
        let user_agent = format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));

        let credentials = if let Some(inst_id) = inst_id {
            let Some(app_creds) = self.app_credentials.clone() else {
                return Err(format_err!(
                    "error setting up github client: app credentials not provided"
                ));
            };
            Credentials::InstallationToken(InstallationTokenGenerator::new(inst_id, app_creds))
        } else {
            let Some(token) = self.token.clone() else {
                return Err(format_err!("error setting up github client: token not provided"));
            };
            Credentials::Token(token)
        };

        Ok(Client::new(user_agent, credentials)?)
    }
}

#[async_trait]
impl GH for GHApi {
    /// [GH::get_file_content]
    async fn get_file_content(&self, src: &Source, path: &str) -> Result<String> {
        let client = self.setup_client(src.inst_id)?;
        let mut content = client
            .repos()
            .get_content_file(&src.owner, &src.repo, path, &src.ref_)
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
