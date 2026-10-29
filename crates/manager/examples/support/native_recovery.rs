use serde::{Deserialize, Serialize};
use std::{
    os::windows::process::CommandExt,
    path::Path,
    process::{Command, Stdio},
};
use windows::{
    core::HSTRING,
    Networking::Connectivity::NetworkInformation,
    Networking::NetworkOperators::{
        NetworkOperatorTetheringAccessPointConfiguration, NetworkOperatorTetheringManager,
        TetheringOperationStatus, TetheringOperationalState, TetheringWiFiAuthenticationKind,
        TetheringWiFiBand,
    },
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Serialize, Deserialize)]
struct Saved {
    ssid: String,
    passphrase: String,
    band: Option<i32>,
    authentication: Option<i32>,
    idle_timeout: Option<bool>,
}

#[derive(Deserialize)]
struct Owned {
    ssid: String,
    passphrase: String,
}

pub fn save(directory: &Path, manager: &NetworkOperatorTetheringManager) -> Result<()> {
    let account = format!(
        "{}\\{}:(OI)(CI)F",
        std::env::var("USERDOMAIN")?,
        std::env::var("USERNAME")?
    );
    let permissions = Command::new("icacls.exe")
        .arg(directory)
        .args([
            "/inheritance:r",
            "/grant:r",
            &account,
            "*S-1-5-18:(OI)(CI)F",
        ])
        .creation_flags(0x08000000)
        .output()?;
    if !permissions.status.success() {
        return Err("Cannot protect recovery directory".into());
    }
    let original = manager.GetCurrentAccessPointConfiguration()?;
    let saved = Saved {
        ssid: original.Ssid()?.to_string_lossy(),
        passphrase: original.Passphrase()?.to_string_lossy(),
        band: original.Band().ok().map(|v| v.0),
        authentication: original.AuthenticationKind().ok().map(|v| v.0),
        idle_timeout: NetworkOperatorTetheringManager::IsNoConnectionsTimeoutEnabled().ok(),
    };
    std::fs::write(directory.join("original.json"), serde_json::to_vec(&saved)?)?;
    let watchdog_log = std::fs::File::create(directory.join("watchdog.log"))?;
    Command::new(std::env::current_exe()?)
        .arg("--watchdog")
        .arg(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(watchdog_log)
        .creation_flags(0x08000000)
        .spawn()?;
    Ok(())
}

/// Restore only our exact network; an outside replacement is left untouched.
pub fn restore(directory: &Path) -> Result<()> {
    let saved: Saved = serde_json::from_slice(&std::fs::read(directory.join("original.json"))?)?;
    let manager = NetworkOperatorTetheringManager::CreateFromConnectionProfile(
        &NetworkInformation::GetInternetConnectionProfile()?,
    )?;
    let current = manager.GetCurrentAccessPointConfiguration()?;
    let ssid = current.Ssid()?.to_string_lossy();
    let passphrase = current.Passphrase()?.to_string_lossy();
    if ssid != saved.ssid || passphrase != saved.passphrase {
        let owned: Owned = serde_json::from_slice(&std::fs::read(directory.join("owned.json"))?)?;
        if ssid != owned.ssid || passphrase != owned.passphrase {
            return Err(
                "Hotspot changed outside this fixture; preserving its configuration".into(),
            );
        }
        if manager.TetheringOperationalState()? != TetheringOperationalState::Off
            && manager.StopTetheringAsync()?.join()?.Status()? != TetheringOperationStatus::Success
        {
            return Err("Could not stop the fixture hotspot".into());
        }
        let configuration = NetworkOperatorTetheringAccessPointConfiguration::new()?;
        configuration.SetSsid(&HSTRING::from(&saved.ssid))?;
        configuration.SetPassphrase(&HSTRING::from(&saved.passphrase))?;
        if let Some(band) = saved.band {
            configuration.SetBand(TetheringWiFiBand(band))?;
        }
        if let Some(authentication) = saved.authentication {
            configuration.SetAuthenticationKind(TetheringWiFiAuthenticationKind(authentication))?;
        }
        manager.ConfigureAccessPointAsync(&configuration)?.join()?;
    }
    match saved.idle_timeout {
        Some(true) => NetworkOperatorTetheringManager::EnableNoConnectionsTimeout()?,
        Some(false) => NetworkOperatorTetheringManager::DisableNoConnectionsTimeout()?,
        None => {}
    }
    let actual = manager.GetCurrentAccessPointConfiguration()?;
    if manager.TetheringOperationalState()? != TetheringOperationalState::Off
        || actual.Ssid()?.to_string_lossy() != saved.ssid
        || actual.Passphrase()?.to_string_lossy() != saved.passphrase
        || saved
            .band
            .is_some_and(|v| actual.Band().map(|b| b.0 != v).unwrap_or(true))
        || saved.authentication.is_some_and(|v| {
            actual
                .AuthenticationKind()
                .map(|a| a.0 != v)
                .unwrap_or(true)
        })
        || saved.idle_timeout.is_some_and(|v| {
            NetworkOperatorTetheringManager::IsNoConnectionsTimeoutEnabled()
                .map(|a| a != v)
                .unwrap_or(true)
        })
    {
        return Err("Original hotspot configuration was not fully restored".into());
    }
    std::fs::write(
        directory.join("restored.json"),
        b"{\"hotspot_off\":true,\"original_configuration_restored\":true}",
    )?;
    Ok(())
}

pub async fn watchdog(directory: &Path) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(21 * 60);
    while std::time::Instant::now() < deadline {
        if directory.join("restored.json").exists() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    std::fs::write(directory.join("stop"), b"watchdog restore")?;
    restore(directory)
}

/// A single compatibility retry for the authorized hardware test. The same
/// owned SSID/password is retained and normal cleanup restores the saved band.
pub fn band_24(directory: &Path) -> Result<()> {
    let owned: Owned = serde_json::from_slice(&std::fs::read(directory.join("owned.json"))?)?;
    let manager = NetworkOperatorTetheringManager::CreateFromConnectionProfile(
        &NetworkInformation::GetInternetConnectionProfile()?,
    )?;
    let configuration = manager.GetCurrentAccessPointConfiguration()?;
    if configuration.Ssid()?.to_string_lossy() != owned.ssid
        || configuration.Passphrase()?.to_string_lossy() != owned.passphrase
    {
        return Err("Not the fixture hotspot; refusing a band change".into());
    }
    if !configuration.IsBandSupported(TetheringWiFiBand::TwoPointFourGigahertz)? {
        return Err("The hotspot does not support 2.4 GHz".into());
    }
    if manager.TetheringOperationalState()? != TetheringOperationalState::Off
        && manager.StopTetheringAsync()?.join()?.Status()? != TetheringOperationStatus::Success
    {
        return Err("Could not stop fixture hotspot for the band retry".into());
    }
    configuration.SetBand(TetheringWiFiBand::TwoPointFourGigahertz)?;
    manager.ConfigureAccessPointAsync(&configuration)?.join()?;
    if manager.StartTetheringAsync()?.join()?.Status()? != TetheringOperationStatus::Success {
        return Err("Could not start fixture hotspot on 2.4 GHz".into());
    }
    println!("Fixture hotspot running on 2.4 GHz");
    Ok(())
}
