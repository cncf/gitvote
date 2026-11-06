//! This module defines the handlers used to process HTTP requests to the
//! supported endpoints.

use anyhow::{Error, Result, format_err};
use askama::Template;
use axum::{
    Router,
    body::Bytes,
    extract::{FromRef, Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{Html, IntoResponse},
    routing::{get, post},
};
#[cfg(not(test))]
use cached::cached;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;
use tracing::{error, instrument, trace};
use uuid::Uuid;

use crate::{
    cfg_repo::{Cfg as RepoCfg, CfgError},
    cfg_svc::Cfg,
    cmd::{self, Command},
    db::DynDB,
    github::{self, DynGH, Event, EventError, PullRequestEvent, PullRequestEventAction, split_full_name},
    results, tmpl,
};

#[cfg(test)]
mod tests;

/// Header representing the kind of the event received.
const GITHUB_EVENT_HEADER: &str = "X-GitHub-Event";

/// Header representing the event payload signature.
const GITHUB_SIGNATURE_HEADER: &str = "X-Hub-Signature-256";

/// Router's state.
#[derive(Clone, FromRef)]
struct RouterState {
    /// Sender used to queue the commands to process.
    cmds_tx: async_channel::Sender<Command>,
    /// Database handle.
    db: DynDB,
    /// GitHub client.
    gh: DynGH,
    /// Secret used to verify the signature of the webhook payloads.
    webhook_secret: String,

    /// Secret tried when the primary one does not verify the payload signature.
    webhook_secret_fallback: Option<String>,
}

/// Sets up the HTTP server router.
pub(crate) fn setup_router(
    cfg: &Cfg,
    db: DynDB,
    gh: DynGH,
    cmds_tx: async_channel::Sender<Command>,
) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/events", post(event))
        .route("/audit/{owner}/{repo}", get(audit))
        .route("/audit/{owner}/{repo}/vote/{vote_id}", get(audit_vote_details))
        .layer(ServiceBuilder::new().layer(TraceLayer::new_for_http()))
        .with_state(RouterState {
            cmds_tx,
            db,
            gh,
            webhook_secret: cfg.github.webhook_secret.clone(),

            webhook_secret_fallback: cfg.github.webhook_secret_fallback.clone(),
        })
}

// Handlers.

/// Handler that renders the audit page for a repository.
async fn audit(
    State(db): State<DynDB>,
    State(gh): State<DynGH>,
    Path((owner, repo)): Path<(String, String)>,
) -> impl IntoResponse {
    // Build the repository full name from the path segments
    let Some(repository_full_name) = repository_full_name(&owner, &repo) else {
        return Err(StatusCode::NOT_FOUND);
    };

    // Check if audit page is enabled for the repository
    let is_audit_enabled = match audit_is_enabled(gh, repository_full_name.clone()).await {
        Ok(is_enabled) => is_enabled,
        Err(err) => {
            error!(?err, repository_full_name, "error checking audit configuration");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    if !is_audit_enabled {
        return Err(StatusCode::NOT_FOUND);
    }

    // Get the repository votes from the database
    let votes = match db.list_votes(&repository_full_name).await {
        Ok(votes) => votes,
        Err(err) => {
            error!(?err, repository_full_name, "error listing repository votes");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    // Render the template, allowing caches to keep it for 15 minutes
    let template = tmpl::Audit::new(repository_full_name, votes);
    match template.render() {
        Ok(html) => Ok(([(header::CACHE_CONTROL, "max-age=900")], Html(html))),
        Err(err) => {
            error!(?err, "error rendering audit template");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Handler that returns vote details HTML fragment for a given vote.
async fn audit_vote_details(
    State(db): State<DynDB>,
    State(gh): State<DynGH>,
    Path((owner, repo, vote_id)): Path<(String, String, Uuid)>,
) -> impl IntoResponse {
    // Build the repository full name from the path segments
    let Some(repository_full_name) = repository_full_name(&owner, &repo) else {
        return Err(StatusCode::NOT_FOUND);
    };

    // Check if audit page is enabled for the repository
    let is_audit_enabled = match audit_is_enabled(gh.clone(), repository_full_name.clone()).await {
        Ok(is_enabled) => is_enabled,
        Err(err) => {
            error!(?err, %vote_id, "error checking audit configuration");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    if !is_audit_enabled {
        return Err(StatusCode::NOT_FOUND);
    }

    // Get vote from database
    let vote = match db.get_vote(vote_id).await {
        Ok(Some(vote)) => vote,
        Ok(None) => return Err(StatusCode::NOT_FOUND),
        Err(err) => {
            error!(?err, %vote_id, "error getting vote");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    // Verify vote belongs to the requested repository
    if vote.repository_full_name != repository_full_name {
        return Err(StatusCode::NOT_FOUND);
    }

    // Get results (from DB if closed, or calculate from GitHub API if open). Closed
    // votes have no results when their comment was deleted before they were closed
    let results = if vote.closed {
        vote.results.clone()
    } else {
        match results::calculate(gh, &owner, &repo, &vote).await {
            Ok(results) => Some(results),
            Err(err) => {
                error!(?err, %vote_id, "error calculating vote results");
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    };

    // Render the template, allowing caches to keep it for 15 minutes
    let template = tmpl::AuditVoteDetails {
        vote: &vote,

        results: results.as_ref(),
    };
    match template.render() {
        Ok(html) => Ok(([(header::CACHE_CONTROL, "max-age=900")], Html(html))),
        Err(err) => {
            error!(?err, %vote_id, "error rendering audit vote details template");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Handler that processes webhook events from GitHub.
#[allow(clippy::let_with_type_underscore)]
#[instrument(skip_all, err(Debug))]
async fn event(
    State(db): State<DynDB>,
    State(gh): State<DynGH>,
    State(cmds_tx): State<async_channel::Sender<Command>>,
    State(webhook_secret): State<String>,
    State(webhook_secret_fallback): State<Option<String>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    // Verify payload signature
    let webhook_secret = webhook_secret.as_bytes();
    let webhook_secret_fallback = webhook_secret_fallback.as_ref().map(String::as_bytes);
    if verify_signature(
        headers.get(GITHUB_SIGNATURE_HEADER),
        webhook_secret,
        webhook_secret_fallback,
        &body[..],
    )
    .is_err()
    {
        return Err((StatusCode::BAD_REQUEST, "no valid signature found".to_string()));
    }

    // Parse event
    let event = match Event::try_from((headers.get(GITHUB_EVENT_HEADER), &body[..])) {
        Ok(event) => event,
        Err(EventError::MissingHeader) => {
            return Err((StatusCode::BAD_REQUEST, EventError::MissingHeader.to_string()));
        }
        Err(EventError::InvalidBody(err)) => {
            return Err((StatusCode::BAD_REQUEST, EventError::InvalidBody(err).to_string()));
        }
        Err(EventError::UnsupportedEvent) => return Ok("unsupported event"),
    };

    // Trace the event received, without the payload, as it includes user content
    let (event_kind, repo, issue_number) = match &event {
        Event::Issue(event) => ("issues", &event.repository.full_name, event.issue.number),
        Event::IssueComment(event) => ("issue_comment", &event.repository.full_name, event.issue.number),
        Event::PullRequest(event) => (
            "pull_request",
            &event.repository.full_name,
            event.pull_request.number,
        ),
    };
    trace!(event_kind, repo, issue_number, "event received");

    // Try to extract command from event (if available)
    let cmd = match Command::from_event(gh.clone(), &event).await {
        Ok(cmd) => cmd,
        Err(err) => {
            // Only opened pull requests are evaluated for automatic commands. Don't
            // report success on them, as a vote that should exist may be missing. The
            // event is not logged, as it includes the content of the pull request
            if let Event::PullRequest(event) = &event {
                let (repo, pr_number) = (&event.repository.full_name, event.pull_request.number);
                error!(?err, repo, pr_number, "error processing automatic command");
                report_automation_failure(db, gh, event).await.map_err(|err| {
                    error!(?err, repo, pr_number, "error reporting automation failure");
                    (StatusCode::INTERNAL_SERVER_ERROR, String::new())
                })?;
            } else {
                error!(?err, "error processing automatic command");
            }
            return Ok("automation failure reported");
        }
    };

    // Queue the command, or update the check of the pull request if there isn't any
    match cmd {
        Some(cmd) => {
            trace!(?cmd, "command detected");

            // Report the check of pull requests opened with commands that cannot create
            // a vote, as they prevent automatic ones from being processed
            if let Event::PullRequest(event) = &event
                && !matches!(cmd, Command::CreateVote(_))
            {
                set_no_vote_command_check_status(db, gh, event).await.map_err(|err| {
                    error!(
                        ?err,
                        repo = event.repository.full_name,
                        pr_number = event.pull_request.number,
                        "error setting pull request check status"
                    );
                    (StatusCode::INTERNAL_SERVER_ERROR, String::new())
                })?;
            }

            // Queue the command for the votes processor
            cmds_tx
                .send(cmd)
                .await
                .expect("commands channel should stay open while the http server runs");
            return Ok("command queued");
        }
        None => {
            // Set the check status of pull requests without commands
            if let Event::PullRequest(event) = event {
                set_check_status(db, gh, &event).await.map_err(|err| {
                    error!(
                        ?err,
                        repo = event.repository.full_name,
                        pr_number = event.pull_request.number,
                        "error setting pull request check status"
                    );
                    (StatusCode::INTERNAL_SERVER_ERROR, String::new())
                })?;
            }
        }
    }

    Ok("no command detected")
}

/// Handler that returns the index document.
#[allow(clippy::unused_async)]
async fn index() -> impl IntoResponse {
    let template = tmpl::Index {};
    match template.render() {
        Ok(html) => Ok(Html(html)),
        Err(err) => {
            error!(?err, "error rendering index template");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

// Helpers.

/// Checks whether the audit page is enabled for the provided repository.
// The cache is left out of test builds so that tests do not share results, which
// leaves its size and TTL bounds deliberately untested, as they are options of the
// cached crate
#[cfg_attr(
    not(test),
    cached(
        max_size = 1000, // Keys come from unauthenticated requests
        ttl_secs = 900, // 15 minutes
        key = "String",
        convert = r#"{ repository_full_name.clone() }"#,
        sync_writes = "by_key"
    )
)]
async fn audit_is_enabled(gh: DynGH, repository_full_name: String) -> Result<bool> {
    // Get the repository installation, as the audit page is disabled without one
    let (owner, repo) = split_full_name(&repository_full_name);
    let inst_id = match gh.get_repository_installation_id(owner, repo).await {
        Ok(inst_id) => inst_id,
        Err(err) => {
            if github::is_not_found_error(&err) {
                return Ok(false);
            }
            return Err(err);
        }
    };

    // Get the repository configuration, as the audit page is disabled without one
    let cfg = match RepoCfg::get(gh, inst_id, owner, repo).await {
        Ok(cfg) => cfg,
        Err(CfgError::ConfigNotFound) => return Ok(false),
        Err(err) => return Err(err.into()),
    };

    Ok(cfg.audit.is_some_and(|audit| audit.enabled))
}

/// Reports that the automation rules of a pull request just opened could not be
/// evaluated, unless it has a vote.
async fn report_automation_failure(db: DynDB, gh: DynGH, event: &PullRequestEvent) -> Result<()> {
    let (owner, repo) = split_full_name(&event.repository.full_name);
    let inst_id = event.installation.id as u64;
    let pr_number = event.pull_request.number;
    cmd::report_automation_failure(db, gh, inst_id, owner, repo, pr_number, None).await
}

/// Builds the full name of a repository from the path segments provided. As the router
/// decodes them, a segment may contain a slash, which isn't valid in these names.
fn repository_full_name(owner: &str, repo: &str) -> Option<String> {
    if owner.contains('/') || repo.contains('/') {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

/// Sets the check status of the pull request referenced in the event provided when
/// it's created or synchronized if it has no vote. This makes it possible to use the
/// `GitVote` check in combination with branch protection.
async fn set_check_status(db: DynDB, gh: DynGH, event: &PullRequestEvent) -> Result<()> {
    // Extract the pull request details from the event
    let (owner, repo) = split_full_name(&event.repository.full_name);
    let inst_id = event.installation.id as u64;
    let pr_number = event.pull_request.number;
    let branch = &event.pull_request.base.reference;

    // Update the check according to the action, arms in variant order (catch-all last)
    match event.action {
        PullRequestEventAction::Opened => {
            // No automatic command was found, so the automation rules were already
            // evaluated and the pull request does not need a vote
            if !gh.is_check_required(inst_id, owner, repo, branch).await? {
                return Ok(());
            }

            // Publish the check reporting that no vote was found
            let check_details = cmd::no_vote_found_check();
            cmd::publish_no_vote_check(db, gh, inst_id, owner, repo, pr_number, &check_details).await?;
        }
        PullRequestEventAction::Synchronize => {
            // Skip the check if it isn't required in the branch
            if !gh.is_check_required(inst_id, owner, repo, branch).await? {
                return Ok(());
            }

            // Skip evaluating the automation rules if a vote owns the check, to avoid
            // their GitHub requests on every push to a pull request with a vote
            if db.has_vote(&event.repository.full_name, pr_number).await? {
                return Ok(());
            }

            // The files changed, so the automation rules are evaluated again, and the
            // vote is looked up once more right before reporting the check
            cmd::create_no_vote_check_run(db, gh, inst_id, owner, repo, pr_number).await?;
        }
        PullRequestEventAction::Other => {}
    }

    Ok(())
}

/// Sets the check status of a pull request just opened with a command that cannot
/// create a vote. These commands prevent automatic ones from being processed.
async fn set_no_vote_command_check_status(db: DynDB, gh: DynGH, event: &PullRequestEvent) -> Result<()> {
    let (owner, repo) = split_full_name(&event.repository.full_name);
    let inst_id = event.installation.id as u64;
    let pr_number = event.pull_request.number;
    let branch = &event.pull_request.base.reference;
    cmd::update_no_vote_check(db, gh, inst_id, owner, repo, pr_number, branch).await
}

/// Verifies that the signature provided is valid for any of the secrets given.
fn verify_signature(
    signature: Option<&HeaderValue>,
    secret: &[u8],
    secret_fallback: Option<&[u8]>,
    body: &[u8],
) -> Result<()> {
    // Decode the signature provided in the header
    let Some(signature) = signature
        .and_then(|value| value.to_str().ok())
        .and_then(|header| header.strip_prefix("sha256="))
        .and_then(|hex_signature| hex::decode(hex_signature).ok())
    else {
        return Err(format_err!("no valid signature found"));
    };

    // Try primary secret
    let mut mac = Hmac::<Sha256>::new_from_slice(secret)?;
    mac.update(body);
    let result = mac.verify_slice(&signature[..]);
    if result.is_ok() {
        return Ok(());
    }

    // Try fallback secret (if available)
    let Some(secret_fallback) = secret_fallback else {
        return result.map_err(Error::new);
    };
    let mut mac = Hmac::<Sha256>::new_from_slice(secret_fallback)?;
    mac.update(body);
    mac.verify_slice(&signature[..]).map_err(Error::new)
}
