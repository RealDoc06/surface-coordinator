//! Job creation, dispatch, lifecycle, offline queue and the HTTP API.

mod common;

use std::time::Duration;

use common::{wait_until, FakeClient, TestServer, API_TOKEN};
use protocol::{ClientMessage, JobResult, JobStatus, ServerMessage};
use serde_json::json;

#[tokio::test]
async fn script_job_runs_through_its_whole_lifecycle() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let mut client = FakeClient::connect(&server, &key, "device-a").await;

    let (status, body) = server
        .post(
            "/api/v1/jobs",
            json!({
                "kind": "script.run",
                "payload": {"shell": "powershell", "body": "Write-Output 'hello'", "timeout_secs": 60},
                "targets": {"type": "devices", "device_ids": ["device-a"]},
            }),
        )
        .await;
    assert_eq!(status, 201, "{body}");
    let job = &body["job"];
    let job_id = job["id"].as_str().unwrap().to_owned();
    assert_eq!(job["kind"], "script.run");
    assert_eq!(job["targets"].as_array().unwrap().len(), 1);

    let (target_id, received_job_id, kind, payload) = client.expect_job().await;
    assert_eq!(received_job_id, job_id);
    assert_eq!(kind, "script.run");
    // Defaults are filled in by the coordinator before dispatch.
    assert_eq!(
        payload,
        json!({"shell": "powershell", "body": "Write-Output 'hello'", "timeout_secs": 60, "run_as": "system"})
    );
    let target = server.wait_for_target_state(&job_id, "sent").await;
    assert_eq!(target["id"], target_id.as_str());
    assert_eq!(target["device_id"], "device-a");

    client.started(&target_id).await;
    let target = server.wait_for_target_state(&job_id, "running").await;
    assert!(target["started_at"].as_i64().is_some());

    client.output(&target_id, 1, "hello\n").await;
    client.output(&target_id, 2, "world\n").await;
    assert!(
        wait_until(|| async { server.single_target(&job_id).await["output"] == "hello\nworld\n" })
            .await,
        "output is readable while the job is still running"
    );

    client.result(&target_id, 0).await;
    client.expect_result_ack(&target_id).await;
    let target = server.wait_for_target_state(&job_id, "succeeded").await;
    assert_eq!(target["exit_code"], 0);
    assert_eq!(target["output"], "hello\nworld\n");
    assert_eq!(target["output_truncated"], false);
    assert!(target["finished_at"].as_i64().is_some());
    assert!(target["reason"].is_null());
}

#[tokio::test]
async fn failed_result_stores_exit_code_reason_and_message() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let mut client = FakeClient::connect(&server, &key, "device-a").await;
    let job = server.create_script_job(json!({"type": "all"})).await;
    let job_id = job["id"].as_str().unwrap();
    let (target_id, ..) = client.expect_job().await;

    client.started(&target_id).await;
    client
        .send(ClientMessage::JobResult(JobResult {
            job_target_id: target_id.clone(),
            status: JobStatus::Failed,
            exit_code: None,
            reason: Some(protocol::reason::UNSUPPORTED.into()),
            message: Some("job kind is not supported".into()),
        }))
        .await;
    client.expect_result_ack(&target_id).await;
    let target = server.wait_for_target_state(job_id, "failed").await;
    assert!(target["exit_code"].is_null());
    assert_eq!(target["reason"], "unsupported");
    assert_eq!(target["message"], "job kind is not supported");
}

#[tokio::test]
async fn all_targets_every_known_device_and_selected_targets_only_those() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let mut a = FakeClient::connect(&server, &key, "device-a").await;
    let mut b = FakeClient::connect(&server, &key, "device-b").await;
    let mut c = FakeClient::connect(&server, &key, "device-c").await;

    let job = server.create_script_job(json!({"type": "all"})).await;
    assert_eq!(job["targets"].as_array().unwrap().len(), 3);
    for client in [&mut a, &mut b, &mut c] {
        let (_, job_id, ..) = client.expect_job().await;
        assert_eq!(job_id, job["id"].as_str().unwrap());
    }

    let job = server
        .create_script_job(json!({"type": "devices", "device_ids": ["device-a", "device-c"]}))
        .await;
    let devices: Vec<&str> = job["targets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["device_id"].as_str().unwrap())
        .collect();
    assert_eq!(devices, ["device-a", "device-c"]);
    a.expect_job().await;
    c.expect_job().await;
    assert!(b.try_recv(Duration::from_millis(200)).await.is_none());
}

#[tokio::test]
async fn job_for_an_offline_device_is_queued_and_delivered_on_reconnect() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let client = FakeClient::connect(&server, &key, "device-a").await;
    client.disconnect().await;
    assert!(wait_until(|| async { !server.is_online("device-a").await }).await);

    let job = server.create_script_job(json!({"type": "devices", "device_ids": ["device-a"]})).await;
    let job_id = job["id"].as_str().unwrap();
    assert_eq!(job["targets"][0]["state"], "queued");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(server.single_target(job_id).await["state"], "queued");

    let mut client = FakeClient::connect(&server, &key, "device-a").await;
    let (target_id, received_job_id, ..) = client.expect_job().await;
    assert_eq!(received_job_id, job_id);
    server.wait_for_target_state(job_id, "sent").await;

    client.started(&target_id).await;
    client.output(&target_id, 1, "late but delivered\n").await;
    client.result(&target_id, 0).await;
    client.expect_result_ack(&target_id).await;
    let target = server.wait_for_target_state(job_id, "succeeded").await;
    assert_eq!(target["output"], "late but delivered\n");
}

#[tokio::test]
async fn queued_jobs_are_delivered_in_creation_order() {
    let server = TestServer::start().await;
    let key = server.new_key();
    FakeClient::connect(&server, &key, "device-a").await.disconnect().await;
    assert!(wait_until(|| async { !server.is_online("device-a").await }).await);

    let mut created = Vec::new();
    for _ in 0..3 {
        let job = server.create_script_job(json!({"type": "all"})).await;
        created.push(job["id"].as_str().unwrap().to_owned());
    }
    let mut client = FakeClient::connect(&server, &key, "device-a").await;
    let mut received = Vec::new();
    for _ in 0..3 {
        received.push(client.expect_job().await.1);
    }
    assert_eq!(received, created);
}

#[tokio::test]
async fn unresolved_targets_are_sent_again_after_a_reconnect() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let mut client = FakeClient::connect(&server, &key, "device-a").await;
    let sent_only = server.create_script_job(json!({"type": "all"})).await;
    let running = server.create_script_job(json!({"type": "all"})).await;
    let finished = server.create_script_job(json!({"type": "all"})).await;

    let (sent_target, ..) = client.expect_job().await;
    let (running_target, ..) = client.expect_job().await;
    let (finished_target, ..) = client.expect_job().await;
    client.started(&running_target).await;
    client.started(&finished_target).await;
    client.result(&finished_target, 0).await;
    client.expect_result_ack(&finished_target).await;
    server.wait_for_target_state(running["id"].as_str().unwrap(), "running").await;
    client.disconnect().await;
    assert!(wait_until(|| async { !server.is_online("device-a").await }).await);

    // After reconnecting, the two unresolved targets come again with the
    // same ids (the client de-duplicates); the finished one does not.
    let mut client = FakeClient::connect(&server, &key, "device-a").await;
    let (first, ..) = client.expect_job().await;
    let (second, ..) = client.expect_job().await;
    assert_eq!([first, second], [sent_target.clone(), running_target.clone()]);
    assert!(client.try_recv(Duration::from_millis(200)).await.is_none());

    // States are not reset by the re-send.
    assert_eq!(server.single_target(sent_only["id"].as_str().unwrap()).await["state"], "sent");
    assert_eq!(server.single_target(running["id"].as_str().unwrap()).await["state"], "running");
    assert_eq!(server.single_target(finished["id"].as_str().unwrap()).await["state"], "succeeded");

    // A result reported after the reconnect is accepted.
    client.result(&running_target, 0).await;
    client.expect_result_ack(&running_target).await;
    server.wait_for_target_state(running["id"].as_str().unwrap(), "succeeded").await;
}

#[tokio::test]
async fn replayed_output_and_results_are_not_applied_twice() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let mut client = FakeClient::connect(&server, &key, "device-a").await;
    let job = server.create_script_job(json!({"type": "all"})).await;
    let job_id = job["id"].as_str().unwrap();
    let (target_id, ..) = client.expect_job().await;

    client.started(&target_id).await;
    client.output(&target_id, 1, "one\n").await;
    client.output(&target_id, 2, "two\n").await;
    // A client replaying after a reconnect sends everything it retained.
    client.started(&target_id).await;
    client.output(&target_id, 1, "one\n").await;
    client.output(&target_id, 2, "two\n").await;
    client.output(&target_id, 3, "three\n").await;
    client.result(&target_id, 0).await;
    client.expect_result_ack(&target_id).await;
    let target = server.wait_for_target_state(job_id, "succeeded").await;
    assert_eq!(target["output"], "one\ntwo\nthree\n");

    // A repeated result is acknowledged again but changes nothing, and
    // output after the result is dropped.
    client.output(&target_id, 4, "too late\n").await;
    client.result(&target_id, 9).await;
    client.expect_result_ack(&target_id).await;
    let target = server.single_target(job_id).await;
    assert_eq!(target["state"], "succeeded");
    assert_eq!(target["exit_code"], 0);
    assert_eq!(target["output"], "one\ntwo\nthree\n");
}

#[tokio::test]
async fn output_is_capped_with_a_truncation_marker() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let mut client = FakeClient::connect(&server, &key, "device-a").await;
    let job = server.create_script_job(json!({"type": "all"})).await;
    let job_id = job["id"].as_str().unwrap();
    let (target_id, ..) = client.expect_job().await;
    client.started(&target_id).await;

    // 6 chunks of 200 KiB of a two-byte character: 1.17 MiB in total.
    let chunk = "é".repeat(100 * 1024);
    for seq in 1..=6 {
        client.output(&target_id, seq, &chunk).await;
    }
    client.result(&target_id, 0).await;
    client.expect_result_ack(&target_id).await;

    let target = server.wait_for_target_state(job_id, "succeeded").await;
    assert_eq!(target["output_truncated"], true);
    let output = target["output"].as_str().unwrap();
    let marker = "\n[output truncated]\n";
    assert!(output.ends_with(marker));
    let kept = &output[..output.len() - marker.len()];
    assert_eq!(kept.len(), protocol::MAX_OUTPUT_BYTES);
    assert!(kept.chars().all(|c| c == 'é'));
}

#[tokio::test]
async fn a_device_cannot_report_on_another_devices_target() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let mut owner = FakeClient::connect(&server, &key, "device-a").await;
    let mut intruder = FakeClient::connect(&server, &key, "device-b").await;
    let job = server.create_script_job(json!({"type": "devices", "device_ids": ["device-a"]})).await;
    let job_id = job["id"].as_str().unwrap();
    let (target_id, ..) = owner.expect_job().await;

    intruder.started(&target_id).await;
    intruder.output(&target_id, 1, "forged\n").await;
    intruder.result(&target_id, 0).await;
    // No acknowledgement for a target that is not the sender's.
    intruder.send(ClientMessage::Heartbeat).await;
    assert_eq!(intruder.recv().await, Some(ServerMessage::HeartbeatAck));

    let target = server.single_target(job_id).await;
    assert_eq!(target["state"], "sent");
    assert_eq!(target["output"], "");
}

#[tokio::test]
async fn queued_targets_expire() {
    let server = TestServer::start_with(|c| c.queued_expiry = Duration::from_millis(300)).await;
    let key = server.new_key();
    FakeClient::connect(&server, &key, "device-a").await.disconnect().await;
    assert!(wait_until(|| async { !server.is_online("device-a").await }).await);

    let job = server.create_script_job(json!({"type": "all"})).await;
    let job_id = job["id"].as_str().unwrap();
    let target = server.wait_for_target_state(job_id, "expired").await;
    assert!(target["finished_at"].as_i64().is_some());

    // An expired target is not delivered when the device returns.
    let mut client = FakeClient::connect(&server, &key, "device-a").await;
    assert!(client.try_recv(Duration::from_millis(200)).await.is_none());
}

#[tokio::test]
async fn jobs_survive_a_restart_and_are_delivered_afterwards() {
    let mut server = TestServer::start().await;
    let key = server.new_key();
    FakeClient::connect(&server, &key, "device-a").await.disconnect().await;
    assert!(wait_until(|| async { !server.is_online("device-a").await }).await);
    let job = server.create_script_job(json!({"type": "all"})).await;
    let job_id = job["id"].as_str().unwrap().to_owned();

    server.restart().await;
    assert_eq!(server.single_target(&job_id).await["state"], "queued");
    let mut client = FakeClient::connect(&server, &key, "device-a").await;
    assert_eq!(client.expect_job().await.1, job_id);
}

#[tokio::test]
async fn job_creation_is_validated() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let _client = FakeClient::connect(&server, &key, "device-a").await;
    let targets = json!({"type": "all"});
    let payload = json!({"shell": "powershell", "body": "Get-Date"});

    let cases = [
        (json!({"kind": "power", "payload": {"action": "restart"}, "targets": targets}), "unsupported_kind"),
        (json!({"kind": "script.run", "payload": {"shell": "bash", "body": "ls"}, "targets": targets}), "invalid_payload"),
        (json!({"kind": "script.run", "payload": {"shell": "cmd", "body": "  "}, "targets": targets}), "invalid_payload"),
        (json!({"kind": "script.run", "payload": {"shell": "cmd", "body": "dir", "timeout_secs": 0}, "targets": targets}), "invalid_payload"),
        (json!({"kind": "script.run", "payload": payload, "targets": {"type": "devices", "device_ids": ["nope"]}}), "unknown_device"),
        (json!({"kind": "script.run", "payload": payload, "targets": {"type": "devices", "device_ids": []}}), "no_targets"),
    ];
    for (body, code) in cases {
        let (status, response) = server.post("/api/v1/jobs", body.clone()).await;
        assert_eq!(status, 400, "{body} -> {response}");
        assert_eq!(response["error"]["code"], code, "{body} -> {response}");
        assert!(response["error"]["message"].is_string());
    }

    // A target type this version does not know is a client error too.
    let (status, _) = server
        .post("/api/v1/jobs", json!({"kind": "script.run", "payload": payload, "targets": {"type": "group", "group_id": "g"}}))
        .await;
    assert!((400..500).contains(&status));

    let (status, body) = server.get("/api/v1/jobs").await;
    assert_eq!(status, 200);
    assert!(body["jobs"].as_array().unwrap().is_empty(), "rejected jobs are not stored");
}

#[tokio::test]
async fn all_with_no_known_devices_is_an_error() {
    let server = TestServer::start().await;
    let (status, response) = server
        .post(
            "/api/v1/jobs",
            json!({"kind": "script.run", "payload": {"shell": "cmd", "body": "dir"}, "targets": {"type": "all"}}),
        )
        .await;
    assert_eq!(status, 400);
    assert_eq!(response["error"]["code"], "no_targets");
}

#[tokio::test]
async fn job_list_shows_states_without_output_and_unknown_job_is_404() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let mut client = FakeClient::connect(&server, &key, "device-a").await;
    let first = server.create_script_job(json!({"type": "all"})).await;
    let second = server.create_script_job(json!({"type": "all"})).await;
    let (target_id, ..) = client.expect_job().await;
    client.expect_job().await;
    client.started(&target_id).await;
    client.output(&target_id, 1, "data\n").await;
    client.result(&target_id, 0).await;
    client.expect_result_ack(&target_id).await;

    let (status, body) = server.get("/api/v1/jobs").await;
    assert_eq!(status, 200);
    let jobs = body["jobs"].as_array().unwrap();
    // Newest first.
    assert_eq!(jobs[0]["id"], second["id"]);
    assert_eq!(jobs[1]["id"], first["id"]);
    assert_eq!(jobs[1]["targets"][0]["state"], "succeeded");
    assert!(jobs[1]["targets"][0].get("output").is_none());
    assert_eq!(jobs[1]["payload"]["shell"], "powershell");

    let (status, body) = server.get("/api/v1/jobs/does-not-exist").await;
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "not_found");
}

#[tokio::test]
async fn api_requires_the_token() {
    let server = TestServer::start().await;
    for path in ["/api/v1/devices", "/api/v1/jobs", "/api/v1/jobs/x"] {
        let response = server.http.get(server.url(path)).send().await.unwrap();
        assert_eq!(response.status(), 401, "{path} without token");
        let response =
            server.http.get(server.url(path)).bearer_auth("wrong-token").send().await.unwrap();
        assert_eq!(response.status(), 401, "{path} with wrong token");
        let response =
            server.http.get(server.url(path)).bearer_auth(API_TOKEN).send().await.unwrap();
        assert_ne!(response.status(), 401, "{path} with token");
    }
    let response = server
        .http
        .post(server.url("/api/v1/jobs"))
        .json(&json!({"kind": "script.run", "payload": {"shell": "cmd", "body": "dir"}, "targets": {"type": "all"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);

    // Health is open and says which protocol the server speaks.
    let response = server.http.get(server.url("/api/v1/health")).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["status"], "ok");
    assert_eq!(body["protocol_version"], protocol::PROTOCOL_VERSION);
}
