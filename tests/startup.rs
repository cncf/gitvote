//! Smoke test that runs the real service binary. It needs no PostgreSQL server: the
//! service connects to the database lazily and keeps serving when it is unavailable.

use std::{fs, path::Path};

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use serde_yaml::Value as YamlValue;

use common::{Service, TempDir, free_port};

mod common;

/// Sample values of the chart, which provide a valid private key.
const CHART_SAMPLE_VALUES: &str = "charts/gitvote/ci/default-values.yaml";

/// Message logged by the service once it has stopped.
const STOPPED_MESSAGE: &str = "gitvote service stopped";

#[tokio::test]
async fn test_service_starts_without_database_and_stops_on_sigterm() -> Result<()> {
    // Setup a configuration with no database listening on the configured port
    let dir = TempDir::new()?;
    let addr = format!("127.0.0.1:{}", free_port()?);
    let config_file = dir.path().join("gitvote.yml");
    fs::write(&config_file, service_config(&addr, free_port()?)?.to_string())?;

    // Start the service and wait for it to answer
    let mut service = Service::start(&dir, &config_file)?;
    service.wait_until_ready(&addr).await.map_err(|err| service.add_output(&err))?;

    // Ask the service to stop
    let status = service.stop().map_err(|err| service.add_output(&err))?;

    // Check it has shut down cleanly
    let output = service.output();
    ensure!(
        status.success(),
        "service failed: {status}\nservice output:\n{output}"
    );
    ensure!(
        output.contains(STOPPED_MESSAGE),
        "service did not stop cleanly:\n{output}"
    );

    Ok(())
}

// Helpers.

/// Returns the sample private key of the chart.
fn sample_private_key() -> Result<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(CHART_SAMPLE_VALUES);
    let values: YamlValue = serde_yaml::from_str(&fs::read_to_string(path)?)?;
    let key = values["gitvote"]["github"]["appPrivateKey"].as_str();
    key.map(str::to_string).context("private key not found in the sample values")
}

/// Returns the service configuration to use. JSON is valid YAML.
fn service_config(addr: &str, db_port: u16) -> Result<Value> {
    Ok(json!({
        "db": {
            "dbname": "gitvote",
            "host": "127.0.0.1",
            "password": "gitvote",
            "port": db_port,
            "user": "gitvote",
        },
        "github": {
            "appId": 1234567,
            "appPrivateKey": sample_private_key()?,
            "webhookSecret": "12345678",
        },
        "addr": addr,
    }))
}
