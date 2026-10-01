//! The hotspot the manager opens when there is no network.
//!
//! Devices enrolled with a key look for the hotspot of that key (its SSID
//! and passphrase follow from the key, see [`protocol::hotspot`]) and
//! connect to the embedded coordinator through it. How the hotspot is turned
//! on depends on the manager's OS; [`native`] picks the implementation.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::Serialize;

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod linux;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod macos;
pub mod routes;
#[cfg(windows)]
pub mod windows;

pub use routes::{router, HotspotService};

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Who turns the hotspot on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Control {
    /// The manager does, through the OS.
    Automatic,
    /// The operator does, following [`HotspotControl::instructions`]; there
    /// is no API for it on this OS.
    Manual,
    /// No hotspot on this OS.
    Unsupported,
}

/// Turns this computer's hotspot on and off.
pub trait HotspotControl: Send + Sync + 'static {
    fn control(&self) -> Control;

    /// Turns the hotspot on with this SSID and WPA2 passphrase. If it is
    /// already on with other settings it is switched to these.
    fn start(&self, ssid: &str, passphrase: &str) -> BoxFuture<Result<(), String>>;

    /// Turns the hotspot off.
    fn stop(&self) -> BoxFuture<Result<(), String>>;

    /// Steps for the operator when control is [`Control::Manual`]; empty
    /// otherwise.
    fn instructions(&self, ssid: &str, passphrase: &str) -> Vec<String>;
}

/// The implementation for the OS the manager runs on.
pub fn native() -> Arc<dyn HotspotControl> {
    #[cfg(windows)]
    let control: Arc<dyn HotspotControl> = Arc::new(self::windows::MobileHotspot::default());
    #[cfg(target_os = "linux")]
    let control: Arc<dyn HotspotControl> = Arc::new(linux::NetworkManager);
    #[cfg(target_os = "macos")]
    let control: Arc<dyn HotspotControl> = Arc::new(macos::InternetSharing);
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    let control: Arc<dyn HotspotControl> = Arc::new(Unsupported);
    control
}

/// No hotspot: the OS has none the manager knows how to use.
pub struct Unsupported;

impl HotspotControl for Unsupported {
    fn control(&self) -> Control {
        Control::Unsupported
    }

    fn start(&self, _ssid: &str, _passphrase: &str) -> BoxFuture<Result<(), String>> {
        Box::pin(async { Err("the manager cannot open a hotspot on this operating system".to_owned()) })
    }

    fn stop(&self) -> BoxFuture<Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn instructions(&self, _ssid: &str, _passphrase: &str) -> Vec<String> {
        Vec::new()
    }
}
