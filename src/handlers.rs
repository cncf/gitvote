//! This module defines the handlers used to process HTTP requests to the
//! supported endpoints.

use anyhow::{Error, Result, format_err};
use askama::Template;
use axum::{
    Router,
    body::Bytes,
    extract::{FromRef, Path, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse},
    routing::{get, post},
};
#[cfg(not(test))]
use std::time::Duration;

#[cfg(not(test))]
use cached::proc_macro::cached;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;
use tracing::{error, instrument, trace};

use crate::{
    cfg_repo::{Cfg as RepoCfg, CfgError},
    cfg_svc::Cfg,
    cmd::Command,
    db::DynDB,
    github::{
        self, CheckDetails, DynGH, Event, EventError, PullRequestEvent, PullRequestEventAction,
        split_full_name,
    },
    tmpl,
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
        Ok(html) => Ok(Html(html)),
        Err(err) => {
            error!(?err, "error rendering audit template");
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
        time = 900, // 15 minutes
        key = "String",
        convert = r#"{ repository_full_name.clone() }"#,
        sync_writes = "by_key",
        result = true
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
mod tests {
    use std::sync::Arc;
    use std::{fs, path::Path};

    use async_channel::Receiver;
    use axum::{
        body::{Body, to_bytes},
        http::{Request, header::CONTENT_TYPE},
    };
    use figment::{Figment, providers::Serialized};
    use futures::future;
    use hyper::Response;
    use mockall::predicate::eq;
    use tower::ServiceExt;

    use crate::github::MockGH;
    use crate::testutil::*;
    use crate::{cmd::CreateVoteInput, db::MockDB};

    use super::*;

    #[tokio::test]
    async fn index() {
        let (router, _) = setup_test_router();

        let response = router
            .oneshot(Request::builder().method("GET").uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[CONTENT_TYPE], "text/html; charset=utf-8");
        assert_eq!(
            get_body(response).await,
            fs::read_to_string("templates/index.html").unwrap().trim_end_matches('\n')
        );
    }

    #[tokio::test]
    async fn audit_disabled_returns_not_found() {
        let cfg = setup_test_config();

        let mut db = MockDB::new();
        db.expect_list_votes().never();
        let db = Arc::new(db);

        let mut gh = MockGH::new();
        gh.expect_get_repository_installation_id()
            .with(eq(ORG), eq(REPO))
            .times(1)
            .returning(|_, _| Box::pin(future::ready(Ok(INST_ID))));
        gh.expect_get_config_file()
            .with(eq(INST_ID), eq(ORG), eq(REPO))
            .times(1)
            .returning(|_, _, _| {
                let config = r"
audit:
  enabled: false
profiles:
  default:
    duration: 1m
    pass_threshold: 50
";
                Box::pin(future::ready(Some(config.trim_start_matches('\n').to_string())))
            });
        let gh = Arc::new(gh);

        let (cmds_tx, _) = async_channel::unbounded();
        let router = setup_router(&cfg, db, gh, cmds_tx);
        let response = router
            .oneshot(Request::builder().method("GET").uri("/audit/org/repo").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn audit_enabled_renders_template() {
        let cfg = setup_test_config();

        let mut db = MockDB::new();
        db.expect_list_votes().with(eq(REPOFN)).times(1).returning({
            let votes = vec![setup_test_vote()];
            move |_| Box::pin(future::ready(Ok(votes.clone())))
        });
        let db = Arc::new(db);

        let mut gh = MockGH::new();
        gh.expect_get_repository_installation_id()
            .with(eq(ORG), eq(REPO))
            .times(1)
            .returning(|_, _| Box::pin(future::ready(Ok(INST_ID))));
        gh.expect_get_config_file()
            .with(eq(INST_ID), eq(ORG), eq(REPO))
            .times(1)
            .returning(|_, _, _| {
                let config = r"
audit:
  enabled: true
profiles:
  default:
    duration: 1m
    pass_threshold: 50
";
                Box::pin(future::ready(Some(config.trim_start_matches('\n').to_string())))
            });
        let gh = Arc::new(gh);

        let (cmds_tx, _) = async_channel::unbounded();
        let router = setup_router(&cfg, db, gh, cmds_tx);
        let response = router
            .oneshot(Request::builder().method("GET").uri("/audit/org/repo").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[CONTENT_TYPE], "text/html; charset=utf-8");
        assert!(!get_body(response).await.is_empty());
    }

    #[tokio::test]
    async fn event_no_signature() {
        let (router, _) = setup_test_router();

        let response = router
            .oneshot(Request::builder().method("POST").uri("/api/events").body(Body::empty()).unwrap())
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
        assert_eq!(get_body(response).await, EventError::MissingHeader.to_string());
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
        let cfg = setup_test_config();
        let db = Arc::new(MockDB::new());
        let mut gh = MockGH::new();
        gh.expect_get_config_file()
            .with(eq(INST_ID), eq(ORG), eq(REPO))
            .times(1)
            .returning(|_, _, _| Box::pin(future::ready(None)));
        gh.expect_is_check_required()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(BRANCH))
            .times(1)
            .returning(|_, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
        let gh = Arc::new(gh);
        let (cmds_tx, cmds_rx) = async_channel::unbounded();
        let router = setup_router(&cfg, db, gh, cmds_tx);

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
            .times(1)
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
            .times(1)
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
            .times(1)
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
            .times(1)
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
            .times(1)
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
            .times(1)
            .returning(|_, _| Box::pin(future::ready(Ok(true))));
        let db = Arc::new(db);
        let mut gh = MockGH::new();
        gh.expect_is_check_required()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(BRANCH))
            .times(1)
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
            .times(1)
            .returning(|_, _| Box::pin(future::ready(Ok(false))));
        let db = Arc::new(db);
        let mut gh = MockGH::new();
        gh.expect_is_check_required()
            .with(eq(INST_ID), eq(ORG), eq(REPO), eq(BRANCH))
            .times(1)
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
            .times(1)
            .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
        let gh = Arc::new(gh);
        let mut event = setup_test_pr_event();
        event.action = PullRequestEventAction::Synchronize;

        assert!(set_check_status(db, gh, &event).await.is_ok());
    }

    fn setup_test_router() -> (Router, Receiver<Command>) {
        let cfg = setup_test_config();
        let db = Arc::new(MockDB::new());
        let gh = Arc::new(MockGH::new());
        let (cmds_tx, cmds_rx) = async_channel::unbounded();
        (setup_router(&cfg, db, gh, cmds_tx), cmds_rx)
    }

    fn setup_test_config() -> Cfg {
        Figment::new()
            .merge(Serialized::default("addr", "127.0.0.1:9000"))
            .merge(Serialized::default("db.host", "127.0.0.1"))
            .merge(Serialized::default("log.format", "pretty"))
            .merge(Serialized::default("github.appId", 1234))
            .merge(Serialized::default("github.appPrivateKey", "key"))
            .merge(Serialized::default("github.webhookSecret", "secret"))
            .extract()
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
