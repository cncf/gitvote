use crate::{
    cfg::CfgProfile,
    github::{DynGH, UserName},
};
use anyhow::{format_err, Result};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fmt};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use tokio_postgres::{types::Json, Row};
use uuid::Uuid;

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
            _ => return Err(format_err!("reaction not supported")),
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
    pub abstain: i64,
    pub not_voted: i64,
    pub binding: i64,
    pub non_binding: i64,
    pub allowed_voters: i64,
    pub votes: HashMap<UserName, UserVote>,
    pub pending_voters: Vec<UserName>,
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
    let reactions = gh
        .get_comment_reactions(inst_id, owner, repo, vote.vote_comment_id)
        .await?;

    // Get list of allowed voters (users with binding votes)
    let allowed_voters = gh
        .get_allowed_voters(inst_id, &vote.cfg, owner, repo, &vote.organization)
        .await?;

    // Track users votes
    let mut votes: HashMap<UserName, UserVote> = HashMap::new();
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

        // Track vote
        let binding = allowed_voters.contains(&username);
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

    // Prepare results and return them
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
    let mut in_favor_percentage = 0.0;
    if !allowed_voters.is_empty() {
        in_favor_percentage = in_favor as f64 / allowed_voters.len() as f64 * 100.0;
    }
    let pending_voters: Vec<UserName> = allowed_voters
        .iter()
        .filter(|user| !votes.contains_key(*user))
        .cloned()
        .collect();

    Ok(VoteResults {
        passed: in_favor_percentage >= vote.cfg.pass_threshold,
        in_favor_percentage,
        pass_threshold: vote.cfg.pass_threshold,
        in_favor,
        against,
        abstain,
        not_voted: pending_voters.len() as i64,
        binding,
        non_binding,
        allowed_voters: allowed_voters.len() as i64,
        votes,
        pending_voters,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::{MockGH, Reaction, User};
    use crate::testutil::*;
    use futures::future::{self};
    use mockall::predicate::eq;
    use std::{sync::Arc, time::Duration};

    #[test]
    fn vote_option_from_reaction() {
        assert_eq!(
            VoteOption::from_reaction(REACTION_IN_FAVOR).unwrap(),
            VoteOption::InFavor
        );
        assert_eq!(
            VoteOption::from_reaction(REACTION_AGAINST).unwrap(),
            VoteOption::Against
        );
        assert_eq!(
            VoteOption::from_reaction(REACTION_ABSTAIN).unwrap(),
            VoteOption::Abstain
        );
        assert!(VoteOption::from_reaction("unsupported").is_err());
    }

    macro_rules! test_calculate {
        ($(
            $func:ident:
            {
                cfg: $cfg:expr,
                reactions: $reactions:expr,
                allowed_voters: $allowed_voters:expr,
                expected_results: $expected_results:expr
            }
        ,)*) => {
        $(
            #[tokio::test]
            async fn $func() {
                // Prepare test data
                let vote = Vote {
                    vote_id: Uuid::parse_str(VOTE_ID).unwrap(),
                    vote_comment_id: COMMENT_ID,
                    created_at: OffsetDateTime::now_utc(),
                    created_by: USER.to_string(),
                    ends_at: OffsetDateTime::now_utc(),
                    closed: false,
                    closed_at: None,
                    checked_at: None,
                    cfg: $cfg.clone(),
                    installation_id: INST_ID as i64,
                    issue_id: ISSUE_ID,
                    issue_number: ISSUE_NUM,
                    is_pull_request: false,
                    repository_full_name: REPOFN.to_string(),
                    organization: Some(ORG.to_string()),
                    results: None,
                };

                // Setup mocks and expectations
                let mut gh = MockGH::new();
                gh.expect_get_comment_reactions()
                    .with(eq(INST_ID), eq(OWNER), eq(REPO), eq(COMMENT_ID))
                    .returning(|_, _, _, _| Box::pin(future::ready(Ok($reactions))));
                gh.expect_get_allowed_voters()
                    .with(eq(INST_ID), eq($cfg), eq(OWNER), eq(REPO), eq(Some(ORG.to_string())))
                    .returning(|_, _, _, _, _| Box::pin(future::ready(Ok($allowed_voters))));

                // Calculate vote results and check we get what we expect
                let results = calculate(Arc::new(gh), OWNER, REPO, &vote)
                    .await
                    .unwrap();
                assert_eq!(results, $expected_results);
            }
        )*
        }
    }

    test_calculate!(
        calculate_unsupported_reactions_are_ignored:
        {
            cfg: CfgProfile {
                duration: Duration::from_secs(1),
                pass_threshold: 50.0,
                ..Default::default()
            },
            reactions: vec![
                Reaction {
                    user: User { login: USER1.to_string() },
                    content: "unsupported".to_string(),
                    created_at: TIMESTAMP.to_string(),
                },
                Reaction {
                    user: User { login: USER1.to_string() },
                    content: REACTION_AGAINST.to_string(),
                    created_at: TIMESTAMP.to_string(),
                },
                Reaction {
                    user: User { login: USER1.to_string() },
                    content: "unsupported".to_string(),
                    created_at: TIMESTAMP.to_string(),
                }
            ],
            allowed_voters: vec![
                USER1.to_string()
            ],
            expected_results: VoteResults {
                passed: false,
                in_favor_percentage: 0.0,
                pass_threshold: 50.0,
                in_favor: 0,
                against: 1,
                abstain: 0,
                not_voted: 0,
                binding: 1,
                non_binding: 0,
                votes: HashMap::from([
                    (
                        USER1.to_string(),
                        UserVote {
                            vote_option: VoteOption::Against,
                            timestamp: OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap(),
                            binding: true,
                        },
                    )
                ]),
                allowed_voters: 1,
                pending_voters: vec![],
            }
        },

        calculate_do_not_count_votes_from_multiple_options_voters:
        {
            cfg: CfgProfile {
                duration: Duration::from_secs(1),
                pass_threshold: 50.0,
                ..Default::default()
            },
            reactions: vec![
                Reaction {
                    user: User { login: USER1.to_string() },
                    content: REACTION_AGAINST.to_string(),
                    created_at: TIMESTAMP.to_string(),
                },
                Reaction {
                    user: User { login: USER1.to_string() },
                    content: REACTION_ABSTAIN.to_string(),
                    created_at: TIMESTAMP.to_string(),
                }
            ],
            allowed_voters: vec![
                USER1.to_string()
            ],
            expected_results: VoteResults {
                passed: false,
                in_favor_percentage: 0.0,
                pass_threshold: 50.0,
                in_favor: 0,
                against: 0,
                abstain: 0,
                not_voted: 1,
                binding: 0,
                non_binding: 0,
                votes: HashMap::new(),
                allowed_voters: 1,
                pending_voters: vec![USER1.to_string()],
            }
        },

        calculate_votes_are_counted_correctly:
        {
            cfg: CfgProfile {
                duration: Duration::from_secs(1),
                pass_threshold: 50.0,
                ..Default::default()
            },
            reactions: vec![
                Reaction {
                    user: User { login: USER1.to_string() },
                    content: REACTION_IN_FAVOR.to_string(),
                    created_at: TIMESTAMP.to_string(),
                },
                Reaction {
                    user: User { login: USER2.to_string() },
                    content: REACTION_AGAINST.to_string(),
                    created_at: TIMESTAMP.to_string(),
                },
                Reaction {
                    user: User { login: USER3.to_string() },
                    content: REACTION_ABSTAIN.to_string(),
                    created_at: TIMESTAMP.to_string(),
                },
                Reaction {
                    user: User { login: USER5.to_string() },
                    content: REACTION_IN_FAVOR.to_string(),
                    created_at: TIMESTAMP.to_string(),
                }
            ],
            allowed_voters: vec![
                USER1.to_string(),
                USER2.to_string(),
                USER3.to_string(),
                USER4.to_string()
            ],
            expected_results: VoteResults {
                passed: false,
                in_favor_percentage: 25.0,
                pass_threshold: 50.0,
                in_favor: 1,
                against: 1,
                abstain: 1,
                not_voted: 1,
                binding: 3,
                non_binding: 1,
                votes: HashMap::from([
                    (
                        USER1.to_string(),
                        UserVote {
                            vote_option: VoteOption::InFavor,
                            timestamp: OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap(),
                            binding: true,
                        },
                    ),
                    (
                        USER2.to_string(),
                        UserVote {
                            vote_option: VoteOption::Against,
                            timestamp: OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap(),
                            binding: true,
                        },
                    ),
                    (
                        USER3.to_string(),
                        UserVote {
                            vote_option: VoteOption::Abstain,
                            timestamp: OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap(),
                            binding: true,
                        },
                    ),
                    (
                        USER5.to_string(),
                        UserVote {
                            vote_option: VoteOption::InFavor,
                            timestamp: OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap(),
                            binding: false,
                        },
                    ),
                ]),
                allowed_voters: 4,
                pending_voters: vec![USER4.to_string()],
            }
        },

        calculate_vote_passes_when_in_favor_percentage_reaches_pass_threshold:
        {
            cfg: CfgProfile {
                duration: Duration::from_secs(1),
                pass_threshold: 75.0,
                ..Default::default()
            },
            reactions: vec![
                Reaction {
                    user: User { login: USER1.to_string() },
                    content: REACTION_IN_FAVOR.to_string(),
                    created_at: TIMESTAMP.to_string(),
                },
                Reaction {
                    user: User { login: USER2.to_string() },
                    content: REACTION_IN_FAVOR.to_string(),
                    created_at: TIMESTAMP.to_string(),
                },
                Reaction {
                    user: User { login: USER3.to_string() },
                    content: REACTION_IN_FAVOR.to_string(),
                    created_at: TIMESTAMP.to_string(),
                }
            ],
            allowed_voters: vec![
                USER1.to_string(),
                USER2.to_string(),
                USER3.to_string(),
                USER4.to_string()
            ],
            expected_results: VoteResults {
                passed: true,
                in_favor_percentage: 75.0,
                pass_threshold: 75.0,
                in_favor: 3,
                against: 0,
                abstain: 0,
                not_voted: 1,
                binding: 3,
                non_binding: 0,
                votes: HashMap::from([
                    (
                        USER1.to_string(),
                        UserVote {
                            vote_option: VoteOption::InFavor,
                            timestamp: OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap(),
                            binding: true,
                        },
                    ),
                    (
                        USER2.to_string(),
                        UserVote {
                            vote_option: VoteOption::InFavor,
                            timestamp: OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap(),
                            binding: true,
                        },
                    ),
                    (
                        USER3.to_string(),
                        UserVote {
                            vote_option: VoteOption::InFavor,
                            timestamp: OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap(),
                            binding: true,
                        },
                    ),
                ]),
                allowed_voters: 4,
                pending_voters: vec![USER4.to_string()],
            }
        },
    );
}
