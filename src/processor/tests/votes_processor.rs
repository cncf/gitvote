use std::sync::Arc;

use mockall::predicate::eq;
use tokio::{sync::oneshot, time::timeout};

use crate::{db::MockDB, github::*, testutil::*};

use super::*;

#[tokio::test(start_paused = true)]
async fn votes_processor_processes_commands_sent_while_running() {
    // Setup database expectations
    let (started_tx, started_rx) = oneshot::channel();
    let (processed_tx, processed_rx) = oneshot::channel();
    let mut db = MockDB::new();
    expect_periodic_workers_run(&mut db);
    db.expect_get_pending_status_checks().times(1).return_once(move || {
        started_tx.send(()).unwrap();
        Box::pin(future::ready(Ok(vec![])))
    });
    db.expect_get_open_vote()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .return_once(move |_, _| {
            processed_tx.send(()).unwrap();
            Box::pin(future::ready(Ok(None)))
        });

    // Run the processor until all its workers have started
    let (cmds_tx, cmds_rx) = async_channel::unbounded();
    let cancel_token = CancellationToken::new();
    let votes_processor = Processor::new(Arc::new(db), Arc::new(MockGH::new()), cmds_tx.clone(), cmds_rx);
    let votes_processor_handle = votes_processor.run(&cancel_token);
    started_rx.await.unwrap();

    // Send a command, as the webhook handler does
    let event = Event::Issue(setup_test_issue_event());
    cmds_tx.send(Command::CheckVote(CheckVoteInput::new(&event))).await.unwrap();

    // Check an idle commands handler processes it
    timeout(WORKER_TIMEOUT, processed_rx).await.unwrap().unwrap();
    cancel_token.cancel();
    assert!(votes_processor_handle.await.iter().all(Result::is_ok));
}

#[tokio::test(start_paused = true)]
async fn votes_processor_processes_pending_status_checks() {
    // Setup database expectations
    let (processed_tx, processed_rx) = oneshot::channel();
    let mut db = MockDB::new();
    expect_periodic_workers_run(&mut db);
    db.expect_get_pending_status_checks().times(1).returning(|| {
        let input = CheckVoteInput {
            issue_number: ISSUE_NUM,
            repository_full_name: REPOFN.to_string(),

            installation_id: None,
        };
        Box::pin(future::ready(Ok(vec![input])))
    });
    db.expect_get_open_vote()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .return_once(move |_, _| {
            processed_tx.send(()).unwrap();
            Box::pin(future::ready(Ok(None)))
        });

    // Run the processor
    let (cmds_tx, cmds_rx) = async_channel::unbounded();
    let cancel_token = CancellationToken::new();
    let votes_processor = Processor::new(Arc::new(db), Arc::new(MockGH::new()), cmds_tx, cmds_rx);
    let votes_processor_handle = votes_processor.run(&cancel_token);

    // Check the status check reaches the commands handlers
    timeout(WORKER_TIMEOUT, processed_rx).await.unwrap().unwrap();
    cancel_token.cancel();
    assert!(votes_processor_handle.await.iter().all(Result::is_ok));
}

#[tokio::test(start_paused = true)]
async fn votes_processor_stops_when_requested() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_claim_vote_to_close().times(1).returning(|| Box::pin(future::ready(Ok(None))));
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

// Helpers.

/// Expects the votes closer and the votes auto closer to run once with nothing to do.
fn expect_periodic_workers_run(db: &mut MockDB) {
    db.expect_claim_vote_to_close().times(1).returning(|| Box::pin(future::ready(Ok(None))));
    db.expect_get_open_votes_with_close_on_passing()
        .times(1)
        .returning(|| Box::pin(future::ready(Ok(vec![]))));
}
