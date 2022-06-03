use anyhow::Result;
use octocrab::Octocrab;
use serde::Deserialize;
use std::time::Duration;

/// Metadata file name.
const METADATA_FILE: &str = ".gitvote.yml";

/// GitVote metadata.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Metadata {
    pub voters: Vec<String>,
    pub pass_threshold: f64,
    #[serde(with = "humantime_serde")]
    pub duration: Duration,
    #[serde(with = "humantime_serde")]
    pub reminder: Duration,
}

impl Metadata {
    /// Create a new metadata instance from the metadata file in the GitHub repo.
    pub(crate) async fn new(
        installation_github_client: &Octocrab,
        owner: &str,
        repo: &str,
    ) -> Result<Option<Self>> {
        let response = installation_github_client
            .repos(owner, repo)
            .get_content()
            .path(METADATA_FILE)
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
