use std::{sync::Arc, time::Duration};

use anyhow::format_err;
use futures::future::{self};
use mockall::predicate::eq;
use proptest::{collection, prelude::*, sample};
use serde_json::json;
use tokio_postgres::types::{FromSql, Type};

use crate::cfg_repo::AllowedVoters;
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

/// Pass threshold bases used in randomly generated votes.
const PROPTEST_THRESHOLD_BASES: [Option<PassThresholdBase>; 5] = [
    None,
    Some(PassThresholdBase::AllowedVoters),
    Some(PassThresholdBase::VotesCast {
        exclude_abstentions: None,
    }),
    Some(PassThresholdBase::VotesCast {
        exclude_abstentions: Some(false),
    }),
    Some(PassThresholdBase::VotesCast {
        exclude_abstentions: Some(true),
    }),
];

/// Number of users that can take part in randomly generated votes.
const PROPTEST_USERS: usize = 6;

/// Pass threshold used to require more votes in favor than against.
const STRICT_MAJORITY_THRESHOLD: f64 = 50.01;

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
        pass_threshold_base in 0..PROPTEST_THRESHOLD_BASES.len(),
    ) {
        // Setup allowed voters, spelled with random casing
        let allowed_voters: Vec<UserName> = allowed_users
            .iter()
            .map(|i| {
                let user = format!("user{i}");
                if allowed_users_uppercase[i - 1] { user.to_uppercase() } else { user }
            })
            .collect();

        // Setup reactions and vote profile
        let reactions: Vec<Reaction> = users_reactions
            .iter()
            .map(|(i, r)| reaction(&format!("user{i}"), PROPTEST_REACTIONS[*r].0))
            .collect();
        let cfg = CfgProfile {
            pass_threshold: f64::from(pass_threshold),
            pass_threshold_base: PROPTEST_THRESHOLD_BASES[pass_threshold_base].clone(),
            ..setup_test_vote().cfg
        };

        // Calculate vote results
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let results = rt.block_on(calculate_results(cfg.clone(), reactions, allowed_voters.clone()));

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
        let expected_against = count_binding(Some(VoteOption::Against));
        let expected_binding = count_binding(None);
        let expected_pending: Vec<UserName> = allowed_voters
            .iter()
            .filter(|voter| !expected_votes.contains_key(&voter.to_lowercase()))
            .cloned()
            .collect();
        let allowed = allowed_voters.len() as i64;
        let expected_base = match cfg.pass_threshold_base.clone().unwrap_or_default() {
            PassThresholdBase::AllowedVoters => allowed,
            PassThresholdBase::VotesCast { exclude_abstentions: Some(true) } => {
                expected_in_favor + expected_against
            }
            PassThresholdBase::VotesCast { .. } => expected_binding,
        };
        let reaches_threshold =
            |base: i64| base > 0 && expected_in_favor * 100 >= i64::from(pass_threshold) * base;
        let expected_passed = reaches_threshold(expected_base);
        #[allow(clippy::cast_precision_loss)]
        let expected_percentage = |count: i64| {
            if expected_base > 0 { count as f64 / expected_base as f64 * 100.0 } else { 0.0 }
        };

        // Check the votes counted
        let votes: BTreeMap<UserName, (VoteOption, bool)> = results
            .votes
            .iter()
            .map(|(user, user_vote)| (user.clone(), (user_vote.vote_option.clone(), user_vote.binding)))
            .collect();
        prop_assert_eq!(&votes, &expected_votes);

        // Check the results summary
        prop_assert_eq!(results.passed, expected_passed);
        prop_assert!((results.in_favor_percentage - expected_percentage(expected_in_favor)).abs() < 1e-9);
        prop_assert!((results.against_percentage - expected_percentage(expected_against)).abs() < 1e-9);
        prop_assert_eq!(results.in_favor, expected_in_favor);
        prop_assert_eq!(results.against, expected_against);
        prop_assert_eq!(results.abstain, count_binding(Some(VoteOption::Abstain)));
        prop_assert_eq!(results.binding, expected_binding);
        prop_assert_eq!(results.non_binding, expected_votes.len() as i64 - expected_binding);
        prop_assert_eq!(results.allowed_voters, allowed);
        prop_assert_eq!(&results.pending_voters, &expected_pending);
        prop_assert_eq!(results.not_voted, expected_pending.len() as i64);

        // Check the results invariants
        prop_assert_eq!(results.in_favor + results.against + results.abstain, results.binding);
        prop_assert_eq!(results.binding + results.not_voted, results.allowed_voters);
        for percentage in [results.in_favor_percentage, results.against_percentage] {
            prop_assert!(percentage.is_finite() && (0.0..=100.0).contains(&percentage));
        }

        // Check the early close check uses all the allowed voters as the base, and
        // that a vote that passes over them also passes over the base configured
        prop_assert_eq!(results.passes_with_all_allowed_voters(), reaches_threshold(allowed));
        prop_assert!(!results.passes_with_all_allowed_voters() || results.passed);
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
            // Calculate vote results and check we get what we expect
            let results = calculate_results($cfg, $reactions, $allowed_voters).await;
            assert_eq!(results, $expected_results);
        }
    )*
    }
}

test_calculate!(
    calculate_allowed_voters_are_matched_case_insensitively:
    {
        cfg: threshold_cfg(50.0),
        reactions: reactions(&votes_cast(1, 0, 0)),
        allowed_voters: vec![USER1.to_uppercase(), USER2.to_uppercase()],
        expected_results: VoteResults {
            passed: true,
            in_favor_percentage: 50.0,
            pass_threshold: 50.0,
            pass_rule: None,
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
        cfg: threshold_cfg(50.0),
        reactions: vec![
            reaction(USER1, REACTION_AGAINST),
            reaction(USER1, REACTION_ABSTAIN),
        ],
        allowed_voters: users(1),
        expected_results: expected_results(50.0, &[], &users(1), (false, 0.0, 0.0))
    },

    calculate_do_not_count_votes_from_non_binding_multiple_options_voters:
    {
        cfg: threshold_cfg(50.0),
        reactions: vec![
            reaction(USER5, REACTION_IN_FAVOR),
            reaction(USER5, REACTION_AGAINST),
        ],
        allowed_voters: users(1),
        expected_results: expected_results(50.0, &[], &users(1), (false, 0.0, 0.0))
    },

    calculate_ignore_further_reactions_from_multiple_options_voters:
    {
        cfg: threshold_cfg(50.0),
        reactions: vec![
            reaction(USER1, REACTION_IN_FAVOR),
            reaction(USER1, REACTION_AGAINST),
            reaction(USER1, REACTION_ABSTAIN),
            reaction(USER2, REACTION_IN_FAVOR),
        ],
        allowed_voters: users(2),
        expected_results: expected_results(
            50.0,
            &[(USER2.to_string(), VoteOption::InFavor)],
            &users(2),
            (true, 50.0, 0.0)
        )
    },

    calculate_no_allowed_voters:
    {
        cfg: threshold_cfg(50.0),
        reactions: reactions(&votes_cast(1, 0, 0)),
        allowed_voters: Vec::<UserName>::new(),
        expected_results: expected_results(50.0, &votes_cast(1, 0, 0), &[], (false, 0.0, 0.0))
    },

    calculate_no_reactions:
    {
        cfg: threshold_cfg(50.0),
        reactions: Vec::<Reaction>::new(),
        allowed_voters: users(2),
        expected_results: expected_results(50.0, &[], &users(2), (false, 0.0, 0.0))
    },

    calculate_pass_threshold_base_example_allowed_voters:
    {
        cfg: CfgProfile {
            pass_threshold_base: Some(PassThresholdBase::AllowedVoters),
            ..threshold_cfg(60.0)
        },
        reactions: reactions(&votes_cast(2, 1, 1)),
        allowed_voters: users(10),
        expected_results: expected_results(
            60.0,
            &votes_cast(2, 1, 1),
            &users(10),
            (false, 2.0 / 10.0 * 100.0, 1.0 / 10.0 * 100.0)
        )
    },

    calculate_pass_threshold_base_example_votes_cast:
    {
        cfg: votes_cast_cfg(60.0, None),
        reactions: reactions(&votes_cast(2, 1, 1)),
        allowed_voters: users(10),
        expected_results: expected_results(
            60.0,
            &votes_cast(2, 1, 1),
            &users(10),
            (false, 2.0 / 4.0 * 100.0, 1.0 / 4.0 * 100.0)
        )
    },

    calculate_pass_threshold_base_example_votes_cast_excluding_abstentions:
    {
        cfg: votes_cast_cfg(60.0, Some(true)),
        reactions: reactions(&votes_cast(2, 1, 1)),
        allowed_voters: users(10),
        expected_results: expected_results(
            60.0,
            &votes_cast(2, 1, 1),
            &users(10),
            (true, 2.0 / 3.0 * 100.0, 1.0 / 3.0 * 100.0)
        )
    },

    calculate_unsupported_reactions_are_ignored:
    {
        cfg: threshold_cfg(50.0),
        reactions: vec![
            reaction(USER1, "unsupported"),
            reaction(USER1, REACTION_AGAINST),
            reaction(USER1, "unsupported"),
        ],
        allowed_voters: users(1),
        expected_results: expected_results(
            50.0,
            &votes_cast(0, 1, 0),
            &users(1),
            (false, 0.0, 100.0)
        )
    },

    calculate_vote_does_not_pass_when_in_favor_percentage_is_below_pass_threshold:
    {
        cfg: threshold_cfg(66.67),
        reactions: reactions(&votes_cast(2, 0, 1)),
        allowed_voters: users(3),
        expected_results: expected_results(
            66.67,
            &votes_cast(2, 0, 1),
            &users(3),
            (false, 2.0 / 3.0 * 100.0, 0.0)
        )
    },

    calculate_vote_passes_when_in_favor_percentage_reaches_pass_threshold:
    {
        cfg: threshold_cfg(75.0),
        reactions: reactions(&votes_cast(3, 0, 0)),
        allowed_voters: users(4),
        expected_results: expected_results(75.0, &votes_cast(3, 0, 0), &users(4), (true, 75.0, 0.0))
    },

    calculate_vote_passes_when_in_favor_votes_exactly_reach_pass_threshold:
    {
        cfg: threshold_cfg(58.0),
        reactions: reactions(&votes_cast(29, 0, 0)),
        allowed_voters: users(50),
        expected_results: expected_results(
            58.0,
            &votes_cast(29, 0, 0),
            &users(50),
            (true, 29.0 / 50.0 * 100.0, 0.0)
        )
    },

    calculate_votes_are_counted_correctly:
    {
        cfg: threshold_cfg(50.0),
        reactions: reactions(&[votes_cast(1, 1, 1), vec![(USER5.to_string(), VoteOption::InFavor)]].concat()),
        allowed_voters: users(4),
        expected_results: VoteResults {
            passed: false,
            in_favor_percentage: 25.0,
            pass_threshold: 50.0,
            pass_rule: None,
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

    calculate_votes_cast_does_not_pass_when_in_favor_percentage_is_below_pass_threshold:
    {
        cfg: votes_cast_cfg(66.67, None),
        reactions: reactions(&votes_cast(2, 1, 0)),
        allowed_voters: users(5),
        expected_results: expected_results(
            66.67,
            &votes_cast(2, 1, 0),
            &users(5),
            (false, 2.0 / 3.0 * 100.0, 1.0 / 3.0 * 100.0)
        )
    },

    calculate_votes_cast_excluding_abstentions_does_not_pass_with_only_abstentions:
    {
        cfg: votes_cast_cfg(50.0, Some(true)),
        reactions: reactions(&votes_cast(0, 0, 2)),
        allowed_voters: users(3),
        expected_results: expected_results(50.0, &votes_cast(0, 0, 2), &users(3), (false, 0.0, 0.0))
    },

    calculate_votes_cast_excluding_abstentions_passes_when_in_favor_votes_exactly_reach_pass_threshold:
    {
        cfg: votes_cast_cfg(58.0, Some(true)),
        reactions: reactions(&votes_cast(29, 21, 5)),
        allowed_voters: users(60),
        expected_results: expected_results(
            58.0,
            &votes_cast(29, 21, 5),
            &users(60),
            (true, 29.0 / 50.0 * 100.0, 21.0 / 50.0 * 100.0)
        )
    },

    calculate_votes_cast_no_allowed_voters:
    {
        cfg: votes_cast_cfg(50.0, None),
        reactions: reactions(&votes_cast(1, 0, 0)),
        allowed_voters: Vec::<UserName>::new(),
        expected_results: expected_results(50.0, &votes_cast(1, 0, 0), &[], (false, 0.0, 0.0))
    },

    calculate_votes_cast_no_reactions:
    {
        cfg: votes_cast_cfg(50.0, None),
        reactions: Vec::<Reaction>::new(),
        allowed_voters: users(2),
        expected_results: expected_results(50.0, &[], &users(2), (false, 0.0, 0.0))
    },

    calculate_votes_cast_only_non_binding_votes:
    {
        cfg: votes_cast_cfg(50.0, None),
        reactions: reactions(&votes_cast(1, 0, 0)),
        allowed_voters: vec![USER2.to_string(), USER3.to_string()],
        expected_results: expected_results(
            50.0,
            &votes_cast(1, 0, 0),
            &[USER2.to_string(), USER3.to_string()],
            (false, 0.0, 0.0)
        )
    },

    calculate_votes_cast_passes_when_in_favor_votes_exactly_reach_pass_threshold:
    {
        cfg: votes_cast_cfg(58.0, None),
        reactions: reactions(&votes_cast(29, 21, 0)),
        allowed_voters: users(60),
        expected_results: expected_results(
            58.0,
            &votes_cast(29, 21, 0),
            &users(60),
            (true, 29.0 / 50.0 * 100.0, 21.0 / 50.0 * 100.0)
        )
    },

    calculate_votes_keep_each_user_timestamp:
    {
        cfg: threshold_cfg(50.0),
        reactions: vec![
            reaction(USER1, REACTION_IN_FAVOR),
            Reaction {
                created_at: TIMESTAMP2.to_string(),
                ..reaction(USER2, REACTION_AGAINST)
            },
            Reaction {
                created_at: TIMESTAMP3.to_string(),
                ..reaction(USER5, REACTION_IN_FAVOR)
            },
        ],
        allowed_voters: users(2),
        expected_results: {
            let mut results = expected_results(
                50.0,
                &[votes_cast(1, 1, 0), vec![(USER5.to_string(), VoteOption::InFavor)]].concat(),
                &users(2),
                (true, 50.0, 50.0)
            );
            results.votes.get_mut(USER2).unwrap().timestamp =
                OffsetDateTime::parse(TIMESTAMP2, &Rfc3339).unwrap();
            results.votes.get_mut(USER5).unwrap().timestamp =
                OffsetDateTime::parse(TIMESTAMP3, &Rfc3339).unwrap();
            results
        }
    },
);

#[tokio::test]
async fn calculate_strict_majority_recipe() {
    // Setup cases (in favor, against, abstain, pass threshold, expected passed)
    let cases = [
        (2, 0, 10, STRICT_MAJORITY_THRESHOLD, true),
        (2, 2, 0, STRICT_MAJORITY_THRESHOLD, false),
        (3, 2, 5, STRICT_MAJORITY_THRESHOLD, true),
        (0, 1, 0, STRICT_MAJORITY_THRESHOLD, false),
        (0, 0, 3, STRICT_MAJORITY_THRESHOLD, false),
        // A tie passes with 50, so it cannot be used to require a strict majority
        (2, 2, 0, 50.0, true),
    ];

    for (in_favor, against, abstain, pass_threshold, expected_passed) in cases {
        // Calculate the results, leaving one allowed voter pending
        let votes = votes_cast(in_favor, against, abstain);
        let allowed_voters = users(in_favor + against + abstain + 1);
        let cfg = votes_cast_cfg(pass_threshold, Some(true));
        let results = calculate_results(cfg, reactions(&votes), allowed_voters).await;

        // Check the vote passes only when expected
        assert_eq!(
            results.passed, expected_passed,
            "votes: ({in_favor}, {against}, {abstain}), pass threshold: {pass_threshold}"
        );
    }
}

#[tokio::test]
async fn calculate_votes_cast_omitted_exclude_abstentions_equals_false() {
    for (in_favor, against, abstain) in [(2, 1, 1), (1, 1, 0), (0, 0, 2)] {
        // Calculate the results with the flag omitted and set to false
        let votes = votes_cast(in_favor, against, abstain);
        let mut results = vec![];
        for exclude_abstentions in [None, Some(false)] {
            let cfg = votes_cast_cfg(50.0, exclude_abstentions);
            results.push(calculate_results(cfg, reactions(&votes), users(5)).await);
        }

        // Check both results are identical
        assert_eq!(
            results[0], results[1],
            "votes: ({in_favor}, {against}, {abstain})"
        );
    }
}

#[test]
fn threshold_reached_strict_majority_limit() {
    // Setup cases (in favor, against, expected result)
    let cases = [(2500, 2499, true), (2501, 2500, false)];

    for (in_favor, against, expected) in cases {
        // Check the result over the votes cast excluding abstentions
        assert_eq!(
            threshold_reached(in_favor, STRICT_MAJORITY_THRESHOLD, in_favor + against),
            expected,
            "votes: ({in_favor}, {against})"
        );
    }
}

#[test]
fn vote_cfg_decode_stored_jsonb() {
    // Setup cases (stored pass threshold base, expected pass threshold base)
    let cases = [
        (None, None),
        (
            Some(json!("allowed_voters")),
            Some(PassThresholdBase::AllowedVoters),
        ),
        (
            Some(json!({"votes_cast": {}})),
            Some(PassThresholdBase::VotesCast {
                exclude_abstentions: None,
            }),
        ),
        (
            Some(json!({"votes_cast": {"exclude_abstentions": false}})),
            Some(PassThresholdBase::VotesCast {
                exclude_abstentions: Some(false),
            }),
        ),
        (
            Some(json!({"votes_cast": {"exclude_abstentions": true}})),
            Some(PassThresholdBase::VotesCast {
                exclude_abstentions: Some(true),
            }),
        ),
    ];

    for (stored_base, expected_base) in cases {
        // Setup JSONB value as stored in the vote cfg column (version byte + JSON)
        let mut stored = json!({
            "duration": "5m",
            "pass_threshold": 50.0,
            "allowed_voters": {"users": [USER1]},
            "close_on_passing": true
        });
        if let Some(stored_base) = &stored_base {
            stored["pass_threshold_base"] = stored_base.clone();
        }
        let mut raw = vec![1_u8];
        raw.extend(serde_json::to_vec(&stored).unwrap());

        // Check it decodes into the expected profile, as done when reading votes
        let Json(cfg) = <Json<CfgProfile> as FromSql>::from_sql(&Type::JSONB, &raw).unwrap();
        assert_eq!(
            cfg,
            CfgProfile {
                duration: Duration::from_mins(5),
                pass_threshold: 50.0,
                allowed_voters: Some(AllowedVoters {
                    users: Some(vec![USER1.to_string()]),
                    ..Default::default()
                }),
                close_on_passing: Some(true),
                pass_threshold_base: expected_base,
                ..Default::default()
            },
            "stored: {stored_base:?}"
        );
    }
}

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
            pass_rule: None,
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
fn vote_results_passes_with_all_allowed_voters() {
    // Setup cases (in favor, allowed voters, pass threshold, expected result)
    let cases = [
        (0, 0, 50.0, false),
        (1, 2, 50.0, true),
        (1, 3, 50.0, false),
        (29, 50, 58.0, true),
        (28, 50, 58.0, false),
        (2, 3, 66.67, false),
        (3, 3, 100.0, true),
    ];

    for (in_favor, allowed_voters, pass_threshold, expected) in cases {
        // Setup results
        let results = VoteResults {
            pass_threshold,
            in_favor,
            allowed_voters,
            ..setup_test_vote_results()
        };

        // Check the result over all the allowed voters
        assert_eq!(
            results.passes_with_all_allowed_voters(),
            expected,
            "in favor: {in_favor}, allowed voters: {allowed_voters}, pass threshold: {pass_threshold}"
        );
    }
}

#[tokio::test]
async fn calculate_vote_count_rule_requires_approvals_and_limits_rejections() {
    let cfg = CfgProfile {
        pass_threshold: 0.0,
        pass_rule: Some(PassRule::VoteCount {
            minimum_approvals: 3,
            maximum_rejections: 0,
        }),
        ..setup_test_vote().cfg
    };

    let passed = calculate_results(cfg.clone(), reactions(&votes_cast(3, 0, 0)), users(4)).await;
    assert!(passed.passed);
    assert!(!passed.passes_with_all_allowed_voters());

    let failed = calculate_results(cfg, reactions(&votes_cast(3, 1, 0)), users(4)).await;
    assert!(!failed.passed);
}

#[test]
fn vote_count_rule_only_closes_early_when_pending_votes_cannot_make_it_fail() {
    let pass_rule = Some(PassRule::VoteCount {
        minimum_approvals: 3,
        maximum_rejections: 1,
    });
    let safe = VoteResults {
        in_favor: 3,
        against: 1,
        not_voted: 0,
        pass_rule: pass_rule.clone(),
        ..setup_test_vote_results()
    };
    assert!(safe.passes_with_all_allowed_voters());

    let pending_rejection_could_fail = VoteResults {
        not_voted: 1,
        pass_rule,
        ..safe
    };
    assert!(!pending_rejection_could_fail.passes_with_all_allowed_voters());
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

// Helpers.

/// Calculate the results of a vote using the configuration, reactions and
/// allowed voters provided.
async fn calculate_results(
    cfg: CfgProfile,
    reactions: Vec<Reaction>,
    allowed_voters: Vec<UserName>,
) -> VoteResults {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_comment_reactions()
        .with(eq(INST_ID), eq(OWNER), eq(REPO), eq(COMMENT_ID))
        .times(1)
        .return_once(move |_, _, _, _| Box::pin(future::ready(Ok(reactions))));
    let expected_cfg = cfg.clone();
    gh.expect_get_allowed_voters()
        .withf(move |inst_id, cfg, owner, repo, org| {
            *inst_id == INST_ID
                && *cfg == expected_cfg
                && owner == OWNER
                && repo == REPO
                && *org == Some(ORG.to_string()).as_ref()
        })
        .times(1)
        .return_once(move |_, _, _, _, _| Box::pin(future::ready(Ok(allowed_voters))));

    // Calculate vote results
    let vote = Vote {
        cfg,
        ..setup_test_vote()
    };
    calculate(Arc::new(gh), OWNER, REPO, &vote).await.unwrap()
}

/// Build the expected results of a vote from the votes cast and allowed voters
/// provided, using the outcome given (passed, in favor and against percentages).
fn expected_results(
    pass_threshold: f64,
    votes_cast: &[(UserName, VoteOption)],
    allowed_voters: &[UserName],
    (passed, in_favor_percentage, against_percentage): (bool, f64, f64),
) -> VoteResults {
    // Prepare votes, which are binding when cast by allowed voters (matched
    // case insensitively)
    let is_allowed_voter =
        |user: &UserName| allowed_voters.iter().any(|voter| voter.eq_ignore_ascii_case(user));
    let timestamp = OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap();
    let votes: BTreeMap<UserName, UserVote> = votes_cast
        .iter()
        .map(|(user, vote_option)| {
            let user_vote = UserVote {
                vote_option: vote_option.clone(),
                timestamp,
                binding: is_allowed_voter(user),
            };
            (user.clone(), user_vote)
        })
        .collect();

    // Prepare counts
    let count_binding = |option: Option<VoteOption>| {
        votes
            .values()
            .filter(|v| v.binding && option.as_ref().is_none_or(|o| *o == v.vote_option))
            .count() as i64
    };
    let binding = count_binding(None);
    let pending_voters: Vec<UserName> = allowed_voters
        .iter()
        .filter(|voter| !votes.keys().any(|user| user.eq_ignore_ascii_case(voter)))
        .cloned()
        .collect();

    VoteResults {
        passed,
        in_favor_percentage,
        pass_threshold,
        pass_rule: None,
        in_favor: count_binding(Some(VoteOption::InFavor)),
        against: count_binding(Some(VoteOption::Against)),
        against_percentage,
        abstain: count_binding(Some(VoteOption::Abstain)),
        not_voted: pending_voters.len() as i64,
        binding,
        non_binding: votes.len() as i64 - binding,
        allowed_voters: allowed_voters.len() as i64,
        votes,
        pending_voters,
    }
}

/// Setup a reaction from the user provided with the content given.
fn reaction(user: &str, content: &str) -> Reaction {
    Reaction {
        user: User {
            login: user.to_string(),
        },
        content: content.to_string(),
        created_at: TIMESTAMP.to_string(),
    }
}

/// Setup reactions for the votes provided.
fn reactions(votes: &[(UserName, VoteOption)]) -> Vec<Reaction> {
    votes
        .iter()
        .map(|(user, vote_option)| {
            let content = match vote_option {
                VoteOption::InFavor => REACTION_IN_FAVOR,
                VoteOption::Against => REACTION_AGAINST,
                VoteOption::Abstain => REACTION_ABSTAIN,
            };
            reaction(user, content)
        })
        .collect()
}

/// Setup a profile using the pass threshold provided and the default pass
/// threshold base.
fn threshold_cfg(pass_threshold: f64) -> CfgProfile {
    CfgProfile {
        duration: Duration::from_secs(1),
        pass_threshold,
        ..Default::default()
    }
}

/// Setup a list with the first users (`user1`, `user2`, ...).
fn users(count: usize) -> Vec<UserName> {
    (1..=count).map(|i| format!("user{i}")).collect()
}

/// Setup the votes cast by the first users: in favor first, then against and
/// then abstain (i.e. `(2, 1, 0)` means `user1` and `user2` in favor and `user3`
/// against).
fn votes_cast(in_favor: usize, against: usize, abstain: usize) -> Vec<(UserName, VoteOption)> {
    [
        (in_favor, VoteOption::InFavor),
        (against, VoteOption::Against),
        (abstain, VoteOption::Abstain),
    ]
    .into_iter()
    .flat_map(|(count, vote_option)| std::iter::repeat_n(vote_option, count))
    .enumerate()
    .map(|(i, vote_option)| (format!("user{}", i + 1), vote_option))
    .collect()
}

/// Setup a profile using the votes cast as the pass threshold base.
fn votes_cast_cfg(pass_threshold: f64, exclude_abstentions: Option<bool>) -> CfgProfile {
    CfgProfile {
        pass_threshold_base: Some(PassThresholdBase::VotesCast { exclude_abstentions }),
        ..threshold_cfg(pass_threshold)
    }
}
