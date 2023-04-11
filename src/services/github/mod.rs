use self::{service::DynSvc, state::State};
use super::{BaseRefConfigStatus, ChangesApplied, ChangesSummary, DynChange, ServiceHandler};
use crate::{directory::DirectoryChange, github::DynGH, services::ChangeApplied};
use anyhow::{Context, Result};
use async_trait::async_trait;
use config::Config;
use std::sync::Arc;
use tracing::debug;

mod legacy;
pub(crate) mod service;
mod state;

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
}

#[async_trait]
impl ServiceHandler for Handler {
    /// [ServiceHandler::get_changes_summary]
    async fn get_changes_summary(&self, head_ref: &str) -> Result<ChangesSummary> {
        let head_state = State::new_from_config(self.cfg.clone(), self.gh.clone(), Some(head_ref)).await?;
        let (changes, base_ref_config_status) =
            match State::new_from_config(self.cfg.clone(), self.gh.clone(), None).await {
                Ok(base_state) => {
                    let changes = base_state
                        .changes(&head_state)
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
        // Get changes between the current and the desired state
        let current_state = State::new_from_service(self.svc.clone())
            .await
            .context("error getting current state from service")?;
        let desired_state = State::new_from_config(self.cfg.clone(), self.gh.clone(), None)
            .await
            .context("error getting desired state from configuration")?;
        let changes = current_state.changes(&desired_state);
        debug!(?changes, "changes between the current and the desired state");

        // Apply changes needed to match desired state
        let mut changes_applied = vec![];
        for change in changes.directory.into_iter() {
            let err = match &change {
                DirectoryChange::TeamRemoved(team_name) => self.svc.remove_team(team_name).await.err(),
                _ => None,
            };
            changes_applied.push(ChangeApplied {
                change: Box::new(change),
                error: err.map(|e| e.to_string()),
            })
        }

        Ok(changes_applied)
    }
}
