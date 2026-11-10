//! Checks the configuration rendered by the chart.
//!
//! For each case below the test renders the chart with helm and checks the following:
//!
//! - the database, GitHub, address and log settings of gitvote.yml get the values of the
//!   case, with the types the service expects, except the private key
//! - the database readiness containers receive their settings as strings
//!
//! Other tests check that helm refuses the values it would corrupt, and that it accepts
//! them when they are set as strings. They also check that the chart refuses invalid
//! application ids, and renders valid ones in decimal.
//!
//! They are all ignored by default as they need helm in the PATH, and fail without it.

use std::{
    fmt::Debug,
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::json;
use serde_yaml::Value;

/// Application id setting, in dotted notation.
const APP_ID_FIELD: &str = "gitvote.github.appID";

/// Cases the chart must render. The application id and the port reach the chart as
/// numbers or strings in a values file and with --set, and the database strings as
/// strings in a values file and as numbers with --set. Unquoted numbers in the latter
/// must be rejected, which test_chart_rejects_strings_helm_would_corrupt covers.
const CASES: [Case; 3] = [
    Case {
        addr: "127.0.0.1:8001",
        app_id: 1_234_567,
        db_host: "127.1",
        dbname: "true",
        name: "numbers from a values file and YAML lookalike strings",
        number_style: NumberStyle::Number,
        password: "12345",
        user: "1234",

        webhook_secret_fallback: Some("87654321"),
    },
    Case {
        addr: "[::1]:8002",
        app_id: 12_345_678,
        db_host: "127.0.0.1",
        dbname: "gitvote: db",
        name: "quoted numbers and YAML syntax in strings",
        number_style: NumberStyle::String,
        password: r#"pa: ss #1 "2" \3"#,
        user: r#""null""#,

        webhook_secret_fallback: None,
    },
    Case {
        addr: "127.0.0.1:8003",
        app_id: 7_654_321,
        db_host: "127.0.0.1",
        dbname: "1234567",
        name: "numbers from --set",
        number_style: NumberStyle::Flag,
        password: "12345678",
        user: "7654321",

        webhook_secret_fallback: Some("old-secret"),
    },
];

/// Chart location, relative to the root of the repository.
const CHART_DIR: &str = "charts/gitvote";

/// Database port used in all cases, which differs from the default of the chart.
const DB_PORT: u16 = 5433;

/// Name of the init container that waits for the database.
const DB_READY_CONTAINER: &str = "check-db-ready";

/// Log format used in all cases, which differs from the default of the chart.
const LOG_FORMAT: &str = "pretty";

/// Templates to render, relative to the chart directory.
const RENDERED_TEMPLATES: [&str; 3] = [
    "templates/dbmigrator_job.yaml",
    "templates/gitvote_deployment.yaml",
    "templates/gitvote_secret.yaml",
];

/// Sample values file, relative to the chart directory.
const SAMPLE_VALUES: &str = "ci/default-values.yaml";

/// Values that must reach the service as strings.
const STRING_FIELDS: [&str; 5] = [
    "db.dbname",
    "db.password",
    "db.user",
    "gitvote.github.webhookSecret",
    "gitvote.github.webhookSecretFallback",
];

/// Webhook secret used in all cases.
const WEBHOOK_SECRET: &str = "12345678";

#[test]
#[ignore = "needs helm"]
fn test_chart_rejects_invalid_app_ids() {
    // Check values from a values file, typed by YAML, are refused
    let values = [
        "true",
        "1.5",
        "0",
        "-5",
        "null",
        r#""""#,
        r#""0123""#,
        r#""12a""#,
        r#""+5""#,
        r#""99999999999999999999""#,
        "9007199254740992",
        "9007199254740993",
        "12345678901234567",
    ];
    for value in values {
        let output = run_helm_template(&unquoted_value_yaml(APP_ID_FIELD, value), &[]);
        assert_app_id_rejected(&output.unwrap(), value);
    }

    // Check values from --set, typed by helm, are refused
    for value in ["true", "1.5", "0", "-5", "null", "", "0123"] {
        let flags = ["--set".to_string(), format!("{APP_ID_FIELD}={value}")];
        let output = run_helm_template("{}", &flags);
        assert_app_id_rejected(&output.unwrap(), value);
    }
}

#[test]
#[ignore = "needs helm"]
fn test_chart_rejects_strings_helm_would_corrupt() {
    for field in STRING_FIELDS {
        // Numbers are refused whatever their size, as the value written cannot be told
        // from them, and helm reads numbers from --set back the same way on upgrades
        // using --reuse-values
        for value in ["12345678", "12345", "1.50", "0123", "yes", "off"] {
            // Render the chart with the value unquoted
            let output = run_helm_template(&unquoted_value_yaml(field, value), &[]).unwrap();

            // Check rendering fails because of the value
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(!output.status.success(), "{field}: {value} was accepted");
            let expected = format!("{field} must be a string");
            assert!(stderr.contains(&expected), "{field}: {value}: {stderr}");
            assert!(stderr.contains("--set-string"), "{field}: {value}: {stderr}");
        }
    }
}

#[test]
#[ignore = "needs helm"]
fn test_chart_renders_app_ids_in_decimal() {
    // Setup whole floats from a values file, past the u32 range of the cases, up to
    // the largest one below 2^53, and the largest id the service accepts as a string
    let app_ids = [
        ("1234567.0", 1_234_567),
        ("1e6", 1_000_000),
        ("12345678901", 12_345_678_901),
        ("9007199254740991", 9_007_199_254_740_991),
        (r#""9223372036854775807""#, i64::MAX),
    ];
    for (value, expected) in app_ids {
        // Render the chart with the value unquoted
        let values = unquoted_value_yaml(APP_ID_FIELD, value);
        let manifests = render_templates(&values, &[]).unwrap();

        // Check the id is printed as a decimal number
        let config = extract_config(&manifests).unwrap();
        let app_id = &config["github"]["appId"];
        assert_eq!(app_id, &Value::from(expected), "{value}");
    }
}

#[test]
#[ignore = "needs helm"]
fn test_chart_renders_config_of_cases() {
    // Run all the cases, so that a failure does not hide the others
    let mut failures = vec![];
    for case in &CASES {
        if let Err(err) = check_case(case) {
            failures.push(format!("{}: {err:#}", case.name));
        }
    }

    // Check no case failed
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
#[ignore = "needs helm"]
fn test_chart_renders_string_fields_set_as_strings() {
    for field in STRING_FIELDS {
        for flag in ["--set", "--set-string"] {
            // Render the chart with a number set with the flag provided
            let flags = [flag.to_string(), format!("{field}=12345678")];
            let output = run_helm_template("{}", &flags).unwrap();

            // Check the value is printed unchanged in the configuration
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(output.status.success(), "{field} with {flag}: {stderr}");
            let stdout = String::from_utf8_lossy(&output.stdout);
            let key = field.rsplit('.').next().unwrap();
            let expected = format!("{key}: \"12345678\"");
            assert!(stdout.contains(&expected), "{field} with {flag}: {stdout}");
        }
    }
}

// Helpers.

/// Chart input of a case.
struct Case {
    /// Address the service listens on.
    addr: &'static str,
    /// GitHub application id.
    app_id: u32,
    /// Database host.
    db_host: &'static str,
    /// Database name.
    dbname: &'static str,
    /// Description of the case, used in failure messages.
    name: &'static str,
    /// How the numbers of the case reach helm.
    number_style: NumberStyle,
    /// Database password.
    password: &'static str,
    /// Database user.
    user: &'static str,

    /// Fallback webhook secret, left unset when missing.
    webhook_secret_fallback: Option<&'static str>,
}

/// How numbers reach helm: as a number or a string in a values file, or using --set,
/// which also sets the database strings of the case.
enum NumberStyle {
    /// Numbers and database strings are passed with --set.
    Flag,
    /// Numbers are written as numbers in a values file.
    Number,
    /// Numbers are written as strings in a values file.
    String,
}

/// Checks helm failed to render the chart because of the application id given.
fn assert_app_id_rejected(output: &Output, value: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{value} was accepted");
    let expected = format!("{APP_ID_FIELD} must be the ID of the GitHub App");
    assert!(stderr.contains(&expected), "{value}: {stderr}");
}

/// Renders the chart for a case and checks the manifests.
fn check_case(case: &Case) -> Result<()> {
    // Render the chart for the case
    let manifests = render_manifests(case)?;

    // Check the database readiness containers
    for kind in ["Deployment", "Job"] {
        let manifest = find_manifest(&manifests, kind)?;
        check_db_ready_env(manifest, case).context(kind)?;
    }

    // Check the service configuration
    check_config(&extract_config(&manifests)?, case)
}

/// Checks the database, GitHub, address and log settings of the configuration have the
/// values of the case. YAML values are compared, so a number printed in place of a
/// string, or a missing or extra setting in those sections, is detected. The private
/// key, taken from the sample values, and the set of top-level keys are not checked.
fn check_config(config: &Value, case: &Case) -> Result<()> {
    // Check the database settings
    let expected_db = json!({
        "dbname": case.dbname,
        "host": case.db_host,
        "password": case.password,
        "port": DB_PORT,
        "user": case.user,
    });
    check_equal("db", &config["db"], &serde_yaml::to_value(expected_db)?)?;

    // Check the GitHub settings, except the private key taken from the sample values
    let mut github = config["github"].clone();
    let github_settings = github.as_mapping_mut().context("github settings not found")?;
    github_settings.remove("appPrivateKey").context("private key not found")?;
    let mut expected_github = json!({
        "appId": case.app_id,
        "webhookSecret": WEBHOOK_SECRET,
    });
    if let Some(fallback) = case.webhook_secret_fallback {
        expected_github["webhookSecretFallback"] = json!(fallback);
    }
    check_equal("github", &github, &serde_yaml::to_value(expected_github)?)?;

    // Check the address and log settings
    check_equal("addr", &config["addr"], &Value::from(case.addr))?;
    let expected_log = json!({ "format": LOG_FORMAT });
    check_equal("log", &config["log"], &serde_yaml::to_value(expected_log)?)
}

/// Checks the database readiness container of the manifest gets strings with the values
/// of the case, as Kubernetes rejects any other type in the environment of a container.
fn check_db_ready_env(manifest: &Value, case: &Case) -> Result<()> {
    // Find the environment of the container
    let containers = manifest["spec"]["template"]["spec"]["initContainers"].as_sequence();
    let container = containers
        .into_iter()
        .flatten()
        .find(|container| container["name"].as_str() == Some(DB_READY_CONTAINER))
        .context("database readiness container not found")?;
    let env = container["env"].as_sequence().context("container has no environment")?;

    // Check the variables, as_str returns a value only for YAML strings
    let db_port = DB_PORT.to_string();
    let expected_values = [
        ("PGHOST", case.db_host),
        ("PGPORT", db_port.as_str()),
        ("PGUSER", case.user),
    ];
    for (name, expected_value) in expected_values {
        let variable = env.iter().find(|variable| variable["name"].as_str() == Some(name));
        let value = variable.and_then(|variable| variable["value"].as_str());
        check_equal(name, &value, &Some(expected_value))?;
    }

    Ok(())
}

/// Fails if the value observed is not the one expected.
fn check_equal<T: Debug + PartialEq>(what: &str, actual: &T, expected: &T) -> Result<()> {
    ensure!(
        actual == expected,
        "{what}: expected {expected:?}, got {actual:?}"
    );
    Ok(())
}

/// Returns the parsed gitvote.yml of the rendered secret.
fn extract_config(manifests: &[Value]) -> Result<Value> {
    let secret = find_manifest(manifests, "Secret")?;
    let config = secret["stringData"]["gitvote.yml"].as_str();
    let config = config.context("gitvote.yml not found in the rendered secret")?;
    serde_yaml::from_str(config).context("error parsing gitvote.yml")
}

/// Returns the rendered manifest of the kind given.
fn find_manifest<'a>(manifests: &'a [Value], kind: &str) -> Result<&'a Value> {
    let manifest = manifests.iter().find(|manifest| manifest["kind"].as_str() == Some(kind));
    manifest.with_context(|| format!("{kind} not found in the rendered manifests"))
}

/// Renders the chart for a case and returns the manifests of the templates to check.
fn render_manifests(case: &Case) -> Result<Vec<Value>> {
    // Setup the values, keeping the number and string types of the case
    let mut values = json!({
        "db": {
            "dbname": case.dbname,
            "host": case.db_host,
            "password": case.password,
            "user": case.user,
        },
        "gitvote": {
            "addr": case.addr,
            "github": {
                "webhookSecret": WEBHOOK_SECRET,
                "webhookSecretFallback": case.webhook_secret_fallback,
            },
        },
        "log": {
            "format": LOG_FORMAT,
        },
    });

    // Pass the numbers in the style of the case
    let mut flags = vec![];
    match case.number_style {
        NumberStyle::Flag => {
            // These take precedence over the values
            let settings = [
                format!("db.dbname={}", case.dbname),
                format!("db.password={}", case.password),
                format!("db.port={DB_PORT}"),
                format!("db.user={}", case.user),
                format!("gitvote.github.appID={}", case.app_id),
            ];
            for setting in settings {
                flags.extend(["--set".to_string(), setting]);
            }
        }
        NumberStyle::Number => {
            values["db"]["port"] = json!(DB_PORT);
            values["gitvote"]["github"]["appID"] = json!(case.app_id);
        }
        NumberStyle::String => {
            values["db"]["port"] = json!(DB_PORT.to_string());
            values["gitvote"]["github"]["appID"] = json!(case.app_id.to_string());
        }
    }

    // Render the templates, JSON is valid YAML and keeps the types chosen above
    render_templates(&values.to_string(), &flags)
}

/// Renders the chart using the values and flags provided, as run_helm_template does,
/// and returns the manifests of the templates to check.
fn render_templates(values: &str, flags: &[String]) -> Result<Vec<Value>> {
    // Render the templates
    let output = run_helm_template(values, flags)?;
    if !output.status.success() {
        bail!(
            "helm template failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    // Parse the manifests
    let stdout = String::from_utf8(output.stdout)?;
    let manifests = serde_yaml::Deserializer::from_str(&stdout).map(Value::deserialize);
    manifests.collect::<Result<_, _>>().context("error parsing the rendered manifests")
}

/// Runs helm template using the sample values, the values given, which helm reads from
/// its standard input, and the flags provided.
fn run_helm_template(values: &str, flags: &[String]) -> Result<Output> {
    // Start helm
    let chart_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(CHART_DIR);
    let mut command = Command::new("helm");
    command.args(["template", "gitvote"]).arg(&chart_dir);
    for template in RENDERED_TEMPLATES {
        command.args(["--show-only", template]);
    }
    let mut child = command
        .arg("--values")
        .arg(chart_dir.join(SAMPLE_VALUES))
        .args(["--values", "-"])
        .args(flags)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("error running helm, which must be in the PATH")?;

    // Send the values, closing its standard input once written
    let mut stdin = child.stdin.take().context("helm standard input not available")?;
    stdin.write_all(values.as_bytes()).context("error sending values to helm")?;
    drop(stdin);

    // Wait for helm to exit and collect its output
    child.wait_with_output().context("error waiting for helm")
}

/// Builds a YAML document setting the field provided, in dotted notation, to the value
/// given without quoting it.
fn unquoted_value_yaml(field: &str, value: &str) -> String {
    field.rsplit('.').fold(value.to_string(), |inner, key| format!("{{{key}: {inner}}}"))
}
