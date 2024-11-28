//! This module defines some types to represent the configuration.

use std::path::{Path, PathBuf};

use anyhow::Result;
use deadpool_postgres::Config as Db;
use figment::{
    providers::{Env, Format, Serialized, Yaml},
    Figment,
};
use serde::{Deserialize, Serialize};

use clowarden_core::cfg::{GitHubApp, Organization, Services};

/// Server configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct Config {
    pub db: Db,
    pub log: Log,
    pub server: HttpServer,
    pub services: Services,
    pub organizations: Option<Vec<Organization>>,
}

impl Config {
    /// Create a new Config instance.
    pub(crate) fn new(config_file: &Path) -> Result<Self> {
        Figment::new()
            .merge(Serialized::default("log.format", "pretty"))
            .merge(Serialized::default("server.addr", "127.0.0.1:9000"))
            .merge(Yaml::file(config_file))
            .merge(Env::prefixed("CLOWARDEN_").split("_").lowercase(false))
            .extract()
            .map_err(Into::into)
    }
}

/// Logs configuration.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub(crate) struct Log {
    pub format: LogFormat,
}

/// Format to use in logs.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all(deserialize = "lowercase"))]
pub(crate) enum LogFormat {
    Json,
    Pretty,
}

/// Http server configuration.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all(deserialize = "camelCase"))]
pub(crate) struct HttpServer {
    pub addr: String,
    pub static_path: PathBuf,
    pub basic_auth: Option<BasicAuth>,
    pub github_app: GitHubApp,
}

/// Basic authentication configuration.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub(crate) struct BasicAuth {
    pub enabled: bool,
    pub username: String,
    pub password: String,
}
