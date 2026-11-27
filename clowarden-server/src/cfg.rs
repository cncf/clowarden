//! This module defines some types to represent the configuration.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use deadpool_postgres::Config as Db;
use figment::{
    Figment,
    providers::{Env, Format, Serialized, Yaml},
};
use serde::{Deserialize, Serialize};

use clowarden_core::cfg::{GitHubApp, Organization, Services};

/// Default frequency of periodic full reconciliations per organization (in
/// seconds).
pub(crate) const DEFAULT_RECONCILE_FREQUENCY: u64 = 6 * 60 * 60; // 6 hours

/// Maximum supported frequency of periodic full reconciliations per
/// organization (in seconds). Caps values well within representable `Instant`
/// deadlines across platforms.
pub(crate) const MAX_RECONCILE_FREQUENCY: u64 = 365 * 24 * 60 * 60; // 1 year

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
        // Load configuration from the sources available
        let cfg: Self = Figment::new()
            .merge(Serialized::default("log.format", "pretty"))
            .merge(Serialized::default("server.addr", "127.0.0.1:9000"))
            .merge(Serialized::default(
                "server.reconcileFrequency",
                DEFAULT_RECONCILE_FREQUENCY,
            ))
            .merge(Yaml::file(config_file))
            .merge(Env::prefixed("CLOWARDEN_").split("_").lowercase(false))
            .extract()?;

        // Validate configuration before it is used
        cfg.validate()?;

        Ok(cfg)
    }

    /// Validate the configuration. Empty webhook secrets and out-of-range
    /// reconcile frequencies are rejected.
    fn validate(&self) -> Result<()> {
        let github_app = &self.server.github_app;
        if github_app.webhook_secret.is_empty() {
            bail!("server.githubApp.webhookSecret must not be empty");
        }
        if github_app.webhook_secret_fallback.as_ref().is_some_and(String::is_empty) {
            bail!("server.githubApp.webhookSecretFallback must not be empty");
        }
        if self.server.reconcile_frequency == 0 {
            bail!("server.reconcileFrequency must be greater than 0");
        }
        if self.server.reconcile_frequency > MAX_RECONCILE_FREQUENCY {
            bail!("server.reconcileFrequency must not exceed {MAX_RECONCILE_FREQUENCY} seconds");
        }

        Ok(())
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
    pub github_app: GitHubApp,
    pub reconcile_frequency: u64,
    pub static_path: PathBuf,

    pub basic_auth: Option<BasicAuth>,
}

/// Basic authentication configuration.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub(crate) struct BasicAuth {
    pub enabled: bool,
    pub username: String,
    pub password: String,
}

#[cfg(test)]
mod tests {
    use clowarden_core::cfg::Service;

    use super::*;

    const EMPTY_WEBHOOK_SECRET_CONFIG: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/testdata/config/empty-webhook-secret.yaml"
    );
    const WEBHOOK_SECRET: &str = "webhook-secret";
    const WEBHOOK_SECRET_FALLBACK: &str = "webhook-secret-fallback";

    #[test]
    fn test_new_rejects_empty_webhook_secret() {
        // Load the configuration file
        let result = Config::new(Path::new(EMPTY_WEBHOOK_SECRET_CONFIG));

        // Check the empty secret was rejected
        assert_eq!(
            result.unwrap_err().to_string(),
            "server.githubApp.webhookSecret must not be empty"
        );
    }

    #[test]
    fn test_validate_accepts_non_empty_secrets() {
        // Setup configurations with and without fallback secret
        let without_fallback = sample_config(WEBHOOK_SECRET, None);
        let with_fallback = sample_config(WEBHOOK_SECRET, Some(WEBHOOK_SECRET_FALLBACK));

        // Check both configurations are valid
        assert!(without_fallback.validate().is_ok());
        assert!(with_fallback.validate().is_ok());
    }

    #[test]
    fn test_validate_rejects_empty_webhook_secret() {
        // Setup configuration with an empty secret
        let cfg = sample_config("", Some(WEBHOOK_SECRET_FALLBACK));

        // Check the empty secret was rejected
        assert_eq!(
            cfg.validate().unwrap_err().to_string(),
            "server.githubApp.webhookSecret must not be empty"
        );
    }

    #[test]
    fn test_validate_rejects_empty_webhook_secret_fallback() {
        // Setup configuration with an empty fallback secret
        let cfg = sample_config(WEBHOOK_SECRET, Some(""));

        // Check the empty fallback secret was rejected
        assert_eq!(
            cfg.validate().unwrap_err().to_string(),
            "server.githubApp.webhookSecretFallback must not be empty"
        );
    }

    #[test]
    fn test_validate_accepts_max_reconcile_frequency() {
        // Setup configuration at the maximum supported reconcile frequency
        let mut cfg = sample_config(WEBHOOK_SECRET, None);
        cfg.server.reconcile_frequency = MAX_RECONCILE_FREQUENCY;

        // Check maximum frequency is accepted
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_validate_rejects_excessive_reconcile_frequency() {
        // Setup configuration above the maximum supported reconcile frequency
        let mut cfg = sample_config(WEBHOOK_SECRET, None);
        cfg.server.reconcile_frequency = u64::MAX;

        // Check excessive frequency was rejected
        assert_eq!(
            cfg.validate().unwrap_err().to_string(),
            format!("server.reconcileFrequency must not exceed {MAX_RECONCILE_FREQUENCY} seconds")
        );
    }

    #[test]
    fn test_validate_rejects_zero_reconcile_frequency() {
        // Setup configuration with a zero reconcile frequency
        let mut cfg = sample_config(WEBHOOK_SECRET, None);
        cfg.server.reconcile_frequency = 0;

        // Check zero frequency was rejected
        assert_eq!(
            cfg.validate().unwrap_err().to_string(),
            "server.reconcileFrequency must be greater than 0"
        );
    }

    // Helpers.

    /// Returns a configuration that uses the webhook secrets provided.
    fn sample_config(webhook_secret: &str, webhook_secret_fallback: Option<&str>) -> Config {
        Config {
            db: Db::default(),
            log: Log {
                format: LogFormat::Json,
            },
            server: HttpServer {
                addr: "127.0.0.1:9000".to_string(),
                github_app: GitHubApp {
                    webhook_secret: webhook_secret.to_string(),
                    webhook_secret_fallback: webhook_secret_fallback.map(ToString::to_string),
                    ..GitHubApp::default()
                },
                reconcile_frequency: DEFAULT_RECONCILE_FREQUENCY,
                static_path: PathBuf::from("/tmp/static"),
                basic_auth: None,
            },
            services: Services {
                github: Service { enabled: true },
            },
            organizations: None,
        }
    }
}
