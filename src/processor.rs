//! This module defines the vote processing logic.

use std::time::Duration;

use anyhow::{Context, Result};
use askama::Template;
use futures::future::{self, JoinAll};
use time::OffsetDateTime;
use tokio::{task::JoinHandle, time::sleep};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, instrument, warn};
use uuid::Uuid;

use crate::{
    cfg_repo::{CfgError, CfgProfile},
    cmd::{CancelVoteInput, CheckVoteInput, Command, CreateVoteInput},
    db::DynDB,
    github::{split_full_name, CheckDetails, DynGH},
    results, tmpl,
};

/// Number of commands handlers workers.
const COMMANDS_HANDLERS_WORKERS: usize = 5;

/// Number of votes closers workers.
const VOTES_CLOSERS_WORKERS: usize = 1;

/// Amount of time the votes closer will sleep when there are no pending votes
/// to close.
const VOTES_CLOSER_PAUSE_ON_NONE: Duration = Duration::from_secs(15);

/// Amount of time the votes closer will sleep when something goes wrong.
const VOTES_CLOSER_PAUSE_ON_ERROR: Duration = Duration::from_secs(30);

/// How often a vote can be checked.
const MAX_VOTE_CHECK_FREQUENCY: Duration = Duration::from_secs(60 * 60 * 24);

/// How often the status checker should run.
const STATUS_CHECK_FREQUENCY: Duration = Duration::from_secs(60 * 30);

/// How often we try to auto close votes that have already passed.
const AUTO_CLOSE_FREQUENCY: Duration = Duration::from_secs(60 * 60 * 24);

/// Label used to tag issues/prs where a vote has been created.
const GITVOTE_LABEL: &str = "gitvote";

/// Label used to tag issues/prs where a vote is open.
const VOTE_OPEN_LABEL: &str = "gitvote/open";

/// Label used to tag issues/prs where a vote is closed.
const VOTE_CLOSED_LABEL: &str = "gitvote/closed";

/// Label used to tag issues/prs where a vote passed.
const VOTE_PASSED_LABEL: &str = "gitvote/passed";

/// Label used to tag issues/prs where a vote failed.
const VOTE_FAILED_LABEL: &str = "gitvote/failed";

/// A votes processor is in charge of creating the requested votes, stopping
/// them at the scheduled time and publishing results, etc. It relies on some
/// specialized workers to handle the different tasks.
pub(crate) struct Processor {
    db: DynDB,
    gh: DynGH,
    cmds_tx: async_channel::Sender<Command>,
    cmds_rx: async_channel::Receiver<Command>,
}

impl Processor {
    /// Create a new votes processor instance.
    pub(crate) fn new(
        db: DynDB,
        gh: DynGH,
        cmds_tx: async_channel::Sender<Command>,
        cmds_rx: async_channel::Receiver<Command>,
    ) -> Self {
        Self {
            db,
            gh,
            cmds_tx,
            cmds_rx,
        }
    }

    /// Start votes processor.
    pub(crate) fn run(self, cancel_token: &CancellationToken) -> JoinAll<JoinHandle<()>> {
        let num_workers = COMMANDS_HANDLERS_WORKERS + VOTES_CLOSERS_WORKERS + 2; // Status checker + auto closer
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

        future::join_all(workers_handles)
    }
}

/// Worker that receives commands from the queue and executes them. Commands
/// are added to the queue when certain events from GitHub are received on the
/// webhook endpoint.
struct CommandsHandler {
    db: DynDB,
    gh: DynGH,
    cmds_rx: async_channel::Receiver<Command>,
}

impl CommandsHandler {
    /// Create a new commands handler instance.
    pub(crate) fn new(db: DynDB, gh: DynGH, cmds_rx: async_channel::Receiver<Command>) -> Self {
        Self { db, gh, cmds_rx }
    }

    /// Start commands handler.
    fn run(self, cancel_token: CancellationToken) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;

                    // Pick next command from the queue and process it
                    Ok(cmd) = self.cmds_rx.recv() => {
                        match cmd {
                            Command::CreateVote(input) => {
                                debug!(repo=input.repository_full_name, issue=input.issue_number, "processing create vote command");
                                _ = self.create_vote(&input).await;
                            }
                            Command::CancelVote(input) => {
                                _ = self.cancel_vote(&input).await;
                            }
                            Command::CheckVote(input) => {
                                _ = self.check_vote(&input).await;
                            }
                        }
                    }

                    // Exit if the worker has been asked to stop
                    () = cancel_token.cancelled() => break,
                }
            }
        })
    }

    /// Create a new vote.
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
                let body = match err {
                    CfgError::ConfigNotFound => tmpl::ConfigNotFound {}.render()?,
                    CfgError::ProfileNotFound => tmpl::ConfigProfileNotFound {}.render()?,
                    CfgError::InvalidConfig(reason) => tmpl::InvalidConfig::new(&reason).render()?,
                };
                self.gh.post_comment(inst_id, owner, repo, i.issue_number, &body).await?;
                return Ok(());
            }
        };

        // Only repository collaborators can create votes
        if !self.gh.user_is_collaborator(inst_id, owner, repo, &i.created_by).await? {
            let body = tmpl::VoteRestricted::new(&i.created_by).render()?;
            self.gh.post_comment(inst_id, owner, repo, i.issue_number, &body).await?;
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

        // Store vote information in database
        let vote_id = self.db.store_vote(vote_comment_id, i, &cfg).await?;

        // Create check run if the vote is on a pull request
        if i.is_pull_request {
            let check_details = CheckDetails {
                status: "in_progress".to_string(),
                conclusion: None,
                summary: "Vote open".to_string(),
            };
            self.gh.create_check_run(inst_id, owner, repo, i.issue_number, &check_details).await?;
        }

        // Update issue/pr labels
        //
        // We try to remove the vote closed and result labels just in case they
        // were left from a previous vote that was closed
        _ = self.gh.remove_label(inst_id, owner, repo, i.issue_number, VOTE_CLOSED_LABEL).await;
        _ = self.gh.remove_label(inst_id, owner, repo, i.issue_number, VOTE_PASSED_LABEL).await;
        _ = self.gh.remove_label(inst_id, owner, repo, i.issue_number, VOTE_FAILED_LABEL).await;
        self.gh
            .add_labels(
                inst_id,
                owner,
                repo,
                i.issue_number,
                &[GITVOTE_LABEL, VOTE_OPEN_LABEL],
            )
            .await?;

        debug!(?vote_id, "created");
        Ok(())
    }

    /// Cancel an open vote.
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

        // Try cancelling the vote open in the issue/pr provided
        let cancelled_vote_id: Option<Uuid> = self
            .db
            .cancel_vote(&i.repository_full_name, i.issue_number)
            .await
            .context("error cancelling vote")?;

        // Post the corresponding comment on the issue/pr
        let body: String;
        match cancelled_vote_id {
            Some(vote_id) => {
                body = tmpl::VoteCancelled::new(&i.cancelled_by, i.is_pull_request).render()?;
                debug!(?vote_id, "cancelled");
            }
            None => {
                body = tmpl::NoVoteInProgress::new(&i.cancelled_by, i.is_pull_request).render()?;
            }
        }
        self.gh.post_comment(inst_id, owner, repo, i.issue_number, &body).await?;

        // Create check run and remove "vote open" label if the vote was cancelled
        if cancelled_vote_id.is_some() {
            // Create check run if the vote is on a pull request
            if i.is_pull_request {
                let check_details = CheckDetails {
                    status: "completed".to_string(),
                    conclusion: Some("success".to_string()),
                    summary: "Vote cancelled".to_string(),
                };
                self.gh.create_check_run(inst_id, owner, repo, i.issue_number, &check_details).await?;
            }

            // Update issue/pr labels
            _ = self.gh.remove_label(inst_id, owner, repo, i.issue_number, VOTE_OPEN_LABEL).await;
        }

        Ok(())
    }

    /// Check the status of a vote. The vote must be open and not have been
    /// checked in MAX_VOTE_CHECK_FREQUENCY.
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
        if let Some(checked_at) = vote.checked_at {
            if OffsetDateTime::now_utc() - checked_at < MAX_VOTE_CHECK_FREQUENCY {
                // Post comment on the issue/pr and return
                let body = tmpl::VoteCheckedRecently {}.render()?;
                self.gh.post_comment(inst_id, owner, repo, vote.issue_number, &body).await?;
                return Ok(());
            }
        }

        // Calculate results
        let (owner, repo) = split_full_name(&vote.repository_full_name);
        let results = results::calculate(self.gh.clone(), owner, repo, &vote).await?;

        // Post vote status comment on the issue/pr
        let body = tmpl::VoteStatus::new(&results).render()?;
        self.gh.post_comment(inst_id, owner, repo, vote.issue_number, &body).await?;

        // Update vote's last check ts
        self.db.update_vote_last_check(vote.vote_id).await?;

        Ok(())
    }
}

/// Worker that periodically checks the database for votes that should be
/// closed and closes them.
struct VotesCloser {
    db: DynDB,
    gh: DynGH,
}

impl VotesCloser {
    /// Create a new votes closer instance.
    pub(crate) fn new(db: DynDB, gh: DynGH) -> Self {
        Self { db, gh }
    }

    /// Start votes closer.
    fn run(self, cancel_token: CancellationToken) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                // Close any pending finished votes
                match self.close_finished_vote().await {
                    Ok(Some(())) => {
                        // One pending finished vote was closed, try to close
                        // another one immediately
                    }
                    Ok(None) => tokio::select! {
                        // No pending finished votes were found, pause unless
                        // we've been asked to stop
                        () = sleep(VOTES_CLOSER_PAUSE_ON_NONE) => {},
                        () = cancel_token.cancelled() => break,
                    },
                    Err(_) => {
                        // Something went wrong closing finished vote, pause
                        // unless we've been asked to stop
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

    /// Close any pending finished vote.
    #[instrument(fields(vote_id), skip_all, err(Debug))]
    async fn close_finished_vote(&self) -> Result<Option<()>> {
        // Try to close any finished vote
        let Some((vote, results)) = self
            .db
            .close_finished_vote(self.gh.clone())
            .await
            .context("error closing finished vote")?
        else {
            return Ok(None);
        };

        // Record vote_id as part of the current span
        tracing::Span::current().record("vote_id", vote.vote_id.to_string());

        // Extract some information from the vote
        let inst_id = vote.installation_id as u64;
        let (owner, repo) = split_full_name(&vote.repository_full_name);

        // Post vote results (if some were returned; if the vote comment was
        // removed, the vote will be closed but no results will be received)
        if let Some(results) = &results {
            // Post comment on the issue/pr
            let body = tmpl::VoteClosed::new(results).render()?;
            self.gh.post_comment(inst_id, owner, repo, vote.issue_number, &body).await?;

            // Post announcement to discussions if enabled in vote profile
            if let Some(discussions) = &vote.cfg.announcements.and_then(|a| a.discussions) {
                let issue_title = vote.issue_title.unwrap_or_default();
                let title = build_announcement_title(vote.issue_number, &issue_title);
                let body =
                    tmpl::VoteClosedAnnouncement::new(vote.issue_number, &issue_title, results).render()?;
                if let Err(err) = self
                    .gh
                    .create_discussion(inst_id, owner, repo, &discussions.category, &title, &body)
                    .await
                {
                    // Permissions required to create discussions may not have
                    // been granted yet for this installation, so we raise a
                    // warning and continue
                    warn!(?err, "error creating announcement discussion");
                }
            }
        }

        // Create check run if the vote is on a pull request
        if vote.is_pull_request {
            let (conclusion, summary) = if let Some(results) = &results {
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
                ("success", "The vote was cancelled".to_string())
            };
            let check_details = CheckDetails {
                status: "completed".to_string(),
                conclusion: Some(conclusion.to_string()),
                summary,
            };
            self.gh.create_check_run(inst_id, owner, repo, vote.issue_number, &check_details).await?;
        }

        // Update issue/pr labels
        let mut labels_to_add = vec![VOTE_CLOSED_LABEL];
        if let Some(results) = &results {
            if results.passed {
                labels_to_add.push(VOTE_PASSED_LABEL);
            } else {
                labels_to_add.push(VOTE_FAILED_LABEL);
            }
        }
        _ = self.gh.remove_label(inst_id, owner, repo, vote.issue_number, VOTE_OPEN_LABEL).await;
        self.gh.add_labels(inst_id, owner, repo, vote.issue_number, &labels_to_add).await?;

        debug!("closed");
        Ok(Some(()))
    }
}

/// Worker that periodically runs status check commands.
struct StatusChecker {
    db: DynDB,
    cmds_tx: async_channel::Sender<Command>,
}

impl StatusChecker {
    /// Create a new status checker instance.
    pub(crate) fn new(db: DynDB, cmds_tx: async_channel::Sender<Command>) -> Self {
        Self { db, cmds_tx }
    }

    /// Start status checker.
    fn run(self, cancel_token: CancellationToken) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                // Process pending periodic status checks
                match self.db.get_pending_status_checks().await {
                    Ok(inputs) => {
                        // Enqueue check vote commands
                        for input in inputs {
                            _ = self.cmds_tx.send(Command::CheckVote(input)).await;
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

/// Worker that periodically auto closes votes that have already passed. This
/// only applies to votes with the `close_on_passing` feature enabled.
struct VotesAutoCloser {
    db: DynDB,
    gh: DynGH,
}

impl VotesAutoCloser {
    /// Create a new votes auto closer instance.
    pub(crate) fn new(db: DynDB, gh: DynGH) -> Self {
        Self { db, gh }
    }

    /// Start votes auto closer.
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
                                    // If the vote has already passed, update its ending timestamp
                                    if results.passed {
                                        if let Err(err) = self.db.update_vote_ends_at(vote_id).await {
                                            error!(?err, ?vote.vote_id, "error updating vote ends at");
                                        }
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

/// Helper function to build an announcement title.
fn build_announcement_title(issue_number: i64, issue_title: &str) -> String {
    format!("{issue_title} #{issue_number} (vote closed)")
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, vec};

    use anyhow::format_err;
    use mockall::predicate::eq;
    use time::ext::NumericalDuration;

    use crate::results::{Vote, REACTION_IN_FAVOR};
    use crate::testutil::*;
    use crate::{cfg_repo::AllowedVoters, db::MockDB, github::*};

    use super::*;

    #[tokio::test]
    async fn votes_processor_stops_when_requested() {
        let mut db = MockDB::new();
        db.expect_close_finished_vote().times(1).returning(|_| Box::pin(future::ready(Ok(None))));
        db.expect_get_pending_status_checks()
            .times(1)
            .returning(|| Box::pin(future::ready(Ok(vec![]))));
        db.expect_get_open_votes_with_close_on_passing()
            .times(1)
            .returning(|| Box::pin(future::ready(Ok(vec![]))));
        let gh = MockGH::new();

        let (cmds_tx, cmds_rx) = async_channel::unbounded();
        let cancel_token = CancellationToken::new();
        let votes_processor = Processor::new(Arc::new(db), Arc::new(gh), cmds_tx, cmds_rx);
        let votes_processor_handle = votes_processor.run(&cancel_token);
        cancel_token.cancel();

        assert!(votes_processor_handle.await.iter().all(Result::is_ok));
    }

    #[tokio::test]
    async fn commands_handler_stops_when_requested() {
        let db = MockDB::new();
        let gh = MockGH::new();

        let (_, cmds_rx) = async_channel::unbounded();
        let cancel_token = CancellationToken::new();
        let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
        let cmds_handler_handle = cmds_handler.run(cancel_token.clone());
        cancel_token.cancel();

        assert!(cmds_handler_handle.await.is_ok());
    }

    #[tokio::test]
    async fn commands_handler_stops_after_processing_queued_cmd() {
        let mut db = MockDB::new();
        db.expect_cancel_vote()
            .with(eq(REPOFN), eq(ISSUE_NUM))
            .times(1)
            .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
        let mut gh = MockGH::new();
        gh.expect_user_is_collaborator()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
            .times(1)
            .returning(|_, _, _, _| Box::pin(future::ready(Ok(true))));

        let (cmds_tx, cmds_rx) = async_channel::unbounded();
        let cancel_token = CancellationToken::new();
        let event = setup_test_issue_event();
        let cmd = Command::CancelVote(CancelVoteInput::new(&Event::Issue(event)));
        cmds_tx.send(cmd).await.unwrap();
        let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx.clone());
        let cmds_handler_handle = cmds_handler.run(cancel_token.clone());
        cancel_token.cancel();

        assert!(cmds_handler_handle.await.is_ok());
        assert!(cmds_rx.is_empty());
    }

    #[tokio::test]
    async fn votes_closer_stops_when_requested_none_closed() {
        let mut db = MockDB::new();
        db.expect_close_finished_vote().times(1).returning(|_| Box::pin(future::ready(Ok(None))));
        let gh = MockGH::new();

        let cancel_token = CancellationToken::new();
        let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
        let votes_closer_handle = votes_closer.run(cancel_token.clone());
        cancel_token.cancel();

        assert!(votes_closer_handle.await.is_ok());
    }

    #[tokio::test]
    async fn votes_closer_stops_when_requested_error_closing() {
        let mut db = MockDB::new();
        db.expect_close_finished_vote()
            .times(1)
            .returning(|_| Box::pin(future::ready(Err(format_err!(ERROR)))));
        let gh = MockGH::new();

        let cancel_token = CancellationToken::new();
        let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
        let votes_closer_handle = votes_closer.run(cancel_token.clone());
        cancel_token.cancel();

        assert!(votes_closer_handle.await.is_ok());
    }

    #[tokio::test]
    async fn status_checker_stops_when_requested_none_pending() {
        let mut db = MockDB::new();
        db.expect_get_pending_status_checks()
            .times(1)
            .returning(|| Box::pin(future::ready(Ok(vec![]))));

        let (cmds_tx, cmds_rx) = async_channel::unbounded();
        let cancel_token = CancellationToken::new();
        let status_checker = StatusChecker::new(Arc::new(db), cmds_tx);
        let status_checker_handle = status_checker.run(cancel_token.clone());
        cancel_token.cancel();

        assert!(status_checker_handle.await.is_ok());
        assert!(cmds_rx.is_empty());
    }

    #[tokio::test]
    async fn status_checker_stops_after_processing_pending_status_check() {
        let check_vote_input = CheckVoteInput {
            repository_full_name: "repo_full_name".to_string(),
            issue_number: 1,
        };
        let check_vote_input_copy = check_vote_input.clone();

        let mut db = MockDB::new();
        db.expect_get_pending_status_checks()
            .times(1)
            .returning(move || Box::pin(future::ready(Ok(vec![check_vote_input_copy.clone()]))));

        let (cmds_tx, cmds_rx) = async_channel::unbounded();
        let cancel_token = CancellationToken::new();
        let status_checker = StatusChecker::new(Arc::new(db), cmds_tx);
        let status_checker_handle = status_checker.run(cancel_token.clone());
        cancel_token.cancel();

        assert!(status_checker_handle.await.is_ok());
        assert_eq!(
            cmds_rx.recv().await.unwrap(),
            Command::CheckVote(check_vote_input)
        );
    }

    #[tokio::test]
    async fn status_checker_stops_when_requested_error_getting_pending() {
        let mut db = MockDB::new();
        db.expect_get_pending_status_checks()
            .times(1)
            .returning(|| Box::pin(future::ready(Err(format_err!(ERROR)))));

        let (cmds_tx, cmds_rx) = async_channel::unbounded();
        let cancel_token = CancellationToken::new();
        let status_checker = StatusChecker::new(Arc::new(db), cmds_tx);
        let status_checker_handle = status_checker.run(cancel_token.clone());
        cancel_token.cancel();

        assert!(status_checker_handle.await.is_ok());
        assert!(cmds_rx.is_empty());
    }

    #[tokio::test]
    async fn votes_auto_closer_stops_when_requested_none_pending() {
        let mut db = MockDB::new();
        db.expect_get_open_votes_with_close_on_passing()
            .times(1)
            .returning(|| Box::pin(future::ready(Ok(vec![]))));
        let gh = MockGH::new();

        let cancel_token = CancellationToken::new();
        let votes_auto_closer = VotesAutoCloser::new(Arc::new(db), Arc::new(gh));
        let votes_auto_closer_handle = votes_auto_closer.run(cancel_token.clone());
        cancel_token.cancel();

        assert!(votes_auto_closer_handle.await.is_ok());
    }

    #[tokio::test]
    async fn votes_auto_closer_stops_after_processing_pending_vote() {
        let mut db = MockDB::new();
        db.expect_get_open_votes_with_close_on_passing()
            .times(1)
            .returning(move || Box::pin(future::ready(Ok(vec![setup_test_vote()]))));
        db.expect_update_vote_ends_at()
            .times(1)
            .returning(move |_| Box::pin(future::ready(Ok(()))));

        let mut gh = MockGH::new();
        gh.expect_get_comment_reactions()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(COMMENT_ID))
            .times(1)
            .returning(|_, _, _, _| {
                Box::pin(future::ready(Ok(vec![Reaction {
                    user: User {
                        login: USER1.to_string(),
                    },
                    content: REACTION_IN_FAVOR.to_string(),
                    created_at: TIMESTAMP.to_string(),
                }])))
            });
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

        let cancel_token = CancellationToken::new();
        let votes_auto_closer = VotesAutoCloser::new(Arc::new(db), Arc::new(gh));
        let votes_auto_closer_handle = votes_auto_closer.run(cancel_token.clone());
        cancel_token.cancel();

        assert!(votes_auto_closer_handle.await.is_ok());
    }

    #[tokio::test]
    async fn votes_auto_closer_stops_when_requested_error_getting_pending() {
        let mut db = MockDB::new();
        db.expect_get_open_votes_with_close_on_passing()
            .times(1)
            .returning(|| Box::pin(future::ready(Err(format_err!(ERROR)))));
        let gh = MockGH::new();

        let cancel_token = CancellationToken::new();
        let votes_auto_closer = VotesAutoCloser::new(Arc::new(db), Arc::new(gh));
        let votes_auto_closer_handle = votes_auto_closer.run(cancel_token.clone());
        cancel_token.cancel();

        assert!(votes_auto_closer_handle.await.is_ok());
    }

    #[tokio::test]
    async fn create_vote_error_getting_configuration_profile() {
        let db = MockDB::new();
        let mut gh = MockGH::new();
        gh.expect_get_config_file()
            .with(eq(INST_ID), eq(ORG), eq(REPO))
            .times(1)
            .returning(|_, _, _| Box::pin(future::ready(None)));
        gh.expect_post_comment()
            .withf(|inst_id, owner, repo, issue_number, body| {
                let expected_body = tmpl::ConfigNotFound {}.render().unwrap();
                *inst_id == INST_ID
                    && owner == ORG
                    && repo == REPO
                    && *issue_number == ISSUE_NUM
                    && body == expected_body.as_str()
            })
            .times(1)
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));

        let (_, cmds_rx) = async_channel::unbounded();
        let event = setup_test_issue_event();
        let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
        cmds_handler.create_vote(&CreateVoteInput::new(None, &Event::Issue(event))).await.unwrap();
    }

    #[tokio::test]
    async fn create_vote_non_collaborator() {
        let db = MockDB::new();
        let mut gh = MockGH::new();
        gh.expect_get_config_file()
            .with(eq(INST_ID), eq(ORG), eq(REPO))
            .times(1)
            .returning(|_, _, _| Box::pin(future::ready(Some(get_test_valid_config()))));
        gh.expect_user_is_collaborator()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
            .times(1)
            .returning(|_, _, _, _| Box::pin(future::ready(Ok(false))));
        gh.expect_post_comment()
            .withf(|inst_id, owner, repo, issue_number, body| {
                let expected_body = tmpl::VoteRestricted::new(USER).render().unwrap();
                *inst_id == INST_ID
                    && owner == ORG
                    && repo == REPO
                    && *issue_number == ISSUE_NUM
                    && body == expected_body.as_str()
            })
            .times(1)
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));

        let (_, cmds_rx) = async_channel::unbounded();
        let event = setup_test_issue_event();
        let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
        cmds_handler.create_vote(&CreateVoteInput::new(None, &Event::Issue(event))).await.unwrap();
    }

    #[tokio::test]
    async fn create_vote_issue_already_has_a_vote() {
        let mut db = MockDB::new();
        db.expect_has_vote_open()
            .with(eq(REPOFN), eq(ISSUE_NUM))
            .times(1)
            .returning(|_, _| Box::pin(future::ready(Ok(true))));
        let mut gh = MockGH::new();
        gh.expect_get_config_file()
            .with(eq(INST_ID), eq(ORG), eq(REPO))
            .times(1)
            .returning(|_, _, _| Box::pin(future::ready(Some(get_test_valid_config()))));
        gh.expect_user_is_collaborator()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
            .times(1)
            .returning(|_, _, _, _| Box::pin(future::ready(Ok(true))));
        gh.expect_post_comment()
            .withf(|inst_id, owner, repo, issue_number, body| {
                let expected_body = tmpl::VoteInProgress::new(USER, false).render().unwrap();
                *inst_id == INST_ID
                    && owner == ORG
                    && repo == REPO
                    && *issue_number == ISSUE_NUM
                    && body == expected_body.as_str()
            })
            .times(1)
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));

        let (_, cmds_rx) = async_channel::unbounded();
        let event = setup_test_issue_event();
        let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
        cmds_handler.create_vote(&CreateVoteInput::new(None, &Event::Issue(event))).await.unwrap();
    }

    #[tokio::test]
    async fn create_vote_success() {
        let event = setup_test_issue_event();
        let create_vote_input = CreateVoteInput::new(None, &Event::Issue(event));
        let cfg = CfgProfile {
            duration: Duration::from_secs(300),
            pass_threshold: 50.0,
            allowed_voters: Some(AllowedVoters::default()),
            ..Default::default()
        };

        let mut db = MockDB::new();
        db.expect_has_vote_open()
            .with(eq(REPOFN), eq(ISSUE_NUM))
            .times(1)
            .returning(|_, _| Box::pin(future::ready(Ok(false))));
        let cfg_copy = cfg.clone();
        let create_vote_input_copy = create_vote_input.clone();
        db.expect_store_vote()
            .withf(move |vote_comment_id, input, cfg| {
                *vote_comment_id == COMMENT_ID && *input == create_vote_input_copy && *cfg == cfg_copy
            })
            .times(1)
            .returning(|_, _, _| Box::pin(future::ready(Ok(Uuid::new_v4()))));
        let mut gh = MockGH::new();
        gh.expect_get_config_file()
            .with(eq(INST_ID), eq(ORG), eq(REPO))
            .times(1)
            .returning(|_, _, _| Box::pin(future::ready(Some(get_test_valid_config()))));
        gh.expect_user_is_collaborator()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
            .times(1)
            .returning(|_, _, _, _| Box::pin(future::ready(Ok(true))));
        let create_vote_input_copy = create_vote_input.clone();
        gh.expect_post_comment()
            .withf(move |inst_id, owner, repo, issue_number, body| {
                let expected_body = tmpl::VoteCreated::new(&create_vote_input_copy, &cfg).render().unwrap();
                *inst_id == INST_ID
                    && owner == ORG
                    && repo == REPO
                    && *issue_number == ISSUE_NUM
                    && body == expected_body.as_str()
            })
            .times(1)
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
        gh.expect_remove_label()
            .with(
                eq(INST_ID),
                eq(ORG),
                eq(REPO),
                eq(ISSUE_NUM),
                eq(VOTE_CLOSED_LABEL),
            )
            .times(1)
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
        gh.expect_remove_label()
            .with(
                eq(INST_ID),
                eq(ORG),
                eq(REPO),
                eq(ISSUE_NUM),
                eq(VOTE_PASSED_LABEL),
            )
            .times(1)
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
        gh.expect_remove_label()
            .with(
                eq(INST_ID),
                eq(ORG),
                eq(REPO),
                eq(ISSUE_NUM),
                eq(VOTE_FAILED_LABEL),
            )
            .times(1)
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
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

        let (_, cmds_rx) = async_channel::unbounded();
        let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
        cmds_handler.create_vote(&create_vote_input).await.unwrap();
    }

    #[tokio::test]
    async fn create_vote_pr_success() {
        let event = setup_test_pr_event();
        let create_vote_input = CreateVoteInput::new(None, &Event::PullRequest(event));
        let cfg = CfgProfile {
            duration: Duration::from_secs(300),
            pass_threshold: 50.0,
            allowed_voters: Some(AllowedVoters::default()),
            ..Default::default()
        };

        let mut db = MockDB::new();
        db.expect_has_vote_open()
            .with(eq(REPOFN), eq(ISSUE_NUM))
            .times(1)
            .returning(|_, _| Box::pin(future::ready(Ok(false))));
        let cfg_copy = cfg.clone();
        let create_vote_input_copy = create_vote_input.clone();
        db.expect_store_vote()
            .withf(move |vote_comment_id, input, cfg| {
                *vote_comment_id == COMMENT_ID && *input == create_vote_input_copy && *cfg == cfg_copy
            })
            .times(1)
            .returning(|_, _, _| Box::pin(future::ready(Ok(Uuid::new_v4()))));
        let mut gh = MockGH::new();
        gh.expect_get_config_file()
            .with(eq(INST_ID), eq(ORG), eq(REPO))
            .times(1)
            .returning(|_, _, _| Box::pin(future::ready(Some(get_test_valid_config()))));
        gh.expect_user_is_collaborator()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
            .times(1)
            .returning(|_, _, _, _| Box::pin(future::ready(Ok(true))));
        let create_vote_input_copy = create_vote_input.clone();
        gh.expect_post_comment()
            .withf(move |inst_id, owner, repo, issue_number, body| {
                let expected_body = tmpl::VoteCreated::new(&create_vote_input_copy, &cfg).render().unwrap();
                *inst_id == INST_ID
                    && owner == ORG
                    && repo == REPO
                    && *issue_number == ISSUE_NUM
                    && body == expected_body.as_str()
            })
            .times(1)
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
        gh.expect_create_check_run()
            .with(
                eq(INST_ID),
                eq(ORG),
                eq(REPO),
                eq(ISSUE_NUM),
                eq(CheckDetails {
                    status: "in_progress".to_string(),
                    conclusion: None,
                    summary: "Vote open".to_string(),
                }),
            )
            .times(1)
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
        gh.expect_remove_label()
            .with(
                eq(INST_ID),
                eq(ORG),
                eq(REPO),
                eq(ISSUE_NUM),
                eq(VOTE_CLOSED_LABEL),
            )
            .times(1)
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
        gh.expect_remove_label()
            .with(
                eq(INST_ID),
                eq(ORG),
                eq(REPO),
                eq(ISSUE_NUM),
                eq(VOTE_PASSED_LABEL),
            )
            .times(1)
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
        gh.expect_remove_label()
            .with(
                eq(INST_ID),
                eq(ORG),
                eq(REPO),
                eq(ISSUE_NUM),
                eq(VOTE_FAILED_LABEL),
            )
            .times(1)
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
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

        let (_, cmds_rx) = async_channel::unbounded();
        let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
        cmds_handler.create_vote(&create_vote_input).await.unwrap();
    }

    #[tokio::test]
    #[should_panic(expected = "error checking if user is collaborator")]
    async fn cancel_vote_error_checking_if_user_is_collaborator() {
        let db = MockDB::new();
        let mut gh = MockGH::new();
        gh.expect_user_is_collaborator()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
            .times(1)
            .returning(|_, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));

        let (_, cmds_rx) = async_channel::unbounded();
        let event = setup_test_issue_comment_event();
        let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
        cmds_handler
            .cancel_vote(&CancelVoteInput::new(&Event::IssueComment(event)))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn cancel_vote_only_collaborators_can_close_votes() {
        let db = MockDB::new();
        let mut gh = MockGH::new();
        gh.expect_user_is_collaborator()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
            .times(1)
            .returning(|_, _, _, _| Box::pin(future::ready(Ok(false))));

        let (_, cmds_rx) = async_channel::unbounded();
        let event = setup_test_issue_comment_event();
        let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
        cmds_handler
            .cancel_vote(&CancelVoteInput::new(&Event::IssueComment(event)))
            .await
            .unwrap();
    }

    #[tokio::test]
    #[should_panic(expected = "error cancelling vote")]
    async fn cancel_vote_error_cancelling() {
        let mut db = MockDB::new();
        db.expect_cancel_vote()
            .with(eq(REPOFN), eq(ISSUE_NUM))
            .times(1)
            .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
        let mut gh = MockGH::new();
        gh.expect_user_is_collaborator()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
            .times(1)
            .returning(|_, _, _, _| Box::pin(future::ready(Ok(true))));

        let (_, cmds_rx) = async_channel::unbounded();
        let event = setup_test_issue_comment_event();
        let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
        cmds_handler
            .cancel_vote(&CancelVoteInput::new(&Event::IssueComment(event)))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn cancel_vote_no_vote_in_progress() {
        let mut db = MockDB::new();
        db.expect_cancel_vote()
            .with(eq(REPOFN), eq(ISSUE_NUM))
            .times(1)
            .returning(|_, _| Box::pin(future::ready(Ok(None))));
        let mut gh = MockGH::new();
        gh.expect_user_is_collaborator()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
            .times(1)
            .returning(|_, _, _, _| Box::pin(future::ready(Ok(true))));
        gh.expect_post_comment()
            .withf(|inst_id, owner, repo, issue_number, body| {
                let expected_body = tmpl::NoVoteInProgress::new(USER, false).render().unwrap();
                *inst_id == INST_ID
                    && owner == ORG
                    && repo == REPO
                    && *issue_number == ISSUE_NUM
                    && body == expected_body.as_str()
            })
            .times(1)
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));

        let (_, cmds_rx) = async_channel::unbounded();
        let event = setup_test_issue_comment_event();
        let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
        cmds_handler
            .cancel_vote(&CancelVoteInput::new(&Event::IssueComment(event)))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn cancel_vote_success() {
        let mut db = MockDB::new();
        db.expect_cancel_vote()
            .with(eq(REPOFN), eq(ISSUE_NUM))
            .times(1)
            .returning(|_, _| Box::pin(future::ready(Ok(Some(Uuid::parse_str(VOTE_ID).unwrap())))));
        let mut gh = MockGH::new();
        gh.expect_user_is_collaborator()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
            .times(1)
            .returning(|_, _, _, _| Box::pin(future::ready(Ok(true))));
        gh.expect_post_comment()
            .withf(|inst_id, owner, repo, issue_number, body| {
                let expected_body = tmpl::VoteCancelled::new(USER, false).render().unwrap();
                *inst_id == INST_ID
                    && owner == ORG
                    && repo == REPO
                    && *issue_number == ISSUE_NUM
                    && body == expected_body.as_str()
            })
            .times(1)
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
        gh.expect_remove_label()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM), eq(VOTE_OPEN_LABEL))
            .times(1)
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));

        let (_, cmds_rx) = async_channel::unbounded();
        let event = setup_test_issue_comment_event();
        let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
        cmds_handler
            .cancel_vote(&CancelVoteInput::new(&Event::IssueComment(event)))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn cancel_vote_in_pr_success() {
        let mut db = MockDB::new();
        db.expect_cancel_vote()
            .with(eq(REPOFN), eq(ISSUE_NUM))
            .times(1)
            .returning(|_, _| Box::pin(future::ready(Ok(Some(Uuid::parse_str(VOTE_ID).unwrap())))));
        let mut gh = MockGH::new();
        gh.expect_user_is_collaborator()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
            .times(1)
            .returning(|_, _, _, _| Box::pin(future::ready(Ok(true))));
        gh.expect_post_comment()
            .withf(|inst_id, owner, repo, issue_number, body| {
                let expected_body = tmpl::VoteCancelled::new(USER, true).render().unwrap();
                *inst_id == INST_ID
                    && owner == ORG
                    && repo == REPO
                    && *issue_number == ISSUE_NUM
                    && body == expected_body.as_str()
            })
            .times(1)
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
        gh.expect_create_check_run()
            .with(
                eq(INST_ID),
                eq(ORG),
                eq(REPO),
                eq(ISSUE_NUM),
                eq(CheckDetails {
                    status: "completed".to_string(),
                    conclusion: Some("success".to_string()),
                    summary: "Vote cancelled".to_string(),
                }),
            )
            .times(1)
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
        gh.expect_remove_label()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM), eq(VOTE_OPEN_LABEL))
            .times(1)
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));

        let (_, cmds_rx) = async_channel::unbounded();
        let event = setup_test_pr_event();
        let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
        cmds_handler.cancel_vote(&CancelVoteInput::new(&Event::PullRequest(event))).await.unwrap();
    }

    #[tokio::test]
    #[should_panic(expected = "error getting open vote")]
    async fn check_vote_error_getting_vote() {
        let mut db = MockDB::new();
        db.expect_get_open_vote()
            .with(eq(REPOFN), eq(ISSUE_NUM))
            .times(1)
            .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
        let gh = MockGH::new();

        let (_, cmds_rx) = async_channel::unbounded();
        let event = setup_test_pr_event();
        let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
        cmds_handler.check_vote(&CheckVoteInput::new(&Event::PullRequest(event))).await.unwrap();
    }

    #[tokio::test]
    async fn check_vote_not_found() {
        let mut db = MockDB::new();
        db.expect_get_open_vote()
            .with(eq(REPOFN), eq(ISSUE_NUM))
            .times(1)
            .returning(|_, _| Box::pin(future::ready(Ok(None))));
        let gh = MockGH::new();

        let (_, cmds_rx) = async_channel::unbounded();
        let event = setup_test_pr_event();
        let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
        cmds_handler.check_vote(&CheckVoteInput::new(&Event::PullRequest(event))).await.unwrap();
    }

    #[tokio::test]
    async fn check_vote_checked_recently() {
        let mut db = MockDB::new();
        db.expect_get_open_vote().with(eq(REPOFN), eq(ISSUE_NUM)).times(1).returning(|_, _| {
            Box::pin(future::ready(Ok(Some(Vote {
                checked_at: OffsetDateTime::now_utc().checked_sub(1.hours()),
                ..setup_test_vote()
            }))))
        });
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

        let (_, cmds_rx) = async_channel::unbounded();
        let event = setup_test_pr_event();
        let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
        cmds_handler.check_vote(&CheckVoteInput::new(&Event::PullRequest(event))).await.unwrap();
    }

    #[tokio::test]
    async fn check_vote_success() {
        let mut db = MockDB::new();
        db.expect_get_open_vote()
            .with(eq(REPOFN), eq(ISSUE_NUM))
            .times(1)
            .returning(|_, _| Box::pin(future::ready(Ok(Some(setup_test_vote())))));
        db.expect_update_vote_last_check()
            .with(eq(Uuid::parse_str(VOTE_ID).unwrap()))
            .times(1)
            .returning(|_| Box::pin(future::ready(Ok(()))));
        let mut gh = MockGH::new();
        gh.expect_get_comment_reactions()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(COMMENT_ID))
            .times(1)
            .returning(|_, _, _, _| {
                Box::pin(future::ready(Ok(vec![Reaction {
                    user: User {
                        login: USER1.to_string(),
                    },
                    content: REACTION_IN_FAVOR.to_string(),
                    created_at: TIMESTAMP.to_string(),
                }])))
            });
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

        let (_, cmds_rx) = async_channel::unbounded();
        let event = setup_test_pr_event();
        let cmds_handler = CommandsHandler::new(Arc::new(db), Arc::new(gh), cmds_rx);
        cmds_handler.check_vote(&CheckVoteInput::new(&Event::PullRequest(event))).await.unwrap();
    }

    #[tokio::test]
    #[should_panic(expected = "error closing finished vote")]
    async fn close_finished_vote_error_closing() {
        let mut db = MockDB::new();
        db.expect_close_finished_vote()
            .times(1)
            .returning(|_| Box::pin(future::ready(Err(format_err!(ERROR)))));
        let gh = MockGH::new();

        let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
        votes_closer.close_finished_vote().await.unwrap();
    }

    #[tokio::test]
    async fn close_finished_vote_none_closed() {
        let mut db = MockDB::new();
        db.expect_close_finished_vote().times(1).returning(|_| Box::pin(future::ready(Ok(None))));
        let gh = MockGH::new();

        let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
        votes_closer.close_finished_vote().await.unwrap();
    }

    #[tokio::test]
    async fn close_finished_vote_on_issue() {
        let results = setup_test_vote_results();
        let results_copy = results.clone();
        let results_copy2 = results.clone();

        let mut db = MockDB::new();
        db.expect_close_finished_vote().times(1).returning(move |_| {
            Box::pin(future::ready(Ok(Some((
                setup_test_vote(),
                Some(results_copy.clone()),
            )))))
        });
        let mut gh = MockGH::new();
        gh.expect_post_comment()
            .withf(move |inst_id, owner, repo, issue_number, body| {
                let expected_body = tmpl::VoteClosed::new(&results_copy2).render().unwrap();
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
                let expected_body =
                    tmpl::VoteClosedAnnouncement::new(ISSUE_NUM, TITLE, &results).render().unwrap();
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

        let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
        votes_closer.close_finished_vote().await.unwrap();
    }

    #[tokio::test]
    async fn close_finished_vote_on_pr_closed_vote_passed() {
        let results = setup_test_vote_results();
        let results_copy = results.clone();
        let results_copy2 = results.clone();

        let mut db = MockDB::new();
        db.expect_close_finished_vote().times(1).returning(move |_| {
            let mut vote = setup_test_vote();
            vote.is_pull_request = true;
            Box::pin(future::ready(Ok(Some((vote, Some(results_copy.clone()))))))
        });
        let mut gh = MockGH::new();
        gh.expect_post_comment()
            .withf(move |inst_id, owner, repo, issue_number, body| {
                let expected_body = tmpl::VoteClosed::new(&results_copy2).render().unwrap();
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
                let expected_body =
                    tmpl::VoteClosedAnnouncement::new(ISSUE_NUM, TITLE, &results).render().unwrap();
                *inst_id == INST_ID
                    && owner == ORG
                    && repo == REPO
                    && category == DISCUSSIONS_CATEGORY
                    && title == build_announcement_title(ISSUE_NUM, TITLE)
                    && body == expected_body.as_str()
            })
            .times(1)
            .returning(|_, _, _, _, _, _| Box::pin(future::ready(Ok(()))));
        gh.expect_create_check_run()
            .with(
                eq(INST_ID),
                eq(ORG),
                eq(REPO),
                eq(ISSUE_NUM),
                eq(CheckDetails {
                    status: "completed".to_string(),
                    conclusion: Some("success".to_string()),
                    summary: "The vote passed! 1 out of 1 voted in favor.".to_string(),
                }),
            )
            .times(1)
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
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

        let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
        votes_closer.close_finished_vote().await.unwrap();
    }

    #[tokio::test]
    async fn close_finished_vote_on_pr_closed_vote_not_passed() {
        let mut results = setup_test_vote_results();
        results.passed = false;
        results.in_favor_percentage = 0.0;
        results.in_favor = 0;
        results.against = 1;
        let results_copy = results.clone();
        let results_copy2 = results.clone();

        let mut db = MockDB::new();
        db.expect_close_finished_vote().times(1).returning(move |_| {
            let mut vote = setup_test_vote();
            vote.is_pull_request = true;
            Box::pin(future::ready(Ok(Some((vote, Some(results_copy.clone()))))))
        });
        let mut gh = MockGH::new();
        gh.expect_post_comment()
            .withf(move |inst_id, owner, repo, issue_number, body| {
                let expected_body = tmpl::VoteClosed::new(&results_copy2).render().unwrap();
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
                let expected_body =
                    tmpl::VoteClosedAnnouncement::new(ISSUE_NUM, TITLE, &results).render().unwrap();
                *inst_id == INST_ID
                    && owner == ORG
                    && repo == REPO
                    && category == DISCUSSIONS_CATEGORY
                    && title == build_announcement_title(ISSUE_NUM, TITLE)
                    && body == expected_body.as_str()
            })
            .times(1)
            .returning(|_, _, _, _, _, _| Box::pin(future::ready(Ok(()))));
        gh.expect_create_check_run()
            .with(
                eq(INST_ID),
                eq(ORG),
                eq(REPO),
                eq(ISSUE_NUM),
                eq(CheckDetails {
                    status: "completed".to_string(),
                    conclusion: Some("failure".to_string()),
                    summary: "The vote did not pass. 0 out of 1 voted in favor.".to_string(),
                }),
            )
            .times(1)
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
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

        let votes_closer = VotesCloser::new(Arc::new(db), Arc::new(gh));
        votes_closer.close_finished_vote().await.unwrap();
    }
}
