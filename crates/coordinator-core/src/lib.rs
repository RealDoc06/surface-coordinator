//! Surface Coordinator server logic.
//!
//! Start a server on an address with a data directory and an auth mode; stop
//! it. The `coordinator` binary is a thin wrapper around this, and the
//! manager embeds the same library.

mod api;
mod auth;
mod client_ws;
mod events;
mod state;
mod store;
mod ui;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use events::Event;
use state::AppState;
pub use store::{Account, EnrollmentKey};
use store::Store;

/// How the HTTP API authenticates callers.
#[derive(Debug, Clone)]
pub enum AuthMode {
    /// Username and password accounts stored in the database (remote
    /// coordinator).
    Accounts,
    /// One secret token, handed to the UI by whoever started the server
    /// (coordinator embedded in the manager). Also accepted as
    /// `Authorization: Bearer <token>`.
    Token(String),
}

/// What the embedding application adds to the coordinator, reported to the
/// UI so it can show the matching controls.
#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    /// Hotspot control is available under `/api/v1/manager`.
    pub hotspot: bool,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Address clients connect to. Unless `ui_bind` is set, the API and the
    /// web UI are served here too. Port 0 picks a free port; see
    /// [`Coordinator::local_addr`].
    pub bind: SocketAddr,
    /// Separate address for the API and web UI. When set, `bind` only
    /// accepts clients.
    pub ui_bind: Option<SocketAddr>,
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
    /// Serve the web UI from this directory instead of the copy built into
    /// the library.
    pub ui_dir: Option<PathBuf>,
    /// Mark the session cookie `Secure`. Set when the coordinator is only
    /// reachable over HTTPS.
    pub secure_cookies: bool,
    pub capabilities: Capabilities,
    /// Routes of the embedding application, served under `/api/v1/manager`
    /// behind the coordinator's authentication.
    pub extra_api_routes: Option<axum::Router>,
}

impl Config {
    pub fn new(bind: SocketAddr, data_dir: impl Into<PathBuf>, auth: AuthMode) -> Config {
        Config {
            bind,
            ui_bind: None,
            data_dir: data_dir.into(),
            auth,
            heartbeat_interval: Duration::from_secs(protocol::DEFAULT_HEARTBEAT_INTERVAL_SECS),
            queued_expiry: Duration::from_secs(24 * 60 * 60),
            expiry_sweep_interval: Duration::from_secs(60),
            ui_dir: None,
            secure_cookies: false,
            capabilities: Capabilities::default(),
            extra_api_routes: None,
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
    ui_addr: SocketAddr,
    state: Arc<AppState>,
    servers: Vec<JoinHandle<()>>,
}

fn serve(state: &Arc<AppState>, listener: TcpListener, router: axum::Router) -> JoinHandle<()> {
    let shutdown = state.shutdown.clone();
    tokio::spawn(async move {
        let service = router.into_make_service_with_connect_info::<SocketAddr>();
        let served = axum::serve(listener, service)
            .with_graceful_shutdown(async move { shutdown.cancelled().await })
            .await;
        if let Err(error) = served {
            tracing::error!(%error, "server stopped with an error");
        }
    })
}

/// Starts a coordinator. It runs until [`Coordinator::stop`] is called.
pub async fn start(config: Config) -> Result<Coordinator, Error> {
    if let AuthMode::Token(token) = &config.auth {
        if token.is_empty() {
            return Err(Error::InvalidConfig("the API token must not be empty".into()));
        }
    }
    if config.heartbeat_interval.is_zero() {
        return Err(Error::InvalidConfig("the heartbeat interval must not be zero".into()));
    }

    let data_dir = config.data_dir.clone();
    let store = tokio::task::spawn_blocking(move || Store::open(&data_dir))
        .await
        .map_err(|e| Error::Internal(e.to_string()))??;
    let token_mode = matches!(config.auth, AuthMode::Token(_));
    store
        .call(move |conn| {
            store::mark_all_devices_offline(conn)?;
            if token_mode {
                // The token changes with every launch; sessions opened with
                // an earlier one must not outlive it.
                store::delete_token_sessions(conn)?;
            }
            Ok(())
        })
        .await?;

    let listener = TcpListener::bind(config.bind).await?;
    let local_addr = listener.local_addr()?;
    let ui_listener = match config.ui_bind {
        Some(address) => Some(TcpListener::bind(address).await?),
        None => None,
    };
    let state = Arc::new(tokio::task::spawn_blocking(move || AppState::new(config, store))
        .await
        .map_err(|e| Error::Internal(e.to_string()))?);

    state.tasks.spawn(expiry_sweep(state.clone()));

    let clients = client_ws::router();
    let app = api::router(state.clone()).fallback(ui::serve);
    let mut servers = Vec::new();
    let ui_addr = match ui_listener {
        Some(ui_listener) => {
            let ui_addr = ui_listener.local_addr()?;
            servers.push(serve(&state, listener, clients.with_state(state.clone())));
            servers.push(serve(&state, ui_listener, app.with_state(state.clone())));
            ui_addr
        }
        None => {
            servers.push(serve(&state, listener, clients.merge(app).with_state(state.clone())));
            local_addr
        }
    };

    tracing::info!(clients = %local_addr, ui = %ui_addr, "coordinator listening");
    Ok(Coordinator { local_addr, ui_addr, state, servers })
}

impl Coordinator {
    /// The address clients connect to.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The address serving the API and the web UI (the same as
    /// [`local_addr`](Self::local_addr) unless `ui_bind` was set).
    pub fn ui_addr(&self) -> SocketAddr {
        self.ui_addr
    }

    /// Stops accepting connections, closes client connections and waits for
    /// everything to finish.
    pub async fn stop(self) {
        self.state.shutdown.cancel();
        self.state.tasks.close();
        self.state.tasks.wait().await;
        for server in self.servers {
            let _ = server.await;
        }
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
            Ok(expired) => {
                if !expired.is_empty() {
                    tracing::info!(expired = expired.len(), "queued job targets expired");
                }
                for change in expired {
                    state.publish(Event::JobTargetUpdated { job_id: change.job_id, target: change.target });
                }
            }
            Err(error) => tracing::error!(%error, "expiry sweep failed"),
        }
    }
}

/// Administration of a data directory: enrollment keys and accounts. Works
/// whether or not a coordinator is running on it.
pub mod admin {
    use std::path::Path;

    use crate::store::{self, Account, EnrollmentKey, Store};
    use crate::{auth, Error};

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

    #[derive(Debug, thiserror::Error)]
    pub enum AccountError {
        #[error("usernames are 1 to 64 characters: letters, digits, '.', '_' and '-'")]
        InvalidUsername,
        #[error("the password must have at least {} characters", auth::MIN_PASSWORD_LEN)]
        WeakPassword,
        #[error("that username is taken")]
        UsernameTaken,
        #[error("no such account")]
        NotFound,
        #[error(transparent)]
        Storage(#[from] Error),
    }

    pub fn create_account(data_dir: &Path, username: &str, password: &str) -> Result<Account, AccountError> {
        if !auth::valid_username(username) {
            return Err(AccountError::InvalidUsername);
        }
        if !auth::valid_password(password) {
            return Err(AccountError::WeakPassword);
        }
        let hash = auth::hash_password(password);
        Store::open(data_dir)?
            .call_blocking(|conn| store::create_account(conn, username, &hash))?
            .ok_or(AccountError::UsernameTaken)
    }

    pub fn list_accounts(data_dir: &Path) -> Result<Vec<Account>, Error> {
        Store::open(data_dir)?.call_blocking(|conn| store::list_accounts(conn))
    }

    /// Sets a new password and ends the account's sessions.
    pub fn set_account_password(data_dir: &Path, username: &str, password: &str) -> Result<(), AccountError> {
        if !auth::valid_password(password) {
            return Err(AccountError::WeakPassword);
        }
        let hash = auth::hash_password(password);
        let changed = Store::open(data_dir)?.call_blocking(|conn| {
            match store::find_account_by_username(conn, username)? {
                Some(account) => store::set_account_password(conn, &account.id, &hash),
                None => Ok(false),
            }
        })?;
        if changed { Ok(()) } else { Err(AccountError::NotFound) }
    }

    pub fn delete_account(data_dir: &Path, username: &str) -> Result<(), AccountError> {
        let deleted = Store::open(data_dir)?.call_blocking(|conn| {
            match store::find_account_by_username(conn, username)? {
                Some(account) => store::delete_account(conn, &account.id),
                None => Ok(false),
            }
        })?;
        if deleted { Ok(()) } else { Err(AccountError::NotFound) }
    }
}
