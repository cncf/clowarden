use anyhow::Result;
use async_trait::async_trait;

pub(crate) mod github;

/// Type alias to represent a Plugin trait object.
pub(crate) type DynPlugin = Box<dyn Plugin + Send + Sync>;

/// Type alias to represent a plugin name.
pub(crate) type PluginName = &'static str;

/// Type alias to represent some configuration changes.
pub(crate) type PluginCfgChanges = Vec<String>;

/// Type alias to represent an execution plan summary.
pub(crate) type PluginExecutionPlan = Vec<String>;

/// Trait that defines some operations a plugin must support.
#[async_trait]
pub(crate) trait Plugin {
    /// Apply the necessary changes so that the current state (as available in
    /// the corresponding service) matches the desired state (as defined in the
    /// directory and plugin configuration).
    async fn execute(&self) -> Result<()>;

    /// Return a summary of the changes detected in the plugin configuration
    /// from the base to the head reference.
    async fn get_config_changes(&self, head_ref: &str) -> Result<PluginCfgChanges>;

    /// Return a summary of the plugin's execution plan (changes it would apply)
    /// by comparing the desired state (as defined in the directory and plugin
    /// configuration) with the current state of the service.
    fn get_execution_plan(&self) -> Result<PluginExecutionPlan>;
}
