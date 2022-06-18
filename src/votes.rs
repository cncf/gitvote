use crate::{
    github::{IssueCommentEvent, IssueCommentEventAction, Reaction, User},
    tmpl,
};
use anyhow::{Context, Result};
use askama::Template;
use config::Config;
use deadpool_postgres::{Pool as DbPool, Transaction};
use futures::future::{self, JoinAll};
use lazy_static::lazy_static;
use octocrab::{models::InstallationId, Octocrab, Page};
use regex::Regex;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc, time::Duration};
use time::OffsetDateTime;
use tokio::{
    sync::broadcast::{self, error::TryRecvError},
    task::JoinHandle,
    time::sleep,
};
use tokio_postgres::types::Json;
use tracing::{debug, error};
use uuid::Uuid;

/// GitHub API base url.
const GITHUB_API_URL: &str = "https://api.github.com";

/// Vote options.
const IN_FAVOR: &str = "In favor";
const AGAINST: &str = "Against";
const ABSTAIN: &str = "Abstain";

/// Vote options reactions.
const IN_FAVOR_REACTION: &str = "+1";
const AGAINST_REACTION: &str = "-1";
const ABSTAIN_REACTION: &str = "eyes";

/// Vote configuration file name.
pub const VOTE_CONFIG_FILE: &str = ".gitvote.yml";

/// Number of commands handlers workers.
const COMMANDS_HANDLERS_WORKERS: usize = 3;

/// Number of votes closers workers.
const VOTES_CLOSERS_WORKERS: usize = 1;

/// Amount of time the votes closer will sleep when there are no pending votes
/// to close.
const VOTES_CLOSER_PAUSE_ON_NONE: Duration = Duration::from_secs(15);

/// Amount of time the votes closer will sleep when something goes wrong.
const VOTES_CLOSER_PAUSE_ON_ERROR: Duration = Duration::from_secs(30);

lazy_static! {
    /// Regex used to detect commands in issues/prs comments.
    pub(crate) static ref CMD: Regex = Regex::new(r#"(?m)^/(vote)-?([a-zA-Z0-9]*)\s*$"#).expect("invalid CMD regexp");
}

/// Vote information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Vote {
    vote_id: Uuid,
    vote_comment_id: i64,
    created_at: OffsetDateTime,
    created_by: String,
    ends_at: OffsetDateTime,
    closed: bool,
    closed_at: Option<OffsetDateTime>,
    cfg: CfgProfile,
    installation_id: i64,
    issue_id: i64,
    issue_number: i64,
    is_pull_request: bool,
    repository_full_name: String,
    organization: Option<String>,
    results: Option<Results>,
}

/// Vote configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(transparent)]
pub(crate) struct Cfg {
    pub profiles: HashMap<String, CfgProfile>,
}

/// Vote configuration profile.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct CfgProfile {
    #[serde(with = "humantime_serde")]
    pub duration: Duration,
    pub pass_threshold: f64,
    pub allowed_voters: Option<AllowedVoters>,
}

/// List of teams and users allowed to vote.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct AllowedVoters {
    pub teams: Option<Vec<String>>,
    pub users: Option<Vec<String>>,
}

impl CfgProfile {
    /// Get the vote configuration profile requested from the config file in
    /// the repository if available.
    pub(crate) async fn get(
        gh: &Octocrab,
        owner: &str,
        repo: &str,
        profile: Option<String>,
    ) -> Result<Option<Self>> {
        // Fetch configuration file
        let response = gh
            .repos(owner, repo)
            .get_content()
            .path(VOTE_CONFIG_FILE)
            .send()
            .await?;
        if response.items.len() != 1 {
            return Ok(None);
        }

        // Return profile requested if exists
        match &response.items[0].decoded_content() {
            Some(content) => {
                let mut cfg: Cfg = serde_yaml::from_str(content)?;
                let profile = profile.unwrap_or_else(|| "default".to_string());
                match cfg.profiles.remove(&profile) {
                    Some(profile) => Ok(Some(profile)),
                    None => Ok(None),
                }
            }
            None => Ok(None),
        }
    }
}

/// Vote results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Results {
    pub passed: bool,
    pub in_favor_percentage: f64,
    pub pass_threshold: f64,
    pub in_favor: i64,
    pub against: i64,
    pub abstain: i64,
    pub not_voted: i64,
    pub voters: HashMap<String, String>,
}

/// Represents a command to be executed, usually created from a GitHub event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum Command {
    CreateVote {
        profile: Option<String>,
        event: IssueCommentEvent,
    },
}

impl Command {
    /// Try to create a new command from an issue comment event.
    pub(crate) fn from_event(event: IssueCommentEvent) -> Option<Self> {
        // Only events with action created are supported at the moment
        if event.action != IssueCommentEventAction::Created {
            return None;
        }

        // Extract command from comment body
        if let Some(content) = &event.comment.body {
            if let Some(captures) = CMD.captures(content) {
                let cmd = captures.get(1).unwrap().as_str();
                let profile = match captures.get(2).unwrap().as_str() {
                    "" => None,
                    profile => Some(profile.to_string()),
                };
                match cmd {
                    "vote" => return Some(Command::CreateVote { profile, event }),
                    _ => return None,
                }
            }
        }
        None
    }
}

/// A votes processor is in charge of creating the requested votes, stopping
/// them at the scheduled time and publishing results, etc.
pub(crate) struct Processor {
    db: DbPool,
    gh_app: Octocrab,
}

impl Processor {
    /// Create a new votes processor instance.
    pub(crate) fn new(cfg: Arc<Config>, db: DbPool) -> Result<Arc<Self>> {
        // Setup application GitHub client
        let app_id = cfg.get_int("github.appID")? as u64;
        let app_private_key = cfg.get_string("github.appPrivateKey")?;
        let app_private_key = jsonwebtoken::EncodingKey::from_rsa_pem(app_private_key.as_bytes())?;
        let gh_app = Octocrab::builder()
            .app(app_id.into(), app_private_key)
            .build()?;

        // Setup votes processor and return it
        let processor = Arc::new(Self { db, gh_app });
        Ok(processor)
    }

    /// Start votes processor.
    pub(crate) fn start(
        self: Arc<Self>,
        cmds_rx: async_channel::Receiver<Command>,
        stop_tx: broadcast::Sender<()>,
    ) -> JoinAll<JoinHandle<()>> {
        let num_workers = COMMANDS_HANDLERS_WORKERS + VOTES_CLOSERS_WORKERS;
        let mut workers_handles = Vec::with_capacity(num_workers);

        // Launch commands handler workers
        for _ in 0..COMMANDS_HANDLERS_WORKERS {
            let handle = self
                .clone()
                .commands_handler(cmds_rx.clone(), stop_tx.subscribe());
            workers_handles.push(handle);
        }

        // Launch votes closer workers
        for _ in 0..VOTES_CLOSERS_WORKERS {
            let handle = self.clone().votes_closer(stop_tx.subscribe());
            workers_handles.push(handle);
        }

        future::join_all(workers_handles)
    }

    /// Worker that receives commands from the queue and executes them.
    /// Commands are added to the queue when certain events from GitHub are
    /// received on the webhook endpoint.
    fn commands_handler(
        self: Arc<Self>,
        cmds_rx: async_channel::Receiver<Command>,
        mut stop_rx: broadcast::Receiver<()>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    // Pick next command from the queue and process it
                    Ok(cmd) = cmds_rx.recv() => {
                        if let Err(err) = match cmd {
                            Command::CreateVote { profile, event } => {
                                self.create_vote(profile, event).await.context("error creating vote")
                            }
                        } {
                            error!("{:#?}", err);
                        }

                        // Exit if the votes processor has been asked to stop
                        if let Some(TryRecvError::Closed) = stop_rx.try_recv().err() {
                            break;
                        }
                    }

                    // Exit if the votes processor has been asked to stop
                    _ = stop_rx.recv() => {
                        break
                    }
                }
            }
        })
    }

    /// Worker that periodically checks the database for votes that should be
    /// closed and closes them.
    fn votes_closer(self: Arc<Self>, mut stop_rx: broadcast::Receiver<()>) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                // Close any pending finished votes
                match self.close_finished_vote().await {
                    Ok(Some(())) => {
                        // One pending finished vote was closed, try to close
                        // another one immediately
                    }
                    Ok(None) => tokio::select! {
                        // No pending finished votes were found, pause unless
                        // we've been asked to stop
                        _ = sleep(VOTES_CLOSER_PAUSE_ON_NONE) => {},
                        _ = stop_rx.recv() => break,
                    },
                    Err(err) => {
                        error!("error closing finished vote: {:#?}", err);
                        tokio::select! {
                            _ = sleep(VOTES_CLOSER_PAUSE_ON_ERROR) => {},
                            _ = stop_rx.recv() => break,
                        }
                    }
                }

                // Exit if the votes processor has been asked to stop
                if let Some(TryRecvError::Closed) = stop_rx.try_recv().err() {
                    break;
                }
            }
        })
    }

    /// Calculate the results of the vote created at the comment provided.
    async fn calculate_vote_results(
        &self,
        gh: &Octocrab,
        cfg: &CfgProfile,
        owner: &str,
        repo: &str,
        vote: &Vote,
    ) -> Result<Results> {
        // Get vote comment reactions (aka votes)
        let reactions = self
            .get_comment_reactions(gh, owner, repo, vote.vote_comment_id)
            .await?;

        // Get list of allowed voters (users with binding votes)
        let allowed_voters = self
            .get_allowed_voters(cfg, gh, owner, repo, &vote.organization)
            .await?;

        // Count votes
        let (mut in_favor, mut against, mut abstain) = (0, 0, 0);
        let mut voters: HashMap<String, String> = HashMap::with_capacity(allowed_voters.len());
        let mut multiple_options_voters: Vec<String> = Vec::new();
        for reaction in reactions {
            let user = reaction.user.login;

            // We only count the votes of users with a binding vote
            if !allowed_voters.contains(&user) {
                continue;
            }

            // Do not count votes of users voting for multiple options
            if multiple_options_voters.contains(&user) {
                continue;
            }
            if voters.contains_key(&user) {
                // User has already voted (multiple options voter)
                multiple_options_voters.push(user.clone());
                voters.remove(&user);
            }

            // Track binding votes
            match reaction.content.as_str() {
                IN_FAVOR_REACTION => {
                    in_favor += 1;
                    voters.insert(user.clone(), IN_FAVOR.to_string());
                }
                AGAINST_REACTION => {
                    against += 1;
                    voters.insert(user.clone(), AGAINST.to_string());
                }
                ABSTAIN_REACTION => {
                    abstain += 1;
                    voters.insert(user.clone(), ABSTAIN.to_string());
                }
                _ => {
                    // Ignore other reactions
                }
            }
        }

        // Prepare results and return them
        let in_favor_percentage = in_favor as f64 / allowed_voters.len() as f64 * 100.0;
        let passed = in_favor_percentage >= cfg.pass_threshold;
        let not_voted = allowed_voters
            .iter()
            .filter(|user| !voters.contains_key(*user))
            .count() as i64;

        Ok(Results {
            passed,
            in_favor_percentage,
            pass_threshold: cfg.pass_threshold,
            in_favor,
            against,
            abstain,
            not_voted,
            voters,
        })
    }

    /// Close any pending finished vote.
    async fn close_finished_vote(&self) -> Result<Option<()>> {
        // Get pending finished vote (if any) from database
        let mut db = self.db.get().await?;
        let tx = db.transaction().await?;
        let vote = match self.get_pending_finished_vote(&tx).await? {
            Some(vote) => vote,
            None => return Ok(None),
        };

        // Calculate results
        let inst_id = InstallationId(vote.installation_id as u64);
        let gh_inst = self.gh_app.installation(inst_id);
        let (owner, repo) = split_full_name(&vote.repository_full_name);
        let results = self
            .calculate_vote_results(&gh_inst, &vote.cfg, owner, repo, &vote)
            .await?;

        // Store results in database
        self.store_vote_results(&tx, vote.vote_id, &results).await?;
        tx.commit().await?;

        // Post vote closed comment on the issue/pr
        let body = tmpl::VoteClosed::new(&results).render()?;
        self.post_comment(&gh_inst, owner, repo, vote.issue_number, body)
            .await?;

        debug!("vote {} closed", &vote.vote_id);
        Ok(Some(()))
    }

    /// Create a new vote.
    async fn create_vote(&self, profile: Option<String>, event: IssueCommentEvent) -> Result<()> {
        // Extract some information from event
        let issue_number = event.issue.number;
        let is_pull_request = event.issue.pull_request.is_some();
        let creator = &event.comment.user.login;
        let repo_full_name = &event.repository.full_name;
        let (owner, repo) = split_full_name(repo_full_name);

        // Setup installation GitHub client
        let inst_id = InstallationId(event.installation.id as u64);
        let gh_inst = self.gh_app.installation(inst_id);

        // Get vote configuration profile
        let cfg = match CfgProfile::get(&gh_inst, owner, repo, profile).await? {
            Some(md) => md,
            None => return Ok(()),
        };

        // Only repository collaborators can create votes
        if !self
            .user_is_collaborator(&gh_inst, owner, repo, creator)
            .await?
        {
            let body = tmpl::VoteRestricted::new(creator).render()?;
            self.post_comment(&gh_inst, owner, repo, issue_number, body)
                .await?;
            return Ok(());
        }

        // Only allow one vote open at the same time per issue/pr
        if self.has_vote_open(repo_full_name, issue_number).await? {
            let body = tmpl::VoteInProgress::new(creator, is_pull_request).render()?;
            self.post_comment(&gh_inst, owner, repo, issue_number, body)
                .await?;
            return Ok(());
        }

        // Post vote created comment on the issue/pr
        let body = tmpl::VoteCreated::new(&event, &cfg).render()?;
        let vote_comment_id = self
            .post_comment(&gh_inst, owner, repo, issue_number, body)
            .await?;

        // Store vote information in database
        let vote_id = self.store_vote(vote_comment_id, &cfg, &event).await?;

        debug!("vote {} created", &vote_id);
        Ok(())
    }

    /// Get all users allowed to vote on a given vote.
    async fn get_allowed_voters(
        &self,
        cfg: &CfgProfile,
        gh: &Octocrab,
        owner: &str,
        repo: &str,
        org: &Option<String>,
    ) -> Result<Vec<String>> {
        let mut allowed_voters: Vec<String> = vec![];

        // Get allowed voters from configuration
        if let Some(cfg_allowed_voters) = &cfg.allowed_voters {
            // Teams
            if org.is_some() {
                if let Some(teams) = &cfg_allowed_voters.teams {
                    for team in teams {
                        if let Ok(members) = self
                            .get_team_members(gh, org.as_ref().unwrap().as_str(), team)
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
            return self.get_collaborators(gh, owner, repo).await;
        }

        Ok(allowed_voters)
    }

    /// Get all repository collaborators.
    async fn get_collaborators(
        &self,
        gh: &Octocrab,
        owner: &str,
        repo: &str,
    ) -> Result<Vec<String>> {
        let url = format!("{}/repos/{}/{}/collaborators", GITHUB_API_URL, owner, repo);
        let first_page: Page<User> = gh.get(url, None::<&()>).await?;
        let collaborators = gh
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
        gh: &Octocrab,
        owner: &str,
        repo: &str,
        comment_id: i64,
    ) -> Result<Vec<Reaction>> {
        let url = format!(
            "{}/repos/{}/{}/issues/comments/{}/reactions",
            GITHUB_API_URL, owner, repo, comment_id
        );
        let first_page: Page<Reaction> = gh.get(url, None::<&()>).await?;
        let reactions = gh.all_pages(first_page).await?;
        Ok(reactions)
    }

    /// Get any pending finished vote.
    async fn get_pending_finished_vote(&self, tx: &Transaction<'_>) -> Result<Option<Vote>> {
        // Get pending finished vote from database (if any)
        let row = match tx
            .query_opt(
                "
                select
                    vote_id,
                    vote_comment_id,
                    created_at,
                    created_by,
                    ends_at,
                    closed,
                    closed_at,
                    cfg,
                    installation_id,
                    issue_id,
                    issue_number,
                    is_pull_request,
                    repository_full_name,
                    organization,
                    results
                from vote
                where current_timestamp > ends_at and closed = false
                for update of vote skip locked
                limit 1
                ",
                &[],
            )
            .await?
        {
            Some(row) => row,
            None => return Ok(None),
        };

        // Prepare vote and return it
        let Json(cfg): Json<CfgProfile> = row.get("cfg");
        let results: Option<Json<Results>> = row.get("results");
        let vote = Vote {
            vote_id: row.get("vote_id"),
            vote_comment_id: row.get("vote_comment_id"),
            created_at: row.get("created_at"),
            created_by: row.get("created_by"),
            ends_at: row.get("ends_at"),
            closed: row.get("closed"),
            closed_at: row.get("closed_at"),
            cfg,
            installation_id: row.get("installation_id"),
            issue_id: row.get("issue_id"),
            issue_number: row.get("issue_number"),
            is_pull_request: row.get("is_pull_request"),
            repository_full_name: row.get("repository_full_name"),
            organization: row.get("organization"),
            results: results.map(|Json(results)| results),
        };
        Ok(Some(vote))
    }

    /// Get all members of the provided team.
    async fn get_team_members(&self, gh: &Octocrab, org: &str, team: &str) -> Result<Vec<String>> {
        let url = format!("{}/orgs/{}/teams/{}/members", GITHUB_API_URL, org, team);
        let first_page: Page<User> = gh.get(url, None::<&()>).await?;
        let members = gh
            .all_pages(first_page)
            .await?
            .into_iter()
            .map(|u| u.login)
            .collect();
        Ok(members)
    }

    /// Check if the issue/pr provided already has a vote open.
    async fn has_vote_open(&self, repository_full_name: &str, issue_number: i64) -> Result<bool> {
        let db = self.db.get().await?;
        let row = db
            .query_one(
                "
                select exists (
                    select 1 from vote
                    where repository_full_name = $1::text
                    and issue_number = $2::bigint
                    and closed = false
                )
                ",
                &[&repository_full_name, &issue_number],
            )
            .await?;
        let vote_in_progress: bool = row.get(0);
        Ok(vote_in_progress)
    }

    /// Post the comment provided in the repository's issue given.
    async fn post_comment(
        &self,
        gh: &Octocrab,
        owner: &str,
        repo: &str,
        issue_number: i64,
        body: impl AsRef<str>,
    ) -> Result<i64> {
        let comment = gh
            .issues(owner, repo)
            .create_comment(issue_number as u64, body)
            .await?;
        Ok(comment.id.0 as i64)
    }

    /// Store the vote provided in the database.
    async fn store_vote(
        &self,
        vote_comment_id: i64,
        cfg: &CfgProfile,
        event: &IssueCommentEvent,
    ) -> Result<Uuid> {
        let organization = event.organization.as_ref().map(|org| org.login.clone());
        let db = self.db.get().await?;
        let row = db
            .query_one(
                "
                insert into vote (
                    vote_comment_id,
                    ends_at,
                    cfg,
                    created_by,
                    installation_id,
                    issue_id,
                    issue_number,
                    is_pull_request,
                    repository_full_name,
                    organization

                ) values (
                    $1::bigint,
                    current_timestamp + ($2::bigint || ' seconds')::interval,
                    $3::jsonb,
                    $4::text,
                    $5::bigint,
                    $6::bigint,
                    $7::bigint,
                    $8::boolean,
                    $9::text,
                    $10::text
                )
                returning vote_id
                ",
                &[
                    &vote_comment_id,
                    &(cfg.duration.as_secs() as i64),
                    &Json(&cfg),
                    &event.comment.user.login,
                    &event.installation.id,
                    &event.issue.id,
                    &event.issue.number,
                    &event.issue.pull_request.is_some(),
                    &event.repository.full_name,
                    &organization,
                ],
            )
            .await?;
        let vote_id: Uuid = row.get("vote_id");
        Ok(vote_id)
    }

    /// Store the vote results provided in the database.
    async fn store_vote_results(
        &self,
        tx: &Transaction<'_>,
        vote_id: Uuid,
        results: &Results,
    ) -> Result<()> {
        tx.execute(
            "
            update vote set
                closed = true,
                closed_at = current_timestamp,
                results = $1::jsonb
            where vote_id = $2::uuid;
            ",
            &[&Json(&results), &vote_id],
        )
        .await?;
        Ok(())
    }

    /// Check if the user given is a collaborator of the provided repository.
    async fn user_is_collaborator(
        &self,
        gh: &Octocrab,
        owner: &str,
        repo: &str,
        user: &str,
    ) -> Result<bool> {
        let url = format!(
            "{}/repos/{}/{}/collaborators/{}",
            GITHUB_API_URL, owner, repo, user,
        );
        let resp = gh._get(url, None::<&()>).await?;
        if resp.status() == StatusCode::NO_CONTENT {
            return Ok(true);
        }
        Ok(false)
    }
}

/// Helper function that splits a repository's full name and returns the owner
/// and the repo name as a tuple.
fn split_full_name(full_name: &str) -> (&str, &str) {
    let mut parts = full_name.split('/');
    (parts.next().unwrap(), parts.next().unwrap())
}
