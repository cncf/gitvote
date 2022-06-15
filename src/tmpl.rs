use crate::{github::IssueCommentEvent, votes};
use askama::Template;

/// Template for the index document.
#[derive(Debug, Clone, Template)]
#[template(path = "index.html")]
pub(crate) struct Index {}

/// Template for the vote closed comment.
#[derive(Debug, Clone, Template)]
#[template(path = "vote-closed.md")]
pub(crate) struct VoteClosed<'a> {
    results: &'a votes::Results,
}

impl<'a> VoteClosed<'a> {
    /// Create a new VoteClosed template.
    pub(crate) fn new(results: &'a votes::Results) -> Self {
        Self { results }
    }
}

/// Template for the vote created comment.
#[derive(Debug, Clone, Template)]
#[template(path = "vote-created.md")]
pub(crate) struct VoteCreated<'a> {
    creator: &'a str,
    issue_title: &'a str,
    issue_number: i64,
    duration: String,
    pass_threshold: f64,
    allowed_voters: Vec<String>,
}

impl<'a> VoteCreated<'a> {
    /// Create a new VoteCreated template.
    pub(crate) fn new(event: &'a IssueCommentEvent, cfg: &'a votes::CfgProfile) -> Self {
        let allowed_voters = match &cfg.allowed_voters {
            Some(v) => v.clone(),
            None => vec![],
        };
        Self {
            creator: &event.comment.user.login,
            issue_title: &event.issue.title,
            issue_number: event.issue.number,
            duration: humantime::format_duration(cfg.duration).to_string(),
            pass_threshold: cfg.pass_threshold,
            allowed_voters,
        }
    }
}

/// Template for the vote in progress comment.
#[derive(Debug, Clone, Template)]
#[template(path = "vote-in-progress.md")]
pub(crate) struct VoteInProgress<'a> {
    user: &'a str,
    is_pull_request: bool,
}

impl<'a> VoteInProgress<'a> {
    /// Create a new VoteInProgress template.
    pub(crate) fn new(user: &'a str, is_pull_request: bool) -> Self {
        Self {
            user,
            is_pull_request,
        }
    }
}

/// Template for the vote restricted comment.
#[derive(Debug, Clone, Template)]
#[template(path = "vote-restricted.md")]
pub(crate) struct VoteRestricted<'a> {
    user: &'a str,
}

impl<'a> VoteRestricted<'a> {
    /// Create a new VoteRestricted template.
    pub(crate) fn new(user: &'a str) -> Self {
        Self { user }
    }
}
