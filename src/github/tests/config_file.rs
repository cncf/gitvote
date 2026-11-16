use axum::http::{Method, StatusCode};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

use crate::testutil::*;

use super::*;

#[tokio::test]
async fn gh_api_get_config_file_error_stops_the_lookup() {
    // Setup cases (failing path, status, expected location in the error)
    let test_cases = [
        (
            "/repos/org/repo/contents/.gitvote.yml",
            StatusCode::FORBIDDEN,
            "org/repo/.gitvote.yml",
        ),
        (
            "/repos/org/repo/contents/.gitvote.yml",
            StatusCode::INTERNAL_SERVER_ERROR,
            "org/repo/.gitvote.yml",
        ),
        (
            "/repos/org/repo/contents/.github/.gitvote.yml",
            StatusCode::SERVICE_UNAVAILABLE,
            "org/repo/.github/.gitvote.yml",
        ),
        (
            "/repos/org/.github/contents/.gitvote.yml",
            StatusCode::SERVICE_UNAVAILABLE,
            "org/.github/.gitvote.yml",
        ),
    ];
    for (failing_path, status, location) in test_cases {
        // Setup mock GitHub API, keeping the org config when it is not the one failing
        let api = MockGitHubApi::start().await;
        let org_path = "/repos/org/.github/contents/.gitvote.yml";
        if failing_path != org_path {
            api.respond(
                Method::GET,
                org_path,
                StatusCode::OK,
                content_json(".gitvote.yml", "org-repo"),
            );
        }
        api.respond(
            Method::GET,
            failing_path,
            status,
            json!({"message": "Lookup failed"}),
        );
        let gh = setup_test_gh_api(&api);

        // Get config file and check the failure is not taken for a missing file
        let err = gh.get_config_file(INST_ID, ORG, REPO).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("error getting {location}"),
            "{failing_path}"
        );
        assert!(format!("{err:#}").contains("Lookup failed"), "{failing_path}");

        // Check the org config was not used instead
        if failing_path != org_path {
            assert_eq!(api.requests(&Method::GET, org_path), vec![], "{failing_path}");
        }
    }
}

#[tokio::test]
async fn gh_api_get_config_file_from_github_directory() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::GET,
        "/repos/org/repo/contents/.github/.gitvote.yml",
        StatusCode::OK,
        content_json(".github/.gitvote.yml", "github-dir"),
    );
    let gh = setup_test_gh_api(&api);

    // Get config file and check the content returned
    assert_eq!(
        gh.get_config_file(INST_ID, ORG, REPO).await.unwrap(),
        Some("github-dir".to_string())
    );

    // Check the org repository was not requested
    assert_eq!(
        api.requests(&Method::GET, "/repos/org/.github/contents/.gitvote.yml"),
        vec![]
    );
}

#[tokio::test]
async fn gh_api_get_config_file_from_org_repository() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::GET,
        "/repos/org/.github/contents/.gitvote.yml",
        StatusCode::OK,
        content_json(".gitvote.yml", "org-repo"),
    );
    let gh = setup_test_gh_api(&api);

    // Get config file and check the content returned
    assert_eq!(
        gh.get_config_file(INST_ID, ORG, REPO).await.unwrap(),
        Some("org-repo".to_string())
    );
}

#[tokio::test]
async fn gh_api_get_config_file_from_repository_root() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::GET,
        "/repos/org/repo/contents/.gitvote.yml",
        StatusCode::OK,
        content_json(".gitvote.yml", "repo-root"),
    );
    api.respond(
        Method::GET,
        "/repos/org/repo/contents/.github/.gitvote.yml",
        StatusCode::OK,
        content_json(".github/.gitvote.yml", "github-dir"),
    );
    let gh = setup_test_gh_api(&api);

    // Get config file and check the repository root takes precedence
    assert_eq!(
        gh.get_config_file(INST_ID, ORG, REPO).await.unwrap(),
        Some("repo-root".to_string())
    );

    // Check the .github directory was not requested
    assert_eq!(
        api.requests(&Method::GET, "/repos/org/repo/contents/.github/.gitvote.yml"),
        vec![]
    );
}

#[tokio::test]
async fn gh_api_get_config_file_moves_to_next_location_on_any_404() {
    // Setup mock GitHub API, using the 404 GitHub returns for empty repositories
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::GET,
        "/repos/org/repo/contents/.gitvote.yml",
        StatusCode::NOT_FOUND,
        json!({"message": "This repository is empty."}),
    );
    api.respond(
        Method::GET,
        "/repos/org/.github/contents/.gitvote.yml",
        StatusCode::OK,
        content_json(".gitvote.yml", "org-repo"),
    );
    let gh = setup_test_gh_api(&api);

    // Get config file and check the content returned
    assert_eq!(
        gh.get_config_file(INST_ID, ORG, REPO).await.unwrap(),
        Some("org-repo".to_string())
    );
}

#[tokio::test]
async fn gh_api_get_config_file_not_found() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    let gh = setup_test_gh_api(&api);

    // Get config file and check all locations were tried
    assert_eq!(gh.get_config_file(INST_ID, ORG, REPO).await.unwrap(), None);
    assert_eq!(
        api.requests(&Method::GET, "/repos/org/repo/contents/.gitvote.yml").len(),
        1
    );
    assert_eq!(
        api.requests(&Method::GET, "/repos/org/repo/contents/.github/.gitvote.yml").len(),
        1
    );
    assert_eq!(
        api.requests(&Method::GET, "/repos/org/.github/contents/.gitvote.yml").len(),
        1
    );
}

#[tokio::test]
async fn gh_api_get_config_file_skips_directories() {
    // Setup mock GitHub API, listing a directory where the file is expected
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::GET,
        "/repos/org/repo/contents/.gitvote.yml",
        StatusCode::OK,
        json!([
            content_json(".gitvote.yml/first", "first"),
            content_json(".gitvote.yml/second", "second")
        ]),
    );
    api.respond(
        Method::GET,
        "/repos/org/repo/contents/.github/.gitvote.yml",
        StatusCode::OK,
        content_json(".github/.gitvote.yml", "github-dir"),
    );
    let gh = setup_test_gh_api(&api);

    // Check the next location is used
    assert_eq!(
        gh.get_config_file(INST_ID, ORG, REPO).await.unwrap(),
        Some("github-dir".to_string())
    );
}

#[tokio::test]
async fn gh_api_get_config_file_token_not_found() {
    // Setup mock GitHub API, which returns a 404 for the first installation token
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::POST,
        &installation_token_path(),
        StatusCode::NOT_FOUND,
        json!({"message": "Not Found"}),
    );
    let gh = setup_test_gh_api(&api);

    // Get config file and check the failure is not taken for a missing file
    let err = gh.get_config_file(INST_ID, ORG, REPO).await.unwrap_err();
    assert!(is_not_found_error(&err));

    // Check the config file was not requested
    assert_eq!(
        api.requests(&Method::GET, "/repos/org/repo/contents/.gitvote.yml"),
        vec![]
    );
}

// Helpers.

/// Builds a GitHub file content JSON object for the path and content provided.
fn content_json(path: &str, content: &str) -> Value {
    let url = format!("https://api.github.com/repos/org/repo/contents/{path}");
    json!({
        "name": ".gitvote.yml",
        "path": path,
        "sha": "sha",
        "encoding": "base64",
        "content": STANDARD.encode(content),
        "size": content.len(),
        "url": url,
        "html_url": null,
        "git_url": null,
        "download_url": null,
        "type": "file",
        "_links": {"self": url},
        "license": null
    })
}
