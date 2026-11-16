use std::{io, net::TcpStream, time::Duration};

use axum::http::{Method, StatusCode};
use serde_json::{Value, json};
use tokio::{
    net::{TcpListener, TcpSocket},
    time::{self, Instant},
};

use crate::testutil::*;

use super::*;

/// Time after which a connection to a listener is considered not to be progressing.
const CONNECT_PROBE_TIMEOUT: Duration = Duration::from_millis(250);

/// Size of a request body that cannot fit in the socket buffers of a connection.
const LARGE_BODY_SIZE: usize = 16 * 1024 * 1024;

/// Local address that makes the system assign a free port to the listener using it.
const LISTENER_ADDR: &str = "127.0.0.1:0";

/// Maximum time a request that timed out can take over the timeouts that apply to it.
const REQUEST_TIMEOUT_MARGIN: Duration = Duration::from_secs(1);

/// Maximum time to wait for a stalled request to fail by itself. Tests run on paused
/// time, so it is not waited for in practice.
const STALLED_REQUEST_LIMIT: Duration = Duration::from_secs(600);

#[tokio::test]
async fn gh_api_read_request_is_retried_after_server_error() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    let path = "/repos/org/repo/collaborators/user";
    api.respond(
        Method::GET,
        path,
        StatusCode::BAD_GATEWAY,
        json!({"message": "Bad Gateway"}),
    );
    api.respond(Method::GET, path, StatusCode::NO_CONTENT, Value::Null);
    let gh = setup_test_gh_api(&api);

    // Check the request succeeds after being sent again
    assert!(gh.user_is_collaborator(INST_ID, ORG, REPO, USER).await.unwrap());
    assert_eq!(api.requests(&Method::GET, path).len(), 2);
}

#[tokio::test(start_paused = true)]
async fn gh_api_read_request_is_retried_after_timeout() {
    // Setup mock GitHub API
    let api = MockGitHubApi::start().await;
    let path = "/repos/org/repo/collaborators/user";
    api.stall(Method::GET, path);
    api.respond(Method::GET, path, StatusCode::NO_CONTENT, Value::Null);
    let gh = setup_test_gh_api_with_short_timeouts(&api);

    // Check the request succeeds after being sent again
    assert!(gh.user_is_collaborator(INST_ID, ORG, REPO, USER).await.unwrap());
    assert_eq!(api.requests(&Method::GET, path).len(), 2);
}

#[tokio::test(start_paused = true)]
async fn gh_api_request_times_out_when_the_server_does_not_accept_connections() {
    // Setup a server that does not take more connections
    let (listener, _connections) = setup_test_full_listener();
    let base_uri = format!("http://{}", listener.local_addr().unwrap());

    // Setup GitHub client
    let gh = setup_test_gh_api_for_uri(&base_uri);

    // Run a request and check it fails by itself after four attempts of ten seconds
    let started_at = Instant::now();
    let request = gh.get_repository_installation_id(ORG, REPO);
    let err = time::timeout(STALLED_REQUEST_LIMIT, request).await.expect("request should time out");
    assert_timeout_error(&err.unwrap_err());
    assert_timeouts_took(started_at, Duration::from_secs(40));
}

#[tokio::test(start_paused = true)]
async fn gh_api_request_times_out_when_the_server_does_not_read() {
    // Setup a server that takes connections and never reads from them, using a small
    // buffer so that the size of the request needed does not depend on the system
    let socket = TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(4096).unwrap();
    socket.bind(LISTENER_ADDR.parse().unwrap()).unwrap();
    let listener = socket.listen(1).unwrap();
    let base_uri = format!("http://{}", listener.local_addr().unwrap());

    // Setup GitHub client, without the read timeout that would also end the request
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let client = build_test_client(single_attempt_client_builder().set_read_timeout(None), &base_uri);

    // Run a request and check it fails by itself after thirty seconds
    let started_at = Instant::now();
    let body = "a".repeat(LARGE_BODY_SIZE);
    let request = client.post::<_, Value>("/stalled", Some(&body));
    let result = time::timeout(STALLED_REQUEST_LIMIT, request).await.expect("request should time out");
    assert_timeout_error(&result.unwrap_err().into());
    assert_timeouts_took(started_at, Duration::from_secs(30));
}

#[tokio::test(start_paused = true)]
async fn gh_api_request_times_out_when_the_server_stalls() {
    // Setup a server that accepts connections and never responds
    let listener = TcpListener::bind(LISTENER_ADDR).await.unwrap();
    let base_uri = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let mut connections = vec![];
        while let Ok((connection, _)) = listener.accept().await {
            connections.push(connection);
        }
    });

    // Setup GitHub client
    let gh = setup_test_gh_api_for_uri(&base_uri);

    // Run a request and check it fails by itself after four attempts of thirty seconds
    let started_at = Instant::now();
    let request = gh.get_repository_installation_id(ORG, REPO);
    let err = time::timeout(STALLED_REQUEST_LIMIT, request).await.expect("request should time out");
    assert_timeout_error(&err.unwrap_err());
    assert_timeouts_took(started_at, Duration::from_secs(120));
}

// Helpers.

/// Checks the time elapsed since the instant provided is the one expected.
fn assert_timeouts_took(started_at: Instant, expected: Duration) {
    let elapsed = started_at.elapsed();
    let range = expected..expected + REQUEST_TIMEOUT_MARGIN;
    assert!(range.contains(&elapsed), "{elapsed:?} not in {range:?}");
}

/// Sets up a listener whose accept queue is full, so that new connections to it stall.
/// It is returned with the connections filling the queue, which must be kept open.
fn setup_test_full_listener() -> (TcpListener, Vec<TcpStream>) {
    // Setup a listener with the shortest accept queue
    let socket = TcpSocket::new_v4().unwrap();
    socket.bind(LISTENER_ADDR.parse().unwrap()).unwrap();
    let listener = socket.listen(1).unwrap();
    let addr = listener.local_addr().unwrap();

    // Connect to it until a connection does not progress
    let mut connections = vec![];
    loop {
        match TcpStream::connect_timeout(&addr, CONNECT_PROBE_TIMEOUT) {
            Ok(connection) => connections.push(connection),
            Err(err) => {
                assert_eq!(err.kind(), io::ErrorKind::TimedOut);
                return (listener, connections);
            }
        }
    }
}
