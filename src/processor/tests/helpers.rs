use std::{ops::RangeInclusive, sync::Arc};

use anyhow::format_err;
use mockall::{Sequence, predicate::eq};
use time::{ext::NumericalDuration, format_description::well_known::Rfc3339};
use uuid::Uuid;

use crate::{
    cfg_repo::AllowedVoters,
    db::MockDB,
    github::*,
    results::{REACTION_IN_FAVOR, Vote},
    testutil::*,
};

use super::*;

/// Returns the details of the check run of a cancelled vote.
pub(super) fn cancelled_check_details() -> CheckDetails {
    CheckDetails {
        status: "completed".to_string(),
        summary: "Vote cancelled".to_string(),

        conclusion: Some("success".to_string()),
    }
}

/// Expects the allowed voters of the test vote to be looked up once, returning only
/// `USER1`.
pub(super) fn expect_allowed_voters(gh: &mut MockGH) {
    gh.expect_get_allowed_voters()
        .withf(|inst_id, cfg, owner, repo, org| {
            *inst_id == INST_ID
                && *cfg == setup_test_vote().cfg
                && owner == ORG
                && repo == REPO
                && *org == Some(ORG.to_string()).as_ref()
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(vec![USER1.to_string()]))));
}

/// Expects the results announcement of the vote provided to be created once in the test
/// discussions category, in the sequence given, with the result provided.
pub(super) fn expect_announcement_discussion(
    gh: &mut MockGH,
    seq: &mut Sequence,
    vote_id: Uuid,
    result: fn() -> Result<()>,
) {
    let expected_marker = tmpl::VoteMarker::ResultsAnnouncement.render(vote_id);
    gh.expect_create_discussion()
        .withf(move |inst_id, owner, repo, category, _, body| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && category == DISCUSSIONS_CATEGORY
                && body.contains(&expected_marker)
        })
        .times(1)
        .in_sequence(seq)
        .returning(move |_, _, _, _, _, _| Box::pin(future::ready(result())));
}

/// Expects a lookup of the results announcement of the vote provided in the test
/// discussions category once, from a time in the range given, with the result provided.
pub(super) fn expect_announcement_lookup(
    gh: &mut MockGH,
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
        .returning(move |_, _, _, _, _, _| Box::pin(future::ready(result())));
}

/// Expects the open vote of the test issue to be cancelled once by the test user,
/// returning the cancelled vote provided (none when there is no open vote).
pub(super) fn expect_cancel_vote(db: &mut MockDB, vote: Option<Vote>) {
    db.expect_cancel_vote()
        .withf(|repository_full_name, issue_number, cancelled_by| {
            repository_full_name == REPOFN && *issue_number == ISSUE_NUM && cancelled_by == USER
        })
        .times(1)
        .return_once(move |_, _, _| Box::pin(future::ready(Ok(vote))));
}

/// Expects the vote provided to be claimed once, until the end of the test claim, as
/// claiming a vote sets it.
pub(super) fn expect_claim_vote(db: &mut MockDB, mut vote: Vote) {
    vote.close_next_attempt_at = Some(test_claimed_until());
    db.expect_claim_vote_to_close()
        .times(1)
        .return_once(move || Box::pin(future::ready(Ok(Some(vote)))));
}

/// Expects a failed attempt to close the test vote to be recorded once with the test
/// claim, with the outcome provided.
pub(super) fn expect_close_failure(db: &mut MockDB, outcome: VoteCloseFailureOutcome) {
    db.expect_record_vote_close_failure()
        .with(eq(test_vote_id()), eq(test_claimed_until()))
        .times(1)
        .returning(move |_, _| Box::pin(future::ready(Ok(outcome))));
}

/// Expects the labels provided to be added to the test issue once, closing its vote, in
/// the sequence given, with the result provided.
pub(super) fn expect_close_labels(
    gh: &mut MockGH,
    seq: &mut Sequence,
    expected_labels: &'static [&'static str],
    result: fn() -> Result<()>,
) {
    gh.expect_add_labels()
        .withf(move |inst_id, owner, repo, issue_number, labels| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && labels == expected_labels
        })
        .times(1)
        .in_sequence(seq)
        .returning(move |_, _, _, _, _| Box::pin(future::ready(result())));
}

/// Expects each of the closing steps provided to be recorded once for the test vote.
pub(super) fn expect_close_steps(db: &mut MockDB, steps: &[VoteCloseStep]) {
    for step in steps {
        db.expect_record_vote_close_step()
            .with(eq(test_vote_id()), eq(*step))
            .times(1)
            .returning(|_, _| Box::pin(future::ready(Ok(()))));
    }
}

/// Expects the command failed comment to be posted once, with the result provided.
pub(super) fn expect_command_failed_comment(gh: &mut MockGH, result: fn() -> Result<i64>) {
    let expected_body = tmpl::CommandFailed {}.render().unwrap();
    gh.expect_post_comment()
        .withf(move |inst_id, owner, repo, issue_number, body| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body == expected_body
        })
        .times(1)
        .returning(move |_, _, _, _, _| Box::pin(future::ready(result())));
}

/// Expects the command failed comment never to be posted.
pub(super) fn expect_command_failed_comment_never(gh: &mut MockGH) {
    let expected_body = tmpl::CommandFailed {}.render().unwrap();
    gh.expect_post_comment().withf(move |_, _, _, _, body| body == expected_body).never();
}

/// Expects a comment with the body provided to be posted once.
pub(super) fn expect_comment(gh: &mut MockGH, expected_body: String) {
    gh.expect_post_comment()
        .withf(move |inst_id, owner, repo, issue_number, body| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body == expected_body
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
}

/// Expects a comment with the body provided to be posted once, in the sequence given,
/// with the result provided.
pub(super) fn expect_comment_in_sequence(
    gh: &mut MockGH,
    seq: &mut Sequence,
    expected_body: String,
    result: fn() -> Result<i64>,
) {
    gh.expect_post_comment()
        .withf(move |inst_id, owner, repo, issue_number, body| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body == expected_body
        })
        .times(1)
        .in_sequence(seq)
        .returning(move |_, _, _, _, _| Box::pin(future::ready(result())));
}

/// Expects a lookup of the comment with the marker provided of the vote given on the
/// test issue once, from a time in the range provided, with the result given.
pub(super) fn expect_comment_lookup(
    gh: &mut MockGH,
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
        .returning(move |_, _, _, _, _, _| Box::pin(future::ready(result())));
}

/// Expects a check run to be created once on the commit provided, with the details
/// given.
pub(super) fn expect_commit_check_run(gh: &mut MockGH, head_sha: &'static str, check_details: CheckDetails) {
    gh.expect_create_commit_check_run()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(head_sha), eq(check_details))
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
}

/// Expects the cancellation of the test vote to be completed once.
pub(super) fn expect_complete_vote_cancellation(db: &mut MockDB) {
    db.expect_complete_vote_cancellation()
        .with(eq(test_vote_id()))
        .times(1)
        .returning(|_| Box::pin(future::ready(Ok(()))));
}

/// Expects the close of the test vote to be marked as complete once.
pub(super) fn expect_complete_vote_close(db: &mut MockDB) {
    db.expect_complete_vote_close()
        .with(eq(test_vote_id()))
        .times(1)
        .returning(|_| Box::pin(future::ready(Ok(()))));
}

/// Expects a lookup of the vote referencing the test comment once, with the result
/// given.
pub(super) fn expect_has_vote_with_comment(db: &mut MockDB, result: Result<bool>) {
    db.expect_has_vote_with_comment()
        .with(eq(COMMENT_ID))
        .times(1)
        .return_once(move |_| Box::pin(future::ready(result)));
}

/// Expects no lookups of what a previous attempt to close or cancel a vote published.
pub(super) fn expect_no_marker_lookups(gh: &mut MockGH) {
    gh.expect_has_comment_with_marker().never();
    gh.expect_has_discussion_with_marker().never();
}

/// Expects the head of the test pull request to be read once, returning the one
/// provided.
pub(super) fn expect_pr_head_sha(gh: &mut MockGH, head_sha: &'static str) {
    gh.expect_get_pr_head_sha()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM))
        .times(1)
        .returning(move |_, _, _, _| Box::pin(future::ready(Ok(head_sha.to_string()))));
}

/// Expects the vote open label to be removed from the test issue once, with the result
/// provided.
pub(super) fn expect_remove_open_label(gh: &mut MockGH, result: fn() -> Result<()>) {
    gh.expect_remove_label()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM), eq(VOTE_OPEN_LABEL))
        .times(1)
        .returning(move |_, _, _, _, _| Box::pin(future::ready(result())));
}

/// Expects a check of whether each of the state steps provided of the test vote is
/// pending, once per step, with the result provided.
pub(super) fn expect_state_steps_pending(db: &mut MockDB, steps: &[VoteCloseStep], is_pending: bool) {
    for step in steps {
        db.expect_is_close_state_step_pending()
            .with(eq(test_vote_id()), eq(*step))
            .times(1)
            .returning(move |_, _| Box::pin(future::ready(Ok(is_pending))));
    }
}

/// Expects the vote created on the test issue or pull request with the default test
/// profile to be stored once, tracking `COMMENT_ID`.
pub(super) fn expect_store_vote(db: &mut MockDB, is_pull_request: bool, result: fn() -> Result<Uuid>) {
    let expected_input = test_create_vote_input(is_pull_request);
    let expected_cfg = test_cfg_profile();
    db.expect_store_vote()
        .withf(move |vote_comment_id, input, cfg| {
            *vote_comment_id == COMMENT_ID && *input == expected_input && *cfg == expected_cfg
        })
        .times(1)
        .returning(move |_, _, _| Box::pin(future::ready(result())));
}

/// Expects the user to be checked as a repository collaborator once.
pub(super) fn expect_user_is_collaborator(gh: &mut MockGH, is_collaborator: bool) {
    gh.expect_user_is_collaborator()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
        .times(1)
        .returning(move |_, _, _, _| Box::pin(future::ready(Ok(is_collaborator))));
}

/// Expects posting the vote cancelled comment of the vote provided to be tried once,
/// failing.
pub(super) fn expect_vote_cancelled_comment_fails(gh: &mut MockGH, vote_id: Uuid, is_pull_request: bool) {
    let expected_body = vote_cancelled_body(vote_id, is_pull_request);
    gh.expect_post_comment()
        .withf(move |inst_id, owner, repo, issue_number, body| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body == expected_body
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
}

/// Expects the results of the vote provided, which are the test ones, to be published
/// once.
pub(super) fn expect_vote_closed_published(gh: &mut MockGH, vote_id: Uuid) {
    let mut seq = Sequence::new();
    expect_comment(gh, vote_closed_body(vote_id));
    expect_announcement_discussion(gh, &mut seq, vote_id, || Ok(()));
    expect_remove_open_label(gh, || Ok(()));
    expect_close_labels(gh, &mut seq, &[VOTE_CLOSED_LABEL, VOTE_PASSED_LABEL], || Ok(()));
}

/// Expects the vote created comment to be posted once, returning `COMMENT_ID`.
pub(super) fn expect_vote_created_comment(gh: &mut MockGH, seq: &mut Sequence, is_pull_request: bool) {
    let expected_body = vote_created_body(is_pull_request);
    gh.expect_post_comment()
        .withf(move |inst_id, owner, repo, issue_number, body| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body == expected_body
        })
        .times(1)
        .in_sequence(seq)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
}

/// Expects the vote in progress comment to be posted once.
pub(super) fn expect_vote_in_progress_comment(gh: &mut MockGH, seq: &mut Sequence, is_pull_request: bool) {
    gh.expect_post_comment()
        .withf(move |inst_id, owner, repo, issue_number, body| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body == vote_in_progress_body(is_pull_request)
        })
        .times(1)
        .in_sequence(seq)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID2))));
}

/// Expects the labels of the test issue to be updated once for a vote just opened.
pub(super) fn expect_vote_open_labels(gh: &mut MockGH) {
    // Expect the labels of a previous vote to be removed
    for label in [VOTE_CLOSED_LABEL, VOTE_PASSED_LABEL, VOTE_FAILED_LABEL] {
        gh.expect_remove_label()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM), eq(label))
            .times(1)
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
    }

    // Expect the labels of the vote just opened to be added
    gh.expect_add_labels()
        .withf(|inst_id, owner, repo, issue_number, labels| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && labels == vec![GITVOTE_LABEL, VOTE_OPEN_LABEL]
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
}

/// Expects the GitHub calls needed to calculate the results of a vote whose
/// allowed voters only include `USER1`.
pub(super) fn expect_vote_results(gh: &mut MockGH, comment_id: i64, reactions: Result<Vec<Reaction>>) {
    gh.expect_get_comment_reactions()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(comment_id))
        .times(1)
        .return_once(move |_, _, _, _| Box::pin(future::ready(reactions)));
    expect_allowed_voters(gh);
}

/// Creates an in favor reaction from the user provided.
pub(super) fn in_favor_reaction(user: &str) -> Reaction {
    Reaction {
        content: REACTION_IN_FAVOR.to_string(),
        created_at: TIMESTAMP.to_string(),
        user: User {
            login: user.to_string(),
        },
    }
}

/// Returns the details of the closing check run of the test vote, which passed.
pub(super) fn passed_check_details() -> CheckDetails {
    CheckDetails {
        status: "completed".to_string(),
        summary: "The vote passed! 1 out of 1 voted in favor.".to_string(),

        conclusion: Some("success".to_string()),
    }
}

/// Sets up a commands handler using the mocks provided.
pub(super) fn setup_cmds_handler(db: MockDB, gh: MockGH) -> CommandsHandler {
    let (_, cmds_rx) = async_channel::unbounded();
    CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx)
}

/// Sets up a vote cancelled by the test user, with the publication of its cancellation
/// pending.
pub(super) fn setup_test_cancelled_vote(is_pull_request: bool) -> Vote {
    Vote {
        close_pending: true,
        closed: true,
        is_pull_request,

        cancelled_at: Some(test_closed_at()),
        cancelled_by: Some(USER.to_string()),
        close_next_attempt_at: Some(test_claimed_until()),
        closed_at: Some(test_closed_at()),
        ..setup_test_vote()
    }
}

/// Sets up a vote closed already, with all its closing side effects pending.
pub(super) fn setup_test_closed_vote(results: Option<VoteResults>) -> Vote {
    Vote {
        close_pending: true,
        closed: true,

        closed_at: Some(test_closed_at()),
        results,
        ..setup_test_vote()
    }
}

/// Returns the time the claim of the votes claimed in the tests ends at.
pub(super) fn test_claimed_until() -> OffsetDateTime {
    OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap() + 30.minutes()
}

/// Returns the time the votes closed or cancelled in the tests were closed at.
pub(super) fn test_closed_at() -> OffsetDateTime {
    OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap()
}

/// Returns the time from which the comments and discussions published by a previous
/// attempt to close or cancel the votes closed in the tests are looked up: ten minutes
/// before they were closed. It is written out rather than derived from the lookup
/// margin, so that the margin applied is checked as well.
pub(super) fn test_lookup_since() -> RangeInclusive<OffsetDateTime> {
    let since = OffsetDateTime::parse("2022-11-30T09:50:00Z", &Rfc3339).unwrap();
    since..=since
}

/// Returns the identifier of the test vote.
pub(super) fn test_vote_id() -> Uuid {
    Uuid::parse_str(VOTE_ID).unwrap()
}

/// Renders the vote cancelled comment body of the vote provided.
pub(super) fn vote_cancelled_body(vote_id: Uuid, is_pull_request: bool) -> String {
    tmpl::VoteCancelled::new(vote_id, USER, is_pull_request).render().unwrap()
}

/// Renders the vote closed comment body of the vote provided, with the test results.
pub(super) fn vote_closed_body(vote_id: Uuid) -> String {
    tmpl::VoteClosed::new(vote_id, &setup_test_vote_results()).render().unwrap()
}

/// Renders the vote created comment body of the vote created on the test issue or pull
/// request with the default profile.
pub(super) fn vote_created_body(is_pull_request: bool) -> String {
    tmpl::VoteCreated::new(&test_create_vote_input(is_pull_request), &test_cfg_profile())
        .render()
        .unwrap()
}

/// Renders the vote in progress comment body.
pub(super) fn vote_in_progress_body(is_pull_request: bool) -> String {
    tmpl::VoteInProgress::new(USER, is_pull_request).render().unwrap()
}

/// Returns the default profile of the valid test configuration file.
fn test_cfg_profile() -> CfgProfile {
    CfgProfile {
        duration: Duration::from_mins(5),
        pass_threshold: 50.0,

        allowed_voters: Some(AllowedVoters::default()),
        ..Default::default()
    }
}

/// Returns the input of a vote created on the test issue or pull request with the
/// default profile.
fn test_create_vote_input(is_pull_request: bool) -> CreateVoteInput {
    let event = if is_pull_request {
        Event::PullRequest(setup_test_pr_event())
    } else {
        Event::Issue(setup_test_issue_event())
    };
    CreateVoteInput::new(None, &event)
}
