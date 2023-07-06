use crate::github::{DynGH, File, TeamSlug, UserName};
use anyhow::{format_err, Result};
use ignore::gitignore::GitignoreBuilder;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, time::Duration};
use thiserror::Error;

/// Default configuration profile.
const DEFAULT_PROFILE: &str = "default";

/// Error message used when teams are listed in the allowed voters section on a
/// repository that does not belong to an organization.
const ERR_TEAMS_NOT_ALLOWED: &str = "teams in allowed voters can only be used in organizations";

/// Type alias to represent a profile name.
type ProfileName = String;

/// GitVote configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct Cfg {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub automation: Option<Automation>,
    pub profiles: HashMap<ProfileName, CfgProfile>,
}

impl Cfg {
    /// Get the GitVote configuration for the repository provided.
    pub(crate) async fn get<'a>(
        gh: DynGH,
        inst_id: u64,
        owner: &'a str,
        repo: &'a str,
    ) -> Result<Self, CfgError> {
        match gh.get_config_file(inst_id, owner, repo).await {
            Some(content) => {
                let cfg: Cfg = serde_yaml::from_str(&content)
                    .map_err(|e| CfgError::InvalidConfig(e.to_string()))?;
                Ok(cfg)
            }
            None => Err(CfgError::ConfigNotFound),
        }
    }
}

/// Automation configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct Automation {
    pub enabled: bool,
    pub rules: Vec<AutomationRule>,
}

/// Automation rule.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct AutomationRule {
    pub patterns: Vec<String>,
    pub profile: ProfileName,
}

impl AutomationRule {
    /// Check if any of the files provided matches any of the rule patterns.
    /// Patterns must follow the gitignore format.
    pub(crate) fn matches(&self, files: &[File]) -> Result<bool> {
        let mut builder = GitignoreBuilder::new("/");
        for pattern in &self.patterns {
            builder.add_line(None, pattern)?;
        }
        let checker = builder.build()?;
        let matches = files
            .iter()
            .any(|file| checker.matched(&file.filename, false).is_ignore());
        Ok(matches)
    }
}

/// Vote configuration profile.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct CfgProfile {
    #[serde(with = "humantime_serde")]
    pub duration: Duration,
    pub pass_threshold: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_voters: Option<AllowedVoters>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub periodic_status_check: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub close_on_passing: Option<bool>,
}

impl CfgProfile {
    /// Get the vote configuration profile requested from the config file in
    /// the repository if available.
    pub(crate) async fn get<'a>(
        gh: DynGH,
        inst_id: u64,
        owner: &'a str,
        is_org: bool,
        repo: &'a str,
        profile_name: Option<String>,
    ) -> Result<Self, CfgError> {
        let mut cfg = Cfg::get(gh, inst_id, owner, repo).await?;
        let profile_name = profile_name.unwrap_or_else(|| DEFAULT_PROFILE.to_string());
        match cfg.profiles.remove(&profile_name) {
            Some(profile) => match profile.validate(is_org) {
                Ok(()) => Ok(profile),
                Err(err) => Err(CfgError::InvalidConfig(err.to_string())),
            },
            None => Err(CfgError::ProfileNotFound),
        }
    }

    /// Check if the configuration profile is valid.
    fn validate(&self, is_org: bool) -> Result<()> {
        // Only repositories that belong to some organization can use teams in
        // the allowed voters configuration section.
        if !is_org {
            if let Some(teams) = self
                .allowed_voters
                .as_ref()
                .and_then(|allowed_voters| allowed_voters.teams.as_ref())
            {
                if !teams.is_empty() {
                    return Err(format_err!(ERR_TEAMS_NOT_ALLOWED));
                }
            }
        }

        Ok(())
    }
}

/// Represents the teams and users allowed to vote.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct AllowedVoters {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub teams: Option<Vec<TeamSlug>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub users: Option<Vec<UserName>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude_team_maintainers: Option<bool>,
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
    use futures::future;
    use mockall::predicate::eq;
    use std::sync::Arc;

    #[test]
    fn automation_rule_matches() {
        let rule = AutomationRule {
            patterns: vec!["*.md".to_string(), "file.txt".to_string()],
            profile: "default".to_string(),
        };
        assert!(rule
            .matches(&[File {
                filename: "README.md".to_string()
            }])
            .unwrap());
        assert!(rule
            .matches(&[File {
                filename: "path/file.txt".to_string()
            }])
            .unwrap());
    }

    #[test]
    fn automation_rule_does_not_match() {
        let rule = AutomationRule {
            patterns: vec!["path/image.svg".to_string()],
            profile: "default".to_string(),
        };
        assert!(!rule
            .matches(&[File {
                filename: "README.md".to_string()
            }])
            .unwrap());
        assert!(!rule
            .matches(&[File {
                filename: "image.svg".to_string()
            }])
            .unwrap());
    }

    #[tokio::test]
    async fn get_cfg_profile_config_not_found() {
        let mut gh = MockGH::new();
        gh.expect_get_config_file()
            .with(eq(INST_ID), eq(OWNER), eq(REPO))
            .returning(|_, _, _| Box::pin(future::ready(None)));
        let gh = Arc::new(gh);

        assert_eq!(
            CfgProfile::get(gh, INST_ID, OWNER, OWNER_IS_ORG, REPO, None)
                .await
                .unwrap_err(),
            CfgError::ConfigNotFound
        );
    }

    #[tokio::test]
    async fn get_cfg_profile_invalid_config_invalid_yaml() {
        let mut gh = MockGH::new();
        gh.expect_get_config_file()
            .with(eq(INST_ID), eq(OWNER), eq(REPO))
            .returning(|_, _, _| Box::pin(future::ready(Some(get_test_invalid_config()))));
        let gh = Arc::new(gh);

        assert!(matches!(
            CfgProfile::get(
                gh,
                INST_ID,
                OWNER,
                OWNER_IS_ORG,
                REPO,
                Some(PROFILE_NAME.to_string())
            )
            .await
            .unwrap_err(),
            CfgError::InvalidConfig(_)
        ));
    }

    #[tokio::test]
    async fn get_cfg_profile_invalid_config_teams_owner_not_org() {
        let mut gh = MockGH::new();
        gh.expect_get_config_file()
            .with(eq(INST_ID), eq(OWNER), eq(REPO))
            .returning(|_, _, _| Box::pin(future::ready(Some(get_test_valid_config()))));
        let gh = Arc::new(gh);

        assert_eq!(
            CfgProfile::get(
                gh,
                INST_ID,
                OWNER,
                !OWNER_IS_ORG,
                REPO,
                Some(PROFILE_NAME.to_string())
            )
            .await
            .unwrap_err(),
            CfgError::InvalidConfig(ERR_TEAMS_NOT_ALLOWED.to_string())
        );
    }

    #[tokio::test]
    async fn get_cfg_profile_profile_not_found() {
        let mut gh = MockGH::new();
        gh.expect_get_config_file()
            .with(eq(INST_ID), eq(OWNER), eq(REPO))
            .returning(|_, _, _| Box::pin(future::ready(Some(get_test_valid_config()))));
        let gh = Arc::new(gh);

        assert_eq!(
            CfgProfile::get(
                gh,
                INST_ID,
                OWNER,
                OWNER_IS_ORG,
                REPO,
                Some("profile9".to_string())
            )
            .await
            .unwrap_err(),
            CfgError::ProfileNotFound
        );
    }

    #[tokio::test]
    async fn get_cfg_profile_default() {
        let mut gh = MockGH::new();
        gh.expect_get_config_file()
            .with(eq(INST_ID), eq(OWNER), eq(REPO))
            .returning(|_, _, _| Box::pin(future::ready(Some(get_test_valid_config()))));
        let gh = Arc::new(gh);

        assert_eq!(
            CfgProfile::get(gh, INST_ID, OWNER, OWNER_IS_ORG, REPO, None)
                .await
                .unwrap(),
            CfgProfile {
                duration: Duration::from_secs(300),
                pass_threshold: 50.0,
                allowed_voters: Some(AllowedVoters::default()),
                ..Default::default()
            }
        );
    }

    #[tokio::test]
    async fn get_cfg_profile_profile1() {
        let mut gh = MockGH::new();
        gh.expect_get_config_file()
            .with(eq(INST_ID), eq(OWNER), eq(REPO))
            .returning(|_, _, _| Box::pin(future::ready(Some(get_test_valid_config()))));
        let gh = Arc::new(gh);

        assert_eq!(
            CfgProfile::get(
                gh,
                INST_ID,
                OWNER,
                OWNER_IS_ORG,
                REPO,
                Some(PROFILE_NAME.to_string())
            )
            .await
            .unwrap(),
            CfgProfile {
                duration: Duration::from_secs(600),
                pass_threshold: 75.0,
                allowed_voters: Some(AllowedVoters {
                    teams: Some(vec![TEAM1.to_string()]),
                    users: Some(vec![USER1.to_string(), USER2.to_string()]),
                    ..Default::default()
                }),
                ..Default::default()
            }
        );
    }
}
