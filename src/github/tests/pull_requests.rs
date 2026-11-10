use axum::http::{Method, StatusCode};
use serde_json::json;

use crate::testutil::*;

use super::*;

#[tokio::test]
async fn gh_api_get_pr_files() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::GET,
        "/repos/org/repo/pulls/1/files",
        StatusCode::OK,
        json!([{"filename": "README.md"}, {"filename": "src/main.rs"}]),
    );
    let gh = setup_test_gh_api(&api);

    // Get pull request files and check the result
    assert_eq!(
        gh.get_pr_files(INST_ID, ORG, REPO, ISSUE_NUM).await.unwrap(),
        vec![
            File {
                filename: "README.md".to_string()
            },
            File {
                filename: "src/main.rs".to_string()
            }
        ]
    );
}

#[tokio::test]
async fn gh_api_get_pr_files_all_pages() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond_with_next_page(
        Method::GET,
        "/repos/org/repo/pulls/1/files",
        json!([{"filename": "README.md"}]),
        "/files-page-2",
    );
    api.respond(
        Method::GET,
        "/files-page-2",
        StatusCode::OK,
        json!([{"filename": "src/main.rs"}]),
    );
    let gh = setup_test_gh_api(&api);

    // Get pull request files and check all pages were collected
    assert_eq!(
        gh.get_pr_files(INST_ID, ORG, REPO, ISSUE_NUM).await.unwrap(),
        vec![
            File {
                filename: "README.md".to_string()
            },
            File {
                filename: "src/main.rs".to_string()
            }
        ]
    );
}

#[tokio::test]
async fn gh_api_get_pr_head_sha() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(Method::GET, "/repos/org/repo/pulls/1", StatusCode::OK, pr_json());
    let gh = setup_test_gh_api(&api);

    // Get the head commit and check it is the one in the pull request
    let head_sha = gh.get_pr_head_sha(INST_ID, ORG, REPO, ISSUE_NUM).await.unwrap();
    assert_eq!(head_sha, HEAD_SHA);
}

#[tokio::test]
async fn gh_api_get_pr_head_sha_pr_without_head() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::GET,
        "/repos/org/repo/pulls/1",
        StatusCode::OK,
        json!({"number": 1}),
    );
    let gh = setup_test_gh_api(&api);

    // Get the head commit and check the error returned
    let err = gh.get_pr_head_sha(INST_ID, ORG, REPO, ISSUE_NUM).await.unwrap_err();
    assert_eq!(err.to_string(), "pull request response missing head");
}
