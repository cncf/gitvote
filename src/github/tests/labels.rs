use axum::http::{Method, StatusCode};
use serde_json::json;

use crate::testutil::*;

use super::*;

/// Path of the requests adding labels to the test issue.
const LABELS_PATH: &str = "/repos/org/repo/issues/1/labels";

#[tokio::test]
async fn gh_api_add_labels() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::POST,
        "/repos/org/repo/issues/1/labels",
        StatusCode::OK,
        json!([]),
    );
    let gh = setup_test_gh_api(&api);

    // Add labels
    gh.add_labels(INST_ID, ORG, REPO, ISSUE_NUM, &["gitvote", "gitvote/open"]).await.unwrap();

    // Check the request sent
    let requests = api.requests(&Method::POST, "/repos/org/repo/issues/1/labels");
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].body,
        Some(json!({"labels": ["gitvote", "gitvote/open"]}))
    );
}

#[tokio::test]
async fn gh_api_add_labels_is_retried_after_server_error() {
    for status in [StatusCode::BAD_GATEWAY, StatusCode::TOO_MANY_REQUESTS] {
        // Setup mock GitHub API, which accepts the labels when they are sent again
        let api = MockGitHubApi::start().await;
        api.respond(Method::POST, LABELS_PATH, status, json!({"message": "Failed"}));
        api.respond(Method::POST, LABELS_PATH, StatusCode::OK, json!([]));
        let gh = setup_test_gh_api(&api);

        // Check the failure is not returned and the labels were sent again
        gh.add_labels(INST_ID, ORG, REPO, ISSUE_NUM, &["gitvote"]).await.unwrap();
        assert_eq!(api.requests(&Method::POST, LABELS_PATH).len(), 2, "{status}");
    }
}

#[tokio::test]
async fn gh_api_add_labels_is_retried_after_token_server_error() {
    // Setup mock GitHub API, whose first installation token request fails
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::POST,
        &installation_token_path(),
        StatusCode::BAD_GATEWAY,
        json!({"message": "Failed"}),
    );
    api.respond(Method::POST, LABELS_PATH, StatusCode::OK, json!([]));
    let gh = setup_test_gh_api(&api);

    // Check the token was requested again and the labels were sent
    gh.add_labels(INST_ID, ORG, REPO, ISSUE_NUM, &["gitvote"]).await.unwrap();
    assert_eq!(api.requests(&Method::POST, &installation_token_path()).len(), 2);
    assert_eq!(api.requests(&Method::POST, LABELS_PATH).len(), 1);
}

#[tokio::test]
async fn gh_api_remove_label() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::DELETE,
        "/repos/org/repo/issues/1/labels/gitvote%2Fopen",
        StatusCode::OK,
        json!([]),
    );
    let gh = setup_test_gh_api(&api);

    // Remove label
    gh.remove_label(INST_ID, ORG, REPO, ISSUE_NUM, "gitvote/open").await.unwrap();

    // Check the request sent
    assert_eq!(
        api.requests(&Method::DELETE, "/repos/org/repo/issues/1/labels/gitvote%2Fopen").len(),
        1
    );
}

#[tokio::test]
async fn gh_api_remove_label_error() {
    // Setup mock GitHub API (label request is not found)
    let api = MockGitHubApi::start().await;
    let gh = setup_test_gh_api(&api);

    // Remove label and check the error is propagated
    let err = gh.remove_label(INST_ID, ORG, REPO, ISSUE_NUM, "gitvote/open").await.unwrap_err();
    assert!(is_not_found_error(&err));
}

#[tokio::test]
async fn gh_api_remove_label_is_retried_after_server_error() {
    // Setup mock GitHub API, which accepts the deletion when it is sent again
    let path = "/repos/org/repo/issues/1/labels/gitvote%2Fopen";
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::DELETE,
        path,
        StatusCode::BAD_GATEWAY,
        json!({"message": "Failed"}),
    );
    api.respond(Method::DELETE, path, StatusCode::OK, json!([]));
    let gh = setup_test_gh_api(&api);

    // Check the failure is not returned and the deletion was sent again
    gh.remove_label(INST_ID, ORG, REPO, ISSUE_NUM, "gitvote/open").await.unwrap();
    assert_eq!(api.requests(&Method::DELETE, path).len(), 2);
}

#[tokio::test]
async fn gh_api_remove_label_that_does_not_exist() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::DELETE,
        "/repos/org/repo/issues/1/labels/gitvote%2Fopen",
        StatusCode::NOT_FOUND,
        json!({"message": "Label does not exist"}),
    );
    let gh = setup_test_gh_api(&api);

    // Remove label and check the error is ignored
    gh.remove_label(INST_ID, ORG, REPO, ISSUE_NUM, "gitvote/open").await.unwrap();
}
