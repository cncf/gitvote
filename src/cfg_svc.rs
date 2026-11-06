//! This module defines some types and functionality to represent and process
//! the `GitVote` service configuration.

use std::{fmt, path::Path};

use anyhow::{Result, bail};
use deadpool_postgres::Config as Db;
use figment::{
    Figment,
    providers::{Env, Format, Serialized, Yaml},
};
use serde::{Deserialize, Serialize};

#[cfg(test)]
mod tests;

/// Placeholder displayed instead of sensitive values in debug output.
const REDACTED: &str = "[REDACTED]";

/// Server configuration.
#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Cfg {
    /// Database configuration.
    pub db: Db,
    /// GitHub application configuration.
    pub github: GitHubApp,

    /// Address the HTTP server listens on, defaulted when not configured.
    pub addr: String,
    /// Logs configuration, defaulted when not configured.
    pub log: Log,
}

impl Cfg {
    /// Creates a new Cfg instance.
    pub(crate) fn new(config_file: &Path) -> Result<Self> {
        Self::load(config_file, Env::prefixed("GITVOTE_"))
    }

    /// Loads the configuration from the file and environment provider given.
    fn load(config_file: &Path, env: Env) -> Result<Self> {
        // A missing file would be read as empty, which only fails on the first missing
        // field or not at all if the environment provides them all
        if !config_file.is_file() {
            bail!("config file not found: {}", config_file.display());
        }

        // Merge the defaults, the file and the environment, in increasing precedence
        Figment::new()
            .merge(Serialized::default("addr", "127.0.0.1:9000"))
            .merge(Serialized::default("log.format", "pretty"))
            .merge(Yaml::file(config_file))
            .merge(env.split("_").lowercase(false))
            .extract()
            .map_err(Into::into)
    }
}

/// GitHub application configuration.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all(deserialize = "camelCase"))]
pub(crate) struct GitHubApp {
    /// GitHub application id.
    pub app_id: i64,
    /// GitHub application private key, in PEM format.
    pub app_private_key: String,
    /// Secret used to verify the signature of the webhook payloads.
    pub webhook_secret: String,

    /// Previous webhook secret, accepted as well while the secret is being rotated.
    pub webhook_secret_fallback: Option<String>,
}

/// Logs configuration.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub(crate) struct Log {
    /// Format to use in logs.
    pub format: LogFormat,
}

/// Format to use in logs.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all(deserialize = "lowercase"))]
pub(crate) enum LogFormat {
    /// Structured logs, one JSON object per line.
    Json,
    /// Human readable logs.
    Pretty,
}

impl fmt::Debug for Cfg {
    /// Formats the configuration redacting the database one, which holds the
    /// database credentials, possibly as part of a connection URL.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cfg")
            .field("db", &REDACTED)
            .field("github", &self.github)
            .field("addr", &self.addr)
            .field("log", &self.log)
            .finish()
    }
}

impl fmt::Debug for GitHubApp {
    /// Formats the GitHub application configuration redacting the private key and
    /// the webhook secrets.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitHubApp")
            .field("app_id", &self.app_id)
            .field("app_private_key", &REDACTED)
            .field("webhook_secret", &REDACTED)
            .field(
                "webhook_secret_fallback",
                &self.webhook_secret_fallback.as_ref().map(|_| REDACTED),
            )
            .finish()
    }
}
