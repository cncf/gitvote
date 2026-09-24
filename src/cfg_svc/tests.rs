use std::{env, fs, path::PathBuf, process};

use super::*;

/// Configuration file with only the required fields.
const MINIMAL_CONFIG: &str = r"
db:
  host: db.example.com
github:
  appId: 1234
  appPrivateKey: key
  webhookSecret: secret
";

#[test]
fn cfg_load_applies_defaults() {
    // Setup configuration file
    let file = TempConfigFile::new("defaults", MINIMAL_CONFIG);

    // Load configuration
    let cfg = load_cfg(&file.path).unwrap();

    // Check defaults are applied
    assert_eq!(cfg.addr, "127.0.0.1:9000");
    assert_eq!(cfg.db.host, Some("db.example.com".to_string()));
    assert_eq!(
        cfg.github,
        GitHubApp {
            app_id: 1234,
            app_private_key: "key".to_string(),
            webhook_secret: "secret".to_string(),
            webhook_secret_fallback: None,
        }
    );
    assert_eq!(cfg.log.format, LogFormat::Pretty);
}

#[test]
fn cfg_load_invalid_log_format_returns_error() {
    // Setup configuration file
    let file = TempConfigFile::new(
        "invalid-log-format",
        &format!("{MINIMAL_CONFIG}log:\n  format: xml\n"),
    );

    // Check configuration cannot be loaded
    let err = load_cfg(&file.path).unwrap_err();
    assert!(err.to_string().contains("xml"), "unexpected error: {err}");
}

#[test]
fn cfg_load_loads_all_fields_from_file() {
    // Setup configuration file
    let file = TempConfigFile::new(
        "all-fields",
        r"
addr: 0.0.0.0:9000
db:
  host: db.example.com
  port: 5433
  dbname: gitvote
  user: postgres
  password: pass
github:
  appId: 1234
  appPrivateKey: key
  webhookSecret: secret
  webhookSecretFallback: old-secret
log:
  format: json
",
    );

    // Load configuration
    let cfg = load_cfg(&file.path).unwrap();

    // Check values match the file content
    assert_eq!(cfg.addr, "0.0.0.0:9000");
    assert_eq!(cfg.db.host, Some("db.example.com".to_string()));
    assert_eq!(cfg.db.port, Some(5433));
    assert_eq!(cfg.db.dbname, Some("gitvote".to_string()));
    assert_eq!(cfg.db.user, Some("postgres".to_string()));
    assert_eq!(cfg.db.password, Some("pass".to_string()));
    assert_eq!(
        cfg.github,
        GitHubApp {
            app_id: 1234,
            app_private_key: "key".to_string(),
            webhook_secret: "secret".to_string(),
            webhook_secret_fallback: Some("old-secret".to_string()),
        }
    );
    assert_eq!(cfg.log.format, LogFormat::Json);
}

#[test]
fn cfg_load_missing_required_field_returns_error() {
    // Setup configuration file without the webhook secret
    let file = TempConfigFile::new(
        "missing-required-field",
        r"
db:
  host: db.example.com
github:
  appId: 1234
  appPrivateKey: key
",
    );

    // Check configuration cannot be loaded
    let err = load_cfg(&file.path).unwrap_err();
    assert!(
        err.to_string().contains("webhookSecret"),
        "unexpected error: {err}"
    );
}

// Helpers.

/// Load the configuration from the file provided, ignoring the process
/// environment so that local `GITVOTE_*` variables cannot affect the tests.
fn load_cfg(config_file: &Path) -> Result<Cfg> {
    Cfg::load(config_file, Env::prefixed("GITVOTE_").filter(|_| false))
}

/// Temporary configuration file removed when dropped.
struct TempConfigFile {
    path: PathBuf,
}

impl TempConfigFile {
    /// Create a new temporary configuration file with the content provided.
    fn new(name: &str, content: &str) -> Self {
        let path = env::temp_dir().join(format!("gitvote-cfg-svc-{name}-{}.yml", process::id()));
        fs::write(&path, content).unwrap();
        Self { path }
    }
}

impl Drop for TempConfigFile {
    fn drop(&mut self) {
        _ = fs::remove_file(&self.path);
    }
}
