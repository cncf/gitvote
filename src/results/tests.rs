use std::{sync::Arc, time::Duration};

use anyhow::format_err;
use futures::future::{self};
use mockall::predicate::eq;
use proptest::{collection, prelude::*, sample};
use serde_json::json;

use crate::github::{MockGH, Reaction, User, UserName};
use crate::testutil::*;

use super::*;

/// Reactions used in randomly generated votes, with the option they map to.
const PROPTEST_REACTIONS: [(&str, Option<VoteOption>); 4] = [
    (REACTION_IN_FAVOR, Some(VoteOption::InFavor)),
    (REACTION_AGAINST, Some(VoteOption::Against)),
    (REACTION_ABSTAIN, Some(VoteOption::Abstain)),
    ("heart", None),
];

/// Number of users that can take part in randomly generated votes.
const PROPTEST_USERS: usize = 6;

/// Additional deterministic timestamps used to check each vote keeps its own.
const TIMESTAMP2: &str = "2022-11-30T11:00:00Z";
const TIMESTAMP3: &str = "2022-11-30T12:00:00Z";

#[tokio::test]
async fn calculate_error_getting_allowed_voters() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_comment_reactions()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(COMMENT_ID))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(vec![]))));
    gh.expect_get_allowed_voters()
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Run and check the error is propagated
    let vote = setup_test_vote();
    let err = calculate(Arc::new(gh), ORG, REPO, &vote).await.unwrap_err();
    assert_eq!(err.to_string(), ERROR);
}

#[tokio::test]
async fn calculate_error_getting_reactions() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_comment_reactions()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(COMMENT_ID))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    gh.expect_get_allowed_voters().never();

    // Run and check the error is propagated
    let vote = setup_test_vote();
    let err = calculate(Arc::new(gh), ORG, REPO, &vote).await.unwrap_err();
    assert_eq!(err.to_string(), ERROR);
}

proptest! {
    #[test]
    fn calculate_results_match_the_votes_cast(
        allowed_users in sample::subsequence((1..=PROPTEST_USERS).collect::<Vec<_>>(), 0..=PROPTEST_USERS),
        allowed_users_uppercase in collection::vec(any::<bool>(), PROPTEST_USERS),
        users_reactions in collection::vec((1..=PROPTEST_USERS, 0..PROPTEST_REACTIONS.len()), 0..20),
        pass_threshold in 1..=100_u32,
    ) {
        // Setup allowed voters, spelled with random casing
        let allowed_voters: Vec<UserName> = allowed_users
            .iter()
            .map(|i| {
                let user = format!("user{i}");
                if allowed_users_uppercase[i - 1] { user.to_uppercase() } else { user }
            })
            .collect();

        // Setup reactions and vote
        let reactions: Vec<Reaction> = users_reactions
            .iter()
            .map(|(i, r)| Reaction {
                user: User { login: format!("user{i}") },
                content: PROPTEST_REACTIONS[*r].0.to_string(),
                created_at: TIMESTAMP.to_string(),
            })
            .collect();
        let mut vote = setup_test_vote();
        vote.cfg.pass_threshold = f64::from(pass_threshold);

        // Setup GitHub expectations
        let mut gh = MockGH::new();
        let reactions_returned = reactions.clone();
        gh.expect_get_comment_reactions()
            .with(eq(INST_ID), eq(OWNER), eq(REPO), eq(COMMENT_ID))
            .times(1)
            .returning(move |_, _, _, _| Box::pin(future::ready(Ok(reactions_returned.clone()))));
        let allowed_voters_returned = allowed_voters.clone();
        gh.expect_get_allowed_voters()
            .withf(|inst_id, _, owner, repo, _| *inst_id == INST_ID && owner == OWNER && repo == REPO)
            .times(1)
            .returning(move |_, _, _, _, _| {
                Box::pin(future::ready(Ok(allowed_voters_returned.clone())))
            });

        // Calculate vote results
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let results = rt.block_on(calculate(Arc::new(gh), OWNER, REPO, &vote)).unwrap();

        // Build expected votes (only users with a single supported reaction count)
        let mut users_options: BTreeMap<UserName, Vec<VoteOption>> = BTreeMap::new();
        for (i, r) in &users_reactions {
            if let Some(option) = &PROPTEST_REACTIONS[*r].1 {
                users_options.entry(format!("user{i}")).or_default().push(option.clone());
            }
        }
        let expected_votes: BTreeMap<UserName, (VoteOption, bool)> = users_options
            .into_iter()
            .filter(|(_, options)| options.len() == 1)
            .map(|(user, options)| {
                let binding = allowed_voters.iter().any(|voter| voter.to_lowercase() == user);
                (user, (options[0].clone(), binding))
            })
            .collect();

        // Build expected results summary using integer arithmetic only
        let count_binding = |option: Option<VoteOption>| {
            expected_votes
                .values()
                .filter(|(vote_option, binding)| *binding && option.as_ref().is_none_or(|o| o == vote_option))
                .count() as i64
        };
        let expected_in_favor = count_binding(Some(VoteOption::InFavor));
        let expected_binding = count_binding(None);
        let expected_pending: Vec<UserName> = allowed_voters
            .iter()
            .filter(|voter| !expected_votes.contains_key(&voter.to_lowercase()))
            .cloned()
            .collect();
        let allowed = allowed_voters.len() as i64;
        let expected_passed = allowed > 0 && expected_in_favor * 100 >= i64::from(pass_threshold) * allowed;

        // Check the votes counted
        let votes: BTreeMap<UserName, (VoteOption, bool)> = results
            .votes
            .iter()
            .map(|(user, user_vote)| (user.clone(), (user_vote.vote_option.clone(), user_vote.binding)))
            .collect();
        prop_assert_eq!(&votes, &expected_votes);

        // Check the results summary
        prop_assert_eq!(results.passed, expected_passed);
        prop_assert_eq!(results.in_favor, expected_in_favor);
        prop_assert_eq!(results.against, count_binding(Some(VoteOption::Against)));
        prop_assert_eq!(results.abstain, count_binding(Some(VoteOption::Abstain)));
        prop_assert_eq!(results.binding, expected_binding);
        prop_assert_eq!(results.non_binding, expected_votes.len() as i64 - expected_binding);
        prop_assert_eq!(results.allowed_voters, allowed);
        prop_assert_eq!(&results.pending_voters, &expected_pending);
        prop_assert_eq!(results.not_voted, expected_pending.len() as i64);

        // Check the results invariants
        prop_assert_eq!(results.in_favor + results.against + results.abstain, results.binding);
        prop_assert_eq!(results.binding + results.not_voted, results.allowed_voters);
    }
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
                issue_title: Some(TITLE.to_string()),
                is_pull_request: false,
                repository_full_name: REPOFN.to_string(),
                organization: Some(ORG.to_string()),
                results: None,
            };

            // Setup mocks and expectations
            let mut gh = MockGH::new();
            gh.expect_get_comment_reactions()
                .with(eq(INST_ID), eq(OWNER), eq(REPO), eq(COMMENT_ID))
                .times(1)
                .returning(|_, _, _, _| Box::pin(future::ready(Ok($reactions))));
            gh.expect_get_allowed_voters()
                .withf(|inst_id, cfg, owner, repo, org| {
                    *inst_id == INST_ID
                        && *cfg == $cfg
                        && owner == OWNER
                        && repo == REPO
                        && *org == Some(ORG.to_string()).as_ref()
                })
                .times(1)
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
    calculate_allowed_voters_are_matched_case_insensitively:
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
            }
        ],
        allowed_voters: vec![
            USER1.to_uppercase(),
            USER2.to_uppercase()
        ],
        expected_results: VoteResults {
            passed: true,
            in_favor_percentage: 50.0,
            pass_threshold: 50.0,
            in_favor: 1,
            against: 0,
            against_percentage: 0.0,
            abstain: 0,
            not_voted: 1,
            binding: 1,
            non_binding: 0,
            votes: BTreeMap::from([
                (
                    USER1.to_string(),
                    UserVote {
                        vote_option: VoteOption::InFavor,
                        timestamp: OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap(),
                        binding: true,
                    },
                )
            ]),
            allowed_voters: 2,
            pending_voters: vec![USER2.to_uppercase()],
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
            against_percentage: 0.0,
            abstain: 0,
            not_voted: 1,
            binding: 0,
            non_binding: 0,
            votes: BTreeMap::new(),
            allowed_voters: 1,
            pending_voters: vec![USER1.to_string()],
        }
    },

    calculate_do_not_count_votes_from_non_binding_multiple_options_voters:
    {
        cfg: CfgProfile {
            duration: Duration::from_secs(1),
            pass_threshold: 50.0,
            ..Default::default()
        },
        reactions: vec![
            Reaction {
                user: User { login: USER5.to_string() },
                content: REACTION_IN_FAVOR.to_string(),
                created_at: TIMESTAMP.to_string(),
            },
            Reaction {
                user: User { login: USER5.to_string() },
                content: REACTION_AGAINST.to_string(),
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
            against_percentage: 0.0,
            abstain: 0,
            not_voted: 1,
            binding: 0,
            non_binding: 0,
            votes: BTreeMap::new(),
            allowed_voters: 1,
            pending_voters: vec![USER1.to_string()],
        }
    },

    calculate_ignore_further_reactions_from_multiple_options_voters:
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
                user: User { login: USER1.to_string() },
                content: REACTION_AGAINST.to_string(),
                created_at: TIMESTAMP.to_string(),
            },
            Reaction {
                user: User { login: USER1.to_string() },
                content: REACTION_ABSTAIN.to_string(),
                created_at: TIMESTAMP.to_string(),
            },
            Reaction {
                user: User { login: USER2.to_string() },
                content: REACTION_IN_FAVOR.to_string(),
                created_at: TIMESTAMP.to_string(),
            }
        ],
        allowed_voters: vec![
            USER1.to_string(),
            USER2.to_string()
        ],
        expected_results: VoteResults {
            passed: true,
            in_favor_percentage: 50.0,
            pass_threshold: 50.0,
            in_favor: 1,
            against: 0,
            against_percentage: 0.0,
            abstain: 0,
            not_voted: 1,
            binding: 1,
            non_binding: 0,
            votes: BTreeMap::from([
                (
                    USER2.to_string(),
                    UserVote {
                        vote_option: VoteOption::InFavor,
                        timestamp: OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap(),
                        binding: true,
                    },
                )
            ]),
            allowed_voters: 2,
            pending_voters: vec![USER1.to_string()],
        }
    },

    calculate_no_allowed_voters:
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
            }
        ],
        allowed_voters: Vec::<UserName>::new(),
        expected_results: VoteResults {
            passed: false,
            in_favor_percentage: 0.0,
            pass_threshold: 50.0,
            in_favor: 0,
            against: 0,
            against_percentage: 0.0,
            abstain: 0,
            not_voted: 0,
            binding: 0,
            non_binding: 1,
            votes: BTreeMap::from([
                (
                    USER1.to_string(),
                    UserVote {
                        vote_option: VoteOption::InFavor,
                        timestamp: OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap(),
                        binding: false,
                    },
                )
            ]),
            allowed_voters: 0,
            pending_voters: vec![],
        }
    },

    calculate_no_reactions:
    {
        cfg: CfgProfile {
            duration: Duration::from_secs(1),
            pass_threshold: 50.0,
            ..Default::default()
        },
        reactions: Vec::<Reaction>::new(),
        allowed_voters: vec![
            USER1.to_string(),
            USER2.to_string()
        ],
        expected_results: VoteResults {
            passed: false,
            in_favor_percentage: 0.0,
            pass_threshold: 50.0,
            in_favor: 0,
            against: 0,
            against_percentage: 0.0,
            abstain: 0,
            not_voted: 2,
            binding: 0,
            non_binding: 0,
            votes: BTreeMap::new(),
            allowed_voters: 2,
            pending_voters: vec![USER1.to_string(), USER2.to_string()],
        }
    },

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
            against_percentage: 100.0,
            abstain: 0,
            not_voted: 0,
            binding: 1,
            non_binding: 0,
            votes: BTreeMap::from([
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

    calculate_vote_does_not_pass_when_in_favor_percentage_is_below_pass_threshold:
    {
        cfg: CfgProfile {
            duration: Duration::from_secs(1),
            pass_threshold: 66.67,
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
                content: REACTION_ABSTAIN.to_string(),
                created_at: TIMESTAMP.to_string(),
            }
        ],
        allowed_voters: vec![
            USER1.to_string(),
            USER2.to_string(),
            USER3.to_string()
        ],
        expected_results: VoteResults {
            passed: false,
            in_favor_percentage: 2.0 / 3.0 * 100.0,
            pass_threshold: 66.67,
            in_favor: 2,
            against: 0,
            against_percentage: 0.0,
            abstain: 1,
            not_voted: 0,
            binding: 3,
            non_binding: 0,
            votes: BTreeMap::from([
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
                        vote_option: VoteOption::Abstain,
                        timestamp: OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap(),
                        binding: true,
                    },
                ),
            ]),
            allowed_voters: 3,
            pending_voters: vec![],
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
            against_percentage: 0.0,
            abstain: 0,
            not_voted: 1,
            binding: 3,
            non_binding: 0,
            votes: BTreeMap::from([
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

    calculate_vote_passes_when_in_favor_votes_exactly_reach_pass_threshold:
    {
        cfg: CfgProfile {
            duration: Duration::from_secs(1),
            pass_threshold: 58.0,
            ..Default::default()
        },
        reactions: (1..=29)
            .map(|i| Reaction {
                user: User { login: format!("user{i}") },
                content: REACTION_IN_FAVOR.to_string(),
                created_at: TIMESTAMP.to_string(),
            })
            .collect::<Vec<_>>(),
        allowed_voters: (1..=50).map(|i| format!("user{i}")).collect::<Vec<_>>(),
        expected_results: VoteResults {
            passed: true,
            in_favor_percentage: 29.0 / 50.0 * 100.0,
            pass_threshold: 58.0,
            in_favor: 29,
            against: 0,
            against_percentage: 0.0,
            abstain: 0,
            not_voted: 21,
            binding: 29,
            non_binding: 0,
            votes: (1..=29)
                .map(|i| {
                    (
                        format!("user{i}"),
                        UserVote {
                            vote_option: VoteOption::InFavor,
                            timestamp: OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap(),
                            binding: true,
                        },
                    )
                })
                .collect(),
            allowed_voters: 50,
            pending_voters: (30..=50).map(|i| format!("user{i}")).collect(),
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
            against_percentage: 25.0,
            abstain: 1,
            not_voted: 1,
            binding: 3,
            non_binding: 1,
            votes: BTreeMap::from([
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

    calculate_votes_keep_each_user_timestamp:
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
                created_at: TIMESTAMP2.to_string(),
            },
            Reaction {
                user: User { login: USER5.to_string() },
                content: REACTION_IN_FAVOR.to_string(),
                created_at: TIMESTAMP3.to_string(),
            }
        ],
        allowed_voters: vec![
            USER1.to_string(),
            USER2.to_string()
        ],
        expected_results: VoteResults {
            passed: true,
            in_favor_percentage: 50.0,
            pass_threshold: 50.0,
            in_favor: 1,
            against: 1,
            against_percentage: 50.0,
            abstain: 0,
            not_voted: 0,
            binding: 2,
            non_binding: 1,
            votes: BTreeMap::from([
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
                        timestamp: OffsetDateTime::parse(TIMESTAMP2, &Rfc3339).unwrap(),
                        binding: true,
                    },
                ),
                (
                    USER5.to_string(),
                    UserVote {
                        vote_option: VoteOption::InFavor,
                        timestamp: OffsetDateTime::parse(TIMESTAMP3, &Rfc3339).unwrap(),
                        binding: false,
                    },
                ),
            ]),
            allowed_voters: 2,
            pending_voters: vec![],
        }
    },
);

#[test]
fn vote_option_display() {
    // Check display strings
    assert_eq!(VoteOption::InFavor.to_string(), "In favor");
    assert_eq!(VoteOption::Against.to_string(), "Against");
    assert_eq!(VoteOption::Abstain.to_string(), "Abstain");
}

#[test]
fn vote_option_from_reaction() {
    // Check supported and unsupported reactions
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

#[test]
fn vote_results_deserialize_stored_json() {
    // Setup JSON as stored in the vote results column
    let stored = json!({
        "passed": false,
        "in_favor_percentage": 25.0,
        "pass_threshold": 50.0,
        "in_favor": 1,
        "against": 1,
        "against_percentage": 25.0,
        "abstain": 1,
        "not_voted": 1,
        "binding": 3,
        "non_binding": 1,
        "allowed_voters": 4,
        "votes": {
            "user1": {"vote_option": "InFavor", "timestamp": [2022, 334, 10, 0, 0, 0, 0, 0, 0], "binding": true},
            "user2": {"vote_option": "Against", "timestamp": [2022, 334, 10, 0, 0, 0, 0, 0, 0], "binding": true},
            "user3": {"vote_option": "Abstain", "timestamp": [2022, 334, 10, 0, 0, 0, 0, 0, 0], "binding": true},
            "user5": {"vote_option": "InFavor", "timestamp": [2022, 334, 10, 0, 0, 0, 0, 0, 0], "binding": false}
        },
        "pending_voters": ["user4"]
    });

    // Check it deserializes into the expected results
    let timestamp = OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap();
    let results: VoteResults = serde_json::from_value(stored).unwrap();
    assert_eq!(
        results,
        VoteResults {
            passed: false,
            in_favor_percentage: 25.0,
            pass_threshold: 50.0,
            in_favor: 1,
            against: 1,
            against_percentage: 25.0,
            abstain: 1,
            not_voted: 1,
            binding: 3,
            non_binding: 1,
            allowed_voters: 4,
            votes: BTreeMap::from([
                (
                    USER1.to_string(),
                    UserVote {
                        vote_option: VoteOption::InFavor,
                        timestamp,
                        binding: true,
                    },
                ),
                (
                    USER2.to_string(),
                    UserVote {
                        vote_option: VoteOption::Against,
                        timestamp,
                        binding: true,
                    },
                ),
                (
                    USER3.to_string(),
                    UserVote {
                        vote_option: VoteOption::Abstain,
                        timestamp,
                        binding: true,
                    },
                ),
                (
                    USER5.to_string(),
                    UserVote {
                        vote_option: VoteOption::InFavor,
                        timestamp,
                        binding: false,
                    },
                ),
            ]),
            pending_voters: vec![USER4.to_string()],
        }
    );
}

#[test]
fn vote_results_serialize() {
    // Check serialized vote results
    assert_eq!(
        serde_json::to_value(setup_test_vote_results()).unwrap(),
        json!({
            "passed": true,
            "in_favor_percentage": 100.0,
            "pass_threshold": 50.0,
            "in_favor": 1,
            "against": 0,
            "against_percentage": 0.0,
            "abstain": 0,
            "not_voted": 0,
            "binding": 1,
            "non_binding": 0,
            "allowed_voters": 1,
            "votes": {
                "user1": {
                    "vote_option": "InFavor",
                    "timestamp": [2022, 334, 10, 0, 0, 0, 0, 0, 0],
                    "binding": true
                }
            },
            "pending_voters": []
        })
    );
}
