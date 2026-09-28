use std::{sync::Arc, vec};

use anyhow::format_err;
use mockall::{Sequence, predicate::eq};
use time::ext::NumericalDuration;

use crate::results::{REACTION_IN_FAVOR, Vote};
use crate::testutil::*;
use crate::{
    cfg_repo::{AllowedVoters, PassThresholdBase},
    db::MockDB,
    github::*,
};

use super::*;

#[tokio::test]
async fn votes_processor_stops_when_requested() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_close_finished_vote().times(1).returning(|_| Box::pin(future::ready(Ok(None))));
    db.expect_get_pending_status_checks()
        .times(1)
        .returning(|| Box::pin(future::ready(Ok(vec![]))));
    db.expect_get_open_votes_with_close_on_passing()
        .times(1)
        .returning(|| Box::pin(future::ready(Ok(vec![]))));
    let gh = MockGH::new();

    // Run the processor until it's asked to stop
    let (cmds_tx, cmds_rx) = async_channel::unbounded();
    let cancel_token = CancellationToken::new();
    let votes_processor = Processor::new(Arc::new(db), Arc::new(gh), cmds_tx, cmds_rx);
    let votes_processor_handle = votes_processor.run(&cancel_token);
    cancel_token.cancel();

    // Check all worker tasks completed
    assert!(votes_processor_handle.await.iter().all(Result::is_ok));
}

#[tokio::test]
async fn commands_handler_processes_create_and_check_vote_cmds() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_get_open_vote()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(None))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(None)));
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

    // Queue commands
    let (cmds_tx, cmds_rx) = async_channel::unbounded();
    let event = Event::Issue(setup_test_issue_event());
    cmds_tx.send(Command::CreateVote(CreateVoteInput::new(None, &event))).await.unwrap();
    cmds_tx.send(Command::CheckVote(CheckVoteInput::new(&event))).await.unwrap();

    // Run the commands handler until it's asked to stop
    let cancel_token = CancellationToken::new();
    let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx.clone());
    let cmds_handler_handle = cmds_handler.run(cancel_token.clone());
    cancel_token.cancel();

    // Check all commands were processed
    assert!(cmds_handler_handle.await.is_ok());
    assert!(cmds_rx.is_empty());
}

#[tokio::test]
async fn commands_handler_stops_after_processing_queued_cmd() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_cancel_vote()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_user_is_collaborator()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(true))));

    // Queue command
    let (cmds_tx, cmds_rx) = async_channel::unbounded();
    let cancel_token = CancellationToken::new();
    let event = setup_test_issue_event();
    let cmd = Command::CancelVote(CancelVoteInput::new(&Event::Issue(event)));
    cmds_tx.send(cmd).await.unwrap();

    // Run the commands handler until it's asked to stop
    let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx.clone());
    let cmds_handler_handle = cmds_handler.run(cancel_token.clone());
    cancel_token.cancel();

    // Check the command was processed
    assert!(cmds_handler_handle.await.is_ok());
    assert!(cmds_rx.is_empty());
}

#[tokio::test]
async fn commands_handler_stops_when_requested() {
    // Setup mocks
    let db = MockDB::new();
    let gh = MockGH::new();

    // Run the commands handler until it's asked to stop
    let (_, cmds_rx) = async_channel::unbounded();
    let cancel_token = CancellationToken::new();
    let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
    let cmds_handler_handle = cmds_handler.run(cancel_token.clone());
    cancel_token.cancel();

    // Check the commands handler completed
    assert!(cmds_handler_handle.await.is_ok());
}

#[tokio::test]
async fn create_vote_error_adding_labels() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_has_vote_open()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(false))));
    db.expect_store_vote()
        .withf(|vote_comment_id, _, _| *vote_comment_id == COMMENT_ID)
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(Uuid::parse_str(VOTE_ID).unwrap()))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_valid_config(&mut gh);
    expect_user_is_collaborator(&mut gh, true);
    gh.expect_post_comment()
        .withf(|inst_id, owner, repo, issue_number, _| {
            *inst_id == INST_ID && owner == ORG && repo == REPO && *issue_number == ISSUE_NUM
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
    gh.expect_remove_label()
        .withf(|inst_id, owner, repo, issue_number, _| {
            *inst_id == INST_ID && owner == ORG && repo == REPO && *issue_number == ISSUE_NUM
        })
        .times(3)
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
        .returning(|_, _, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Run and check the error is propagated
    let input = CreateVoteInput::new(None, &Event::Issue(setup_test_issue_event()));
    let err = setup_cmds_handler(db, gh).create_vote(&input).await.unwrap_err();
    assert_eq!(err.to_string(), ERROR);
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
async fn create_vote_error_getting_configuration_profile() {
    // Setup GitHub expectations
    let db = MockDB::new();
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(None)));
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
        .withf(|inst_id, owner, repo, issue_number, _| {
            *inst_id == INST_ID && owner == ORG && repo == REPO && *issue_number == ISSUE_NUM
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
    db.expect_store_vote()
        .withf(|vote_comment_id, _, _| *vote_comment_id == COMMENT_ID)
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_valid_config(&mut gh);
    expect_user_is_collaborator(&mut gh, true);
    gh.expect_post_comment()
        .withf(|inst_id, owner, repo, issue_number, _| {
            *inst_id == INST_ID && owner == ORG && repo == REPO && *issue_number == ISSUE_NUM
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
    gh.expect_create_check_run().never();
    gh.expect_remove_label().never();
    gh.expect_add_labels().never();

    // Run and check the error is propagated
    let input = CreateVoteInput::new(None, &Event::PullRequest(setup_test_pr_event()));
    let err = setup_cmds_handler(db, gh).create_vote(&input).await.unwrap_err();
    assert_eq!(err.to_string(), ERROR);
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
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Some(get_test_valid_config()))));
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
async fn create_vote_non_collaborator() {
    // Setup GitHub expectations
    let db = MockDB::new();
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Some(get_test_valid_config()))));
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
    db.expect_store_vote()
        .withf(|vote_comment_id, _, _| *vote_comment_id == COMMENT_ID)
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(Uuid::parse_str(VOTE_ID).unwrap()))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_valid_config(&mut gh);
    expect_user_is_collaborator(&mut gh, true);
    gh.expect_post_comment()
        .withf(|inst_id, owner, repo, issue_number, _| {
            *inst_id == INST_ID && owner == ORG && repo == REPO && *issue_number == ISSUE_NUM
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
    gh.expect_create_check_run()
        .withf(|inst_id, owner, repo, issue_number, _| {
            *inst_id == INST_ID && owner == ORG && repo == REPO && *issue_number == ISSUE_NUM
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    gh.expect_remove_label().never();
    gh.expect_add_labels().never();

    // Run and check the error is propagated
    let input = CreateVoteInput::new(None, &Event::PullRequest(setup_test_pr_event()));
    let err = setup_cmds_handler(db, gh).create_vote(&input).await.unwrap_err();
    assert_eq!(err.to_string(), ERROR);
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
        .returning(|_, _, _| Box::pin(future::ready(Ok(Uuid::new_v4()))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Some(get_test_valid_config()))));
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
    gh.expect_create_check_run()
        .with(
            eq(INST_ID),
            eq(ORG),
            eq(REPO),
            eq(ISSUE_NUM),
            eq(CheckDetails {
                status: "in_progress".to_string(),
                conclusion: None,
                summary: "Vote open".to_string(),
            }),
        )
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
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
async fn create_vote_removing_labels_errors_are_ignored() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_has_vote_open()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(false))));
    db.expect_store_vote()
        .withf(|vote_comment_id, _, _| *vote_comment_id == COMMENT_ID)
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(Uuid::parse_str(VOTE_ID).unwrap()))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_valid_config(&mut gh);
    expect_user_is_collaborator(&mut gh, true);
    gh.expect_post_comment()
        .withf(|inst_id, owner, repo, issue_number, _| {
            *inst_id == INST_ID && owner == ORG && repo == REPO && *issue_number == ISSUE_NUM
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
    gh.expect_remove_label()
        .withf(|inst_id, owner, repo, issue_number, _| {
            *inst_id == INST_ID && owner == ORG && repo == REPO && *issue_number == ISSUE_NUM
        })
        .times(3)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
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
        .returning(|_, _, _| Box::pin(future::ready(Ok(Uuid::new_v4()))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Some(get_test_valid_config()))));
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
#[should_panic(expected = "error cancelling vote")]
async fn cancel_vote_error_cancelling() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_cancel_vote()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_user_is_collaborator()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(true))));

    // Run and check the error is propagated
    let (_, cmds_rx) = async_channel::unbounded();
    let event = setup_test_issue_comment_event();
    let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
    cmds_handler
        .cancel_vote(&CancelVoteInput::new(&Event::IssueComment(event)))
        .await
        .unwrap();
}

#[tokio::test]
#[should_panic(expected = "error checking if user is collaborator")]
async fn cancel_vote_error_checking_if_user_is_collaborator() {
    // Setup GitHub expectations
    let db = MockDB::new();
    let mut gh = MockGH::new();
    gh.expect_user_is_collaborator()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Run and check the error is propagated
    let (_, cmds_rx) = async_channel::unbounded();
    let event = setup_test_issue_comment_event();
    let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
    cmds_handler
        .cancel_vote(&CancelVoteInput::new(&Event::IssueComment(event)))
        .await
        .unwrap();
}

#[tokio::test]
async fn cancel_vote_error_posting_comment() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_cancel_vote()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(Some(Uuid::parse_str(VOTE_ID).unwrap())))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_user_is_collaborator(&mut gh, true);
    gh.expect_post_comment()
        .withf(|inst_id, owner, repo, issue_number, _| {
            *inst_id == INST_ID && owner == ORG && repo == REPO && *issue_number == ISSUE_NUM
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    gh.expect_remove_label().never();

    // Run and check the error is propagated
    let input = CancelVoteInput::new(&Event::IssueComment(setup_test_issue_comment_event()));
    let err = setup_cmds_handler(db, gh).cancel_vote(&input).await.unwrap_err();
    assert_eq!(err.to_string(), ERROR);
}

#[tokio::test]
async fn cancel_vote_in_pr_error_creating_check_run() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_cancel_vote()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(Some(Uuid::parse_str(VOTE_ID).unwrap())))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_user_is_collaborator(&mut gh, true);
    gh.expect_post_comment()
        .withf(|inst_id, owner, repo, issue_number, body| {
            let expected_body = tmpl::VoteCancelled::new(USER, true).render().unwrap();
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body == expected_body.as_str()
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
    gh.expect_create_check_run()
        .withf(|inst_id, owner, repo, issue_number, _| {
            *inst_id == INST_ID && owner == ORG && repo == REPO && *issue_number == ISSUE_NUM
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    gh.expect_remove_label().never();

    // Run and check the error is propagated
    let input = CancelVoteInput::new(&Event::PullRequest(setup_test_pr_event()));
    let err = setup_cmds_handler(db, gh).cancel_vote(&input).await.unwrap_err();
    assert_eq!(err.to_string(), ERROR);
}

#[tokio::test]
async fn cancel_vote_in_pr_no_vote_in_progress() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_cancel_vote()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(None))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_user_is_collaborator(&mut gh, true);
    gh.expect_post_comment()
        .withf(|inst_id, owner, repo, issue_number, body| {
            let expected_body = tmpl::NoVoteInProgress::new(USER, true).render().unwrap();
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body == expected_body.as_str()
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
    gh.expect_create_check_run().never();
    gh.expect_remove_label().never();

    // Run and check the vote is not cancelled
    let input = CancelVoteInput::new(&Event::PullRequest(setup_test_pr_event()));
    setup_cmds_handler(db, gh).cancel_vote(&input).await.unwrap();
}

#[tokio::test]
async fn cancel_vote_in_pr_success() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_cancel_vote()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(Some(Uuid::parse_str(VOTE_ID).unwrap())))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_user_is_collaborator()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(true))));
    gh.expect_post_comment()
        .withf(|inst_id, owner, repo, issue_number, body| {
            let expected_body = tmpl::VoteCancelled::new(USER, true).render().unwrap();
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body == expected_body.as_str()
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
    gh.expect_create_check_run()
        .with(
            eq(INST_ID),
            eq(ORG),
            eq(REPO),
            eq(ISSUE_NUM),
            eq(CheckDetails {
                status: "completed".to_string(),
                conclusion: Some("success".to_string()),
                summary: "Vote cancelled".to_string(),
            }),
        )
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
    gh.expect_remove_label()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM), eq(VOTE_OPEN_LABEL))
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));

    // Run and check the vote is cancelled successfully
    let (_, cmds_rx) = async_channel::unbounded();
    let event = setup_test_pr_event();
    let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
    cmds_handler.cancel_vote(&CancelVoteInput::new(&Event::PullRequest(event))).await.unwrap();
}

#[tokio::test]
async fn cancel_vote_no_vote_in_progress() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_cancel_vote()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(None))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_user_is_collaborator()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(true))));
    gh.expect_post_comment()
        .withf(|inst_id, owner, repo, issue_number, body| {
            let expected_body = tmpl::NoVoteInProgress::new(USER, false).render().unwrap();
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body == expected_body.as_str()
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));

    // Run and check the vote is not cancelled
    let (_, cmds_rx) = async_channel::unbounded();
    let event = setup_test_issue_comment_event();
    let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
    cmds_handler
        .cancel_vote(&CancelVoteInput::new(&Event::IssueComment(event)))
        .await
        .unwrap();
}

#[tokio::test]
async fn cancel_vote_only_collaborators_can_close_votes() {
    // Setup GitHub expectations
    let db = MockDB::new();
    let mut gh = MockGH::new();
    gh.expect_user_is_collaborator()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(false))));

    // Run and check the vote is not cancelled
    let (_, cmds_rx) = async_channel::unbounded();
    let event = setup_test_issue_comment_event();
    let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
    cmds_handler
        .cancel_vote(&CancelVoteInput::new(&Event::IssueComment(event)))
        .await
        .unwrap();
}

#[tokio::test]
async fn cancel_vote_removing_label_error_is_ignored() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_cancel_vote()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(Some(Uuid::parse_str(VOTE_ID).unwrap())))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_user_is_collaborator(&mut gh, true);
    gh.expect_post_comment()
        .withf(|inst_id, owner, repo, issue_number, _| {
            *inst_id == INST_ID && owner == ORG && repo == REPO && *issue_number == ISSUE_NUM
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
    gh.expect_remove_label()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM), eq(VOTE_OPEN_LABEL))
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Run and check the vote is cancelled successfully
    let input = CancelVoteInput::new(&Event::IssueComment(setup_test_issue_comment_event()));
    setup_cmds_handler(db, gh).cancel_vote(&input).await.unwrap();
}

#[tokio::test]
async fn cancel_vote_success() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_cancel_vote()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(Some(Uuid::parse_str(VOTE_ID).unwrap())))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_user_is_collaborator()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(true))));
    gh.expect_post_comment()
        .withf(|inst_id, owner, repo, issue_number, body| {
            let expected_body = tmpl::VoteCancelled::new(USER, false).render().unwrap();
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body == expected_body.as_str()
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
    gh.expect_remove_label()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM), eq(VOTE_OPEN_LABEL))
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));

    // Run and check the vote is cancelled successfully
    let (_, cmds_rx) = async_channel::unbounded();
    let event = setup_test_issue_comment_event();
    let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
    cmds_handler
        .cancel_vote(&CancelVoteInput::new(&Event::IssueComment(event)))
        .await
        .unwrap();
}

#[tokio::test]
async fn check_vote_checked_long_ago() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_get_open_vote().with(eq(REPOFN), eq(ISSUE_NUM)).times(1).returning(|_, _| {
        Box::pin(future::ready(Ok(Some(Vote {
            checked_at: OffsetDateTime::now_utc().checked_sub(25.hours()),
            ..setup_test_vote()
        }))))
    });
    db.expect_update_vote_last_check()
        .with(eq(Uuid::parse_str(VOTE_ID).unwrap()))
        .times(1)
        .returning(|_| Box::pin(future::ready(Ok(()))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_vote_results(&mut gh, COMMENT_ID, Ok(vec![in_favor_reaction(USER1)]));
    gh.expect_post_comment()
        .withf(|inst_id, owner, repo, issue_number, body| {
            let results = setup_test_vote_results();
            let expected_body = tmpl::VoteStatus::new(&results).render().unwrap();
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body == expected_body.as_str()
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));

    // Run the check
    let input = CheckVoteInput::new(&Event::PullRequest(setup_test_pr_event()));
    setup_cmds_handler(db, gh).check_vote(&input).await.unwrap();
}

#[tokio::test]
async fn check_vote_checked_recently() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_get_open_vote().with(eq(REPOFN), eq(ISSUE_NUM)).times(1).returning(|_, _| {
        Box::pin(future::ready(Ok(Some(Vote {
            checked_at: OffsetDateTime::now_utc().checked_sub(1.hours()),
            ..setup_test_vote()
        }))))
    });

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_post_comment()
        .withf(|inst_id, owner, repo, issue_number, body| {
            let expected_body = tmpl::VoteCheckedRecently {}.render().unwrap();
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body == expected_body.as_str()
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));

    // Run and check the recently checked comment is posted
    let (_, cmds_rx) = async_channel::unbounded();
    let event = setup_test_pr_event();
    let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
    cmds_handler.check_vote(&CheckVoteInput::new(&Event::PullRequest(event))).await.unwrap();
}

#[tokio::test]
async fn check_vote_error_calculating_results() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_get_open_vote()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(Some(setup_test_vote())))));
    db.expect_update_vote_last_check().never();

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_comment_reactions()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(COMMENT_ID))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    gh.expect_post_comment().never();

    // Run and check the error is propagated
    let input = CheckVoteInput::new(&Event::PullRequest(setup_test_pr_event()));
    let err = setup_cmds_handler(db, gh).check_vote(&input).await.unwrap_err();
    assert_eq!(err.to_string(), ERROR);
}

#[tokio::test]
#[should_panic(expected = "error getting open vote")]
async fn check_vote_error_getting_vote() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_get_open_vote()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    let gh = MockGH::new();

    // Run and check the error is propagated
    let (_, cmds_rx) = async_channel::unbounded();
    let event = setup_test_pr_event();
    let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
    cmds_handler.check_vote(&CheckVoteInput::new(&Event::PullRequest(event))).await.unwrap();
}

#[tokio::test]
async fn check_vote_error_updating_last_check() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_get_open_vote()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(Some(setup_test_vote())))));
    db.expect_update_vote_last_check()
        .with(eq(Uuid::parse_str(VOTE_ID).unwrap()))
        .times(1)
        .returning(|_| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_vote_results(&mut gh, COMMENT_ID, Ok(vec![in_favor_reaction(USER1)]));
    gh.expect_post_comment()
        .withf(|inst_id, owner, repo, issue_number, _| {
            *inst_id == INST_ID && owner == ORG && repo == REPO && *issue_number == ISSUE_NUM
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));

    // Run and check the error is propagated
    let input = CheckVoteInput::new(&Event::PullRequest(setup_test_pr_event()));
    let err = setup_cmds_handler(db, gh).check_vote(&input).await.unwrap_err();
    assert_eq!(err.to_string(), ERROR);
}

#[tokio::test]
async fn check_vote_not_found() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_get_open_vote()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(None))));
    let gh = MockGH::new();

    // Run and check the missing vote is ignored
    let (_, cmds_rx) = async_channel::unbounded();
    let event = setup_test_pr_event();
    let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
    cmds_handler.check_vote(&CheckVoteInput::new(&Event::PullRequest(event))).await.unwrap();
}

#[tokio::test]
async fn check_vote_success() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_get_open_vote()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(Some(setup_test_vote())))));
    db.expect_update_vote_last_check()
        .with(eq(Uuid::parse_str(VOTE_ID).unwrap()))
        .times(1)
        .returning(|_| Box::pin(future::ready(Ok(()))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_comment_reactions()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(COMMENT_ID))
        .times(1)
        .returning(|_, _, _, _| {
            Box::pin(future::ready(Ok(vec![Reaction {
                user: User {
                    login: USER1.to_string(),
                },
                content: REACTION_IN_FAVOR.to_string(),
                created_at: TIMESTAMP.to_string(),
            }])))
        });
    gh.expect_get_allowed_voters()
        .withf(|inst_id, cfg, owner, repo, org| {
            *inst_id == INST_ID
                && *cfg == setup_test_vote().cfg
                && owner == ORG
                && repo == REPO
                && *org == Some(ORG.to_string()).as_ref()
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(vec![USER1.to_string()]))));
    gh.expect_post_comment()
        .withf(|inst_id, owner, repo, issue_number, body| {
            let results = setup_test_vote_results();
            let expected_body = tmpl::VoteStatus::new(&results).render().unwrap();
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body == expected_body.as_str()
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));

    // Run and check the vote status is posted
    let (_, cmds_rx) = async_channel::unbounded();
    let event = setup_test_pr_event();
    let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
    cmds_handler.check_vote(&CheckVoteInput::new(&Event::PullRequest(event))).await.unwrap();
}

#[tokio::test]
async fn votes_closer_closes_pending_votes_until_none_left() {
    // Setup database expectations
    let (none_left_tx, none_left_rx) = tokio::sync::oneshot::channel();
    let mut seq = Sequence::new();
    let mut db = MockDB::new();
    db.expect_close_finished_vote()
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_| Box::pin(future::ready(Ok(Some((setup_test_vote(), None))))));
    db.expect_close_finished_vote().times(1).in_sequence(&mut seq).return_once(move |_| {
        none_left_tx.send(()).unwrap();
        Box::pin(future::ready(Ok(None)))
    });

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_remove_label()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM), eq(VOTE_OPEN_LABEL))
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
    gh.expect_add_labels()
        .withf(|inst_id, owner, repo, issue_number, labels| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && labels == vec![VOTE_CLOSED_LABEL]
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));

    // Run the votes closer until no pending votes are left
    let cancel_token = CancellationToken::new();
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    let votes_closer_handle = votes_closer.run(cancel_token.clone());
    none_left_rx.await.unwrap();
    cancel_token.cancel();

    // Check the worker completed (expectations are verified on drop)
    assert!(votes_closer_handle.await.is_ok());
}

#[tokio::test]
async fn votes_closer_stops_when_requested_error_closing() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_close_finished_vote()
        .times(1)
        .returning(|_| Box::pin(future::ready(Err(format_err!(ERROR)))));
    let gh = MockGH::new();

    // Run the votes closer until it's asked to stop
    let cancel_token = CancellationToken::new();
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    let votes_closer_handle = votes_closer.run(cancel_token.clone());
    cancel_token.cancel();

    // Check the worker completed
    assert!(votes_closer_handle.await.is_ok());
}

#[tokio::test]
async fn votes_closer_stops_when_requested_none_closed() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_close_finished_vote().times(1).returning(|_| Box::pin(future::ready(Ok(None))));
    let gh = MockGH::new();

    // Run the votes closer until it's asked to stop
    let cancel_token = CancellationToken::new();
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    let votes_closer_handle = votes_closer.run(cancel_token.clone());
    cancel_token.cancel();

    // Check the worker completed
    assert!(votes_closer_handle.await.is_ok());
}

#[tokio::test]
async fn close_finished_vote_error_adding_labels() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_close_finished_vote().times(1).returning(|_| {
        let mut vote = setup_test_vote();
        vote.cfg.announcements = None;
        Box::pin(future::ready(Ok(Some((vote, Some(setup_test_vote_results()))))))
    });

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_post_comment()
        .withf(|inst_id, owner, repo, issue_number, _| {
            *inst_id == INST_ID && owner == ORG && repo == REPO && *issue_number == ISSUE_NUM
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
    gh.expect_remove_label()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM), eq(VOTE_OPEN_LABEL))
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
    gh.expect_add_labels()
        .withf(|inst_id, owner, repo, issue_number, labels| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && labels == vec![VOTE_CLOSED_LABEL, VOTE_PASSED_LABEL]
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Run and check the error is propagated
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    let err = votes_closer.close_finished_vote().await.unwrap_err();
    assert_eq!(err.to_string(), ERROR);
}

#[tokio::test]
#[should_panic(expected = "error closing finished vote")]
async fn close_finished_vote_error_closing() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_close_finished_vote()
        .times(1)
        .returning(|_| Box::pin(future::ready(Err(format_err!(ERROR)))));
    let gh = MockGH::new();

    // Run and check the error is propagated
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    votes_closer.close_finished_vote().await.unwrap();
}

#[tokio::test]
async fn close_finished_vote_error_creating_announcement_is_ignored() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_close_finished_vote().times(1).returning(|_| {
        Box::pin(future::ready(Ok(Some((
            setup_test_vote(),
            Some(setup_test_vote_results()),
        )))))
    });

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_post_comment()
        .withf(|inst_id, owner, repo, issue_number, _| {
            *inst_id == INST_ID && owner == ORG && repo == REPO && *issue_number == ISSUE_NUM
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
    gh.expect_create_discussion()
        .withf(|inst_id, owner, repo, category, _, _| {
            *inst_id == INST_ID && owner == ORG && repo == REPO && category == DISCUSSIONS_CATEGORY
        })
        .times(1)
        .returning(|_, _, _, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    gh.expect_remove_label()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM), eq(VOTE_OPEN_LABEL))
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
    gh.expect_add_labels()
        .withf(|inst_id, owner, repo, issue_number, labels| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && labels == vec![VOTE_CLOSED_LABEL, VOTE_PASSED_LABEL]
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));

    // Run and check the vote is closed successfully
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_error_posting_comment() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_close_finished_vote().times(1).returning(|_| {
        Box::pin(future::ready(Ok(Some((
            setup_test_vote(),
            Some(setup_test_vote_results()),
        )))))
    });

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_post_comment()
        .withf(|inst_id, owner, repo, issue_number, _| {
            *inst_id == INST_ID && owner == ORG && repo == REPO && *issue_number == ISSUE_NUM
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    gh.expect_create_discussion().never();
    gh.expect_remove_label().never();
    gh.expect_add_labels().never();

    // Run and check the error is propagated
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    let err = votes_closer.close_finished_vote().await.unwrap_err();
    assert_eq!(err.to_string(), ERROR);
}

#[tokio::test]
async fn close_finished_vote_none_closed() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_close_finished_vote().times(1).returning(|_| Box::pin(future::ready(Ok(None))));
    let gh = MockGH::new();

    // Run and check no vote is closed
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    votes_closer.close_finished_vote().await.unwrap();
}

#[tokio::test]
async fn close_finished_vote_on_issue() {
    // Setup vote results
    let results = setup_test_vote_results();
    let results_copy = results.clone();
    let results_copy2 = results.clone();

    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_close_finished_vote().times(1).returning(move |_| {
        Box::pin(future::ready(Ok(Some((
            setup_test_vote(),
            Some(results_copy.clone()),
        )))))
    });

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_post_comment()
        .withf(move |inst_id, owner, repo, issue_number, body| {
            let expected_body = tmpl::VoteClosed::new(&results_copy2).render().unwrap();
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body == expected_body.as_str()
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
    gh.expect_create_discussion()
        .withf(move |inst_id, owner, repo, category, title, body| {
            let expected_body =
                tmpl::VoteClosedAnnouncement::new(ISSUE_NUM, TITLE, &results).render().unwrap();
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && category == DISCUSSIONS_CATEGORY
                && title == build_announcement_title(ISSUE_NUM, TITLE)
                && body == expected_body.as_str()
        })
        .times(1)
        .returning(|_, _, _, _, _, _| Box::pin(future::ready(Ok(()))));
    gh.expect_remove_label()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM), eq(VOTE_OPEN_LABEL))
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
    gh.expect_add_labels()
        .withf(|inst_id, owner, repo, issue_number, labels| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && labels == vec![VOTE_CLOSED_LABEL, VOTE_PASSED_LABEL]
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));

    // Run and check the vote is closed
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    votes_closer.close_finished_vote().await.unwrap();
}

#[tokio::test]
async fn close_finished_vote_on_issue_without_results() {
    // Setup database expectations (vote comment was deleted)
    let mut db = MockDB::new();
    db.expect_close_finished_vote()
        .times(1)
        .returning(|_| Box::pin(future::ready(Ok(Some((setup_test_vote(), None))))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_post_comment().never();
    gh.expect_create_discussion().never();
    gh.expect_create_check_run().never();
    gh.expect_remove_label()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM), eq(VOTE_OPEN_LABEL))
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
    gh.expect_add_labels()
        .withf(|inst_id, owner, repo, issue_number, labels| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && labels == vec![VOTE_CLOSED_LABEL]
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));

    // Run and check the vote is closed
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_on_pr_closed_vote_not_passed() {
    // Setup vote results
    let mut results = setup_test_vote_results();
    results.passed = false;
    results.in_favor_percentage = 0.0;
    results.in_favor = 0;
    results.against = 1;
    let results_copy = results.clone();
    let results_copy2 = results.clone();

    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_close_finished_vote().times(1).returning(move |_| {
        let mut vote = setup_test_vote();
        vote.is_pull_request = true;
        Box::pin(future::ready(Ok(Some((vote, Some(results_copy.clone()))))))
    });

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_post_comment()
        .withf(move |inst_id, owner, repo, issue_number, body| {
            let expected_body = tmpl::VoteClosed::new(&results_copy2).render().unwrap();
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body == expected_body.as_str()
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
    gh.expect_create_discussion()
        .withf(move |inst_id, owner, repo, category, title, body| {
            let expected_body =
                tmpl::VoteClosedAnnouncement::new(ISSUE_NUM, TITLE, &results).render().unwrap();
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && category == DISCUSSIONS_CATEGORY
                && title == build_announcement_title(ISSUE_NUM, TITLE)
                && body == expected_body.as_str()
        })
        .times(1)
        .returning(|_, _, _, _, _, _| Box::pin(future::ready(Ok(()))));
    gh.expect_create_check_run()
        .with(
            eq(INST_ID),
            eq(ORG),
            eq(REPO),
            eq(ISSUE_NUM),
            eq(CheckDetails {
                status: "completed".to_string(),
                conclusion: Some("failure".to_string()),
                summary: "The vote did not pass. 0 out of 1 voted in favor.".to_string(),
            }),
        )
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
    gh.expect_remove_label()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM), eq(VOTE_OPEN_LABEL))
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
    gh.expect_add_labels()
        .withf(|inst_id, owner, repo, issue_number, labels| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && labels == vec![VOTE_CLOSED_LABEL, VOTE_FAILED_LABEL]
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));

    // Run and check the PR vote is closed
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    votes_closer.close_finished_vote().await.unwrap();
}

#[tokio::test]
async fn close_finished_vote_on_pr_closed_vote_passed() {
    // Setup vote results
    let results = setup_test_vote_results();
    let results_copy = results.clone();
    let results_copy2 = results.clone();

    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_close_finished_vote().times(1).returning(move |_| {
        let mut vote = setup_test_vote();
        vote.is_pull_request = true;
        Box::pin(future::ready(Ok(Some((vote, Some(results_copy.clone()))))))
    });

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_post_comment()
        .withf(move |inst_id, owner, repo, issue_number, body| {
            let expected_body = tmpl::VoteClosed::new(&results_copy2).render().unwrap();
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body == expected_body.as_str()
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
    gh.expect_create_discussion()
        .withf(move |inst_id, owner, repo, category, title, body| {
            let expected_body =
                tmpl::VoteClosedAnnouncement::new(ISSUE_NUM, TITLE, &results).render().unwrap();
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && category == DISCUSSIONS_CATEGORY
                && title == build_announcement_title(ISSUE_NUM, TITLE)
                && body == expected_body.as_str()
        })
        .times(1)
        .returning(|_, _, _, _, _, _| Box::pin(future::ready(Ok(()))));
    gh.expect_create_check_run()
        .with(
            eq(INST_ID),
            eq(ORG),
            eq(REPO),
            eq(ISSUE_NUM),
            eq(CheckDetails {
                status: "completed".to_string(),
                conclusion: Some("success".to_string()),
                summary: "The vote passed! 1 out of 1 voted in favor.".to_string(),
            }),
        )
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
    gh.expect_remove_label()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM), eq(VOTE_OPEN_LABEL))
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
    gh.expect_add_labels()
        .withf(|inst_id, owner, repo, issue_number, labels| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && labels == vec![VOTE_CLOSED_LABEL, VOTE_PASSED_LABEL]
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));

    // Run and check the PR vote is closed
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    votes_closer.close_finished_vote().await.unwrap();
}

#[tokio::test]
async fn close_finished_vote_on_pr_without_results() {
    // Setup database expectations (vote comment was deleted)
    let mut db = MockDB::new();
    db.expect_close_finished_vote().times(1).returning(|_| {
        let mut vote = setup_test_vote();
        vote.is_pull_request = true;
        Box::pin(future::ready(Ok(Some((vote, None)))))
    });

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_post_comment().never();
    gh.expect_create_discussion().never();
    gh.expect_create_check_run()
        .with(
            eq(INST_ID),
            eq(ORG),
            eq(REPO),
            eq(ISSUE_NUM),
            eq(CheckDetails {
                status: "completed".to_string(),
                conclusion: Some("success".to_string()),
                summary: "The vote was cancelled".to_string(),
            }),
        )
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
    gh.expect_remove_label()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM), eq(VOTE_OPEN_LABEL))
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
    gh.expect_add_labels()
        .withf(|inst_id, owner, repo, issue_number, labels| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && labels == vec![VOTE_CLOSED_LABEL]
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));

    // Run and check the vote is closed
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_removing_label_error_is_ignored() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_close_finished_vote()
        .times(1)
        .returning(|_| Box::pin(future::ready(Ok(Some((setup_test_vote(), None))))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_remove_label()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM), eq(VOTE_OPEN_LABEL))
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    gh.expect_add_labels()
        .withf(|inst_id, owner, repo, issue_number, labels| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && labels == vec![VOTE_CLOSED_LABEL]
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));

    // Run and check the vote is closed successfully
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_without_announcements() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_close_finished_vote().times(1).returning(|_| {
        let mut vote = setup_test_vote();
        vote.cfg.announcements = None;
        Box::pin(future::ready(Ok(Some((vote, Some(setup_test_vote_results()))))))
    });

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_post_comment()
        .withf(|inst_id, owner, repo, issue_number, body| {
            let expected_body = tmpl::VoteClosed::new(&setup_test_vote_results()).render().unwrap();
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body == expected_body.as_str()
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
    gh.expect_create_discussion().never();
    gh.expect_remove_label()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM), eq(VOTE_OPEN_LABEL))
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
    gh.expect_add_labels()
        .withf(|inst_id, owner, repo, issue_number, labels| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && labels == vec![VOTE_CLOSED_LABEL, VOTE_PASSED_LABEL]
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));

    // Run and check the vote is closed
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_without_issue_title() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_close_finished_vote().times(1).returning(|_| {
        let mut vote = setup_test_vote();
        vote.issue_title = None;
        Box::pin(future::ready(Ok(Some((vote, Some(setup_test_vote_results()))))))
    });

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_post_comment()
        .withf(|inst_id, owner, repo, issue_number, _| {
            *inst_id == INST_ID && owner == ORG && repo == REPO && *issue_number == ISSUE_NUM
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
    gh.expect_create_discussion()
        .withf(|inst_id, owner, repo, category, title, body| {
            let results = setup_test_vote_results();
            let expected_body = tmpl::VoteClosedAnnouncement::new(ISSUE_NUM, "", &results).render().unwrap();
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && category == DISCUSSIONS_CATEGORY
                && title == build_announcement_title(ISSUE_NUM, "")
                && body == expected_body.as_str()
        })
        .times(1)
        .returning(|_, _, _, _, _, _| Box::pin(future::ready(Ok(()))));
    gh.expect_remove_label()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM), eq(VOTE_OPEN_LABEL))
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
    gh.expect_add_labels()
        .withf(|inst_id, owner, repo, issue_number, labels| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && labels == vec![VOTE_CLOSED_LABEL, VOTE_PASSED_LABEL]
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));

    // Run and check the vote is closed
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn status_checker_enqueues_all_pending_status_checks() {
    // Setup database expectations
    let first_input = CheckVoteInput {
        issue_number: 1,
        repository_full_name: REPOFN.to_string(),
    };
    let second_input = CheckVoteInput {
        issue_number: 2,
        repository_full_name: REPOFN.to_string(),
    };
    let mut db = MockDB::new();
    db.expect_get_pending_status_checks().times(1).returning({
        let inputs = vec![first_input.clone(), second_input.clone()];
        move || Box::pin(future::ready(Ok(inputs.clone())))
    });

    // Run the status checker until it's asked to stop
    let (cmds_tx, cmds_rx) = async_channel::unbounded();
    let cancel_token = CancellationToken::new();
    let status_checker = StatusChecker::new(Arc::new(db), cmds_tx);
    let status_checker_handle = status_checker.run(cancel_token.clone());
    cancel_token.cancel();

    // Check commands were enqueued in order
    assert!(status_checker_handle.await.is_ok());
    assert_eq!(cmds_rx.recv().await.unwrap(), Command::CheckVote(first_input));
    assert_eq!(cmds_rx.recv().await.unwrap(), Command::CheckVote(second_input));
    assert!(cmds_rx.is_empty());
}

#[tokio::test]
async fn status_checker_stops_after_processing_pending_status_check() {
    // Setup input
    let check_vote_input = CheckVoteInput {
        repository_full_name: "repo_full_name".to_string(),
        issue_number: 1,
    };
    let check_vote_input_copy = check_vote_input.clone();

    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_get_pending_status_checks()
        .times(1)
        .returning(move || Box::pin(future::ready(Ok(vec![check_vote_input_copy.clone()]))));

    // Run the status checker until it's asked to stop
    let (cmds_tx, cmds_rx) = async_channel::unbounded();
    let cancel_token = CancellationToken::new();
    let status_checker = StatusChecker::new(Arc::new(db), cmds_tx);
    let status_checker_handle = status_checker.run(cancel_token.clone());
    cancel_token.cancel();

    // Check the command was enqueued
    assert!(status_checker_handle.await.is_ok());
    assert_eq!(
        cmds_rx.recv().await.unwrap(),
        Command::CheckVote(check_vote_input)
    );
}

#[tokio::test]
async fn status_checker_stops_when_requested_error_getting_pending() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_get_pending_status_checks()
        .times(1)
        .returning(|| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Run the status checker until it's asked to stop
    let (cmds_tx, cmds_rx) = async_channel::unbounded();
    let cancel_token = CancellationToken::new();
    let status_checker = StatusChecker::new(Arc::new(db), cmds_tx);
    let status_checker_handle = status_checker.run(cancel_token.clone());
    cancel_token.cancel();

    // Check no commands were enqueued
    assert!(status_checker_handle.await.is_ok());
    assert!(cmds_rx.is_empty());
}

#[tokio::test]
async fn status_checker_stops_when_requested_none_pending() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_get_pending_status_checks()
        .times(1)
        .returning(|| Box::pin(future::ready(Ok(vec![]))));

    // Run the status checker until it's asked to stop
    let (cmds_tx, cmds_rx) = async_channel::unbounded();
    let cancel_token = CancellationToken::new();
    let status_checker = StatusChecker::new(Arc::new(db), cmds_tx);
    let status_checker_handle = status_checker.run(cancel_token.clone());
    cancel_token.cancel();

    // Check no commands were enqueued
    assert!(status_checker_handle.await.is_ok());
    assert!(cmds_rx.is_empty());
}

#[tokio::test]
async fn votes_auto_closer_closes_vote_only_when_passing_over_all_allowed_voters() {
    // Setup cases (exclude abstentions, allowed voters, expected ends at updates).
    // In all cases USER1 votes in favor and the vote passes over the votes cast.
    let cases = [
        (None, vec![USER1, USER2, USER3], 0),
        (None, vec![USER1, USER2], 1),
        (Some(true), vec![USER1, USER2], 1),
        (None, vec![], 0),
    ];

    for (exclude_abstentions, allowed_voters, expected_updates) in cases {
        // Setup vote using the votes cast as the pass threshold base
        let mut vote = setup_test_vote();
        vote.cfg.pass_threshold_base = Some(PassThresholdBase::VotesCast { exclude_abstentions });

        // Setup database expectations
        let mut db = MockDB::new();
        db.expect_get_open_votes_with_close_on_passing()
            .times(1)
            .return_once(move || Box::pin(future::ready(Ok(vec![vote]))));
        db.expect_update_vote_ends_at()
            .with(eq(Uuid::parse_str(VOTE_ID).unwrap()))
            .times(expected_updates)
            .returning(|_| Box::pin(future::ready(Ok(()))));

        // Setup GitHub expectations
        let mut gh = MockGH::new();
        gh.expect_get_comment_reactions()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(COMMENT_ID))
            .times(1)
            .returning(|_, _, _, _| Box::pin(future::ready(Ok(vec![in_favor_reaction(USER1)]))));
        let allowed_voters: Vec<UserName> = allowed_voters.into_iter().map(ToString::to_string).collect();
        gh.expect_get_allowed_voters()
            .withf(|inst_id, _, owner, repo, _| *inst_id == INST_ID && owner == ORG && repo == REPO)
            .times(1)
            .return_once(move |_, _, _, _, _| Box::pin(future::ready(Ok(allowed_voters))));

        // Run the votes auto closer until it's asked to stop
        let cancel_token = CancellationToken::new();
        let votes_auto_closer = VotesAutoCloser::new(Arc::new(db), Arc::new(gh));
        let votes_auto_closer_handle = votes_auto_closer.run(cancel_token.clone());
        cancel_token.cancel();

        // Check the worker completed (expectations are verified on drop)
        assert!(
            votes_auto_closer_handle.await.is_ok(),
            "exclude abstentions: {exclude_abstentions:?}, expected updates: {expected_updates}"
        );
    }
}

#[tokio::test]
async fn votes_auto_closer_continues_after_error_calculating_results() {
    // Setup votes
    let first_vote = setup_test_vote();
    let mut second_vote = setup_test_vote();
    second_vote.vote_id = Uuid::parse_str(VOTE_ID2).unwrap();
    second_vote.vote_comment_id = COMMENT_ID2;

    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_get_open_votes_with_close_on_passing().times(1).returning({
        let votes = vec![first_vote, second_vote];
        move || Box::pin(future::ready(Ok(votes.clone())))
    });
    db.expect_update_vote_ends_at()
        .with(eq(Uuid::parse_str(VOTE_ID2).unwrap()))
        .times(1)
        .returning(|_| Box::pin(future::ready(Ok(()))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_comment_reactions()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(COMMENT_ID))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    expect_vote_results(&mut gh, COMMENT_ID2, Ok(vec![in_favor_reaction(USER1)]));

    // Run the votes auto closer until it's asked to stop
    let cancel_token = CancellationToken::new();
    let votes_auto_closer = VotesAutoCloser::new(Arc::new(db), Arc::new(gh));
    let votes_auto_closer_handle = votes_auto_closer.run(cancel_token.clone());
    cancel_token.cancel();

    // Check the worker completed (expectations are verified on drop)
    assert!(votes_auto_closer_handle.await.is_ok());
}

#[tokio::test]
async fn votes_auto_closer_continues_after_error_updating_ends_at() {
    // Setup votes
    let first_vote = setup_test_vote();
    let mut second_vote = setup_test_vote();
    second_vote.vote_id = Uuid::parse_str(VOTE_ID2).unwrap();
    second_vote.vote_comment_id = COMMENT_ID2;

    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_get_open_votes_with_close_on_passing().times(1).returning({
        let votes = vec![first_vote, second_vote];
        move || Box::pin(future::ready(Ok(votes.clone())))
    });
    db.expect_update_vote_ends_at()
        .with(eq(Uuid::parse_str(VOTE_ID).unwrap()))
        .times(1)
        .returning(|_| Box::pin(future::ready(Err(format_err!(ERROR)))));
    db.expect_update_vote_ends_at()
        .with(eq(Uuid::parse_str(VOTE_ID2).unwrap()))
        .times(1)
        .returning(|_| Box::pin(future::ready(Ok(()))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_comment_reactions()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(COMMENT_ID))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(vec![in_favor_reaction(USER1)]))));
    gh.expect_get_comment_reactions()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(COMMENT_ID2))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(vec![in_favor_reaction(USER1)]))));
    gh.expect_get_allowed_voters()
        .withf(|inst_id, _, owner, repo, _| *inst_id == INST_ID && owner == ORG && repo == REPO)
        .times(2)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(vec![USER1.to_string()]))));

    // Run the votes auto closer until it's asked to stop
    let cancel_token = CancellationToken::new();
    let votes_auto_closer = VotesAutoCloser::new(Arc::new(db), Arc::new(gh));
    let votes_auto_closer_handle = votes_auto_closer.run(cancel_token.clone());
    cancel_token.cancel();

    // Check the worker completed (expectations are verified on drop)
    assert!(votes_auto_closer_handle.await.is_ok());
}

#[tokio::test]
async fn votes_auto_closer_does_not_close_vote_not_passed() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_get_open_votes_with_close_on_passing()
        .times(1)
        .returning(|| Box::pin(future::ready(Ok(vec![setup_test_vote()]))));
    db.expect_update_vote_ends_at().never();

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_vote_results(&mut gh, COMMENT_ID, Ok(vec![]));

    // Run the votes auto closer until it's asked to stop
    let cancel_token = CancellationToken::new();
    let votes_auto_closer = VotesAutoCloser::new(Arc::new(db), Arc::new(gh));
    let votes_auto_closer_handle = votes_auto_closer.run(cancel_token.clone());
    cancel_token.cancel();

    // Check the worker completed (expectations are verified on drop)
    assert!(votes_auto_closer_handle.await.is_ok());
}

#[tokio::test]
async fn votes_auto_closer_stops_after_processing_pending_vote() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_get_open_votes_with_close_on_passing()
        .times(1)
        .returning(move || Box::pin(future::ready(Ok(vec![setup_test_vote()]))));
    db.expect_update_vote_ends_at()
        .times(1)
        .returning(move |_| Box::pin(future::ready(Ok(()))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_comment_reactions()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(COMMENT_ID))
        .times(1)
        .returning(|_, _, _, _| {
            Box::pin(future::ready(Ok(vec![Reaction {
                user: User {
                    login: USER1.to_string(),
                },
                content: REACTION_IN_FAVOR.to_string(),
                created_at: TIMESTAMP.to_string(),
            }])))
        });
    gh.expect_get_allowed_voters()
        .withf(|inst_id, cfg, owner, repo, org| {
            *inst_id == INST_ID
                && *cfg == setup_test_vote().cfg
                && owner == ORG
                && repo == REPO
                && *org == Some(ORG.to_string()).as_ref()
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(vec![USER1.to_string()]))));

    // Run the votes auto closer until it's asked to stop
    let cancel_token = CancellationToken::new();
    let votes_auto_closer = VotesAutoCloser::new(Arc::new(db), Arc::new(gh));
    let votes_auto_closer_handle = votes_auto_closer.run(cancel_token.clone());
    cancel_token.cancel();

    // Check the worker completed
    assert!(votes_auto_closer_handle.await.is_ok());
}

#[tokio::test]
async fn votes_auto_closer_stops_when_requested_error_getting_pending() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_get_open_votes_with_close_on_passing()
        .times(1)
        .returning(|| Box::pin(future::ready(Err(format_err!(ERROR)))));
    let gh = MockGH::new();

    // Run the votes auto closer until it's asked to stop
    let cancel_token = CancellationToken::new();
    let votes_auto_closer = VotesAutoCloser::new(Arc::new(db), Arc::new(gh));
    let votes_auto_closer_handle = votes_auto_closer.run(cancel_token.clone());
    cancel_token.cancel();

    // Check the worker completed
    assert!(votes_auto_closer_handle.await.is_ok());
}

#[tokio::test]
async fn votes_auto_closer_stops_when_requested_none_pending() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_get_open_votes_with_close_on_passing()
        .times(1)
        .returning(|| Box::pin(future::ready(Ok(vec![]))));
    let gh = MockGH::new();

    // Run the votes auto closer until it's asked to stop
    let cancel_token = CancellationToken::new();
    let votes_auto_closer = VotesAutoCloser::new(Arc::new(db), Arc::new(gh));
    let votes_auto_closer_handle = votes_auto_closer.run(cancel_token.clone());
    cancel_token.cancel();

    // Check the worker completed
    assert!(votes_auto_closer_handle.await.is_ok());
}

#[test]
fn build_announcement_title_includes_issue_number() {
    assert_eq!(
        build_announcement_title(ISSUE_NUM, TITLE),
        "Test title #1 (vote closed)"
    );
}

// Helpers.

/// Expect the user to be checked as a repository collaborator once.
fn expect_user_is_collaborator(gh: &mut MockGH, is_collaborator: bool) {
    gh.expect_user_is_collaborator()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
        .times(1)
        .returning(move |_, _, _, _| Box::pin(future::ready(Ok(is_collaborator))));
}

/// Expect the valid test configuration file to be requested once.
fn expect_valid_config(gh: &mut MockGH) {
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Some(get_test_valid_config()))));
}

/// Expect the GitHub calls needed to calculate the results of a vote whose
/// allowed voters only include `USER1`.
fn expect_vote_results(gh: &mut MockGH, comment_id: i64, reactions: Result<Vec<Reaction>>) {
    gh.expect_get_comment_reactions()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(comment_id))
        .times(1)
        .return_once(move |_, _, _, _| Box::pin(future::ready(reactions)));
    gh.expect_get_allowed_voters()
        .withf(|inst_id, cfg, owner, repo, org| {
            *inst_id == INST_ID
                && *cfg == setup_test_vote().cfg
                && owner == ORG
                && repo == REPO
                && *org == Some(ORG.to_string()).as_ref()
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(vec![USER1.to_string()]))));
}

/// Create an in favor reaction from the user provided.
fn in_favor_reaction(user: &str) -> Reaction {
    Reaction {
        user: User {
            login: user.to_string(),
        },
        content: REACTION_IN_FAVOR.to_string(),
        created_at: TIMESTAMP.to_string(),
    }
}

/// Setup a commands handler using the mocks provided.
fn setup_cmds_handler(db: MockDB, gh: MockGH) -> CommandsHandler {
    let (_, cmds_rx) = async_channel::unbounded();
    CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx)
}
