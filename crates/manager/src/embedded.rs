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

pub struct Embedded {
    coordinator: Coordinator,
    hotspot: Arc<HotspotService>,
    token: String,
}

/// Options that differ between the app and tests.
pub struct Options {
    /// Where clients connect.
    pub client_bind: SocketAddr,
    /// Turns this computer's hotspot on and off.
    pub hotspot: Arc<dyn HotspotControl>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            client_bind: SocketAddr::from(([0, 0, 0, 0], protocol::EMBEDDED_CLIENT_PORT)),
            hotspot: hotspot::native(),
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
        config.capabilities = Capabilities { hotspot: control != Control::Unsupported };
        config.extra_api_routes = Some(hotspot::router(hotspot.clone()));
        let coordinator = coordinator_core::start(config).await?;
        hotspot.set_client_port(coordinator.local_addr().port());
        Ok(Embedded { coordinator, hotspot, token })
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
