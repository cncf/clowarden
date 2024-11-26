//! This module defines an abstraction layer over the service's (GitHub) API.

use std::sync::Arc;

use anyhow::{format_err, Context, Result};
use async_trait::async_trait;
use cached::proc_macro::cached;
#[cfg(test)]
use mockall::automock;
use octorust::{
    auth::{Credentials, InstallationTokenGenerator, JWTCredentials},
    types::{
        Affiliation, Collaborator, MinimalRepository, Order, OrganizationInvitation, OrgsListMembersFilter,
        OrgsListMembersRole, Privacy, ReposAddCollaboratorRequest, ReposCreateInOrgRequest,
        ReposCreateInOrgRequestVisibility, ReposListOrgSort, ReposListOrgType, ReposUpdateInvitationRequest,
        ReposUpdateRequest, RepositoryInvitation, SimpleUser, Team, TeamMembership, TeamMembershipRole,
        TeamsAddUpdateMembershipUserInOrgRequest, TeamsAddUpdateRepoPermissionsInOrgRequest,
        TeamsCreateRequest, TeamsListMembersInOrgRole,
    },
    Client,
};
use tokio::time::{sleep, Duration};

use crate::{
    cfg::{GitHubApp, Organization},
    directory::{self, TeamName, UserName},
};

use super::state::{Repository, RepositoryName, Role, Visibility};

/// Trait that defines some operations a Svc implementation must support.
#[async_trait]
#[cfg_attr(test, automock)]
pub trait Svc {
    /// Add repository to organization.
    async fn add_repository(&self, ctx: &Ctx, repo: &Repository) -> Result<()>;

    /// Add collaborator to repository.
    async fn add_repository_collaborator(
        &self,
        ctx: &Ctx,
        repo_name: &RepositoryName,
        user_name: &UserName,
        role: &Role,
    ) -> Result<()>;

    /// Add team to repository.
    async fn add_repository_team(
        &self,
        ctx: &Ctx,
        repo_name: &RepositoryName,
        team_name: &TeamName,
        role: &Role,
    ) -> Result<()>;

    /// Add team to organization.
    async fn add_team(&self, ctx: &Ctx, team: &directory::Team) -> Result<()>;

    /// Add maintainer to the team.
    async fn add_team_maintainer(&self, ctx: &Ctx, team_name: &TeamName, user_name: &UserName) -> Result<()>;

    /// Add member to the team.
    async fn add_team_member(&self, ctx: &Ctx, team_name: &TeamName, user_name: &UserName) -> Result<()>;

    /// Get user's membership in team provided.
    async fn get_team_membership(
        &self,
        ctx: &Ctx,
        team_name: &TeamName,
        user_name: &UserName,
    ) -> Result<TeamMembership>;

    /// Get user login.
    async fn get_user_login(&self, ctx: &Ctx, user_name: &UserName) -> Result<UserName>;

    /// List organization admins.
    async fn list_org_admins(&self, ctx: &Ctx) -> Result<Vec<SimpleUser>>;

    /// List organization members.
    async fn list_org_members(&self, ctx: &Ctx) -> Result<Vec<SimpleUser>>;

    /// List repositories in the organization.
    async fn list_repositories(&self, ctx: &Ctx) -> Result<Vec<MinimalRepository>>;

    /// List repository's collaborators.
    async fn list_repository_collaborators(
        &self,
        ctx: &Ctx,
        repo_name: &RepositoryName,
    ) -> Result<Vec<Collaborator>>;

    /// List repository's invitations.
    async fn list_repository_invitations(
        &self,
        ctx: &Ctx,
        repo_name: &RepositoryName,
    ) -> Result<Vec<RepositoryInvitation>>;

    /// List repository's teams.
    async fn list_repository_teams(&self, ctx: &Ctx, repo_name: &RepositoryName) -> Result<Vec<Team>>;

    /// List team's invitations.
    async fn list_team_invitations(
        &self,
        ctx: &Ctx,
        team_name: &TeamName,
    ) -> Result<Vec<OrganizationInvitation>>;

    /// List team's maintainers.
    async fn list_team_maintainers(&self, ctx: &Ctx, team_name: &TeamName) -> Result<Vec<SimpleUser>>;

    /// List team's members.
    async fn list_team_members(&self, ctx: &Ctx, team_name: &TeamName) -> Result<Vec<SimpleUser>>;

    /// List teams in the organization.
    async fn list_teams(&self, ctx: &Ctx) -> Result<Vec<Team>>;

    /// Remove collaborator from repository.
    async fn remove_repository_collaborator(
        &self,
        ctx: &Ctx,
        repo_name: &RepositoryName,
        user_name: &UserName,
    ) -> Result<()>;

    /// Remove repository invitation.
    async fn remove_repository_invitation(
        &self,
        ctx: &Ctx,
        repo_name: &RepositoryName,
        invitation_id: i64,
    ) -> Result<()>;

    /// Remove team from repository.
    async fn remove_repository_team(
        &self,
        ctx: &Ctx,
        repo_name: &RepositoryName,
        team_name: &TeamName,
    ) -> Result<()>;

    /// Remove team from organization.
    async fn remove_team(&self, ctx: &Ctx, team_name: &TeamName) -> Result<()>;

    /// Remove maintainer from the team.
    async fn remove_team_maintainer(
        &self,
        ctx: &Ctx,
        team_name: &TeamName,
        user_name: &UserName,
    ) -> Result<()>;

    /// Remove member from the team.
    async fn remove_team_member(&self, ctx: &Ctx, team_name: &TeamName, user_name: &UserName) -> Result<()>;

    /// Update collaborator role in repository.
    async fn update_repository_collaborator_role(
        &self,
        ctx: &Ctx,
        repo_name: &RepositoryName,
        user_name: &UserName,
        role: &Role,
    ) -> Result<()>;

    /// Update repository invitation.
    async fn update_repository_invitation(
        &self,
        ctx: &Ctx,
        repo_name: &RepositoryName,
        invitation_id: i64,
        role: &Role,
    ) -> Result<()>;

    /// Update team role in repository.
    async fn update_repository_team_role(
        &self,
        ctx: &Ctx,
        repo_name: &RepositoryName,
        team_name: &TeamName,
        role: &Role,
    ) -> Result<()>;

    /// Update repository visibility.
    async fn update_repository_visibility(
        &self,
        ctx: &Ctx,
        repo_name: &RepositoryName,
        visibility: &Visibility,
    ) -> Result<()>;
}

/// Type alias to represent a Svc trait object.
pub type DynSvc = Arc<dyn Svc + Send + Sync>;

/// Svc implementation backed by the GitHub API.
#[derive(Default)]
pub struct SvcApi {
    app_credentials: Option<JWTCredentials>,
    token: Option<String>,
}

impl SvcApi {
    /// Create a new SvcApi instance using the token provided.
    #[must_use]
    pub fn new_with_token(token: String) -> Self {
        Self {
            token: Some(token),
            ..Default::default()
        }
    }

    /// Create a new SvcApi instance using the app credentials provided in the
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
impl Svc for SvcApi {
    /// [Svc::add_repository]
    async fn add_repository(&self, ctx: &Ctx, repo: &Repository) -> Result<()> {
        let client = self.setup_client(ctx.inst_id)?;

        // Create repository
        let visibility = match repo.visibility {
            Some(Visibility::Internal) => Some(ReposCreateInOrgRequestVisibility::Internal),
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
            description: String::new(),
            gitignore_template: String::new(),
            has_issues: None,
            has_projects: None,
            has_wiki: None,
            homepage: String::new(),
            is_template: None,
            license_template: String::new(),
            name: repo.name.clone(),
            private: None,
            team_id: 0,
            visibility,
        };
        client.repos().create_in_org(&ctx.org, &body).await?;
        sleep(Duration::from_secs(1)).await;

        // Add repository teams
        if let Some(teams) = &repo.teams {
            for (team_name, role) in teams {
                self.add_repository_team(ctx, &repo.name, team_name, role).await?;
            }
        }

        // Add repository collaborators
        if let Some(collaborators) = &repo.collaborators {
            for (user_name, role) in collaborators {
                self.add_repository_collaborator(ctx, &repo.name, user_name, role).await?;
            }
        }

        Ok(())
    }

    /// [Svc::add_repository_collaborator]
    async fn add_repository_collaborator(
        &self,
        ctx: &Ctx,
        repo_name: &RepositoryName,
        user_name: &UserName,
        role: &Role,
    ) -> Result<()> {
        let client = self.setup_client(ctx.inst_id)?;
        let body = ReposAddCollaboratorRequest {
            permission: Some(role.into()),
            permissions: String::new(),
        };
        client.repos().add_collaborator(&ctx.org, repo_name, user_name, &body).await?;
        Ok(())
    }

    /// [Svc::add_repository_team]
    async fn add_repository_team(
        &self,
        ctx: &Ctx,
        repo_name: &RepositoryName,
        team_name: &TeamName,
        role: &Role,
    ) -> Result<()> {
        let client = self.setup_client(ctx.inst_id)?;
        let body = TeamsAddUpdateRepoPermissionsInOrgRequest {
            permission: Some(role.into()),
        };
        client
            .teams()
            .add_or_update_repo_permissions_in_org(&ctx.org, team_name, &ctx.org, repo_name, &body)
            .await?;
        Ok(())
    }

    /// [Svc::add_team]
    async fn add_team(&self, ctx: &Ctx, team: &directory::Team) -> Result<()> {
        // Create team
        let client = self.setup_client(ctx.inst_id)?;
        let body = TeamsCreateRequest {
            name: team.name.clone(),
            description: String::new(),
            maintainers: team.maintainers.clone(),
            parent_team_id: 0,
            permission: None,
            privacy: Some(Privacy::Closed),
            repo_names: vec![],
        };
        client.teams().create(&ctx.org, &body).await?;
        sleep(Duration::from_secs(1)).await;

        // Add team members
        for user_name in &team.members {
            self.add_team_member(ctx, &team.name, user_name).await?;
        }

        Ok(())
    }

    /// [Svc::add_team_maintainer]
    async fn add_team_maintainer(&self, ctx: &Ctx, team_name: &TeamName, user_name: &UserName) -> Result<()> {
        let client = self.setup_client(ctx.inst_id)?;
        let body = TeamsAddUpdateMembershipUserInOrgRequest {
            role: Some(TeamMembershipRole::Maintainer),
        };
        client
            .teams()
            .add_or_update_membership_for_user_in_org(&ctx.org, team_name, user_name, &body)
            .await?;
        Ok(())
    }

    /// [Svc::add_team_member]
    async fn add_team_member(&self, ctx: &Ctx, team_name: &TeamName, user_name: &UserName) -> Result<()> {
        let client = self.setup_client(ctx.inst_id)?;
        let body = TeamsAddUpdateMembershipUserInOrgRequest {
            role: Some(TeamMembershipRole::Member),
        };
        client
            .teams()
            .add_or_update_membership_for_user_in_org(&ctx.org, team_name, user_name, &body)
            .await?;
        Ok(())
    }

    /// [Svc::get_team_membership]
    async fn get_team_membership(
        &self,
        ctx: &Ctx,
        team_name: &TeamName,
        user_name: &UserName,
    ) -> Result<TeamMembership> {
        let client = self.setup_client(ctx.inst_id)?;
        Ok(client.teams().get_membership_for_user_in_org(&ctx.org, team_name, user_name).await?)
    }

    /// [Svc::get_user_login]
    async fn get_user_login(&self, ctx: &Ctx, user_name: &UserName) -> Result<UserName> {
        let client = self.setup_client(ctx.inst_id)?;
        Ok(client.users().get_by_username_public_user(user_name).await?.login)
    }

    /// [Svc::list_org_admins]
    async fn list_org_admins(&self, ctx: &Ctx) -> Result<Vec<SimpleUser>> {
        #[cached(
            time = 60,
            sync_writes = true,
            result = true,
            key = "String",
            convert = r#"{ format!("{}", org) }"#
        )]
        async fn inner(client: &Client, org: &str) -> Result<Vec<SimpleUser>> {
            let members = client
                .orgs()
                .list_all_members(org, OrgsListMembersFilter::All, OrgsListMembersRole::Admin)
                .await?;
            Ok(members)
        }
        let client = self.setup_client(ctx.inst_id)?;
        inner(&client, &ctx.org).await
    }

    /// [Svc::list_org_members]
    async fn list_org_members(&self, ctx: &Ctx) -> Result<Vec<SimpleUser>> {
        #[cached(
            time = 60,
            sync_writes = true,
            result = true,
            key = "String",
            convert = r#"{ format!("{}", org) }"#
        )]
        async fn inner(client: &Client, org: &str) -> Result<Vec<SimpleUser>> {
            let members = client
                .orgs()
                .list_all_members(org, OrgsListMembersFilter::All, OrgsListMembersRole::All)
                .await?;
            Ok(members)
        }
        let client = self.setup_client(ctx.inst_id)?;
        inner(&client, &ctx.org).await
    }

    /// [Svc::list_repositories]
    async fn list_repositories(&self, ctx: &Ctx) -> Result<Vec<MinimalRepository>> {
        let client = self.setup_client(ctx.inst_id)?;
        let repos = client
            .repos()
            .list_all_for_org(
                &ctx.org,
                ReposListOrgType::All,
                ReposListOrgSort::FullName,
                Order::Asc,
            )
            .await?;
        Ok(repos)
    }

    /// [Svc::list_repository_collaborators]
    async fn list_repository_collaborators(
        &self,
        ctx: &Ctx,
        repo_name: &RepositoryName,
    ) -> Result<Vec<Collaborator>> {
        let client = self.setup_client(ctx.inst_id)?;
        let collaborators =
            client.repos().list_all_collaborators(&ctx.org, repo_name, Affiliation::Direct).await?;
        Ok(collaborators)
    }

    /// [Svc::list_repository_invitations]
    async fn list_repository_invitations(
        &self,
        ctx: &Ctx,
        repo_name: &RepositoryName,
    ) -> Result<Vec<RepositoryInvitation>> {
        #[cached(
            time = 60,
            sync_writes = true,
            result = true,
            key = "String",
            convert = r#"{ format!("{}", repo_name) }"#
        )]
        async fn inner(client: &Client, org: &str, repo_name: &str) -> Result<Vec<RepositoryInvitation>> {
            let invitations = client.repos().list_all_invitations(org, repo_name).await?;
            Ok(invitations)
        }
        let client = self.setup_client(ctx.inst_id)?;
        inner(&client, &ctx.org, repo_name).await
    }

    /// [Svc::list_repository_teams]
    async fn list_repository_teams(&self, ctx: &Ctx, repo_name: &RepositoryName) -> Result<Vec<Team>> {
        let client = self.setup_client(ctx.inst_id)?;
        let teams = client.repos().list_all_teams(&ctx.org, repo_name).await?;
        Ok(teams)
    }

    /// [Svc::list_team_invitations]
    async fn list_team_invitations(
        &self,
        ctx: &Ctx,
        team_name: &TeamName,
    ) -> Result<Vec<OrganizationInvitation>> {
        let client = self.setup_client(ctx.inst_id)?;
        let invitations = client.teams().list_all_pending_invitations_in_org(&ctx.org, team_name).await?;
        Ok(invitations)
    }

    /// [Svc::list_team_maintainers]
    async fn list_team_maintainers(&self, ctx: &Ctx, team_name: &TeamName) -> Result<Vec<SimpleUser>> {
        let client = self.setup_client(ctx.inst_id)?;
        let maintainers = client
            .teams()
            .list_all_members_in_org(&ctx.org, team_name, TeamsListMembersInOrgRole::Maintainer)
            .await?;
        Ok(maintainers)
    }

    /// [Svc::list_team_members]
    async fn list_team_members(&self, ctx: &Ctx, team_name: &TeamName) -> Result<Vec<SimpleUser>> {
        let client = self.setup_client(ctx.inst_id)?;
        let members = client
            .teams()
            .list_all_members_in_org(&ctx.org, team_name, TeamsListMembersInOrgRole::Member)
            .await?;
        Ok(members)
    }

    /// [Svc::list_teams]
    async fn list_teams(&self, ctx: &Ctx) -> Result<Vec<Team>> {
        let client = self.setup_client(ctx.inst_id)?;
        let teams = client.teams().list_all(&ctx.org).await?;
        Ok(teams)
    }

    /// [Svc::remove_repository_collaborator]
    async fn remove_repository_collaborator(
        &self,
        ctx: &Ctx,
        repo_name: &RepositoryName,
        user_name: &UserName,
    ) -> Result<()> {
        let client = self.setup_client(ctx.inst_id)?;
        client.repos().remove_collaborator(&ctx.org, repo_name, user_name).await?;
        Ok(())
    }

    /// [Svc::remove_repository_invitation]
    async fn remove_repository_invitation(
        &self,
        ctx: &Ctx,
        repo_name: &RepositoryName,
        invitation_id: i64,
    ) -> Result<()> {
        let client = self.setup_client(ctx.inst_id)?;
        client.repos().delete_invitation(&ctx.org, repo_name, invitation_id).await?;
        Ok(())
    }

    /// [Svc::remove_repository_team]
    async fn remove_repository_team(
        &self,
        ctx: &Ctx,
        repo_name: &RepositoryName,
        team_name: &TeamName,
    ) -> Result<()> {
        let client = self.setup_client(ctx.inst_id)?;
        client.teams().remove_repo_in_org(&ctx.org, team_name, &ctx.org, repo_name).await?;
        Ok(())
    }

    /// [Svc::remove_team]
    async fn remove_team(&self, ctx: &Ctx, team_name: &TeamName) -> Result<()> {
        let client = self.setup_client(ctx.inst_id)?;
        client.teams().delete_in_org(&ctx.org, team_name).await?;
        Ok(())
    }

    /// [Svc::remove_team_maintainer]
    async fn remove_team_maintainer(
        &self,
        ctx: &Ctx,
        team_name: &TeamName,
        user_name: &UserName,
    ) -> Result<()> {
        let client = self.setup_client(ctx.inst_id)?;
        client.teams().remove_membership_for_user_in_org(&ctx.org, team_name, user_name).await?;
        Ok(())
    }

    /// [Svc::remove_team_member]
    async fn remove_team_member(&self, ctx: &Ctx, team_name: &TeamName, user_name: &UserName) -> Result<()> {
        let client = self.setup_client(ctx.inst_id)?;
        client.teams().remove_membership_for_user_in_org(&ctx.org, team_name, user_name).await?;
        Ok(())
    }

    /// [Svc::update_repository_collaborator_role]
    async fn update_repository_collaborator_role(
        &self,
        ctx: &Ctx,
        repo_name: &RepositoryName,
        user_name: &UserName,
        role: &Role,
    ) -> Result<()> {
        let client = self.setup_client(ctx.inst_id)?;
        let body = ReposAddCollaboratorRequest {
            permission: Some(role.into()),
            permissions: String::new(),
        };
        client.repos().add_collaborator(&ctx.org, repo_name, user_name, &body).await?;
        Ok(())
    }

    /// [Svc::update_repository_invitation]
    async fn update_repository_invitation(
        &self,
        ctx: &Ctx,
        repo_name: &RepositoryName,
        invitation_id: i64,
        role: &Role,
    ) -> Result<()> {
        let client = self.setup_client(ctx.inst_id)?;
        let body = ReposUpdateInvitationRequest {
            permissions: Some(role.into()),
        };
        client.repos().update_invitation(&ctx.org, repo_name, invitation_id, &body).await?;
        Ok(())
    }

    /// [Svc::update_repository_team_role]
    async fn update_repository_team_role(
        &self,
        ctx: &Ctx,
        repo_name: &RepositoryName,
        team_name: &TeamName,
        role: &Role,
    ) -> Result<()> {
        let client = self.setup_client(ctx.inst_id)?;
        let body = TeamsAddUpdateRepoPermissionsInOrgRequest {
            permission: Some(role.into()),
        };
        client
            .teams()
            .add_or_update_repo_permissions_in_org(&ctx.org, team_name, &ctx.org, repo_name, &body)
            .await?;
        Ok(())
    }

    /// [Svc::update_repository_visibility]
    async fn update_repository_visibility(
        &self,
        ctx: &Ctx,
        repo_name: &RepositoryName,
        visibility: &Visibility,
    ) -> Result<()> {
        let client = self.setup_client(ctx.inst_id)?;
        let visibility = match visibility {
            Visibility::Internal => Some(ReposCreateInOrgRequestVisibility::Internal),
            Visibility::Private => Some(ReposCreateInOrgRequestVisibility::Private),
            Visibility::Public => Some(ReposCreateInOrgRequestVisibility::Public),
        };
        let body = ReposUpdateRequest {
            allow_auto_merge: None,
            allow_merge_commit: None,
            allow_rebase_merge: None,
            allow_squash_merge: None,
            archived: None,
            default_branch: String::new(),
            delete_branch_on_merge: None,
            description: String::new(),
            has_issues: None,
            has_projects: None,
            has_wiki: None,
            homepage: String::new(),
            is_template: None,
            name: repo_name.clone(),
            private: None,
            security_and_analysis: None,
            visibility,
        };
        client.repos().update(&ctx.org, repo_name, &body).await?;
        Ok(())
    }
}

/// Information about the target of a GitHub API request.
pub struct Ctx {
    pub inst_id: Option<i64>,
    pub org: String,
}

impl From<&Organization> for Ctx {
    fn from(org: &Organization) -> Self {
        Ctx {
            inst_id: Some(org.installation_id),
            org: org.name.clone(),
        }
    }
}
