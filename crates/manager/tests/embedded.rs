//! The embedded coordinator without a window.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use manager::embedded::{Embedded, Options};
use manager::{hotspot, remote, update};
use serde_json::{json, Value};

fn options() -> Options {
    Options {
        // The app uses the fixed client port; tests must not collide.
        client_bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        // Never this computer's real hotspot.
        hotspot: Arc::new(hotspot::Unsupported),
        // Nor its real Wi-Fi.
        wifi: Arc::new(manager::wifi::Unsupported),
        online_coordinator: None,
    }
}

#[tokio::test]
async fn the_entry_url_signs_the_ui_in() {
    let data_dir = tempfile::tempdir().unwrap();
    let embedded = Embedded::start(data_dir.path(), options()).await.unwrap();
    let origin = embedded.origin();
    assert!(origin.starts_with("http://127.0.0.1:"), "{origin}");

    // The UI listens on loopback, separately from the client port.
    let ui_port: u16 = origin.rsplit(':').next().unwrap().parse().unwrap();
    assert_ne!(ui_port, embedded.client_addr().port());

    // What the UI does on load: take the token from the fragment and
    // exchange it for a session.
    let entry = embedded.entry_url();
    let (page, token) = entry.split_once("/#token=").expect("token in the fragment");
    assert_eq!(page, origin);
    assert_eq!(token.len(), 64);

    let http = reqwest::Client::builder().cookie_store(true).build().unwrap();
    let session: Value =
        http.get(format!("{origin}/api/v1/session")).send().await.unwrap().json().await.unwrap();
    assert_eq!(session["auth_mode"], "token");
    assert_eq!(session["authenticated"], false);
    let response = http
        .post(format!("{origin}/api/v1/auth/token"))
        .json(&json!({ "token": token }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    let devices = http.get(format!("{origin}/api/v1/devices")).send().await.unwrap();
    assert_eq!(devices.status(), 200);

    // The same check the manager runs against a remote coordinator.
    let health = remote::check(&origin).await.unwrap();
    assert_eq!(health.protocol_version, protocol::PROTOCOL_VERSION);

    embedded.stop().await;
    assert!(matches!(remote::check(&origin).await, Err(remote::RemoteError::Unreachable(_))));
}

#[tokio::test]
async fn every_launch_has_its_own_token_and_keeps_its_data() {
    let data_dir = tempfile::tempdir().unwrap();
    let first = Embedded::start(data_dir.path(), options()).await.unwrap();
    let first_entry = first.entry_url();
    coordinator_core::admin::create_enrollment_key(data_dir.path(), "kept").unwrap();
    first.stop().await;

    let second = Embedded::start(data_dir.path(), options()).await.unwrap();
    let token = |entry: &str| entry.split_once("#token=").unwrap().1.to_owned();
    assert_ne!(token(&first_entry), token(&second.entry_url()));
    assert_eq!(coordinator_core::admin::list_enrollment_keys(data_dir.path()).unwrap().len(), 1);
    second.stop().await;
}

#[derive(Default)]
struct FakeHotspot(Mutex<Vec<&'static str>>);

impl hotspot::HotspotControl for FakeHotspot {
    fn control(&self) -> hotspot::Control {
        hotspot::Control::Automatic
    }

    fn start(&self, _ssid: &str, _passphrase: &str) -> hotspot::BoxFuture<Result<(), String>> {
        self.0.lock().unwrap().push("start");
        Box::pin(async { Ok(()) })
    }

    fn stop(&self) -> hotspot::BoxFuture<Result<(), String>> {
        self.0.lock().unwrap().push("stop");
        Box::pin(async { Ok(()) })
    }

    fn instructions(&self, _ssid: &str, _passphrase: &str) -> Vec<String> {
        Vec::new()
    }
}

#[tokio::test]
async fn failed_installation_restores_the_coordinator_and_data_without_restarting_its_hotspot() {
    let data_dir = tempfile::tempdir().unwrap();
    let hotspot = Arc::new(FakeHotspot::default());
    let first =
        Embedded::start(data_dir.path(), Options { hotspot: hotspot.clone(), ..options() }).await.unwrap();
    let client_addr = first.client_addr();
    let original_token = first.api_token().to_owned();
    let key = coordinator_core::admin::create_enrollment_key(data_dir.path(), "kept").unwrap();
    let http = reqwest::Client::new();
    http.post(format!("{}/api/v1/manager/hotspot/start", first.origin()))
        .bearer_auth(&original_token)
        .json(&json!({ "key_id": key.id }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    let mut embedded = Some(first);
    let progress = Mutex::new(Vec::new());
    let message = update::install_with_recovery(
        &mut embedded,
        data_dir.path(),
        Options { client_bind: client_addr, hotspot: hotspot.clone(), ..options() },
        || {
            // The installer must run only after both listeners and the
            // manager-owned hotspot have been stopped.
            assert!(std::net::TcpListener::bind(client_addr).is_ok());
            assert_eq!(*hotspot.0.lock().unwrap(), ["start", "stop"]);
            Err(tauri_plugin_updater::Error::InvalidUpdaterFormat)
        },
        |text| progress.lock().unwrap().push(text.to_owned()),
    )
    .await
    .unwrap_err();

    let restored = embedded.take().expect("coordinator restored after installer failure");
    assert_eq!(restored.client_addr(), client_addr);
    assert_ne!(restored.api_token(), original_token);
    assert_eq!(coordinator_core::admin::list_enrollment_keys(data_dir.path()).unwrap()[0].id, key.id);
    assert!(remote::check(&restored.origin()).await.is_ok());
    let status: Value = http
        .get(format!("{}/api/v1/manager/hotspot", restored.origin()))
        .bearer_auth(restored.api_token())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["state"], "off");
    assert_eq!(*hotspot.0.lock().unwrap(), ["start", "stop"]);
    assert!(message.contains("local coordinator is running again"), "{message}");
    assert!(message.contains("start the hotspot again"), "{message}");
    assert_eq!(
        *progress.lock().unwrap(),
        ["Stopping the local coordinator…", "Installing the update…", "Restoring the local coordinator…"]
    );
    restored.stop().await;
}

#[tokio::test]
async fn failed_installation_does_not_start_a_coordinator_that_was_not_running() {
    let data_dir = tempfile::tempdir().unwrap();
    let mut embedded = None;
    let message = update::install_with_recovery(
        &mut embedded,
        data_dir.path(),
        options(),
        || Err(tauri_plugin_updater::Error::InvalidUpdaterFormat),
        |_| {},
    )
    .await
    .unwrap_err();
    assert!(embedded.is_none());
    assert!(!message.contains("restarted"), "{message}");
    assert!(!message.contains("running again"), "{message}");
}

#[tokio::test]
async fn a_failed_recovery_leaves_a_clear_retry_instruction() {
    let data_dir = tempfile::tempdir().unwrap();
    let first = Embedded::start(data_dir.path(), options()).await.unwrap();
    let client_addr = first.client_addr();
    let mut embedded = Some(first);
    let mut obstruction = None;
    let message = update::install_with_recovery(
        &mut embedded,
        data_dir.path(),
        Options { client_bind: client_addr, ..options() },
        || {
            // Model another process claiming the client port after shutdown.
            obstruction = Some(std::net::TcpListener::bind(client_addr).unwrap());
            Err(tauri_plugin_updater::Error::InvalidUpdaterFormat)
        },
        |_| {},
    )
    .await
    .unwrap_err();
    assert!(embedded.is_none());
    assert!(message.contains("could not be restarted"), "{message}");
    assert!(message.contains("Use Start local coordinator to try again"), "{message}");
    drop(obstruction);
}

#[tokio::test]
async fn something_that_is_not_a_coordinator_is_recognised() {
    // A plain HTTP server that answers 404 to everything.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, axum::Router::new()).await;
    });
    assert_eq!(remote::check(&origin).await, Err(remote::RemoteError::NotACoordinator));
}
