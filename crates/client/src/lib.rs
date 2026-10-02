//! Surface Coordinator device agent.
//!
//! The core here is platform independent: it keeps one outbound WebSocket to
//! the coordinator, reconnects with backoff, falls back to the manager's
//! hotspot when no coordinator can be reached, sends heartbeats and runs
//! jobs. Anything that touches the operating system goes through
//! [`platform::Platform`].

mod backoff;
pub mod config;
mod connection;
#[cfg(windows)]
pub mod install;
mod jobs;
mod ledger;
pub mod logfile;
mod network;
pub mod platform;
#[cfg(windows)]
pub mod service;
pub mod update;

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use protocol::NetworkInfo;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub use backoff::BackoffConfig;
use backoff::Backoff;
use connection::{Attempt, Ended};
use jobs::{Job, JobManager, UpdateContext, WorkerContext};
use network::Network;
use platform::{Platform, PlatformError, SystemInfo, WifiConnection, WifiJoinRequest};

/// Exit code of a client stopping to be restarted, after `client.update`
/// replaced its executable or put the previous one back. Not zero, so that
/// the service manager's restart-on-failure starts it again.
pub const RESTART_EXIT_CODE: i32 = 3;

/// The public key client updates are verified against, from
/// `release-keys/client-update.pub` at build time. Empty if the build had
/// none: such a client refuses every update.
pub const BUILT_IN_UPDATE_KEY: &str = env!("SC_CLIENT_UPDATE_PUBKEY");

#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Base URL of the coordinator, `ws://host:port` or `wss://host`.
    /// Without one the client looks for the manager's hotspot straight away.
    /// A coordinator URL given by a `wifi.join` job takes precedence.
    pub coordinator_url: Option<String>,
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
    /// How long without a coordinator before the client starts searching
    /// for the hotspot.
    pub search_after: Duration,
    /// While searching: time between rounds of trying the coordinator and
    /// scanning.
    pub search_interval: Duration,
    /// How long a `wifi.join` waits for a coordinator on the new network
    /// before going back.
    pub handoff_timeout: Duration,
    /// The same while the device is on no Wi-Fi network at all. Nothing is
    /// lost by looking often then, so the hotspot is found soon after it
    /// comes up.
    pub offline_search_interval: Duration,
    /// While searching on a device that may not see Wi-Fi networks and is on
    /// another network: time between attempts to join the hotspot without
    /// knowing it is there. Each attempt takes the device off that network
    /// for a moment, hence much rarer than a scan. A device on no network
    /// has nothing to lose and tries every round.
    pub blind_join_interval: Duration,
    /// Port of the coordinator at the hotspot's gateway.
    pub hotspot_port: u16,
    /// How long a `power` job waits for the coordinator to acknowledge its
    /// result before restarting, shutting down or suspending the device.
    pub power_ack_timeout: Duration,
    /// Key that `client.update` signatures must verify against; normally
    /// [`BUILT_IN_UPDATE_KEY`]. Empty refuses every update.
    pub update_public_key: String,
    /// Longest silence while downloading an update before giving up.
    pub download_timeout: Duration,
}

impl ClientConfig {
    pub fn new(
        coordinator_url: Option<String>,
        key_id: impl Into<String>,
        key_secret: impl Into<String>,
        device_id: impl Into<String>,
    ) -> ClientConfig {
        ClientConfig {
            coordinator_url,
            key_id: key_id.into(),
            key_secret: key_secret.into(),
            device_id: device_id.into(),
            client_version: env!("CARGO_PKG_VERSION").to_owned(),
            state_dir: None,
            backoff: BackoffConfig::default(),
            handshake_timeout: Duration::from_secs(10),
            search_after: Duration::from_secs(60),
            search_interval: Duration::from_secs(30),
            handoff_timeout: Duration::from_secs(60),
            offline_search_interval: Duration::from_secs(10),
            blind_join_interval: Duration::from_secs(5 * 60),
            hotspot_port: protocol::EMBEDDED_CLIENT_PORT,
            power_ack_timeout: Duration::from_secs(5),
            update_public_key: BUILT_IN_UPDATE_KEY.to_owned(),
            download_timeout: Duration::from_secs(30),
        }
    }

    /// The configuration of a client keeping its state in `state_dir`.
    pub fn from_settings(settings: config::Settings, state_dir: &Path) -> ClientConfig {
        let config::Settings { coordinator_url, key_id, key_secret, device_id } = settings;
        let mut config = ClientConfig::new(coordinator_url, key_id, key_secret, device_id);
        config.state_dir = Some(state_dir.to_owned());
        config
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
    network: Arc<Network>,
}

impl<P: Platform> Client<P> {
    pub fn new(config: ClientConfig, platform: Arc<P>) -> io::Result<Client<P>> {
        let state_dir = config.state_dir.as_deref();
        let (jobs, queue) = JobManager::new(state_dir)?;
        let network = Arc::new(Network::load(config.coordinator_url.clone(), state_dir)?);
        Ok(Client { config, platform, jobs, queue, network })
    }

    /// Runs until `shutdown` is cancelled, reconnecting whenever the
    /// connection is lost and falling back to the hotspot when no
    /// coordinator can be reached. Also returns, without connecting, when
    /// an update is rolled back and the platform has been asked to restart
    /// the client.
    pub async fn run(self, shutdown: CancellationToken) {
        let Client { config, platform, jobs, queue, network } = self;
        let state_dir = config.state_dir.as_deref();
        let completion =
            match update::on_start(platform.as_ref(), &jobs, &config.client_version, state_dir).await {
                update::Startup::Restart => return,
                update::Startup::Continue(completion) => completion,
            };
        // An update in progress completes with the first handshake.
        let completion = completion.map(|completion| {
            let (platform, jobs, network) = (platform.clone(), jobs.clone(), network.clone());
            tokio::spawn(async move {
                network.wait_for_connection_after(0).await;
                completion.complete(platform.as_ref(), &jobs).await;
            })
        });

        let system = platform.system_info();
        // Jobs keep running while the connection is down.
        let context = WorkerContext {
            network: network.clone(),
            handoff_timeout: config.handoff_timeout,
            power_ack_timeout: config.power_ack_timeout,
            update: UpdateContext {
                client_version: config.client_version.clone(),
                public_key: config.update_public_key.clone(),
                state_dir: config.state_dir.clone(),
                download_timeout: config.download_timeout,
            },
        };
        let worker = tokio::spawn(jobs::run_worker(jobs.clone(), platform.clone(), context, queue));

        match network.coordinator_url() {
            Some(url) => tracing::info!(%url, "connecting to the coordinator"),
            None => tracing::info!("no coordinator configured; looking for the hotspot"),
        }
        let mut supervisor = Supervisor {
            hotspot_ssid: protocol::hotspot::ssid(&config.key_id),
            backoff: Backoff::new(config.backoff.clone()),
            config: &config,
            platform: platform.as_ref(),
            system,
            jobs: &jobs,
            network: &network,
            shutdown: &shutdown,
            disconnected_since: Instant::now(),
            search_requested: false,
            searching: false,
            wifi_unsupported_noted: false,
            wifi_hidden_noted: false,
            last_blind_join: None,
            on_wifi: false,
        };
        supervisor.run().await;
        // A job still running is cut short here; it is recorded as started,
        // so the next run reports it as interrupted.
        worker.abort();
        if let Some(completion) = completion {
            completion.abort();
        }
    }
}

/// How one round of the network state machine ended.
enum Round {
    Shutdown,
    /// A session was established and has ended.
    HadSession,
    /// No coordinator could be reached.
    NotConnected,
}

/// The network state machine.
///
/// Each round tries, in this order and only until a session comes about:
/// the coordinator (stored override, else configured); the coordinator at
/// the gateway if the device is on the hotspot; and, while searching, the
/// hotspot after scanning for and joining it. Nothing is scanned or joined
/// while a session is up, so a working connection is never given up for the
/// hotspot.
///
/// Where the operating system withholds Wi-Fi names (Windows 11 with
/// Location services off) the client cannot look: it asks whatever gateway
/// it has, and while searching joins the hotspot without knowing whether it
/// is in range. The handshake tells a coordinator from anything else.
struct Supervisor<'a, P: Platform> {
    config: &'a ClientConfig,
    platform: &'a P,
    system: SystemInfo,
    jobs: &'a JobManager,
    network: &'a Network,
    shutdown: &'a CancellationToken,
    hotspot_ssid: String,
    backoff: Backoff,
    /// End of the last session, or the start of the client.
    disconnected_since: Instant,
    /// A failed handoff asked for a search; holds until the next session.
    search_requested: bool,
    /// Whether the last round was searching, to log changes only.
    searching: bool,
    /// Wi-Fi being unsupported is said once, not every round.
    wifi_unsupported_noted: bool,
    /// So is Wi-Fi names being withheld.
    wifi_hidden_noted: bool,
    /// When the hotspot was last joined without seeing it.
    last_blind_join: Option<Instant>,
    /// Whether the device was on a Wi-Fi network at the start of the round.
    on_wifi: bool,
}

impl<P: Platform> Supervisor<'_, P> {
    async fn run(&mut self) {
        loop {
            if self.network.start_round() {
                self.search_requested = true;
            }
            let (round, searching) = self.round().await;
            let delay = match round {
                Round::Shutdown => return,
                Round::NotConnected if searching && !self.on_wifi => {
                    self.config.offline_search_interval.min(self.config.search_interval)
                }
                Round::NotConnected if searching => self.config.search_interval,
                Round::HadSession | Round::NotConnected => self.backoff.next_delay(),
            };
            tracing::debug!(retry_in = ?delay, "waiting before the next attempt");
            tokio::select! {
                _ = self.shutdown.cancelled() => return,
                _ = tokio::time::sleep(delay) => {}
                // A handoff wants a new connection, or a search, now.
                _ = self.network.interrupted() => {}
            }
        }
    }

    /// One round; also returns whether it was searching.
    async fn round(&mut self) -> (Round, bool) {
        // Asked once per round: for `hello`, and to see whether the device
        // is on the hotspot.
        let current = self.wifi_current().await;
        self.on_wifi = current.is_some();

        if let Some(url) = self.network.coordinator_url() {
            if let Some(round) = self.attempt(&url, network_info(current.as_ref())).await {
                return (round, false);
            }
        }

        let searching = self.searching();
        if searching != self.searching {
            self.searching = searching;
            if searching {
                tracing::info!(ssid = %self.hotspot_ssid, "no coordinator reachable; searching for the hotspot");
            }
        }

        match current {
            Some(on_hotspot) if on_hotspot.ssid.as_deref() == Some(self.hotspot_ssid.as_str()) => {
                // Already there (joined earlier, or by hand): no need to scan.
                let round = self.try_gateway(&on_hotspot).await;
                return (round, searching);
            }
            Some(unnamed) if unnamed.ssid.is_none() => {
                // This may be the hotspot; asking its gateway costs one
                // connection attempt.
                let round = self.try_gateway(&unnamed).await;
                if !matches!(round, Round::NotConnected) {
                    return (round, searching);
                }
            }
            _ => {}
        }
        if !searching {
            return (Round::NotConnected, false);
        }
        let round = match self.join_hotspot().await {
            Ok(Some(on_hotspot)) => self.try_gateway(&on_hotspot).await,
            Ok(None) => Round::NotConnected,
            Err(round) => round,
        };
        (round, true)
    }

    fn searching(&self) -> bool {
        // A handoff in progress decides itself when to give up on the new
        // network; searching meanwhile would pull the device back early.
        if self.network.handoff_in_progress() {
            return false;
        }
        self.network.coordinator_url().is_none()
            || self.search_requested
            || self.disconnected_since.elapsed() >= self.config.search_after
    }

    /// Connects to `url` and serves the session if one comes about. `None`
    /// means the coordinator could not be reached.
    async fn attempt(&mut self, url: &str, network_info: Option<NetworkInfo>) -> Option<Round> {
        let attempt = Attempt {
            url,
            network_info,
            system: &self.system,
            jobs: self.jobs,
            network: self.network,
        };
        match connection::run(self.config, attempt, self.shutdown).await {
            Ended::Shutdown => Some(Round::Shutdown),
            Ended::Lost { reason, connected_for: Some(lasted) } => {
                tracing::warn!(%url, %reason, "connection to coordinator lost");
                self.disconnected_since = Instant::now();
                self.search_requested = false;
                self.searching = false;
                self.last_blind_join = None;
                if lasted >= self.config.backoff.stable_after {
                    self.backoff.reset();
                }
                Some(Round::HadSession)
            }
            Ended::Lost { reason, connected_for: None } => {
                tracing::warn!(%url, %reason, "cannot reach coordinator");
                None
            }
        }
    }

    async fn try_gateway(&mut self, on_hotspot: &WifiConnection) -> Round {
        let Some(gateway) = on_hotspot.gateway else {
            tracing::debug!("on the hotspot but without a gateway yet");
            return Round::NotConnected;
        };
        // `SocketAddr` puts brackets around an IPv6 address.
        let url = format!("ws://{}", SocketAddr::new(gateway, self.config.hotspot_port));
        self.attempt(&url, network_info(Some(on_hotspot))).await.unwrap_or(Round::NotConnected)
    }

    /// Scans, and joins the hotspot if it is in range. Returns the
    /// connection once on it; `Err` only on shutdown.
    async fn join_hotspot(&mut self) -> Result<Option<WifiConnection>, Round> {
        let network = self.network;
        let _wifi = network.wifi_lock().await;
        match self.unless_shutdown(self.platform.wifi_scan()).await? {
            Ok(visible) if visible.contains(&self.hotspot_ssid) => {
                tracing::info!(ssid = %self.hotspot_ssid, "hotspot in range; joining it");
            }
            Ok(_) => {
                tracing::debug!(ssid = %self.hotspot_ssid, "hotspot not in range");
                return Ok(None);
            }
            Err(PlatformError::AccessDenied(message)) => {
                if !self.wifi_hidden_noted {
                    self.wifi_hidden_noted = true;
                    tracing::warn!(%message, "cannot see Wi-Fi networks; joining the hotspot without looking for it");
                }
                let waited = self.last_blind_join.map(|at| at.elapsed());
                // On another network an attempt costs that connection.
                if self.on_wifi && waited.is_some_and(|waited| waited < self.config.blind_join_interval) {
                    return Ok(None);
                }
                self.last_blind_join = Some(Instant::now());
                tracing::info!(ssid = %self.hotspot_ssid, "trying to join the hotspot");
            }
            Err(error) => {
                self.note_wifi_error("scanning for the hotspot", error);
                return Ok(None);
            }
        }
        let request = WifiJoinRequest {
            ssid: self.hotspot_ssid.clone(),
            passphrase: Some(protocol::hotspot::passphrase(&self.config.key_secret)),
            // Windows must not prefer the hotspot later on its own.
            auto_connect: false,
        };
        if let Err(error) = self.unless_shutdown(self.platform.wifi_join(request)).await? {
            self.note_wifi_error("joining the hotspot", error);
            return Ok(None);
        }
        let current = self.wifi_current().await;
        // An unnamed connection right after a successful join is the hotspot
        // as far as anyone can tell; the gateway's handshake decides.
        Ok(current.filter(|c| c.ssid.as_ref().is_none_or(|ssid| *ssid == self.hotspot_ssid)))
    }

    /// Platform calls can take seconds; shutdown should not wait for them.
    async fn unless_shutdown<T>(&self, operation: impl Future<Output = T>) -> Result<T, Round> {
        tokio::select! {
            _ = self.shutdown.cancelled() => Err(Round::Shutdown),
            output = operation => Ok(output),
        }
    }

    async fn wifi_current(&mut self) -> Option<WifiConnection> {
        match self.platform.wifi_current().await {
            Ok(current) => current,
            Err(error) => {
                self.note_wifi_error("querying the Wi-Fi connection", error);
                None
            }
        }
    }

    fn note_wifi_error(&mut self, what: &str, error: PlatformError) {
        match error {
            PlatformError::Unsupported(message) => {
                if !self.wifi_unsupported_noted {
                    self.wifi_unsupported_noted = true;
                    tracing::info!(%message, "Wi-Fi is not available; the client cannot fall back to the hotspot");
                }
            }
            PlatformError::AccessDenied(message) => {
                if !self.wifi_hidden_noted {
                    self.wifi_hidden_noted = true;
                    tracing::warn!(%message, "{what} was refused");
                }
            }
            PlatformError::Failed(message) | PlatformError::NoUserSession(message) => {
                tracing::warn!(%message, "{what} failed")
            }
        }
    }
}

/// What `hello` says about the network.
fn network_info(current: Option<&WifiConnection>) -> Option<NetworkInfo> {
    current.map(|connection| NetworkInfo {
        ssid: connection.ssid.clone(),
        ip: connection.ip.map(|ip| ip.to_string()),
    })
}
