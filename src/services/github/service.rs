use super::state::{Repository, RepositoryName, Role, Visibility};
use crate::directory::{self, TeamName, UserName};
use anyhow::{Context, Error};
use async_trait::async_trait;
use config::Config;
#[cfg(test)]
use mockall::automock;
use octorust::{
    auth::{Credentials, InstallationTokenGenerator, JWTCredentials},
    types::{
        Affiliation, Collaborator, MinimalRepository, Order, Privacy, ReposAddCollaboratorRequest,
        ReposCreateInOrgRequest, ReposCreateInOrgRequestVisibility, ReposListOrgSort, ReposListOrgType,
        ReposUpdateRequest, SimpleUser, Team, TeamMembershipRole, TeamsAddUpdateMembershipUserInOrgRequest,
        TeamsAddUpdateRepoPermissionsInOrgRequest, TeamsCreateRequest, TeamsListMembersInOrgRole,
    },
    Client, ClientError,
};
use std::sync::Arc;

/// Trait that defines some operations a Svc implementation must support.
#[async_trait]
#[cfg_attr(test, automock)]
pub(crate) trait Svc {
    /// Add repository to organization.
    async fn add_repository(&self, repo: &Repository) -> Result<(), ClientError>;

    /// Add collaborator to repository.
    async fn add_repository_collaborator(
        &self,
        repo_name: &RepositoryName,
        user_name: &UserName,
        role: &Role,
    ) -> Result<(), ClientError>;

    /// Add team to repository.
    async fn add_repository_team(
        &self,
        repo_name: &RepositoryName,
        team_name: &TeamName,
        role: &Role,
    ) -> Result<(), ClientError>;

    /// Add team to organization.
    async fn add_team(&self, team: &directory::Team) -> Result<(), ClientError>;

    /// Add maintainer to the team.
    async fn add_team_maintainer(
        &self,
        team_name: &TeamName,
        user_name: &UserName,
    ) -> Result<(), ClientError>;

    /// Add member to the team.
    async fn add_team_member(&self, team_name: &TeamName, user_name: &UserName) -> Result<(), ClientError>;

    /// List repositories in the organization.
    async fn list_repositories(&self) -> Result<Vec<MinimalRepository>, ClientError>;

    /// List repository's collaborators.
    async fn list_repository_collaborators(
        &self,
        repo_name: &RepositoryName,
    ) -> Result<Vec<Collaborator>, ClientError>;

    /// List repository's teams.
    async fn list_repository_teams(&self, repo_name: &RepositoryName) -> Result<Vec<Team>, ClientError>;

    /// List team's maintainers.
    async fn list_team_maintainers(&self, team_name: &TeamName) -> Result<Vec<SimpleUser>, ClientError>;

    /// List team's members.
    async fn list_team_members(&self, team_name: &TeamName) -> Result<Vec<SimpleUser>, ClientError>;

    /// List teams in the organization.
    async fn list_teams(&self) -> Result<Vec<Team>, ClientError>;

    /// Remove repository from organization.
    async fn remove_repository(&self, repo_name: &RepositoryName) -> Result<(), ClientError>;

    /// Remove collaborator from repository.
    async fn remove_repository_collaborator(
        &self,
        repo_name: &RepositoryName,
        user_name: &UserName,
    ) -> Result<(), ClientError>;

    /// Remove team from repository.
    async fn remove_repository_team(
        &self,
        repo_name: &RepositoryName,
        team_name: &TeamName,
    ) -> Result<(), ClientError>;

    /// Remove team from organization.
    async fn remove_team(&self, team_name: &TeamName) -> Result<(), ClientError>;

    /// Remove maintainer from the team.
    async fn remove_team_maintainer(
        &self,
        team_name: &TeamName,
        user_name: &UserName,
    ) -> Result<(), ClientError>;

    /// Remove member from the team.
    async fn remove_team_member(&self, team_name: &TeamName, user_name: &UserName)
        -> Result<(), ClientError>;

    /// Update collaborator role in repository.
    async fn update_repository_collaborator_role(
        &self,
        repo_name: &RepositoryName,
        user_name: &UserName,
        role: &Role,
    ) -> Result<(), ClientError>;

    /// Update team role in repository.
    async fn update_repository_team_role(
        &self,
        repo_name: &RepositoryName,
        team_name: &TeamName,
        role: &Role,
    ) -> Result<(), ClientError>;

    /// Update repository visibility.
    async fn update_repository_visibility(
        &self,
        repo_name: &RepositoryName,
        visibility: &Visibility,
    ) -> Result<(), ClientError>;
}

/// Type alias to represent a Svc trait object.
pub(crate) type DynSvc = Arc<dyn Svc + Send + Sync>;

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
        let app_private_key =
            pem::parse(cfg.get_string("githubApp.privateKey").unwrap())?.contents().to_owned();
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
    /// [Svc::add_repository]
    async fn add_repository(&self, repo: &Repository) -> Result<(), ClientError> {
        // Create repository
        let visibility = match repo.visibility {
            Some(Visibility::Private) => Some(ReposCreateInOrgRequestVisibility::Private),
            Some(Visibility::Public) => Some(ReposCreateInOrgRequestVisibility::Public),
            None => None,
        };
        let body = ReposCreateInOrgRequest {
            allow_auto_merge: None,
            allow_merge_commit: None,
            allow_rebase_merge: None,
            allow_squash_merge: None,
            auto_init: None,
            delete_branch_on_merge: None,
            description: "".to_string(),
            gitignore_template: "".to_string(),
            has_issues: None,
            has_projects: None,
            has_wiki: None,
            homepage: "".to_string(),
            is_template: None,
            license_template: "".to_string(),
            name: repo.name.clone(),
            private: None,
            team_id: 0,
            visibility,
        };
        self.client.repos().create_in_org(&self.org, &body).await?;

        // Add repository teams
        if let Some(teams) = &repo.teams {
            for (team_name, role) in teams {
                self.add_repository_team(&repo.name, team_name, role).await?;
            }
        }

        // Add repository collaborators
        if let Some(collaborators) = &repo.collaborators {
            for (user_name, role) in collaborators {
                self.add_repository_collaborator(&repo.name, user_name, role).await?;
            }
        }

        Ok(())
    }

    /// [Svc::add_repository_collaborator]
    async fn add_repository_collaborator(
        &self,
        repo_name: &RepositoryName,
        user_name: &UserName,
        role: &Role,
    ) -> Result<(), ClientError> {
        let body = ReposAddCollaboratorRequest {
            permission: Some(role.into()),
            permissions: "".to_string(),
        };
        self.client.repos().add_collaborator(&self.org, repo_name, user_name, &body).await?;
        Ok(())
    }

    /// [Svc::add_repository_team]
    async fn add_repository_team(
        &self,
        repo_name: &RepositoryName,
        team_name: &TeamName,
        role: &Role,
    ) -> Result<(), ClientError> {
        let body = TeamsAddUpdateRepoPermissionsInOrgRequest {
            permission: Some(role.into()),
        };
        self.client
            .teams()
            .add_or_update_repo_permissions_in_org(&self.org, team_name, &self.org, repo_name, &body)
            .await
    }

    /// [Svc::add_team]
    async fn add_team(&self, team: &directory::Team) -> Result<(), ClientError> {
        // Create team
        let body = TeamsCreateRequest {
            name: team.name.clone(),
            description: "".to_string(),
            maintainers: team.maintainers.clone(),
            parent_team_id: 0,
            permission: None,
            privacy: Some(Privacy::Closed),
            repo_names: vec![],
        };
        self.client.teams().create(&self.org, &body).await?;

        // Add team members
        for user_name in &team.members {
            self.add_team_member(&team.name, user_name).await?;
        }

        Ok(())
    }

    /// [Svc::add_team_maintainer]
    async fn add_team_maintainer(
        &self,
        team_name: &TeamName,
        user_name: &UserName,
    ) -> Result<(), ClientError> {
        let body = TeamsAddUpdateMembershipUserInOrgRequest {
            role: Some(TeamMembershipRole::Maintainer),
        };
        self.client
            .teams()
            .add_or_update_membership_for_user_in_org(&self.org, team_name, user_name, &body)
            .await?;
        Ok(())
    }

    /// [Svc::add_team_member]
    async fn add_team_member(&self, team_name: &TeamName, user_name: &UserName) -> Result<(), ClientError> {
        let body = TeamsAddUpdateMembershipUserInOrgRequest {
            role: Some(TeamMembershipRole::Member),
        };
        self.client
            .teams()
            .add_or_update_membership_for_user_in_org(&self.org, team_name, user_name, &body)
            .await?;
        Ok(())
    }

    /// [Svc::list_repositories]
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

    /// [Svc::list_repository_collaborators]
    async fn list_repository_collaborators(
        &self,
        repo_name: &RepositoryName,
    ) -> Result<Vec<Collaborator>, ClientError> {
        self.client.repos().list_all_collaborators(&self.org, repo_name, Affiliation::All).await
    }

    /// [Svc::list_repository_teams]
    async fn list_repository_teams(&self, repo_name: &RepositoryName) -> Result<Vec<Team>, ClientError> {
        self.client.repos().list_all_teams(&self.org, repo_name).await
    }

    /// [Svc::list_team_maintainers]
    async fn list_team_maintainers(&self, team_name: &TeamName) -> Result<Vec<SimpleUser>, ClientError> {
        self.client
            .teams()
            .list_all_members_in_org(&self.org, team_name, TeamsListMembersInOrgRole::Maintainer)
            .await
    }

    /// [Svc::list_team_members]
    async fn list_team_members(&self, team_name: &TeamName) -> Result<Vec<SimpleUser>, ClientError> {
        self.client
            .teams()
            .list_all_members_in_org(&self.org, team_name, TeamsListMembersInOrgRole::Member)
            .await
    }

    /// [Svc::list_teams]
    async fn list_teams(&self) -> Result<Vec<Team>, ClientError> {
        self.client.teams().list_all(&self.org).await
    }

    /// [Svc::remove_repository]
    async fn remove_repository(&self, repo_name: &RepositoryName) -> Result<(), ClientError> {
        self.client.repos().delete(&self.org, repo_name).await
    }

    /// [Svc::remove_repository_collaborator]
    async fn remove_repository_collaborator(
        &self,
        repo_name: &RepositoryName,
        user_name: &UserName,
    ) -> Result<(), ClientError> {
        self.client.repos().remove_collaborator(&self.org, repo_name, user_name).await
    }

    /// [Svc::remove_repository_team]
    async fn remove_repository_team(
        &self,
        repo_name: &RepositoryName,
        team_name: &TeamName,
    ) -> Result<(), ClientError> {
        self.client.teams().remove_repo_in_org(&self.org, team_name, &self.org, repo_name).await
    }

    /// [Svc::remove_team]
    async fn remove_team(&self, team_name: &TeamName) -> Result<(), ClientError> {
        self.client.teams().delete_in_org(&self.org, team_name).await
    }

    /// [Svc::remove_team_maintainer]
    async fn remove_team_maintainer(
        &self,
        team_name: &TeamName,
        user_name: &UserName,
    ) -> Result<(), ClientError> {
        self.client
            .teams()
            .remove_membership_for_user_in_org(&self.org, team_name, user_name)
            .await
    }

    /// [Svc::remove_team_member]
    async fn remove_team_member(
        &self,
        team_name: &TeamName,
        user_name: &UserName,
    ) -> Result<(), ClientError> {
        self.client
            .teams()
            .remove_membership_for_user_in_org(&self.org, team_name, user_name)
            .await
    }

    /// [Svc::update_repository_collaborator_role]
    async fn update_repository_collaborator_role(
        &self,
        repo_name: &RepositoryName,
        user_name: &UserName,
        role: &Role,
    ) -> Result<(), ClientError> {
        let body = ReposAddCollaboratorRequest {
            permission: Some(role.into()),
            permissions: "".to_string(),
        };
        self.client.repos().add_collaborator(&self.org, repo_name, user_name, &body).await?;
        Ok(())
    }

    /// [Svc::update_repository_team_role]
    async fn update_repository_team_role(
        &self,
        repo_name: &RepositoryName,
        team_name: &TeamName,
        role: &Role,
    ) -> Result<(), ClientError> {
        let body = TeamsAddUpdateRepoPermissionsInOrgRequest {
            permission: Some(role.into()),
        };
        self.client
            .teams()
            .add_or_update_repo_permissions_in_org(&self.org, team_name, &self.org, repo_name, &body)
            .await
    }

    /// [Svc::update_repository_visibility]
    async fn update_repository_visibility(
        &self,
        repo_name: &RepositoryName,
        visibility: &Visibility,
    ) -> Result<(), ClientError> {
        let visibility = match visibility {
            Visibility::Private => Some(ReposCreateInOrgRequestVisibility::Private),
            Visibility::Public => Some(ReposCreateInOrgRequestVisibility::Public),
        };
        let body = ReposUpdateRequest {
            allow_auto_merge: None,
            allow_merge_commit: None,
            allow_rebase_merge: None,
            allow_squash_merge: None,
            archived: None,
            default_branch: "".to_string(),
            delete_branch_on_merge: None,
            description: "".to_string(),
            has_issues: None,
            has_projects: None,
            has_wiki: None,
            homepage: "".to_string(),
            is_template: None,
            name: repo_name.clone(),
            private: None,
            security_and_analysis: None,
            visibility,
        };
        self.client.repos().update(&self.org, repo_name, &body).await?;
        Ok(())
    }
}
