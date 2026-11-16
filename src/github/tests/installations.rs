use axum::http::{Method, StatusCode};
use serde_json::json;

use crate::testutil::*;

use super::*;

#[tokio::test]
async fn gh_api_get_repository_installation_id() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::GET,
        "/repos/org/repo/installation",
        StatusCode::OK,
        json!({"id": INST_ID, "account": author_json(ORG), "permissions": {}, "events": []}),
    );
    let gh = setup_test_gh_api(&api);

    // Get installation id and check the result
    assert_eq!(
        gh.get_repository_installation_id(ORG, REPO).await.unwrap(),
        INST_ID
    );
}

#[tokio::test]
async fn gh_api_get_repository_installation_id_not_found() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    let gh = setup_test_gh_api(&api);

    // Get installation id and check a not found error is returned
    let err = gh.get_repository_installation_id(ORG, REPO).await.unwrap_err();
    assert!(is_not_found_error(&err));
}

#[tokio::test]
async fn gh_api_get_repository_installation_id_uses_app_jwt() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::GET,
        "/repos/org/repo/installation",
        StatusCode::OK,
        json!({"id": INST_ID, "account": author_json(ORG), "permissions": {}, "events": []}),
    );
    let gh = setup_test_gh_api(&api);

    // Get installation id
    gh.get_repository_installation_id(ORG, REPO).await.unwrap();

    // Check the application JWT was used and no installation token was requested
    let requests = api.requests(&Method::GET, "/repos/org/repo/installation");
    let authorization = requests[0].authorization.as_ref().unwrap();
    assert!(authorization.starts_with("Bearer ey"));
    let token_path = installation_token_path();
    assert_eq!(api.requests(&Method::POST, &token_path), vec![]);
}
