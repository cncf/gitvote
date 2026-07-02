use std::{fs, path::Path, sync::LazyLock};

use anyhow::format_err;
use aws_lc_rs::{
    encoding::AsDer,
    rsa::{KeyPair, KeySize},
};
use axum::http::{Method, StatusCode};
use base64::{Engine, engine::general_purpose::STANDARD};
use jsonwebtoken::EncodingKey;
use octocrab::models::AppId;
use serde_json::{Value, json};

use crate::{
    cfg_repo::{AllowedVoters, CfgProfile},
    testutil::*,
};

use super::*;

/// Private key used to authenticate as the test GitHub application.
static APP_PRIVATE_KEY: LazyLock<EncodingKey> = LazyLock::new(|| {
    let key_pair = KeyPair::generate(KeySize::Rsa2048).unwrap();
    let der = key_pair.as_der().unwrap();
    let pem = format!(
        "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
        STANDARD.encode(der.as_ref())
    );
    EncodingKey::from_rsa_pem(pem.as_bytes()).unwrap()
});

#[test]
fn event_try_from_invalid_body() {
    // Setup event header
    let header = HeaderValue::from_static("issues");

    // Parse event and check the error
    assert!(matches!(
        Event::try_from((Some(&header), b"{}".as_slice())),
        Err(EventError::InvalidBody(_))
    ));
}

#[test]
fn event_try_from_issue_comment_event() {
    // Setup event payload
    let header = HeaderValue::from_static("issue_comment");
    let body = fs::read(Path::new(TESTDATA_PATH).join("event-cmd.json")).unwrap();

    // Parse event and check the result
    let mut expected_event = setup_test_issue_comment_event();
    expected_event.action = IssueCommentEventAction::Created;
    expected_event.comment.body = Some("/vote".to_string());
    assert_eq!(
        Event::try_from((Some(&header), body.as_slice())).unwrap(),
        Event::IssueComment(expected_event)
    );
}

#[test]
fn event_try_from_issue_event() {
    // Setup event payload
    let header = HeaderValue::from_static("issues");
    let body = json!({
        "action": "opened",
        "installation": {"id": INST_ID},
        "issue": {
            "id": ISSUE_ID,
            "number": ISSUE_NUM,
            "title": TITLE,
            "body": "/vote",
            "pull_request": {"url": "https://api.github.com/repos/org/repo/pulls/1"}
        },
        "repository": {"full_name": REPOFN},
        "sender": {"login": USER}
    });

    // Parse event and check the result
    let mut expected_event = setup_test_issue_event();
    expected_event.action = IssueEventAction::Opened;
    expected_event.issue.body = Some("/vote".to_string());
    expected_event.issue.pull_request = Some(PullRequestInIssue {
        url: "https://api.github.com/repos/org/repo/pulls/1".to_string(),
    });
    expected_event.organization = None;
    assert_eq!(
        Event::try_from((Some(&header), body.to_string().as_bytes())).unwrap(),
        Event::Issue(expected_event)
    );
}

#[test]
fn event_try_from_issue_event_unknown_action() {
    // Setup event payload
    let header = HeaderValue::from_static("issues");
    let mut body = serde_json::to_value(setup_test_issue_event()).unwrap();
    body["action"] = json!("edited");

    // Parse event and check the action
    let Event::Issue(event) = Event::try_from((Some(&header), body.to_string().as_bytes())).unwrap() else {
        panic!("expected an issue event");
    };
    assert_eq!(event.action, IssueEventAction::Other);
}

#[test]
fn event_try_from_missing_header() {
    assert_eq!(
        Event::try_from((None, b"{}".as_slice())),
        Err(EventError::MissingHeader)
    );
}

#[test]
fn event_try_from_pr_event() {
    // Setup event payload
    let header = HeaderValue::from_static("pull_request");
    let body = json!({
        "action": "synchronize",
        "installation": {"id": INST_ID},
        "pull_request": {
            "id": ISSUE_ID,
            "number": ISSUE_NUM,
            "title": TITLE,
            "body": null,
            "base": {"ref": BRANCH}
        },
        "repository": {"full_name": REPOFN},
        "organization": {"login": ORG},
        "sender": {"login": USER}
    });

    // Parse event and check the result
    let mut expected_event = setup_test_pr_event();
    expected_event.action = PullRequestEventAction::Synchronize;
    assert_eq!(
        Event::try_from((Some(&header), body.to_string().as_bytes())).unwrap(),
        Event::PullRequest(expected_event)
    );
}

#[test]
fn event_try_from_pr_event_unknown_action() {
    // Setup event payload
    let header = HeaderValue::from_static("pull_request");
    let mut body = serde_json::to_value(setup_test_pr_event()).unwrap();
    body["action"] = json!("closed");

    // Parse event and check the action
    let Event::PullRequest(event) = Event::try_from((Some(&header), body.to_string().as_bytes())).unwrap()
    else {
        panic!("expected a pull request event");
    };
    assert_eq!(event.action, PullRequestEventAction::Other);
}

#[test]
fn event_try_from_unsupported_event() {
    // Setup event header
    let header = HeaderValue::from_static("push");

    // Parse event and check the error
    assert_eq!(
        Event::try_from((Some(&header), b"{}".as_slice())),
        Err(EventError::UnsupportedEvent)
    );
}

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
        conclusion: Some("success".to_string()),
        summary: "Vote passed".to_string(),
    };
    gh.create_check_run(INST_ID, ORG, REPO, ISSUE_NUM, &check_details).await.unwrap();

    // Check the request sent
    let requests = api.requests(&Method::POST, "/repos/org/repo/check-runs");
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].body,
        Some(json!({
            "name": "GitVote",
            "head_sha": "abc123",
            "status": "completed",
            "conclusion": "success",
            "output": {
                "title": "Vote passed",
                "summary": "Vote passed"
            }
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
        conclusion: None,
        summary: "Vote open".to_string(),
    };
    gh.create_check_run(INST_ID, ORG, REPO, ISSUE_NUM, &check_details).await.unwrap();

    // Check the request sent does not include a conclusion
    let requests = api.requests(&Method::POST, "/repos/org/repo/check-runs");
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].body,
        Some(json!({
            "name": "GitVote",
            "head_sha": "abc123",
            "status": "in_progress",
            "output": {
                "title": "Vote open",
                "summary": "Vote open"
            }
        }))
    );
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
        conclusion: None,
        summary: "Vote open".to_string(),
    };
    let err = gh.create_check_run(INST_ID, ORG, REPO, ISSUE_NUM, &check_details).await.unwrap_err();

    // Check the error returned and that no check run was created
    assert_eq!(err.to_string(), "pull request response missing head");
    assert!(api.requests(&Method::POST, "/repos/org/repo/check-runs").is_empty());
}

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
    assert_eq!(api.requests(&Method::POST, "/graphql").len(), 2);
}

#[tokio::test]
async fn gh_api_get_allowed_voters_deduplicates_users_case_insensitively() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::GET,
        "/orgs/org/teams/team1/members",
        StatusCode::OK,
        json!([{"login": USER1}]),
    );
    let gh = setup_test_gh_api(&api);

    // Get allowed voters
    let cfg = CfgProfile {
        allowed_voters: Some(AllowedVoters {
            teams: Some(vec![TEAM1.to_string()]),
            users: Some(vec![
                USER1.to_uppercase(),
                USER2.to_string(),
                USER2.to_uppercase(),
            ]),
            ..Default::default()
        }),
        ..Default::default()
    };
    let allowed_voters =
        gh.get_allowed_voters(INST_ID, &cfg, ORG, REPO, Some(&ORG.to_string())).await.unwrap();

    // Check allowed voters are deduplicated keeping the first spelling seen
    assert_eq!(allowed_voters, vec![USER1.to_string(), USER2.to_string()]);
}

#[tokio::test]
async fn gh_api_get_allowed_voters_excluding_team_maintainers() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::GET,
        "/orgs/org/teams/team1/members",
        StatusCode::OK,
        json!([{"login": USER1}]),
    );
    let gh = setup_test_gh_api(&api);

    // Get allowed voters
    let cfg = CfgProfile {
        allowed_voters: Some(AllowedVoters {
            teams: Some(vec![TEAM1.to_string()]),
            exclude_team_maintainers: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };
    let allowed_voters =
        gh.get_allowed_voters(INST_ID, &cfg, ORG, REPO, Some(&ORG.to_string())).await.unwrap();

    // Check allowed voters and team members role requested
    assert_eq!(allowed_voters, vec![USER1.to_string()]);
    let requests = api.requests(&Method::GET, "/orgs/org/teams/team1/members");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].query, Some("role=member".to_string()));
}

#[tokio::test]
async fn gh_api_get_allowed_voters_falls_back_to_collaborators() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::GET,
        "/repos/org/repo/collaborators",
        StatusCode::OK,
        json!([{"login": USER1}, {"login": USER2}]),
    );
    let gh = setup_test_gh_api(&api);

    // Get allowed voters
    let cfg = CfgProfile::default();
    let allowed_voters =
        gh.get_allowed_voters(INST_ID, &cfg, ORG, REPO, Some(&ORG.to_string())).await.unwrap();

    // Check all collaborators are allowed to vote
    assert_eq!(allowed_voters, vec![USER1.to_string(), USER2.to_string()]);
}

#[tokio::test]
async fn gh_api_get_allowed_voters_falls_back_to_collaborators_when_teams_are_empty() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::GET,
        "/orgs/org/teams/team1/members",
        StatusCode::OK,
        json!([]),
    );
    api.respond(
        Method::GET,
        "/repos/org/repo/collaborators",
        StatusCode::OK,
        json!([{"login": USER1}]),
    );
    let gh = setup_test_gh_api(&api);

    // Get allowed voters
    let cfg = CfgProfile {
        allowed_voters: Some(AllowedVoters {
            teams: Some(vec![TEAM1.to_string()]),
            ..Default::default()
        }),
        ..Default::default()
    };
    let allowed_voters =
        gh.get_allowed_voters(INST_ID, &cfg, ORG, REPO, Some(&ORG.to_string())).await.unwrap();

    // Check all collaborators are allowed to vote
    assert_eq!(allowed_voters, vec![USER1.to_string()]);
}

#[tokio::test]
async fn gh_api_get_allowed_voters_ignores_team_errors() {
    // Setup mock GitHub API (team members request is not found)
    let api = MockGitHubApi::start().await;
    let gh = setup_test_gh_api(&api);

    // Get allowed voters
    let cfg = CfgProfile {
        allowed_voters: Some(AllowedVoters {
            teams: Some(vec![TEAM1.to_string()]),
            users: Some(vec![USER2.to_string()]),
            ..Default::default()
        }),
        ..Default::default()
    };
    let allowed_voters =
        gh.get_allowed_voters(INST_ID, &cfg, ORG, REPO, Some(&ORG.to_string())).await.unwrap();

    // Check only configured users are allowed to vote
    assert_eq!(allowed_voters, vec![USER2.to_string()]);
    assert_eq!(
        api.requests(&Method::GET, "/orgs/org/teams/team1/members").len(),
        1
    );
}

#[tokio::test]
async fn gh_api_get_allowed_voters_ignores_teams_without_org() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    let gh = setup_test_gh_api(&api);

    // Get allowed voters
    let cfg = CfgProfile {
        allowed_voters: Some(AllowedVoters {
            teams: Some(vec![TEAM1.to_string()]),
            users: Some(vec![USER2.to_string()]),
            ..Default::default()
        }),
        ..Default::default()
    };
    let allowed_voters = gh.get_allowed_voters(INST_ID, &cfg, OWNER, REPO, None).await.unwrap();

    // Check teams were not requested
    assert_eq!(allowed_voters, vec![USER2.to_string()]);
    assert!(api.requests(&Method::GET, "/orgs/owner/teams/team1/members").is_empty());
}

#[tokio::test]
async fn gh_api_get_allowed_voters_teams_and_users() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::GET,
        "/orgs/org/teams/team1/members",
        StatusCode::OK,
        json!([{"login": USER1}, {"login": USER2}]),
    );
    api.respond(
        Method::GET,
        "/orgs/org/teams/team2/members",
        StatusCode::OK,
        json!([{"login": USER2}, {"login": USER3}]),
    );
    let gh = setup_test_gh_api(&api);

    // Get allowed voters
    let cfg = CfgProfile {
        allowed_voters: Some(AllowedVoters {
            teams: Some(vec![TEAM1.to_string(), "team2".to_string()]),
            users: Some(vec![USER3.to_string(), USER4.to_string()]),
            ..Default::default()
        }),
        ..Default::default()
    };
    let allowed_voters =
        gh.get_allowed_voters(INST_ID, &cfg, ORG, REPO, Some(&ORG.to_string())).await.unwrap();

    // Check allowed voters are deduplicated and all team roles were requested
    assert_eq!(
        allowed_voters,
        vec![
            USER1.to_string(),
            USER2.to_string(),
            USER3.to_string(),
            USER4.to_string()
        ]
    );
    let requests = api.requests(&Method::GET, "/orgs/org/teams/team1/members");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].query, Some("role=all".to_string()));
}

#[tokio::test]
async fn gh_api_get_collaborators_all_pages() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond_with_next_page(
        Method::GET,
        "/repos/org/repo/collaborators",
        json!([{"login": USER1}]),
        "/collaborators-page-2",
    );
    api.respond(
        Method::GET,
        "/collaborators-page-2",
        StatusCode::OK,
        json!([{"login": USER2}]),
    );
    let gh = setup_test_gh_api(&api);

    // Get collaborators and check all pages were collected
    assert_eq!(
        gh.get_collaborators(INST_ID, ORG, REPO).await.unwrap(),
        vec![USER1.to_string(), USER2.to_string()]
    );
}

#[tokio::test]
async fn gh_api_get_comment_reactions() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::GET,
        "/repos/org/repo/issues/comments/1234/reactions",
        StatusCode::OK,
        json!([{"user": {"login": USER1}, "content": "+1", "created_at": TIMESTAMP}]),
    );
    let gh = setup_test_gh_api(&api);

    // Get reactions and check the result
    assert_eq!(
        gh.get_comment_reactions(INST_ID, ORG, REPO, COMMENT_ID).await.unwrap(),
        vec![Reaction {
            user: User {
                login: USER1.to_string()
            },
            content: "+1".to_string(),
            created_at: TIMESTAMP.to_string(),
        }]
    );
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
        gh.get_config_file(INST_ID, ORG, REPO).await,
        Some("github-dir".to_string())
    );
    assert!(api.requests(&Method::GET, "/repos/org/.github/contents/.gitvote.yml").is_empty());
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
        gh.get_config_file(INST_ID, ORG, REPO).await,
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
        gh.get_config_file(INST_ID, ORG, REPO).await,
        Some("repo-root".to_string())
    );
    assert!(api.requests(&Method::GET, "/repos/org/repo/contents/.github/.gitvote.yml").is_empty());
}

#[tokio::test]
async fn gh_api_get_config_file_not_found() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    let gh = setup_test_gh_api(&api);

    // Get config file and check all locations were tried
    assert_eq!(gh.get_config_file(INST_ID, ORG, REPO).await, None);
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

#[tokio::test]
async fn gh_api_post_comment() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::POST,
        "/repos/org/repo/issues/1/comments",
        StatusCode::CREATED,
        json!({
            "id": COMMENT_ID2,
            "node_id": "IC_1",
            "url": "https://api.github.com/repos/org/repo/issues/comments/5678",
            "html_url": "https://github.com/org/repo/issues/1#issuecomment-5678",
            "body": "comment",
            "user": author_json(USER),
            "created_at": TIMESTAMP
        }),
    );
    let gh = setup_test_gh_api(&api);

    // Post comment and check the comment id returned
    assert_eq!(
        gh.post_comment(INST_ID, ORG, REPO, ISSUE_NUM, "comment").await.unwrap(),
        COMMENT_ID2
    );
    let requests = api.requests(&Method::POST, "/repos/org/repo/issues/1/comments");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].body, Some(json!({"body": "comment"})));
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

#[tokio::test]
async fn gh_api_user_is_collaborator() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::GET,
        "/repos/org/repo/collaborators/user",
        StatusCode::NO_CONTENT,
        Value::Null,
    );
    let gh = setup_test_gh_api(&api);

    // Check the user is a collaborator
    assert!(gh.user_is_collaborator(INST_ID, ORG, REPO, USER).await.unwrap());
}

#[tokio::test]
async fn gh_api_user_is_not_collaborator() {
    // Setup mock GitHub API (collaborator request is not found)
    let api = MockGitHubApi::start().await;
    let gh = setup_test_gh_api(&api);

    // Check the user is not a collaborator
    assert!(!gh.user_is_collaborator(INST_ID, ORG, REPO, USER).await.unwrap());
}

#[tokio::test]
async fn is_not_found_error_github_not_found() {
    assert!(is_not_found_error(&setup_test_not_found_error().await));
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
fn is_not_found_error_other_error() {
    assert!(!is_not_found_error(&format_err!("Not Found")));
}

#[test]
fn split_full_name_extra_parts_are_ignored() {
    assert_eq!(split_full_name("org/repo/extra"), ("org", "repo"));
}

#[test]
fn split_full_name_owner_and_repo() {
    assert_eq!(split_full_name(REPOFN), (ORG, REPO));
}

// Helpers.

/// Build a GitHub user JSON object for the login provided.
fn author_json(login: &str) -> Value {
    let url = format!("https://api.github.com/users/{login}");
    json!({
        "login": login,
        "id": 1,
        "node_id": "U_1",
        "avatar_url": url,
        "gravatar_id": "",
        "url": url,
        "html_url": url,
        "followers_url": url,
        "following_url": url,
        "gists_url": url,
        "starred_url": url,
        "subscriptions_url": url,
        "organizations_url": url,
        "repos_url": url,
        "events_url": url,
        "received_events_url": url,
        "type": "User",
        "site_admin": false
    })
}

/// Build a GitHub file content JSON object for the path and content provided.
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

/// Build a GitHub pull request JSON object.
fn pr_json() -> Value {
    json!({
        "number": ISSUE_NUM,
        "head": {"ref": "feature", "sha": "abc123"}
    })
}

/// Setup a `GHApi` instance that uses the mock GitHub API provided.
fn setup_test_gh_api(api: &MockGitHubApi) -> GHApi {
    // Setup installation token response
    api.respond(
        Method::POST,
        &format!("/app/installations/{INST_ID}/access_tokens"),
        StatusCode::CREATED,
        json!({"token": "installation-token", "permissions": {}}),
    );

    // Setup application client
    let app_client = Octocrab::builder()
        .base_uri(api.base_uri())
        .unwrap()
        .app(AppId(1), APP_PRIVATE_KEY.clone())
        .build()
        .unwrap();
    GHApi::new(app_client)
}
