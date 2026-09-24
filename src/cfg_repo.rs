//! This module defines some types and functionality to represent and process
//! the `GitVote` configuration that GitHub repositories can use to enable and
//! customize the service.

use std::{collections::HashMap, time::Duration};

use anyhow::{Result, bail};
use ignore::gitignore::GitignoreBuilder;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::github::{DynGH, File, TeamSlug, UserName};

/// Default configuration profile.
const DEFAULT_PROFILE: &str = "default";

/// Error message used when the pass threshold is not a valid percentage.
const ERR_INVALID_PASS_THRESHOLD: &str = "pass threshold must be greater than 0 and not greater than 100";

/// Error message used when teams are listed in the allowed voters section on a
/// repository that does not belong to an organization.
const ERR_TEAMS_NOT_ALLOWED: &str = "teams in allowed voters can only be used in organizations";

/// Type alias to represent a profile name.
type ProfileName = String;

/// `GitVote` configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct Cfg {
    pub profiles: HashMap<ProfileName, CfgProfile>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub audit: Option<Audit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub automation: Option<Automation>,
}

impl Cfg {
    /// Get the `GitVote` configuration for the repository provided.
    pub(crate) async fn get<'a>(
        gh: DynGH,
        inst_id: u64,
        owner: &'a str,
        repo: &'a str,
    ) -> Result<Self, CfgError> {
        match gh.get_config_file(inst_id, owner, repo).await {
            Some(content) => {
                let cfg: Cfg =
                    serde_yaml::from_str(&content).map_err(|e| CfgError::InvalidConfig(e.to_string()))?;
                Ok(cfg)
            }
            None => Err(CfgError::ConfigNotFound),
        }
    }
}

/// Audit configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct Audit {
    pub enabled: bool,
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
        let matches = files.iter().any(|file| checker.matched(&file.filename, false).is_ignore());
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
    pub announcements: Option<Announcements>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub periodic_status_check: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub close_on_passing: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub close_on_passing_min_wait: Option<String>,
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
        // The pass threshold must be a percentage in the (0, 100] range. Written
        // as a negated range check so that NaN values are rejected as well.
        if !(self.pass_threshold > 0.0 && self.pass_threshold <= 100.0) {
            bail!(ERR_INVALID_PASS_THRESHOLD);
        }

        // Only repositories that belong to some organization can use teams in
        // the allowed voters configuration section.
        if !is_org
            && let Some(teams) =
                self.allowed_voters.as_ref().and_then(|allowed_voters| allowed_voters.teams.as_ref())
            && !teams.is_empty()
        {
            bail!(ERR_TEAMS_NOT_ALLOWED);
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

/// Announcements configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct Announcements {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discussions: Option<DiscussionsAnnouncements>,
}

/// GitHub discussions announcements configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct DiscussionsAnnouncements {
    pub category: String,
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
mod tests;
