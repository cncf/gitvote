//! This module defines some types and functionality to represent and process
//! the `GitVote` configuration that GitHub repositories can use to enable and
//! customize the service.

use std::{collections::HashMap, time::Duration};

use anyhow::{Result, bail, format_err};
use ignore::gitignore::GitignoreBuilder;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::github::{DynGH, File, TeamSlug, UserName};

#[cfg(test)]
mod tests;

/// Default configuration profile.
const DEFAULT_PROFILE: &str = "default";

/// Error message used when the close on passing minimum wait is not a valid period.
const ERR_INVALID_CLOSE_ON_PASSING_MIN_WAIT: &str = "close_on_passing_min_wait must be at least 1 second \
     and not greater than 36500 days, in the same format as duration";

/// Error message used when the duration is outside the range allowed.
const ERR_INVALID_DURATION: &str = "duration must be at least 1 second and not greater than 36500 days";

/// Error message used when the pass threshold is not a valid percentage.
const ERR_INVALID_PASS_THRESHOLD: &str = "pass threshold must be greater than 0 and not greater than 100";

/// Error message used when the periodic status check is not a valid period.
const ERR_INVALID_PERIODIC_STATUS_CHECK: &str = "periodic_status_check must be at least 1 day and not \
     greater than 36500 days, in the same format as duration";

/// Error message used when teams are listed in the allowed voters section on a
/// repository that does not belong to an organization.
const ERR_TEAMS_NOT_ALLOWED: &str = "teams in allowed voters can only be used in organizations";

/// Maximum duration of a vote, which is the longest period allowed as well. It is far
/// beyond any useful value, and it keeps them well within the range of a PostgreSQL
/// interval. Comparing against it needs no arithmetic on the value configured, so any
/// duration that fits in the type is safely handled.
const MAX_DURATION: Duration = Duration::from_hours(36_500 * 24);

/// Minimum close on passing wait. Periods are stored as a whole number of seconds, so a
/// shorter one would be stored as no time at all, which never lets the vote close early.
const MIN_CLOSE_ON_PASSING_MIN_WAIT: Duration = Duration::from_secs(1);

/// Minimum duration of a vote. Votes end a whole number of seconds after being created,
/// so a shorter duration would end the vote as soon as it is created.
const MIN_DURATION: Duration = Duration::from_secs(1);

/// Minimum period between status checks. A vote is checked once a day at most, so a
/// shorter period would never run a status check.
const MIN_PERIODIC_STATUS_CHECK: Duration = Duration::from_hours(24);

/// `GitVote` configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct Cfg {
    /// Vote configuration profiles, by name.
    pub profiles: HashMap<ProfileName, CfgProfile>,

    /// Audit configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audit: Option<Audit>,
    /// Automation configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub automation: Option<Automation>,
}

impl Cfg {
    /// Gets the `GitVote` configuration for the repository provided.
    pub(crate) async fn get<'a>(
        gh: DynGH,
        inst_id: u64,
        owner: &'a str,
        repo: &'a str,
    ) -> Result<Self, CfgError> {
        match gh.get_config_file(inst_id, owner, repo).await.map_err(CfgError::ConfigLookup)? {
            Some(content) => {
                let cfg: Cfg =
                    serde_yaml::from_str(&content).map_err(|err| CfgError::InvalidConfig(err.to_string()))?;
                Ok(cfg)
            }
            None => Err(CfgError::ConfigNotFound),
        }
    }
}

/// Represents the teams and users allowed to vote.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct AllowedVoters {
    /// Whether the maintainers of the teams listed are excluded from the voters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude_team_maintainers: Option<bool>,
    /// Teams whose members are allowed to vote.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub teams: Option<Vec<TeamSlug>>,
    /// Users allowed to vote.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub users: Option<Vec<UserName>>,
}

/// Announcements configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct Announcements {
    /// GitHub discussions announcements configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discussions: Option<DiscussionsAnnouncements>,
}

/// Audit configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct Audit {
    /// Whether the audit page of the repository is enabled.
    pub enabled: bool,
}

/// Automation configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct Automation {
    /// Whether votes are created automatically on pull requests.
    pub enabled: bool,
    /// Rules evaluated to decide if a vote is created, in order.
    pub rules: Vec<AutomationRule>,
}

/// Automation rule.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct AutomationRule {
    /// Patterns, in the gitignore format, the pull request files are matched against.
    pub patterns: Vec<String>,
    /// Name of the profile used to create the vote when the rule matches.
    pub profile: ProfileName,
}

impl AutomationRule {
    /// Checks if any of the files provided matches any of the rule patterns.
    /// Patterns must follow the gitignore format, so a pattern matching a directory
    /// also matches all the files below it.
    pub(crate) fn matches(&self, files: &[File]) -> Result<bool> {
        // Build a gitignore matcher from the rule patterns
        let mut builder = GitignoreBuilder::new("/");
        for pattern in &self.patterns {
            builder.add_line(None, pattern)?;
        }
        let checker = builder.build()?;

        // Check if any of the files, or any of its parent directories, matches
        let matches = files
            .iter()
            .any(|file| checker.matched_path_or_any_parents(&file.filename, false).is_ignore());
        Ok(matches)
    }
}

/// Errors that may occur while getting the configuration profile.
#[derive(Debug, Error)]
pub(crate) enum CfgError {
    /// The config file could not be looked up in the repository.
    #[error("error looking up config file")]
    ConfigLookup(#[source] anyhow::Error),
    /// The repository has no config file.
    #[error("config not found")]
    ConfigNotFound,
    /// The config file is not valid.
    #[error("invalid config: {0}")]
    InvalidConfig(String),
    /// The profile requested is not defined in the config file.
    #[error("profile not found")]
    ProfileNotFound,
}

/// Vote configuration profile.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct CfgProfile {
    /// Time the vote stays open.
    #[serde(with = "humantime_serde")]
    pub duration: Duration,
    /// Percentage of votes in favor required for the vote to pass.
    pub pass_threshold: f64,

    /// Teams and users allowed to vote.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_voters: Option<AllowedVoters>,
    /// Announcements configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub announcements: Option<Announcements>,
    /// Whether the vote is closed as soon as it passes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub close_on_passing: Option<bool>,
    /// Minimum period the vote stays open before being closed on passing. In the config
    /// file it uses the format of the duration, and it is a number of seconds (i.e.
    /// `172800 seconds`) in the profiles used to create votes. Stored votes may hold
    /// any text PostgreSQL reads as an interval, so it must not be parsed when read
    /// from them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub close_on_passing_min_wait: Option<String>,
    /// Base used to calculate the pass threshold percentage.
    // The singleton map representation allows using the `votes_cast:` map syntax
    // in YAML, instead of the `!votes_cast` tag expected by default
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "serde_yaml::with::singleton_map"
    )]
    pub pass_threshold_base: Option<PassThresholdBase>,
    /// Period between the status checks run automatically while the vote is open. It is
    /// represented the same way as the close on passing minimum wait.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub periodic_status_check: Option<String>,
}

impl CfgProfile {
    /// Gets the vote configuration profile requested from the config file in
    /// the repository if available. The profile returned is valid and ready to be
    /// stored in a vote, with its periods written as a number of seconds.
    pub(crate) async fn get<'a>(
        gh: DynGH,
        inst_id: u64,
        owner: &'a str,
        is_org: bool,
        repo: &'a str,
        profile_name: Option<String>,
    ) -> Result<Self, CfgError> {
        // Get the repository configuration
        let mut cfg = Cfg::get(gh, inst_id, owner, repo).await?;

        // Pick the profile requested, or the default one
        let profile_name = profile_name.unwrap_or_else(|| DEFAULT_PROFILE.to_string());
        let Some(mut profile) = cfg.profiles.remove(&profile_name) else {
            return Err(CfgError::ProfileNotFound);
        };

        // Validate the profile and prepare its periods to be stored
        profile
            .validate(is_org)
            .and_then(|()| profile.normalize_periods())
            .map_err(|err| CfgError::InvalidConfig(err.to_string()))?;

        Ok(profile)
    }

    /// Rewrites the periods as a number of seconds. PostgreSQL reads the periods stored
    /// in the votes as intervals, and it does not read the format used in the config
    /// file the same way (i.e. `1M` is a month in the config file and a minute in an
    /// interval). A number of seconds is read as exactly that.
    fn normalize_periods(&mut self) -> Result<()> {
        // Rewrite each of the periods set as its number of whole seconds
        let periods = [
            &mut self.close_on_passing_min_wait,
            &mut self.periodic_status_check,
        ];
        for period in periods.into_iter().flatten() {
            *period = format!("{} seconds", humantime::parse_duration(period)?.as_secs());
        }

        Ok(())
    }

    /// Checks if the configuration profile is valid.
    fn validate(&self, is_org: bool) -> Result<()> {
        // A vote must stay open for some time. The end of the vote is calculated by
        // PostgreSQL from the duration, which can be out of its range when too large
        if !(MIN_DURATION..=MAX_DURATION).contains(&self.duration) {
            bail!(ERR_INVALID_DURATION);
        }

        // The pass threshold must be a percentage in the (0, 100] range. Written
        // as a negated range check so that NaN values are rejected as well
        if !(self.pass_threshold > 0.0 && self.pass_threshold <= 100.0) {
            bail!(ERR_INVALID_PASS_THRESHOLD);
        }

        // The periods use the same format as the duration, each one with its own minimum
        if let Some(period) = &self.close_on_passing_min_wait {
            validate_period(
                period,
                MIN_CLOSE_ON_PASSING_MIN_WAIT,
                ERR_INVALID_CLOSE_ON_PASSING_MIN_WAIT,
            )?;
        }
        if let Some(period) = &self.periodic_status_check {
            validate_period(
                period,
                MIN_PERIODIC_STATUS_CHECK,
                ERR_INVALID_PERIODIC_STATUS_CHECK,
            )?;
        }

        // Only repositories that belong to some organization can use teams in
        // the allowed voters configuration section
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

/// GitHub discussions announcements configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct DiscussionsAnnouncements {
    /// Slug of the discussions category where the results are announced.
    pub category: String,
}

/// Base used to calculate the pass threshold percentage.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PassThresholdBase {
    /// All users with binding votes.
    #[default]
    AllowedVoters,
    /// Binding votes cast.
    VotesCast {
        /// Whether abstentions are left out of the votes cast.
        #[serde(skip_serializing_if = "Option::is_none")]
        exclude_abstentions: Option<bool>,
    },
}

/// Type alias to represent a profile name.
type ProfileName = String;

/// Checks if the value provided is a period in the format of the duration that is not
/// shorter than the minimum given nor longer than the maximum duration. The error
/// returned starts with the message provided, followed by the reason when the value
/// cannot be parsed.
fn validate_period(value: &str, min: Duration, error: &str) -> Result<()> {
    // Parse the period, which uses the same format as the duration
    let period = humantime::parse_duration(value).map_err(|err| format_err!("{error}: {err}"))?;

    // Check the period is within the range allowed
    if !(min..=MAX_DURATION).contains(&period) {
        bail!("{error}");
    }

    Ok(())
}
