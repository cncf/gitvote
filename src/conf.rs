use anyhow::Result;
use octocrab::Octocrab;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// GitVote repository configuration file name.
pub const REPO_CONFIG_FILE: &str = ".gitvote.yml";

/// GitVote repository configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct RepoConfig {
    pub voters: Vec<String>,
    pub pass_threshold: f64,
    #[serde(with = "humantime_serde")]
    pub duration: Duration,
}

impl RepoConfig {
    /// Create a new repo config instance from the config file in GitHub.
    pub(crate) async fn new(
        installation_github_client: &Octocrab,
        owner: &str,
        repo: &str,
    ) -> Result<Option<Self>> {
        let response = installation_github_client
            .repos(owner, repo)
            .get_content()
            .path(REPO_CONFIG_FILE)
            .send()
            .await?;
        if response.items.len() != 1 {
            return Ok(None);
        }
        match &response.items[0].decoded_content() {
            Some(content) => Ok(serde_yaml::from_str(content)?),
            None => Ok(None),
        }
    }
}
