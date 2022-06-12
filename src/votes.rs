use crate::{
    conf::RepoConfig,
    github::{IssueCommentEvent, IssueCommentEventAction, Reaction},
    templates,
};
use anyhow::{Context, Result};
use askama::Template;
use config::Config;
use deadpool_postgres::Pool as DbPool;
use futures::future::{self, JoinAll};
use lazy_static::lazy_static;
use octocrab::{models::InstallationId, Octocrab, Page};
use regex::Regex;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{
    sync::{
        broadcast::{self, error::TryRecvError},
        mpsc,
    },
    task::JoinHandle,
    time::sleep,
};
use tokio_postgres::types::Json;
use tracing::{debug, error};
use uuid::Uuid;

/// Vote options.
const IN_FAVOR: &str = "In favor";
const AGAINST: &str = "Against";
const ABSTAIN: &str = "Abstain";
const NOT_VOTED: &str = "Not voted";

/// Vote options reactions.
const IN_FAVOR_REACTION: &str = "+1";
const AGAINST_REACTION: &str = "-1";
const ABSTAIN_REACTION: &str = "eyes";

/// Amount of time the votes closer will sleep when there are no pending votes
/// to close.
const VOTES_CLOSER_PAUSE_ON_NONE: Duration = Duration::from_secs(15);

/// Amount of time the votes closer will sleep when something goes wrong.
const VOTES_CLOSER_PAUSE_ON_ERROR: Duration = Duration::from_secs(30);

lazy_static! {
    /// Regex used to detect commands in issues/prs comments.
    pub(crate) static ref CMD: Regex = Regex::new(r#"(?m)^/(?P<cmd>vote)\s*$"#).expect("invalid CMD regexp");
}

/// Vote results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Results {
    pub passed: bool,
    pub in_favor_percentage: f64,
    pub pass_threshold: f64,
    pub in_favor: u64,
    pub against: u64,
    pub abstain: u64,
    pub not_voted: u64,
    pub voters: HashMap<String, String>,
}

/// Represents a command to be executed, usually created from a GitHub event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum Command {
    CreateVote { event: IssueCommentEvent },
}

impl Command {
    /// Try to create a new command from an issue comment event.
    pub(crate) fn from_event(event: IssueCommentEvent) -> Option<Self> {
        // Only events with action created are supported at the moment
        if event.action != IssueCommentEventAction::Created {
            return None;
        }

        // Extract command from comment body
        if let Some(content) = &event.comment.body {
            if let Some(captures) = CMD.captures(content) {
                let cmd = captures.get(1).unwrap().as_str();
                match cmd {
                    "vote" => return Some(Command::CreateVote { event }),
                    _ => return None,
                }
            }
        }
        None
    }
}

/// A votes processor is in charge of creating the requested votes, stopping
/// them at the scheduled time and publishing results, etc.
pub(crate) struct Processor {
    db: DbPool,
    app_github_client: Octocrab,
}

impl Processor {
    /// Create a new votes processor instance.
    pub(crate) fn new(cfg: Arc<Config>, db: DbPool) -> Result<Arc<Self>> {
        // Setup application GitHub client
        let app_id = cfg.get_int("github.appID")? as u64;
        let app_private_key = cfg.get_string("github.appPrivateKey")?;
        let app_private_key = jsonwebtoken::EncodingKey::from_rsa_pem(app_private_key.as_bytes())?;
        let app_github_client = Octocrab::builder()
            .app(app_id.into(), app_private_key)
            .build()?;

        // Setup votes processor and return it
        let processor = Arc::new(Self {
            db,
            app_github_client,
        });
        Ok(processor)
    }

    /// Start votes processor.
    pub(crate) fn start(
        self: Arc<Self>,
        cmds_rx: mpsc::Receiver<Command>,
        stop_tx: broadcast::Sender<()>,
    ) -> JoinAll<JoinHandle<()>> {
        // Launch commands handler
        let commands_handler = self.clone().commands_handler(cmds_rx, stop_tx.subscribe());

        // Launch votes closer
        let votes_closer = self.votes_closer(stop_tx.subscribe());

        future::join_all(vec![commands_handler, votes_closer])
    }

    /// Worker that receives commands from the queue and executes them.
    /// Commands are added to the queue when certain events from GitHub are
    /// received on the webhook endpoint.
    fn commands_handler(
        self: Arc<Self>,
        mut cmds_rx: mpsc::Receiver<Command>,
        mut stop_rx: broadcast::Receiver<()>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            debug!("[commands handler] started");
            loop {
                tokio::select! {
                    // Pick next command from the queue and process it
                    Some(cmd) = cmds_rx.recv() => {
                        if let Err(err) = match cmd {
                            Command::CreateVote { event } => {
                                self.create_vote(event).await.context("error creating vote")
                            }
                        } {
                            error!("{:#?}", err);
                        }

                        // Exit if the votes processor has been asked to stop
                        if let Some(TryRecvError::Closed) = stop_rx.try_recv().err() {
                            break;
                        }
                    }

                    // Exit if the votes processor has been asked to stop
                    _ = stop_rx.recv() => {
                        break
                    }
                }
            }
            debug!("[commands handler] stopped");
        })
    }

    /// Create a new vote.
    async fn create_vote(&self, event: IssueCommentEvent) -> Result<()> {
        // Setup installation GitHub client
        let installation_id = InstallationId(event.installation.id);
        let installation_github_client = self.app_github_client.installation(installation_id);

        // Only repository collaborators can create votes
        let (owner, repo) = split_full_name(&event.repository.full_name);
        let url = format!(
            "https://api.github.com/repos/{}/{}/collaborators/{}",
            owner, repo, event.comment.user.login,
        );
        let resp = installation_github_client._get(url, None::<&()>).await?;
        if resp.status() != StatusCode::NO_CONTENT {
            installation_github_client
                .issues(owner, repo)
                .create_comment(
                    event.issue.number,
                    templates::VoteRestricted::new(&event.comment.user.login).render()?,
                )
                .await?;

            return Ok(());
        }

        // Only allow one vote open at the same time per issue/pr
        let db = self.db.get().await?;
        let row = db
            .query_one(
                "
                select exists (
                    select 1 from vote
                    where repository_full_name = $1::text
                    and issue_number = $2::bigint
                    and closed = false
                )
                ",
                &[&event.repository.full_name, &(event.issue.number as i64)],
            )
            .await?;
        let vote_in_progress: bool = row.get(0);
        if vote_in_progress {
            installation_github_client
                .issues(owner, repo)
                .create_comment(
                    event.issue.number,
                    templates::VoteInProgress::new(&event.comment.user.login).render()?,
                )
                .await?;

            return Ok(());
        }

        // Get repository configuration
        let cfg = match RepoConfig::new(&installation_github_client, owner, repo)
            .await
            .context("error getting repository configuration")?
        {
            Some(md) => md,
            None => return Ok(()),
        };

        // Post vote created comment on the issue/pr
        let vote_comment = installation_github_client
            .issues(owner, repo)
            .create_comment(
                event.issue.number,
                templates::VoteCreated::new(&event, &cfg).render()?,
            )
            .await?;

        // Store vote information in database
        let row = db
            .query_one(
                "
            insert into vote (
                vote_comment_id,
                ends_at,
                config,
                created_by,
                installation_id,
                issue_id,
                issue_number,
                repository_full_name

            ) values (
                $1::bigint,
                current_timestamp + ($2::bigint || ' seconds')::interval,
                $3::jsonb,
                $4::text,
                $5::bigint,
                $6::bigint,
                $7::bigint,
                $8::text
            )
            returning vote_id
            ",
                &[
                    &(vote_comment.id.0 as i64),
                    &(cfg.duration.as_secs() as i64),
                    &Json(&cfg),
                    &event.comment.user.login,
                    &(event.installation.id as i64),
                    &(event.issue.id as i64),
                    &(event.issue.number as i64),
                    &event.repository.full_name,
                ],
            )
            .await?;
        let vote_id: Uuid = row.get("vote_id");

        debug!("vote {} created", &vote_id);
        Ok(())
    }

    /// Worker that periodically checks the database for votes that should be
    /// closed and closes them.
    fn votes_closer(self: Arc<Self>, mut stop_rx: broadcast::Receiver<()>) -> JoinHandle<()> {
        tokio::spawn(async move {
            debug!("[votes closer] started");
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
                        _ = sleep(VOTES_CLOSER_PAUSE_ON_NONE) => {},
                        _ = stop_rx.recv() => break,
                    },
                    Err(err) => {
                        error!("error closing finished vote: {:#?}", err);
                        tokio::select! {
                            _ = sleep(VOTES_CLOSER_PAUSE_ON_ERROR) => {},
                            _ = stop_rx.recv() => break,
                        }
                    }
                }

                // Exit if the votes processor has been asked to stop
                if let Some(TryRecvError::Closed) = stop_rx.try_recv().err() {
                    break;
                }
            }
            debug!("[votes closer] stopped");
        })
    }

    /// Close any pending finished vote.
    async fn close_finished_vote(&self) -> Result<Option<()>> {
        // Get pending finished vote (if any) from database
        let mut db = self.db.get().await?;
        let tx = db.transaction().await?;
        let row = match tx
            .query_opt(
                "
                select
                    vote_id,
                    vote_comment_id,
                    config,
                    installation_id,
                    issue_number,
                    repository_full_name
                from vote
                where current_timestamp > ends_at and closed = false
                for update of vote skip locked
                limit 1
                ",
                &[],
            )
            .await?
        {
            // Pending finished vote to close found, proceed and close it
            Some(row) => row,

            // No finished votes to close, return
            None => return Ok(None),
        };
        let vote_id: Uuid = row.get("vote_id");
        let vote_comment_id: i64 = row.get("vote_comment_id");
        let Json(cfg): Json<RepoConfig> = row.get("config");
        let installation_id: i64 = row.get("installation_id");
        let issue_number: i64 = row.get("issue_number");
        let repository_full_name: String = row.get("repository_full_name");

        // Calculate results
        let installation_id = InstallationId(installation_id as u64);
        let installation_github_client = self.app_github_client.installation(installation_id);
        let (owner, repo) = split_full_name(&repository_full_name);
        let results = self
            .calculate_vote_results(
                &installation_github_client,
                &cfg,
                owner,
                repo,
                vote_comment_id,
            )
            .await?;

        // Store results in database
        tx.execute(
            "
            update vote set
                closed = true,
                closed_at = current_timestamp,
                results = $1::jsonb
            where vote_id = $2::uuid;
            ",
            &[&Json(&results), &vote_id],
        )
        .await?;
        tx.commit().await?;

        // Post vote closed comment on the issue/pr
        installation_github_client
            .issues(owner, repo)
            .create_comment(
                issue_number as u64,
                templates::VoteClosed::new(&results).render()?,
            )
            .await?;

        debug!("vote {} closed", &vote_id);
        Ok(Some(()))
    }

    /// Calculate the results of the vote created at the comment provided.
    async fn calculate_vote_results(
        &self,
        installation_github_client: &Octocrab,
        cfg: &RepoConfig,
        owner: &str,
        repo: &str,
        vote_comment_id: i64,
    ) -> Result<Results> {
        // Get vote comment reactions (aka votes)
        let url = format!(
            "https://api.github.com/repos/{}/{}/issues/comments/{}/reactions",
            owner, repo, vote_comment_id
        );
        let first_page: Page<Reaction> = installation_github_client.get(url, None::<&()>).await?;
        let reactions = installation_github_client.all_pages(first_page).await?;

        // Count votes
        let (mut in_favor, mut against, mut abstain) = (0, 0, 0);
        let mut voters: HashMap<String, String> = HashMap::with_capacity(cfg.voters.len());
        let mut multiple_options_voters: Vec<String> = Vec::new();
        for reaction in reactions {
            let user = reaction.user.login;

            // We only count the votes of users with a binding vote
            if !cfg.voters.contains(&user) {
                continue;
            }

            // Do not count votes of users voting for multiple options
            if multiple_options_voters.contains(&user) {
                continue;
            }
            if voters.contains_key(&user) {
                // User has already voted (multiple options voter)
                multiple_options_voters.push(user.clone());
                voters.remove(&user);
            }

            // Track binding votes
            match reaction.content.as_str() {
                IN_FAVOR_REACTION => {
                    in_favor += 1;
                    voters.insert(user.clone(), IN_FAVOR.to_string());
                }
                AGAINST_REACTION => {
                    against += 1;
                    voters.insert(user.clone(), AGAINST.to_string());
                }
                ABSTAIN_REACTION => {
                    abstain += 1;
                    voters.insert(user.clone(), ABSTAIN.to_string());
                }
                _ => {
                    // Ignore other reactions
                }
            }
        }

        // Add users with a binding vote who did not vote to the list of voters
        let mut not_voted = 0;
        for user in &cfg.voters {
            if !voters.contains_key(user) {
                not_voted += 1;
                voters.insert(user.clone(), NOT_VOTED.to_string());
            }
        }

        // Prepare results and return them
        let in_favor_percentage = in_favor as f64 / cfg.voters.len() as f64 * 100.0;
        let passed = in_favor_percentage >= cfg.pass_threshold;

        Ok(Results {
            passed,
            in_favor_percentage,
            pass_threshold: cfg.pass_threshold,
            in_favor,
            against,
            abstain,
            not_voted,
            voters,
        })
    }
}

/// Helper function that splits a repository's full name and returns the owner
/// and the repo name as a tuple.
fn split_full_name(full_name: &str) -> (&str, &str) {
    let mut parts = full_name.split('/');
    (parts.next().unwrap(), parts.next().unwrap())
}
