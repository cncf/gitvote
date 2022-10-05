use crate::{
    github::{TeamSlug, UserName},
    votes::{CfgProfile, CreateVoteInput, VoteResults},
};
use askama::Template;

/// Template for the config not found comment.
#[derive(Debug, Clone, Template)]
#[template(path = "config-not-found.md")]
pub(crate) struct ConfigNotFound {}

/// Template for the config profile not found comment.
#[derive(Debug, Clone, Template)]
#[template(path = "config-profile-not-found.md")]
pub(crate) struct ConfigProfileNotFound {}

/// Template for the index document.
#[derive(Debug, Clone, Template)]
#[template(path = "index.html")]
pub(crate) struct Index {}

/// Template for the invalid config comment.
#[derive(Debug, Clone, Template)]
#[template(path = "invalid-config.md")]
pub(crate) struct InvalidConfig<'a> {
    reason: &'a str,
}

impl<'a> InvalidConfig<'a> {
    /// Create a new InvalidConfig template.
    pub(crate) fn new(reason: &'a str) -> Self {
        Self { reason }
    }
}

/// Template for the no vote in progress comment.
#[derive(Debug, Clone, Template)]
#[template(path = "no-vote-in-progress.md")]
pub(crate) struct NoVoteInProgress<'a> {
    user: &'a str,
    is_pull_request: bool,
}

impl<'a> NoVoteInProgress<'a> {
    /// Create a new NoVoteInProgress template.
    pub(crate) fn new(user: &'a str, is_pull_request: bool) -> Self {
        Self {
            user,
            is_pull_request,
        }
    }
}

/// Template for the vote cancelled comment.
#[derive(Debug, Clone, Template)]
#[template(path = "vote-cancelled.md")]
pub(crate) struct VoteCancelled<'a> {
    user: &'a str,
    is_pull_request: bool,
}

impl<'a> VoteCancelled<'a> {
    /// Create a new VoteCancelled template.
    pub(crate) fn new(user: &'a str, is_pull_request: bool) -> Self {
        Self {
            user,
            is_pull_request,
        }
    }
}

/// Template for the vote closed comment.
#[derive(Debug, Clone, Template)]
#[template(path = "vote-closed.md")]
pub(crate) struct VoteClosed<'a> {
    results: &'a VoteResults,
}

impl<'a> VoteClosed<'a> {
    /// Create a new VoteClosed template.
    pub(crate) fn new(results: &'a VoteResults) -> Self {
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
    org: &'a str,
    teams: &'a [TeamSlug],
    users: &'a [UserName],
}

impl<'a> VoteCreated<'a> {
    /// Create a new VoteCreated template.
    pub(crate) fn new(input: &'a CreateVoteInput, cfg: &'a CfgProfile) -> Self {
        // Prepare teams and users allowed to vote
        let (mut teams, mut users): (&[TeamSlug], &[UserName]) = (&[], &[]);
        if let Some(allowed_voters) = &cfg.allowed_voters {
            if let Some(v) = &allowed_voters.teams {
                teams = v.as_slice();
            }
            if let Some(v) = &allowed_voters.users {
                users = v.as_slice();
            }
        }

        // Get organization name if available
        let org = match &input.organization {
            Some(org) => org.as_ref(),
            None => "",
        };

        Self {
            creator: &input.created_by,
            issue_title: &input.issue_title,
            issue_number: input.issue_number,
            duration: humantime::format_duration(cfg.duration).to_string(),
            pass_threshold: cfg.pass_threshold,
            org,
            teams,
            users,
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
