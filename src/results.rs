//! This module defines the logic to calculate vote results.

use std::{collections::BTreeMap, fmt};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio_postgres::{Row, types::Json};
use uuid::Uuid;

use crate::{
    cfg_repo::{CfgProfile, PassThresholdBase},
    github::{DynGH, UserName},
};

/// Supported reactions.
pub(crate) const REACTION_IN_FAVOR: &str = "+1";
pub(crate) const REACTION_AGAINST: &str = "-1";
pub(crate) const REACTION_ABSTAIN: &str = "eyes";

/// Vote information.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(clippy::struct_field_names)]
pub(crate) struct Vote {
    pub vote_id: Uuid,
    pub vote_comment_id: i64,
    pub created_at: OffsetDateTime,
    pub created_by: String,
    pub ends_at: OffsetDateTime,
    pub closed: bool,
    pub closed_at: Option<OffsetDateTime>,
    pub checked_at: Option<OffsetDateTime>,
    pub cfg: CfgProfile,
    pub installation_id: i64,
    pub issue_id: i64,
    pub issue_number: i64,
    pub issue_title: Option<String>,
    pub is_pull_request: bool,
    pub repository_full_name: String,
    pub organization: Option<String>,
    pub results: Option<VoteResults>,
}

impl From<&Row> for Vote {
    fn from(row: &Row) -> Self {
        let Json(cfg): Json<CfgProfile> = row.get("cfg");
        let results: Option<Json<VoteResults>> = row.get("results");
        Self {
            vote_id: row.get("vote_id"),
            vote_comment_id: row.get("vote_comment_id"),
            created_at: row.get("created_at"),
            created_by: row.get("created_by"),
            ends_at: row.get("ends_at"),
            closed: row.get("closed"),
            closed_at: row.get("closed_at"),
            checked_at: row.get("checked_at"),
            cfg,
            installation_id: row.get("installation_id"),
            issue_id: row.get("issue_id"),
            issue_number: row.get("issue_number"),
            issue_title: row.get("issue_title"),
            is_pull_request: row.get("is_pull_request"),
            repository_full_name: row.get("repository_full_name"),
            organization: row.get("organization"),
            results: results.map(|Json(results)| results),
        }
    }
}

/// Vote options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum VoteOption {
    InFavor,
    Against,
    Abstain,
}

impl VoteOption {
    /// Create a new vote option from a reaction string.
    fn from_reaction(reaction: &str) -> Result<Self> {
        let vote_option = match reaction {
            REACTION_IN_FAVOR => Self::InFavor,
            REACTION_AGAINST => Self::Against,
            REACTION_ABSTAIN => Self::Abstain,
            _ => bail!("reaction not supported"),
        };
        Ok(vote_option)
    }
}

impl fmt::Display for VoteOption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::InFavor => "In favor",
            Self::Against => "Against",
            Self::Abstain => "Abstain",
        };
        write!(f, "{s}")
    }
}

/// Vote results information.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct VoteResults {
    pub passed: bool,
    pub in_favor_percentage: f64,
    pub pass_threshold: f64,
    pub in_favor: i64,
    pub against: i64,
    pub against_percentage: f64,
    pub abstain: i64,
    pub not_voted: i64,
    pub binding: i64,
    pub non_binding: i64,
    pub allowed_voters: i64,
    pub votes: BTreeMap<UserName, UserVote>,
    pub pending_voters: Vec<UserName>,
}

impl VoteResults {
    /// Check if the vote passes when calculated over all the allowed voters, so
    /// that further votes from pending voters cannot make it fail (assuming the
    /// votes already cast and the allowed voters do not change).
    pub(crate) fn passes_with_all_allowed_voters(&self) -> bool {
        threshold_reached(self.in_favor, self.pass_threshold, self.allowed_voters)
    }
}

/// User's vote details.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct UserVote {
    pub vote_option: VoteOption,
    pub timestamp: OffsetDateTime,
    pub binding: bool,
}

/// Calculate vote results.
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
                vote_option,
                timestamp: OffsetDateTime::parse(reaction.created_at.as_str(), &Rfc3339)
                    .expect("created_at timestamp to be valid"),
                binding,
            },
        );
    }

    // Count binding votes per option and the number of binding and
    // non-binding voters (only binding votes count towards the result)
    let (mut in_favor, mut against, mut abstain, mut binding, mut non_binding) = (0, 0, 0, 0, 0);
    for user_vote in votes.values() {
        if user_vote.binding {
            match user_vote.vote_option {
                VoteOption::InFavor => in_favor += 1,
                VoteOption::Against => against += 1,
                VoteOption::Abstain => abstain += 1,
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

    // Calculate the in favor and against percentages over the threshold base
    let mut in_favor_percentage = 0.0;
    let mut against_percentage = 0.0;
    #[allow(clippy::cast_precision_loss)]
    if threshold_base > 0 {
        in_favor_percentage = in_favor as f64 / threshold_base as f64 * 100.0;
        against_percentage = against as f64 / threshold_base as f64 * 100.0;
    }

    // Collect the allowed voters who have not voted yet
    let pending_voters: Vec<UserName> = allowed_voters
        .iter()
        .filter(|user| !votes.keys().any(|voter| voter.eq_ignore_ascii_case(user)))
        .cloned()
        .collect();

    // Check if the vote passed
    let passed = threshold_reached(in_favor, vote.cfg.pass_threshold, threshold_base);

    Ok(VoteResults {
        passed,
        in_favor_percentage,
        pass_threshold: vote.cfg.pass_threshold,
        in_favor,
        against,
        against_percentage,
        abstain,
        not_voted: pending_voters.len() as i64,
        binding,
        non_binding,
        allowed_voters: allowed_voters_count,
        votes,
        pending_voters,
    })
}

/// Check if the votes in favor reach the pass threshold over the base provided,
/// comparing without dividing so that exact matches are not lost to rounding.
#[allow(clippy::cast_precision_loss)]
fn threshold_reached(in_favor: i64, pass_threshold: f64, base: i64) -> bool {
    base > 0 && in_favor as f64 * 100.0 >= pass_threshold * base as f64
}

#[cfg(test)]
mod tests;
