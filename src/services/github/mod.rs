use self::state::{RepositoryChange, State};
use super::{ChangesSummary, ServiceHandler};
use crate::github::DynGH;
use anyhow::Result;
use async_trait::async_trait;
use config::Config;
use std::sync::Arc;

mod legacy;
pub(crate) mod state;

/// Type alias to represent a reconcile plan.
pub(crate) type ReconcilePlan = Vec<RepositoryChange>;

/// GitHub's service handler.
pub(crate) struct Handler {
    cfg: Arc<Config>,
    gh: DynGH,
}

impl Handler {
    /// Create a new Handler instance.
    pub(crate) fn new(cfg: Arc<Config>, gh: DynGH) -> Self {
        Self { cfg, gh }
    }

    /// Get reconcile plan to go from the current to the desired state.
    pub(crate) async fn get_reconcile_plan(
        &self,
        current_state: &State,
        desired_state: &State,
    ) -> Result<ReconcilePlan> {
        todo!()
    }
}

#[async_trait]
impl ServiceHandler for Handler {
    /// [ServiceHandler::get_state_changes_summary]
    async fn get_changes_summary(&self, head_ref: &str) -> Result<ChangesSummary> {
        let state_base = State::new_from_config(self.cfg.clone(), self.gh.clone(), None).await?;
        let state_head =
            State::new_from_config(self.cfg.clone(), self.gh.clone(), Some(head_ref)).await?;
        let state_changes_summary = state_base
            .changes(&state_head)
            .repositories
            .into_iter()
            .map(|change| change.to_string())
            .collect();
        Ok(state_changes_summary)
    }

    /// [ServiceHandler::reconcile]
    async fn reconcile(&self) -> Result<()> {
        todo!()
    }
}
