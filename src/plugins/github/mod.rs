use self::cfg::Cfg;
use super::{PluginCfgChanges, PluginExecutionPlan};
use crate::github::DynGH;
use anyhow::Result;
use async_trait::async_trait;
use config::Config;
use std::sync::Arc;

pub(crate) mod cfg;
mod legacy;

pub(crate) struct Plugin {
    cfg: Arc<Config>,
    gh: DynGH,
}

impl Plugin {
    /// Create a new Plugin instance.
    pub(crate) fn new(cfg: Arc<Config>, gh: DynGH) -> Self {
        Self { cfg, gh }
    }
}

#[async_trait]
impl super::Plugin for Plugin {
    /// [super::Plugin::execute]
    async fn execute(&self) -> Result<()> {
        todo!()
    }

    /// [super::Plugin::get_config_changes]
    async fn get_config_changes(&self, head_ref: &str) -> Result<PluginCfgChanges> {
        let cfg_base = Cfg::get(self.cfg.clone(), self.gh.clone(), None).await?;
        let cfg_head = Cfg::get(self.cfg.clone(), self.gh.clone(), Some(head_ref)).await?;
        let changes = cfg_base
            .changes(&cfg_head)
            .into_iter()
            .map(|change| change.to_string())
            .collect();
        Ok(changes)
    }

    /// [super::Plugin::get_execution_plan]
    fn get_execution_plan(&self) -> Result<PluginExecutionPlan> {
        todo!()
    }
}
