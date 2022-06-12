use crate::{
    conf::{RepoConfig, REPO_CONFIG_FILE},
    github::IssueCommentEvent,
    votes,
};
use askama::Template;

/// Template for the index document.
#[derive(Debug, Clone, Template)]
#[template(path = "index.html")]
pub(crate) struct Index {}

/// Template for the vote created comment.
#[derive(Debug, Clone, Template)]
#[template(path = "vote-created.md")]
pub(crate) struct VoteCreated<'a> {
    creator: &'a str,
    issue_title: &'a str,
    issue_number: u64,
    config_url: String,
    voters: &'a Vec<String>,
    duration: String,
    pass_threshold: f64,
}

impl<'a> VoteCreated<'a> {
    /// Create a new VoteCreated template.
    pub(crate) fn new(event: &'a IssueCommentEvent, cfg: &'a RepoConfig) -> Self {
        Self {
            creator: &event.comment.user.login,
            issue_title: &event.issue.title,
            issue_number: event.issue.number,
            config_url: format!(
                "https://github.com/{}/blob/HEAD/{}",
                &event.repository.full_name, REPO_CONFIG_FILE
            ),
            voters: &cfg.voters,
            duration: humantime::format_duration(cfg.duration).to_string(),
            pass_threshold: cfg.pass_threshold,
        }
    }
}

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

/// Template for the vote in progress comment.
#[derive(Debug, Clone, Template)]
#[template(path = "vote-in-progress.md")]
pub(crate) struct VoteInProgress<'a> {
    user: &'a str,
}

impl<'a> VoteInProgress<'a> {
    /// Create a new VoteInProgress template.
    pub(crate) fn new(user: &'a str) -> Self {
        Self { user }
    }
}
