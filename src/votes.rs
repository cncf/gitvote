use crate::{
    events::{IssueCommentEvent, IssueCommentEventAction},
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
use std::{fs, sync::Arc};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_postgres::types::Json;
use tracing::error;

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

/// A votes processor is in charge of creating the votes requested, stopping
/// them at the scheduled time and publishing results, etc.
pub(crate) struct Processor {
    db: DbPool,
    app_github_client: Octocrab,
}

impl Processor {
    /// Create a new votes processor instance and start it.
    pub(crate) fn start(
        cfg: Arc<Config>,
        db: DbPool,
        cmds_rx: mpsc::Receiver<Command>,
    ) -> Result<JoinAll<JoinHandle<()>>> {
        // Setup application GitHub client
        let app_id = cfg.get_int("github.appID")? as u64;
        let app_private_key_path = cfg.get_string("github.appPrivateKey")?;
        let app_private_key = fs::read(app_private_key_path)?;
        let app_private_key = jsonwebtoken::EncodingKey::from_rsa_pem(&app_private_key[..])?;
        let app_github_client = Octocrab::builder()
            .app(app_id.into(), app_private_key)
            .build()?;

        // Setup processor and launch specialized workers
        let processor = Self {
            db,
            app_github_client,
        };
        let commands_handler = processor.commands_handler(cmds_rx);

        Ok(future::join_all(vec![commands_handler]))
    }

    /// Receive commands from the queue and executes them. Commands are added
    /// to the queue when certain events are received on the webhook endpoint.
    fn commands_handler(self, mut cmds_rx: mpsc::Receiver<Command>) -> JoinHandle<()> {
        // Spawn a task to dispatch handle commands
        tokio::spawn(async move {
            while let Some(cmd) = cmds_rx.recv().await {
                if let Err(err) = match cmd {
                    Command::CreateVote { event } => {
                        self.create_vote(event).await.context("error creating vote")
                    }
                } {
                    error!("{:#?}", err);
                }
            }
        })
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

        // Store vote information in database
        //
        // Metadata will be stored as well as we want the vote to be processed
        // with the configuration available at the moment the vote was created
        let db = self.db.get().await?;
        db.execute(
            "
            insert into vote (event, metadata, ends_at)
            values ($1::jsonb, $2::jsonb, current_timestamp + ($3::bigint || ' seconds')::interval)
            ",
            &[&Json(&event), &Json(&md), &(md.duration.as_secs() as i64)],
        )
        .await?;

        // Post vote created comment on the issue/pr
        installation_github_client
            .issues(owner, repo)
            .create_comment(
                event.issue.number,
                templates::VoteCreated::new(&event, &md).render()?,
            )
            .await?;

        Ok(())
    }
}

/// Helper function that splits a repository's full name and returns the owner
/// and the repo name as a tuple.
fn split_full_name(full_name: &str) -> (&str, &str) {
    let mut parts = full_name.split('/');
    (parts.next().unwrap(), parts.next().unwrap())
}
