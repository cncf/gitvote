use std::sync::Arc;

use anyhow::format_err;
use mockall::predicate::eq;
use time::ext::NumericalDuration;

use crate::{
    db::MockDB,
    github::*,
    results::{REACTION_IN_FAVOR, Vote},
    testutil::*,
};

use super::*;

#[tokio::test]
async fn check_vote_checked_long_ago() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_get_open_vote().with(eq(REPOFN), eq(ISSUE_NUM)).times(1).returning(|_, _| {
        Box::pin(future::ready(Ok(Some(Vote {
            checked_at: OffsetDateTime::now_utc().checked_sub(25.hours()),
            ..setup_test_vote()
        }))))
    });
    db.expect_update_vote_last_check()
        .with(eq(test_vote_id()))
        .times(1)
        .returning(|_| Box::pin(future::ready(Ok(()))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_vote_results(&mut gh, COMMENT_ID, Ok(vec![in_favor_reaction(USER1)]));
    gh.expect_post_comment()
        .withf(|inst_id, owner, repo, issue_number, body| {
            let results = setup_test_vote_results();
            let expected_body = tmpl::VoteStatus::new(&results).render().unwrap();
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body == expected_body.as_str()
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));

    // Run and check the vote status is posted
    let input = CheckVoteInput::new(&Event::PullRequest(setup_test_pr_event()));
    setup_cmds_handler(db, gh).check_vote(&input).await.unwrap();
}

#[tokio::test]
async fn check_vote_checked_recently() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_get_open_vote().with(eq(REPOFN), eq(ISSUE_NUM)).times(1).returning(|_, _| {
        Box::pin(future::ready(Ok(Some(Vote {
            checked_at: OffsetDateTime::now_utc().checked_sub(1.hours()),
            ..setup_test_vote()
        }))))
    });

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_post_comment()
        .withf(|inst_id, owner, repo, issue_number, body| {
            let expected_body = tmpl::VoteCheckedRecently {}.render().unwrap();
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body == expected_body.as_str()
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));

    // Run and check the recently checked comment is posted
    let (_, cmds_rx) = async_channel::unbounded();
    let event = setup_test_pr_event();
    let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
    cmds_handler.check_vote(&CheckVoteInput::new(&Event::PullRequest(event))).await.unwrap();
}

#[tokio::test]
async fn check_vote_error_calculating_results() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_get_open_vote()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(Some(setup_test_vote())))));
    db.expect_update_vote_last_check().never();

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_comment_reactions()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(COMMENT_ID))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    gh.expect_post_comment().never();

    // Run and check the error is propagated
    let input = CheckVoteInput::new(&Event::PullRequest(setup_test_pr_event()));
    let err = setup_cmds_handler(db, gh).check_vote(&input).await.unwrap_err();
    assert_eq!(err.to_string(), ERROR);
}

#[tokio::test]
async fn check_vote_error_getting_vote() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_get_open_vote()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    db.expect_update_vote_last_check().never();

    // Setup GitHub expectations: no status is posted
    let mut gh = MockGH::new();
    gh.expect_post_comment().never();

    // Run and check the error is propagated
    let input = CheckVoteInput::new(&Event::PullRequest(setup_test_pr_event()));
    let err = setup_cmds_handler(db, gh).check_vote(&input).await.unwrap_err();
    assert_eq!(err.to_string(), "error getting open vote");
    assert_eq!(err.root_cause().to_string(), ERROR);
}

#[tokio::test]
async fn check_vote_error_updating_last_check_is_ignored() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_get_open_vote()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(Some(setup_test_vote())))));
    db.expect_update_vote_last_check()
        .with(eq(test_vote_id()))
        .times(1)
        .returning(|_| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_vote_results(&mut gh, COMMENT_ID, Ok(vec![in_favor_reaction(USER1)]));
    gh.expect_post_comment()
        .withf(|inst_id, owner, repo, issue_number, body| {
            let results = setup_test_vote_results();
            let expected_body = tmpl::VoteStatus::new(&results).render().unwrap();
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body == expected_body.as_str()
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));

    // Run and check the status posted makes the check succeed
    let input = CheckVoteInput::new(&Event::PullRequest(setup_test_pr_event()));
    setup_cmds_handler(db, gh).check_vote(&input).await.unwrap();
}

#[tokio::test]
async fn check_vote_not_found() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_get_open_vote()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(None))));
    let gh = MockGH::new();

    // Run and check the missing vote is ignored
    let (_, cmds_rx) = async_channel::unbounded();
    let event = setup_test_pr_event();
    let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
    cmds_handler.check_vote(&CheckVoteInput::new(&Event::PullRequest(event))).await.unwrap();
}

#[tokio::test]
async fn check_vote_success() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_get_open_vote()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(Some(setup_test_vote())))));
    db.expect_update_vote_last_check()
        .with(eq(test_vote_id()))
        .times(1)
        .returning(|_| Box::pin(future::ready(Ok(()))));

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
    gh.expect_post_comment()
        .withf(|inst_id, owner, repo, issue_number, body| {
            let results = setup_test_vote_results();
            let expected_body = tmpl::VoteStatus::new(&results).render().unwrap();
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body == expected_body.as_str()
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));

    // Run and check the vote status is posted
    let (_, cmds_rx) = async_channel::unbounded();
    let event = setup_test_pr_event();
    let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
    cmds_handler.check_vote(&CheckVoteInput::new(&Event::PullRequest(event))).await.unwrap();
}
