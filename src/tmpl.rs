//! This module defines the templates used for the GitHub comments and the web pages.

use std::collections::{BTreeMap, HashMap};

use askama::Template;
use serde::Serialize;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

use crate::{
    cfg_repo::{CfgProfile, PassThresholdBase},
    cmd::CreateVoteInput,
    github::{TeamSlug, UserName},
    results::{Vote, VoteOption, VoteResults},
};

#[cfg(test)]
mod tests;

/// Date since when votes participation is tracked.
const PARTICIPATION_TRACKING_START: &str = "2024-01-01T00:00:00Z";

/// Template for the audit page.
#[allow(dead_code)]
#[derive(Debug, Clone, Template)]
#[template(path = "audit.html")]
pub(crate) struct Audit {
    /// Participation statistics of the binding voters per year and user.
    participation_stats: AuditParticipationStats,
    /// Full name of the audited repository.
    repository_full_name: String,
    /// Votes of the repository.
    votes: Vec<Vote>,
}

impl Audit {
    /// Creates a new `Audit` template.
    pub(crate) fn new(repository_full_name: String, votes: Vec<Vote>) -> Self {
        let participation_stats = Self::calculate_participation_stats(&votes);

        Self {
            participation_stats,
            repository_full_name,
            votes,
        }
    }

    /// Calculates the participation statistics from the provided votes.
    fn calculate_participation_stats(votes: &[Vote]) -> AuditParticipationStats {
        // Parse the date since when participation is tracked
        let tracking_start = OffsetDateTime::parse(PARTICIPATION_TRACKING_START, &Rfc3339)
            .expect("participation tracking start should be a valid rfc 3339 timestamp");

        // Merge the case variants of a username into a single spelling
        let usernames = Self::collect_usernames(votes);
        let username = |user: &UserName| -> UserName {
            usernames
                .get(&user.to_ascii_lowercase())
                .map_or_else(|| user.clone(), |user| (*user).clone())
        };

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
                let stats_year_user = stats_year.entry(username(user)).or_default();

                // Update stats based on the vote option
                match user_vote.vote_option {
                    VoteOption::Abstain => stats_year_user.votes_abstain += 1,
                    VoteOption::Against => stats_year_user.votes_against += 1,
                    VoteOption::InFavor => stats_year_user.votes_in_favor += 1,
                }
            }

            // Update stats not voted count based on pending voters
            for user in &results.pending_voters {
                stats_year.entry(username(user)).or_default().not_voted += 1;
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

    /// Collects the spelling to use for each username, keyed by its lowercase form as
    /// GitHub usernames are case insensitive. Reactions provide the GitHub login, so it
    /// is preferred over the spelling of pending voters, which may come from the
    /// configuration.
    fn collect_usernames(votes: &[Vote]) -> HashMap<String, &UserName> {
        let mut usernames = HashMap::new();
        let results = || votes.iter().filter_map(|vote| vote.results.as_ref());

        // Take the spelling of the users who reacted to the vote comments first
        for user in results().flat_map(|results| results.votes.keys()) {
            usernames.entry(user.to_ascii_lowercase()).or_insert(user);
        }

        // Fall back to the spelling of the pending voters
        for user in results().flat_map(|results| &results.pending_voters) {
            usernames.entry(user.to_ascii_lowercase()).or_insert(user);
        }

        usernames
    }
}

/// Nested map with participation statistics per year and user.
type AuditParticipationStats = BTreeMap<i32, BTreeMap<UserName, AuditParticipationStatsUser>>;

/// User participation statistics.
#[derive(Debug, Clone, Default, Serialize)]
struct AuditParticipationStatsUser {
    /// Number of votes the user was allowed to vote on but did not.
    not_voted: i64,
    /// Percentage of the votes the user was allowed to vote on where they voted.
    participation_percentage: f64,
    /// Number of binding abstain votes cast by the user.
    votes_abstain: i64,
    /// Number of binding against votes cast by the user.
    votes_against: i64,
    /// Number of binding in favor votes cast by the user.
    votes_in_favor: i64,
}

/// Template for the audit vote details fragment.
#[derive(Debug, Clone, Template)]
#[template(path = "audit-vote-details.html")]
pub(crate) struct AuditVoteDetails<'a> {
    /// Vote whose details are rendered.
    pub vote: &'a Vote,

    /// Vote results, missing if the vote comment was deleted before the vote closed.
    pub results: Option<&'a VoteResults>,
}

/// Template for the automation failed comment.
#[derive(Debug, Clone, Template)]
#[template(path = "automation-failed.md")]
pub(crate) struct AutomationFailed<'a> {
    /// Description of the failure, in case there is one for the users.
    details: Option<&'a str>,
}

impl<'a> AutomationFailed<'a> {
    /// Creates a new `AutomationFailed` template.
    pub(crate) fn new(details: Option<&'a str>) -> Self {
        Self { details }
    }
}

/// Template for the command failed comment.
#[derive(Debug, Clone, Template)]
#[template(path = "command-failed.md")]
pub(crate) struct CommandFailed {}

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
    /// Reason why the configuration is not valid.
    reason: &'a str,
}

impl<'a> InvalidConfig<'a> {
    /// Creates a new `InvalidConfig` template.
    pub(crate) fn new(reason: &'a str) -> Self {
        Self { reason }
    }
}

/// Template for the no vote in progress comment.
#[derive(Debug, Clone, Template)]
#[template(path = "no-vote-in-progress.md")]
pub(crate) struct NoVoteInProgress<'a> {
    /// Whether the comment is on a pull request rather than on an issue.
    is_pull_request: bool,
    /// User who tried to cancel the vote.
    user: &'a str,
}

impl<'a> NoVoteInProgress<'a> {
    /// Creates a new `NoVoteInProgress` template.
    pub(crate) fn new(user: &'a str, is_pull_request: bool) -> Self {
        Self {
            is_pull_request,
            user,
        }
    }
}

/// Template for the vote cancelled comment.
#[derive(Debug, Clone, Template)]
#[template(path = "vote-cancelled.md")]
pub(crate) struct VoteCancelled<'a> {
    /// Whether the vote is on a pull request rather than on an issue.
    is_pull_request: bool,
    /// Hidden marker identifying the cancellation of the vote.
    marker: String,
    /// User who cancelled the vote.
    user: &'a str,
}

impl<'a> VoteCancelled<'a> {
    /// Creates a new `VoteCancelled` template for the vote provided.
    pub(crate) fn new(vote_id: Uuid, user: &'a str, is_pull_request: bool) -> Self {
        Self {
            is_pull_request,
            marker: VoteMarker::CancellationComment.render(vote_id),
            user,
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
    /// Hidden marker identifying the results comment of the vote.
    marker: String,
    /// Results of the vote.
    results: &'a VoteResults,
}

impl<'a> VoteClosed<'a> {
    /// Creates a new `VoteClosed` template for the vote provided.
    pub(crate) fn new(vote_id: Uuid, results: &'a VoteResults) -> Self {
        Self {
            marker: VoteMarker::ResultsComment.render(vote_id),
            results,
        }
    }
}

/// Template for the vote closed announcement.
#[derive(Debug, Clone, Template)]
#[template(path = "vote-closed-announcement.md")]
pub(crate) struct VoteClosedAnnouncement<'a> {
    /// Number of the issue/pr of the vote.
    issue_number: i64,
    /// Title of the issue/pr of the vote.
    issue_title: &'a str,
    /// Hidden marker identifying the results announcement of the vote.
    marker: String,
    /// Results of the vote.
    results: &'a VoteResults,
}

impl<'a> VoteClosedAnnouncement<'a> {
    /// Creates a new `VoteClosedAnnouncement` template for the vote provided.
    pub(crate) fn new(
        vote_id: Uuid,
        issue_number: i64,
        issue_title: &'a str,
        results: &'a VoteResults,
    ) -> Self {
        Self {
            issue_number,
            issue_title,
            marker: VoteMarker::ResultsAnnouncement.render(vote_id),
            results,
        }
    }
}

/// Template for the vote created comment.
#[derive(Debug, Clone, Template)]
#[template(path = "vote-created.md")]
pub(crate) struct VoteCreated<'a> {
    /// User who created the vote.
    creator: &'a str,
    /// Human readable duration of the vote.
    duration: String,
    /// Whether abstentions are left out of the votes cast pass threshold base.
    exclude_abstentions: bool,
    /// Number of the issue/pr of the vote.
    issue_number: i64,
    /// Title of the issue/pr of the vote.
    issue_title: &'a str,
    /// Organization owning the repository, empty if it is owned by a user.
    org: &'a str,
    /// Percentage of in favor votes required for the vote to pass.
    pass_threshold: f64,
    /// Whether the pass threshold applies to the votes cast rather than to the
    /// allowed voters.
    pass_threshold_base_votes_cast: bool,
    /// Teams whose members have binding votes.
    teams: &'a [TeamSlug],
    /// Users who have binding votes.
    users: &'a [UserName],
}

impl<'a> VoteCreated<'a> {
    /// Creates a new `VoteCreated` template.
    pub(crate) fn new(input: &'a CreateVoteInput, cfg: &'a CfgProfile) -> Self {
        // Prepare teams and users allowed to vote
        let (mut teams, mut users): (&[TeamSlug], &[UserName]) = (&[], &[]);
        if let Some(allowed_voters) = &cfg.allowed_voters {
            if let Some(allowed_teams) = &allowed_voters.teams {
                teams = allowed_teams.as_slice();
            }
            if let Some(allowed_users) = &allowed_voters.users {
                users = allowed_users.as_slice();
            }
        }

        // Get organization name if available
        let org = match &input.organization {
            Some(org) => org.as_ref(),
            None => "",
        };

        // Prepare pass threshold base details
        let (pass_threshold_base_votes_cast, exclude_abstentions) =
            match cfg.pass_threshold_base.clone().unwrap_or_default() {
                PassThresholdBase::AllowedVoters => (false, false),
                PassThresholdBase::VotesCast { exclude_abstentions } => {
                    (true, exclude_abstentions.unwrap_or(false))
                }
            };

        Self {
            creator: &input.created_by,
            duration: humantime::format_duration(cfg.duration).to_string(),
            exclude_abstentions,
            issue_number: input.issue_number,
            issue_title: &input.issue_title,
            org,
            pass_threshold: cfg.pass_threshold,
            pass_threshold_base_votes_cast,
            teams,
            users,
        }
    }
}

/// Template for the vote in progress comment.
#[derive(Debug, Clone, Template)]
#[template(path = "vote-in-progress.md")]
pub(crate) struct VoteInProgress<'a> {
    /// Whether the vote is on a pull request rather than on an issue.
    is_pull_request: bool,
    /// User who tried to create a new vote.
    user: &'a str,
}

impl<'a> VoteInProgress<'a> {
    /// Creates a new `VoteInProgress` template.
    pub(crate) fn new(user: &'a str, is_pull_request: bool) -> Self {
        Self {
            is_pull_request,
            user,
        }
    }
}

/// Hidden marker embedded in the body of the comments and discussions published when a
/// vote is closed or cancelled. It identifies the vote and the step that published it,
/// so that a retry can find what a previous attempt published before posting it again.
/// The templates must render it last, as only a body ending with it counts, so that a
/// marker in some text users control, like an issue title, is not taken for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VoteMarker {
    /// Cancellation comment.
    CancellationComment,
    /// Results announcement discussion.
    ResultsAnnouncement,
    /// Results comment.
    ResultsComment,
}

impl VoteMarker {
    /// Renders the marker of the vote provided as an HTML comment, which GitHub hides.
    pub(crate) fn render(self, vote_id: Uuid) -> String {
        let step = match self {
            VoteMarker::CancellationComment => "vote-cancelled",
            VoteMarker::ResultsAnnouncement => "vote-closed-announcement",
            VoteMarker::ResultsComment => "vote-closed",
        };
        format!("<!-- gitvote:{step}:{vote_id} -->")
    }
}

/// Template for the vote restricted comment.
#[derive(Debug, Clone, Template)]
#[template(path = "vote-restricted.md")]
pub(crate) struct VoteRestricted<'a> {
    /// User who tried to create the vote without being a repository collaborator.
    user: &'a str,
}

impl<'a> VoteRestricted<'a> {
    /// Creates a new `VoteRestricted` template.
    pub(crate) fn new(user: &'a str) -> Self {
        Self { user }
    }
}

/// Template for the vote status comment.
#[derive(Debug, Clone, Template)]
#[template(path = "vote-status.md")]
pub(crate) struct VoteStatus<'a> {
    /// Current results of the vote.
    results: &'a VoteResults,
}

impl<'a> VoteStatus<'a> {
    /// Creates a new `VoteStatus` template.
    pub(crate) fn new(results: &'a VoteResults) -> Self {
        Self { results }
    }
}

/// Custom filters used by the templates, which Askama resolves by the module name.
#[allow(
    clippy::inline_always,
    clippy::trivially_copy_pass_by_ref,
    clippy::unnecessary_wraps,
    clippy::unused_self
)]
mod filters {
    use std::collections::BTreeMap;

    use crate::{github::UserName, results::UserVote};

    /// Template filter that returns the text provided as a Markdown code span. The
    /// delimiter is longer than any run of backticks in the text, which is padded with
    /// spaces when needed, as Markdown removes them, so that the text is kept intact.
    #[askama::filter_fn]
    pub(crate) fn code_span(text: &str, _: &dyn askama::Values) -> askama::Result<String> {
        let longest_run = text.split(|character| character != '`').map(str::len).max().unwrap_or_default();
        let delimiter = "`".repeat(longest_run + 1);
        let padding = if text.starts_with('`') || text.ends_with('`') {
            " "
        } else {
            ""
        };
        Ok(format!("{delimiter}{padding}{text}{padding}{delimiter}"))
    }

    /// Template filter that returns up to the requested number of non-binding
    /// votes from the votes collection provided sorted by timestamp (oldest
    /// first).
    #[askama::filter_fn]
    pub(crate) fn non_binding(
        votes: &BTreeMap<UserName, UserVote>,
        _: &dyn askama::Values,
        max: &i64,
    ) -> askama::Result<Vec<(UserName, UserVote)>> {
        let mut non_binding_votes: Vec<(UserName, UserVote)> = votes
            .iter()
            .filter(|(_, vote)| !vote.binding)
            .map(|(user, vote)| (user.clone(), vote.clone()))
            .collect();
        non_binding_votes.sort_by_key(|(_, vote)| vote.timestamp);
        #[allow(clippy::cast_possible_truncation)]
        Ok(non_binding_votes.into_iter().take(*max as usize).collect())
    }
}
