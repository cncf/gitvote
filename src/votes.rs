use crate::{
    db::DynDB,
    github::{DynGH, IssueCommentEvent, IssueCommentEventAction},
    tmpl,
};
use anyhow::{format_err, Context, Result};
use askama::Template;
use futures::future::{self, JoinAll};
use lazy_static::lazy_static;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fmt, sync::Arc, time::Duration};
use time::OffsetDateTime;
use tokio::{
    sync::broadcast::{self, error::TryRecvError},
    task::JoinHandle,
    time::sleep,
};
use tracing::{debug, error};
use uuid::Uuid;

/// Number of commands handlers workers.
const COMMANDS_HANDLERS_WORKERS: usize = 5;

/// Number of votes closers workers.
const VOTES_CLOSERS_WORKERS: usize = 1;

/// Default configuration profile.
const DEFAULT_PROFILE: &str = "default";

/// Vote command.
const VOTE_CMD: &str = "vote";

/// Amount of time the votes closer will sleep when there are no pending votes
/// to close.
const VOTES_CLOSER_PAUSE_ON_NONE: Duration = Duration::from_secs(15);

/// Amount of time the votes closer will sleep when something goes wrong.
const VOTES_CLOSER_PAUSE_ON_ERROR: Duration = Duration::from_secs(30);

lazy_static! {
    /// Regex used to detect commands in issues/prs comments.
    pub(crate) static ref CMD: Regex = Regex::new(r#"(?m)^/(vote)-?([a-zA-Z0-9]*)\s*$"#).expect("invalid CMD regexp");
}

/// Errors that may occur while getting the configuration profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum CfgError {
    ConfigNotFound,
    InvalidConfig(String),
    ProfileNotFound,
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

impl CfgProfile {
    /// Get the vote configuration profile requested from the config file in
    /// the repository if available.
    pub(crate) async fn get<'a>(
        gh: DynGH,
        inst_id: u64,
        owner: &'a str,
        repo: &'a str,
        profile: Option<String>,
    ) -> Result<Self, CfgError> {
        match gh.get_config_file(inst_id, owner, repo).await {
            Some(content) => {
                let mut cfg: Cfg = serde_yaml::from_str(&content)
                    .map_err(|e| CfgError::InvalidConfig(e.to_string()))?;
                let profile = profile.unwrap_or_else(|| DEFAULT_PROFILE.to_string());
                match cfg.profiles.remove(&profile) {
                    Some(profile) => Ok(profile),
                    None => Err(CfgError::ProfileNotFound),
                }
            }
            None => Err(CfgError::ConfigNotFound),
        }
    }
}

/// Represents the teams and users allowed to vote.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct AllowedVoters {
    pub teams: Option<Vec<String>>,
    pub users: Option<Vec<String>>,
}

/// Vote information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Vote {
    pub vote_id: Uuid,
    pub vote_comment_id: i64,
    pub created_at: OffsetDateTime,
    pub created_by: String,
    pub ends_at: OffsetDateTime,
    pub closed: bool,
    pub closed_at: Option<OffsetDateTime>,
    pub cfg: CfgProfile,
    pub installation_id: i64,
    pub issue_id: i64,
    pub issue_number: i64,
    pub is_pull_request: bool,
    pub repository_full_name: String,
    pub organization: Option<String>,
    pub results: Option<VoteResults>,
}

/// Vote options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum VoteOption {
    InFavor,
    Against,
    Abstain,
}

impl fmt::Display for VoteOption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::InFavor => "In favor",
            Self::Against => "Against",
            Self::Abstain => "Abstain",
        };
        write!(f, "{}", s)
    }
}

impl VoteOption {
    /// Create a new vote option from a reaction string.
    fn from_reaction(reaction: &str) -> Result<Self> {
        let vote_option = match reaction {
            "+1" => Self::InFavor,
            "-1" => Self::Against,
            "eyes" => Self::Abstain,
            _ => return Err(format_err!("reaction not supported")),
        };
        Ok(vote_option)
    }
}

/// Vote results information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct VoteResults {
    pub passed: bool,
    pub in_favor_percentage: f64,
    pub pass_threshold: f64,
    pub in_favor: i64,
    pub against: i64,
    pub abstain: i64,
    pub not_voted: i64,
    pub voters: HashMap<String, VoteOption>,
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
                let cmd = captures.get(1)?.as_str();
                let profile = match captures.get(2)?.as_str() {
                    "" => None,
                    profile => Some(profile.to_string()),
                };
                match cmd {
                    VOTE_CMD => return Some(Command::CreateVote { profile, event }),
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
    db: DynDB,
    gh: DynGH,
}

impl Processor {
    /// Create a new votes processor instance.
    pub(crate) fn new(db: DynDB, gh: DynGH) -> Result<Arc<Self>> {
        let processor = Arc::new(Self { db, gh });
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

    /// Create a new vote.
    async fn create_vote(&self, profile: Option<String>, event: IssueCommentEvent) -> Result<()> {
        // Extract some information from event
        let issue_number = event.issue.number;
        let is_pull_request = event.issue.pull_request.is_some();
        let creator = &event.comment.user.login;
        let repo_full_name = &event.repository.full_name;
        let (owner, repo) = split_full_name(repo_full_name);
        let inst_id = event.installation.id as u64;

        // Get vote configuration profile
        let cfg = match CfgProfile::get(self.gh.clone(), inst_id, owner, repo, profile).await {
            Ok(cfg) => cfg,
            Err(err) => {
                let body = match err {
                    CfgError::ConfigNotFound => tmpl::ConfigNotFound {}.render()?,
                    CfgError::ProfileNotFound => tmpl::ConfigProfileNotFound {}.render()?,
                    CfgError::InvalidConfig(reason) => tmpl::InvalidConfig::new(reason).render()?,
                };
                self.gh
                    .post_comment(inst_id, owner, repo, issue_number, &body)
                    .await?;
                return Ok(());
            }
        };

        // Only repository collaborators can create votes
        if !self
            .gh
            .user_is_collaborator(inst_id, owner, repo, creator)
            .await?
        {
            let body = tmpl::VoteRestricted::new(creator).render()?;
            self.gh
                .post_comment(inst_id, owner, repo, issue_number, &body)
                .await?;
            return Ok(());
        }

        // Only allow one vote open at the same time per issue/pr
        if self.db.has_vote_open(repo_full_name, issue_number).await? {
            let body = tmpl::VoteInProgress::new(creator, is_pull_request).render()?;
            self.gh
                .post_comment(inst_id, owner, repo, issue_number, &body)
                .await?;
            return Ok(());
        }

        // Post vote created comment on the issue/pr
        let body = tmpl::VoteCreated::new(&event, &cfg).render()?;
        let vote_comment_id = self
            .gh
            .post_comment(inst_id, owner, repo, issue_number, &body)
            .await?;

        // Store vote information in database
        let vote_id = self.db.store_vote(vote_comment_id, &cfg, &event).await?;

        debug!("vote {} created", &vote_id);
        Ok(())
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

    /// Close any pending finished vote.
    async fn close_finished_vote(&self) -> Result<Option<()>> {
        // Get pending finished vote (if any) from database
        let mut db = self.db.pool().get().await?;
        let tx = db.transaction().await?;
        let vote = match self.db.get_pending_finished_vote(&tx).await? {
            Some(vote) => vote,
            None => return Ok(None),
        };

        // Calculate results
        let (owner, repo) = split_full_name(&vote.repository_full_name);
        let results = self
            .calculate_vote_results(&vote.cfg, owner, repo, &vote)
            .await?;

        // Store results in database
        self.db
            .store_vote_results(&tx, vote.vote_id, &results)
            .await?;
        tx.commit().await?;

        // Post vote closed comment on the issue/pr
        let inst_id = vote.installation_id as u64;
        let body = tmpl::VoteClosed::new(&results).render()?;
        self.gh
            .post_comment(inst_id, owner, repo, vote.issue_number, &body)
            .await?;

        debug!("vote {} closed", &vote.vote_id);
        Ok(Some(()))
    }

    /// Calculate vote results.
    async fn calculate_vote_results(
        &self,
        cfg: &CfgProfile,
        owner: &str,
        repo: &str,
        vote: &Vote,
    ) -> Result<VoteResults> {
        // Get vote comment reactions (aka votes)
        let inst_id = vote.installation_id as u64;
        let reactions = self
            .gh
            .get_comment_reactions(inst_id, owner, repo, vote.vote_comment_id)
            .await?;

        // Get list of allowed voters (users with binding votes)
        let allowed_voters = self
            .gh
            .get_allowed_voters(inst_id, cfg, owner, repo, &vote.organization)
            .await?;

        // Track users votes
        let mut voters: HashMap<String, VoteOption> = HashMap::new();
        let mut multiple_options_voters: Vec<String> = Vec::new();
        for reaction in reactions {
            // Get vote option from reaction
            let user = reaction.user.login;
            let vote_option = match VoteOption::from_reaction(reaction.content.as_str()) {
                Ok(vote_option) => vote_option,
                Err(_) => {
                    // Ignore unsupported reactions
                    continue;
                }
            };

            // We only count the votes of the users with a binding vote
            if !allowed_voters.contains(&user) {
                continue;
            }

            // Do not count votes of users voting for multiple options
            if multiple_options_voters.contains(&user) {
                continue;
            }
            if voters.contains_key(&user) {
                // User has already voted (multiple options voter), we have to
                // remote their vote as we can't know which one to pick
                multiple_options_voters.push(user.clone());
                voters.remove(&user);
                continue;
            }

            // Track binding vote
            voters.insert(user, vote_option);
        }

        // Prepare results and return them
        let (mut in_favor, mut against, mut abstain) = (0, 0, 0);
        for (_, vote_option) in voters.iter() {
            match vote_option {
                VoteOption::InFavor => in_favor += 1,
                VoteOption::Against => against += 1,
                VoteOption::Abstain => abstain += 1,
            }
        }
        let in_favor_percentage = in_favor as f64 / allowed_voters.len() as f64 * 100.0;
        let passed = in_favor_percentage >= cfg.pass_threshold;
        let not_voted = allowed_voters
            .iter()
            .filter(|user| !voters.contains_key(*user))
            .count() as i64;

        Ok(VoteResults {
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
}

/// Helper function that splits a repository's full name and returns the owner
/// and the repo name as a tuple.
fn split_full_name(full_name: &str) -> (&str, &str) {
    let mut parts = full_name.split('/');
    (parts.next().unwrap(), parts.next().unwrap())
}
