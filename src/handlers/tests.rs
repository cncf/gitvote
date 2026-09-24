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

use crate::github::{IssueEventAction, MockGH};
use crate::testutil::*;
use crate::{cmd::CreateVoteInput, db::MockDB};

use super::*;

#[tokio::test]
async fn index() {
    // Setup router
    let (router, _) = setup_test_router();

    // Run the request
    let response = router
        .oneshot(Request::builder().method("GET").uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();

    // Check the response
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CONTENT_TYPE], "text/html; charset=utf-8");
    assert_eq!(
        get_body(response).await,
        fs::read_to_string("templates/index.html").unwrap().trim_end_matches('\n')
    );
}

#[tokio::test]
async fn audit_config_not_found_returns_not_found() {
    // Setup mocks
    let mut db = MockDB::new();
    db.expect_list_votes().never();
    let mut gh = MockGH::new();
    expect_installation_id(&mut gh);
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(None)));

    // Run the request and check the response
    let response = send_get_request(db, gh, "/audit/org/repo").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn audit_disabled_returns_not_found() {
    // Setup configuration
    let cfg = setup_test_config();

    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_list_votes().never();
    let db = Arc::new(db);

    // Setup GitHub expectations
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

    // Run the request
    let (cmds_tx, _) = async_channel::unbounded();
    let router = setup_router(&cfg, db, gh, cmds_tx);
    let response = router
        .oneshot(Request::builder().method("GET").uri("/audit/org/repo").body(Body::empty()).unwrap())
        .await
        .unwrap();

    // Check the response
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn audit_enabled_renders_template() {
    // Setup configuration
    let cfg = setup_test_config();

    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_list_votes().with(eq(REPOFN)).times(1).returning({
        let votes = vec![setup_test_vote()];
        move |_| Box::pin(future::ready(Ok(votes.clone())))
    });
    let db = Arc::new(db);

    // Setup GitHub expectations
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

    // Run the request
    let (cmds_tx, _) = async_channel::unbounded();
    let router = setup_router(&cfg, db, gh, cmds_tx);
    let response = router
        .oneshot(Request::builder().method("GET").uri("/audit/org/repo").body(Body::empty()).unwrap())
        .await
        .unwrap();

    // Check the response
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CONTENT_TYPE], "text/html; charset=utf-8");
    assert_eq!(response.headers()["cache-control"], "max-age=900");
    assert!(!get_body(response).await.is_empty());
}

#[tokio::test]
async fn audit_error_getting_installation_returns_internal_server_error() {
    // Setup mocks
    let mut db = MockDB::new();
    db.expect_list_votes().never();
    let mut gh = MockGH::new();
    gh.expect_get_repository_installation_id()
        .with(eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    gh.expect_get_config_file().never();

    // Run the request and check the response
    let response = send_get_request(db, gh, "/audit/org/repo").await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn audit_error_listing_votes_returns_internal_server_error() {
    // Setup mocks
    let mut db = MockDB::new();
    db.expect_list_votes()
        .with(eq(REPOFN))
        .times(1)
        .returning(|_| Box::pin(future::ready(Err(format_err!(ERROR)))));
    let mut gh = MockGH::new();
    expect_audit_cfg(&mut gh, true);

    // Run the request and check the response
    let response = send_get_request(db, gh, "/audit/org/repo").await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn audit_installation_not_found_returns_not_found() {
    // Setup mocks
    let not_found_error = setup_test_not_found_error().await;
    let mut db = MockDB::new();
    db.expect_list_votes().never();
    let mut gh = MockGH::new();
    gh.expect_get_repository_installation_id()
        .with(eq(ORG), eq(REPO))
        .times(1)
        .return_once(move |_, _| Box::pin(future::ready(Err(not_found_error))));
    gh.expect_get_config_file().never();

    // Run the request and check the response
    let response = send_get_request(db, gh, "/audit/org/repo").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn audit_invalid_config_returns_internal_server_error() {
    // Setup mocks
    let mut db = MockDB::new();
    db.expect_list_votes().never();
    let mut gh = MockGH::new();
    expect_installation_id(&mut gh);
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Some(get_test_invalid_config()))));

    // Run the request and check the response
    let response = send_get_request(db, gh, "/audit/org/repo").await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn audit_not_configured_returns_not_found() {
    // Setup mocks
    let mut db = MockDB::new();
    db.expect_list_votes().never();
    let mut gh = MockGH::new();
    expect_installation_id(&mut gh);
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Some(get_test_valid_config()))));

    // Run the request and check the response
    let response = send_get_request(db, gh, "/audit/org/repo").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn audit_vote_details_audit_disabled_returns_not_found() {
    // Setup configuration
    let cfg = setup_test_config();

    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_get_vote().never();
    let db = Arc::new(db);

    // Setup GitHub expectations
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

    // Run the request
    let (cmds_tx, _) = async_channel::unbounded();
    let router = setup_router(&cfg, db, gh, cmds_tx);
    let response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/audit/{ORG}/{REPO}/vote/{VOTE_ID}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // Check the response
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn audit_vote_details_closed_vote_renders_template() {
    // Setup configuration
    let cfg = setup_test_config();

    // Setup database expectations
    let mut db = MockDB::new();
    let vote_id = Uuid::parse_str(VOTE_ID).unwrap();
    db.expect_get_vote().with(eq(vote_id)).times(1).returning(|_| {
        let mut vote = setup_test_vote();
        vote.results = Some(setup_test_vote_results());
        Box::pin(future::ready(Ok(Some(vote))))
    });
    let db = Arc::new(db);

    // Setup GitHub expectations
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

    // Run the request
    let (cmds_tx, _) = async_channel::unbounded();
    let router = setup_router(&cfg, db, gh, cmds_tx);
    let response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/audit/{ORG}/{REPO}/vote/{VOTE_ID}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // Check the response
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CONTENT_TYPE], "text/html; charset=utf-8");
    assert_eq!(response.headers()["cache-control"], "max-age=900");
    assert!(!get_body(response).await.is_empty());
}

#[tokio::test]
async fn audit_vote_details_error_calculating_results_returns_internal_server_error() {
    // Setup mocks
    let mut db = MockDB::new();
    db.expect_get_vote()
        .with(eq(Uuid::parse_str(VOTE_ID).unwrap()))
        .times(1)
        .returning(|_| Box::pin(future::ready(Ok(Some(setup_test_vote())))));
    let mut gh = MockGH::new();
    expect_audit_cfg(&mut gh, true);
    gh.expect_get_comment_reactions()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(COMMENT_ID))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Run the request and check the response
    let response = send_get_request(db, gh, &format!("/audit/{ORG}/{REPO}/vote/{VOTE_ID}")).await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn audit_vote_details_error_checking_audit_returns_internal_server_error() {
    // Setup mocks
    let mut db = MockDB::new();
    db.expect_get_vote().never();
    let mut gh = MockGH::new();
    gh.expect_get_repository_installation_id()
        .with(eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Run the request and check the response
    let response = send_get_request(db, gh, &format!("/audit/{ORG}/{REPO}/vote/{VOTE_ID}")).await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn audit_vote_details_error_getting_vote_returns_internal_server_error() {
    // Setup mocks
    let mut db = MockDB::new();
    db.expect_get_vote()
        .with(eq(Uuid::parse_str(VOTE_ID).unwrap()))
        .times(1)
        .returning(|_| Box::pin(future::ready(Err(format_err!(ERROR)))));
    let mut gh = MockGH::new();
    expect_audit_cfg(&mut gh, true);

    // Run the request and check the response
    let response = send_get_request(db, gh, &format!("/audit/{ORG}/{REPO}/vote/{VOTE_ID}")).await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn audit_vote_details_invalid_vote_id_returns_bad_request() {
    // Setup mocks (no calls expected)
    let db = MockDB::new();
    let gh = MockGH::new();

    // Run the request and check the response
    let response = send_get_request(db, gh, &format!("/audit/{ORG}/{REPO}/vote/invalid")).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn audit_vote_details_open_vote_renders_template() {
    // Setup configuration
    let cfg = setup_test_config();

    // Setup database expectations
    let mut db = MockDB::new();
    let vote_id = Uuid::parse_str(VOTE_ID).unwrap();
    db.expect_get_vote().with(eq(vote_id)).times(1).returning(|_| {
        let vote = setup_test_vote();
        Box::pin(future::ready(Ok(Some(vote))))
    });
    let db = Arc::new(db);

    // Setup GitHub expectations
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
    gh.expect_get_comment_reactions()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(COMMENT_ID))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(vec![]))));
    gh.expect_get_allowed_voters()
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(vec![USER1.to_string()]))));
    let gh = Arc::new(gh);

    // Run the request
    let (cmds_tx, _) = async_channel::unbounded();
    let router = setup_router(&cfg, db, gh, cmds_tx);
    let response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/audit/{ORG}/{REPO}/vote/{VOTE_ID}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // Check the response
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CONTENT_TYPE], "text/html; charset=utf-8");
    assert_eq!(response.headers()["cache-control"], "max-age=900");
    assert!(!get_body(response).await.is_empty());
}

#[tokio::test]
async fn audit_vote_details_vote_not_found() {
    // Setup configuration
    let cfg = setup_test_config();

    // Setup database expectations
    let mut db = MockDB::new();
    let vote_id = Uuid::parse_str(VOTE_ID).unwrap();
    db.expect_get_vote()
        .with(eq(vote_id))
        .times(1)
        .returning(|_| Box::pin(future::ready(Ok(None))));
    let db = Arc::new(db);

    // Setup GitHub expectations
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

    // Run the request
    let (cmds_tx, _) = async_channel::unbounded();
    let router = setup_router(&cfg, db, gh, cmds_tx);
    let response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/audit/{ORG}/{REPO}/vote/{VOTE_ID}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // Check the response
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn audit_vote_details_vote_wrong_repo_returns_not_found() {
    // Setup configuration
    let cfg = setup_test_config();

    // Setup database expectations
    let mut db = MockDB::new();
    let vote_id = Uuid::parse_str(VOTE_ID).unwrap();
    db.expect_get_vote().with(eq(vote_id)).times(1).returning(|_| {
        let mut vote = setup_test_vote();
        vote.repository_full_name = "other/repo".to_string();
        Box::pin(future::ready(Ok(Some(vote))))
    });
    let db = Arc::new(db);

    // Setup GitHub expectations
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

    // Run the request
    let (cmds_tx, _) = async_channel::unbounded();
    let router = setup_router(&cfg, db, gh, cmds_tx);
    let response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/audit/{ORG}/{REPO}/vote/{VOTE_ID}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // Check the response
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn event_invalid_body() {
    // Setup router
    let (router, _) = setup_test_router();

    // Run the request with an invalid body
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

    // Check the response
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        get_body(response).await,
        "invalid body: key must be a string at line 1 column 2",
    );
}

#[tokio::test]
async fn event_invalid_signature() {
    // Setup router
    let (router, _) = setup_test_router();

    // Run the request with an invalid signature
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

    // Check the response
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(get_body(response).await, "no valid signature found",);
}

#[tokio::test]
async fn event_issue_with_cmd() {
    // Setup event payload
    let (router, cmds_rx) = setup_test_router();
    let mut event = setup_test_issue_event();
    event.action = IssueEventAction::Opened;
    event.issue.body = Some("/vote".to_string());
    let body = serde_json::to_vec(&event).unwrap();

    // Run the request
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/events")
                .header(GITHUB_EVENT_HEADER, "issues")
                .header(GITHUB_SIGNATURE_HEADER, generate_signature(body.as_slice()))
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    // Check the response and the command queued
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(get_body(response).await, "command queued");
    assert_eq!(
        cmds_rx.recv().await.unwrap(),
        Command::CreateVote(CreateVoteInput::new(None, &Event::Issue(event)))
    );
}

#[tokio::test]
async fn event_missing_header() {
    // Setup router
    let (router, _) = setup_test_router();

    // Run the request without the event header
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

    // Check the response
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(get_body(response).await, EventError::MissingHeader.to_string());
}

#[tokio::test]
async fn event_no_signature() {
    // Setup router
    let (router, _) = setup_test_router();

    // Run the request without a signature
    let response = router
        .oneshot(Request::builder().method("POST").uri("/api/events").body(Body::empty()).unwrap())
        .await
        .unwrap();

    // Check the response
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(get_body(response).await, "no valid signature found",);
}

#[tokio::test]
async fn event_pr_without_cmd_check_not_required() {
    // Setup mocks
    let cfg = setup_test_config();
    let mut db = MockDB::new();
    db.expect_has_vote().never();
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(None)));
    gh.expect_is_check_required()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(BRANCH))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(false))));
    gh.expect_create_check_run().never();
    let (cmds_tx, cmds_rx) = async_channel::unbounded();
    let router = setup_router(&cfg, Arc::new(db), Arc::new(gh), cmds_tx);

    // Run the request
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

    // Check the response
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(get_body(response).await, "no command detected");
    assert!(cmds_rx.is_empty());
}

#[tokio::test]
async fn event_pr_without_cmd_set_check_status_failed() {
    // Setup mocks
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

    // Run the request
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

    // Check the response
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(get_body(response).await, "",);
    assert!(cmds_rx.is_empty());
}

#[tokio::test]
async fn event_unsupported() {
    // Setup router
    let (router, cmds_rx) = setup_test_router();

    // Run the request
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

    // Check the response
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(get_body(response).await, "unsupported event");
    assert!(cmds_rx.is_empty());
}

#[tokio::test]
async fn event_valid_fallback_signature() {
    // Setup router using a fallback webhook secret
    let mut cfg = setup_test_config();
    cfg.github.webhook_secret = "new-secret".to_string();
    cfg.github.webhook_secret_fallback = Some("secret".to_string());
    let (cmds_tx, cmds_rx) = async_channel::unbounded();
    let router = setup_router(&cfg, Arc::new(MockDB::new()), Arc::new(MockGH::new()), cmds_tx);

    // Run the request signed with the fallback secret
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

    // Check the event was accepted
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(get_body(response).await, "command queued");
    assert!(!cmds_rx.is_empty());
}

#[tokio::test]
async fn event_with_cmd() {
    // Setup router
    let (router, cmds_rx) = setup_test_router();

    // Run the request
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

    // Check the response and the command queued
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(get_body(response).await, "command queued");
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
    // Setup router
    let (router, cmds_rx) = setup_test_router();

    // Run the request
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

    // Check the response and the command queued
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(get_body(response).await, "command queued");
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
async fn event_without_cmd() {
    // Setup router
    let (router, cmds_rx) = setup_test_router();

    // Run the request
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

    // Check the response and no command was queued
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(get_body(response).await, "no command detected");
    assert!(cmds_rx.is_empty());
}

#[tokio::test]
async fn set_check_status_pr_opened_check_required() {
    // Setup mocks
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

    // Run and check the result
    assert!(set_check_status(db, gh, &event).await.is_ok());
}

#[tokio::test]
async fn set_check_status_pr_opened_error_creating_check_run() {
    // Setup mocks
    let db = Arc::new(MockDB::new());
    let mut gh = MockGH::new();
    gh.expect_is_check_required()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(BRANCH))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(true))));
    gh.expect_create_check_run()
        .withf(|inst_id, owner, repo, issue_number, _| {
            *inst_id == INST_ID && owner == ORG && repo == REPO && *issue_number == ISSUE_NUM
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    let gh = Arc::new(gh);
    let mut event = setup_test_pr_event();
    event.action = PullRequestEventAction::Opened;

    // Run and check the error is propagated
    let err = set_check_status(db, gh, &event).await.unwrap_err();
    assert_eq!(err.to_string(), ERROR);
}

#[tokio::test]
async fn set_check_status_pr_opened_is_check_required_failed() {
    // Setup mocks
    let db = Arc::new(MockDB::new());
    let mut gh = MockGH::new();
    gh.expect_is_check_required()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(BRANCH))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    let gh = Arc::new(gh);
    let mut event = setup_test_pr_event();
    event.action = PullRequestEventAction::Opened;

    // Run and check the error is returned
    assert!(set_check_status(db, gh, &event).await.is_err());
}

#[tokio::test]
async fn set_check_status_pr_opened_no_check_required() {
    // Setup mocks
    let db = Arc::new(MockDB::new());
    let mut gh = MockGH::new();
    gh.expect_is_check_required()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(BRANCH))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(false))));
    let gh = Arc::new(gh);
    let mut event = setup_test_pr_event();
    event.action = PullRequestEventAction::Opened;

    // Run and check the result
    assert!(set_check_status(db, gh, &event).await.is_ok());
}

#[tokio::test]
async fn set_check_status_pr_synchronized_check_required_with_vote() {
    // Setup mocks
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

    // Run and check the result
    assert!(set_check_status(db, gh, &event).await.is_ok());
}

#[tokio::test]
async fn set_check_status_pr_synchronized_check_required_without_vote() {
    // Setup mocks
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

    // Run and check the result
    assert!(set_check_status(db, gh, &event).await.is_ok());
}

#[tokio::test]
async fn set_check_status_pr_synchronized_error_checking_vote() {
    // Setup mocks
    let mut db = MockDB::new();
    db.expect_has_vote()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    let db = Arc::new(db);
    let mut gh = MockGH::new();
    gh.expect_is_check_required()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(BRANCH))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(true))));
    gh.expect_create_check_run().never();
    let gh = Arc::new(gh);
    let mut event = setup_test_pr_event();
    event.action = PullRequestEventAction::Synchronize;

    // Run and check the error is propagated
    let err = set_check_status(db, gh, &event).await.unwrap_err();
    assert_eq!(err.to_string(), ERROR);
}

#[tokio::test]
async fn set_check_status_pr_synchronized_no_check_required() {
    // Setup mocks
    let db = Arc::new(MockDB::new());
    let mut gh = MockGH::new();
    gh.expect_is_check_required()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(BRANCH))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(false))));
    let gh = Arc::new(gh);
    let mut event = setup_test_pr_event();
    event.action = PullRequestEventAction::Synchronize;

    // Run and check the result
    assert!(set_check_status(db, gh, &event).await.is_ok());
}

#[tokio::test]
async fn set_check_status_unsupported_pr_action() {
    // Setup event
    let db = Arc::new(MockDB::new());
    let gh = Arc::new(MockGH::new());
    let mut event = setup_test_pr_event();
    event.action = PullRequestEventAction::Other;

    // Run and check the result
    assert!(set_check_status(db, gh, &event).await.is_ok());
}

#[test]
fn verify_signature_invalid_hex() {
    // Setup invalid signature
    let signature = HeaderValue::from_static("sha256=zz");

    // Check the signature is rejected
    assert!(verify_signature(Some(&signature), b"secret", None, b"body").is_err());
}

#[test]
fn verify_signature_invalid_with_fallback() {
    // Setup signature
    let signature = HeaderValue::from_str(&generate_signature(b"body")).unwrap();

    // Check the signature is rejected
    assert!(verify_signature(Some(&signature), b"new-secret", Some(b"old-secret"), b"body").is_err());
}

#[test]
fn verify_signature_invalid_without_fallback() {
    // Setup signature
    let signature = HeaderValue::from_str(&generate_signature(b"body")).unwrap();

    // Check the signature is rejected
    assert!(verify_signature(Some(&signature), b"new-secret", None, b"body").is_err());
}

#[test]
fn verify_signature_missing() {
    assert!(verify_signature(None, b"secret", None, b"body").is_err());
}

#[test]
fn verify_signature_missing_prefix() {
    // Setup signature without the prefix
    let signature = generate_signature(b"body");
    let signature = HeaderValue::from_str(signature.trim_start_matches("sha256=")).unwrap();

    // Check the signature is rejected
    assert!(verify_signature(Some(&signature), b"secret", None, b"body").is_err());
}

#[test]
fn verify_signature_tampered_body() {
    // Setup signature
    let signature = HeaderValue::from_str(&generate_signature(b"body")).unwrap();

    // Check the tampered body is rejected
    assert!(verify_signature(Some(&signature), b"secret", None, b"tampered").is_err());
}

#[test]
fn verify_signature_valid_fallback() {
    // Setup signature
    let signature = HeaderValue::from_str(&generate_signature(b"body")).unwrap();

    // Check the fallback secret is accepted
    assert!(verify_signature(Some(&signature), b"new-secret", Some(b"secret"), b"body").is_ok());
}

#[test]
fn verify_signature_valid_primary() {
    // Setup signature
    let signature = HeaderValue::from_str(&generate_signature(b"body")).unwrap();

    // Check the primary secret is accepted
    assert!(verify_signature(Some(&signature), b"secret", Some(b"old-secret"), b"body").is_ok());
}

// Helpers.

/// Setup a router with default mocks, returning it with the commands receiver.
fn setup_test_router() -> (Router, Receiver<Command>) {
    let cfg = setup_test_config();
    let db = Arc::new(MockDB::new());
    let gh = Arc::new(MockGH::new());
    let (cmds_tx, cmds_rx) = async_channel::unbounded();
    (setup_router(&cfg, db, gh, cmds_tx), cmds_rx)
}

/// Setup a service configuration for tests.
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

/// Read the body of the response provided.
async fn get_body(response: Response<Body>) -> Bytes {
    to_bytes(response.into_body(), usize::MAX).await.unwrap()
}

/// Generate the signature of the body provided using the test webhook secret.
fn generate_signature(body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(b"secret").unwrap();
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// Expect the repository installation and configuration file to be requested
/// once, returning a configuration with the audit page enabled or disabled.
fn expect_audit_cfg(gh: &mut MockGH, audit_enabled: bool) {
    expect_installation_id(gh);
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(move |_, _, _| {
            let config = format!(
                "audit:\n  enabled: {audit_enabled}\nprofiles:\n  default:\n    duration: 1m\n    pass_threshold: 50\n"
            );
            Box::pin(future::ready(Some(config)))
        });
}

/// Expect the repository installation id to be requested once.
fn expect_installation_id(gh: &mut MockGH) {
    gh.expect_get_repository_installation_id()
        .with(eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(INST_ID))));
}

/// Send a GET request to the router built with the mocks provided.
async fn send_get_request(db: MockDB, gh: MockGH, uri: &str) -> Response<Body> {
    let (cmds_tx, _) = async_channel::unbounded();
    let router = setup_router(&setup_test_config(), Arc::new(db), Arc::new(gh), cmds_tx);
    router
        .oneshot(Request::builder().method("GET").uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap()
}
