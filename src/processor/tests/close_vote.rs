use std::sync::Arc;

use anyhow::format_err;
use mockall::{Sequence, predicate::eq};

use crate::{db::MockDB, github::*, testutil::*};

use super::*;

#[tokio::test]
async fn close_vote_cancelled_vote_error_completing_cancellation_is_returned_with_context() {
    // Setup a cancelled vote with all its steps done
    let mut vote = setup_test_cancelled_vote(false);
    vote.close_comment_posted_at = Some(test_closed_at());
    vote.close_labels_updated_at = Some(test_closed_at());

    // Setup database expectations: completing the cancellation fails
    let mut db = MockDB::new();
    db.expect_complete_vote_cancellation()
        .with(eq(test_vote_id()))
        .times(1)
        .returning(|_| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Run and check the error says what failed
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(MockGH::new()));
    let err = votes_closer.close_vote(&vote).await.unwrap_err();
    assert_eq!(err.to_string(), "error completing vote cancellation");
    assert_eq!(err.root_cause().to_string(), ERROR);
}

#[tokio::test]
async fn close_vote_error_checking_state_step_is_returned_with_context() {
    // Setup database expectations: checking if the labels step is pending fails
    let mut db = MockDB::new();
    db.expect_is_close_state_step_pending()
        .with(eq(test_vote_id()), eq(VoteCloseStep::Labels))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    db.expect_record_vote_close_step().never();
    db.expect_complete_vote_close().never();

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_remove_label().never();
    gh.expect_add_labels().never();

    // Run and check the error says which step failed and why
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    let err = votes_closer.close_vote(&setup_test_closed_vote(None)).await.unwrap_err();
    assert_eq!(err.to_string(), "error running labels step");
    assert_eq!(
        err.chain().nth(1).unwrap().to_string(),
        "error checking if vote close step is pending"
    );
    assert_eq!(err.root_cause().to_string(), ERROR);
}

#[tokio::test]
async fn close_vote_error_closing_vote_in_database_is_returned_with_context() {
    // Setup database expectations: closing the vote fails
    let mut db = MockDB::new();
    db.expect_close_vote()
        .withf(|vote_id, results, head_sha| {
            *vote_id == test_vote_id() && results == &Some(&setup_test_vote_results()) && head_sha.is_none()
        })
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    db.expect_record_vote_close_step().never();
    db.expect_complete_vote_close().never();

    // Setup GitHub expectations: the results are calculated, and nothing is published
    let mut gh = MockGH::new();
    expect_vote_results(&mut gh, COMMENT_ID, Ok(vec![in_favor_reaction(USER1)]));
    gh.expect_post_comment().never();

    // Run and check the error says what failed
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    let err = votes_closer.close_vote(&setup_test_vote()).await.unwrap_err();
    assert_eq!(err.to_string(), "error closing vote in database");
    assert_eq!(err.root_cause().to_string(), ERROR);
}

#[tokio::test]
async fn close_vote_error_completing_close_is_returned_with_context() {
    // Setup a closed vote with all its steps done
    let mut vote = setup_test_closed_vote(None);
    vote.close_labels_updated_at = Some(test_closed_at());

    // Setup database expectations: completing the close fails
    let mut db = MockDB::new();
    db.expect_complete_vote_close()
        .with(eq(test_vote_id()))
        .times(1)
        .returning(|_| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Run and check the error says what failed
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(MockGH::new()));
    let err = votes_closer.close_vote(&vote).await.unwrap_err();
    assert_eq!(err.to_string(), "error completing vote close");
    assert_eq!(err.root_cause().to_string(), ERROR);
}

#[tokio::test]
async fn close_vote_error_recording_step_is_returned_with_context() {
    // Setup database expectations: the labels step was superseded by a newer vote, and
    // recording it as done fails
    let mut db = MockDB::new();
    expect_state_steps_pending(&mut db, &[VoteCloseStep::Labels], false);
    db.expect_record_vote_close_step()
        .with(eq(test_vote_id()), eq(VoteCloseStep::Labels))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    db.expect_complete_vote_close().never();

    // Run and check the error says which step failed and why
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(MockGH::new()));
    let err = votes_closer.close_vote(&setup_test_closed_vote(None)).await.unwrap_err();
    assert_eq!(err.to_string(), "error running labels step");
    assert_eq!(
        err.chain().nth(1).unwrap().to_string(),
        "error recording vote close step"
    );
    assert_eq!(err.root_cause().to_string(), ERROR);
}

#[tokio::test]
async fn close_vote_returns_the_first_error_after_trying_all_the_steps() {
    // Setup a closed vote on a pull request with all its steps pending
    let mut vote = setup_test_closed_vote(Some(setup_test_vote_results()));
    vote.is_pull_request = true;

    // Setup database expectations
    let mut db = MockDB::new();
    expect_state_steps_pending(&mut db, &[VoteCloseStep::CheckRun, VoteCloseStep::Labels], true);
    db.expect_record_vote_close_step().never();
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
        || Err(format_err!("comment error")),
    );
    expect_announcement_discussion(&mut gh, &mut Sequence::new(), test_vote_id(), || {
        Err(format_err!("discussion error"))
    });
    gh.expect_create_check_run()
        .with(
            eq(INST_ID),
            eq(ORG),
            eq(REPO),
            eq(ISSUE_NUM),
            eq(passed_check_details()),
        )
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Err(format_err!("check run error")))));
    expect_remove_open_label(&mut gh, || Ok(()));
    expect_close_labels(
        &mut gh,
        &mut Sequence::new(),
        &[VOTE_CLOSED_LABEL, VOTE_PASSED_LABEL],
        || Err(format_err!("labels error")),
    );

    // Run and check the error returned is the one from the first step
    let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
    let err = votes_closer.close_vote(&vote).await.unwrap_err();
    assert_eq!(err.to_string(), "error running comment step");
    assert_eq!(err.root_cause().to_string(), "comment error");
}
