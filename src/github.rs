use crate::votes::CfgProfile;
use anyhow::Result;
use async_trait::async_trait;
use axum::http::HeaderValue;
#[cfg(test)]
use mockall::automock;
use octocrab::{models::InstallationId, Octocrab, Page};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// GitHub API base url.
pub(crate) const GITHUB_API_URL: &str = "https://api.github.com";

/// Configuration file name.
pub(crate) const CONFIG_FILE: &str = ".gitvote.yml";

/// Repository where the organization wide config file should be located.
pub(crate) const ORG_CONFIG_REPO: &str = ".github";

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
    async fn get_team_members(&self, inst_id: u64, org: &str, team: &str) -> Result<Vec<UserName>>;

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
    /// Get all users allowed to vote on a given vote.
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

    /// Get all repository collaborators.
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

    /// Get reactions for the provided comment.
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

    /// Get configuration file.
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

    /// Get all members of the provided team.
    async fn get_team_members(&self, inst_id: u64, org: &str, team: &str) -> Result<Vec<UserName>> {
        let client = self.app_client.installation(InstallationId(inst_id));
        let url = format!("{}/orgs/{}/teams/{}/members", GITHUB_API_URL, org, team);
        let first_page: Page<User> = client.get(url, None::<&()>).await?;
        let members = client
            .all_pages(first_page)
            .await?
            .into_iter()
            .map(|u| u.login)
            .collect();
        Ok(members)
    }

    /// Post the comment provided in the repository's issue/pr given.
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

    /// Check if the user given is a collaborator of the provided repository.
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

/// Event triggered with activity related to an issue or pull request comment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct IssueCommentEvent {
    pub action: IssueCommentEventAction,
    pub comment: Comment,
    pub installation: Installation,
    pub issue: Issue,
    pub repository: Repository,
    pub organization: Option<Organization>,
}

/// Action performed on the comment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum IssueCommentEventAction {
    Created,
    Deleted,
    Edited,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Comment {
    pub id: i64,
    pub body: Option<String>,
    pub user: User,
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
    pub pull_request: Option<PullRequest>,
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
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Reaction {
    pub user: User,
    pub content: String,
}
