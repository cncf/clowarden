use anyhow::{Context, Error};
use async_trait::async_trait;
use config::Config;
#[cfg(test)]
use mockall::automock;
use octorust::{
    auth::{Credentials, InstallationTokenGenerator, JWTCredentials},
    types::{
        Affiliation, Collaborator, MinimalRepository, Order, ReposListOrgSort, ReposListOrgType, SimpleUser,
        Team, TeamsListMembersInOrgRole,
    },
    Client, ClientError,
};
use std::sync::Arc;

/// Type alias to represent a Svc trait object.
pub(crate) type DynSvc = Arc<dyn Svc + Send + Sync>;

/// Trait that defines some operations a Svc implementation must support.
#[async_trait]
#[cfg_attr(test, automock)]
pub(crate) trait Svc {
    /// List repositories in the organization.
    async fn list_repositories(&self) -> Result<Vec<MinimalRepository>, ClientError>;

    /// List repository's collaborators.
    async fn list_repository_collaborators(&self, repo_name: &str) -> Result<Vec<Collaborator>, ClientError>;

    /// List repository's teams.
    async fn list_repository_teams(&self, repo_name: &str) -> Result<Vec<Team>, ClientError>;

    /// List team's maintainers.
    async fn list_team_maintainers(&self, team_name: &str) -> Result<Vec<SimpleUser>, ClientError>;

    /// List team's members.
    async fn list_team_members(&self, team_name: &str) -> Result<Vec<SimpleUser>, ClientError>;

    /// List teams in the organization.
    async fn list_teams(&self) -> Result<Vec<Team>, ClientError>;

    /// Remove team.
    async fn remove_team(&self, team_name: &str) -> Result<(), ClientError>;
}

/// Svc implementation backed by the GitHub API.
pub(crate) struct SvcApi {
    client: Client,
    org: String,
}

impl SvcApi {
    /// Create a new SvcApi instance.
    pub(crate) fn new(cfg: Arc<Config>) -> Result<Self, Error> {
        // Setup GitHub app credentials
        let app_id = cfg.get_int("githubApp.appId").unwrap();
        let app_private_key = pem::parse(cfg.get_string("githubApp.privateKey").unwrap())?
            .contents()
            .to_owned();
        let credentials =
            JWTCredentials::new(app_id, app_private_key).context("error setting up credentials")?;

        // Setup GitHub API client
        let inst_id = cfg.get_int("githubApp.installationId").unwrap();
        let tg = InstallationTokenGenerator::new(inst_id, credentials);
        let client = Client::new(
            format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
            Credentials::InstallationToken(tg),
        )?;

        Ok(Self {
            client,
            org: cfg.get_string("config.organization").unwrap(),
        })
    }
}

#[async_trait]
impl Svc for SvcApi {
    async fn list_repositories(&self) -> Result<Vec<MinimalRepository>, ClientError> {
        self.client
            .repos()
            .list_all_for_org(
                &self.org,
                ReposListOrgType::All,
                ReposListOrgSort::FullName,
                Order::Asc,
            )
            .await
    }

    async fn list_repository_collaborators(&self, repo_name: &str) -> Result<Vec<Collaborator>, ClientError> {
        self.client
            .repos()
            .list_all_collaborators(&self.org, repo_name, Affiliation::All)
            .await
    }

    async fn list_repository_teams(&self, repo_name: &str) -> Result<Vec<Team>, ClientError> {
        self.client.repos().list_all_teams(&self.org, repo_name).await
    }

    async fn list_team_maintainers(&self, team_name: &str) -> Result<Vec<SimpleUser>, ClientError> {
        self.client
            .teams()
            .list_all_members_in_org(&self.org, team_name, TeamsListMembersInOrgRole::Maintainer)
            .await
    }

    async fn list_team_members(&self, team_name: &str) -> Result<Vec<SimpleUser>, ClientError> {
        self.client
            .teams()
            .list_all_members_in_org(&self.org, team_name, TeamsListMembersInOrgRole::Member)
            .await
    }

    async fn list_teams(&self) -> Result<Vec<Team>, ClientError> {
        self.client.teams().list_all(&self.org).await
    }

    async fn remove_team(&self, team_name: &str) -> Result<(), ClientError> {
        self.client.teams().delete_in_org(&self.org, team_name).await
    }
}
