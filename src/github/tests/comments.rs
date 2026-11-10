use axum::http::{Method, StatusCode};
use serde_json::{Value, json};

use crate::testutil::*;

use super::*;

/// Path of the requests creating comments in the test issue.
const COMMENTS_PATH: &str = "/repos/org/repo/issues/1/comments";

#[tokio::test]
async fn gh_api_delete_comment() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::DELETE,
        "/repos/org/repo/issues/comments/3003",
        StatusCode::NO_CONTENT,
        json!(null),
    );
    let gh = setup_test_gh_api(&api);

    // Delete comment and check the request
    gh.delete_comment(INST_ID, ORG, REPO, COMMENT_ID).await.unwrap();
    assert_eq!(
        api.requests(&Method::DELETE, "/repos/org/repo/issues/comments/3003").len(),
        1
    );
}

#[tokio::test]
async fn gh_api_delete_comment_error() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::DELETE,
        "/repos/org/repo/issues/comments/3003",
        StatusCode::FORBIDDEN,
        json!({"message": "Forbidden"}),
    );
    let gh = setup_test_gh_api(&api);

    // Delete comment and check the error is propagated
    let err = gh.delete_comment(INST_ID, ORG, REPO, COMMENT_ID).await.unwrap_err();
    assert!(format!("{err:#}").contains("Forbidden"));
}

#[tokio::test]
async fn gh_api_delete_comment_is_retried_after_server_error() {
    // Setup mock GitHub API, which accepts the deletion when it is sent again
    let path = "/repos/org/repo/issues/comments/3003";
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::DELETE,
        path,
        StatusCode::BAD_GATEWAY,
        json!({"message": "Failed"}),
    );
    api.respond(Method::DELETE, path, StatusCode::NO_CONTENT, json!(null));
    let gh = setup_test_gh_api(&api);

    // Check the failure is not returned and the deletion was sent again
    gh.delete_comment(INST_ID, ORG, REPO, COMMENT_ID).await.unwrap();
    assert_eq!(api.requests(&Method::DELETE, path).len(), 2);
}

#[tokio::test]
async fn gh_api_has_comment_with_marker() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::GET,
        COMMENTS_PATH,
        StatusCode::OK,
        json!([
            issue_comment_json("comment", Some(TEST_APP_ID)),
            issue_comment_json(&format!("results\n{MARKER}\n"), Some(TEST_APP_ID)),
        ]),
    );
    let gh = setup_test_gh_api(&api);

    // Check the comment is found, ignoring the whitespace after the marker
    let since = OffsetDateTime::parse("2022-11-30T10:00:00.5Z", &Rfc3339).unwrap();
    assert!(gh.has_comment_with_marker(INST_ID, ORG, REPO, ISSUE_NUM, MARKER, since).await.unwrap());

    // Check the comments were listed since the time provided, in whole seconds
    let requests = api.requests(&Method::GET, COMMENTS_PATH);
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].authorization,
        Some(format!("Bearer {INSTALLATION_TOKEN}"))
    );
    assert_eq!(
        requests[0].query.as_deref(),
        Some("per_page=100&since=2022-11-30T10%3A00%3A00Z")
    );
}

#[tokio::test]
async fn gh_api_has_comment_with_marker_all_pages() {
    // Setup mock GitHub API, with the comment on the second page
    let api = MockGitHubApi::start().await;
    api.respond_with_next_page(
        Method::GET,
        COMMENTS_PATH,
        json!([issue_comment_json("comment", Some(TEST_APP_ID))]),
        "/comments-page-2",
    );
    api.respond(
        Method::GET,
        "/comments-page-2",
        StatusCode::OK,
        json!([issue_comment_json(MARKER, Some(TEST_APP_ID))]),
    );
    let gh = setup_test_gh_api(&api);

    // Check the comment is found
    let since = OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap();
    assert!(gh.has_comment_with_marker(INST_ID, ORG, REPO, ISSUE_NUM, MARKER, since).await.unwrap());

    // Check the second page was requested once
    assert_eq!(api.requests(&Method::GET, "/comments-page-2").len(), 1);
}

#[tokio::test]
async fn gh_api_has_comment_with_marker_error_on_next_page() {
    // Setup mock GitHub API, which fails on the second page
    let api = MockGitHubApi::start().await;
    api.respond_with_next_page(
        Method::GET,
        COMMENTS_PATH,
        json!([issue_comment_json("comment", Some(TEST_APP_ID))]),
        "/comments-page-2",
    );
    api.respond(
        Method::GET,
        "/comments-page-2",
        StatusCode::FORBIDDEN,
        json!({"message": "Forbidden"}),
    );
    let gh = setup_test_gh_api(&api);

    // Check the failure is not taken for the comment not being found
    let since = OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap();
    let err = gh
        .has_comment_with_marker(INST_ID, ORG, REPO, ISSUE_NUM, MARKER, since)
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "error listing issue comments");
    assert!(format!("{err:#}").contains("Forbidden"), "{err:#}");
}

#[tokio::test]
async fn gh_api_has_comment_with_marker_errors() {
    // Setup cases (status, expected requests)
    let test_cases = [
        (StatusCode::FORBIDDEN, 1),
        (StatusCode::NOT_FOUND, 1),
        (StatusCode::BAD_GATEWAY, 4),
    ];
    for (status, expected_requests) in test_cases {
        // Setup mock GitHub API
        let api = MockGitHubApi::start().await;
        api.respond(
            Method::GET,
            COMMENTS_PATH,
            status,
            json!({"message": "Lookup failed"}),
        );
        let gh = setup_test_gh_api(&api);

        // Check the failure is not taken for the comment not being found
        let since = OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap();
        let err = gh
            .has_comment_with_marker(INST_ID, ORG, REPO, ISSUE_NUM, MARKER, since)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "error listing issue comments", "{status}");
        assert!(format!("{err:#}").contains("Lookup failed"), "{status}: {err:#}");

        // Check the requests sent, as the client retries server errors
        assert_eq!(
            api.requests(&Method::GET, COMMENTS_PATH).len(),
            expected_requests,
            "{status}"
        );
    }
}

#[tokio::test]
async fn gh_api_has_comment_with_marker_ignores_comments_not_posted_by_the_app() {
    // Setup mock GitHub API, with the marker in comments posted by a user and by another
    // application
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::GET,
        COMMENTS_PATH,
        StatusCode::OK,
        json!([
            issue_comment_json(MARKER, None),
            issue_comment_json(MARKER, Some(TEST_APP_ID + 1)),
        ]),
    );
    let gh = setup_test_gh_api(&api);

    // Check the comment is not found
    let since = OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap();
    assert!(!gh.has_comment_with_marker(INST_ID, ORG, REPO, ISSUE_NUM, MARKER, since).await.unwrap());
}

#[tokio::test]
async fn gh_api_has_comment_with_marker_ignores_marker_not_at_the_end() {
    // Setup mock GitHub API, with the marker in the middle of a comment of the app, as
    // when an issue title with the marker is quoted in the vote created comment
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::GET,
        COMMENTS_PATH,
        StatusCode::OK,
        json!([issue_comment_json(
            &format!("Vote created for `{MARKER}`\n\nPlease vote"),
            Some(TEST_APP_ID)
        )]),
    );
    let gh = setup_test_gh_api(&api);

    // Check the comment is not found
    let since = OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap();
    assert!(!gh.has_comment_with_marker(INST_ID, ORG, REPO, ISSUE_NUM, MARKER, since).await.unwrap());

    // Check the comments were listed once
    assert_eq!(api.requests(&Method::GET, COMMENTS_PATH).len(), 1);
}

#[tokio::test]
async fn gh_api_has_comment_with_marker_not_found() {
    // Setup mock GitHub API, with comments of the app without the marker or without body
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::GET,
        COMMENTS_PATH,
        StatusCode::OK,
        json!([
            issue_comment_json("comment", Some(TEST_APP_ID)),
            {"body": null, "performed_via_github_app": {"id": TEST_APP_ID}},
        ]),
    );
    let gh = setup_test_gh_api(&api);

    // Check the comment is not found
    let since = OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap();
    assert!(!gh.has_comment_with_marker(INST_ID, ORG, REPO, ISSUE_NUM, MARKER, since).await.unwrap());

    // Check the comments were listed once
    assert_eq!(api.requests(&Method::GET, COMMENTS_PATH).len(), 1);
}

#[tokio::test]
async fn gh_api_has_comment_with_marker_stops_at_the_page_with_it() {
    // Setup mock GitHub API, with the comment on the first page of two
    let api = MockGitHubApi::start().await;
    api.respond_with_next_page(
        Method::GET,
        COMMENTS_PATH,
        json!([issue_comment_json(MARKER, Some(TEST_APP_ID))]),
        "/comments-page-2",
    );
    let gh = setup_test_gh_api(&api);

    // Check the comment is found without getting the second page
    let since = OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap();
    assert!(gh.has_comment_with_marker(INST_ID, ORG, REPO, ISSUE_NUM, MARKER, since).await.unwrap());
    assert_eq!(api.requests(&Method::GET, "/comments-page-2"), vec![]);
}

#[tokio::test]
async fn gh_api_post_comment() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::POST,
        "/repos/org/repo/issues/1/comments",
        StatusCode::CREATED,
        comment_json(),
    );
    let gh = setup_test_gh_api(&api);

    // Post comment and check the comment id returned
    assert_eq!(
        gh.post_comment(INST_ID, ORG, REPO, ISSUE_NUM, "comment").await.unwrap(),
        COMMENT_ID2
    );

    // Check the comment sent
    let requests = api.requests(&Method::POST, "/repos/org/repo/issues/1/comments");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].body, Some(json!({"body": "comment"})));
}

#[tokio::test]
async fn gh_api_post_comment_is_not_resent_after_server_error() {
    // Setup cases (status)
    let test_cases = [
        StatusCode::INTERNAL_SERVER_ERROR,
        StatusCode::BAD_GATEWAY,
        StatusCode::TOO_MANY_REQUESTS,
    ];
    for status in test_cases {
        // Setup mock GitHub API, which would accept the comment if it was sent again
        let api = MockGitHubApi::start().await;
        api.respond(Method::POST, COMMENTS_PATH, status, json!({"message": "Failed"}));
        api.respond(Method::POST, COMMENTS_PATH, StatusCode::CREATED, comment_json());
        let gh = setup_test_gh_api(&api);

        // Check the failure returned is the one from GitHub, and the comment was sent
        // just once
        let err = gh.post_comment(INST_ID, ORG, REPO, ISSUE_NUM, "comment").await.unwrap_err();
        assert!(format!("{err:#}").contains("Failed"), "{status}: {err:#}");
        assert_eq!(api.requests(&Method::POST, COMMENTS_PATH).len(), 1, "{status}");
    }
}

#[tokio::test(start_paused = true)]
async fn gh_api_post_comment_is_not_resent_after_timeout() {
    // Setup mock GitHub API, which would accept the comment if it was sent again
    let api = MockGitHubApi::start().await;
    api.stall(Method::POST, COMMENTS_PATH);
    api.respond(Method::POST, COMMENTS_PATH, StatusCode::CREATED, comment_json());
    let gh = setup_test_gh_api_with_short_timeouts(&api);

    // Check the failure returned is the timeout, and the comment was sent just once
    let err = gh.post_comment(INST_ID, ORG, REPO, ISSUE_NUM, "comment").await.unwrap_err();
    assert_timeout_error(&err);
    assert_eq!(api.requests(&Method::POST, COMMENTS_PATH).len(), 1);
}

#[tokio::test]
async fn gh_api_post_comment_uses_installation_token() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::POST,
        "/repos/org/repo/issues/1/comments",
        StatusCode::FORBIDDEN,
        json!({"message": "Forbidden"}),
    );
    let gh = setup_test_gh_api(&api);

    // Post comment, whose response is not relevant here
    gh.post_comment(INST_ID, ORG, REPO, ISSUE_NUM, "comment").await.unwrap_err();

    // Check the token was requested with the application JWT
    let token_path = installation_token_path();
    let token_requests = api.requests(&Method::POST, &token_path);
    assert_eq!(token_requests.len(), 1);
    let authorization = token_requests[0].authorization.as_ref().unwrap();
    assert!(authorization.starts_with("Bearer ey"));

    // Check the comment was posted with the installation token
    let requests = api.requests(&Method::POST, "/repos/org/repo/issues/1/comments");
    assert_eq!(
        requests[0].authorization,
        Some(format!("Bearer {INSTALLATION_TOKEN}"))
    );
}

// Helpers.

/// Builds a GitHub comment JSON object.
fn comment_json() -> Value {
    json!({
        "id": COMMENT_ID2,
        "node_id": "IC_1",
        "url": "https://api.github.com/repos/org/repo/issues/comments/5678",
        "html_url": "https://github.com/org/repo/issues/1#issuecomment-5678",
        "body": "comment",
        "user": author_json(USER),
        "created_at": TIMESTAMP
    })
}

/// Builds a GitHub issue comment JSON object with the body provided, posted by the
/// application given (if any), including only the fields the client reads.
fn issue_comment_json(body: &str, app_id: Option<u64>) -> Value {
    json!({
        "body": body,
        "performed_via_github_app": app_id.map(|id| json!({"id": id, "slug": "gitvote"}))
    })
}
