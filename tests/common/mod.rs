//! Helpers shared by the integration tests that run the real service binary.

use std::{
    env,
    fs::{self, File},
    net::{Ipv4Addr, TcpListener},
    path::{Path, PathBuf},
    process::{self, Child, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicU32, Ordering},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Error, Result, bail, ensure, format_err};
use tokio::time::sleep;

/// Name of the file in the temporary directory that receives the service output.
const OUTPUT_FILE: &str = "service.log";

/// Amount of time between checks of the state of the service.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Amount of time the readiness check waits for each answer from the service.
const READINESS_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// Amount of time the service has to get ready or to exit.
const SERVICE_TIMEOUT: Duration = Duration::from_secs(30);

/// Number of temporary directories created by this test process.
static TEMP_DIRS_CREATED: AtomicU32 = AtomicU32::new(0);

/// Running service process, killed on drop so that a failed test does not leak it.
pub(crate) struct Service {
    /// Service process.
    child: Child,
    /// File that receives the output of the service.
    output_file: PathBuf,
}

impl Service {
    /// Starts the service with the configuration file given, the same way the
    /// deployment does, sending its output to a file in the directory provided.
    pub(crate) fn start(dir: &TempDir, config_file: &Path) -> Result<Self> {
        // Setup the output file, shared by stdout and stderr to keep the lines in order
        let output_file = dir.path().join(OUTPUT_FILE);
        let stdout = File::create(&output_file).context("error creating service output file")?;
        let stderr = stdout.try_clone().context("error cloning service output file")?;

        // Start the service, without the settings of the environment running the tests
        let mut command = Command::new(env!("CARGO_BIN_EXE_gitvote"));
        command
            .arg("-c")
            .arg(config_file)
            .env_remove("RUST_LOG")
            .stderr(Stdio::from(stderr))
            .stdout(Stdio::from(stdout));
        for (name, _) in env::vars_os() {
            if name.to_string_lossy().starts_with("GITVOTE_") {
                command.env_remove(name);
            }
        }
        let child = command.spawn().context("error starting service")?;

        Ok(Self { child, output_file })
    }

    /// Adds the output produced so far by the service to the error given.
    pub(crate) fn add_output(&self, err: &Error) -> Error {
        format_err!("{err:#}\nservice output:\n{}", self.output())
    }

    /// Returns the output produced so far by the service.
    pub(crate) fn output(&self) -> String {
        fs::read_to_string(&self.output_file).unwrap_or_default()
    }

    /// Sends a SIGTERM to the service and waits for it to exit.
    pub(crate) fn stop(&mut self) -> Result<ExitStatus> {
        // Ask the service to stop
        let pid = self.child.id().to_string();
        let kill_status =
            Command::new("kill").args(["-TERM", &pid]).status().context("error running kill")?;
        ensure!(kill_status.success(), "kill failed: {kill_status}");

        // Wait for the service to exit
        let deadline = Instant::now() + SERVICE_TIMEOUT;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().context("error checking service")? {
                return Ok(status);
            }
            thread::sleep(POLL_INTERVAL);
        }
        bail!("service did not exit after SIGTERM");
    }

    /// Waits for the service to answer on the address given, failing if it exits first.
    pub(crate) async fn wait_until_ready(&mut self, addr: &str) -> Result<()> {
        // Setup the readiness request and its deadline
        let client = http_client()?;
        let url = format!("http://{addr}/");
        let deadline = Instant::now() + SERVICE_TIMEOUT;
        while Instant::now() < deadline {
            // The service must be the one answering, not a process reusing its address
            if let Some(status) = self.child.try_wait().context("error checking service")? {
                bail!("service exited early: {status}");
            }

            // Check if the service is listening
            let response = client.get(&url).timeout(READINESS_REQUEST_TIMEOUT).send().await;
            if response.is_ok_and(|response| response.status().is_success()) {
                return Ok(());
            }
            sleep(POLL_INTERVAL).await;
        }
        bail!("service did not answer on {url}");
    }
}

/// Temporary directory in the target directory, removed on drop.
pub(crate) struct TempDir {
    /// Location of the directory.
    path: PathBuf,
}

impl TempDir {
    /// Creates a new temporary directory.
    pub(crate) fn new() -> Result<Self> {
        let id = TEMP_DIRS_CREATED.fetch_add(1, Ordering::Relaxed);
        let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{}-{id}", process::id()));
        fs::create_dir_all(&path).context("error creating temporary directory")?;
        Ok(Self { path })
    }

    /// Returns the path of the directory.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Service {
    /// [`Drop::drop`].
    fn drop(&mut self) {
        // Both are no-ops when the service has already exited and been waited for
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for TempDir {
    /// [`Drop::drop`].
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Returns a local port that is currently free.
pub(crate) fn free_port() -> Result<u16> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).context("error binding free port")?;
    Ok(listener.local_addr().context("error getting free port")?.port())
}

/// Returns an HTTP client for the service, ignoring any proxy set in the environment.
fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder().no_proxy().build().context("error building http client")
}
