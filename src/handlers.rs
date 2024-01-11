use crate::{cmd::Command, db::DynDB, github::*, tmpl};
use anyhow::{format_err, Error, Result};
use axum::{
    body::Bytes,
    extract::{FromRef, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use config::Config;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::sync::Arc;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;
use tracing::{error, instrument, trace};

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
}

/// Setup HTTP server router.
pub(crate) fn setup_router(
    cfg: &Arc<Config>,
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
        .layer(ServiceBuilder::new().layer(TraceLayer::new_for_http()))
        .with_state(RouterState {
            db,
            gh,
            cmds_tx,
            webhook_secret,
        });

    Ok(router)
}

/// Handler that returns the index document.
#[allow(clippy::unused_async)]
async fn index() -> impl IntoResponse {
    tmpl::Index {}
}

/// Handler that processes webhook events from GitHub.
#[allow(clippy::let_with_type_underscore)]
#[instrument(skip_all, err(Debug))]
async fn event(
    State(db): State<DynDB>,
    State(gh): State<DynGH>,
    State(cmds_tx): State<async_channel::Sender<Command>>,
    State(webhook_secret): State<String>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    // Verify payload signature
    if verify_signature(
        headers.get(GITHUB_SIGNATURE_HEADER),
        webhook_secret.as_bytes(),
        &body[..],
    )
    .is_err()
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "no valid signature found".to_string(),
        ));
    };

    // Parse event
    let event = match Event::try_from((headers.get(GITHUB_EVENT_HEADER), &body[..])) {
        Ok(event) => event,
        Err(EventError::MissingHeader) => {
            return Err((
                StatusCode::BAD_REQUEST,
                EventError::MissingHeader.to_string(),
            ))
        }
        Err(EventError::InvalidBody(err)) => {
            return Err((
                StatusCode::BAD_REQUEST,
                EventError::InvalidBody(err).to_string(),
            ))
        }
        Err(EventError::UnsupportedEvent) => return Ok(()),
    };
    trace!(?event, "event received");

    // Try to extract command from event (if available) and queue it
    match Command::from_event(gh.clone(), &event).await {
        Some(cmd) => {
            trace!(?cmd, "command detected");
            cmds_tx.send(cmd).await.unwrap();
        }
        None => {
            if let Event::PullRequest(event) = event {
                set_check_status(db, gh, &event).await.map_err(|err| {
                    error!(?err, ?event, "error setting pull request check status");
                    (StatusCode::INTERNAL_SERVER_ERROR, String::new())
                })?;
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
            gh.create_check_run(inst_id, owner, repo, pr, &check_details)
                .await?;
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
            gh.create_check_run(inst_id, owner, repo, pr, &check_details)
                .await?;
        }
        PullRequestEventAction::Other => {}
    };

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use crate::{cmd::CreateVoteInput, db::MockDB};
    use async_channel::Receiver;
    use axum::body::to_bytes;
    use axum::{
        body::Body,
        http::{header::CONTENT_TYPE, Request},
    };
    use futures::future;
    use hyper::Response;
    use mockall::predicate::eq;
    use std::{fs, path::Path};
    use tower::ServiceExt;

    #[tokio::test]
    async fn index() {
        let (router, _) = setup_test_router();

        let response = router
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[CONTENT_TYPE], "text/html; charset=utf-8");
        assert_eq!(
            get_body(response).await,
            fs::read_to_string("templates/index.html")
                .unwrap()
                .trim_end_matches('\n')
        );
    }

    #[tokio::test]
    async fn event_no_signature() {
        let (router, _) = setup_test_router();

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(get_body(response).await, "no valid signature found",);
    }

    #[tokio::test]
    async fn event_invalid_signature() {
        let (router, _) = setup_test_router();

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/events")
                    .header(GITHUB_SIGNATURE_HEADER, "invalid-signature")
                    .body(Body::from(
                        fs::read(Path::new(TESTDATA_PATH).join("event-cmd.json")).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(get_body(response).await, "no valid signature found",);
    }

    #[tokio::test]
    async fn event_missing_header() {
        let (router, _) = setup_test_router();

        let body = fs::read(Path::new(TESTDATA_PATH).join("event-cmd.json")).unwrap();
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/events")
                    .header(GITHUB_SIGNATURE_HEADER, generate_signature(body.as_slice()))
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            get_body(response).await,
            EventError::MissingHeader.to_string()
        );
    }

    #[tokio::test]
    async fn event_invalid_body() {
        let (router, _) = setup_test_router();

        let body = b"{`invalid body";
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/events")
                    .header(GITHUB_EVENT_HEADER, "issue_comment")
                    .header(GITHUB_SIGNATURE_HEADER, generate_signature(body))
                    .body(Body::from(body.to_vec()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            get_body(response).await,
            "invalid body: key must be a string at line 1 column 2",
        );
    }

    #[tokio::test]
    async fn event_unsupported() {
        let (router, cmds_rx) = setup_test_router();

        let body = fs::read(Path::new(TESTDATA_PATH).join("event-cmd.json")).unwrap();
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/events")
                    .header(GITHUB_EVENT_HEADER, "unsupported")
                    .header(GITHUB_SIGNATURE_HEADER, generate_signature(body.as_slice()))
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(cmds_rx.is_empty());
    }

    #[tokio::test]
    async fn event_without_cmd() {
        let (router, cmds_rx) = setup_test_router();

        let body = fs::read(Path::new(TESTDATA_PATH).join("event-no-cmd.json")).unwrap();
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/events")
                    .header(GITHUB_EVENT_HEADER, "issue_comment")
                    .header(GITHUB_SIGNATURE_HEADER, generate_signature(body.as_slice()))
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(cmds_rx.is_empty());
    }

    #[tokio::test]
    async fn event_with_cmd() {
        let (router, cmds_rx) = setup_test_router();

        let body = fs::read(Path::new(TESTDATA_PATH).join("event-cmd.json")).unwrap();
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/events")
                    .header(GITHUB_EVENT_HEADER, "issue_comment")
                    .header(GITHUB_SIGNATURE_HEADER, generate_signature(body.as_slice()))
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            cmds_rx.recv().await.unwrap(),
            Command::CreateVote(CreateVoteInput {
                profile_name: None,
                created_by: USER.to_string(),
                installation_id: INST_ID as i64,
                issue_id: ISSUE_ID,
                issue_number: ISSUE_NUM,
                issue_title: TITLE.to_string(),
                is_pull_request: false,
                repository_full_name: REPOFN.to_string(),
                organization: Some(ORG.to_string()),
            })
        );
    }

    #[tokio::test]
    async fn event_with_cmd_with_profile() {
        let (router, cmds_rx) = setup_test_router();

        let body = fs::read(Path::new(TESTDATA_PATH).join("event-cmd-profile.json")).unwrap();
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/events")
                    .header(GITHUB_EVENT_HEADER, "issue_comment")
                    .header(GITHUB_SIGNATURE_HEADER, generate_signature(body.as_slice()))
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            cmds_rx.recv().await.unwrap(),
            Command::CreateVote(CreateVoteInput {
                profile_name: Some(PROFILE_NAME.to_string()),
                created_by: USER.to_string(),
                installation_id: INST_ID as i64,
                issue_id: ISSUE_ID,
                issue_number: ISSUE_NUM,
                issue_title: TITLE.to_string(),
                is_pull_request: false,
                repository_full_name: REPOFN.to_string(),
                organization: Some(ORG.to_string()),
            })
        );
    }

    #[tokio::test]
    async fn event_pr_without_cmd_set_check_status_failed() {
        let cfg = Arc::new(setup_test_config());
        let db = Arc::new(MockDB::new());
        let mut gh = MockGH::new();
        gh.expect_get_config_file()
            .with(eq(INST_ID), eq(ORG), eq(REPO))
            .returning(|_, _, _| Box::pin(future::ready(None)));
        gh.expect_is_check_required()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(BRANCH))
            .returning(|_, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
        let gh = Arc::new(gh);
        let (cmds_tx, cmds_rx) = async_channel::unbounded();
        let router = setup_router(&cfg, db, gh, cmds_tx).unwrap();

        let body = fs::read(Path::new(TESTDATA_PATH).join("event-pr-no-cmd.json")).unwrap();
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/events")
                    .header(GITHUB_EVENT_HEADER, "pull_request")
                    .header(GITHUB_SIGNATURE_HEADER, generate_signature(body.as_slice()))
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(get_body(response).await, "",);
        assert!(cmds_rx.is_empty());
    }

    #[tokio::test]
    async fn set_check_status_unsupported_pr_action() {
        let db = Arc::new(MockDB::new());
        let gh = Arc::new(MockGH::new());
        let mut event = setup_test_pr_event();
        event.action = PullRequestEventAction::Other;

        assert!(set_check_status(db, gh, &event).await.is_ok());
    }

    #[tokio::test]
    async fn set_check_status_pr_opened_is_check_required_failed() {
        let db = Arc::new(MockDB::new());
        let mut gh = MockGH::new();
        gh.expect_is_check_required()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(BRANCH))
            .returning(|_, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
        let gh = Arc::new(gh);
        let mut event = setup_test_pr_event();
        event.action = PullRequestEventAction::Opened;

        assert!(set_check_status(db, gh, &event).await.is_err());
    }

    #[tokio::test]
    async fn set_check_status_pr_opened_no_check_required() {
        let db = Arc::new(MockDB::new());
        let mut gh = MockGH::new();
        gh.expect_is_check_required()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(BRANCH))
            .returning(|_, _, _, _| Box::pin(future::ready(Ok(false))));
        let gh = Arc::new(gh);
        let mut event = setup_test_pr_event();
        event.action = PullRequestEventAction::Opened;

        assert!(set_check_status(db, gh, &event).await.is_ok());
    }

    #[tokio::test]
    async fn set_check_status_pr_opened_check_required() {
        let db = Arc::new(MockDB::new());
        let mut gh = MockGH::new();
        gh.expect_is_check_required()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(BRANCH))
            .returning(|_, _, _, _| Box::pin(future::ready(Ok(true))));
        gh.expect_create_check_run()
            .with(
                eq(INST_ID),
                eq(ORG),
                eq(REPO),
                eq(ISSUE_NUM),
                eq(CheckDetails {
                    status: "completed".to_string(),
                    conclusion: Some("success".to_string()),
                    summary: "No vote found".to_string(),
                }),
            )
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
        let gh = Arc::new(gh);
        let mut event = setup_test_pr_event();
        event.action = PullRequestEventAction::Opened;

        assert!(set_check_status(db, gh, &event).await.is_ok());
    }

    #[tokio::test]
    async fn set_check_status_pr_synchronized_no_check_required() {
        let db = Arc::new(MockDB::new());
        let mut gh = MockGH::new();
        gh.expect_is_check_required()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(BRANCH))
            .returning(|_, _, _, _| Box::pin(future::ready(Ok(false))));
        let gh = Arc::new(gh);
        let mut event = setup_test_pr_event();
        event.action = PullRequestEventAction::Synchronize;

        assert!(set_check_status(db, gh, &event).await.is_ok());
    }

    #[tokio::test]
    async fn set_check_status_pr_synchronized_check_required_with_vote() {
        let mut db = MockDB::new();
        db.expect_has_vote()
            .with(eq(REPOFN), eq(ISSUE_NUM))
            .returning(|_, _| Box::pin(future::ready(Ok(true))));
        let db = Arc::new(db);
        let mut gh = MockGH::new();
        gh.expect_is_check_required()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(BRANCH))
            .returning(|_, _, _, _| Box::pin(future::ready(Ok(true))));
        let gh = Arc::new(gh);
        let mut event = setup_test_pr_event();
        event.action = PullRequestEventAction::Synchronize;

        assert!(set_check_status(db, gh, &event).await.is_ok());
    }

    #[tokio::test]
    async fn set_check_status_pr_synchronized_check_required_without_vote() {
        let mut db = MockDB::new();
        db.expect_has_vote()
            .with(eq(REPOFN), eq(ISSUE_NUM))
            .returning(|_, _| Box::pin(future::ready(Ok(false))));
        let db = Arc::new(db);
        let mut gh = MockGH::new();
        gh.expect_is_check_required()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(BRANCH))
            .returning(|_, _, _, _| Box::pin(future::ready(Ok(true))));
        gh.expect_create_check_run()
            .with(
                eq(INST_ID),
                eq(ORG),
                eq(REPO),
                eq(ISSUE_NUM),
                eq(CheckDetails {
                    status: "completed".to_string(),
                    conclusion: Some("success".to_string()),
                    summary: "No vote found".to_string(),
                }),
            )
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
        let gh = Arc::new(gh);
        let mut event = setup_test_pr_event();
        event.action = PullRequestEventAction::Synchronize;

        assert!(set_check_status(db, gh, &event).await.is_ok());
    }

    fn setup_test_router() -> (Router, Receiver<Command>) {
        let cfg = Arc::new(setup_test_config());
        let db = Arc::new(MockDB::new());
        let gh = Arc::new(MockGH::new());
        let (cmds_tx, cmds_rx) = async_channel::unbounded();
        (setup_router(&cfg, db, gh, cmds_tx).unwrap(), cmds_rx)
    }

    fn setup_test_config() -> Config {
        Config::builder()
            .set_default("github.webhookSecret", "secret")
            .unwrap()
            .build()
            .unwrap()
    }

    async fn get_body(response: Response<Body>) -> Bytes {
        to_bytes(response.into_body(), usize::MAX).await.unwrap()
    }

    fn generate_signature(body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(b"secret").unwrap();
        mac.update(body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }
}
