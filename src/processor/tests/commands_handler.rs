use std::sync::Arc;

use anyhow::format_err;
use mockall::predicate::eq;
use tokio::sync::oneshot;

use crate::{db::MockDB, github::*, testutil::*};

use super::*;

#[tokio::test]
async fn commands_handler_continues_after_failed_cmd() {
    // Setup the failing commands and the expectations that make them fail
    type ExpectFailure = fn(&mut MockDB, &mut MockGH);
    let event = Event::Issue(setup_test_issue_event());
    let cases: [(Command, ExpectFailure); 3] = [
        (Command::CancelVote(CancelVoteInput::new(&event)), |db, gh| {
            expect_user_is_collaborator(gh, true);
            db.expect_cancel_vote()
                .withf(|repository_full_name, issue_number, cancelled_by| {
                    repository_full_name == REPOFN && *issue_number == ISSUE_NUM && cancelled_by == USER
                })
                .times(1)
                .returning(|_, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
        }),
        (Command::CheckVote(CheckVoteInput::new(&event)), |db, _| {
            db.expect_get_open_vote()
                .with(eq(REPOFN), eq(ISSUE_NUM))
                .times(1)
                .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
        }),
        (
            Command::CreateVote(CreateVoteInput::new(None, &event)),
            |_, gh| {
                gh.expect_get_config_file()
                    .with(eq(INST_ID), eq(ORG), eq(REPO))
                    .times(1)
                    .returning(|_, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
            },
        ),
    ];

    for (failed_cmd, expect_failure) in cases {
        // Setup expectations: reporting the failure is best effort
        let mut db = MockDB::new();
        let mut gh = MockGH::new();
        expect_failure(&mut db, &mut gh);
        expect_command_failed_comment(&mut gh, || Err(format_err!(ERROR)));

        // Setup the expectations of the next command
        db.expect_get_open_vote()
            .with(eq(REPOFN), eq(ISSUE_NUM))
            .times(1)
            .returning(|_, _| Box::pin(future::ready(Ok(None))));

        // Queue commands
        let (cmds_tx, cmds_rx) = async_channel::unbounded();
        cmds_tx.send(failed_cmd).await.unwrap();
        cmds_tx.send(Command::CheckVote(CheckVoteInput::new(&event))).await.unwrap();

        // Run the commands handler until it's asked to stop
        let cancel_token = CancellationToken::new();
        let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx.clone());
        let cmds_handler_handle = cmds_handler.run(cancel_token.clone());
        cancel_token.cancel();

        // Check the command after the failed one was processed
        assert!(cmds_handler_handle.await.is_ok());
        assert!(cmds_rx.is_empty());
    }
}

#[tokio::test]
async fn commands_handler_finishes_cmd_in_progress_when_requested() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_has_vote_open()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(false))));
    expect_store_vote(&mut db, false, || Ok(test_vote_id()));

    // Setup GitHub expectations: the vote comment is held until released
    let (posting_tx, posting_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel::<()>();
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
        .return_once(move |_, _, _, _, _| {
            posting_tx.send(()).unwrap();
            Box::pin(async move {
                release_rx.await.unwrap();
                Ok(COMMENT_ID)
            })
        });
    expect_vote_open_labels(&mut gh);

    // Run the commands handler until the command is in progress
    let (cmds_tx, cmds_rx) = async_channel::unbounded();
    let event = Event::Issue(setup_test_issue_event());
    cmds_tx.send(Command::CreateVote(CreateVoteInput::new(None, &event))).await.unwrap();
    let cancel_token = CancellationToken::new();
    let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
    let cmds_handler_handle = cmds_handler.run(cancel_token.clone());
    posting_rx.await.unwrap();

    // Ask it to stop, letting the worker notice before the command can complete
    cancel_token.cancel();
    tokio::task::yield_now().await;
    release_tx.send(()).unwrap();

    // Check the command completed (expectations are verified on drop)
    assert!(cmds_handler_handle.await.is_ok());
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
        .withf(|repository_full_name, issue_number, cancelled_by| {
            repository_full_name == REPOFN && *issue_number == ISSUE_NUM && cancelled_by == USER
        })
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_user_is_collaborator()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(true))));
    expect_command_failed_comment(&mut gh, || Ok(COMMENT_ID));

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
