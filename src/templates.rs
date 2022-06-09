use crate::{
    github::IssueCommentEvent,
    metadata::{Metadata, METADATA_FILE},
    votes::Results,
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
    metadata_url: String,
    voters: &'a Vec<String>,
    duration: String,
    pass_threshold: f64,
}

impl<'a> VoteCreated<'a> {
    /// Create a new VoteCreated template.
    pub(crate) fn new(event: &'a IssueCommentEvent, metadata: &'a Metadata) -> Self {
        Self {
            creator: &event.comment.user.login,
            issue_title: &event.issue.title,
            issue_number: event.issue.number,
            metadata_url: format!(
                "https://github.com/{}/blob/HEAD/{}",
                &event.repository.full_name, METADATA_FILE
            ),
            voters: &metadata.voters,
            duration: humantime::format_duration(metadata.duration).to_string(),
            pass_threshold: metadata.pass_threshold,
        }
    }
}

/// Template for the vote closed comment.
#[derive(Debug, Clone, Template)]
#[template(path = "vote-closed.md")]
pub(crate) struct VoteClosed<'a> {
    results: &'a Results,
}

impl<'a> VoteClosed<'a> {
    /// Create a new VoteClosed template.
    pub(crate) fn new(results: &'a Results) -> Self {
        Self { results }
    }
}
