//! This module defines some types and functionality to represent and process
//! the `GitVote` service configuration.

use std::path::Path;

use anyhow::Result;
use deadpool_postgres::Config as Db;
use figment::{
    Figment,
    providers::{Env, Format, Serialized, Yaml},
};
use serde::{Deserialize, Serialize};

/// Server configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct Cfg {
    pub addr: String,
    pub db: Db,
    pub github: GitHubApp,
    pub log: Log,
}

impl Cfg {
    /// Create a new Cfg instance.
    pub(crate) fn new(config_file: &Path) -> Result<Self> {
        Figment::new()
            .merge(Serialized::default("addr", "127.0.0.1:9000"))
            .merge(Serialized::default("log.format", "pretty"))
            .merge(Yaml::file(config_file))
            .merge(Env::prefixed("GITVOTE_").split("_").lowercase(false))
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

/// GitHub application configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all(deserialize = "camelCase"))]
pub struct GitHubApp {
    pub app_id: i64,
    pub app_private_key: String,
    pub webhook_secret: String,
    pub webhook_secret_fallback: Option<String>,
}
