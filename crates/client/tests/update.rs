//! `client.update` against a fake coordinator with a fake download server
//! and a fake platform whose "executable" is a file in a temporary
//! directory: installing, completing after the restart, every way an
//! update is refused, cancelling, and the rollback after three starts
//! without a handshake. Signing keys are generated per test run.
//!
//! Only where client updates exist (`protocol::update::current_target`).
#![cfg(all(windows, target_arch = "x86_64"))]

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use client::update::{read_marker, Marker, DOWNLOAD_FILE};
use client::ClientConfig;
use common::{
    client_config, start_client, FakeCoordinator, FakePlatform, RunningClient, Served, Unreachable,
    UpdateServer, WAIT,
};
use protocol::update::{self, TARGET_WINDOWS_X86_64};
use protocol::{reason, JobResult, JobStatus, OutputStream};
use serde_json::{json, Value};

const QUIET: Duration = Duration::from_millis(300);
const RUNNING: &str = "1.4.0";
const NEW: &str = "1.5.0";
const OLD_EXE: &[u8] = b"fake executable of version 1.4.0";
const NEW_EXE: &[u8] = b"fake executable of version 1.5.0, a little longer";

/// A throwaway Ed25519 key pair, base64.
struct Keys {
    signing: String,
    public: String,
}

fn keys() -> Keys {
    let seed: [u8; 32] = rand::random();
    let signing = STANDARD.encode(seed);
    let public = update::public_key_for(&signing).unwrap();
    Keys { signing, public }
}

fn target() -> &'static str {
    update::current_target().expect("these tests run on a platform with client updates")
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    update::parse_sha256_hex(&update::sha256_hex(bytes)).unwrap()
}

fn signature(keys: &Keys, version: &str, target: &str, bytes: &[u8]) -> String {
    // With a trailing newline, as the coordinator serves it.
    format!("{}\n", update::sign(&keys.signing, version, target, &digest(bytes)).unwrap())
}

fn artifact_path(version: &str) -> String {
    format!("/updates/client/{version}/{}", target())
}

fn payload(version: &str, bytes: &[u8]) -> Value {
    json!({
        "version": version,
        "target": target(),
        "size": bytes.len(),
        "sha256": update::sha256_hex(bytes),
    })
}

/// A device: its state directory, and an install directory with the
/// running executable.
struct Device {
    state: tempfile::TempDir,
    install: tempfile::TempDir,
}

impl Device {
    fn new() -> Device {
        let device = Device { state: tempfile::tempdir().unwrap(), install: tempfile::tempdir().unwrap() };
        std::fs::write(device.exe(), OLD_EXE).unwrap();
        device
    }

    fn exe(&self) -> PathBuf {
        self.install.path().join("client.exe")
    }

    fn old_exe(&self) -> PathBuf {
        self.install.path().join("client.old.exe")
    }

    fn failed_exe(&self) -> PathBuf {
        self.install.path().join("client.failed.exe")
    }

    fn download(&self) -> PathBuf {
        self.install.path().join(DOWNLOAD_FILE)
    }

    fn marker(&self) -> Option<Marker> {
        read_marker(self.state.path()).unwrap()
    }

    fn platform(&self) -> Arc<FakePlatform> {
        let platform = FakePlatform::new();
        platform.set_executable(&self.exe());
        platform
    }

    fn config(&self, coordinator_url: String, version: &str, keys: &Keys) -> ClientConfig {
        let mut config = client_config("127.0.0.1:1".parse().unwrap());
        config.coordinator_url = Some(coordinator_url);
        config.client_version = version.into();
        config.update_public_key = keys.public.clone();
        config.state_dir = Some(self.state.path().to_owned());
        config
    }

    fn start(&self, coordinator: &FakeCoordinator, version: &str, keys: &Keys) -> (RunningClient, Arc<FakePlatform>) {
        let platform = self.platform();
        let config = self.config(format!("ws://{}", coordinator.addr), version, keys);
        (start_client(config, platform.clone()), platform)
    }

    /// The job's entry in the client's job ledger.
    fn ledger_entry(&self, id: &str) -> Value {
        let ledger: Value = serde_json::from_slice(&std::fs::read(self.state.path().join("jobs.json")).unwrap()).unwrap();
        ledger["entries"].as_array().unwrap().iter().find(|entry| entry["id"] == id).cloned().unwrap()
    }

    /// Nothing of a failed update is left, and nothing was replaced.
    fn assert_untouched(&self, platform: &FakePlatform) {
        assert_eq!(platform.update_calls(), Vec::<&str>::new());
        assert_eq!(std::fs::read(self.exe()).unwrap(), OLD_EXE);
        assert!(!self.download().exists(), "the download was left behind");
        assert!(!self.old_exe().exists());
        assert_eq!(self.marker(), None);
    }
}

async fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !condition() {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The client stops by itself, as after a restart request.
async fn stops_by_itself(client: RunningClient) {
    tokio::time::timeout(WAIT, client.task).await.expect("the client did not stop by itself").unwrap();
}

fn assert_failed(result: &JobResult, expected: &str) {
    assert_eq!(result.status, JobStatus::Failed, "{result:?}");
    assert_eq!(result.reason.as_deref(), Some(expected), "{result:?}");
}

/// Serves `bytes` as version `NEW` with a valid signature.
fn publish(updates: &UpdateServer, keys: &Keys, bytes: &[u8]) {
    updates.serve(&artifact_path(NEW), Served::Bytes(bytes.to_vec()));
    let sig = signature(keys, NEW, target(), bytes);
    updates.serve(&format!("{}.sig", artifact_path(NEW)), Served::Bytes(sig.into_bytes()));
}

/// Runs a `client.update` to `NEW` through to the restart request and
/// checks everything it leaves for the next start.
async fn install_update(device: &Device, keys: &Keys, id: &str) {
    let updates = UpdateServer::start().await;
    publish(&updates, keys, NEW_EXE);
    let coordinator = FakeCoordinator::with_updates(&updates).await;
    let (client, platform) = device.start(&coordinator, RUNNING, keys);
    let (mut connection, _) = coordinator.accept().await;

    connection.send_job(id, "client.update", payload(NEW, NEW_EXE)).await;
    connection.expect_started(id).await;
    connection
        .expect_output(id, 1, OutputStream::Stdout, &format!("restarting to complete the update to {NEW}\n"))
        .await;
    // Running, not finished: no result before the client goes.
    assert_eq!(connection.try_recv(QUIET).await, None);
    stops_by_itself(client).await;

    assert_eq!(platform.update_calls(), ["swap", "restart"]);
    assert_eq!(std::fs::read(device.exe()).unwrap(), NEW_EXE);
    assert_eq!(std::fs::read(device.old_exe()).unwrap(), OLD_EXE);
    assert!(!device.download().exists());
    assert_eq!(
        device.marker(),
        Some(Marker {
            job_target_id: id.into(),
            previous_version: RUNNING.into(),
            new_version: NEW.into(),
            starts: 0,
            rolled_back: false,
        })
    );
    let entry = device.ledger_entry(id);
    assert_eq!(entry["result"], Value::Null, "{entry}");
    assert_eq!(entry["awaiting_restart"], true, "{entry}");
    assert_eq!(
        updates.requests(),
        [artifact_path(NEW), format!("{}.sig", artifact_path(NEW))]
    );
}

// ------------------------------------------------------------ installing and completing

#[tokio::test]
async fn an_update_is_downloaded_verified_swapped_in_and_the_client_restarts() {
    let keys = keys();
    let device = Device::new();
    install_update(&device, &keys, "t1").await;
}

#[tokio::test]
async fn the_new_version_reports_success_once_after_its_first_handshake() {
    let keys = keys();
    let device = Device::new();
    install_update(&device, &keys, "t1").await;

    // The service manager starts the new executable.
    let coordinator = FakeCoordinator::start().await;
    let (client, platform) = device.start(&coordinator, NEW, &keys);
    let (mut connection, hello) = coordinator.accept().await;
    assert_eq!(hello.client_version, NEW);
    // Still running as far as the coordinator knows, then done.
    connection.expect_started("t1").await;
    let result = connection.expect_result("t1").await;
    assert_eq!(result.status, JobStatus::Succeeded, "{result:?}");
    assert_eq!(result.message.as_deref(), Some("updated to 1.5.0"));
    assert_eq!(platform.update_calls(), ["remove_old"]);
    assert!(!device.old_exe().exists());
    assert_eq!(std::fs::read(device.exe()).unwrap(), NEW_EXE);
    assert_eq!(device.marker(), None);

    // Sent again by a coordinator that missed the result: not run again.
    connection.send_job("t1", "client.update", payload(NEW, NEW_EXE)).await;
    assert_eq!(connection.try_recv(QUIET).await, None);
    connection.ack("t1").await;
    connection.drop_abruptly();
    let (mut again, _) = coordinator.accept().await;
    assert_eq!(again.try_recv(QUIET).await, None, "reported more than once");
    client.stop().await;

    // A later, ordinary restart has nothing to do with the update.
    let (client, platform) = device.start(&coordinator, NEW, &keys);
    let (mut connection, _) = coordinator.accept().await;
    assert_eq!(connection.try_recv(QUIET).await, None);
    assert_eq!(platform.update_calls(), Vec::<&str>::new());
    client.stop().await;
}

#[tokio::test]
async fn an_ordinary_restart_without_an_update_is_unaffected() {
    let keys = keys();
    let device = Device::new();
    let coordinator = FakeCoordinator::start().await;
    let (client, _) = device.start(&coordinator, RUNNING, &keys);
    let (mut connection, _) = coordinator.accept().await;
    // Cut short by the restart.
    connection.send_script("t1", "wait").await;
    connection.expect_started("t1").await;
    client.stop().await;

    let (client, platform) = device.start(&coordinator, RUNNING, &keys);
    let (mut connection, _) = coordinator.accept().await;
    connection.expect_started("t1").await;
    assert_failed(&connection.expect_result("t1").await, reason::INTERRUPTED);
    assert_eq!(connection.try_recv(QUIET).await, None);
    assert_eq!(platform.update_calls(), Vec::<&str>::new());
    assert_eq!(std::fs::read(device.exe()).unwrap(), OLD_EXE);
    client.stop().await;
}

// ------------------------------------------------------------ rollback

#[tokio::test]
async fn three_starts_without_a_handshake_restore_the_previous_version_which_reports_the_failure() {
    let keys = keys();
    let device = Device::new();
    install_update(&device, &keys, "t1").await;

    // The new version starts, but never reaches a coordinator.
    let nowhere = Unreachable::start().await;
    for start in 1..=3 {
        let platform = device.platform();
        let client = start_client(device.config(nowhere.url(), NEW, &keys), platform.clone());
        wait_for("the start to be counted", || device.marker().is_some_and(|m| m.starts == start)).await;
        client.stop().await;
        assert_eq!(platform.update_calls(), Vec::<&str>::new(), "start {start}");
    }

    // The fourth start puts the previous version back and restarts.
    let platform = device.platform();
    let client = start_client(device.config(nowhere.url(), NEW, &keys), platform.clone());
    stops_by_itself(client).await;
    assert_eq!(platform.update_calls(), ["restore", "restart"]);
    assert_eq!(std::fs::read(device.exe()).unwrap(), OLD_EXE);
    assert_eq!(std::fs::read(device.failed_exe()).unwrap(), NEW_EXE);
    let marker = device.marker().unwrap();
    assert!(marker.rolled_back);
    assert_eq!(marker.starts, 4);

    // The previous version reports the job as failed.
    let coordinator = FakeCoordinator::start().await;
    let (client, platform) = device.start(&coordinator, RUNNING, &keys);
    let (mut connection, _) = coordinator.accept().await;
    connection.expect_started("t1").await;
    let result = connection.expect_result("t1").await;
    assert_failed(&result, reason::ROLLED_BACK);
    assert_eq!(
        result.message.as_deref(),
        Some("version 1.5.0 did not complete a handshake in 3 starts; version 1.4.0 restored")
    );
    assert_eq!(platform.update_calls(), ["remove_old"]);
    assert!(!device.failed_exe().exists());
    assert_eq!(device.marker(), None);
    client.stop().await;
}

#[tokio::test]
async fn a_marker_matching_neither_version_is_dropped_and_the_job_fails() {
    let keys = keys();
    let device = Device::new();
    install_update(&device, &keys, "t1").await;

    let coordinator = FakeCoordinator::start().await;
    let (client, platform) = device.start(&coordinator, "1.6.0", &keys);
    let (mut connection, _) = coordinator.accept().await;
    connection.expect_started("t1").await;
    let result = connection.expect_result("t1").await;
    assert_failed(&result, reason::ERROR);
    assert_eq!(device.marker(), None);
    assert_eq!(platform.update_calls(), Vec::<&str>::new());
    client.stop().await;
}

// ------------------------------------------------------------ refused updates

/// Sends a `client.update` for `NEW` with `payload` against a server set up
/// by `serve`, and returns the result. Checks that nothing was replaced.
async fn refused(keys: &Keys, payload: Value, serve: impl FnOnce(&UpdateServer)) -> (JobResult, Vec<String>) {
    let device = Device::new();
    let updates = UpdateServer::start().await;
    serve(&updates);
    let coordinator = FakeCoordinator::with_updates(&updates).await;
    let (client, platform) = device.start(&coordinator, RUNNING, keys);
    let (mut connection, _) = coordinator.accept().await;
    connection.send_job("t1", "client.update", payload).await;
    connection.expect_started("t1").await;
    let result = connection.expect_result("t1").await;
    device.assert_untouched(&platform);
    // Finished, not awaiting a restart.
    assert_ne!(device.ledger_entry("t1")["result"], Value::Null);
    client.stop().await;
    (result, updates.requests())
}

#[tokio::test]
async fn a_download_with_the_wrong_hash_is_refused() {
    let keys = keys();
    let tampered = b"fake executable of version 1.5.0, a little LONGER";
    let (result, _) = refused(&keys, payload(NEW, NEW_EXE), |updates| publish(updates, &keys, tampered)).await;
    assert_failed(&result, reason::BAD_HASH);
}

#[tokio::test]
async fn a_download_longer_than_announced_is_refused() {
    let keys = keys();
    let mut longer = NEW_EXE.to_vec();
    longer.extend_from_slice(b" and then some");
    let (result, requests) = refused(&keys, payload(NEW, NEW_EXE), |updates| publish(updates, &keys, &longer)).await;
    assert_failed(&result, reason::BAD_HASH);
    assert_eq!(result.message.as_deref(), Some(&*format!("the download is larger than the announced {} bytes", NEW_EXE.len())));
    // Refused before the signature was even looked at.
    assert_eq!(requests, [artifact_path(NEW)]);
}

#[tokio::test]
async fn a_download_shorter_than_announced_fails() {
    let keys = keys();
    let shorter = &NEW_EXE[..10];
    let (result, _) = refused(&keys, payload(NEW, NEW_EXE), |updates| publish(updates, &keys, shorter)).await;
    assert_failed(&result, reason::DOWNLOAD_FAILED);
}

#[tokio::test]
async fn a_signature_by_another_key_is_refused() {
    let keys = keys();
    let impostor = self::keys();
    let (result, _) = refused(&keys, payload(NEW, NEW_EXE), |updates| publish(updates, &impostor, NEW_EXE)).await;
    assert_failed(&result, reason::BAD_SIGNATURE);
}

#[tokio::test]
async fn a_signature_for_another_version_or_target_is_refused() {
    let keys = keys();
    for (version, target) in [("1.4.9", target()), (NEW, "linux-x86_64")] {
        let (result, _) = refused(&keys, payload(NEW, NEW_EXE), |updates| {
            updates.serve(&artifact_path(NEW), Served::Bytes(NEW_EXE.to_vec()));
            let moved = signature(&keys, version, target, NEW_EXE);
            updates.serve(&format!("{}.sig", artifact_path(NEW)), Served::Bytes(moved.into_bytes()));
        })
        .await;
        assert_failed(&result, reason::BAD_SIGNATURE);
    }
}

#[tokio::test]
async fn a_missing_artifact_or_signature_is_a_failed_download() {
    let keys = keys();
    let (result, _) = refused(&keys, payload(NEW, NEW_EXE), |_| {}).await;
    assert_failed(&result, reason::DOWNLOAD_FAILED);
    assert!(result.message.as_deref().unwrap().contains("404"), "{result:?}");

    let (result, _) = refused(&keys, payload(NEW, NEW_EXE), |updates| {
        updates.serve(&artifact_path(NEW), Served::Bytes(NEW_EXE.to_vec()));
    })
    .await;
    assert_failed(&result, reason::DOWNLOAD_FAILED);
    assert!(result.message.as_deref().unwrap().contains("signature"), "{result:?}");
}

#[tokio::test]
async fn a_coordinator_that_cannot_be_reached_for_the_download_fails_it() {
    let keys = keys();
    let device = Device::new();
    let updates = UpdateServer::start().await;
    publish(&updates, &keys, NEW_EXE);
    let coordinator = FakeCoordinator::with_updates(&updates).await;
    let (client, platform) = device.start(&coordinator, RUNNING, &keys);
    let (mut connection, _) = coordinator.accept().await;
    // The session stays up, but the port refuses new connections.
    coordinator.stop_listening();
    connection.send_job("t1", "client.update", payload(NEW, NEW_EXE)).await;
    connection.expect_started("t1").await;
    let result = connection.expect_result("t1").await;
    assert_failed(&result, reason::DOWNLOAD_FAILED);
    device.assert_untouched(&platform);
    client.stop().await;
}

#[tokio::test]
async fn an_older_version_is_refused_unless_the_job_allows_a_downgrade() {
    let keys = keys();
    let older = "1.3.9";
    let (result, requests) = refused(&keys, payload(older, NEW_EXE), |_| {}).await;
    assert_failed(&result, reason::DOWNGRADE_REFUSED);
    assert!(requests.is_empty());

    // Allowed: installed like any update.
    let device = Device::new();
    let updates = UpdateServer::start().await;
    updates.serve(&artifact_path(older), Served::Bytes(NEW_EXE.to_vec()));
    let sig = signature(&keys, older, target(), NEW_EXE);
    updates.serve(&format!("{}.sig", artifact_path(older)), Served::Bytes(sig.into_bytes()));
    let coordinator = FakeCoordinator::with_updates(&updates).await;
    let (client, platform) = device.start(&coordinator, RUNNING, &keys);
    let (mut connection, _) = coordinator.accept().await;
    let mut downgrade = payload(older, NEW_EXE);
    downgrade["downgrade"] = json!(true);
    connection.send_job("t1", "client.update", downgrade).await;
    connection.expect_started("t1").await;
    connection
        .expect_output("t1", 1, OutputStream::Stdout, "restarting to complete the update to 1.3.9\n")
        .await;
    stops_by_itself(client).await;
    assert_eq!(platform.update_calls(), ["swap", "restart"]);
    assert_eq!(device.marker().unwrap().new_version, older);
}

#[tokio::test]
async fn the_running_version_succeeds_without_a_download() {
    let keys = keys();
    let device = Device::new();
    let updates = UpdateServer::start().await;
    let coordinator = FakeCoordinator::with_updates(&updates).await;
    let (client, platform) = device.start(&coordinator, RUNNING, &keys);
    let (mut connection, _) = coordinator.accept().await;
    connection.send_job("t1", "client.update", payload(RUNNING, OLD_EXE)).await;
    connection.expect_started("t1").await;
    let result = connection.expect_result("t1").await;
    assert_eq!(result.status, JobStatus::Succeeded, "{result:?}");
    assert_eq!(result.message.as_deref(), Some("already on version 1.4.0"));
    assert!(updates.requests().is_empty());
    device.assert_untouched(&platform);
    client.stop().await;
}

#[tokio::test]
async fn a_build_without_an_update_key_refuses_every_update() {
    let mut keys = keys();
    keys.public = String::new();
    let (result, requests) = refused(&keys, payload(NEW, NEW_EXE), |_| {}).await;
    assert_failed(&result, reason::UNSUPPORTED);
    assert_eq!(result.message.as_deref(), Some("this client build has no update key"));
    assert!(requests.is_empty());
}

#[tokio::test]
async fn an_update_for_another_target_is_unsupported() {
    let keys = keys();
    let mut other = payload(NEW, NEW_EXE);
    other["target"] = json!("linux-x86_64");
    let (result, requests) = refused(&keys, other, |_| {}).await;
    assert_failed(&result, reason::UNSUPPORTED);
    assert!(requests.is_empty());
    assert_eq!(target(), TARGET_WINDOWS_X86_64);
}

#[tokio::test]
async fn a_malformed_update_payload_is_invalid() {
    let keys = keys();
    for bad in [
        json!({"version": "1.5", "target": target(), "size": 1, "sha256": update::sha256_hex(b"x")}),
        json!({"version": NEW, "target": target(), "size": 1, "sha256": "not-a-digest"}),
        json!({"version": NEW}),
    ] {
        let (result, _) = refused(&keys, bad, |_| {}).await;
        assert_failed(&result, reason::INVALID_PAYLOAD);
    }
}

// ------------------------------------------------------------ cancelling

#[tokio::test]
async fn an_update_cancelled_while_downloading_leaves_nothing_behind() {
    let keys = keys();
    let device = Device::new();
    let updates = UpdateServer::start().await;
    updates.serve(&artifact_path(NEW), Served::Stall(NEW_EXE.to_vec(), 8));
    let coordinator = FakeCoordinator::with_updates(&updates).await;
    let (client, platform) = device.start(&coordinator, RUNNING, &keys);
    let (mut connection, _) = coordinator.accept().await;

    connection.send_job("t1", "client.update", payload(NEW, NEW_EXE)).await;
    connection.expect_started("t1").await;
    wait_for("the download to begin", || device.download().exists()).await;
    connection.cancel("t1").await;
    let result = connection.expect_result("t1").await;
    assert_failed(&result, reason::CANCELLED);
    device.assert_untouched(&platform);
    updates.release();
    client.stop().await;
}
