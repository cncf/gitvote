//! This module defines the commands supported and the logic to parse them from
//! GitHub events.

use anyhow::Result;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;
use tracing::error;

use crate::{
    cfg_repo::{Cfg, CfgError},
    github::{
        DynGH, Event, IssueCommentEventAction, IssueEventAction, PullRequestEventAction, split_full_name,
    },
};

/// Available commands.
const CMD_CREATE_VOTE: &str = "vote";
const CMD_CANCEL_VOTE: &str = "cancel-vote";
const CMD_CHECK_VOTE: &str = "check-vote";

/// Regex used to detect commands in issues/prs comments.
static CMD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^/(vote|cancel-vote|check-vote)-?([a-zA-Z0-9_-]*)\s*$").expect("invalid CMD regexp")
});

/// Represents a command to be executed, usually created from a GitHub event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(clippy::enum_variant_names)]
pub(crate) enum Command {
    CreateVote(CreateVoteInput),
    CancelVote(CancelVoteInput),
    CheckVote(CheckVoteInput),
}

impl Command {
    /// Try to create a new command from an event.
    ///
    /// A command can be created from an event in two different ways:
    ///
    /// 1. A manual command is found in the event (e.g. `/vote` on comment).
    /// 2. The event triggers the creation of an automatic command (e.g. a new
    ///    PR is created and one of the affected files matches a predefined
    ///    pattern).
    ///
    /// Manual commands have preference, and if one is found, automatic ones
    /// won't be processed.
    ///
    pub(crate) async fn from_event(gh: DynGH, event: &Event) -> Option<Self> {
        if let Some(cmd) = Command::from_event_manual(event) {
            Some(cmd)
        } else {
            match Command::from_event_automatic(gh, event).await {
                Ok(cmd) => cmd,
                Err(err) => {
                    error!(?err, ?event, "error processing automatic command");
                    None
                }
            }
        }
    }

    /// Get manual command from event, if available.
    fn from_event_manual(event: &Event) -> Option<Self> {
        // Get the content where we'll try to extract the command from
        let content = match event {
            Event::Issue(event) if event.action == IssueEventAction::Opened => &event.issue.body,
            Event::IssueComment(event) if event.action == IssueCommentEventAction::Created => {
                &event.comment.body
            }
            Event::PullRequest(event) if event.action == PullRequestEventAction::Opened => {
                &event.pull_request.body
            }
            _ => return None,
        };

        // Create a new command from the content (if possible)
        if let Some(content) = content
            && let Some(captures) = CMD.captures(content)
        {
            let cmd = captures.get(1)?.as_str();
            let profile = match captures.get(2)?.as_str() {
                "" => None,
                profile => Some(profile),
            };
            match cmd {
                CMD_CREATE_VOTE => {
                    return Some(Command::CreateVote(CreateVoteInput::new(profile, event)));
                }
                CMD_CANCEL_VOTE => return Some(Command::CancelVote(CancelVoteInput::new(event))),
                CMD_CHECK_VOTE => return Some(Command::CheckVote(CheckVoteInput::new(event))),
                _ => return None,
            }
        }

        None
    }

    /// Create automatic command from event, if applicable.
    async fn from_event_automatic(gh: DynGH, event: &Event) -> Result<Option<Self>> {
        match event {
            // Pull request opened
            Event::PullRequest(event) if event.action == PullRequestEventAction::Opened => {
                // Get configuration
                let inst_id = event.installation.id as u64;
                let (owner, repo) = split_full_name(&event.repository.full_name);
                let cfg = match Cfg::get(gh.clone(), inst_id, owner, repo).await {
                    Err(CfgError::ConfigNotFound) => return Ok(None),
                    Err(err) => return Err(err.into()),
                    Ok(cfg) => cfg,
                };

                // Process automation if enabled
                if let Some(automation) = cfg.automation {
                    if !automation.enabled || automation.rules.is_empty() {
                        return Ok(None);
                    }

                    // Check if any of the PR files matches the automation rules
                    let pr_number = event.pull_request.number;
                    let pr_files = gh.get_pr_files(inst_id, owner, repo, pr_number).await?;
                    for rule in automation.rules {
                        if rule.matches(pr_files.as_slice())? {
                            let cmd = Command::CreateVote(CreateVoteInput::new(
                                Some(&rule.profile),
                                &Event::PullRequest(event.clone()),
                            ));
                            return Ok(Some(cmd));
                        }
                    }
                }

                Ok(None)
            }
            _ => Ok(None),
        }
    }
}

/// Information required to create a new vote.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CreateVoteInput {
    pub profile_name: Option<String>,
    pub created_by: String,
    pub installation_id: i64,
    pub issue_id: i64,
    pub issue_number: i64,
    pub issue_title: String,
    pub is_pull_request: bool,
    pub repository_full_name: String,
    pub organization: Option<String>,
}

impl CreateVoteInput {
    /// Create a new `CreateVoteInput` instance from the profile and event
    /// provided.
    pub(crate) fn new(profile_name: Option<&str>, event: &Event) -> Self {
        match event {
            Event::Issue(event) => Self {
                profile_name: profile_name.map(ToString::to_string),
                created_by: event.sender.login.clone(),
                installation_id: event.installation.id,
                issue_id: event.issue.id,
                issue_number: event.issue.number,
                issue_title: event.issue.title.clone(),
                is_pull_request: event.issue.pull_request.is_some(),
                repository_full_name: event.repository.full_name.clone(),
                organization: event.organization.as_ref().map(|o| o.login.clone()),
            },
            Event::IssueComment(event) => Self {
                profile_name: profile_name.map(ToString::to_string),
                created_by: event.sender.login.clone(),
                installation_id: event.installation.id,
                issue_id: event.issue.id,
                issue_number: event.issue.number,
                issue_title: event.issue.title.clone(),
                is_pull_request: event.issue.pull_request.is_some(),
                repository_full_name: event.repository.full_name.clone(),
                organization: event.organization.as_ref().map(|o| o.login.clone()),
            },
            Event::PullRequest(event) => Self {
                profile_name: profile_name.map(ToString::to_string),
                created_by: event.sender.login.clone(),
                installation_id: event.installation.id,
                issue_id: event.pull_request.id,
                issue_number: event.pull_request.number,
                issue_title: event.pull_request.title.clone(),
                is_pull_request: true,
                repository_full_name: event.repository.full_name.clone(),
                organization: event.organization.as_ref().map(|o| o.login.clone()),
            },
        }
    }
}

/// Information required to cancel an open vote.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CancelVoteInput {
    pub cancelled_by: String,
    pub installation_id: i64,
    pub issue_number: i64,
    pub is_pull_request: bool,
    pub repository_full_name: String,
}

impl CancelVoteInput {
    /// Create a new `CancelVoteInput` instance from the event provided.
    pub(crate) fn new(event: &Event) -> Self {
        match event {
            Event::Issue(event) => Self {
                cancelled_by: event.sender.login.clone(),
                installation_id: event.installation.id,
                issue_number: event.issue.number,
                is_pull_request: event.issue.pull_request.is_some(),
                repository_full_name: event.repository.full_name.clone(),
            },
            Event::IssueComment(event) => Self {
                cancelled_by: event.sender.login.clone(),
                installation_id: event.installation.id,
                issue_number: event.issue.number,
                is_pull_request: event.issue.pull_request.is_some(),
                repository_full_name: event.repository.full_name.clone(),
            },
            Event::PullRequest(event) => Self {
                cancelled_by: event.sender.login.clone(),
                installation_id: event.installation.id,
                issue_number: event.pull_request.number,
                is_pull_request: true,
                repository_full_name: event.repository.full_name.clone(),
            },
        }
    }
}

/// Information required to check the status of an open vote.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CheckVoteInput {
    pub issue_number: i64,
    pub repository_full_name: String,
}

impl CheckVoteInput {
    /// Create a new `CheckVoteInput` instance from the event provided.
    pub(crate) fn new(event: &Event) -> Self {
        match event {
            Event::Issue(event) => Self {
                issue_number: event.issue.number,
                repository_full_name: event.repository.full_name.clone(),
            },
            Event::IssueComment(event) => Self {
                issue_number: event.issue.number,
                repository_full_name: event.repository.full_name.clone(),
            },
            Event::PullRequest(event) => Self {
                issue_number: event.pull_request.number,
                repository_full_name: event.repository.full_name.clone(),
            },
        }
    }
}

#[cfg(test)]
mod tests;
