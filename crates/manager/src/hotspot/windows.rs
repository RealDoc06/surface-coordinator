//! Windows: Mobile Hotspot through the WinRT tethering API.
//!
//! Starting the hotspot replaces the network name and password configured
//! in Settings → Network → Mobile hotspot; Windows keeps only one. The WinRT
//! calls block, so they run on the blocking thread pool.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ::windows::core::HSTRING;
use ::windows::Networking::Connectivity::NetworkInformation;
use ::windows::Networking::NetworkOperators::{
    NetworkOperatorTetheringAccessPointConfiguration, NetworkOperatorTetheringManager,
    NetworkOperatorTetheringOperationResult, TetheringOperationStatus, TetheringOperationalState,
    TetheringWiFiAuthenticationKind,
};

use super::{BoxFuture, Control, HotspotControl};

#[derive(Default)]
pub struct MobileHotspot {
    /// Windows turns an idle Mobile Hotspot off after a few minutes, but
    /// devices may only come looking for it much later. The manager switches
    /// that timeout off while its hotspot runs and back on when it stops,
    /// if it was on before.
    restore_timeout: Arc<AtomicBool>,
}

impl HotspotControl for MobileHotspot {
    fn control(&self) -> Control {
        Control::Automatic
    }

    fn start(&self, ssid: &str, passphrase: &str) -> BoxFuture<Result<(), String>> {
        let (ssid, passphrase) = (ssid.to_owned(), passphrase.to_owned());
        let restore_timeout = self.restore_timeout.clone();
        Box::pin(blocking(move || start(&ssid, &passphrase, &restore_timeout)))
    }

    fn stop(&self) -> BoxFuture<Result<(), String>> {
        let restore_timeout = self.restore_timeout.clone();
        Box::pin(blocking(move || stop(&restore_timeout)))
    }

    fn instructions(&self, _ssid: &str, _passphrase: &str) -> Vec<String> {
        Vec::new()
    }
}

async fn blocking(work: impl FnOnce() -> Result<(), String> + Send + 'static) -> Result<(), String> {
    tokio::task::spawn_blocking(work)
        .await
        .unwrap_or_else(|error| Err(format!("the hotspot task failed: {error}")))
}

/// Whether Mobile Hotspot is on. Only reads.
pub fn operational_state() -> Result<TetheringOperationalState, String> {
    winrt("read the Mobile Hotspot state", tethering_manager()?.TetheringOperationalState())
}

fn start(ssid: &str, passphrase: &str, restore_timeout: &AtomicBool) -> Result<(), String> {
    let manager = tethering_manager()?;
    let state = winrt("read the Mobile Hotspot state", manager.TetheringOperationalState())?;
    if state == TetheringOperationalState::On {
        if let Ok(current) = manager.GetCurrentAccessPointConfiguration() {
            let same = |value: ::windows::core::Result<HSTRING>, wanted: &str| {
                value.map(|value| value.to_string_lossy() == wanted).unwrap_or(false)
            };
            if same(current.Ssid(), ssid) && same(current.Passphrase(), passphrase) {
                // Already ours: restarting would drop the connected devices.
                return Ok(());
            }
        }
        // Switching to another network: Windows applies a configuration at
        // the next start.
        let result = winrt("stop Mobile Hotspot", manager.StopTetheringAsync().and_then(|op| op.join()))?;
        check("stop Mobile Hotspot", result)?;
    }

    let configuration = winrt(
        "create a Mobile Hotspot configuration",
        NetworkOperatorTetheringAccessPointConfiguration::new(),
    )?;
    winrt("set the Mobile Hotspot name", configuration.SetSsid(&HSTRING::from(ssid)))?;
    winrt("set the Mobile Hotspot password", configuration.SetPassphrase(&HSTRING::from(passphrase)))?;
    // Clients store the hotspot as a WPA2 network. Windows versions before
    // WPA3 support have no such setting and always use WPA2.
    if configuration.IsAuthenticationKindSupported(TetheringWiFiAuthenticationKind::Wpa2).unwrap_or(false) {
        if let Err(error) = configuration.SetAuthenticationKind(TetheringWiFiAuthenticationKind::Wpa2) {
            tracing::debug!(%error, "could not select WPA2 for Mobile Hotspot");
        }
    }
    winrt(
        "configure Mobile Hotspot",
        manager.ConfigureAccessPointAsync(&configuration).and_then(|action| action.join()),
    )?;

    disable_idle_timeout(restore_timeout);
    let result = winrt("start Mobile Hotspot", manager.StartTetheringAsync().and_then(|op| op.join()))?;
    check("start Mobile Hotspot", result)
}

fn stop(restore_timeout: &AtomicBool) -> Result<(), String> {
    let manager = tethering_manager()?;
    let state = winrt("read the Mobile Hotspot state", manager.TetheringOperationalState())?;
    if state != TetheringOperationalState::Off {
        let result = winrt("stop Mobile Hotspot", manager.StopTetheringAsync().and_then(|op| op.join()))?;
        check("stop Mobile Hotspot", result)?;
    }
    if restore_timeout.swap(false, Ordering::SeqCst) {
        if let Err(error) = NetworkOperatorTetheringManager::EnableNoConnectionsTimeout() {
            tracing::warn!(%error, "could not switch the Mobile Hotspot idle timeout back on");
        }
    }
    Ok(())
}

/// Best effort: without it the hotspot still works until it has been idle
/// for a while.
fn disable_idle_timeout(restore_timeout: &AtomicBool) {
    match NetworkOperatorTetheringManager::IsNoConnectionsTimeoutEnabled() {
        Ok(true) => match NetworkOperatorTetheringManager::DisableNoConnectionsTimeout() {
            Ok(()) => restore_timeout.store(true, Ordering::SeqCst),
            Err(error) => tracing::warn!(%error, "could not switch the Mobile Hotspot idle timeout off"),
        },
        Ok(false) => {}
        Err(error) => tracing::debug!(%error, "this Windows cannot read the Mobile Hotspot idle timeout"),
    }
}

/// Windows ties Mobile Hotspot to a connection whose traffic it would share.
/// Without internet any connection profile does; one that is not connected
/// still lets the hotspot run, with nothing to share.
fn tethering_manager() -> Result<NetworkOperatorTetheringManager, String> {
    let mut profiles = Vec::new();
    if let Ok(profile) = NetworkInformation::GetInternetConnectionProfile() {
        profiles.push(profile);
    }
    if let Ok(all) = NetworkInformation::GetConnectionProfiles() {
        profiles.extend(all);
    }
    let mut last_error = None;
    for profile in profiles {
        match NetworkOperatorTetheringManager::CreateFromConnectionProfile(&profile) {
            Ok(manager) => return Ok(manager),
            Err(error) => last_error = Some(error),
        }
    }
    Err(match last_error {
        None => "Windows can only start Mobile Hotspot when the computer has a network connection profile \
                 (Wi-Fi, Ethernet or cellular). Connect to any network once, or plug in a network cable, \
                 and try again."
            .to_owned(),
        Some(error) => format!("Windows cannot use Mobile Hotspot with any network connection: {error}"),
    })
}

fn winrt<T>(action: &str, result: ::windows::core::Result<T>) -> Result<T, String> {
    result.map_err(|error| format!("Windows could not {action}: {error}"))
}

fn check(action: &str, result: NetworkOperatorTetheringOperationResult) -> Result<(), String> {
    let status = winrt(action, result.Status())?;
    if status == TetheringOperationStatus::Success {
        return Ok(());
    }
    let mut message = format!("Windows could not {action}: {}", reason(status));
    if let Ok(details) = result.AdditionalErrorMessage() {
        let details = details.to_string_lossy();
        if !details.trim().is_empty() {
            message = format!("{message} ({})", details.trim());
        }
    }
    Err(message)
}

fn reason(status: TetheringOperationStatus) -> String {
    let text = match status {
        TetheringOperationStatus::WiFiDeviceOff => "Wi-Fi is turned off",
        TetheringOperationStatus::RadioRestriction => "a radio restriction such as airplane mode is on",
        TetheringOperationStatus::BandInterference => "the Wi-Fi adapter is busy on another band",
        TetheringOperationStatus::OperationInProgress => {
            "Mobile Hotspot is already being turned on or off; try again in a moment"
        }
        TetheringOperationStatus::AlreadyOn => "Mobile Hotspot is already on",
        TetheringOperationStatus::NetworkLimitedConnectivity => "the shared connection has limited connectivity",
        TetheringOperationStatus::MobileBroadbandDeviceOff => "the mobile broadband device is off",
        TetheringOperationStatus::BluetoothDeviceOff => "Bluetooth is turned off",
        TetheringOperationStatus::EntitlementCheckTimeout | TetheringOperationStatus::EntitlementCheckFailure => {
            "the mobile operator does not allow tethering"
        }
        _ => return format!("unknown reason (status {})", status.0),
    };
    text.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads the state of this computer's Mobile Hotspot and changes
    /// nothing. Starting it for real is on the manual test checklist.
    #[test]
    fn the_hotspot_state_can_be_read() {
        match operational_state() {
            Ok(state) => assert!(
                [
                    TetheringOperationalState::Unknown,
                    TetheringOperationalState::On,
                    TetheringOperationalState::Off,
                    TetheringOperationalState::InTransition,
                ]
                .contains(&state),
                "{state:?}"
            ),
            // A computer without any connection profile.
            Err(message) => assert!(message.contains("Mobile Hotspot"), "{message}"),
        }
    }
}
