use std::sync::Arc;

use anyhow::format_err;
use mockall::Sequence;
use tokio::{sync::oneshot, time::Instant};

use crate::{db::MockDB, testutil::*};

use super::*;

#[tokio::test(start_paused = true)]
async fn status_checker_enqueues_all_pending_status_checks() {
    // Setup database expectations
    let first_input = CheckVoteInput {
        issue_number: 1,
        repository_full_name: REPOFN.to_string(),

        installation_id: None,
    };
    let second_input = CheckVoteInput {
        issue_number: 2,
        repository_full_name: REPOFN.to_string(),

        installation_id: None,
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

#[tokio::test(start_paused = true)]
async fn status_checker_pauses_and_resumes_after_error() {
    // Setup database expectations
    let (resumed_tx, resumed_rx) = oneshot::channel();
    let mut seq = Sequence::new();
    let mut db = MockDB::new();
    db.expect_get_pending_status_checks()
        .times(1)
        .in_sequence(&mut seq)
        .returning(|| Box::pin(future::ready(Err(format_err!(ERROR)))));
    db.expect_get_pending_status_checks().times(1).in_sequence(&mut seq).return_once(move || {
        resumed_tx.send(Instant::now()).unwrap();
        Box::pin(future::ready(Ok(vec![])))
    });

    // Run the status checker until it resumes
    let started_at = Instant::now();
    let (cmds_tx, _cmds_rx) = async_channel::unbounded();
    let cancel_token = CancellationToken::new();
    let status_checker = StatusChecker::new(Arc::new(db), cmds_tx);
    let status_checker_handle = status_checker.run(cancel_token.clone());
    let resumed_at = resumed_rx.await.unwrap();
    cancel_token.cancel();

    // Check the worker waited until the next run
    assert!(status_checker_handle.await.is_ok());
    assert_eq!(resumed_at - started_at, Duration::from_mins(30));
}

#[tokio::test(start_paused = true)]
async fn status_checker_stops_after_processing_pending_status_check() {
    // Setup input
    let check_vote_input = CheckVoteInput {
        issue_number: 1,
        repository_full_name: "repo_full_name".to_string(),

        installation_id: None,
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

#[tokio::test(start_paused = true)]
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

#[tokio::test(start_paused = true)]
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
