//! This modules defines some test utilities.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use octocrab::Octocrab;
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::net::TcpListener;
use uuid::Uuid;

use crate::{
    cfg_repo::{AllowedVoters, Announcements, CfgProfile, DiscussionsAnnouncements},
    github::*,
    results::{UserVote, Vote, VoteOption, VoteResults, calculate},
};

pub(crate) const BRANCH: &str = "main";
pub(crate) const COMMENT_ID: i64 = 1234;
pub(crate) const COMMENT_ID2: i64 = 5678;
pub(crate) const ERROR: &str = "fake error";
pub(crate) const INST_ID: u64 = 1234;
pub(crate) const ISSUE_ID: i64 = 1234;
pub(crate) const ISSUE_NUM: i64 = 1;
pub(crate) const ORG: &str = "org";
pub(crate) const OWNER: &str = "owner";
pub(crate) const OWNER_IS_ORG: bool = true;
pub(crate) const PROFILE_NAME: &str = "profile1";
pub(crate) const REPO: &str = "repo";
pub(crate) const REPOFN: &str = "org/repo";
pub(crate) const TESTDATA_PATH: &str = "src/testdata";
pub(crate) const TITLE: &str = "Test title";
pub(crate) const DISCUSSIONS_CATEGORY: &str = "announcements";
pub(crate) const USER: &str = "user";
pub(crate) const USER1: &str = "user1";
pub(crate) const USER2: &str = "user2";
pub(crate) const USER3: &str = "user3";
pub(crate) const USER4: &str = "user4";
pub(crate) const USER5: &str = "user5";
pub(crate) const TEAM1: &str = "team1";
pub(crate) const VOTE_ID: &str = "00000000-0000-0000-0000-000000000001";
pub(crate) const VOTE_ID2: &str = "00000000-0000-0000-0000-000000000002";
pub(crate) const TIMESTAMP: &str = "2022-11-30T10:00:00Z";

pub(crate) fn get_test_invalid_config() -> String {
    fs::read_to_string(Path::new(TESTDATA_PATH).join("config-invalid.yml")).unwrap()
}

pub(crate) fn get_test_valid_config() -> String {
    fs::read_to_string(Path::new(TESTDATA_PATH).join("config.yml")).unwrap()
}

pub(crate) fn setup_test_issue_event() -> IssueEvent {
    IssueEvent {
        action: IssueEventAction::Other,
        installation: Installation { id: INST_ID as i64 },
        issue: Issue {
            id: ISSUE_ID,
            number: ISSUE_NUM,
            title: TITLE.to_string(),
            body: None,
            pull_request: None,
        },
        repository: Repository {
            full_name: REPOFN.to_string(),
        },
        organization: Some(Organization {
            login: ORG.to_string(),
        }),
        sender: User {
            login: USER.to_string(),
        },
    }
}

pub(crate) fn setup_test_issue_comment_event() -> IssueCommentEvent {
    IssueCommentEvent {
        action: IssueCommentEventAction::Other,
        comment: Comment {
            id: COMMENT_ID,
            body: None,
        },
        installation: Installation { id: INST_ID as i64 },
        issue: Issue {
            id: ISSUE_ID,
            number: ISSUE_NUM,
            title: TITLE.to_string(),
            body: None,
            pull_request: None,
        },
        repository: Repository {
            full_name: REPOFN.to_string(),
        },
        organization: Some(Organization {
            login: ORG.to_string(),
        }),
        sender: User {
            login: USER.to_string(),
        },
    }
}

pub(crate) fn setup_test_pr_event() -> PullRequestEvent {
    PullRequestEvent {
        action: PullRequestEventAction::Other,
        installation: Installation { id: INST_ID as i64 },
        pull_request: PullRequest {
            id: ISSUE_ID,
            number: ISSUE_NUM,
            title: TITLE.to_string(),
            body: None,
            base: PullRequestBase {
                reference: BRANCH.to_string(),
            },
        },
        repository: Repository {
            full_name: REPOFN.to_string(),
        },
        organization: Some(Organization {
            login: ORG.to_string(),
        }),
        sender: User {
            login: USER.to_string(),
        },
    }
}

pub(crate) fn setup_test_vote() -> Vote {
    Vote {
        vote_id: Uuid::parse_str(VOTE_ID).unwrap(),
        vote_comment_id: COMMENT_ID,
        created_at: OffsetDateTime::now_utc(),
        created_by: USER.to_string(),
        ends_at: OffsetDateTime::now_utc(),
        closed: false,
        closed_at: None,
        checked_at: None,
        cfg: CfgProfile {
            duration: Duration::from_mins(5),
            pass_threshold: 50.0,
            allowed_voters: Some(AllowedVoters {
                users: Some(vec![USER1.to_string()]),
                ..Default::default()
            }),
            announcements: Some(Announcements {
                discussions: Some(DiscussionsAnnouncements {
                    category: DISCUSSIONS_CATEGORY.to_string(),
                }),
            }),
            ..Default::default()
        },
        installation_id: INST_ID as i64,
        issue_id: ISSUE_ID,
        issue_number: ISSUE_NUM,
        issue_title: Some(TITLE.to_string()),
        is_pull_request: false,
        repository_full_name: REPOFN.to_string(),
        organization: Some(ORG.to_string()),
        results: None,
    }
}

pub(crate) fn setup_test_vote_results() -> VoteResults {
    VoteResults {
        passed: true,
        in_favor_percentage: 100.0,
        pass_threshold: 50.0,
        in_favor: 1,
        against: 0,
        against_percentage: 0.0,
        abstain: 0,
        not_voted: 0,
        binding: 1,
        non_binding: 0,
        votes: BTreeMap::from([(
            USER1.to_string(),
            UserVote {
                vote_option: VoteOption::InFavor,
                timestamp: OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap(),
                binding: true,
            },
        )]),
        allowed_voters: 1,
        pending_voters: vec![],
    }
}

pub(crate) fn setup_test_vote_with_calculated_results(
    created_at: &str,
    allowed_voters: Vec<UserName>,
    reactions: Vec<Reaction>,
) -> Vote {
    // Setup GitHub mock
    let mut gh = MockGH::new();
    gh.expect_get_comment_reactions().return_once(move |_, _, _, _| {
        Box::pin(async move { Ok::<Vec<Reaction>, anyhow::Error>(reactions) })
    });
    gh.expect_get_allowed_voters().return_once(move |_, _, _, _, _| {
        Box::pin(async move { Ok::<Vec<UserName>, anyhow::Error>(allowed_voters) })
    });

    // Setup vote
    let mut vote = setup_test_vote();
    let created_at_ts = OffsetDateTime::parse(created_at, &Rfc3339).expect("valid created_at timestamp");
    vote.created_at = created_at_ts;

    // Calculate results and attach to vote
    let runtime = tokio::runtime::Runtime::new().expect("runtime creation should succeed");
    let results = runtime
        .block_on(calculate(Arc::new(gh), OWNER, REPO, &vote))
        .expect("vote calculation should succeed");
    vote.results = Some(results);

    vote
}

/// Get a GitHub "Not Found" error as returned by the GitHub client.
pub(crate) async fn setup_test_not_found_error() -> anyhow::Error {
    let api = MockGitHubApi::start().await;
    let client = Octocrab::builder().base_uri(api.base_uri()).unwrap().build().unwrap();
    client.get::<Value, _, ()>("/not-found", None).await.unwrap_err().into()
}

/// Mock GitHub API server that returns canned responses and records the
/// requests received. Responses registered for the same request are returned
/// in order, repeating the last one. Requests without a canned response get a
/// GitHub "Not Found" error response.
pub(crate) struct MockGitHubApi {
    state: MockGitHubApiState,
}

impl MockGitHubApi {
    /// Start a new mock GitHub API server listening on a random local port.
    pub(crate) async fn start() -> Self {
        // Octocrab needs a process-wide rustls provider to build its client
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

        // Bind listener and prepare state
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = MockGitHubApiState {
            base_uri: format!("http://{}", listener.local_addr().unwrap()),
            ..Default::default()
        };

        // Launch server
        let router = Router::new().fallback(handle_mock_github_api_request).with_state(state.clone());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        Self { state }
    }

    /// Base URI of the mock server.
    pub(crate) fn base_uri(&self) -> &str {
        &self.state.base_uri
    }

    /// Requests received by the mock server matching the method and path
    /// provided.
    pub(crate) fn requests(&self, method: &Method, path: &str) -> Vec<MockGitHubApiRequest> {
        let requests = self.state.requests.lock().unwrap();
        requests.iter().filter(|r| r.method == *method && r.path == path).cloned().collect()
    }

    /// Register a response returned for the method and path provided.
    pub(crate) fn respond(&self, method: Method, path: &str, status: StatusCode, body: Value) {
        self.register(method, path, status, body, None);
    }

    /// Register the response returned for the method and path provided,
    /// including a pagination link to the next page path.
    pub(crate) fn respond_with_next_page(
        &self,
        method: Method,
        path: &str,
        body: Value,
        next_page_path: &str,
    ) {
        self.register(
            method,
            path,
            StatusCode::OK,
            body,
            Some(next_page_path.to_string()),
        );
    }

    /// Register a response in the mock server state.
    fn register(
        &self,
        method: Method,
        path: &str,
        status: StatusCode,
        body: Value,
        next_page_path: Option<String>,
    ) {
        self.state.responses.lock().unwrap().entry((method, path.to_string())).or_default().push(
            MockGitHubApiResponse {
                body,
                status,
                next_page_path,
            },
        );
    }
}

/// Request received by the mock GitHub API.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MockGitHubApiRequest {
    pub method: Method,
    pub path: String,

    pub body: Option<Value>,
    pub query: Option<String>,
}

/// Canned response returned by the mock GitHub API.
#[derive(Debug, Clone)]
struct MockGitHubApiResponse {
    body: Value,
    status: StatusCode,

    next_page_path: Option<String>,
}

/// Canned responses of the mock GitHub API by method and path.
type MockGitHubApiResponses = HashMap<(Method, String), Vec<MockGitHubApiResponse>>;

/// Mock GitHub API server state.
#[derive(Debug, Clone, Default)]
struct MockGitHubApiState {
    base_uri: String,
    requests: Arc<Mutex<Vec<MockGitHubApiRequest>>>,
    responses: Arc<Mutex<MockGitHubApiResponses>>,
}

/// Handle a mock GitHub API request, recording it and returning the matching response.
#[allow(clippy::unused_async)]
async fn handle_mock_github_api_request(
    State(state): State<MockGitHubApiState>,
    method: Method,
    uri: Uri,
    body: Bytes,
) -> Response {
    // Record request
    let path = uri.path().to_string();
    state.requests.lock().unwrap().push(MockGitHubApiRequest {
        method: method.clone(),
        path: path.clone(),
        body: serde_json::from_slice(&body).ok(),
        query: uri.query().map(ToString::to_string),
    });

    // Return the next registered response or a not found error
    let mut responses = state.responses.lock().unwrap();
    let Some(queued_responses) = responses.get_mut(&(method, path)) else {
        return (StatusCode::NOT_FOUND, axum::Json(json!({"message": "Not Found"}))).into_response();
    };
    let response = if queued_responses.len() > 1 {
        queued_responses.remove(0)
    } else {
        queued_responses[0].clone()
    };
    let mut http_response = (response.status, axum::Json(response.body.clone())).into_response();
    if let Some(next_page_path) = &response.next_page_path {
        let link = format!(r#"<{}{next_page_path}>; rel="next""#, state.base_uri);
        http_response.headers_mut().insert(header::LINK, HeaderValue::from_str(&link).unwrap());
    }
    http_response
}
