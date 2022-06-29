use crate::{
    db::DynDB,
    github::{CheckDetails, DynGH, Event, EventError, PullRequestEvent, PullRequestEventAction},
    tmpl,
    votes::{split_full_name, Command},
};
use anyhow::{format_err, Error, Result};
use axum::{
    body::Bytes,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Extension, Router,
};
use config::Config;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::sync::Arc;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;
use tracing::{error, trace};

/// Header representing the kind of the event received.
const GITHUB_EVENT_HEADER: &str = "X-GitHub-Event";

/// Header representing the event payload signature.
const GITHUB_SIGNATURE_HEADER: &str = "X-Hub-Signature-256";

/// Setup HTTP server router.
pub(crate) async fn setup_router(
    cfg: Arc<Config>,
    db: DynDB,
    gh: DynGH,
    cmds_tx: async_channel::Sender<Command>,
) -> Result<Router> {
    // Setup webhook secret
    let webhook_secret = cfg.get_string("github.webhookSecret")?;

    // Setup router
    let router = Router::new()
        .route("/", get(index))
        .route("/api/events", post(event))
        .layer(
            ServiceBuilder::new()
                .layer(TraceLayer::new_for_http())
                .layer(Extension(webhook_secret))
                .layer(Extension(db))
                .layer(Extension(gh))
                .layer(Extension(cmds_tx)),
        );

    Ok(router)
}

/// Handler that returns the index document.
async fn index() -> impl IntoResponse {
    tmpl::Index {}
}

/// Handler that processes webhook events from GitHub.
async fn event(
    headers: HeaderMap,
    body: Bytes,
    Extension(webhook_secret): Extension<String>,
    Extension(db): Extension<DynDB>,
    Extension(gh): Extension<DynGH>,
    Extension(cmds_tx): Extension<async_channel::Sender<Command>>,
) -> Result<(), StatusCode> {
    // Verify payload signature
    if verify_signature(
        headers.get(GITHUB_SIGNATURE_HEADER),
        webhook_secret.as_bytes(),
        &body[..],
    )
    .is_err()
    {
        return Err(StatusCode::BAD_REQUEST);
    };

    // Parse event
    let event = match Event::try_from((headers.get(GITHUB_EVENT_HEADER), &body[..])) {
        Ok(event) => event,
        Err(EventError::MissingHeader) => return Err(StatusCode::BAD_REQUEST),
        Err(EventError::InvalidBody(_)) => return Err(StatusCode::BAD_REQUEST),
        Err(EventError::UnsupportedEvent) => return Ok(()),
    };
    trace!("event received: {:?}", event);

    // Try to extract command from event (if available) and queue it
    match Command::from_event(event.clone()) {
        Some(cmd) => {
            trace!("command detected: {:?}", &cmd);
            cmds_tx.send(cmd).await.unwrap()
        }
        None => {
            if let Event::PullRequest(event) = event {
                set_check_status(db, gh, event).await.map_err(|err| {
                    error!("error setting pull request check status: {:#?}", err);
                    StatusCode::INTERNAL_SERVER_ERROR
                })?
            }
        }
    };

    Ok(())
}

/// Verify that the signature provided is valid.
fn verify_signature(signature: Option<&HeaderValue>, secret: &[u8], body: &[u8]) -> Result<()> {
    if let Some(signature) = signature
        .and_then(|s| s.to_str().ok())
        .and_then(|s| s.strip_prefix("sha256="))
        .and_then(|s| hex::decode(s).ok())
    {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret)?;
        mac.update(body);
        mac.verify_slice(&signature[..]).map_err(Error::new)
    } else {
        Err(format_err!("no valid signature found"))
    }
}

/// Set a success check status to the pull request referenced in the event
/// provided when it's created or synchronized if no vote has been created on
/// it yet. This makes it possible to use the GitVote check in combination with
/// branch protection.
async fn set_check_status(db: DynDB, gh: DynGH, event: PullRequestEvent) -> Result<()> {
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
            gh.create_check_run(inst_id, owner, repo, pr, check_details)
                .await?
        }
        PullRequestEventAction::Synchronize => {
            if !gh.is_check_required(inst_id, owner, repo, branch).await? {
                return Ok(());
            }
            if db
                .has_vote(&event.repository.full_name, event.pull_request.number)
                .await?
            {
                return Ok(());
            }
            gh.create_check_run(inst_id, owner, repo, pr, check_details)
                .await?
        }
        _ => {}
    };

    Ok(())
}
