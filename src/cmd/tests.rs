use std::sync::Arc;

use anyhow::format_err;
use futures::future;
use mockall::predicate::eq;

use crate::{
    cfg_repo::CfgError,
    db::MockDB,
    github::{File, MockGH, Organization, PullRequestInIssue},
    testutil::*,
};

use super::*;

// Commands and their inputs.

#[tokio::test]
async fn automatic_command_from_issue_event_is_ignored() {
    // Setup event (no GitHub calls expected)
    let gh = Arc::new(MockGH::new());
    let mut event = setup_test_issue_event();
    event.action = IssueEventAction::Opened;
    let event = Event::Issue(event);

    // Run and check no command is created
    assert_eq!(Command::from_event_automatic(gh, &event).await.unwrap(), None);
}

#[tokio::test]
async fn automatic_command_from_pr_event() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_valid_config(&mut gh);
    expect_pr_files(
        &mut gh,
        Ok(vec![File {
            filename: "README.md".to_string(),
        }]),
    );
    let gh = Arc::new(gh);

    // Setup event
    let mut event = setup_test_pr_event();
    event.action = PullRequestEventAction::Opened;
    let event = Event::PullRequest(event);

    // Run and check the automatic command is created
    assert_eq!(
        Command::from_event_automatic(gh, &event.clone()).await.unwrap(),
        Some(Command::CreateVote(CreateVoteInput::new_automatic(
            "default", &event
        )))
    );
}

#[tokio::test]
async fn automatic_command_from_pr_event_automation_disabled() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_config_file(
        &mut gh,
        r#"
automation:
  enabled: false
  rules:
    - patterns: ["*.md"]
      profile: default
profiles:
  default:
    duration: 5m
    pass_threshold: 50
"#,
    );
    gh.expect_get_pr_files().never();

    // Setup event
    let event = setup_test_pr_opened_event();

    // Run and check no command is created
    assert_eq!(
        Command::from_event_automatic(Arc::new(gh), &event).await.unwrap(),
        None
    );
}

#[tokio::test]
async fn automatic_command_from_pr_event_automation_not_configured() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_config_file(
        &mut gh,
        r"
profiles:
  default:
    duration: 5m
    pass_threshold: 50
",
    );
    gh.expect_get_pr_files().never();

    // Setup event
    let event = setup_test_pr_opened_event();

    // Run and check no command is created
    assert_eq!(
        Command::from_event_automatic(Arc::new(gh), &event).await.unwrap(),
        None
    );
}

#[tokio::test]
async fn automatic_command_from_pr_event_automation_without_rules() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_config_file(
        &mut gh,
        r"
automation:
  enabled: true
  rules: []
profiles:
  default:
    duration: 5m
    pass_threshold: 50
",
    );
    gh.expect_get_pr_files().never();

    // Setup event
    let event = setup_test_pr_opened_event();

    // Run and check no command is created
    assert_eq!(
        Command::from_event_automatic(Arc::new(gh), &event).await.unwrap(),
        None
    );
}

#[tokio::test]
async fn automatic_command_from_pr_event_config_not_found() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_no_config(&mut gh);
    gh.expect_get_pr_files().never();

    // Setup event
    let event = setup_test_pr_opened_event();

    // Run and check no command is created
    assert_eq!(
        Command::from_event_automatic(Arc::new(gh), &event).await.unwrap(),
        None
    );
}

#[tokio::test]
async fn automatic_command_from_pr_event_error_getting_config() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    gh.expect_get_pr_files().never();

    // Setup event
    let event = setup_test_pr_opened_event();

    // Run and check the error is propagated instead of reporting no command
    let err = Command::from_event_automatic(Arc::new(gh), &event).await.unwrap_err();
    assert!(matches!(
        err.downcast_ref::<CfgError>(),
        Some(CfgError::ConfigLookup(_))
    ));
}

#[tokio::test]
async fn automatic_command_from_pr_event_error_getting_pr_files() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_valid_config(&mut gh);
    expect_pr_files(&mut gh, Err(format_err!(ERROR)));

    // Setup event
    let event = setup_test_pr_opened_event();

    // Run and check the error is propagated
    let err = Command::from_event_automatic(Arc::new(gh), &event).await.unwrap_err();
    assert_eq!(err.to_string(), ERROR);
}

#[tokio::test]
async fn automatic_command_from_pr_event_first_matching_rule_wins() {
    // Setup cases (patterns of each rule, files in the PR, expected profile)
    let cases = [
        (["*.md", "*.md"], vec!["README.md"], "default"),
        (["docs/", "*.md"], vec!["docs/guide.md"], "default"),
        (["*.md", "docs/**"], vec!["docs/guide.md"], "default"),
        (["*.md", "src/**"], vec!["src/main.rs", "README.md"], "default"),
        (["*.md", "src/**"], vec!["src/main.rs"], PROFILE_NAME),
    ];

    for ([first_pattern, second_pattern], files, expected_profile) in cases {
        // Setup GitHub expectations
        let mut gh = MockGH::new();
        expect_config_content(
            &mut gh,
            format!(
                r#"
automation:
  enabled: true
  rules:
    - patterns: ["{first_pattern}"]
      profile: default
    - patterns: ["{second_pattern}"]
      profile: profile1
profiles:
  default:
    duration: 5m
    pass_threshold: 50
  profile1:
    duration: 10m
    pass_threshold: 75
"#
            ),
        );
        let pr_files = files.iter().map(|filename| File {
            filename: (*filename).to_string(),
        });
        expect_pr_files(&mut gh, Ok(pr_files.collect()));

        // Setup event
        let event = setup_test_pr_opened_event();

        // Run and check the profile of the first rule that matches is used
        assert_eq!(
            Command::from_event_automatic(Arc::new(gh), &event).await.unwrap(),
            Some(Command::CreateVote(CreateVoteInput::new_automatic(
                expected_profile,
                &event
            ))),
            "patterns: {first_pattern}, {second_pattern}, files: {files:?}"
        );
    }
}

#[tokio::test]
async fn automatic_command_from_pr_event_invalid_config() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_config_content(&mut gh, get_test_invalid_config());
    gh.expect_get_pr_files().never();

    // Setup event
    let event = setup_test_pr_opened_event();

    // Run and check the configuration error is propagated
    let err = Command::from_event_automatic(Arc::new(gh), &event).await.unwrap_err();
    assert!(matches!(
        err.downcast_ref::<CfgError>(),
        Some(CfgError::InvalidConfig(_))
    ));
}

#[tokio::test]
async fn automatic_command_from_pr_event_no_matching_files() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_valid_config(&mut gh);
    expect_pr_files(
        &mut gh,
        Ok(vec![File {
            filename: "src/main.rs".to_string(),
        }]),
    );

    // Setup event
    let event = setup_test_pr_opened_event();

    // Run and check no command is created
    assert_eq!(
        Command::from_event_automatic(Arc::new(gh), &event).await.unwrap(),
        None
    );
}

#[tokio::test]
async fn automatic_command_from_pr_event_second_rule_matches() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_config_file(
        &mut gh,
        r#"
automation:
  enabled: true
  rules:
    - patterns: ["*.md"]
      profile: default
    - patterns: ["src/**"]
      profile: profile1
profiles:
  default:
    duration: 5m
    pass_threshold: 50
  profile1:
    duration: 10m
    pass_threshold: 75
"#,
    );
    expect_pr_files(
        &mut gh,
        Ok(vec![File {
            filename: "src/main.rs".to_string(),
        }]),
    );

    // Setup event
    let event = setup_test_pr_opened_event();

    // Run and check the matching rule profile is used
    assert_eq!(
        Command::from_event_automatic(Arc::new(gh), &event).await.unwrap(),
        Some(Command::CreateVote(CreateVoteInput::new_automatic(
            PROFILE_NAME,
            &event
        )))
    );
}

#[tokio::test]
async fn automatic_command_from_pr_event_unsupported_action() {
    // Setup event (no GitHub calls expected)
    let gh = Arc::new(MockGH::new());
    let mut event = setup_test_pr_event();
    event.action = PullRequestEventAction::Synchronize;
    let event = Event::PullRequest(event);

    // Run and check no command is created
    assert_eq!(Command::from_event_automatic(gh, &event).await.unwrap(), None);
}

#[test]
fn cancel_vote_input_from_issue_comment_event_on_pr() {
    // Setup event
    let mut event = setup_test_issue_comment_event();
    event.installation.id = 1;
    event.issue.number = 2;
    event.issue.pull_request = Some(PullRequestInIssue {
        url: "https://api.github.com/repos/org/repo/pulls/2".to_string(),
    });

    // Check input
    assert_eq!(
        CancelVoteInput::new(&Event::IssueComment(event)),
        CancelVoteInput {
            cancelled_by: USER.to_string(),
            installation_id: 1,
            is_pull_request: true,
            issue_number: 2,
            repository_full_name: REPOFN.to_string(),
        }
    );
}

#[test]
fn cancel_vote_input_from_issue_event() {
    // Setup event
    let mut event = setup_test_issue_event();
    event.installation.id = 1;
    event.issue.number = 2;

    // Check input
    assert_eq!(
        CancelVoteInput::new(&Event::Issue(event)),
        CancelVoteInput {
            cancelled_by: USER.to_string(),
            installation_id: 1,
            is_pull_request: false,
            issue_number: 2,
            repository_full_name: REPOFN.to_string(),
        }
    );
}

#[test]
fn cancel_vote_input_from_pr_event() {
    // Setup event
    let mut event = setup_test_pr_event();
    event.installation.id = 1;
    event.pull_request.number = 2;

    // Check input
    assert_eq!(
        CancelVoteInput::new(&Event::PullRequest(event)),
        CancelVoteInput {
            cancelled_by: USER.to_string(),
            installation_id: 1,
            is_pull_request: true,
            issue_number: 2,
            repository_full_name: REPOFN.to_string(),
        }
    );
}

#[test]
fn check_vote_input_from_issue_comment_event() {
    // Setup event
    let mut event = setup_test_issue_comment_event();
    event.issue.number = 2;

    // Check input
    assert_eq!(
        CheckVoteInput::new(&Event::IssueComment(event)),
        CheckVoteInput {
            issue_number: 2,
            repository_full_name: REPOFN.to_string(),

            installation_id: Some(INST_ID as i64),
        }
    );
}

#[test]
fn check_vote_input_from_issue_event() {
    // Setup event
    let mut event = setup_test_issue_event();
    event.issue.number = 2;

    // Check input
    assert_eq!(
        CheckVoteInput::new(&Event::Issue(event)),
        CheckVoteInput {
            issue_number: 2,
            repository_full_name: REPOFN.to_string(),

            installation_id: Some(INST_ID as i64),
        }
    );
}

#[test]
fn check_vote_input_from_pr_event() {
    // Setup event
    let mut event = setup_test_pr_event();
    event.pull_request.number = 2;

    // Check input
    assert_eq!(
        CheckVoteInput::new(&Event::PullRequest(event)),
        CheckVoteInput {
            issue_number: 2,
            repository_full_name: REPOFN.to_string(),

            installation_id: Some(INST_ID as i64),
        }
    );
}

#[tokio::test]
async fn command_from_event_automatic_command() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_valid_config(&mut gh);
    expect_pr_files(
        &mut gh,
        Ok(vec![File {
            filename: "README.md".to_string(),
        }]),
    );

    // Setup event
    let event = setup_test_pr_opened_event();

    // Run and check the automatic command is returned
    assert_eq!(
        Command::from_event(Arc::new(gh), &event).await.unwrap(),
        Some(Command::CreateVote(CreateVoteInput::new_automatic(
            "default", &event
        )))
    );
}

#[tokio::test]
async fn command_from_event_automatic_command_error() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_config_content(&mut gh, get_test_invalid_config());
    gh.expect_get_pr_files().never();

    // Setup event
    let event = setup_test_pr_opened_event();

    // Run and check the error is returned, so that it is not mistaken for no command
    let err = Command::from_event(Arc::new(gh), &event).await.unwrap_err();
    assert!(matches!(
        err.downcast_ref::<CfgError>(),
        Some(CfgError::InvalidConfig(_))
    ));
}

#[tokio::test]
async fn command_from_event_manual_command_takes_precedence() {
    // Setup event with a manual command (no GitHub calls expected)
    let gh = Arc::new(MockGH::new());
    let mut event = setup_test_pr_event();
    event.action = PullRequestEventAction::Opened;
    event.pull_request.body = Some(format!("/{CMD_CREATE_VOTE}-{PROFILE_NAME}"));
    let event = Event::PullRequest(event);

    // Run and check the manual command is returned
    assert_eq!(
        Command::from_event(gh, &event).await.unwrap(),
        Some(Command::CreateVote(CreateVoteInput::new(
            Some(PROFILE_NAME),
            &event
        )))
    );
}

#[tokio::test]
async fn command_from_event_no_command() {
    // Setup event without command (no GitHub calls expected)
    let gh = Arc::new(MockGH::new());
    let mut event = setup_test_issue_comment_event();
    event.action = IssueCommentEventAction::Created;
    event.comment.body = Some("Hi!".to_string());
    let event = Event::IssueComment(event);

    // Run and check no command is returned
    assert_eq!(Command::from_event(gh, &event).await.unwrap(), None);
}

#[test]
fn create_vote_input_from_issue_comment_event() {
    // Setup event
    let mut event = setup_test_issue_comment_event();
    event.installation.id = 1;
    event.issue.id = 2;
    event.issue.number = 3;

    // Check input
    assert_eq!(
        CreateVoteInput::new(Some(PROFILE_NAME), &Event::IssueComment(event)),
        CreateVoteInput {
            created_by: USER.to_string(),
            installation_id: 1,
            is_automatic: false,
            is_pull_request: false,
            issue_id: 2,
            issue_number: 3,
            issue_title: TITLE.to_string(),
            repository_full_name: REPOFN.to_string(),

            opened_pr_base_branch: None,
            organization: Some(ORG.to_string()),
            profile_name: Some(PROFILE_NAME.to_string()),
        }
    );
}

#[test]
fn create_vote_input_from_issue_comment_event_on_pr() {
    // Setup event
    let mut event = setup_test_issue_comment_event();
    event.issue.pull_request = Some(PullRequestInIssue {
        url: "https://api.github.com/repos/org/repo/pulls/1".to_string(),
    });

    // Check input is flagged as pull request
    let input = CreateVoteInput::new(None, &Event::IssueComment(event));
    assert!(input.is_pull_request);
}

#[test]
fn create_vote_input_from_issue_event() {
    // Setup event
    let mut event = setup_test_issue_event();
    event.installation.id = 1;
    event.issue.id = 2;
    event.issue.number = 3;

    // Check input
    assert_eq!(
        CreateVoteInput::new(None, &Event::Issue(event)),
        CreateVoteInput {
            created_by: USER.to_string(),
            installation_id: 1,
            is_automatic: false,
            is_pull_request: false,
            issue_id: 2,
            issue_number: 3,
            issue_title: TITLE.to_string(),
            repository_full_name: REPOFN.to_string(),

            opened_pr_base_branch: None,
            organization: Some(ORG.to_string()),
            profile_name: None,
        }
    );
}

#[test]
fn create_vote_input_from_issue_event_without_organization() {
    // Setup event
    let mut event = setup_test_issue_event();
    event.organization = None;

    // Check input has no organization
    let input = CreateVoteInput::new(None, &Event::Issue(event));
    assert_eq!(input.organization, None);
}

#[test]
fn create_vote_input_from_pr_event() {
    // Setup event
    let mut event = setup_test_pr_event();
    event.installation.id = 1;
    event.pull_request.id = 2;
    event.pull_request.number = 3;
    event.organization = Some(Organization {
        login: "other-org".to_string(),
    });

    // Check input
    assert_eq!(
        CreateVoteInput::new(Some(PROFILE_NAME), &Event::PullRequest(event)),
        CreateVoteInput {
            created_by: USER.to_string(),
            installation_id: 1,
            is_automatic: false,
            is_pull_request: true,
            issue_id: 2,
            issue_number: 3,
            issue_title: TITLE.to_string(),
            repository_full_name: REPOFN.to_string(),

            opened_pr_base_branch: Some(BRANCH.to_string()),
            organization: Some("other-org".to_string()),
            profile_name: Some(PROFILE_NAME.to_string()),
        }
    );
}

#[test]
fn create_vote_input_new_automatic() {
    // Setup event
    let event = Event::PullRequest(setup_test_pr_event());

    // Check only the origin of the vote differs from the one of a manual command
    let input = CreateVoteInput::new_automatic(PROFILE_NAME, &event);
    assert!(input.is_automatic);
    assert_eq!(
        input,
        CreateVoteInput {
            is_automatic: true,
            ..CreateVoteInput::new(Some(PROFILE_NAME), &event)
        }
    );
}

#[test]
fn manual_command_from_issue_comment_event_cancel_vote_cmd() {
    // Setup event
    let mut event = setup_test_issue_comment_event();
    event.action = IssueCommentEventAction::Created;
    event.comment.body = Some(format!("/{CMD_CANCEL_VOTE}"));
    let event = Event::IssueComment(event);

    // Run and check the manual command is returned
    assert_eq!(
        Command::from_event_manual(&event),
        Some(Command::CancelVote(CancelVoteInput::new(&event)))
    );
}

#[test]
fn manual_command_from_issue_comment_event_check_vote_cmd() {
    // Setup event
    let mut event = setup_test_issue_comment_event();
    event.action = IssueCommentEventAction::Created;
    event.comment.body = Some(format!("/{CMD_CHECK_VOTE}"));
    let event = Event::IssueComment(event);

    // Run and check the manual command is returned
    assert_eq!(
        Command::from_event_manual(&event),
        Some(Command::CheckVote(CheckVoteInput::new(&event)))
    );
}

#[test]
fn manual_command_from_issue_comment_event_cmd_in_issue_body_is_ignored() {
    // Setup event with command only in issue body
    let mut event = setup_test_issue_comment_event();
    event.action = IssueCommentEventAction::Created;
    event.comment.body = Some("Hi!".to_string());
    event.issue.body = Some(format!("/{CMD_CREATE_VOTE}"));
    let event = Event::IssueComment(event);

    // Run and check no command is returned
    assert_eq!(Command::from_event_manual(&event), None);
}

#[test]
fn manual_command_from_issue_comment_event_cmd_not_at_line_start() {
    // Setup event with command not at line start
    let mut event = setup_test_issue_comment_event();
    event.action = IssueCommentEventAction::Created;
    event.comment.body = Some(format!("Please /{CMD_CREATE_VOTE}"));
    let event = Event::IssueComment(event);

    // Run and check no command is returned
    assert_eq!(Command::from_event_manual(&event), None);
}

#[test]
fn manual_command_from_issue_comment_event_cmd_on_later_line() {
    // Setup event with command on a later line
    let mut event = setup_test_issue_comment_event();
    event.action = IssueCommentEventAction::Created;
    event.comment.body = Some(format!("Let's vote on this\n\n/{CMD_CREATE_VOTE}\n"));
    let event = Event::IssueComment(event);

    // Run and check the manual command is returned
    assert_eq!(
        Command::from_event_manual(&event),
        Some(Command::CreateVote(CreateVoteInput::new(None, &event)))
    );
}

#[test]
fn manual_command_from_issue_comment_event_cmd_with_trailing_text() {
    // Setup event with command followed by text
    let mut event = setup_test_issue_comment_event();
    event.action = IssueCommentEventAction::Created;
    event.comment.body = Some(format!("/{CMD_CREATE_VOTE} now"));
    let event = Event::IssueComment(event);

    // Run and check no command is returned
    assert_eq!(Command::from_event_manual(&event), None);
}

#[test]
fn manual_command_from_issue_comment_event_cmd_with_trailing_whitespace() {
    // Setup event with command followed by whitespace
    let mut event = setup_test_issue_comment_event();
    event.action = IssueCommentEventAction::Created;
    event.comment.body = Some(format!("/{CMD_CREATE_VOTE}  \t"));
    let event = Event::IssueComment(event);

    // Run and check the manual command is returned
    assert_eq!(
        Command::from_event_manual(&event),
        Some(Command::CreateVote(CreateVoteInput::new(None, &event)))
    );
}

#[test]
fn manual_command_from_issue_comment_event_create_vote_cmd_default_profile() {
    // Setup event
    let mut event = setup_test_issue_comment_event();
    event.action = IssueCommentEventAction::Created;
    event.comment.body = Some(format!("/{CMD_CREATE_VOTE}"));
    let event = Event::IssueComment(event);

    // Run and check the manual command is returned
    assert_eq!(
        Command::from_event_manual(&event),
        Some(Command::CreateVote(CreateVoteInput::new(None, &event)))
    );
}

#[test]
fn manual_command_from_issue_comment_event_create_vote_cmd_profile1() {
    // Setup event
    let mut event = setup_test_issue_comment_event();
    event.action = IssueCommentEventAction::Created;
    event.comment.body = Some(format!("/{CMD_CREATE_VOTE}-{PROFILE_NAME}"));
    let event = Event::IssueComment(event);

    // Run and check the manual command is returned
    assert_eq!(
        Command::from_event_manual(&event),
        Some(Command::CreateVote(CreateVoteInput::new(
            Some(PROFILE_NAME),
            &event
        )))
    );
}

#[test]
fn manual_command_from_issue_comment_event_crlf_line_endings() {
    // Setup cases (comment body, expected command name, expected profile)
    let cases = [
        ("/vote\r\n", CMD_CREATE_VOTE, None),
        ("/vote\r", CMD_CREATE_VOTE, None),
        ("Some text\r\n/vote\r\nMore text", CMD_CREATE_VOTE, None),
        (
            "Some text\r\n\r\n/vote-profile1\r\n",
            CMD_CREATE_VOTE,
            Some(PROFILE_NAME),
        ),
        ("/vote-profile1\r\nMore text", CMD_CREATE_VOTE, Some(PROFILE_NAME)),
        ("/cancel-vote\r\n/vote\r\n", CMD_CANCEL_VOTE, None),
        ("Some text\r\n/cancel-vote\r\n", CMD_CANCEL_VOTE, None),
        ("/check-vote\r\n", CMD_CHECK_VOTE, None),
        ("Some text\r\n/check-vote\r\nMore text", CMD_CHECK_VOTE, None),
    ];

    for (body, cmd, profile) in cases {
        // Setup event
        let mut event = setup_test_issue_comment_event();
        event.action = IssueCommentEventAction::Created;
        event.comment.body = Some(body.to_string());
        let event = Event::IssueComment(event);

        // Run and check the expected command is returned
        let expected_cmd = match cmd {
            CMD_CANCEL_VOTE => Command::CancelVote(CancelVoteInput::new(&event)),
            CMD_CHECK_VOTE => Command::CheckVote(CheckVoteInput::new(&event)),
            _ => Command::CreateVote(CreateVoteInput::new(profile, &event)),
        };
        assert_eq!(
            Command::from_event_manual(&event),
            Some(expected_cmd),
            "body: {body:?}"
        );
    }
}

#[test]
fn manual_command_from_issue_comment_event_first_cmd_wins() {
    // Setup event with multiple commands
    let mut event = setup_test_issue_comment_event();
    event.action = IssueCommentEventAction::Created;
    event.comment.body = Some(format!("/{CMD_CANCEL_VOTE}\n/{CMD_CREATE_VOTE}"));
    let event = Event::IssueComment(event);

    // Run and check the first manual command is returned
    assert_eq!(
        Command::from_event_manual(&event),
        Some(Command::CancelVote(CancelVoteInput::new(&event)))
    );
}

#[test]
fn manual_command_from_issue_comment_event_indented_or_quoted_cmds_are_not_commands() {
    // Check each indented or quoted command body
    for body in [
        " /vote",
        "    /vote",
        "\t/vote",
        "> /vote",
        ">/vote",
        "> /cancel-vote",
        "> /check-vote",
        "- /vote",
        "* /vote",
        "`/vote`",
        "```\n /vote\n```",
    ] {
        // Setup event
        let mut event = setup_test_issue_comment_event();
        event.action = IssueCommentEventAction::Created;
        event.comment.body = Some(body.to_string());
        let event = Event::IssueComment(event);

        // Run and check no command is returned
        assert_eq!(Command::from_event_manual(&event), None, "body: {body:?}");
    }
}

#[test]
fn manual_command_from_issue_comment_event_invalid_suffix_does_not_hide_later_cmd() {
    // Setup event with a valid command after a line that is not a command anymore
    let mut event = setup_test_issue_comment_event();
    event.action = IssueCommentEventAction::Created;
    event.comment.body = Some("/cancel-vote-profile1\n/check-vote".to_string());
    let event = Event::IssueComment(event);

    // Run and check the valid command is returned
    assert_eq!(
        Command::from_event_manual(&event),
        Some(Command::CheckVote(CheckVoteInput::new(&event)))
    );
}

#[test]
fn manual_command_from_issue_comment_event_invalid_suffixes_are_not_commands() {
    // Check each command body with an invalid suffix
    for body in [
        "/cancel-vote-",
        "/cancel-vote-profile1",
        "/cancel-votes",
        "/check-vote-",
        "/check-vote-profile1",
        "/check-votes",
        "/vote-",
        "/vote-café",
        "/vote-foo bar",
        "/vote-foo.bar",
        "/vote-foo/bar",
        "/voter",
        "/votes",
        "/vote_profile1",
    ] {
        // Setup event
        let mut event = setup_test_issue_comment_event();
        event.action = IssueCommentEventAction::Created;
        event.comment.body = Some(body.to_string());
        let event = Event::IssueComment(event);

        // Run and check no command is returned
        assert_eq!(Command::from_event_manual(&event), None, "body: {body}");
    }
}

#[test]
fn manual_command_from_issue_comment_event_no_body() {
    // Setup event without body
    let mut event = setup_test_issue_comment_event();
    event.action = IssueCommentEventAction::Created;
    event.comment.body = None;
    let event = Event::IssueComment(event);

    // Run and check no command is returned
    assert_eq!(Command::from_event_manual(&event), None);
}

#[test]
fn manual_command_from_issue_comment_event_profile_keeps_its_case() {
    // Check each profile name with uppercase letters
    for profile in ["Profile1", "PROFILE", "myProfile-A_1"] {
        // Setup event
        let mut event = setup_test_issue_comment_event();
        event.action = IssueCommentEventAction::Created;
        event.comment.body = Some(format!("/{CMD_CREATE_VOTE}-{profile}"));
        let event = Event::IssueComment(event);

        // Run and check the profile is returned as it was written
        assert_eq!(
            Command::from_event_manual(&event),
            Some(Command::CreateVote(CreateVoteInput::new(Some(profile), &event)))
        );
    }
}

#[test]
fn manual_command_from_issue_comment_event_profile_with_dash_and_underscore() {
    // Check each profile name with dashes or underscores
    for profile in ["supply-chain-removal", "my_profile", "my-profile_1"] {
        // Setup event
        let mut event = setup_test_issue_comment_event();
        event.action = IssueCommentEventAction::Created;
        event.comment.body = Some(format!("/{CMD_CREATE_VOTE}-{profile}"));
        let event = Event::IssueComment(event);

        // Run and check the manual command is returned with the full profile
        assert_eq!(
            Command::from_event_manual(&event),
            Some(Command::CreateVote(CreateVoteInput::new(Some(profile), &event)))
        );
    }
}

#[test]
fn manual_command_from_issue_comment_event_unsupported_action() {
    // Setup event with unsupported action
    let mut event = setup_test_issue_comment_event();
    event.action = IssueCommentEventAction::Other;
    event.comment.body = Some(format!("/{CMD_CREATE_VOTE}"));
    let event = Event::IssueComment(event);

    // Run and check no command is returned
    assert_eq!(Command::from_event_manual(&event), None);
}

#[test]
fn manual_command_from_issue_event_cancel_vote_cmd() {
    // Setup event
    let mut event = setup_test_issue_event();
    event.action = IssueEventAction::Opened;
    event.issue.body = Some(format!("/{CMD_CANCEL_VOTE}"));
    let event = Event::Issue(event);

    // Run and check the manual command is returned
    assert_eq!(
        Command::from_event_manual(&event),
        Some(Command::CancelVote(CancelVoteInput::new(&event)))
    );
}

#[test]
fn manual_command_from_issue_event_check_vote_cmd() {
    // Setup event
    let mut event = setup_test_issue_event();
    event.action = IssueEventAction::Opened;
    event.issue.body = Some(format!("/{CMD_CHECK_VOTE}"));
    let event = Event::Issue(event);

    // Run and check the manual command is returned
    assert_eq!(
        Command::from_event_manual(&event),
        Some(Command::CheckVote(CheckVoteInput::new(&event)))
    );
}

#[test]
fn manual_command_from_issue_event_create_vote_cmd_default_profile() {
    // Setup event
    let mut event = setup_test_issue_event();
    event.action = IssueEventAction::Opened;
    event.issue.body = Some(format!("/{CMD_CREATE_VOTE}"));
    let event = Event::Issue(event);

    // Run and check the manual command is returned
    assert_eq!(
        Command::from_event_manual(&event),
        Some(Command::CreateVote(CreateVoteInput::new(None, &event)))
    );
}

#[test]
fn manual_command_from_issue_event_create_vote_cmd_profile1() {
    // Setup event
    let mut event = setup_test_issue_event();
    event.action = IssueEventAction::Opened;
    event.issue.body = Some(format!("/{CMD_CREATE_VOTE}-{PROFILE_NAME}"));
    let event = Event::Issue(event);

    // Run and check the manual command is returned
    assert_eq!(
        Command::from_event_manual(&event),
        Some(Command::CreateVote(CreateVoteInput::new(
            Some("profile1"),
            &event
        )))
    );
}

#[test]
fn manual_command_from_issue_event_no_body() {
    // Setup event without body
    let mut event = setup_test_issue_event();
    event.action = IssueEventAction::Opened;
    event.issue.body = None;
    let event = Event::Issue(event);

    // Run and check no command is returned
    assert_eq!(Command::from_event_manual(&event), None);
}

#[test]
fn manual_command_from_issue_event_no_cmd() {
    // Setup event without command
    let mut event = setup_test_issue_event();
    event.action = IssueEventAction::Opened;
    event.issue.body = Some("Hi!".to_string());
    let event = Event::Issue(event);

    // Run and check no command is returned
    assert_eq!(Command::from_event_manual(&event), None);
}

#[test]
fn manual_command_from_issue_event_unsupported_action() {
    // Setup event with unsupported action
    let mut event = setup_test_issue_event();
    event.action = IssueEventAction::Other;
    event.issue.body = Some(format!("/{CMD_CREATE_VOTE}"));
    let event = Event::Issue(event);

    // Run and check no command is returned
    assert_eq!(Command::from_event_manual(&event), None);
}

#[test]
fn manual_command_from_pr_event_cancel_vote_cmd() {
    // Setup event
    let mut event = setup_test_pr_event();
    event.action = PullRequestEventAction::Opened;
    event.pull_request.body = Some(format!("/{CMD_CANCEL_VOTE}"));
    let event = Event::PullRequest(event);

    // Run and check the manual command is returned
    assert_eq!(
        Command::from_event_manual(&event),
        Some(Command::CancelVote(CancelVoteInput::new(&event)))
    );
}

#[test]
fn manual_command_from_pr_event_check_vote_cmd() {
    // Setup event
    let mut event = setup_test_pr_event();
    event.action = PullRequestEventAction::Opened;
    event.pull_request.body = Some(format!("/{CMD_CHECK_VOTE}"));
    let event = Event::PullRequest(event);

    // Run and check the manual command is returned
    assert_eq!(
        Command::from_event_manual(&event),
        Some(Command::CheckVote(CheckVoteInput::new(&event)))
    );
}

#[test]
fn manual_command_from_pr_event_create_vote_cmd_default_profile() {
    // Setup event
    let mut event = setup_test_pr_event();
    event.action = PullRequestEventAction::Opened;
    event.pull_request.body = Some(format!("/{CMD_CREATE_VOTE}"));
    let event = Event::PullRequest(event);

    // Run and check the manual command is returned
    assert_eq!(
        Command::from_event_manual(&event),
        Some(Command::CreateVote(CreateVoteInput::new(None, &event)))
    );
}

#[test]
fn manual_command_from_pr_event_create_vote_cmd_profile1() {
    // Setup event
    let mut event = setup_test_pr_event();
    event.action = PullRequestEventAction::Opened;
    event.pull_request.body = Some(format!("/{CMD_CREATE_VOTE}-{PROFILE_NAME}"));
    let event = Event::PullRequest(event);

    // Run and check the manual command is returned
    assert_eq!(
        Command::from_event_manual(&event),
        Some(Command::CreateVote(CreateVoteInput::new(
            Some(PROFILE_NAME),
            &event
        )))
    );
}

#[test]
fn manual_command_from_pr_event_synchronize_action() {
    // Setup event with synchronize action
    let mut event = setup_test_pr_event();
    event.action = PullRequestEventAction::Synchronize;
    event.pull_request.body = Some(format!("/{CMD_CREATE_VOTE}"));
    let event = Event::PullRequest(event);

    // Run and check no command is returned
    assert_eq!(Command::from_event_manual(&event), None);
}

#[test]
fn manual_command_from_pr_event_unsupported_action() {
    // Setup event with unsupported action
    let mut event = setup_test_pr_event();
    event.action = PullRequestEventAction::Other;
    event.pull_request.body = Some(format!("/{CMD_CREATE_VOTE}"));
    let event = Event::PullRequest(event);

    // Run and check no command is returned
    assert_eq!(Command::from_event_manual(&event), None);
}

// Module functions.

#[tokio::test]
async fn create_no_vote_check_run_automation_disabled_reports_success() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Ok(false));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_config_file(
        &mut gh,
        r#"
automation:
  enabled: false
  rules:
    - patterns:
        - "*.md"
      profile: default
profiles: {}
"#,
    );
    gh.expect_get_pr_files().never();
    expect_check_run(&mut gh, no_vote_found_check());

    // Run and check the check run is created
    create_no_vote_check_run(Arc::new(db), Arc::new(gh), INST_ID, ORG, REPO, ISSUE_NUM)
        .await
        .unwrap();
}

#[tokio::test]
async fn create_no_vote_check_run_config_not_found_reports_success() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Ok(false));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_no_config(&mut gh);
    gh.expect_get_pr_files().never();
    expect_check_run(&mut gh, no_vote_found_check());

    // Run and check the check run is created
    create_no_vote_check_run(Arc::new(db), Arc::new(gh), INST_ID, ORG, REPO, ISSUE_NUM)
        .await
        .unwrap();
}

#[tokio::test]
async fn create_no_vote_check_run_error_checking_vote_is_returned() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Err(format_err!(ERROR)));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_no_config(&mut gh);
    gh.expect_create_check_run().never();

    // Run and check the error is returned
    let err = create_no_vote_check_run(Arc::new(db), Arc::new(gh), INST_ID, ORG, REPO, ISSUE_NUM)
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), ERROR);
}

#[tokio::test]
async fn create_no_vote_check_run_error_creating_check_run_is_returned() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Ok(false));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_no_config(&mut gh);
    gh.expect_create_check_run()
        .with(
            eq(INST_ID),
            eq(ORG),
            eq(REPO),
            eq(ISSUE_NUM),
            eq(no_vote_found_check()),
        )
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Run and check the error is returned
    let err = create_no_vote_check_run(Arc::new(db), Arc::new(gh), INST_ID, ORG, REPO, ISSUE_NUM)
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), ERROR);
}

#[tokio::test]
async fn create_no_vote_check_run_error_getting_config_reports_failure() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Ok(false));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    gh.expect_get_pr_files().never();
    expect_check_run(&mut gh, automation_failed_check());

    // Run and check the check run is created
    create_no_vote_check_run(Arc::new(db), Arc::new(gh), INST_ID, ORG, REPO, ISSUE_NUM)
        .await
        .unwrap();
}

#[tokio::test]
async fn create_no_vote_check_run_error_getting_pr_files_reports_failure() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Ok(false));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_valid_config(&mut gh);
    expect_pr_files(&mut gh, Err(format_err!(ERROR)));
    expect_check_run(&mut gh, automation_failed_check());

    // Run and check the check run is created
    create_no_vote_check_run(Arc::new(db), Arc::new(gh), INST_ID, ORG, REPO, ISSUE_NUM)
        .await
        .unwrap();
}

#[tokio::test]
async fn create_no_vote_check_run_invalid_config_reports_failure() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Ok(false));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_config_content(&mut gh, get_test_invalid_config());
    gh.expect_get_pr_files().never();
    expect_check_run(&mut gh, automation_failed_check());

    // Run and check the check run is created
    create_no_vote_check_run(Arc::new(db), Arc::new(gh), INST_ID, ORG, REPO, ISSUE_NUM)
        .await
        .unwrap();
}

#[tokio::test]
async fn create_no_vote_check_run_no_rule_matches_reports_success() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Ok(false));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_valid_config(&mut gh);
    expect_pr_files(
        &mut gh,
        Ok(vec![File {
            filename: "main.rs".to_string(),
        }]),
    );
    expect_check_run(&mut gh, no_vote_found_check());

    // Run and check the check run is created
    create_no_vote_check_run(Arc::new(db), Arc::new(gh), INST_ID, ORG, REPO, ISSUE_NUM)
        .await
        .unwrap();
}

#[tokio::test]
async fn create_no_vote_check_run_rule_matches_reports_failure() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Ok(false));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_valid_config(&mut gh);
    expect_pr_files(
        &mut gh,
        Ok(vec![File {
            filename: "README.md".to_string(),
        }]),
    );
    expect_check_run(
        &mut gh,
        CheckDetails {
            status: "completed".to_string(),
            summary: CHECK_SUMMARY_AUTOMATION_RULE_MATCHED.to_string(),

            conclusion: Some("failure".to_string()),
        },
    );

    // Run and check the check run is created
    create_no_vote_check_run(Arc::new(db), Arc::new(gh), INST_ID, ORG, REPO, ISSUE_NUM)
        .await
        .unwrap();
}

#[tokio::test]
async fn create_no_vote_check_run_with_vote_skips_check_run() {
    // Setup cases (config result, pr files requests) for every check outcome, all owned
    // by the vote: success, rule matched and failed evaluation
    type ConfigResult = fn() -> Result<Option<String>>;
    let cases: [(ConfigResult, usize); 3] = [
        (|| Ok(None), 0),
        (|| Ok(Some(get_test_valid_config())), 1),
        (|| Err(format_err!(ERROR)), 0),
    ];

    for (config_result, pr_files_requests) in cases {
        // Setup database expectations
        let mut db = MockDB::new();
        expect_has_vote(&mut db, Ok(true));

        // Setup GitHub expectations
        let mut gh = MockGH::new();
        gh.expect_get_config_file()
            .with(eq(INST_ID), eq(ORG), eq(REPO))
            .times(1)
            .returning(move |_, _, _| Box::pin(future::ready(config_result())));
        gh.expect_get_pr_files()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM))
            .times(pr_files_requests)
            .returning(|_, _, _, _| {
                Box::pin(future::ready(Ok(vec![File {
                    filename: "README.md".to_string(),
                }])))
            });
        gh.expect_create_check_run().never();

        // Run and check nothing is reported
        create_no_vote_check_run(Arc::new(db), Arc::new(gh), INST_ID, ORG, REPO, ISSUE_NUM)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn publish_no_vote_check_error_checking_vote_is_returned() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Err(format_err!(ERROR)));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_create_check_run().never();

    // Setup check details
    let check_details = no_vote_found_check();

    // Run and check the error is returned
    let err = publish_no_vote_check(
        Arc::new(db),
        Arc::new(gh),
        INST_ID,
        ORG,
        REPO,
        ISSUE_NUM,
        &check_details,
    )
    .await
    .unwrap_err();
    assert_eq!(err.to_string(), ERROR);
}

#[tokio::test]
async fn publish_no_vote_check_with_vote_skips_check_run() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Ok(true));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_create_check_run().never();

    // Setup check details
    let check_details = no_vote_found_check();

    // Run and check nothing is reported
    publish_no_vote_check(
        Arc::new(db),
        Arc::new(gh),
        INST_ID,
        ORG,
        REPO,
        ISSUE_NUM,
        &check_details,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn publish_no_vote_check_without_vote_creates_check_run() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Ok(false));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_check_run(&mut gh, no_vote_found_check());

    // Setup check details
    let check_details = no_vote_found_check();

    // Run and check the check run is created
    publish_no_vote_check(
        Arc::new(db),
        Arc::new(gh),
        INST_ID,
        ORG,
        REPO,
        ISSUE_NUM,
        &check_details,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn update_no_vote_check_check_not_required_skips_evaluation() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_has_vote().never();

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_is_check_required(&mut gh, false);
    gh.expect_get_config_file().never();
    gh.expect_create_check_run().never();

    // Run and check nothing is reported
    update_no_vote_check(Arc::new(db), Arc::new(gh), INST_ID, ORG, REPO, ISSUE_NUM, BRANCH)
        .await
        .unwrap();
}

#[tokio::test]
async fn update_no_vote_check_check_required_creates_check_run() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Ok(false));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_is_check_required(&mut gh, true);
    expect_no_config(&mut gh);
    expect_check_run(&mut gh, no_vote_found_check());

    // Run and check the check run is created
    update_no_vote_check(Arc::new(db), Arc::new(gh), INST_ID, ORG, REPO, ISSUE_NUM, BRANCH)
        .await
        .unwrap();
}

#[tokio::test]
async fn update_no_vote_check_error_checking_if_required_is_returned() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_has_vote().never();

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_is_check_required()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(BRANCH))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    gh.expect_create_check_run().never();

    // Run and check the error is returned
    let err = update_no_vote_check(Arc::new(db), Arc::new(gh), INST_ID, ORG, REPO, ISSUE_NUM, BRANCH)
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), ERROR);
}

#[tokio::test]
async fn update_no_vote_check_with_vote_skips_check_run() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Ok(true));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_is_check_required(&mut gh, true);
    expect_no_config(&mut gh);
    gh.expect_create_check_run().never();

    // Run and check nothing is reported
    update_no_vote_check(Arc::new(db), Arc::new(gh), INST_ID, ORG, REPO, ISSUE_NUM, BRANCH)
        .await
        .unwrap();
}

// Helpers.

/// Expects a single configuration file request returning the owned content provided.
fn expect_config_content(gh: &mut MockGH, config: String) {
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .return_once(move |_, _, _| Box::pin(future::ready(Ok(Some(config)))));
}

/// Expects a single configuration file request returning the content provided.
fn expect_config_file(gh: &mut MockGH, config: &'static str) {
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(move |_, _, _| {
            Box::pin(future::ready(Ok(Some(
                config.trim_start_matches('\n').to_string(),
            ))))
        });
}

/// Expects a single pull request files request returning the result provided.
fn expect_pr_files(gh: &mut MockGH, files: Result<Vec<File>>) {
    gh.expect_get_pr_files()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM))
        .times(1)
        .return_once(move |_, _, _, _| Box::pin(future::ready(files)));
}

/// Sets up a pull request opened event without body.
fn setup_test_pr_opened_event() -> Event {
    let mut event = setup_test_pr_event();
    event.action = PullRequestEventAction::Opened;
    Event::PullRequest(event)
}
