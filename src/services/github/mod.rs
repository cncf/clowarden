use self::state::State;
use super::{ActionsSummary, BaseRefConfigStatus, ChangesSummary, ServiceHandler};
use crate::github::DynGH;
use anyhow::Result;
use async_trait::async_trait;
use config::Config;
use std::sync::Arc;

mod legacy;
pub(crate) mod state;

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
}

#[async_trait]
impl ServiceHandler for Handler {
    /// [ServiceHandler::get_state_changes_summary]
    async fn get_changes_summary(&self, head_ref: &str) -> Result<ChangesSummary> {
        let state_head = State::new_from_config(self.cfg.clone(), self.gh.clone(), Some(head_ref)).await?;
        match State::new_from_config(self.cfg.clone(), self.gh.clone(), None).await {
            Ok(state_base) => {
                let state_changes = state_base
                    .changes(&state_head)
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
        todo!()
    }
}
