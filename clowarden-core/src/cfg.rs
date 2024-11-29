//! This module defines some types that represent parts of the configuration.

use serde::{Deserialize, Serialize};

/// GitHub application configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all(deserialize = "camelCase"))]
pub struct GitHubApp {
    pub app_id: i64,
    pub private_key: String,
    pub webhook_secret: String,
    pub webhook_secret_fallback: Option<String>,
}

/// Organization configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all(deserialize = "camelCase"))]
pub struct Organization {
    pub name: String,
    pub installation_id: i64,
    pub repository: String,
    pub branch: String,
    pub legacy: Legacy,
}

/// Organization legacy configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all(deserialize = "camelCase"))]
pub struct Legacy {
    pub enabled: bool,
    pub sheriff_permissions_path: String,
    pub cncf_people_path: Option<String>,
}

/// Services configuration.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Services {
    pub github: Service,
}

/// Service configuration.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Service {
    pub enabled: bool,
}
