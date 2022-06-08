use axum::http::HeaderValue;
use serde::{Deserialize, Serialize};

/// Errors that may occur while creating a new event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum EventError {
    HeaderMissing,
    UnsupportedEvent,
}

/// Represents the kind of a GitHub webhook event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum Event {
    IssueComment,
}

impl TryFrom<Option<&HeaderValue>> for Event {
    type Error = EventError;

    fn try_from(value: Option<&HeaderValue>) -> Result<Self, Self::Error> {
        match value {
            Some(value) => match value.as_bytes() {
                b"issue_comment" => Ok(Event::IssueComment),
                _ => Err(EventError::UnsupportedEvent),
            },
            None => Err(EventError::HeaderMissing),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct IssueCommentEvent {
    pub action: IssueCommentEventAction,
    pub comment: Comment,
    pub installation: Installation,
    pub issue: Issue,
    pub repository: Repository,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum IssueCommentEventAction {
    Created,
    Deleted,
    Edited,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Comment {
    pub id: u64,
    pub body: Option<String>,
    pub user: User,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct User {
    pub login: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Installation {
    pub id: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Issue {
    pub number: u64,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Repository {
    pub full_name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Reaction {
    pub user: User,
    pub content: String,
}
