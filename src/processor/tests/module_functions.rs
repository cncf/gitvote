use anyhow::format_err;
use tokio::time::Instant;

use crate::testutil::*;

use super::*;

#[test]
fn build_announcement_title_includes_issue_number() {
    assert_eq!(
        build_announcement_title(ISSUE_NUM, TITLE),
        "Test title #1 (vote closed)"
    );
}

#[tokio::test(start_paused = true)]
async fn run_close_attempt_fails_once_the_attempt_times_out() {
    // Run an attempt that never completes
    let started_at = Instant::now();
    let err = run_close_attempt(future::pending()).await.unwrap_err();

    // Check it failed after the attempt timeout
    assert_eq!(err.to_string(), "attempt timed out after 1200 seconds");
    assert_eq!(started_at.elapsed(), CLOSE_ATTEMPT_TIMEOUT);
}

#[tokio::test]
async fn run_close_attempt_returns_the_attempt_outcome() {
    assert!(run_close_attempt(future::ready(Ok(()))).await.is_ok());
    let err = run_close_attempt(future::ready(Err(format_err!(ERROR)))).await.unwrap_err();
    assert_eq!(err.to_string(), ERROR);
}
