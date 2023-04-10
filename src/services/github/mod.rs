use self::{service::DynSvc, state::State};
use super::{ActionsSummary, BaseRefConfigStatus, ChangesSummary, ServiceHandler};
use crate::{directory::Change, github::DynGH};
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
    /// [ServiceHandler::get_state_changes_summary]
    async fn get_changes_summary(&self, head_ref: &str) -> Result<ChangesSummary> {
        let head_state = State::new_from_config(self.cfg.clone(), self.gh.clone(), Some(head_ref)).await?;
        match State::new_from_config(self.cfg.clone(), self.gh.clone(), None).await {
            Ok(base_state) => {
                let state_changes = base_state
                    .changes(&head_state)
                    .repositories
                    .into_iter()
                    .map(|change| change.template_format().unwrap())
                    .collect();
                return Ok((state_changes, BaseRefConfigStatus::Valid));
            }
            Err(_) => Ok((vec![], BaseRefConfigStatus::Invalid)),
        }
    }

    /// [ServiceHandler::reconcile]
    async fn reconcile(&self) -> Result<ActionsSummary> {
        // Get changes between the current and the desired state
        let current_state = State::new_from_service(self.svc.clone())
            .await
            .context("error getting current state from service")?;
        let desired_state = State::new_from_config(self.cfg.clone(), self.gh.clone(), None)
            .await
            .context("error getting desired state from configuration")?;
        let changes = current_state.changes(&desired_state);
        debug!(?changes);

        // Execute actions needed to match desired state
        for change in changes.directory {
            match change {
                Change::TeamAdded(_) => {}
                Change::TeamRemoved(team_name) => {
                    self.svc.remove_team(&team_name).await?;
                }
                _ => {}
            }
        }

        Ok(vec![])
    }
}
