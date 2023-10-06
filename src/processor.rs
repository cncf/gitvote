use crate::{
    cfg::{CfgError, CfgProfile},
    cmd::{CancelVoteInput, CheckVoteInput, Command, CreateVoteInput},
    db::DynDB,
    github::{split_full_name, CheckDetails, DynGH},
    results, tmpl,
};
use anyhow::Result;
use askama::Template;
use futures::future::{self, JoinAll};
use std::{sync::Arc, time::Duration};
use time::OffsetDateTime;
use tokio::{
    sync::broadcast::{self, error::TryRecvError},
    task::JoinHandle,
    time::sleep,
};
use tracing::{debug, error, instrument};
use uuid::Uuid;

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

/// A votes processor is in charge of creating the requested votes, stopping
/// them at the scheduled time and publishing results, etc.
pub(crate) struct Processor {
    db: DynDB,
    gh: DynGH,
}

impl Processor {
    /// Create a new votes processor instance.
    pub(crate) fn new(db: DynDB, gh: DynGH) -> Arc<Self> {
        Arc::new(Self { db, gh })
    }

    /// Start votes processor.
    pub(crate) fn start(
        self: Arc<Self>,
        cmds_tx: async_channel::Sender<Command>,
        cmds_rx: &async_channel::Receiver<Command>,
        stop_tx: &broadcast::Sender<()>,
    ) -> JoinAll<JoinHandle<()>> {
        let num_workers = COMMANDS_HANDLERS_WORKERS + VOTES_CLOSERS_WORKERS + 2; // Status checker + auto closer
        let mut workers_handles = Vec::with_capacity(num_workers);

        // Launch commands handler workers
        for _ in 0..COMMANDS_HANDLERS_WORKERS {
            let handle = self
                .clone()
                .commands_handler(cmds_rx.clone(), stop_tx.subscribe());
            workers_handles.push(handle);
        }

        // Launch votes closer workers
        for _ in 0..VOTES_CLOSERS_WORKERS {
            let handle = self.clone().votes_closer(stop_tx.subscribe());
            workers_handles.push(handle);
        }

        // Launch status checker
        let status_checker_handle = self.clone().status_checker(cmds_tx, stop_tx.subscribe());
        workers_handles.push(status_checker_handle);

        // Launch votes auto closer
        let votes_auto_closer_handle = self.votes_auto_closer(stop_tx.subscribe());
        workers_handles.push(votes_auto_closer_handle);

        future::join_all(workers_handles)
    }

    /// Worker that receives commands from the queue and executes them.
    /// Commands are added to the queue when certain events from GitHub are
    /// received on the webhook endpoint.
    fn commands_handler(
        self: Arc<Self>,
        cmds_rx: async_channel::Receiver<Command>,
        mut stop_rx: broadcast::Receiver<()>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;

                    // Pick next command from the queue and process it
                    Ok(cmd) = cmds_rx.recv() => {
                        match cmd {
                            Command::CreateVote(input) => {
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

                    // Exit if the votes processor has been asked to stop
                    _ = stop_rx.recv() => {
                        break
                    }
                }
            }
        })
    }

    /// Worker that periodically checks the database for votes that should be
    /// closed and closes them.
    fn votes_closer(self: Arc<Self>, mut stop_rx: broadcast::Receiver<()>) -> JoinHandle<()> {
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
                        _ = stop_rx.recv() => break,
                    },
                    Err(_) => {
                        // Something went wrong closing finished vote, pause
                        // unless we've been asked to stop
                        tokio::select! {
                            () = sleep(VOTES_CLOSER_PAUSE_ON_ERROR) => {},
                            _ = stop_rx.recv() => break,
                        }
                    }
                }

                // Exit if the votes processor has been asked to stop
                if let Some(TryRecvError::Closed) = stop_rx.try_recv().err() {
                    break;
                }
            }
        })
    }

    /// Worker that periodically runs status check commands.
    fn status_checker(
        self: Arc<Self>,
        cmds_tx: async_channel::Sender<Command>,
        mut stop_rx: broadcast::Receiver<()>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                // Process pending periodic status checks
                match self.db.get_pending_status_checks().await {
                    Ok(inputs) => {
                        // Enqueue check vote commands
                        for input in inputs {
                            _ = cmds_tx.send(Command::CheckVote(input)).await;
                        }
                    }
                    Err(err) => {
                        error!(?err, "error getting pending status checks");
                    }
                }

                // Pause until it's time for the next run or exit if the votes
                // processor has been asked to stop
                tokio::select! {
                    () = sleep(STATUS_CHECK_FREQUENCY) => {},
                    _ = stop_rx.recv() => break,
                }
            }
        })
    }

    /// Worker that periodically auto closes votes that have already passed.
    /// This only applies to votes with the close_on_passing feature enabled.
    fn votes_auto_closer(self: Arc<Self>, mut stop_rx: broadcast::Receiver<()>) -> JoinHandle<()> {
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
                                        if let Err(err) = self.db.update_vote_ends_at(vote_id).await
                                        {
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
                    _ = stop_rx.recv() => break,
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
                    CfgError::InvalidConfig(reason) => {
                        tmpl::InvalidConfig::new(&reason).render()?
                    }
                };
                self.gh
                    .post_comment(inst_id, owner, repo, i.issue_number, &body)
                    .await?;
                return Ok(());
            }
        };

        // Only repository collaborators can create votes
        if !self
            .gh
            .user_is_collaborator(inst_id, owner, repo, &i.created_by)
            .await?
        {
            let body = tmpl::VoteRestricted::new(&i.created_by).render()?;
            self.gh
                .post_comment(inst_id, owner, repo, i.issue_number, &body)
                .await?;
            return Ok(());
        }

        // Only allow one vote open at the same time per issue/pr
        if self
            .db
            .has_vote_open(&i.repository_full_name, i.issue_number)
            .await?
        {
            let body = tmpl::VoteInProgress::new(&i.created_by, i.is_pull_request).render()?;
            self.gh
                .post_comment(inst_id, owner, repo, i.issue_number, &body)
                .await?;
            return Ok(());
        }

        // Post vote created comment on the issue/pr
        let body = tmpl::VoteCreated::new(i, &cfg).render()?;
        let vote_comment_id = self
            .gh
            .post_comment(inst_id, owner, repo, i.issue_number, &body)
            .await?;

        // Store vote information in database
        let vote_id = self.db.store_vote(vote_comment_id, i, &cfg).await?;

        // Create check run if the vote is on a pull request
        if i.is_pull_request {
            let check_details = CheckDetails {
                status: "in_progress".to_string(),
                conclusion: None,
                summary: "Vote open".to_string(),
            };
            self.gh
                .create_check_run(inst_id, owner, repo, i.issue_number, &check_details)
                .await?;
        }

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
            .await?
        {
            return Ok(());
        }

        // Try cancelling the vote open in the issue/pr provided
        let cancelled_vote_id: Option<Uuid> = self
            .db
            .cancel_vote(&i.repository_full_name, i.issue_number)
            .await?;

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
        self.gh
            .post_comment(inst_id, owner, repo, i.issue_number, &body)
            .await?;

        // Create check run if needed
        if cancelled_vote_id.is_some() && i.is_pull_request {
            let check_details = CheckDetails {
                status: "completed".to_string(),
                conclusion: Some("success".to_string()),
                summary: "Vote cancelled".to_string(),
            };
            self.gh
                .create_check_run(inst_id, owner, repo, i.issue_number, &check_details)
                .await?;
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
            .await?
        else {
            return Ok(());
        };

        // Record vote_id as part of the current span
        tracing::Span::current().record("vote_id", &vote.vote_id.to_string());

        // Check if the vote has already been checked recently
        let inst_id = vote.installation_id as u64;
        let (owner, repo) = split_full_name(&vote.repository_full_name);
        if let Some(checked_at) = vote.checked_at {
            if OffsetDateTime::now_utc() - checked_at < MAX_VOTE_CHECK_FREQUENCY {
                // Post comment on the issue/pr and return
                let body = tmpl::VoteCheckedRecently {}.render()?;
                self.gh
                    .post_comment(inst_id, owner, repo, vote.issue_number, &body)
                    .await?;
                return Ok(());
            }
        }

        // Calculate results
        let (owner, repo) = split_full_name(&vote.repository_full_name);
        let results = results::calculate(self.gh.clone(), owner, repo, &vote).await?;

        // Post vote status comment on the issue/pr
        let body = tmpl::VoteStatus::new(&results).render()?;
        self.gh
            .post_comment(inst_id, owner, repo, vote.issue_number, &body)
            .await?;

        // Update vote's last check ts
        self.db.update_vote_last_check(vote.vote_id).await?;

        Ok(())
    }

    /// Close any pending finished vote.
    #[instrument(fields(vote_id), skip_all, err(Debug))]
    async fn close_finished_vote(&self) -> Result<Option<()>> {
        // Try to close any finished vote
        let Some((vote, results)) = self.db.close_finished_vote(self.gh.clone()).await? else {
            return Ok(None);
        };

        // Record vote_id as part of the current span
        tracing::Span::current().record("vote_id", &vote.vote_id.to_string());

        // Post vote closed comment on the issue/pr if results were returned
        // (i.e. if the vote comment was removed, the vote will be closed but
        // no results will be received)
        let inst_id = vote.installation_id as u64;
        let (owner, repo) = split_full_name(&vote.repository_full_name);
        if let Some(results) = &results {
            let body = tmpl::VoteClosed::new(results).render()?;
            self.gh
                .post_comment(inst_id, owner, repo, vote.issue_number, &body)
                .await?;
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
            self.gh
                .create_check_run(inst_id, owner, repo, vote.issue_number, &check_details)
                .await?;
        }

        debug!("closed");
        Ok(Some(()))
    }
}

#[cfg(test)]
mod tests {
    use std::vec;

    use super::*;
    use crate::results::{Vote, REACTION_IN_FAVOR};
    use crate::testutil::*;
    use crate::{cfg::AllowedVoters, db::MockDB, github::*};
    use anyhow::format_err;
    use mockall::predicate::eq;
    use time::ext::NumericalDuration;

    #[tokio::test]
    async fn votes_processor_stops_when_requested() {
        let mut db = MockDB::new();
        db.expect_close_finished_vote()
            .returning(|_| Box::pin(future::ready(Ok(None))));
        db.expect_get_pending_status_checks()
            .returning(|| Box::pin(future::ready(Ok(vec![]))));
        db.expect_get_open_votes_with_close_on_passing()
            .returning(|| Box::pin(future::ready(Ok(vec![]))));
        let gh = MockGH::new();
        let votes_processor = Processor::new(Arc::new(db), Arc::new(gh));

        let (cmds_tx, cmds_rx) = async_channel::unbounded();
        let (stop_tx, _): (broadcast::Sender<()>, _) = broadcast::channel(1);
        let processor = votes_processor.start(cmds_tx, &cmds_rx, &stop_tx);

        drop(stop_tx);
        assert!(processor.await.iter().all(Result::is_ok));
    }

    #[tokio::test]
    async fn commands_handler_stops_when_requested() {
        let db = MockDB::new();
        let gh = MockGH::new();
        let votes_processor = Processor::new(Arc::new(db), Arc::new(gh));

        let (_, cmds_rx) = async_channel::unbounded();
        let (stop_tx, _): (broadcast::Sender<()>, _) = broadcast::channel(1);
        let cmds_handler = votes_processor.commands_handler(cmds_rx.clone(), stop_tx.subscribe());

        drop(stop_tx);
        assert!(cmds_handler.await.is_ok());
    }

    #[tokio::test]
    async fn commands_handler_stops_after_processing_queued_cmd() {
        let mut db = MockDB::new();
        db.expect_cancel_vote()
            .with(eq(REPOFN), eq(ISSUE_NUM))
            .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
        let mut gh = MockGH::new();
        gh.expect_user_is_collaborator()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
            .returning(|_, _, _, _| Box::pin(future::ready(Ok(true))));
        let votes_processor = Processor::new(Arc::new(db), Arc::new(gh));

        let (cmds_tx, cmds_rx) = async_channel::unbounded();
        let event = setup_test_issue_event();
        let cmd = Command::CancelVote(CancelVoteInput::new(&Event::Issue(event)));
        cmds_tx.send(cmd).await.unwrap();
        let (stop_tx, _): (broadcast::Sender<()>, _) = broadcast::channel(1);

        let cmds_handler = votes_processor.commands_handler(cmds_rx.clone(), stop_tx.subscribe());
        drop(stop_tx);
        assert!(cmds_handler.await.is_ok());
        assert!(cmds_rx.is_empty());
    }

    #[tokio::test]
    async fn votes_closer_stops_when_requested_none_closed() {
        let mut db = MockDB::new();
        db.expect_close_finished_vote()
            .returning(|_| Box::pin(future::ready(Ok(None))));
        let gh = MockGH::new();
        let votes_processor = Processor::new(Arc::new(db), Arc::new(gh));

        let (stop_tx, _): (broadcast::Sender<()>, _) = broadcast::channel(1);
        let votes_closer = votes_processor.votes_closer(stop_tx.subscribe());

        drop(stop_tx);
        assert!(votes_closer.await.is_ok());
    }

    #[tokio::test]
    async fn votes_closer_stops_when_requested_error_closing() {
        let mut db = MockDB::new();
        db.expect_close_finished_vote()
            .returning(|_| Box::pin(future::ready(Err(format_err!(ERROR)))));
        let gh = MockGH::new();
        let votes_processor = Processor::new(Arc::new(db), Arc::new(gh));

        let (stop_tx, _): (broadcast::Sender<()>, _) = broadcast::channel(1);
        let votes_closer = votes_processor.votes_closer(stop_tx.subscribe());

        drop(stop_tx);
        assert!(votes_closer.await.is_ok());
    }

    #[tokio::test]
    async fn status_checker_stops_when_requested_none_pending() {
        let mut db = MockDB::new();
        db.expect_get_pending_status_checks()
            .returning(|| Box::pin(future::ready(Ok(vec![]))));
        let gh = MockGH::new();
        let votes_processor = Processor::new(Arc::new(db), Arc::new(gh));

        let (cmds_tx, cmds_rx) = async_channel::unbounded();
        let (stop_tx, _): (broadcast::Sender<()>, _) = broadcast::channel(1);
        let status_check_scheduler = votes_processor.status_checker(cmds_tx, stop_tx.subscribe());

        drop(stop_tx);
        assert!(status_check_scheduler.await.is_ok());
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
            .returning(move || Box::pin(future::ready(Ok(vec![check_vote_input_copy.clone()]))));
        let gh = MockGH::new();
        let votes_processor = Processor::new(Arc::new(db), Arc::new(gh));

        let (cmds_tx, cmds_rx) = async_channel::unbounded();
        let (stop_tx, _): (broadcast::Sender<()>, _) = broadcast::channel(1);
        let status_check_scheduler = votes_processor.status_checker(cmds_tx, stop_tx.subscribe());

        drop(stop_tx);
        assert!(status_check_scheduler.await.is_ok());
        assert_eq!(
            cmds_rx.recv().await.unwrap(),
            Command::CheckVote(check_vote_input)
        );
    }

    #[tokio::test]
    async fn status_checker_stops_when_requested_error_getting_pending() {
        let mut db = MockDB::new();
        db.expect_get_pending_status_checks()
            .returning(|| Box::pin(future::ready(Err(format_err!(ERROR)))));
        let gh = MockGH::new();
        let votes_processor = Processor::new(Arc::new(db), Arc::new(gh));

        let (cmds_tx, cmds_rx) = async_channel::unbounded();
        let (stop_tx, _): (broadcast::Sender<()>, _) = broadcast::channel(1);
        let status_check_scheduler = votes_processor.status_checker(cmds_tx, stop_tx.subscribe());

        drop(stop_tx);
        assert!(status_check_scheduler.await.is_ok());
        assert!(cmds_rx.is_empty());
    }

    #[tokio::test]
    async fn votes_auto_closer_stops_when_requested_none_pending() {
        let mut db = MockDB::new();
        db.expect_get_open_votes_with_close_on_passing()
            .returning(|| Box::pin(future::ready(Ok(vec![]))));
        let gh = MockGH::new();
        let votes_processor = Processor::new(Arc::new(db), Arc::new(gh));

        let (stop_tx, _): (broadcast::Sender<()>, _) = broadcast::channel(1);
        let votes_auto_closer = votes_processor.votes_auto_closer(stop_tx.subscribe());

        drop(stop_tx);
        assert!(votes_auto_closer.await.is_ok());
    }

    #[tokio::test]
    async fn votes_auto_closer_stops_after_processing_pending_vote() {
        let mut db = MockDB::new();
        db.expect_get_open_votes_with_close_on_passing()
            .returning(move || Box::pin(future::ready(Ok(vec![setup_test_vote()]))));
        db.expect_update_vote_ends_at()
            .returning(move |_| Box::pin(future::ready(Ok(()))));

        let mut gh = MockGH::new();
        gh.expect_get_comment_reactions()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(COMMENT_ID))
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
            .with(
                eq(INST_ID),
                eq(setup_test_vote().cfg),
                eq(ORG),
                eq(REPO),
                eq(Some(ORG.to_string())),
            )
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(vec![USER1.to_string()]))));

        let votes_processor = Processor::new(Arc::new(db), Arc::new(gh));

        let (stop_tx, _): (broadcast::Sender<()>, _) = broadcast::channel(1);
        let votes_auto_closer = votes_processor.votes_auto_closer(stop_tx.subscribe());

        drop(stop_tx);
        assert!(votes_auto_closer.await.is_ok());
    }

    #[tokio::test]
    async fn votes_auto_closer_stops_when_requested_error_getting_pending() {
        let mut db = MockDB::new();
        db.expect_get_open_votes_with_close_on_passing()
            .returning(|| Box::pin(future::ready(Err(format_err!(ERROR)))));
        let gh = MockGH::new();
        let votes_processor = Processor::new(Arc::new(db), Arc::new(gh));

        let (stop_tx, _): (broadcast::Sender<()>, _) = broadcast::channel(1);
        let votes_auto_closer = votes_processor.votes_auto_closer(stop_tx.subscribe());

        drop(stop_tx);
        assert!(votes_auto_closer.await.is_ok());
    }

    #[tokio::test]
    async fn create_vote_error_getting_configuration_profile() {
        let db = MockDB::new();
        let mut gh = MockGH::new();
        gh.expect_get_config_file()
            .with(eq(INST_ID), eq(ORG), eq(REPO))
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
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
        let event = setup_test_issue_event();

        let votes_processor = Processor::new(Arc::new(db), Arc::new(gh));
        votes_processor
            .create_vote(&CreateVoteInput::new(None, &Event::Issue(event)))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn create_vote_non_collaborator() {
        let db = MockDB::new();
        let mut gh = MockGH::new();
        gh.expect_get_config_file()
            .with(eq(INST_ID), eq(ORG), eq(REPO))
            .returning(|_, _, _| Box::pin(future::ready(Some(get_test_valid_config()))));
        gh.expect_user_is_collaborator()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
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
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
        let event = setup_test_issue_event();

        let votes_processor = Processor::new(Arc::new(db), Arc::new(gh));
        votes_processor
            .create_vote(&CreateVoteInput::new(None, &Event::Issue(event)))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn create_vote_issue_already_has_a_vote() {
        let mut db = MockDB::new();
        db.expect_has_vote_open()
            .with(eq(REPOFN), eq(ISSUE_NUM))
            .returning(|_, _| Box::pin(future::ready(Ok(true))));
        let mut gh = MockGH::new();
        gh.expect_get_config_file()
            .with(eq(INST_ID), eq(ORG), eq(REPO))
            .returning(|_, _, _| Box::pin(future::ready(Some(get_test_valid_config()))));
        gh.expect_user_is_collaborator()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
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
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
        let event = setup_test_issue_event();

        let votes_processor = Processor::new(Arc::new(db), Arc::new(gh));
        votes_processor
            .create_vote(&CreateVoteInput::new(None, &Event::Issue(event)))
            .await
            .unwrap();
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
            .returning(|_, _| Box::pin(future::ready(Ok(false))));
        let cfg_copy = cfg.clone();
        let create_vote_input_copy = create_vote_input.clone();
        db.expect_store_vote()
            .withf(move |vote_comment_id, input, cfg| {
                *vote_comment_id == COMMENT_ID
                    && *input == create_vote_input_copy
                    && *cfg == cfg_copy
            })
            .returning(|_, _, _| Box::pin(future::ready(Ok(Uuid::new_v4()))));
        let mut gh = MockGH::new();
        gh.expect_get_config_file()
            .with(eq(INST_ID), eq(ORG), eq(REPO))
            .returning(|_, _, _| Box::pin(future::ready(Some(get_test_valid_config()))));
        gh.expect_user_is_collaborator()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
            .returning(|_, _, _, _| Box::pin(future::ready(Ok(true))));
        let create_vote_input_copy = create_vote_input.clone();
        gh.expect_post_comment()
            .withf(move |inst_id, owner, repo, issue_number, body| {
                let expected_body = tmpl::VoteCreated::new(&create_vote_input_copy, &cfg)
                    .render()
                    .unwrap();
                *inst_id == INST_ID
                    && owner == ORG
                    && repo == REPO
                    && *issue_number == ISSUE_NUM
                    && body == expected_body.as_str()
            })
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));

        let votes_processor = Processor::new(Arc::new(db), Arc::new(gh));
        votes_processor
            .create_vote(&create_vote_input)
            .await
            .unwrap();
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
            .returning(|_, _| Box::pin(future::ready(Ok(false))));
        let cfg_copy = cfg.clone();
        let create_vote_input_copy = create_vote_input.clone();
        db.expect_store_vote()
            .withf(move |vote_comment_id, input, cfg| {
                *vote_comment_id == COMMENT_ID
                    && *input == create_vote_input_copy
                    && *cfg == cfg_copy
            })
            .returning(|_, _, _| Box::pin(future::ready(Ok(Uuid::new_v4()))));
        let mut gh = MockGH::new();
        gh.expect_get_config_file()
            .with(eq(INST_ID), eq(ORG), eq(REPO))
            .returning(|_, _, _| Box::pin(future::ready(Some(get_test_valid_config()))));
        gh.expect_user_is_collaborator()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
            .returning(|_, _, _, _| Box::pin(future::ready(Ok(true))));
        let create_vote_input_copy = create_vote_input.clone();
        gh.expect_post_comment()
            .withf(move |inst_id, owner, repo, issue_number, body| {
                let expected_body = tmpl::VoteCreated::new(&create_vote_input_copy, &cfg)
                    .render()
                    .unwrap();
                *inst_id == INST_ID
                    && owner == ORG
                    && repo == REPO
                    && *issue_number == ISSUE_NUM
                    && body == expected_body.as_str()
            })
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
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));

        let votes_processor = Processor::new(Arc::new(db), Arc::new(gh));
        votes_processor
            .create_vote(&create_vote_input)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn cancel_vote_error_checking_if_user_is_collaborator() {
        let db = MockDB::new();
        let mut gh = MockGH::new();
        gh.expect_user_is_collaborator()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
            .returning(|_, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
        let event = setup_test_issue_comment_event();

        let votes_processor = Processor::new(Arc::new(db), Arc::new(gh));
        votes_processor
            .cancel_vote(&CancelVoteInput::new(&Event::IssueComment(event)))
            .await
            .unwrap_err();
    }

    #[tokio::test]
    async fn cancel_vote_only_collaborators_can_close_votes() {
        let db = MockDB::new();
        let mut gh = MockGH::new();
        gh.expect_user_is_collaborator()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
            .returning(|_, _, _, _| Box::pin(future::ready(Ok(false))));
        let event = setup_test_issue_comment_event();

        let votes_processor = Processor::new(Arc::new(db), Arc::new(gh));
        votes_processor
            .cancel_vote(&CancelVoteInput::new(&Event::IssueComment(event)))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn cancel_vote_error_cancelling() {
        let mut db = MockDB::new();
        db.expect_cancel_vote()
            .with(eq(REPOFN), eq(ISSUE_NUM))
            .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
        let mut gh = MockGH::new();
        gh.expect_user_is_collaborator()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
            .returning(|_, _, _, _| Box::pin(future::ready(Ok(true))));
        let event = setup_test_issue_comment_event();

        let votes_processor = Processor::new(Arc::new(db), Arc::new(gh));
        votes_processor
            .cancel_vote(&CancelVoteInput::new(&Event::IssueComment(event)))
            .await
            .unwrap_err();
    }

    #[tokio::test]
    async fn cancel_vote_no_vote_in_progress() {
        let mut db = MockDB::new();
        db.expect_cancel_vote()
            .with(eq(REPOFN), eq(ISSUE_NUM))
            .returning(|_, _| Box::pin(future::ready(Ok(None))));
        let mut gh = MockGH::new();
        gh.expect_user_is_collaborator()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
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
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
        let event = setup_test_issue_comment_event();

        let votes_processor = Processor::new(Arc::new(db), Arc::new(gh));
        votes_processor
            .cancel_vote(&CancelVoteInput::new(&Event::IssueComment(event)))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn cancel_vote_success() {
        let mut db = MockDB::new();
        db.expect_cancel_vote()
            .with(eq(REPOFN), eq(ISSUE_NUM))
            .returning(|_, _| Box::pin(future::ready(Ok(Some(Uuid::parse_str(VOTE_ID).unwrap())))));
        let mut gh = MockGH::new();
        gh.expect_user_is_collaborator()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
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
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
        let event = setup_test_issue_comment_event();

        let votes_processor = Processor::new(Arc::new(db), Arc::new(gh));
        votes_processor
            .cancel_vote(&CancelVoteInput::new(&Event::IssueComment(event)))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn cancel_vote_in_pr_success() {
        let mut db = MockDB::new();
        db.expect_cancel_vote()
            .with(eq(REPOFN), eq(ISSUE_NUM))
            .returning(|_, _| Box::pin(future::ready(Ok(Some(Uuid::parse_str(VOTE_ID).unwrap())))));
        let mut gh = MockGH::new();
        gh.expect_user_is_collaborator()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(USER))
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
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
        let event = setup_test_pr_event();

        let votes_processor = Processor::new(Arc::new(db), Arc::new(gh));
        votes_processor
            .cancel_vote(&CancelVoteInput::new(&Event::PullRequest(event)))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn check_vote_error_getting_vote() {
        let mut db = MockDB::new();
        db.expect_get_open_vote()
            .with(eq(REPOFN), eq(ISSUE_NUM))
            .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
        let gh = MockGH::new();
        let event = setup_test_pr_event();

        let votes_processor = Processor::new(Arc::new(db), Arc::new(gh));
        votes_processor
            .check_vote(&CheckVoteInput::new(&Event::PullRequest(event)))
            .await
            .unwrap_err();
    }

    #[tokio::test]
    async fn check_vote_not_found() {
        let mut db = MockDB::new();
        db.expect_get_open_vote()
            .with(eq(REPOFN), eq(ISSUE_NUM))
            .returning(|_, _| Box::pin(future::ready(Ok(None))));
        let gh = MockGH::new();
        let event = setup_test_pr_event();

        let votes_processor = Processor::new(Arc::new(db), Arc::new(gh));
        votes_processor
            .check_vote(&CheckVoteInput::new(&Event::PullRequest(event)))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn check_vote_checked_recently() {
        let mut db = MockDB::new();
        db.expect_get_open_vote()
            .with(eq(REPOFN), eq(ISSUE_NUM))
            .returning(|_, _| {
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
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
        let event = setup_test_pr_event();

        let votes_processor = Processor::new(Arc::new(db), Arc::new(gh));
        votes_processor
            .check_vote(&CheckVoteInput::new(&Event::PullRequest(event)))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn check_vote_success() {
        let mut db = MockDB::new();
        db.expect_get_open_vote()
            .with(eq(REPOFN), eq(ISSUE_NUM))
            .returning(|_, _| Box::pin(future::ready(Ok(Some(setup_test_vote())))));
        db.expect_update_vote_last_check()
            .with(eq(Uuid::parse_str(VOTE_ID).unwrap()))
            .returning(|_| Box::pin(future::ready(Ok(()))));
        let mut gh = MockGH::new();
        gh.expect_get_comment_reactions()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(COMMENT_ID))
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
            .with(
                eq(INST_ID),
                eq(setup_test_vote().cfg),
                eq(ORG),
                eq(REPO),
                eq(Some(ORG.to_string())),
            )
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
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
        let event = setup_test_pr_event();

        let votes_processor = Processor::new(Arc::new(db), Arc::new(gh));
        votes_processor
            .check_vote(&CheckVoteInput::new(&Event::PullRequest(event)))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn close_finished_vote_error_closing() {
        let mut db = MockDB::new();
        db.expect_close_finished_vote()
            .returning(|_| Box::pin(future::ready(Err(format_err!(ERROR)))));
        let gh = MockGH::new();

        let votes_processor = Processor::new(Arc::new(db), Arc::new(gh));
        votes_processor.close_finished_vote().await.unwrap_err();
    }

    #[tokio::test]
    async fn close_finished_vote_none_closed() {
        let mut db = MockDB::new();
        db.expect_close_finished_vote()
            .returning(|_| Box::pin(future::ready(Ok(None))));
        let gh = MockGH::new();

        let votes_processor = Processor::new(Arc::new(db), Arc::new(gh));
        votes_processor.close_finished_vote().await.unwrap();
    }

    #[tokio::test]
    async fn close_finished_vote_on_issue() {
        let results = setup_test_vote_results();
        let results_copy = results.clone();

        let mut db = MockDB::new();
        db.expect_close_finished_vote().returning(move |_| {
            Box::pin(future::ready(Ok(Some((
                setup_test_vote(),
                Some(results_copy.clone()),
            )))))
        });
        let mut gh = MockGH::new();
        gh.expect_post_comment()
            .withf(move |inst_id, owner, repo, issue_number, body| {
                let expected_body = tmpl::VoteClosed::new(&results).render().unwrap();
                *inst_id == INST_ID
                    && owner == ORG
                    && repo == REPO
                    && *issue_number == ISSUE_NUM
                    && body == expected_body.as_str()
            })
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));

        let votes_processor = Processor::new(Arc::new(db), Arc::new(gh));
        votes_processor.close_finished_vote().await.unwrap();
    }

    #[tokio::test]
    async fn close_finished_vote_on_pr_closed_vote_passed() {
        let results = setup_test_vote_results();
        let results_copy = results.clone();

        let mut db = MockDB::new();
        db.expect_close_finished_vote().returning(move |_| {
            let mut vote = setup_test_vote();
            vote.is_pull_request = true;
            Box::pin(future::ready(Ok(Some((vote, Some(results_copy.clone()))))))
        });
        let mut gh = MockGH::new();
        gh.expect_post_comment()
            .withf(move |inst_id, owner, repo, issue_number, body| {
                let expected_body = tmpl::VoteClosed::new(&results).render().unwrap();
                *inst_id == INST_ID
                    && owner == ORG
                    && repo == REPO
                    && *issue_number == ISSUE_NUM
                    && body == expected_body.as_str()
            })
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
                    summary: "The vote passed! 1 out of 1 voted in favor.".to_string(),
                }),
            )
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));

        let votes_processor = Processor::new(Arc::new(db), Arc::new(gh));
        votes_processor.close_finished_vote().await.unwrap();
    }

    #[tokio::test]
    async fn close_finished_vote_on_pr_closed_vote_not_passed() {
        let mut results = setup_test_vote_results();
        results.passed = false;
        results.in_favor_percentage = 0.0;
        results.in_favor = 0;
        results.against = 1;
        let results_copy = results.clone();

        let mut db = MockDB::new();
        db.expect_close_finished_vote().returning(move |_| {
            let mut vote = setup_test_vote();
            vote.is_pull_request = true;
            Box::pin(future::ready(Ok(Some((vote, Some(results_copy.clone()))))))
        });
        let mut gh = MockGH::new();
        gh.expect_post_comment()
            .withf(move |inst_id, owner, repo, issue_number, body| {
                let expected_body = tmpl::VoteClosed::new(&results).render().unwrap();
                *inst_id == INST_ID
                    && owner == ORG
                    && repo == REPO
                    && *issue_number == ISSUE_NUM
                    && body == expected_body.as_str()
            })
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(COMMENT_ID))));
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
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));

        let votes_processor = Processor::new(Arc::new(db), Arc::new(gh));
        votes_processor.close_finished_vote().await.unwrap();
    }
}
