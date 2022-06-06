use crate::{events::IssueCommentEvent, Args};
use anyhow::Result;
use octocrab::Octocrab;
use serde::{Deserialize, Serialize};
use std::fs;
use tokio::{sync::mpsc::Receiver, task::JoinHandle};
use tracing::debug;

/// Errors that may occur while creating a new command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum CommandError {
    CommandNotFound,
}

/// Represents a command to be executed, usually created from a GitHub event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum Command {
    CreateVote {},
}

impl TryFrom<IssueCommentEvent> for Command {
    type Error = CommandError;

    fn try_from(event: IssueCommentEvent) -> Result<Self, Self::Error> {
        match event.comment.body {
            Some(content) => match content.as_str() {
                "/vote" => Ok(Command::CreateVote {}),
                _ => Err(CommandError::CommandNotFound),
            },
            None => Err(CommandError::CommandNotFound),
        }
    }
}

/// A manager is in charge of managing votes. It creates them when requested
/// via commands, closes them at the scheduled time and publishes results.
#[derive(Debug)]
pub(crate) struct Manager {
    app_github_client: Octocrab,
    commands_dispatcher: JoinHandle<()>,
}

impl Manager {
    /// Create a new manager instance.
    pub(crate) fn new(args: Args, cmds_rx: Receiver<Command>) -> Result<Self> {
        // Setup application Github client
        let app_private_key = fs::read(args.app_private_key)?;
        let app_private_key = jsonwebtoken::EncodingKey::from_rsa_pem(&app_private_key[..])?;
        let app_github_client = Octocrab::builder()
            .app(args.app_id.into(), app_private_key)
            .build()?;

        // Launch commands dispatcher
        let commands_dispatcher = Manager::commands_dispatcher(cmds_rx);

        Ok(Self {
            app_github_client,
            commands_dispatcher,
        })
    }

    /// Stop the manager instance.
    pub(crate) fn stop(&self) {
        self.commands_dispatcher.abort();
    }

    /// Launch commands dispatcher.
    fn commands_dispatcher(mut cmds_rx: Receiver<Command>) -> JoinHandle<()> {
        tokio::spawn(async move {
            while let Some(cmd) = cmds_rx.recv().await {
                debug!("{:?}", cmd);
            }
        })
    }
}
