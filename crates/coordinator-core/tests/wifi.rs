//! `wifi.join` jobs: validation, password redaction and unconfirmed handoffs.

mod common;

use std::time::Duration;

use common::{wait_until, FakeClient, TestServer};
use protocol::{ClientMessage, JobResult, JobStatus};
use serde_json::{json, Value};

const MASK: &str = "********";
const HANDOFF_MESSAGE: &str = "The device disconnected before confirming the new \
network. Check the device or its destination coordinator before retrying.";

async fn create_wifi_job(server: &TestServer, payload: Value, targets: Value) -> (u16, Value) {
    server
        .post("/api/v1/jobs", json!({"kind": "wifi.join", "payload": payload, "targets": targets}))
        .await
}

fn one_device(id: &str) -> Value {
    json!({"type": "devices", "device_ids": [id]})
}

/// A `wifi.join` job for one device, already started on the client.
async fn started_wifi_job(server: &TestServer, client: &mut FakeClient, device_id: &str) -> (String, String) {
    let (status, body) =
        create_wifi_job(server, json!({"ssid": "fake-office", "password": "fake-passphrase"}), one_device(device_id)).await;
    assert_eq!(status, 201, "{body}");
    let job_id = body["job"]["id"].as_str().unwrap().to_owned();
    let (target_id, ..) = client.expect_job().await;
    client.started(&target_id).await;
    server.wait_for_target_state(&job_id, "running").await;
    (job_id, target_id)
}

#[tokio::test]
async fn a_wifi_join_job_is_dispatched_with_its_full_payload() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let mut client = FakeClient::connect(&server, &key, "device-a").await;

    let payload = json!({
        "ssid": "fake-office",
        "password": "fake-passphrase",
        "coordinator_url": "wss://coordinator.example.test/client",
    });
    let (status, body) = create_wifi_job(&server, payload.clone(), one_device("device-a")).await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["job"]["kind"], "wifi.join");

    let (_, job_id, kind, received) = client.expect_job().await;
    assert_eq!(job_id, body["job"]["id"].as_str().unwrap());
    assert_eq!(kind, "wifi.join");
    assert_eq!(received, payload, "the device gets the real password");
}

#[tokio::test]
async fn an_open_network_needs_neither_password_nor_coordinator_url() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let mut client = FakeClient::connect(&server, &key, "device-a").await;

    let (status, body) = create_wifi_job(&server, json!({"ssid": "fake-open"}), one_device("device-a")).await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["job"]["payload"], json!({"ssid": "fake-open"}));
    assert_eq!(client.expect_job().await.3, json!({"ssid": "fake-open"}));
}

#[tokio::test]
async fn wifi_join_payloads_are_validated() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let _client = FakeClient::connect(&server, &key, "device-a").await;

    let long_url = format!("wss://example.test/{}", "a".repeat(2048));
    let rejected = [
        json!({}),
        json!({"ssid": ""}),
        json!({"ssid": "x".repeat(33)}),
        // 17 two-byte characters are 34 bytes: the limit is in bytes.
        json!({"ssid": "\u{e9}".repeat(17)}),
        json!({"ssid": 5}),
        json!({"ssid": "fake-office", "password": "short"}),
        json!({"ssid": "fake-office", "password": "p".repeat(64)}),
        json!({"ssid": "fake-office", "password": 12345678}),
        json!({"ssid": "fake-office", "coordinator_url": "http://coordinator.example.test"}),
        json!({"ssid": "fake-office", "coordinator_url": "coordinator.example.test"}),
        json!({"ssid": "fake-office", "coordinator_url": long_url}),
    ];
    for payload in rejected {
        let (status, body) = create_wifi_job(&server, payload.clone(), one_device("device-a")).await;
        assert_eq!(status, 400, "{payload} should be refused");
        assert_eq!(body["error"]["code"], "invalid_payload", "{payload}");
        assert!(!body["error"]["message"].as_str().unwrap().is_empty());
    }
    let (_, body) = server.get("/api/v1/jobs").await;
    assert!(body["jobs"].as_array().unwrap().is_empty(), "nothing was created");

    // The limits themselves are fine.
    let accepted = [
        json!({"ssid": "x"}),
        json!({"ssid": "x".repeat(32)}),
        json!({"ssid": "fake-office", "password": "p".repeat(8)}),
        json!({"ssid": "fake-office", "password": "p".repeat(63)}),
        json!({"ssid": "fake-office", "coordinator_url": "ws://192.0.2.1:8080/client"}),
        json!({"ssid": "fake-office", "coordinator_url": format!("wss://{}", "a".repeat(2048 - 6))}),
    ];
    for payload in accepted {
        let (status, body) = create_wifi_job(&server, payload.clone(), one_device("device-a")).await;
        assert_eq!(status, 201, "{payload} should be accepted: {body}");
    }
}

#[tokio::test]
async fn the_api_never_shows_the_password_but_the_device_still_gets_it() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let mut client = FakeClient::connect(&server, &key, "device-a").await;

    let payload = json!({"ssid": "fake-office", "password": "fake-passphrase"});
    let (status, body) = create_wifi_job(&server, payload.clone(), one_device("device-a")).await;
    assert_eq!(status, 201, "{body}");
    let masked = json!({"ssid": "fake-office", "password": MASK});
    assert_eq!(body["job"]["payload"], masked);
    let job_id = body["job"]["id"].as_str().unwrap();

    let (_, listed) = server.get("/api/v1/jobs").await;
    assert_eq!(listed["jobs"][0]["payload"], masked);
    assert_eq!(server.job(job_id).await["payload"], masked);
    for view in [listed.to_string(), server.job(job_id).await.to_string(), body.to_string()] {
        assert!(!view.contains("fake-passphrase"), "password leaked in {view}");
    }

    assert_eq!(client.expect_job().await.3, payload);

    // Redelivery after a reconnect carries the real password too.
    client.disconnect().await;
    let mut client = FakeClient::connect(&server, &key, "device-a").await;
    assert_eq!(client.expect_job().await.3, payload);
}

#[tokio::test]
async fn a_running_wifi_join_whose_device_never_returns_is_unconfirmed() {
    let server = TestServer::start_with(|c| c.handoff_grace = Duration::from_millis(200)).await;
    let key = server.new_key();
    let mut client = FakeClient::connect(&server, &key, "device-a").await;
    let (job_id, _) = started_wifi_job(&server, &mut client, "device-a").await;

    client.disconnect().await;
    let target = server.wait_for_target_state(&job_id, "failed").await;
    assert!(target["exit_code"].is_null());
    assert_eq!(target["reason"], "handoff_unconfirmed");
    assert_eq!(target["message"], HANDOFF_MESSAGE);
    assert!(target["finished_at"].as_i64().unwrap() >= target["started_at"].as_i64().unwrap());
}

#[tokio::test]
async fn a_device_that_comes_back_and_reports_failure_stays_failed() {
    // A grace long enough that reconnecting cannot lose the race on a slow
    // machine, short enough to wait out below.
    let server = TestServer::start_with(|c| c.handoff_grace = Duration::from_millis(600)).await;
    let key = server.new_key();
    let mut client = FakeClient::connect(&server, &key, "device-a").await;
    let (job_id, target_id) = started_wifi_job(&server, &mut client, "device-a").await;

    client.disconnect().await;
    assert!(wait_until(|| async { !server.is_online("device-a").await }).await);
    let mut client = FakeClient::connect(&server, &key, "device-a").await;
    // The unresolved target is delivered again; the join failed.
    assert_eq!(client.expect_job().await.0, target_id);
    client
        .send(ClientMessage::JobResult(JobResult {
            job_target_id: target_id.clone(),
            status: JobStatus::Failed,
            exit_code: None,
            reason: Some("wifi_failed".into()),
            message: Some("could not join fake-office".into()),
        }))
        .await;
    client.expect_result_ack(&target_id).await;
    server.wait_for_target_state(&job_id, "failed").await;

    // Well past the grace, with the device offline again.
    client.disconnect().await;
    tokio::time::sleep(Duration::from_millis(900)).await;
    let target = server.single_target(&job_id).await;
    assert_eq!(target["state"], "failed");
    assert_eq!(target["message"], "could not join fake-office");
}

#[tokio::test]
async fn a_wifi_join_whose_device_stays_online_is_left_running() {
    let server = TestServer::start_with(|c| c.handoff_grace = Duration::from_millis(200)).await;
    let key = server.new_key();
    let mut client = FakeClient::connect(&server, &key, "device-a").await;
    let (job_id, _) = started_wifi_job(&server, &mut client, "device-a").await;

    // Stays connected for several grace periods and sweeps.
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(server.single_target(&job_id).await["state"], "running");
}

#[tokio::test]
async fn only_running_wifi_join_targets_are_marked_unconfirmed() {
    let server = TestServer::start_with(|c| c.handoff_grace = Duration::from_millis(200)).await;
    let key = server.new_key();
    let mut script_device = FakeClient::connect(&server, &key, "device-script").await;
    let mut sent_device = FakeClient::connect(&server, &key, "device-sent").await;
    let mut wifi_device = FakeClient::connect(&server, &key, "device-wifi").await;

    // A running script.run on a device that goes away.
    let job = server.create_script_job(one_device("device-script")).await;
    let script_job = job["id"].as_str().unwrap().to_owned();
    let (target_id, ..) = script_device.expect_job().await;
    script_device.started(&target_id).await;
    server.wait_for_target_state(&script_job, "running").await;

    // A wifi.join that was delivered but never started.
    let (status, body) =
        create_wifi_job(&server, json!({"ssid": "fake-office"}), one_device("device-sent")).await;
    assert_eq!(status, 201, "{body}");
    let sent_job = body["job"]["id"].as_str().unwrap().to_owned();
    sent_device.expect_job().await;
    server.wait_for_target_state(&sent_job, "sent").await;

    // A running wifi.join: the control that the sweep does act.
    let (wifi_job, _) = started_wifi_job(&server, &mut wifi_device, "device-wifi").await;

    script_device.disconnect().await;
    sent_device.disconnect().await;
    wifi_device.disconnect().await;
    server.wait_for_target_state(&wifi_job, "failed").await;
    // The sweep has now run at least once after the grace for the others too.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(server.single_target(&script_job).await["state"], "running");
    assert_eq!(server.single_target(&sent_job).await["state"], "sent");
}

#[tokio::test]
async fn a_late_wifi_result_resolves_an_unconfirmed_handoff_without_repeating_the_join() {
    let server = TestServer::start_with(|c| c.handoff_grace = Duration::from_millis(100)).await;
    let key = server.new_key();
    let mut client = FakeClient::connect(&server, &key, "device-a").await;
    let (job_id, target_id) = started_wifi_job(&server, &mut client, "device-a").await;
    client.disconnect().await;
    let unconfirmed = server.wait_for_target_state(&job_id, "failed").await;
    assert_eq!(unconfirmed["reason"], "handoff_unconfirmed");

    let mut client = FakeClient::connect(&server, &key, "device-a").await;
    assert!(client.try_recv(Duration::from_millis(100)).await.is_none());
    client.result(&target_id, 0).await;
    client.expect_result_ack(&target_id).await;
    let confirmed = server.wait_for_target_state(&job_id, "succeeded").await;
    assert_eq!(confirmed["exit_code"], 0);
    assert!(confirmed["reason"].is_null());
    assert!(confirmed["message"].is_null());

    // Once confirmed it has the same immutable result semantics as other jobs.
    client.result(&target_id, 1).await;
    client.expect_result_ack(&target_id).await;
    assert_eq!(server.single_target(&job_id).await, confirmed);
}
