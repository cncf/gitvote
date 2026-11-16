use std::sync::Arc;

use anyhow::format_err;
use mockall::{Sequence, predicate::eq};
use tokio::{sync::oneshot, time::Instant};
use uuid::Uuid;

use crate::{cfg_repo::PassThresholdBase, db::MockDB, github::*, results::REACTION_IN_FAVOR, testutil::*};

use super::*;

#[tokio::test(start_paused = true)]
async fn votes_auto_closer_closes_vote_only_when_passing_over_all_allowed_voters() {
    // Setup cases (exclude abstentions, allowed voters, expected ends at updates). In
    // all cases USER1 votes in favor and the vote passes over the votes cast
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
        let expected_cfg = vote.cfg.clone();

        // Setup database expectations
        let mut db = MockDB::new();
        db.expect_get_open_votes_with_close_on_passing()
            .times(1)
            .return_once(move || Box::pin(future::ready(Ok(vec![vote]))));
        db.expect_update_vote_ends_at()
            .with(eq(test_vote_id()))
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
            .withf(move |inst_id, cfg, owner, repo, org| {
                *inst_id == INST_ID
                    && *cfg == expected_cfg
                    && owner == ORG
                    && repo == REPO
                    && *org == Some(ORG.to_string()).as_ref()
            })
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

#[tokio::test(start_paused = true)]
async fn votes_auto_closer_continues_after_error_calculating_results() {
    // Setup votes
    let first_vote = setup_test_vote();
    let mut second_vote = setup_test_vote();
    second_vote.vote_comment_id = COMMENT_ID2;
    second_vote.vote_id = Uuid::parse_str(VOTE_ID2).unwrap();

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

#[tokio::test(start_paused = true)]
async fn votes_auto_closer_continues_after_error_updating_ends_at() {
    // Setup votes
    let first_vote = setup_test_vote();
    let mut second_vote = setup_test_vote();
    second_vote.vote_comment_id = COMMENT_ID2;
    second_vote.vote_id = Uuid::parse_str(VOTE_ID2).unwrap();

    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_get_open_votes_with_close_on_passing().times(1).returning({
        let votes = vec![first_vote, second_vote];
        move || Box::pin(future::ready(Ok(votes.clone())))
    });
    db.expect_update_vote_ends_at()
        .with(eq(test_vote_id()))
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
        .withf(|inst_id, cfg, owner, repo, org| {
            *inst_id == INST_ID
                && *cfg == setup_test_vote().cfg
                && owner == ORG
                && repo == REPO
                && *org == Some(ORG.to_string()).as_ref()
        })
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

#[tokio::test(start_paused = true)]
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

#[tokio::test(start_paused = true)]
async fn votes_auto_closer_pauses_and_resumes_after_error() {
    // Setup database expectations
    let (resumed_tx, resumed_rx) = oneshot::channel();
    let mut seq = Sequence::new();
    let mut db = MockDB::new();
    db.expect_get_open_votes_with_close_on_passing()
        .times(1)
        .in_sequence(&mut seq)
        .returning(|| Box::pin(future::ready(Err(format_err!(ERROR)))));
    db.expect_get_open_votes_with_close_on_passing()
        .times(1)
        .in_sequence(&mut seq)
        .return_once(move || {
            resumed_tx.send(Instant::now()).unwrap();
            Box::pin(future::ready(Ok(vec![])))
        });

    // Run the votes auto closer until it resumes
    let started_at = Instant::now();
    let cancel_token = CancellationToken::new();
    let votes_auto_closer = VotesAutoCloser::new(Arc::new(db), Arc::new(MockGH::new()));
    let votes_auto_closer_handle = votes_auto_closer.run(cancel_token.clone());
    let resumed_at = resumed_rx.await.unwrap();
    cancel_token.cancel();

    // Check the worker waited until the next run
    assert!(votes_auto_closer_handle.await.is_ok());
    assert_eq!(resumed_at - started_at, Duration::from_hours(24));
}

#[tokio::test(start_paused = true)]
async fn votes_auto_closer_stops_after_processing_pending_vote() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_get_open_votes_with_close_on_passing()
        .times(1)
        .returning(move || Box::pin(future::ready(Ok(vec![setup_test_vote()]))));
    db.expect_update_vote_ends_at()
        .with(eq(test_vote_id()))
        .times(1)
        .returning(move |_| Box::pin(future::ready(Ok(()))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_comment_reactions()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(COMMENT_ID))
        .times(1)
        .returning(|_, _, _, _| {
            Box::pin(future::ready(Ok(vec![Reaction {
                content: REACTION_IN_FAVOR.to_string(),
                created_at: TIMESTAMP.to_string(),
                user: User {
                    login: USER1.to_string(),
                },
            }])))
        });
    expect_allowed_voters(&mut gh);

    // Run the votes auto closer until it's asked to stop
    let cancel_token = CancellationToken::new();
    let votes_auto_closer = VotesAutoCloser::new(Arc::new(db), Arc::new(gh));
    let votes_auto_closer_handle = votes_auto_closer.run(cancel_token.clone());
    cancel_token.cancel();

    // Check the worker completed
    assert!(votes_auto_closer_handle.await.is_ok());
}

#[tokio::test(start_paused = true)]
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

#[tokio::test(start_paused = true)]
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
