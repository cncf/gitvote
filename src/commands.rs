use std::fs;

use crate::{events::IssueCommentEvent, Args};
use anyhow::Result;
use lazy_static::lazy_static;
use octocrab::Octocrab;
use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::{sync::mpsc::Receiver, task::JoinHandle};
use tracing::debug;

lazy_static! {
    pub(crate) static ref CMD: Regex = Regex::new(r#"^/([a-z]+)$"#).expect("invalid CMD regexp");
}

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

/// A processor is in charge of executing commands.
#[derive(Debug)]
pub(crate) struct Processor {
    cmds_rx: Receiver<Command>,
    app_github_client: Octocrab,
}

impl Processor {
    /// Create a new processor instance.
    pub(crate) fn new(args: Args, cmds_rx: Receiver<Command>) -> Result<Self> {
        // Setup application Github client
        let app_private_key = fs::read(args.app_private_key)?;
        let app_private_key = jsonwebtoken::EncodingKey::from_rsa_pem(&app_private_key[..])?;
        let app_github_client = Octocrab::builder()
            .app(args.app_id.into(), app_private_key)
            .build()?;

        Ok(Self {
            cmds_rx,
            app_github_client,
        })
    }

    /// Start processing commands.
    pub(crate) fn start(mut self) -> JoinHandle<()> {
        tokio::spawn(async move {
            while let Some(cmd) = self.cmds_rx.recv().await {
                debug!("{:?}", cmd);
            }
        })
    }
}
