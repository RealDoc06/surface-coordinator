//! `job.cancel` against a fake coordinator and a fake platform: a waiting
//! job never runs, a running script is stopped, a job never received is
//! settled as cancelled, anything else is ignored.

mod common;

use std::time::Duration;

use common::{client_config, start_client, FakeCoordinator, FakePlatform};
use protocol::{reason, JobResult, JobStatus, OutputStream};
use serde_json::json;

const QUIET: Duration = Duration::from_millis(300);

fn assert_cancelled(result: &JobResult) {
    assert_eq!(result.status, JobStatus::Failed, "{result:?}");
    assert_eq!(result.reason.as_deref(), Some(reason::CANCELLED), "{result:?}");
    assert_eq!(result.exit_code, None, "{result:?}");
}

#[tokio::test]
async fn a_waiting_job_that_is_cancelled_never_runs_and_is_reported_cancelled() {
    let coordinator = FakeCoordinator::start().await;
    let platform = FakePlatform::new();
    let client = start_client(client_config(coordinator.addr), platform.clone());
    let (mut connection, _) = coordinator.accept().await;

    connection.send_script("blocker", "wait\nout blocker done").await;
    connection.send_script("t2", "out must never run").await;
    connection.expect_started("blocker").await;

    connection.cancel("t2").await;
    // Reported at once, without `job.started`: it never started.
    assert_cancelled(&connection.expect_result("t2").await);

    platform.release();
    connection.expect_output("blocker", 1, OutputStream::Stdout, "blocker done\n").await;
    connection.expect_result("blocker").await;
    assert_eq!(connection.try_recv(QUIET).await, None);

    // Sent again by the coordinator: still never runs.
    connection.send_script("t2", "out must never run").await;
    assert_eq!(connection.try_recv(QUIET).await, None);
    assert_eq!(platform.runs().len(), 1);
    client.stop().await;
}

#[tokio::test]
async fn a_cancelled_waiting_job_is_replayed_without_job_started_until_acknowledged() {
    let coordinator = FakeCoordinator::start().await;
    let platform = FakePlatform::new();
    let client = start_client(client_config(coordinator.addr), platform.clone());
    let (mut first, _) = coordinator.accept().await;

    first.send_script("blocker", "wait").await;
    first.send_script("t2", "out must never run").await;
    first.expect_started("blocker").await;
    first.cancel("t2").await;
    assert_cancelled(&first.expect_result("t2").await);
    // Lost before the coordinator acknowledged it.
    first.drop_abruptly();

    let (mut second, _) = coordinator.accept().await;
    // The job still running is replayed as started; the cancelled one only
    // as its result.
    second.expect_started("blocker").await;
    assert_cancelled(&second.expect_result("t2").await);
    second.ack("t2").await;
    second.send_script("t2", "out must never run").await;
    assert_eq!(second.try_recv(QUIET).await, None);

    platform.release();
    second.expect_result("blocker").await;
    second.ack("blocker").await;
    second.drop_abruptly();

    // Acknowledged: nothing more to replay.
    let (mut third, _) = coordinator.accept().await;
    assert_eq!(third.try_recv(QUIET).await, None);
    assert_eq!(platform.runs().len(), 1);
    client.stop().await;
}

#[tokio::test]
async fn a_running_script_is_stopped_and_reported_cancelled() {
    let coordinator = FakeCoordinator::start().await;
    let platform = FakePlatform::new();
    let client = start_client(client_config(coordinator.addr), platform.clone());
    let (mut connection, _) = coordinator.accept().await;

    connection.send_script("t1", "out before\nwait\nout after").await;
    connection.expect_started("t1").await;
    connection.expect_output("t1", 1, OutputStream::Stdout, "before\n").await;

    connection.cancel("t1").await;
    assert_cancelled(&connection.expect_result("t1").await);
    // The script was stopped where it was, not left to run on.
    assert_eq!(platform.scripts_dropped(), 1);

    // The next job runs as usual.
    connection.send_script("t2", "out next").await;
    connection.expect_started("t2").await;
    connection.expect_output("t2", 1, OutputStream::Stdout, "next\n").await;
    assert_eq!(connection.expect_result("t2").await.status, JobStatus::Succeeded);

    // The cancelled job is not run again when it is sent again.
    connection.send_script("t1", "out before\nwait\nout after").await;
    assert_eq!(connection.try_recv(QUIET).await, None);
    assert_eq!(platform.runs().len(), 2);
    client.stop().await;
}

#[tokio::test]
async fn cancelling_a_job_never_received_reports_it_cancelled_and_it_never_runs() {
    let coordinator = FakeCoordinator::start().await;
    let platform = FakePlatform::new();
    let client = start_client(client_config(coordinator.addr), platform.clone());
    let (mut connection, _) = coordinator.accept().await;

    // The `job` message was lost; the coordinator still thinks it is out.
    connection.cancel("lost-job").await;
    let result = connection.expect_result("lost-job").await;
    assert_cancelled(&result);
    assert_eq!(result.message.as_deref(), Some("cancelled before it was received"));

    // The copy sent later is a duplicate, also after a reconnect.
    connection.send_script("lost-job", "out must never run").await;
    assert_eq!(connection.try_recv(QUIET).await, None);
    connection.drop_abruptly();
    let (mut again, _) = coordinator.accept().await;
    // Replayed until acknowledged, without `job.started`.
    assert_cancelled(&again.expect_result("lost-job").await);
    again.ack("lost-job").await;
    again.send_script("lost-job", "out must never run").await;
    assert_eq!(again.try_recv(QUIET).await, None);
    assert!(platform.runs().is_empty());
    client.stop().await;
}

#[tokio::test]
async fn cancelling_a_finished_job_is_ignored() {
    let coordinator = FakeCoordinator::start().await;
    let platform = FakePlatform::new();
    let client = start_client(client_config(coordinator.addr), platform.clone());
    let (mut connection, _) = coordinator.accept().await;

    connection.send_script("t1", "out done").await;
    connection.expect_started("t1").await;
    connection.expect_output("t1", 1, OutputStream::Stdout, "done
").await;
    assert_eq!(connection.expect_result("t1").await.status, JobStatus::Succeeded);
    // Finished but not yet acknowledged, then acknowledged.
    connection.cancel("t1").await;
    assert_eq!(connection.try_recv(QUIET).await, None);
    connection.ack("t1").await;
    connection.cancel("t1").await;
    assert_eq!(connection.try_recv(QUIET).await, None);

    // Nothing was disturbed.
    connection.send_script("t2", "exit 0").await;
    connection.expect_started("t2").await;
    assert_eq!(connection.expect_result("t2").await.status, JobStatus::Succeeded);
    assert_eq!(platform.scripts_dropped(), 0);
    client.stop().await;
}

#[tokio::test]
async fn a_setting_being_applied_is_past_the_point_of_no_return() {
    let coordinator = FakeCoordinator::start().await;
    let platform = FakePlatform::new();
    platform.hold_applies();
    let client = start_client(client_config(coordinator.addr), platform.clone());
    let (mut connection, _) = coordinator.accept().await;

    connection
        .send_job("t1", "setting.apply", json!({"setting": "audio.mute", "value": true}))
        .await;
    connection.expect_started("t1").await;
    connection.cancel("t1").await;
    // Not stopped half way: the setting is applied and reported as usual.
    assert_eq!(connection.try_recv(QUIET).await, None);
    platform.release();
    assert_eq!(connection.expect_result("t1").await.status, JobStatus::Succeeded);
    assert_eq!(platform.applied().len(), 1);
    client.stop().await;
}

#[tokio::test]
async fn a_power_job_cancelled_while_waiting_is_never_carried_out() {
    let coordinator = FakeCoordinator::start().await;
    let platform = FakePlatform::new();
    let client = start_client(client_config(coordinator.addr), platform.clone());
    let (mut connection, _) = coordinator.accept().await;

    connection.send_script("blocker", "wait").await;
    connection.send_job("t1", "power", json!({"action": "restart"})).await;
    connection.expect_started("blocker").await;
    connection.cancel("t1").await;
    assert_cancelled(&connection.expect_result("t1").await);

    platform.release();
    connection.expect_result("blocker").await;
    // Longer than the client waits for an acknowledgement in these tests.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(platform.power_calls().is_empty());
    client.stop().await;
}
