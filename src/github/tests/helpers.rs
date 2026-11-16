use std::{io, sync::LazyLock, time::Duration};

use aws_lc_rs::{
    encoding::AsDer,
    rsa::{KeyPair, KeySize},
};
use axum::http::{Method, StatusCode};
use base64::{Engine, engine::general_purpose::STANDARD};
use jsonwebtoken::EncodingKey;
use octocrab::models::AppId;
use serde_json::{Value, json};

use crate::testutil::*;

use super::*;

/// Read timeout used in the tests. They run on paused time, so it only elapses once
/// nothing else can make progress.
const TEST_READ_TIMEOUT: Duration = Duration::from_millis(500);

/// Private key used to authenticate as the test GitHub application.
static APP_PRIVATE_KEY: LazyLock<EncodingKey> = LazyLock::new(|| {
    // Generate a key pair
    let key_pair = KeyPair::generate(KeySize::Rsa2048).unwrap();
    let der = key_pair.as_der().unwrap();

    // Encode it as the PEM the GitHub client expects
    let pem = format!(
        "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
        STANDARD.encode(der.as_ref())
    );
    EncodingKey::from_rsa_pem(pem.as_bytes()).unwrap()
});

/// Checks the error provided was caused by a timeout of the GitHub client.
pub(super) fn assert_timeout_error(err: &anyhow::Error) {
    let is_timeout = |source: &(dyn std::error::Error + 'static)| {
        source
            .downcast_ref::<io::Error>()
            .is_some_and(|err| err.kind() == io::ErrorKind::TimedOut)
    };
    assert!(err.chain().any(is_timeout), "{err:?}");
}

/// Builds a GitHub user JSON object for the login provided.
pub(super) fn author_json(login: &str) -> Value {
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

/// Builds a GitHub client from the builder provided that uses the base URI given.
pub(super) fn build_test_client(builder: ClientBuilder, base_uri: &str) -> Octocrab {
    builder
        .base_uri(base_uri)
        .unwrap()
        .app(AppId(TEST_APP_ID), APP_PRIVATE_KEY.clone())
        .build()
        .unwrap()
}

/// Returns the path of the requests creating an installation token for the test
/// installation.
pub(super) fn installation_token_path() -> String {
    format!("/app/installations/{INST_ID}/access_tokens")
}

/// Builds a GitHub pull request JSON object.
pub(super) fn pr_json() -> Value {
    json!({
        "number": ISSUE_NUM,
        "head": {"ref": "feature", "sha": HEAD_SHA}
    })
}

/// Sets up a `GHApi` instance that uses the mock GitHub API provided.
pub(super) fn setup_test_gh_api(api: &MockGitHubApi) -> GHApi {
    respond_with_installation_token(api);
    setup_test_gh_api_for_uri(api.base_uri())
}

/// Sets up a `GHApi` instance that uses the base URI provided.
pub(super) fn setup_test_gh_api_for_uri(base_uri: &str) -> GHApi {
    // Install the crypto provider, which fails when another test already installed it
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    // Setup application clients
    GHApi::new(
        TEST_APP_ID,
        build_test_client(retrying_client_builder(), base_uri),
        build_test_client(single_attempt_client_builder(), base_uri),
    )
}

/// Sets up a `GHApi` instance that uses the mock GitHub API provided, whose requests
/// time out after `TEST_READ_TIMEOUT`.
pub(super) fn setup_test_gh_api_with_short_timeouts(api: &MockGitHubApi) -> GHApi {
    // Setup installation token response
    respond_with_installation_token(api);

    // Setup application clients with short read timeouts
    GHApi::new(
        TEST_APP_ID,
        build_test_client(
            retrying_client_builder().set_read_timeout(Some(TEST_READ_TIMEOUT)),
            api.base_uri(),
        ),
        build_test_client(
            single_attempt_client_builder().set_read_timeout(Some(TEST_READ_TIMEOUT)),
            api.base_uri(),
        ),
    )
}

/// Registers the response to the installation token requests in the mock GitHub API.
fn respond_with_installation_token(api: &MockGitHubApi) {
    api.respond(
        Method::POST,
        &installation_token_path(),
        StatusCode::CREATED,
        json!({"token": INSTALLATION_TOKEN, "permissions": {}}),
    );
}
