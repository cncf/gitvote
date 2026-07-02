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
    cmd::Command,
    db::DynDB,
    github::{
        self, CheckDetails, DynGH, Event, EventError, PullRequestEvent, PullRequestEventAction,
        split_full_name,
    },
    results, tmpl,
};

/// Header representing the kind of the event received.
const GITHUB_EVENT_HEADER: &str = "X-GitHub-Event";

/// Header representing the event payload signature.
const GITHUB_SIGNATURE_HEADER: &str = "X-Hub-Signature-256";

/// Router's state.
#[derive(Clone, FromRef)]
struct RouterState {
    db: DynDB,
    gh: DynGH,
    cmds_tx: async_channel::Sender<Command>,
    webhook_secret: String,
    webhook_secret_fallback: Option<String>,
}

/// Setup HTTP server router.
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
            db,
            gh,
            cmds_tx,
            webhook_secret: cfg.github.webhook_secret.clone(),
            webhook_secret_fallback: cfg.github.webhook_secret_fallback.clone(),
        })
}

// Handlers.

/// Handler that returns the index document.
#[allow(clippy::unused_async)]
async fn index() -> impl IntoResponse {
    let template = tmpl::Index {};
    match template.render() {
        Ok(html) => Ok(Html(html)),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// Handler that renders the audit page for a repository.
async fn audit(
    State(db): State<DynDB>,
    State(gh): State<DynGH>,
    Path((owner, repo)): Path<(String, String)>,
) -> impl IntoResponse {
    let repository_full_name = format!("{owner}/{repo}");

    // Check if audit page is enabled for the repository
    let audit_enabled = match audit_is_enabled(gh, repository_full_name.clone()).await {
        Ok(enabled) => enabled,
        Err(err) => {
            error!(?err, repository_full_name, "error checking audit configuration");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    if !audit_enabled {
        return Err(StatusCode::NOT_FOUND);
    }

    // Prepare and render template
    let votes = match db.list_votes(&repository_full_name).await {
        Ok(votes) => votes,
        Err(err) => {
            error!(?err, repository_full_name, "error listing repository votes");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
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
    let repository_full_name = format!("{owner}/{repo}");

    // Check if audit page is enabled for the repository
    let audit_enabled = match audit_is_enabled(gh.clone(), repository_full_name.clone()).await {
        Ok(enabled) => enabled,
        Err(err) => {
            error!(?err, %vote_id, "error checking audit configuration");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    if !audit_enabled {
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

    // Get results (from DB if closed, or calculate from GitHub API if open)
    let results = if let Some(results) = &vote.results {
        results.clone()
    } else {
        match results::calculate(gh, &owner, &repo, &vote).await {
            Ok(results) => results,
            Err(err) => {
                error!(?err, %vote_id, "error calculating vote results");
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    };

    // Render template
    let template = tmpl::AuditVoteDetails {
        results: &results,
        vote: &vote,
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
    trace!(?event, "event received");

    // Try to extract command from event (if available) and queue it
    match Command::from_event(gh.clone(), &event).await {
        Some(cmd) => {
            trace!(?cmd, "command detected");
            cmds_tx.send(cmd).await.unwrap();
            return Ok("command queued");
        }
        None => {
            if let Event::PullRequest(event) = event {
                set_check_status(db, gh, &event).await.map_err(|err| {
                    error!(?err, ?event, "error setting pull request check status");
                    (StatusCode::INTERNAL_SERVER_ERROR, String::new())
                })?;
            }
        }
    }

    Ok("no command detected")
}

// Helpers.

/// Check whether the audit page is enabled for the provided repository.
#[cfg_attr(
    not(test),
    cached(
        ttl = 900, // 15 minutes
        key = "String",
        convert = r#"{ repository_full_name.clone() }"#,
        sync_writes = "by_key"
    )
)]
async fn audit_is_enabled(gh: DynGH, repository_full_name: String) -> Result<bool> {
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
    let cfg = match RepoCfg::get(gh, inst_id, owner, repo).await {
        Ok(cfg) => cfg,
        Err(CfgError::ConfigNotFound) => return Ok(false),
        Err(err) => return Err(err.into()),
    };
    Ok(cfg.audit.is_some_and(|audit| audit.enabled))
}

/// Set a success check status to the pull request referenced in the event
/// provided when it's created or synchronized if no vote has been created on
/// it yet. This makes it possible to use the `GitVote` check in combination
/// with branch protection.
async fn set_check_status(db: DynDB, gh: DynGH, event: &PullRequestEvent) -> Result<()> {
    let (owner, repo) = split_full_name(&event.repository.full_name);
    let inst_id = event.installation.id as u64;
    let pr = event.pull_request.number;
    let branch = &event.pull_request.base.reference;
    let check_details = CheckDetails {
        status: "completed".to_string(),
        conclusion: Some("success".to_string()),
        summary: "No vote found".to_string(),
    };

    match event.action {
        PullRequestEventAction::Opened => {
            if !gh.is_check_required(inst_id, owner, repo, branch).await? {
                return Ok(());
            }
            gh.create_check_run(inst_id, owner, repo, pr, &check_details).await?;
        }
        PullRequestEventAction::Synchronize => {
            if !gh.is_check_required(inst_id, owner, repo, branch).await? {
                return Ok(());
            }
            if db.has_vote(&event.repository.full_name, event.pull_request.number).await? {
                return Ok(());
            }
            gh.create_check_run(inst_id, owner, repo, pr, &check_details).await?;
        }
        PullRequestEventAction::Other => {}
    }

    Ok(())
}

/// Verify that the signature provided is valid.
fn verify_signature(
    signature: Option<&HeaderValue>,
    secret: &[u8],
    secret_fallback: Option<&[u8]>,
    body: &[u8],
) -> Result<()> {
    if let Some(signature) = signature
        .and_then(|s| s.to_str().ok())
        .and_then(|s| s.strip_prefix("sha256="))
        .and_then(|s| hex::decode(s).ok())
    {
        // Try primary secret
        let mut mac = Hmac::<Sha256>::new_from_slice(secret)?;
        mac.update(body);
        let result = mac.verify_slice(&signature[..]);
        if result.is_ok() {
            return Ok(());
        }
        if secret_fallback.is_none() {
            return result.map_err(Error::new);
        }

        // Try fallback secret (if available)
        let mut mac = Hmac::<Sha256>::new_from_slice(secret_fallback.expect("secret should be set"))?;
        mac.update(body);
        mac.verify_slice(&signature[..]).map_err(Error::new)
    } else {
        Err(format_err!("no valid signature found"))
    }
}

#[cfg(test)]
mod tests;
