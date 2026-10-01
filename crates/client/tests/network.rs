//! Network state machine and Wi-Fi handoff against a fake platform and two
//! fake coordinators: "remote", the configured coordinator, and "gateway",
//! the coordinator embedded in the manager at the hotspot's gateway address.

mod common;

use std::time::{Duration, Instant};

use client::platform::{PlatformError, WifiJoinRequest};
use client::ClientConfig;
use common::{
    client_config, start_client, wifi_on, FakeCoordinator, FakePlatform, Unreachable, KEY_ID,
    SECRET,
};
use protocol::{hotspot, JobResult, JobStatus};
use serde_json::{json, Value};

const QUIET: Duration = Duration::from_millis(300);
/// The network an operator moves devices to. Not the hotspot.
const OFFICE: &str = "Office-Net-Fixture";
const OFFICE_PASSWORD: &str = "fake-office-passphrase";

fn hotspot_ssid() -> String {
    hotspot::ssid(KEY_ID)
}

/// A client configured for `remote` whose hotspot coordinator is `gateway`.
fn config(remote: Option<std::net::SocketAddr>, gateway: &FakeCoordinator) -> ClientConfig {
    let mut config = client_config(gateway.addr);
    config.coordinator_url = remote.map(|addr| format!("ws://{addr}"));
    config.hotspot_port = gateway.addr.port();
    config
}

fn wifi_join(ssid: &str, password: Option<&str>, coordinator_url: Option<String>) -> Value {
    let mut payload = json!({ "ssid": ssid });
    if let Some(password) = password {
        payload["password"] = json!(password);
    }
    if let Some(url) = coordinator_url {
        payload["coordinator_url"] = json!(url);
    }
    payload
}

fn assert_failed(result: &JobResult, reason: &str) {
    assert_eq!(result.status, JobStatus::Failed, "{result:?}");
    assert_eq!(result.reason.as_deref(), Some(reason), "{result:?}");
    assert_eq!(result.exit_code, None);
}

// ------------------------------------------------------------ state machine

#[tokio::test]
async fn no_scan_or_join_happens_while_a_session_is_up() {
    let remote = FakeCoordinator::start().await;
    let gateway = FakeCoordinator::start().await;
    let platform = FakePlatform::with_wifi();
    platform.set_current(Some(wifi_on(OFFICE)));
    platform.set_visible(&[OFFICE, &hotspot_ssid()]);
    platform.on_join(&hotspot_ssid(), Ok(wifi_on(&hotspot_ssid())));
    let client = start_client(config(Some(remote.addr), &gateway), platform.clone());

    let (mut connection, hello) = remote.accept().await;
    // The network goes into `hello`.
    let network = hello.network.expect("hello carries the network");
    assert_eq!(network.ssid.as_deref(), Some(OFFICE));
    assert_eq!(network.ip.as_deref(), Some("192.168.137.23"));

    // Well past the search threshold.
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(platform.scans().is_empty());
    assert!(platform.joins().is_empty());
    assert!(gateway.no_connection_within(Duration::from_millis(10)).await);
    // Still the same, working session.
    connection.send_script("t1", "out still here").await;
    connection.expect_started("t1").await;
    client.stop().await;
}

#[tokio::test]
async fn an_unreachable_coordinator_leads_to_the_hotspot_after_the_threshold() {
    let remote = Unreachable::start().await;
    let gateway = FakeCoordinator::start().await;
    let platform = FakePlatform::with_wifi();
    platform.set_current(Some(wifi_on(OFFICE)));
    platform.set_visible(&[OFFICE, &hotspot_ssid()]);
    platform.on_join(&hotspot_ssid(), Ok(wifi_on(&hotspot_ssid())));
    let config = config(Some(remote.addr), &gateway);
    let search_after = config.search_after;

    let started = Instant::now();
    let client = start_client(config, platform.clone());
    let (_connection, hello) = gateway.accept().await;

    // Nothing was scanned before the threshold.
    let scans = platform.scans();
    assert!(!scans.is_empty());
    assert!(scans[0] >= started + search_after, "first scan after {:?}", scans[0] - started);
    // The hotspot was joined with what the enrollment key implies, and not
    // as a network Windows would come back to by itself.
    assert_eq!(
        platform.joins(),
        [WifiJoinRequest {
            ssid: hotspot_ssid(),
            passphrase: Some(hotspot::passphrase(SECRET)),
            auto_connect: false,
        }]
    );
    let network = hello.network.expect("hello carries the network");
    assert_eq!(network.ssid, Some(hotspot_ssid()));
    client.stop().await;
}

#[tokio::test]
async fn without_a_configured_coordinator_searching_starts_at_once() {
    let gateway = FakeCoordinator::start().await;
    let platform = FakePlatform::with_wifi();
    platform.set_visible(&[&hotspot_ssid()]);
    platform.on_join(&hotspot_ssid(), Ok(wifi_on(&hotspot_ssid())));
    let mut config = config(None, &gateway);
    // Only the missing URL can explain a search within the test's patience.
    config.search_after = Duration::from_secs(600);
    let client = start_client(config, platform.clone());

    let (_connection, hello) = gateway.accept().await;
    assert_eq!(hello.network.unwrap().ssid, Some(hotspot_ssid()));
    assert_eq!(platform.scans().len(), 1);
    assert_eq!(platform.joins().len(), 1);
    client.stop().await;
}

#[tokio::test]
async fn an_invisible_hotspot_is_not_joined_and_scans_repeat_every_interval() {
    let gateway = FakeCoordinator::start().await;
    let platform = FakePlatform::with_wifi();
    platform.set_visible(&[OFFICE, "Some-Neighbour"]);
    let config = config(None, &gateway);
    let interval = config.search_interval;
    let client = start_client(config, platform.clone());

    tokio::time::sleep(Duration::from_millis(650)).await;
    let scans = platform.scans();
    assert!(scans.len() >= 3, "only {} scans", scans.len());
    assert!(scans.len() <= 8, "{} scans: not waiting the interval", scans.len());
    for pair in scans.windows(2) {
        // Timers may round to the millisecond.
        assert!(pair[1] - pair[0] >= interval - Duration::from_millis(5), "{:?}", pair[1] - pair[0]);
    }
    assert!(platform.joins().is_empty());
    assert!(gateway.no_connection_within(Duration::from_millis(10)).await);
    client.stop().await;
}

#[tokio::test]
async fn a_device_already_on_the_hotspot_tries_the_gateway_without_scanning() {
    let remote = Unreachable::start().await;
    let gateway = FakeCoordinator::start().await;
    let platform = FakePlatform::with_wifi();
    platform.set_current(Some(wifi_on(&hotspot_ssid())));
    platform.set_visible(&[&hotspot_ssid()]);
    let mut config = config(Some(remote.addr), &gateway);
    // Far from searching: the gateway is tried because the device is there.
    config.search_after = Duration::from_secs(600);
    let client = start_client(config, platform.clone());

    let (_connection, hello) = gateway.accept().await;
    assert_eq!(hello.network.unwrap().ssid, Some(hotspot_ssid()));
    assert!(platform.scans().is_empty());
    assert!(platform.joins().is_empty());
    client.stop().await;
}

#[tokio::test]
async fn the_configured_coordinator_is_preferred_once_it_is_back() {
    let remote = FakeCoordinator::start().await;
    let gateway = FakeCoordinator::start().await;
    let platform = FakePlatform::with_wifi();
    platform.set_current(Some(wifi_on(&hotspot_ssid())));
    let client = start_client(config(Some(remote.addr), &gateway), platform.clone());

    // The configured coordinator is tried first, but turns the client away...
    let mut refused = remote.accept_raw().await;
    refused.expect_hello().await;
    refused.send(protocol::ServerMessage::Reject { reason: "unknown_key".into() }).await;
    // ...so the client falls back to the gateway of the hotspot it is on.
    let (on_hotspot, _) = gateway.accept().await;

    // The hotspot coordinator goes away; the configured one now accepts.
    on_hotspot.drop_abruptly();
    let (mut connection, _) = remote.accept().await;
    connection.send_script("t1", "out on the remote").await;
    connection.expect_started("t1").await;
    assert!(gateway.no_connection_within(QUIET).await);
    assert!(platform.scans().is_empty());
    client.stop().await;
}

// ------------------------------------------------------------ wifi.join

#[tokio::test]
async fn wifi_join_moves_the_device_to_the_new_coordinator_and_reports_success_there() {
    let remote = FakeCoordinator::start().await;
    let gateway = FakeCoordinator::start().await;
    let state_dir = tempfile::tempdir().unwrap();
    let platform = FakePlatform::with_wifi();
    platform.set_current(Some(wifi_on(&hotspot_ssid())));
    platform.on_join(OFFICE, Ok(wifi_on(OFFICE)));
    let mut config = config(None, &gateway);
    config.state_dir = Some(state_dir.path().to_owned());
    let client = start_client(config.clone(), platform.clone());

    let (mut on_hotspot, _) = gateway.accept().await;
    let remote_url = format!("ws://{}", remote.addr);
    on_hotspot
        .send_job("move", "wifi.join", wifi_join(OFFICE, Some(OFFICE_PASSWORD), Some(remote_url.clone())))
        .await;
    on_hotspot.expect_started("move").await;
    // The client leaves the hotspot coordinator at once.
    assert!(on_hotspot.closed_by_client().await);

    let (mut connection, hello) = remote.accept().await;
    assert_eq!(hello.network.unwrap().ssid.as_deref(), Some(OFFICE));
    // The job is reported to the coordinator the device reached.
    connection.expect_started("move").await;
    let result = connection.expect_result("move").await;
    assert_eq!(result.status, JobStatus::Succeeded, "{result:?}");
    assert_eq!(result.exit_code, None);
    assert_eq!(result.reason, None);
    assert!(result.message.as_deref().unwrap_or("").contains(OFFICE), "{result:?}");
    connection.ack("move").await;
    // Long enough for the client to take the acknowledgement in.
    assert_eq!(connection.try_recv(QUIET).await, None);

    assert_eq!(
        platform.joins(),
        [WifiJoinRequest {
            ssid: OFFICE.into(),
            passphrase: Some(OFFICE_PASSWORD.into()),
            auto_connect: true,
        }]
    );
    let stored: Value =
        serde_json::from_slice(&std::fs::read(state_dir.path().join("network.json")).unwrap()).unwrap();
    assert_eq!(stored["coordinator_url"], remote_url.as_str());
    client.stop().await;
    drop(connection);

    // The new coordinator survives a restart, without any configured URL
    // and without Wi-Fi to fall back on.
    let client = start_client(config, FakePlatform::new());
    let (mut connection, _) = remote.accept().await;
    // Acknowledged before the restart: nothing to replay.
    assert_eq!(connection.try_recv(QUIET).await, None);
    client.stop().await;
}

#[tokio::test]
async fn wifi_join_that_cannot_connect_fails_on_the_same_session() {
    let gateway = FakeCoordinator::start().await;
    let state_dir = tempfile::tempdir().unwrap();
    let platform = FakePlatform::with_wifi();
    platform.set_current(Some(wifi_on(&hotspot_ssid())));
    platform.on_join(OFFICE, Err(PlatformError::Failed("the passphrase was refused".into())));
    let mut config = config(None, &gateway);
    config.state_dir = Some(state_dir.path().to_owned());
    let client = start_client(config, platform.clone());

    let (mut connection, _) = gateway.accept().await;
    let other = Unreachable::start().await;
    connection
        .send_job("move", "wifi.join", wifi_join(OFFICE, Some(OFFICE_PASSWORD), Some(other.url())))
        .await;
    connection.expect_started("move").await;
    let result = connection.expect_result("move").await;
    assert_failed(&result, "error");
    assert_eq!(result.message.as_deref(), Some("the passphrase was refused"));

    // Nothing changed: same session, no reconnect, no stored URL.
    connection.send_script("t1", "out still here").await;
    connection.expect_started("t1").await;
    assert!(gateway.no_connection_within(QUIET).await);
    let stored = std::fs::read(state_dir.path().join("network.json"))
        .map(|bytes| serde_json::from_slice::<Value>(&bytes).unwrap())
        .unwrap_or(Value::Null);
    assert!(stored.get("coordinator_url").is_none_or(Value::is_null), "{stored}");
    assert_eq!(platform.current(), Some(wifi_on(&hotspot_ssid())));
    client.stop().await;
}

#[tokio::test]
async fn wifi_join_without_a_coordinator_on_the_new_network_returns_to_the_hotspot() {
    let gateway = FakeCoordinator::start().await;
    let state_dir = tempfile::tempdir().unwrap();
    let platform = FakePlatform::with_wifi();
    platform.set_current(Some(wifi_on(&hotspot_ssid())));
    platform.set_visible(&[OFFICE, &hotspot_ssid()]);
    platform.on_join(OFFICE, Ok(wifi_on(OFFICE)));
    platform.on_join(&hotspot_ssid(), Ok(wifi_on(&hotspot_ssid())));
    let mut config = config(None, &gateway);
    config.state_dir = Some(state_dir.path().to_owned());
    let handoff_timeout = config.handoff_timeout;
    let client = start_client(config, platform.clone());

    let (mut first, _) = gateway.accept().await;
    let nowhere = Unreachable::start().await;
    first
        .send_job("move", "wifi.join", wifi_join(OFFICE, Some(OFFICE_PASSWORD), Some(nowhere.url())))
        .await;
    let sent = Instant::now();
    first.expect_started("move").await;
    assert!(first.closed_by_client().await);

    // Back on the hotspot once the handoff has given up, and not before.
    let (mut back, hello) = gateway.accept().await;
    assert!(sent.elapsed() >= handoff_timeout, "back after {:?}", sent.elapsed());
    assert_eq!(hello.network.unwrap().ssid, Some(hotspot_ssid()));
    back.expect_started("move").await;
    let result = back.expect_result("move").await;
    assert_failed(&result, "timeout");
    assert!(result.message.as_deref().unwrap_or("").contains(OFFICE), "{result:?}");

    // One scan, after the handoff gave up, then the hotspot again.
    assert_eq!(platform.scans().len(), 1);
    let joins = platform.joins();
    assert_eq!(joins.len(), 2, "{joins:?}");
    assert_eq!((joins[0].ssid.as_str(), joins[0].auto_connect), (OFFICE, true));
    assert_eq!((joins[1].ssid.clone(), joins[1].auto_connect), (hotspot_ssid(), false));
    // The unreachable coordinator was rolled back.
    let stored: Value =
        serde_json::from_slice(&std::fs::read(state_dir.path().join("network.json")).unwrap()).unwrap();
    assert!(stored.get("coordinator_url").is_none_or(Value::is_null), "{stored}");
    client.stop().await;
}

#[tokio::test]
async fn wifi_join_with_an_invalid_payload_does_not_touch_the_network() {
    let remote = FakeCoordinator::start().await;
    let gateway = FakeCoordinator::start().await;
    let platform = FakePlatform::with_wifi();
    platform.on_join(OFFICE, Ok(wifi_on(OFFICE)));
    let client = start_client(config(Some(remote.addr), &gateway), platform.clone());
    let (mut connection, _) = remote.accept().await;

    let invalid = [
        ("empty-ssid", wifi_join("", Some(OFFICE_PASSWORD), None)),
        ("long-ssid", wifi_join(&"x".repeat(33), None, None)),
        ("http-url", wifi_join(OFFICE, None, Some("http://coordinator.example".into()))),
        ("no-scheme", wifi_join(OFFICE, None, Some("coordinator.example".into()))),
        ("not-an-object", json!("Office")),
        ("ssid-missing", json!({"password": OFFICE_PASSWORD})),
    ];
    for (id, payload) in invalid {
        connection.send_job(id, "wifi.join", payload).await;
        connection.expect_started(id).await;
        let result = connection.expect_result(id).await;
        assert_failed(&result, "invalid_payload");
        assert!(result.message.is_some());
    }
    assert!(platform.joins().is_empty());
    assert_eq!(connection.try_recv(QUIET).await, None);
    client.stop().await;
}

#[tokio::test]
async fn wifi_join_on_a_platform_without_wifi_fails_as_unsupported() {
    let remote = FakeCoordinator::start().await;
    let gateway = FakeCoordinator::start().await;
    let client = start_client(config(Some(remote.addr), &gateway), FakePlatform::new());
    let (mut connection, _) = remote.accept().await;

    connection.send_job("move", "wifi.join", wifi_join(OFFICE, None, None)).await;
    connection.expect_started("move").await;
    assert_failed(&connection.expect_result("move").await, "unsupported");
    // Still connected to the same coordinator.
    connection.send_script("t1", "exit 0").await;
    connection.expect_started("t1").await;
    assert_eq!(connection.expect_result("t1").await.status, JobStatus::Succeeded);
    assert!(gateway.no_connection_within(QUIET).await);
    client.stop().await;
}
