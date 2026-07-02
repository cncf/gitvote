//! This module defines the templates used for the GitHub comments.

use std::collections::BTreeMap;

use askama::Template;
use serde::Serialize;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{
    cfg_repo::CfgProfile,
    cmd::CreateVoteInput,
    github::{TeamSlug, UserName},
    results::{Vote, VoteOption, VoteResults},
};

/// Date since when votes participation is tracked.
const PARTICIPATION_TRACKING_START: &str = "2024-01-01T00:00:00Z";

/// Template for the audit page.
#[allow(dead_code)]
#[derive(Debug, Clone, Template)]
#[template(path = "audit.html")]
pub(crate) struct Audit {
    pub participation_stats: AuditParticipationStats,
    pub repository_full_name: String,
    pub votes: Vec<Vote>,
}

/// Template for the audit vote details fragment.
#[derive(Debug, Clone, Template)]
#[template(path = "audit-vote-details.html")]
pub(crate) struct AuditVoteDetails<'a> {
    pub results: &'a VoteResults,
    pub vote: &'a Vote,
}

impl Audit {
    /// Create a new `Audit` template.
    pub(crate) fn new(repository_full_name: String, votes: Vec<Vote>) -> Self {
        let participation_stats = Self::calculate_participation_stats(&votes);

        Self {
            participation_stats,
            repository_full_name,
            votes,
        }
    }

    /// Calculate the participation statistics from the provided votes.
    fn calculate_participation_stats(votes: &[Vote]) -> AuditParticipationStats {
        let tracking_start = OffsetDateTime::parse(PARTICIPATION_TRACKING_START, &Rfc3339).unwrap();

        // Aggregate stats per year and user based on votes provided
        let mut stats: AuditParticipationStats = BTreeMap::new();
        for vote in votes {
            // Ignore votes created before the tracking start date
            if vote.created_at < tracking_start {
                continue;
            }

            // Ignore votes without results
            let Some(results) = &vote.results else {
                continue;
            };

            // Get or create the year stats entry
            let year = vote.created_at.year();
            let stats_year = stats.entry(year).or_default();

            // Update stats based on votes
            for (user, user_vote) in &results.votes {
                // Ignore non-binding votes
                if !user_vote.binding {
                    continue;
                }

                // Get or create the year-user stats entry
                let stats_year_user = stats_year.entry(user.clone()).or_default();

                // Update stats based on the vote option
                match user_vote.vote_option {
                    VoteOption::InFavor => stats_year_user.votes_in_favor += 1,
                    VoteOption::Against => stats_year_user.votes_against += 1,
                    VoteOption::Abstain => stats_year_user.votes_abstain += 1,
                }
            }

            // Update stats not voted count based on pending voters
            for user in &results.pending_voters {
                stats_year.entry(user.clone()).or_default().not_voted += 1;
            }
        }

        // Calculate and set participation percentage for each voter
        for voters in stats.values_mut() {
            for voter in voters.values_mut() {
                let voted_count = voter.votes_abstain + voter.votes_against + voter.votes_in_favor;
                let total = voted_count + voter.not_voted;

                #[allow(clippy::cast_precision_loss)]
                {
                    voter.participation_percentage = if total == 0 {
                        0.0
                    } else {
                        (voted_count as f64 / total as f64) * 100.0
                    };
                }
            }
        }

        stats
    }
}

/// Nested map with participation statistics per year and user.
pub(crate) type AuditParticipationStats = BTreeMap<i32, BTreeMap<UserName, AuditParticipationStatsUser>>;

/// User participation statistics.
#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct AuditParticipationStatsUser {
    pub not_voted: i64,
    pub participation_percentage: f64,
    pub votes_abstain: i64,
    pub votes_against: i64,
    pub votes_in_favor: i64,
}

/// Template for the config not found comment.
#[derive(Debug, Clone, Template)]
#[template(path = "config-not-found.md")]
pub(crate) struct ConfigNotFound {}

/// Template for the config profile not found comment.
#[derive(Debug, Clone, Template)]
#[template(path = "config-profile-not-found.md")]
pub(crate) struct ConfigProfileNotFound {}

/// Template for the index document.
#[derive(Debug, Clone, Template)]
#[template(path = "index.html")]
pub(crate) struct Index {}

/// Template for the invalid config comment.
#[derive(Debug, Clone, Template)]
#[template(path = "invalid-config.md")]
pub(crate) struct InvalidConfig<'a> {
    reason: &'a str,
}

impl<'a> InvalidConfig<'a> {
    /// Create a new `InvalidConfig` template.
    pub(crate) fn new(reason: &'a str) -> Self {
        Self { reason }
    }
}

/// Template for the no vote in progress comment.
#[derive(Debug, Clone, Template)]
#[template(path = "no-vote-in-progress.md")]
pub(crate) struct NoVoteInProgress<'a> {
    user: &'a str,
    is_pull_request: bool,
}

impl<'a> NoVoteInProgress<'a> {
    /// Create a new `NoVoteInProgress` template.
    pub(crate) fn new(user: &'a str, is_pull_request: bool) -> Self {
        Self {
            user,
            is_pull_request,
        }
    }
}

/// Template for the vote cancelled comment.
#[derive(Debug, Clone, Template)]
#[template(path = "vote-cancelled.md")]
pub(crate) struct VoteCancelled<'a> {
    user: &'a str,
    is_pull_request: bool,
}

impl<'a> VoteCancelled<'a> {
    /// Create a new `VoteCancelled` template.
    pub(crate) fn new(user: &'a str, is_pull_request: bool) -> Self {
        Self {
            user,
            is_pull_request,
        }
    }
}

/// Template for the vote checked recently comment.
#[derive(Debug, Clone, Template)]
#[template(path = "vote-checked-recently.md")]
pub(crate) struct VoteCheckedRecently {}

/// Template for the vote closed comment.
#[derive(Debug, Clone, Template)]
#[template(path = "vote-closed.md")]
pub(crate) struct VoteClosed<'a> {
    results: &'a VoteResults,
}

impl<'a> VoteClosed<'a> {
    /// Create a new `VoteClosed` template.
    pub(crate) fn new(results: &'a VoteResults) -> Self {
        Self { results }
    }
}

/// Template for the vote closed announcement.
#[derive(Debug, Clone, Template)]
#[template(path = "vote-closed-announcement.md")]
pub(crate) struct VoteClosedAnnouncement<'a> {
    issue_number: i64,
    issue_title: &'a str,
    results: &'a VoteResults,
}

impl<'a> VoteClosedAnnouncement<'a> {
    /// Create a new `VoteClosedAnnouncement` template.
    pub(crate) fn new(issue_number: i64, issue_title: &'a str, results: &'a VoteResults) -> Self {
        Self {
            issue_number,
            issue_title,
            results,
        }
    }
}

/// Template for the vote created comment.
#[derive(Debug, Clone, Template)]
#[template(path = "vote-created.md")]
pub(crate) struct VoteCreated<'a> {
    creator: &'a str,
    issue_title: &'a str,
    issue_number: i64,
    duration: String,
    pass_threshold: f64,
    org: &'a str,
    teams: &'a [TeamSlug],
    users: &'a [UserName],
}

impl<'a> VoteCreated<'a> {
    /// Create a new `VoteCreated` template.
    pub(crate) fn new(input: &'a CreateVoteInput, cfg: &'a CfgProfile) -> Self {
        // Prepare teams and users allowed to vote
        let (mut teams, mut users): (&[TeamSlug], &[UserName]) = (&[], &[]);
        if let Some(allowed_voters) = &cfg.allowed_voters {
            if let Some(v) = &allowed_voters.teams {
                teams = v.as_slice();
            }
            if let Some(v) = &allowed_voters.users {
                users = v.as_slice();
            }
        }

        // Get organization name if available
        let org = match &input.organization {
            Some(org) => org.as_ref(),
            None => "",
        };

        Self {
            creator: &input.created_by,
            issue_title: &input.issue_title,
            issue_number: input.issue_number,
            duration: humantime::format_duration(cfg.duration).to_string(),
            pass_threshold: cfg.pass_threshold,
            org,
            teams,
            users,
        }
    }
}

/// Template for the vote in progress comment.
#[derive(Debug, Clone, Template)]
#[template(path = "vote-in-progress.md")]
pub(crate) struct VoteInProgress<'a> {
    user: &'a str,
    is_pull_request: bool,
}

impl<'a> VoteInProgress<'a> {
    /// Create a new `VoteInProgress` template.
    pub(crate) fn new(user: &'a str, is_pull_request: bool) -> Self {
        Self {
            user,
            is_pull_request,
        }
    }
}

/// Template for the vote restricted comment.
#[derive(Debug, Clone, Template)]
#[template(path = "vote-restricted.md")]
pub(crate) struct VoteRestricted<'a> {
    user: &'a str,
}

impl<'a> VoteRestricted<'a> {
    /// Create a new `VoteRestricted` template.
    pub(crate) fn new(user: &'a str) -> Self {
        Self { user }
    }
}

/// Template for the vote status comment.
#[derive(Debug, Clone, Template)]
#[template(path = "vote-status.md")]
pub(crate) struct VoteStatus<'a> {
    results: &'a VoteResults,
}

impl<'a> VoteStatus<'a> {
    /// Create a new `VoteStatus` template.
    pub(crate) fn new(results: &'a VoteResults) -> Self {
        Self { results }
    }
}

#[allow(
    clippy::inline_always,
    clippy::trivially_copy_pass_by_ref,
    clippy::unnecessary_wraps
)]
mod filters {
    use std::collections::BTreeMap;

    use crate::{github::UserName, results::UserVote};

    /// Template filter that returns up to the requested number of non-binding
    /// votes from the votes collection provided sorted by timestamp (oldest
    /// first).
    #[askama::filter_fn]
    pub(crate) fn non_binding(
        votes: &BTreeMap<UserName, UserVote>,
        _: &dyn askama::Values,
        max: &i64,
    ) -> askama::Result<Vec<(UserName, UserVote)>> {
        let mut non_binding_votes: Vec<(UserName, UserVote)> =
            votes.iter().filter(|(_, v)| !v.binding).map(|(n, v)| (n.clone(), v.clone())).collect();
        non_binding_votes.sort_by_key(|a| a.1.timestamp);
        #[allow(clippy::cast_possible_truncation)]
        Ok(non_binding_votes.into_iter().take(*max as usize).collect())
    }
}

#[cfg(test)]
mod tests;
