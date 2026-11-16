use std::sync::Arc;

use anyhow::format_err;
use mockall::{Sequence, predicate::eq};
use tokio::{sync::oneshot, time::Instant};

use crate::{db::MockDB, github::*, results::Vote, testutil::*};

use super::*;

#[tokio::test]
async fn votes_closer_closes_pending_votes_until_none_left() {
    // Setup database expectations
    let (none_left_tx, none_left_rx) = oneshot::channel();
    let mut seq = Sequence::new();
    let mut db = MockDB::new();
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], true);
    db.expect_claim_vote_to_close()
        .times(1)
        .in_sequence(&mut seq)
        .returning(|| Box::pin(future::ready(Ok(Some(setup_test_closed_vote(None))))));
    expect_close_steps(&mut db, &[VoteCloseStep::Labels]);
    expect_complete_vote_close(&mut db);
    db.expect_claim_vote_to_close().times(1).in_sequence(&mut seq).return_once(move || {
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

#[tokio::test(start_paused = true)]
async fn votes_closer_continues_without_pause_after_failing_to_close_vote() {
    // Setup database expectations
    let (next_vote_tx, next_vote_rx) = oneshot::channel();
    let mut seq = Sequence::new();
    let mut db = MockDB::new();
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], true);
    db.expect_claim_vote_to_close().times(1).in_sequence(&mut seq).returning(|| {
        let vote = Vote {
            close_next_attempt_at: Some(test_claimed_until()),
            ..setup_test_closed_vote(None)
        };
        Box::pin(future::ready(Ok(Some(vote))))
    });
    expect_close_failure(&mut db, VoteCloseFailureOutcome::RetryScheduled);
    db.expect_claim_vote_to_close().times(1).in_sequence(&mut seq).return_once(move || {
        next_vote_tx.send(Instant::now()).unwrap();
        Box::pin(future::ready(Ok(None)))
    });

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_remove_open_label(&mut gh, || Ok(()));
    expect_close_labels(&mut gh, &mut Sequence::new(), &[VOTE_CLOSED_LABEL], || {
        Err(format_err!(ERROR))
    });

    // Run the votes closer until it asks for the next vote
    let started_at = Instant::now();
    let cancel_token = CancellationToken::new();
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    let votes_closer_handle = votes_closer.run(cancel_token.clone());
    let next_vote_at = next_vote_rx.await.unwrap();
    cancel_token.cancel();

    // Check the failing vote did not hold up the next one
    assert!(votes_closer_handle.await.is_ok());
    assert_eq!(next_vote_at - started_at, Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn votes_closer_pauses_and_resumes_after_error() {
    // Setup database expectations
    let (resumed_tx, resumed_rx) = oneshot::channel();
    let mut seq = Sequence::new();
    let mut db = MockDB::new();
    db.expect_claim_vote_to_close()
        .times(1)
        .in_sequence(&mut seq)
        .returning(|| Box::pin(future::ready(Err(format_err!(ERROR)))));
    db.expect_claim_vote_to_close().times(1).in_sequence(&mut seq).return_once(move || {
        resumed_tx.send(Instant::now()).unwrap();
        Box::pin(future::ready(Ok(None)))
    });

    // Run the votes closer until it resumes
    let started_at = Instant::now();
    let cancel_token = CancellationToken::new();
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(MockGH::new()));
    let votes_closer_handle = votes_closer.run(cancel_token.clone());
    let resumed_at = resumed_rx.await.unwrap();
    cancel_token.cancel();

    // Check the worker waited for the error pause
    assert!(votes_closer_handle.await.is_ok());
    assert_eq!(resumed_at - started_at, Duration::from_secs(30));
}

#[tokio::test(start_paused = true)]
async fn votes_closer_pauses_and_resumes_when_none_pending() {
    // Setup database expectations
    let (resumed_tx, resumed_rx) = oneshot::channel();
    let mut seq = Sequence::new();
    let mut db = MockDB::new();
    db.expect_claim_vote_to_close()
        .times(1)
        .in_sequence(&mut seq)
        .returning(|| Box::pin(future::ready(Ok(None))));
    db.expect_claim_vote_to_close().times(1).in_sequence(&mut seq).return_once(move || {
        resumed_tx.send(Instant::now()).unwrap();
        Box::pin(future::ready(Ok(None)))
    });

    // Run the votes closer until it resumes
    let started_at = Instant::now();
    let cancel_token = CancellationToken::new();
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(MockGH::new()));
    let votes_closer_handle = votes_closer.run(cancel_token.clone());
    let resumed_at = resumed_rx.await.unwrap();
    cancel_token.cancel();

    // Check the worker waited for the pause used when there is nothing to do
    assert!(votes_closer_handle.await.is_ok());
    assert_eq!(resumed_at - started_at, Duration::from_secs(15));
}

#[tokio::test(start_paused = true)]
async fn votes_closer_stops_during_pause_when_requested() {
    // Setup the claim results that make the worker pause
    let cases: [fn() -> Result<Option<Vote>>; 2] = [|| Ok(None), || Err(format_err!(ERROR))];

    for claim_result in cases {
        // Setup database expectations
        let (claimed_tx, claimed_rx) = oneshot::channel();
        let mut db = MockDB::new();
        db.expect_claim_vote_to_close().times(1).return_once(move || {
            claimed_tx.send(()).unwrap();
            Box::pin(future::ready(claim_result()))
        });

        // Run the votes closer until it pauses
        let started_at = Instant::now();
        let cancel_token = CancellationToken::new();
        let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(MockGH::new()));
        let votes_closer_handle = votes_closer.run(cancel_token.clone());
        claimed_rx.await.unwrap();
        tokio::task::yield_now().await;

        // Ask it to stop
        cancel_token.cancel();

        // Check the worker stopped without waiting for the pause to end
        assert!(votes_closer_handle.await.is_ok());
        assert_eq!(started_at.elapsed(), Duration::ZERO);
    }
}

#[tokio::test]
async fn votes_closer_stops_when_requested_error_closing() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_claim_vote_to_close()
        .times(1)
        .returning(|| Box::pin(future::ready(Err(format_err!(ERROR)))));
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
    db.expect_claim_vote_to_close().times(1).returning(|| Box::pin(future::ready(Ok(None))));
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
async fn votes_closer_stops_when_requested_with_votes_pending() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_claim_vote(&mut db, setup_test_closed_vote(None));
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], true);
    expect_close_steps(&mut db, &[VoteCloseStep::Labels]);
    expect_complete_vote_close(&mut db);

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_remove_open_label(&mut gh, || Ok(()));
    expect_close_labels(&mut gh, &mut Sequence::new(), &[VOTE_CLOSED_LABEL], || Ok(()));

    // Run the votes closer, asking it to stop before it starts
    let cancel_token = CancellationToken::new();
    cancel_token.cancel();
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));

    // Check the worker completed without asking for another vote
    assert!(votes_closer.run(cancel_token).await.is_ok());
}
