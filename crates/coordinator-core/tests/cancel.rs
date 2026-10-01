//! Cancelling jobs: queued, in flight and finished targets, online and
//! offline devices.

mod common;

use std::time::Duration;

use common::{wait_until, FakeClient, TestServer, API_TOKEN};
use serde_json::{json, Value};

fn one_device(id: &str) -> Value {
    json!({"type": "devices", "device_ids": [id]})
}

/// Connects a device and takes it offline again, so jobs for it queue up.
async fn offline_device(server: &TestServer, key: &coordinator_core::EnrollmentKey, id: &str) {
    FakeClient::connect(server, key, id).await.disconnect().await;
    assert!(wait_until(|| async { !server.is_online(id).await }).await);
}

fn states(job: &Value) -> Vec<String> {
    job["targets"].as_array().unwrap().iter().map(|t| t["state"].as_str().unwrap().to_owned()).collect()
}

#[tokio::test]
async fn cancelling_a_job_that_is_still_queued_fails_its_targets_at_once() {
    let server = TestServer::start().await;
    let key = server.new_key();
    offline_device(&server, &key, "device-a").await;
    offline_device(&server, &key, "device-b").await;
    let job = server.create_script_job(json!({"type": "all"})).await;
    let other = server.create_script_job(json!({"type": "all"})).await;
    let job_id = job["id"].as_str().unwrap();

    let (status, body) = server.cancel(job_id).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["job"]["id"], job_id);
    for target in body["job"]["targets"].as_array().unwrap() {
        assert_eq!(target["state"], "failed");
        assert_eq!(target["reason"], "cancelled");
        assert!(target["exit_code"].is_null());
        assert!(target["finished_at"].as_i64().is_some());
        assert!(target.get("output").is_none(), "the response carries no output");
    }
    assert_eq!(states(&server.job(job_id).await), ["failed", "failed"]);

    // Other jobs are not affected.
    assert_eq!(states(&server.job(other["id"].as_str().unwrap()).await), ["queued", "queued"]);

    // Cancelling again changes nothing.
    let before = server.job(job_id).await;
    let (status, body) = server.cancel(job_id).await;
    assert_eq!(status, 200);
    assert_eq!(states(&body["job"]), ["failed", "failed"]);
    assert_eq!(server.job(job_id).await, before);
}

#[tokio::test]
async fn a_cancelled_queued_target_is_not_delivered_when_the_device_returns() {
    let server = TestServer::start().await;
    let key = server.new_key();
    offline_device(&server, &key, "device-a").await;
    let job = server.create_script_job(one_device("device-a")).await;
    let job_id = job["id"].as_str().unwrap();
    assert_eq!(server.cancel(job_id).await.0, 200);

    let mut client = FakeClient::connect(&server, &key, "device-a").await;
    assert!(client.try_recv(Duration::from_millis(200)).await.is_none());
    assert_eq!(server.single_target(job_id).await["reason"], "cancelled");
}

#[tokio::test]
async fn a_target_in_flight_on_an_online_device_is_told_to_stop_and_changes_when_the_device_confirms() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let mut client = FakeClient::connect(&server, &key, "device-a").await;
    let sent = server.create_script_job(one_device("device-a")).await;
    let running = server.create_script_job(one_device("device-a")).await;
    let (sent_target, ..) = client.expect_job().await;
    let (running_target, ..) = client.expect_job().await;
    client.started(&running_target).await;
    client.output(&running_target, 1, "partial\n").await;
    let sent_id = sent["id"].as_str().unwrap();
    let running_id = running["id"].as_str().unwrap();
    server.wait_for_target_state(running_id, "running").await;

    let (status, body) = server.cancel(sent_id).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["job"]["targets"][0]["state"], "sent", "unchanged until the device answers");
    assert_eq!(client.expect_cancel().await, sent_target);
    let (status, body) = server.cancel(running_id).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["job"]["targets"][0]["state"], "running");
    assert_eq!(client.expect_cancel().await, running_target);

    // Nothing moved yet.
    assert_eq!(server.single_target(sent_id).await["state"], "sent");
    assert_eq!(server.single_target(running_id).await["state"], "running");

    client.result_cancelled(&sent_target).await;
    client.expect_result_ack(&sent_target).await;
    client.result_cancelled(&running_target).await;
    client.expect_result_ack(&running_target).await;
    for (job_id, output) in [(sent_id, ""), (running_id, "partial\n")] {
        let target = server.wait_for_target_state(job_id, "failed").await;
        assert_eq!(target["reason"], "cancelled");
        assert_eq!(target["message"], "cancelled by the operator");
        assert!(target["finished_at"].as_i64().is_some());
        assert_eq!(target["output"], output, "output produced before the cancel is kept");
    }
}

#[tokio::test]
async fn only_targets_that_are_not_finished_are_cancelled() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let mut a = FakeClient::connect(&server, &key, "device-a").await;
    let mut b = FakeClient::connect(&server, &key, "device-b").await;
    offline_device(&server, &key, "device-c").await;
    let job = server.create_script_job(json!({"type": "all"})).await;
    let job_id = job["id"].as_str().unwrap();
    let (a_target, ..) = a.expect_job().await;
    let (b_target, ..) = b.expect_job().await;
    a.started(&a_target).await;
    a.result(&a_target, 0).await;
    a.expect_result_ack(&a_target).await;
    b.started(&b_target).await;
    b.result(&b_target, 3).await;
    b.expect_result_ack(&b_target).await;

    let (status, body) = server.cancel(job_id).await;
    assert_eq!(status, 200, "{body}");
    let targets = body["job"]["targets"].as_array().unwrap();
    let find = |device: &str| targets.iter().find(|t| t["device_id"] == device).unwrap();
    assert_eq!(find("device-a")["state"], "succeeded");
    assert_eq!(find("device-a")["exit_code"], 0);
    assert_eq!(find("device-b")["state"], "failed");
    assert_eq!(find("device-b")["reason"], "nonzero_exit");
    assert_eq!(find("device-b")["exit_code"], 3);
    assert_eq!(find("device-c")["state"], "failed");
    assert_eq!(find("device-c")["reason"], "cancelled");

    // Finished devices are not sent anything.
    assert!(a.try_recv(Duration::from_millis(200)).await.is_none());
    assert!(b.try_recv(Duration::from_millis(200)).await.is_none());
}

#[tokio::test]
async fn a_running_target_whose_device_is_offline_is_cancelled_at_once() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let mut client = FakeClient::connect(&server, &key, "device-a").await;
    let job = server.create_script_job(one_device("device-a")).await;
    let job_id = job["id"].as_str().unwrap();
    let (target_id, ..) = client.expect_job().await;
    client.started(&target_id).await;
    server.wait_for_target_state(job_id, "running").await;
    client.disconnect().await;
    assert!(wait_until(|| async { !server.is_online("device-a").await }).await);

    let (status, body) = server.cancel(job_id).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["job"]["targets"][0]["state"], "failed");
    assert_eq!(body["job"]["targets"][0]["reason"], "cancelled");
    assert!(body["job"]["targets"][0]["finished_at"].as_i64().is_some());

    // The target is finished, so the device is not given it again.
    let mut client = FakeClient::connect(&server, &key, "device-a").await;
    assert!(client.try_recv(Duration::from_millis(200)).await.is_none());
}

#[tokio::test]
async fn a_late_result_does_not_overwrite_a_target_cancelled_while_its_device_was_offline() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let mut client = FakeClient::connect(&server, &key, "device-a").await;
    let job = server.create_script_job(one_device("device-a")).await;
    let job_id = job["id"].as_str().unwrap();
    let (target_id, ..) = client.expect_job().await;
    client.started(&target_id).await;
    server.wait_for_target_state(job_id, "running").await;
    client.disconnect().await;
    assert!(wait_until(|| async { !server.is_online("device-a").await }).await);
    assert_eq!(server.cancel(job_id).await.0, 200);

    // The script kept running on the device and finishes after it reconnects.
    let mut client = FakeClient::connect(&server, &key, "device-a").await;
    client.output(&target_id, 1, "too late\n").await;
    client.result(&target_id, 0).await;
    client.expect_result_ack(&target_id).await;
    let target = server.single_target(job_id).await;
    assert_eq!(target["state"], "failed");
    assert_eq!(target["reason"], "cancelled");
    assert!(target["exit_code"].is_null());
    assert_eq!(target["output"], "");
}

#[tokio::test]
async fn an_unknown_job_cannot_be_cancelled() {
    let server = TestServer::start().await;
    let (status, body) = server.cancel("no-such-job").await;
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "not_found");
}

#[tokio::test]
async fn cancelling_needs_authentication() {
    let server = TestServer::start().await;
    let key = server.new_key();
    offline_device(&server, &key, "device-a").await;
    let job = server.create_script_job(json!({"type": "all"})).await;
    let job_id = job["id"].as_str().unwrap();

    let response = TestServer::client()
        .post(server.url(&format!("/api/v1/jobs/{job_id}/cancel")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    assert_eq!(states(&server.job(job_id).await), ["queued"]);

    let response = TestServer::client()
        .post(server.url(&format!("/api/v1/jobs/{job_id}/cancel")))
        .bearer_auth(API_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
}
