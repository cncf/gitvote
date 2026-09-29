use std::{collections::BTreeMap, env, fs};

use askama::Template;
use serde_json::json;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

use crate::{
    cmd::CreateVoteInput,
    github::{Event, Reaction, User},
    results::{REACTION_ABSTAIN, REACTION_AGAINST, REACTION_IN_FAVOR, UserVote, VoteOption, VoteResults},
    testutil::*,
};

use super::*;

#[test]
fn test_audit() {
    // Setup votes
    let mut closed_vote = setup_test_audit_vote("2024-03-01T10:00:00Z");
    closed_vote.closed = true;
    closed_vote.closed_at = Some(OffsetDateTime::parse("2024-03-02T10:00:00Z", &Rfc3339).unwrap());
    closed_vote.results = Some(setup_test_vote_results());
    let mut open_vote = setup_test_audit_vote("2024-04-01T10:00:00Z");
    open_vote.vote_id = Uuid::parse_str(VOTE_ID2).unwrap();
    open_vote.issue_number = 2;
    open_vote.issue_title = None;
    open_vote.is_pull_request = true;

    // Render template and check the output
    let output = Audit::new(REPOFN.to_string(), vec![open_vote, closed_vote]).render().unwrap();
    check_golden_file("audit", &output);
}

#[test]
fn test_audit_no_votes() {
    // Render template and check the output
    let output = Audit::new(REPOFN.to_string(), vec![]).render().unwrap();
    check_golden_file("audit-no-votes", &output);
}

#[test]
fn test_audit_vote_details_closed_pr_passed() {
    // Setup vote and results
    let mut vote = setup_test_audit_vote("2024-03-01T10:00:00Z");
    vote.closed = true;
    vote.closed_at = Some(OffsetDateTime::parse("2024-03-02T10:00:00Z", &Rfc3339).unwrap());
    vote.is_pull_request = true;
    let results = VoteResults {
        passed: true,
        in_favor_percentage: 50.0,
        pass_threshold: 50.0,
        in_favor: 2,
        against: 1,
        against_percentage: 25.0,
        abstain: 1,
        not_voted: 0,
        binding: 4,
        non_binding: 1,
        allowed_voters: 4,
        votes: BTreeMap::from([
            user_vote("alice", VoteOption::InFavor, true),
            user_vote("bob", VoteOption::InFavor, true),
            user_vote("carol", VoteOption::Against, true),
            user_vote("dave", VoteOption::Abstain, true),
            user_vote("supporter", VoteOption::InFavor, false),
        ]),
        pending_voters: vec![],
    };

    // Render template and check the output
    let output = AuditVoteDetails {
        results: &results,
        vote: &vote,
    }
    .render()
    .unwrap();
    check_golden_file("audit-vote-details-closed-pr-passed", &output);
}

#[test]
fn test_audit_vote_details_open_issue_without_title() {
    // Setup vote and results
    let mut vote = setup_test_audit_vote("2024-03-01T10:00:00Z");
    vote.issue_title = None;
    let results = VoteResults {
        passed: false,
        in_favor_percentage: 0.0,
        pass_threshold: 50.0,
        in_favor: 0,
        against: 1,
        against_percentage: 50.0,
        abstain: 0,
        not_voted: 1,
        binding: 1,
        non_binding: 0,
        allowed_voters: 2,
        votes: BTreeMap::from([user_vote("alice", VoteOption::Against, true)]),
        pending_voters: vec!["bob".to_string()],
    };

    // Render template and check the output
    let output = AuditVoteDetails {
        results: &results,
        vote: &vote,
    }
    .render()
    .unwrap();
    check_golden_file("audit-vote-details-open-issue-without-title", &output);
}

#[allow(clippy::too_many_lines)]
#[test]
fn test_calculate_participation() {
    // Setup test votes
    let votes = vec![
        setup_test_vote_with_calculated_results(
            "2024-02-01T12:00:00Z",
            vec!["alice".to_string(), "bob".to_string(), "carol".to_string()],
            vec![
                Reaction {
                    content: REACTION_IN_FAVOR.to_string(),
                    created_at: "2024-02-01T12:00:00Z".to_string(),
                    user: User {
                        login: "alice".to_string(),
                    },
                },
                Reaction {
                    content: REACTION_AGAINST.to_string(),
                    created_at: "2024-02-01T12:00:00Z".to_string(),
                    user: User {
                        login: "bob".to_string(),
                    },
                },
                Reaction {
                    content: REACTION_IN_FAVOR.to_string(),
                    created_at: "2024-02-01T12:00:00Z".to_string(),
                    user: User {
                        login: "dave".to_string(),
                    },
                },
            ],
        ),
        setup_test_vote_with_calculated_results(
            "2024-05-15T10:00:00Z",
            vec!["alice".to_string(), "bob".to_string(), "carol".to_string()],
            vec![
                Reaction {
                    content: REACTION_ABSTAIN.to_string(),
                    created_at: "2024-05-15T10:00:00Z".to_string(),
                    user: User {
                        login: "alice".to_string(),
                    },
                },
                Reaction {
                    content: REACTION_IN_FAVOR.to_string(),
                    created_at: "2024-05-15T10:00:00Z".to_string(),
                    user: User {
                        login: "carol".to_string(),
                    },
                },
            ],
        ),
        setup_test_vote_with_calculated_results(
            "2025-03-10T09:30:00Z",
            vec!["alice".to_string(), "bob".to_string(), "carol".to_string()],
            vec![
                Reaction {
                    content: REACTION_ABSTAIN.to_string(),
                    created_at: "2025-03-10T09:30:00Z".to_string(),
                    user: User {
                        login: "alice".to_string(),
                    },
                },
                Reaction {
                    content: REACTION_IN_FAVOR.to_string(),
                    created_at: "2025-03-10T09:30:00Z".to_string(),
                    user: User {
                        login: "carol".to_string(),
                    },
                },
            ],
        ),
        setup_test_vote_with_calculated_results(
            "2025-06-20T15:45:00Z",
            vec!["alice".to_string(), "bob".to_string(), "carol".to_string()],
            vec![
                Reaction {
                    content: REACTION_IN_FAVOR.to_string(),
                    created_at: "2025-06-20T15:45:00Z".to_string(),
                    user: User {
                        login: "alice".to_string(),
                    },
                },
                Reaction {
                    content: REACTION_AGAINST.to_string(),
                    created_at: "2025-06-20T15:45:00Z".to_string(),
                    user: User {
                        login: "bob".to_string(),
                    },
                },
            ],
        ),
        setup_test_vote_with_calculated_results(
            "2023-12-15T09:30:00Z",
            vec!["alice".to_string(), "carol".to_string()],
            vec![Reaction {
                content: REACTION_AGAINST.to_string(),
                created_at: "2023-12-15T09:30:00Z".to_string(),
                user: User {
                    login: "alice".to_string(),
                },
            }],
        ),
    ];

    // Calculate participation
    let participation = Audit::calculate_participation_stats(&votes);

    // Check results match expected values
    let actual = serde_json::to_value(&participation).unwrap();
    let expected = json!({
        "2024": {
            "alice": {
                "not_voted": 0,
                "participation_percentage": 100.0,
                "votes_abstain": 1,
                "votes_against": 0,
                "votes_in_favor": 1
            },
            "bob": {
                "not_voted": 1,
                "participation_percentage": 50.0,
                "votes_abstain": 0,
                "votes_against": 1,
                "votes_in_favor": 0
            },
            "carol": {
                "not_voted": 1,
                "participation_percentage": 50.0,
                "votes_abstain": 0,
                "votes_against": 0,
                "votes_in_favor": 1
            }
        },
        "2025": {
            "alice": {
                "not_voted": 0,
                "participation_percentage": 100.0,
                "votes_abstain": 1,
                "votes_against": 0,
                "votes_in_favor": 1
            },
            "bob": {
                "not_voted": 1,
                "participation_percentage": 50.0,
                "votes_abstain": 0,
                "votes_against": 1,
                "votes_in_favor": 0
            },
            "carol": {
                "not_voted": 1,
                "participation_percentage": 50.0,
                "votes_abstain": 0,
                "votes_against": 0,
                "votes_in_favor": 1
            }
        }
    });
    assert_eq!(actual, expected);
}

#[test]
fn test_calculate_participation_ignores_votes_without_results() {
    // Setup vote without results
    let vote = setup_test_audit_vote("2024-03-01T10:00:00Z");

    // Check votes without results are ignored
    assert!(Audit::calculate_participation_stats(&[vote]).is_empty());
}

#[test]
fn test_config_not_found() {
    // Render template and check the output
    let tmpl = ConfigNotFound {};
    let output = tmpl.render().unwrap();
    check_golden_file("config-not-found", &output);
}

#[test]
fn test_config_profile_not_found() {
    // Render template and check the output
    let tmpl = ConfigProfileNotFound {};
    let output = tmpl.render().unwrap();
    check_golden_file("config-profile-not-found", &output);
}

#[test]
fn test_invalid_config() {
    // Render template and check the output
    let tmpl = InvalidConfig::new("Missing required field: pass_threshold");
    let output = tmpl.render().unwrap();
    check_golden_file("invalid-config", &output);
}

#[test]
fn test_no_vote_in_progress_issue() {
    // Render template and check the output
    let tmpl = NoVoteInProgress::new("testuser", false);
    let output = tmpl.render().unwrap();
    check_golden_file("no-vote-in-progress-issue", &output);
}

#[test]
fn test_no_vote_in_progress_pr() {
    // Render template and check the output
    let tmpl = NoVoteInProgress::new("testuser", true);
    let output = tmpl.render().unwrap();
    check_golden_file("no-vote-in-progress-pr", &output);
}

#[test]
fn test_non_binding_filter() {
    // Create a dummy struct that implements askama::Values
    struct DummyValues;
    impl askama::Values for DummyValues {
        fn get_value(&self, _: &str) -> Option<&(dyn std::any::Any + 'static)> {
            None
        }
    }

    let mut votes = BTreeMap::new();

    // Add some binding votes
    votes.insert(
        "alice".to_string(),
        UserVote {
            vote_option: VoteOption::InFavor,
            timestamp: OffsetDateTime::parse("2023-01-05T10:00:00Z", &Rfc3339).unwrap(),
            binding: true,
        },
    );

    // Add non-binding votes with different timestamps
    for i in 0..5 {
        votes.insert(
            format!("supporter{i}"),
            UserVote {
                vote_option: VoteOption::InFavor,
                timestamp: OffsetDateTime::parse(&format!("2023-01-05T{:02}:00:00Z", 11 + i), &Rfc3339)
                    .unwrap(),
                binding: false,
            },
        );
    }

    // Test with limit of 3
    let dummy_values = DummyValues;

    let filtered = filters::non_binding::default().with_max(&3).execute(&votes, &dummy_values).unwrap();
    assert_eq!(filtered.len(), 3);

    // Verify they are sorted by timestamp
    assert_eq!(filtered[0].0, "supporter0");
    assert_eq!(filtered[1].0, "supporter1");
    assert_eq!(filtered[2].0, "supporter2");

    // Test with limit larger than available non-binding votes
    let filtered = filters::non_binding::default().with_max(&10).execute(&votes, &dummy_values).unwrap();
    assert_eq!(filtered.len(), 5);
}

#[test]
fn test_vote_cancelled_issue() {
    // Render template and check the output
    let tmpl = VoteCancelled::new("testuser", false);
    let output = tmpl.render().unwrap();
    check_golden_file("vote-cancelled-issue", &output);
}

#[test]
fn test_vote_cancelled_pr() {
    // Render template and check the output
    let tmpl = VoteCancelled::new("testuser", true);
    let output = tmpl.render().unwrap();
    check_golden_file("vote-cancelled-pr", &output);
}

#[test]
fn test_vote_checked_recently() {
    // Render template and check the output
    let tmpl = VoteCheckedRecently {};
    let output = tmpl.render().unwrap();
    check_golden_file("vote-checked-recently", &output);
}

#[test]
fn test_vote_closed_announcement() {
    // Setup votes
    let mut votes = BTreeMap::new();
    votes.insert(
        "alice".to_string(),
        UserVote {
            vote_option: VoteOption::InFavor,
            timestamp: OffsetDateTime::parse("2023-01-04T10:00:00Z", &Rfc3339).unwrap(),
            binding: true,
        },
    );
    votes.insert(
        "bob".to_string(),
        UserVote {
            vote_option: VoteOption::InFavor,
            timestamp: OffsetDateTime::parse("2023-01-04T11:00:00Z", &Rfc3339).unwrap(),
            binding: true,
        },
    );
    votes.insert(
        "charlie".to_string(),
        UserVote {
            vote_option: VoteOption::Abstain,
            timestamp: OffsetDateTime::parse("2023-01-04T12:00:00Z", &Rfc3339).unwrap(),
            binding: true,
        },
    );

    // Setup results
    let results = VoteResults {
        passed: true,
        in_favor_percentage: 66.67,
        pass_threshold: 50.0,
        in_favor: 2,
        against: 0,
        against_percentage: 0.0,
        abstain: 1,
        not_voted: 0,
        binding: 3,
        non_binding: 0,
        allowed_voters: 3,
        votes: votes.into_iter().collect(),
        pending_voters: vec![],
    };

    // Render template and check the output
    let tmpl = VoteClosedAnnouncement::new(123, "Implement RFC-42", &results);
    let output = tmpl.render().unwrap();
    check_golden_file("vote-closed-announcement", &output);
}

#[test]
fn test_vote_closed_failed() {
    // Setup votes
    let mut votes = BTreeMap::new();
    votes.insert(
        "alice".to_string(),
        UserVote {
            vote_option: VoteOption::Against,
            timestamp: OffsetDateTime::parse("2023-01-02T10:00:00Z", &Rfc3339).unwrap(),
            binding: true,
        },
    );
    votes.insert(
        "bob".to_string(),
        UserVote {
            vote_option: VoteOption::InFavor,
            timestamp: OffsetDateTime::parse("2023-01-02T11:00:00Z", &Rfc3339).unwrap(),
            binding: true,
        },
    );
    votes.insert(
        "charlie".to_string(),
        UserVote {
            vote_option: VoteOption::Against,
            timestamp: OffsetDateTime::parse("2023-01-02T12:00:00Z", &Rfc3339).unwrap(),
            binding: true,
        },
    );
    votes.insert(
        "dave".to_string(),
        UserVote {
            vote_option: VoteOption::InFavor,
            timestamp: OffsetDateTime::parse("2023-01-02T13:00:00Z", &Rfc3339).unwrap(),
            binding: true,
        },
    );
    votes.insert(
        "eve".to_string(),
        UserVote {
            vote_option: VoteOption::Against,
            timestamp: OffsetDateTime::parse("2023-01-02T14:00:00Z", &Rfc3339).unwrap(),
            binding: true,
        },
    );

    // Setup results
    let results = VoteResults {
        passed: false,
        in_favor_percentage: 40.0,
        pass_threshold: 50.0,
        in_favor: 2,
        against: 3,
        against_percentage: 60.0,
        abstain: 0,
        not_voted: 0,
        binding: 5,
        non_binding: 0,
        allowed_voters: 5,
        votes: votes.into_iter().collect(),
        pending_voters: vec![],
    };

    // Render template and check the output
    let tmpl = VoteClosed::new(&results);
    let output = tmpl.render().unwrap();
    check_golden_file("vote-closed-failed", &output);
}

#[test]
fn test_vote_closed_no_votes() {
    // Setup results
    let results = VoteResults {
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
        allowed_voters: 2,
        votes: BTreeMap::new(),
        pending_voters: vec!["alice".to_string(), "bob".to_string()],
    };

    // Render template and check the output
    let output = VoteClosed::new(&results).render().unwrap();
    check_golden_file("vote-closed-no-votes", &output);
}

#[test]
fn test_vote_closed_non_binding_only() {
    // Setup results
    let results = VoteResults {
        passed: false,
        in_favor_percentage: 0.0,
        pass_threshold: 50.0,
        in_favor: 0,
        against: 0,
        against_percentage: 0.0,
        abstain: 0,
        not_voted: 1,
        binding: 0,
        non_binding: 2,
        allowed_voters: 1,
        votes: BTreeMap::from([
            user_vote("supporter1", VoteOption::InFavor, false),
            user_vote("supporter2", VoteOption::Against, false),
        ]),
        pending_voters: vec!["alice".to_string()],
    };

    // Render template and check the output
    let output = VoteClosed::new(&results).render().unwrap();
    check_golden_file("vote-closed-non-binding-only", &output);
}

#[test]
fn test_vote_closed_non_binding_truncated() {
    // Setup results
    let results = setup_test_results_with_many_non_binding_votes();

    // Render template and check the output
    let output = VoteClosed::new(&results).render().unwrap();
    check_golden_file("vote-closed-non-binding-truncated", &output);
}

#[test]
fn test_vote_closed_passed() {
    // Setup votes
    let mut votes = BTreeMap::new();
    votes.insert(
        "alice".to_string(),
        UserVote {
            vote_option: VoteOption::InFavor,
            timestamp: OffsetDateTime::parse("2023-01-01T10:00:00Z", &Rfc3339).unwrap(),
            binding: true,
        },
    );
    votes.insert(
        "bob".to_string(),
        UserVote {
            vote_option: VoteOption::InFavor,
            timestamp: OffsetDateTime::parse("2023-01-01T11:00:00Z", &Rfc3339).unwrap(),
            binding: true,
        },
    );
    votes.insert(
        "charlie".to_string(),
        UserVote {
            vote_option: VoteOption::Against,
            timestamp: OffsetDateTime::parse("2023-01-01T12:00:00Z", &Rfc3339).unwrap(),
            binding: true,
        },
    );
    votes.insert(
        "dave".to_string(),
        UserVote {
            vote_option: VoteOption::InFavor,
            timestamp: OffsetDateTime::parse("2023-01-01T13:00:00Z", &Rfc3339).unwrap(),
            binding: true,
        },
    );
    votes.insert(
        "eve".to_string(),
        UserVote {
            vote_option: VoteOption::InFavor,
            timestamp: OffsetDateTime::parse("2023-01-01T14:00:00Z", &Rfc3339).unwrap(),
            binding: true,
        },
    );
    votes.insert(
        "supporter1".to_string(),
        UserVote {
            vote_option: VoteOption::InFavor,
            timestamp: OffsetDateTime::parse("2023-01-01T15:00:00Z", &Rfc3339).unwrap(),
            binding: false,
        },
    );
    votes.insert(
        "supporter2".to_string(),
        UserVote {
            vote_option: VoteOption::InFavor,
            timestamp: OffsetDateTime::parse("2023-01-01T16:00:00Z", &Rfc3339).unwrap(),
            binding: false,
        },
    );

    // Setup results
    let results = VoteResults {
        passed: true,
        in_favor_percentage: 80.0,
        pass_threshold: 50.0,
        in_favor: 4,
        against: 1,
        against_percentage: 20.0,
        abstain: 0,
        not_voted: 0,
        binding: 5,
        non_binding: 2,
        allowed_voters: 5,
        votes: votes.into_iter().collect(),
        pending_voters: vec![],
    };

    // Render template and check the output
    let tmpl = VoteClosed::new(&results);
    let output = tmpl.render().unwrap();
    check_golden_file("vote-closed-passed", &output);
}

#[test]
fn test_vote_created_all_collaborators() {
    // Setup input and configuration
    let event = Event::Issue(setup_test_issue_event());
    let input = CreateVoteInput::new(None, &event);
    let cfg = CfgProfile {
        duration: std::time::Duration::from_hours(24), // 1 day
        pass_threshold: 75.0,
        ..Default::default()
    };

    // Render template and check the output
    let tmpl = VoteCreated::new(&input, &cfg);
    let output = tmpl.render().unwrap();
    check_golden_file("vote-created-all-collaborators", &output);
}

#[test]
fn test_vote_created_users_only() {
    // Setup input and configuration
    let input = CreateVoteInput::new(None, &Event::Issue(setup_test_issue_event()));
    let cfg = CfgProfile {
        duration: std::time::Duration::from_hours(24),
        pass_threshold: 66.0,
        allowed_voters: Some(crate::cfg_repo::AllowedVoters {
            users: Some(vec!["alice".into(), "bob".into()]),
            ..Default::default()
        }),
        ..Default::default()
    };

    // Render template and check the output
    let output = VoteCreated::new(&input, &cfg).render().unwrap();
    check_golden_file("vote-created-users-only", &output);
}

#[test]
fn test_vote_created_votes_cast() {
    // Setup input and configuration
    let input = CreateVoteInput::new(None, &Event::Issue(setup_test_issue_event()));
    let cfg = CfgProfile {
        duration: std::time::Duration::from_hours(24),
        pass_threshold: 60.0,
        pass_threshold_base: Some(PassThresholdBase::VotesCast {
            exclude_abstentions: None,
        }),
        ..Default::default()
    };

    // Render template and check the output
    let output = VoteCreated::new(&input, &cfg).render().unwrap();
    check_golden_file("vote-created-votes-cast", &output);
}

#[test]
fn test_vote_created_votes_cast_excluding_abstentions() {
    // Setup input and configuration
    let input = CreateVoteInput::new(None, &Event::Issue(setup_test_issue_event()));
    let cfg = CfgProfile {
        duration: std::time::Duration::from_hours(24),
        pass_threshold: 50.01,
        pass_threshold_base: Some(PassThresholdBase::VotesCast {
            exclude_abstentions: Some(true),
        }),
        ..Default::default()
    };

    // Render template and check the output
    let output = VoteCreated::new(&input, &cfg).render().unwrap();
    check_golden_file("vote-created-votes-cast-excluding-abstentions", &output);
}

#[test]
fn test_vote_created_votes_cast_omitted_exclude_abstentions_equals_false() {
    // Setup input and render the template with the flag omitted and set to false
    let input = CreateVoteInput::new(None, &Event::Issue(setup_test_issue_event()));
    let outputs: Vec<String> = [None, Some(false)]
        .into_iter()
        .map(|exclude_abstentions| {
            let cfg = CfgProfile {
                duration: std::time::Duration::from_hours(24),
                pass_threshold: 60.0,
                pass_threshold_base: Some(PassThresholdBase::VotesCast { exclude_abstentions }),
                ..Default::default()
            };
            VoteCreated::new(&input, &cfg).render().unwrap()
        })
        .collect();

    // Check both outputs are identical
    assert_eq!(outputs[0], outputs[1]);
}

#[test]
fn test_vote_created_with_teams_and_users() {
    // Setup input and configuration
    let mut event = setup_test_issue_event();
    event.issue.title = "Add new feature X".to_string();
    event.issue.number = 42;
    let event = Event::Issue(event);
    let input = CreateVoteInput::new(None, &event);

    let cfg = CfgProfile {
        duration: std::time::Duration::from_hours(72), // 3 days
        pass_threshold: 51.0,
        allowed_voters: Some(crate::cfg_repo::AllowedVoters {
            teams: Some(vec!["core-team".into(), "maintainers".into()]),
            users: Some(vec!["alice".into(), "bob".into()]),
            exclude_team_maintainers: None,
        }),
        ..Default::default()
    };

    // Render template and check the output
    let tmpl = VoteCreated::new(&input, &cfg);
    let output = tmpl.render().unwrap();
    check_golden_file("vote-created-with-teams-and-users", &output);
}

#[test]
fn test_vote_in_progress_issue() {
    // Render template and check the output
    let tmpl = VoteInProgress::new("testuser", false);
    let output = tmpl.render().unwrap();
    check_golden_file("vote-in-progress-issue", &output);
}

#[test]
fn test_vote_in_progress_pr() {
    // Render template and check the output
    let tmpl = VoteInProgress::new("testuser", true);
    let output = tmpl.render().unwrap();
    check_golden_file("vote-in-progress-pr", &output);
}

#[test]
fn test_vote_restricted() {
    // Render template and check the output
    let tmpl = VoteRestricted::new("testuser");
    let output = tmpl.render().unwrap();
    check_golden_file("vote-restricted", &output);
}

#[test]
fn test_vote_status_in_progress() {
    // Setup votes
    let mut votes = BTreeMap::new();
    votes.insert(
        "alice".to_string(),
        UserVote {
            vote_option: VoteOption::InFavor,
            timestamp: OffsetDateTime::parse("2023-01-03T10:00:00Z", &Rfc3339).unwrap(),
            binding: true,
        },
    );
    votes.insert(
        "bob".to_string(),
        UserVote {
            vote_option: VoteOption::Abstain,
            timestamp: OffsetDateTime::parse("2023-01-03T11:00:00Z", &Rfc3339).unwrap(),
            binding: true,
        },
    );
    votes.insert(
        "supporter".to_string(),
        UserVote {
            vote_option: VoteOption::InFavor,
            timestamp: OffsetDateTime::parse("2023-01-03T12:00:00Z", &Rfc3339).unwrap(),
            binding: false,
        },
    );

    // Setup results
    let results = VoteResults {
        passed: false,
        in_favor_percentage: 33.33,
        pass_threshold: 50.0,
        in_favor: 1,
        against: 0,
        against_percentage: 0.0,
        abstain: 1,
        not_voted: 1,
        binding: 2,
        non_binding: 1,
        allowed_voters: 3,
        votes: votes.into_iter().collect(),
        pending_voters: vec!["charlie".to_string()],
    };

    // Render template and check the output
    let tmpl = VoteStatus::new(&results);
    let output = tmpl.render().unwrap();
    check_golden_file("vote-status-in-progress", &output);
}

#[test]
fn test_vote_status_non_binding_truncated() {
    // Setup results
    let results = setup_test_results_with_many_non_binding_votes();

    // Render template and check the output
    let output = VoteStatus::new(&results).render().unwrap();
    check_golden_file("vote-status-non-binding-truncated", &output);
}

// Helpers.

/// Check the output matches the golden file, regenerating it when requested.
fn check_golden_file(name: &str, actual: &str) {
    if env::var("REGENERATE_GOLDEN_FILES").is_ok() {
        write_golden_file(name, actual);
    } else {
        let expected = read_golden_file(name);
        assert_eq!(actual, expected, "output does not match golden file ({name})");
    }
}

/// Get the path of the golden file with the name provided.
fn golden_file_path(name: &str) -> String {
    format!("{TESTDATA_PATH}/templates/{name}.golden")
}

/// Read the content of the golden file with the name provided.
fn read_golden_file(name: &str) -> String {
    let path = golden_file_path(name);
    fs::read_to_string(&path).unwrap_or_else(|_| panic!("error reading golden file: {path}"))
}

/// Setup a vote with deterministic timestamps created at the time provided.
fn setup_test_audit_vote(created_at: &str) -> Vote {
    let created_at = OffsetDateTime::parse(created_at, &Rfc3339).unwrap();
    Vote {
        created_at,
        ends_at: created_at + time::Duration::days(7),
        ..setup_test_vote()
    }
}

/// Setup results with more non-binding votes than the comments can display.
fn setup_test_results_with_many_non_binding_votes() -> VoteResults {
    // Setup non-binding votes with newer votes on lower usernames
    let first_timestamp = OffsetDateTime::parse("2023-01-01T00:00:00Z", &Rfc3339).unwrap();
    let mut votes: BTreeMap<String, UserVote> = (0..302)
        .map(|i| {
            (
                format!("supporter{:03}", 301 - i),
                UserVote {
                    vote_option: VoteOption::InFavor,
                    timestamp: first_timestamp + time::Duration::minutes(i),
                    binding: false,
                },
            )
        })
        .collect();
    let (user, binding_vote) = user_vote("alice", VoteOption::InFavor, true);
    votes.insert(user, binding_vote);

    VoteResults {
        passed: true,
        in_favor_percentage: 100.0,
        pass_threshold: 50.0,
        in_favor: 1,
        against: 0,
        against_percentage: 0.0,
        abstain: 0,
        not_voted: 0,
        binding: 1,
        non_binding: 302,
        allowed_voters: 1,
        votes,
        pending_voters: vec![],
    }
}

/// Create a user vote entry with a deterministic timestamp.
fn user_vote(user: &str, vote_option: VoteOption, binding: bool) -> (String, UserVote) {
    (
        user.to_string(),
        UserVote {
            vote_option,
            timestamp: OffsetDateTime::parse("2024-03-01T12:00:00Z", &Rfc3339).unwrap(),
            binding,
        },
    )
}

/// Write the content provided to the golden file with the name provided.
fn write_golden_file(name: &str, content: &str) {
    let path = golden_file_path(name);
    fs::write(&path, content).expect("write golden file should succeed");
}
