use anyhow::format_err;
use axum::http::{Method, StatusCode};
use serde_json::{Value, json};

use crate::testutil::*;

use super::*;

#[tokio::test]
async fn is_not_found_error_github_not_found() {
    assert!(is_not_found_error(&setup_test_not_found_error().await));
}

#[tokio::test]
async fn is_not_found_error_github_not_found_other_message() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::GET,
        "/empty",
        StatusCode::NOT_FOUND,
        json!({"message": "This repository is empty."}),
    );
    let client = Octocrab::builder().base_uri(api.base_uri()).unwrap().build().unwrap();

    // Get GitHub error and check it is a not found error
    let err: Error = client.get::<Value, _, ()>("/empty", None).await.unwrap_err().into();
    assert!(is_not_found_error(&err));
}

#[test]
fn is_not_found_error_other_error() {
    assert!(!is_not_found_error(&format_err!("Not Found")));
}

#[tokio::test]
async fn is_not_found_error_other_github_error() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::GET,
        "/invalid",
        StatusCode::UNPROCESSABLE_ENTITY,
        json!({"message": "Validation Failed"}),
    );
    let client = Octocrab::builder().base_uri(api.base_uri()).unwrap().build().unwrap();

    // Get GitHub error and check it is not a not found error
    let err: Error = client.get::<Value, _, ()>("/invalid", None).await.unwrap_err().into();
    assert!(!is_not_found_error(&err));
}

#[test]
fn split_full_name_extra_parts_are_ignored() {
    assert_eq!(split_full_name("org/repo/extra"), ("org", "repo"));
}

#[test]
fn split_full_name_owner_and_repo() {
    assert_eq!(split_full_name(REPOFN), (ORG, REPO));
}
