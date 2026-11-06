use std::sync::Arc;

use anyhow::format_err;
use mockall::{Sequence, predicate::eq};

use crate::{cfg_repo::AllowedVoters, db::MockDB, github::*, testutil::*};

use super::*;

#[tokio::test]
async fn create_vote_automatic_error_reporting_failure_is_not_reported_again() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_valid_config(&mut gh);
    gh.expect_is_check_required().never();
    gh.expect_create_check_run()
        .with(
            eq(INST_ID),
            eq(ORG),
            eq(REPO),
            eq(ISSUE_NUM),
            eq(cmd::automation_failed_check()),
        )
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    let details = tmpl::ConfigProfileNotFound {}.render().unwrap();
    expect_comment(
        &mut gh,
        tmpl::AutomationFailed::new(Some(&details)).render().unwrap(),
    );

    // Setup database expectations: the pull request has no vote
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Ok(false));

    // Run the command and check the comment was still posted, and is the only one
    let event = Event::PullRequest(setup_test_pr_event());
    let input = CreateVoteInput::new_automatic("typo", &event);
    let cmd = Command::CreateVote(input);
    setup_cmds_handler(db, gh).process_command(cmd).await;
}

#[tokio::test]
async fn create_vote_automatic_invalid_profile_reports_automation_failure() {
    // Setup cases (profile, expected reason)
    let test_cases = [
        (
            "duration: 36501d\n    pass_threshold: 50",
            "duration must be at least 1 second and not greater than 36500 days",
        ),
        (
            "duration: 5m\n    pass_threshold: 50\n    periodic_status_check: 12 hours",
            "periodic_status_check",
        ),
        (
            "duration: 5m\n    pass_threshold: 0",
            "pass threshold must be greater than 0",
        ),
    ];
    for (profile, expected_reason) in test_cases {
        // Setup event of a pull request matching an automation rule
        let mut event = setup_test_pr_event();
        event.action = PullRequestEventAction::Opened;
        let event = Event::PullRequest(event);

        // Setup GitHub expectations: the check does not depend on being required
        let config = format!(
            r#"
automation:
  enabled: true
  rules:
    - patterns: ["README.md"]
      profile: default
profiles:
  default:
    {profile}
"#
        );
        let mut gh = MockGH::new();
        gh.expect_get_config_file()
            .with(eq(INST_ID), eq(ORG), eq(REPO))
            .times(2)
            .returning(move |_, _, _| Box::pin(future::ready(Ok(Some(config.clone())))));
        gh.expect_get_pr_files()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM))
            .times(1)
            .returning(|_, _, _, _| {
                let file = File {
                    filename: "README.md".to_string(),
                };
                Box::pin(future::ready(Ok(vec![file])))
            });
        gh.expect_is_check_required().never();
        gh.expect_user_is_collaborator().never();
        expect_check_run(&mut gh, cmd::automation_failed_check());
        gh.expect_post_comment()
            .withf(move |inst_id, owner, repo, issue_number, body| {
                *inst_id == INST_ID
                    && owner == ORG
                    && repo == REPO
                    && *issue_number == ISSUE_NUM
                    && body.contains("Something went wrong while processing the automation rules")
                    && body.contains("Something went wrong while processing the configuration file")
                    && body.contains(expected_reason)
                    && body.contains("`/vote`")
            })
            .times(1)
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
        let gh = Arc::new(gh);

        // Setup database expectations: the pull request has no vote
        let mut db = MockDB::new();
        expect_has_vote(&mut db, Ok(false));
        db.expect_store_vote().never();

        // Run the automatic command created from the event
        let cmd = Command::from_event(gh.clone(), &event).await.unwrap().unwrap();
        let (_, cmds_rx) = async_channel::unbounded();
        CommandsHandler::new(Arc::new(db), gh, cmds_rx).process_command(cmd).await;
    }
}

#[tokio::test]
async fn create_vote_automatic_missing_profile_reports_automation_failure() {
    // Setup event of a pull request matching an automation rule
    let mut event = setup_test_pr_event();
    event.action = PullRequestEventAction::Opened;
    let event = Event::PullRequest(event);

    // Setup GitHub expectations: the check does not depend on being required
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(2)
        .returning(|_, _, _| {
            let config = r#"
automation:
  enabled: true
  rules:
    - patterns: ["README.md"]
      profile: typo
profiles:
  default:
    duration: 5m
    pass_threshold: 50
"#;
            Box::pin(future::ready(Ok(Some(config.to_string()))))
        });
    gh.expect_get_pr_files()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _, _, _| {
            let file = File {
                filename: "README.md".to_string(),
            };
            Box::pin(future::ready(Ok(vec![file])))
        });
    gh.expect_is_check_required().never();
    expect_check_run(&mut gh, cmd::automation_failed_check());
    let details = tmpl::ConfigProfileNotFound {}.render().unwrap();
    expect_comment(
        &mut gh,
        tmpl::AutomationFailed::new(Some(&details)).render().unwrap(),
    );
    let gh = Arc::new(gh);

    // Setup database expectations: the pull request has no vote
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Ok(false));

    // Run the automatic command created from the event
    let cmd = Command::from_event(gh.clone(), &event).await.unwrap().unwrap();
    let (_, cmds_rx) = async_channel::unbounded();
    CommandsHandler::new(Arc::new(db), gh, cmds_rx).process_command(cmd).await;
}

#[tokio::test]
async fn create_vote_error_adding_labels() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_has_vote_open()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(false))));
    expect_store_vote(&mut db, false, || Ok(test_vote_id()));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_valid_config(&mut gh);
    expect_user_is_collaborator(&mut gh, true);
    expect_vote_created_comment(&mut gh, &mut Sequence::new(), false);
    for label in [VOTE_CLOSED_LABEL, VOTE_PASSED_LABEL, VOTE_FAILED_LABEL] {
        gh.expect_remove_label()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM), eq(label))
            .times(1)
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
    }
    gh.expect_add_labels()
        .withf(|inst_id, owner, repo, issue_number, labels| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && labels == vec![GITVOTE_LABEL, VOTE_OPEN_LABEL]
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Run and check the setup error is propagated with its step
    let input = CreateVoteInput::new(None, &Event::Issue(setup_test_issue_event()));
    let err = setup_cmds_handler(db, gh).create_vote(&input).await.unwrap_err();
    assert!(err.is::<VoteSetupError>());
    assert_eq!(err.to_string(), "error running labels step");
    assert_eq!(err.root_cause().to_string(), ERROR);
}

#[tokio::test]
async fn create_vote_error_checking_if_user_is_collaborator() {
    // Setup GitHub expectations
    let db = MockDB::new();
    let mut gh = MockGH::new();
    expect_valid_config(&mut gh);
    gh.expect_user_is_collaborator()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    gh.expect_post_comment().never();

    // Run and check the error is propagated
    let input = CreateVoteInput::new(None, &Event::Issue(setup_test_issue_event()));
    let err = setup_cmds_handler(db, gh).create_vote(&input).await.unwrap_err();
    assert_eq!(err.to_string(), ERROR);
}

#[tokio::test]
async fn create_vote_error_checking_if_vote_is_open() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_has_vote_open()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    db.expect_store_vote().never();

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_valid_config(&mut gh);
    expect_user_is_collaborator(&mut gh, true);
    gh.expect_post_comment().never();

    // Run and check the error is propagated
    let input = CreateVoteInput::new(None, &Event::Issue(setup_test_issue_event()));
    let err = setup_cmds_handler(db, gh).create_vote(&input).await.unwrap_err();
    assert_eq!(err.to_string(), ERROR);
}

#[tokio::test]
async fn create_vote_error_getting_configuration_file() {
    // Setup GitHub expectations
    let db = MockDB::new();
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    gh.expect_user_is_collaborator().never();
    gh.expect_post_comment().never();

    // Run and check the error is propagated instead of reporting a missing config
    let input = CreateVoteInput::new(None, &Event::Issue(setup_test_issue_event()));
    let err = setup_cmds_handler(db, gh).create_vote(&input).await.unwrap_err();
    assert_eq!(err.to_string(), ERROR);
}

#[tokio::test]
async fn create_vote_error_getting_configuration_profile() {
    // Setup GitHub expectations
    let db = MockDB::new();
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(None))));
    gh.expect_post_comment()
        .withf(|inst_id, owner, repo, issue_number, body| {
            let expected_body = tmpl::ConfigNotFound {}.render().unwrap();
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body == expected_body.as_str()
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));

    // Run and check the configuration error is posted
    let (_, cmds_rx) = async_channel::unbounded();
    let event = setup_test_issue_event();
    let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
    cmds_handler.create_vote(&CreateVoteInput::new(None, &Event::Issue(event))).await.unwrap();
}

#[tokio::test]
async fn create_vote_error_posting_vote_created_comment() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_has_vote_open()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(false))));
    db.expect_store_vote().never();

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_valid_config(&mut gh);
    expect_user_is_collaborator(&mut gh, true);
    gh.expect_post_comment()
        .withf(|inst_id, owner, repo, issue_number, body| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body == vote_created_body(false)
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    gh.expect_add_labels().never();

    // Run and check the error is propagated
    let input = CreateVoteInput::new(None, &Event::Issue(setup_test_issue_event()));
    let err = setup_cmds_handler(db, gh).create_vote(&input).await.unwrap_err();
    assert_eq!(err.to_string(), ERROR);
}

#[tokio::test]
async fn create_vote_error_storing_vote() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_has_vote_open()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(false))));
    expect_store_vote(&mut db, true, || Err(format_err!(ERROR)));
    expect_has_vote_with_comment(&mut db, Ok(false));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_valid_config(&mut gh);
    expect_user_is_collaborator(&mut gh, true);
    expect_vote_created_comment(&mut gh, &mut Sequence::new(), true);
    gh.expect_delete_comment()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(COMMENT_ID))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(()))));
    gh.expect_create_check_run().never();
    gh.expect_remove_label().never();
    gh.expect_add_labels().never();

    // Run and check the error is propagated
    let input = CreateVoteInput::new(None, &Event::PullRequest(setup_test_pr_event()));
    let err = setup_cmds_handler(db, gh).create_vote(&input).await.unwrap_err();
    assert_eq!(err.to_string(), ERROR);
}

#[tokio::test]
async fn create_vote_error_storing_vote_and_deleting_comment() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_has_vote_open()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(false))));
    expect_store_vote(&mut db, false, || Err(format_err!(ERROR)));
    expect_has_vote_with_comment(&mut db, Ok(false));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_valid_config(&mut gh);
    expect_user_is_collaborator(&mut gh, true);
    expect_vote_created_comment(&mut gh, &mut Sequence::new(), false);
    gh.expect_delete_comment()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(COMMENT_ID))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Err(format_err!("delete error")))));

    // Run and check the error storing the vote is the one propagated
    let input = CreateVoteInput::new(None, &Event::Issue(setup_test_issue_event()));
    let err = setup_cmds_handler(db, gh).create_vote(&input).await.unwrap_err();
    assert_eq!(err.to_string(), ERROR);
}

#[tokio::test]
async fn create_vote_error_storing_vote_lookup_error_keeps_comment() {
    // Setup database expectations: it is unknown whether the vote was stored
    let mut db = MockDB::new();
    db.expect_has_vote_open()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(false))));
    expect_store_vote(&mut db, false, || Err(format_err!(ERROR)));
    expect_has_vote_with_comment(&mut db, Err(format_err!("lookup error")));

    // Setup GitHub expectations: the vote is not set up and its comment is kept
    let mut gh = MockGH::new();
    let mut seq = Sequence::new();
    expect_valid_config(&mut gh);
    expect_user_is_collaborator(&mut gh, true);
    expect_vote_created_comment(&mut gh, &mut seq, false);
    gh.expect_delete_comment().never();
    gh.expect_remove_label().never();
    gh.expect_add_labels().never();

    // Run and check the error storing the vote is the one propagated
    let input = CreateVoteInput::new(None, &Event::Issue(setup_test_issue_event()));
    let err = setup_cmds_handler(db, gh).create_vote(&input).await.unwrap_err();
    assert_eq!(err.to_string(), ERROR);
}

#[tokio::test]
async fn create_vote_error_storing_vote_stored_anyway_sets_it_up() {
    // Setup database expectations: the vote was stored despite the error
    let mut db = MockDB::new();
    db.expect_has_vote_open()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(false))));
    expect_store_vote(&mut db, false, || Err(format_err!(ERROR)));
    expect_has_vote_with_comment(&mut db, Ok(true));

    // Setup GitHub expectations: the comment is kept and the vote is set up
    let mut gh = MockGH::new();
    let mut seq = Sequence::new();
    expect_valid_config(&mut gh);
    expect_user_is_collaborator(&mut gh, true);
    expect_vote_created_comment(&mut gh, &mut seq, false);
    gh.expect_delete_comment().never();
    gh.expect_create_check_run().never();
    expect_vote_open_labels(&mut gh);

    // Run and check the vote is created successfully
    let input = CreateVoteInput::new(None, &Event::Issue(setup_test_issue_event()));
    setup_cmds_handler(db, gh).create_vote(&input).await.unwrap();
}

#[tokio::test]
async fn create_vote_invalid_config_duration_too_long() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_store_vote().never();

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| {
            let config = r"
profiles:
  default:
    duration: 36501d
    pass_threshold: 50
";
            Box::pin(future::ready(Ok(Some(config.to_string()))))
        });
    gh.expect_user_is_collaborator().never();
    gh.expect_post_comment()
        .withf(|inst_id, owner, repo, issue_number, body| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body.contains("Something went wrong while processing the configuration file")
                && body.contains("duration must be at least 1 second and not greater than 36500 days")
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));

    // Run the command and check only the invalid configuration comment is posted
    let input = CreateVoteInput::new(None, &Event::Issue(setup_test_issue_event()));
    setup_cmds_handler(db, gh).create_vote(&input).await.unwrap();
}

#[tokio::test]
async fn create_vote_invalid_config_duration_zero() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_store_vote().never();

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| {
            let config = r"
profiles:
  default:
    duration: 0s
    pass_threshold: 50
";
            Box::pin(future::ready(Ok(Some(config.to_string()))))
        });
    gh.expect_user_is_collaborator().never();
    gh.expect_post_comment()
        .withf(|inst_id, owner, repo, issue_number, body| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body.contains("Something went wrong while processing the configuration file")
                && body.contains("duration must be at least 1 second and not greater than 36500 days")
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));

    // Run the command and check only the invalid configuration comment is posted
    let input = CreateVoteInput::new(None, &Event::Issue(setup_test_issue_event()));
    setup_cmds_handler(db, gh).create_vote(&input).await.unwrap();
}

#[tokio::test]
async fn create_vote_invalid_config_periodic_status_check() {
    // Setup GitHub expectations
    let db = MockDB::new();
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| {
            let config = r#"
profiles:
  default:
    duration: 5m
    pass_threshold: 50
    periodic_status_check: "5 dayz"
"#;
            Box::pin(future::ready(Ok(Some(config.to_string()))))
        });
    gh.expect_user_is_collaborator().never();
    gh.expect_post_comment()
        .withf(|inst_id, owner, repo, issue_number, body| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body.contains("Something went wrong while processing the configuration file")
                && body.contains("periodic_status_check must be a number of days or weeks")
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));

    // Run the command and check no vote is created
    let input = CreateVoteInput::new(None, &Event::Issue(setup_test_issue_event()));
    setup_cmds_handler(db, gh).create_vote(&input).await.unwrap();
}

#[tokio::test]
async fn create_vote_invalid_config_teams_owner_not_org() {
    // Setup GitHub expectations
    let db = MockDB::new();
    let mut gh = MockGH::new();
    expect_valid_config(&mut gh);
    gh.expect_user_is_collaborator().never();
    gh.expect_post_comment()
        .withf(|inst_id, owner, repo, issue_number, body| {
            let expected_body =
                tmpl::InvalidConfig::new("teams in allowed voters can only be used in organizations")
                    .render()
                    .unwrap();
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body == expected_body.as_str()
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));

    // Run the command on a repository that does not belong to an organization
    let mut event = setup_test_issue_event();
    event.organization = None;
    let input = CreateVoteInput::new(Some(PROFILE_NAME), &Event::Issue(event));
    setup_cmds_handler(db, gh).create_vote(&input).await.unwrap();
}

#[tokio::test]
async fn create_vote_issue_already_has_a_vote() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_has_vote_open()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(true))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_valid_config(&mut gh);
    gh.expect_user_is_collaborator()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(true))));
    gh.expect_post_comment()
        .withf(|inst_id, owner, repo, issue_number, body| {
            let expected_body = tmpl::VoteInProgress::new(USER, false).render().unwrap();
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body == expected_body.as_str()
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));

    // Run and check the vote is not created
    let (_, cmds_rx) = async_channel::unbounded();
    let event = setup_test_issue_event();
    let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
    cmds_handler.create_vote(&CreateVoteInput::new(None, &Event::Issue(event))).await.unwrap();
}

#[tokio::test]
async fn create_vote_lost_race_error_deleting_comment_is_ignored() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_store_vote_lost_race(&mut db, false);

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    let mut seq = Sequence::new();
    expect_valid_config(&mut gh);
    expect_user_is_collaborator(&mut gh, true);
    expect_vote_created_comment(&mut gh, &mut seq, false);
    gh.expect_delete_comment()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(COMMENT_ID))
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    expect_vote_in_progress_comment(&mut gh, &mut seq, false);

    // Run and check the vote is reported as already in progress
    let input = CreateVoteInput::new(None, &Event::Issue(setup_test_issue_event()));
    setup_cmds_handler(db, gh).create_vote(&input).await.unwrap();
}

#[tokio::test]
async fn create_vote_lost_race_on_issue_and_pr() {
    for is_pull_request in [false, true] {
        // Setup database expectations
        let mut db = MockDB::new();
        expect_store_vote_lost_race(&mut db, is_pull_request);

        // Setup GitHub expectations
        let mut gh = MockGH::new();
        let mut seq = Sequence::new();
        expect_valid_config(&mut gh);
        expect_user_is_collaborator(&mut gh, true);
        expect_vote_created_comment(&mut gh, &mut seq, is_pull_request);
        gh.expect_delete_comment()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(COMMENT_ID))
            .times(1)
            .in_sequence(&mut seq)
            .returning(|_, _, _, _| Box::pin(future::ready(Ok(()))));
        expect_vote_in_progress_comment(&mut gh, &mut seq, is_pull_request);
        gh.expect_create_check_run().never();
        gh.expect_remove_label().never();
        gh.expect_add_labels().never();

        // Run and check the vote is reported as already in progress
        let event = if is_pull_request {
            Event::PullRequest(setup_test_pr_event())
        } else {
            Event::Issue(setup_test_issue_event())
        };
        let input = CreateVoteInput::new(None, &event);
        setup_cmds_handler(db, gh).create_vote(&input).await.unwrap();
    }
}

#[tokio::test]
async fn create_vote_non_collaborator() {
    // Setup GitHub expectations
    let db = MockDB::new();
    let mut gh = MockGH::new();
    expect_valid_config(&mut gh);
    gh.expect_user_is_collaborator()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(false))));
    gh.expect_post_comment()
        .withf(|inst_id, owner, repo, issue_number, body| {
            let expected_body = tmpl::VoteRestricted::new(USER).render().unwrap();
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body == expected_body.as_str()
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));

    // Run and check the vote is not created
    let (_, cmds_rx) = async_channel::unbounded();
    let event = setup_test_issue_event();
    let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
    cmds_handler.create_vote(&CreateVoteInput::new(None, &Event::Issue(event))).await.unwrap();
}

#[tokio::test]
async fn create_vote_pr_error_creating_check_run() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_has_vote_open()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(false))));
    expect_store_vote(&mut db, true, || Ok(test_vote_id()));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_valid_config(&mut gh);
    expect_user_is_collaborator(&mut gh, true);
    expect_vote_created_comment(&mut gh, &mut Sequence::new(), true);
    gh.expect_create_check_run()
        .with(
            eq(INST_ID),
            eq(ORG),
            eq(REPO),
            eq(ISSUE_NUM),
            eq(CheckDetails {
                status: "in_progress".to_string(),
                summary: "Vote open".to_string(),

                conclusion: None,
            }),
        )
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    for label in [VOTE_CLOSED_LABEL, VOTE_PASSED_LABEL, VOTE_FAILED_LABEL] {
        gh.expect_remove_label()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM), eq(label))
            .times(1)
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
    }
    gh.expect_add_labels()
        .withf(|inst_id, owner, repo, issue_number, labels| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && labels == vec![GITVOTE_LABEL, VOTE_OPEN_LABEL]
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));

    // Run and check the labels were updated and the setup error is propagated
    let input = CreateVoteInput::new(None, &Event::PullRequest(setup_test_pr_event()));
    let err = setup_cmds_handler(db, gh).create_vote(&input).await.unwrap_err();
    assert!(err.is::<VoteSetupError>());
    assert_eq!(err.to_string(), "error running check run step");
    assert_eq!(err.root_cause().to_string(), ERROR);
}

#[tokio::test]
async fn create_vote_pr_error_creating_check_run_and_adding_labels() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_has_vote_open()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(false))));
    expect_store_vote(&mut db, true, || Ok(test_vote_id()));

    // Setup GitHub expectations: both setup steps are tried and fail
    let mut gh = MockGH::new();
    let mut seq = Sequence::new();
    expect_valid_config(&mut gh);
    expect_user_is_collaborator(&mut gh, true);
    expect_vote_created_comment(&mut gh, &mut seq, true);
    gh.expect_create_check_run()
        .with(
            eq(INST_ID),
            eq(ORG),
            eq(REPO),
            eq(ISSUE_NUM),
            eq(CheckDetails {
                status: "in_progress".to_string(),
                summary: "Vote open".to_string(),

                conclusion: None,
            }),
        )
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Err(format_err!("check run error")))));
    for label in [VOTE_CLOSED_LABEL, VOTE_PASSED_LABEL, VOTE_FAILED_LABEL] {
        gh.expect_remove_label()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM), eq(label))
            .times(1)
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
    }
    gh.expect_add_labels()
        .withf(|inst_id, owner, repo, issue_number, labels| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && labels == vec![GITVOTE_LABEL, VOTE_OPEN_LABEL]
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Err(format_err!("labels error")))));

    // Run and check the first setup error is the one propagated
    let input = CreateVoteInput::new(None, &Event::PullRequest(setup_test_pr_event()));
    let err = setup_cmds_handler(db, gh).create_vote(&input).await.unwrap_err();
    assert!(err.is::<VoteSetupError>());
    assert_eq!(err.to_string(), "error running check run step");
    assert_eq!(err.root_cause().to_string(), "check run error");
}

#[tokio::test]
async fn create_vote_pr_success() {
    // Setup input
    let event = setup_test_pr_event();
    let create_vote_input = CreateVoteInput::new(None, &Event::PullRequest(event));
    let cfg = CfgProfile {
        duration: Duration::from_mins(5),
        pass_threshold: 50.0,

        allowed_voters: Some(AllowedVoters::default()),
        ..Default::default()
    };

    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_has_vote_open()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(false))));
    let cfg_copy = cfg.clone();
    let create_vote_input_copy = create_vote_input.clone();
    db.expect_store_vote()
        .withf(move |vote_comment_id, input, cfg| {
            *vote_comment_id == COMMENT_ID && *input == create_vote_input_copy && *cfg == cfg_copy
        })
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(test_vote_id()))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_valid_config(&mut gh);
    gh.expect_user_is_collaborator()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(true))));
    let create_vote_input_copy = create_vote_input.clone();
    gh.expect_post_comment()
        .withf(move |inst_id, owner, repo, issue_number, body| {
            let expected_body = tmpl::VoteCreated::new(&create_vote_input_copy, &cfg).render().unwrap();
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body == expected_body.as_str()
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
    expect_check_run(
        &mut gh,
        CheckDetails {
            status: "in_progress".to_string(),
            summary: "Vote open".to_string(),

            conclusion: None,
        },
    );
    gh.expect_remove_label()
        .with(
            eq(INST_ID),
            eq(ORG),
            eq(REPO),
            eq(ISSUE_NUM),
            eq(VOTE_CLOSED_LABEL),
        )
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
    gh.expect_remove_label()
        .with(
            eq(INST_ID),
            eq(ORG),
            eq(REPO),
            eq(ISSUE_NUM),
            eq(VOTE_PASSED_LABEL),
        )
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
    gh.expect_remove_label()
        .with(
            eq(INST_ID),
            eq(ORG),
            eq(REPO),
            eq(ISSUE_NUM),
            eq(VOTE_FAILED_LABEL),
        )
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
    gh.expect_add_labels()
        .withf(|inst_id, owner, repo, issue_number, labels| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && labels == vec![GITVOTE_LABEL, VOTE_OPEN_LABEL]
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));

    // Run and check the vote is created successfully
    let (_, cmds_rx) = async_channel::unbounded();
    let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
    cmds_handler.create_vote(&create_vote_input).await.unwrap();
}

#[tokio::test]
async fn create_vote_profile_not_found() {
    // Setup GitHub expectations
    let db = MockDB::new();
    let mut gh = MockGH::new();
    expect_valid_config(&mut gh);
    gh.expect_user_is_collaborator().never();
    gh.expect_post_comment()
        .withf(|inst_id, owner, repo, issue_number, body| {
            let expected_body = tmpl::ConfigProfileNotFound {}.render().unwrap();
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body == expected_body.as_str()
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));

    // Run the command using a profile that does not exist
    let input = CreateVoteInput::new(Some("profile9"), &Event::Issue(setup_test_issue_event()));
    setup_cmds_handler(db, gh).create_vote(&input).await.unwrap();
}

#[tokio::test]
async fn create_vote_rejected_on_issue_does_not_update_check() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_valid_config(&mut gh);
    expect_user_is_collaborator(&mut gh, false);
    expect_comment(&mut gh, tmpl::VoteRestricted::new(USER).render().unwrap());
    gh.expect_is_check_required().never();
    gh.expect_create_check_run().never();

    // Run the command
    let input = CreateVoteInput::new(None, &Event::Issue(setup_test_issue_event()));
    setup_cmds_handler(MockDB::new(), gh).create_vote(&input).await.unwrap();
}

#[tokio::test]
async fn create_vote_rejected_on_opened_pr_automatic_cmd_from_non_collaborator() {
    // Setup event of a pull request opened by a user who is not a collaborator
    let sender = "external-contributor";
    let mut event = setup_test_pr_event();
    event.action = PullRequestEventAction::Opened;
    event.sender.login = sender.to_string();
    let event = Event::PullRequest(event);

    // Setup GitHub expectations: the rules are evaluated for the command and the check
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(3)
        .returning(|_, _, _| Box::pin(future::ready(Ok(Some(get_test_valid_config())))));
    gh.expect_get_pr_files()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM))
        .times(2)
        .returning(|_, _, _, _| {
            let file = File {
                filename: "README.md".to_string(),
            };
            Box::pin(future::ready(Ok(vec![file])))
        });
    gh.expect_user_is_collaborator()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(sender))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(false))));
    expect_comment(&mut gh, tmpl::VoteRestricted::new(sender).render().unwrap());
    expect_is_check_required(&mut gh, true);
    expect_check_run(
        &mut gh,
        CheckDetails {
            status: "completed".to_string(),
            summary: "An automation rule matches but there is no vote, maintainers should run /vote"
                .to_string(),

            conclusion: Some("failure".to_string()),
        },
    );
    let gh = Arc::new(gh);

    // Setup database expectations
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Ok(false));
    db.expect_store_vote().never();

    // Run the automatic command created from the event
    let Some(Command::CreateVote(input)) = Command::from_event(gh.clone(), &event).await.unwrap() else {
        panic!("expected a create vote command");
    };
    let (_, cmds_rx) = async_channel::unbounded();
    CommandsHandler::new(Arc::new(db), gh, cmds_rx).create_vote(&input).await.unwrap();
}

#[tokio::test]
async fn create_vote_rejected_on_opened_pr_check_not_required() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_valid_config(&mut gh);
    expect_comment(&mut gh, tmpl::ConfigProfileNotFound {}.render().unwrap());
    expect_is_check_required(&mut gh, false);
    gh.expect_create_check_run().never();

    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_has_vote().never();

    // Run the command using a profile that does not exist
    let input = CreateVoteInput::new(Some("typo"), &Event::PullRequest(setup_test_pr_event()));
    setup_cmds_handler(db, gh).create_vote(&input).await.unwrap();
}

#[tokio::test]
async fn create_vote_rejected_on_opened_pr_config_not_found_updates_check() {
    // Setup GitHub expectations: the configuration is also needed to decide the check
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(2)
        .returning(|_, _, _| Box::pin(future::ready(Ok(None))));
    let expected_body = tmpl::ConfigNotFound {}.render().unwrap();
    expect_comment(&mut gh, expected_body);
    expect_is_check_required(&mut gh, true);
    expect_check_run(&mut gh, cmd::no_vote_found_check());

    // Setup database expectations
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Ok(false));

    // Run the command
    let input = CreateVoteInput::new(None, &Event::PullRequest(setup_test_pr_event()));
    setup_cmds_handler(db, gh).create_vote(&input).await.unwrap();
}

#[tokio::test]
async fn create_vote_rejected_on_opened_pr_error_updating_check_is_ignored() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_valid_config(&mut gh);
    expect_comment(&mut gh, tmpl::ConfigProfileNotFound {}.render().unwrap());
    gh.expect_is_check_required()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(BRANCH))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    gh.expect_create_check_run().never();

    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_has_vote().never();

    // Run the command using a profile that does not exist
    let input = CreateVoteInput::new(Some("typo"), &Event::PullRequest(setup_test_pr_event()));
    setup_cmds_handler(db, gh).create_vote(&input).await.unwrap();
}

#[tokio::test]
async fn create_vote_rejected_on_opened_pr_non_collaborator_rule_matches_reports_failure() {
    // Setup GitHub expectations: the configuration is also needed to decide the check
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(2)
        .returning(|_, _, _| Box::pin(future::ready(Ok(Some(get_test_valid_config())))));
    expect_user_is_collaborator(&mut gh, false);
    expect_comment(&mut gh, tmpl::VoteRestricted::new(USER).render().unwrap());
    expect_is_check_required(&mut gh, true);
    gh.expect_get_pr_files()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _, _, _| {
            let file = File {
                filename: "README.md".to_string(),
            };
            Box::pin(future::ready(Ok(vec![file])))
        });
    expect_check_run(
        &mut gh,
        CheckDetails {
            status: "completed".to_string(),
            summary: "An automation rule matches but there is no vote, maintainers should run /vote"
                .to_string(),

            conclusion: Some("failure".to_string()),
        },
    );

    // Setup database expectations
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Ok(false));

    // Run the command
    let input = CreateVoteInput::new(None, &Event::PullRequest(setup_test_pr_event()));
    setup_cmds_handler(db, gh).create_vote(&input).await.unwrap();
}

#[tokio::test]
async fn create_vote_rejected_on_opened_pr_profile_not_found_updates_check() {
    // Setup GitHub expectations: the configuration is also needed to decide the check
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(2)
        .returning(|_, _, _| Box::pin(future::ready(Ok(Some(get_test_valid_config())))));
    expect_comment(&mut gh, tmpl::ConfigProfileNotFound {}.render().unwrap());
    expect_is_check_required(&mut gh, true);
    gh.expect_get_pr_files()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _, _, _| {
            let file = File {
                filename: "main.rs".to_string(),
            };
            Box::pin(future::ready(Ok(vec![file])))
        });
    expect_check_run(&mut gh, cmd::no_vote_found_check());

    // Setup database expectations
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Ok(false));

    // Run the command using a profile that does not exist
    let input = CreateVoteInput::new(Some("typo"), &Event::PullRequest(setup_test_pr_event()));
    setup_cmds_handler(db, gh).create_vote(&input).await.unwrap();
}

#[tokio::test]
async fn create_vote_rejected_on_opened_pr_with_vote_does_not_update_check() {
    // A vote created concurrently by a maintainer owns the check, whatever the outcome
    // of the lookup: a failed one is ignored like other errors updating the check
    let cases: [fn() -> Result<bool>; 2] = [|| Ok(true), || Err(format_err!(ERROR))];
    for has_vote_result in cases {
        // Setup GitHub expectations: the check is also decided from the configuration
        let mut gh = MockGH::new();
        gh.expect_get_config_file()
            .with(eq(INST_ID), eq(ORG), eq(REPO))
            .times(2)
            .returning(|_, _, _| Box::pin(future::ready(Ok(Some(get_test_valid_config())))));
        expect_user_is_collaborator(&mut gh, false);
        expect_comment(&mut gh, tmpl::VoteRestricted::new(USER).render().unwrap());
        expect_is_check_required(&mut gh, true);
        gh.expect_get_pr_files()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM))
            .times(1)
            .returning(|_, _, _, _| {
                let file = File {
                    filename: "main.rs".to_string(),
                };
                Box::pin(future::ready(Ok(vec![file])))
            });
        gh.expect_create_check_run().never();

        // Setup database expectations
        let mut db = MockDB::new();
        expect_has_vote(&mut db, has_vote_result());
        db.expect_store_vote().never();

        // Run the command and check it succeeds without reporting the check
        let input = CreateVoteInput::new(None, &Event::PullRequest(setup_test_pr_event()));
        setup_cmds_handler(db, gh).create_vote(&input).await.unwrap();
    }
}

#[tokio::test]
async fn create_vote_removing_labels_errors_are_ignored() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_has_vote_open()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(false))));
    expect_store_vote(&mut db, false, || Ok(test_vote_id()));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_valid_config(&mut gh);
    expect_user_is_collaborator(&mut gh, true);
    expect_vote_created_comment(&mut gh, &mut Sequence::new(), false);
    for label in [VOTE_CLOSED_LABEL, VOTE_PASSED_LABEL, VOTE_FAILED_LABEL] {
        gh.expect_remove_label()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM), eq(label))
            .times(1)
            .returning(|_, _, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    }
    gh.expect_add_labels()
        .withf(|inst_id, owner, repo, issue_number, labels| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && labels == vec![GITVOTE_LABEL, VOTE_OPEN_LABEL]
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));

    // Run and check the vote is created successfully
    let input = CreateVoteInput::new(None, &Event::Issue(setup_test_issue_event()));
    setup_cmds_handler(db, gh).create_vote(&input).await.unwrap();
}

#[tokio::test]
async fn create_vote_success() {
    // Setup input
    let event = setup_test_issue_event();
    let create_vote_input = CreateVoteInput::new(None, &Event::Issue(event));
    let cfg = CfgProfile {
        duration: Duration::from_mins(5),
        pass_threshold: 50.0,

        allowed_voters: Some(AllowedVoters::default()),
        ..Default::default()
    };

    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_has_vote_open()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(false))));
    let cfg_copy = cfg.clone();
    let create_vote_input_copy = create_vote_input.clone();
    db.expect_store_vote()
        .withf(move |vote_comment_id, input, cfg| {
            *vote_comment_id == COMMENT_ID && *input == create_vote_input_copy && *cfg == cfg_copy
        })
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(test_vote_id()))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_valid_config(&mut gh);
    gh.expect_user_is_collaborator()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(true))));
    let create_vote_input_copy = create_vote_input.clone();
    gh.expect_post_comment()
        .withf(move |inst_id, owner, repo, issue_number, body| {
            let expected_body = tmpl::VoteCreated::new(&create_vote_input_copy, &cfg).render().unwrap();
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body == expected_body.as_str()
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
    gh.expect_remove_label()
        .with(
            eq(INST_ID),
            eq(ORG),
            eq(REPO),
            eq(ISSUE_NUM),
            eq(VOTE_CLOSED_LABEL),
        )
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
    gh.expect_remove_label()
        .with(
            eq(INST_ID),
            eq(ORG),
            eq(REPO),
            eq(ISSUE_NUM),
            eq(VOTE_PASSED_LABEL),
        )
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
    gh.expect_remove_label()
        .with(
            eq(INST_ID),
            eq(ORG),
            eq(REPO),
            eq(ISSUE_NUM),
            eq(VOTE_FAILED_LABEL),
        )
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
    gh.expect_add_labels()
        .withf(|inst_id, owner, repo, issue_number, labels| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && labels == vec![GITVOTE_LABEL, VOTE_OPEN_LABEL]
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));

    // Run and check the vote is created successfully
    let (_, cmds_rx) = async_channel::unbounded();
    let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
    cmds_handler.create_vote(&create_vote_input).await.unwrap();
}

#[tokio::test]
async fn create_vote_vote_in_progress_on_opened_pr_keeps_check() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_has_vote_open()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(true))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_valid_config(&mut gh);
    expect_user_is_collaborator(&mut gh, true);
    expect_comment(&mut gh, vote_in_progress_body(true));
    gh.expect_is_check_required().never();
    gh.expect_create_check_run().never();

    // Run the command
    let input = CreateVoteInput::new(None, &Event::PullRequest(setup_test_pr_event()));
    setup_cmds_handler(db, gh).create_vote(&input).await.unwrap();
}

// Helpers.

/// Expects storing a vote to fail once because the issue/pr has a vote open already.
fn expect_store_vote_lost_race(db: &mut MockDB, is_pull_request: bool) {
    // Expect the open vote check to pass and storing the vote to lose the race
    db.expect_has_vote_open()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(false))));
    expect_store_vote(db, is_pull_request, || Err(VoteAlreadyOpenError.into()));

    // Expect no lookup of the vote, as the error says that nothing was stored
    db.expect_has_vote_with_comment().never();
}
