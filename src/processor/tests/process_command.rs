use anyhow::format_err;
use mockall::{Sequence, predicate::eq};

use crate::{db::MockDB, github::*, testutil::*};

use super::*;

#[tokio::test]
async fn process_command_cancel_vote_error_cancelling_posts_failure_comment() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_cancel_vote()
        .withf(|repository_full_name, issue_number, cancelled_by| {
            repository_full_name == REPOFN && *issue_number == ISSUE_NUM && cancelled_by == USER
        })
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Setup GitHub expectations: nothing is published but the failure comment
    let mut gh = MockGH::new();
    expect_user_is_collaborator(&mut gh, true);
    expect_command_failed_comment(&mut gh, || Ok(COMMENT_ID));
    gh.expect_create_check_run().never();
    gh.expect_remove_label().never();

    // Run the command
    let input = CancelVoteInput::new(&Event::PullRequest(setup_test_pr_event()));
    setup_cmds_handler(db, gh).process_command(Command::CancelVote(input)).await;
}

#[tokio::test]
async fn process_command_cancel_vote_failed_posts_comment() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_user_is_collaborator()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    expect_command_failed_comment(&mut gh, || Ok(COMMENT_ID));

    // Run the command
    let event = Event::IssueComment(setup_test_issue_comment_event());
    let cmd = Command::CancelVote(CancelVoteInput::new(&event));
    setup_cmds_handler(MockDB::new(), gh).process_command(cmd).await;
}

#[tokio::test]
async fn process_command_cancel_vote_publish_failed_does_not_post_failure_comment() {
    // Setup cases: outcome of posting the comment, creating the check run and recording
    // the labels step. The vote is cancelled already, so trying again would not help
    type Outcome = fn() -> Result<()>;
    let ok: Outcome = || Ok(());
    let err: Outcome = || Err(format_err!(ERROR));
    let cases = [(err, ok, ok), (ok, err, ok), (ok, ok, err)];

    for (comment_result, check_run_result, labels_step_result) in cases {
        // Setup database expectations
        let mut db = MockDB::new();
        expect_cancel_vote(&mut db, Some(setup_test_cancelled_vote(true)));
        expect_state_steps_pending(&mut db, &[VoteCloseStep::CheckRun, VoteCloseStep::Labels], true);
        let recorded_steps = [
            (VoteCloseStep::Comment, comment_result().is_ok()),
            (VoteCloseStep::CheckRun, check_run_result().is_ok()),
        ];
        for (step, recorded) in recorded_steps {
            db.expect_record_vote_close_step()
                .with(eq(test_vote_id()), eq(step))
                .times(usize::from(recorded))
                .returning(|_, _| Box::pin(future::ready(Ok(()))));
        }
        db.expect_record_vote_close_step()
            .with(eq(test_vote_id()), eq(VoteCloseStep::Labels))
            .times(1)
            .returning(move |_, _| Box::pin(future::ready(labels_step_result())));
        db.expect_complete_vote_cancellation().never();
        expect_close_failure(&mut db, VoteCloseFailureOutcome::RetryScheduled);

        // Setup GitHub expectations
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
            .returning(move |_, _, _, _, _| Box::pin(future::ready(comment_result().map(|()| COMMENT_ID))));
        let failed_body = tmpl::CommandFailed {}.render().unwrap();
        gh.expect_post_comment().withf(move |_, _, _, _, body| body == failed_body).never();
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

        // Run and check the expectations are met
        let input = CancelVoteInput::new(&Event::PullRequest(setup_test_pr_event()));
        setup_cmds_handler(db, gh).process_command(Command::CancelVote(input)).await;
    }
}

#[tokio::test]
async fn process_command_check_vote_error_updating_last_check_posts_no_failure_comment() {
    // Setup database expectations: the last check cannot be updated
    let mut db = MockDB::new();
    db.expect_get_open_vote()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(Some(setup_test_vote())))));
    db.expect_update_vote_last_check()
        .with(eq(test_vote_id()))
        .times(1)
        .returning(|_| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Setup GitHub expectations: only the vote status comment is posted
    let mut gh = MockGH::new();
    expect_vote_results(&mut gh, COMMENT_ID, Ok(vec![in_favor_reaction(USER1)]));
    expect_comment(
        &mut gh,
        tmpl::VoteStatus::new(&setup_test_vote_results()).render().unwrap(),
    );
    expect_command_failed_comment_never(&mut gh);

    // Run the command
    let event = Event::IssueComment(setup_test_issue_comment_event());
    let cmd = Command::CheckVote(CheckVoteInput::new(&event));
    setup_cmds_handler(db, gh).process_command(cmd).await;
}

#[tokio::test]
async fn process_command_check_vote_failed_posts_comment() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_get_open_vote()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    expect_command_failed_comment(&mut gh, || Ok(COMMENT_ID));

    // Run the command
    let event = Event::IssueComment(setup_test_issue_comment_event());
    let cmd = Command::CheckVote(CheckVoteInput::new(&event));
    setup_cmds_handler(db, gh).process_command(cmd).await;
}

#[tokio::test]
async fn process_command_create_vote_automatic_failed_reports_automation_failure() {
    // Each case sets up one of the failures of a command that nobody ran
    let test_cases: [fn(&mut MockDB, &mut MockGH); 3] = [
        // Configuration lookup
        |_, gh| {
            gh.expect_get_config_file()
                .with(eq(INST_ID), eq(ORG), eq(REPO))
                .times(1)
                .returning(|_, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
        },
        // Collaborator lookup
        |_, gh| {
            expect_valid_config(gh);
            gh.expect_user_is_collaborator()
                .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
                .times(1)
                .returning(|_, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
        },
        // Open vote lookup
        |db, gh| {
            expect_valid_config(gh);
            expect_user_is_collaborator(gh, true);
            db.expect_has_vote_open()
                .with(eq(REPOFN), eq(ISSUE_NUM))
                .times(1)
                .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
        },
    ];
    for setup_failure in test_cases {
        // Setup expectations: the only comment posted and the only check reported, as
        // the pull request has no vote
        let mut db = MockDB::new();
        let mut gh = MockGH::new();
        setup_failure(&mut db, &mut gh);
        expect_has_vote(&mut db, Ok(false));
        gh.expect_is_check_required().never();
        expect_check_run(&mut gh, cmd::automation_failed_check());
        expect_comment(&mut gh, tmpl::AutomationFailed::new(None).render().unwrap());

        // Run the command
        let event = Event::PullRequest(setup_test_pr_event());
        let cmd = Command::CreateVote(CreateVoteInput::new_automatic(PROFILE_NAME, &event));
        setup_cmds_handler(db, gh).process_command(cmd).await;
    }
}

#[tokio::test]
async fn process_command_create_vote_automatic_failed_with_vote_reports_nothing() {
    // Setup expectations: the configuration lookup fails on a pull request with a vote
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Ok(true));
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    gh.expect_create_check_run().never();
    gh.expect_post_comment().never();

    // Run the command
    let event = Event::PullRequest(setup_test_pr_event());
    let cmd = Command::CreateVote(CreateVoteInput::new_automatic(PROFILE_NAME, &event));
    setup_cmds_handler(db, gh).process_command(cmd).await;
}

#[tokio::test]
async fn process_command_create_vote_failed_on_issue_posts_comment() {
    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    expect_command_failed_comment(&mut gh, || Ok(COMMENT_ID));
    gh.expect_create_check_run().never();

    // Run the command
    let event = Event::Issue(setup_test_issue_event());
    let cmd = Command::CreateVote(CreateVoteInput::new(None, &event));
    setup_cmds_handler(MockDB::new(), gh).process_command(cmd).await;
}

#[tokio::test]
async fn process_command_create_vote_failed_on_opened_pr_error_checking_vote_reports_check() {
    // Setup GitHub expectations: the check is reported even if the vote is unknown
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    expect_command_failed_comment(&mut gh, || Ok(COMMENT_ID));
    expect_check_run(&mut gh, cmd::vote_not_created_check());

    // Setup database expectations: the vote lookup fails
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Err(format_err!(ERROR)));

    // Run the command
    let event = Event::PullRequest(setup_test_pr_event());
    let cmd = Command::CreateVote(CreateVoteInput::new(None, &event));
    setup_cmds_handler(db, gh).process_command(cmd).await;
}

#[tokio::test]
async fn process_command_create_vote_failed_on_opened_pr_reports_failure_check() {
    // Setup GitHub expectations: the check must be tried even if the comment fails
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    expect_command_failed_comment(&mut gh, || Err(format_err!(ERROR)));
    gh.expect_is_check_required().never();
    expect_check_run(&mut gh, cmd::vote_not_created_check());

    // Setup database expectations: the pull request has no vote
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Ok(false));

    // Run the command
    let event = Event::PullRequest(setup_test_pr_event());
    let cmd = Command::CreateVote(CreateVoteInput::new(None, &event));
    setup_cmds_handler(db, gh).process_command(cmd).await;
}

#[tokio::test]
async fn process_command_create_vote_failed_on_opened_pr_with_vote_reports_no_check() {
    // Setup GitHub expectations: the comment is posted, but the vote owns the check
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    expect_command_failed_comment(&mut gh, || Ok(COMMENT_ID));
    gh.expect_create_check_run().never();

    // Setup database expectations: the pull request has a vote
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Ok(true));

    // Run the command
    let event = Event::PullRequest(setup_test_pr_event());
    let cmd = Command::CreateVote(CreateVoteInput::new(None, &event));
    setup_cmds_handler(db, gh).process_command(cmd).await;
}

#[tokio::test]
async fn process_command_create_vote_pr_check_run_failed_does_not_ask_to_try_again() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_has_vote_open()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(false))));
    expect_store_vote(&mut db, true, || Ok(test_vote_id()));

    // Setup GitHub expectations: the vote created comment is posted, creating the check
    // run fails, the labels are updated anyway, and no failure is reported
    let mut gh = MockGH::new();
    let mut seq = Sequence::new();
    expect_valid_config(&mut gh);
    expect_user_is_collaborator(&mut gh, true);
    expect_vote_created_comment(&mut gh, &mut seq, true);
    expect_command_failed_comment_never(&mut gh);
    gh.expect_create_check_run()
        .with(
            eq(INST_ID),
            eq(ORG),
            eq(REPO),
            eq(ISSUE_NUM),
            eq(CheckDetails {
                status: "in_progress".to_string(),
                summary: "Vote open".to_string(),

                conclusion: None,
            }),
        )
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    expect_vote_open_labels(&mut gh);

    // Run the command
    let event = Event::PullRequest(setup_test_pr_event());
    let cmd = Command::CreateVote(CreateVoteInput::new(None, &event));
    setup_cmds_handler(db, gh).process_command(cmd).await;
}

#[tokio::test]
async fn process_command_create_vote_setup_failed_does_not_ask_to_try_again() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_has_vote_open()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(false))));
    expect_store_vote(&mut db, false, || Ok(test_vote_id()));

    // Setup GitHub expectations: only the vote created comment is posted
    let mut gh = MockGH::new();
    let mut seq = Sequence::new();
    expect_valid_config(&mut gh);
    expect_user_is_collaborator(&mut gh, true);
    expect_vote_created_comment(&mut gh, &mut seq, false);
    expect_command_failed_comment_never(&mut gh);
    for label in [VOTE_CLOSED_LABEL, VOTE_PASSED_LABEL, VOTE_FAILED_LABEL] {
        gh.expect_remove_label()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM), eq(label))
            .times(1)
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
    }
    gh.expect_add_labels()
        .withf(|inst_id, owner, repo, issue_number, labels| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && labels == vec![GITVOTE_LABEL, VOTE_OPEN_LABEL]
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Run the command
    let event = Event::Issue(setup_test_issue_event());
    let cmd = Command::CreateVote(CreateVoteInput::new(None, &event));
    setup_cmds_handler(db, gh).process_command(cmd).await;
}

#[tokio::test]
async fn process_command_create_vote_stored_despite_error_sets_up_vote() {
    // Setup database expectations: the vote was stored despite the error
    let mut db = MockDB::new();
    db.expect_has_vote_open()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(false))));
    expect_store_vote(&mut db, true, || Err(format_err!(ERROR)));
    expect_has_vote_with_comment(&mut db, Ok(true));
    db.expect_has_vote().never();

    // Setup GitHub expectations: the vote is set up as open and no failure is reported
    let mut gh = MockGH::new();
    let mut seq = Sequence::new();
    expect_valid_config(&mut gh);
    expect_user_is_collaborator(&mut gh, true);
    expect_vote_created_comment(&mut gh, &mut seq, true);
    expect_command_failed_comment_never(&mut gh);
    gh.expect_delete_comment().never();
    expect_check_run(
        &mut gh,
        CheckDetails {
            status: "in_progress".to_string(),
            summary: "Vote open".to_string(),

            conclusion: None,
        },
    );
    expect_vote_open_labels(&mut gh);

    // Run the command
    let event = Event::PullRequest(setup_test_pr_event());
    let cmd = Command::CreateVote(CreateVoteInput::new(None, &event));
    setup_cmds_handler(db, gh).process_command(cmd).await;
}

#[tokio::test]
async fn process_command_periodic_check_vote_failed_posts_no_comment() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_get_open_vote()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_post_comment().never();

    // Run the command, as queued by the status checker
    let cmd = Command::CheckVote(CheckVoteInput {
        issue_number: ISSUE_NUM,
        repository_full_name: REPOFN.to_string(),

        installation_id: None,
    });
    setup_cmds_handler(db, gh).process_command(cmd).await;
}
