use crate::{
    events::{IssueCommentEvent, IssueCommentEventAction, Reaction},
    metadata::Metadata,
    templates,
};
use anyhow::{Context, Result};
use askama::Template;
use config::Config;
use deadpool_postgres::Pool as DbPool;
use futures::future::{self, JoinAll};
use octocrab::{models::InstallationId, Octocrab};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, sync::Arc};
use tokio::{
    sync::{broadcast, mpsc},
    task::JoinHandle,
    time::{self, Duration},
};
use tokio_postgres::types::Json;
use tracing::error;
use uuid::Uuid;

/// How often do we check the database for votes that should be closed (in
/// seconds).
const VOTES_CLOSER_FREQUENCY: u64 = 30;

/// Vote options.
const IN_FAVOR: &str = "In favor";
const AGAINST: &str = "Against";
const ABSTAIN: &str = "Abstain";
const NOT_VOTED: &str = "Not voted";

/// Errors that may occur while creating a new command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum CommandError {
    CommandNotFound,
    UnsupportedEventAction,
}

/// Represents a command to be executed, usually created from a GitHub event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum Command {
    CreateVote { event: IssueCommentEvent },
}

impl TryFrom<IssueCommentEvent> for Command {
    type Error = CommandError;

    /// Try to create a new command from an issue comment event.
    fn try_from(event: IssueCommentEvent) -> Result<Self, Self::Error> {
        if event.action != IssueCommentEventAction::Created {
            // We only react when a comment with a command is created for now
            return Err(CommandError::UnsupportedEventAction);
        }
        match &event.comment.body {
            Some(content) => match content.as_str() {
                "/vote" => Ok(Command::CreateVote { event }),
                _ => Err(CommandError::CommandNotFound),
            },
            None => Err(CommandError::CommandNotFound),
        }
    }
}

/// Represents the results of a vote.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct VoteResults {
    pub passed: bool,
    pub in_favor_percentage: f64,
    pub pass_threshold: f64,
    pub in_favor: u64,
    pub against: u64,
    pub abstain: u64,
    pub not_voted: u64,
    pub voters: HashMap<String, String>,
}

/// A votes processor is in charge of creating the votes requested, stopping
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
        let app_private_key_path = cfg.get_string("github.appPrivateKey")?;
        let app_private_key = fs::read(app_private_key_path)?;
        let app_private_key = jsonwebtoken::EncodingKey::from_rsa_pem(&app_private_key[..])?;
        let app_github_client = Octocrab::builder()
            .app(app_id.into(), app_private_key)
            .build()?;

        // Setup votes processor and return it.
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

    /// Receive commands from the queue and executes them. Commands are added
    /// to the queue when certain events from GitHub are received on the
    /// webhook endpoint.
    fn commands_handler(
        self: Arc<Self>,
        mut cmds_rx: mpsc::Receiver<Command>,
        mut stop_rx: broadcast::Receiver<()>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
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
                    }

                    // Exit if the votes processor has been asked to stop
                    _ = stop_rx.recv() => {
                        break
                    }
                }
            }
        })
    }

    /// Close votes that have finished periodically.
    fn votes_closer(self: Arc<Self>, mut stop_rx: broadcast::Receiver<()>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_secs(VOTES_CLOSER_FREQUENCY));
            loop {
                tokio::select! {
                    // Call close_finished_votes periodically.
                    _ = interval.tick() => {
                        if let Err(err) = self.close_finished_votes().await {
                            error!("error closing finished votes: {:#?}", err);
                        }
                    },

                    // Exit if the votes processor has been asked to stop
                    _ = stop_rx.recv() => {
                        break
                    }
                }
            }
        })
    }

    /// Close votes that have finished.
    async fn close_finished_votes(&self) -> Result<()> {
        // Get finished votes not yet closed from database
        let mut votes: Vec<Uuid> = Vec::new();
        let db = self.db.get().await?;
        let rows = db
            .query(
                "
                select vote_id
                from vote
                where current_timestamp > ends_at
                and closed is false
                ",
                &[],
            )
            .await?;
        for row in rows {
            votes.push(row.get("vote_id"));
        }

        // Close them
        for vote_id in votes {
            if let Err(err) = self.close_vote(vote_id).await {
                error!("error closing vote {}: {}", vote_id, err);
            }
        }

        Ok(())
    }

    /// Create a new vote.
    async fn create_vote(&self, event: IssueCommentEvent) -> Result<()> {
        // Setup installation GitHub client
        let installation_id = InstallationId(event.installation.id);
        let installation_github_client = self.app_github_client.installation(installation_id);

        // Get metadata from repository
        let (owner, repo) = split_full_name(&event.repository.full_name);
        let md = match Metadata::from_repo(&installation_github_client, owner, repo)
            .await
            .context("error getting metadata")?
        {
            Some(md) => md,
            None => return Ok(()),
        };

        // Post vote created comment on the issue/pr
        let vote_comment = installation_github_client
            .issues(owner, repo)
            .create_comment(
                event.issue.number,
                templates::VoteCreated::new(&event, &md).render()?,
            )
            .await?;

        // Store vote information in database
        //
        // Metadata will be stored as well as we want the vote to be processed
        // with the configuration available at the moment the vote was created
        let db = self.db.get().await?;
        db.execute(
            "
            insert into vote (
                vote_comment_id,
                event,
                metadata,
                ends_at
            ) values (
                $1::bigint,
                $2::jsonb,
                $3::jsonb,
                current_timestamp + ($4::bigint || ' seconds')::interval
            )
            ",
            &[
                &(vote_comment.id.0 as i64),
                &Json(&event),
                &Json(&md),
                &(md.duration.as_secs() as i64),
            ],
        )
        .await?;

        Ok(())
    }

    /// Close the vote provided.
    async fn close_vote(&self, vote_id: Uuid) -> Result<()> {
        // Setup transaction
        let mut db = self.db.get().await?;
        let tx = db.transaction().await?;

        // Get vote information from database
        let row = tx
            .query_one(
                "
                select vote_comment_id, event, metadata
                from vote
                where vote_id = $1::uuid
                for update
                ",
                &[&vote_id],
            )
            .await?;
        let vote_comment_id: i64 = row.get("vote_comment_id");
        let Json(event): Json<IssueCommentEvent> = row.get("event");
        let Json(md): Json<Metadata> = row.get("metadata");

        // Calculate results
        let installation_id = InstallationId(event.installation.id);
        let installation_github_client = self.app_github_client.installation(installation_id);
        let (owner, repo) = split_full_name(&event.repository.full_name);
        let results = self
            .calculate_vote_results(
                &installation_github_client,
                &md,
                owner,
                repo,
                vote_comment_id,
            )
            .await?;

        // Store results in database and commit transaction
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
                event.issue.number,
                templates::VoteClosed::new(&results).render()?,
            )
            .await?;

        Ok(())
    }

    /// Calculate the results of the vote created at the comment provided.
    async fn calculate_vote_results(
        &self,
        installation_github_client: &Octocrab,
        md: &Metadata,
        owner: &str,
        repo: &str,
        vote_comment_id: i64,
    ) -> Result<VoteResults> {
        // Get vote comment reactions (aka votes)
        let url = format!(
            "https://api.github.com/repos/{}/{}/issues/comments/{}/reactions",
            owner, repo, vote_comment_id
        );
        let reactions: Vec<Reaction> = installation_github_client.get(url, None::<&()>).await?;

        // Count votes
        let (mut in_favor, mut against, mut abstain) = (0, 0, 0);
        let mut voters: HashMap<String, String> = HashMap::with_capacity(md.voters.len());
        let mut multiple_options_voters: Vec<String> = Vec::new();
        for reaction in reactions {
            let user = reaction.user.login;

            // We only count the votes of user with a binding vote
            if !md.voters.contains(&user) {
                continue;
            }

            // Do not count votes of users voting for multiple options
            if multiple_options_voters.contains(&user) {
                continue;
            }
            if voters.contains_key(&user) {
                multiple_options_voters.push(user.clone());
                voters.remove(&user);
            }

            // Track binding votes
            match reaction.content.as_str() {
                "+1" => {
                    in_favor += 1;
                    voters.insert(user.clone(), IN_FAVOR.to_string());
                }
                "-1" => {
                    against += 1;
                    voters.insert(user.clone(), AGAINST.to_string());
                }
                "eyes" => {
                    abstain += 1;
                    voters.insert(user.clone(), ABSTAIN.to_string());
                }
                _ => {}
            }
        }

        // Add users with binding vote who did not vote to the list of voters
        let mut not_voted = 0;
        for user in &md.voters {
            if !voters.contains_key(user) {
                not_voted += 1;
                voters.insert(user.clone(), NOT_VOTED.to_string());
            }
        }

        // Prepare results and return them
        let in_favor_percentage = in_favor as f64 / md.voters.len() as f64;
        let passed = in_favor_percentage >= md.pass_threshold;

        Ok(VoteResults {
            passed,
            in_favor_percentage,
            pass_threshold: md.pass_threshold,
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
