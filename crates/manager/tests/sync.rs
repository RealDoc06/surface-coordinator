//! Copying enrollment keys from an online coordinator (a real one in
//! accounts mode) into the embedded one.

use std::net::SocketAddr;
use std::sync::Arc;

use coordinator_core::{admin, AuthMode, Config, Coordinator};
use manager::embedded::{Embedded, Options};
use manager::sync::{pull_keys, SyncError, SyncReport};
use manager::{hotspot, wifi};
use serde_json::json;

const USERNAME: &str = "operator";
const PASSWORD: &str = "fixture-password-only-for-tests";

struct Online {
    coordinator: Coordinator,
    data_dir: tempfile::TempDir,
    origin: String,
}

async fn online() -> Online {
    let data_dir = tempfile::tempdir().unwrap();
    let mut config = Config::new(SocketAddr::from(([127, 0, 0, 1], 0)), data_dir.path(), AuthMode::Accounts);
    // Never ask the real release source.
    config.updates.enabled = false;
    config.update_public_key = None;
    let coordinator = coordinator_core::start(config).await.unwrap();
    admin::create_account(data_dir.path(), USERNAME, PASSWORD).unwrap();
    let origin = format!("http://{}", coordinator.ui_addr());
    Online { coordinator, data_dir, origin }
}

/// Logs in as the browser would and returns the value of the session
/// cookie, which is what the manager reads from the window.
async fn log_in(origin: &str) -> String {
    let response = reqwest::Client::new()
        .post(format!("{origin}/api/v1/auth/login"))
        .json(&json!({"username": USERNAME, "password": PASSWORD}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let header = response.headers()["set-cookie"].to_str().unwrap();
    let pair = header.split(';').next().unwrap();
    pair.strip_prefix("sc_session=").expect("the session cookie").to_owned()
}

async fn local() -> (Embedded, tempfile::TempDir) {
    let data_dir = tempfile::tempdir().unwrap();
    let options = Options {
        client_bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        hotspot: Arc::new(hotspot::Unsupported),
        wifi: Arc::new(wifi::Unsupported),
        online_coordinator: None,
    };
    (Embedded::start(data_dir.path(), options).await.unwrap(), data_dir)
}

/// Ids and secrets of the keys in a data directory, revoked or not.
fn keys(data_dir: &std::path::Path) -> Vec<(String, String)> {
    let mut keys: Vec<_> =
        admin::list_enrollment_keys(data_dir).unwrap().into_iter().map(|key| (key.id, key.secret)).collect();
    keys.sort();
    keys
}

#[tokio::test]
async fn active_keys_are_copied_once_and_revoked_ones_are_left_behind() {
    let online = online().await;
    let first = admin::create_enrollment_key(online.data_dir.path(), "Classroom").unwrap();
    let second = admin::create_enrollment_key(online.data_dir.path(), "Lab").unwrap();
    let revoked = admin::create_enrollment_key(online.data_dir.path(), "Old").unwrap();
    admin::revoke_enrollment_key(online.data_dir.path(), &revoked.id).unwrap();
    let cookie = log_in(&online.origin).await;
    let (embedded, local_dir) = local().await;

    let report = pull_keys(&online.origin, &cookie, &embedded.origin(), embedded.api_token()).await.unwrap();
    assert_eq!(report, SyncReport { imported: 2, already_present: 0, skipped_revoked: 1, revoked: 0 });
    // Same ids and secrets: devices enrolled online are accepted here, and
    // the hotspot for a key has the same name and passphrase.
    let mut expected = vec![(first.id.clone(), first.secret), (second.id, second.secret)];
    expected.sort();
    assert_eq!(keys(local_dir.path()), expected);
    let labels: Vec<_> =
        admin::list_enrollment_keys(local_dir.path()).unwrap().into_iter().map(|key| key.label).collect();
    assert!(labels.contains(&"Classroom".to_owned()), "{labels:?}");

    let again = pull_keys(&online.origin, &cookie, &embedded.origin(), embedded.api_token()).await.unwrap();
    assert_eq!(again, SyncReport { imported: 0, already_present: 2, skipped_revoked: 1, revoked: 0 });
    assert_eq!(keys(local_dir.path()), expected);

    embedded.stop().await;
    online.coordinator.stop().await;
}

#[tokio::test]
async fn revoking_an_online_key_also_revokes_its_previously_synced_copy() {
    let online = online().await;
    let key = admin::create_enrollment_key(online.data_dir.path(), "Retired classroom").unwrap();
    let cookie = log_in(&online.origin).await;
    let (embedded, local_dir) = local().await;
    pull_keys(&online.origin, &cookie, &embedded.origin(), embedded.api_token()).await.unwrap();
    // Keys belonging only to this manager remain usable.
    let local_key = admin::create_enrollment_key(local_dir.path(), "Offline classroom").unwrap();
    admin::revoke_enrollment_key(online.data_dir.path(), &key.id).unwrap();

    let report = pull_keys(&online.origin, &cookie, &embedded.origin(), embedded.api_token()).await.unwrap();
    assert_eq!(report.revoked, 1);
    let local_keys = admin::list_enrollment_keys(local_dir.path()).unwrap();
    assert!(local_keys.iter().find(|local| local.id == key.id).unwrap().revoked.is_some());
    assert!(local_keys.iter().find(|local| local.id == local_key.id).unwrap().revoked.is_none());

    let again = pull_keys(&online.origin, &cookie, &embedded.origin(), embedded.api_token()).await.unwrap();
    assert_eq!(again.revoked, 0);
    embedded.stop().await;
    online.coordinator.stop().await;
}

#[tokio::test]
async fn an_ended_session_is_not_logged_in_and_nothing_is_imported() {
    let online = online().await;
    admin::create_enrollment_key(online.data_dir.path(), "Classroom").unwrap();
    let cookie = log_in(&online.origin).await;
    let logout = reqwest::Client::new()
        .post(format!("{}/api/v1/auth/logout", online.origin))
        .header("Cookie", format!("sc_session={cookie}"))
        .send()
        .await
        .unwrap();
    assert_eq!(logout.status(), 204);
    let (embedded, local_dir) = local().await;

    for stale in [cookie.as_str(), "not-a-session"] {
        let outcome = pull_keys(&online.origin, stale, &embedded.origin(), embedded.api_token()).await;
        assert!(matches!(outcome, Err(SyncError::NotLoggedIn)), "{outcome:?}");
    }
    assert_eq!(keys(local_dir.path()), vec![]);

    embedded.stop().await;
    online.coordinator.stop().await;
}

#[tokio::test]
async fn an_unreachable_online_coordinator_is_an_error() {
    // A port nothing listens on any more.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gone = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let (embedded, local_dir) = local().await;

    let outcome = pull_keys(&gone, "whatever", &embedded.origin(), embedded.api_token()).await;
    assert!(matches!(outcome, Err(SyncError::Unreachable(_))), "{outcome:?}");
    assert_eq!(keys(local_dir.path()), vec![]);
    embedded.stop().await;
}
