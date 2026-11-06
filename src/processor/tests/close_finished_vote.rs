use std::{ops::RangeInclusive, sync::Arc};

use anyhow::format_err;
use mockall::{Sequence, predicate::eq};
use time::{ext::NumericalDuration, format_description::well_known::Rfc3339};
use tokio::time::Instant;
use uuid::Uuid;

use crate::{db::MockDB, github::*, results::REACTION_AGAINST, testutil::*};

use super::*;

#[tokio::test]
async fn close_finished_vote_allowed_voters_not_found_does_not_close_vote() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_claim_vote(&mut db, setup_test_vote());
    db.expect_close_vote().never();
    expect_close_failure(&mut db, VoteCloseFailureOutcome::RetryScheduled);

    // Setup GitHub expectations
    let not_found_error = setup_test_not_found_error().await;
    let mut gh = MockGH::new();
    gh.expect_get_comment_reactions()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(COMMENT_ID))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(vec![]))));
    gh.expect_get_allowed_voters()
        .withf(|inst_id, cfg, owner, repo, org| {
            *inst_id == INST_ID
                && *cfg == setup_test_vote().cfg
                && owner == ORG
                && repo == REPO
                && *org == Some(ORG.to_string()).as_ref()
        })
        .times(1)
        .return_once(move |_, _, _, _, _| Box::pin(future::ready(Err(not_found_error))));
    gh.expect_post_comment().never();

    // Run and check the failure is recorded, as only the comment can be missing
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_announcement_rejected_is_not_retried() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_claim_vote(&mut db, setup_test_closed_vote(Some(setup_test_vote_results())));
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], true);
    expect_close_steps(
        &mut db,
        &[
            VoteCloseStep::Announcement,
            VoteCloseStep::Comment,
            VoteCloseStep::Labels,
        ],
    );
    expect_complete_vote_close(&mut db);
    db.expect_record_vote_close_failure().never();

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_comment_lookup(
        &mut gh,
        tmpl::VoteMarker::ResultsComment,
        test_vote_id(),
        test_lookup_since(),
        || Ok(false),
    );
    expect_announcement_lookup(&mut gh, test_vote_id(), test_lookup_since(), || Ok(false));
    expect_comment(&mut gh, vote_closed_body(test_vote_id()));
    expect_announcement_discussion(&mut gh, &mut Sequence::new(), test_vote_id(), || {
        Err(format_err!(DiscussionRejectedError(format_err!(ERROR))))
    });
    expect_remove_open_label(&mut gh, || Ok(()));
    expect_close_labels(
        &mut gh,
        &mut Sequence::new(),
        &[VOTE_CLOSED_LABEL, VOTE_PASSED_LABEL],
        || Ok(()),
    );

    // Run and check the vote is closed successfully
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test(start_paused = true)]
async fn close_finished_vote_attempt_timeout_records_failure() {
    // Setup database expectations: the vote is not closed, and the attempt is recorded
    // as failed once it times out
    let mut db = MockDB::new();
    expect_claim_vote(&mut db, setup_test_vote());
    db.expect_close_vote().never();
    db.expect_complete_vote_close().never();
    expect_close_failure(&mut db, VoteCloseFailureOutcome::RetryScheduled);

    // Setup GitHub expectations: getting the reactions never completes
    let mut gh = MockGH::new();
    gh.expect_get_comment_reactions()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(COMMENT_ID))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::pending()));
    gh.expect_post_comment().never();

    // Run the votes closer
    let started_at = Instant::now();
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));

    // Check the attempt was stopped once it timed out
    assert_eq!(started_at.elapsed(), CLOSE_ATTEMPT_TIMEOUT);
}

#[tokio::test]
async fn close_finished_vote_calculates_and_stores_results() {
    // Setup database expectations
    let mut seq = Sequence::new();
    let mut db = MockDB::new();
    expect_claim_vote(&mut db, setup_test_vote());
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], true);
    db.expect_close_vote()
        .withf(|vote_id, results, head_sha| {
            *vote_id == test_vote_id() && results == &Some(&setup_test_vote_results()) && head_sha.is_none()
        })
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _| Box::pin(future::ready(Ok(true))));
    expect_close_steps(
        &mut db,
        &[
            VoteCloseStep::Comment,
            VoteCloseStep::Announcement,
            VoteCloseStep::Labels,
        ],
    );
    db.expect_complete_vote_close()
        .with(eq(test_vote_id()))
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_| Box::pin(future::ready(Ok(()))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_no_marker_lookups(&mut gh);
    expect_vote_results(&mut gh, COMMENT_ID, Ok(vec![in_favor_reaction(USER1)]));
    gh.expect_get_pr_head_sha().never();
    gh.expect_post_comment()
        .withf(|inst_id, owner, repo, issue_number, body| {
            let expected_body = vote_closed_body(test_vote_id());
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body == expected_body.as_str()
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
    expect_announcement_discussion(&mut gh, &mut Sequence::new(), test_vote_id(), || Ok(()));
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
async fn close_finished_vote_cancellation_abandoned_is_not_an_error() {
    // Setup database expectations: the failure was the last attempt, so the
    // cancellation was abandoned when recording it
    let mut db = MockDB::new();
    expect_claim_vote(&mut db, setup_test_cancelled_vote(false));
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], true);
    expect_close_steps(&mut db, &[VoteCloseStep::Labels]);
    db.expect_complete_vote_cancellation().never();
    expect_close_failure(&mut db, VoteCloseFailureOutcome::CancellationAbandoned);

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_comment_lookup(
        &mut gh,
        tmpl::VoteMarker::CancellationComment,
        test_vote_id(),
        test_lookup_since(),
        || Ok(false),
    );
    expect_vote_cancelled_comment_fails(&mut gh, test_vote_id(), false);
    expect_remove_open_label(&mut gh, || Ok(()));

    // Run and check the votes closer can continue with the next vote
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_cancelled_vote_error_recording_skipped_step_records_failure() {
    // Setup database expectations: the comment was posted by a previous attempt, and
    // the labels step was superseded by a newer vote, failing to record it as done
    let mut db = MockDB::new();
    let mut vote = setup_test_cancelled_vote(false);
    vote.close_comment_posted_at = Some(test_closed_at());
    expect_claim_vote(&mut db, vote);
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], false);
    db.expect_record_vote_close_step()
        .with(eq(test_vote_id()), eq(VoteCloseStep::Labels))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    db.expect_complete_vote_cancellation().never();
    expect_close_failure(&mut db, VoteCloseFailureOutcome::RetryScheduled);

    // Setup GitHub expectations: nothing is published
    let mut gh = MockGH::new();
    expect_no_marker_lookups(&mut gh);
    gh.expect_post_comment().never();
    gh.expect_remove_label().never();

    // Run and check the failure is recorded, leaving the step for the retry
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_cancelled_vote_error_records_failure() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_claim_vote(&mut db, setup_test_cancelled_vote(false));
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], true);
    expect_close_steps(&mut db, &[VoteCloseStep::Labels]);
    db.expect_complete_vote_cancellation().never();
    expect_close_failure(&mut db, VoteCloseFailureOutcome::RetryScheduled);

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_comment_lookup(
        &mut gh,
        tmpl::VoteMarker::CancellationComment,
        test_vote_id(),
        test_lookup_since(),
        || Ok(false),
    );
    expect_vote_cancelled_comment_fails(&mut gh, test_vote_id(), false);
    expect_remove_open_label(&mut gh, || Ok(()));

    // Run and check the failure is recorded
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_cancelled_vote_publishes_missing_cancellation_steps() {
    // Setup database expectations: the comment was posted by a previous attempt
    let mut db = MockDB::new();
    let mut vote = setup_test_cancelled_vote(true);
    vote.close_attempts = 1;
    vote.close_comment_posted_at = Some(test_closed_at());
    expect_claim_vote(&mut db, vote);
    expect_state_steps_pending(&mut db, &[VoteCloseStep::CheckRun, VoteCloseStep::Labels], true);
    db.expect_close_vote().never();
    expect_close_steps(&mut db, &[VoteCloseStep::CheckRun, VoteCloseStep::Labels]);
    expect_complete_vote_cancellation(&mut db);
    db.expect_complete_vote_close().never();

    // Setup GitHub expectations: no results are calculated nor published
    let mut gh = MockGH::new();
    gh.expect_get_comment_reactions().never();
    gh.expect_post_comment().never();
    gh.expect_create_discussion().never();
    gh.expect_get_pr_head_sha().never();
    gh.expect_create_commit_check_run().never();
    expect_check_run(&mut gh, cancelled_check_details());
    expect_remove_open_label(&mut gh, || Ok(()));
    gh.expect_add_labels().never();

    // Run and check the cancellation is published
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_cancelled_vote_retry_lookup_error_records_failure() {
    // Setup database expectations: the comment is not recorded, as it is unknown whether
    // the previous attempt posted it
    let mut db = MockDB::new();
    expect_claim_vote(&mut db, setup_test_cancelled_vote(false));
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], true);
    expect_close_steps(&mut db, &[VoteCloseStep::Labels]);
    db.expect_complete_vote_cancellation().never();
    expect_close_failure(&mut db, VoteCloseFailureOutcome::RetryScheduled);

    // Setup GitHub expectations: the comment is not posted blindly
    let mut gh = MockGH::new();
    expect_comment_lookup(
        &mut gh,
        tmpl::VoteMarker::CancellationComment,
        test_vote_id(),
        test_lookup_since(),
        || Err(format_err!(ERROR)),
    );
    gh.expect_post_comment().never();
    expect_remove_open_label(&mut gh, || Ok(()));

    // Run and check the failure is recorded
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_cancelled_vote_retry_records_comment_found_published() {
    // Setup database expectations: the comment posted by the previous attempt is
    // recorded as done
    let mut db = MockDB::new();
    expect_claim_vote(&mut db, setup_test_cancelled_vote(false));
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], true);
    expect_close_steps(&mut db, &[VoteCloseStep::Comment, VoteCloseStep::Labels]);
    expect_complete_vote_cancellation(&mut db);
    db.expect_record_vote_close_failure().never();

    // Setup GitHub expectations: the comment is found, so it is not posted again
    let mut gh = MockGH::new();
    expect_comment_lookup(
        &mut gh,
        tmpl::VoteMarker::CancellationComment,
        test_vote_id(),
        test_lookup_since(),
        || Ok(true),
    );
    gh.expect_post_comment().never();
    expect_remove_open_label(&mut gh, || Ok(()));

    // Run and check the cancellation is published
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_claim_taken_over_is_not_an_error() {
    // Setup database expectations: the claim was taken over when recording the failure
    let mut db = MockDB::new();
    expect_claim_vote(&mut db, setup_test_closed_vote(None));
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], true);
    expect_close_failure(&mut db, VoteCloseFailureOutcome::ClaimLost);
    db.expect_complete_vote_close().never();

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_remove_open_label(&mut gh, || Ok(()));
    gh.expect_add_labels()
        .withf(|inst_id, owner, repo, issue_number, labels| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && labels == vec![VOTE_CLOSED_LABEL]
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Run and check the votes closer can continue with the next vote
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_claimed_vote_without_claim_returns_error() {
    // Setup database expectations: the vote claimed has no claim, so the failure cannot
    // be recorded
    let mut db = MockDB::new();
    db.expect_claim_vote_to_close()
        .times(1)
        .return_once(|| Box::pin(future::ready(Ok(Some(setup_test_closed_vote(None))))));
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], true);
    db.expect_record_vote_close_step().never();
    db.expect_complete_vote_close().never();
    db.expect_record_vote_close_failure().never();

    // Setup GitHub expectations: updating the labels fails
    let mut gh = MockGH::new();
    expect_remove_open_label(&mut gh, || Ok(()));
    expect_close_labels(&mut gh, &mut Sequence::new(), &[VOTE_CLOSED_LABEL], || {
        Err(format_err!(ERROR))
    });

    // Run and check the error is returned, so that the votes closer pauses
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    let err = votes_closer.close_finished_vote().await.unwrap_err();
    assert_eq!(err.to_string(), "claimed vote without claim");
}

#[tokio::test]
async fn close_finished_vote_comment_deleted_closes_vote_without_results() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_claim_vote(&mut db, setup_test_vote());
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], true);
    db.expect_close_vote()
        .withf(|vote_id, results, head_sha| {
            *vote_id == test_vote_id() && results.is_none() && head_sha.is_none()
        })
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(true))));
    expect_close_steps(&mut db, &[VoteCloseStep::Labels]);
    expect_complete_vote_close(&mut db);

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_comment_reactions()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(COMMENT_ID))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Err(CommentNotFoundError.into()))));
    gh.expect_get_allowed_voters().never();
    gh.expect_post_comment().never();
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
                && labels == vec![VOTE_CLOSED_LABEL]
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));

    // Run and check the vote is closed
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_error_adding_labels() {
    // Setup database expectations
    let mut db = MockDB::new();
    let mut vote = setup_test_closed_vote(Some(setup_test_vote_results()));
    vote.cfg.announcements = None;
    expect_claim_vote(&mut db, vote);
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], true);
    expect_close_steps(&mut db, &[VoteCloseStep::Comment]);
    expect_close_failure(&mut db, VoteCloseFailureOutcome::RetryScheduled);
    db.expect_complete_vote_close().never();

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_comment_lookup(
        &mut gh,
        tmpl::VoteMarker::ResultsComment,
        test_vote_id(),
        test_lookup_since(),
        || Ok(false),
    );
    expect_comment(&mut gh, vote_closed_body(test_vote_id()));
    expect_remove_open_label(&mut gh, || Ok(()));
    expect_close_labels(
        &mut gh,
        &mut Sequence::new(),
        &[VOTE_CLOSED_LABEL, VOTE_PASSED_LABEL],
        || Err(format_err!(ERROR)),
    );

    // Run and check the failure is recorded and does not stop the votes closer
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_error_calculating_results_does_not_close_vote() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_claim_vote(&mut db, setup_test_vote());
    db.expect_close_vote().never();
    expect_close_failure(&mut db, VoteCloseFailureOutcome::RetryScheduled);
    db.expect_complete_vote_close().never();

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_comment_reactions()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(COMMENT_ID))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    gh.expect_post_comment().never();

    // Run and check the failure is recorded and does not stop the votes closer
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_error_checking_state_step_runs_next_step() {
    // Setup database expectations: only the check of the first step fails
    let mut db = MockDB::new();
    let mut seq = Sequence::new();
    let mut vote = setup_test_closed_vote(Some(setup_test_vote_results()));
    vote.cfg.announcements = None;
    vote.is_pull_request = true;
    expect_claim_vote(&mut db, vote);
    db.expect_is_close_state_step_pending()
        .with(eq(test_vote_id()), eq(VoteCloseStep::CheckRun))
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    db.expect_is_close_state_step_pending()
        .with(eq(test_vote_id()), eq(VoteCloseStep::Labels))
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _| Box::pin(future::ready(Ok(true))));
    expect_close_steps(&mut db, &[VoteCloseStep::Comment, VoteCloseStep::Labels]);
    expect_close_failure(&mut db, VoteCloseFailureOutcome::RetryScheduled);
    db.expect_complete_vote_close().never();

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_comment_lookup(
        &mut gh,
        tmpl::VoteMarker::ResultsComment,
        test_vote_id(),
        test_lookup_since(),
        || Ok(false),
    );
    expect_comment(&mut gh, vote_closed_body(test_vote_id()));
    gh.expect_create_check_run().never();
    expect_remove_open_label(&mut gh, || Ok(()));
    expect_close_labels(
        &mut gh,
        &mut Sequence::new(),
        &[VOTE_CLOSED_LABEL, VOTE_PASSED_LABEL],
        || Ok(()),
    );

    // Run and check the failure is recorded, leaving the check run for the retry
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_error_checking_state_steps_publishes_no_state() {
    // Setup database expectations
    let mut db = MockDB::new();
    let mut vote = setup_test_closed_vote(Some(setup_test_vote_results()));
    vote.cfg.announcements = None;
    vote.is_pull_request = true;
    expect_claim_vote(&mut db, vote);
    for step in [VoteCloseStep::CheckRun, VoteCloseStep::Labels] {
        db.expect_is_close_state_step_pending()
            .with(eq(test_vote_id()), eq(step))
            .times(1)
            .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    }
    expect_close_steps(&mut db, &[VoteCloseStep::Comment]);
    expect_close_failure(&mut db, VoteCloseFailureOutcome::RetryScheduled);
    db.expect_complete_vote_close().never();

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_comment_lookup(
        &mut gh,
        tmpl::VoteMarker::ResultsComment,
        test_vote_id(),
        test_lookup_since(),
        || Ok(false),
    );
    expect_comment(&mut gh, vote_closed_body(test_vote_id()));
    gh.expect_create_check_run().never();
    gh.expect_remove_label().never();
    gh.expect_add_labels().never();

    // Run and check the failure is recorded, leaving the state for the retry
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_error_claiming_vote() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_claim_vote_to_close()
        .times(1)
        .returning(|| Box::pin(future::ready(Err(format_err!(ERROR)))));
    let gh = MockGH::new();

    // Run and check the error is propagated
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    let err = votes_closer.close_finished_vote().await.unwrap_err();
    assert_eq!(err.to_string(), "error claiming vote to close");
    assert_eq!(err.root_cause().to_string(), ERROR);
}

#[tokio::test]
async fn close_finished_vote_error_closing_vote_records_failure() {
    // Setup database expectations: closing the vote fails, so nothing is recorded as
    // published and the close is not completed
    let mut db = MockDB::new();
    expect_claim_vote(&mut db, setup_test_vote());
    db.expect_close_vote()
        .withf(|vote_id, results, head_sha| {
            *vote_id == test_vote_id() && results == &Some(&setup_test_vote_results()) && head_sha.is_none()
        })
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    db.expect_is_close_state_step_pending().never();
    db.expect_record_vote_close_step().never();
    db.expect_complete_vote_close().never();
    expect_close_failure(&mut db, VoteCloseFailureOutcome::RetryScheduled);

    // Setup GitHub expectations: the results are calculated, and nothing is published
    let mut gh = MockGH::new();
    expect_vote_results(&mut gh, COMMENT_ID, Ok(vec![in_favor_reaction(USER1)]));
    expect_no_marker_lookups(&mut gh);
    gh.expect_post_comment().never();
    gh.expect_create_discussion().never();
    gh.expect_create_check_run().never();
    gh.expect_create_commit_check_run().never();
    gh.expect_remove_label().never();
    gh.expect_add_labels().never();

    // Run and check the failure is recorded and does not stop the votes closer
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_error_completing_close() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_claim_vote(&mut db, setup_test_closed_vote(None));
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], true);
    expect_close_steps(&mut db, &[VoteCloseStep::Labels]);
    db.expect_complete_vote_close()
        .with(eq(test_vote_id()))
        .times(1)
        .returning(|_| Box::pin(future::ready(Err(format_err!(ERROR)))));
    expect_close_failure(&mut db, VoteCloseFailureOutcome::RetryScheduled);

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_remove_open_label(&mut gh, || Ok(()));
    expect_close_labels(&mut gh, &mut Sequence::new(), &[VOTE_CLOSED_LABEL], || Ok(()));

    // Run and check the failure is recorded so that the close is completed later
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_error_creating_announcement_is_retried() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_claim_vote(&mut db, setup_test_closed_vote(Some(setup_test_vote_results())));
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], true);
    expect_close_steps(&mut db, &[VoteCloseStep::Comment, VoteCloseStep::Labels]);
    expect_close_failure(&mut db, VoteCloseFailureOutcome::RetryScheduled);
    db.expect_complete_vote_close().never();

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_comment_lookup(
        &mut gh,
        tmpl::VoteMarker::ResultsComment,
        test_vote_id(),
        test_lookup_since(),
        || Ok(false),
    );
    expect_announcement_lookup(&mut gh, test_vote_id(), test_lookup_since(), || Ok(false));
    expect_comment(&mut gh, vote_closed_body(test_vote_id()));
    expect_announcement_discussion(&mut gh, &mut Sequence::new(), test_vote_id(), || {
        Err(format_err!(ERROR))
    });
    expect_remove_open_label(&mut gh, || Ok(()));
    expect_close_labels(
        &mut gh,
        &mut Sequence::new(),
        &[VOTE_CLOSED_LABEL, VOTE_PASSED_LABEL],
        || Ok(()),
    );

    // Run and check the failure is recorded, so that the announcement is retried
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_error_creating_check_run() {
    // Setup database expectations
    let mut db = MockDB::new();
    let mut vote = setup_test_closed_vote(Some(setup_test_vote_results()));
    vote.cfg.announcements = None;
    vote.is_pull_request = true;
    expect_claim_vote(&mut db, vote);
    expect_state_steps_pending(&mut db, &[VoteCloseStep::CheckRun, VoteCloseStep::Labels], true);
    expect_close_steps(&mut db, &[VoteCloseStep::Comment, VoteCloseStep::Labels]);
    expect_close_failure(&mut db, VoteCloseFailureOutcome::RetryScheduled);
    db.expect_complete_vote_close().never();

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_comment_lookup(
        &mut gh,
        tmpl::VoteMarker::ResultsComment,
        test_vote_id(),
        test_lookup_since(),
        || Ok(false),
    );
    expect_comment(&mut gh, vote_closed_body(test_vote_id()));
    gh.expect_create_check_run()
        .with(
            eq(INST_ID),
            eq(ORG),
            eq(REPO),
            eq(ISSUE_NUM),
            eq(passed_check_details()),
        )
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    expect_remove_open_label(&mut gh, || Ok(()));
    expect_close_labels(
        &mut gh,
        &mut Sequence::new(),
        &[VOTE_CLOSED_LABEL, VOTE_PASSED_LABEL],
        || Ok(()),
    );

    // Run and check the failure is recorded and the later steps are done anyway
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_error_getting_head_closes_vote_with_its_results() {
    // Setup database expectations
    let mut db = MockDB::new();
    let mut vote = setup_test_vote();
    vote.is_pull_request = true;
    expect_claim_vote(&mut db, vote);
    expect_state_steps_pending(&mut db, &[VoteCloseStep::CheckRun, VoteCloseStep::Labels], true);
    db.expect_close_vote()
        .withf(|vote_id, results, head_sha| {
            *vote_id == test_vote_id() && results == &Some(&setup_test_vote_results()) && head_sha.is_none()
        })
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(true))));
    expect_close_steps(
        &mut db,
        &[
            VoteCloseStep::Comment,
            VoteCloseStep::Announcement,
            VoteCloseStep::CheckRun,
            VoteCloseStep::Labels,
        ],
    );
    expect_complete_vote_close(&mut db);
    db.expect_record_vote_close_failure().never();

    // Setup GitHub expectations: the check run falls back to the current head
    let mut gh = MockGH::new();
    expect_vote_results(&mut gh, COMMENT_ID, Ok(vec![in_favor_reaction(USER1)]));
    gh.expect_get_pr_head_sha()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    expect_comment(&mut gh, vote_closed_body(test_vote_id()));
    expect_announcement_discussion(&mut gh, &mut Sequence::new(), test_vote_id(), || Ok(()));
    gh.expect_create_commit_check_run().never();
    expect_check_run(&mut gh, passed_check_details());
    expect_remove_open_label(&mut gh, || Ok(()));
    expect_close_labels(
        &mut gh,
        &mut Sequence::new(),
        &[VOTE_CLOSED_LABEL, VOTE_PASSED_LABEL],
        || Ok(()),
    );

    // Run and check the vote is closed
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_error_posting_comment() {
    // Setup database expectations
    let mut db = MockDB::new();
    let mut vote = setup_test_closed_vote(Some(setup_test_vote_results()));
    vote.is_pull_request = true;
    expect_claim_vote(&mut db, vote);
    expect_state_steps_pending(&mut db, &[VoteCloseStep::CheckRun, VoteCloseStep::Labels], true);
    expect_close_steps(
        &mut db,
        &[
            VoteCloseStep::Announcement,
            VoteCloseStep::CheckRun,
            VoteCloseStep::Labels,
        ],
    );
    expect_close_failure(&mut db, VoteCloseFailureOutcome::RetryScheduled);
    db.expect_complete_vote_close().never();

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_comment_lookup(
        &mut gh,
        tmpl::VoteMarker::ResultsComment,
        test_vote_id(),
        test_lookup_since(),
        || Ok(false),
    );
    expect_announcement_lookup(&mut gh, test_vote_id(), test_lookup_since(), || Ok(false));
    expect_comment_in_sequence(
        &mut gh,
        &mut Sequence::new(),
        vote_closed_body(test_vote_id()),
        || Err(format_err!(ERROR)),
    );
    expect_announcement_discussion(&mut gh, &mut Sequence::new(), test_vote_id(), || Ok(()));
    expect_check_run(&mut gh, passed_check_details());
    expect_remove_open_label(&mut gh, || Ok(()));
    expect_close_labels(
        &mut gh,
        &mut Sequence::new(),
        &[VOTE_CLOSED_LABEL, VOTE_PASSED_LABEL],
        || Ok(()),
    );

    // Run and check the failure is recorded and the later steps are done anyway
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_error_recording_failure() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_claim_vote(&mut db, setup_test_closed_vote(None));
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], true);
    db.expect_record_vote_close_failure()
        .with(eq(test_vote_id()), eq(test_claimed_until()))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_remove_open_label(&mut gh, || Ok(()));
    expect_close_labels(&mut gh, &mut Sequence::new(), &[VOTE_CLOSED_LABEL], || {
        Err(format_err!("labels error"))
    });

    // Run and check the error is propagated, so that the votes closer pauses
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    let err = votes_closer.close_finished_vote().await.unwrap_err();
    assert_eq!(err.to_string(), "error recording vote close failure");
    assert_eq!(err.root_cause().to_string(), ERROR);
}

#[tokio::test]
async fn close_finished_vote_error_recording_skipped_step_records_failure() {
    // Setup database expectations: the labels step was superseded by a newer vote, and
    // recording it as done fails
    let mut db = MockDB::new();
    expect_claim_vote(&mut db, setup_test_closed_vote(None));
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], false);
    db.expect_record_vote_close_step()
        .with(eq(test_vote_id()), eq(VoteCloseStep::Labels))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    db.expect_complete_vote_close().never();
    expect_close_failure(&mut db, VoteCloseFailureOutcome::RetryScheduled);

    // Setup GitHub expectations: the labels superseded are not updated
    let mut gh = MockGH::new();
    gh.expect_remove_label().never();
    gh.expect_add_labels().never();

    // Run and check the failure is recorded, leaving the step for the retry
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_error_recording_step_does_not_stop_later_steps() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_claim_vote(&mut db, setup_test_closed_vote(Some(setup_test_vote_results())));
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], true);
    db.expect_record_vote_close_step()
        .with(eq(test_vote_id()), eq(VoteCloseStep::Comment))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    expect_close_steps(&mut db, &[VoteCloseStep::Announcement, VoteCloseStep::Labels]);
    expect_close_failure(&mut db, VoteCloseFailureOutcome::RetryScheduled);
    db.expect_complete_vote_close().never();

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_comment_lookup(
        &mut gh,
        tmpl::VoteMarker::ResultsComment,
        test_vote_id(),
        test_lookup_since(),
        || Ok(false),
    );
    expect_announcement_lookup(&mut gh, test_vote_id(), test_lookup_since(), || Ok(false));
    expect_comment(&mut gh, vote_closed_body(test_vote_id()));
    expect_announcement_discussion(&mut gh, &mut Sequence::new(), test_vote_id(), || Ok(()));
    expect_remove_open_label(&mut gh, || Ok(()));
    expect_close_labels(
        &mut gh,
        &mut Sequence::new(),
        &[VOTE_CLOSED_LABEL, VOTE_PASSED_LABEL],
        || Ok(()),
    );

    // Run and check the failure is recorded and the later steps are done anyway
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_ignores_reactions_after_the_end() {
    // Setup a vote finished some time ago
    let vote = setup_test_vote();
    let late_reaction = Reaction {
        content: REACTION_AGAINST.to_string(),
        created_at: (vote.ends_at + 1.hours()).format(&Rfc3339).unwrap(),
        ..in_favor_reaction(USER1)
    };

    // Setup database expectations: the results are the ones of the reaction in time
    let mut db = MockDB::new();
    expect_claim_vote(&mut db, vote);
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], true);
    db.expect_close_vote()
        .withf(|vote_id, results, head_sha| {
            *vote_id == test_vote_id() && results == &Some(&setup_test_vote_results()) && head_sha.is_none()
        })
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(true))));
    expect_close_steps(
        &mut db,
        &[
            VoteCloseStep::Comment,
            VoteCloseStep::Announcement,
            VoteCloseStep::Labels,
        ],
    );
    expect_complete_vote_close(&mut db);

    // Setup GitHub expectations: without the late reaction being ignored, the user
    // would have voted twice and the vote would not count
    let mut gh = MockGH::new();
    let reactions = vec![in_favor_reaction(USER1), late_reaction];
    expect_vote_results(&mut gh, COMMENT_ID, Ok(reactions));
    expect_vote_closed_published(&mut gh, test_vote_id());

    // Run and check the vote is closed
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_newer_vote_created_during_check_run_skips_labels() {
    // Setup database expectations: the newer vote shows up after the first check
    let mut db = MockDB::new();
    let mut seq = Sequence::new();
    let mut vote = setup_test_closed_vote(Some(setup_test_vote_results()));
    vote.cfg.announcements = None;
    vote.is_pull_request = true;
    expect_claim_vote(&mut db, vote);
    db.expect_is_close_state_step_pending()
        .with(eq(test_vote_id()), eq(VoteCloseStep::CheckRun))
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _| Box::pin(future::ready(Ok(true))));
    db.expect_is_close_state_step_pending()
        .with(eq(test_vote_id()), eq(VoteCloseStep::Labels))
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _| Box::pin(future::ready(Ok(false))));
    expect_close_steps(
        &mut db,
        &[
            VoteCloseStep::CheckRun,
            VoteCloseStep::Comment,
            VoteCloseStep::Labels,
        ],
    );
    expect_complete_vote_close(&mut db);

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_comment_lookup(
        &mut gh,
        tmpl::VoteMarker::ResultsComment,
        test_vote_id(),
        test_lookup_since(),
        || Ok(false),
    );
    expect_comment(&mut gh, vote_closed_body(test_vote_id()));
    expect_check_run(&mut gh, passed_check_details());
    gh.expect_remove_label().never();
    gh.expect_add_labels().never();

    // Run and check the labels of the newer vote were left alone
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_newer_vote_on_issue_skips_labels() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_claim_vote(&mut db, setup_test_closed_vote(None));
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], false);
    expect_close_steps(&mut db, &[VoteCloseStep::Labels]);
    expect_complete_vote_close(&mut db);

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_create_check_run().never();
    gh.expect_remove_label().never();
    gh.expect_add_labels().never();

    // Run and check the vote is closed successfully
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_newer_vote_on_pr_skips_check_run_and_labels() {
    // Setup database expectations
    let mut db = MockDB::new();
    let mut vote = setup_test_closed_vote(Some(setup_test_vote_results()));
    vote.is_pull_request = true;
    vote.close_head_sha = Some(HEAD_SHA.to_string());
    expect_claim_vote(&mut db, vote);
    expect_state_steps_pending(&mut db, &[VoteCloseStep::CheckRun, VoteCloseStep::Labels], false);
    expect_close_steps(
        &mut db,
        &[
            VoteCloseStep::Announcement,
            VoteCloseStep::CheckRun,
            VoteCloseStep::Comment,
            VoteCloseStep::Labels,
        ],
    );
    expect_complete_vote_close(&mut db);

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_comment_lookup(
        &mut gh,
        tmpl::VoteMarker::ResultsComment,
        test_vote_id(),
        test_lookup_since(),
        || Ok(false),
    );
    expect_announcement_lookup(&mut gh, test_vote_id(), test_lookup_since(), || Ok(false));
    expect_comment(&mut gh, vote_closed_body(test_vote_id()));
    expect_announcement_discussion(&mut gh, &mut Sequence::new(), test_vote_id(), || Ok(()));
    gh.expect_get_pr_head_sha().never();
    gh.expect_create_check_run().never();
    gh.expect_create_commit_check_run().never();
    gh.expect_remove_label().never();
    gh.expect_add_labels().never();

    // Run and check only the results were published
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_no_attempts_left() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_claim_vote(&mut db, setup_test_closed_vote(None));
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], true);
    expect_close_failure(&mut db, VoteCloseFailureOutcome::NoAttemptsLeft);
    db.expect_complete_vote_close().never();

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_remove_open_label(&mut gh, || Ok(()));
    expect_close_labels(&mut gh, &mut Sequence::new(), &[VOTE_CLOSED_LABEL], || {
        Err(format_err!(ERROR))
    });

    // Run and check the votes closer can continue with the next vote
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_none_closed() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_claim_vote_to_close().times(1).returning(|| Box::pin(future::ready(Ok(None))));
    let gh = MockGH::new();

    // Run and check no vote is closed
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), None);
}

#[tokio::test]
async fn close_finished_vote_on_issue() {
    // Setup vote results
    let results = setup_test_vote_results();
    let results_copy = results.clone();
    let results_copy2 = results.clone();

    // Setup database expectations
    let mut db = MockDB::new();
    expect_claim_vote(&mut db, setup_test_closed_vote(Some(results_copy)));
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], true);
    expect_close_steps(
        &mut db,
        &[
            VoteCloseStep::Comment,
            VoteCloseStep::Announcement,
            VoteCloseStep::Labels,
        ],
    );
    expect_complete_vote_close(&mut db);

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_comment_lookup(
        &mut gh,
        tmpl::VoteMarker::ResultsComment,
        test_vote_id(),
        test_lookup_since(),
        || Ok(false),
    );
    expect_announcement_lookup(&mut gh, test_vote_id(), test_lookup_since(), || Ok(false));
    gh.expect_post_comment()
        .withf(move |inst_id, owner, repo, issue_number, body| {
            let expected_body = tmpl::VoteClosed::new(test_vote_id(), &results_copy2).render().unwrap();
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
            let expected_body = tmpl::VoteClosedAnnouncement::new(test_vote_id(), ISSUE_NUM, TITLE, &results)
                .render()
                .unwrap();
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
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_on_issue_without_results() {
    // Setup database expectations (vote comment was deleted)
    let mut db = MockDB::new();
    expect_claim_vote(&mut db, setup_test_closed_vote(None));
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], true);
    expect_close_steps(&mut db, &[VoteCloseStep::Labels]);
    expect_complete_vote_close(&mut db);

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
    results.against = 1;
    results.in_favor = 0;
    results.in_favor_percentage = 0.0;
    results.passed = false;
    let results_copy = results.clone();
    let results_copy2 = results.clone();

    // Setup database expectations
    let mut db = MockDB::new();
    let mut vote = setup_test_closed_vote(Some(results_copy));
    vote.is_pull_request = true;
    expect_claim_vote(&mut db, vote);
    expect_state_steps_pending(&mut db, &[VoteCloseStep::CheckRun, VoteCloseStep::Labels], true);
    expect_close_steps(
        &mut db,
        &[
            VoteCloseStep::Comment,
            VoteCloseStep::Announcement,
            VoteCloseStep::CheckRun,
            VoteCloseStep::Labels,
        ],
    );
    expect_complete_vote_close(&mut db);

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_comment_lookup(
        &mut gh,
        tmpl::VoteMarker::ResultsComment,
        test_vote_id(),
        test_lookup_since(),
        || Ok(false),
    );
    expect_announcement_lookup(&mut gh, test_vote_id(), test_lookup_since(), || Ok(false));
    gh.expect_post_comment()
        .withf(move |inst_id, owner, repo, issue_number, body| {
            let expected_body = tmpl::VoteClosed::new(test_vote_id(), &results_copy2).render().unwrap();
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
            let expected_body = tmpl::VoteClosedAnnouncement::new(test_vote_id(), ISSUE_NUM, TITLE, &results)
                .render()
                .unwrap();
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && category == DISCUSSIONS_CATEGORY
                && title == build_announcement_title(ISSUE_NUM, TITLE)
                && body == expected_body.as_str()
        })
        .times(1)
        .returning(|_, _, _, _, _, _| Box::pin(future::ready(Ok(()))));
    expect_check_run(
        &mut gh,
        CheckDetails {
            status: "completed".to_string(),
            summary: "The vote did not pass. 0 out of 1 voted in favor.".to_string(),

            conclusion: Some("failure".to_string()),
        },
    );
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
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_on_pr_closed_vote_passed() {
    // Setup vote results
    let results = setup_test_vote_results();
    let results_copy = results.clone();
    let results_copy2 = results.clone();

    // Setup database expectations
    let mut db = MockDB::new();
    let mut vote = setup_test_closed_vote(Some(results_copy));
    vote.is_pull_request = true;
    expect_claim_vote(&mut db, vote);
    expect_state_steps_pending(&mut db, &[VoteCloseStep::CheckRun, VoteCloseStep::Labels], true);
    expect_close_steps(
        &mut db,
        &[
            VoteCloseStep::Comment,
            VoteCloseStep::Announcement,
            VoteCloseStep::CheckRun,
            VoteCloseStep::Labels,
        ],
    );
    expect_complete_vote_close(&mut db);

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_comment_lookup(
        &mut gh,
        tmpl::VoteMarker::ResultsComment,
        test_vote_id(),
        test_lookup_since(),
        || Ok(false),
    );
    expect_announcement_lookup(&mut gh, test_vote_id(), test_lookup_since(), || Ok(false));
    gh.expect_post_comment()
        .withf(move |inst_id, owner, repo, issue_number, body| {
            let expected_body = tmpl::VoteClosed::new(test_vote_id(), &results_copy2).render().unwrap();
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
            let expected_body = tmpl::VoteClosedAnnouncement::new(test_vote_id(), ISSUE_NUM, TITLE, &results)
                .render()
                .unwrap();
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && category == DISCUSSIONS_CATEGORY
                && title == build_announcement_title(ISSUE_NUM, TITLE)
                && body == expected_body.as_str()
        })
        .times(1)
        .returning(|_, _, _, _, _, _| Box::pin(future::ready(Ok(()))));
    expect_check_run(
        &mut gh,
        CheckDetails {
            status: "completed".to_string(),
            summary: "The vote passed! 1 out of 1 voted in favor.".to_string(),

            conclusion: Some("success".to_string()),
        },
    );
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
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_on_pr_error_calculating_results_does_not_read_head() {
    // Setup database expectations
    let mut db = MockDB::new();
    let mut vote = setup_test_vote();
    vote.is_pull_request = true;
    expect_claim_vote(&mut db, vote);
    db.expect_close_vote().never();
    expect_close_failure(&mut db, VoteCloseFailureOutcome::RetryScheduled);
    db.expect_complete_vote_close().never();

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_comment_reactions()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(COMMENT_ID))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    gh.expect_get_pr_head_sha().never();
    gh.expect_post_comment().never();

    // Run and check the failure is recorded and does not stop the votes closer
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_on_pr_stored_head_is_used_by_retries() {
    // Setup database expectations
    let mut db = MockDB::new();
    let mut vote = setup_test_closed_vote(Some(setup_test_vote_results()));
    vote.cfg.announcements = None;
    vote.close_attempts = 1;
    vote.is_pull_request = true;
    vote.close_comment_posted_at = Some(test_closed_at());
    vote.close_head_sha = Some(HEAD_SHA.to_string());
    expect_claim_vote(&mut db, vote);
    expect_state_steps_pending(&mut db, &[VoteCloseStep::CheckRun, VoteCloseStep::Labels], true);
    db.expect_close_vote().never();
    expect_close_steps(&mut db, &[VoteCloseStep::CheckRun, VoteCloseStep::Labels]);
    expect_complete_vote_close(&mut db);

    // Setup GitHub expectations: the head of the pull request is not read again
    let mut gh = MockGH::new();
    gh.expect_get_pr_head_sha().never();
    gh.expect_create_check_run().never();
    expect_commit_check_run(&mut gh, HEAD_SHA, passed_check_details());
    expect_remove_open_label(&mut gh, || Ok(()));
    expect_close_labels(
        &mut gh,
        &mut Sequence::new(),
        &[VOTE_CLOSED_LABEL, VOTE_PASSED_LABEL],
        || Ok(()),
    );

    // Run and check the vote is processed
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_on_pr_stores_head_and_creates_check_run_on_it() {
    // Setup database expectations
    let mut db = MockDB::new();
    let mut vote = setup_test_vote();
    vote.is_pull_request = true;
    expect_claim_vote(&mut db, vote);
    expect_state_steps_pending(&mut db, &[VoteCloseStep::CheckRun, VoteCloseStep::Labels], true);
    db.expect_close_vote()
        .withf(|vote_id, results, head_sha| {
            *vote_id == test_vote_id()
                && results == &Some(&setup_test_vote_results())
                && *head_sha == Some(HEAD_SHA)
        })
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(true))));
    expect_close_steps(
        &mut db,
        &[
            VoteCloseStep::Comment,
            VoteCloseStep::Announcement,
            VoteCloseStep::CheckRun,
            VoteCloseStep::Labels,
        ],
    );
    expect_complete_vote_close(&mut db);

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_vote_results(&mut gh, COMMENT_ID, Ok(vec![in_favor_reaction(USER1)]));
    expect_pr_head_sha(&mut gh, HEAD_SHA);
    expect_comment(&mut gh, vote_closed_body(test_vote_id()));
    expect_announcement_discussion(&mut gh, &mut Sequence::new(), test_vote_id(), || Ok(()));
    gh.expect_create_check_run().never();
    expect_commit_check_run(&mut gh, HEAD_SHA, passed_check_details());
    expect_remove_open_label(&mut gh, || Ok(()));
    expect_close_labels(
        &mut gh,
        &mut Sequence::new(),
        &[VOTE_CLOSED_LABEL, VOTE_PASSED_LABEL],
        || Ok(()),
    );

    // Run and check the vote is closed
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_on_pr_without_results() {
    // Setup database expectations (vote comment was deleted)
    let mut db = MockDB::new();
    let mut vote = setup_test_closed_vote(None);
    vote.is_pull_request = true;
    expect_claim_vote(&mut db, vote);
    expect_state_steps_pending(&mut db, &[VoteCloseStep::CheckRun, VoteCloseStep::Labels], true);
    expect_close_steps(&mut db, &[VoteCloseStep::CheckRun, VoteCloseStep::Labels]);
    expect_complete_vote_close(&mut db);

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_post_comment().never();
    gh.expect_create_discussion().never();
    expect_check_run(
        &mut gh,
        CheckDetails {
            status: "completed".to_string(),
            summary: CHECK_SUMMARY_NO_RESULTS.to_string(),

            conclusion: Some("failure".to_string()),
        },
    );
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
async fn close_finished_vote_open_vote_retry_does_not_look_up_markers() {
    // Setup database expectations: previous attempts failed before closing the vote
    let mut db = MockDB::new();
    let mut vote = setup_test_vote();
    vote.close_attempts = 3;
    expect_claim_vote(&mut db, vote);
    db.expect_close_vote()
        .withf(|vote_id, results, head_sha| {
            *vote_id == test_vote_id() && results == &Some(&setup_test_vote_results()) && head_sha.is_none()
        })
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(true))));
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], true);
    expect_close_steps(
        &mut db,
        &[
            VoteCloseStep::Announcement,
            VoteCloseStep::Comment,
            VoteCloseStep::Labels,
        ],
    );
    expect_complete_vote_close(&mut db);

    // Setup GitHub expectations: nothing can have been published before the vote was
    // closed, so the results are posted without looking them up
    let mut gh = MockGH::new();
    expect_vote_results(&mut gh, COMMENT_ID, Ok(vec![in_favor_reaction(USER1)]));
    expect_no_marker_lookups(&mut gh);
    expect_vote_closed_published(&mut gh, test_vote_id());

    // Run and check the vote is closed
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_removing_label_error_is_ignored() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_claim_vote(&mut db, setup_test_closed_vote(None));
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], true);
    expect_close_steps(&mut db, &[VoteCloseStep::Labels]);
    expect_complete_vote_close(&mut db);

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
async fn close_finished_vote_retry_announcement_lookup_error_records_failure() {
    // Setup database expectations: the announcement is not recorded, as it is unknown
    // whether the previous attempt created it
    let mut db = MockDB::new();
    let mut vote = setup_test_closed_vote(Some(setup_test_vote_results()));
    vote.close_comment_posted_at = Some(test_closed_at());
    vote.close_labels_updated_at = Some(test_closed_at());
    expect_claim_vote(&mut db, vote);
    db.expect_record_vote_close_step().never();
    db.expect_complete_vote_close().never();
    expect_close_failure(&mut db, VoteCloseFailureOutcome::RetryScheduled);

    // Setup GitHub expectations: the announcement is not created blindly
    let mut gh = MockGH::new();
    gh.expect_has_comment_with_marker().never();
    expect_announcement_lookup(&mut gh, test_vote_id(), test_lookup_since(), || {
        Err(format_err!(ERROR))
    });
    gh.expect_create_discussion().never();

    // Run and check the failure is recorded
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_retry_announcement_lookup_rejected_is_not_retried() {
    // Setup database expectations: the announcement is given up, as when its creation
    // is rejected
    let mut db = MockDB::new();
    let mut vote = setup_test_closed_vote(Some(setup_test_vote_results()));
    vote.close_comment_posted_at = Some(test_closed_at());
    vote.close_labels_updated_at = Some(test_closed_at());
    expect_claim_vote(&mut db, vote);
    expect_close_steps(&mut db, &[VoteCloseStep::Announcement]);
    expect_complete_vote_close(&mut db);
    db.expect_record_vote_close_failure().never();

    // Setup GitHub expectations: GitHub rejects the lookup for a reason that retrying
    // will not solve
    let mut gh = MockGH::new();
    expect_announcement_lookup(&mut gh, test_vote_id(), test_lookup_since(), || {
        Err(DiscussionRejectedError(format_err!(ERROR)).into())
    });
    gh.expect_create_discussion().never();

    // Run and check the vote close is complete
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_retry_comment_lookup_error_records_failure() {
    // Setup database expectations: the comment is not recorded, as it is unknown whether
    // the previous attempt posted it, while the other steps are done
    let mut db = MockDB::new();
    expect_claim_vote(&mut db, setup_test_closed_vote(Some(setup_test_vote_results())));
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], true);
    expect_close_steps(&mut db, &[VoteCloseStep::Announcement, VoteCloseStep::Labels]);
    db.expect_complete_vote_close().never();
    expect_close_failure(&mut db, VoteCloseFailureOutcome::RetryScheduled);

    // Setup GitHub expectations: the comment is not posted blindly
    let mut gh = MockGH::new();
    expect_comment_lookup(
        &mut gh,
        tmpl::VoteMarker::ResultsComment,
        test_vote_id(),
        test_lookup_since(),
        || Err(format_err!(ERROR)),
    );
    gh.expect_post_comment().never();
    expect_announcement_lookup(&mut gh, test_vote_id(), test_lookup_since(), || Ok(false));
    expect_announcement_discussion(&mut gh, &mut Sequence::new(), test_vote_id(), || Ok(()));
    expect_remove_open_label(&mut gh, || Ok(()));
    expect_close_labels(
        &mut gh,
        &mut Sequence::new(),
        &[VOTE_CLOSED_LABEL, VOTE_PASSED_LABEL],
        || Ok(()),
    );

    // Run and check the failure is recorded
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_retry_looks_up_steps_before_posting_them() {
    // Setup database expectations
    let mut db = MockDB::new();
    let mut vote = setup_test_closed_vote(Some(setup_test_vote_results()));
    vote.close_labels_updated_at = Some(test_closed_at());
    expect_claim_vote(&mut db, vote);
    expect_close_steps(&mut db, &[VoteCloseStep::Announcement, VoteCloseStep::Comment]);
    expect_complete_vote_close(&mut db);

    // Setup GitHub expectations: the comment and the announcement are not found, so they
    // are posted after looking them up
    let mut seq = Sequence::new();
    let mut gh = MockGH::new();
    expect_comment_lookup_in_sequence(
        &mut gh,
        &mut seq,
        tmpl::VoteMarker::ResultsComment,
        test_vote_id(),
        test_lookup_since(),
        || Ok(false),
    );
    expect_comment_in_sequence(&mut gh, &mut seq, vote_closed_body(test_vote_id()), || {
        Ok(COMMENT_ID)
    });
    expect_announcement_lookup_in_sequence(&mut gh, &mut seq, test_vote_id(), test_lookup_since(), || {
        Ok(false)
    });
    expect_announcement_discussion(&mut gh, &mut seq, test_vote_id(), || Ok(()));

    // Run and check the vote close is complete
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_retry_only_runs_missing_steps() {
    use VoteCloseStep as Step;

    // Setup the cases: the steps done already and the ones the retry has to run
    let done_at = test_closed_at();
    let cases = [
        (
            vec![],
            vec![Step::Comment, Step::Announcement, Step::CheckRun, Step::Labels],
        ),
        (
            vec![Step::Comment],
            vec![Step::Announcement, Step::CheckRun, Step::Labels],
        ),
        (
            vec![Step::Announcement],
            vec![Step::Comment, Step::CheckRun, Step::Labels],
        ),
        (
            vec![Step::Comment, Step::Announcement],
            vec![Step::CheckRun, Step::Labels],
        ),
        (
            vec![Step::Comment, Step::Announcement, Step::CheckRun],
            vec![Step::Labels],
        ),
        (
            vec![Step::CheckRun, Step::Labels],
            vec![Step::Comment, Step::Announcement],
        ),
        (
            vec![Step::Comment, Step::Announcement, Step::CheckRun, Step::Labels],
            vec![],
        ),
    ];

    for (done, missing) in cases {
        // Setup a pull request vote closed already with the steps done recorded
        let mut vote = setup_test_closed_vote(Some(setup_test_vote_results()));
        vote.close_attempts = 1;
        vote.is_pull_request = true;
        vote.close_announcement_posted_at = done.contains(&Step::Announcement).then_some(done_at);
        vote.close_check_run_created_at = done.contains(&Step::CheckRun).then_some(done_at);
        vote.close_comment_posted_at = done.contains(&Step::Comment).then_some(done_at);
        vote.close_labels_updated_at = done.contains(&Step::Labels).then_some(done_at);

        // Setup database expectations
        let mut db = MockDB::new();
        expect_claim_vote(&mut db, vote);
        let state_steps: Vec<_> =
            [Step::CheckRun, Step::Labels].into_iter().filter(|s| missing.contains(s)).collect();
        expect_state_steps_pending(&mut db, &state_steps, true);
        db.expect_close_vote().never();
        expect_close_steps(&mut db, &missing);
        expect_complete_vote_close(&mut db);

        // Setup GitHub expectations: the comment and the announcement missing are looked
        // up before posting them, as a previous attempt may have posted them, and none
        // of the steps done is published again
        let mut gh = MockGH::new();
        gh.expect_get_comment_reactions().never();
        if missing.contains(&Step::Comment) {
            expect_comment_lookup(
                &mut gh,
                tmpl::VoteMarker::ResultsComment,
                test_vote_id(),
                test_lookup_since(),
                || Ok(false),
            );
            expect_comment(&mut gh, vote_closed_body(test_vote_id()));
        } else {
            gh.expect_has_comment_with_marker().never();
            gh.expect_post_comment().never();
        }
        if missing.contains(&Step::Announcement) {
            expect_announcement_lookup(&mut gh, test_vote_id(), test_lookup_since(), || Ok(false));
            expect_announcement_discussion(&mut gh, &mut Sequence::new(), test_vote_id(), || Ok(()));
        } else {
            gh.expect_has_discussion_with_marker().never();
            gh.expect_create_discussion().never();
        }
        if missing.contains(&Step::CheckRun) {
            expect_check_run(&mut gh, passed_check_details());
        } else {
            gh.expect_create_check_run().never();
        }
        if missing.contains(&Step::Labels) {
            expect_remove_open_label(&mut gh, || Ok(()));
            expect_close_labels(
                &mut gh,
                &mut Sequence::new(),
                &[VOTE_CLOSED_LABEL, VOTE_PASSED_LABEL],
                || Ok(()),
            );
        } else {
            gh.expect_remove_label().never();
            gh.expect_add_labels().never();
        }

        // Run and check the vote is processed
        let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
        assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
    }
}

#[tokio::test]
async fn close_finished_vote_retry_records_steps_found_published() {
    // Setup database expectations: a vote closed by a worker that died before recording
    // anything else, so its claim expired without a failure being recorded
    let mut db = MockDB::new();
    let vote = setup_test_closed_vote(Some(setup_test_vote_results()));
    assert_eq!(vote.close_attempts, 0);
    expect_claim_vote(&mut db, vote);
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], true);
    expect_close_steps(
        &mut db,
        &[
            VoteCloseStep::Announcement,
            VoteCloseStep::Comment,
            VoteCloseStep::Labels,
        ],
    );
    expect_complete_vote_close(&mut db);
    db.expect_record_vote_close_failure().never();

    // Setup GitHub expectations: the comment and the announcement are found, so they are
    // not posted again
    let mut gh = MockGH::new();
    expect_comment_lookup(
        &mut gh,
        tmpl::VoteMarker::ResultsComment,
        test_vote_id(),
        test_lookup_since(),
        || Ok(true),
    );
    expect_announcement_lookup(&mut gh, test_vote_id(), test_lookup_since(), || Ok(true));
    gh.expect_post_comment().never();
    gh.expect_create_discussion().never();
    expect_remove_open_label(&mut gh, || Ok(()));
    expect_close_labels(
        &mut gh,
        &mut Sequence::new(),
        &[VOTE_CLOSED_LABEL, VOTE_PASSED_LABEL],
        || Ok(()),
    );

    // Run and check the vote close is complete
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_vote_cancelled_after_being_claimed() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_claim_vote(&mut db, setup_test_vote());
    db.expect_close_vote()
        .withf(|vote_id, results, head_sha| {
            *vote_id == test_vote_id() && results == &Some(&setup_test_vote_results()) && head_sha.is_none()
        })
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(false))));
    db.expect_record_vote_close_step().never();
    db.expect_complete_vote_close().never();

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_vote_results(&mut gh, COMMENT_ID, Ok(vec![in_favor_reaction(USER1)]));
    gh.expect_post_comment().never();
    gh.expect_add_labels().never();

    // Run and check nothing is published
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_without_announcements() {
    // Setup database expectations
    let mut db = MockDB::new();
    let mut vote = setup_test_closed_vote(Some(setup_test_vote_results()));
    vote.cfg.announcements = None;
    expect_claim_vote(&mut db, vote);
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], true);
    expect_close_steps(&mut db, &[VoteCloseStep::Comment, VoteCloseStep::Labels]);
    expect_complete_vote_close(&mut db);

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_comment_lookup(
        &mut gh,
        tmpl::VoteMarker::ResultsComment,
        test_vote_id(),
        test_lookup_since(),
        || Ok(false),
    );
    expect_comment(&mut gh, vote_closed_body(test_vote_id()));
    gh.expect_has_discussion_with_marker().never();
    gh.expect_create_discussion().never();
    expect_remove_open_label(&mut gh, || Ok(()));
    expect_close_labels(
        &mut gh,
        &mut Sequence::new(),
        &[VOTE_CLOSED_LABEL, VOTE_PASSED_LABEL],
        || Ok(()),
    );

    // Run and check the vote is closed
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
}

#[tokio::test]
async fn close_finished_vote_without_issue_title() {
    // Setup database expectations
    let mut db = MockDB::new();
    let mut vote = setup_test_closed_vote(Some(setup_test_vote_results()));
    vote.issue_title = None;
    expect_claim_vote(&mut db, vote);
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], true);
    expect_close_steps(
        &mut db,
        &[
            VoteCloseStep::Comment,
            VoteCloseStep::Announcement,
            VoteCloseStep::Labels,
        ],
    );
    expect_complete_vote_close(&mut db);

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_comment_lookup(
        &mut gh,
        tmpl::VoteMarker::ResultsComment,
        test_vote_id(),
        test_lookup_since(),
        || Ok(false),
    );
    expect_announcement_lookup(&mut gh, test_vote_id(), test_lookup_since(), || Ok(false));
    expect_comment(&mut gh, vote_closed_body(test_vote_id()));
    gh.expect_create_discussion()
        .withf(|inst_id, owner, repo, category, title, body| {
            let results = setup_test_vote_results();
            let expected_body = tmpl::VoteClosedAnnouncement::new(test_vote_id(), ISSUE_NUM, "", &results)
                .render()
                .unwrap();
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

// Helpers.

/// Expects a lookup of the results announcement of the vote provided in the test
/// discussions category once, in the sequence given, from a time in the range provided,
/// with the result given.
fn expect_announcement_lookup_in_sequence(
    gh: &mut MockGH,
    seq: &mut Sequence,
    vote_id: Uuid,
    since: RangeInclusive<OffsetDateTime>,
    result: fn() -> Result<bool>,
) {
    let expected_marker = tmpl::VoteMarker::ResultsAnnouncement.render(vote_id);
    gh.expect_has_discussion_with_marker()
        .withf(move |inst_id, owner, repo, category, marker, lookup_since| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && category == DISCUSSIONS_CATEGORY
                && marker == expected_marker
                && since.contains(lookup_since)
        })
        .times(1)
        .in_sequence(seq)
        .returning(move |_, _, _, _, _, _| Box::pin(future::ready(result())));
}

/// Expects a lookup of the comment with the marker provided of the vote given on the
/// test issue once, in the sequence provided, from a time in the range given, with the
/// result provided.
fn expect_comment_lookup_in_sequence(
    gh: &mut MockGH,
    seq: &mut Sequence,
    marker: tmpl::VoteMarker,
    vote_id: Uuid,
    since: RangeInclusive<OffsetDateTime>,
    result: fn() -> Result<bool>,
) {
    let expected_marker = marker.render(vote_id);
    gh.expect_has_comment_with_marker()
        .withf(move |inst_id, owner, repo, issue_number, marker, lookup_since| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && marker == expected_marker
                && since.contains(lookup_since)
        })
        .times(1)
        .in_sequence(seq)
        .returning(move |_, _, _, _, _, _| Box::pin(future::ready(result())));
}
