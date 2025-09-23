//! This module contains the implementation of the GitHub service handler.

use std::collections::HashSet;

use anyhow::{Context, Result, format_err};
use as_any::Downcast;
use async_trait::async_trait;
use state::Changes;
use tracing::debug;

use crate::{
    cfg::Organization,
    directory::{DirectoryChange, UserName},
    github::{DynGH, Source},
    multierror::MultiError,
    services::ChangeApplied,
};

use super::{BaseRefConfigStatus, ChangesApplied, ChangesSummary, DynChange, ServiceHandler};

use self::{
    service::{Ctx, DynSvc},
    state::{RepositoryChange, RepositoryInvitationId, RepositoryName},
};

mod legacy;
pub mod service;
pub mod state;
pub use state::State;

/// GitHub's service name.
pub const SERVICE_NAME: &str = "github";

/// GitHub's service handler.
pub struct Handler {
    gh: DynGH,
    svc: DynSvc,
}

impl Handler {
    /// Create a new Handler instance.
    pub fn new(gh: DynGH, svc: DynSvc) -> Self {
        Self { gh, svc }
    }

    /// Helper function to get the invitation id for a given user in a
    /// repository (when available).
    async fn get_repository_invitation(
        &self,
        ctx: &Ctx,
        repo_name: &RepositoryName,
        user_name: &UserName,
    ) -> Result<Option<RepositoryInvitationId>> {
        let invitation_id =
            self.svc.list_repository_invitations(ctx, repo_name).await?.iter().find_map(|i| {
                if i.invitee.is_some() && &i.invitee.as_ref().unwrap().login == user_name {
                    return Some(i.id);
                }
                None
            });
        Ok(invitation_id)
    }

    /// Validate users found in some of the changes provided.
    async fn validate_users(&self, ctx: &Ctx, changes: &Changes) -> Result<()> {
        let mut merr = MultiError::new(Some("invalid github service configuration".to_string()));

        // Collect users to validate from changes
        let mut users_to_validate = HashSet::new();
        for change in &changes.directory {
            if let DirectoryChange::TeamMemberAdded(_, user_name) = change {
                users_to_validate.insert(user_name);
            }
        }
        for change in &changes.repositories {
            if let RepositoryChange::CollaboratorAdded(_, user_name, _) = change {
                users_to_validate.insert(user_name);
            }
        }

        // Validate users collected
        for user_name in users_to_validate {
            match self.svc.get_user_login(ctx, user_name).await {
                Ok(valid_user_name) => {
                    if user_name != &valid_user_name {
                        merr.push(format_err!(
                            "user[{user_name}]: invalid username, should be {valid_user_name}",
                        ));
                    }
                }
                Err(err) => {
                    merr.push(format_err!("user[{user_name}]: error validating username: {err}"));
                }
            }
        }

        if merr.contains_errors() {
            return Err(merr.into());
        }
        Ok(())
    }
}

#[async_trait]
impl ServiceHandler for Handler {
    /// [ServiceHandler::get_changes_summary]
    async fn get_changes_summary(&self, org: &Organization, head_src: &Source) -> Result<ChangesSummary> {
        let ctx = Ctx::from(org);
        let base_src = Source::from(org);
        let head_state =
            State::new_from_config(self.gh.clone(), self.svc.clone(), &org.legacy, &ctx, head_src).await?;
        let (changes, base_ref_config_status) =
            match State::new_from_config(self.gh.clone(), self.svc.clone(), &org.legacy, &ctx, &base_src)
                .await
            {
                Ok(base_state) => {
                    let changes = base_state.diff(&head_state);
                    self.validate_users(&ctx, &changes).await?;
                    let repositories_changes = changes
                        .repositories
                        .into_iter()
                        .map(|change| Box::new(change) as DynChange)
                        .collect();
                    (repositories_changes, BaseRefConfigStatus::Valid)
                }
                Err(_) => (vec![], BaseRefConfigStatus::Invalid),
            };

        Ok(ChangesSummary {
            changes,
            base_ref_config_status,
        })
    }

    /// [ServiceHandler::reconcile]
    async fn reconcile(&self, org: &Organization) -> Result<ChangesApplied> {
        // Get changes between the actual and the desired state
        let ctx = Ctx::from(org);
        let src = Source::from(org);
        let actual_state = State::new_from_service(self.svc.clone(), &ctx)
            .await
            .context("error getting actual state from service")?;
        let desired_state =
            State::new_from_config(self.gh.clone(), self.svc.clone(), &org.legacy, &ctx, &src)
                .await
                .context("error getting desired state from configuration")?;
        let changes = actual_state.diff(&desired_state);
        debug!(?changes, "changes between the actual and the desired state");

        // Apply changes needed to match desired state
        let mut changes_applied = vec![];

        // Apply directory changes
        let ctx = Ctx::from(org);
        for change in changes.directory {
            let err = match &change {
                DirectoryChange::TeamAdded(team) => self.svc.add_team(&ctx, team).await.err(),
                DirectoryChange::TeamRemoved(team_name) => self.svc.remove_team(&ctx, team_name).await.err(),
                DirectoryChange::TeamMaintainerAdded(team_name, user_name) => {
                    self.svc.add_team_maintainer(&ctx, team_name, user_name).await.err()
                }
                DirectoryChange::TeamMaintainerRemoved(team_name, user_name) => {
                    self.svc.remove_team_maintainer(&ctx, team_name, user_name).await.err()
                }
                DirectoryChange::TeamMemberAdded(team_name, user_name) => {
                    self.svc.add_team_member(&ctx, team_name, user_name).await.err()
                }
                DirectoryChange::TeamMemberRemoved(team_name, user_name) => {
                    self.svc.remove_team_member(&ctx, team_name, user_name).await.err()
                }
                DirectoryChange::UserAdded(_)
                | DirectoryChange::UserRemoved(_)
                | DirectoryChange::UserUpdated(_) => continue,
            };
            changes_applied.push(ChangeApplied {
                change: Box::new(change),
                error: err.map(|e| e.to_string()),
                applied_at: time::OffsetDateTime::now_utc(),
            });
        }

        // Apply repositories changes
        'changes_repositories: for change in changes.repositories {
            let err = match &change {
                RepositoryChange::RepositoryAdded(repo) => self.svc.add_repository(&ctx, repo).await.err(),
                RepositoryChange::TeamAdded(repo_name, team_name, role) => {
                    self.svc.add_repository_team(&ctx, repo_name, team_name, role).await.err()
                }
                RepositoryChange::TeamRemoved(repo_name, team_name) => {
                    // If the team has just been deleted from the directory in
                    // this reconciliation, there is no need to remove it from
                    // the repository as this will be done automatically when
                    // the team is deleted from GitHub
                    for entry in &changes_applied {
                        let change = (*entry.change).downcast_ref::<DirectoryChange>();
                        if let Some(DirectoryChange::TeamRemoved(team_removed_name)) = change
                            && team_name == team_removed_name
                        {
                            continue 'changes_repositories;
                        }
                    }

                    self.svc.remove_repository_team(&ctx, repo_name, team_name).await.err()
                }
                RepositoryChange::TeamRoleUpdated(repo_name, team_name, role) => {
                    self.svc.update_repository_team_role(&ctx, repo_name, team_name, role).await.err()
                }
                RepositoryChange::CollaboratorAdded(repo_name, user_name, role) => {
                    self.svc.add_repository_collaborator(&ctx, repo_name, user_name, role).await.err()
                }
                RepositoryChange::CollaboratorRemoved(repo_name, user_name) => {
                    if let Some(invitation_id) =
                        self.get_repository_invitation(&ctx, repo_name, user_name).await?
                    {
                        self.svc.remove_repository_invitation(&ctx, repo_name, invitation_id).await.err()
                    } else {
                        self.svc.remove_repository_collaborator(&ctx, repo_name, user_name).await.err()
                    }
                }
                RepositoryChange::CollaboratorRoleUpdated(repo_name, user_name, role) => {
                    if let Some(invitation_id) =
                        self.get_repository_invitation(&ctx, repo_name, user_name).await?
                    {
                        self.svc
                            .update_repository_invitation(&ctx, repo_name, invitation_id, role)
                            .await
                            .err()
                    } else {
                        self.svc
                            .update_repository_collaborator_role(&ctx, repo_name, user_name, role)
                            .await
                            .err()
                    }
                }
                RepositoryChange::VisibilityUpdated(repo_name, visibility) => {
                    self.svc.update_repository_visibility(&ctx, repo_name, visibility).await.err()
                }
            };
            changes_applied.push(ChangeApplied {
                change: Box::new(change),
                error: err.map(|e| e.to_string()),
                applied_at: time::OffsetDateTime::now_utc(),
            });
        }

        Ok(changes_applied)
    }
}
