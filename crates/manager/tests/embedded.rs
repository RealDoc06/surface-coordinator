//! The embedded coordinator without a window.

use std::net::SocketAddr;
use std::sync::Arc;

use manager::embedded::{Embedded, Options};
use manager::{hotspot, remote};
use serde_json::{json, Value};

fn options() -> Options {
    Options {
        // The app uses the fixed client port; tests must not collide.
        client_bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        // Never this computer's real hotspot.
        hotspot: Arc::new(hotspot::Unsupported),
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
