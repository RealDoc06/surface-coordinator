//! The automatic, staged rollout of the newest cached client release: one
//! canary device first, the rest once a device has been updated, halted when
//! the canary fails. Fake clients report their version in `hello`; a device
//! "now on the new version" is the fake client reconnecting with it.

mod common;

use std::time::Duration;

use common::{connect_as, wait_until, FakeClient, Keys, TestServer};
use serde_json::{json, Value};

/// Long enough for several sweeps (every 50 ms in tests) to have run.
const QUIET: Duration = Duration::from_millis(300);

async fn start(keys: &Keys, auto_update: bool) -> TestServer {
    TestServer::start_with(|config| {
        config.update_public_key = Some(keys.public.clone());
        config.auto_update = auto_update;
    })
    .await
}

async fn rollout(server: &TestServer) -> Value {
    server.updates().await["rollout"].clone()
}

/// Waits until `Updates.rollout` has these fields (others are not compared).
async fn wait_for_rollout(server: &TestServer, expected: Value) -> Value {
    let matches = |rollout: &Value| expected.as_object().unwrap().iter().all(|(key, value)| rollout[key] == *value);
    let reached = wait_until(|| async { matches(&rollout(server).await) }).await;
    let rollout = rollout(server).await;
    assert!(reached, "rollout never reached {expected}; last seen: {rollout}");
    rollout
}

/// Waits for a `client.update` of `version` and returns (job_target_id, job_id).
async fn expect_update(client: &mut FakeClient, version: &str) -> (String, String) {
    let (target, job_id, kind, payload) = client.expect_job().await;
    assert_eq!(kind, "client.update");
    assert_eq!(payload["version"], version);
    assert_eq!(payload["downgrade"], false);
    (target, job_id)
}

async fn report(client: &mut FakeClient, target: &str, exit_code: i32) {
    client.started(target).await;
    client.result(target, exit_code).await;
    client.expect_result_ack(target).await;
}

/// The device restarts on the new version.
async fn reconnect_on(server: &TestServer, client: FakeClient, key: &coordinator_core::EnrollmentKey, version: &str) -> FakeClient {
    let id = client.device_id.clone();
    client.disconnect().await;
    connect_as(server, key, &id, version).await
}

async fn update_jobs(server: &TestServer) -> Vec<Value> {
    let (_, body) = server.get("/api/v1/jobs").await;
    body["jobs"].as_array().unwrap().iter().filter(|job| job["kind"] == "client.update").cloned().collect()
}

#[tokio::test]
async fn with_auto_update_off_an_upload_updates_nobody() {
    let keys = Keys::generate();
    let server = start(&keys, false).await;
    let key = server.new_key();
    let mut old = connect_as(&server, &key, "device-old", "1.0.0").await;

    let body = server.upload_release(&keys, "1.1.0").await;
    assert_eq!(body["auto_update"], false);
    assert!(body["rollout"].is_null(), "{body}");
    assert!(old.try_recv(QUIET).await.is_none());
    assert!(update_jobs(&server).await.is_empty());
}

#[tokio::test]
async fn the_canary_goes_first_and_the_others_follow_its_success() {
    let keys = Keys::generate();
    let server = start(&keys, true).await;
    let key = server.new_key();
    let mut a = connect_as(&server, &key, "device-a", "1.0.0").await;
    let mut c = connect_as(&server, &key, "device-c", "1.0.0").await;
    let mut b = connect_as(&server, &key, "device-b", "1.0.0").await;

    let body = server.upload_release(&keys, "1.1.0").await;
    assert_eq!(body["auto_update"], true);
    assert_eq!(
        body["rollout"],
        json!({"version": "1.1.0", "state": "canary", "canary_device_id": "device-a", "updated": 0, "pending": 3, "failed": 0})
    );

    // The lowest id goes first, alone, as the rollout's own job.
    let (canary_target, canary_job) = expect_update(&mut a, "1.1.0").await;
    let job = server.job(&canary_job).await;
    assert_eq!(job["created_by"], "auto-update");
    assert_eq!(job["targets"].as_array().unwrap().len(), 1);
    // The others wait while the canary's attempt is unresolved.
    assert!(b.try_recv(QUIET).await.is_none());
    assert!(c.try_recv(Duration::ZERO).await.is_none());

    report(&mut a, &canary_target, 0).await;
    let (b_target, b_job) = expect_update(&mut b, "1.1.0").await;
    let (c_target, c_job) = expect_update(&mut c, "1.1.0").await;
    assert_eq!(b_job, c_job, "the rest get one job");
    assert_eq!(server.job(&b_job).await["created_by"], "auto-update");
    wait_for_rollout(&server, json!({"state": "rolling", "canary_device_id": "device-a", "updated": 0, "pending": 2})).await;

    let _a = reconnect_on(&server, a, &key, "1.1.0").await;
    wait_for_rollout(&server, json!({"state": "rolling", "updated": 1, "pending": 2})).await;
    report(&mut b, &b_target, 0).await;
    report(&mut c, &c_target, 0).await;
    let _b = reconnect_on(&server, b, &key, "1.1.0").await;
    let _c = reconnect_on(&server, c, &key, "1.1.0").await;
    let done = wait_for_rollout(&server, json!({"state": "done", "updated": 3})).await;
    assert_eq!(done, json!({"version": "1.1.0", "state": "done", "canary_device_id": "device-a", "updated": 3, "pending": 0, "failed": 0}));

    tokio::time::sleep(QUIET).await;
    assert_eq!(update_jobs(&server).await.len(), 2);
}

#[tokio::test]
async fn a_failed_canary_halts_the_rollout_until_a_device_is_updated_by_hand() {
    let keys = Keys::generate();
    let server = start(&keys, true).await;
    let key = server.new_key();
    let mut a = connect_as(&server, &key, "device-a", "1.0.0").await;
    let mut b = connect_as(&server, &key, "device-b", "1.0.0").await;
    let mut c = connect_as(&server, &key, "device-c", "1.0.0").await;
    server.upload_release(&keys, "1.1.0").await;

    let (canary_target, _) = expect_update(&mut a, "1.1.0").await;
    report(&mut a, &canary_target, 1).await;
    wait_for_rollout(
        &server,
        json!({"version": "1.1.0", "state": "halted", "canary_device_id": "device-a", "updated": 0, "pending": 2, "failed": 1}),
    )
    .await;
    assert!(b.try_recv(QUIET).await.is_none());
    assert!(c.try_recv(Duration::ZERO).await.is_none());

    // An operator updates device-b; its success lets the rest follow.
    let (status, body) =
        server.create_job("client.update", json!({"version": "1.1.0"}), json!({"type": "devices", "device_ids": ["device-b"]})).await;
    assert_eq!(status, 201, "{body}");
    let (b_target, _) = expect_update(&mut b, "1.1.0").await;
    report(&mut b, &b_target, 0).await;
    let (_, c_job) = expect_update(&mut c, "1.1.0").await;
    let job = server.job(&c_job).await;
    assert_eq!(job["created_by"], "auto-update");
    assert_eq!(job["targets"].as_array().unwrap().len(), 1, "the failed canary is not tried again");
    wait_for_rollout(&server, json!({"state": "rolling", "pending": 1, "failed": 1})).await;
    assert!(a.try_recv(QUIET).await.is_none());
}

#[tokio::test]
async fn the_rollout_skips_devices_on_the_version_picks_up_late_ones_and_never_tries_twice() {
    let keys = Keys::generate();
    let server = start(&keys, false).await;
    let key = server.new_key();
    let mut current = connect_as(&server, &key, "device-current", "1.1.0").await;
    let mut newer = connect_as(&server, &key, "device-newer", "2.0.0").await;
    let mut odd = connect_as(&server, &key, "device-odd", "0.0.0-test").await;
    let mut old = connect_as(&server, &key, "device-old", "1.0.0").await;
    // Outdated with the lowest id, but offline: not the canary.
    connect_as(&server, &key, "device-late", "1.0.0").await.disconnect().await;
    assert!(wait_until(|| async { !server.is_online("device-late").await }).await);
    server.upload_release(&keys, "1.1.0").await;
    assert!(old.try_recv(QUIET).await.is_none());

    // Turning auto-update on rolls out what is already cached.
    let (status, body) = server.post("/api/v1/updates/auto", json!({"enabled": true})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["auto_update"], true);
    assert_eq!(
        body["rollout"],
        json!({"version": "1.1.0", "state": "canary", "canary_device_id": "device-old", "updated": 1, "pending": 2, "failed": 0})
    );
    let (canary_target, _) = expect_update(&mut old, "1.1.0").await;
    for client in [&mut current, &mut newer, &mut odd] {
        assert!(client.try_recv(Duration::from_millis(100)).await.is_none(), "{} was targeted", client.device_id);
    }
    report(&mut old, &canary_target, 0).await;
    let _old = reconnect_on(&server, old, &key, "1.1.0").await;
    wait_for_rollout(&server, json!({"state": "rolling", "updated": 2, "pending": 1})).await;

    // The offline device is picked up when it comes online.
    let mut late = connect_as(&server, &key, "device-late", "1.0.0").await;
    let (late_target, late_job) = expect_update(&mut late, "1.1.0").await;
    assert_eq!(server.job(&late_job).await["created_by"], "auto-update");
    report(&mut late, &late_target, 1).await;
    wait_for_rollout(&server, json!({"state": "done", "updated": 2, "pending": 0, "failed": 1})).await;

    // One automatic attempt per device and version, however often it returns.
    let mut late = reconnect_on(&server, late, &key, "1.0.0").await;
    assert!(late.try_recv(QUIET).await.is_none());
    assert_eq!(update_jobs(&server).await.len(), 2);
}

#[tokio::test]
async fn the_auto_update_setting_is_stored_and_the_configuration_is_only_its_first_default() {
    let keys = Keys::generate();
    let mut server = start(&keys, true).await;
    assert_eq!(server.updates().await["auto_update"], true);
    assert!(server.updates().await["rollout"].is_null(), "nothing cached");

    let (status, body) = server.post("/api/v1/updates/auto", json!({"enabled": false})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["auto_update"], false);

    server.restart_with(|config| config.auto_update = true).await;
    assert_eq!(server.updates().await["auto_update"], false);
}
