//! This module defines an abstraction layer over the service's (GitHub) API.

use std::{sync::Arc, time::Duration};

use anyhow::{Error, Result, format_err};
use async_trait::async_trait;
#[cfg(test)]
use mockall::automock;
use octorust::types::{
    Affiliation, Collaborator, MinimalRepository, Order, OrganizationInvitation, OrgsListMembersFilter,
    OrgsListMembersRole, Privacy, ReposAddCollaboratorRequest, ReposCreateInOrgRequest,
    ReposCreateInOrgRequestVisibility, ReposListOrgSort, ReposListOrgType, ReposUpdateInvitationRequest,
    ReposUpdateRequest, RepositoryInvitation, SimpleUser, Team, TeamMembership, TeamMembershipRole,
    TeamsAddUpdateMembershipUserInOrgRequest, TeamsAddUpdateRepoPermissionsInOrgRequest, TeamsCreateRequest,
    TeamsListMembersInOrgRole,
};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::{
    cfg::{GitHubApp, Organization},
    directory::{self, TeamName, UserName},
    github::{CancelledError, ClientPool, RequestKind},
};

use super::state::{Repository, RepositoryName, Role, Visibility};

/// Delay after creating a repository or team before attaching teams,
/// collaborators, or members, giving GitHub's eventual consistency time to
/// make the newly created resource visible.
const POST_CREATE_PROPAGATION_DELAY: Duration = Duration::from_secs(1);

/// Trait that defines some operations a Svc implementation must support.
#[async_trait]
#[cfg_attr(test, automock)]
pub trait Svc {
    /// Add repository to organization.
    async fn add_repository(&self, ctx: &Ctx, repo: &Repository) -> Result<()>;

    /// Add collaborator to repository, returning the created invitation when
    /// GitHub creates one (HTTP `201`) or `None` when access is granted
    /// directly without an invitation (HTTP `204`).
    async fn add_repository_collaborator(
        &self,
        ctx: &Ctx,
        repo_name: &RepositoryName,
        user_name: &UserName,
        role: &Role,
    ) -> Result<Option<RepositoryInvitation>>;

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
pub struct SvcApi {
    /// Shared GitHub API client pool and rate-limit governor.
    pool: Arc<ClientPool>,
}

impl SvcApi {
    /// Create a new SvcApi instance backed by the shared [`ClientPool`].
    #[must_use]
    pub fn new(pool: Arc<ClientPool>) -> Self {
        Self { pool }
    }

    /// Create a new SvcApi instance using the app credentials provided in the
    /// configuration.
    pub fn new_with_app_creds(gh_app: &GitHubApp) -> Result<Self> {
        Ok(Self {
            pool: Arc::new(ClientPool::new_with_app_creds(gh_app)?),
        })
    }

    /// Create a new SvcApi instance using the token provided.
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
impl Svc for SvcApi {
    /// [`Svc::add_repository`].
    async fn add_repository(&self, ctx: &Ctx, repo: &Repository) -> Result<()> {
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
        self.pool
            .execute(
                ctx.inst_id,
                RequestKind::NonIdempotentWrite,
                &ctx.cancel_token,
                |client| {
                    let body = body.clone();
                    async move { client.repos().create_in_org(&ctx.org, &body).await }
                },
            )
            .await?;

        // Wait briefly for GitHub eventual consistency before follow-up grants
        let has_teams = repo.teams.as_ref().is_some_and(|teams| !teams.is_empty());
        let has_collaborators =
            repo.collaborators.as_ref().is_some_and(|collaborators| !collaborators.is_empty());
        if has_teams || has_collaborators {
            sleep_post_create(&ctx.cancel_token).await?;
        }

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

    /// [`Svc::add_repository_collaborator`].
    async fn add_repository_collaborator(
        &self,
        ctx: &Ctx,
        repo_name: &RepositoryName,
        user_name: &UserName,
        role: &Role,
    ) -> Result<Option<RepositoryInvitation>> {
        let body = ReposAddCollaboratorRequest {
            permission: Some(role.into()),
            permissions: String::new(),
        };
        let invitation = self
            .pool
            .execute(
                ctx.inst_id,
                RequestKind::IdempotentWrite,
                &ctx.cancel_token,
                |client| {
                    let body = body.clone();
                    async move {
                        client.repos().add_collaborator(&ctx.org, repo_name, user_name, &body).await
                    }
                },
            )
            .await?;

        // Reject an already-expired invitation so callers never record stale
        // access as a newly applied collaborator invitation
        if invitation.as_ref().is_some_and(|inv| inv.expired) {
            return Err(format_err!(
                "github returned an expired invitation when adding collaborator {user_name} to {repo_name}"
            ));
        }

        Ok(invitation)
    }

    /// [`Svc::add_repository_team`].
    async fn add_repository_team(
        &self,
        ctx: &Ctx,
        repo_name: &RepositoryName,
        team_name: &TeamName,
        role: &Role,
    ) -> Result<()> {
        let body = TeamsAddUpdateRepoPermissionsInOrgRequest {
            permission: Some(role.into()),
        };
        self.pool
            .execute(
                ctx.inst_id,
                RequestKind::IdempotentWrite,
                &ctx.cancel_token,
                |client| {
                    let body = body.clone();
                    async move {
                        client
                            .teams()
                            .add_or_update_repo_permissions_in_org(
                                &ctx.org, team_name, &ctx.org, repo_name, &body,
                            )
                            .await
                    }
                },
            )
            .await?;
        Ok(())
    }

    /// [`Svc::add_team`].
    async fn add_team(&self, ctx: &Ctx, team: &directory::Team) -> Result<()> {
        // Create team
        let body = TeamsCreateRequest {
            name: team.name.clone(),
            description: String::new(),
            maintainers: team.maintainers.clone(),
            parent_team_id: 0,
            permission: None,
            privacy: Some(Privacy::Closed),
            repo_names: vec![],
        };
        self.pool
            .execute(
                ctx.inst_id,
                RequestKind::NonIdempotentWrite,
                &ctx.cancel_token,
                |client| {
                    let body = body.clone();
                    async move { client.teams().create(&ctx.org, &body).await }
                },
            )
            .await?;

        // Wait briefly for GitHub eventual consistency before attaching members
        // or granting repository access to the new team
        sleep_post_create(&ctx.cancel_token).await?;

        // Add team members
        for user_name in &team.members {
            self.add_team_member(ctx, &team.name, user_name).await?;
        }

        Ok(())
    }

    /// [`Svc::add_team_maintainer`].
    async fn add_team_maintainer(&self, ctx: &Ctx, team_name: &TeamName, user_name: &UserName) -> Result<()> {
        let body = TeamsAddUpdateMembershipUserInOrgRequest {
            role: Some(TeamMembershipRole::Maintainer),
        };
        self.pool
            .execute(
                ctx.inst_id,
                RequestKind::IdempotentWrite,
                &ctx.cancel_token,
                |client| {
                    let body = body.clone();
                    async move {
                        client
                            .teams()
                            .add_or_update_membership_for_user_in_org(&ctx.org, team_name, user_name, &body)
                            .await
                    }
                },
            )
            .await?;
        Ok(())
    }

    /// [`Svc::add_team_member`].
    async fn add_team_member(&self, ctx: &Ctx, team_name: &TeamName, user_name: &UserName) -> Result<()> {
        let body = TeamsAddUpdateMembershipUserInOrgRequest {
            role: Some(TeamMembershipRole::Member),
        };
        self.pool
            .execute(
                ctx.inst_id,
                RequestKind::IdempotentWrite,
                &ctx.cancel_token,
                |client| {
                    let body = body.clone();
                    async move {
                        client
                            .teams()
                            .add_or_update_membership_for_user_in_org(&ctx.org, team_name, user_name, &body)
                            .await
                    }
                },
            )
            .await?;
        Ok(())
    }

    /// [`Svc::get_team_membership`].
    async fn get_team_membership(
        &self,
        ctx: &Ctx,
        team_name: &TeamName,
        user_name: &UserName,
    ) -> Result<TeamMembership> {
        self.pool
            .execute(
                ctx.inst_id,
                RequestKind::Read,
                &ctx.cancel_token,
                |client| async move {
                    client.teams().get_membership_for_user_in_org(&ctx.org, team_name, user_name).await
                },
            )
            .await
    }

    /// [`Svc::get_user_login`].
    async fn get_user_login(&self, ctx: &Ctx, user_name: &UserName) -> Result<UserName> {
        let user = self
            .pool
            .execute(
                ctx.inst_id,
                RequestKind::Read,
                &ctx.cancel_token,
                |client| async move { client.users().get_by_username_public_user(user_name).await },
            )
            .await?;
        Ok(user.login)
    }

    /// [`Svc::list_org_admins`].
    async fn list_org_admins(&self, ctx: &Ctx) -> Result<Vec<SimpleUser>> {
        self.pool
            .execute(
                ctx.inst_id,
                RequestKind::Read,
                &ctx.cancel_token,
                |client| async move {
                    client
                        .orgs()
                        .list_all_members(&ctx.org, OrgsListMembersFilter::All, OrgsListMembersRole::Admin)
                        .await
                },
            )
            .await
    }

    /// [`Svc::list_org_members`].
    async fn list_org_members(&self, ctx: &Ctx) -> Result<Vec<SimpleUser>> {
        self.pool
            .execute(
                ctx.inst_id,
                RequestKind::Read,
                &ctx.cancel_token,
                |client| async move {
                    client
                        .orgs()
                        .list_all_members(&ctx.org, OrgsListMembersFilter::All, OrgsListMembersRole::All)
                        .await
                },
            )
            .await
    }

    /// [`Svc::list_repositories`].
    async fn list_repositories(&self, ctx: &Ctx) -> Result<Vec<MinimalRepository>> {
        self.pool
            .execute(
                ctx.inst_id,
                RequestKind::Read,
                &ctx.cancel_token,
                |client| async move {
                    client
                        .repos()
                        .list_all_for_org(
                            &ctx.org,
                            ReposListOrgType::All,
                            ReposListOrgSort::FullName,
                            Order::Asc,
                        )
                        .await
                },
            )
            .await
    }

    /// [`Svc::list_repository_collaborators`].
    async fn list_repository_collaborators(
        &self,
        ctx: &Ctx,
        repo_name: &RepositoryName,
    ) -> Result<Vec<Collaborator>> {
        self.pool
            .execute(
                ctx.inst_id,
                RequestKind::Read,
                &ctx.cancel_token,
                |client| async move {
                    client.repos().list_all_collaborators(&ctx.org, repo_name, Affiliation::Direct).await
                },
            )
            .await
    }

    /// [`Svc::list_repository_invitations`].
    async fn list_repository_invitations(
        &self,
        ctx: &Ctx,
        repo_name: &RepositoryName,
    ) -> Result<Vec<RepositoryInvitation>> {
        self.pool
            .execute(
                ctx.inst_id,
                RequestKind::Read,
                &ctx.cancel_token,
                |client| async move { client.repos().list_all_invitations(&ctx.org, repo_name).await },
            )
            .await
    }

    /// [`Svc::list_repository_teams`].
    async fn list_repository_teams(&self, ctx: &Ctx, repo_name: &RepositoryName) -> Result<Vec<Team>> {
        self.pool
            .execute(
                ctx.inst_id,
                RequestKind::Read,
                &ctx.cancel_token,
                |client| async move { client.repos().list_all_teams(&ctx.org, repo_name).await },
            )
            .await
    }

    /// [`Svc::list_team_invitations`].
    async fn list_team_invitations(
        &self,
        ctx: &Ctx,
        team_name: &TeamName,
    ) -> Result<Vec<OrganizationInvitation>> {
        self.pool
            .execute(
                ctx.inst_id,
                RequestKind::Read,
                &ctx.cancel_token,
                |client| async move {
                    client.teams().list_all_pending_invitations_in_org(&ctx.org, team_name).await
                },
            )
            .await
    }

    /// [`Svc::list_team_maintainers`].
    async fn list_team_maintainers(&self, ctx: &Ctx, team_name: &TeamName) -> Result<Vec<SimpleUser>> {
        self.pool
            .execute(
                ctx.inst_id,
                RequestKind::Read,
                &ctx.cancel_token,
                |client| async move {
                    client
                        .teams()
                        .list_all_members_in_org(&ctx.org, team_name, TeamsListMembersInOrgRole::Maintainer)
                        .await
                },
            )
            .await
    }

    /// [`Svc::list_team_members`].
    async fn list_team_members(&self, ctx: &Ctx, team_name: &TeamName) -> Result<Vec<SimpleUser>> {
        self.pool
            .execute(
                ctx.inst_id,
                RequestKind::Read,
                &ctx.cancel_token,
                |client| async move {
                    client
                        .teams()
                        .list_all_members_in_org(&ctx.org, team_name, TeamsListMembersInOrgRole::Member)
                        .await
                },
            )
            .await
    }

    /// [`Svc::list_teams`].
    async fn list_teams(&self, ctx: &Ctx) -> Result<Vec<Team>> {
        self.pool
            .execute(
                ctx.inst_id,
                RequestKind::Read,
                &ctx.cancel_token,
                |client| async move { client.teams().list_all(&ctx.org).await },
            )
            .await
    }

    /// [`Svc::remove_repository_collaborator`].
    async fn remove_repository_collaborator(
        &self,
        ctx: &Ctx,
        repo_name: &RepositoryName,
        user_name: &UserName,
    ) -> Result<()> {
        self.pool
            .execute(
                ctx.inst_id,
                RequestKind::IdempotentWrite,
                &ctx.cancel_token,
                |client| async move { client.repos().remove_collaborator(&ctx.org, repo_name, user_name).await },
            )
            .await?;
        Ok(())
    }

    /// [`Svc::remove_repository_invitation`].
    async fn remove_repository_invitation(
        &self,
        ctx: &Ctx,
        repo_name: &RepositoryName,
        invitation_id: i64,
    ) -> Result<()> {
        self.pool
            .execute(
                ctx.inst_id,
                RequestKind::IdempotentWrite,
                &ctx.cancel_token,
                |client| async move {
                    client.repos().delete_invitation(&ctx.org, repo_name, invitation_id).await
                },
            )
            .await?;
        Ok(())
    }

    /// [`Svc::remove_repository_team`].
    async fn remove_repository_team(
        &self,
        ctx: &Ctx,
        repo_name: &RepositoryName,
        team_name: &TeamName,
    ) -> Result<()> {
        self.pool
            .execute(
                ctx.inst_id,
                RequestKind::IdempotentWrite,
                &ctx.cancel_token,
                |client| async move {
                    client.teams().remove_repo_in_org(&ctx.org, team_name, &ctx.org, repo_name).await
                },
            )
            .await?;
        Ok(())
    }

    /// [`Svc::remove_team`].
    async fn remove_team(&self, ctx: &Ctx, team_name: &TeamName) -> Result<()> {
        self.pool
            .execute(
                ctx.inst_id,
                RequestKind::IdempotentWrite,
                &ctx.cancel_token,
                |client| async move { client.teams().delete_in_org(&ctx.org, team_name).await },
            )
            .await?;
        Ok(())
    }

    /// [`Svc::remove_team_maintainer`].
    async fn remove_team_maintainer(
        &self,
        ctx: &Ctx,
        team_name: &TeamName,
        user_name: &UserName,
    ) -> Result<()> {
        self.pool
            .execute(
                ctx.inst_id,
                RequestKind::IdempotentWrite,
                &ctx.cancel_token,
                |client| async move {
                    client.teams().remove_membership_for_user_in_org(&ctx.org, team_name, user_name).await
                },
            )
            .await?;
        Ok(())
    }

    /// [`Svc::remove_team_member`].
    async fn remove_team_member(&self, ctx: &Ctx, team_name: &TeamName, user_name: &UserName) -> Result<()> {
        self.pool
            .execute(
                ctx.inst_id,
                RequestKind::IdempotentWrite,
                &ctx.cancel_token,
                |client| async move {
                    client.teams().remove_membership_for_user_in_org(&ctx.org, team_name, user_name).await
                },
            )
            .await?;
        Ok(())
    }

    /// [`Svc::update_repository_collaborator_role`].
    async fn update_repository_collaborator_role(
        &self,
        ctx: &Ctx,
        repo_name: &RepositoryName,
        user_name: &UserName,
        role: &Role,
    ) -> Result<()> {
        let body = ReposAddCollaboratorRequest {
            permission: Some(role.into()),
            permissions: String::new(),
        };
        self.pool
            .execute(
                ctx.inst_id,
                RequestKind::IdempotentWrite,
                &ctx.cancel_token,
                |client| {
                    let body = body.clone();
                    async move {
                        client.repos().add_collaborator(&ctx.org, repo_name, user_name, &body).await
                    }
                },
            )
            .await?;
        Ok(())
    }

    /// [`Svc::update_repository_invitation`].
    async fn update_repository_invitation(
        &self,
        ctx: &Ctx,
        repo_name: &RepositoryName,
        invitation_id: i64,
        role: &Role,
    ) -> Result<()> {
        let body = ReposUpdateInvitationRequest {
            permissions: Some(role.into()),
        };
        self.pool
            .execute(
                ctx.inst_id,
                RequestKind::IdempotentWrite,
                &ctx.cancel_token,
                |client| {
                    let body = body.clone();
                    async move {
                        client.repos().update_invitation(&ctx.org, repo_name, invitation_id, &body).await
                    }
                },
            )
            .await?;
        Ok(())
    }

    /// [`Svc::update_repository_team_role`].
    async fn update_repository_team_role(
        &self,
        ctx: &Ctx,
        repo_name: &RepositoryName,
        team_name: &TeamName,
        role: &Role,
    ) -> Result<()> {
        let body = TeamsAddUpdateRepoPermissionsInOrgRequest {
            permission: Some(role.into()),
        };
        self.pool
            .execute(
                ctx.inst_id,
                RequestKind::IdempotentWrite,
                &ctx.cancel_token,
                |client| {
                    let body = body.clone();
                    async move {
                        client
                            .teams()
                            .add_or_update_repo_permissions_in_org(
                                &ctx.org, team_name, &ctx.org, repo_name, &body,
                            )
                            .await
                    }
                },
            )
            .await?;
        Ok(())
    }

    /// [`Svc::update_repository_visibility`].
    async fn update_repository_visibility(
        &self,
        ctx: &Ctx,
        repo_name: &RepositoryName,
        visibility: &Visibility,
    ) -> Result<()> {
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
        self.pool
            .execute(
                ctx.inst_id,
                RequestKind::IdempotentWrite,
                &ctx.cancel_token,
                |client| {
                    let body = body.clone();
                    async move { client.repos().update(&ctx.org, repo_name, &body).await }
                },
            )
            .await?;
        Ok(())
    }
}

/// Information about the target of a GitHub API request.
#[derive(Clone, Default)]
pub struct Ctx {
    /// Cancellation token interrupting rate-limit waits before dispatch.
    pub cancel_token: CancellationToken,
    /// GitHub organization name.
    pub org: String,

    /// GitHub App installation id, when authenticating as a GitHub App.
    pub inst_id: Option<i64>,
}

impl Ctx {
    /// Create a new context bound to `org` and `cancel_token`.
    #[must_use]
    pub fn from_org_with_cancel(org: &Organization, cancel_token: CancellationToken) -> Self {
        Self {
            cancel_token,
            org: org.name.clone(),
            inst_id: Some(org.installation_id),
        }
    }
}

impl From<&Organization> for Ctx {
    fn from(org: &Organization) -> Self {
        Self::from_org_with_cancel(org, CancellationToken::new())
    }
}

// Helpers.

/// Sleep briefly after creating a GitHub resource so subsequent mutations do
/// not hit eventual-consistency 404s, while remaining cancellation-aware.
async fn sleep_post_create(cancel_token: &CancellationToken) -> Result<()> {
    tokio::select! {
        biased;

        () = cancel_token.cancelled() => Err(Error::new(CancelledError)),
        () = sleep(POST_CREATE_PROPAGATION_DELAY) => Ok(()),
    }
}

#[cfg(test)]
mod tests;
