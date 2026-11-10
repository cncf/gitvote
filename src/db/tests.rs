//! Database contract tests.
//!
//! These tests run the real `PgDB` against a database with the real migrations
//! applied, so they are ignored by default. To run them, point the
//! `GITVOTE_TEST_DB_URL` environment variable at a migrated database and use
//! `cargo test db_contracts -- --ignored`. That database is only used as a
//! template: each test works on its own clone, which is dropped when it ends.

use std::{error::Error as _, time::Duration};

use anyhow::Error;
use futures::future;
use serde_json::{Value, json};
use time::{Duration as TimeDuration, OffsetDateTime};
use tokio::time::timeout;
use uuid::Uuid;

use crate::{
    cfg_repo::{AllowedVoters, Announcements, CfgProfile, DiscussionsAnnouncements, PassThresholdBase},
    cmd::CreateVoteInput,
    testutil::*,
};

use super::*;

/// Cause of the error reading a vote whose stored configuration profile is not valid.
const CFG_DECODE_ERROR: &str = "missing field `duration`";

/// Maximum time to wait for a claim using a pool of one connection, which must reuse the
/// connection it holds instead of waiting for another one.
const CLAIM_TIMEOUT: Duration = Duration::from_secs(5);

/// Time allowed between setting the delay of the next attempt and reading it back.
const CLOSE_RETRY_DELAY_TOLERANCE_SECS: f64 = 5.0;

/// Full name of a repository different from the one used by default in the tests.
const OTHER_REPOFN: &str = "org/other-repo";

/// Cause of the error reading a vote whose stored results are not valid.
const RESULTS_DECODE_ERROR: &str = "missing field `abstain`";

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_cancel_vote_closes_and_claims_the_open_vote() {
    // Setup database with a finished vote that failed to close twice
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            close_attempts: 2,
            ends_in: "-1 minute",
            ..Default::default()
        })
        .await;

    // Cancel the vote
    let vote = db.cancel_vote(REPOFN, ISSUE_NUM, USER1).await.unwrap().unwrap();

    // Check the vote returned is closed and cancelled, with its cancellation pending
    assert_eq!(vote.close_attempts, 0);
    assert!(vote.close_pending);
    assert!(vote.closed);
    assert_eq!(vote.vote_id, vote_id);
    assert_eq!(vote.cancelled_at, vote.closed_at);
    assert_eq!(vote.cancelled_by.as_deref(), Some(USER1));
    assert_eq!(vote.close_announcement_posted_at, None);
    assert_eq!(vote.close_check_run_created_at, None);
    assert_eq!(vote.close_comment_posted_at, None);
    assert_eq!(vote.close_labels_updated_at, None);
    assert!(vote.closed_at.is_some());
    assert_eq!(vote.results, None);
    assert_eq!(test_db.read_vote(vote_id).await, Some(vote.clone()));

    // Check the publication of the cancellation is claimed by the caller
    let (close_attempts, delay) = test_db.close_retry_state().await;
    assert_eq!(close_attempts, 0);
    assert_close_retry_delay(
        delay,
        f64::from(i32::try_from(CLOSE_CLAIM_DURATION_SECS).unwrap()),
    );
    assert_eq!(db.claim_vote_to_close().await.unwrap(), None);

    // Check the votes closer can claim it once the claim expires
    test_db.expire_close_claims().await;
    let claimed_vote = db.claim_vote_to_close().await.unwrap().unwrap();
    assert_eq!(claimed_vote.vote_id, vote_id);
    assert_eq!(claimed_vote.cancelled_by.as_deref(), Some(USER1));
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_cancel_vote_concurrent_calls_cancel_once() {
    // Setup database with an open vote
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db.seed_vote(SeedVote::default()).await;

    // Cancel the vote concurrently
    let calls = (0..TEST_DB_POOL_SIZE).map(|_| db.cancel_vote(REPOFN, ISSUE_NUM, USER1));
    let outcomes = future::join_all(calls).await;

    // Check only one call cancelled the vote
    let cancelled: Vec<Vote> = outcomes.into_iter().filter_map(|outcome| outcome.unwrap()).collect();
    assert_eq!(cancelled.len(), 1);
    assert_eq!(cancelled[0].vote_id, vote_id);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_cancel_vote_without_open_vote_returns_none() {
    // Setup database with a closed vote
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            closed: true,
            ..Default::default()
        })
        .await;
    let vote = test_db.read_vote(vote_id).await;

    // Try cancelling the vote
    let cancelled_vote = db.cancel_vote(REPOFN, ISSUE_NUM, USER1).await.unwrap();

    // Check nothing was cancelled and the closed vote did not change
    assert_eq!(cancelled_vote, None);
    assert_eq!(test_db.read_vote(vote_id).await, vote);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_claim_vote_to_close_concurrent_calls_claim_once() {
    // Setup database with a finished vote
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            ends_in: "-1 minute",
            ..Default::default()
        })
        .await;

    // Claim the vote concurrently
    let calls = (0..TEST_DB_POOL_SIZE).map(|_| db.claim_vote_to_close());
    let outcomes = future::join_all(calls).await;

    // Check only one call got the vote
    let claimed: Vec<Vote> = outcomes.into_iter().filter_map(|outcome| outcome.unwrap()).collect();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].vote_id, vote_id);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_claim_vote_to_close_expired_claim_can_be_claimed_again() {
    // Setup database with a vote claimed by a worker that never finished
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            ends_in: "-1 hour",

            close_next_attempt_in: Some("-1 minute"),
            ..Default::default()
        })
        .await;

    // Check the vote is claimed, and not claimed twice
    let vote = db.claim_vote_to_close().await.unwrap().unwrap();
    assert_eq!(vote.vote_id, vote_id);
    assert_eq!(db.claim_vote_to_close().await.unwrap(), None);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_claim_vote_to_close_returns_the_longest_waiting_first() {
    // Setup database with several votes with closing work due
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let newest = test_db
        .seed_vote(SeedVote {
            ends_in: "-1 minute",
            issue_number: 1,
            ..Default::default()
        })
        .await;
    let oldest = test_db
        .seed_vote(SeedVote {
            close_pending: true,
            closed: true,
            issue_number: 2,

            close_next_attempt_in: Some("-3 hours"),
            ..Default::default()
        })
        .await;
    let middle = test_db
        .seed_vote(SeedVote {
            ends_in: "-2 hours",
            issue_number: 3,
            ..Default::default()
        })
        .await;

    // Check the votes are claimed in order, and only once
    for vote_id in [oldest, middle, newest] {
        assert_eq!(db.claim_vote_to_close().await.unwrap().unwrap().vote_id, vote_id);
    }
    assert_eq!(db.claim_vote_to_close().await.unwrap(), None);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_claim_vote_to_close_selection() {
    // Setup votes covering each selection condition: attempts, close pending, closed,
    // ends in, next attempt in and whether the vote is selected or not
    let cases = [
        // Selected: finished votes, even without attempts left, as they are retried
        // until they close
        (0, false, false, "-1 minute", None, true),
        (3, false, false, "-1 minute", None, true),
        (3, false, false, "-1 minute", Some("-1 minute"), true),
        (12, false, false, "-1 minute", None, true),
        (100, false, false, "-1 minute", Some("-1 minute"), true),
        // Selected: closed votes with side effects pending and attempts left
        (0, true, true, "-1 minute", None, true),
        (0, true, true, "1 day", None, true),
        (11, true, true, "-1 minute", Some("-1 minute"), true),
        // Not selected: votes open that have not finished
        (0, false, false, "1 minute", None, false),
        // Not selected: closed votes with nothing pending, as the ones closed before
        // the closing steps were tracked
        (0, false, true, "-1 minute", None, false),
        // Not selected: votes waiting for a retry
        (1, false, false, "-1 minute", Some("1 minute"), false),
        (1, true, true, "-1 minute", Some("1 minute"), false),
        // Not selected: closed votes with side effects pending without attempts left
        (12, true, true, "-1 minute", Some("-1 minute"), false),
        (13, true, true, "-1 minute", None, false),
    ];

    // Setup database
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let mut expected = vec![];
    for (index, case) in cases.into_iter().enumerate() {
        let (close_attempts, close_pending, closed, ends_in, close_next_attempt_in, selected) = case;
        let issue_number = index as i64 + 1;
        test_db
            .seed_vote(SeedVote {
                close_attempts,
                close_pending,
                closed,
                ends_in,
                issue_number,

                close_next_attempt_in,
                ..Default::default()
            })
            .await;
        if selected {
            expected.push(issue_number);
        }
    }

    // Claim all the votes available
    let mut claimed = vec![];
    while let Some(vote) = db.claim_vote_to_close().await.unwrap() {
        claimed.push(vote.issue_number);
        assert!(
            claimed.len() <= expected.len(),
            "votes claimed twice: {claimed:?}"
        );
    }

    // Check only the expected votes were claimed
    claimed.sort_unstable();
    assert_eq!(claimed, expected);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_claim_vote_to_close_undecodable_cancelled_vote_last_attempt_is_abandoned() {
    // Setup database with a cancelled vote without the required cfg fields, on its last
    // attempt to publish its cancellation
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            cfg: json!({"close_on_passing": true}),
            close_attempts: CLOSE_MAX_ATTEMPTS - 1,
            close_pending: true,
            closed: true,

            cancelled_ago: Some("1 day"),
            close_next_attempt_in: Some("-1 minute"),
            ..Default::default()
        })
        .await;

    // Check the error returned tells the cancellation was abandoned
    let err = db.claim_vote_to_close().await.unwrap_err();
    assert_eq!(err.to_string(), "cancellation abandoned, no attempts left");
    assert_decode_error(&err, CFG_DECODE_ERROR);

    // Check the vote is deleted and nothing else is claimed
    assert_eq!(test_db.read_vote(vote_id).await, None);
    assert_eq!(db.claim_vote_to_close().await.unwrap(), None);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_claim_vote_to_close_undecodable_closed_vote_last_attempt_has_no_attempts_left() {
    // Setup database with a closed vote without the required cfg fields, on its last
    // attempt to run its closing side effects
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            cfg: json!({"close_on_passing": true}),
            close_attempts: CLOSE_MAX_ATTEMPTS - 1,
            close_pending: true,
            closed: true,
            ends_in: "-1 day",

            close_next_attempt_in: Some("-1 minute"),
            ..Default::default()
        })
        .await;

    // Check the error tells there are no attempts left for the vote identified
    let err = db.claim_vote_to_close().await.unwrap_err();
    assert_eq!(err.to_string(), "no attempts left");
    assert_eq!(
        err.chain().nth(1).unwrap().to_string(),
        format!("error decoding claimed vote {vote_id}")
    );
    assert_decode_error(&err, CFG_DECODE_ERROR);

    // Check the last failed attempt was recorded and nothing else is claimed
    let (close_attempts, _) = test_db.close_retry_state().await;
    assert_eq!(close_attempts, CLOSE_MAX_ATTEMPTS);
    assert_eq!(db.claim_vote_to_close().await.unwrap(), None);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_claim_vote_to_close_undecodable_vote_counts_as_failure() {
    // Setup database with a finished vote without the required cfg fields
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            cfg: json!({"close_on_passing": true}),
            ends_in: "-1 minute",
            ..Default::default()
        })
        .await;

    // Check the error identifies the vote and it does not block the next claim
    let err = db.claim_vote_to_close().await.unwrap_err();
    assert_eq!(err.to_string(), format!("error decoding claimed vote {vote_id}"));
    assert_decode_error(&err, CFG_DECODE_ERROR);
    assert_eq!(db.claim_vote_to_close().await.unwrap(), None);

    // Check the failed attempt was recorded
    let (close_attempts, delay) = test_db.close_retry_state().await;
    assert_eq!(close_attempts, 1);
    assert_close_retry_delay(delay, 60.0);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_claim_vote_to_close_undecodable_vote_does_not_block_other_votes() {
    // Setup database with an undecodable vote that has been waiting the longest
    let test_db = TestDB::new().await;
    let db = test_db.db();
    test_db
        .seed_vote(SeedVote {
            cfg: json!({"close_on_passing": true}),
            ends_in: "-2 hours",
            ..Default::default()
        })
        .await;
    let vote_id = test_db
        .seed_vote(SeedVote {
            ends_in: "-1 hour",
            issue_number: 2,
            ..Default::default()
        })
        .await;

    // Check the valid vote is claimed right after the failure
    assert_decode_error(&db.claim_vote_to_close().await.unwrap_err(), CFG_DECODE_ERROR);
    assert_eq!(db.claim_vote_to_close().await.unwrap().unwrap().vote_id, vote_id);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_claim_vote_to_close_undecodable_vote_does_not_wait_for_another_connection() {
    // Setup database with a single connection and a finished vote without the required
    // cfg fields
    let test_db = TestDB::with_pool_size(1).await;
    let db = test_db.db();
    test_db
        .seed_vote(SeedVote {
            cfg: json!({"close_on_passing": true}),
            ends_in: "-1 minute",
            ..Default::default()
        })
        .await;

    // Check the claim fails without waiting for a connection it already holds
    let outcome = timeout(CLAIM_TIMEOUT, db.claim_vote_to_close()).await;
    assert_decode_error(
        &outcome.expect("claim should not wait for another connection").unwrap_err(),
        CFG_DECODE_ERROR,
    );

    // Check the failed attempt was recorded
    let (close_attempts, delay) = test_db.close_retry_state().await;
    assert_eq!(close_attempts, 1);
    assert_close_retry_delay(delay, 60.0);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_claim_vote_to_close_undecodable_vote_recording_error_keeps_the_decode_error() {
    // Setup database with a finished vote without the required cfg fields, where
    // recording a failed attempt fails
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            cfg: json!({"close_on_passing": true}),
            ends_in: "-1 minute",
            ..Default::default()
        })
        .await;
    test_db.fail_vote_operation("update of close_attempts").await;

    // Check the error returned is the decode one, identifying the vote
    let err = db.claim_vote_to_close().await.unwrap_err();
    assert_eq!(err.to_string(), format!("error decoding claimed vote {vote_id}"));
    assert_decode_error(&err, CFG_DECODE_ERROR);

    // Check the failed attempt was not recorded, so the vote stays claimed until its
    // claim expires
    let (close_attempts, delay) = test_db.close_retry_state().await;
    assert_eq!(close_attempts, 0);
    assert_close_retry_delay(
        delay,
        f64::from(i32::try_from(CLOSE_CLAIM_DURATION_SECS).unwrap()),
    );
    assert_eq!(db.claim_vote_to_close().await.unwrap(), None);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_close_vote_cancelled_vote_is_not_closed() {
    // Setup database with a vote cancelled after being claimed to close
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            ends_in: "-1 minute",
            ..Default::default()
        })
        .await;
    claim_vote_to_close_until(&db).await;
    let vote = db.cancel_vote(REPOFN, ISSUE_NUM, USER1).await.unwrap().unwrap();

    // Check closing it is reported and leaves the cancellation untouched
    let results = setup_test_vote_results();
    assert!(!db.close_vote(vote_id, Some(&results), Some(HEAD_SHA)).await.unwrap());
    assert_eq!(test_db.read_vote(vote_id).await, Some(vote));
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_close_vote_closed_vote_is_not_closed_again() {
    // Setup database with a closed vote
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let results = setup_test_vote_results();
    let vote_id = test_db.seed_vote(SeedVote::default()).await;
    assert!(db.close_vote(vote_id, Some(&results), Some(HEAD_SHA)).await.unwrap());
    let vote = db.get_vote(vote_id).await.unwrap().unwrap();

    // Check closing it again does not change it
    assert!(!db.close_vote(vote_id, None, Some("def456")).await.unwrap());
    assert_eq!(db.get_vote(vote_id).await.unwrap(), Some(vote));
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_close_vote_leaves_side_effects_pending_until_complete() {
    // Setup database with a claimed vote that failed more attempts than the limit
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            close_attempts: CLOSE_MAX_ATTEMPTS + 3,
            ends_in: "-1 minute",
            ..Default::default()
        })
        .await;
    let claimed_until = claim_vote_to_close_until(&db).await;

    // Close the vote
    let results = setup_test_vote_results();
    assert!(db.close_vote(vote_id, Some(&results), Some(HEAD_SHA)).await.unwrap());

    // Check the vote is closed, with its attempts restarted, its claim kept and side
    // effects pending
    let vote = db.get_vote(vote_id).await.unwrap().unwrap();
    assert_eq!(vote.close_attempts, 0);
    assert!(vote.close_pending);
    assert!(vote.closed);
    assert_eq!(vote.close_head_sha.as_deref(), Some(HEAD_SHA));
    assert_eq!(vote.close_next_attempt_at, Some(claimed_until));
    assert!(vote.closed_at.is_some());
    assert_eq!(vote.results.as_ref(), Some(&results));

    // Check the closed vote is not claimed while the attempt is running
    assert_eq!(db.claim_vote_to_close().await.unwrap(), None);

    // Check a failure of its side effects starts the backoff over, with attempts left
    assert_eq!(
        db.record_vote_close_failure(vote_id, claimed_until).await.unwrap(),
        VoteCloseFailureOutcome::RetryScheduled
    );
    let (close_attempts, delay) = test_db.close_retry_state().await;
    assert_eq!(close_attempts, 1);
    assert_close_retry_delay(delay, CLOSE_RETRY_BASE_DELAY_SECS);

    // Check the vote can be claimed with its results once the retry is due
    test_db.expire_close_claims().await;
    let claimed_vote = db.claim_vote_to_close().await.unwrap().unwrap();
    assert_eq!(claimed_vote.vote_id, vote_id);
    assert_eq!(claimed_vote.close_head_sha.as_deref(), Some(HEAD_SHA));
    assert_eq!(claimed_vote.results, Some(results));

    // Check nothing is left to do once the close is complete
    db.complete_vote_close(vote_id).await.unwrap();
    let vote = db.get_vote(vote_id).await.unwrap().unwrap();
    assert!(!vote.close_pending);
    assert_eq!(vote.close_next_attempt_at, None);
    test_db.expire_close_claims().await;
    assert_eq!(db.claim_vote_to_close().await.unwrap(), None);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_close_vote_unknown_vote_is_not_closed() {
    // Setup database without votes
    let test_db = TestDB::new().await;
    let db = test_db.db();

    // Check closing a vote that does not exist is reported
    let vote_id = Uuid::parse_str(VOTE_ID).unwrap();
    assert!(!db.close_vote(vote_id, None, None).await.unwrap());
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_close_vote_without_results_stores_sql_null() {
    // Setup database
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db.seed_vote(SeedVote::default()).await;

    // Close the vote without results, as when its comment has been deleted
    assert!(db.close_vote(vote_id, None, None).await.unwrap());

    // Check the results are stored as SQL null and the vote can be read back
    assert!(test_db.results_are_sql_null(vote_id).await);
    let vote = db.get_vote(vote_id).await.unwrap().unwrap();
    assert!(vote.closed);
    assert!(vote.closed_at.is_some());
    assert_eq!(vote.results, None);
    assert_eq!(db.list_votes(REPOFN).await.unwrap(), vec![vote]);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_complete_vote_cancellation_deletes_only_cancelled_votes() {
    // Setup database with an open vote, a closed vote and a cancelled vote
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let open_vote_id = test_db.seed_vote(SeedVote::default()).await;
    let closed_vote_id = test_db
        .seed_vote(SeedVote {
            close_pending: true,
            closed: true,
            issue_number: 2,
            ..Default::default()
        })
        .await;
    let cancelled_vote_id = test_db
        .seed_vote(SeedVote {
            issue_number: 3,
            ..Default::default()
        })
        .await;
    db.cancel_vote(REPOFN, 3, USER1).await.unwrap().unwrap();

    // Complete the cancellation of all of them
    for vote_id in [open_vote_id, closed_vote_id, cancelled_vote_id] {
        db.complete_vote_cancellation(vote_id).await.unwrap();
    }

    // Check only the cancelled vote was deleted
    assert!(test_db.read_vote(open_vote_id).await.is_some());
    assert!(test_db.read_vote(closed_vote_id).await.is_some());
    assert_eq!(test_db.read_vote(cancelled_vote_id).await, None);
    assert!(!db.has_vote(REPOFN, 3).await.unwrap());
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_get_open_votes_with_close_on_passing_selection() {
    // Setup votes covering each selection condition, created two days ago by default
    let enabled = json!({"close_on_passing": true});
    let with_min_wait =
        |min_wait: Value| json!({"close_on_passing": true, "close_on_passing_min_wait": min_wait});
    let cases = [
        // Selected: no min wait, null min wait and min wait elapsed
        (enabled.clone(), false, true),
        (with_min_wait(Value::Null), false, true),
        (with_min_wait(json!("1 day")), false, true),
        // Not selected: closed, not enabled and min wait not elapsed
        (enabled, true, false),
        (json!({"close_on_passing": false}), false, false),
        (json!({}), false, false),
        (json!({"close_on_passing": null}), false, false),
        (json!({"close_on_passing": "banana"}), false, false),
        (with_min_wait(json!("1 week")), false, false),
        // Bad stored min wait values must not close the vote or fail the query
        (with_min_wait(json!("")), false, false),
        (with_min_wait(json!("-1 day")), false, false),
        (with_min_wait(json!("0 days")), false, false),
        (with_min_wait(json!("1 wek")), false, false),
        (with_min_wait(json!("100000000 years")), false, false),
        (with_min_wait(json!("infinity")), false, false),
    ];

    // Setup database
    let test_db = TestDB::new().await;
    let mut expected = vec![];
    for (index, (cfg, closed, selected)) in cases.into_iter().enumerate() {
        let issue_number = index as i64 + 1;
        test_db
            .seed_vote(SeedVote {
                cfg: merge_cfg(&cfg),
                closed,
                issue_number,
                ..Default::default()
            })
            .await;
        if selected {
            expected.push(issue_number);
        }
    }

    // Check only the expected votes are selected
    let votes = test_db.db().get_open_votes_with_close_on_passing().await.unwrap();
    let mut selected: Vec<i64> = votes.iter().map(|vote| vote.issue_number).collect();
    selected.sort_unstable();
    assert_eq!(selected, expected);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_get_pending_status_checks_selection() {
    // Setup votes covering each selection condition: check period, closed, created ago,
    // ends in, checked ago and whether the vote is selected or not
    let cases = [
        // Selected
        (Some("1 day"), false, "2 days", "10 days", None, true),
        (Some("24 hours"), false, "2 days", "10 days", None, true),
        (Some("1 day"), false, "25 hours", "10 days", None, true),
        (Some("1 day"), false, "2 days", "10 days", Some("25 hours"), true),
        (Some("1 day"), false, "2 days", "61 minutes", None, true),
        // Not selected
        (None, false, "2 days", "10 days", None, false),
        (Some("1 day"), false, "2 days", "10 days", Some("23 hours"), false),
        (Some("1 day"), true, "2 days", "10 days", None, false),
        (Some("1 day"), false, "23 hours", "10 days", None, false),
        (Some("1 day"), false, "2 days", "59 minutes", None, false),
        (Some("1 week"), false, "2 days", "10 days", None, false),
        (Some("23 hours"), false, "2 days", "10 days", None, false),
        // Bad stored values must be skipped and must not fail the query
        (Some(""), false, "2 days", "10 days", None, false),
        (Some("-1 day"), false, "2 days", "10 days", None, false),
        (Some("100000000 years"), false, "2 days", "10 days", None, false),
        (Some("banana"), false, "2 days", "10 days", None, false),
        (Some("banana"), false, "2 days", "59 minutes", None, false),
        (Some("infinity"), false, "2 days", "10 days", None, false),
        (Some("weekly"), false, "2 days", "10 days", None, false),
    ];

    // Setup database, including a vote without the status check configured
    let test_db = TestDB::new().await;
    let mut expected = vec![];
    for (index, case) in cases.into_iter().enumerate() {
        let (check_period, closed, created_ago, ends_in, checked_ago, selected) = case;
        let issue_number = index as i64 + 1;
        test_db
            .seed_vote(SeedVote {
                cfg: merge_cfg(&json!({"periodic_status_check": check_period})),
                closed,
                created_ago,
                ends_in,
                issue_number,

                checked_ago,
                ..Default::default()
            })
            .await;
        if selected {
            expected.push(issue_number);
        }
    }
    test_db
        .seed_vote(SeedVote {
            issue_number: 100,
            ..Default::default()
        })
        .await;

    // Check only the expected votes are selected
    let inputs = test_db.db().get_pending_status_checks().await.unwrap();
    assert!(inputs.iter().all(|input| input.repository_full_name == REPOFN));
    let mut selected: Vec<i64> = inputs.iter().map(|input| input.issue_number).collect();
    selected.sort_unstable();
    assert_eq!(selected, expected);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_get_vote_unknown_id_returns_none() {
    // Setup database
    let test_db = TestDB::new().await;

    // Check an unknown vote is not found
    assert_eq!(
        test_db.db().get_vote(Uuid::parse_str(VOTE_ID).unwrap()).await.unwrap(),
        None
    );
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_has_vote_cancelled_vote_counts_until_its_check_run_is_done() {
    // Setup database with a pull request vote whose cancellation is pending, as well as
    // a closed vote with its closing check run created in another pull request
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let cancelled_vote_id = test_db
        .seed_vote(SeedVote {
            is_pull_request: true,
            ..Default::default()
        })
        .await;
    db.cancel_vote(REPOFN, ISSUE_NUM, USER1).await.unwrap().unwrap();
    let closed_vote_id = test_db
        .seed_vote(SeedVote {
            close_pending: true,
            closed: true,
            is_pull_request: true,
            issue_number: 2,
            ..Default::default()
        })
        .await;
    db.record_vote_close_step(closed_vote_id, VoteCloseStep::CheckRun).await.unwrap();

    // Check the cancelled vote counts while its check run is pending, even if the other
    // steps are done
    db.record_vote_close_step(cancelled_vote_id, VoteCloseStep::Comment).await.unwrap();
    db.record_vote_close_step(cancelled_vote_id, VoteCloseStep::Labels).await.unwrap();
    assert!(db.has_vote(REPOFN, ISSUE_NUM).await.unwrap());

    // Check the cancelled vote stops counting once its check run is done, while the
    // closed vote keeps counting
    db.record_vote_close_step(cancelled_vote_id, VoteCloseStep::CheckRun).await.unwrap();
    assert!(!db.has_vote(REPOFN, ISSUE_NUM).await.unwrap());
    assert!(db.has_vote(REPOFN, 2).await.unwrap());

    // Check the cancelled vote is still stored, as its cancellation is not completed
    assert!(test_db.read_vote(cancelled_vote_id).await.is_some());
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_has_vote_with_comment_selection() {
    // Setup database without votes
    let test_db = TestDB::new().await;
    let db = test_db.db();

    // Check no vote references the comment
    assert!(!db.has_vote_with_comment(COMMENT_ID).await.unwrap());

    // Check an open vote references its comment, and only that one
    let vote_id = test_db.seed_vote(SeedVote::default()).await;
    assert!(db.has_vote_with_comment(COMMENT_ID).await.unwrap());
    assert!(!db.has_vote_with_comment(COMMENT_ID2).await.unwrap());

    // Check a closed vote still references its comment
    assert!(db.close_vote(vote_id, None, None).await.unwrap());
    assert!(db.has_vote_with_comment(COMMENT_ID).await.unwrap());

    // Check a vote whose cancellation is pending still references its comment
    test_db
        .seed_vote(SeedVote {
            issue_number: 2,
            vote_comment_id: COMMENT_ID2,
            ..Default::default()
        })
        .await;
    db.cancel_vote(REPOFN, 2, USER1).await.unwrap().unwrap();
    assert!(db.has_vote_with_comment(COMMENT_ID2).await.unwrap());
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_is_close_state_step_pending_other_steps_return_errors() {
    // Setup database
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db.seed_vote(SeedVote::default()).await;

    // Check the steps that do not publish the state of the vote cannot be checked
    for step in [VoteCloseStep::Announcement, VoteCloseStep::Comment] {
        let err = db.is_close_state_step_pending(vote_id, step).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "only the check run and labels steps publish the state of a vote"
        );
    }
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_is_close_state_step_pending_selection() {
    // Setup database with a closed vote and an older one in the same issue
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            closed: true,
            created_ago: "2 days",
            ..Default::default()
        })
        .await;
    let older_vote_id = test_db
        .seed_vote(SeedVote {
            closed: true,
            created_ago: "3 days",
            ..Default::default()
        })
        .await;

    // Check the steps are pending in the newest vote only
    assert_eq!(state_steps_pending(&db, vote_id).await, [true, true]);
    assert_eq!(state_steps_pending(&db, older_vote_id).await, [false, false]);

    // Check newer votes on other issues and repositories do not count
    test_db
        .seed_vote(SeedVote {
            created_ago: "1 day",
            issue_number: 2,
            ..Default::default()
        })
        .await;
    test_db
        .seed_vote(SeedVote {
            created_ago: "1 day",
            repository_full_name: OTHER_REPOFN,
            ..Default::default()
        })
        .await;
    assert_eq!(state_steps_pending(&db, vote_id).await, [true, true]);

    // Check a step recorded as done is not pending anymore, unlike the other one
    db.record_vote_close_step(vote_id, VoteCloseStep::CheckRun).await.unwrap();
    assert_eq!(state_steps_pending(&db, vote_id).await, [false, true]);

    // Check an open newer vote counts, and that it keeps counting once cancelled, while
    // its cancellation is pending publication
    let newer_vote_id = test_db
        .seed_vote(SeedVote {
            created_ago: "1 day",
            ..Default::default()
        })
        .await;
    assert_eq!(state_steps_pending(&db, vote_id).await, [false, false]);
    db.cancel_vote(REPOFN, ISSUE_NUM, USER1).await.unwrap().unwrap();
    assert_eq!(state_steps_pending(&db, vote_id).await, [false, false]);

    // Check a newer vote is not seen once deleted, as storing it is what marks the steps
    // of the older votes as done
    db.complete_vote_cancellation(newer_vote_id).await.unwrap();
    assert_eq!(state_steps_pending(&db, vote_id).await, [false, true]);

    // Check a closed newer vote counts
    test_db
        .seed_vote(SeedVote {
            closed: true,
            created_ago: "1 day",
            ..Default::default()
        })
        .await;
    assert_eq!(state_steps_pending(&db, vote_id).await, [false, false]);

    // Check an unknown vote has no steps pending
    let unknown_vote_id = Uuid::parse_str(VOTE_ID).unwrap();
    assert_eq!(state_steps_pending(&db, unknown_vote_id).await, [false, false]);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_reads_are_scoped_by_repository_issue_and_state() {
    // Setup database
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let closed_vote_id = test_db
        .seed_vote(SeedVote {
            closed: true,
            created_ago: "3 days",
            ..Default::default()
        })
        .await;
    let open_vote_id = test_db
        .seed_vote(SeedVote {
            created_ago: "1 day",
            ..Default::default()
        })
        .await;
    let closed_only_vote_id = test_db
        .seed_vote(SeedVote {
            closed: true,
            created_ago: "2 days",
            issue_number: 2,
            ..Default::default()
        })
        .await;
    let other_repo_vote_id = test_db
        .seed_vote(SeedVote {
            repository_full_name: OTHER_REPOFN,
            ..Default::default()
        })
        .await;

    // Check an issue with only a closed vote has a vote but not an open one
    assert!(db.has_vote(REPOFN, 2).await.unwrap());
    assert!(!db.has_vote_open(REPOFN, 2).await.unwrap());
    assert_eq!(db.get_open_vote(REPOFN, 2).await.unwrap(), None);

    // Check an issue without votes has none
    assert!(!db.has_vote(REPOFN, 3).await.unwrap());
    assert!(!db.has_vote_open(REPOFN, 3).await.unwrap());

    // Check the open vote found is the one in the repository requested
    assert!(db.has_vote_open(REPOFN, ISSUE_NUM).await.unwrap());
    let open_vote = db.get_open_vote(REPOFN, ISSUE_NUM).await.unwrap().unwrap();
    assert_eq!(open_vote.vote_id, open_vote_id);

    // Check votes are limited to the repository requested, newest first
    let votes = db.list_votes(REPOFN).await.unwrap();
    let vote_ids: Vec<Uuid> = votes.iter().map(|vote| vote.vote_id).collect();
    assert_eq!(vote_ids, vec![open_vote_id, closed_only_vote_id, closed_vote_id]);

    // Check closed votes cannot be cancelled
    assert_eq!(db.cancel_vote(REPOFN, 2, USER1).await.unwrap(), None);
    assert!(db.get_vote(closed_only_vote_id).await.unwrap().is_some());

    // Check cancelling affects only the open vote in the repository requested
    let cancelled_vote = db.cancel_vote(REPOFN, ISSUE_NUM, USER1).await.unwrap().unwrap();
    assert_eq!(cancelled_vote.vote_id, open_vote_id);
    assert_eq!(db.get_vote(open_vote_id).await.unwrap(), None);
    assert!(db.get_vote(closed_vote_id).await.unwrap().is_some());
    let other_repo_vote = db.get_vote(other_repo_vote_id).await.unwrap().unwrap();
    assert!(!other_repo_vote.closed);
    assert_eq!(other_repo_vote.cancelled_at, None);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_reads_of_pending_cancellations() {
    // Setup database with a closed vote and a newer vote whose cancellation is pending,
    // each one with its own comment
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let closed_vote_id = test_db
        .seed_vote(SeedVote {
            closed: true,
            created_ago: "3 days",
            vote_comment_id: COMMENT_ID2,
            ..Default::default()
        })
        .await;
    let cancelled_vote_id = test_db
        .seed_vote(SeedVote {
            cfg: merge_cfg(&json!({
                "close_on_passing": true,
                "periodic_status_check": "1 day"
            })),
            ..Default::default()
        })
        .await;
    db.cancel_vote(REPOFN, ISSUE_NUM, USER1).await.unwrap().unwrap();

    // Check the audit reads leave the cancelled vote out
    assert_eq!(db.get_vote(cancelled_vote_id).await.unwrap(), None);
    let votes = db.list_votes(REPOFN).await.unwrap();
    let vote_ids: Vec<Uuid> = votes.iter().map(|vote| vote.vote_id).collect();
    assert_eq!(vote_ids, vec![closed_vote_id]);

    // Check the cancelled vote is not open, so a new vote can be created
    assert!(!db.has_vote_open(REPOFN, ISSUE_NUM).await.unwrap());
    assert_eq!(db.get_open_vote(REPOFN, ISSUE_NUM).await.unwrap(), None);
    assert_eq!(db.get_open_votes_with_close_on_passing().await.unwrap(), vec![]);
    assert_eq!(db.get_pending_status_checks().await.unwrap(), vec![]);

    // Check the cancelled vote still exists for the other reads. The closed vote makes
    // the issue have a vote anyway, so how a pending cancellation counts for that is
    // checked by db_contracts_has_vote_cancelled_vote_counts_until_its_check_run_is_done
    assert!(db.has_vote_with_comment(COMMENT_ID).await.unwrap());
    assert_eq!(state_steps_pending(&db, closed_vote_id).await, [false, false]);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_record_vote_close_failure_backs_off_until_no_attempts_left() {
    // Setup database with a closed vote claimed to run its side effects pending
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            close_pending: true,
            closed: true,
            ..Default::default()
        })
        .await;
    let mut claimed_until = claim_vote_to_close_until(&db).await;

    // Fail all the attempts but the last one, checking the delay used for the next one
    let delays = [
        60.0, 120.0, 240.0, 480.0, 960.0, 1920.0, 3840.0, 7680.0, 15360.0, 21600.0, 21600.0,
    ];
    for (expected_attempts, expected_delay) in (1..).zip(delays) {
        assert_eq!(
            db.record_vote_close_failure(vote_id, claimed_until).await.unwrap(),
            VoteCloseFailureOutcome::RetryScheduled
        );
        let (close_attempts, delay) = test_db.close_retry_state().await;
        assert_eq!(close_attempts, expected_attempts);
        assert_close_retry_delay(delay, expected_delay);

        // The vote is not selected until the delay is over
        assert_eq!(db.claim_vote_to_close().await.unwrap(), None);
        test_db.expire_close_claims().await;
        claimed_until = claim_vote_to_close_until(&db).await;
    }

    // Check no more attempts are made after the last one, keeping the closed vote
    assert_eq!(
        db.record_vote_close_failure(vote_id, claimed_until).await.unwrap(),
        VoteCloseFailureOutcome::NoAttemptsLeft
    );
    test_db.expire_close_claims().await;
    assert_eq!(db.claim_vote_to_close().await.unwrap(), None);
    let vote = db.get_vote(vote_id).await.unwrap().unwrap();
    assert_eq!(vote.close_attempts, CLOSE_MAX_ATTEMPTS);
    assert!(vote.close_pending);
    assert!(vote.closed);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_record_vote_close_failure_cancelled_vote_last_attempt_abandons_the_cancellation() {
    // Setup database with a cancelled pull request vote two attempts away from the
    // limit, whose check run step is pending, claimed to publish its cancellation
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            close_attempts: CLOSE_MAX_ATTEMPTS - 2,
            close_pending: true,
            closed: true,
            is_pull_request: true,

            cancelled_ago: Some("1 day"),
            close_next_attempt_in: Some("-1 minute"),
            ..Default::default()
        })
        .await;
    let claimed_until = claim_vote_to_close_until(&db).await;

    // Check the attempt before the last one is rescheduled, keeping the vote
    assert_eq!(
        db.record_vote_close_failure(vote_id, claimed_until).await.unwrap(),
        VoteCloseFailureOutcome::RetryScheduled
    );
    let vote = test_db.read_vote(vote_id).await.unwrap();
    assert_eq!(vote.close_attempts, CLOSE_MAX_ATTEMPTS - 1);
    assert!(db.has_vote(REPOFN, ISSUE_NUM).await.unwrap());

    // Fail the last attempt
    test_db.expire_close_claims().await;
    let claimed_until = claim_vote_to_close_until(&db).await;
    assert_eq!(
        db.record_vote_close_failure(vote_id, claimed_until).await.unwrap(),
        VoteCloseFailureOutcome::CancellationAbandoned
    );

    // Check the vote is deleted, so the pull request has no vote and nothing is retried
    assert_eq!(test_db.read_vote(vote_id).await, None);
    assert!(!db.has_vote(REPOFN, ISSUE_NUM).await.unwrap());
    assert!(!db.has_vote_open(REPOFN, ISSUE_NUM).await.unwrap());
    test_db.expire_close_claims().await;
    assert_eq!(db.claim_vote_to_close().await.unwrap(), None);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_record_vote_close_failure_cancelled_vote_last_attempt_with_expired_claim_is_not_recorded()
 {
    // Setup database with a cancelled vote on its last attempt, claimed twice as the
    // first claim expired
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            close_attempts: CLOSE_MAX_ATTEMPTS - 1,
            close_pending: true,
            closed: true,

            cancelled_ago: Some("1 day"),
            close_next_attempt_in: Some("-1 minute"),
            ..Default::default()
        })
        .await;
    let expired_claimed_until = claim_vote_to_close_until(&db).await;
    test_db.expire_close_claims().await;
    let claimed_until = claim_vote_to_close_until(&db).await;

    // Check the failure of the worker with the expired claim does not delete the vote
    assert_eq!(
        db.record_vote_close_failure(vote_id, expired_claimed_until).await.unwrap(),
        VoteCloseFailureOutcome::ClaimLost
    );
    let vote = test_db.read_vote(vote_id).await.unwrap();
    assert_eq!(vote.close_attempts, CLOSE_MAX_ATTEMPTS - 1);

    // Check the worker holding the claim abandons the cancellation
    assert_eq!(
        db.record_vote_close_failure(vote_id, claimed_until).await.unwrap(),
        VoteCloseFailureOutcome::CancellationAbandoned
    );
    assert_eq!(test_db.read_vote(vote_id).await, None);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_record_vote_close_failure_deleted_vote_is_not_recorded() {
    // Setup database
    let test_db = TestDB::new().await;

    // Check nothing is recorded for a vote that does not exist anymore
    let vote_id = Uuid::parse_str(VOTE_ID).unwrap();
    assert_eq!(
        test_db.db().record_vote_close_failure(vote_id, OffsetDateTime::now_utc()).await.unwrap(),
        VoteCloseFailureOutcome::ClaimLost
    );
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_record_vote_close_failure_expired_claim_claimed_again_is_not_recorded() {
    // Setup database with a finished vote claimed twice, as the first claim expired
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            ends_in: "-1 minute",
            ..Default::default()
        })
        .await;
    let expired_claimed_until = claim_vote_to_close_until(&db).await;
    test_db.expire_close_claims().await;
    let claimed_until = claim_vote_to_close_until(&db).await;

    // Check the failure of the worker with the expired claim is not recorded
    assert_eq!(
        db.record_vote_close_failure(vote_id, expired_claimed_until).await.unwrap(),
        VoteCloseFailureOutcome::ClaimLost
    );

    // Check the vote is still claimed by the worker that claimed it again
    let (close_attempts, delay) = test_db.close_retry_state().await;
    assert_eq!(close_attempts, 0);
    assert_close_retry_delay(
        delay,
        f64::from(i32::try_from(CLOSE_CLAIM_DURATION_SECS).unwrap()),
    );
    assert_eq!(db.claim_vote_to_close().await.unwrap(), None);
    assert_eq!(
        db.record_vote_close_failure(vote_id, claimed_until).await.unwrap(),
        VoteCloseFailureOutcome::RetryScheduled
    );
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_record_vote_close_failure_open_vote_delay_stays_capped_with_many_attempts() {
    // Setup database with a finished vote that failed far more attempts than the ones
    // needed to overflow the power of the delay without a cap
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            close_attempts: 100_000,
            ends_in: "-1 minute",
            ..Default::default()
        })
        .await;
    let claimed_until = claim_vote_to_close_until(&db).await;

    // Check the failure is rescheduled with the maximum delay
    assert_eq!(
        db.record_vote_close_failure(vote_id, claimed_until).await.unwrap(),
        VoteCloseFailureOutcome::RetryScheduled
    );
    let (close_attempts, delay) = test_db.close_retry_state().await;
    assert_eq!(close_attempts, 100_001);
    assert_close_retry_delay(delay, CLOSE_RETRY_MAX_DELAY_SECS);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_record_vote_close_failure_open_vote_keeps_retrying_after_the_limit() {
    // Setup database with a finished vote that failed all the attempts of the limit
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            close_attempts: CLOSE_MAX_ATTEMPTS,
            ends_in: "-1 minute",
            ..Default::default()
        })
        .await;
    let claimed_until = claim_vote_to_close_until(&db).await;

    // Check the failure is rescheduled with the maximum delay, keeping the vote open
    assert_eq!(
        db.record_vote_close_failure(vote_id, claimed_until).await.unwrap(),
        VoteCloseFailureOutcome::RetryScheduled
    );
    let (close_attempts, delay) = test_db.close_retry_state().await;
    assert_eq!(close_attempts, CLOSE_MAX_ATTEMPTS + 1);
    assert_close_retry_delay(delay, CLOSE_RETRY_MAX_DELAY_SECS);
    assert!(db.has_vote_open(REPOFN, ISSUE_NUM).await.unwrap());

    // Check the vote is claimed again once the delay is over
    assert_eq!(db.claim_vote_to_close().await.unwrap(), None);
    test_db.expire_close_claims().await;
    let vote = db.claim_vote_to_close().await.unwrap().unwrap();
    assert_eq!(vote.vote_id, vote_id);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_record_vote_close_failure_vote_cancelled_while_claimed_is_not_recorded() {
    // Setup database with a finished vote claimed to close it, and cancelled after that
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            ends_in: "-1 minute",
            ..Default::default()
        })
        .await;
    let closer_claimed_until = claim_vote_to_close_until(&db).await;
    let cancelled_vote = db.cancel_vote(REPOFN, ISSUE_NUM, USER1).await.unwrap().unwrap();
    let cancellation_claimed_until = cancelled_vote.close_next_attempt_at.unwrap();
    assert_ne!(cancellation_claimed_until, closer_claimed_until);

    // Check the failure of the votes closer is not recorded
    assert_eq!(
        db.record_vote_close_failure(vote_id, closer_claimed_until).await.unwrap(),
        VoteCloseFailureOutcome::ClaimLost
    );

    // Check the publication of the cancellation is still claimed by its publisher
    let (close_attempts, delay) = test_db.close_retry_state().await;
    assert_eq!(close_attempts, 0);
    assert_close_retry_delay(
        delay,
        f64::from(i32::try_from(CLOSE_CLAIM_DURATION_SECS).unwrap()),
    );
    assert_eq!(db.claim_vote_to_close().await.unwrap(), None);
    assert_eq!(
        db.record_vote_close_failure(vote_id, cancellation_claimed_until).await.unwrap(),
        VoteCloseFailureOutcome::RetryScheduled
    );
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_record_vote_close_step_keeps_the_first_timestamp() {
    // Setup database with a vote with all its steps recorded
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db.seed_vote(SeedVote::default()).await;
    let steps = [
        VoteCloseStep::Announcement,
        VoteCloseStep::CheckRun,
        VoteCloseStep::Comment,
        VoteCloseStep::Labels,
    ];
    for step in steps {
        db.record_vote_close_step(vote_id, step).await.unwrap();
    }
    let vote = db.get_vote(vote_id).await.unwrap().unwrap();

    // Record the steps again
    for step in steps {
        db.record_vote_close_step(vote_id, step).await.unwrap();
    }

    // Check the timestamps did not change
    let vote_recorded_again = db.get_vote(vote_id).await.unwrap().unwrap();
    assert!(vote.close_announcement_posted_at.is_some());
    assert_eq!(
        vote_recorded_again.close_announcement_posted_at,
        vote.close_announcement_posted_at
    );
    assert!(vote.close_check_run_created_at.is_some());
    assert_eq!(
        vote_recorded_again.close_check_run_created_at,
        vote.close_check_run_created_at
    );
    assert!(vote.close_comment_posted_at.is_some());
    assert_eq!(
        vote_recorded_again.close_comment_posted_at,
        vote.close_comment_posted_at
    );
    assert!(vote.close_labels_updated_at.is_some());
    assert_eq!(
        vote_recorded_again.close_labels_updated_at,
        vote.close_labels_updated_at
    );
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_record_vote_close_step_sets_the_step_timestamp() {
    // Setup database
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db.seed_vote(SeedVote::default()).await;
    let other_vote_id = test_db
        .seed_vote(SeedVote {
            issue_number: 2,
            ..Default::default()
        })
        .await;

    // Check recording the comment step sets only its timestamp. The steps below are
    // recorded one by one, each check depending on the ones recorded before it
    let is_set = |timestamp: Option<OffsetDateTime>| timestamp.is_some();
    db.record_vote_close_step(vote_id, VoteCloseStep::Comment).await.unwrap();
    let vote = db.get_vote(vote_id).await.unwrap().unwrap();
    assert!(is_set(vote.close_comment_posted_at));
    assert!(!is_set(vote.close_announcement_posted_at));
    assert!(!is_set(vote.close_check_run_created_at));
    assert!(!is_set(vote.close_labels_updated_at));

    // Check recording the announcement step sets its timestamp, leaving the later ones
    db.record_vote_close_step(vote_id, VoteCloseStep::Announcement).await.unwrap();
    let vote = db.get_vote(vote_id).await.unwrap().unwrap();
    assert!(is_set(vote.close_announcement_posted_at));
    assert!(!is_set(vote.close_check_run_created_at));
    assert!(!is_set(vote.close_labels_updated_at));

    // Check recording the check run step sets its timestamp, leaving the labels one
    db.record_vote_close_step(vote_id, VoteCloseStep::CheckRun).await.unwrap();
    let vote = db.get_vote(vote_id).await.unwrap().unwrap();
    assert!(is_set(vote.close_check_run_created_at));
    assert!(!is_set(vote.close_labels_updated_at));

    // Check recording the labels step sets its timestamp
    db.record_vote_close_step(vote_id, VoteCloseStep::Labels).await.unwrap();
    let vote = db.get_vote(vote_id).await.unwrap().unwrap();
    assert!(is_set(vote.close_labels_updated_at));

    // Check other votes were not updated
    let other_vote = db.get_vote(other_vote_id).await.unwrap().unwrap();
    assert!(!is_set(other_vote.close_announcement_posted_at));
    assert!(!is_set(other_vote.close_check_run_created_at));
    assert!(!is_set(other_vote.close_comment_posted_at));
    assert!(!is_set(other_vote.close_labels_updated_at));
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_selection_queries_read_the_keys_of_stored_profiles() {
    // Setup database and inputs
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let cfg = CfgProfile {
        duration: Duration::from_hours(240),

        close_on_passing: Some(true),
        close_on_passing_min_wait: Some("1 day".to_string()),
        periodic_status_check: Some("1 day".to_string()),
        ..Default::default()
    };
    let other_issue_input = CreateVoteInput {
        issue_number: 2,
        ..setup_test_create_vote_input()
    };

    // Store a vote with the features enabled, and another one with them disabled
    let input = setup_test_create_vote_input();
    let vote_id = db.store_vote(COMMENT_ID, &input, &cfg).await.unwrap();
    let default_cfg = CfgProfile {
        duration: Duration::from_hours(240),
        ..Default::default()
    };
    db.store_vote(COMMENT_ID2, &other_issue_input, &default_cfg).await.unwrap();

    // Check no vote is selected until the periods have elapsed
    assert_eq!(db.get_open_votes_with_close_on_passing().await.unwrap(), vec![]);
    assert_eq!(db.get_pending_status_checks().await.unwrap(), vec![]);

    // Backdate the votes past the configured periods
    test_db.backdate_votes("2 days").await;

    // Check only the vote with the features enabled is selected
    let votes = db.get_open_votes_with_close_on_passing().await.unwrap();
    assert_eq!(votes.len(), 1);
    assert_eq!(votes[0].vote_id, vote_id);
    let inputs = db.get_pending_status_checks().await.unwrap();
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0].issue_number, ISSUE_NUM);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_store_vote_allows_a_new_vote_after_closing() {
    // Setup database with a closed vote
    let test_db = TestDB::new().await;
    let db = test_db.db();
    test_db
        .seed_vote(SeedVote {
            closed: true,
            ..Default::default()
        })
        .await;

    // Store a new vote in the same issue
    let input = setup_test_create_vote_input();
    let vote_id = db.store_vote(COMMENT_ID, &input, &CfgProfile::default()).await.unwrap();

    // Check it is the open vote
    let vote = db.get_open_vote(REPOFN, ISSUE_NUM).await.unwrap().unwrap();
    assert_eq!(vote.vote_id, vote_id);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_store_vote_allows_open_votes_in_other_issues() {
    // Setup database and inputs
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let cfg = CfgProfile::default();
    let input = setup_test_create_vote_input();
    let other_issue_input = CreateVoteInput {
        issue_number: ISSUE_NUM + 1,
        ..setup_test_create_vote_input()
    };
    let other_repo_input = CreateVoteInput {
        repository_full_name: OTHER_REPOFN.to_string(),
        ..setup_test_create_vote_input()
    };

    // Check all the votes are stored
    db.store_vote(COMMENT_ID, &input, &cfg).await.unwrap();
    db.store_vote(COMMENT_ID, &other_issue_input, &cfg).await.unwrap();
    db.store_vote(COMMENT_ID, &other_repo_input, &cfg).await.unwrap();
    assert!(db.has_vote_open(REPOFN, ISSUE_NUM).await.unwrap());
    assert!(db.has_vote_open(REPOFN, ISSUE_NUM + 1).await.unwrap());
    assert!(db.has_vote_open(OTHER_REPOFN, ISSUE_NUM).await.unwrap());
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_store_vote_concurrent_calls_store_one_vote() {
    // Setup database and input
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let cfg = CfgProfile::default();
    let input = setup_test_create_vote_input();

    // Store the same vote concurrently
    let calls = (0..TEST_DB_POOL_SIZE).map(|_| db.store_vote(COMMENT_ID, &input, &cfg));
    let outcomes = future::join_all(calls).await;

    // Check one call stored the vote and all the others were told it is open
    let (stored, rejected): (Vec<_>, Vec<_>) = outcomes.into_iter().partition(Result::is_ok);
    assert_eq!(stored.len(), 1);
    assert!(
        rejected.iter().all(|outcome| {
            outcome.as_ref().unwrap_err().downcast_ref::<VoteAlreadyOpenError>().is_some()
        })
    );
    assert_eq!(db.list_votes(REPOFN).await.unwrap().len(), 1);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_store_vote_rejected_does_not_supersede_older_votes() {
    // Setup database with a closed vote with its closing steps pending and an open vote
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let closed_vote_id = test_db
        .seed_vote(SeedVote {
            close_pending: true,
            closed: true,
            created_ago: "3 days",
            ..Default::default()
        })
        .await;
    test_db.seed_vote(SeedVote::default()).await;
    let closed_vote = test_db.read_vote(closed_vote_id).await;

    // Try storing another vote in the same issue
    let input = setup_test_create_vote_input();
    let err = db.store_vote(COMMENT_ID2, &input, &CfgProfile::default()).await.unwrap_err();

    // Check the closed vote did not change
    assert!(err.downcast_ref::<VoteAlreadyOpenError>().is_some());
    assert_eq!(test_db.read_vote(closed_vote_id).await, closed_vote);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_store_vote_rejects_second_open_vote() {
    // Setup database with an open vote
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let cfg = CfgProfile::default();
    let input = setup_test_create_vote_input();
    let vote_id = db.store_vote(COMMENT_ID, &input, &cfg).await.unwrap();

    // Try storing another vote in the same issue
    let err = db.store_vote(COMMENT_ID2, &input, &cfg).await.unwrap_err();

    // Check the error can be told apart and the open vote did not change
    assert!(err.downcast_ref::<VoteAlreadyOpenError>().is_some());
    let votes = db.list_votes(REPOFN).await.unwrap();
    assert_eq!(votes.len(), 1);
    assert_eq!(votes[0].vote_comment_id, COMMENT_ID);
    assert_eq!(votes[0].vote_id, vote_id);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_store_vote_round_trips_every_field() {
    // Setup database and input
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let input = setup_test_create_vote_input();
    let cfg = setup_test_full_cfg_profile();

    // Store the vote
    let vote_id = db.store_vote(COMMENT_ID, &input, &cfg).await.unwrap();

    // Check every required field is read back from the column it was written to
    let vote = db.get_vote(vote_id).await.unwrap().unwrap();
    assert_eq!(vote.cfg, cfg);
    assert_eq!(vote.close_attempts, 0);
    assert!(!vote.close_pending);
    assert!(!vote.closed);
    assert_eq!(vote.ends_at - vote.created_at, TimeDuration::hours(1));
    assert_eq!(vote.created_by, USER);
    assert_eq!(vote.installation_id, INST_ID as i64);
    assert!(vote.is_pull_request);
    assert_eq!(vote.issue_id, ISSUE_ID);
    assert_eq!(vote.issue_number, ISSUE_NUM);
    assert_eq!(vote.repository_full_name, REPOFN);
    assert_eq!(vote.vote_comment_id, COMMENT_ID);
    assert_eq!(vote.vote_id, vote_id);

    // Check every optional field is read back from the column it was written to
    assert_eq!(vote.cancelled_at, None);
    assert_eq!(vote.cancelled_by, None);
    assert_eq!(vote.checked_at, None);
    assert_eq!(vote.close_announcement_posted_at, None);
    assert_eq!(vote.close_check_run_created_at, None);
    assert_eq!(vote.close_comment_posted_at, None);
    assert_eq!(vote.close_head_sha, None);
    assert_eq!(vote.close_labels_updated_at, None);
    assert_eq!(vote.close_next_attempt_at, None);
    assert_eq!(vote.closed_at, None);
    assert_eq!(vote.issue_title.as_deref(), Some(TITLE));
    assert_eq!(vote.organization.as_deref(), Some(ORG));
    assert_eq!(vote.results, None);

    // Check the other reads return the same vote
    let open_vote = db.get_open_vote(REPOFN, ISSUE_NUM).await.unwrap();
    assert_eq!(open_vote.as_ref(), Some(&vote));
    assert_eq!(db.list_votes(REPOFN).await.unwrap(), vec![vote]);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_store_vote_round_trips_pass_threshold_bases() {
    // Setup cases (pass threshold base)
    let bases = [
        None,
        Some(PassThresholdBase::AllowedVoters),
        Some(PassThresholdBase::VotesCast {
            exclude_abstentions: None,
        }),
        Some(PassThresholdBase::VotesCast {
            exclude_abstentions: Some(false),
        }),
        Some(PassThresholdBase::VotesCast {
            exclude_abstentions: Some(true),
        }),
    ];

    // Setup database
    let test_db = TestDB::new().await;
    let db = test_db.db();

    // Check each base round trips
    for (index, pass_threshold_base) in bases.into_iter().enumerate() {
        // Store a vote using the base, each one in its own issue
        let input = CreateVoteInput {
            issue_number: index as i64 + 1,
            ..setup_test_create_vote_input()
        };
        let cfg = CfgProfile {
            pass_threshold_base,
            ..setup_test_full_cfg_profile()
        };
        let vote_id = db.store_vote(COMMENT_ID, &input, &cfg).await.unwrap();

        // Check the configuration is read back unchanged
        let vote = db.get_vote(vote_id).await.unwrap().unwrap();
        assert_eq!(vote.cfg, cfg);
    }
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_store_vote_superseded_steps_stay_done_after_the_new_vote_is_cancelled() {
    // Setup database with a closed vote with its closing steps pending
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let closed_vote_id = test_db
        .seed_vote(SeedVote {
            close_pending: true,
            closed: true,
            ..Default::default()
        })
        .await;

    // Store a new vote in the same issue, and cancel and delete it
    let input = setup_test_create_vote_input();
    let new_vote_id = db.store_vote(COMMENT_ID2, &input, &CfgProfile::default()).await.unwrap();
    db.cancel_vote(REPOFN, ISSUE_NUM, USER1).await.unwrap().unwrap();
    db.complete_vote_cancellation(new_vote_id).await.unwrap();

    // Check the closed vote still has its check run and labels steps done, even though
    // the vote that superseded them does not exist anymore
    assert_eq!(test_db.read_vote(new_vote_id).await, None);
    let closed_vote = db.get_vote(closed_vote_id).await.unwrap().unwrap();
    assert!(closed_vote.close_pending);
    assert!(closed_vote.close_check_run_created_at.is_some());
    assert!(closed_vote.close_labels_updated_at.is_some());
    assert_eq!(state_steps_pending(&db, closed_vote_id).await, [false, false]);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_store_vote_supersedes_state_steps_of_older_votes() {
    // Setup database with votes in the same issue: one with its closing steps pending,
    // one with its check run done already and one closed before steps were tracked
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let pending_vote_id = test_db
        .seed_vote(SeedVote {
            close_pending: true,
            closed: true,
            created_ago: "3 days",
            ..Default::default()
        })
        .await;
    let check_run_done_vote_id = test_db
        .seed_vote(SeedVote {
            close_pending: true,
            closed: true,
            ..Default::default()
        })
        .await;
    db.record_vote_close_step(check_run_done_vote_id, VoteCloseStep::CheckRun).await.unwrap();
    let check_run_done_vote = db.get_vote(check_run_done_vote_id).await.unwrap().unwrap();
    let untracked_vote_id = test_db
        .seed_vote(SeedVote {
            closed: true,
            created_ago: "4 days",
            ..Default::default()
        })
        .await;

    // Setup votes with their closing steps pending in another issue and repository
    let other_issue_vote_id = test_db
        .seed_vote(SeedVote {
            close_pending: true,
            closed: true,
            issue_number: 2,
            ..Default::default()
        })
        .await;
    let other_repo_vote_id = test_db
        .seed_vote(SeedVote {
            close_pending: true,
            closed: true,
            repository_full_name: OTHER_REPOFN,
            ..Default::default()
        })
        .await;
    let unaffected_votes = [
        (untracked_vote_id, test_db.read_vote(untracked_vote_id).await),
        (other_issue_vote_id, test_db.read_vote(other_issue_vote_id).await),
        (other_repo_vote_id, test_db.read_vote(other_repo_vote_id).await),
    ];

    // Store a new vote in the issue
    let input = setup_test_create_vote_input();
    let new_vote_id = db.store_vote(COMMENT_ID2, &input, &CfgProfile::default()).await.unwrap();

    // Check only the check run and labels steps of the pending vote are marked as done
    let pending_vote = db.get_vote(pending_vote_id).await.unwrap().unwrap();
    assert!(pending_vote.close_pending);
    assert_eq!(pending_vote.close_announcement_posted_at, None);
    assert!(pending_vote.close_check_run_created_at.is_some());
    assert_eq!(pending_vote.close_comment_posted_at, None);
    assert!(pending_vote.close_labels_updated_at.is_some());

    // Check the steps done already keep the time they were done
    let vote = db.get_vote(check_run_done_vote_id).await.unwrap().unwrap();
    assert_eq!(
        vote.close_check_run_created_at,
        check_run_done_vote.close_check_run_created_at
    );
    assert!(vote.close_labels_updated_at.is_some());

    // Check the other votes did not change, nor did the new one
    for (vote_id, vote) in unaffected_votes {
        assert_eq!(test_db.read_vote(vote_id).await, vote);
    }
    let new_vote = db.get_vote(new_vote_id).await.unwrap().unwrap();
    assert_eq!(new_vote.close_check_run_created_at, None);
    assert_eq!(new_vote.close_labels_updated_at, None);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_store_vote_without_organization() {
    // Setup database and input
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let input = CreateVoteInput {
        organization: None,
        ..setup_test_create_vote_input()
    };

    // Store the vote
    let vote_id = db.store_vote(COMMENT_ID, &input, &CfgProfile::default()).await.unwrap();

    // Check the vote does not have an organization
    let vote = db.get_vote(vote_id).await.unwrap().unwrap();
    assert_eq!(vote.organization, None);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_undecodable_cfg_returns_errors() {
    // Setup database with a finished vote without the required cfg fields
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            cfg: json!({"close_on_passing": true}),
            ends_in: "-1 minute",
            ..Default::default()
        })
        .await;

    // Check all the reads return the decoding error instead of panicking
    assert_decode_error(&db.claim_vote_to_close().await.unwrap_err(), CFG_DECODE_ERROR);
    assert_decode_error(
        &db.get_open_vote(REPOFN, ISSUE_NUM).await.unwrap_err(),
        CFG_DECODE_ERROR,
    );
    assert_decode_error(
        &db.get_open_votes_with_close_on_passing().await.unwrap_err(),
        CFG_DECODE_ERROR,
    );
    assert_decode_error(&db.get_vote(vote_id).await.unwrap_err(), CFG_DECODE_ERROR);
    assert_decode_error(&db.list_votes(REPOFN).await.unwrap_err(), CFG_DECODE_ERROR);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_undecodable_results_return_errors() {
    // Setup database with a closed vote with results in an unknown format
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            closed: true,

            results: Some(json!({"unknown": true})),
            ..Default::default()
        })
        .await;

    // Check the reads return the decoding error instead of panicking
    assert_decode_error(&db.get_vote(vote_id).await.unwrap_err(), RESULTS_DECODE_ERROR);
    assert_decode_error(&db.list_votes(REPOFN).await.unwrap_err(), RESULTS_DECODE_ERROR);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_update_vote_ends_at_does_not_move_the_end_of_a_finished_vote() {
    // Setup database with a finished vote waiting to be closed
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            ends_in: "-1 hour",
            ..Default::default()
        })
        .await;
    let vote = db.get_vote(vote_id).await.unwrap();

    // Update its ending timestamp
    db.update_vote_ends_at(vote_id).await.unwrap();

    // Check it did not change, so reactions after the end are still ignored
    assert_eq!(db.get_vote(vote_id).await.unwrap(), vote);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_update_vote_ends_at_finishes_only_the_vote_provided() {
    // Setup database with two open votes
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db.seed_vote(SeedVote::default()).await;
    let other_vote_id = test_db
        .seed_vote(SeedVote {
            issue_number: 2,
            ..Default::default()
        })
        .await;
    let other_vote = db.get_vote(other_vote_id).await.unwrap();
    assert_eq!(db.claim_vote_to_close().await.unwrap(), None);

    // Update the ending timestamp of one of the votes
    db.update_vote_ends_at(vote_id).await.unwrap();

    // Check only that vote can be closed now, and the other one did not change
    assert_eq!(db.claim_vote_to_close().await.unwrap().unwrap().vote_id, vote_id);
    assert_eq!(db.claim_vote_to_close().await.unwrap(), None);
    assert_eq!(db.get_vote(other_vote_id).await.unwrap(), other_vote);
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_update_vote_last_check_updates_only_the_vote_provided() {
    // Setup database with two votes due a status check
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let cfg = merge_cfg(&json!({"periodic_status_check": "1 day"}));
    let vote_id = test_db
        .seed_vote(SeedVote {
            cfg: cfg.clone(),
            ..Default::default()
        })
        .await;
    let other_vote_id = test_db
        .seed_vote(SeedVote {
            cfg,
            issue_number: 2,
            ..Default::default()
        })
        .await;
    assert_eq!(db.get_pending_status_checks().await.unwrap().len(), 2);

    // Update the last check of one of the votes
    db.update_vote_last_check(vote_id).await.unwrap();

    // Check only the other vote is still due a status check
    let inputs = db.get_pending_status_checks().await.unwrap();
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0].issue_number, 2);
    assert!(db.get_vote(vote_id).await.unwrap().unwrap().checked_at.is_some());
    assert_eq!(
        db.get_vote(other_vote_id).await.unwrap().unwrap().checked_at,
        None
    );
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_vote_with_json_null_results_has_no_results() {
    // Setup database with a closed vote with JSON null results
    let test_db = TestDB::new().await;
    let db = test_db.db();
    let vote_id = test_db
        .seed_vote(SeedVote {
            closed: true,

            results: Some(Value::Null),
            ..Default::default()
        })
        .await;
    assert!(!test_db.results_are_sql_null(vote_id).await);

    // Check the vote can be read
    let vote = db.get_vote(vote_id).await.unwrap().unwrap();
    assert!(vote.closed);
    assert_eq!(vote.results, None);
    assert_eq!(db.list_votes(REPOFN).await.unwrap(), vec![vote]);
}

// Helpers.

/// Checks the delay until the next attempt is the one expected, allowing for the time
/// elapsed since it was set.
fn assert_close_retry_delay(delay: f64, expected: f64) {
    let range = expected - CLOSE_RETRY_DELAY_TOLERANCE_SECS..=expected;
    assert!(range.contains(&delay), "{delay} not in {range:?}");
}

/// Checks the error provided is the one returned when a stored column cannot be decoded.
fn assert_decode_error(err: &Error, expected_cause: &str) {
    let err = err.downcast_ref::<tokio_postgres::Error>().expect("database error");
    assert!(err.to_string().starts_with("error deserializing column"), "{err}");
    let cause = err.source().expect("error should have a cause").to_string();
    assert!(cause.contains(expected_cause), "{cause}");
}

/// Claims the next vote with closing work due, which must exist, returning the time its
/// claim ends at.
async fn claim_vote_to_close_until(db: &PgDB) -> OffsetDateTime {
    let vote = db.claim_vote_to_close().await.unwrap().expect("vote to close");
    vote.close_next_attempt_at.expect("claimed vote should have a claim")
}

/// Adds the fields provided to a vote configuration profile with the required ones.
fn merge_cfg(fields: &Value) -> Value {
    let mut cfg = setup_seed_cfg();
    cfg.as_object_mut().unwrap().extend(fields.as_object().unwrap().clone());
    cfg
}

/// Sets up the input to create a vote, with a distinct value for each field.
fn setup_test_create_vote_input() -> CreateVoteInput {
    CreateVoteInput {
        created_by: USER.to_string(),
        installation_id: INST_ID as i64,
        is_automatic: false,
        is_pull_request: true,
        issue_id: ISSUE_ID,
        issue_number: ISSUE_NUM,
        issue_title: TITLE.to_string(),
        repository_full_name: REPOFN.to_string(),

        opened_pr_base_branch: None,
        organization: Some(ORG.to_string()),
        profile_name: Some(PROFILE_NAME.to_string()),
    }
}

/// Sets up a vote configuration profile with all the optional sections set.
fn setup_test_full_cfg_profile() -> CfgProfile {
    CfgProfile {
        duration: Duration::from_hours(1),
        pass_threshold: 66.5,

        allowed_voters: Some(AllowedVoters {
            exclude_team_maintainers: Some(true),
            teams: Some(vec![TEAM1.to_string()]),
            users: Some(vec![USER1.to_string()]),
        }),
        announcements: Some(Announcements {
            discussions: Some(DiscussionsAnnouncements {
                category: DISCUSSIONS_CATEGORY.to_string(),
            }),
        }),
        close_on_passing: Some(true),
        close_on_passing_min_wait: Some("2 days".to_string()),
        pass_threshold_base: Some(PassThresholdBase::VotesCast {
            exclude_abstentions: Some(true),
        }),
        periodic_status_check: Some("1 week".to_string()),
    }
}

/// Checks if the check run and labels steps of the vote provided are pending, in that
/// order.
async fn state_steps_pending(db: &PgDB, vote_id: Uuid) -> [bool; 2] {
    let check_run = db.is_close_state_step_pending(vote_id, VoteCloseStep::CheckRun).await.unwrap();
    let labels = db.is_close_state_step_pending(vote_id, VoteCloseStep::Labels).await.unwrap();
    [check_run, labels]
}
