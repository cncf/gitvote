use crate::github::{DynGH, TeamSlug, UserName};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, time::Duration};
use thiserror::Error;

/// Default configuration profile.
const DEFAULT_PROFILE: &str = "default";

/// Type alias to represent a profile name.
type ProfileName = String;

/// Vote configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(transparent)]
struct Cfg {
    pub profiles: HashMap<ProfileName, CfgProfile>,
}

/// Vote configuration profile.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct CfgProfile {
    #[serde(with = "humantime_serde")]
    pub duration: Duration,
    pub pass_threshold: f64,
    pub allowed_voters: Option<AllowedVoters>,
}

impl CfgProfile {
    /// Get the vote configuration profile requested from the config file in
    /// the repository if available.
    pub(crate) async fn get<'a>(
        gh: DynGH,
        inst_id: u64,
        owner: &'a str,
        repo: &'a str,
        profile: Option<String>,
    ) -> Result<Self, CfgError> {
        match gh.get_config_file(inst_id, owner, repo).await {
            Some(content) => {
                let mut cfg: Cfg = serde_yaml::from_str(&content)
                    .map_err(|e| CfgError::InvalidConfig(e.to_string()))?;
                let profile = profile.unwrap_or_else(|| DEFAULT_PROFILE.to_string());
                match cfg.profiles.remove(&profile) {
                    Some(profile) => Ok(profile),
                    None => Err(CfgError::ProfileNotFound),
                }
            }
            None => Err(CfgError::ConfigNotFound),
        }
    }
}

/// Represents the teams and users allowed to vote.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct AllowedVoters {
    pub teams: Option<Vec<TeamSlug>>,
    pub users: Option<Vec<UserName>>,
}

/// Errors that may occur while getting the configuration profile.
#[derive(Debug, Error, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum CfgError {
    #[error("config not found")]
    ConfigNotFound,
    #[error("invalid config: {0}")]
    InvalidConfig(String),
    #[error("profile not found")]
    ProfileNotFound,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::MockGH;
    use crate::testutil::*;
    use futures::future::{self};
    use mockall::predicate::eq;
    use std::sync::Arc;

    #[tokio::test]
    async fn get_cfg_profile_config_not_found() {
        let mut gh = MockGH::new();
        gh.expect_get_config_file()
            .with(eq(INST_ID as u64), eq(ORG), eq(REPO))
            .returning(|_, _, _| Box::pin(future::ready(None)));
        let gh = Arc::new(gh);

        assert_eq!(
            CfgProfile::get(gh, INST_ID, ORG, REPO, None)
                .await
                .unwrap_err(),
            CfgError::ConfigNotFound
        )
    }

    #[tokio::test]
    async fn get_cfg_profile_invalid_config() {
        let mut gh = MockGH::new();
        gh.expect_get_config_file()
            .with(eq(INST_ID), eq(ORG), eq(REPO))
            .returning(|_, _, _| Box::pin(future::ready(Some(get_test_invalid_config()))));
        let gh = Arc::new(gh);

        assert!(matches!(
            CfgProfile::get(gh, INST_ID, ORG, REPO, Some(PROFILE.to_string()))
                .await
                .unwrap_err(),
            CfgError::InvalidConfig(_)
        ))
    }

    #[tokio::test]
    async fn get_cfg_profile_profile_not_found() {
        let mut gh = MockGH::new();
        gh.expect_get_config_file()
            .with(eq(INST_ID), eq(ORG), eq(REPO))
            .returning(|_, _, _| Box::pin(future::ready(Some(get_test_valid_config()))));
        let gh = Arc::new(gh);

        assert_eq!(
            CfgProfile::get(gh, INST_ID, ORG, REPO, Some("profile9".to_string()))
                .await
                .unwrap_err(),
            CfgError::ProfileNotFound
        )
    }

    #[tokio::test]
    async fn get_cfg_profile_default() {
        let mut gh = MockGH::new();
        gh.expect_get_config_file()
            .with(eq(INST_ID), eq(ORG), eq(REPO))
            .returning(|_, _, _| Box::pin(future::ready(Some(get_test_valid_config()))));
        let gh = Arc::new(gh);

        assert_eq!(
            CfgProfile::get(gh, INST_ID, ORG, REPO, None).await.unwrap(),
            CfgProfile {
                duration: Duration::from_secs(300),
                pass_threshold: 50.0,
                allowed_voters: Some(AllowedVoters {
                    teams: None,
                    users: None
                }),
            }
        )
    }

    #[tokio::test]
    async fn get_cfg_profile_profile1() {
        let mut gh = MockGH::new();
        gh.expect_get_config_file()
            .with(eq(INST_ID), eq(ORG), eq(REPO))
            .returning(|_, _, _| Box::pin(future::ready(Some(get_test_valid_config()))));
        let gh = Arc::new(gh);

        assert_eq!(
            CfgProfile::get(gh, INST_ID, ORG, REPO, Some(PROFILE.to_string()))
                .await
                .unwrap(),
            CfgProfile {
                duration: Duration::from_secs(600),
                pass_threshold: 75.0,
                allowed_voters: Some(AllowedVoters {
                    teams: Some(vec![TEAM1.to_string()]),
                    users: Some(vec![USER1.to_string(), USER2.to_string()]),
                }),
            }
        )
    }
}
