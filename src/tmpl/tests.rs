use std::{collections::BTreeMap, env, fs};

use askama::Template;
use serde_json::json;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

use crate::{
    cfg_repo::AllowedVoters,
    cmd::CreateVoteInput,
    github::{Event, Reaction, User},
    results::{REACTION_ABSTAIN, REACTION_AGAINST, REACTION_IN_FAVOR, UserVote, VoteOption, VoteResults},
    testutil::*,
};

use super::*;

/// Truncation notice as it must be rendered in the non-binding votes section.
const TRUNCATION_NOTICE: &str = "</summary>

<i>(displaying only the first 300 non-binding votes)</i>

| User | Vote  | Timestamp |";

#[test]
fn test_audit() {
    // Setup votes
    let mut closed_vote = setup_test_audit_vote("2024-03-01T10:00:00Z");
    closed_vote.closed = true;
    closed_vote.closed_at = Some(OffsetDateTime::parse("2024-03-02T10:00:00Z", &Rfc3339).unwrap());
    closed_vote.results = Some(setup_test_vote_results());
    let mut open_vote = setup_test_audit_vote("2024-04-01T10:00:00Z");
    open_vote.is_pull_request = true;
    open_vote.issue_number = 2;
    open_vote.vote_id = Uuid::parse_str(VOTE_ID2).unwrap();
    open_vote.issue_title = None;

    // Render template and check the output
    let output = Audit::new(REPOFN.to_string(), vec![open_vote, closed_vote]).render().unwrap();
    check_golden_file("audit", &output);
}

#[test]
fn test_audit_closed_vote_without_results() {
    // Setup closed vote without results
    let mut vote = setup_test_audit_vote("2024-03-01T10:00:00Z");
    vote.closed = true;
    vote.closed_at = Some(OffsetDateTime::parse("2024-03-02T10:00:00Z", &Rfc3339).unwrap());
    let vote_id = vote.vote_id;

    // Render template
    let output = Audit::new(REPOFN.to_string(), vec![vote]).render().unwrap();

    // Check the details link is kept and no outcome is displayed
    assert!(output.contains(&format!(r#"hx-get="/audit/{REPOFN}/vote/{vote_id}""#)));
    assert!(!output.contains(r#"<span class="badge pending">Pending</span>"#));
    assert!(!output.contains(r#"<span class="badge">Pending</span>"#));
}

#[test]
fn test_audit_failed_vote_displays_failed_badges() {
    // Setup closed vote that got votes in favor but did not pass
    let results = VoteResults {
        against: 3,
        against_percentage: 60.0,
        allowed_voters: 5,
        binding: 5,
        in_favor: 2,
        in_favor_percentage: 40.0,
        pass_threshold: 50.0,
        passed: false,
        ..setup_test_vote_results()
    };
    let vote = setup_test_audit_closed_vote("2024-03-01T10:00:00Z", results);

    // Render template
    let output = Audit::new(REPOFN.to_string(), vec![vote]).render().unwrap();

    // Check the status, outcome and modal badges all say the vote failed
    assert_eq!(
        output.matches(r#"<span class="badge fail">Failed</span>"#).count(),
        3
    );
    assert!(!output.contains(r#"<span class="badge pass">"#));
}

#[test]
fn test_audit_no_votes() {
    // Render template and check the output
    let output = Audit::new(REPOFN.to_string(), vec![]).render().unwrap();
    check_golden_file("audit-no-votes", &output);
}

#[test]
fn test_audit_participation_displays_each_count_in_its_column() {
    // Setup a binding vote per option cast by the first user in the latest year
    let vote_options = [
        (VoteOption::InFavor, 4),
        (VoteOption::Against, 3),
        (VoteOption::Abstain, 2),
    ];
    let mut votes: Vec<Vote> = vote_options
        .into_iter()
        .flat_map(|(vote_option, count)| std::iter::repeat_n(vote_option, count))
        .map(|vote_option| {
            let results = VoteResults {
                votes: BTreeMap::from([user_vote("alice", vote_option, true)]),
                ..setup_test_vote_results()
            };
            setup_test_audit_closed_vote("2025-03-01T10:00:00Z", results)
        })
        .collect();

    // Setup votes where the same user did not vote
    let results = VoteResults {
        pending_voters: vec!["alice".to_string()],
        votes: BTreeMap::new(),
        ..setup_test_vote_results()
    };
    votes.extend(std::iter::repeat_n(
        setup_test_audit_closed_vote("2025-04-01T10:00:00Z", results),
        5,
    ));

    // Setup a vote from another user in the previous year
    let results = VoteResults {
        votes: BTreeMap::from([user_vote("bob", VoteOption::InFavor, true)]),
        ..setup_test_vote_results()
    };
    votes.push(setup_test_audit_closed_vote("2024-03-01T10:00:00Z", results));

    // Render template
    let output = Audit::new(REPOFN.to_string(), votes).render().unwrap();

    // Check the latest year is displayed first
    let latest_year = output.find(r#"participation-year__title">2025<"#).unwrap();
    let previous_year = output.find(r#"participation-year__title">2024<"#).unwrap();
    assert!(latest_year < previous_year);

    // Check the latest year displays each count in its own column
    let latest_year_output = &output[latest_year..previous_year];
    for cell in [
        "<td>alice</td>",
        r#"data-label="Voted">9</td>"#,
        r#"data-label="Not voted">5</td>"#,
        r#"<span class="percentage-full">64.29</span>"#,
        r#"<span class="percentage-mobile">64</span>"#,
        r#"data-label="In favor">4</td>"#,
        r#"data-label="Against">3</td>"#,
        r#"data-label="Abstain">2</td>"#,
    ] {
        assert!(latest_year_output.contains(cell), "cell: {cell}");
    }
    assert!(!latest_year_output.contains("<td>bob</td>"));

    // Check the previous year only displays its own voter
    let previous_year_output = &output[previous_year..];
    assert!(previous_year_output.contains("<td>bob</td>"));
    assert!(previous_year_output.contains(r#"data-label="Voted">1</td>"#));
    assert!(!previous_year_output.contains("<td>alice</td>"));
}

#[test]
fn test_audit_user_controlled_values_are_escaped() {
    // Setup cases (name, setter of the value, value, expected escaped value)
    type Setter = fn(&mut Vote, &str);
    let cases: [(&str, Setter, &str, &str); 4] = [
        (
            "issue title",
            |vote, value| vote.issue_title = Some(value.to_string()),
            r#"<script>alert("x")</script> & 'q'"#,
            "&#60;script&#62;alert(&#34;x&#34;)&#60;/script&#62; &#38; &#39;q&#39;",
        ),
        (
            "created by",
            |vote, value| vote.created_by = value.to_string(),
            "<b>creator</b>",
            "&#60;b&#62;creator&#60;/b&#62;",
        ),
        (
            "voter",
            |vote, value| {
                let (_, user_vote) = user_vote(value, VoteOption::InFavor, true);
                let results = vote.results.as_mut().unwrap();
                results.votes = BTreeMap::from([(value.to_string(), user_vote)]);
            },
            "<img src=x>",
            "&#60;img src=x&#62;",
        ),
        (
            "pending voter",
            |vote, value| vote.results.as_mut().unwrap().pending_voters = vec![value.to_string()],
            "<i>pend</i>",
            "&#60;i&#62;pend&#60;/i&#62;",
        ),
    ];

    for (name, set_value, value, escaped_value) in cases {
        // Setup closed vote with the user controlled value provided
        let results = setup_test_vote_results();
        let mut vote = setup_test_audit_closed_vote("2024-03-01T10:00:00Z", results);
        set_value(&mut vote, value);

        // Render the audit page and the vote details fragment
        let details = AuditVoteDetails {
            vote: &vote,

            results: vote.results.as_ref(),
        };
        let details_output = details.render().unwrap();
        let audit_output = Audit::new(REPOFN.to_string(), vec![vote.clone()]).render().unwrap();

        // Check the value is escaped in both
        for output in [audit_output, details_output] {
            assert!(!output.contains(value), "{name} not escaped");
            assert!(output.contains(escaped_value), "{name} not displayed");
        }
    }
}

#[test]
fn test_audit_vote_details_closed_pr_passed() {
    // Setup vote and results
    let mut vote = setup_test_audit_vote("2024-03-01T10:00:00Z");
    vote.closed = true;
    vote.is_pull_request = true;
    vote.closed_at = Some(OffsetDateTime::parse("2024-03-02T10:00:00Z", &Rfc3339).unwrap());
    let results = VoteResults {
        abstain: 1,
        against: 1,
        against_percentage: 25.0,
        allowed_voters: 4,
        binding: 4,
        in_favor: 2,
        in_favor_percentage: 50.0,
        non_binding: 1,
        not_voted: 0,
        pass_threshold: 50.0,
        passed: true,
        pending_voters: vec![],
        votes: BTreeMap::from([
            user_vote("alice", VoteOption::InFavor, true),
            user_vote("bob", VoteOption::InFavor, true),
            user_vote("carol", VoteOption::Against, true),
            user_vote("dave", VoteOption::Abstain, true),
            user_vote("supporter", VoteOption::InFavor, false),
        ]),
    };

    // Render template and check the output
    let output = AuditVoteDetails {
        vote: &vote,

        results: Some(&results),
    }
    .render()
    .unwrap();
    check_golden_file("audit-vote-details-closed-pr-passed", &output);
}

#[test]
fn test_audit_vote_details_closed_without_results() {
    // Setup closed vote without results
    let mut vote = setup_test_audit_vote("2024-03-01T10:00:00Z");
    vote.closed = true;
    vote.closed_at = Some(OffsetDateTime::parse("2024-03-02T10:00:00Z", &Rfc3339).unwrap());

    // Render template and check the output
    let tmpl = AuditVoteDetails {
        vote: &vote,

        results: None,
    };
    let output = tmpl.render().unwrap();
    check_golden_file("audit-vote-details-closed-without-results", &output);
}

#[test]
fn test_audit_vote_details_open_issue_without_title() {
    // Setup vote and results
    let mut vote = setup_test_audit_vote("2024-03-01T10:00:00Z");
    vote.issue_title = None;
    let results = VoteResults {
        abstain: 0,
        against: 1,
        against_percentage: 50.0,
        allowed_voters: 2,
        binding: 1,
        in_favor: 0,
        in_favor_percentage: 0.0,
        non_binding: 0,
        not_voted: 1,
        pass_threshold: 50.0,
        passed: false,
        pending_voters: vec!["bob".to_string()],
        votes: BTreeMap::from([user_vote("alice", VoteOption::Against, true)]),
    };

    // Render template and check the output
    let output = AuditVoteDetails {
        vote: &vote,

        results: Some(&results),
    }
    .render()
    .unwrap();
    check_golden_file("audit-vote-details-open-issue-without-title", &output);
}

#[test]
fn test_automation_failed() {
    // Render template and check the output
    let tmpl = AutomationFailed::new(None);
    let output = tmpl.render().unwrap();
    check_golden_file("automation-failed", &output);
}

#[test]
fn test_automation_failed_with_details() {
    // Render template and check the output
    let details = InvalidConfig::new("Missing required field: pass_threshold").render().unwrap();
    let tmpl = AutomationFailed::new(Some(&details));
    let output = tmpl.render().unwrap();
    check_golden_file("automation-failed-details", &output);
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
fn test_calculate_participation_includes_votes_at_tracking_start() {
    // Setup vote created exactly when tracking starts
    let vote =
        setup_test_vote_with_calculated_results("2024-01-01T00:00:00Z", vec!["alice".to_string()], vec![]);

    // Check the vote is counted
    let participation = Audit::calculate_participation_stats(&[vote]);
    assert_eq!(participation[&2024]["alice"].not_voted, 1);
}

#[test]
fn test_calculate_participation_merges_pending_voters_case_variants() {
    // Setup votes where the username is always pending with a different spelling
    let votes = [
        setup_test_vote_with_calculated_results("2024-02-01T12:00:00Z", vec!["alice".to_string()], vec![]),
        setup_test_vote_with_calculated_results("2024-03-01T12:00:00Z", vec!["ALICE".to_string()], vec![]),
    ];

    // Calculate participation
    let participation = Audit::calculate_participation_stats(&votes);

    // Check the first spelling is used for a single entry
    let actual = serde_json::to_value(&participation).unwrap();
    let expected = json!({
        "2024": {
            "alice": {
                "not_voted": 2,
                "participation_percentage": 0.0,
                "votes_abstain": 0,
                "votes_against": 0,
                "votes_in_favor": 0
            }
        }
    });
    assert_eq!(actual, expected);
}

#[test]
fn test_calculate_participation_merges_username_case_variants() {
    // Setup votes with a reaction from each spelling, and a vote with the user pending
    let voted = setup_test_vote_with_alice_reaction("2024-02-01T12:00:00Z", "Alice");
    let voted_lowercase = setup_test_vote_with_alice_reaction("2024-04-01T12:00:00Z", "alice");
    let pending =
        setup_test_vote_with_calculated_results("2024-03-01T12:00:00Z", vec!["alice".to_string()], vec![]);

    // Setup cases (votes, expected spelling, votes in favor, not voted, participation)
    let cases = [
        // The login is used over the pending spelling, whatever the votes order
        ([voted.clone(), pending.clone()], "Alice", 1, 1, 50.0),
        ([pending, voted.clone()], "Alice", 1, 1, 50.0),
        // Different logins use the first one
        ([voted.clone(), voted_lowercase.clone()], "Alice", 2, 0, 100.0),
        ([voted_lowercase, voted], "alice", 2, 0, 100.0),
    ];

    for (votes, username, votes_in_favor, not_voted, participation_percentage) in cases {
        // Calculate participation
        let participation = Audit::calculate_participation_stats(&votes);

        // Check there is a single entry
        let actual = serde_json::to_value(&participation).unwrap();
        let expected = json!({
            "2024": {
                username: {
                    "not_voted": not_voted,
                    "participation_percentage": participation_percentage,
                    "votes_abstain": 0,
                    "votes_against": 0,
                    "votes_in_favor": votes_in_favor
                }
            }
        });
        assert_eq!(actual, expected);
    }
}

#[test]
fn test_command_failed() {
    // Render template and check the output
    let tmpl = CommandFailed {};
    let output = tmpl.render().unwrap();
    check_golden_file("command-failed", &output);
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
    // Setup the values passed to the filter, which it does not use
    struct DummyValues;
    impl askama::Values for DummyValues {
        fn get_value(&self, _: &str) -> Option<&(dyn std::any::Any + 'static)> {
            None
        }
    }
    let dummy_values = DummyValues;

    // Setup a binding vote
    let mut votes = BTreeMap::new();
    votes.insert(
        "alice".to_string(),
        UserVote {
            binding: true,
            timestamp: OffsetDateTime::parse("2023-01-05T10:00:00Z", &Rfc3339).unwrap(),
            vote_option: VoteOption::InFavor,
        },
    );

    // Setup non-binding votes with different timestamps
    for i in 0..5 {
        votes.insert(
            format!("supporter{i}"),
            UserVote {
                binding: false,
                timestamp: OffsetDateTime::parse(&format!("2023-01-05T{:02}:00:00Z", 11 + i), &Rfc3339)
                    .unwrap(),
                vote_option: VoteOption::InFavor,
            },
        );
    }

    // Filter with a limit lower than the non-binding votes available
    let filtered = filters::non_binding::default().with_max(&3).execute(&votes, &dummy_values).unwrap();

    // Check only the first non-binding votes are returned, sorted by timestamp
    assert_eq!(filtered.len(), 3);
    assert_eq!(filtered[0].0, "supporter0");
    assert_eq!(filtered[1].0, "supporter1");
    assert_eq!(filtered[2].0, "supporter2");

    // Filter with a limit larger than the non-binding votes available
    let filtered = filters::non_binding::default().with_max(&10).execute(&votes, &dummy_values).unwrap();

    // Check all the non-binding votes are returned
    assert_eq!(filtered.len(), 5);
}

#[test]
fn test_vote_cancelled_issue() {
    // Render template and check the output
    let tmpl = VoteCancelled::new(Uuid::parse_str(VOTE_ID).unwrap(), "testuser", false);
    let output = tmpl.render().unwrap();
    check_golden_file("vote-cancelled-issue", &output);
}

#[test]
fn test_vote_cancelled_pr() {
    // Render template and check the output
    let tmpl = VoteCancelled::new(Uuid::parse_str(VOTE_ID).unwrap(), "testuser", true);
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
            binding: true,
            timestamp: OffsetDateTime::parse("2023-01-04T10:00:00Z", &Rfc3339).unwrap(),
            vote_option: VoteOption::InFavor,
        },
    );
    votes.insert(
        "bob".to_string(),
        UserVote {
            binding: true,
            timestamp: OffsetDateTime::parse("2023-01-04T11:00:00Z", &Rfc3339).unwrap(),
            vote_option: VoteOption::InFavor,
        },
    );
    votes.insert(
        "charlie".to_string(),
        UserVote {
            binding: true,
            timestamp: OffsetDateTime::parse("2023-01-04T12:00:00Z", &Rfc3339).unwrap(),
            vote_option: VoteOption::Abstain,
        },
    );

    // Setup results
    let results = VoteResults {
        abstain: 1,
        against: 0,
        against_percentage: 0.0,
        allowed_voters: 3,
        binding: 3,
        in_favor: 2,
        in_favor_percentage: 66.67,
        non_binding: 0,
        not_voted: 0,
        pass_threshold: 50.0,
        passed: true,
        pending_voters: vec![],
        votes: votes.into_iter().collect(),
    };

    // Render template and check the output
    let tmpl = VoteClosedAnnouncement::new(
        Uuid::parse_str(VOTE_ID).unwrap(),
        123,
        "Implement RFC-42",
        &results,
    );
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
            binding: true,
            timestamp: OffsetDateTime::parse("2023-01-02T10:00:00Z", &Rfc3339).unwrap(),
            vote_option: VoteOption::Against,
        },
    );
    votes.insert(
        "bob".to_string(),
        UserVote {
            binding: true,
            timestamp: OffsetDateTime::parse("2023-01-02T11:00:00Z", &Rfc3339).unwrap(),
            vote_option: VoteOption::InFavor,
        },
    );
    votes.insert(
        "charlie".to_string(),
        UserVote {
            binding: true,
            timestamp: OffsetDateTime::parse("2023-01-02T12:00:00Z", &Rfc3339).unwrap(),
            vote_option: VoteOption::Against,
        },
    );
    votes.insert(
        "dave".to_string(),
        UserVote {
            binding: true,
            timestamp: OffsetDateTime::parse("2023-01-02T13:00:00Z", &Rfc3339).unwrap(),
            vote_option: VoteOption::InFavor,
        },
    );
    votes.insert(
        "eve".to_string(),
        UserVote {
            binding: true,
            timestamp: OffsetDateTime::parse("2023-01-02T14:00:00Z", &Rfc3339).unwrap(),
            vote_option: VoteOption::Against,
        },
    );

    // Setup results
    let results = VoteResults {
        abstain: 0,
        against: 3,
        against_percentage: 60.0,
        allowed_voters: 5,
        binding: 5,
        in_favor: 2,
        in_favor_percentage: 40.0,
        non_binding: 0,
        not_voted: 0,
        pass_threshold: 50.0,
        passed: false,
        pending_voters: vec![],
        votes: votes.into_iter().collect(),
    };

    // Render template and check the output
    let tmpl = VoteClosed::new(Uuid::parse_str(VOTE_ID).unwrap(), &results);
    let output = tmpl.render().unwrap();
    check_golden_file("vote-closed-failed", &output);
}

#[test]
fn test_vote_closed_no_votes() {
    // Setup results
    let results = VoteResults {
        abstain: 0,
        against: 0,
        against_percentage: 0.0,
        allowed_voters: 2,
        binding: 0,
        in_favor: 0,
        in_favor_percentage: 0.0,
        non_binding: 0,
        not_voted: 2,
        pass_threshold: 50.0,
        passed: false,
        pending_voters: vec!["alice".to_string(), "bob".to_string()],
        votes: BTreeMap::new(),
    };

    // Render template and check the output
    let output = VoteClosed::new(Uuid::parse_str(VOTE_ID).unwrap(), &results).render().unwrap();
    check_golden_file("vote-closed-no-votes", &output);
}

#[test]
fn test_vote_closed_non_binding_only() {
    // Setup results
    let results = VoteResults {
        abstain: 0,
        against: 0,
        against_percentage: 0.0,
        allowed_voters: 1,
        binding: 0,
        in_favor: 0,
        in_favor_percentage: 0.0,
        non_binding: 2,
        not_voted: 1,
        pass_threshold: 50.0,
        passed: false,
        pending_voters: vec!["alice".to_string()],
        votes: BTreeMap::from([
            user_vote("supporter1", VoteOption::InFavor, false),
            user_vote("supporter2", VoteOption::Against, false),
        ]),
    };

    // Render template and check the output
    let output = VoteClosed::new(Uuid::parse_str(VOTE_ID).unwrap(), &results).render().unwrap();
    check_golden_file("vote-closed-non-binding-only", &output);
}

#[test]
fn test_vote_closed_non_binding_truncated() {
    // Setup results
    let results = setup_test_results_with_many_non_binding_votes();

    // Render template and check the output
    let output = VoteClosed::new(Uuid::parse_str(VOTE_ID).unwrap(), &results).render().unwrap();
    check_golden_file("vote-closed-non-binding-truncated", &output);

    // Check the notice is a paragraph, as any indentation would make it a code block
    assert!(output.contains(TRUNCATION_NOTICE), "unexpected output: {output}");
}

#[test]
fn test_vote_closed_passed() {
    // Setup votes
    let mut votes = BTreeMap::new();
    votes.insert(
        "alice".to_string(),
        UserVote {
            binding: true,
            timestamp: OffsetDateTime::parse("2023-01-01T10:00:00Z", &Rfc3339).unwrap(),
            vote_option: VoteOption::InFavor,
        },
    );
    votes.insert(
        "bob".to_string(),
        UserVote {
            binding: true,
            timestamp: OffsetDateTime::parse("2023-01-01T11:00:00Z", &Rfc3339).unwrap(),
            vote_option: VoteOption::InFavor,
        },
    );
    votes.insert(
        "charlie".to_string(),
        UserVote {
            binding: true,
            timestamp: OffsetDateTime::parse("2023-01-01T12:00:00Z", &Rfc3339).unwrap(),
            vote_option: VoteOption::Against,
        },
    );
    votes.insert(
        "dave".to_string(),
        UserVote {
            binding: true,
            timestamp: OffsetDateTime::parse("2023-01-01T13:00:00Z", &Rfc3339).unwrap(),
            vote_option: VoteOption::InFavor,
        },
    );
    votes.insert(
        "eve".to_string(),
        UserVote {
            binding: true,
            timestamp: OffsetDateTime::parse("2023-01-01T14:00:00Z", &Rfc3339).unwrap(),
            vote_option: VoteOption::InFavor,
        },
    );
    votes.insert(
        "supporter1".to_string(),
        UserVote {
            binding: false,
            timestamp: OffsetDateTime::parse("2023-01-01T15:00:00Z", &Rfc3339).unwrap(),
            vote_option: VoteOption::InFavor,
        },
    );
    votes.insert(
        "supporter2".to_string(),
        UserVote {
            binding: false,
            timestamp: OffsetDateTime::parse("2023-01-01T16:00:00Z", &Rfc3339).unwrap(),
            vote_option: VoteOption::InFavor,
        },
    );

    // Setup results
    let results = VoteResults {
        abstain: 0,
        against: 1,
        against_percentage: 20.0,
        allowed_voters: 5,
        binding: 5,
        in_favor: 4,
        in_favor_percentage: 80.0,
        non_binding: 2,
        not_voted: 0,
        pass_threshold: 50.0,
        passed: true,
        pending_voters: vec![],
        votes: votes.into_iter().collect(),
    };

    // Render template and check the output
    let tmpl = VoteClosed::new(Uuid::parse_str(VOTE_ID).unwrap(), &results);
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
fn test_vote_created_allowed_voters_lists() {
    // Setup cases (teams, users, expected to list the team)
    let cases = [
        (Some(vec!["core".to_string()]), None, true),
        (Some(vec!["core".to_string()]), Some(vec![]), true),
        (Some(vec![]), Some(vec![]), false),
        (None, None, false),
    ];

    for (teams, users, expected_team) in cases {
        // Setup input and configuration
        let input = CreateVoteInput::new(None, &Event::Issue(setup_test_issue_event()));
        let cfg = CfgProfile {
            duration: std::time::Duration::from_hours(24),
            pass_threshold: 75.0,

            allowed_voters: Some(AllowedVoters {
                teams: teams.clone(),
                users: users.clone(),
                ..Default::default()
            }),
            ..Default::default()
        };

        // Render template
        let output = VoteCreated::new(&input, &cfg).render().unwrap();

        // Check the comment never says all collaborators can vote when a team is listed
        let context = format!("teams: {teams:?}, users: {users:?}, output: {output}");
        assert_eq!(output.contains("| @org/core |"), expected_team, "{context}");
        assert_eq!(
            output.contains("The members of the following teams have binding votes:"),
            expected_team,
            "{context}"
        );
        assert_eq!(
            output.contains("All repository collaborators have binding votes."),
            !expected_team,
            "{context}"
        );
        assert!(
            !output.contains("The following users have binding votes:"),
            "{context}"
        );
    }
}

#[test]
fn test_vote_created_title_code_span() {
    // Setup cases (issue title, expected code span)
    let cases = [
        ("Plain title", "`Plain title`"),
        ("Rename `foo` to bar", "``Rename `foo` to bar``"),
        ("Rename `foo`", "`` Rename `foo` ``"),
        ("`foo` is renamed", "`` `foo` is renamed ``"),
        ("`foo`", "`` `foo` ``"),
        ("One ` backtick", "``One ` backtick``"),
        ("Use ``foo`` or `bar` here", "```Use ``foo`` or `bar` here```"),
    ];

    for (title, expected) in cases {
        // Setup input and configuration
        let mut event = setup_test_issue_event();
        event.issue.title = title.to_string();
        let input = CreateVoteInput::new(None, &Event::Issue(event));
        let cfg = CfgProfile {
            duration: std::time::Duration::from_hours(24), // 1 day
            pass_threshold: 75.0,
            ..Default::default()
        };

        // Render template and check the title
        let output = VoteCreated::new(&input, &cfg).render().unwrap();
        assert!(
            output.contains(&format!("has called for a vote on {expected} (#{ISSUE_NUM}).")),
            "title: {title}, output: {output}"
        );
    }
}

#[test]
fn test_vote_created_users_only() {
    // Setup input and configuration
    let input = CreateVoteInput::new(None, &Event::Issue(setup_test_issue_event()));
    let cfg = CfgProfile {
        duration: std::time::Duration::from_hours(24),
        pass_threshold: 66.0,

        allowed_voters: Some(AllowedVoters {
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
    event.issue.number = 42;
    event.issue.title = "Add new feature X".to_string();
    let event = Event::Issue(event);
    let input = CreateVoteInput::new(None, &event);
    let cfg = CfgProfile {
        duration: std::time::Duration::from_hours(72), // 3 days
        pass_threshold: 51.0,

        allowed_voters: Some(AllowedVoters {
            exclude_team_maintainers: None,
            teams: Some(vec!["core-team".into(), "maintainers".into()]),
            users: Some(vec!["alice".into(), "bob".into()]),
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
fn test_vote_marker_render_identifies_vote_and_step() {
    // Setup cases (marker, expected rendering)
    let vote_id = Uuid::parse_str(VOTE_ID).unwrap();
    let cases = [
        (
            VoteMarker::CancellationComment,
            "<!-- gitvote:vote-cancelled:00000000-0000-0000-0000-000000000001 -->",
        ),
        (
            VoteMarker::ResultsAnnouncement,
            "<!-- gitvote:vote-closed-announcement:00000000-0000-0000-0000-000000000001 -->",
        ),
        (
            VoteMarker::ResultsComment,
            "<!-- gitvote:vote-closed:00000000-0000-0000-0000-000000000001 -->",
        ),
    ];

    // Check each marker identifies the vote and the step
    for (marker, expected) in cases {
        assert_eq!(marker.render(vote_id), expected, "{marker:?}");
    }
}

#[test]
fn test_vote_restricted() {
    // Render template and check the output
    let tmpl = VoteRestricted::new("testuser");
    let output = tmpl.render().unwrap();
    check_golden_file("vote-restricted", &output);
}

#[test]
fn test_vote_status_binding_votes_only() {
    // Setup results with a binding vote and nobody else left to vote
    let results = VoteResults {
        abstain: 0,
        against: 1,
        against_percentage: 100.0,
        allowed_voters: 1,
        binding: 1,
        in_favor: 0,
        in_favor_percentage: 0.0,
        non_binding: 0,
        not_voted: 0,
        pass_threshold: 50.0,
        passed: false,
        pending_voters: vec![],
        votes: BTreeMap::from([user_vote("bob", VoteOption::Against, true)]),
    };

    // Render template
    let output = VoteStatus::new(&results).render().unwrap();

    // Check the output has the binding vote only
    assert!(output.contains("### Binding votes (1)"));
    assert!(output.contains("| bob | Against |"));
    assert!(!output.contains("*Pending*"));
    assert!(!output.contains("<details>"));
    assert!(!output.contains("Non-binding votes"));
}

#[test]
fn test_vote_status_in_progress() {
    // Setup votes
    let mut votes = BTreeMap::new();
    votes.insert(
        "alice".to_string(),
        UserVote {
            binding: true,
            timestamp: OffsetDateTime::parse("2023-01-03T10:00:00Z", &Rfc3339).unwrap(),
            vote_option: VoteOption::InFavor,
        },
    );
    votes.insert(
        "bob".to_string(),
        UserVote {
            binding: true,
            timestamp: OffsetDateTime::parse("2023-01-03T11:00:00Z", &Rfc3339).unwrap(),
            vote_option: VoteOption::Abstain,
        },
    );
    votes.insert(
        "supporter".to_string(),
        UserVote {
            binding: false,
            timestamp: OffsetDateTime::parse("2023-01-03T12:00:00Z", &Rfc3339).unwrap(),
            vote_option: VoteOption::InFavor,
        },
    );

    // Setup results
    let results = VoteResults {
        abstain: 1,
        against: 0,
        against_percentage: 0.0,
        allowed_voters: 3,
        binding: 2,
        in_favor: 1,
        in_favor_percentage: 33.33,
        non_binding: 1,
        not_voted: 1,
        pass_threshold: 50.0,
        passed: false,
        pending_voters: vec!["charlie".to_string()],
        votes: votes.into_iter().collect(),
    };

    // Render template and check the output
    let tmpl = VoteStatus::new(&results);
    let output = tmpl.render().unwrap();
    check_golden_file("vote-status-in-progress", &output);
}

#[test]
fn test_vote_status_no_votes() {
    // Setup results with no votes yet
    let results = VoteResults {
        abstain: 0,
        against: 0,
        against_percentage: 0.0,
        allowed_voters: 2,
        binding: 0,
        in_favor: 0,
        in_favor_percentage: 0.0,
        non_binding: 0,
        not_voted: 2,
        pass_threshold: 50.0,
        passed: false,
        pending_voters: vec!["alice".to_string(), "bob".to_string()],
        votes: BTreeMap::new(),
    };

    // Render template
    let output = VoteStatus::new(&results).render().unwrap();

    // Check the output has the pending voters only
    assert!(output.contains("### Binding votes (0)"));
    assert!(output.contains("| @alice | *Pending* |"));
    assert!(output.contains("| @bob | *Pending* |"));
    assert!(!output.contains("<details>"));
    assert!(!output.contains("Non-binding votes"));
}

#[test]
fn test_vote_status_non_binding_truncated() {
    // Setup results
    let results = setup_test_results_with_many_non_binding_votes();

    // Render template and check the output
    let output = VoteStatus::new(&results).render().unwrap();
    check_golden_file("vote-status-non-binding-truncated", &output);

    // Check the notice is a paragraph, as any indentation would make it a code block
    assert!(output.contains(TRUNCATION_NOTICE), "unexpected output: {output}");
}

// Helpers.

/// Checks the output matches the golden file, regenerating it when requested.
fn check_golden_file(name: &str, actual: &str) {
    if env::var("REGENERATE_GOLDEN_FILES").is_ok() {
        write_golden_file(name, actual);
    } else {
        let expected = read_golden_file(name);
        assert_eq!(actual, expected, "output does not match golden file ({name})");
    }
}

/// Returns the path of the golden file with the name provided.
fn golden_file_path(name: &str) -> String {
    format!("{TESTDATA_PATH}/templates/{name}.golden")
}

/// Reads the content of the golden file with the name provided.
fn read_golden_file(name: &str) -> String {
    let path = golden_file_path(name);
    fs::read_to_string(&path).unwrap_or_else(|_| panic!("error reading golden file: {path}"))
}

/// Sets up a closed vote with the results provided, created at the time provided.
fn setup_test_audit_closed_vote(created_at: &str, results: VoteResults) -> Vote {
    let vote = setup_test_audit_vote(created_at);
    Vote {
        closed: true,

        closed_at: Some(vote.ends_at),
        results: Some(results),
        ..vote
    }
}

/// Sets up a vote with deterministic timestamps created at the time provided.
fn setup_test_audit_vote(created_at: &str) -> Vote {
    let created_at = OffsetDateTime::parse(created_at, &Rfc3339).unwrap();
    Vote {
        created_at,
        ends_at: created_at + time::Duration::days(7),
        ..setup_test_vote()
    }
}

/// Sets up results with more non-binding votes than the comments can display.
fn setup_test_results_with_many_non_binding_votes() -> VoteResults {
    // Setup non-binding votes with newer votes on lower usernames
    let first_timestamp = OffsetDateTime::parse("2023-01-01T00:00:00Z", &Rfc3339).unwrap();
    let mut votes: BTreeMap<String, UserVote> = (0..302)
        .map(|i| {
            (
                format!("supporter{:03}", 301 - i),
                UserVote {
                    binding: false,
                    timestamp: first_timestamp + time::Duration::minutes(i),
                    vote_option: VoteOption::InFavor,
                },
            )
        })
        .collect();

    // Setup a single binding vote
    let (user, binding_vote) = user_vote("alice", VoteOption::InFavor, true);
    votes.insert(user, binding_vote);

    // Prepare the results
    VoteResults {
        abstain: 0,
        against: 0,
        against_percentage: 0.0,
        allowed_voters: 1,
        binding: 1,
        in_favor: 1,
        in_favor_percentage: 100.0,
        non_binding: 302,
        not_voted: 0,
        pass_threshold: 50.0,
        passed: true,
        pending_voters: vec![],
        votes,
    }
}

/// Sets up a vote where the only allowed voter, `alice`, voted in favor using the login
/// provided.
fn setup_test_vote_with_alice_reaction(created_at: &str, login: &str) -> Vote {
    setup_test_vote_with_calculated_results(
        created_at,
        vec!["alice".to_string()],
        vec![Reaction {
            content: REACTION_IN_FAVOR.to_string(),
            created_at: created_at.to_string(),
            user: User {
                login: login.to_string(),
            },
        }],
    )
}

/// Creates a user vote entry with a deterministic timestamp.
fn user_vote(user: &str, vote_option: VoteOption, binding: bool) -> (String, UserVote) {
    (
        user.to_string(),
        UserVote {
            binding,
            timestamp: OffsetDateTime::parse("2024-03-01T12:00:00Z", &Rfc3339).unwrap(),
            vote_option,
        },
    )
}

/// Writes the content provided to the golden file with the name provided.
fn write_golden_file(name: &str, content: &str) {
    let path = golden_file_path(name);
    fs::write(&path, content).expect("write golden file should succeed");
}
