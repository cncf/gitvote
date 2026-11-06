use axum::http::{Method, StatusCode};
use serde_json::{Value, json};

use crate::{
    cfg_repo::{AllowedVoters, CfgProfile},
    testutil::*,
};

use super::*;

#[tokio::test]
async fn gh_api_get_allowed_voters_all_team_pages() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond_with_next_page(
        Method::GET,
        "/orgs/org/teams/team1/members",
        json!([{"login": USER1}]),
        "/members-page-2",
    );
    api.respond(
        Method::GET,
        "/members-page-2",
        StatusCode::OK,
        json!([{"login": USER2}]),
    );
    let gh = setup_test_gh_api(&api);

    // Get allowed voters
    let cfg = setup_test_teams_cfg(None);
    let allowed_voters =
        gh.get_allowed_voters(INST_ID, &cfg, ORG, REPO, Some(&ORG.to_string())).await.unwrap();

    // Check the members of all pages are allowed to vote
    assert_eq!(allowed_voters, vec![USER1.to_string(), USER2.to_string()]);
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
async fn gh_api_get_allowed_voters_does_not_fall_back_to_collaborators_when_teams_are_empty() {
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
    let cfg = setup_test_teams_cfg(None);
    let allowed_voters =
        gh.get_allowed_voters(INST_ID, &cfg, ORG, REPO, Some(&ORG.to_string())).await.unwrap();

    // Check the electorate was not widened to all collaborators
    assert_eq!(allowed_voters, Vec::<UserName>::new());
    assert_eq!(
        api.requests(&Method::GET, "/repos/org/repo/collaborators"),
        vec![]
    );
}

#[tokio::test]
async fn gh_api_get_allowed_voters_does_not_fall_back_to_collaborators_when_teams_are_not_found() {
    // Setup mock GitHub API (team members request is not found)
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::GET,
        "/repos/org/repo/collaborators",
        StatusCode::OK,
        json!([{"login": USER1}]),
    );
    let gh = setup_test_gh_api(&api);

    // Get allowed voters
    let cfg = setup_test_teams_cfg(None);
    let allowed_voters =
        gh.get_allowed_voters(INST_ID, &cfg, ORG, REPO, Some(&ORG.to_string())).await.unwrap();

    // Check the electorate was not widened to all collaborators
    assert_eq!(allowed_voters, Vec::<UserName>::new());
    assert_eq!(
        api.requests(&Method::GET, "/repos/org/repo/collaborators"),
        vec![]
    );
}

#[tokio::test]
async fn gh_api_get_allowed_voters_does_not_fall_back_to_collaborators_with_only_users() {
    for teams in [None, Some(vec![])] {
        // Setup mock GitHub API, with collaborators other than the users listed
        let api = MockGitHubApi::start().await;
        api.respond(
            Method::GET,
            "/repos/org/repo/collaborators",
            StatusCode::OK,
            json!([{"login": USER3}]),
        );
        let gh = setup_test_gh_api(&api);

        // Get allowed voters of a profile listing users and no teams
        let cfg = CfgProfile {
            allowed_voters: Some(AllowedVoters {
                teams: teams.clone(),
                users: Some(vec![USER1.to_string(), USER2.to_string()]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let allowed_voters =
            gh.get_allowed_voters(INST_ID, &cfg, ORG, REPO, Some(&ORG.to_string())).await.unwrap();

        // Check only the users listed are allowed to vote, without asking for the
        // collaborators
        assert_eq!(
            allowed_voters,
            vec![USER1.to_string(), USER2.to_string()],
            "{teams:?}"
        );
        assert_eq!(
            api.requests(&Method::GET, "/repos/org/repo/collaborators"),
            vec![],
            "{teams:?}"
        );
    }
}

#[tokio::test]
async fn gh_api_get_allowed_voters_does_not_fall_back_to_collaborators_without_org() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::GET,
        "/repos/owner/repo/collaborators",
        StatusCode::OK,
        json!([{"login": USER1}]),
    );
    let gh = setup_test_gh_api(&api);

    // Get allowed voters
    let cfg = setup_test_teams_cfg(None);
    let allowed_voters = gh.get_allowed_voters(INST_ID, &cfg, OWNER, REPO, None).await.unwrap();

    // Check the electorate was not widened to all collaborators
    assert_eq!(allowed_voters, Vec::<UserName>::new());
    assert_eq!(
        api.requests(&Method::GET, "/repos/owner/repo/collaborators"),
        vec![]
    );
}

#[tokio::test]
async fn gh_api_get_allowed_voters_error_getting_installation_token() {
    // Setup mock GitHub API, which returns a 404 for the first installation token
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::POST,
        &installation_token_path(),
        StatusCode::NOT_FOUND,
        json!({"message": "Not Found"}),
    );
    let gh = setup_test_gh_api(&api);

    // Get allowed voters and check the failure is not taken for a missing team
    let cfg = setup_test_teams_cfg(Some(vec![USER2.to_string()]));
    let err = gh.get_allowed_voters(INST_ID, &cfg, ORG, REPO, Some(&ORG.to_string())).await.unwrap_err();
    assert!(is_not_found_error(&err));
    assert!(!err.is::<TeamNotFoundError>());

    // Check the team members were not requested
    assert_eq!(
        api.requests(&Method::GET, "/orgs/org/teams/team1/members"),
        vec![]
    );
}

#[tokio::test]
async fn gh_api_get_allowed_voters_error_getting_next_team_page() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    api.respond_with_next_page(
        Method::GET,
        "/orgs/org/teams/team1/members",
        json!([{"login": USER1}]),
        "/members-page-2",
    );
    api.respond(
        Method::GET,
        "/members-page-2",
        StatusCode::FORBIDDEN,
        json!({"message": "Forbidden"}),
    );
    let gh = setup_test_gh_api(&api);

    // Get allowed voters and check an incomplete team is an error
    let cfg = setup_test_teams_cfg(None);
    let err = gh.get_allowed_voters(INST_ID, &cfg, ORG, REPO, Some(&ORG.to_string())).await.unwrap_err();
    assert!(format!("{err:#}").contains("Forbidden"));
}

#[tokio::test]
async fn gh_api_get_allowed_voters_error_getting_team_members() {
    // Setup cases (team members status, users allowed besides the team)
    let test_cases = [
        (StatusCode::UNAUTHORIZED, None),
        (StatusCode::FORBIDDEN, None),
        (StatusCode::FORBIDDEN, Some(vec![USER2.to_string()])),
        (StatusCode::INTERNAL_SERVER_ERROR, None),
        (StatusCode::BAD_GATEWAY, None),
        (StatusCode::BAD_GATEWAY, Some(vec![USER2.to_string()])),
    ];
    for (status, users) in test_cases {
        // Setup mock GitHub API
        let api = MockGitHubApi::start().await;
        api.respond(
            Method::GET,
            "/orgs/org/teams/team1/members",
            status,
            json!({"message": "Team lookup failed"}),
        );
        api.respond(
            Method::GET,
            "/repos/org/repo/collaborators",
            StatusCode::OK,
            json!([{"login": USER1}]),
        );
        let gh = setup_test_gh_api(&api);

        // Get allowed voters
        let cfg = setup_test_teams_cfg(users.clone());
        let err = gh.get_allowed_voters(INST_ID, &cfg, ORG, REPO, Some(&ORG.to_string())).await.unwrap_err();

        // Check the failure is propagated without using the collaborators instead
        assert!(
            format!("{err:#}").contains("Team lookup failed"),
            "{status}, {users:?}: {err:#}"
        );
        assert_eq!(
            api.requests(&Method::GET, "/repos/org/repo/collaborators"),
            vec![],
            "{status}, {users:?}"
        );
    }
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
            exclude_team_maintainers: Some(true),
            teams: Some(vec![TEAM1.to_string()]),
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
async fn gh_api_get_allowed_voters_falls_back_to_collaborators_when_lists_are_empty() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
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
            teams: Some(vec![]),
            users: Some(vec![]),
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
    assert_eq!(
        api.requests(&Method::GET, "/orgs/owner/teams/team1/members"),
        vec![]
    );
}

#[tokio::test]
async fn gh_api_get_allowed_voters_skips_teams_not_found() {
    // Setup mock GitHub API (the first team members request is not found)
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::GET,
        "/orgs/org/teams/team2/members",
        StatusCode::OK,
        json!([{"login": USER1}]),
    );
    let gh = setup_test_gh_api(&api);

    // Get allowed voters
    let cfg = CfgProfile {
        allowed_voters: Some(AllowedVoters {
            teams: Some(vec![TEAM1.to_string(), "team2".to_string()]),
            users: Some(vec![USER2.to_string()]),
            ..Default::default()
        }),
        ..Default::default()
    };
    let allowed_voters =
        gh.get_allowed_voters(INST_ID, &cfg, ORG, REPO, Some(&ORG.to_string())).await.unwrap();

    // Check the other team and the configured users are allowed to vote
    assert_eq!(allowed_voters, vec![USER1.to_string(), USER2.to_string()]);
    assert_eq!(
        api.requests(&Method::GET, "/orgs/org/teams/team1/members").len(),
        1
    );
}

#[tokio::test]
async fn gh_api_get_allowed_voters_skips_teams_with_invalid_slug() {
    // Setup mock GitHub API (the encoded slug of the first team is not found)
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::GET,
        "/orgs/org/teams/team2/members",
        StatusCode::OK,
        json!([{"login": USER1}]),
    );
    let gh = setup_test_gh_api(&api);

    // Get allowed voters
    let cfg = CfgProfile {
        allowed_voters: Some(AllowedVoters {
            teams: Some(vec!["Core Maintainers/x".to_string(), "team2".to_string()]),
            users: Some(vec![USER2.to_string()]),
            ..Default::default()
        }),
        ..Default::default()
    };
    let allowed_voters =
        gh.get_allowed_voters(INST_ID, &cfg, ORG, REPO, Some(&ORG.to_string())).await.unwrap();

    // Check the slug was requested encoded and the other team and users are allowed
    assert_eq!(allowed_voters, vec![USER1.to_string(), USER2.to_string()]);
    assert_eq!(
        api.requests(&Method::GET, "/orgs/org/teams/Core%20Maintainers%2Fx/members").len(),
        1
    );
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
async fn gh_api_user_is_collaborator_error() {
    // Setup cases (status, expected error detail, expected requests)
    let test_cases = [
        (StatusCode::OK, "unexpected status", 1),
        (StatusCode::UNAUTHORIZED, "Lookup failed", 1),
        (StatusCode::FORBIDDEN, "Lookup failed", 1),
        (StatusCode::INTERNAL_SERVER_ERROR, "Lookup failed", 4),
        (StatusCode::BAD_GATEWAY, "Lookup failed", 4),
    ];
    for (status, expected_err, expected_requests) in test_cases {
        // Setup mock GitHub API
        let api = MockGitHubApi::start().await;
        api.respond(
            Method::GET,
            "/repos/org/repo/collaborators/user",
            status,
            json!({"message": "Lookup failed"}),
        );
        let gh = setup_test_gh_api(&api);

        // Check the failure is not taken for the user not being a collaborator
        let err = gh.user_is_collaborator(INST_ID, ORG, REPO, USER).await.unwrap_err();
        assert!(format!("{err:#}").contains(expected_err), "{status}: {err:#}");

        // Check the requests sent, as the client retries server errors
        assert_eq!(
            api.requests(&Method::GET, "/repos/org/repo/collaborators/user").len(),
            expected_requests,
            "{status}"
        );
    }
}

#[tokio::test]
async fn gh_api_user_is_collaborator_token_not_found() {
    // Setup mock GitHub API, which returns a 404 for the first installation token
    let api = MockGitHubApi::start().await;
    api.respond(
        Method::POST,
        &installation_token_path(),
        StatusCode::NOT_FOUND,
        json!({"message": "Not Found"}),
    );
    let gh = setup_test_gh_api(&api);

    // Check the failure is not taken for the user not being a collaborator
    let err = gh.user_is_collaborator(INST_ID, ORG, REPO, USER).await.unwrap_err();
    assert!(is_not_found_error(&err));

    // Check the collaborator was not requested
    assert_eq!(
        api.requests(&Method::GET, "/repos/org/repo/collaborators/user"),
        vec![]
    );
}

#[tokio::test]
async fn gh_api_user_is_not_collaborator() {
    // Setup mock GitHub API (collaborator request is not found)
    let api = MockGitHubApi::start().await;
    let gh = setup_test_gh_api(&api);

    // Check the user is not a collaborator
    assert!(!gh.user_is_collaborator(INST_ID, ORG, REPO, USER).await.unwrap());
}

// Helpers.

/// Sets up a configuration profile listing `TEAM1` and the users provided.
fn setup_test_teams_cfg(users: Option<Vec<UserName>>) -> CfgProfile {
    CfgProfile {
        allowed_voters: Some(AllowedVoters {
            teams: Some(vec![TEAM1.to_string()]),
            users,
            ..Default::default()
        }),
        ..Default::default()
    }
}
