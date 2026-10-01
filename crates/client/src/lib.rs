//! Surface Coordinator device agent.
//!
//! The core here is platform independent: it keeps one outbound WebSocket to
//! the coordinator, reconnects with backoff, sends heartbeats and runs jobs.
//! Anything that touches the operating system goes through
//! [`platform::Platform`].

mod backoff;
mod connection;
mod jobs;
mod ledger;
pub mod platform;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub use backoff::BackoffConfig;
use backoff::Backoff;
use connection::Ended;
use jobs::{Job, JobManager};
use platform::Platform;

#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Base URL of the coordinator, `ws://host:port` or `wss://host`.
    pub coordinator_url: String,
    pub key_id: String,
    /// The enrollment secret. Never sent; only used to compute MACs.
    pub key_secret: String,
    pub device_id: String,
    pub client_version: String,
    /// Where the record of started jobs is kept. Without it, duplicate
    /// protection does not survive a restart of the client.
    pub state_dir: Option<PathBuf>,
    pub backoff: BackoffConfig,
    pub handshake_timeout: Duration,
}

impl ClientConfig {
    pub fn new(
        coordinator_url: impl Into<String>,
        key_id: impl Into<String>,
        key_secret: impl Into<String>,
        device_id: impl Into<String>,
    ) -> ClientConfig {
        ClientConfig {
            coordinator_url: coordinator_url.into(),
            key_id: key_id.into(),
            key_secret: key_secret.into(),
            device_id: device_id.into(),
            client_version: env!("CARGO_PKG_VERSION").to_owned(),
            state_dir: None,
            backoff: BackoffConfig::default(),
            handshake_timeout: Duration::from_secs(10),
        }
    }
}

/// Returns the device id stored in `state_dir`, generating and storing one
/// the first time.
pub fn load_or_create_device_id(state_dir: &Path) -> io::Result<String> {
    std::fs::create_dir_all(state_dir)?;
    let path = state_dir.join("device_id");
    match std::fs::read_to_string(&path) {
        Ok(existing) if !existing.trim().is_empty() => Ok(existing.trim().to_owned()),
        Ok(_) => write_new_device_id(&path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => write_new_device_id(&path),
        Err(error) => Err(error),
    }
}

fn write_new_device_id(path: &Path) -> io::Result<String> {
    let id = uuid::Uuid::new_v4().simple().to_string();
    std::fs::write(path, &id)?;
    Ok(id)
}

pub struct Client<P: Platform> {
    config: ClientConfig,
    platform: Arc<P>,
    jobs: JobManager,
    queue: mpsc::UnboundedReceiver<Job>,
}

impl<P: Platform> Client<P> {
    pub fn new(config: ClientConfig, platform: Arc<P>) -> io::Result<Client<P>> {
        let (jobs, queue) = JobManager::new(config.state_dir.as_deref())?;
        Ok(Client { config, platform, jobs, queue })
    }

    /// Runs until `shutdown` is cancelled, reconnecting whenever the
    /// connection is lost.
    pub async fn run(self, shutdown: CancellationToken) {
        let Client { config, platform, jobs, queue } = self;
        let system = platform.system_info();
        // Jobs keep running while the connection is down.
        let worker = tokio::spawn(jobs::run_worker(jobs.clone(), platform, queue));

        let mut backoff = Backoff::new(config.backoff.clone());
        loop {
            match connection::run(&config, &system, &jobs, &shutdown).await {
                Ended::Shutdown => break,
                Ended::Lost { reason, connected_for } => {
                    if connected_for.is_some_and(|d| d >= config.backoff.stable_after) {
                        backoff.reset();
                    }
                    let delay = backoff.next_delay();
                    tracing::warn!(%reason, retry_in = ?delay, "not connected to coordinator");
                    tokio::select! {
                        _ = shutdown.cancelled() => break,
                        _ = tokio::time::sleep(delay) => {}
                    }
                }
            }
        }
        // A job still running is cut short here; it is recorded as started,
        // so the next run reports it as interrupted.
        worker.abort();
    }
}
