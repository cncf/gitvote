use crate::cfg::CfgProfile;
use anyhow::Result;
use async_trait::async_trait;
use axum::http::HeaderValue;
#[cfg(test)]
use mockall::automock;
use octocrab::{models::InstallationId, Octocrab, Page};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use thiserror::Error;

/// GitHub API base url.
const GITHUB_API_URL: &str = "https://api.github.com";

/// Configuration file name.
const CONFIG_FILE: &str = ".gitvote.yml";

/// Repository where the organization wide config file should be located.
const ORG_CONFIG_REPO: &str = ".github";

/// Name used for the check run in GitHub.
const GITVOTE_CHECK_NAME: &str = "GitVote";

/// Type alias to represent a GH trait object.
pub(crate) type DynGH = Arc<dyn GH + Send + Sync>;

/// Type alias to represent a comment id.
type CommentId = i64;

/// Type alias to represent a team slug.
pub(crate) type TeamSlug = String;

/// Type alias to represent a username.
pub(crate) type UserName = String;

/// Trait that defines some operations a GH implementation must support.
#[async_trait]
#[cfg_attr(test, automock)]
pub(crate) trait GH {
    /// Create a check run for the head commit in the provided pull request.
    async fn create_check_run(
        &self,
        inst_id: u64,
        owner: &str,
        repo: &str,
        issue_number: i64,
        check_details: CheckDetails,
    ) -> Result<()>;

    /// Get all users allowed to vote on a given vote.
    async fn get_allowed_voters(
        &self,
        inst_id: u64,
        cfg: &CfgProfile,
        owner: &str,
        repo: &str,
        org: &Option<String>,
    ) -> Result<Vec<UserName>>;

    /// Get all repository collaborators.
    #[allow(dead_code)]
    async fn get_collaborators(
        &self,
        inst_id: u64,
        owner: &str,
        repo: &str,
    ) -> Result<Vec<UserName>>;

    /// Get reactions for the provided comment.
    async fn get_comment_reactions(
        &self,
        inst_id: u64,
        owner: &str,
        repo: &str,
        comment_id: i64,
    ) -> Result<Vec<Reaction>>;

    /// Get configuration file.
    async fn get_config_file(&self, inst_id: u64, owner: &str, repo: &str) -> Option<String>;

    /// Get all members of the provided team.
    #[allow(dead_code)]
    async fn get_team_members(&self, inst_id: u64, org: &str, team: &str) -> Result<Vec<UserName>>;

    /// Verify if the GitVote check is required via branch protection in the
    /// repository's branch provided.
    async fn is_check_required(
        &self,
        inst_id: u64,
        owner: &str,
        repo: &str,
        branch: &str,
    ) -> Result<bool>;

    /// Post the comment provided in the repository's issue given.
    async fn post_comment(
        &self,
        inst_id: u64,
        owner: &str,
        repo: &str,
        issue_number: i64,
        body: &str,
    ) -> Result<CommentId>;

    /// Check if the user given is a collaborator of the provided repository.
    async fn user_is_collaborator(
        &self,
        inst_id: u64,
        owner: &str,
        repo: &str,
        user: &str,
    ) -> Result<bool>;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CheckDetails {
    pub status: String,
    pub conclusion: Option<String>,
    pub summary: String,
}

/// GH implementation backed by the GitHub API.
pub(crate) struct GHApi {
    app_client: Octocrab,
}

impl GHApi {
    /// Create a new GHApi instance.
    pub(crate) fn new(app_client: Octocrab) -> Self {
        Self { app_client }
    }
}

#[async_trait]
impl GH for GHApi {
    async fn create_check_run(
        &self,
        inst_id: u64,
        owner: &str,
        repo: &str,
        issue_number: i64,
        check_details: CheckDetails,
    ) -> Result<()> {
        let client = self.app_client.installation(InstallationId(inst_id));
        let pr = client.pulls(owner, repo).get(issue_number as u64).await?;
        let url = format!("{}/repos/{}/{}/check-runs", GITHUB_API_URL, owner, repo);
        let mut body = json!({
            "name": GITVOTE_CHECK_NAME,
            "head_sha": pr.head.sha,
            "status": check_details.status,
            "output": {
                "title": check_details.summary,
                "summary": check_details.summary,
            }
        });
        if let Some(conclusion) = check_details.conclusion {
            body["conclusion"] = json!(conclusion);
        };
        let _: Value = client.post(url, Some(&body)).await?; // Do not remove let _: Value
        Ok(())
    }

    async fn get_allowed_voters(
        &self,
        inst_id: u64,
        cfg: &CfgProfile,
        owner: &str,
        repo: &str,
        org: &Option<String>,
    ) -> Result<Vec<UserName>> {
        let mut allowed_voters: Vec<UserName> = vec![];

        // Get allowed voters from configuration
        if let Some(cfg_allowed_voters) = &cfg.allowed_voters {
            // Teams
            if org.is_some() {
                if let Some(teams) = &cfg_allowed_voters.teams {
                    for team in teams {
                        if let Ok(members) = self
                            .get_team_members(inst_id, org.as_ref().unwrap().as_str(), team)
                            .await
                        {
                            for user in members {
                                if !allowed_voters.contains(&user) {
                                    allowed_voters.push(user.to_owned());
                                }
                            }
                        }
                    }
                }
            }

            // Users
            if let Some(users) = &cfg_allowed_voters.users {
                for user in users {
                    if !allowed_voters.contains(user) {
                        allowed_voters.push(user.to_owned());
                    }
                }
            }
        }

        // If no allowed voters can be found in the configuration, all
        // repository collaborators are allowed to vote
        if allowed_voters.is_empty() {
            return self.get_collaborators(inst_id, owner, repo).await;
        }

        Ok(allowed_voters)
    }

    async fn get_collaborators(
        &self,
        inst_id: u64,
        owner: &str,
        repo: &str,
    ) -> Result<Vec<UserName>> {
        let client = self.app_client.installation(InstallationId(inst_id));
        let url = format!("{}/repos/{}/{}/collaborators", GITHUB_API_URL, owner, repo);
        let first_page: Page<User> = client.get(url, None::<&()>).await?;
        let collaborators = client
            .all_pages(first_page)
            .await?
            .into_iter()
            .map(|u| u.login)
            .collect();
        Ok(collaborators)
    }

    async fn get_comment_reactions(
        &self,
        inst_id: u64,
        owner: &str,
        repo: &str,
        comment_id: i64,
    ) -> Result<Vec<Reaction>> {
        let client = self.app_client.installation(InstallationId(inst_id));
        let url = format!(
            "{}/repos/{}/{}/issues/comments/{}/reactions",
            GITHUB_API_URL, owner, repo, comment_id
        );
        let first_page: Page<Reaction> = client.get(url, None::<&()>).await?;
        let reactions = client.all_pages(first_page).await?;
        Ok(reactions)
    }

    async fn get_config_file(&self, inst_id: u64, owner: &str, repo: &str) -> Option<String> {
        let client = self.app_client.installation(InstallationId(inst_id));

        // Try to get the config file from the repository. Otherwise try
        // getting the organization wide config file in the .github repo.
        let mut content: Option<String> = None;
        for repo in &[repo, ORG_CONFIG_REPO] {
            match client
                .repos(owner, *repo)
                .get_content()
                .path(CONFIG_FILE)
                .send()
                .await
            {
                Ok(resp) => {
                    if resp.items.len() == 1 {
                        content = resp.items[0].decoded_content();
                        break;
                    }
                }
                Err(_) => continue,
            }
        }

        content
    }

    async fn get_team_members(&self, inst_id: u64, org: &str, team: &str) -> Result<Vec<UserName>> {
        let client = self.app_client.installation(InstallationId(inst_id));
        let url = format!("{}/orgs/{}/teams/{}/members", GITHUB_API_URL, org, team);
        let first_page: Page<User> = client.get(url, None::<&()>).await?;
        let members: Vec<UserName> = client
            .all_pages(first_page)
            .await?
            .into_iter()
            .map(|u| u.login)
            .collect();
        Ok(members)
    }

    async fn is_check_required(
        &self,
        inst_id: u64,
        owner: &str,
        repo: &str,
        branch: &str,
    ) -> Result<bool> {
        let client = self.app_client.installation(InstallationId(inst_id));
        let url = format!(
            "{}/repos/{}/{}/branches/{}",
            GITHUB_API_URL, owner, repo, branch
        );
        let branch: Branch = client.get(url, None::<&()>).await?;
        let is_check_required = if let Some(required_checks) = branch
            .protection
            .and_then(|protection| protection.required_status_checks)
        {
            required_checks
                .contexts
                .iter()
                .any(|context| context == GITVOTE_CHECK_NAME)
        } else {
            false
        };
        Ok(is_check_required)
    }

    async fn post_comment(
        &self,
        inst_id: u64,
        owner: &str,
        repo: &str,
        issue_number: i64,
        body: &str,
    ) -> Result<i64> {
        let client = self.app_client.installation(InstallationId(inst_id));
        let comment = client
            .issues(owner, repo)
            .create_comment(issue_number as u64, body)
            .await?;
        Ok(comment.id.0 as i64)
    }

    async fn user_is_collaborator(
        &self,
        inst_id: u64,
        owner: &str,
        repo: &str,
        user: &str,
    ) -> Result<bool> {
        let client = self.app_client.installation(InstallationId(inst_id));
        let url = format!(
            "{}/repos/{}/{}/collaborators/{}",
            GITHUB_API_URL, owner, repo, user,
        );
        let resp = client._get(url, None::<&()>).await?;
        if resp.status() == StatusCode::NO_CONTENT {
            return Ok(true);
        }
        Ok(false)
    }
}

/// Represents a GitHub event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum Event {
    Issue(IssueEvent),
    IssueComment(IssueCommentEvent),
    PullRequest(PullRequestEvent),
}

impl TryFrom<(Option<&HeaderValue>, &[u8])> for Event {
    type Error = EventError;

    fn try_from(
        (event_name, event_body): (Option<&HeaderValue>, &[u8]),
    ) -> Result<Self, Self::Error> {
        match event_name {
            Some(event_name) => match event_name.as_bytes() {
                b"issues" => {
                    let event: IssueEvent = serde_json::from_slice(event_body)
                        .map_err(|err| EventError::InvalidBody(err.to_string()))?;
                    Ok(Event::Issue(event))
                }
                b"issue_comment" => {
                    let event: IssueCommentEvent = serde_json::from_slice(event_body)
                        .map_err(|err| EventError::InvalidBody(err.to_string()))?;
                    Ok(Event::IssueComment(event))
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

/// Errors that may occur while creating a new event instance.
#[derive(Debug, Error, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum EventError {
    #[error("event header missing")]
    MissingHeader,
    #[error("unsupported event")]
    UnsupportedEvent,
    #[error("invalid body: {0}")]
    InvalidBody(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct IssueEvent {
    pub action: IssueEventAction,
    pub installation: Installation,
    pub issue: Issue,
    pub repository: Repository,
    pub organization: Option<Organization>,
    pub sender: User,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum IssueEventAction {
    Opened,
    Edited,
    Deleted,
    Pinned,
    Unpinned,
    Closed,
    Reopened,
    Assigned,
    Unassigned,
    Labeled,
    Unlabeled,
    Locked,
    Unlocked,
    Transferred,
    Milestoned,
    Demilestoned,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct IssueCommentEvent {
    pub action: IssueCommentEventAction,
    pub comment: Comment,
    pub installation: Installation,
    pub issue: Issue,
    pub repository: Repository,
    pub organization: Option<Organization>,
    pub sender: User,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum IssueCommentEventAction {
    Created,
    Deleted,
    Edited,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PullRequestEvent {
    pub action: PullRequestEventAction,
    pub installation: Installation,
    pub pull_request: PullRequest,
    pub repository: Repository,
    pub organization: Option<Organization>,
    pub sender: User,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PullRequestEventAction {
    Assigned,
    AutoMergeDisabled,
    AutoMergeEnabled,
    Closed,
    ConvertedToDraft,
    Edited,
    Labeled,
    Locked,
    Opened,
    ReadyForReview,
    Reopened,
    ReviewRequestRemoved,
    ReviewRequested,
    Synchronize,
    Unassigned,
    Unlabeled,
    Unlocked,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Comment {
    pub id: i64,
    pub body: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct User {
    pub login: UserName,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Installation {
    pub id: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Issue {
    pub id: i64,
    pub number: i64,
    pub title: String,
    pub body: Option<String>,
    pub pull_request: Option<PullRequestInIssue>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Repository {
    pub full_name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Organization {
    pub login: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PullRequest {
    pub id: i64,
    pub number: i64,
    pub title: String,
    pub body: Option<String>,
    pub base: PullRequestBase,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PullRequestBase {
    #[serde(rename = "ref")]
    pub reference: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PullRequestInIssue {
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Reaction {
    pub user: User,
    pub content: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Branch {
    pub name: String,
    pub protection: Option<Protection>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Protection {
    pub required_status_checks: Option<RequiredStatusCheck>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct RequiredStatusCheck {
    pub contexts: Vec<String>,
}

/// Helper function that splits a repository's full name and returns the owner
/// and the repo name as a tuple.
pub(crate) fn split_full_name(full_name: &str) -> (&str, &str) {
    let mut parts = full_name.split('/');
    (parts.next().unwrap(), parts.next().unwrap())
}
