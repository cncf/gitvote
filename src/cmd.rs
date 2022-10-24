use crate::github::*;
use lazy_static::lazy_static;
use regex::Regex;
use serde::{Deserialize, Serialize};

/// Available commands.
const CMD_CREATE_VOTE: &str = "vote";
const CMD_CANCEL_VOTE: &str = "cancel-vote";

lazy_static! {
    /// Regex used to detect commands in issues/prs comments.
    static ref CMD: Regex = Regex::new(r#"(?m)^/(vote|cancel-vote)-?([a-zA-Z0-9]*)\s*$"#)
        .expect("invalid CMD regexp");
}

/// Represents a command to be executed, usually created from a GitHub event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum Command {
    CreateVote(CreateVoteInput),
    CancelVote(CancelVoteInput),
}

impl Command {
    /// Try to create a new command from an event.
    pub(crate) fn from_event(event: Event) -> Option<Self> {
        // Get the content where we'll try to extract the command from
        let content = match event {
            Event::Issue(ref event) => {
                if event.action != IssueEventAction::Opened {
                    return None;
                }
                &event.issue.body
            }
            Event::IssueComment(ref event) => {
                if event.action != IssueCommentEventAction::Created {
                    return None;
                }
                &event.comment.body
            }
            Event::PullRequest(ref event) => {
                if event.action != PullRequestEventAction::Opened {
                    return None;
                }
                &event.pull_request.body
            }
        };

        // Create a new command from the content (if possible)
        if let Some(content) = content {
            if let Some(captures) = CMD.captures(content) {
                let cmd = captures.get(1)?.as_str();
                let profile = match captures.get(2)?.as_str() {
                    "" => None,
                    profile => Some(profile.to_string()),
                };
                match cmd {
                    CMD_CREATE_VOTE => {
                        return Some(Command::CreateVote(CreateVoteInput::new(profile, event)))
                    }
                    CMD_CANCEL_VOTE => {
                        return Some(Command::CancelVote(CancelVoteInput::new(event)))
                    }
                    _ => return None,
                }
            }
        }

        None
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
    /// Create a new CreateVoteInput instance from the profile and event
    /// provided.
    pub(crate) fn new(profile_name: Option<String>, event: Event) -> Self {
        match event {
            Event::Issue(event) => Self {
                profile_name,
                created_by: event.sender.login,
                installation_id: event.installation.id,
                issue_id: event.issue.id,
                issue_number: event.issue.number,
                issue_title: event.issue.title,
                is_pull_request: event.issue.pull_request.is_some(),
                repository_full_name: event.repository.full_name,
                organization: event.organization.map(|o| o.login),
            },
            Event::IssueComment(event) => Self {
                profile_name,
                created_by: event.sender.login,
                installation_id: event.installation.id,
                issue_id: event.issue.id,
                issue_number: event.issue.number,
                issue_title: event.issue.title,
                is_pull_request: event.issue.pull_request.is_some(),
                repository_full_name: event.repository.full_name,
                organization: event.organization.map(|o| o.login),
            },
            Event::PullRequest(event) => Self {
                profile_name,
                created_by: event.sender.login,
                installation_id: event.installation.id,
                issue_id: event.pull_request.id,
                issue_number: event.pull_request.number,
                issue_title: event.pull_request.title,
                is_pull_request: true,
                repository_full_name: event.repository.full_name,
                organization: event.organization.map(|o| o.login),
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
    /// Create a new CancelVoteInput instance from the event provided.
    pub(crate) fn new(event: Event) -> Self {
        match event {
            Event::Issue(event) => Self {
                cancelled_by: event.sender.login,
                installation_id: event.installation.id,
                issue_number: event.issue.number,
                is_pull_request: event.issue.pull_request.is_some(),
                repository_full_name: event.repository.full_name,
            },
            Event::IssueComment(event) => Self {
                cancelled_by: event.sender.login,
                installation_id: event.installation.id,
                issue_number: event.issue.number,
                is_pull_request: event.issue.pull_request.is_some(),
                repository_full_name: event.repository.full_name,
            },
            Event::PullRequest(event) => Self {
                cancelled_by: event.sender.login,
                installation_id: event.installation.id,
                issue_number: event.pull_request.number,
                is_pull_request: true,
                repository_full_name: event.repository.full_name,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;

    #[test]
    fn command_from_issue_event_unsupported_action() {
        let mut event = setup_test_issue_event();
        event.action = IssueEventAction::Assigned;
        event.issue.body = Some(format!("/{}", CMD_CREATE_VOTE));
        let event = Event::Issue(event);

        assert_eq!(Command::from_event(event), None);
    }

    #[test]
    fn command_from_issue_event_no_cmd() {
        let mut event = setup_test_issue_event();
        event.action = IssueEventAction::Opened;
        event.issue.body = Some("Hi!".to_string());
        let event = Event::Issue(event);

        assert_eq!(Command::from_event(event), None);
    }

    #[test]
    fn command_from_issue_event_create_vote_cmd_default_profile() {
        let mut event = setup_test_issue_event();
        event.action = IssueEventAction::Opened;
        event.issue.body = Some(format!("/{}", CMD_CREATE_VOTE));
        let event = Event::Issue(event);

        assert_eq!(
            Command::from_event(event.clone()),
            Some(Command::CreateVote(CreateVoteInput::new(None, event)))
        );
    }

    #[test]
    fn command_from_issue_event_create_vote_cmd_profile1() {
        let mut event = setup_test_issue_event();
        event.action = IssueEventAction::Opened;
        event.issue.body = Some(format!("/{}-{}", CMD_CREATE_VOTE, PROFILE_NAME));
        let event = Event::Issue(event);

        assert_eq!(
            Command::from_event(event.clone()),
            Some(Command::CreateVote(CreateVoteInput::new(
                Some("profile1".to_string()),
                event
            )))
        );
    }

    #[test]
    fn command_from_issue_comment_event_unsupported_action() {
        let mut event = setup_test_issue_comment_event();
        event.action = IssueCommentEventAction::Edited;
        event.issue.body = Some(CMD_CREATE_VOTE.to_string());
        let event = Event::IssueComment(event);

        assert_eq!(Command::from_event(event), None);
    }

    #[test]
    fn command_from_issue_comment_event_create_vote_cmd_default_profile() {
        let mut event = setup_test_issue_comment_event();
        event.action = IssueCommentEventAction::Created;
        event.comment.body = Some(format!("/{}", CMD_CREATE_VOTE));
        let event = Event::IssueComment(event);

        assert_eq!(
            Command::from_event(event.clone()),
            Some(Command::CreateVote(CreateVoteInput::new(None, event)))
        );
    }

    #[test]
    fn command_from_issue_comment_event_cancel_vote_cmd() {
        let mut event = setup_test_issue_comment_event();
        event.action = IssueCommentEventAction::Created;
        event.comment.body = Some(format!("/{}", CMD_CANCEL_VOTE));
        let event = Event::IssueComment(event);

        assert_eq!(
            Command::from_event(event.clone()),
            Some(Command::CancelVote(CancelVoteInput::new(event)))
        );
    }

    #[test]
    fn command_from_pr_event_unsupported_action() {
        let mut event = setup_test_pr_event();
        event.action = PullRequestEventAction::Edited;
        event.pull_request.body = Some(CMD_CREATE_VOTE.to_string());
        let event = Event::PullRequest(event);

        assert_eq!(Command::from_event(event), None);
    }

    #[test]
    fn command_from_pr_event_create_vote_cmd_default_profile() {
        let mut event = setup_test_pr_event();
        event.action = PullRequestEventAction::Opened;
        event.pull_request.body = Some(format!("/{}", CMD_CREATE_VOTE));
        let event = Event::PullRequest(event);

        assert_eq!(
            Command::from_event(event.clone()),
            Some(Command::CreateVote(CreateVoteInput::new(None, event)))
        );
    }
}
