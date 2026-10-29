//! Opt-in hardware fixture. Uses a fresh directory and stops after 20 minutes
//! or when a `stop` file is created. It refuses to replace an active hotspot.
//! Run `cargo run -p manager --example native-validation -- <fresh-directory>`.

#[cfg(windows)]
#[path = "support/native_recovery.rs"]
mod recovery;

#[cfg(windows)]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use manager::embedded::{Embedded, Options};
    use windows::Networking::Connectivity::NetworkInformation;
    use windows::Networking::NetworkOperators::{
        NetworkOperatorTetheringManager, TetheringOperationalState,
    };

    let argument = std::env::args_os()
        .nth(1)
        .ok_or("provide a fresh state directory")?;
    if argument == "--restore" || argument == "--watchdog" || argument == "--band-24" {
        let directory = PathBuf::from(
            std::env::args_os()
                .nth(2)
                .ok_or("provide the fixture directory")?,
        );
        return if argument == "--watchdog" {
            recovery::watchdog(&directory).await
        } else if argument == "--band-24" {
            recovery::band_24(&directory)
        } else {
            recovery::restore(&directory)
        };
    }
    let directory = PathBuf::from(argument);
    if directory.exists() {
        return Err("the fixture requires a new directory".into());
    }
    let profile = NetworkInformation::GetInternetConnectionProfile()?;
    let tethering = NetworkOperatorTetheringManager::CreateFromConnectionProfile(&profile)?;
    if tethering.TetheringOperationalState()? != TetheringOperationalState::Off {
        return Err("Mobile Hotspot must be off before the fixture starts".into());
    }
    std::fs::create_dir_all(&directory)?;
    recovery::save(&directory, &tethering)?;
    let options = Options {
        online_coordinator: std::env::args().nth(2),
        ..Options::default()
    };
    let embedded = Embedded::start(&directory.join("coordinator"), options).await?;
    std::fs::write(
        directory.join("access.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "pid": std::process::id(),
            "origin": embedded.origin(),
            "client_port": embedded.client_addr().port(),
            "token": embedded.api_token(),
        }))?,
    )?;
    println!(
        "Native fixture ready; access is in {}",
        directory.join("access.json").display()
    );
    let deadline = Instant::now() + Duration::from_secs(20 * 60);
    while !directory.join("stop").exists() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    embedded.stop().await;
    // The fixture changes only a previously stopped hotspot. Restore its saved
    // settings once our hotspot is off, preserving an outside replacement.
    recovery::restore(&directory)?;
    println!("Native fixture stopped");
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("This hardware fixture requires Windows.");
}
