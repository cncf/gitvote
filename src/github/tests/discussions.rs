use axum::http::{Method, StatusCode};
use serde_json::{Value, json};

use crate::testutil::*;

use super::*;

#[tokio::test]
async fn gh_api_create_discussion() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::POST,
        "/graphql",
        StatusCode::OK,
        json!({"data": {"repository": {"id": "R_1", "discussionCategory": {"id": "C_1"}}}}),
    );
    api.respond(
        Method::POST,
        "/graphql",
        StatusCode::OK,
        json!({"data": {"createDiscussion": {"discussion": {"id": "D_1"}}}}),
    );
    let gh = setup_test_gh_api(&api);

    // Create discussion
    gh.create_discussion(INST_ID, ORG, REPO, DISCUSSIONS_CATEGORY, "title", "body")
        .await
        .unwrap();

    // Check the repository query and the create discussion mutation sent
    let requests = api.requests(&Method::POST, "/graphql");
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].body.as_ref().unwrap()["variables"],
        json!({"owner": ORG, "repo": REPO, "category": DISCUSSIONS_CATEGORY})
    );
    assert_eq!(
        requests[1].body.as_ref().unwrap()["variables"],
        json!({"repositoryId": "R_1", "categoryId": "C_1", "title": "title", "body": "body"})
    );
}

#[tokio::test]
async fn gh_api_create_discussion_category_not_found() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::POST,
        "/graphql",
        StatusCode::OK,
        json!({"data": {"repository": {"id": "R_1", "discussionCategory": null}}}),
    );
    let gh = setup_test_gh_api(&api);

    // Create discussion
    let err = gh
        .create_discussion(INST_ID, ORG, REPO, DISCUSSIONS_CATEGORY, "title", "body")
        .await
        .unwrap_err();

    // Check the error returned and that only the repository query was sent
    assert_eq!(
        err.to_string(),
        "something went wrong while fetching repository details for announcement"
    );
    assert!(err.is::<DiscussionRejectedError>());
    let requests = api.requests(&Method::POST, "/graphql");
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].body.as_ref().unwrap()["variables"],
        json!({"owner": ORG, "repo": REPO, "category": DISCUSSIONS_CATEGORY})
    );
}

#[tokio::test]
async fn gh_api_create_discussion_error_creating_discussion() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::POST,
        "/graphql",
        StatusCode::OK,
        json!({"data": {"repository": {"id": "R_1", "discussionCategory": {"id": "C_1"}}}}),
    );
    api.respond(
        Method::POST,
        "/graphql",
        StatusCode::OK,
        json!({"data": null, "errors": [{"message": "Resource not accessible by integration"}]}),
    );
    let gh = setup_test_gh_api(&api);

    // Create discussion and check the error is propagated
    let err = gh
        .create_discussion(INST_ID, ORG, REPO, DISCUSSIONS_CATEGORY, "title", "body")
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "error creating announcement discussion");
    assert!(err.is::<DiscussionRejectedError>());

    // Check the mutation was sent once, after the repository query
    assert_eq!(api.requests(&Method::POST, "/graphql").len(), 2);
}

#[tokio::test]
async fn gh_api_create_discussion_is_not_resent_after_server_error() {
    for status in [StatusCode::BAD_GATEWAY, StatusCode::TOO_MANY_REQUESTS] {
        // Setup mock GitHub API, which would accept the mutation if it was sent again
        let api = MockGitHubApi::start().await;
        api.respond(
            Method::POST,
            "/graphql",
            StatusCode::OK,
            json!({"data": {"repository": {"id": "R_1", "discussionCategory": {"id": "C_1"}}}}),
        );
        api.respond(Method::POST, "/graphql", status, json!({"message": "Failed"}));
        api.respond(
            Method::POST,
            "/graphql",
            StatusCode::OK,
            json!({"data": {"createDiscussion": {"discussion": {"id": "D_1"}}}}),
        );
        let gh = setup_test_gh_api(&api);

        // Check the failure is returned, as a retryable one
        let err = gh
            .create_discussion(INST_ID, ORG, REPO, DISCUSSIONS_CATEGORY, "title", "body")
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("Failed"), "{status}: {err:#}");
        assert!(!err.is::<DiscussionRejectedError>(), "{status}");

        // Check the mutation was sent just once, after the repository query
        assert_eq!(api.requests(&Method::POST, "/graphql").len(), 2, "{status}");
    }
}

#[tokio::test]
async fn gh_api_create_discussion_rejected_errors() {
    // Setup cases (response, expected error detail, rejected)
    let test_cases = [
        // Missing permissions on the repository query
        (
            json!({
                "data": {"repository": {"id": "R_1", "discussionCategory": null}},
                "errors": [{
                    "type": "FORBIDDEN",
                    "path": ["repository", "discussionCategory"],
                    "message": "Resource not accessible by integration"
                }]
            }),
            "Resource not accessible by integration",
            true,
        ),
        // Category not found
        (
            json!({
                "data": {"repository": {"id": "R_1", "discussionCategory": null}},
                "errors": [{
                    "type": "NOT_FOUND",
                    "path": ["repository", "discussionCategory"],
                    "message": "Could not resolve to a DiscussionCategory with the slug 'x'."
                }]
            }),
            "Could not resolve to a DiscussionCategory",
            true,
        ),
        // Repository not found
        (
            json!({
                "data": {"repository": null},
                "errors": [{
                    "type": "NOT_FOUND",
                    "path": ["repository"],
                    "message": "Could not resolve to a Repository with the name 'org/repo'."
                }]
            }),
            "Could not resolve to a Repository",
            true,
        ),
        // Rate limited, which is reported with a successful status
        (
            json!({
                "errors": [{
                    "type": "RATE_LIMITED",
                    "message": "API rate limit exceeded for installation."
                }]
            }),
            "API rate limit exceeded for installation.",
            false,
        ),
        // Something failed on GitHub
        (
            json!({
                "data": null,
                "errors": [{"message": "Something went wrong while executing your query."}]
            }),
            "Something went wrong while executing your query.",
            false,
        ),
    ];
    for (response, expected_detail, is_rejected) in test_cases {
        // Setup mock GitHub API
        let api = MockGitHubApi::start().await;
        api.respond(Method::POST, "/graphql", StatusCode::OK, response.clone());
        let gh = setup_test_gh_api(&api);

        // Check the error reported and how it is classified
        let err = gh
            .create_discussion(INST_ID, ORG, REPO, DISCUSSIONS_CATEGORY, "title", "body")
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains(expected_detail),
            "{response}: {err:#}"
        );
        assert_eq!(err.is::<DiscussionRejectedError>(), is_rejected, "{response}");

        // Check the discussion was not created after the failed repository query
        assert_eq!(api.requests(&Method::POST, "/graphql").len(), 1, "{response}");
    }
}

#[tokio::test]
async fn gh_api_create_discussion_server_error_on_repository_query_is_retried() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::POST,
        "/graphql",
        StatusCode::BAD_GATEWAY,
        json!({"message": "Bad Gateway"}),
    );
    api.respond(
        Method::POST,
        "/graphql",
        StatusCode::OK,
        json!({"data": {"repository": {"id": "R_1", "discussionCategory": {"id": "C_1"}}}}),
    );
    api.respond(
        Method::POST,
        "/graphql",
        StatusCode::OK,
        json!({"data": {"createDiscussion": {"discussion": {"id": "D_1"}}}}),
    );
    let gh = setup_test_gh_api(&api);

    // Check the discussion is created, as the query only reads data
    gh.create_discussion(INST_ID, ORG, REPO, DISCUSSIONS_CATEGORY, "title", "body")
        .await
        .unwrap();
    assert_eq!(api.requests(&Method::POST, "/graphql").len(), 3);
}

#[tokio::test]
async fn gh_api_has_discussion_with_marker() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    respond_with_announcement_repo(&api);
    api.respond(
        Method::POST,
        "/graphql",
        StatusCode::OK,
        discussions_page_json(
            &[
                discussion_json("announcement", "2022-11-30T10:02:00Z", true),
                discussion_json(&format!("results\n{MARKER}\n"), "2022-11-30T10:01:00Z", true),
            ],
            None,
        ),
    );
    let gh = setup_test_gh_api(&api);

    // Check the discussion is found, ignoring the whitespace after the marker
    let since = OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap();
    assert!(
        gh.has_discussion_with_marker(INST_ID, ORG, REPO, DISCUSSIONS_CATEGORY, MARKER, since)
            .await
            .unwrap()
    );

    // Check the repository query and the discussions query sent
    let requests = api.requests(&Method::POST, "/graphql");
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].body.as_ref().unwrap()["variables"],
        json!({"owner": ORG, "repo": REPO, "category": DISCUSSIONS_CATEGORY})
    );
    assert_eq!(
        requests[1].body.as_ref().unwrap()["variables"],
        json!({"owner": ORG, "repo": REPO, "categoryId": "C_1", "after": null})
    );
}

#[tokio::test]
async fn gh_api_has_discussion_with_marker_all_pages() {
    // Setup mock GitHub API, with the discussion on the second page
    let api = MockGitHubApi::start().await;
    respond_with_announcement_repo(&api);
    api.respond(
        Method::POST,
        "/graphql",
        StatusCode::OK,
        discussions_page_json(
            &[discussion_json("announcement", "2022-11-30T10:02:00Z", true)],
            Some("cursor-1"),
        ),
    );
    api.respond(
        Method::POST,
        "/graphql",
        StatusCode::OK,
        discussions_page_json(&[discussion_json(MARKER, "2022-11-30T10:01:00Z", true)], None),
    );
    let gh = setup_test_gh_api(&api);

    // Check the discussion is found
    let since = OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap();
    assert!(
        gh.has_discussion_with_marker(INST_ID, ORG, REPO, DISCUSSIONS_CATEGORY, MARKER, since)
            .await
            .unwrap()
    );

    // Check the second page was requested after the first one
    let requests = api.requests(&Method::POST, "/graphql");
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[2].body.as_ref().unwrap()["variables"],
        json!({"owner": ORG, "repo": REPO, "categoryId": "C_1", "after": "cursor-1"})
    );
}

#[tokio::test]
async fn gh_api_has_discussion_with_marker_category_not_found() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::POST,
        "/graphql",
        StatusCode::OK,
        json!({"data": {"repository": {"id": "R_1", "discussionCategory": null}}}),
    );
    let gh = setup_test_gh_api(&api);

    // Check the lookup is rejected, as the creation of the discussion would be
    let since = OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap();
    let err = gh
        .has_discussion_with_marker(INST_ID, ORG, REPO, DISCUSSIONS_CATEGORY, MARKER, since)
        .await
        .unwrap_err();
    assert!(err.is::<DiscussionRejectedError>());

    // Check the discussions were not listed after the repository query
    assert_eq!(api.requests(&Method::POST, "/graphql").len(), 1);
}

#[tokio::test]
async fn gh_api_has_discussion_with_marker_errors() {
    // Setup cases (response, error, error detail, rejected)
    let test_cases = [
        // Missing permissions
        (
            json!({
                "data": null,
                "errors": [{"type": "FORBIDDEN", "message": "Resource not accessible by integration"}]
            }),
            "error listing announcement discussions",
            "Resource not accessible by integration",
            true,
        ),
        // Rate limited, which is reported with a successful status
        (
            json!({
                "errors": [{"type": "RATE_LIMITED", "message": "API rate limit exceeded for installation."}]
            }),
            "error listing announcement discussions",
            "API rate limit exceeded for installation.",
            false,
        ),
        // Repository missing from a response without errors
        (
            json!({"data": {"repository": null}}),
            "repository not found listing announcement discussions",
            "repository not found listing announcement discussions",
            false,
        ),
        // Discussion with an invalid creation time
        (
            discussions_page_json(&[discussion_json(MARKER, "yesterday", true)], None),
            "invalid creation time of announcement discussion",
            "the 'year' component could not be parsed",
            false,
        ),
        // Next page without cursor
        (
            json!({"data": {"repository": {"discussions": {
                "nodes": [],
                "pageInfo": {"endCursor": null, "hasNextPage": true}
            }}}}),
            "next page of announcement discussions without cursor",
            "next page of announcement discussions without cursor",
            false,
        ),
    ];
    for (response, expected_err, expected_detail, is_rejected) in test_cases {
        // Setup mock GitHub API
        let api = MockGitHubApi::start().await;
        respond_with_announcement_repo(&api);
        api.respond(Method::POST, "/graphql", StatusCode::OK, response.clone());
        let gh = setup_test_gh_api(&api);

        // Check the failure is not taken for the discussion not being found, and how it
        // is classified
        let since = OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap();
        let err = gh
            .has_discussion_with_marker(INST_ID, ORG, REPO, DISCUSSIONS_CATEGORY, MARKER, since)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), expected_err, "{response}");
        assert!(
            format!("{err:#}").contains(expected_detail),
            "{response}: {err:#}"
        );
        assert_eq!(err.is::<DiscussionRejectedError>(), is_rejected, "{response}");

        // Check the failure happened listing the discussions, after the repository query
        assert_eq!(api.requests(&Method::POST, "/graphql").len(), 2, "{response}");
    }
}

#[tokio::test]
async fn gh_api_has_discussion_with_marker_ignores_discussions_not_created_by_the_app() {
    // Setup mock GitHub API, with the marker in a discussion created by someone else
    let api = MockGitHubApi::start().await;
    respond_with_announcement_repo(&api);
    api.respond(
        Method::POST,
        "/graphql",
        StatusCode::OK,
        discussions_page_json(&[discussion_json(MARKER, "2022-11-30T10:01:00Z", false)], None),
    );
    let gh = setup_test_gh_api(&api);

    // Check the discussion is not found
    let since = OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap();
    assert!(
        !gh.has_discussion_with_marker(INST_ID, ORG, REPO, DISCUSSIONS_CATEGORY, MARKER, since)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn gh_api_has_discussion_with_marker_ignores_marker_not_at_the_end() {
    // Setup mock GitHub API, with the marker in the middle of a discussion of the app,
    // as when an issue title with the marker is quoted in another vote's announcement
    let api = MockGitHubApi::start().await;
    respond_with_announcement_repo(&api);
    api.respond(
        Method::POST,
        "/graphql",
        StatusCode::OK,
        discussions_page_json(
            &[discussion_json(
                &format!("The vote for \"**{MARKER}**\" is now closed.\n\nResults"),
                "2022-11-30T10:01:00Z",
                true,
            )],
            None,
        ),
    );
    let gh = setup_test_gh_api(&api);

    // Check the discussion is not found
    let since = OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap();
    assert!(
        !gh.has_discussion_with_marker(INST_ID, ORG, REPO, DISCUSSIONS_CATEGORY, MARKER, since)
            .await
            .unwrap()
    );

    // Check the discussions were listed once, after the repository query
    assert_eq!(api.requests(&Method::POST, "/graphql").len(), 2);
}

#[tokio::test]
async fn gh_api_has_discussion_with_marker_stops_at_older_discussions() {
    // Setup mock GitHub API, with a discussion created before the time provided followed
    // by more pages, whose discussions are older
    let api = MockGitHubApi::start().await;
    respond_with_announcement_repo(&api);
    api.respond(
        Method::POST,
        "/graphql",
        StatusCode::OK,
        discussions_page_json(
            &[
                discussion_json("announcement", "2022-11-30T10:01:00Z", true),
                discussion_json(MARKER, "2022-11-30T09:59:59Z", true),
            ],
            Some("cursor-1"),
        ),
    );
    let gh = setup_test_gh_api(&api);

    // Check the discussion is not found without getting the next page
    let since = OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap();
    assert!(
        !gh.has_discussion_with_marker(INST_ID, ORG, REPO, DISCUSSIONS_CATEGORY, MARKER, since)
            .await
            .unwrap()
    );
    assert_eq!(api.requests(&Method::POST, "/graphql").len(), 2);
}

// Helpers.

/// Builds a GitHub discussion JSON object as returned by the announcement discussions
/// query.
fn discussion_json(body: &str, created_at: &str, viewer_did_author: bool) -> Value {
    json!({"body": body, "createdAt": created_at, "viewerDidAuthor": viewer_did_author})
}

/// Builds a response of the announcement discussions query with the discussions
/// provided, and the cursor of the next page if there is one.
fn discussions_page_json(discussions: &[Value], next_cursor: Option<&str>) -> Value {
    json!({"data": {"repository": {"discussions": {
        "nodes": discussions,
        "pageInfo": {"endCursor": next_cursor, "hasNextPage": next_cursor.is_some()}
    }}}})
}

/// Registers the response to the announcement repository query in the mock GitHub API.
fn respond_with_announcement_repo(api: &MockGitHubApi) {
    api.respond(
        Method::POST,
        "/graphql",
        StatusCode::OK,
        json!({"data": {"repository": {"id": "R_1", "discussionCategory": {"id": "C_1"}}}}),
    );
}
