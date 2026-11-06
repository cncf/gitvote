//! This module defines some test utilities.

use std::{
    collections::{BTreeMap, HashMap},
    env, fs,
    path::Path,
    str::FromStr,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use anyhow::Result;
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use deadpool_postgres::{Manager, Pool};
use futures::future;
use mockall::predicate::eq;
use octocrab::Octocrab;
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::net::TcpListener;
use tokio_postgres::{Config, NoTls, types::Json};
use uuid::Uuid;

use crate::{
    cfg_repo::{AllowedVoters, Announcements, CfgProfile, DiscussionsAnnouncements},
    db::{MockDB, PgDB},
    github::*,
    results::{UserVote, Vote, VoteOption, VoteResults, calculate},
};

/// Base branch of the test pull request.
pub(crate) const BRANCH: &str = "main";

/// Identifier of the test vote comment.
pub(crate) const COMMENT_ID: i64 = 3003;

/// Identifier of a second test comment.
pub(crate) const COMMENT_ID2: i64 = 5678;

/// Discussions category where the test vote results are announced.
pub(crate) const DISCUSSIONS_CATEGORY: &str = "announcements";

/// Message of the errors returned by the mocks.
pub(crate) const ERROR: &str = "fake error";

/// Head commit of the test pull request.
pub(crate) const HEAD_SHA: &str = "abc123";

/// Identifier of the test GitHub application installation.
pub(crate) const INST_ID: u64 = 1001;

/// Token of the test GitHub application installation.
pub(crate) const INSTALLATION_TOKEN: &str = "installation-token";

/// Identifier of the test issue.
pub(crate) const ISSUE_ID: i64 = 2002;

/// Number of the test issue or pull request.
pub(crate) const ISSUE_NUM: i64 = 1;

/// Database used to run the statements that create and drop the test databases.
const MAINTENANCE_DB: &str = "postgres";

/// Organization owning the test repository, as in its full name.
pub(crate) const ORG: &str = "org";

/// Generic repository owner, used where the owner is not taken from `REPOFN`.
pub(crate) const OWNER: &str = "owner";

/// Whether the repository owner is an organization.
pub(crate) const OWNER_IS_ORG: bool = true;

/// Name of the test vote configuration profile.
pub(crate) const PROFILE_NAME: &str = "profile1";

/// Name of the test repository.
pub(crate) const REPO: &str = "repo";

/// Full name of the test repository.
pub(crate) const REPOFN: &str = "org/repo";

/// Slug of the test team.
pub(crate) const TEAM1: &str = "team1";

/// Maximum number of connections in the pool of a test database.
pub(crate) const TEST_DB_POOL_SIZE: usize = 4;

/// Environment variable with the URL of the migrated template database.
const TEST_DB_URL_VAR: &str = "GITVOTE_TEST_DB_URL";

/// Path of the directory holding the test data files.
pub(crate) const TESTDATA_PATH: &str = "src/testdata";

/// Fixed timestamp in RFC 3339 format, used for instance in the test user votes.
pub(crate) const TIMESTAMP: &str = "2022-11-30T10:00:00Z";

/// Title of the test issue or pull request.
pub(crate) const TITLE: &str = "Test title";

/// Login of the test user, who sends the events and creates the votes.
pub(crate) const USER: &str = "user";

/// Login of the first additional test user.
pub(crate) const USER1: &str = "user1";

/// Login of the second additional test user.
pub(crate) const USER2: &str = "user2";

/// Login of the third additional test user.
pub(crate) const USER3: &str = "user3";

/// Login of the fourth additional test user.
pub(crate) const USER4: &str = "user4";

/// Login of the fifth additional test user.
pub(crate) const USER5: &str = "user5";

/// Identifier of the test vote.
pub(crate) const VOTE_ID: &str = "00000000-0000-0000-0000-000000000001";

/// Identifier of a second test vote.
pub(crate) const VOTE_ID2: &str = "00000000-0000-0000-0000-000000000002";

// Mock GitHub API.

/// Mock GitHub API server that returns canned responses and records the
/// requests received. Responses registered for the same request are returned
/// in order, repeating the last one. Requests without a canned response get a
/// GitHub "Not Found" error response. As GitHub does, requests using the wrong
/// credentials are rejected (see [`has_mock_github_api_credentials`]).
pub(crate) struct MockGitHubApi {
    /// State shared with the server handling the requests.
    state: MockGitHubApiState,
}

impl MockGitHubApi {
    /// Starts a new mock GitHub API server listening on a random local port.
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

    /// Returns the base URI of the mock server.
    pub(crate) fn base_uri(&self) -> &str {
        &self.state.base_uri
    }

    /// Returns the requests received by the mock server matching the method and path
    /// provided.
    pub(crate) fn requests(&self, method: &Method, path: &str) -> Vec<MockGitHubApiRequest> {
        let requests = self.state.requests.lock().unwrap();
        requests
            .iter()
            .filter(|request| request.method == *method && request.path == path)
            .cloned()
            .collect()
    }

    /// Registers a response returned for the method and path provided.
    pub(crate) fn respond(&self, method: Method, path: &str, status: StatusCode, body: Value) {
        self.register(method, path, status, body, None);
    }

    /// Registers the response returned for the method and path provided,
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

    /// Registers a request for the method and path provided that is recorded, as if it
    /// had been processed, but never gets a response.
    pub(crate) fn stall(&self, method: Method, path: &str) {
        self.push_response(
            method,
            path,
            MockGitHubApiResponse {
                body: Value::Null,
                is_stalled: true,
                status: StatusCode::OK,

                next_page_path: None,
            },
        );
    }

    /// Adds a response to the mock server state.
    fn push_response(&self, method: Method, path: &str, response: MockGitHubApiResponse) {
        let mut responses = self.state.responses.lock().unwrap();
        responses.entry((method, path.to_string())).or_default().push(response);
    }

    /// Registers a response in the mock server state.
    fn register(
        &self,
        method: Method,
        path: &str,
        status: StatusCode,
        body: Value,
        next_page_path: Option<String>,
    ) {
        self.push_response(
            method,
            path,
            MockGitHubApiResponse {
                body,
                is_stalled: false,
                status,

                next_page_path,
            },
        );
    }
}

/// Request received by the mock GitHub API.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MockGitHubApiRequest {
    /// HTTP method of the request.
    pub method: Method,
    /// Path of the request URI.
    pub path: String,

    /// Value of the authorization header, if any.
    pub authorization: Option<String>,
    /// Body of the request, if it is valid JSON.
    pub body: Option<Value>,
    /// Query string of the request URI, if any.
    pub query: Option<String>,
}

/// Canned response returned by the mock GitHub API.
#[derive(Debug, Clone)]
struct MockGitHubApiResponse {
    /// JSON body of the response.
    body: Value,
    /// Whether the request is left without a response.
    is_stalled: bool,
    /// HTTP status of the response.
    status: StatusCode,

    /// Path of the next page, linked from the response for pagination.
    next_page_path: Option<String>,
}

/// Canned responses of the mock GitHub API by method and path.
type MockGitHubApiResponses = HashMap<(Method, String), Vec<MockGitHubApiResponse>>;

/// Mock GitHub API server state.
#[derive(Debug, Clone, Default)]
struct MockGitHubApiState {
    /// Base URI of the mock server.
    base_uri: String,
    /// Requests received by the mock server, in arrival order.
    requests: Arc<Mutex<Vec<MockGitHubApiRequest>>>,
    /// Canned responses registered in the mock server.
    responses: Arc<Mutex<MockGitHubApiResponses>>,
}

// Test database.

/// Vote inserted directly in the database, to reach states the `DB` trait cannot
/// produce. Times are expressed as intervals relative to the current timestamp.
pub(crate) struct SeedVote {
    /// Configuration of the vote.
    pub cfg: Value,
    /// Number of failed attempts to complete the vote closing.
    pub close_attempts: i32,
    /// Whether some of the side effects of closing the vote are still pending.
    pub close_pending: bool,
    /// Whether the vote is closed, which also sets its close time to now.
    pub closed: bool,
    /// How long ago the vote was created.
    pub created_ago: &'static str,
    /// How long until the vote ends, negative if it has already ended.
    pub ends_in: &'static str,
    /// Whether the vote is on a pull request rather than on an issue.
    pub is_pull_request: bool,
    /// Number of the issue or pull request the vote is on.
    pub issue_number: i64,
    /// Full name of the repository the vote is on.
    pub repository_full_name: &'static str,
    /// Identifier of the vote comment, whose reactions are the votes.
    pub vote_comment_id: i64,

    /// How long ago the vote was cancelled by the test user, if it was.
    pub cancelled_ago: Option<&'static str>,
    /// How long ago the status of the vote was last checked, if it was.
    pub checked_ago: Option<&'static str>,
    /// Head commit of the pull request when the vote was closed, if known.
    pub close_head_sha: Option<&'static str>,
    /// How long until the next attempt to complete the vote closing can run, negative
    /// if it is due.
    pub close_next_attempt_in: Option<&'static str>,
    /// Results of the vote, if they have been calculated.
    pub results: Option<Value>,
}

/// Database cloned from the migrated template for the exclusive use of a test.
pub(crate) struct TestDB {
    /// Connection settings of the maintenance database, used to drop this one.
    maintenance_config: Config,
    /// Name of the database.
    name: String,
    /// Connection pool of the database.
    pool: Pool,
}

impl TestDB {
    /// Clones the template database, panicking if it is not available.
    pub(crate) async fn new() -> Self {
        Self::with_pool_size(TEST_DB_POOL_SIZE).await
    }

    /// Moves the creation of all the votes back in time by the interval provided.
    pub(crate) async fn backdate_votes(&self, interval: &str) {
        let client = self.pool.get().await.unwrap();
        client
            .execute(
                "update vote set created_at = created_at - $1::text::interval",
                &[&interval],
            )
            .await
            .unwrap();
    }

    /// Returns the number of failed attempts of the only vote in the database, as well
    /// as the seconds left until its next attempt.
    pub(crate) async fn close_retry_state(&self) -> (i32, f64) {
        let client = self.pool.get().await.unwrap();
        let row = client
            .query_one(
                "
                select
                    close_attempts,
                    extract(epoch from close_next_attempt_at - current_timestamp)::float8 as delay
                from vote
                ",
                &[],
            )
            .await
            .unwrap();
        (row.get("close_attempts"), row.get("delay"))
    }

    /// Returns a `PgDB` instance connected to this database.
    pub(crate) fn db(&self) -> PgDB {
        PgDB::new(self.pool.clone())
    }

    /// Makes all the attempts waiting for a retry or a claim to expire due now.
    pub(crate) async fn expire_close_claims(&self) {
        let client = self.pool.get().await.unwrap();
        client
            .execute(
                "
                update vote set close_next_attempt_at = current_timestamp - '1 second'::interval
                where close_next_attempt_at is not null
                ",
                &[],
            )
            .await
            .unwrap();
    }

    /// Makes the operation provided on votes fail with a trigger. It can be any trigger
    /// event, like `delete`, `update` or `update of close_attempts`.
    pub(crate) async fn fail_vote_operation(&self, operation: &str) {
        let client = self.pool.get().await.unwrap();
        client
            .batch_execute(&format!(
                "
                create function fail_vote_operation() returns trigger as $$
                begin
                    raise exception 'vote {operation} failed';
                end;
                $$ language plpgsql;
                create trigger fail_vote_operation before {operation} on vote
                for each row execute function fail_vote_operation();
                "
            ))
            .await
            .unwrap();
    }

    /// Returns the vote provided, including the cancelled votes the `DB` reads leave
    /// out.
    pub(crate) async fn read_vote(&self, vote_id: Uuid) -> Option<Vote> {
        let client = self.pool.get().await.unwrap();
        client
            .query_opt("select * from vote where vote_id = $1::uuid", &[&vote_id])
            .await
            .unwrap()
            .map(|row| Vote::try_from(&row).unwrap())
    }

    /// Checks if the results of the vote provided are stored as SQL null.
    pub(crate) async fn results_are_sql_null(&self, vote_id: Uuid) -> bool {
        let client = self.pool.get().await.unwrap();
        client
            .query_one(
                "select results is null from vote where vote_id = $1::uuid",
                &[&vote_id],
            )
            .await
            .unwrap()
            .get(0)
    }

    /// Inserts the vote provided, returning its id. A cancelled vote is cancelled by
    /// the test user.
    pub(crate) async fn seed_vote(&self, vote: SeedVote) -> Uuid {
        let client = self.pool.get().await.unwrap();
        client
            .query_one(
                "
                insert into vote (
                    cfg,
                    close_attempts,
                    close_pending,
                    closed,
                    created_at,
                    created_by,
                    ends_at,
                    installation_id,
                    is_pull_request,
                    issue_id,
                    issue_number,
                    repository_full_name,
                    vote_comment_id,

                    cancelled_at,
                    cancelled_by,
                    checked_at,
                    close_head_sha,
                    close_next_attempt_at,
                    closed_at,
                    organization,
                    results
                ) values (
                    $1::jsonb,
                    $2::integer,
                    $3::boolean,
                    $4::boolean,
                    current_timestamp - $5::text::interval,
                    $6::text,
                    current_timestamp + $7::text::interval,
                    $8::bigint,
                    $9::boolean,
                    $10::bigint,
                    $11::bigint,
                    $12::text,
                    $13::bigint,

                    current_timestamp - $14::text::interval,
                    case when $14::text is not null then $6::text end,
                    current_timestamp - $15::text::interval,
                    $16::text,
                    current_timestamp + $17::text::interval,
                    case when $4::boolean then current_timestamp end,
                    $18::text,
                    $19::jsonb
                )
                returning vote_id
                ",
                &[
                    &Json(&vote.cfg),
                    &vote.close_attempts,
                    &vote.close_pending,
                    &vote.closed,
                    &vote.created_ago,
                    &USER,
                    &vote.ends_in,
                    &(INST_ID as i64),
                    &vote.is_pull_request,
                    &ISSUE_ID,
                    &vote.issue_number,
                    &vote.repository_full_name,
                    &vote.vote_comment_id,
                    &vote.cancelled_ago,
                    &vote.checked_ago,
                    &vote.close_head_sha,
                    &vote.close_next_attempt_in,
                    &ORG,
                    &vote.results.as_ref().map(Json),
                ],
            )
            .await
            .unwrap()
            .get("vote_id")
    }

    /// Clones the template database like [`TestDB::new`], using a connection pool of
    /// the size provided.
    pub(crate) async fn with_pool_size(pool_size: usize) -> Self {
        // Prepare the connection settings from the template database URL
        let url = env::var(TEST_DB_URL_VAR)
            .unwrap_or_else(|_| panic!("{TEST_DB_URL_VAR} must be set to run the database contract tests"));
        let template_config = Config::from_str(&url).expect("database url should be valid");
        let template = template_config.get_dbname().expect("database url should include a database");
        let mut maintenance_config = template_config.clone();
        maintenance_config.dbname(MAINTENANCE_DB);

        // Clone the template database
        let name = format!("gitvote_contract_{}", Uuid::new_v4().simple());
        let statement = format!(
            "create database {} template {}",
            quote_identifier(&name),
            quote_identifier(template)
        );
        run_maintenance_statement(&maintenance_config, &statement).await;

        // Setup the connection pool of the new database
        let mut config = template_config.clone();
        config.dbname(&name);
        let pool = Pool::builder(Manager::new(config, NoTls))
            .max_size(pool_size)
            .build()
            .expect("pool should be created");

        Self {
            maintenance_config,
            name,
            pool,
        }
    }
}

impl Default for SeedVote {
    /// Returns an open vote created two days ago that ends in ten days.
    fn default() -> Self {
        Self {
            cfg: setup_seed_cfg(),
            close_attempts: 0,
            close_pending: false,
            closed: false,
            created_ago: "2 days",
            ends_in: "10 days",
            is_pull_request: false,
            issue_number: ISSUE_NUM,
            repository_full_name: REPOFN,
            vote_comment_id: COMMENT_ID,

            cancelled_ago: None,
            checked_ago: None,
            close_head_sha: None,
            close_next_attempt_in: None,
            results: None,
        }
    }
}

impl Drop for TestDB {
    /// Drops the database, from a separate thread as this cannot be async.
    fn drop(&mut self) {
        // Stop handing out connections to the database about to be dropped
        self.pool.close();

        // Drop the database from a thread with its own runtime
        let config = self.maintenance_config.clone();
        let statement = format!(
            "drop database if exists {} with (force)",
            quote_identifier(&self.name)
        );
        let result = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime should be created");
            runtime.block_on(run_maintenance_statement(&config, &statement));
        })
        .join();

        // Do not panic while unwinding, as that would hide the test failure
        assert!(
            result.is_ok() || thread::panicking(),
            "test database {} should be dropped",
            self.name
        );
    }
}

// Fixtures and expectations.

/// Expects a check run to be created once with the details provided.
pub(crate) fn expect_check_run(gh: &mut MockGH, check_details: CheckDetails) {
    gh.expect_create_check_run()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(ISSUE_NUM), eq(check_details))
        .times(1)
        .returning(|_, _, _, _, _| Box::pin(future::ready(Ok(()))));
}

/// Expects a lookup of a vote on the test pull request once, with the result given.
pub(crate) fn expect_has_vote(db: &mut MockDB, result: Result<bool>) {
    db.expect_has_vote()
        .with(eq(REPOFN), eq(ISSUE_NUM))
        .times(1)
        .return_once(move |_, _| Box::pin(future::ready(result)));
}

/// Expects the check to be requested once for the test branch.
pub(crate) fn expect_is_check_required(gh: &mut MockGH, is_required: bool) {
    gh.expect_is_check_required()
        .with(eq(INST_ID), eq(ORG), eq(REPO), eq(BRANCH))
        .times(1)
        .returning(move |_, _, _, _| Box::pin(future::ready(Ok(is_required))));
}

/// Expects the configuration file to be requested once, without finding it.
pub(crate) fn expect_no_config(gh: &mut MockGH) {
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(None))));
}

/// Expects the valid test configuration file to be requested once.
pub(crate) fn expect_valid_config(gh: &mut MockGH) {
    gh.expect_get_config_file()
        .with(eq(INST_ID), eq(ORG), eq(REPO))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(Some(get_test_valid_config())))));
}

/// Returns the content of the invalid test configuration file.
pub(crate) fn get_test_invalid_config() -> String {
    fs::read_to_string(Path::new(TESTDATA_PATH).join("config-invalid.yml")).unwrap()
}

/// Returns the content of the valid test configuration file.
pub(crate) fn get_test_valid_config() -> String {
    fs::read_to_string(Path::new(TESTDATA_PATH).join("config.yml")).unwrap()
}

/// Sets up the minimal vote configuration profile that can be read back.
pub(crate) fn setup_seed_cfg() -> Value {
    json!({"duration": "5m", "pass_threshold": 50.0})
}

/// Sets up an issue comment event on the test issue, sent by the test user.
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
        sender: User {
            login: USER.to_string(),
        },

        organization: Some(Organization {
            login: ORG.to_string(),
        }),
    }
}

/// Sets up an issue event on the test issue, sent by the test user.
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
        sender: User {
            login: USER.to_string(),
        },

        organization: Some(Organization {
            login: ORG.to_string(),
        }),
    }
}

/// Returns a GitHub "Not Found" error as returned by the GitHub client.
pub(crate) async fn setup_test_not_found_error() -> anyhow::Error {
    let api = MockGitHubApi::start().await;
    let client = Octocrab::builder().base_uri(api.base_uri()).unwrap().build().unwrap();
    client.get::<Value, _, ()>("/not-found", None).await.unwrap_err().into()
}

/// Sets up a pull request event on the test pull request, sent by the test user.
pub(crate) fn setup_test_pr_event() -> PullRequestEvent {
    PullRequestEvent {
        action: PullRequestEventAction::Other,
        installation: Installation { id: INST_ID as i64 },
        pull_request: PullRequest {
            base: PullRequestBase {
                reference: BRANCH.to_string(),
            },
            id: ISSUE_ID,
            number: ISSUE_NUM,
            title: TITLE.to_string(),

            body: None,
        },
        repository: Repository {
            full_name: REPOFN.to_string(),
        },
        sender: User {
            login: USER.to_string(),
        },

        organization: Some(Organization {
            login: ORG.to_string(),
        }),
    }
}

/// Sets up an open vote on the test issue, created by the test user, where only the
/// first additional test user has a binding vote.
pub(crate) fn setup_test_vote() -> Vote {
    Vote {
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
        close_attempts: 0,
        close_pending: false,
        closed: false,
        created_at: OffsetDateTime::now_utc(),
        created_by: USER.to_string(),
        ends_at: OffsetDateTime::now_utc(),
        installation_id: INST_ID as i64,
        is_pull_request: false,
        issue_id: ISSUE_ID,
        issue_number: ISSUE_NUM,
        repository_full_name: REPOFN.to_string(),
        vote_comment_id: COMMENT_ID,
        vote_id: Uuid::parse_str(VOTE_ID).unwrap(),

        cancelled_at: None,
        cancelled_by: None,
        checked_at: None,
        close_announcement_posted_at: None,
        close_check_run_created_at: None,
        close_comment_posted_at: None,
        close_head_sha: None,
        close_labels_updated_at: None,
        close_next_attempt_at: None,
        closed_at: None,
        issue_title: Some(TITLE.to_string()),
        organization: Some(ORG.to_string()),
        results: None,
    }
}

/// Sets up the results of a passed vote with a single binding in favor vote cast by
/// the first additional test user.
pub(crate) fn setup_test_vote_results() -> VoteResults {
    VoteResults {
        abstain: 0,
        against: 0,
        against_percentage: 0.0,
        allowed_voters: 1,
        binding: 1,
        in_favor: 1,
        in_favor_percentage: 100.0,
        non_binding: 0,
        not_voted: 0,
        pass_threshold: 50.0,
        passed: true,
        pending_voters: vec![],
        votes: BTreeMap::from([(
            USER1.to_string(),
            UserVote {
                binding: true,
                timestamp: OffsetDateTime::parse(TIMESTAMP, &Rfc3339).unwrap(),
                vote_option: VoteOption::InFavor,
            },
        )]),
    }
}

/// Sets up the test vote created at the time provided, with the results calculated
/// from the allowed voters and reactions given.
pub(crate) fn setup_test_vote_with_calculated_results(
    created_at: &str,
    allowed_voters: Vec<UserName>,
    reactions: Vec<Reaction>,
) -> Vote {
    // Setup GitHub mock
    let mut gh = MockGH::new();
    gh.expect_get_comment_reactions()
        .with(eq(INST_ID), eq(OWNER), eq(REPO), eq(COMMENT_ID))
        .times(1)
        .return_once(move |_, _, _, _| {
            Box::pin(async move { Ok::<Vec<Reaction>, anyhow::Error>(reactions) })
        });
    gh.expect_get_allowed_voters()
        .withf(|inst_id, cfg, owner, repo, org| {
            *inst_id == INST_ID
                && *cfg == setup_test_vote().cfg
                && owner == OWNER
                && repo == REPO
                && *org == setup_test_vote().organization.as_ref()
        })
        .times(1)
        .return_once(move |_, _, _, _, _| {
            Box::pin(async move { Ok::<Vec<UserName>, anyhow::Error>(allowed_voters) })
        });

    // Setup vote
    let mut vote = setup_test_vote();
    vote.created_at = OffsetDateTime::parse(created_at, &Rfc3339).expect("valid created_at timestamp");

    // Calculate results and attach to vote
    let runtime = tokio::runtime::Runtime::new().expect("runtime creation should succeed");
    let results = runtime
        .block_on(calculate(Arc::new(gh), OWNER, REPO, &vote))
        .expect("vote calculation should succeed");
    vote.results = Some(results);

    vote
}

// Helpers.

/// Handles a mock GitHub API request, recording it and returning the matching
/// response.
#[allow(clippy::unused_async)]
async fn handle_mock_github_api_request(
    State(state): State<MockGitHubApiState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // Record request
    let path = uri.path().to_string();
    let authorization = headers.get(header::AUTHORIZATION).map(|value| value.to_str().unwrap().to_string());
    state.requests.lock().unwrap().push(MockGitHubApiRequest {
        method: method.clone(),
        path: path.clone(),

        authorization: authorization.clone(),
        body: serde_json::from_slice(&body).ok(),
        query: uri.query().map(ToString::to_string),
    });

    // Reject requests using the wrong credentials
    if !has_mock_github_api_credentials(&path, authorization.as_deref()) {
        let body = json!({"message": "Bad credentials"});
        return (StatusCode::UNAUTHORIZED, axum::Json(body)).into_response();
    }

    // Return the next registered response or a not found error
    let response = {
        let mut responses = state.responses.lock().unwrap();
        let Some(queued_responses) = responses.get_mut(&(method, path)) else {
            return (StatusCode::NOT_FOUND, axum::Json(json!({"message": "Not Found"}))).into_response();
        };
        if queued_responses.len() > 1 {
            queued_responses.remove(0)
        } else {
            queued_responses[0].clone()
        }
    };

    // Never answer stalled requests
    if response.is_stalled {
        return std::future::pending().await;
    }

    // Prepare the response
    let mut http_response = (response.status, axum::Json(response.body.clone())).into_response();
    if let Some(next_page_path) = &response.next_page_path {
        let link = format!(r#"<{}{next_page_path}>; rel="next""#, state.base_uri);
        http_response.headers_mut().insert(header::LINK, HeaderValue::from_str(&link).unwrap());
    }
    http_response
}

/// Checks if the authorization provided is the one GitHub requires for the path. The
/// application endpoints need the application JWT, and the repository, organization
/// and GraphQL ones the installation token. Other paths are not checked, as the client
/// does not send credentials to the absolute URLs of the mock server's next pages.
fn has_mock_github_api_credentials(path: &str, authorization: Option<&str>) -> bool {
    let is_installation_token = authorization == Some(&format!("Bearer {INSTALLATION_TOKEN}"));
    if path.starts_with("/app/") || path.ends_with("/installation") {
        authorization.is_some_and(|value| value.starts_with("Bearer ")) && !is_installation_token
    } else if path.starts_with("/repos/") || path.starts_with("/orgs/") || path == "/graphql" {
        is_installation_token
    } else {
        true
    }
}

/// Quotes the database identifier provided.
fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// Runs a statement in the maintenance database.
async fn run_maintenance_statement(config: &Config, statement: &str) {
    // Connect to the maintenance database
    let (client, connection) = config.connect(NoTls).await.expect("database should be reachable");
    let connection = tokio::spawn(connection);

    // Run the statement, then wait for the connection to close
    client.batch_execute(statement).await.expect("maintenance statement should succeed");
    drop(client);
    connection.await.unwrap().unwrap();
}
