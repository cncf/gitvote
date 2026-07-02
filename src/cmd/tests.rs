use std::sync::Arc;

use anyhow::format_err;
use futures::future;
use mockall::predicate::eq;

use crate::{
    cfg_repo::CfgError,
    github::{File, MockGH, Organization, PullRequestInIssue},
    testutil::*,
};

use super::*;

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
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Some(get_test_valid_config()))));
    gh.expect_get_pr_files()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _, _, _| {
            Box::pin(future::ready(Ok(vec![File {
                filename: "README.md".to_string(),
            }])))
        });
    let gh = Arc::new(gh);

    // Setup event
    let mut event = setup_test_pr_event();
    event.action = PullRequestEventAction::Opened;
    let event = Event::PullRequest(event);

    // Run and check the automatic command is created
    assert_eq!(
        Command::from_event_automatic(gh, &event.clone()).await.unwrap(),
        Some(Command::CreateVote(CreateVoteInput::new(Some("default"), &event)))
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

    // Run and check no command is created
    let event = setup_test_pr_opened_event();
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

    // Run and check no command is created
    let event = setup_test_pr_opened_event();
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

    // Run and check no command is created
    let event = setup_test_pr_opened_event();
    assert_eq!(
        Command::from_event_automatic(Arc::new(gh), &event).await.unwrap(),
        None
    );
}

#[tokio::test]
async fn automatic_command_from_pr_event_config_not_found() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(None)));
    gh.expect_get_pr_files().never();

    // Run and check no command is created
    let event = setup_test_pr_opened_event();
    assert_eq!(
        Command::from_event_automatic(Arc::new(gh), &event).await.unwrap(),
        None
    );
}

#[tokio::test]
async fn automatic_command_from_pr_event_error_getting_pr_files() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Some(get_test_valid_config()))));
    gh.expect_get_pr_files()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Run and check the error is propagated
    let event = setup_test_pr_opened_event();
    let err = Command::from_event_automatic(Arc::new(gh), &event).await.unwrap_err();
    assert_eq!(err.to_string(), ERROR);
}

#[tokio::test]
async fn automatic_command_from_pr_event_invalid_config() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Some(get_test_invalid_config()))));
    gh.expect_get_pr_files().never();

    // Run and check the configuration error is propagated
    let event = setup_test_pr_opened_event();
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
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Some(get_test_valid_config()))));
    gh.expect_get_pr_files()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _, _, _| {
            Box::pin(future::ready(Ok(vec![File {
                filename: "src/main.rs".to_string(),
            }])))
        });

    // Run and check no command is created
    let event = setup_test_pr_opened_event();
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
    gh.expect_get_pr_files()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _, _, _| {
            Box::pin(future::ready(Ok(vec![File {
                filename: "src/main.rs".to_string(),
            }])))
        });

    // Run and check the matching rule profile is used
    let event = setup_test_pr_opened_event();
    assert_eq!(
        Command::from_event_automatic(Arc::new(gh), &event).await.unwrap(),
        Some(Command::CreateVote(CreateVoteInput::new(
            Some(PROFILE_NAME),
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
            issue_number: 2,
            is_pull_request: true,
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
            issue_number: 2,
            is_pull_request: false,
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
            issue_number: 2,
            is_pull_request: true,
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
        }
    );
}

#[tokio::test]
async fn command_from_event_automatic_command() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Some(get_test_valid_config()))));
    gh.expect_get_pr_files()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _, _, _| {
            Box::pin(future::ready(Ok(vec![File {
                filename: "README.md".to_string(),
            }])))
        });

    // Run and check the automatic command is returned
    let event = setup_test_pr_opened_event();
    assert_eq!(
        Command::from_event(Arc::new(gh), &event).await,
        Some(Command::CreateVote(CreateVoteInput::new(Some("default"), &event)))
    );
}

#[tokio::test]
async fn command_from_event_automatic_command_error() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Some(get_test_invalid_config()))));
    gh.expect_get_pr_files().never();

    // Run and check errors result in no command
    let event = setup_test_pr_opened_event();
    assert_eq!(Command::from_event(Arc::new(gh), &event).await, None);
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
        Command::from_event(gh, &event).await,
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
    assert_eq!(Command::from_event(gh, &event).await, None);
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
            profile_name: Some(PROFILE_NAME.to_string()),
            created_by: USER.to_string(),
            installation_id: 1,
            issue_id: 2,
            issue_number: 3,
            issue_title: TITLE.to_string(),
            is_pull_request: false,
            repository_full_name: REPOFN.to_string(),
            organization: Some(ORG.to_string()),
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
            profile_name: None,
            created_by: USER.to_string(),
            installation_id: 1,
            issue_id: 2,
            issue_number: 3,
            issue_title: TITLE.to_string(),
            is_pull_request: false,
            repository_full_name: REPOFN.to_string(),
            organization: Some(ORG.to_string()),
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
            profile_name: Some(PROFILE_NAME.to_string()),
            created_by: USER.to_string(),
            installation_id: 1,
            issue_id: 2,
            issue_number: 3,
            issue_title: TITLE.to_string(),
            is_pull_request: true,
            repository_full_name: REPOFN.to_string(),
            organization: Some("other-org".to_string()),
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

// Helpers.

/// Expect a single configuration file request returning the content provided.
fn expect_config_file(gh: &mut MockGH, config: &'static str) {
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(move |_, _, _| Box::pin(future::ready(Some(config.trim_start_matches('\n').to_string()))));
}

/// Setup a pull request opened event without body.
fn setup_test_pr_opened_event() -> Event {
    let mut event = setup_test_pr_event();
    event.action = PullRequestEventAction::Opened;
    Event::PullRequest(event)
}
