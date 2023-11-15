//! This module defines some types and traits that service handlers
//! implementations will rely upon.

use crate::{cfg::Organization, github::Source};
use anyhow::Result;
use as_any::AsAny;
use async_trait::async_trait;
use std::fmt::Debug;

pub mod github;

/// Type alias to represent a service name.
pub type ServiceName = &'static str;

/// Trait that defines some operations a service handler must support.
#[async_trait]
pub trait ServiceHandler {
    /// Return a summary of the changes detected in the service's state as
    /// defined in the configuration from the base to the head reference.
    async fn get_changes_summary(&self, org: &Organization, head_src: &Source) -> Result<ChangesSummary>;

    /// Apply the changes needed so that the actual state (as defined in the
    /// service) matches the desired state (as defined in the configuration).
    async fn reconcile(&self, org: &Organization) -> Result<ChangesApplied>;
}

/// Type alias to represent a service handler trait object.
pub type DynServiceHandler = Box<dyn ServiceHandler + Send + Sync>;

/// Represents a summary of changes detected in the service's state as defined
/// in the configuration from the base to the head reference.
pub struct ChangesSummary {
    pub changes: Vec<DynChange>,
    pub base_ref_config_status: BaseRefConfigStatus,
}

/// Type alias to represent some changes applied on a service.
pub type ChangesApplied = Vec<ChangeApplied>;

/// Represents a change applied on a service in an attempt to get closer to the
/// desired state.
#[derive(Debug)]
pub struct ChangeApplied {
    pub change: DynChange,
    pub error: Option<String>,
    pub applied_at: time::OffsetDateTime,
}

/// Trait that defines some operations a Change implementation must support.
pub trait Change: AsAny + Debug {
    /// Return some details about the change.
    fn details(&self) -> ChangeDetails;

    /// Keywords used to facilitate locating specific changes on searches.
    fn keywords(&self) -> Vec<&str>;

    /// Format change to be used on a template.
    fn template_format(&self) -> Result<String>;
}

/// Type alias to represent a change trait object.
pub type DynChange = Box<dyn Change + Send + Sync>;

/// Status of the configuration in the base reference.
#[derive(Debug, Clone, PartialEq)]
pub enum BaseRefConfigStatus {
    Valid,
    Invalid,
    Unknown,
}

impl BaseRefConfigStatus {
    /// Check if the configuration is invalid.
    #[must_use]
    pub fn is_invalid(&self) -> bool {
        *self == BaseRefConfigStatus::Invalid
    }
}

/// ChangeDetails represents some details about a change.
#[derive(Debug, Clone, PartialEq)]
pub struct ChangeDetails {
    pub kind: String,
    pub extra: serde_json::Value,
}
