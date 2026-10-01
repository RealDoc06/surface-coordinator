//! The coordinator embedded in the manager.
//!
//! Same code as the remote coordinator, with its own database. The web UI
//! and API listen on loopback only and are opened with a token generated for
//! this launch; clients connect on a fixed port on every interface.

use std::net::SocketAddr;
use std::path::Path;

use coordinator_core::{AuthMode, Capabilities, Config, Coordinator};

pub struct Embedded {
    coordinator: Coordinator,
    token: String,
}

/// Options that differ between the app and tests.
pub struct Options {
    /// Where clients connect.
    pub client_bind: SocketAddr,
    pub capabilities: Capabilities,
    /// Manager routes served under `/api/v1/manager`.
    pub extra_api_routes: Option<axum::Router>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            client_bind: SocketAddr::from(([0, 0, 0, 0], protocol::EMBEDDED_CLIENT_PORT)),
            capabilities: Capabilities::default(),
            extra_api_routes: None,
        }
    }
}

impl Embedded {
    pub async fn start(data_dir: &Path, options: Options) -> Result<Embedded, coordinator_core::Error> {
        let token = hex::encode(rand::random::<[u8; 32]>());
        let mut config = Config::new(options.client_bind, data_dir, AuthMode::Token(token.clone()));
        config.ui_bind = Some(SocketAddr::from(([127, 0, 0, 1], 0)));
        config.capabilities = options.capabilities;
        config.extra_api_routes = options.extra_api_routes;
        let coordinator = coordinator_core::start(config).await?;
        Ok(Embedded { coordinator, token })
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

    pub async fn stop(self) {
        self.coordinator.stop().await;
    }
}
