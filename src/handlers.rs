use crate::{
    github::{Event, EventError, IssueCommentEvent},
    tmpl,
    votes::Command,
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
use tokio::sync::mpsc::Sender;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;
use tracing::error;

/// Header representing the kind of the event received.
const GITHUB_EVENT_HEADER: &str = "X-GitHub-Event";

/// Header representing the event payload signature.
const GITHUB_SIGNATURE_HEADER: &str = "X-Hub-Signature-256";

/// Setup HTTP server router.
pub(crate) async fn setup_router(cfg: Arc<Config>, cmds_tx: Sender<Command>) -> Result<Router> {
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
    Extension(cmds_tx): Extension<Sender<Command>>,
) -> Result<(), StatusCode> {
    // Verify signature
    if verify_signature(
        headers.get(GITHUB_SIGNATURE_HEADER),
        webhook_secret.as_bytes(),
        &body[..],
    )
    .is_err()
    {
        return Err(StatusCode::BAD_REQUEST);
    };

    // Parse event kind
    let event = match Event::try_from(headers.get(GITHUB_EVENT_HEADER)) {
        Ok(event) => event,
        Err(EventError::HeaderMissing) => return Err(StatusCode::BAD_REQUEST),
        Err(EventError::UnsupportedEvent) => return Ok(()),
    };

    // Take action based on the kind of event received
    match event {
        // Extract command from comment (if available) and queue it
        Event::IssueComment => {
            let event: IssueCommentEvent = serde_json::from_slice(&body[..]).map_err(|err| {
                error!("error deserializing event: {}", err);
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
            match Command::from_event(event) {
                Some(cmd) => cmds_tx.send(cmd).await.unwrap(),
                None => return Ok(()),
            };
        }
    }

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
