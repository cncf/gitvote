//! This module defines the vote processing logic.

use std::time::Duration;

use anyhow::{Context, Result, format_err};
use askama::Template;
use futures::future::{self, JoinAll};
use thiserror::Error;
use time::OffsetDateTime;
use tokio::{
    task::JoinHandle,
    time::{sleep, timeout},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, instrument, warn};

use crate::{
    cfg_repo::{CfgError, CfgProfile},
    cmd::{self, CancelVoteInput, CheckVoteInput, Command, CreateVoteInput},
    db::{CLOSE_CLAIM_DURATION_SECS, DynDB, VoteAlreadyOpenError, VoteCloseFailureOutcome, VoteCloseStep},
    github::{CheckDetails, CommentNotFoundError, DiscussionRejectedError, DynGH, split_full_name},
    results::{self, Vote, VoteResults},
    tmpl,
};

#[cfg(test)]
mod tests;

/// How often we try to auto close votes that have already passed.
const AUTO_CLOSE_FREQUENCY: Duration = Duration::from_hours(24);

/// Summary of the check of a vote closed without results.
const CHECK_SUMMARY_NO_RESULTS: &str =
    "The vote closed without results as its comment was not found, please run /vote again";

/// Maximum time an attempt to close a vote, or to publish its cancellation, can take. An
/// attempt taking longer is stopped and recorded as failed like any other failure, so
/// that it ends before the claim of the vote expires and another worker can claim it.
const CLOSE_ATTEMPT_TIMEOUT: Duration = Duration::from_mins(20);

/// Minimum time left on the claim of a vote when its attempt times out, so that there is
/// time to record the failure before another worker can claim the vote.
const CLOSE_CLAIM_MARGIN: Duration = Duration::from_mins(10);

// Attempts must end before their claim expires, leaving the margin to record the outcome
const _: () = assert!(
    CLOSE_ATTEMPT_TIMEOUT.as_secs() + CLOSE_CLAIM_MARGIN.as_secs() <= CLOSE_CLAIM_DURATION_SECS as u64,
    "attempts to close a vote must end well before their claim expires"
);

/// Number of commands handlers workers.
const COMMANDS_HANDLERS_WORKERS: usize = 5;

/// Label used to tag issues/prs where a vote has been created.
const GITVOTE_LABEL: &str = "gitvote";

/// How long before a vote was closed to start looking for the comments and discussions
/// that a previous attempt published, to allow for clock differences with GitHub.
const MARKER_LOOKUP_MARGIN: Duration = Duration::from_mins(10);

/// How often a vote can be checked.
const MAX_VOTE_CHECK_FREQUENCY: Duration = Duration::from_hours(24);

/// How often the status checker should run.
const STATUS_CHECK_FREQUENCY: Duration = Duration::from_mins(30);

/// Label used to tag issues/prs where a vote is closed.
const VOTE_CLOSED_LABEL: &str = "gitvote/closed";

/// Label used to tag issues/prs where a vote failed.
const VOTE_FAILED_LABEL: &str = "gitvote/failed";

/// Label used to tag issues/prs where a vote is open.
const VOTE_OPEN_LABEL: &str = "gitvote/open";

/// Label used to tag issues/prs where a vote passed.
const VOTE_PASSED_LABEL: &str = "gitvote/passed";

/// Amount of time the votes closer will sleep when it cannot pick the next vote or
/// record the outcome of an attempt. Failures closing a vote do not cause a pause, so
/// that they do not hold up other votes: they are retried later, with a backoff.
const VOTES_CLOSER_PAUSE_ON_ERROR: Duration = Duration::from_secs(30);

/// Amount of time the votes closer will sleep when there are no pending votes
/// to close.
const VOTES_CLOSER_PAUSE_ON_NONE: Duration = Duration::from_secs(15);

/// Number of votes closers workers.
const VOTES_CLOSERS_WORKERS: usize = 1;

/// A votes processor is in charge of creating the requested votes, stopping
/// them at the scheduled time and publishing results, etc. It relies on some
/// specialized workers to handle the different tasks.
pub(crate) struct Processor {
    /// Receiving end of the commands queue, shared by the commands handlers.
    cmds_rx: async_channel::Receiver<Command>,
    /// Sending end of the commands queue, used by the status checker.
    cmds_tx: async_channel::Sender<Command>,
    /// Database handle.
    db: DynDB,
    /// GitHub client.
    gh: DynGH,
}

impl Processor {
    /// Creates a new votes processor instance.
    pub(crate) fn new(
        db: DynDB,
        gh: DynGH,
        cmds_tx: async_channel::Sender<Command>,
        cmds_rx: async_channel::Receiver<Command>,
    ) -> Self {
        Self {
            cmds_rx,
            cmds_tx,
            db,
            gh,
        }
    }

    /// Starts the votes processor, launching all its workers.
    pub(crate) fn run(self, cancel_token: &CancellationToken) -> JoinAll<JoinHandle<()>> {
        // Reserve room for the handles of all the workers, including the status checker
        // and the votes auto closer
        let num_workers = COMMANDS_HANDLERS_WORKERS + VOTES_CLOSERS_WORKERS + 2;
        let mut workers_handles = Vec::with_capacity(num_workers);

        // Launch commands handler workers
        for _ in 0..COMMANDS_HANDLERS_WORKERS {
            let cmds_handler = CommandsHandler::new(self.db.clone(), self.gh.clone(), self.cmds_rx.clone());
            let cmds_handler_handle = cmds_handler.run(cancel_token.clone());
            workers_handles.push(cmds_handler_handle);
        }

        // Launch votes closer workers
        for _ in 0..VOTES_CLOSERS_WORKERS {
            let votes_closer = VotesCloser::new(self.db.clone(), self.gh.clone());
            let votes_closer_handler = votes_closer.run(cancel_token.clone());
            workers_handles.push(votes_closer_handler);
        }

        // Launch status checker
        let status_checker = StatusChecker::new(self.db.clone(), self.cmds_tx.clone());
        let status_checker_handle = status_checker.run(cancel_token.clone());
        workers_handles.push(status_checker_handle);

        // Launch votes auto closer
        let votes_auto_closer = VotesAutoCloser::new(self.db.clone(), self.gh.clone());
        let votes_auto_closer_handle = votes_auto_closer.run(cancel_token.clone());
        workers_handles.push(votes_auto_closer_handle);

        // Return a future that completes once all the workers have finished
        future::join_all(workers_handles)
    }
}

/// Worker that receives commands from the queue and executes them. Commands
/// are added to the queue when certain events from GitHub are received on the
/// webhook endpoint.
struct CommandsHandler {
    /// Receiving end of the commands queue.
    cmds_rx: async_channel::Receiver<Command>,
    /// Database handle.
    db: DynDB,
    /// GitHub client.
    gh: DynGH,
}

impl CommandsHandler {
    /// Creates a new commands handler instance.
    fn new(db: DynDB, gh: DynGH, cmds_rx: async_channel::Receiver<Command>) -> Self {
        Self { cmds_rx, db, gh }
    }

    /// Starts the commands handler.
    fn run(self, cancel_token: CancellationToken) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;

                    // Pick next command from the queue and process it
                    Ok(cmd) = self.cmds_rx.recv() => self.process_command(cmd).await,

                    // Exit if the worker has been asked to stop
                    () = cancel_token.cancelled() => break,
                }
            }
        })
    }

    /// Cancels an open vote. The cancellation is committed before anything is published,
    /// so failing to publish it does not fail the command: the votes closer retries it
    /// until it has no attempts left, when the cancellation is abandoned.
    #[instrument(fields(repo = i.repository_full_name, issue_number = i.issue_number), skip_all, err(Debug))]
    async fn cancel_vote(&self, i: &CancelVoteInput) -> Result<()> {
        // Only repository collaborators can cancel votes
        let inst_id = i.installation_id as u64;
        let (owner, repo) = split_full_name(&i.repository_full_name);
        if !self
            .gh
            .user_is_collaborator(inst_id, owner, repo, &i.cancelled_by)
            .await
            .context("error checking if user is collaborator")?
        {
            return Ok(());
        }

        // Cancel the vote open in the issue/pr provided, claiming the publication of the
        // cancellation, or report there was none
        let Some(vote) = self
            .db
            .cancel_vote(&i.repository_full_name, i.issue_number, &i.cancelled_by)
            .await
            .context("error cancelling vote")?
        else {
            let body = tmpl::NoVoteInProgress::new(&i.cancelled_by, i.is_pull_request).render()?;
            self.gh.post_comment(inst_id, owner, repo, i.issue_number, &body).await?;
            return Ok(());
        };

        // Publish the cancellation right away, which is its first attempt. A failure is
        // recorded so the votes closer retries it later, and if that fails too it
        // retries once the claim expires
        let vote_id = vote.vote_id;
        let publish = publish_vote_cancellation(&self.db, &self.gh, &vote, false);
        if let Err(err) = run_close_attempt(publish).await {
            // Record the failure, which is only done while the claim is held
            let record_result = async {
                let claimed_until = vote.close_next_attempt_at.context("cancelled vote without claim")?;
                self.db
                    .record_vote_close_failure(vote_id, claimed_until)
                    .await
                    .context("error recording vote cancellation failure")
            }
            .await;

            // Log the publication error according to the outcome recorded
            match record_result {
                Ok(VoteCloseFailureOutcome::CancellationAbandoned) => error!(
                    ?err,
                    ?vote_id,
                    "error publishing vote cancellation, no attempts left, it was abandoned"
                ),
                Ok(VoteCloseFailureOutcome::ClaimLost) => warn!(
                    ?err,
                    ?vote_id,
                    "error publishing vote cancellation, its claim was taken over"
                ),
                Ok(VoteCloseFailureOutcome::NoAttemptsLeft) => error!(
                    ?err,
                    ?vote_id,
                    "error publishing vote cancellation, no attempts left"
                ),
                Ok(VoteCloseFailureOutcome::RetryScheduled) => warn!(
                    ?err,
                    ?vote_id,
                    "error publishing vote cancellation, it will be retried"
                ),
                Err(record_err) => warn!(
                    ?err,
                    ?record_err,
                    ?vote_id,
                    "error publishing vote cancellation, it will be retried once its claim expires"
                ),
            }
            return Ok(());
        }

        debug!(?vote_id, "cancelled");
        Ok(())
    }

    /// Checks the status of a vote. The vote must be open and not have been
    /// checked in `MAX_VOTE_CHECK_FREQUENCY`.
    #[instrument(fields(vote_id), skip_all, err(Debug))]
    async fn check_vote(&self, i: &CheckVoteInput) -> Result<()> {
        // Get open vote (if any) from database
        let Some(vote) = self
            .db
            .get_open_vote(&i.repository_full_name, i.issue_number)
            .await
            .context("error getting open vote")?
        else {
            return Ok(());
        };

        // Record vote_id as part of the current span
        tracing::Span::current().record("vote_id", vote.vote_id.to_string());

        // Check if the vote has already been checked recently
        let inst_id = vote.installation_id as u64;
        let (owner, repo) = split_full_name(&vote.repository_full_name);
        if let Some(checked_at) = vote.checked_at
            && OffsetDateTime::now_utc() - checked_at < MAX_VOTE_CHECK_FREQUENCY
        {
            // Post comment on the issue/pr and return
            let body = tmpl::VoteCheckedRecently {}.render()?;
            self.gh.post_comment(inst_id, owner, repo, vote.issue_number, &body).await?;
            return Ok(());
        }

        // Calculate results
        let (owner, repo) = split_full_name(&vote.repository_full_name);
        let results = results::calculate(self.gh.clone(), owner, repo, &vote).await?;

        // Post vote status comment on the issue/pr
        let body = tmpl::VoteStatus::new(&results).render()?;
        self.gh.post_comment(inst_id, owner, repo, vote.issue_number, &body).await?;

        // Update vote's last check ts. The status has been posted already, so this is
        // done on a best effort basis, as asking to check the vote again would post it
        // twice
        if let Err(err) = self.db.update_vote_last_check(vote.vote_id).await {
            warn!(
                ?err,
                repository_full_name = vote.repository_full_name,
                issue_number = vote.issue_number,
                vote_id = %vote.vote_id,
                "error updating vote last check"
            );
        }

        Ok(())
    }

    /// Creates a new vote.
    #[instrument(fields(repo = i.repository_full_name, issue_number = i.issue_number), skip_all, err(Debug))]
    async fn create_vote(&self, i: &CreateVoteInput) -> Result<()> {
        // Get vote configuration profile
        let inst_id = i.installation_id as u64;
        let (owner, repo) = split_full_name(&i.repository_full_name);
        let cfg = match CfgProfile::get(
            self.gh.clone(),
            inst_id,
            owner,
            i.organization.is_some(),
            repo,
            i.profile_name.clone(),
        )
        .await
        {
            Ok(cfg) => cfg,
            Err(err) => {
                // Explain why the configuration cannot be used, unless it could not be
                // looked up, which is an error
                let body = match err {
                    CfgError::ConfigLookup(err) => return Err(err),
                    CfgError::ConfigNotFound => tmpl::ConfigNotFound {}.render()?,
                    CfgError::InvalidConfig(reason) => tmpl::InvalidConfig::new(&reason).render()?,
                    CfgError::ProfileNotFound => tmpl::ConfigProfileNotFound {}.render()?,
                };

                // This is an automation failure if a rule requested the vote, which
                // is reported even if the check is not required
                if i.is_automatic {
                    if let Err(err) = cmd::report_automation_failure(
                        self.db.clone(),
                        self.gh.clone(),
                        inst_id,
                        owner,
                        repo,
                        i.issue_number,
                        Some(&body),
                    )
                    .await
                    {
                        warn!(?err, "error reporting automation failure");
                    }
                    return Ok(());
                }

                // Otherwise post the explanation on the issue/pr and update its check
                self.gh.post_comment(inst_id, owner, repo, i.issue_number, &body).await?;
                self.update_rejected_vote_check(i).await;
                return Ok(());
            }
        };

        // Only repository collaborators can create votes
        if !self.gh.user_is_collaborator(inst_id, owner, repo, &i.created_by).await? {
            let body = tmpl::VoteRestricted::new(&i.created_by).render()?;
            self.gh.post_comment(inst_id, owner, repo, i.issue_number, &body).await?;
            self.update_rejected_vote_check(i).await;
            return Ok(());
        }

        // Only allow one vote open at the same time per issue/pr
        if self.db.has_vote_open(&i.repository_full_name, i.issue_number).await? {
            let body = tmpl::VoteInProgress::new(&i.created_by, i.is_pull_request).render()?;
            self.gh.post_comment(inst_id, owner, repo, i.issue_number, &body).await?;
            return Ok(());
        }

        // Post vote created comment on the issue/pr
        let body = tmpl::VoteCreated::new(i, &cfg).render()?;
        let vote_comment_id = self.gh.post_comment(inst_id, owner, repo, i.issue_number, &body).await?;

        // Store vote information in database. The vote may have been stored despite an
        // error, in which case it is open and must be set up all the same
        let vote_id = match self.db.store_vote(vote_comment_id, i, &cfg).await {
            Ok(vote_id) => Some(vote_id),
            Err(err) => match self.handle_store_vote_error(i, vote_comment_id, err).await? {
                StoreVoteRecovery::Handled => return Ok(()),
                StoreVoteRecovery::VoteStored => None,
            },
        };

        // Set up the vote, which is open at this point
        self.set_up_vote(i).await.map_err(VoteSetupError)?;

        debug!(?vote_id, "created");
        Ok(())
    }

    /// Deletes a vote comment no vote is tracking, on a best effort basis.
    async fn delete_orphan_vote_comment(&self, i: &CreateVoteInput, vote_comment_id: i64) {
        let inst_id = i.installation_id as u64;
        let (owner, repo) = split_full_name(&i.repository_full_name);
        if let Err(err) = self.gh.delete_comment(inst_id, owner, repo, vote_comment_id).await {
            warn!(?err, "error deleting orphan vote comment");
        }
    }

    /// Handles an error storing a vote whose comment has been posted already. The error
    /// is returned unless the vote is known to be open, either because another worker
    /// won the race to create it, which is reported on the issue/pr, or because it was
    /// stored despite the error, in which case the caller must set it up.
    async fn handle_store_vote_error(
        &self,
        i: &CreateVoteInput,
        vote_comment_id: i64,
        err: anyhow::Error,
    ) -> Result<StoreVoteRecovery> {
        // Another worker won the race to create the vote, so nothing was stored and the
        // vote comment must not stay around
        if err.is::<VoteAlreadyOpenError>() {
            self.delete_orphan_vote_comment(i, vote_comment_id).await;
            let inst_id = i.installation_id as u64;
            let (owner, repo) = split_full_name(&i.repository_full_name);
            let body = tmpl::VoteInProgress::new(&i.created_by, i.is_pull_request).render()?;
            self.gh.post_comment(inst_id, owner, repo, i.issue_number, &body).await?;
            return Ok(StoreVoteRecovery::Handled);
        }

        // The vote may have been stored even if the database could not confirm it, so
        // the vote comment is only deleted once it is known that no vote is tracking it
        match self.db.has_vote_with_comment(vote_comment_id).await {
            Ok(false) => self.delete_orphan_vote_comment(i, vote_comment_id).await,
            Ok(true) => {
                warn!(
                    ?err,
                    repository_full_name = i.repository_full_name,
                    issue_number = i.issue_number,
                    vote_comment_id,
                    "vote stored despite the error, setting it up"
                );
                return Ok(StoreVoteRecovery::VoteStored);
            }
            Err(lookup_err) => {
                warn!(
                    ?lookup_err,
                    vote_comment_id, "error looking up vote, keeping its comment"
                );
            }
        }

        // The vote is not known to be open, so the error is returned
        Err(err)
    }

    /// Posts a comment on the issue/pr provided saying that the command failed and that
    /// it can be tried again. This is done on a best effort basis.
    async fn post_command_failed_comment(
        &self,
        installation_id: i64,
        repository_full_name: &str,
        issue_number: i64,
    ) {
        // Render and post the comment, logging any failure
        let inst_id = installation_id as u64;
        let (owner, repo) = split_full_name(repository_full_name);
        let template = tmpl::CommandFailed {};
        let result = match template.render() {
            Ok(body) => self.gh.post_comment(inst_id, owner, repo, issue_number, &body).await,
            Err(err) => Err(err.into()),
        };
        if let Err(err) = result {
            warn!(
                ?err,
                repository_full_name, issue_number, "error posting command failed comment"
            );
        }
    }

    /// Executes the command provided, reporting on the issue/pr when it fails. The error
    /// itself is logged by each of the commands.
    async fn process_command(&self, cmd: Command) {
        match cmd {
            Command::CancelVote(input) => {
                if self.cancel_vote(&input).await.is_err() {
                    self.post_command_failed_comment(
                        input.installation_id,
                        &input.repository_full_name,
                        input.issue_number,
                    )
                    .await;
                }
            }
            Command::CheckVote(input) => {
                if self.check_vote(&input).await.is_err()
                    && let Some(installation_id) = input.installation_id
                {
                    self.post_command_failed_comment(
                        installation_id,
                        &input.repository_full_name,
                        input.issue_number,
                    )
                    .await;
                }
            }
            Command::CreateVote(input) => {
                debug!(
                    repo = input.repository_full_name,
                    issue = input.issue_number,
                    "processing create vote command"
                );
                if let Err(err) = self.create_vote(&input).await
                    && !err.is::<VoteSetupError>()
                {
                    self.report_create_vote_failure(&input).await;
                }
            }
        }
    }

    /// Reports that creating a vote failed, on a best effort basis. On pull requests
    /// just opened a failure check is reported too, as nothing else would report the
    /// check, and reporting success could allow merging them without the vote requested.
    /// Nobody ran a command on automatic votes, so they are reported as an automation
    /// failure. No check is reported if the pull request has a vote, which owns it, like
    /// one stored despite the failure.
    async fn report_create_vote_failure(&self, i: &CreateVoteInput) {
        let inst_id = i.installation_id as u64;
        let (owner, repo) = split_full_name(&i.repository_full_name);

        // Report the automation failure
        if i.is_automatic {
            if let Err(err) = cmd::report_automation_failure(
                self.db.clone(),
                self.gh.clone(),
                inst_id,
                owner,
                repo,
                i.issue_number,
                None,
            )
            .await
            {
                warn!(
                    ?err,
                    repository_full_name = i.repository_full_name,
                    issue_number = i.issue_number,
                    "error reporting automation failure"
                );
            }
            return;
        }

        // Post the comment
        self.post_command_failed_comment(i.installation_id, &i.repository_full_name, i.issue_number)
            .await;

        // Report the failure check, unless a vote owns it
        if i.opened_pr_base_branch.is_some() {
            let check_details = cmd::vote_not_created_check();
            if let Err(err) = cmd::publish_failure_check(
                self.db.clone(),
                self.gh.clone(),
                inst_id,
                owner,
                repo,
                i.issue_number,
                &check_details,
            )
            .await
            {
                warn!(
                    ?err,
                    repository_full_name = i.repository_full_name,
                    issue_number = i.issue_number,
                    "error reporting vote not created check"
                );
            }
        }
    }

    /// Sets up a vote just opened, creating its in progress check run on pull requests
    /// and updating the labels of its issue/pr. The vote is open already, so both steps
    /// are tried even if the other one fails.
    async fn set_up_vote(&self, i: &CreateVoteInput) -> Result<()> {
        let inst_id = i.installation_id as u64;
        let (owner, repo) = split_full_name(&i.repository_full_name);

        // Create check run if the vote is on a pull request
        let check_run_result = if i.is_pull_request {
            let check_details = CheckDetails {
                status: "in_progress".to_string(),
                summary: "Vote open".to_string(),
                conclusion: None,
            };
            self.gh.create_check_run(inst_id, owner, repo, i.issue_number, &check_details).await
        } else {
            Ok(())
        };

        // Update issue/pr labels, removing first the vote closed and result labels just
        // in case they were left from a previous vote that was closed
        for label in [VOTE_CLOSED_LABEL, VOTE_FAILED_LABEL, VOTE_PASSED_LABEL] {
            remove_label_best_effort(&self.gh, inst_id, owner, repo, i.issue_number, label).await;
        }
        let labels_result = self
            .gh
            .add_labels(
                inst_id,
                owner,
                repo,
                i.issue_number,
                &[GITVOTE_LABEL, VOTE_OPEN_LABEL],
            )
            .await;

        // Report any failure once all the steps have been tried
        combine_step_outcomes(vec![("check run", check_run_result), ("labels", labels_result)])
    }

    /// Updates the check of a pull request just opened whose vote was rejected, as
    /// nothing else would report it. This is done on a best effort basis: the user has
    /// been told about the rejection already, and a required check not reported blocks
    /// the merge. The check is left alone if the pull request has a vote, which owns it,
    /// like one created concurrently by a maintainer.
    async fn update_rejected_vote_check(&self, i: &CreateVoteInput) {
        let Some(branch) = &i.opened_pr_base_branch else {
            return;
        };
        let inst_id = i.installation_id as u64;
        let (owner, repo) = split_full_name(&i.repository_full_name);
        if let Err(err) = cmd::update_no_vote_check(
            self.db.clone(),
            self.gh.clone(),
            inst_id,
            owner,
            repo,
            i.issue_number,
            branch,
        )
        .await
        {
            warn!(?err, "error updating rejected vote check");
        }
    }
}

/// Worker that periodically runs status check commands.
struct StatusChecker {
    /// Sending end of the commands queue, where the check vote commands are enqueued.
    cmds_tx: async_channel::Sender<Command>,
    /// Database handle.
    db: DynDB,
}

impl StatusChecker {
    /// Creates a new status checker instance.
    fn new(db: DynDB, cmds_tx: async_channel::Sender<Command>) -> Self {
        Self { cmds_tx, db }
    }

    /// Starts the status checker.
    fn run(self, cancel_token: CancellationToken) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                // Process pending periodic status checks
                match self.db.get_pending_status_checks().await {
                    Ok(inputs) => {
                        // Enqueue check vote commands, which only fails if the queue
                        // has been closed
                        for input in inputs {
                            if let Err(err) = self.cmds_tx.send(Command::CheckVote(input)).await {
                                warn!(?err, "error enqueuing check vote command");
                            }
                        }
                    }
                    Err(err) => {
                        error!(?err, "error getting pending status checks");
                    }
                }

                // Pause until it's time for the next run or exit if the worker
                // has been asked to stop
                tokio::select! {
                    () = sleep(STATUS_CHECK_FREQUENCY) => {},
                    () = cancel_token.cancelled() => break,
                }
            }
        })
    }
}

/// Outcome of handling an error storing a vote that leaves it known to be open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StoreVoteRecovery {
    /// Another worker created the vote, which has been reported on the issue/pr, so
    /// nothing else must be done.
    Handled,
    /// The vote was stored despite the error, so it must be set up.
    VoteStored,
}

/// Error returned when a vote was created but some of the steps that follow failed.
/// The vote is open, so the command must not be tried again.
#[derive(Debug, Error)]
#[error(transparent)]
struct VoteSetupError(
    /// Error of the first setup step that failed.
    anyhow::Error,
);

/// Worker that periodically auto closes votes that have already passed over all
/// the allowed voters. This only applies to votes with the `close_on_passing`
/// feature enabled.
struct VotesAutoCloser {
    /// Database handle.
    db: DynDB,
    /// GitHub client.
    gh: DynGH,
}

impl VotesAutoCloser {
    /// Creates a new votes auto closer instance.
    fn new(db: DynDB, gh: DynGH) -> Self {
        Self { db, gh }
    }

    /// Starts the votes auto closer.
    fn run(self, cancel_token: CancellationToken) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                // Process pending automatic vote closes
                match self.db.get_open_votes_with_close_on_passing().await {
                    Ok(votes) => {
                        for vote in &votes {
                            let vote_id = vote.vote_id;
                            let (owner, repo) = split_full_name(&vote.repository_full_name);

                            // Calculate vote results
                            match results::calculate(self.gh.clone(), owner, repo, vote).await {
                                Ok(results) => {
                                    // Close the vote early (update its ending
                                    // timestamp) only once it passes over all the
                                    // allowed voters, so that further votes from
                                    // pending voters cannot make it fail. A vote
                                    // passing over the votes cast may stay open
                                    if results.passes_with_all_allowed_voters()
                                        && let Err(err) = self.db.update_vote_ends_at(vote_id).await
                                    {
                                        error!(?err, ?vote.vote_id, "error updating vote ends at");
                                    }
                                }
                                Err(err) => error!(?err, ?vote_id, "error calculating results"),
                            }
                        }
                    }
                    Err(err) => {
                        error!(?err, "error getting votes with close_on_passing enabled");
                    }
                }

                // Pause until it's time for the next run or exit if the votes
                // processor has been asked to stop
                tokio::select! {
                    () = sleep(AUTO_CLOSE_FREQUENCY) => {},
                    () = cancel_token.cancelled() => break,
                }
            }
        })
    }
}

/// Worker that periodically checks the database for votes that should be
/// closed and closes them, retrying the closing side effects that failed, as well
/// as the publication of the cancellations that failed.
struct VotesCloser {
    /// Database handle.
    db: DynDB,
    /// GitHub client.
    gh: DynGH,
}

impl VotesCloser {
    /// Creates a new votes closer instance.
    fn new(db: DynDB, gh: DynGH) -> Self {
        Self { db, gh }
    }

    /// Starts the votes closer.
    fn run(self, cancel_token: CancellationToken) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                // Process the next vote with closing work due
                match self.close_finished_vote().await {
                    Ok(Some(())) => {
                        // One vote with closing work due was processed, try to
                        // process another one immediately
                    }
                    Ok(None) => tokio::select! {
                        // No votes with closing work due were found, pause unless
                        // we've been asked to stop
                        () = sleep(VOTES_CLOSER_PAUSE_ON_NONE) => {},
                        () = cancel_token.cancelled() => break,
                    },
                    Err(_) => {
                        // Something went wrong getting the next vote or recording
                        // the outcome, pause unless we've been asked to stop
                        tokio::select! {
                            () = sleep(VOTES_CLOSER_PAUSE_ON_ERROR) => {},
                            () = cancel_token.cancelled() => break,
                        }
                    }
                }

                // Exit if the worker has been asked to stop
                if cancel_token.is_cancelled() {
                    break;
                }
            }
        })
    }

    /// Tries to make progress on the next vote with closing work due: either a finished
    /// vote to close, or a closed vote with some of its side effects missing. A failure
    /// is recorded so that the attempt is retried later, as described in
    /// [`DB::record_vote_close_failure`](crate::db::DB::record_vote_close_failure), and
    /// it is not returned, as the votes closer can continue with the next vote.
    #[instrument(fields(vote_id), skip_all, err(Debug))]
    async fn close_finished_vote(&self) -> Result<Option<()>> {
        // Claim the next vote with closing work due
        let Some(vote) = self.db.claim_vote_to_close().await.context("error claiming vote to close")? else {
            return Ok(None);
        };

        // Record vote_id as part of the current span
        tracing::Span::current().record("vote_id", vote.vote_id.to_string());

        // Close the vote, recording the failure if something goes wrong or it takes too
        // long. It is recorded only while the claim is held, as the vote may have been
        // cancelled meanwhile
        if let Err(err) = run_close_attempt(self.close_vote(&vote)).await {
            let record_result = async {
                let claimed_until = vote.close_next_attempt_at.context("claimed vote without claim")?;
                self.db
                    .record_vote_close_failure(vote.vote_id, claimed_until)
                    .await
                    .context("error recording vote close failure")
            }
            .await;

            // Log the close error if it could not be recorded, as only the recording one
            // is returned, which makes the votes closer pause and the span log it
            let outcome = match record_result {
                Ok(outcome) => outcome,
                Err(record_err) => {
                    warn!(
                        ?err,
                        "error closing vote, it will be retried once its claim expires"
                    );
                    return Err(record_err);
                }
            };

            // Log the close error according to the outcome recorded
            match outcome {
                VoteCloseFailureOutcome::CancellationAbandoned => {
                    error!(
                        ?err,
                        "error publishing vote cancellation, no attempts left, it was abandoned"
                    );
                }
                VoteCloseFailureOutcome::ClaimLost => {
                    warn!(?err, "error closing vote, its claim was taken over");
                }
                VoteCloseFailureOutcome::NoAttemptsLeft => {
                    error!(?err, "error closing vote, no attempts left");
                }
                VoteCloseFailureOutcome::RetryScheduled => {
                    warn!(?err, "error closing vote, it will be retried");
                }
            }
            return Ok(Some(()));
        }

        debug!("closed");
        Ok(Some(()))
    }

    /// Closes the vote provided if needed and runs the side effects still missing. The
    /// cancellation of a cancelled vote is published instead, as it has no results.
    ///
    /// A vote claimed closed already had a previous attempt, which may have published
    /// some steps without recording them: only an attempt by the votes closer closes a
    /// vote, before publishing anything, and cancelling a vote closes it and publishes
    /// its cancellation right away. This holds when a worker died holding the claim too,
    /// and unlike the failed attempts count it is not reset when the vote is closed.
    async fn close_vote(&self, vote: &Vote) -> Result<()> {
        // Publish the cancellation of a cancelled vote, retrying the steps still missing
        let is_retry = vote.closed;
        if vote.cancelled_at.is_some() {
            return publish_vote_cancellation(&self.db, &self.gh, vote, is_retry).await;
        }

        // Calculate and store the results, unless a previous attempt already did. The
        // head commit is read last, right before closing the vote, and stored with it
        let (results, head_sha) = if vote.closed {
            (vote.results.clone(), vote.close_head_sha.clone())
        } else {
            let (owner, repo) = split_full_name(&vote.repository_full_name);
            let results = match results::calculate(self.gh.clone(), owner, repo, vote).await {
                Ok(results) => Some(results),
                // Vote comment was deleted. We still want to proceed and close the
                // vote so that we don't try again to close it
                Err(err) if err.is::<CommentNotFoundError>() => None,
                Err(err) => return Err(err),
            };
            let head_sha = self.get_close_head_sha(vote).await;
            if !self
                .db
                .close_vote(vote.vote_id, results.as_ref(), head_sha.as_deref())
                .await
                .context("error closing vote in database")?
            {
                // The vote was cancelled after being claimed, nothing else to do
                return Ok(());
            }
            (results, head_sha)
        };

        // Try all the side effects missing, recording each one as soon as it is done.
        // They are independent, so a failure must not hold up the others. A step is
        // recorded after its GitHub call, so if that call fails after GitHub processed
        // it, or the process or the database fail in between, the next attempt repeats
        // the step. Retries look for the comment and the announcement a previous attempt
        // posted before posting them again, while the check run and labels are repeated
        let mut outcomes = vec![];
        if let Some(results) = &results {
            if vote.close_comment_posted_at.is_none() {
                outcomes.push(("comment", self.post_close_comment(vote, results, is_retry).await));
            }
            if vote.close_announcement_posted_at.is_none() {
                let outcome = self.post_close_announcement(vote, results, is_retry).await;
                outcomes.push(("announcement", outcome));
            }
        }
        outcomes.extend(self.publish_close_state(vote, results.as_ref(), head_sha.as_deref()).await);

        // The attempt failed if any of them did
        combine_step_outcomes(outcomes)?;

        // Nothing else is left to do for this vote
        self.db.complete_vote_close(vote.vote_id).await.context("error completing vote close")
    }

    /// Creates the check run of a vote being closed on the commit provided, which is the
    /// head of the pull request when the vote closed. When that commit is unknown, the
    /// check run is created on the current head.
    async fn create_close_check_run(
        &self,
        vote: &Vote,
        results: Option<&VoteResults>,
        head_sha: Option<&str>,
    ) -> Result<()> {
        // Prepare the check run details
        let (conclusion, summary) = if let Some(results) = results {
            if results.passed {
                (
                    "success",
                    format!(
                        "The vote passed! {} out of {} voted in favor.",
                        results.in_favor, results.allowed_voters
                    ),
                )
            } else {
                (
                    "failure",
                    format!(
                        "The vote did not pass. {} out of {} voted in favor.",
                        results.in_favor, results.allowed_voters
                    ),
                )
            }
        } else {
            ("failure", CHECK_SUMMARY_NO_RESULTS.to_string())
        };
        let check_details = CheckDetails {
            status: "completed".to_string(),
            summary,
            conclusion: Some(conclusion.to_string()),
        };

        // Create the check run
        let inst_id = vote.installation_id as u64;
        let (owner, repo) = split_full_name(&vote.repository_full_name);
        if let Some(head_sha) = head_sha {
            self.gh.create_commit_check_run(inst_id, owner, repo, head_sha, &check_details).await?;
        } else {
            self.gh.create_check_run(inst_id, owner, repo, vote.issue_number, &check_details).await?;
        }

        // Record that it has been created
        self.db
            .record_vote_close_step(vote.vote_id, VoteCloseStep::CheckRun)
            .await
            .context("error recording vote close step")
    }

    /// Gets the head commit of the pull request of a vote about to be closed. Votes on
    /// issues have none. A failure reading it must not hold up closing the vote, as that
    /// could leave it without results, so it is logged and the head is left unknown.
    async fn get_close_head_sha(&self, vote: &Vote) -> Option<String> {
        if !vote.is_pull_request {
            return None;
        }
        let inst_id = vote.installation_id as u64;
        let (owner, repo) = split_full_name(&vote.repository_full_name);
        match self.gh.get_pr_head_sha(inst_id, owner, repo, vote.issue_number).await {
            Ok(head_sha) => Some(head_sha),
            Err(err) => {
                warn!(?err, "error getting head commit of vote being closed");
                None
            }
        }
    }

    /// Posts the announcement of a vote being closed if enabled in the vote profile. On
    /// a retry, an announcement created by a previous attempt is recorded instead of
    /// being created again.
    async fn post_close_announcement(
        &self,
        vote: &Vote,
        results: &VoteResults,
        is_retry: bool,
    ) -> Result<()> {
        // Announcements are only posted when enabled in the vote profile
        let Some(discussions) = vote.cfg.announcements.as_ref().and_then(|a| a.discussions.as_ref()) else {
            return Ok(());
        };

        // Prepare the announcement
        let inst_id = vote.installation_id as u64;
        let (owner, repo) = split_full_name(&vote.repository_full_name);
        let issue_title = vote.issue_title.as_deref().unwrap_or_default();
        let title = build_announcement_title(vote.issue_number, issue_title);
        let body = tmpl::VoteClosedAnnouncement::new(vote.vote_id, vote.issue_number, issue_title, results)
            .render()?;

        // Post it to discussions, unless a previous attempt created it already
        let category = &discussions.category;
        let publish = async {
            if is_retry {
                let marker = tmpl::VoteMarker::ResultsAnnouncement.render(vote.vote_id);
                let since = marker_lookup_since(vote)?;
                if self
                    .gh
                    .has_discussion_with_marker(inst_id, owner, repo, category, &marker, since)
                    .await
                    .context("error looking up announcement discussion")?
                {
                    debug!("announcement discussion created by a previous attempt found");
                    return Ok(());
                }
            }
            self.gh.create_discussion(inst_id, owner, repo, category, &title, &body).await
        };
        match publish.await {
            Ok(()) => {}
            // Permissions required to create or look up discussions may not have been
            // granted yet for this installation, or the category may not exist. Retrying
            // won't help, so we raise a warning and give up on the announcement
            Err(err) if err.is::<DiscussionRejectedError>() => {
                warn!(?err, "announcement discussion rejected");
            }
            Err(err) => return Err(err),
        }

        // Record that it has been posted, or given up on when rejected
        self.db
            .record_vote_close_step(vote.vote_id, VoteCloseStep::Announcement)
            .await
            .context("error recording vote close step")
    }

    /// Posts the results comment of a vote being closed. On a retry, a comment posted by
    /// a previous attempt is recorded instead of being posted again.
    async fn post_close_comment(&self, vote: &Vote, results: &VoteResults, is_retry: bool) -> Result<()> {
        // Prepare the comment
        let inst_id = vote.installation_id as u64;
        let (owner, repo) = split_full_name(&vote.repository_full_name);
        let body = tmpl::VoteClosed::new(vote.vote_id, results).render()?;

        // Post it on the issue/pr, unless a previous attempt posted it already
        let marker = tmpl::VoteMarker::ResultsComment.render(vote.vote_id);
        if is_retry
            && has_previous_comment(&self.gh, vote, &marker)
                .await
                .context("error looking up results comment")?
        {
            debug!("results comment posted by a previous attempt found");
        } else {
            self.gh.post_comment(inst_id, owner, repo, vote.issue_number, &body).await?;
        }

        // Record that it has been posted
        self.db
            .record_vote_close_step(vote.vote_id, VoteCloseStep::Comment)
            .await
            .context("error recording vote close step")
    }

    /// Publishes the state of a vote being closed on its issue/pr, running the steps
    /// still missing of the check run and the labels. The outcome of each step tried is
    /// returned along with its name.
    async fn publish_close_state(
        &self,
        vote: &Vote,
        results: Option<&VoteResults>,
        head_sha: Option<&str>,
    ) -> Vec<(&'static str, Result<()>)> {
        // Create the check run of pull requests, unless a previous attempt did
        let mut outcomes = vec![];
        if vote.is_pull_request && vote.close_check_run_created_at.is_none() {
            let publish = self.create_close_check_run(vote, results, head_sha);
            let outcome = run_close_state_step(&self.db, vote, VoteCloseStep::CheckRun, publish).await;
            outcomes.push(("check run", outcome));
        }

        // Update the labels, unless a previous attempt did
        if vote.close_labels_updated_at.is_none() {
            let publish = self.update_close_labels(vote, results);
            let outcome = run_close_state_step(&self.db, vote, VoteCloseStep::Labels, publish).await;
            outcomes.push(("labels", outcome));
        }
        outcomes
    }

    /// Updates the labels of the issue/pr of a vote being closed.
    async fn update_close_labels(&self, vote: &Vote, results: Option<&VoteResults>) -> Result<()> {
        // Prepare the labels to add
        let mut labels_to_add = vec![VOTE_CLOSED_LABEL];
        if let Some(results) = results {
            if results.passed {
                labels_to_add.push(VOTE_PASSED_LABEL);
            } else {
                labels_to_add.push(VOTE_FAILED_LABEL);
            }
        }

        // Update the labels, removing the vote open one on a best effort basis
        let inst_id = vote.installation_id as u64;
        let (owner, repo) = split_full_name(&vote.repository_full_name);
        remove_label_best_effort(&self.gh, inst_id, owner, repo, vote.issue_number, VOTE_OPEN_LABEL).await;
        self.gh.add_labels(inst_id, owner, repo, vote.issue_number, &labels_to_add).await?;

        // Record that they have been updated
        self.db
            .record_vote_close_step(vote.vote_id, VoteCloseStep::Labels)
            .await
            .context("error recording vote close step")
    }
}

// Helpers.

/// Builds the title of the announcement of a closed vote.
fn build_announcement_title(issue_number: i64, issue_title: &str) -> String {
    format!("{issue_title} #{issue_number} (vote closed)")
}

/// Combines the outcomes of the steps tried, along with their names, in an attempt to
/// set up, close or cancel a vote. The attempt failed if any of them did: the first
/// error is the one returned, with the name of its step as context, and the others are
/// logged.
fn combine_step_outcomes(outcomes: Vec<(&'static str, Result<()>)>) -> Result<()> {
    let mut errors = outcomes.into_iter().filter_map(|(step, result)| result.err().map(|err| (step, err)));
    if let Some((first_step, first_err)) = errors.next() {
        for (step, err) in errors {
            warn!(?err, step, "error running vote step");
        }
        return Err(first_err.context(format!("error running {first_step} step")));
    }
    Ok(())
}

/// Checks if a previous attempt to close or cancel the vote provided posted a comment
/// with the marker given on its issue/pr. Only the comments posted since shortly before
/// the vote was closed are looked at, which cancelling it also does.
async fn has_previous_comment(gh: &DynGH, vote: &Vote, marker: &str) -> Result<bool> {
    let inst_id = vote.installation_id as u64;
    let (owner, repo) = split_full_name(&vote.repository_full_name);
    let since = marker_lookup_since(vote)?;
    gh.has_comment_with_marker(inst_id, owner, repo, vote.issue_number, marker, since).await
}

/// Returns the time from which to look for what a previous attempt to close or cancel
/// the vote provided published: shortly before the vote was closed, which cancelling it
/// also does, allowing for clock differences between the database and GitHub.
fn marker_lookup_since(vote: &Vote) -> Result<OffsetDateTime> {
    let closed_at = vote.closed_at.context("closed vote without close time")?;
    Ok(closed_at - MARKER_LOOKUP_MARGIN)
}

/// Publishes the cancellation of the vote provided on its issue/pr, running the steps
/// still missing, and deletes the vote once they are all done. This is done right after
/// the vote is cancelled by the commands handler, which is the first attempt, and
/// retried by the votes closer when it fails, so the caller must record the failure. On
/// a retry, a cancellation comment posted by a previous attempt is recorded instead of
/// being posted again. There are no results, so no announcement.
async fn publish_vote_cancellation(db: &DynDB, gh: &DynGH, vote: &Vote, is_retry: bool) -> Result<()> {
    let inst_id = vote.installation_id as u64;
    let (owner, repo) = split_full_name(&vote.repository_full_name);

    // Try all the steps missing, recording each one as soon as it is done. They are
    // independent, so a failure must not hold up the others. The comment is posted
    // first, unless a previous attempt posted it already
    let mut outcomes = vec![];
    if vote.close_comment_posted_at.is_none() {
        let publish = async {
            let cancelled_by = vote.cancelled_by.as_deref().context("cancelled vote without user")?;
            let marker = tmpl::VoteMarker::CancellationComment.render(vote.vote_id);
            if is_retry
                && has_previous_comment(gh, vote, &marker)
                    .await
                    .context("error looking up cancellation comment")?
            {
                debug!("cancellation comment posted by a previous attempt found");
            } else {
                let body =
                    tmpl::VoteCancelled::new(vote.vote_id, cancelled_by, vote.is_pull_request).render()?;
                gh.post_comment(inst_id, owner, repo, vote.issue_number, &body).await?;
            }

            // Record that it has been posted
            db.record_vote_close_step(vote.vote_id, VoteCloseStep::Comment)
                .await
                .context("error recording vote close step")
        };
        outcomes.push(("comment", publish.await));
    }

    // The check run of pull requests and the labels are skipped if a newer vote has
    // superseded them
    if vote.is_pull_request && vote.close_check_run_created_at.is_none() {
        let publish = async {
            // Create the check run
            let check_details = CheckDetails {
                status: "completed".to_string(),
                summary: "Vote cancelled".to_string(),
                conclusion: Some("success".to_string()),
            };
            gh.create_check_run(inst_id, owner, repo, vote.issue_number, &check_details).await?;

            // Record that it has been created
            db.record_vote_close_step(vote.vote_id, VoteCloseStep::CheckRun)
                .await
                .context("error recording vote close step")
        };
        let outcome = run_close_state_step(db, vote, VoteCloseStep::CheckRun, publish).await;
        outcomes.push(("check run", outcome));
    }
    if vote.close_labels_updated_at.is_none() {
        // Update the labels and record it. Removing the vote open label is best effort,
        // as when closing the vote
        let publish = async {
            remove_label_best_effort(gh, inst_id, owner, repo, vote.issue_number, VOTE_OPEN_LABEL).await;
            db.record_vote_close_step(vote.vote_id, VoteCloseStep::Labels)
                .await
                .context("error recording vote close step")
        };
        let outcome = run_close_state_step(db, vote, VoteCloseStep::Labels, publish).await;
        outcomes.push(("labels", outcome));
    }

    // The attempt failed if any of them did
    combine_step_outcomes(outcomes)?;

    // Nothing else is needed from the vote once its cancellation has been published
    db.complete_vote_cancellation(vote.vote_id)
        .await
        .context("error completing vote cancellation")
}

/// Removes the label provided from the issue/pr given on a best effort basis, logging
/// the failure. A label the issue/pr does not have is not a failure.
async fn remove_label_best_effort(
    gh: &DynGH,
    inst_id: u64,
    owner: &str,
    repo: &str,
    issue_number: i64,
    label: &str,
) {
    if let Err(err) = gh.remove_label(inst_id, owner, repo, issue_number, label).await {
        warn!(?err, owner, repo, issue_number, label, "error removing label");
    }
}

/// Runs an attempt to close a vote or to publish its cancellation, failing it if it
/// takes longer than [`CLOSE_ATTEMPT_TIMEOUT`]. The attempt is stopped then, wherever it
/// is, so it may have published some steps without recording them.
async fn run_close_attempt(attempt: impl Future<Output = Result<()>>) -> Result<()> {
    match timeout(CLOSE_ATTEMPT_TIMEOUT, attempt).await {
        Ok(result) => result,
        Err(_) => Err(format_err!(
            "attempt timed out after {} seconds",
            CLOSE_ATTEMPT_TIMEOUT.as_secs()
        )),
    }
}

/// Runs a step publishing the state of a vote being closed or cancelled. The state
/// reflects the latest vote, so the step is only published while it is pending, as
/// described in
/// [`DB::is_close_state_step_pending`](crate::db::DB::is_close_state_step_pending).
/// Otherwise it is recorded as done without publishing anything. The vote stored is
/// checked right before each step, rather than the one read when claiming it, as a newer
/// vote can be created, and even cancelled and deleted, while a previous step is waiting
/// for GitHub. A newer vote created while the step itself is waiting for GitHub is not
/// seen, as that would require holding a lock across the GitHub call.
async fn run_close_state_step(
    db: &DynDB,
    vote: &Vote,
    step: VoteCloseStep,
    publish: impl Future<Output = Result<()>>,
) -> Result<()> {
    // Record the step as done without publishing it if it is not pending anymore
    if !db
        .is_close_state_step_pending(vote.vote_id, step)
        .await
        .context("error checking if vote close step is pending")?
    {
        return db
            .record_vote_close_step(vote.vote_id, step)
            .await
            .context("error recording vote close step");
    }

    // Publish the state
    publish.await
}
