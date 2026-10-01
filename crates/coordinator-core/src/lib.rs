//! Surface Coordinator server logic.
//!
//! Start a server on an address with a data directory and an auth mode; stop
//! it. The `coordinator` binary is a thin wrapper around this, and the
//! manager embeds the same library.

mod api;
mod client_ws;
mod state;
mod store;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use state::AppState;
pub use store::EnrollmentKey;
use store::Store;

/// How the HTTP API authenticates callers.
#[derive(Debug, Clone)]
pub enum AuthMode {
    /// Every API request must carry `Authorization: Bearer <token>`.
    Token(String),
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Address to listen on. Port 0 picks a free port; see
    /// [`Coordinator::local_addr`].
    pub bind: SocketAddr,
    /// Directory holding the database. Created if missing.
    pub data_dir: PathBuf,
    pub auth: AuthMode,
    /// Interval announced to clients. A device is marked offline after
    /// three intervals without a message.
    pub heartbeat_interval: Duration,
    /// How long a job may stay queued for an offline device.
    pub queued_expiry: Duration,
    /// How often queued targets are checked for expiry.
    pub expiry_sweep_interval: Duration,
}

impl Config {
    pub fn new(bind: SocketAddr, data_dir: impl Into<PathBuf>, auth: AuthMode) -> Config {
        Config {
            bind,
            data_dir: data_dir.into(),
            auth,
            heartbeat_interval: Duration::from_secs(protocol::DEFAULT_HEARTBEAT_INTERVAL_SECS),
            queued_expiry: Duration::from_secs(24 * 60 * 60),
            expiry_sweep_interval: Duration::from_secs(60),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("internal error: {0}")]
    Internal(String),
}

/// A running coordinator.
pub struct Coordinator {
    local_addr: SocketAddr,
    state: Arc<AppState>,
    server: JoinHandle<()>,
}

/// Starts a coordinator. It runs until [`Coordinator::stop`] is called.
pub async fn start(config: Config) -> Result<Coordinator, Error> {
    let AuthMode::Token(token) = &config.auth;
    if token.is_empty() {
        return Err(Error::InvalidConfig("the API token must not be empty".into()));
    }
    if config.heartbeat_interval.is_zero() {
        return Err(Error::InvalidConfig("the heartbeat interval must not be zero".into()));
    }

    let data_dir = config.data_dir.clone();
    let store = tokio::task::spawn_blocking(move || Store::open(&data_dir))
        .await
        .map_err(|e| Error::Internal(e.to_string()))??;
    store.call(|conn| store::mark_all_devices_offline(conn)).await?;

    let listener = TcpListener::bind(config.bind).await?;
    let local_addr = listener.local_addr()?;
    let state = Arc::new(AppState::new(config, store));

    state.tasks.spawn(expiry_sweep(state.clone()));

    let router = client_ws::router().merge(api::router(state.clone())).with_state(state.clone());
    let shutdown = state.shutdown.clone();
    let server = tokio::spawn(async move {
        let service = router.into_make_service_with_connect_info::<SocketAddr>();
        let served = axum::serve(listener, service)
            .with_graceful_shutdown(async move { shutdown.cancelled().await })
            .await;
        if let Err(error) = served {
            tracing::error!(%error, "server stopped with an error");
        }
    });

    tracing::info!(%local_addr, "coordinator listening");
    Ok(Coordinator { local_addr, state, server })
}

impl Coordinator {
    /// The address the server is actually listening on.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Stops accepting connections, closes client connections and waits for
    /// everything to finish.
    pub async fn stop(self) {
        self.state.shutdown.cancel();
        self.state.tasks.close();
        self.state.tasks.wait().await;
        let _ = self.server.await;
        tracing::info!("coordinator stopped");
    }
}

async fn expiry_sweep(state: Arc<AppState>) {
    let mut ticker = tokio::time::interval(state.config.expiry_sweep_interval);
    loop {
        tokio::select! {
            _ = state.shutdown.cancelled() => return,
            _ = ticker.tick() => {}
        }
        let cutoff = store::now_ms() - state.config.queued_expiry.as_millis() as i64;
        match state.store.call(move |conn| store::expire_queued_targets(conn, cutoff)).await {
            Ok(0) => {}
            Ok(expired) => tracing::info!(expired, "queued job targets expired"),
            Err(error) => tracing::error!(%error, "expiry sweep failed"),
        }
    }
}

/// Enrollment key management on a data directory. Works whether or not a
/// coordinator is running on it.
pub mod admin {
    use std::path::Path;

    use crate::store::{self, EnrollmentKey, Store};
    use crate::Error;

    /// Creates a key with a freshly generated secret.
    pub fn create_enrollment_key(data_dir: &Path, label: &str) -> Result<EnrollmentKey, Error> {
        Store::open(data_dir)?.call_blocking(|conn| store::create_enrollment_key(conn, label))
    }

    pub fn list_enrollment_keys(data_dir: &Path) -> Result<Vec<EnrollmentKey>, Error> {
        Store::open(data_dir)?.call_blocking(|conn| store::list_enrollment_keys(conn))
    }

    /// Returns false if there is no such active key. Devices already
    /// connected stay connected; the key is refused at their next handshake.
    pub fn revoke_enrollment_key(data_dir: &Path, id: &str) -> Result<bool, Error> {
        Store::open(data_dir)?.call_blocking(|conn| store::revoke_enrollment_key(conn, id))
    }
}
