use axum::http::{Method, StatusCode};
use serde_json::json;

use crate::testutil::*;

use super::*;

#[tokio::test]
async fn gh_api_get_comment_reactions() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::GET,
        "/repos/org/repo/issues/comments/3003/reactions",
        StatusCode::OK,
        json!([{"content": "+1", "created_at": TIMESTAMP, "user": {"login": USER1}}]),
    );
    let gh = setup_test_gh_api(&api);

    // Get reactions and check the result
    assert_eq!(
        gh.get_comment_reactions(INST_ID, ORG, REPO, COMMENT_ID).await.unwrap(),
        vec![Reaction {
            content: "+1".to_string(),
            created_at: TIMESTAMP.to_string(),
            user: User {
                login: USER1.to_string()
            },
        }]
    );

    // Check the installation token was only requested once
    let token_path = installation_token_path();
    assert_eq!(api.requests(&Method::POST, &token_path).len(), 1);
}

#[tokio::test]
async fn gh_api_get_comment_reactions_all_pages() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond_with_next_page(
        Method::GET,
        "/repos/org/repo/issues/comments/3003/reactions",
        json!([{"content": "+1", "created_at": TIMESTAMP, "user": {"login": USER1}}]),
        "/reactions-page-2",
    );
    api.respond(
        Method::GET,
        "/reactions-page-2",
        StatusCode::OK,
        json!([{"content": "-1", "created_at": TIMESTAMP, "user": {"login": USER2}}]),
    );
    let gh = setup_test_gh_api(&api);

    // Get reactions and check all pages were collected in order
    assert_eq!(
        gh.get_comment_reactions(INST_ID, ORG, REPO, COMMENT_ID).await.unwrap(),
        vec![
            Reaction {
                content: "+1".to_string(),
                created_at: TIMESTAMP.to_string(),
                user: User {
                    login: USER1.to_string()
                },
            },
            Reaction {
                content: "-1".to_string(),
                created_at: TIMESTAMP.to_string(),
                user: User {
                    login: USER2.to_string()
                },
            }
        ]
    );
}

#[tokio::test]
async fn gh_api_get_comment_reactions_comment_not_found() {
    // Setup mock GitHub API, which returns a 404 for the reactions
    let api = MockGitHubApi::start().await;
    let gh = setup_test_gh_api(&api);

    // Get reactions and check the comment is reported as not found
    let err = gh.get_comment_reactions(INST_ID, ORG, REPO, COMMENT_ID).await.unwrap_err();
    assert!(err.is::<CommentNotFoundError>());
}

#[tokio::test]
async fn gh_api_get_comment_reactions_comment_not_found_on_next_page() {
    // Setup mock GitHub API, which returns a 404 for the second page of reactions
    let api = MockGitHubApi::start().await;
    api.respond_with_next_page(
        Method::GET,
        "/repos/org/repo/issues/comments/3003/reactions",
        json!([{"content": "+1", "created_at": TIMESTAMP, "user": {"login": USER1}}]),
        "/reactions-page-2",
    );
    let gh = setup_test_gh_api(&api);

    // Get reactions and check the comment is reported as not found
    let err = gh.get_comment_reactions(INST_ID, ORG, REPO, COMMENT_ID).await.unwrap_err();
    assert!(err.is::<CommentNotFoundError>());
}

#[tokio::test]
async fn gh_api_get_comment_reactions_other_error() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::GET,
        "/repos/org/repo/issues/comments/3003/reactions",
        StatusCode::FORBIDDEN,
        json!({"message": "Forbidden"}),
    );
    let gh = setup_test_gh_api(&api);

    // Get reactions and check the error is the one returned by GitHub, which does not
    // say the comment does not exist
    let err = gh.get_comment_reactions(INST_ID, ORG, REPO, COMMENT_ID).await.unwrap_err();
    assert!(format!("{err:#}").contains("Forbidden"), "{err:#}");
    assert!(!err.is::<CommentNotFoundError>());

    // Check the reactions were requested just once
    let reactions_path = "/repos/org/repo/issues/comments/3003/reactions";
    assert_eq!(api.requests(&Method::GET, reactions_path).len(), 1);
}

#[tokio::test]
async fn gh_api_get_comment_reactions_skips_reactions_without_user() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::GET,
        "/repos/org/repo/issues/comments/3003/reactions",
        StatusCode::OK,
        json!([
            {"content": "-1", "created_at": TIMESTAMP, "user": null},
            {"content": "+1", "created_at": TIMESTAMP, "user": {"login": USER1}}
        ]),
    );
    let gh = setup_test_gh_api(&api);

    // Get reactions and check only the one with a user is returned
    assert_eq!(
        gh.get_comment_reactions(INST_ID, ORG, REPO, COMMENT_ID).await.unwrap(),
        vec![Reaction {
            content: "+1".to_string(),
            created_at: TIMESTAMP.to_string(),
            user: User {
                login: USER1.to_string()
            },
        }]
    );
}

#[tokio::test]
async fn gh_api_get_comment_reactions_token_not_found() {
    // Setup mock GitHub API, which returns a 404 for the first installation token
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::POST,
        &installation_token_path(),
        StatusCode::NOT_FOUND,
        json!({"message": "Not Found"}),
    );
    let gh = setup_test_gh_api(&api);

    // Get reactions and check the error does not say the comment does not exist
    let err = gh.get_comment_reactions(INST_ID, ORG, REPO, COMMENT_ID).await.unwrap_err();
    assert!(is_not_found_error(&err));
    assert!(!err.is::<CommentNotFoundError>());

    // Check the reactions were not requested
    let reactions_path = "/repos/org/repo/issues/comments/3003/reactions";
    assert_eq!(api.requests(&Method::GET, reactions_path).len(), 0);
}
