//! This module defines the logic to calculate vote results.

use std::{collections::BTreeMap, fmt};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio_postgres::{Row, types::Json};
use uuid::Uuid;

use crate::{
    cfg_repo::{CfgProfile, PassThresholdBase},
    github::{DynGH, UserName},
};

#[cfg(test)]
mod tests;

/// Reaction used to abstain.
pub(crate) const REACTION_ABSTAIN: &str = "eyes";

/// Reaction used to vote against.
pub(crate) const REACTION_AGAINST: &str = "-1";

/// Reaction used to vote in favor.
pub(crate) const REACTION_IN_FAVOR: &str = "+1";

/// Vote information.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(clippy::struct_field_names)]
pub(crate) struct Vote {
    /// Configuration profile the vote was created with.
    pub cfg: CfgProfile,
    /// Number of failed attempts to complete the vote closing.
    pub close_attempts: i32,
    /// Whether the vote is closed and some of its closing side effects are missing.
    pub close_pending: bool,
    /// Whether the vote is closed.
    pub closed: bool,
    /// When the vote was created.
    pub created_at: OffsetDateTime,
    /// User who created the vote.
    pub created_by: String,
    /// When the vote ends. Reactions created after this time are not counted.
    pub ends_at: OffsetDateTime,
    /// GitHub application installation id of the repository.
    pub installation_id: i64,
    /// Whether the vote was created on a pull request rather than on an issue.
    pub is_pull_request: bool,
    /// Id of the issue or pull request.
    pub issue_id: i64,
    /// Number of the issue or pull request.
    pub issue_number: i64,
    /// Full name of the repository (owner/repo).
    pub repository_full_name: String,
    /// Id of the comment where votes are cast as reactions.
    pub vote_comment_id: i64,
    /// Vote identifier.
    pub vote_id: Uuid,

    /// When the vote was cancelled. A cancelled vote is closed, and it is deleted once
    /// its cancellation has been published.
    pub cancelled_at: Option<OffsetDateTime>,
    /// User who cancelled the vote.
    pub cancelled_by: Option<String>,
    /// When the status of the vote was last checked.
    pub checked_at: Option<OffsetDateTime>,
    /// When the results announcement was posted to discussions.
    pub close_announcement_posted_at: Option<OffsetDateTime>,
    /// When the closing check run was created.
    pub close_check_run_created_at: Option<OffsetDateTime>,
    /// When the closing comment was posted.
    pub close_comment_posted_at: Option<OffsetDateTime>,
    /// Head commit of the pull request when the vote was closed. Votes on issues have
    /// none, nor do votes closed when the head could not be read.
    pub close_head_sha: Option<String>,
    /// When the closing labels were updated.
    pub close_labels_updated_at: Option<OffsetDateTime>,
    /// Earliest time the next attempt to complete the vote closing can run.
    pub close_next_attempt_at: Option<OffsetDateTime>,
    /// When the vote was closed.
    pub closed_at: Option<OffsetDateTime>,
    /// Title of the issue or pull request, when known.
    pub issue_title: Option<String>,
    /// Organization the repository belongs to, if any.
    pub organization: Option<String>,
    /// Results stored when the vote was closed, if any.
    pub results: Option<VoteResults>,
}

/// Vote results as read from the database. Absent results are stored as SQL null,
/// but a previous version still running during an upgrade may write a JSON null.
type StoredVoteResults = Option<Json<Option<VoteResults>>>;

/// User's vote details.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct UserVote {
    /// Whether the vote is binding, as it was cast by an allowed voter.
    pub binding: bool,
    /// When the vote was cast.
    pub timestamp: OffsetDateTime,
    /// Option voted.
    pub vote_option: VoteOption,
}

/// Vote options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum VoteOption {
    /// Abstain from voting.
    Abstain,
    /// Vote against.
    Against,
    /// Vote in favor.
    InFavor,
}

impl VoteOption {
    /// Creates a new vote option from a reaction string.
    fn from_reaction(reaction: &str) -> Result<Self> {
        let vote_option = match reaction {
            REACTION_ABSTAIN => Self::Abstain,
            REACTION_AGAINST => Self::Against,
            REACTION_IN_FAVOR => Self::InFavor,
            _ => bail!("reaction not supported"),
        };
        Ok(vote_option)
    }
}

/// Vote results information.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct VoteResults {
    /// Number of binding votes to abstain.
    pub abstain: i64,
    /// Number of binding votes against.
    pub against: i64,
    /// Percentage of binding votes against over the pass threshold base.
    pub against_percentage: f64,
    /// Number of allowed voters (users with binding votes).
    pub allowed_voters: i64,
    /// Number of binding votes cast.
    pub binding: i64,
    /// Number of binding votes in favor.
    pub in_favor: i64,
    /// Percentage of binding votes in favor over the pass threshold base.
    pub in_favor_percentage: f64,
    /// Number of non-binding votes cast.
    pub non_binding: i64,
    /// Number of allowed voters who had not voted when the results were calculated.
    pub not_voted: i64,
    /// Percentage of binding votes in favor required for the vote to pass.
    pub pass_threshold: f64,
    /// Whether the vote passed.
    pub passed: bool,
    /// Allowed voters who had not voted when the results were calculated.
    pub pending_voters: Vec<UserName>,
    /// Votes cast, by user.
    pub votes: BTreeMap<UserName, UserVote>,
}

impl VoteResults {
    /// Checks if the vote passes when calculated over all the allowed voters, so
    /// that further votes from pending voters cannot make it fail (assuming the
    /// votes already cast and the allowed voters do not change).
    pub(crate) fn passes_with_all_allowed_voters(&self) -> bool {
        threshold_reached(self.in_favor, self.pass_threshold, self.allowed_voters)
    }
}

impl TryFrom<&Row> for Vote {
    type Error = anyhow::Error;

    /// Decodes a vote from a row of the vote table.
    fn try_from(row: &Row) -> Result<Self> {
        // Decode the columns stored as JSON
        let Json(cfg): Json<CfgProfile> = row.try_get("cfg")?;
        let results: StoredVoteResults = row.try_get("results")?;

        // Decode the remaining columns into the vote
        Ok(Self {
            cfg,
            close_attempts: row.try_get("close_attempts")?,
            close_pending: row.try_get("close_pending")?,
            closed: row.try_get("closed")?,
            created_at: row.try_get("created_at")?,
            created_by: row.try_get("created_by")?,
            ends_at: row.try_get("ends_at")?,
            installation_id: row.try_get("installation_id")?,
            is_pull_request: row.try_get("is_pull_request")?,
            issue_id: row.try_get("issue_id")?,
            issue_number: row.try_get("issue_number")?,
            repository_full_name: row.try_get("repository_full_name")?,
            vote_comment_id: row.try_get("vote_comment_id")?,
            vote_id: row.try_get("vote_id")?,

            cancelled_at: row.try_get("cancelled_at")?,
            cancelled_by: row.try_get("cancelled_by")?,
            checked_at: row.try_get("checked_at")?,
            close_announcement_posted_at: row.try_get("close_announcement_posted_at")?,
            close_check_run_created_at: row.try_get("close_check_run_created_at")?,
            close_comment_posted_at: row.try_get("close_comment_posted_at")?,
            close_head_sha: row.try_get("close_head_sha")?,
            close_labels_updated_at: row.try_get("close_labels_updated_at")?,
            close_next_attempt_at: row.try_get("close_next_attempt_at")?,
            closed_at: row.try_get("closed_at")?,
            issue_title: row.try_get("issue_title")?,
            organization: row.try_get("organization")?,
            results: results.and_then(|Json(results)| results),
        })
    }
}

impl fmt::Display for VoteOption {
    /// Formats the vote option as displayed to users.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Abstain => "Abstain",
            Self::Against => "Against",
            Self::InFavor => "In favor",
        };
        write!(f, "{s}")
    }
}

/// Calculates vote results. Reactions created after the end of the vote are ignored, as
/// closing it may be retried long after that. Removing a reaction cannot be detected, so
/// a reaction removed after the end of the vote does not count if the results are
/// calculated after that.
pub(crate) async fn calculate<'a>(
    gh: DynGH,
    owner: &'a str,
    repo: &'a str,
    vote: &'a Vote,
) -> Result<VoteResults> {
    // Get vote comment reactions (aka votes)
    let inst_id = vote.installation_id as u64;
    let reactions = gh.get_comment_reactions(inst_id, owner, repo, vote.vote_comment_id).await?;

    // Get list of allowed voters (users with binding votes)
    let allowed_voters =
        gh.get_allowed_voters(inst_id, &vote.cfg, owner, repo, vote.organization.as_ref()).await?;

    // Track users votes
    let mut votes: BTreeMap<UserName, UserVote> = BTreeMap::new();
    let mut multiple_options_voters: Vec<UserName> = Vec::new();
    for reaction in reactions {
        // Get vote option from reaction
        let username: UserName = reaction.user.login;
        let Ok(vote_option) = VoteOption::from_reaction(reaction.content.as_str()) else {
            continue;
        };

        // Ignore votes cast after the end of the vote, so that they cannot affect the
        // vote of a user who voted in time either
        let timestamp = OffsetDateTime::parse(reaction.created_at.as_str(), &Rfc3339)
            .context("invalid reaction timestamp")?;
        if timestamp > vote.ends_at {
            continue;
        }

        // Do not count votes of users voting for multiple options
        if multiple_options_voters.contains(&username) {
            continue;
        }
        if votes.contains_key(&username) {
            // User has already voted (multiple options voter), we have to
            // remove their vote as we can't know which one to pick
            multiple_options_voters.push(username.clone());
            votes.remove(&username);
            continue;
        }

        // Track vote (GitHub usernames are case insensitive)
        let binding = allowed_voters.iter().any(|voter| voter.eq_ignore_ascii_case(&username));
        votes.insert(
            username,
            UserVote {
                binding,
                timestamp,
                vote_option,
            },
        );
    }

    // Count binding votes per option and the number of binding and
    // non-binding voters (only binding votes count towards the result)
    let (mut abstain, mut against, mut binding, mut in_favor, mut non_binding) = (0, 0, 0, 0, 0);
    for user_vote in votes.values() {
        if user_vote.binding {
            match user_vote.vote_option {
                VoteOption::Abstain => abstain += 1,
                VoteOption::Against => against += 1,
                VoteOption::InFavor => in_favor += 1,
            }
            binding += 1;
        } else {
            non_binding += 1;
        }
    }

    // Select the base used to calculate the percentages and the pass threshold
    let allowed_voters_count = allowed_voters.len() as i64;
    let threshold_base = match vote.cfg.pass_threshold_base.clone().unwrap_or_default() {
        PassThresholdBase::AllowedVoters => allowed_voters_count,
        PassThresholdBase::VotesCast { exclude_abstentions } => {
            if exclude_abstentions.unwrap_or(false) {
                in_favor + against
            } else {
                binding
            }
        }
    };

    // Calculate the against and in favor percentages over the threshold base
    let mut against_percentage = 0.0;
    let mut in_favor_percentage = 0.0;
    #[allow(clippy::cast_precision_loss)]
    if threshold_base > 0 {
        against_percentage = against as f64 / threshold_base as f64 * 100.0;
        in_favor_percentage = in_favor as f64 / threshold_base as f64 * 100.0;
    }

    // Collect the allowed voters who have not voted yet
    let pending_voters: Vec<UserName> = allowed_voters
        .iter()
        .filter(|user| !votes.keys().any(|voter| voter.eq_ignore_ascii_case(user)))
        .cloned()
        .collect();

    // Check if the vote passed
    let passed = threshold_reached(in_favor, vote.cfg.pass_threshold, threshold_base);

    // Prepare the vote results (not voted is counted before pending voters are moved)
    Ok(VoteResults {
        abstain,
        against,
        against_percentage,
        allowed_voters: allowed_voters_count,
        binding,
        in_favor,
        in_favor_percentage,
        non_binding,
        not_voted: pending_voters.len() as i64,
        pass_threshold: vote.cfg.pass_threshold,
        passed,
        pending_voters,
        votes,
    })
}

/// Checks if the votes in favor reach the pass threshold over the base provided. The
/// threshold is not multiplied by the base, as the rounding of a decimal threshold could
/// leave the product above the exact value and miss a vote that reaches it exactly.
#[allow(clippy::cast_precision_loss)]
fn threshold_reached(in_favor: i64, pass_threshold: f64, base: i64) -> bool {
    base > 0 && in_favor as f64 * 100.0 / base as f64 >= pass_threshold
}
