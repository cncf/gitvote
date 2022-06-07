use crate::{
    events::{IssueCommentEvent, IssueCommentEventAction},
    metadata::Metadata,
    templates,
};
use anyhow::{Context, Result};
use askama::Template;
use config::Config;
use octocrab::{models::InstallationId, Octocrab};
use serde::{Deserialize, Serialize};
use std::{fs, sync::Arc};
use tokio::{sync::mpsc::Receiver, task::JoinHandle};
use tracing::error;

/// Errors that may occur while creating a new command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

/// A commands dispatcher receives commands from the queue and executes them.
pub(crate) fn commands_dispatcher(
    mut cmds_rx: Receiver<Command>,
    manager: Arc<Manager>,
) -> JoinHandle<()> {
    // Spawn a task to dispatch incoming commands
    // TODO(sergio): add multiple workers
    tokio::spawn(async move {
        while let Some(cmd) = cmds_rx.recv().await {
            if let Err(err) = match cmd {
                Command::CreateVote { event } => manager
                    .create_vote(event)
                    .await
                    .context("error creating vote"),
            } {
                error!("{:#?}", err);
            }
        }
    })
}

/// A manager is in charge of managing votes: create them, stop at scheduled
/// time, publish results, etc.
#[derive(Debug)]
pub(crate) struct Manager {
    app_github_client: Octocrab,
}

impl Manager {
    /// Create a new manager instance.
    pub(crate) fn new(cfg: Arc<Config>) -> Result<Self> {
        // Setup application Github client
        let app_id = cfg.get_int("github.appID")? as u64;
        let app_private_key_path = cfg.get_string("github.appPrivateKey")?;
        let app_private_key = fs::read(app_private_key_path)?;
        let app_private_key = jsonwebtoken::EncodingKey::from_rsa_pem(&app_private_key[..])?;
        let app_github_client = Octocrab::builder()
            .app(app_id.into(), app_private_key)
            .build()?;

        Ok(Self { app_github_client })
    }

    /// Create a new vote.
    async fn create_vote(&self, event: IssueCommentEvent) -> Result<()> {
        // Setup installation GitHub client
        let installation_id = InstallationId(event.installation.id);
        let installation_github_client = self.app_github_client.installation(installation_id);

        // Get metadata from repository
        let mut parts = event.repository.full_name.split('/');
        let (owner, repo) = (parts.next().unwrap(), parts.next().unwrap());
        let md = match Metadata::from_remote(&installation_github_client, owner, repo)
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
        // TODO

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
