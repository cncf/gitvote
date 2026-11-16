use anyhow::format_err;
use mockall::{Sequence, predicate::eq};
use tokio::time::Instant;

use crate::{db::MockDB, github::*, testutil::*};

use super::*;

#[tokio::test]
async fn cancel_vote_cancellation_abandoned_is_not_an_error() {
    // Setup database expectations: the failure was the last attempt, so the
    // cancellation was abandoned when recording it
    let mut db = MockDB::new();
    expect_cancel_vote(&mut db, Some(setup_test_cancelled_vote(false)));
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], true);
    expect_close_steps(&mut db, &[VoteCloseStep::Labels]);
    db.expect_complete_vote_cancellation().never();
    expect_close_failure(&mut db, VoteCloseFailureOutcome::CancellationAbandoned);

    // Setup GitHub expectations: posting the comment fails
    let mut gh = MockGH::new();
    expect_user_is_collaborator(&mut gh, true);
    expect_vote_cancelled_comment_fails(&mut gh, test_vote_id(), false);
    expect_remove_open_label(&mut gh, || Ok(()));

    // Run and check the command succeeds, so no failure comment would be posted
    let input = CancelVoteInput::new(&Event::IssueComment(setup_test_issue_comment_event()));
    setup_cmds_handler(db, gh).cancel_vote(&input).await.unwrap();
}

#[tokio::test]
async fn cancel_vote_claim_taken_over_is_not_an_error() {
    // Setup database expectations: the claim was taken over when recording the failure
    let mut db = MockDB::new();
    expect_cancel_vote(&mut db, Some(setup_test_cancelled_vote(false)));
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], true);
    expect_close_steps(&mut db, &[VoteCloseStep::Labels]);
    db.expect_complete_vote_cancellation().never();
    expect_close_failure(&mut db, VoteCloseFailureOutcome::ClaimLost);

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_user_is_collaborator(&mut gh, true);
    expect_vote_cancelled_comment_fails(&mut gh, test_vote_id(), false);
    expect_remove_open_label(&mut gh, || Ok(()));

    // Run and check the command succeeds, as the owner of the claim publishes it
    let input = CancelVoteInput::new(&Event::IssueComment(setup_test_issue_comment_event()));
    setup_cmds_handler(db, gh).cancel_vote(&input).await.unwrap();
}

#[tokio::test]
async fn cancel_vote_error_cancelling() {
    // Setup database expectations: nothing is committed, so nothing else is done
    let mut db = MockDB::new();
    db.expect_cancel_vote()
        .withf(|repository_full_name, issue_number, cancelled_by| {
            repository_full_name == REPOFN && *issue_number == ISSUE_NUM && cancelled_by == USER
        })
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    db.expect_record_vote_close_step().never();
    db.expect_record_vote_close_failure().never();
    db.expect_complete_vote_cancellation().never();

    // Setup GitHub expectations: the cancellation is not published
    let mut gh = MockGH::new();
    expect_user_is_collaborator(&mut gh, true);
    gh.expect_post_comment().never();
    gh.expect_create_check_run().never();
    gh.expect_remove_label().never();

    // Run and check the error is propagated
    let input = CancelVoteInput::new(&Event::PullRequest(setup_test_pr_event()));
    let err = setup_cmds_handler(db, gh).cancel_vote(&input).await.unwrap_err();
    assert_eq!(err.to_string(), "error cancelling vote");
    assert_eq!(err.root_cause().to_string(), ERROR);
}

#[tokio::test]
async fn cancel_vote_error_checking_if_user_is_collaborator() {
    // Setup database expectations: the vote is not cancelled
    let mut db = MockDB::new();
    db.expect_cancel_vote().never();

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_user_is_collaborator()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Run and check the error is propagated
    let input = CancelVoteInput::new(&Event::IssueComment(setup_test_issue_comment_event()));
    let err = setup_cmds_handler(db, gh).cancel_vote(&input).await.unwrap_err();
    assert_eq!(err.to_string(), "error checking if user is collaborator");
    assert_eq!(err.root_cause().to_string(), ERROR);
}

#[tokio::test]
async fn cancel_vote_error_deleting_vote_records_failure() {
    // Setup database expectations: the vote cannot be deleted once published
    let mut db = MockDB::new();
    expect_cancel_vote(&mut db, Some(setup_test_cancelled_vote(false)));
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], true);
    expect_close_steps(&mut db, &[VoteCloseStep::Comment, VoteCloseStep::Labels]);
    db.expect_complete_vote_cancellation()
        .with(eq(test_vote_id()))
        .times(1)
        .returning(|_| Box::pin(future::ready(Err(format_err!(ERROR)))));
    expect_close_failure(&mut db, VoteCloseFailureOutcome::RetryScheduled);

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_user_is_collaborator(&mut gh, true);
    expect_comment(&mut gh, vote_cancelled_body(test_vote_id(), false));
    expect_remove_open_label(&mut gh, || Ok(()));

    // Run and check the command succeeds, as the cancellation is retried later
    let input = CancelVoteInput::new(&Event::IssueComment(setup_test_issue_comment_event()));
    setup_cmds_handler(db, gh).cancel_vote(&input).await.unwrap();
}

#[tokio::test]
async fn cancel_vote_error_posting_comment_records_failure() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_cancel_vote(&mut db, Some(setup_test_cancelled_vote(false)));
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], true);
    expect_close_steps(&mut db, &[VoteCloseStep::Labels]);
    db.expect_complete_vote_cancellation().never();
    expect_close_failure(&mut db, VoteCloseFailureOutcome::RetryScheduled);

    // Setup GitHub expectations: the other steps are tried even if the comment fails,
    // and the first attempt does not look up a comment posted before
    let mut gh = MockGH::new();
    expect_user_is_collaborator(&mut gh, true);
    expect_no_marker_lookups(&mut gh);
    expect_vote_cancelled_comment_fails(&mut gh, test_vote_id(), false);
    expect_remove_open_label(&mut gh, || Ok(()));

    // Run and check the command succeeds, as the cancellation is retried later
    let input = CancelVoteInput::new(&Event::IssueComment(setup_test_issue_comment_event()));
    setup_cmds_handler(db, gh).cancel_vote(&input).await.unwrap();
}

#[tokio::test]
async fn cancel_vote_error_recording_failure_is_ignored() {
    // Setup database expectations: the failure cannot be recorded either
    let mut db = MockDB::new();
    expect_cancel_vote(&mut db, Some(setup_test_cancelled_vote(false)));
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], true);
    expect_close_steps(&mut db, &[VoteCloseStep::Labels]);
    db.expect_complete_vote_cancellation().never();
    db.expect_record_vote_close_failure()
        .with(eq(test_vote_id()), eq(test_claimed_until()))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_user_is_collaborator(&mut gh, true);
    expect_vote_cancelled_comment_fails(&mut gh, test_vote_id(), false);
    expect_remove_open_label(&mut gh, || Ok(()));

    // Run and check the command succeeds, as the cancellation is retried once the
    // claim expires
    let input = CancelVoteInput::new(&Event::IssueComment(setup_test_issue_comment_event()));
    setup_cmds_handler(db, gh).cancel_vote(&input).await.unwrap();
}

#[tokio::test]
async fn cancel_vote_in_pr_error_creating_check_run_records_failure() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_cancel_vote(&mut db, Some(setup_test_cancelled_vote(true)));
    expect_state_steps_pending(&mut db, &[VoteCloseStep::CheckRun, VoteCloseStep::Labels], true);
    expect_close_steps(&mut db, &[VoteCloseStep::Comment, VoteCloseStep::Labels]);
    db.expect_complete_vote_cancellation().never();
    expect_close_failure(&mut db, VoteCloseFailureOutcome::RetryScheduled);

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_user_is_collaborator(&mut gh, true);
    expect_comment(&mut gh, vote_cancelled_body(test_vote_id(), true));
    gh.expect_create_check_run()
        .with(
            eq(INST_ID),
            eq(ORG),
            eq(REPO),
            eq(ISSUE_NUM),
            eq(cancelled_check_details()),
        )
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    expect_remove_open_label(&mut gh, || Ok(()));

    // Run and check the command succeeds, as the cancellation is retried later
    let input = CancelVoteInput::new(&Event::PullRequest(setup_test_pr_event()));
    setup_cmds_handler(db, gh).cancel_vote(&input).await.unwrap();
}

#[tokio::test]
async fn cancel_vote_in_pr_newer_vote_skips_check_run_and_labels() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_cancel_vote(&mut db, Some(setup_test_cancelled_vote(true)));
    expect_state_steps_pending(&mut db, &[VoteCloseStep::CheckRun, VoteCloseStep::Labels], false);
    expect_close_steps(
        &mut db,
        &[
            VoteCloseStep::CheckRun,
            VoteCloseStep::Comment,
            VoteCloseStep::Labels,
        ],
    );
    expect_complete_vote_cancellation(&mut db);

    // Setup GitHub expectations: only the comment is posted
    let mut gh = MockGH::new();
    expect_user_is_collaborator(&mut gh, true);
    expect_comment(&mut gh, vote_cancelled_body(test_vote_id(), true));
    gh.expect_create_check_run().never();
    gh.expect_remove_label().never();

    // Run and check the state of the newer vote was left alone
    let input = CancelVoteInput::new(&Event::PullRequest(setup_test_pr_event()));
    setup_cmds_handler(db, gh).cancel_vote(&input).await.unwrap();
}

#[tokio::test]
async fn cancel_vote_in_pr_no_vote_in_progress() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_cancel_vote(&mut db, None);
    db.expect_complete_vote_cancellation().never();

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_user_is_collaborator(&mut gh, true);
    expect_comment(&mut gh, tmpl::NoVoteInProgress::new(USER, true).render().unwrap());
    gh.expect_create_check_run().never();
    gh.expect_remove_label().never();

    // Run and check the vote is not cancelled
    let input = CancelVoteInput::new(&Event::PullRequest(setup_test_pr_event()));
    setup_cmds_handler(db, gh).cancel_vote(&input).await.unwrap();
}

#[tokio::test]
async fn cancel_vote_in_pr_success() {
    // Setup database expectations: the vote is cancelled before anything is published
    let mut seq = Sequence::new();
    let mut db = MockDB::new();
    db.expect_cancel_vote()
        .withf(|repository_full_name, issue_number, cancelled_by| {
            repository_full_name == REPOFN && *issue_number == ISSUE_NUM && cancelled_by == USER
        })
        .times(1)
        .in_sequence(&mut seq)
        .return_once(|_, _, _| Box::pin(future::ready(Ok(Some(setup_test_cancelled_vote(true))))));
    expect_state_steps_pending(&mut db, &[VoteCloseStep::CheckRun, VoteCloseStep::Labels], true);
    expect_close_steps(
        &mut db,
        &[
            VoteCloseStep::CheckRun,
            VoteCloseStep::Comment,
            VoteCloseStep::Labels,
        ],
    );
    expect_complete_vote_cancellation(&mut db);

    // Setup GitHub expectations: the comment is posted first, after the cancellation
    let mut gh = MockGH::new();
    expect_user_is_collaborator(&mut gh, true);
    let expected_body = vote_cancelled_body(test_vote_id(), true);
    gh.expect_post_comment()
        .withf(move |inst_id, owner, repo, issue_number, body| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body == expected_body
        })
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
    gh.expect_create_check_run()
        .with(
            eq(INST_ID),
            eq(ORG),
            eq(REPO),
            eq(ISSUE_NUM),
            eq(cancelled_check_details()),
        )
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
    expect_remove_open_label(&mut gh, || Ok(()));

    // Run and check the vote is cancelled successfully
    let input = CancelVoteInput::new(&Event::PullRequest(setup_test_pr_event()));
    setup_cmds_handler(db, gh).cancel_vote(&input).await.unwrap();
}

#[tokio::test]
async fn cancel_vote_no_vote_in_progress() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_cancel_vote(&mut db, None);

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_user_is_collaborator(&mut gh, true);
    expect_comment(
        &mut gh,
        tmpl::NoVoteInProgress::new(USER, false).render().unwrap(),
    );
    gh.expect_remove_label().never();

    // Run and check the vote is not cancelled
    let input = CancelVoteInput::new(&Event::IssueComment(setup_test_issue_comment_event()));
    setup_cmds_handler(db, gh).cancel_vote(&input).await.unwrap();
}

#[tokio::test]
async fn cancel_vote_only_collaborators_can_close_votes() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_cancel_vote().never();

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_user_is_collaborator(&mut gh, false);

    // Run and check the vote is not cancelled
    let input = CancelVoteInput::new(&Event::IssueComment(setup_test_issue_comment_event()));
    setup_cmds_handler(db, gh).cancel_vote(&input).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn cancel_vote_publication_timeout_records_failure() {
    // Setup database expectations: the vote is cancelled, and nothing is recorded as
    // done once its publication times out
    let mut db = MockDB::new();
    expect_cancel_vote(&mut db, Some(setup_test_cancelled_vote(false)));
    db.expect_record_vote_close_step().never();
    db.expect_complete_vote_cancellation().never();
    expect_close_failure(&mut db, VoteCloseFailureOutcome::RetryScheduled);

    // Setup GitHub expectations: posting the comment never completes, and the first
    // attempt does not look up a comment posted before
    let mut gh = MockGH::new();
    expect_user_is_collaborator(&mut gh, true);
    expect_no_marker_lookups(&mut gh);
    let expected_body = vote_cancelled_body(test_vote_id(), false);
    gh.expect_post_comment()
        .withf(move |inst_id, owner, repo, issue_number, body| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body == expected_body
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::pending()));
    gh.expect_remove_label().never();

    // Run the command
    let started_at = Instant::now();
    let input = CancelVoteInput::new(&Event::IssueComment(setup_test_issue_comment_event()));
    setup_cmds_handler(db, gh).cancel_vote(&input).await.unwrap();

    // Check the publication was stopped once the attempt timed out
    assert_eq!(started_at.elapsed(), CLOSE_ATTEMPT_TIMEOUT);
}

#[tokio::test]
async fn cancel_vote_removing_label_error_is_ignored() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_cancel_vote(&mut db, Some(setup_test_cancelled_vote(false)));
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], true);
    expect_close_steps(&mut db, &[VoteCloseStep::Comment, VoteCloseStep::Labels]);
    expect_complete_vote_cancellation(&mut db);
    db.expect_record_vote_close_failure().never();

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_user_is_collaborator(&mut gh, true);
    expect_comment(&mut gh, vote_cancelled_body(test_vote_id(), false));
    expect_remove_open_label(&mut gh, || Err(format_err!(ERROR)));

    // Run and check the vote is cancelled successfully
    let input = CancelVoteInput::new(&Event::IssueComment(setup_test_issue_comment_event()));
    setup_cmds_handler(db, gh).cancel_vote(&input).await.unwrap();
}

#[tokio::test]
async fn cancel_vote_success() {
    // Setup database expectations
    let mut db = MockDB::new();
    expect_cancel_vote(&mut db, Some(setup_test_cancelled_vote(false)));
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], true);
    expect_close_steps(&mut db, &[VoteCloseStep::Comment, VoteCloseStep::Labels]);
    expect_complete_vote_cancellation(&mut db);
    db.expect_record_vote_close_failure().never();

    // Setup GitHub expectations: votes on issues have no check, and the first attempt
    // does not look up a comment posted before
    let mut gh = MockGH::new();
    expect_user_is_collaborator(&mut gh, true);
    expect_no_marker_lookups(&mut gh);
    expect_comment(&mut gh, vote_cancelled_body(test_vote_id(), false));
    gh.expect_create_check_run().never();
    expect_remove_open_label(&mut gh, || Ok(()));

    // Run and check the vote is cancelled successfully
    let input = CancelVoteInput::new(&Event::IssueComment(setup_test_issue_comment_event()));
    setup_cmds_handler(db, gh).cancel_vote(&input).await.unwrap();
}
