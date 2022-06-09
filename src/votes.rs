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

lazy_static! {
    /// Regex used to detect commands in issues/prs comments.
    pub(crate) static ref CMD: Regex = Regex::new(r#"(?m)^/(?P<cmd>vote)\s*$"#).expect("invalid CMD regexp");
}

/// How often do we check the database for votes that should be closed.
const VOTES_CLOSER_FREQUENCY: Duration = Duration::from_secs(30);

/// Vote options.
const IN_FAVOR: &str = "In favor";
const AGAINST: &str = "Against";
const ABSTAIN: &str = "Abstain";
const NOT_VOTED: &str = "Not voted";

/// Vote options reactions.
const IN_FAVOR_REACTION: &str = "+1";
const AGAINST_REACTION: &str = "-1";
const ABSTAIN_REACTION: &str = "eyes";

/// Represents the results of a vote.
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
        let app_private_key_path = cfg.get_string("github.appPrivateKey")?;
        let app_private_key = fs::read(app_private_key_path)?;
        let app_private_key = jsonwebtoken::EncodingKey::from_rsa_pem(&app_private_key[..])?;
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

    /// Worker that periodically checks the database for votes that should be
    /// closed and closes them.
    fn votes_closer(self: Arc<Self>, mut stop_rx: broadcast::Receiver<()>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = time::interval(VOTES_CLOSER_FREQUENCY);
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
            let vote_id: Uuid = row.get("vote_id");
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

        // Get repository configuration
        let (owner, repo) = split_full_name(&event.repository.full_name);
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
        let db = self.db.get().await?;
        db.execute(
            "
            insert into vote (
                vote_comment_id,
                event,
                config,
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
                &Json(&cfg),
                &(cfg.duration.as_secs() as i64),
            ],
        )
        .await?;

        Ok(())
    }

    /// Close the vote provided.
    async fn close_vote(&self, vote_id: Uuid) -> Result<()> {
        // Get vote information from database
        let mut db = self.db.get().await?;
        let tx = db.transaction().await?;
        let row = tx
            .query_one(
                "
                select vote_comment_id, event, config
                from vote
                where vote_id = $1::uuid
                for update
                ",
                &[&vote_id],
            )
            .await?;
        let vote_comment_id: i64 = row.get("vote_comment_id");
        let Json(event): Json<IssueCommentEvent> = row.get("event");
        let Json(cfg): Json<RepoConfig> = row.get("config");

        // Calculate results
        let installation_id = InstallationId(event.installation.id);
        let installation_github_client = self.app_github_client.installation(installation_id);
        let (owner, repo) = split_full_name(&event.repository.full_name);
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
