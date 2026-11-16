//! This module defines the commands supported and the logic to parse them from
//! GitHub events.

use std::sync::LazyLock;

use anyhow::Result;
use askama::Template;
use regex::Regex;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::{
    cfg_repo::{AutomationRule, Cfg, CfgError},
    db::DynDB,
    github::{
        CheckDetails, DynGH, Event, IssueCommentEventAction, IssueEventAction, PullRequestEventAction,
        split_full_name,
    },
    tmpl,
};

#[cfg(test)]
mod tests;

/// Summary of the check reported when the automation rules could not be evaluated.
const CHECK_SUMMARY_AUTOMATION_FAILED: &str =
    "Automation rules could not be evaluated, maintainers should run /vote";

/// Summary of the check reported when an automation rule matches a pull request that
/// has no vote.
const CHECK_SUMMARY_AUTOMATION_RULE_MATCHED: &str =
    "An automation rule matches but there is no vote, maintainers should run /vote";

/// Summary of the check reported when a pull request does not need a vote.
const CHECK_SUMMARY_NO_VOTE_FOUND: &str = "No vote found";

/// Summary of the check reported when creating a vote failed.
const CHECK_SUMMARY_VOTE_NOT_CREATED: &str = "The vote could not be created, please run /vote again";

/// Command used to cancel an open vote.
const CMD_CANCEL_VOTE: &str = "cancel-vote";

/// Command used to check the status of an open vote.
const CMD_CHECK_VOTE: &str = "check-vote";

/// Command used to create a new vote.
const CMD_CREATE_VOTE: &str = "vote";

/// Regex used to detect commands in issues/prs comments. Only the create vote command
/// accepts a suffix, a hyphen followed by the name of the profile to use.
static CMD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^/(?:(vote)(?:-([a-zA-Z0-9_-]+))?|(cancel-vote|check-vote))\s*$")
        .expect("CMD regexp literal must be valid")
});

/// Represents a command to be executed, usually created from a GitHub event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(clippy::enum_variant_names)]
pub(crate) enum Command {
    /// Cancel an open vote.
    CancelVote(CancelVoteInput),
    /// Check the status of an open vote.
    CheckVote(CheckVoteInput),
    /// Create a new vote.
    CreateVote(CreateVoteInput),
}

impl Command {
    /// Tries to create a new command from an event.
    ///
    /// A command can be created from an event in two different ways:
    ///
    /// 1. A manual command is found in the event (e.g. `/vote` on comment).
    /// 2. The event triggers the creation of an automatic command (e.g. a new
    ///    PR is created and one of the affected files matches a predefined
    ///    pattern).
    ///
    /// Manual commands have preference, and if one is found, automatic ones
    /// won't be processed.
    ///
    /// An error means the automation rules could not be evaluated, so a vote
    /// that should have been created automatically may be missing.
    pub(crate) async fn from_event(gh: DynGH, event: &Event) -> Result<Option<Self>> {
        // Look for a manual command first, as it has preference
        if let Some(cmd) = Command::from_event_manual(event) {
            return Ok(Some(cmd));
        }

        // Otherwise, check if the event triggers an automatic command
        Command::from_event_automatic(gh, event).await
    }

    /// Creates automatic command from event, if applicable.
    async fn from_event_automatic(gh: DynGH, event: &Event) -> Result<Option<Self>> {
        match event {
            // Pull request opened
            Event::PullRequest(event) if event.action == PullRequestEventAction::Opened => {
                // Find the first automation rule matching the pull request
                let inst_id = event.installation.id as u64;
                let (owner, repo) = split_full_name(&event.repository.full_name);
                let pr_number = event.pull_request.number;
                let rule = find_automation_rule(gh, inst_id, owner, repo, pr_number).await?;

                // Create the vote using the profile of the rule
                Ok(rule.map(|rule| {
                    Command::CreateVote(CreateVoteInput::new_automatic(
                        &rule.profile,
                        &Event::PullRequest(event.clone()),
                    ))
                }))
            }
            _ => Ok(None),
        }
    }

    /// Gets manual command from event, if available.
    fn from_event_manual(event: &Event) -> Option<Self> {
        // Get the content where we'll try to extract the command from
        let content = match event {
            Event::Issue(event) if event.action == IssueEventAction::Opened => &event.issue.body,
            Event::IssueComment(event) if event.action == IssueCommentEventAction::Created => {
                &event.comment.body
            }
            Event::PullRequest(event) if event.action == PullRequestEventAction::Opened => {
                &event.pull_request.body
            }
            _ => return None,
        };

        // Create a new command from the content (if possible)
        if let Some(content) = content
            && let Some(captures) = CMD.captures(content)
        {
            let cmd = captures.get(1).or_else(|| captures.get(3))?.as_str();
            let profile = captures.get(2).map(|profile| profile.as_str());
            match cmd {
                CMD_CANCEL_VOTE => return Some(Command::CancelVote(CancelVoteInput::new(event))),
                CMD_CHECK_VOTE => return Some(Command::CheckVote(CheckVoteInput::new(event))),
                CMD_CREATE_VOTE => {
                    return Some(Command::CreateVote(CreateVoteInput::new(profile, event)));
                }
                _ => return None,
            }
        }

        None
    }
}

/// Information required to cancel an open vote.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CancelVoteInput {
    /// Login of the user who requested the cancellation.
    pub cancelled_by: String,
    /// GitHub application installation id.
    pub installation_id: i64,
    /// Whether the vote is on a pull request.
    pub is_pull_request: bool,
    /// Number of the issue or pull request of the vote.
    pub issue_number: i64,
    /// Full name of the repository, in the `owner/repo` format.
    pub repository_full_name: String,
}

impl CancelVoteInput {
    /// Creates a new `CancelVoteInput` instance from the event provided.
    pub(crate) fn new(event: &Event) -> Self {
        match event {
            Event::Issue(event) => Self {
                cancelled_by: event.sender.login.clone(),
                installation_id: event.installation.id,
                is_pull_request: event.issue.pull_request.is_some(),
                issue_number: event.issue.number,
                repository_full_name: event.repository.full_name.clone(),
            },
            Event::IssueComment(event) => Self {
                cancelled_by: event.sender.login.clone(),
                installation_id: event.installation.id,
                is_pull_request: event.issue.pull_request.is_some(),
                issue_number: event.issue.number,
                repository_full_name: event.repository.full_name.clone(),
            },
            Event::PullRequest(event) => Self {
                cancelled_by: event.sender.login.clone(),
                installation_id: event.installation.id,
                is_pull_request: true,
                issue_number: event.pull_request.number,
                repository_full_name: event.repository.full_name.clone(),
            },
        }
    }
}

/// Information required to check the status of an open vote.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CheckVoteInput {
    /// Number of the issue or pull request of the vote.
    pub issue_number: i64,
    /// Full name of the repository, in the `owner/repo` format.
    pub repository_full_name: String,

    /// Installation id, only set when a user requested the check. Failures are not
    /// reported on periodic status checks, as nobody is waiting for them.
    pub installation_id: Option<i64>,
}

impl CheckVoteInput {
    /// Creates a new `CheckVoteInput` instance from the event provided.
    pub(crate) fn new(event: &Event) -> Self {
        match event {
            Event::Issue(event) => Self {
                issue_number: event.issue.number,
                repository_full_name: event.repository.full_name.clone(),

                installation_id: Some(event.installation.id),
            },
            Event::IssueComment(event) => Self {
                issue_number: event.issue.number,
                repository_full_name: event.repository.full_name.clone(),

                installation_id: Some(event.installation.id),
            },
            Event::PullRequest(event) => Self {
                issue_number: event.pull_request.number,
                repository_full_name: event.repository.full_name.clone(),

                installation_id: Some(event.installation.id),
            },
        }
    }
}

/// Information required to create a new vote.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CreateVoteInput {
    /// Login of the user who requested the vote.
    pub created_by: String,
    /// GitHub application installation id.
    pub installation_id: i64,
    /// Whether the vote was requested by an automation rule, not by a user's command.
    pub is_automatic: bool,
    /// Whether the vote is on a pull request.
    pub is_pull_request: bool,
    /// Id of the issue or pull request of the vote.
    pub issue_id: i64,
    /// Number of the issue or pull request of the vote.
    pub issue_number: i64,
    /// Title of the issue or pull request of the vote.
    pub issue_title: String,
    /// Full name of the repository, in the `owner/repo` format.
    pub repository_full_name: String,

    /// Base branch of the pull request when the command comes from its opened event.
    /// As no other event reports the check on these pull requests, the vote processor
    /// must do it if the vote is not created.
    pub opened_pr_base_branch: Option<String>,
    /// Login of the organization the repository belongs to, if any.
    pub organization: Option<String>,
    /// Name of the configuration profile to use, the default one when not set.
    pub profile_name: Option<String>,
}

impl CreateVoteInput {
    /// Creates a new `CreateVoteInput` instance from the profile and event
    /// provided.
    pub(crate) fn new(profile_name: Option<&str>, event: &Event) -> Self {
        match event {
            Event::Issue(event) => Self {
                created_by: event.sender.login.clone(),
                installation_id: event.installation.id,
                is_automatic: false,
                is_pull_request: event.issue.pull_request.is_some(),
                issue_id: event.issue.id,
                issue_number: event.issue.number,
                issue_title: event.issue.title.clone(),
                repository_full_name: event.repository.full_name.clone(),

                opened_pr_base_branch: None,
                organization: event.organization.as_ref().map(|organization| organization.login.clone()),
                profile_name: profile_name.map(ToString::to_string),
            },
            Event::IssueComment(event) => Self {
                created_by: event.sender.login.clone(),
                installation_id: event.installation.id,
                is_automatic: false,
                is_pull_request: event.issue.pull_request.is_some(),
                issue_id: event.issue.id,
                issue_number: event.issue.number,
                issue_title: event.issue.title.clone(),
                repository_full_name: event.repository.full_name.clone(),

                opened_pr_base_branch: None,
                organization: event.organization.as_ref().map(|organization| organization.login.clone()),
                profile_name: profile_name.map(ToString::to_string),
            },
            Event::PullRequest(event) => Self {
                created_by: event.sender.login.clone(),
                installation_id: event.installation.id,
                is_automatic: false,
                is_pull_request: true,
                issue_id: event.pull_request.id,
                issue_number: event.pull_request.number,
                issue_title: event.pull_request.title.clone(),
                repository_full_name: event.repository.full_name.clone(),

                opened_pr_base_branch: Some(event.pull_request.base.reference.clone()),
                organization: event.organization.as_ref().map(|organization| organization.login.clone()),
                profile_name: profile_name.map(ToString::to_string),
            },
        }
    }

    /// Creates a new `CreateVoteInput` instance for a vote requested by an automation
    /// rule using the profile provided.
    pub(crate) fn new_automatic(profile_name: &str, event: &Event) -> Self {
        Self {
            is_automatic: true,
            ..Self::new(Some(profile_name), event)
        }
    }
}

/// Returns the details of the failure check of a pull request whose automation rules
/// could not be evaluated.
pub(crate) fn automation_failed_check() -> CheckDetails {
    failed_check(CHECK_SUMMARY_AUTOMATION_FAILED)
}

/// Creates the check run of a pull request with no vote. The automation rules are
/// evaluated only to decide the check, no vote is started: the check fails if any of
/// them matches or they cannot be evaluated, as a vote may be missing. Nothing is
/// reported if the pull request has a vote, as described in [`publish_no_vote_check`].
pub(crate) async fn create_no_vote_check_run(
    db: DynDB,
    gh: DynGH,
    inst_id: u64,
    owner: &str,
    repo: &str,
    pr_number: i64,
) -> Result<()> {
    // Decide the check from the automation rules
    let check_details = match find_automation_rule(gh.clone(), inst_id, owner, repo, pr_number).await {
        Ok(None) => no_vote_found_check(),
        Ok(Some(_)) => failed_check(CHECK_SUMMARY_AUTOMATION_RULE_MATCHED),
        Err(err) => {
            warn!(?err, owner, repo, pr_number, "error evaluating automation rules");
            automation_failed_check()
        }
    };

    // Report the check, unless the pull request has a vote
    publish_no_vote_check(db, gh, inst_id, owner, repo, pr_number, &check_details).await
}

/// Returns the details of the success check of a pull request that needs no vote.
pub(crate) fn no_vote_found_check() -> CheckDetails {
    CheckDetails {
        status: "completed".to_string(),
        summary: CHECK_SUMMARY_NO_VOTE_FOUND.to_string(),

        conclusion: Some("success".to_string()),
    }
}

/// Reports the failure check provided on a pull request just opened, unless a vote owns
/// it, as described in [`is_check_owned_by_vote`].
pub(crate) async fn publish_failure_check(
    db: DynDB,
    gh: DynGH,
    inst_id: u64,
    owner: &str,
    repo: &str,
    pr_number: i64,
    check_details: &CheckDetails,
) -> Result<()> {
    // Skip the check if a vote owns it
    if is_check_owned_by_vote(db, owner, repo, pr_number).await {
        return Ok(());
    }

    // Report the check
    gh.create_check_run(inst_id, owner, repo, pr_number, check_details).await
}

/// Reports the check provided on a pull request with no vote. Nothing is reported if
/// the pull request has a vote, open or closed, as the vote owns the check: a late,
/// replayed or concurrent event must not replace the check of an open vote or the result
/// of a closed one. A cancelled vote owns it only until its cancellation check has been
/// reported, as described in [`DB::has_vote`](crate::db::DB::has_vote). The vote is
/// looked up right before reporting the check to narrow the window of the race.
pub(crate) async fn publish_no_vote_check(
    db: DynDB,
    gh: DynGH,
    inst_id: u64,
    owner: &str,
    repo: &str,
    pr_number: i64,
    check_details: &CheckDetails,
) -> Result<()> {
    // Skip the check if a vote owns it
    let repository_full_name = format!("{owner}/{repo}");
    if db.has_vote(&repository_full_name, pr_number).await? {
        return Ok(());
    }

    // Report the check
    gh.create_check_run(inst_id, owner, repo, pr_number, check_details).await
}

/// Reports an automation failure on a pull request just opened, with a failure check and
/// a comment asking maintainers to run the command manually, which includes the details
/// provided. Both are tried even if the other fails, as they are the only trace of the
/// failure. The check does not depend on being required, as a vote would also have
/// reported it. Nothing is reported if the pull request has a vote, as described in
/// [`is_check_owned_by_vote`]: no vote is missing then, like when the event that
/// created it is delivered again.
pub(crate) async fn report_automation_failure(
    db: DynDB,
    gh: DynGH,
    inst_id: u64,
    owner: &str,
    repo: &str,
    pr_number: i64,
    details: Option<&str>,
) -> Result<()> {
    // Skip the report if a vote owns the check
    if is_check_owned_by_vote(db, owner, repo, pr_number).await {
        return Ok(());
    }

    // Report the failure check
    let check_result = gh.create_check_run(inst_id, owner, repo, pr_number, &automation_failed_check()).await;

    // Post the comment
    let body = tmpl::AutomationFailed::new(details).render()?;
    let comment_result = gh.post_comment(inst_id, owner, repo, pr_number, &body).await;

    // Propagate any failure once both reports have been tried, the check one first
    check_result?;
    comment_result?;
    Ok(())
}

/// Updates the check of a pull request with no vote, if the check is required in the
/// branch provided. Nothing is reported if the pull request has a vote, as described in
/// [`publish_no_vote_check`].
pub(crate) async fn update_no_vote_check(
    db: DynDB,
    gh: DynGH,
    inst_id: u64,
    owner: &str,
    repo: &str,
    pr_number: i64,
    branch: &str,
) -> Result<()> {
    // Skip the check if it is not required in the branch
    if !gh.is_check_required(inst_id, owner, repo, branch).await? {
        return Ok(());
    }

    // Create the check run
    create_no_vote_check_run(db, gh, inst_id, owner, repo, pr_number).await
}

/// Returns the details of the failure check of a pull request where creating a vote
/// failed.
pub(crate) fn vote_not_created_check() -> CheckDetails {
    failed_check(CHECK_SUMMARY_VOTE_NOT_CREATED)
}

/// Returns the details of a failure check with the summary provided.
fn failed_check(summary: &str) -> CheckDetails {
    CheckDetails {
        status: "completed".to_string(),
        summary: summary.to_string(),

        conclusion: Some("failure".to_string()),
    }
}

/// Finds the first automation rule matching the files of the pull request, if the
/// repository has automation enabled.
async fn find_automation_rule(
    gh: DynGH,
    inst_id: u64,
    owner: &str,
    repo: &str,
    pr_number: i64,
) -> Result<Option<AutomationRule>> {
    // Get configuration (the specific not found arm must precede the generic error one)
    let cfg = match Cfg::get(gh.clone(), inst_id, owner, repo).await {
        Err(CfgError::ConfigNotFound) => return Ok(None),
        Err(err) => return Err(err.into()),
        Ok(cfg) => cfg,
    };

    // Automation must be enabled and have some rules
    let Some(automation) = cfg.automation else {
        return Ok(None);
    };
    if !automation.enabled || automation.rules.is_empty() {
        return Ok(None);
    }

    // Check if any of the PR files matches the automation rules, the first match wins
    let pr_files = gh.get_pr_files(inst_id, owner, repo, pr_number).await?;
    for rule in automation.rules {
        if rule.matches(pr_files.as_slice())? {
            return Ok(Some(rule));
        }
    }

    Ok(None)
}

/// Checks if a vote owns the check of the pull request provided, so that no failure is
/// reported on it, as described in [`publish_no_vote_check`]. Unlike the checks reported
/// through that function, which are skipped when the vote lookup fails, the failures
/// reported after this are reported anyway: the report may be the only trace of the
/// failure, and a failure check never allows merging a pull request.
async fn is_check_owned_by_vote(db: DynDB, owner: &str, repo: &str, pr_number: i64) -> bool {
    let repository_full_name = format!("{owner}/{repo}");
    match db.has_vote(&repository_full_name, pr_number).await {
        Ok(has_vote) => has_vote,
        Err(err) => {
            warn!(?err, owner, repo, pr_number, "error looking up vote");
            false
        }
    }
}
