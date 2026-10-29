//! Client core against a fake coordinator and a fake platform: handshake,
//! heartbeat, reconnect, job lifecycle and duplicate protection.

mod common;

use std::time::Duration;

use client::platform::ScriptRequest;
use common::{
    client_config, start_client, start_client_with_state, FakeCoordinator, FakePlatform, DEVICE_ID,
    KEY_ID, SECRET,
};
use protocol::handshake::{compute_mac, generate_nonce, is_valid_nonce, Role};
use protocol::{ClientMessage, JobStatus, OutputStream, RunAs, ServerMessage, Shell};
use serde_json::json;

const QUIET: Duration = Duration::from_millis(200);

// ------------------------------------------------------------ handshake

#[tokio::test]
async fn hello_identifies_the_device_and_the_handshake_completes() {
    let coordinator = FakeCoordinator::start().await;
    let client = start_client(client_config(coordinator.addr), FakePlatform::new());

    // `accept` checks the client's MAC.
    let (_connection, hello) = coordinator.accept().await;
    assert_eq!(hello.protocol_version, protocol::PROTOCOL_VERSION);
    assert_eq!(hello.device_id, DEVICE_ID);
    assert_eq!(hello.key_id, KEY_ID);
    assert_eq!(hello.hostname, "FAKE-HOST");
    assert_eq!(hello.os, "fake-os");
    assert_eq!(hello.client_version, "9.9.9-test");
    assert!(is_valid_nonce(&hello.nonce));
    client.stop().await;
}

#[tokio::test]
async fn client_refuses_a_coordinator_that_does_not_hold_the_key() {
    let coordinator = FakeCoordinator::start().await;
    let client = start_client(client_config(coordinator.addr), FakePlatform::new());

    let mut connection = coordinator.accept_raw().await;
    let hello = connection.expect_hello().await;
    let server_nonce = generate_nonce();
    let forged = compute_mac("some-other-fake-secret", Role::Coordinator, &hello.nonce, &server_nonce);
    connection.send(ServerMessage::Challenge { nonce: server_nonce, mac: forged }).await;
    // The client hangs up without revealing its own MAC...
    assert_eq!(connection.recv().await, None);

    // ...and tries again with a fresh nonce.
    let mut retry = coordinator.accept_raw().await;
    let second = retry.expect_hello().await;
    assert_ne!(second.nonce, hello.nonce);
    client.stop().await;
}

#[tokio::test]
async fn client_does_not_accept_its_own_role_mac_from_the_coordinator() {
    let coordinator = FakeCoordinator::start().await;
    let client = start_client(client_config(coordinator.addr), FakePlatform::new());

    let mut connection = coordinator.accept_raw().await;
    let hello = connection.expect_hello().await;
    let server_nonce = generate_nonce();
    // Right key, but the MAC a client would send rather than a coordinator.
    let wrong_role = compute_mac(SECRET, Role::Client, &hello.nonce, &server_nonce);
    connection.send(ServerMessage::Challenge { nonce: server_nonce, mac: wrong_role }).await;
    assert_eq!(connection.recv().await, None);
    client.stop().await;
}

#[tokio::test]
async fn a_rejected_client_keeps_retrying() {
    let coordinator = FakeCoordinator::start().await;
    let client = start_client(client_config(coordinator.addr), FakePlatform::new());

    for _ in 0..2 {
        let mut connection = coordinator.accept_raw().await;
        connection.expect_hello().await;
        connection.send(ServerMessage::Reject { reason: "unknown_key".into() }).await;
        assert!(connection.closed_by_client().await);
    }
    // Once the coordinator knows the key, the same client gets in.
    let (_connection, _) = coordinator.accept().await;
    client.stop().await;
}

// ------------------------------------------------------------ heartbeat and reconnect

#[tokio::test]
async fn heartbeats_are_sent_at_the_announced_interval() {
    let mut coordinator = FakeCoordinator::start().await;
    coordinator.heartbeat_interval_ms = 50;
    let client = start_client(client_config(coordinator.addr), FakePlatform::new());
    let (mut connection, _) = coordinator.accept().await;

    let started = tokio::time::Instant::now();
    for _ in 0..4 {
        assert_eq!(connection.recv_with_heartbeats().await, Some(ClientMessage::Heartbeat));
        connection.send(ServerMessage::HeartbeatAck).await;
    }
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(150), "4 heartbeats took only {elapsed:?}");
    assert!(elapsed < Duration::from_secs(2), "4 heartbeats took {elapsed:?}");
    client.stop().await;
}

#[tokio::test]
async fn client_reconnects_when_the_coordinator_goes_silent() {
    let mut coordinator = FakeCoordinator::start().await;
    coordinator.heartbeat_interval_ms = 50;
    let client = start_client(client_config(coordinator.addr), FakePlatform::new());

    // Never acknowledge: after three silent intervals the client gives up
    // on this connection and opens a new one.
    let (mut silent, _) = coordinator.accept().await;
    let (_fresh, _) = coordinator.accept().await;
    assert!(silent.closed_by_client().await);
    client.stop().await;
}

#[tokio::test]
async fn client_reconnects_after_the_connection_drops() {
    let coordinator = FakeCoordinator::start().await;
    let client = start_client(client_config(coordinator.addr), FakePlatform::new());

    let (first, hello) = coordinator.accept().await;
    first.drop_abruptly();
    let (mut second, again) = coordinator.accept().await;
    assert_eq!(again.device_id, hello.device_id);
    assert_ne!(again.nonce, hello.nonce);

    // The new connection is fully functional.
    second.send_script("t1", "out hi").await;
    second.expect_started("t1").await;
    client.stop().await;
}

#[tokio::test]
async fn client_keeps_trying_while_the_coordinator_is_down() {
    // Reserve a port, then close it so connections are refused.
    let coordinator = FakeCoordinator::start().await;
    let addr = coordinator.addr;
    drop(coordinator);
    let client = start_client(client_config(addr), FakePlatform::new());
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The coordinator comes up on the same address later.
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        // Another process took the port; nothing to verify.
        Err(_) => return client.stop().await,
    };
    let (stream, _) = tokio::time::timeout(common::WAIT, listener.accept()).await.unwrap().unwrap();
    drop(stream);
    client.stop().await;
}

#[tokio::test]
async fn unknown_and_malformed_messages_are_ignored() {
    let coordinator = FakeCoordinator::start().await;
    let platform = FakePlatform::new();
    let client = start_client(client_config(coordinator.addr), platform.clone());
    let (mut connection, _) = coordinator.accept().await;

    connection.send_text(r#"{"type":"config.push","settings":{"a":1}}"#).await;
    connection.send_text("not json at all").await;
    connection.send_text(r#"{"type":"job","job_id":"missing fields"}"#).await;

    // Still connected and working.
    connection.send_script("t1", "out still here").await;
    connection.expect_started("t1").await;
    connection.expect_output("t1", 1, OutputStream::Stdout, "still here\n").await;
    assert_eq!(connection.expect_result("t1").await.status, JobStatus::Succeeded);
    client.stop().await;
}

// ------------------------------------------------------------ job lifecycle

#[tokio::test]
async fn script_job_reports_started_output_and_result() {
    let coordinator = FakeCoordinator::start().await;
    let platform = FakePlatform::new();
    let client = start_client(client_config(coordinator.addr), platform.clone());
    let (mut connection, _) = coordinator.accept().await;

    connection
        .send_job(
            "t1",
            "script.run",
            json!({"shell": "cmd", "body": "out one\nerr two\nout three", "timeout_secs": 42, "run_as": "system"}),
        )
        .await;
    connection.expect_started("t1").await;
    connection.expect_output("t1", 1, OutputStream::Stdout, "one\n").await;
    connection.expect_output("t1", 2, OutputStream::Stderr, "two\n").await;
    connection.expect_output("t1", 3, OutputStream::Stdout, "three\n").await;
    let result = connection.expect_result("t1").await;
    assert_eq!(result.status, JobStatus::Succeeded);
    assert_eq!(result.exit_code, Some(0));
    assert_eq!(result.reason, None);

    // The platform got exactly what the payload said.
    assert_eq!(
        platform.runs(),
        [ScriptRequest {
            shell: Shell::Cmd,
            body: "out one\nerr two\nout three".into(),
            timeout: Duration::from_secs(42),
            run_as: RunAs::System,
        }]
    );
    client.stop().await;
}

#[tokio::test]
async fn failures_are_reported_with_a_reason() {
    let coordinator = FakeCoordinator::start().await;
    let platform = FakePlatform::new();
    let client = start_client(client_config(coordinator.addr), platform.clone());
    let (mut connection, _) = coordinator.accept().await;

    connection.send_script("exit", "out partial\nexit 3").await;
    connection.expect_started("exit").await;
    connection.expect_output("exit", 1, OutputStream::Stdout, "partial\n").await;
    let result = connection.expect_result("exit").await;
    assert_eq!(result.status, JobStatus::Failed);
    assert_eq!(result.exit_code, Some(3));
    assert_eq!(result.reason.as_deref(), Some("nonzero_exit"));

    connection.send_script("timeout", "timeout").await;
    connection.expect_started("timeout").await;
    let result = connection.expect_result("timeout").await;
    assert_eq!(result.status, JobStatus::Failed);
    assert_eq!(result.exit_code, None);
    assert_eq!(result.reason.as_deref(), Some("timeout"));

    connection.send_script("unsupported", "unsupported").await;
    connection.expect_started("unsupported").await;
    let result = connection.expect_result("unsupported").await;
    assert_eq!(result.status, JobStatus::Failed);
    assert_eq!(result.reason.as_deref(), Some("unsupported"));

    connection.send_script("error", "fail could not start shell").await;
    connection.expect_started("error").await;
    let result = connection.expect_result("error").await;
    assert_eq!(result.status, JobStatus::Failed);
    assert_eq!(result.reason.as_deref(), Some("error"));
    assert_eq!(result.message.as_deref(), Some("could not start shell"));
    client.stop().await;
}

#[tokio::test]
async fn unsupported_job_kind_fails_as_unsupported() {
    let coordinator = FakeCoordinator::start().await;
    let platform = FakePlatform::new();
    let client = start_client(client_config(coordinator.addr), platform.clone());
    let (mut connection, _) = coordinator.accept().await;

    connection.send_job("t1", "example.kind_from_the_future", json!({"anything": true})).await;
    connection.expect_started("t1").await;
    let result = connection.expect_result("t1").await;
    assert_eq!(result.status, JobStatus::Failed);
    assert_eq!(result.reason.as_deref(), Some("unsupported"));
    assert_eq!(result.exit_code, None);
    assert!(platform.runs().is_empty());

    // The client carries on with jobs it does support.
    connection.send_script("t2", "exit 0").await;
    connection.expect_started("t2").await;
    assert_eq!(connection.expect_result("t2").await.status, JobStatus::Succeeded);
    client.stop().await;
}

#[tokio::test]
async fn invalid_payload_fails_without_running_anything() {
    let coordinator = FakeCoordinator::start().await;
    let platform = FakePlatform::new();
    let client = start_client(client_config(coordinator.addr), platform.clone());
    let (mut connection, _) = coordinator.accept().await;

    connection.send_job("t1", "script.run", json!({"shell": "bash", "body": "ls"})).await;
    connection.expect_started("t1").await;
    let result = connection.expect_result("t1").await;
    assert_eq!(result.status, JobStatus::Failed);
    assert_eq!(result.reason.as_deref(), Some("invalid_payload"));
    assert!(result.message.is_some());
    assert!(platform.runs().is_empty());
    client.stop().await;
}

#[tokio::test]
async fn jobs_run_one_at_a_time_in_arrival_order() {
    let coordinator = FakeCoordinator::start().await;
    let platform = FakePlatform::new();
    let client = start_client(client_config(coordinator.addr), platform.clone());
    let (mut connection, _) = coordinator.accept().await;

    connection.send_script("first", "wait\nout first done").await;
    connection.send_script("second", "out second done").await;
    connection.expect_started("first").await;
    // The second job does not start while the first is running.
    assert_eq!(connection.try_recv(QUIET).await, None);
    assert_eq!(platform.runs().len(), 1);

    platform.release();
    connection.expect_output("first", 1, OutputStream::Stdout, "first done\n").await;
    connection.expect_result("first").await;
    connection.expect_started("second").await;
    connection.expect_output("second", 1, OutputStream::Stdout, "second done\n").await;
    connection.expect_result("second").await;
    client.stop().await;
}

// ------------------------------------------------------------ duplicates

#[tokio::test]
async fn a_job_delivered_twice_runs_once() {
    let coordinator = FakeCoordinator::start().await;
    let platform = FakePlatform::new();
    let client = start_client(client_config(coordinator.addr), platform.clone());
    let (mut connection, _) = coordinator.accept().await;

    connection.send_script("queued-behind", "wait").await;
    connection.send_script("t1", "out ran").await;
    // Duplicate while it is still waiting its turn...
    connection.send_script("t1", "out ran").await;
    connection.expect_started("queued-behind").await;
    // ...while the first job is running...
    connection.send_script("queued-behind", "wait").await;
    platform.release();
    connection.expect_result("queued-behind").await;
    connection.expect_started("t1").await;
    connection.expect_output("t1", 1, OutputStream::Stdout, "ran\n").await;
    connection.expect_result("t1").await;
    // ...after it finished, and after its result was acknowledged.
    connection.send_script("t1", "out ran").await;
    connection.ack("t1").await;
    connection.send_script("t1", "out ran").await;

    assert_eq!(connection.try_recv(QUIET).await, None);
    assert_eq!(platform.runs().len(), 2);
    client.stop().await;
}

// ------------------------------------------------------------ across reconnects

#[tokio::test]
async fn a_job_running_when_the_connection_drops_finishes_and_reports_after_reconnect() {
    let coordinator = FakeCoordinator::start().await;
    let platform = FakePlatform::new();
    let client = start_client(client_config(coordinator.addr), platform.clone());

    let (mut first, _) = coordinator.accept().await;
    first.send_script("t1", "out before\nwait\nout after\nexit 7").await;
    first.expect_started("t1").await;
    first.expect_output("t1", 1, OutputStream::Stdout, "before\n").await;
    first.drop_abruptly();

    // The job finishes while the client is disconnected.
    platform.release();

    // Everything about the unacknowledged job is replayed: the coordinator
    // drops what it already has by sequence number.
    let (mut second, _) = coordinator.accept().await;
    second.expect_started("t1").await;
    second.expect_output("t1", 1, OutputStream::Stdout, "before\n").await;
    second.expect_output("t1", 2, OutputStream::Stdout, "after\n").await;
    let result = second.expect_result("t1").await;
    assert_eq!(result.status, JobStatus::Failed);
    assert_eq!(result.exit_code, Some(7));
    assert_eq!(platform.runs().len(), 1);
    client.stop().await;
}

#[tokio::test]
async fn a_result_is_replayed_until_it_is_acknowledged() {
    let coordinator = FakeCoordinator::start().await;
    let platform = FakePlatform::new();
    let client = start_client(client_config(coordinator.addr), platform.clone());

    let (mut first, _) = coordinator.accept().await;
    first.send_script("t1", "out data").await;
    first.expect_started("t1").await;
    first.expect_output("t1", 1, OutputStream::Stdout, "data\n").await;
    first.expect_result("t1").await;
    // No acknowledgement before the connection is lost.
    first.drop_abruptly();

    let (mut second, _) = coordinator.accept().await;
    // The coordinator re-sends the job it considers unresolved; the client
    // must not run it again.
    second.send_script("t1", "out data").await;
    second.expect_started("t1").await;
    second.expect_output("t1", 1, OutputStream::Stdout, "data\n").await;
    second.expect_result("t1").await;
    second.ack("t1").await;
    assert_eq!(second.try_recv(QUIET).await, None);
    second.drop_abruptly();

    // Acknowledged: nothing is replayed on the next connection.
    let (mut third, _) = coordinator.accept().await;
    assert_eq!(third.try_recv(QUIET).await, None);
    assert_eq!(platform.runs().len(), 1);
    client.stop().await;
}

#[tokio::test]
async fn a_job_in_progress_is_replayed_as_started_and_then_completes_once() {
    let coordinator = FakeCoordinator::start().await;
    let platform = FakePlatform::new();
    let client = start_client(client_config(coordinator.addr), platform.clone());

    let (mut first, _) = coordinator.accept().await;
    first.send_script("t1", "wait\nout done").await;
    first.expect_started("t1").await;
    first.drop_abruptly();

    let (mut second, _) = coordinator.accept().await;
    second.send_script("t1", "wait\nout done").await;
    second.expect_started("t1").await;
    assert_eq!(second.try_recv(QUIET).await, None);

    platform.release();
    second.expect_output("t1", 1, OutputStream::Stdout, "done\n").await;
    assert_eq!(second.expect_result("t1").await.status, JobStatus::Succeeded);
    assert_eq!(platform.runs().len(), 1);
    client.stop().await;
}

// ------------------------------------------------------------ across restarts

#[tokio::test]
async fn a_job_cannot_run_without_a_durable_start_record() {
    let coordinator = FakeCoordinator::start().await;
    let state = tempfile::tempdir().unwrap();
    let platform = FakePlatform::new();
    let client = start_client_with_state(coordinator.addr, state.path(), platform.clone());
    let (mut connection, _) = coordinator.accept().await;
    let blocked = state.path().join("jobs.json.tmp");
    std::fs::create_dir(&blocked).unwrap();

    connection.send_script("blocked", "out must not execute").await;
    // The first message must be failure, not job.started or script output.
    let result = connection.expect_result("blocked").await;
    assert_eq!(result.status, JobStatus::Failed);
    assert_eq!(result.reason.as_deref(), Some(protocol::reason::ERROR));
    assert!(result.message.as_deref().unwrap().contains("job was not run"));
    assert!(result.message.as_deref().unwrap().contains("jobs.json"));
    assert!(platform.runs().is_empty());
    connection.drop_abruptly();

    // Reconnection replays the failure without claiming that the job ran.
    let (mut connection, _) = coordinator.accept().await;
    assert_eq!(connection.expect_result("blocked").await, result);
    std::fs::remove_dir(blocked).unwrap();
    connection.send_script("blocked", "out must still not execute").await;
    connection.send_script("after-repair", "out working").await;
    connection.expect_started("after-repair").await;
    connection.expect_output("after-repair", 1, OutputStream::Stdout, "working\n").await;
    assert_eq!(connection.expect_result("after-repair").await.status, JobStatus::Succeeded);
    assert_eq!(platform.runs().len(), 1);
    client.stop().await;
}

#[tokio::test]
async fn a_restarted_client_reports_an_interrupted_job_instead_of_running_it_again() {
    let coordinator = FakeCoordinator::start().await;
    let state_dir = tempfile::tempdir().unwrap();

    let platform = FakePlatform::new();
    let client = start_client_with_state(coordinator.addr, state_dir.path(), platform.clone());
    let (mut connection, _) = coordinator.accept().await;
    connection.send_script("done", "exit 0").await;
    connection.expect_started("done").await;
    connection.expect_result("done").await;
    connection.ack("done").await;
    connection.send_script("running", "wait").await;
    connection.expect_started("running").await;
    connection.send_script("waiting", "out never started before the restart").await;
    assert_eq!(connection.try_recv(QUIET).await, None);
    // The client process dies with one job running and one waiting.
    client.stop().await;
    drop(connection);

    let platform = FakePlatform::new();
    let client = start_client_with_state(coordinator.addr, state_dir.path(), platform.clone());
    let (mut connection, _) = coordinator.accept().await;
    // The job that was running is reported as interrupted...
    connection.expect_started("running").await;
    let result = connection.expect_result("running").await;
    assert_eq!(result.status, JobStatus::Failed);
    assert_eq!(result.reason.as_deref(), Some("interrupted"));

    // ...and is not run when the coordinator re-sends it; neither is the
    // one that had already finished. The one that never started does run.
    connection.send_script("running", "wait").await;
    connection.send_script("done", "exit 0").await;
    connection.send_script("waiting", "out started after the restart").await;
    connection.expect_started("waiting").await;
    connection.expect_output("waiting", 1, OutputStream::Stdout, "started after the restart\n").await;
    connection.expect_result("waiting").await;
    assert_eq!(platform.runs().len(), 1);
    client.stop().await;
}

#[tokio::test]
async fn device_id_is_generated_once_and_persisted() {
    let state_dir = tempfile::tempdir().unwrap();
    let first = client::load_or_create_device_id(state_dir.path()).unwrap();
    let second = client::load_or_create_device_id(state_dir.path()).unwrap();
    assert_eq!(first, second);
    assert!(first.len() >= 32);
    let other_dir = tempfile::tempdir().unwrap();
    assert_ne!(client::load_or_create_device_id(other_dir.path()).unwrap(), first);
}
