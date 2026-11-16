use std::{fs, path::Path, sync::Arc};

use async_channel::Receiver;
use axum::{
    body::{Body, to_bytes},
    http::{Request, header::CONTENT_TYPE},
};
use figment::{Figment, providers::Serialized};
use futures::future;
use hmac::digest::MacError;
use hyper::Response;
use mockall::{Sequence, predicate::eq};
use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};
use tower::ServiceExt;

use crate::{
    cmd::{CancelVoteInput, CheckVoteInput, CreateVoteInput},
    db::{DB, MockDB, VoteCloseStep},
    github::{CheckDetails, File, IssueEventAction, MockGH},
    results::{Vote, VoteResults},
    testutil::*,
};

use super::*;

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
        .returning(|_, _, _| Box::pin(future::ready(Ok(None))));

    // Run the request and check the response
    let response = send_get_request(db, gh, "/audit/org/repo").await;
    assert_empty_uncached_error(response, StatusCode::NOT_FOUND).await;
}

#[tokio::test]
async fn audit_disabled_returns_not_found() {
    // Setup mocks
    let mut db = MockDB::new();
    db.expect_list_votes().never();
    let mut gh = MockGH::new();
    expect_audit_cfg(&mut gh, false);

    // Run the request and check the response
    let response = send_get_request(db, gh, "/audit/org/repo").await;
    assert_empty_uncached_error(response, StatusCode::NOT_FOUND).await;
}

#[tokio::test]
async fn audit_enabled_renders_template() {
    // Setup mocks
    let mut db = MockDB::new();
    db.expect_list_votes().with(eq(REPOFN)).times(1).returning(|_| {
        let votes = vec![setup_test_vote(), setup_test_closed_vote()];
        Box::pin(future::ready(Ok(votes)))
    });
    let mut gh = MockGH::new();
    expect_audit_cfg(&mut gh, true);

    // Run the request
    let response = send_get_request(db, gh, "/audit/org/repo").await;

    // Check the response
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CONTENT_TYPE], "text/html; charset=utf-8");
    assert_eq!(response.headers()["cache-control"], "max-age=900");

    // Check the page displays the votes of the repository
    let body = get_normalized_body(response).await;
    assert!(body.contains("<h1>org/repo</h1>"));
    assert!(body.contains("Test title (#1)"));
    assert!(body.contains("Closed vote title (#2)"));
    assert!(body.contains(r#"<span class="badge pass">Passed</span>"#));

    // Check the open vote links to the route serving its details
    assert!(body.contains(&format!(r#"hx-get="/audit/{ORG}/{REPO}/vote/{VOTE_ID}""#)));
    assert!(!body.contains(&format!(r#"hx-get="/audit/{ORG}/{REPO}/vote/{VOTE_ID2}""#)));
}

#[tokio::test]
async fn audit_error_getting_config_file_returns_internal_server_error() {
    // Setup mocks
    let mut db = MockDB::new();
    db.expect_list_votes().never();
    let mut gh = MockGH::new();
    expect_installation_id(&mut gh);
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Run the request and check the failure is not reported as audit being disabled
    let response = send_get_request(db, gh, "/audit/org/repo").await;
    assert_empty_uncached_error(response, StatusCode::INTERNAL_SERVER_ERROR).await;
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
    assert_empty_uncached_error(response, StatusCode::INTERNAL_SERVER_ERROR).await;
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
    assert_empty_uncached_error(response, StatusCode::INTERNAL_SERVER_ERROR).await;
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
    assert_empty_uncached_error(response, StatusCode::NOT_FOUND).await;
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
        .returning(|_, _, _| Box::pin(future::ready(Ok(Some(get_test_invalid_config())))));

    // Run the request and check the response
    let response = send_get_request(db, gh, "/audit/org/repo").await;
    assert_empty_uncached_error(response, StatusCode::INTERNAL_SERVER_ERROR).await;
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
        .returning(|_, _, _| Box::pin(future::ready(Ok(Some(get_test_valid_config())))));

    // Run the request and check the response
    let response = send_get_request(db, gh, "/audit/org/repo").await;
    assert_empty_uncached_error(response, StatusCode::NOT_FOUND).await;
}

#[tokio::test]
async fn audit_segment_with_encoded_slash_returns_not_found() {
    for uri in [
        "/audit/org%2Fother/repo".to_string(),
        "/audit/org/repo%2Fother".to_string(),
        format!("/audit/org%2Fother/repo/vote/{VOTE_ID}"),
        format!("/audit/org/repo%2Fother/vote/{VOTE_ID}"),
    ] {
        // Setup mocks (no calls expected)
        let mut db = MockDB::new();
        db.expect_get_vote().never();
        db.expect_list_votes().never();
        let mut gh = MockGH::new();
        gh.expect_get_repository_installation_id().never();

        // Run the request and check the response
        let response = send_get_request(db, gh, &uri).await;
        assert_empty_uncached_error(response, StatusCode::NOT_FOUND).await;
    }
}

#[tokio::test]
async fn audit_vote_details_audit_disabled_returns_not_found() {
    // Setup mocks
    let mut db = MockDB::new();
    db.expect_get_vote().never();
    let mut gh = MockGH::new();
    expect_audit_cfg(&mut gh, false);

    // Run the request and check the response
    let response = send_get_request(db, gh, &format!("/audit/{ORG}/{REPO}/vote/{VOTE_ID}")).await;
    assert_empty_uncached_error(response, StatusCode::NOT_FOUND).await;
}

#[tokio::test]
async fn audit_vote_details_closed_vote_renders_template() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_get_vote()
        .with(eq(Uuid::parse_str(VOTE_ID2).unwrap()))
        .times(1)
        .returning(|_| Box::pin(future::ready(Ok(Some(setup_test_closed_vote())))));

    // Setup GitHub expectations: the stored results must be used
    let mut gh = MockGH::new();
    expect_audit_cfg(&mut gh, true);
    gh.expect_get_allowed_voters().never();
    gh.expect_get_comment_reactions().never();

    // Run the request
    let response = send_get_request(db, gh, &format!("/audit/{ORG}/{REPO}/vote/{VOTE_ID2}")).await;

    // Check the response
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CONTENT_TYPE], "text/html; charset=utf-8");
    assert_eq!(response.headers()["cache-control"], "max-age=900");

    // Check the fragment displays the stored results
    let body = get_normalized_body(response).await;
    let closed_at = OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap();
    assert!(body.contains("Closed vote title (#2)"));
    assert!(body.contains(&closed_at.to_string()));
    assert!(body.contains(r#"<span class="badge pass">Passed</span>"#));
    assert_summary_stat(&body, "In favor", 3);
    assert_summary_stat(&body, "Against", 2);
    assert_summary_stat(&body, "Abstain", 1);
    assert_summary_stat(&body, "Not voted", 4);
    assert!(body.contains(&format!("<td>@{USER1}</td>")));
}

#[tokio::test]
async fn audit_vote_details_closed_vote_without_results_renders_notice() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_get_vote().with(eq(Uuid::parse_str(VOTE_ID2).unwrap())).times(1).returning(|_| {
        let vote = Vote {
            results: None,
            ..setup_test_closed_vote()
        };
        Box::pin(future::ready(Ok(Some(vote))))
    });

    // Setup GitHub expectations: results must never be calculated for closed votes
    let mut gh = MockGH::new();
    expect_audit_cfg(&mut gh, true);
    gh.expect_get_allowed_voters().never();
    gh.expect_get_comment_reactions().never();

    // Run the request
    let response = send_get_request(db, gh, &format!("/audit/{ORG}/{REPO}/vote/{VOTE_ID2}")).await;

    // Check the response
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CONTENT_TYPE], "text/html; charset=utf-8");
    assert_eq!(response.headers()["cache-control"], "max-age=900");

    // Check the fragment explains why there are no results
    let body = get_normalized_body(response).await;
    assert!(body.contains("Closed vote title (#2)"));
    assert!(body.contains("the vote comment was deleted before the vote closed"));
    assert!(!body.contains("Passed"));
    assert!(!body.contains("Failed"));
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
    assert_empty_uncached_error(response, StatusCode::INTERNAL_SERVER_ERROR).await;
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
    assert_empty_uncached_error(response, StatusCode::INTERNAL_SERVER_ERROR).await;
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
    assert_empty_uncached_error(response, StatusCode::INTERNAL_SERVER_ERROR).await;
}

#[tokio::test]
async fn audit_vote_details_invalid_vote_id_returns_bad_request() {
    // Setup mocks (no calls expected)
    let db = MockDB::new();
    let gh = MockGH::new();

    // Run the request and check the response
    let response = send_get_request(db, gh, &format!("/audit/{ORG}/{REPO}/vote/invalid")).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(!response.headers().contains_key(header::CACHE_CONTROL));
    let body = get_body(response).await;
    let body = str::from_utf8(&body).unwrap();
    assert!(body.starts_with("Invalid URL: "), "unexpected body: {body}");
}

#[tokio::test]
async fn audit_vote_details_open_vote_renders_template() {
    // Setup database expectations
    let mut db = MockDB::new();
    db.expect_get_vote()
        .with(eq(Uuid::parse_str(VOTE_ID).unwrap()))
        .times(1)
        .returning(|_| Box::pin(future::ready(Ok(Some(setup_test_vote())))));

    // Setup GitHub expectations: the results must be calculated
    let mut gh = MockGH::new();
    expect_audit_cfg(&mut gh, true);
    gh.expect_get_comment_reactions()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(COMMENT_ID))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(vec![]))));
    let expected_cfg = setup_test_vote().cfg;
    gh.expect_get_allowed_voters()
        .withf(move |inst_id, cfg, owner, repo, org| {
            *inst_id == INST_ID
                && *cfg == expected_cfg
                && owner == ORG
                && repo == REPO
                && *org == Some(ORG.to_string()).as_ref()
        })
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(vec![USER1.to_string()]))));

    // Run the request
    let response = send_get_request(db, gh, &format!("/audit/{ORG}/{REPO}/vote/{VOTE_ID}")).await;

    // Check the response
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CONTENT_TYPE], "text/html; charset=utf-8");
    assert_eq!(response.headers()["cache-control"], "max-age=900");

    // Check the fragment displays the calculated results
    let body = get_normalized_body(response).await;
    assert!(body.contains(r#"<span class="badge fail">Failed</span>"#));
    assert_summary_stat(&body, "In favor", 0);
    assert_summary_stat(&body, "Not voted", 1);
    assert!(body.contains(&format!(
        r#"<td>@{USER1}</td> <td> <span class="vote-chip not-voted">Not voted</span>"#
    )));
}

#[tokio::test]
async fn audit_vote_details_vote_not_found() {
    // Setup mocks
    let mut db = MockDB::new();
    db.expect_get_vote()
        .with(eq(Uuid::parse_str(VOTE_ID).unwrap()))
        .times(1)
        .returning(|_| Box::pin(future::ready(Ok(None))));
    let mut gh = MockGH::new();
    expect_audit_cfg(&mut gh, true);

    // Run the request and check the response
    let response = send_get_request(db, gh, &format!("/audit/{ORG}/{REPO}/vote/{VOTE_ID}")).await;
    assert_empty_uncached_error(response, StatusCode::NOT_FOUND).await;
}

#[tokio::test]
async fn audit_vote_details_vote_wrong_repo_returns_not_found() {
    // Setup mocks
    let mut db = MockDB::new();
    db.expect_get_vote().with(eq(Uuid::parse_str(VOTE_ID).unwrap())).times(1).returning(|_| {
        let mut vote = setup_test_vote();
        vote.repository_full_name = "other/repo".to_string();
        Box::pin(future::ready(Ok(Some(vote))))
    });
    let mut gh = MockGH::new();
    expect_audit_cfg(&mut gh, true);

    // Run the request and check the response
    let response = send_get_request(db, gh, &format!("/audit/{ORG}/{REPO}/vote/{VOTE_ID}")).await;
    assert_empty_uncached_error(response, StatusCode::NOT_FOUND).await;
}

#[tokio::test]
#[ignore = "needs a migrated PostgreSQL database in GITVOTE_TEST_DB_URL"]
async fn db_contracts_set_check_status_pr_synchronized_during_pending_cancellation() {
    // Setup cases: whether the check run step of the cancellation is done, which hands
    // the check of the heads pushed afterwards back to the pull request
    for check_run_done in [false, true] {
        // Setup database with a vote on the pull request whose cancellation is pending
        let test_db = TestDB::new().await;
        let db = test_db.db();
        let vote_id = test_db
            .seed_vote(SeedVote {
                is_pull_request: true,
                ..Default::default()
            })
            .await;
        db.cancel_vote(REPOFN, ISSUE_NUM, USER1).await.unwrap().unwrap();
        if check_run_done {
            db.record_vote_close_step(vote_id, VoteCloseStep::CheckRun).await.unwrap();
        }

        // Setup GitHub expectations: the check is reported once the cancellation's is
        let mut gh = MockGH::new();
        expect_is_check_required(&mut gh, true);
        if check_run_done {
            expect_no_config(&mut gh);
            expect_check_run(&mut gh, cmd::no_vote_found_check());
        } else {
            gh.expect_get_config_file().never();
            gh.expect_create_check_run().never();
        }

        // Setup the synchronized pull request event
        let mut event = setup_test_pr_event();
        event.action = PullRequestEventAction::Synchronize;

        // Run and check the result
        set_check_status(Arc::new(db), Arc::new(gh), &event).await.unwrap();
    }
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
    assert!(get_body(response).await.starts_with(b"invalid body: "));
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
    assert_eq!(get_body(response).await, "no valid signature found");
}

#[tokio::test]
async fn event_issue_with_cmd() {
    // Setup router and event payload
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
    assert_eq!(get_body(response).await, "no valid signature found");
}

#[tokio::test]
async fn event_pr_automation_failure_error_checking_vote_still_reports_check_and_comment() {
    // Setup mocks: the failure is reported as the vote lookup fails
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Err(format_err!(ERROR)));
    let mut gh = MockGH::new();
    expect_invalid_config(&mut gh);
    expect_check_run(&mut gh, cmd::automation_failed_check());
    expect_automation_failed_comment(&mut gh, || Ok(COMMENT_ID));

    // Setup router
    let (router, cmds_rx) = setup_test_router_with_mocks(db, gh);

    // Run the request
    let response = send_pr_event(router, &setup_test_pr_opened_event("Hi!")).await;

    // Check the response and no vote was started
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(get_body(response).await, "automation failure reported");
    assert!(cmds_rx.is_empty());
}

#[tokio::test]
async fn event_pr_automation_failure_error_creating_check_run_still_posts_comment() {
    // Setup mocks
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Ok(false));
    let mut gh = MockGH::new();
    expect_invalid_config(&mut gh);
    gh.expect_create_check_run()
        .with(
            eq(INST_ID),
            eq(ORG),
            eq(REPO),
            eq(ISSUE_NUM),
            eq(cmd::automation_failed_check()),
        )
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    expect_automation_failed_comment(&mut gh, || Ok(COMMENT_ID));

    // Setup router
    let (router, cmds_rx) = setup_test_router_with_mocks(db, gh);

    // Run the request
    let response = send_pr_event(router, &setup_test_pr_opened_event("Hi!")).await;

    // Check the response
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(get_body(response).await, "");
    assert!(cmds_rx.is_empty());
}

#[tokio::test]
async fn event_pr_automation_failure_error_posting_comment() {
    // Setup mocks
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Ok(false));
    let mut gh = MockGH::new();
    expect_invalid_config(&mut gh);
    expect_check_run(&mut gh, cmd::automation_failed_check());
    expect_automation_failed_comment(&mut gh, || Err(format_err!(ERROR)));

    // Setup router
    let (router, cmds_rx) = setup_test_router_with_mocks(db, gh);

    // Run the request
    let response = send_pr_event(router, &setup_test_pr_opened_event("Hi!")).await;

    // Check the response
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(get_body(response).await, "");
    assert!(cmds_rx.is_empty());
}

#[tokio::test]
async fn event_pr_automation_failure_getting_files_reports_check_and_comment() {
    // Setup mocks
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Ok(false));
    let mut gh = MockGH::new();
    expect_valid_config(&mut gh);
    gh.expect_get_pr_files()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    gh.expect_is_check_required().never();
    expect_check_run(&mut gh, cmd::automation_failed_check());
    expect_automation_failed_comment(&mut gh, || Ok(COMMENT_ID));

    // Setup router
    let (router, cmds_rx) = setup_test_router_with_mocks(db, gh);

    // Run the request
    let response = send_pr_event(router, &setup_test_pr_opened_event("Hi!")).await;

    // Check the response and no vote was started
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(get_body(response).await, "automation failure reported");
    assert!(cmds_rx.is_empty());
}

#[tokio::test]
async fn event_pr_automation_failure_invalid_config_reports_check_and_comment() {
    // Setup mocks
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Ok(false));
    let mut gh = MockGH::new();
    expect_invalid_config(&mut gh);
    gh.expect_get_pr_files().never();
    gh.expect_is_check_required().never();
    expect_check_run(&mut gh, cmd::automation_failed_check());
    expect_automation_failed_comment(&mut gh, || Ok(COMMENT_ID));

    // Setup router
    let (router, cmds_rx) = setup_test_router_with_mocks(db, gh);

    // Run the request
    let response = send_pr_event(router, &setup_test_pr_opened_event("Hi!")).await;

    // Check the response and no vote was started
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(get_body(response).await, "automation failure reported");
    assert!(cmds_rx.is_empty());
}

#[tokio::test]
async fn event_pr_automation_failure_with_vote_reports_nothing() {
    // Setup mocks: the pull request has a vote, like when its opened event is replayed
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Ok(true));
    let mut gh = MockGH::new();
    expect_invalid_config(&mut gh);
    gh.expect_create_check_run().never();
    gh.expect_post_comment().never();

    // Setup router
    let (router, cmds_rx) = setup_test_router_with_mocks(db, gh);

    // Run the request
    let response = send_pr_event(router, &setup_test_pr_opened_event("Hi!")).await;

    // Check the response and no vote was started
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(get_body(response).await, "automation failure reported");
    assert!(cmds_rx.is_empty());
}

#[tokio::test]
async fn event_pr_synchronized_rule_matches_reports_failure_without_starting_vote() {
    // Setup mocks: the vote is looked up before evaluating the rules and again right
    // before reporting the check
    let mut db = MockDB::new();
    db.expect_has_vote()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(2)
        .returning(|_, _| Box::pin(future::ready(Ok(false))));
    let mut gh = MockGH::new();
    expect_is_check_required(&mut gh, true);
    expect_valid_config(&mut gh);
    expect_pr_files(&mut gh, "README.md");
    expect_check_run(&mut gh, setup_test_rule_matched_check());

    // Setup router
    let (router, cmds_rx) = setup_test_router_with_mocks(db, gh);

    // Setup the synchronized pull request event
    let mut event = setup_test_pr_event();
    event.action = PullRequestEventAction::Synchronize;

    // Run the request
    let response = send_pr_event(router, &event).await;

    // Check the response and no vote was started
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(get_body(response).await, "no command detected");
    assert!(cmds_rx.is_empty());
}

#[tokio::test]
async fn event_pr_with_automatic_cmd_does_not_set_check_status() {
    // Setup mocks
    let mut db = MockDB::new();
    db.expect_has_vote().never();
    let mut gh = MockGH::new();
    expect_valid_config(&mut gh);
    expect_pr_files(&mut gh, "README.md");
    gh.expect_is_check_required().never();
    gh.expect_create_check_run().never();

    // Setup router
    let (router, cmds_rx) = setup_test_router_with_mocks(db, gh);

    // Run the request
    let event = setup_test_pr_opened_event("Hi!");
    let response = send_pr_event(router, &event).await;

    // Check the response and the command queued
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(get_body(response).await, "command queued");
    assert_eq!(
        cmds_rx.recv().await.unwrap(),
        Command::CreateVote(CreateVoteInput::new_automatic(
            "default",
            &Event::PullRequest(event)
        ))
    );
}

#[tokio::test]
async fn event_pr_with_cancel_vote_cmd_sets_check_status() {
    // Setup mocks
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Ok(false));
    let mut gh = MockGH::new();
    expect_is_check_required(&mut gh, true);
    expect_no_config(&mut gh);
    expect_check_run(&mut gh, cmd::no_vote_found_check());

    // Setup router
    let (router, cmds_rx) = setup_test_router_with_mocks(db, gh);

    // Run the request
    let event = setup_test_pr_opened_event("/cancel-vote");
    let response = send_pr_event(router, &event).await;

    // Check the response and the command queued
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(get_body(response).await, "command queued");
    assert_eq!(
        cmds_rx.recv().await.unwrap(),
        Command::CancelVote(CancelVoteInput::new(&Event::PullRequest(event)))
    );
}

#[tokio::test]
async fn event_pr_with_check_vote_cmd_check_not_required() {
    // Setup mocks
    let mut db = MockDB::new();
    db.expect_has_vote().never();
    let mut gh = MockGH::new();
    expect_is_check_required(&mut gh, false);
    gh.expect_get_config_file().never();
    gh.expect_create_check_run().never();

    // Setup router
    let (router, cmds_rx) = setup_test_router_with_mocks(db, gh);

    // Run the request
    let event = setup_test_pr_opened_event("/check-vote");
    let response = send_pr_event(router, &event).await;

    // Check the response and the command queued
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(get_body(response).await, "command queued");
    assert_eq!(
        cmds_rx.recv().await.unwrap(),
        Command::CheckVote(CheckVoteInput::new(&Event::PullRequest(event)))
    );
}

#[tokio::test]
async fn event_pr_with_check_vote_cmd_error_checking_vote() {
    // Setup mocks
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Err(format_err!(ERROR)));
    let mut gh = MockGH::new();
    expect_is_check_required(&mut gh, true);
    expect_no_config(&mut gh);
    gh.expect_create_check_run().never();

    // Setup router
    let (router, cmds_rx) = setup_test_router_with_mocks(db, gh);

    // Run the request
    let response = send_pr_event(router, &setup_test_pr_opened_event("/check-vote")).await;

    // Check the response and the command was left for the redelivery of the event
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(get_body(response).await, "");
    assert!(cmds_rx.is_empty());
}

#[tokio::test]
async fn event_pr_with_check_vote_cmd_error_setting_check_status() {
    // Setup mocks
    let mut db = MockDB::new();
    db.expect_has_vote().never();
    let mut gh = MockGH::new();
    gh.expect_is_check_required()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(BRANCH))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    gh.expect_create_check_run().never();

    // Setup router
    let (router, cmds_rx) = setup_test_router_with_mocks(db, gh);

    // Run the request
    let response = send_pr_event(router, &setup_test_pr_opened_event("/check-vote")).await;

    // Check the response and the command was left for the redelivery of the event
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(get_body(response).await, "");
    assert!(cmds_rx.is_empty());
}

#[tokio::test]
async fn event_pr_with_check_vote_cmd_rule_matches_reports_failure() {
    // Setup mocks
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Ok(false));
    let mut gh = MockGH::new();
    expect_is_check_required(&mut gh, true);
    expect_valid_config(&mut gh);
    expect_pr_files(&mut gh, "README.md");
    expect_check_run(&mut gh, setup_test_rule_matched_check());

    // Setup router
    let (router, cmds_rx) = setup_test_router_with_mocks(db, gh);

    // Run the request
    let event = setup_test_pr_opened_event("/check-vote");
    let response = send_pr_event(router, &event).await;

    // Check the response and the command queued
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(get_body(response).await, "command queued");
    assert_eq!(
        cmds_rx.recv().await.unwrap(),
        Command::CheckVote(CheckVoteInput::new(&Event::PullRequest(event)))
    );
}

#[tokio::test]
async fn event_pr_with_create_vote_cmd_does_not_set_check_status() {
    // Setup mocks
    let mut db = MockDB::new();
    db.expect_has_vote().never();
    let mut gh = MockGH::new();
    gh.expect_is_check_required().never();
    gh.expect_create_check_run().never();

    // Setup router
    let (router, cmds_rx) = setup_test_router_with_mocks(db, gh);

    // Run the request
    let response = send_pr_event(router, &setup_test_pr_opened_event("/vote")).await;

    // Check the response and the command queued
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(get_body(response).await, "command queued");
    assert_eq!(
        cmds_rx.recv().await.unwrap(),
        Command::CreateVote(CreateVoteInput {
            created_by: USER.to_string(),
            installation_id: INST_ID as i64,
            is_automatic: false,
            is_pull_request: true,
            issue_id: ISSUE_ID,
            issue_number: ISSUE_NUM,
            issue_title: TITLE.to_string(),
            repository_full_name: REPOFN.to_string(),

            opened_pr_base_branch: Some(BRANCH.to_string()),
            organization: Some(ORG.to_string()),
            profile_name: None,
        })
    );
}

#[tokio::test]
async fn event_pr_with_no_vote_cmd_with_vote_does_not_set_check_status() {
    // Setup cases: a vote created or replayed concurrently owns the check of the pull
    // request, whatever command it was sent with
    type ExpectedCmd = fn(&Event) -> Command;
    let cases: [(&str, ExpectedCmd); 2] = [
        ("/cancel-vote", |event| {
            Command::CancelVote(CancelVoteInput::new(event))
        }),
        ("/check-vote", |event| {
            Command::CheckVote(CheckVoteInput::new(event))
        }),
    ];
    for (body, expected_cmd) in cases {
        // Setup mocks
        let mut db = MockDB::new();
        expect_has_vote(&mut db, Ok(true));
        let mut gh = MockGH::new();
        expect_is_check_required(&mut gh, true);
        expect_no_config(&mut gh);
        gh.expect_create_check_run().never();

        // Setup router
        let (router, cmds_rx) = setup_test_router_with_mocks(db, gh);

        // Run the request
        let event = setup_test_pr_opened_event(body);
        let response = send_pr_event(router, &event).await;

        // Check the response and the command queued
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(get_body(response).await, "command queued");
        assert_eq!(
            cmds_rx.recv().await.unwrap(),
            expected_cmd(&Event::PullRequest(event))
        );
    }
}

#[tokio::test]
async fn event_pr_without_cmd_check_not_required() {
    // Setup mocks
    let mut db = MockDB::new();
    db.expect_has_vote().never();
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(None))));
    gh.expect_is_check_required()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(BRANCH))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(false))));
    gh.expect_create_check_run().never();

    // Setup router
    let (router, cmds_rx) = setup_test_router_with_mocks(db, gh);

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
    let mut db = MockDB::new();
    db.expect_has_vote().never();
    let mut gh = MockGH::new();
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(None))));
    gh.expect_is_check_required()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(BRANCH))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    gh.expect_create_check_run().never();

    // Setup router
    let (router, cmds_rx) = setup_test_router_with_mocks(db, gh);

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
    assert_eq!(get_body(response).await, "");
    assert!(cmds_rx.is_empty());
}

#[tokio::test]
async fn event_signature_from_other_secret_is_rejected() {
    // Setup router and a well formed signature made with another secret
    let (router, cmds_rx) = setup_test_router();
    let body = fs::read(Path::new(TESTDATA_PATH).join("event-cmd.json")).unwrap();
    let signature = generate_signature_with_secret(b"other-secret", &body);

    // Run the request
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/events")
                .header(GITHUB_EVENT_HEADER, "issue_comment")
                .header(GITHUB_SIGNATURE_HEADER, signature)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    // Check the response and that no command was queued
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(get_body(response).await, "no valid signature found");
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
            created_by: USER.to_string(),
            installation_id: INST_ID as i64,
            is_automatic: false,
            is_pull_request: false,
            issue_id: ISSUE_ID,
            issue_number: ISSUE_NUM,
            issue_title: TITLE.to_string(),
            repository_full_name: REPOFN.to_string(),

            opened_pr_base_branch: None,
            organization: Some(ORG.to_string()),
            profile_name: None,
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
            created_by: USER.to_string(),
            installation_id: INST_ID as i64,
            is_automatic: false,
            is_pull_request: false,
            issue_id: ISSUE_ID,
            issue_number: ISSUE_NUM,
            issue_title: TITLE.to_string(),
            repository_full_name: REPOFN.to_string(),

            opened_pr_base_branch: None,
            organization: Some(ORG.to_string()),
            profile_name: Some(PROFILE_NAME.to_string()),
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
async fn set_check_status_pr_opened_check_required() {
    // Setup mocks
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Ok(false));
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
                summary: "No vote found".to_string(),

                conclusion: Some("success".to_string()),
            }),
        )
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
    let gh = Arc::new(gh);

    // Setup the opened pull request event
    let mut event = setup_test_pr_event();
    event.action = PullRequestEventAction::Opened;

    // Run and check the result
    assert!(set_check_status(db, gh, &event).await.is_ok());
}

#[tokio::test]
async fn set_check_status_pr_opened_check_required_with_vote() {
    // Setup mocks: the automation rules are not evaluated again
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Ok(true));
    let mut gh = MockGH::new();
    expect_is_check_required(&mut gh, true);
    gh.expect_get_config_file().never();
    gh.expect_create_check_run().never();

    // Setup the opened pull request event
    let mut event = setup_test_pr_event();
    event.action = PullRequestEventAction::Opened;

    // Run and check nothing is reported
    set_check_status(Arc::new(db), Arc::new(gh), &event).await.unwrap();
}

#[tokio::test]
async fn set_check_status_pr_opened_error_checking_vote() {
    // Setup mocks
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Err(format_err!(ERROR)));
    let mut gh = MockGH::new();
    expect_is_check_required(&mut gh, true);
    gh.expect_create_check_run().never();

    // Setup the opened pull request event
    let mut event = setup_test_pr_event();
    event.action = PullRequestEventAction::Opened;

    // Run and check the error is propagated
    let err = set_check_status(Arc::new(db), Arc::new(gh), &event).await.unwrap_err();
    assert_eq!(err.to_string(), ERROR);
}

#[tokio::test]
async fn set_check_status_pr_opened_error_creating_check_run() {
    // Setup mocks
    let mut db = MockDB::new();
    expect_has_vote(&mut db, Ok(false));
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
            eq(cmd::no_vote_found_check()),
        )
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    let gh = Arc::new(gh);

    // Setup the opened pull request event
    let mut event = setup_test_pr_event();
    event.action = PullRequestEventAction::Opened;

    // Run and check the error is propagated
    let err = set_check_status(db, gh, &event).await.unwrap_err();
    assert_eq!(err.to_string(), ERROR);
}

#[tokio::test]
async fn set_check_status_pr_opened_is_check_required_failed() {
    // Setup mocks
    let mut db = MockDB::new();
    db.expect_has_vote().never();
    let db = Arc::new(db);
    let mut gh = MockGH::new();
    gh.expect_is_check_required()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(BRANCH))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    gh.expect_create_check_run().never();
    let gh = Arc::new(gh);

    // Setup the opened pull request event
    let mut event = setup_test_pr_event();
    event.action = PullRequestEventAction::Opened;

    // Run and check the error is returned
    let err = set_check_status(db, gh, &event).await.unwrap_err();
    assert_eq!(err.to_string(), ERROR);
}

#[tokio::test]
async fn set_check_status_pr_opened_no_check_required() {
    // Setup mocks
    let mut db = MockDB::new();
    db.expect_has_vote().never();
    let db = Arc::new(db);
    let mut gh = MockGH::new();
    gh.expect_is_check_required()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(BRANCH))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(false))));
    gh.expect_create_check_run().never();
    let gh = Arc::new(gh);

    // Setup the opened pull request event
    let mut event = setup_test_pr_event();
    event.action = PullRequestEventAction::Opened;

    // Run and check the result
    assert!(set_check_status(db, gh, &event).await.is_ok());
}

#[tokio::test]
async fn set_check_status_pr_synchronized_check_required_with_vote() {
    // Setup mocks: the automation rules are not evaluated
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
    gh.expect_get_config_file().never();
    gh.expect_create_check_run().never();
    let gh = Arc::new(gh);

    // Setup the synchronized pull request event
    let mut event = setup_test_pr_event();
    event.action = PullRequestEventAction::Synchronize;

    // Run and check the result
    assert!(set_check_status(db, gh, &event).await.is_ok());
}

#[tokio::test]
async fn set_check_status_pr_synchronized_check_required_without_vote() {
    // Setup mocks: the vote is looked up before evaluating the rules and again right
    // before reporting the check
    let mut db = MockDB::new();
    db.expect_has_vote()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(2)
        .returning(|_, _| Box::pin(future::ready(Ok(false))));
    let db = Arc::new(db);
    let mut gh = MockGH::new();
    gh.expect_is_check_required()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(BRANCH))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(true))));
    expect_no_config(&mut gh);
    gh.expect_create_check_run()
        .with(
            eq(INST_ID),
            eq(ORG),
            eq(REPO),
            eq(ISSUE_NUM),
            eq(CheckDetails {
                status: "completed".to_string(),
                summary: "No vote found".to_string(),

                conclusion: Some("success".to_string()),
            }),
        )
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
    let gh = Arc::new(gh);

    // Setup the synchronized pull request event
    let mut event = setup_test_pr_event();
    event.action = PullRequestEventAction::Synchronize;

    // Run and check the result
    assert!(set_check_status(db, gh, &event).await.is_ok());
}

#[tokio::test]
async fn set_check_status_pr_synchronized_error_checking_vote() {
    // Setup mocks: the automation rules are not evaluated
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
    gh.expect_get_config_file().never();
    gh.expect_create_check_run().never();
    let gh = Arc::new(gh);

    // Setup the synchronized pull request event
    let mut event = setup_test_pr_event();
    event.action = PullRequestEventAction::Synchronize;

    // Run and check the error is propagated
    let err = set_check_status(db, gh, &event).await.unwrap_err();
    assert_eq!(err.to_string(), ERROR);
}

#[tokio::test]
async fn set_check_status_pr_synchronized_evaluation_fails_reports_failure() {
    // Setup mocks: the vote is looked up before evaluating the rules and again right
    // before reporting the check
    let mut db = MockDB::new();
    db.expect_has_vote()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(2)
        .returning(|_, _| Box::pin(future::ready(Ok(false))));
    let mut gh = MockGH::new();
    expect_is_check_required(&mut gh, true);
    expect_valid_config(&mut gh);
    gh.expect_get_pr_files()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    expect_check_run(&mut gh, cmd::automation_failed_check());
    gh.expect_post_comment().never();

    // Setup the synchronized pull request event
    let mut event = setup_test_pr_event();
    event.action = PullRequestEventAction::Synchronize;

    // Run and check the result
    set_check_status(Arc::new(db), Arc::new(gh), &event).await.unwrap();
}

#[tokio::test]
async fn set_check_status_pr_synchronized_no_check_required() {
    // Setup mocks
    let mut db = MockDB::new();
    db.expect_has_vote().never();
    let db = Arc::new(db);
    let mut gh = MockGH::new();
    gh.expect_is_check_required()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(BRANCH))
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(false))));
    gh.expect_create_check_run().never();
    let gh = Arc::new(gh);

    // Setup the synchronized pull request event
    let mut event = setup_test_pr_event();
    event.action = PullRequestEventAction::Synchronize;

    // Run and check the result
    assert!(set_check_status(db, gh, &event).await.is_ok());
}

#[tokio::test]
async fn set_check_status_pr_synchronized_no_rule_matches_reports_success() {
    // Setup mocks: the vote is looked up before evaluating the rules and again right
    // before reporting the check
    let mut db = MockDB::new();
    db.expect_has_vote()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(2)
        .returning(|_, _| Box::pin(future::ready(Ok(false))));
    let mut gh = MockGH::new();
    expect_is_check_required(&mut gh, true);
    expect_valid_config(&mut gh);
    expect_pr_files(&mut gh, "main.rs");
    expect_check_run(&mut gh, cmd::no_vote_found_check());

    // Setup the synchronized pull request event
    let mut event = setup_test_pr_event();
    event.action = PullRequestEventAction::Synchronize;

    // Run and check the result
    set_check_status(Arc::new(db), Arc::new(gh), &event).await.unwrap();
}

#[tokio::test]
async fn set_check_status_pr_synchronized_vote_created_while_evaluating() {
    // Setup mocks: a vote is created while the automation rules are evaluated
    let mut db = MockDB::new();
    let mut seq = Sequence::new();
    for has_vote in [false, true] {
        db.expect_has_vote()
            .with(eq(REPOFN), eq(ISSUE_NUM))
            .times(1)
            .in_sequence(&mut seq)
            .returning(move |_, _| Box::pin(future::ready(Ok(has_vote))));
    }
    let mut gh = MockGH::new();
    expect_is_check_required(&mut gh, true);
    expect_no_config(&mut gh);
    gh.expect_create_check_run().never();

    // Setup the synchronized pull request event
    let mut event = setup_test_pr_event();
    event.action = PullRequestEventAction::Synchronize;

    // Run and check nothing is reported
    set_check_status(Arc::new(db), Arc::new(gh), &event).await.unwrap();
}

#[tokio::test]
async fn set_check_status_unsupported_pr_action() {
    // Setup mocks (no calls expected)
    let db = Arc::new(MockDB::new());
    let gh = Arc::new(MockGH::new());

    // Setup the pull request event with an unsupported action
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
    let err = verify_signature(Some(&signature), b"secret", None, b"body").unwrap_err();
    assert_eq!(err.to_string(), "no valid signature found");
}

#[test]
fn verify_signature_invalid_with_fallback() {
    // Setup signature
    let signature = HeaderValue::from_str(&generate_signature(b"body")).unwrap();

    // Check the signature does not match any of the secrets
    let err = verify_signature(Some(&signature), b"new-secret", Some(b"old-secret"), b"body").unwrap_err();
    assert!(err.is::<MacError>(), "unexpected error: {err}");
}

#[test]
fn verify_signature_invalid_without_fallback() {
    // Setup signature
    let signature = HeaderValue::from_str(&generate_signature(b"body")).unwrap();

    // Check the signature does not match the secret
    let err = verify_signature(Some(&signature), b"new-secret", None, b"body").unwrap_err();
    assert!(err.is::<MacError>(), "unexpected error: {err}");
}

#[test]
fn verify_signature_missing() {
    let err = verify_signature(None, b"secret", None, b"body").unwrap_err();
    assert_eq!(err.to_string(), "no valid signature found");
}

#[test]
fn verify_signature_missing_prefix() {
    // Setup signature without the prefix
    let signature = generate_signature(b"body");
    let signature = HeaderValue::from_str(signature.trim_start_matches("sha256=")).unwrap();

    // Check the signature is rejected
    let err = verify_signature(Some(&signature), b"secret", None, b"body").unwrap_err();
    assert_eq!(err.to_string(), "no valid signature found");
}

#[test]
fn verify_signature_tampered_body() {
    // Setup signature
    let signature = HeaderValue::from_str(&generate_signature(b"body")).unwrap();

    // Check the tampered body is rejected
    let err = verify_signature(Some(&signature), b"secret", None, b"tampered").unwrap_err();
    assert!(err.is::<MacError>(), "unexpected error: {err}");
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

/// Checks the response is an error with the status provided, an empty body and
/// no cache headers.
async fn assert_empty_uncached_error(response: Response<Body>, status: StatusCode) {
    assert_eq!(response.status(), status);
    assert!(!response.headers().contains_key(header::CACHE_CONTROL));
    assert!(get_body(response).await.is_empty());
}

/// Checks the normalized vote details provided display the summary value.
fn assert_summary_stat(body: &str, label: &str, value: i64) {
    let stat = format!(r#"<div class="label">{label}</div> <div class="value">{value}</div>"#);
    assert!(body.contains(&stat), "summary should include {stat}");
}

/// Expects the repository installation and configuration file to be requested
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
            Box::pin(future::ready(Ok(Some(config))))
        });
}

/// Expects the automation failed comment to be posted once, with the result provided.
fn expect_automation_failed_comment(gh: &mut MockGH, result: fn() -> Result<i64>) {
    let expected_body = tmpl::AutomationFailed::new(None).render().unwrap();
    gh.expect_post_comment()
        .withf(move |inst_id, owner, repo, issue_number, body| {
            *inst_id == INST_ID
                && owner == ORG
                && repo == REPO
                && *issue_number == ISSUE_NUM
                && body == expected_body
        })
        .times(1)
        .returning(move |_, _, _, _, _| Box::pin(future::ready(result())));
}

/// Expects the repository installation id to be requested once.
fn expect_installation_id(gh: &mut MockGH) {
    gh.expect_get_repository_installation_id()
        .with(eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(INST_ID))));
}

/// Expects the invalid test configuration file to be requested once.
fn expect_invalid_config(gh: &mut MockGH) {
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(Some(get_test_invalid_config())))));
}

/// Expects the pull request files to be requested once, returning the file provided.
fn expect_pr_files(gh: &mut MockGH, filename: &'static str) {
    gh.expect_get_pr_files()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM))
        .times(1)
        .returning(move |_, _, _, _| {
            let file = File {
                filename: filename.to_string(),
            };
            Box::pin(future::ready(Ok(vec![file])))
        });
}

/// Generates the signature of the body provided using the test webhook secret.
fn generate_signature(body: &[u8]) -> String {
    generate_signature_with_secret(b"secret", body)
}

/// Generates the signature of the body provided using the secret provided.
fn generate_signature_with_secret(secret: &[u8], body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).unwrap();
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// Reads the body of the response provided.
async fn get_body(response: Response<Body>) -> Bytes {
    to_bytes(response.into_body(), usize::MAX).await.unwrap()
}

/// Reads the body of the response provided, collapsing whitespace runs so that
/// assertions do not depend on the template indentation.
async fn get_normalized_body(response: Response<Body>) -> String {
    let body = get_body(response).await;
    let body = str::from_utf8(&body).unwrap();
    body.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Sends a GET request to the router built with the mocks provided.
async fn send_get_request(db: MockDB, gh: MockGH, uri: &str) -> Response<Body> {
    let (cmds_tx, _) = async_channel::unbounded();
    let router = setup_router(&setup_test_config(), Arc::new(db), Arc::new(gh), cmds_tx);
    router
        .oneshot(Request::builder().method("GET").uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap()
}

/// Sends a signed pull request event to the router provided.
async fn send_pr_event(router: Router, event: &PullRequestEvent) -> Response<Body> {
    let body = serde_json::to_vec(event).unwrap();
    router
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
        .unwrap()
}

/// Sets up a vote as left by the votes closer, with distinct stored results.
fn setup_test_closed_vote() -> Vote {
    let closed_at = OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap();
    Vote {
        closed: true,
        created_at: closed_at - Duration::days(1),
        ends_at: closed_at,
        issue_number: 2,
        vote_id: Uuid::parse_str(VOTE_ID2).unwrap(),

        closed_at: Some(closed_at),
        issue_title: Some("Closed vote title".to_string()),
        results: Some(VoteResults {
            abstain: 1,
            against: 2,
            against_percentage: 20.0,
            allowed_voters: 10,
            binding: 6,
            in_favor: 3,
            in_favor_percentage: 30.0,
            not_voted: 4,
            pass_threshold: 25.0,
            passed: true,
            ..setup_test_vote_results()
        }),
        ..setup_test_vote()
    }
}

/// Sets up a service configuration for tests.
fn setup_test_config() -> Cfg {
    Figment::new()
        .merge(Serialized::default("db.host", "127.0.0.1"))
        .merge(Serialized::default("github.appId", 1234))
        .merge(Serialized::default("github.appPrivateKey", "key"))
        .merge(Serialized::default("github.webhookSecret", "secret"))
        .merge(Serialized::default("addr", "127.0.0.1:9000"))
        .merge(Serialized::default("log.format", "pretty"))
        .extract()
        .unwrap()
}

/// Sets up a pull request opened event with the body provided.
fn setup_test_pr_opened_event(body: &str) -> PullRequestEvent {
    let mut event = setup_test_pr_event();
    event.action = PullRequestEventAction::Opened;
    event.pull_request.body = Some(body.to_string());
    event
}

/// Sets up a router with default mocks, returning it with the commands receiver.
fn setup_test_router() -> (Router, Receiver<Command>) {
    let cfg = setup_test_config();
    let db = Arc::new(MockDB::new());
    let gh = Arc::new(MockGH::new());
    let (cmds_tx, cmds_rx) = async_channel::unbounded();
    (setup_router(&cfg, db, gh, cmds_tx), cmds_rx)
}

/// Sets up a router with the mocks provided, returning it with the commands receiver.
fn setup_test_router_with_mocks(db: MockDB, gh: MockGH) -> (Router, Receiver<Command>) {
    let (cmds_tx, cmds_rx) = async_channel::unbounded();
    let router = setup_router(&setup_test_config(), Arc::new(db), Arc::new(gh), cmds_tx);
    (router, cmds_rx)
}

/// Sets up the check reported when a rule matches a pull request with no vote.
fn setup_test_rule_matched_check() -> CheckDetails {
    CheckDetails {
        status: "completed".to_string(),
        summary: "An automation rule matches but there is no vote, maintainers should run /vote".to_string(),

        conclusion: Some("failure".to_string()),
    }
}
