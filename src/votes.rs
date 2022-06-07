use crate::{
    events::{IssueCommentEvent, IssueCommentEventAction},
    metadata::{Metadata, METADATA_FILE},
};
use anyhow::{Context, Result};
use askama::Template;
use config::Config;
use octocrab::{models::InstallationId, Octocrab};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, sync::Arc};
use tokio::time::{sleep, Duration};
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

        // Post vote created comment
        installation_github_client
            .issues(owner, repo)
            .create_comment(
                event.issue.number,
                VoteCreatedTemplate::new(&event, &md).render()?,
            )
            .await?;

        // Wait vote duration
        sleep(Duration::from_secs(30)).await;

        // Post vote closed comment
        let mut voters = HashMap::new();
        voters.insert("tegioz", "In favor");
        voters.insert("cynthia-sg", "Not voted");
        installation_github_client
            .issues(owner, repo)
            .create_comment(
                event.issue.number,
                VoteClosedTemplate {
                    passed: true,
                    in_favor_percentage: 50.0,
                    pass_threshold: 50.0,
                    in_favor: 1,
                    against: 0,
                    abstain: 0,
                    not_voted: 1,
                    voters,
                }
                .render()?,
            )
            .await?;

        Ok(())
    }
}

/// Template for the vote created comment.
#[derive(Debug, Clone, Template)]
#[template(path = "vote-created.md")]
struct VoteCreatedTemplate<'a> {
    creator: &'a str,
    issue_title: &'a str,
    issue_number: u64,
    metadata_url: String,
    voters: &'a Vec<String>,
    duration: String,
    pass_threshold: f64,
}

impl<'a> VoteCreatedTemplate<'a> {
    fn new(event: &'a IssueCommentEvent, md: &'a Metadata) -> Self {
        Self {
            creator: &event.comment.user.login,
            issue_title: &event.issue.title,
            issue_number: event.issue.number,
            metadata_url: format!(
                "https://github.com/{}/blob/HEAD/{}",
                &event.repository.full_name, METADATA_FILE
            ),
            voters: &md.voters,
            duration: humantime::format_duration(md.duration).to_string(),
            pass_threshold: md.pass_threshold,
        }
    }
}

/// Template for the vote closed comment.
#[derive(Debug, Clone, Template)]
#[template(path = "vote-closed.md")]
struct VoteClosedTemplate<'a> {
    passed: bool,
    in_favor_percentage: f64,
    pass_threshold: f64,
    in_favor: u64,
    against: u64,
    abstain: u64,
    not_voted: u64,
    voters: HashMap<&'a str, &'a str>,
}
