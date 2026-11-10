//! This module defines an abstraction layer over the GitHub API.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Error, Result, bail, format_err};
use async_trait::async_trait;
use axum::http::HeaderValue;
use graphql_client::GraphQLQuery;
use http::StatusCode;
#[cfg(test)]
use mockall::automock;
use octocrab::{
    DefaultOctocrabBuilderConfig, NoAuth, NoSvc, NotLayerReady, Octocrab, OctocrabBuilder, Page,
    map_github_error,
    models::{CommentId as OctocrabCommentId, InstallationId},
    service::middleware::retry::RetryConfig,
};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};
use tracing::{debug, warn};

use crate::cfg_repo::CfgProfile;

#[cfg(test)]
mod tests;

/// Characters escaped in a branch name used in a URL path. Slashes are kept, as the
/// GitHub API expects a branch name containing them as multiple path segments.
const BRANCH_ENCODE_SET: &AsciiSet =
    &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'/').remove(b'_').remove(b'~');

/// Number of comments requested per page when looking for a comment with a marker,
/// which is the maximum GitHub allows.
const COMMENTS_PAGE_SIZE: u8 = 100;

/// Configuration file name.
const CONFIG_FILE: &str = ".gitvote.yml";

/// Maximum time allowed to establish a connection with the GitHub API.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Messages of the GraphQL errors that retrying the creation of a discussion will not
/// solve: permissions not granted, and a repository or category that does not exist.
const DISCUSSION_REJECTED_MESSAGES: [&str; 2] =
    ["Resource not accessible by integration", "Could not resolve to a"];

/// Name used for the check run in GitHub.
const GITVOTE_CHECK_NAME: &str = "GitVote";

/// Repository where the organization wide config file should be located.
const ORG_CONFIG_REPO: &str = ".github";

/// Characters escaped in a value used as a single URL path segment, like a team slug.
/// Slashes are escaped too, so that the value is not split into several segments.
const PATH_SEGMENT_ENCODE_SET: &AsciiSet =
    &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

/// Maximum time a read from the GitHub API can be waiting for data.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Number of times the retrying client sends a request again after a failure.
const REQUEST_MAX_RETRIES: usize = 3;

/// Maximum time a write to the GitHub API can be waiting to send data.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Trait that defines some operations a GH implementation must support.
#[async_trait]
#[allow(clippy::ref_option_ref)]
#[cfg_attr(test, automock)]
pub(crate) trait GH {
    /// Adds labels to the provided issue.
    async fn add_labels(
        &self,
        inst_id: u64,
        owner: &str,
        repo: &str,
        issue_number: i64,
        labels: &[&str],
    ) -> Result<()>;

    /// Creates a check run for the head commit in the provided pull request.
    async fn create_check_run(
        &self,
        inst_id: u64,
        owner: &str,
        repo: &str,
        issue_number: i64,
        check_details: &CheckDetails,
    ) -> Result<()>;

    /// Creates a check run for the commit provided.
    async fn create_commit_check_run(
        &self,
        inst_id: u64,
        owner: &str,
        repo: &str,
        head_sha: &str,
        check_details: &CheckDetails,
    ) -> Result<()>;

    /// Creates a new discussion in the repository provided. If GitHub rejects it for a
    /// reason that retrying will not solve the error returned is a
    /// [`DiscussionRejectedError`].
    async fn create_discussion(
        &self,
        inst_id: u64,
        owner: &str,
        repo: &str,
        category: &str,
        title: &str,
        body: &str,
    ) -> Result<()>;

    /// Deletes the comment provided.
    async fn delete_comment(&self, inst_id: u64, owner: &str, repo: &str, comment_id: i64) -> Result<()>;

    /// Returns all users allowed to vote on a given vote. Teams that do not exist are
    /// skipped, and all repository collaborators are allowed to vote only when the
    /// profile lists no teams or users.
    async fn get_allowed_voters(
        &self,
        inst_id: u64,
        cfg: &CfgProfile,
        owner: &str,
        repo: &str,
        org: Option<&String>,
    ) -> Result<Vec<UserName>>;

    /// Returns all repository collaborators.
    #[allow(dead_code)]
    async fn get_collaborators(&self, inst_id: u64, owner: &str, repo: &str) -> Result<Vec<UserName>>;

    /// Returns the reactions for the provided comment. If the comment does not exist the
    /// error returned is a [`CommentNotFoundError`].
    async fn get_comment_reactions(
        &self,
        inst_id: u64,
        owner: &str,
        repo: &str,
        comment_id: i64,
    ) -> Result<Vec<Reaction>>;

    /// Returns the configuration file, if there is one in any of the supported
    /// locations.
    async fn get_config_file(&self, inst_id: u64, owner: &str, repo: &str) -> Result<Option<String>>;

    /// Returns the pull request files.
    async fn get_pr_files(&self, inst_id: u64, owner: &str, repo: &str, pr_number: i64) -> Result<Vec<File>>;

    /// Returns the head commit of the provided pull request.
    async fn get_pr_head_sha(&self, inst_id: u64, owner: &str, repo: &str, pr_number: i64) -> Result<String>;

    /// Returns the repository installation identifier.
    async fn get_repository_installation_id(&self, owner: &str, repo: &str) -> Result<u64>;

    /// Returns all members of the provided team. If the team does not exist the error
    /// returned is a [`TeamNotFoundError`].
    #[allow(dead_code)]
    async fn get_team_members(
        &self,
        inst_id: u64,
        org: &str,
        team: &str,
        exclude_maintainers: bool,
    ) -> Result<Vec<UserName>>;

    /// Checks if this application posted a comment ending with the marker provided in
    /// the repository's issue given since the time provided. Comments posted by anyone
    /// else are ignored, as anyone can write the marker, and so are the ones with the
    /// marker elsewhere in their body, as it may be part of some text users control.
    async fn has_comment_with_marker(
        &self,
        inst_id: u64,
        owner: &str,
        repo: &str,
        issue_number: i64,
        marker: &str,
        since: OffsetDateTime,
    ) -> Result<bool>;

    /// Checks if this application created a discussion ending with the marker provided
    /// in the repository's discussion category given since the time provided.
    /// Discussions created by anyone else are ignored, as anyone can write the marker,
    /// and so are the ones with the marker elsewhere in their body, as it may be part
    /// of some text users control. If GitHub rejects the lookup for a reason that
    /// retrying will not solve, like the creation of a discussion, the error returned
    /// is a [`DiscussionRejectedError`].
    async fn has_discussion_with_marker(
        &self,
        inst_id: u64,
        owner: &str,
        repo: &str,
        category: &str,
        marker: &str,
        since: OffsetDateTime,
    ) -> Result<bool>;

    /// Verifies if the `GitVote` check is required via branch protection in the
    /// repository's branch provided.
    async fn is_check_required(&self, inst_id: u64, owner: &str, repo: &str, branch: &str) -> Result<bool>;

    /// Posts the comment provided in the repository's issue given.
    async fn post_comment(
        &self,
        inst_id: u64,
        owner: &str,
        repo: &str,
        issue_number: i64,
        body: &str,
    ) -> Result<CommentId>;

    /// Removes a label from the provided issue.
    async fn remove_label(
        &self,
        inst_id: u64,
        owner: &str,
        repo: &str,
        issue_number: i64,
        label: &str,
    ) -> Result<()>;

    /// Checks if the user given is a collaborator of the provided repository.
    async fn user_is_collaborator(&self, inst_id: u64, owner: &str, repo: &str, user: &str) -> Result<bool>;
}

/// Type alias to represent the builder of a GitHub client.
pub(crate) type ClientBuilder = OctocrabBuilder<NoSvc, DefaultOctocrabBuilderConfig, NoAuth, NotLayerReady>;

/// Type alias to represent a comment id.
type CommentId = i64;

/// Type alias used by the GraphQL queries to represent the `DateTime` scalar, which is
/// an RFC 3339 timestamp.
type DateTime = String;

/// Type alias to represent a GH trait object.
pub(crate) type DynGH = Arc<dyn GH + Send + Sync>;

/// Type alias to represent a team slug.
pub(crate) type TeamSlug = String;

/// Type alias to represent a username.
pub(crate) type UserName = String;

/// Announcement discussions query.
#[derive(Debug, Clone, GraphQLQuery)]
#[graphql(
    schema_path = "src/graphql/github_schema.graphql",
    query_path = "src/graphql/announcement_discussions_query.graphql",
    response_derives = "Debug, PartialEq, Eq"
)]
struct AnnouncementDiscussionsQuery;

/// Announcement repository query.
#[derive(Debug, Clone, GraphQLQuery)]
#[graphql(
    schema_path = "src/graphql/github_schema.graphql",
    query_path = "src/graphql/announcement_repo_query.graphql",
    response_derives = "Debug, PartialEq, Eq"
)]
struct AnnouncementRepoQuery;

/// Repository branch as returned by the GitHub API, with only the fields needed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Branch {
    /// Name of the branch.
    name: String,

    /// Protection settings of the branch, if it is protected.
    protection: Option<Protection>,
}

/// Details of a check run to create.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CheckDetails {
    /// Status of the check run, like `in_progress` or `completed`.
    pub status: String,
    /// Summary of the check run, also used as its title.
    pub summary: String,

    /// Conclusion of the check run, required when it is completed.
    pub conclusion: Option<String>,
}

/// Issue or pull request comment included in an event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Comment {
    /// Identifier of the comment.
    pub id: i64,

    /// Body of the comment, which may be missing.
    pub body: Option<String>,
}

/// Error returned when a comment does not exist, usually because it was deleted.
#[derive(Debug, Error)]
#[error("comment not found")]
pub(crate) struct CommentNotFoundError;

/// Create discussion mutation.
#[derive(Debug, Clone, GraphQLQuery)]
#[graphql(
    schema_path = "src/graphql/github_schema.graphql",
    query_path = "src/graphql/create_discussion.graphql",
    response_derives = "Debug, PartialEq, Eq"
)]
struct CreateDiscussion;

/// Error returned when a discussion cannot be created for a reason that retrying will
/// not solve, like missing permissions or a category that does not exist.
#[derive(Debug, Error)]
#[error(transparent)]
pub(crate) struct DiscussionRejectedError(
    /// Error GitHub returned when rejecting the request.
    pub Error,
);

/// Represents a GitHub event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum Event {
    /// Issue event.
    Issue(IssueEvent),
    /// Issue comment event, also sent for pull request comments.
    IssueComment(IssueCommentEvent),
    /// Pull request event.
    PullRequest(PullRequestEvent),
}

/// Errors that may occur while creating a new event instance.
#[derive(Debug, Error, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum EventError {
    /// The event body could not be parsed.
    #[error("invalid body: {0}")]
    InvalidBody(String),
    /// The event type header was not provided.
    #[error("event header missing")]
    MissingHeader,
    /// The event type is not supported.
    #[error("unsupported event")]
    UnsupportedEvent,
}

/// Pull request file as returned by the GitHub API, with only the fields needed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct File {
    /// Path of the file in the repository.
    pub filename: String,
}

/// GH implementation backed by the GitHub API. Octocrab's retry policies do not look at
/// the request, so requests are split between two clients depending on whether they can
/// be repeated.
pub(crate) struct GHApi {
    /// Identifier of the GitHub application, which tells apart what it published.
    app_id: u64,
    /// Client for requests that are safe to repeat: the ones reading data, deletions and
    /// adding labels, which change nothing the second time, and creating check runs, as
    /// the latest one replaces the previous ones with the same name.
    retrying_client: Octocrab,
    /// Client for requests that must be sent only once within a call: posting comments
    /// and creating discussions. A request that failed may have been processed, so
    /// sending it again could leave a visible duplicate.
    single_attempt_client: Octocrab,
}

impl GHApi {
    /// Creates a new `GHApi` instance.
    pub(crate) fn new(app_id: u64, retrying_client: Octocrab, single_attempt_client: Octocrab) -> Self {
        Self {
            app_id,
            retrying_client,
            single_attempt_client,
        }
    }
}

/// GitHub application as returned by the GitHub API, with only the fields needed.
#[derive(Debug, Deserialize)]
struct GitHubAppResponse {
    /// Identifier of the application.
    id: u64,
}

/// GitHub application installation an event was delivered to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Installation {
    /// Identifier of the installation.
    pub id: i64,
}

/// Issue included in an event, which may be a pull request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Issue {
    /// Identifier of the issue.
    pub id: i64,
    /// Number of the issue in the repository.
    pub number: i64,
    /// Title of the issue.
    pub title: String,

    /// Body of the issue, which may be missing.
    pub body: Option<String>,
    /// Pull request details, only present when the issue is a pull request.
    pub pull_request: Option<PullRequestInIssue>,
}

/// Issue comment event payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct IssueCommentEvent {
    /// Action performed on the comment.
    pub action: IssueCommentEventAction,
    /// Comment the event refers to.
    pub comment: Comment,
    /// Installation the event was delivered to.
    pub installation: Installation,
    /// Issue or pull request the comment belongs to.
    pub issue: Issue,
    /// Repository the issue belongs to.
    pub repository: Repository,
    /// User who triggered the event.
    pub sender: User,

    /// Organization the repository belongs to, if any.
    pub organization: Option<Organization>,
}

/// Actions of an issue comment event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum IssueCommentEventAction {
    /// A comment was created.
    Created,
    /// Any other action, which is not handled.
    #[serde(other)]
    Other,
}

/// Issue comment as returned by the GitHub API, with only the fields needed to look for
/// a marker in the comments posted by this application.
#[derive(Debug, Deserialize)]
struct IssueCommentResponse {
    /// Body of the comment, which may be missing.
    body: Option<String>,
    /// Application the comment was posted by, if any.
    performed_via_github_app: Option<GitHubAppResponse>,
}

/// Issue event payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct IssueEvent {
    /// Action performed on the issue.
    pub action: IssueEventAction,
    /// Installation the event was delivered to.
    pub installation: Installation,
    /// Issue the event refers to.
    pub issue: Issue,
    /// Repository the issue belongs to.
    pub repository: Repository,
    /// User who triggered the event.
    pub sender: User,

    /// Organization the repository belongs to, if any.
    pub organization: Option<Organization>,
}

/// Actions of an issue event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum IssueEventAction {
    /// An issue was opened.
    Opened,
    /// Any other action, which is not handled.
    #[serde(other)]
    Other,
}

/// Organization included in an event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Organization {
    /// Login of the organization.
    pub login: String,
}

/// Branch protection settings as returned by the GitHub API.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Protection {
    /// Status checks required to pass before merging, if any.
    required_status_checks: Option<RequiredStatusCheck>,
}

/// Pull request included in an event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PullRequest {
    /// Branch the pull request is meant to be merged into.
    pub base: PullRequestBase,
    /// Identifier of the pull request.
    pub id: i64,
    /// Number of the pull request in the repository.
    pub number: i64,
    /// Title of the pull request.
    pub title: String,

    /// Body of the pull request, which may be missing.
    pub body: Option<String>,
}

/// Base branch of a pull request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PullRequestBase {
    /// Name of the base branch.
    #[serde(rename = "ref")]
    pub reference: String,
}

/// Pull request event payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PullRequestEvent {
    /// Action performed on the pull request.
    pub action: PullRequestEventAction,
    /// Installation the event was delivered to.
    pub installation: Installation,
    /// Pull request the event refers to.
    pub pull_request: PullRequest,
    /// Repository the pull request belongs to.
    pub repository: Repository,
    /// User who triggered the event.
    pub sender: User,

    /// Organization the repository belongs to, if any.
    pub organization: Option<Organization>,
}

/// Actions of a pull request event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PullRequestEventAction {
    /// A pull request was opened.
    Opened,
    /// New commits were pushed to a pull request.
    Synchronize,
    // Serde requires the catch-all variant to be the last one
    /// Any other action, which is not handled.
    #[serde(other)]
    Other,
}

/// Pull request details included in an issue that is a pull request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PullRequestInIssue {
    /// API URL of the pull request.
    pub url: String,
}

/// Reaction to a comment made by a user.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Reaction {
    /// Content of the reaction, like `+1` or `heart`.
    pub content: String,
    /// Time the reaction was created, as an RFC 3339 timestamp.
    pub created_at: String,
    /// User who reacted.
    pub user: User,
}

/// Reaction as returned by the GitHub API, where the user may be null.
#[derive(Debug, Deserialize)]
struct ReactionResponse {
    /// Content of the reaction.
    content: String,
    /// Time the reaction was created, as an RFC 3339 timestamp.
    created_at: String,

    /// User who reacted, missing when the account no longer exists.
    user: Option<User>,
}

/// Repository included in an event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Repository {
    /// Full name of the repository, in the `owner/repo` format.
    pub full_name: String,
}

/// Status checks required by a branch protection rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct RequiredStatusCheck {
    /// Names of the required checks.
    contexts: Vec<String>,
}

/// Error returned when a team does not exist, or it is not visible to the installation.
#[derive(Debug, Error)]
#[error("team not found")]
struct TeamNotFoundError;

/// GitHub user.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct User {
    /// Login of the user.
    pub login: UserName,
}

#[async_trait]
impl GH for GHApi {
    /// [`GH::add_labels`].
    async fn add_labels(
        &self,
        inst_id: u64,
        owner: &str,
        repo: &str,
        issue_number: i64,
        labels: &[&str],
    ) -> Result<()> {
        let client = self.retrying_client.installation(InstallationId(inst_id))?;
        let labels = labels.iter().map(ToString::to_string).collect::<Vec<String>>();
        client.issues(owner, repo).add_labels(issue_number as u64, &labels).await?;
        Ok(())
    }

    /// [`GH::create_check_run`].
    async fn create_check_run(
        &self,
        inst_id: u64,
        owner: &str,
        repo: &str,
        issue_number: i64,
        check_details: &CheckDetails,
    ) -> Result<()> {
        // Get the head commit of the pull request
        let client = self.retrying_client.installation(InstallationId(inst_id))?;
        let head_sha = fetch_pr_head_sha(&client, owner, repo, issue_number).await?;

        // Create the check run
        post_check_run(&client, owner, repo, &head_sha, check_details).await
    }

    /// [`GH::create_commit_check_run`].
    async fn create_commit_check_run(
        &self,
        inst_id: u64,
        owner: &str,
        repo: &str,
        head_sha: &str,
        check_details: &CheckDetails,
    ) -> Result<()> {
        let client = self.retrying_client.installation(InstallationId(inst_id))?;
        post_check_run(&client, owner, repo, head_sha, check_details).await
    }

    /// [`GH::create_discussion`].
    async fn create_discussion(
        &self,
        inst_id: u64,
        owner: &str,
        repo: &str,
        category: &str,
        title: &str,
        body: &str,
    ) -> Result<()> {
        // Fetch some repository details needed to create a discussion
        let client = self.retrying_client.installation(InstallationId(inst_id))?;
        let (repository_id, category_id) = fetch_announcement_repo(&client, owner, repo, category).await?;

        // Create discussion
        let client = self.single_attempt_client.installation(InstallationId(inst_id))?;
        let _: create_discussion::ResponseData = client
            .graphql(&CreateDiscussion::build_query(create_discussion::Variables {
                repository_id,
                category_id,
                title: title.to_string(),
                body: body.to_string(),
            }))
            .await
            .map_err(map_discussion_error)
            .context("error creating announcement discussion")?;

        Ok(())
    }

    /// [`GH::delete_comment`].
    async fn delete_comment(&self, inst_id: u64, owner: &str, repo: &str, comment_id: i64) -> Result<()> {
        let client = self.retrying_client.installation(InstallationId(inst_id))?;
        client.issues(owner, repo).delete_comment(OctocrabCommentId(comment_id as u64)).await?;
        Ok(())
    }

    /// [`GH::get_allowed_voters`].
    async fn get_allowed_voters(
        &self,
        inst_id: u64,
        cfg: &CfgProfile,
        owner: &str,
        repo: &str,
        org: Option<&String>,
    ) -> Result<Vec<UserName>> {
        // All repository collaborators are allowed to vote if the profile lists no
        // teams or users, so a list that ends up empty must not widen the electorate
        let lists_voters = cfg.allowed_voters.as_ref().is_some_and(|voters| {
            voters.teams.as_ref().is_some_and(|teams| !teams.is_empty())
                || voters.users.as_ref().is_some_and(|users| !users.is_empty())
        });
        if !lists_voters {
            return self.get_collaborators(inst_id, owner, repo).await;
        }

        // Get allowed voters from configuration
        let mut allowed_voters: Vec<UserName> = vec![];
        if let Some(cfg_allowed_voters) = &cfg.allowed_voters {
            // Teams
            if let Some(org) = org
                && let Some(teams) = &cfg_allowed_voters.teams
            {
                let exclude_maintainers = cfg_allowed_voters.exclude_team_maintainers.unwrap_or(false);
                for team in teams {
                    // Skip teams that do not exist, any other failure must be retried
                    // as the electorate would be incomplete
                    let members =
                        match self.get_team_members(inst_id, org.as_str(), team, exclude_maintainers).await {
                            Ok(members) => members,
                            Err(err) if err.is::<TeamNotFoundError>() => {
                                warn!(org, team, "skipping allowed voters team not found");
                                continue;
                            }
                            Err(err) => return Err(err),
                        };

                    // Add the members not added yet (usernames are case insensitive)
                    for user in members {
                        if !allowed_voters.iter().any(|voter| voter.eq_ignore_ascii_case(&user)) {
                            allowed_voters.push(user.clone());
                        }
                    }
                }
            }

            // Users (GitHub usernames are case insensitive, so team members'
            // spelling is kept when they are also listed as users)
            if let Some(users) = &cfg_allowed_voters.users {
                for user in users {
                    if !allowed_voters.iter().any(|voter| voter.eq_ignore_ascii_case(user)) {
                        allowed_voters.push(user.clone());
                    }
                }
            }
        }

        Ok(allowed_voters)
    }

    /// [`GH::get_collaborators`].
    async fn get_collaborators(&self, inst_id: u64, owner: &str, repo: &str) -> Result<Vec<UserName>> {
        // Get all the pages of collaborators
        let client = self.retrying_client.installation(InstallationId(inst_id))?;
        let url = format!("/repos/{owner}/{repo}/collaborators");
        let first_page: Page<User> = client.get(url, None::<&()>).await?;
        let collaborators = client.all_pages(first_page).await?.into_iter().map(|user| user.login).collect();
        Ok(collaborators)
    }

    /// [`GH::get_comment_reactions`].
    async fn get_comment_reactions(
        &self,
        inst_id: u64,
        owner: &str,
        repo: &str,
        comment_id: i64,
    ) -> Result<Vec<Reaction>> {
        // Get the installation token first: a 404 on it says nothing about the comment
        let client = self.retrying_client.installation(InstallationId(inst_id))?;
        client.installation_token().await?;

        // Get all the reactions, telling apart a comment that does not exist
        let url = format!("/repos/{owner}/{repo}/issues/comments/{comment_id}/reactions");
        let reactions = async {
            let first_page: Page<ReactionResponse> = client.get(url, None::<&()>).await?;
            client.all_pages(first_page).await
        };
        let reactions = reactions.await.map_err(|err| map_not_found_error(err, CommentNotFoundError))?;

        // Skip reactions without a user, as they cannot be counted as a vote
        let reactions = reactions
            .into_iter()
            .filter_map(|reaction| {
                let Some(user) = reaction.user else {
                    debug!(comment_id, "skipping reaction without user");
                    return None;
                };
                Some(Reaction {
                    content: reaction.content,
                    created_at: reaction.created_at,
                    user,
                })
            })
            .collect();
        Ok(reactions)
    }

    /// [`GH::get_config_file`].
    async fn get_config_file(&self, inst_id: u64, owner: &str, repo: &str) -> Result<Option<String>> {
        // Get the installation token first, as a 404 on it says nothing about the file
        let client = self.retrying_client.installation(InstallationId(inst_id))?;
        client.installation_token().await?;

        // Check possible locations for configuration file in order of precedence
        let config_path = format!(".github/{CONFIG_FILE}");
        let candidates = [
            // Check repository root
            (repo, CONFIG_FILE),
            // Check repository `.github` directory
            (repo, config_path.as_str()),
            // Check organization-wide `.github` repository
            (ORG_CONFIG_REPO, CONFIG_FILE),
        ];
        for (repo, path) in candidates {
            match client.repos(owner, repo).get_content().path(path).send().await {
                Ok(resp) if resp.items.len() == 1 => return Ok(resp.items[0].decoded_content()),
                Ok(_) => {}
                Err(err) => {
                    // Only a 404 proves the file is not in this location, moving on
                    // after any other failure could apply a lower precedence config
                    let err = Error::from(err);
                    if !is_not_found_error(&err) {
                        return Err(err.context(format!("error getting {owner}/{repo}/{path}")));
                    }
                }
            }
        }

        Ok(None)
    }

    /// [`GH::get_pr_files`].
    async fn get_pr_files(&self, inst_id: u64, owner: &str, repo: &str, pr_number: i64) -> Result<Vec<File>> {
        // Get all the pages of files
        let client = self.retrying_client.installation(InstallationId(inst_id))?;
        let url = format!("/repos/{owner}/{repo}/pulls/{pr_number}/files");
        let first_page: Page<File> = client.get(url, None::<&()>).await?;
        let files: Vec<File> = client.all_pages(first_page).await?;
        Ok(files)
    }

    /// [`GH::get_pr_head_sha`].
    async fn get_pr_head_sha(&self, inst_id: u64, owner: &str, repo: &str, pr_number: i64) -> Result<String> {
        let client = self.retrying_client.installation(InstallationId(inst_id))?;
        fetch_pr_head_sha(&client, owner, repo, pr_number).await
    }

    /// [`GH::get_repository_installation_id`].
    async fn get_repository_installation_id(&self, owner: &str, repo: &str) -> Result<u64> {
        let installation = self.retrying_client.apps().get_repository_installation(owner, repo).await?;
        Ok(installation.id.0)
    }

    /// [`GH::get_team_members`].
    async fn get_team_members(
        &self,
        inst_id: u64,
        org: &str,
        team: &str,
        exclude_maintainers: bool,
    ) -> Result<Vec<UserName>> {
        // Get the installation token first, as a 404 on it says nothing about the team
        let client = self.retrying_client.installation(InstallationId(inst_id))?;
        client.installation_token().await?;

        // Get all the members, telling apart a team that does not exist. The slug is
        // encoded, so that an invalid one is not found rather than an invalid request
        let team = utf8_percent_encode(team, PATH_SEGMENT_ENCODE_SET);
        let url = format!("/orgs/{org}/teams/{team}/members");
        let params = json!({"role": if exclude_maintainers { "member" } else { "all" }});
        let members = async {
            let first_page: Page<User> = client.get(url, Some(&params)).await?;
            client.all_pages(first_page).await
        };
        let members = members.await.map_err(|err| map_not_found_error(err, TeamNotFoundError))?;
        Ok(members.into_iter().map(|member| member.login).collect())
    }

    /// [`GH::has_comment_with_marker`].
    async fn has_comment_with_marker(
        &self,
        inst_id: u64,
        owner: &str,
        repo: &str,
        issue_number: i64,
        marker: &str,
        since: OffsetDateTime,
    ) -> Result<bool> {
        // Get the first page of the comments updated since the time provided. It is sent
        // in UTC with whole seconds, rounding it down so that no comment is left out
        let client = self.retrying_client.installation(InstallationId(inst_id))?;
        let since = since.to_offset(UtcOffset::UTC).replace_nanosecond(0)?.format(&Rfc3339)?;
        let url = format!("/repos/{owner}/{repo}/issues/{issue_number}/comments");
        let params = json!({"per_page": COMMENTS_PAGE_SIZE, "since": since});
        let mut page: Page<IssueCommentResponse> =
            client.get(url, Some(&params)).await.context("error listing issue comments")?;

        // Look for the comments posted by this application that end with the marker,
        // moving on to the next page until one is found
        loop {
            // Check the comments in the page
            let has_marker = page.items.iter().any(|comment| {
                let is_app_comment =
                    comment.performed_via_github_app.as_ref().is_some_and(|app| app.id == self.app_id);
                is_app_comment && comment.body.as_deref().is_some_and(|body| ends_with_marker(body, marker))
            });
            if has_marker {
                return Ok(true);
            }

            // Move on to the next page, if any
            match client.get_page(&page.next).await.context("error listing issue comments")? {
                Some(next_page) => page = next_page,
                None => return Ok(false),
            }
        }
    }

    /// [`GH::has_discussion_with_marker`].
    async fn has_discussion_with_marker(
        &self,
        inst_id: u64,
        owner: &str,
        repo: &str,
        category: &str,
        marker: &str,
        since: OffsetDateTime,
    ) -> Result<bool> {
        // Get the category the discussions are in
        let client = self.retrying_client.installation(InstallationId(inst_id))?;
        let (_, category_id) = fetch_announcement_repo(&client, owner, repo, category).await?;

        // Look for the discussions this application created that end with the marker,
        // from the newest one, until the ones created before the time provided are
        // reached
        let mut after = None;
        loop {
            // Get the next page of discussions
            let response: announcement_discussions_query::ResponseData = client
                .graphql(&AnnouncementDiscussionsQuery::build_query(
                    announcement_discussions_query::Variables {
                        owner: owner.to_string(),
                        repo: repo.to_string(),
                        category_id: category_id.clone(),
                        after,
                    },
                ))
                .await
                .map_err(map_discussion_error)
                .context("error listing announcement discussions")?;
            let discussions = response
                .repository
                .context("repository not found listing announcement discussions")?
                .discussions;

            // Check the discussions in the page
            for discussion in discussions.nodes.into_iter().flatten().flatten() {
                let created_at = OffsetDateTime::parse(&discussion.created_at, &Rfc3339)
                    .context("invalid creation time of announcement discussion")?;
                if created_at < since {
                    return Ok(false);
                }
                if discussion.viewer_did_author && ends_with_marker(&discussion.body, marker) {
                    return Ok(true);
                }
            }

            // Move on to the next page, if any
            if !discussions.page_info.has_next_page {
                return Ok(false);
            }
            let cursor = discussions
                .page_info
                .end_cursor
                .context("next page of announcement discussions without cursor")?;
            after = Some(cursor);
        }
    }

    /// [`GH::is_check_required`].
    async fn is_check_required(&self, inst_id: u64, owner: &str, repo: &str, branch: &str) -> Result<bool> {
        // Get the branch, encoding its name for the URL path
        let client = self.retrying_client.installation(InstallationId(inst_id))?;
        let branch = utf8_percent_encode(branch, BRANCH_ENCODE_SET);
        let url = format!("/repos/{owner}/{repo}/branches/{branch}");
        let branch: Branch = client.get(url, None::<&()>).await?;

        // Check if the GitVote check is among the ones its protection requires
        let is_check_required = branch
            .protection
            .and_then(|protection| protection.required_status_checks)
            .is_some_and(|required_checks| {
                required_checks.contexts.iter().any(|context| context == GITVOTE_CHECK_NAME)
            });
        Ok(is_check_required)
    }

    /// [`GH::post_comment`].
    async fn post_comment(
        &self,
        inst_id: u64,
        owner: &str,
        repo: &str,
        issue_number: i64,
        body: &str,
    ) -> Result<CommentId> {
        let client = self.single_attempt_client.installation(InstallationId(inst_id))?;
        let comment = client.issues(owner, repo).create_comment(issue_number as u64, body).await?;
        Ok(comment.id.0 as i64)
    }

    /// [`GH::remove_label`].
    async fn remove_label(
        &self,
        inst_id: u64,
        owner: &str,
        repo: &str,
        issue_number: i64,
        label: &str,
    ) -> Result<()> {
        // Remove the label, a label the issue does not have is already removed
        let client = self.retrying_client.installation(InstallationId(inst_id))?;
        match client.issues(owner, repo).remove_label(issue_number as u64, label).await {
            Ok(_) => Ok(()),
            Err(octocrab::Error::GitHub { source, backtrace: _ })
                if source.message == "Label does not exist" =>
            {
                Ok(())
            }
            Err(err) => Err(err.into()),
        }
    }

    /// [`GH::user_is_collaborator`].
    async fn user_is_collaborator(&self, inst_id: u64, owner: &str, repo: &str, user: &str) -> Result<bool> {
        // Ask GitHub whether the user is a collaborator
        let client = self.retrying_client.installation(InstallationId(inst_id))?;
        let url = format!("/repos/{owner}/{repo}/collaborators/{user}");
        let resp = client._get(url).await?;

        // Only a 404 means the user is not a collaborator
        match resp.status() {
            StatusCode::NO_CONTENT => Ok(true),
            StatusCode::NOT_FOUND => Ok(false),
            status => {
                map_github_error(resp).await?;
                bail!("unexpected status checking if user is collaborator: {status}")
            }
        }
    }
}

impl TryFrom<(Option<&HeaderValue>, &[u8])> for Event {
    type Error = EventError;

    /// Parses the event body provided according to the event type in the header.
    fn try_from((event_name, event_body): (Option<&HeaderValue>, &[u8])) -> Result<Self, Self::Error> {
        match event_name {
            Some(event_name) => match event_name.as_bytes() {
                b"issue_comment" => {
                    let event: IssueCommentEvent = serde_json::from_slice(event_body)
                        .map_err(|err| EventError::InvalidBody(err.to_string()))?;
                    Ok(Event::IssueComment(event))
                }
                b"issues" => {
                    let event: IssueEvent = serde_json::from_slice(event_body)
                        .map_err(|err| EventError::InvalidBody(err.to_string()))?;
                    Ok(Event::Issue(event))
                }
                b"pull_request" => {
                    let event: PullRequestEvent = serde_json::from_slice(event_body)
                        .map_err(|err| EventError::InvalidBody(err.to_string()))?;
                    Ok(Event::PullRequest(event))
                }
                _ => Err(EventError::UnsupportedEvent),
            },
            None => Err(EventError::MissingHeader),
        }
    }
}

// Helpers.

/// Checks if the provided error is a 404 error from GitHub. The status is what matters,
/// as the message varies (for example, it is different for empty repositories).
pub(crate) fn is_not_found_error(err: &Error) -> bool {
    if let Some(octocrab::Error::GitHub { source, backtrace: _ }) = err.downcast_ref::<octocrab::Error>()
        && source.status_code == StatusCode::NOT_FOUND
    {
        return true;
    }
    false
}

/// Returns the builder of the GitHub client for requests that are safe to repeat.
pub(crate) fn retrying_client_builder() -> ClientBuilder {
    client_builder().add_retry_config(RetryConfig::Simple(REQUEST_MAX_RETRIES))
}

/// Returns the builder of the GitHub client for requests that must be sent only once.
pub(crate) fn single_attempt_client_builder() -> ClientBuilder {
    client_builder().add_retry_config(RetryConfig::None)
}

/// Splits a repository's full name and returns the owner and the repo name as a tuple.
/// Any parts after the repo name are ignored.
pub(crate) fn split_full_name(full_name: &str) -> (&str, &str) {
    let mut parts = full_name.split('/');
    let owner = parts.next().expect("split always returns at least one part");
    let repo = parts.next().expect("repository full names from GitHub have the owner/repo format");
    (owner, repo)
}

/// Returns the builder of a GitHub client, with the timeouts for API requests set.
fn client_builder() -> ClientBuilder {
    Octocrab::builder()
        .set_connect_timeout(Some(CONNECT_TIMEOUT))
        .set_read_timeout(Some(READ_TIMEOUT))
        .set_write_timeout(Some(WRITE_TIMEOUT))
}

/// Returns whether the body provided ends with the marker given, ignoring trailing
/// whitespace. The templates render the marker last, after any text users control, like
/// an issue title, so a marker found anywhere else in the body does not count.
fn ends_with_marker(body: &str, marker: &str) -> bool {
    body.trim_end().ends_with(marker)
}

/// Returns the identifiers of the repository provided and of its discussion category
/// given. If either does not exist, or GitHub rejects the query for a reason that
/// retrying will not solve, the error returned is a [`DiscussionRejectedError`].
async fn fetch_announcement_repo(
    client: &Octocrab,
    owner: &str,
    repo: &str,
    category: &str,
) -> Result<(String, String)> {
    // Query the repository and its discussion category
    let response: announcement_repo_query::ResponseData = client
        .graphql(&AnnouncementRepoQuery::build_query(
            announcement_repo_query::Variables {
                owner: owner.to_string(),
                repo: repo.to_string(),
                category: category.to_string(),
            },
        ))
        .await
        .map_err(map_discussion_error)?;

    // Both of them must exist
    let Some((repository_id, category_id)) = response.repository.and_then(|repository| {
        let discussion_category = repository.discussion_category?;
        Some((repository.id, discussion_category.id))
    }) else {
        return Err(DiscussionRejectedError(format_err!(
            "something went wrong while fetching repository details for announcement"
        ))
        .into());
    };
    Ok((repository_id, category_id))
}

/// Returns the head commit of the pull request provided.
async fn fetch_pr_head_sha(client: &Octocrab, owner: &str, repo: &str, pr_number: i64) -> Result<String> {
    let pr = client.pulls(owner, repo).get(pr_number as u64).await?;
    Ok(pr.head.context("pull request response missing head")?.sha)
}

/// Replaces an error creating a discussion that retrying will not solve with a
/// [`DiscussionRejectedError`]. Anything else may be transient, so it is left as is.
fn map_discussion_error(err: octocrab::Error) -> Error {
    if let octocrab::Error::Graphql { source, backtrace: _ } = &err
        && source
            .0
            .iter()
            .any(|error| DISCUSSION_REJECTED_MESSAGES.iter().any(|message| error.message.contains(message)))
    {
        return DiscussionRejectedError(err.into()).into();
    }
    err.into()
}

/// Replaces a 404 error from GitHub with the error provided.
fn map_not_found_error(err: octocrab::Error, not_found_err: impl Into<Error>) -> Error {
    let err = Error::from(err);
    if is_not_found_error(&err) {
        not_found_err.into()
    } else {
        err
    }
}

/// Creates a check run for the commit provided.
async fn post_check_run(
    client: &Octocrab,
    owner: &str,
    repo: &str,
    head_sha: &str,
    check_details: &CheckDetails,
) -> Result<()> {
    // Prepare the check run
    let url = format!("/repos/{owner}/{repo}/check-runs");
    let mut body = json!({
        "head_sha": head_sha,
        "name": GITVOTE_CHECK_NAME,
        "output": {
            "summary": check_details.summary,
            "title": check_details.summary,
        },
        "status": check_details.status,
    });
    if let Some(conclusion) = &check_details.conclusion {
        body["conclusion"] = json!(conclusion);
    }

    // Create it
    let _: Value = client.post(url, Some(&body)).await?;
    Ok(())
}
