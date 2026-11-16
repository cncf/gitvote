use std::{
    ops::RangeInclusive,
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
};

use anyhow::format_err;
use mockall::{Sequence, predicate::eq};
use time::{ext::NumericalDuration, format_description::well_known::Rfc3339};
use tokio::{
    sync::{Barrier, oneshot},
    time::timeout,
};
use uuid::Uuid;

use crate::{
    db::{CLOSE_MAX_ATTEMPTS, DB},
    github::*,
    testutil::*,
};

use super::*;

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_cancel_vote_db_failure_publishes_nothing() {
    // Setup database with an open vote on a pull request that cannot be updated
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            is_pull_request: true,
            ..Default::default()
        })
        .await;
    let vote = test_db.read_vote(vote_id).await;
    test_db.fail_vote_operation("update").await;

    // Setup GitHub expectations: only the failure comment is posted, so no success check
    // can exist while the vote is open
    let mut gh = MockGH::new();
    expect_user_is_collaborator(&mut gh, true);
    expect_command_failed_comment(&mut gh, || Ok(COMMENT_ID2));
    gh.expect_create_check_run().never();
    gh.expect_remove_label().never();

    // Try cancelling the vote
    let input = CancelVoteInput::new(&Event::PullRequest(setup_test_pr_event()));
    setup_real_db_cmds_handler(&test_db, gh).process_command(Command::CancelVote(input)).await;

    // Check the vote is still open and did not change
    assert_eq!(test_db.read_vote(vote_id).await, vote);
    assert!(db.has_vote_open(REPOFN, ISSUE_NUM).await.unwrap());
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_cancel_vote_failed_delete_is_retried_by_the_closer() {
    // Setup database with an open vote that cannot be deleted
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db.seed_vote(SeedVote::default()).await;
    test_db.fail_vote_operation("delete").await;

    // Setup GitHub expectations: the cancellation is published, and the failure to
    // delete the vote does not ask for the command to be tried again
    let mut gh = MockGH::new();
    expect_user_is_collaborator(&mut gh, true);
    expect_comment(&mut gh, vote_cancelled_body(vote_id, false));
    expect_command_failed_comment_never(&mut gh);
    expect_remove_open_label(&mut gh, || Ok(()));

    // Cancel the vote
    let input = CancelVoteInput::new(&Event::IssueComment(setup_test_issue_comment_event()));
    setup_real_db_cmds_handler(&test_db, gh).process_command(Command::CancelVote(input)).await;

    // Check the cancellation is published and the failure deleting the vote recorded
    let vote = test_db.read_vote(vote_id).await.unwrap();
    assert_eq!(vote.close_attempts, 1);
    assert!(vote.close_pending);
    assert!(vote.closed);
    assert_eq!(vote.cancelled_by.as_deref(), Some(USER));
    assert!(vote.close_comment_posted_at.is_some());
    assert!(vote.close_labels_updated_at.is_some());
    assert_eq!(db.get_vote(vote_id).await.unwrap(), None);

    // Check the retry publishes nothing again (the mock rejects any call), failing to
    // delete the vote once more
    test_db.expire_close_claims().await;
    let votes_closer = setup_real_db_votes_closer(&test_db, MockGH::new());
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
    assert_eq!(test_db.read_vote(vote_id).await.unwrap().close_attempts, 2);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_cancel_vote_new_vote_supersedes_pending_cancellation_state() {
    // Setup database with an open vote on a pull request
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            is_pull_request: true,
            ..Default::default()
        })
        .await;

    // Setup GitHub expectations: the comment and the check run of the cancellation fail
    let mut gh = MockGH::new();
    expect_user_is_collaborator(&mut gh, true);
    expect_vote_cancelled_comment_fails(&mut gh, vote_id, true);
    expect_command_failed_comment_never(&mut gh);
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

    // Cancel the vote, leaving its comment and check run pending
    let input = CancelVoteInput::new(&Event::PullRequest(setup_test_pr_event()));
    setup_real_db_cmds_handler(&test_db, gh).process_command(Command::CancelVote(input)).await;
    let vote = test_db.read_vote(vote_id).await.unwrap();
    assert_eq!(vote.close_check_run_created_at, None);
    assert_eq!(vote.close_comment_posted_at, None);

    // Create a new vote while the cancellation is pending
    let input = CreateVoteInput::new(None, &Event::PullRequest(setup_test_pr_event()));
    let new_vote_id = db.store_vote(COMMENT_ID2, &input, &setup_test_vote().cfg).await.unwrap();

    // Check the new vote supersedes the pending check run but not the comment
    let vote = test_db.read_vote(vote_id).await.unwrap();
    assert!(vote.close_check_run_created_at.is_some());
    assert_eq!(vote.close_comment_posted_at, None);

    // Setup GitHub expectations of the retry: only the comment is posted, once it is
    // known that the first attempt did not post it
    let since = lookup_since(vote.closed_at.unwrap());
    let mut gh = MockGH::new();
    expect_comment_lookup(
        &mut gh,
        tmpl::VoteMarker::CancellationComment,
        vote_id,
        since,
        || Ok(false),
    );
    expect_comment(&mut gh, vote_cancelled_body(vote_id, true));
    gh.expect_create_check_run().never();
    gh.expect_remove_label().never();

    // Run the retry
    test_db.expire_close_claims().await;
    let votes_closer = setup_real_db_votes_closer(&test_db, gh);
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));

    // Check the cancelled vote is gone and the new vote is still open
    assert_eq!(test_db.read_vote(vote_id).await, None);
    let open_vote = db.get_open_vote(REPOFN, ISSUE_NUM).await.unwrap().unwrap();
    assert_eq!(open_vote.vote_id, new_vote_id);
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), None);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_cancel_vote_publish_failure_is_retried_by_the_closer() {
    // Setup cases: outcome of posting the comment and creating the check run the first
    // time, as well as the number of times each one is retried
    type CommentOutcome = fn() -> Result<i64>;
    type CheckRunOutcome = fn() -> Result<()>;
    let cases: [(CommentOutcome, CheckRunOutcome, usize, usize); 2] = [
        (|| Err(format_err!(ERROR)), || Ok(()), 1, 0),
        (|| Ok(COMMENT_ID), || Err(format_err!(ERROR)), 0, 1),
    ];

    for (comment_result, check_run_result, comment_retries, check_run_retries) in cases {
        // Setup database with an open vote on a pull request
        let test_db = TestDB::new().await;
        let db = test_db.db();
        let vote_id = test_db
            .seed_vote(SeedVote {
                is_pull_request: true,
                ..Default::default()
            })
            .await;

        // Setup GitHub expectations: the failure does not ask for the command to be
        // tried again, as the vote is cancelled already
        let mut gh = MockGH::new();
        expect_user_is_collaborator(&mut gh, true);
        expect_comment_in_sequence(
            &mut gh,
            &mut Sequence::new(),
            vote_cancelled_body(vote_id, true),
            comment_result,
        );
        expect_command_failed_comment_never(&mut gh);
        gh.expect_create_check_run()
            .with(
                eq(INST_ID),
                eq(ORG),
                eq(REPO),
                eq(ISSUE_NUM),
                eq(cancelled_check_details()),
            )
            .times(1)
            .returning(move |_, _, _, _, _| Box::pin(future::ready(check_run_result())));
        expect_remove_open_label(&mut gh, || Ok(()));

        // Cancel the vote
        let input = CancelVoteInput::new(&Event::PullRequest(setup_test_pr_event()));
        setup_real_db_cmds_handler(&test_db, gh).process_command(Command::CancelVote(input)).await;

        // Check the vote is cancelled, with the steps done and the failure recorded
        let vote = test_db.read_vote(vote_id).await.unwrap();
        assert_eq!(vote.close_attempts, 1);
        assert!(vote.close_pending);
        assert!(vote.closed);
        assert_eq!(vote.cancelled_by.as_deref(), Some(USER));
        assert_eq!(vote.close_check_run_created_at.is_some(), check_run_retries == 0);
        assert_eq!(vote.close_comment_posted_at.is_some(), comment_retries == 0);
        assert!(vote.close_labels_updated_at.is_some());

        // Check the vote is not open nor listed, and not retried before the backoff ends
        assert_eq!(db.get_open_vote(REPOFN, ISSUE_NUM).await.unwrap(), None);
        assert_eq!(db.get_vote(vote_id).await.unwrap(), None);
        assert_eq!(db.list_votes(REPOFN).await.unwrap(), vec![]);
        assert_eq!(db.claim_vote_to_close().await.unwrap(), None);

        // Setup GitHub expectations of the retry: only the missing steps are run, and
        // the comment is only posted once it is known that the first attempt did not
        // post it
        let expected_marker = tmpl::VoteMarker::CancellationComment.render(vote_id);
        let since = lookup_since(vote.closed_at.unwrap());
        let mut gh = MockGH::new();
        gh.expect_has_comment_with_marker()
            .withf(move |inst_id, owner, repo, issue_number, marker, lookup_since| {
                *inst_id == INST_ID
                    && owner == ORG
                    && repo == REPO
                    && *issue_number == ISSUE_NUM
                    && marker == expected_marker
                    && since.contains(lookup_since)
            })
            .times(comment_retries)
            .returning(|_, _, _, _, _, _| Box::pin(future::ready(Ok(false))));
        let expected_body = vote_cancelled_body(vote_id, true);
        gh.expect_post_comment()
            .withf(move |inst_id, owner, repo, issue_number, body| {
                *inst_id == INST_ID
                    && owner == ORG
                    && repo == REPO
                    && *issue_number == ISSUE_NUM
                    && body == expected_body
            })
            .times(comment_retries)
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
        gh.expect_create_check_run()
            .with(
                eq(INST_ID),
                eq(ORG),
                eq(REPO),
                eq(ISSUE_NUM),
                eq(cancelled_check_details()),
            )
            .times(check_run_retries)
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
        gh.expect_remove_label().never();

        // Run the retry and check the vote is deleted once the cancellation is published
        test_db.expire_close_claims().await;
        let votes_closer = setup_real_db_votes_closer(&test_db, gh);
        assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
        assert_eq!(test_db.read_vote(vote_id).await, None);
        assert_eq!(votes_closer.close_finished_vote().await.unwrap(), None);
    }
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_cancel_vote_publishes_once_closed_and_deletes_the_vote() {
    // Setup database with an open vote on a pull request
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            is_pull_request: true,
            ..Default::default()
        })
        .await;

    // Setup GitHub expectations: the success check is created once the vote is closed
    let check_db = Arc::new(test_db.db());
    let mut gh = MockGH::new();
    expect_user_is_collaborator(&mut gh, true);
    expect_comment(&mut gh, vote_cancelled_body(vote_id, true));
    gh.expect_create_check_run()
        .with(
            eq(INST_ID),
            eq(ORG),
            eq(REPO),
            eq(ISSUE_NUM),
            eq(cancelled_check_details()),
        )
        .times(1)
        .returning(move |_, _, _, _, _| {
            let check_db = check_db.clone();
            Box::pin(async move {
                assert!(!check_db.has_vote_open(REPOFN, ISSUE_NUM).await.unwrap());
                Ok(())
            })
        });
    expect_remove_open_label(&mut gh, || Ok(()));

    // Cancel the vote
    let input = CancelVoteInput::new(&Event::PullRequest(setup_test_pr_event()));
    setup_real_db_cmds_handler(&test_db, gh).cancel_vote(&input).await.unwrap();

    // Check the vote was deleted and nothing is left for the votes closer
    assert_eq!(test_db.read_vote(vote_id).await, None);
    assert!(!db.has_vote(REPOFN, ISSUE_NUM).await.unwrap());
    test_db.expire_close_claims().await;
    let votes_closer = setup_real_db_votes_closer(&test_db, MockGH::new());
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), None);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_close_finished_vote_announcement_failure_is_retried() {
    // Setup database with a finished vote
    let started_at = OffsetDateTime::now_utc();
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db.seed_vote(seed_finished_vote()).await;

    // Setup GitHub expectations: the announcement fails, just the first time, and the
    // retry looks it up before creating it again
    let since = lookup_since_closed_after(started_at);
    let mut gh = MockGH::new();
    let mut seq = Sequence::new();
    expect_vote_results(&mut gh, COMMENT_ID, Ok(vec![in_favor_reaction(USER1)]));
    expect_comment(&mut gh, vote_closed_body(vote_id));
    expect_announcement_discussion(&mut gh, &mut seq, vote_id, || Err(format_err!(ERROR)));
    expect_remove_open_label(&mut gh, || Ok(()));
    expect_close_labels(&mut gh, &mut seq, &[VOTE_CLOSED_LABEL, VOTE_PASSED_LABEL], || {
        Ok(())
    });
    expect_announcement_lookup(&mut gh, vote_id, since, || Ok(false));
    expect_announcement_discussion(&mut gh, &mut seq, vote_id, || Ok(()));

    // Try closing the vote, checking the attempt failed with the rest done
    let votes_closer = setup_real_db_votes_closer(&test_db, gh);
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
    let vote = db.get_vote(vote_id).await.unwrap().unwrap();
    assert_eq!(vote.close_attempts, 1);
    assert!(vote.close_pending);
    assert_eq!(vote.close_announcement_posted_at, None);
    assert!(vote.close_comment_posted_at.is_some());
    assert!(vote.close_labels_updated_at.is_some());

    // Run the retry (the mock rejects repeating any of the completed steps)
    test_db.expire_close_claims().await;
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));

    // Check nothing else is pending
    let vote = db.get_vote(vote_id).await.unwrap().unwrap();
    assert!(!vote.close_pending);
    assert!(vote.close_announcement_posted_at.is_some());
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), None);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_close_finished_vote_announcement_rejected_completes_the_close() {
    // Setup database with a finished vote
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db.seed_vote(seed_finished_vote()).await;

    // Setup GitHub expectations: the announcement is rejected
    let mut gh = MockGH::new();
    expect_vote_results(&mut gh, COMMENT_ID, Ok(vec![in_favor_reaction(USER1)]));
    expect_comment(&mut gh, vote_closed_body(vote_id));
    expect_announcement_discussion(&mut gh, &mut Sequence::new(), vote_id, || {
        Err(format_err!(DiscussionRejectedError(format_err!(ERROR))))
    });
    expect_remove_open_label(&mut gh, || Ok(()));
    expect_close_labels(
        &mut gh,
        &mut Sequence::new(),
        &[VOTE_CLOSED_LABEL, VOTE_PASSED_LABEL],
        || Ok(()),
    );

    // Close the vote
    let votes_closer = setup_real_db_votes_closer(&test_db, gh);
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));

    // Check the close is complete and there is nothing to retry
    let vote = db.get_vote(vote_id).await.unwrap().unwrap();
    assert_eq!(vote.close_attempts, 0);
    assert!(!vote.close_pending);
    assert!(vote.close_announcement_posted_at.is_some());
    test_db.expire_close_claims().await;
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), None);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_close_finished_vote_cancellation_last_attempt_is_abandoned() {
    // Setup database with a cancelled pull request vote on its last attempt to publish
    // the cancellation, with all its steps pending
    let started_at = OffsetDateTime::now_utc();
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            close_attempts: CLOSE_MAX_ATTEMPTS - 1,
            close_pending: true,
            closed: true,
            is_pull_request: true,

            cancelled_ago: Some("1 day"),
            close_next_attempt_in: Some("-1 minute"),
            ..Default::default()
        })
        .await;
    assert!(db.has_vote(REPOFN, ISSUE_NUM).await.unwrap());

    // Setup GitHub expectations: the comment is not found, and posting it and the check
    // run fail
    let since = lookup_since_closed_after(started_at);
    let mut gh = MockGH::new();
    expect_comment_lookup(
        &mut gh,
        tmpl::VoteMarker::CancellationComment,
        vote_id,
        since,
        || Ok(false),
    );
    expect_vote_cancelled_comment_fails(&mut gh, vote_id, true);
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

    // Run the last attempt
    let votes_closer = setup_real_db_votes_closer(&test_db, gh);
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));

    // Check the cancellation is abandoned, deleting the vote, so the pull request has
    // no vote that owns its check and nothing else is retried
    assert_eq!(test_db.read_vote(vote_id).await, None);
    assert!(!db.has_vote(REPOFN, ISSUE_NUM).await.unwrap());
    test_db.expire_close_claims().await;
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), None);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_close_finished_vote_cancelled_while_closing_publishes_nothing() {
    // Setup database with a finished vote
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db.seed_vote(seed_finished_vote()).await;

    // Setup GitHub expectations: nothing is published once the vote has been cancelled
    let mut gh = MockGH::new();
    let (calculating_rx, release_tx) = expect_held_vote_results(&mut gh);
    gh.expect_post_comment().never();
    gh.expect_create_discussion().never();
    gh.expect_remove_label().never();
    gh.expect_add_labels().never();

    // Start closing the vote and wait until its results are being calculated
    let votes_closer = setup_real_db_votes_closer(&test_db, gh);
    let closing = tokio::spawn(async move { votes_closer.close_finished_vote().await });
    calculating_rx.await.unwrap();

    // Cancel the vote before the results are ready
    let cancelled_vote = db.cancel_vote(REPOFN, ISSUE_NUM, USER).await.unwrap().unwrap();
    assert_eq!(cancelled_vote.vote_id, vote_id);

    // Check the closer finishes leaving the cancelled vote as it was
    release_tx.send(()).unwrap();
    assert_eq!(closing.await.unwrap().unwrap(), Some(()));
    assert_eq!(test_db.read_vote(vote_id).await, Some(cancelled_vote));
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_close_finished_vote_cancelled_while_failing_keeps_the_cancellation_claim() {
    // Setup database with a finished vote
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db.seed_vote(seed_finished_vote()).await;

    // Setup GitHub expectations: the results calculation fails once released
    let mut gh = MockGH::new();
    let (calculating_tx, calculating_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel::<()>();
    gh.expect_get_comment_reactions()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(COMMENT_ID))
        .times(1)
        .return_once(move |_, _, _, _| {
            calculating_tx.send(()).unwrap();
            Box::pin(async move {
                release_rx.await.unwrap();
                Err(format_err!(ERROR))
            })
        });
    gh.expect_post_comment().never();

    // Start closing the vote and wait until its results are being calculated
    let votes_closer = setup_real_db_votes_closer(&test_db, gh);
    let closing = tokio::spawn(async move { votes_closer.close_finished_vote().await });
    calculating_rx.await.unwrap();

    // Cancel the vote before the results calculation fails
    let cancelled_vote = db.cancel_vote(REPOFN, ISSUE_NUM, USER).await.unwrap().unwrap();
    assert_eq!(cancelled_vote.vote_id, vote_id);

    // Check the failure of the closer leaves the claim of the cancellation as it was
    release_tx.send(()).unwrap();
    assert_eq!(closing.await.unwrap().unwrap(), Some(()));
    assert_eq!(test_db.read_vote(vote_id).await, Some(cancelled_vote));
    assert_eq!(db.claim_vote_to_close().await.unwrap(), None);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_close_finished_vote_closed_with_unknown_head_completes() {
    // Setup database with a vote closed without knowing the head of its pull request
    let started_at = OffsetDateTime::now_utc();
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            close_pending: true,
            closed: true,
            is_pull_request: true,

            results: Some(serde_json::to_value(setup_test_vote_results()).unwrap()),
            ..seed_finished_vote()
        })
        .await;

    // Setup GitHub expectations: the results published by a previous attempt are looked
    // up, and the check run is created on the current head
    let since = lookup_since_closed_after(started_at);
    let mut gh = MockGH::new();
    expect_comment_lookup(
        &mut gh,
        tmpl::VoteMarker::ResultsComment,
        vote_id,
        since.clone(),
        || Ok(false),
    );
    expect_announcement_lookup(&mut gh, vote_id, since, || Ok(false));
    expect_vote_closed_published(&mut gh, vote_id);
    gh.expect_get_pr_head_sha().never();
    gh.expect_create_commit_check_run().never();
    expect_check_run(&mut gh, passed_check_details());

    // Run the pending steps
    let votes_closer = setup_real_db_votes_closer(&test_db, gh);
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));

    // Check the close is complete
    let vote = db.get_vote(vote_id).await.unwrap().unwrap();
    assert!(!vote.close_pending);
    assert!(vote.close_check_run_created_at.is_some());
    assert_eq!(vote.close_head_sha, None);
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), None);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_close_finished_vote_comment_deleted_closes_vote_without_results() {
    // Setup database with a finished vote
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db.seed_vote(seed_finished_vote()).await;

    // Setup GitHub expectations: the vote comment is gone, so only labels are updated
    let mut gh = MockGH::new();
    gh.expect_get_comment_reactions()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(COMMENT_ID))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Err(CommentNotFoundError.into()))));
    gh.expect_post_comment().never();
    expect_remove_open_label(&mut gh, || Ok(()));
    expect_close_labels(&mut gh, &mut Sequence::new(), &[VOTE_CLOSED_LABEL], || Ok(()));

    // Close the vote
    let votes_closer = setup_real_db_votes_closer(&test_db, gh);
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));

    // Check it can be read back, closed and without results
    let vote = db.get_vote(vote_id).await.unwrap().unwrap();
    assert!(!vote.close_pending);
    assert!(vote.closed);
    assert!(vote.closed_at.is_some());
    assert_eq!(vote.results, None);
    assert_eq!(db.list_votes(REPOFN).await.unwrap(), vec![vote]);

    // Check there is nothing else to do
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), None);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_close_finished_vote_comment_deleted_on_pr_reports_failure_check() {
    // Setup database with a finished vote on a pull request
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            is_pull_request: true,
            ..seed_finished_vote()
        })
        .await;

    // Setup GitHub expectations: the vote comment is gone, so a failure check is created
    // on the head of the pull request
    let mut gh = MockGH::new();
    gh.expect_get_comment_reactions()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(COMMENT_ID))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Err(CommentNotFoundError.into()))));
    expect_pr_head_sha(&mut gh, HEAD_SHA);
    expect_commit_check_run(
        &mut gh,
        HEAD_SHA,
        CheckDetails {
            status: "completed".to_string(),
            summary: CHECK_SUMMARY_NO_RESULTS.to_string(),

            conclusion: Some("failure".to_string()),
        },
    );
    expect_remove_open_label(&mut gh, || Ok(()));
    expect_close_labels(&mut gh, &mut Sequence::new(), &[VOTE_CLOSED_LABEL], || Ok(()));

    // Close the vote
    let votes_closer = setup_real_db_votes_closer(&test_db, gh);
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));

    // Check it is closed without results
    let vote = db.get_vote(vote_id).await.unwrap().unwrap();
    assert!(!vote.close_pending);
    assert!(vote.closed);
    assert_eq!(vote.close_head_sha.as_deref(), Some(HEAD_SHA));
    assert_eq!(vote.results, None);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_close_finished_vote_concurrent_closers_close_the_vote_once() {
    // Setup database with a finished vote
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db.seed_vote(seed_finished_vote()).await;

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    let (calculating_rx, release_tx) = expect_held_vote_results(&mut gh);
    expect_vote_closed_published(&mut gh, vote_id);

    // Start closing the vote and wait until its results are being calculated
    let votes_closer = setup_real_db_votes_closer(&test_db, gh);
    let closing = tokio::spawn(async move { votes_closer.close_finished_vote().await });
    calculating_rx.await.unwrap();

    // Check another closer does not get the vote (its mock rejects all the calls)
    let other_votes_closer = setup_real_db_votes_closer(&test_db, MockGH::new());
    assert_eq!(other_votes_closer.close_finished_vote().await.unwrap(), None);

    // Check the first closer completes the close
    release_tx.send(()).unwrap();
    assert_eq!(closing.await.unwrap().unwrap(), Some(()));
    let vote = db.get_vote(vote_id).await.unwrap().unwrap();
    assert!(!vote.close_pending);
    assert!(vote.closed);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_close_finished_vote_github_error_keeps_vote_open_until_retried() {
    // Setup database with a finished vote
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db.seed_vote(seed_finished_vote()).await;

    // Setup GitHub expectations: the results can only be calculated the second time
    let mut gh = MockGH::new();
    expect_vote_results_fail_once(&mut gh);
    expect_vote_closed_published(&mut gh, vote_id);

    // Try closing the vote, checking the failure leaves it open and recorded
    let votes_closer = setup_real_db_votes_closer(&test_db, gh);
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
    let vote = db.get_vote(vote_id).await.unwrap().unwrap();
    assert_eq!(vote.close_attempts, 1);
    assert!(!vote.closed);
    assert_eq!(vote.closed_at, None);
    assert_eq!(vote.results, None);

    // Check it is not retried until the delay is over
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), None);

    // Check the retry closes it, storing the results calculated
    test_db.expire_close_claims().await;
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
    let vote = db.get_vote(vote_id).await.unwrap().unwrap();
    assert!(!vote.close_pending);
    assert!(vote.closed);
    assert!(vote.closed_at.is_some());
    assert_eq!(vote.results, Some(setup_test_vote_results()));

    // Check there is nothing else to do
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), None);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_close_finished_vote_moved_end_ignores_later_reactions() {
    // Setup database with an open vote that is closed early
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            ends_in: "10 days",
            ..seed_finished_vote()
        })
        .await;
    db.update_vote_ends_at(vote_id).await.unwrap();

    // Setup GitHub expectations: the reaction in favor is the only one in time, as the
    // other one was added after the new end of the vote and before the original one
    let late_reaction = Reaction {
        created_at: (OffsetDateTime::now_utc() + 1.hours()).format(&Rfc3339).unwrap(),
        ..in_favor_reaction(USER2)
    };
    let mut gh = MockGH::new();
    let reactions = vec![in_favor_reaction(USER1), late_reaction];
    expect_vote_results(&mut gh, COMMENT_ID, Ok(reactions));
    expect_vote_closed_published(&mut gh, vote_id);

    // Close the vote
    let votes_closer = setup_real_db_votes_closer(&test_db, gh);
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));

    // Check the results only include the reaction in time
    let vote = db.get_vote(vote_id).await.unwrap().unwrap();
    assert_eq!(vote.results, Some(setup_test_vote_results()));
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_close_finished_vote_newer_vote_cancelled_while_closing_publishes_no_state() {
    // Setup database with a finished vote on a pull request
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            is_pull_request: true,
            ..seed_finished_vote()
        })
        .await;

    // Setup GitHub expectations: the results comment is held until released, and no
    // check run or labels are published after that
    let mut gh = MockGH::new();
    expect_vote_results(&mut gh, COMMENT_ID, Ok(vec![in_favor_reaction(USER1)]));
    expect_pr_head_sha(&mut gh, HEAD_SHA);
    let (posting_tx, posting_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel::<()>();
    gh.expect_post_comment()
        .with(
            eq(INST_ID),
            eq(ORG),
            eq(REPO),
            eq(ISSUE_NUM),
            eq(vote_closed_body(vote_id)),
        )
        .times(1)
        .return_once(move |_, _, _, _, _| {
            posting_tx.send(()).unwrap();
            Box::pin(async move {
                release_rx.await.unwrap();
                Ok(COMMENT_ID)
            })
        });
    expect_announcement_discussion(&mut gh, &mut Sequence::new(), vote_id, || Ok(()));
    gh.expect_create_commit_check_run().never();
    gh.expect_create_check_run().never();
    gh.expect_remove_label().never();
    gh.expect_add_labels().never();

    // Start closing the vote and wait until its results comment is being posted
    let votes_closer = setup_real_db_votes_closer(&test_db, gh);
    let closing = tokio::spawn(async move { votes_closer.close_finished_vote().await });
    posting_rx.await.unwrap();

    // Create and cancel a newer vote, deleting it as if its cancellation was published
    let input = CreateVoteInput::new(None, &Event::PullRequest(setup_test_pr_event()));
    let newer_vote_id = db.store_vote(COMMENT_ID2, &input, &setup_test_vote().cfg).await.unwrap();
    db.cancel_vote(REPOFN, ISSUE_NUM, USER).await.unwrap().unwrap();
    db.complete_vote_cancellation(newer_vote_id).await.unwrap();
    assert_eq!(test_db.read_vote(newer_vote_id).await, None);

    // Check the close completes without publishing the state superseded
    release_tx.send(()).unwrap();
    assert_eq!(closing.await.unwrap().unwrap(), Some(()));
    let vote = db.get_vote(vote_id).await.unwrap().unwrap();
    assert!(!vote.close_pending);
    assert!(vote.close_announcement_posted_at.is_some());
    assert!(vote.close_check_run_created_at.is_some());
    assert!(vote.close_comment_posted_at.is_some());
    assert!(vote.close_labels_updated_at.is_some());
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_close_finished_vote_not_found_error_keeps_vote_open() {
    // Setup database with a finished vote
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db.seed_vote(seed_finished_vote()).await;

    // Setup GitHub expectations: a 404 that does not say the comment was deleted, as
    // the one returned when the installation token cannot be created
    let not_found_error = setup_test_not_found_error().await;
    let mut gh = MockGH::new();
    gh.expect_get_comment_reactions()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(COMMENT_ID))
        .times(1)
        .return_once(move |_, _, _, _| Box::pin(future::ready(Err(not_found_error))));
    gh.expect_post_comment().never();

    // Try closing the vote
    let votes_closer = setup_real_db_votes_closer(&test_db, gh);
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));

    // Check the vote stays open with the failure recorded, so that it is retried
    let vote = db.get_vote(vote_id).await.unwrap().unwrap();
    assert_eq!(vote.close_attempts, 1);
    assert!(!vote.closed);
    assert_eq!(vote.closed_at, None);
    assert_eq!(vote.results, None);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_close_finished_vote_open_vote_keeps_retrying_after_the_attempts_limit() {
    // Setup database with a finished vote that failed all the attempts of the limit
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            close_attempts: CLOSE_MAX_ATTEMPTS,
            ..seed_finished_vote()
        })
        .await;

    // Setup GitHub expectations: the results can only be calculated the second time
    let mut gh = MockGH::new();
    expect_vote_results_fail_once(&mut gh);
    expect_vote_closed_published(&mut gh, vote_id);

    // Try closing the vote, checking the failure leaves it open with a retry scheduled
    let votes_closer = setup_real_db_votes_closer(&test_db, gh);
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
    let vote = db.get_vote(vote_id).await.unwrap().unwrap();
    assert_eq!(vote.close_attempts, CLOSE_MAX_ATTEMPTS + 1);
    assert!(!vote.closed);
    assert_eq!(vote.results, None);
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), None);

    // Check the retry closes it, restarting its attempts, and publishes the results
    test_db.expire_close_claims().await;
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
    let vote = db.get_vote(vote_id).await.unwrap().unwrap();
    assert_eq!(vote.close_attempts, 0);
    assert!(!vote.close_pending);
    assert!(vote.closed);
    assert_eq!(vote.results, Some(setup_test_vote_results()));

    // Check there is nothing else to do
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), None);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_close_finished_vote_retry_after_newer_vote_cancelled_publishes_no_state() {
    // Setup database with a finished vote on a pull request
    let started_at = OffsetDateTime::now_utc();
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            is_pull_request: true,
            ..seed_finished_vote()
        })
        .await;

    // Setup GitHub expectations: all the steps fail the first time, the retry looks up
    // the results before posting them, and no check run or labels can be published by it
    let since = lookup_since_closed_after(started_at);
    let mut gh = MockGH::new();
    let mut seq = Sequence::new();
    expect_vote_results(&mut gh, COMMENT_ID, Ok(vec![in_favor_reaction(USER1)]));
    expect_pr_head_sha(&mut gh, HEAD_SHA);
    expect_close_steps_fail_once(&mut gh, &mut seq, vote_id);
    expect_comment_lookup(
        &mut gh,
        tmpl::VoteMarker::ResultsComment,
        vote_id,
        since.clone(),
        || Ok(false),
    );
    expect_announcement_lookup(&mut gh, vote_id, since, || Ok(false));
    expect_comment_in_sequence(&mut gh, &mut seq, vote_closed_body(vote_id), || Ok(COMMENT_ID));
    expect_announcement_discussion(&mut gh, &mut seq, vote_id, || Ok(()));

    // Try closing the vote, which fails
    let votes_closer = setup_real_db_votes_closer(&test_db, gh);
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));

    // Create and cancel a newer vote, deleting it as if its cancellation was published
    let input = CreateVoteInput::new(None, &Event::PullRequest(setup_test_pr_event()));
    let newer_vote_id = db.store_vote(COMMENT_ID2, &input, &setup_test_vote().cfg).await.unwrap();
    db.cancel_vote(REPOFN, ISSUE_NUM, USER).await.unwrap().unwrap();
    db.complete_vote_cancellation(newer_vote_id).await.unwrap();
    assert_eq!(test_db.read_vote(newer_vote_id).await, None);

    // Run the retry once the newer vote is gone
    test_db.expire_close_claims().await;
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));

    // Check the close is complete, with the steps superseded recorded as done
    let vote = db.get_vote(vote_id).await.unwrap().unwrap();
    assert!(!vote.close_pending);
    assert!(vote.close_announcement_posted_at.is_some());
    assert!(vote.close_check_run_created_at.is_some());
    assert!(vote.close_comment_posted_at.is_some());
    assert!(vote.close_labels_updated_at.is_some());
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), None);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_close_finished_vote_retry_only_runs_missing_steps() {
    // Setup database with a finished vote on a pull request
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            is_pull_request: true,
            ..seed_finished_vote()
        })
        .await;

    // Setup GitHub expectations: only the check run fails, and just the first time. The
    // head is read only once, so the retry cannot land on a commit pushed since then
    let mut seq = Sequence::new();
    let mut gh = MockGH::new();
    expect_vote_results(&mut gh, COMMENT_ID, Ok(vec![in_favor_reaction(USER1)]));
    expect_pr_head_sha(&mut gh, HEAD_SHA);
    expect_vote_closed_published(&mut gh, vote_id);
    gh.expect_create_check_run().never();
    gh.expect_create_commit_check_run()
        .with(
            eq(INST_ID),
            eq(ORG),
            eq(REPO),
            eq(HEAD_SHA),
            eq(passed_check_details()),
        )
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    gh.expect_create_commit_check_run()
        .with(
            eq(INST_ID),
            eq(ORG),
            eq(REPO),
            eq(HEAD_SHA),
            eq(passed_check_details()),
        )
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));

    // Try closing the vote, checking what was done is recorded
    let votes_closer = setup_real_db_votes_closer(&test_db, gh);
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
    let vote = db.get_vote(vote_id).await.unwrap().unwrap();
    assert_eq!(vote.close_attempts, 1);
    assert!(vote.close_pending);
    assert!(vote.closed);
    assert!(vote.close_announcement_posted_at.is_some());
    assert_eq!(vote.close_check_run_created_at, None);
    assert!(vote.close_comment_posted_at.is_some());
    assert_eq!(vote.close_head_sha.as_deref(), Some(HEAD_SHA));
    assert!(vote.close_labels_updated_at.is_some());
    assert_eq!(vote.results, Some(setup_test_vote_results()));

    // Run the retry (the mock rejects repeating any of the completed steps)
    test_db.expire_close_claims().await;
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));

    // Check nothing else is pending
    let vote = db.get_vote(vote_id).await.unwrap().unwrap();
    assert!(!vote.close_pending);
    assert!(vote.close_check_run_created_at.is_some());
    assert!(vote.close_labels_updated_at.is_some());
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), None);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_close_finished_vote_retry_with_closed_newer_vote_publishes_no_state() {
    run_close_retry_with_newer_vote(SeedVote {
        closed: true,
        created_ago: "1 minute",
        is_pull_request: true,
        ..Default::default()
    })
    .await;
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_close_finished_vote_retry_with_open_newer_vote_publishes_no_state() {
    run_close_retry_with_newer_vote(SeedVote {
        created_ago: "1 minute",
        is_pull_request: true,
        ..Default::default()
    })
    .await;
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_close_finished_vote_retry_without_newer_vote_publishes_state() {
    // Setup database with a finished vote on a pull request and an older one
    let started_at = OffsetDateTime::now_utc();
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            is_pull_request: true,
            ..seed_finished_vote()
        })
        .await;
    test_db
        .seed_vote(SeedVote {
            closed: true,
            created_ago: "3 days",
            is_pull_request: true,
            ..Default::default()
        })
        .await;

    // Setup GitHub expectations: all the steps fail the first time, and the retry looks
    // up the results before posting them
    let since = lookup_since_closed_after(started_at);
    let mut gh = MockGH::new();
    let mut seq = Sequence::new();
    expect_vote_results(&mut gh, COMMENT_ID, Ok(vec![in_favor_reaction(USER1)]));
    expect_pr_head_sha(&mut gh, HEAD_SHA);
    expect_close_steps_fail_once(&mut gh, &mut seq, vote_id);
    expect_comment_lookup(
        &mut gh,
        tmpl::VoteMarker::ResultsComment,
        vote_id,
        since.clone(),
        || Ok(false),
    );
    expect_announcement_lookup(&mut gh, vote_id, since, || Ok(false));
    expect_comment_in_sequence(&mut gh, &mut seq, vote_closed_body(vote_id), || Ok(COMMENT_ID));
    expect_announcement_discussion(&mut gh, &mut seq, vote_id, || Ok(()));
    gh.expect_create_commit_check_run()
        .with(
            eq(INST_ID),
            eq(ORG),
            eq(REPO),
            eq(HEAD_SHA),
            eq(passed_check_details()),
        )
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
    expect_remove_open_label(&mut gh, || Ok(()));
    expect_close_labels(&mut gh, &mut seq, &[VOTE_CLOSED_LABEL, VOTE_PASSED_LABEL], || {
        Ok(())
    });

    // Try closing the vote, which fails
    let votes_closer = setup_real_db_votes_closer(&test_db, gh);
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));

    // Check the retry publishes the state of the vote, the latest one in the issue/pr
    test_db.expire_close_claims().await;
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
    let vote = db.get_vote(vote_id).await.unwrap().unwrap();
    assert!(!vote.close_pending);
    assert!(vote.close_check_run_created_at.is_some());
    assert!(vote.close_labels_updated_at.is_some());
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_close_finished_vote_takeover_records_steps_found_published() {
    // Setup database with a vote closed by a worker that died holding its claim, which
    // expired without a failure being recorded
    let started_at = OffsetDateTime::now_utc();
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            close_pending: true,
            closed: true,

            close_next_attempt_in: Some("-1 minute"),
            results: Some(serde_json::to_value(setup_test_vote_results()).unwrap()),
            ..seed_finished_vote()
        })
        .await;

    // Setup GitHub expectations: the comment and the announcement the dead worker posted
    // are found, so they are not posted again
    let since = lookup_since_closed_after(started_at);
    let mut gh = MockGH::new();
    expect_comment_lookup(
        &mut gh,
        tmpl::VoteMarker::ResultsComment,
        vote_id,
        since.clone(),
        || Ok(true),
    );
    expect_announcement_lookup(&mut gh, vote_id, since, || Ok(true));
    gh.expect_post_comment().never();
    gh.expect_create_discussion().never();
    expect_remove_open_label(&mut gh, || Ok(()));
    expect_close_labels(
        &mut gh,
        &mut Sequence::new(),
        &[VOTE_CLOSED_LABEL, VOTE_PASSED_LABEL],
        || Ok(()),
    );

    // Take over the vote
    let votes_closer = setup_real_db_votes_closer(&test_db, gh);
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));

    // Check the steps found are recorded and the close is complete
    let vote = db.get_vote(vote_id).await.unwrap().unwrap();
    assert_eq!(vote.close_attempts, 0);
    assert!(!vote.close_pending);
    assert!(vote.close_announcement_posted_at.is_some());
    assert!(vote.close_comment_posted_at.is_some());
    assert!(vote.close_labels_updated_at.is_some());
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), None);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_commands_handlers_concurrent_create_vote_stores_one_vote() {
    // Setup database
    let test_db = TestDB::new().await;
    let db = test_db.db();

    // Setup GitHub expectations: both vote comments are posted before storing the votes
    let barrier = Arc::new(Barrier::new(2));
    let deleted_comment_id = Arc::new(AtomicI64::new(0));
    let deleted_comment_id_copy = deleted_comment_id.clone();
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(2)
        .returning(|_, _, _| Box::pin(future::ready(Ok(Some(get_test_valid_config())))));
    gh.expect_user_is_collaborator()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
        .times(2)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(true))));
    for comment_id in [COMMENT_ID, COMMENT_ID + 1] {
        let barrier = barrier.clone();
        let expected_body = vote_created_body(false);
        gh.expect_post_comment()
            .withf(move |inst_id, owner, repo, issue_number, body| {
                *inst_id == INST_ID
                    && owner == ORG
                    && repo == REPO
                    && *issue_number == ISSUE_NUM
                    && body == expected_body
            })
            .times(1)
            .return_once(move |_, _, _, _, _| {
                Box::pin(async move {
                    timeout(WORKER_TIMEOUT, barrier.wait()).await.unwrap();
                    Ok(comment_id)
                })
            });
    }
    gh.expect_delete_comment()
        .withf(|inst_id, owner, repo, _| *inst_id == INST_ID && owner == ORG && repo == REPO)
        .times(1)
        .returning(move |_, _, _, comment_id| {
            deleted_comment_id_copy.store(comment_id, Ordering::SeqCst);
            Box::pin(future::ready(Ok(())))
        });
    expect_vote_in_progress_comment(&mut gh, &mut Sequence::new(), false);
    expect_vote_open_labels(&mut gh);

    // Queue the same command twice
    let (cmds_tx, cmds_rx) = async_channel::unbounded();
    let event = Event::Issue(setup_test_issue_event());
    for _ in 0..2 {
        cmds_tx.send(Command::CreateVote(CreateVoteInput::new(None, &event))).await.unwrap();
    }

    // Run all the commands handlers until the commands have been processed
    let gh: DynGH = Arc::new(gh);
    let cancel_token = CancellationToken::new();
    let handles = (0..COMMANDS_HANDLERS_WORKERS).map(|_| {
        let db: DynDB = Arc::new(test_db.db());
        CommandsHandler::new(db, gh.clone(), cmds_rx.clone()).run(cancel_token.clone())
    });
    let handles: Vec<_> = handles.collect();
    cancel_token.cancel();
    assert!(future::join_all(handles).await.iter().all(Result::is_ok));

    // Check the vote stored tracks the comment that was not deleted
    let votes = db.list_votes(REPOFN).await.unwrap();
    assert_eq!(votes.len(), 1);
    let vote_comment_ids = [
        votes[0].vote_comment_id,
        deleted_comment_id.load(Ordering::SeqCst),
    ];
    assert!(vote_comment_ids.contains(&COMMENT_ID));
    assert!(vote_comment_ids.contains(&(COMMENT_ID + 1)));

    // Check the vote can still be read and cancelled
    let vote = db.get_open_vote(REPOFN, ISSUE_NUM).await.unwrap().unwrap();
    assert_eq!(vote.vote_id, votes[0].vote_id);
    let cancelled_vote = db.cancel_vote(REPOFN, ISSUE_NUM, USER).await.unwrap().unwrap();
    assert_eq!(cancelled_vote.vote_id, vote.vote_id);
}

// Helpers.

/// Expects all the closing steps of the vote provided, which passed, to be tried and
/// fail once, in the sequence given. Removing the vote open label is best effort, so it
/// succeeds.
fn expect_close_steps_fail_once(gh: &mut MockGH, seq: &mut Sequence, vote_id: Uuid) {
    expect_comment_in_sequence(gh, seq, vote_closed_body(vote_id), || Err(format_err!(ERROR)));
    expect_announcement_discussion(gh, seq, vote_id, || Err(format_err!(ERROR)));
    gh.expect_create_commit_check_run()
        .with(
            eq(INST_ID),
            eq(ORG),
            eq(REPO),
            eq(HEAD_SHA),
            eq(passed_check_details()),
        )
        .times(1)
        .in_sequence(seq)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    expect_remove_open_label(gh, || Ok(()));
    expect_close_labels(gh, seq, &[VOTE_CLOSED_LABEL, VOTE_PASSED_LABEL], || {
        Err(format_err!(ERROR))
    });
}

/// Expects the GitHub calls needed to calculate the results of a vote whose allowed
/// voters only include `USER1`, holding them until released. It returns the receiver
/// notified when the calculation starts and the sender to release it.
fn expect_held_vote_results(gh: &mut MockGH) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
    // Expect the reactions to be requested once, holding them until released
    let (calculating_tx, calculating_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel::<()>();
    gh.expect_get_comment_reactions()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(COMMENT_ID))
        .times(1)
        .return_once(move |_, _, _, _| {
            calculating_tx.send(()).unwrap();
            Box::pin(async move {
                release_rx.await.unwrap();
                Ok(vec![in_favor_reaction(USER1)])
            })
        });

    // Expect the allowed voters to be looked up
    expect_allowed_voters(gh);

    (calculating_rx, release_tx)
}

/// Expects getting the reactions of the test vote comment to fail once, and the results
/// of the vote, whose allowed voters only include `USER1`, to be calculated by the next
/// attempt.
fn expect_vote_results_fail_once(gh: &mut MockGH) {
    // Expect getting the reactions to fail the first time only
    let mut seq = Sequence::new();
    gh.expect_get_comment_reactions()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(COMMENT_ID))
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    gh.expect_get_comment_reactions()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(COMMENT_ID))
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(vec![in_favor_reaction(USER1)]))));

    // Expect the allowed voters to be looked up by the attempt that gets the reactions
    expect_allowed_voters(gh);
}

/// Returns the time from which the comments and discussions published by a previous
/// attempt to close or cancel a vote closed at the time provided are looked up.
fn lookup_since(closed_at: OffsetDateTime) -> RangeInclusive<OffsetDateTime> {
    let since = closed_at - MARKER_LOOKUP_MARGIN;
    since..=since
}

/// Returns the range of times from which the comments and discussions published by a
/// previous attempt are looked up, for a vote closed in the test database during a test
/// that started at the time provided.
fn lookup_since_closed_after(started_at: OffsetDateTime) -> RangeInclusive<OffsetDateTime> {
    let since = started_at - MARKER_LOOKUP_MARGIN;
    (since - 1.seconds())..=(since + 1.minutes())
}

/// Runs the retry of a close that failed on all its steps once the vote provided, newer
/// than the one being closed, exists. Only the results must be published by the retry.
async fn run_close_retry_with_newer_vote(newer_vote: SeedVote) {
    // Setup database with a finished vote on a pull request
    let started_at = OffsetDateTime::now_utc();
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            is_pull_request: true,
            ..seed_finished_vote()
        })
        .await;

    // Setup GitHub expectations: all the steps fail the first time, the retry looks up
    // the results before posting them, and no check run or labels can be published by it
    let since = lookup_since_closed_after(started_at);
    let mut gh = MockGH::new();
    let mut seq = Sequence::new();
    expect_vote_results(&mut gh, COMMENT_ID, Ok(vec![in_favor_reaction(USER1)]));
    expect_pr_head_sha(&mut gh, HEAD_SHA);
    expect_close_steps_fail_once(&mut gh, &mut seq, vote_id);
    expect_comment_lookup(
        &mut gh,
        tmpl::VoteMarker::ResultsComment,
        vote_id,
        since.clone(),
        || Ok(false),
    );
    expect_announcement_lookup(&mut gh, vote_id, since, || Ok(false));
    expect_comment_in_sequence(&mut gh, &mut seq, vote_closed_body(vote_id), || Ok(COMMENT_ID));
    expect_announcement_discussion(&mut gh, &mut seq, vote_id, || Ok(()));

    // Try closing the vote, which fails
    let votes_closer = setup_real_db_votes_closer(&test_db, gh);
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));
    let vote = db.get_vote(vote_id).await.unwrap().unwrap();
    assert!(vote.close_pending);
    assert_eq!(vote.close_check_run_created_at, None);
    assert_eq!(vote.close_labels_updated_at, None);

    // Run the retry with the newer vote in place
    let newer_vote_is_closed = newer_vote.closed;
    let newer_vote_id = test_db.seed_vote(newer_vote).await;
    test_db.expire_close_claims().await;
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), Some(()));

    // Check the close is complete, with the steps skipped recorded as done
    let vote = db.get_vote(vote_id).await.unwrap().unwrap();
    assert!(!vote.close_pending);
    assert!(vote.close_announcement_posted_at.is_some());
    assert!(vote.close_check_run_created_at.is_some());
    assert!(vote.close_comment_posted_at.is_some());
    assert!(vote.close_labels_updated_at.is_some());
    assert_eq!(votes_closer.close_finished_vote().await.unwrap(), None);

    // Check the newer vote was not affected
    let newer_vote = db.get_vote(newer_vote_id).await.unwrap().unwrap();
    assert_eq!(newer_vote.closed, newer_vote_is_closed);
}

/// Returns a seed vote ready to be closed, using the configuration of the test vote.
fn seed_finished_vote() -> SeedVote {
    SeedVote {
        cfg: serde_json::to_value(setup_test_vote().cfg).unwrap(),
        ends_in: "-1 minute",
        ..Default::default()
    }
}

/// Sets up a commands handler using the test database and the GitHub mock provided.
fn setup_real_db_cmds_handler(test_db: &TestDB, gh: MockGH) -> CommandsHandler {
    let (_, cmds_rx) = async_channel::unbounded();
    CommandsHandler::new(Arc::new(test_db.db()), Arc::new(gh), cmds_rx)
}

/// Sets up a votes closer using the test database and the GitHub mock provided.
fn setup_real_db_votes_closer(test_db: &TestDB, gh: MockGH) -> VotesCloser {
    VotesCloser::new(Arc::new(test_db.db()), Arc::new(gh))
}
