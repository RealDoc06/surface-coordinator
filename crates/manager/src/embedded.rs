//! The coordinator embedded in the manager.
//!
//! Same code as the remote coordinator, with its own database. The web UI
//! and API listen on loopback only and are opened with a token generated for
//! this launch; clients connect on a fixed port on every interface. The
//! hotspot is controlled through it and stops with it.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use coordinator_core::{AuthMode, Capabilities, Config, Coordinator};

use crate::hotspot::{self, Control, HotspotControl, HotspotService};
use crate::wifi::{self, WifiService, WifiSource};

/// Devices are declared offline after three of these.
const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

pub struct Embedded {
    coordinator: Coordinator,
    hotspot: Arc<HotspotService>,
    wifi: Arc<WifiService>,
    token: String,
}

/// Options that differ between the app and tests.
pub struct Options {
    /// Where clients connect.
    pub client_bind: SocketAddr,
    /// Turns this computer's hotspot on and off.
    pub hotspot: Arc<dyn HotspotControl>,
    /// Reads the Wi-Fi network this computer is on.
    pub wifi: Arc<dyn WifiSource>,
    /// Origin of the online coordinator the operator uses, e.g.
    /// `https://coordinator.example.org`. Devices moved to Wi-Fi go there.
    pub online_coordinator: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            client_bind: SocketAddr::from(([0, 0, 0, 0], protocol::EMBEDDED_CLIENT_PORT)),
            hotspot: hotspot::native(),
            wifi: wifi::native(),
            online_coordinator: None,
        }
    }
}

impl Embedded {
    pub async fn start(data_dir: &Path, options: Options) -> Result<Embedded, coordinator_core::Error> {
        let token = hex::encode(rand::random::<[u8; 32]>());
        let control = options.hotspot.control();
        let hotspot = Arc::new(HotspotService::new(options.hotspot, data_dir, options.client_bind.port()));

        let mut config = Config::new(options.client_bind, data_dir, AuthMode::Token(token.clone()));
        config.ui_bind = Some(SocketAddr::from(([127, 0, 0, 1], 0)));
        // An operator next to the devices watches them come and go: with the
        // default 30 s a device that left would still show as online for a
        // minute and a half.
        config.heartbeat_interval = HEARTBEAT_INTERVAL;
        // Client builds reach a coordinator by upload; the repository has no
        // releases to look for, and on a hotspot there is no internet anyway.
        config.updates.enabled = false;
        config.capabilities = Capabilities { hotspot: control != Control::Unsupported };
        let wifi = Arc::new(WifiService::new(options.wifi));
        wifi.set_online_coordinator(options.online_coordinator.as_deref());
        config.extra_api_routes = Some(hotspot::router(hotspot.clone()).merge(wifi::router(wifi.clone())));
        let coordinator = coordinator_core::start(config).await?;
        let client_port = coordinator.local_addr().port();
        hotspot.set_client_port(client_port);
        wifi.connect(format!("http://{}", coordinator.ui_addr()), token.clone(), client_port);
        Ok(Embedded { coordinator, hotspot, wifi, token })
    }

    /// The token for this launch, for the manager's own calls to the API
    /// (`Authorization: Bearer`).
    pub fn api_token(&self) -> &str {
        &self.token
    }

    /// The operator chose another online coordinator (or none) while this
    /// one runs.
    pub fn set_online_coordinator(&self, origin: Option<&str>) {
        self.wifi.set_online_coordinator(origin);
    }

    /// Origin of the web UI, e.g. `http://127.0.0.1:51234`.
    pub fn origin(&self) -> String {
        format!("http://{}", self.coordinator.ui_addr())
    }

    /// The address to open: the UI signs itself in with the token in the
    /// fragment, which is never sent to a server or written to a log.
    pub fn entry_url(&self) -> String {
        format!("{}/#token={}", self.origin(), self.token)
    }

    /// Where clients connect.
    pub fn client_addr(&self) -> SocketAddr {
        self.coordinator.local_addr()
    }

    /// Stops the hotspot, then the coordinator: devices on the hotspot can
    /// reach nothing else, so neither runs without the other.
    pub async fn stop(self) {
        self.hotspot.shutdown().await;
        self.coordinator.stop().await;
    }
}
