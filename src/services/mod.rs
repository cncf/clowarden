use anyhow::Result;
use async_trait::async_trait;

pub(crate) mod github;

/// Type alias to represent a service name.
pub(crate) type ServiceName = &'static str;

/// Type alias to represent a service handler trait object.
pub(crate) type DynServiceHandler = Box<dyn ServiceHandler + Send + Sync>;

/// Type alias to represent some service state changes.
pub(crate) type ChangesSummary = Vec<String>;

/// Trait that defines some operations a service handler must support.
#[async_trait]
pub(crate) trait ServiceHandler {
    /// Return a summary of the changes detected in the service's state as
    /// defined in the configuration from the base to the head reference.
    async fn get_changes_summary(&self, head_ref: &str) -> Result<ChangesSummary>;

    /// Execute the actions needed so that the current state (as defined in the
    /// service) matches the desired state (as defined in the configuration).
    async fn reconcile(&self) -> Result<()>;
}
