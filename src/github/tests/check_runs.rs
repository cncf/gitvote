use axum::http::{Method, StatusCode};
use serde_json::json;

use crate::testutil::*;

use super::*;

/// Path of the requests creating check runs in the test repository.
const CHECK_RUNS_PATH: &str = "/repos/org/repo/check-runs";

#[tokio::test]
async fn gh_api_create_check_run_completed() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(Method::GET, "/repos/org/repo/pulls/1", StatusCode::OK, pr_json());
    api.respond(
        Method::POST,
        "/repos/org/repo/check-runs",
        StatusCode::CREATED,
        json!({}),
    );
    let gh = setup_test_gh_api(&api);

    // Create check run
    let check_details = CheckDetails {
        status: "completed".to_string(),
        summary: "Vote passed".to_string(),

        conclusion: Some("success".to_string()),
    };
    gh.create_check_run(INST_ID, ORG, REPO, ISSUE_NUM, &check_details).await.unwrap();

    // Check the request sent
    let requests = api.requests(&Method::POST, "/repos/org/repo/check-runs");
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].body,
        Some(json!({
            "head_sha": "abc123",
            "name": "GitVote",
            "output": {
                "summary": "Vote passed",
                "title": "Vote passed"
            },
            "status": "completed",
            "conclusion": "success"
        }))
    );
}

#[tokio::test]
async fn gh_api_create_check_run_in_progress() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(Method::GET, "/repos/org/repo/pulls/1", StatusCode::OK, pr_json());
    api.respond(
        Method::POST,
        "/repos/org/repo/check-runs",
        StatusCode::CREATED,
        json!({}),
    );
    let gh = setup_test_gh_api(&api);

    // Create check run
    let check_details = CheckDetails {
        status: "in_progress".to_string(),
        summary: "Vote open".to_string(),

        conclusion: None,
    };
    gh.create_check_run(INST_ID, ORG, REPO, ISSUE_NUM, &check_details).await.unwrap();

    // Check the request sent does not include a conclusion
    let requests = api.requests(&Method::POST, "/repos/org/repo/check-runs");
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].body,
        Some(json!({
            "head_sha": "abc123",
            "name": "GitVote",
            "output": {
                "summary": "Vote open",
                "title": "Vote open"
            },
            "status": "in_progress"
        }))
    );
}

#[tokio::test]
async fn gh_api_create_check_run_is_retried_after_server_error() {
    for status in [StatusCode::BAD_GATEWAY, StatusCode::TOO_MANY_REQUESTS] {
        // Setup mock GitHub API, which accepts the check run when it is sent again
        let api = MockGitHubApi::start().await;
        api.respond(Method::GET, "/repos/org/repo/pulls/1", StatusCode::OK, pr_json());
        api.respond(
            Method::POST,
            CHECK_RUNS_PATH,
            status,
            json!({"message": "Failed"}),
        );
        api.respond(Method::POST, CHECK_RUNS_PATH, StatusCode::CREATED, json!({}));
        let gh = setup_test_gh_api(&api);

        // Create check run, checking the failure is not returned
        let check_details = CheckDetails {
            status: "in_progress".to_string(),
            summary: "Vote open".to_string(),

            conclusion: None,
        };
        gh.create_check_run(INST_ID, ORG, REPO, ISSUE_NUM, &check_details).await.unwrap();

        // Check the check run was sent again
        assert_eq!(api.requests(&Method::POST, CHECK_RUNS_PATH).len(), 2, "{status}");
    }
}

#[tokio::test]
async fn gh_api_create_check_run_is_retried_after_token_server_error() {
    // Setup mock GitHub API, whose first installation token request fails
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::POST,
        &installation_token_path(),
        StatusCode::BAD_GATEWAY,
        json!({"message": "Failed"}),
    );
    api.respond(Method::GET, "/repos/org/repo/pulls/1", StatusCode::OK, pr_json());
    api.respond(Method::POST, CHECK_RUNS_PATH, StatusCode::CREATED, json!({}));
    let gh = setup_test_gh_api(&api);

    // Create check run
    let check_details = CheckDetails {
        status: "in_progress".to_string(),
        summary: "Vote open".to_string(),

        conclusion: None,
    };
    gh.create_check_run(INST_ID, ORG, REPO, ISSUE_NUM, &check_details).await.unwrap();

    // Check the token was requested again, and that one token is used for the whole call
    assert_eq!(api.requests(&Method::POST, &installation_token_path()).len(), 2);
    assert_eq!(api.requests(&Method::POST, CHECK_RUNS_PATH).len(), 1);
}

#[tokio::test]
async fn gh_api_create_check_run_pr_without_head() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::GET,
        "/repos/org/repo/pulls/1",
        StatusCode::OK,
        json!({"number": 1}),
    );
    let gh = setup_test_gh_api(&api);

    // Create check run
    let check_details = CheckDetails {
        status: "in_progress".to_string(),
        summary: "Vote open".to_string(),

        conclusion: None,
    };
    let err = gh.create_check_run(INST_ID, ORG, REPO, ISSUE_NUM, &check_details).await.unwrap_err();

    // Check the error returned and that no check run was created
    assert_eq!(err.to_string(), "pull request response missing head");
    assert_eq!(api.requests(&Method::POST, "/repos/org/repo/check-runs"), vec![]);
}

#[tokio::test]
async fn gh_api_create_commit_check_run() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::POST,
        "/repos/org/repo/check-runs",
        StatusCode::CREATED,
        json!({}),
    );
    let gh = setup_test_gh_api(&api);

    // Create check run
    let check_details = CheckDetails {
        status: "completed".to_string(),
        summary: "Vote passed".to_string(),

        conclusion: Some("success".to_string()),
    };
    gh.create_commit_check_run(INST_ID, ORG, REPO, "def456", &check_details).await.unwrap();

    // Check the request sent uses the commit provided, without reading the pull request
    let requests = api.requests(&Method::POST, "/repos/org/repo/check-runs");
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].body,
        Some(json!({
            "head_sha": "def456",
            "name": "GitVote",
            "output": {
                "summary": "Vote passed",
                "title": "Vote passed"
            },
            "status": "completed",
            "conclusion": "success"
        }))
    );
    assert_eq!(api.requests(&Method::GET, "/repos/org/repo/pulls/1"), vec![]);
}

#[tokio::test]
async fn gh_api_is_check_required() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::GET,
        "/repos/org/repo/branches/main",
        StatusCode::OK,
        json!({
            "name": BRANCH,
            "protection": {"required_status_checks": {"contexts": ["ci", "GitVote"]}}
        }),
    );
    let gh = setup_test_gh_api(&api);

    // Check the GitVote check is required
    assert!(gh.is_check_required(INST_ID, ORG, REPO, BRANCH).await.unwrap());
}

#[tokio::test]
async fn gh_api_is_check_required_encodes_branch() {
    // Setup cases (branch, expected path)
    let test_cases = [
        ("fix#1", "/repos/org/repo/branches/fix%231"),
        ("fix?1", "/repos/org/repo/branches/fix%3F1"),
        ("100%", "/repos/org/repo/branches/100%25"),
        ("feat+añadir", "/repos/org/repo/branches/feat%2Ba%C3%B1adir"),
        ("release/1.0", "/repos/org/repo/branches/release/1.0"),
        ("user_a/fix-1~", "/repos/org/repo/branches/user_a/fix-1~"),
    ];
    for (branch, path) in test_cases {
        // Setup mock GitHub API
        let api = MockGitHubApi::start().await;
        api.respond(
            Method::GET,
            path,
            StatusCode::OK,
            json!({
                "name": branch,
                "protection": {"required_status_checks": {"contexts": ["GitVote"]}}
            }),
        );
        let gh = setup_test_gh_api(&api);

        // Check the branch requested is the one provided
        assert!(
            gh.is_check_required(INST_ID, ORG, REPO, branch).await.unwrap(),
            "{branch}"
        );
    }
}

#[tokio::test]
async fn gh_api_is_check_required_other_checks_required() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::GET,
        "/repos/org/repo/branches/main",
        StatusCode::OK,
        json!({
            "name": BRANCH,
            "protection": {"required_status_checks": {"contexts": ["ci"]}}
        }),
    );
    let gh = setup_test_gh_api(&api);

    // Check the GitVote check is not required
    assert!(!gh.is_check_required(INST_ID, ORG, REPO, BRANCH).await.unwrap());
}

#[tokio::test]
async fn gh_api_is_check_required_unprotected_branch() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::GET,
        "/repos/org/repo/branches/main",
        StatusCode::OK,
        json!({"name": BRANCH, "protection": null}),
    );
    let gh = setup_test_gh_api(&api);

    // Check the GitVote check is not required
    assert!(!gh.is_check_required(INST_ID, ORG, REPO, BRANCH).await.unwrap());
}
