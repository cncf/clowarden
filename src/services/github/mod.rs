use self::{
    service::DynSvc,
    state::{RepositoryChange, RepositoryInvitationId, RepositoryName, State},
};
use super::{BaseRefConfigStatus, ChangesApplied, ChangesSummary, DynChange, ServiceHandler};
use crate::{
    directory::{DirectoryChange, UserName},
    github::DynGH,
    services::ChangeApplied,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use config::Config;
use std::sync::Arc;
use tracing::debug;

mod legacy;
pub(crate) mod service;
mod state;

/// GitHub's service name.
pub(crate) const SERVICE_NAME: &str = "github";

/// GitHub's service handler.
pub(crate) struct Handler {
    cfg: Arc<Config>,
    gh: DynGH,
    svc: DynSvc,
}

impl Handler {
    /// Create a new Handler instance.
    pub(crate) fn new(cfg: Arc<Config>, gh: DynGH, svc: DynSvc) -> Self {
        Self { cfg, gh, svc }
    }

    /// Helper function to get the invitation id for a given user in a
    /// repository (when available).
    async fn get_repository_invitation(
        &self,
        repo_name: &RepositoryName,
        user_name: &UserName,
    ) -> Result<Option<RepositoryInvitationId>> {
        let invitation_id = self.svc.list_repository_invitations(repo_name).await?.iter().find_map(|i| {
            if i.invitee.is_some() && &i.invitee.as_ref().unwrap().login == user_name {
                return Some(i.id);
            }
            None
        });
        Ok(invitation_id)
    }
}

#[async_trait]
impl ServiceHandler for Handler {
    /// [ServiceHandler::get_changes_summary]
    async fn get_changes_summary(&self, head_ref: &str) -> Result<ChangesSummary> {
        let head_state = State::new_from_config(
            self.cfg.clone(),
            self.gh.clone(),
            self.svc.clone(),
            Some(head_ref),
        )
        .await?;
        let (changes, base_ref_config_status) =
            match State::new_from_config(self.cfg.clone(), self.gh.clone(), self.svc.clone(), None).await {
                Ok(base_state) => {
                    let changes = base_state
                        .diff(&head_state)
                        .repositories
                        .into_iter()
                        .map(|change| Box::new(change) as DynChange)
                        .collect();
                    (changes, BaseRefConfigStatus::Valid)
                }
                Err(_) => (vec![], BaseRefConfigStatus::Invalid),
            };

        Ok(ChangesSummary {
            changes,
            base_ref_config_status,
        })
    }

    /// [ServiceHandler::reconcile]
    async fn reconcile(&self) -> Result<ChangesApplied> {
        // Get changes between the actual and the desired state
        let actual_state = State::new_from_service(self.svc.clone())
            .await
            .context("error getting actual state from service")?;
        let desired_state = State::new_from_config(self.cfg.clone(), self.gh.clone(), self.svc.clone(), None)
            .await
            .context("error getting desired state from configuration")?;
        let changes = actual_state.diff(&desired_state);
        debug!(?changes, "changes between the actual and the desired state");

        // Apply changes needed to match desired state
        let mut changes_applied = vec![];

        // Apply directory changes
        for change in changes.directory.into_iter() {
            let err = match &change {
                DirectoryChange::TeamAdded(team) => self.svc.add_team(team).await.err(),
                DirectoryChange::TeamRemoved(team_name) => self.svc.remove_team(team_name).await.err(),
                DirectoryChange::TeamMaintainerAdded(team_name, user_name) => {
                    self.svc.add_team_maintainer(team_name, user_name).await.err()
                }
                DirectoryChange::TeamMaintainerRemoved(team_name, user_name) => {
                    self.svc.remove_team_maintainer(team_name, user_name).await.err()
                }
                DirectoryChange::TeamMemberAdded(team_name, user_name) => {
                    self.svc.add_team_member(team_name, user_name).await.err()
                }
                DirectoryChange::TeamMemberRemoved(team_name, user_name) => {
                    self.svc.remove_team_member(team_name, user_name).await.err()
                }
                DirectoryChange::UserAdded(_) => continue,
                DirectoryChange::UserRemoved(_) => continue,
                DirectoryChange::UserUpdated(_) => continue,
            };
            changes_applied.push(ChangeApplied {
                change: Box::new(change),
                error: err.map(|e| e.to_string()),
                applied_at: time::OffsetDateTime::now_utc(),
            })
        }

        // Apply repositories changes
        for change in changes.repositories.into_iter() {
            let err = match &change {
                RepositoryChange::RepositoryAdded(repo) => self.svc.add_repository(repo).await.err(),
                RepositoryChange::TeamAdded(repo_name, team_name, role) => {
                    self.svc.add_repository_team(repo_name, team_name, role).await.err()
                }
                RepositoryChange::TeamRemoved(repo_name, team_name) => {
                    self.svc.remove_repository_team(repo_name, team_name).await.err()
                }
                RepositoryChange::TeamRoleUpdated(repo_name, team_name, role) => {
                    self.svc.update_repository_team_role(repo_name, team_name, role).await.err()
                }
                RepositoryChange::CollaboratorAdded(repo_name, user_name, role) => {
                    self.svc.add_repository_collaborator(repo_name, user_name, role).await.err()
                }
                RepositoryChange::CollaboratorRemoved(repo_name, user_name) => {
                    if let Some(invitation_id) = self.get_repository_invitation(repo_name, user_name).await? {
                        self.svc.remove_repository_invitation(repo_name, invitation_id).await.err()
                    } else {
                        self.svc.remove_repository_collaborator(repo_name, user_name).await.err()
                    }
                }
                RepositoryChange::CollaboratorRoleUpdated(repo_name, user_name, role) => {
                    if let Some(invitation_id) = self.get_repository_invitation(repo_name, user_name).await? {
                        self.svc.update_repository_invitation(repo_name, invitation_id, role).await.err()
                    } else {
                        self.svc.update_repository_collaborator_role(repo_name, user_name, role).await.err()
                    }
                }
                RepositoryChange::VisibilityUpdated(repo_name, visibility) => {
                    self.svc.update_repository_visibility(repo_name, visibility).await.err()
                }
            };
            changes_applied.push(ChangeApplied {
                change: Box::new(change),
                error: err.map(|e| e.to_string()),
                applied_at: time::OffsetDateTime::now_utc(),
            })
        }

        Ok(changes_applied)
    }
}
