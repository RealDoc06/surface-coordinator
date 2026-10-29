//! `app.launch`, `setting.apply`, `power` and scripts run as the user,
//! against a fake coordinator and a fake platform.

mod common;

use std::time::{Duration, Instant};

use client::platform::PlatformError;
use common::{client_config, start_client, start_client_with_state, FakeCoordinator, FakePlatform, WAIT};
use protocol::{reason, JobResult, JobStatus, PowerAction, RunAs};
use serde_json::{json, Value};

const QUIET: Duration = Duration::from_millis(300);
const FAKE_APP: &str = r"C:\Fake Programs\Kiosk\kiosk.exe";

fn assert_failed(result: &JobResult, reason: &str) {
    assert_eq!(result.status, JobStatus::Failed, "{result:?}");
    assert_eq!(result.reason.as_deref(), Some(reason), "{result:?}");
    assert_eq!(result.exit_code, None, "{result:?}");
}

fn assert_succeeded(result: &JobResult, message: &str) {
    assert_eq!(result.status, JobStatus::Succeeded, "{result:?}");
    assert_eq!(result.reason, None);
    assert_eq!(result.exit_code, None);
    assert_eq!(result.message.as_deref(), Some(message));
}

/// Waits for the platform to have carried out `count` power actions.
async fn power_calls(platform: &FakePlatform, count: usize) -> Vec<(PowerAction, Instant)> {
    let deadline = Instant::now() + WAIT;
    loop {
        let calls = platform.power_calls();
        if calls.len() >= count || Instant::now() >= deadline {
            return calls;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ------------------------------------------------------------ app.launch

#[tokio::test]
async fn app_launch_starts_the_program_and_reports_its_pid() {
    let coordinator = FakeCoordinator::start().await;
    let platform = FakePlatform::new();
    let client = start_client(client_config(coordinator.addr), platform.clone());
    let (mut connection, _) = coordinator.accept().await;

    connection
        .send_job("t1", "app.launch", json!({"path": FAKE_APP, "arguments": ["--kiosk", "two words"]}))
        .await;
    connection.expect_started("t1").await;
    let result = connection.expect_result("t1").await;
    assert_succeeded(&result, &format!("started {FAKE_APP} (pid 4242)"));
    assert_eq!(
        platform.launches(),
        [(FAKE_APP.to_owned(), vec!["--kiosk".to_owned(), "two words".to_owned()])]
    );

    // Arguments are optional.
    connection.send_job("t2", "app.launch", json!({"path": "notepad.exe"})).await;
    connection.expect_started("t2").await;
    assert_eq!(connection.expect_result("t2").await.status, JobStatus::Succeeded);
    assert_eq!(platform.launches()[1], ("notepad.exe".to_owned(), vec![]));
    client.stop().await;
}

#[tokio::test]
async fn app_launch_failures_are_reported_with_a_reason() {
    let coordinator = FakeCoordinator::start().await;
    let platform = FakePlatform::new();
    platform.on_launch("nobody-logged-in.exe", Err(PlatformError::NoUserSession("no one is logged in".into())));
    platform.on_launch("elsewhere.exe", Err(PlatformError::Unsupported("no apps here".into())));
    platform.on_launch("missing.exe", Err(PlatformError::Failed("the file does not exist".into())));
    let client = start_client(client_config(coordinator.addr), platform.clone());
    let (mut connection, _) = coordinator.accept().await;

    for (id, path, reason) in [
        ("session", "nobody-logged-in.exe", reason::NO_USER_SESSION),
        ("unsupported", "elsewhere.exe", reason::UNSUPPORTED),
        ("error", "missing.exe", reason::ERROR),
    ] {
        connection.send_job(id, "app.launch", json!({"path": path})).await;
        connection.expect_started(id).await;
        let result = connection.expect_result(id).await;
        assert_failed(&result, reason);
        if id == "error" {
            assert_eq!(result.message.as_deref(), Some("the file does not exist"));
        }
    }
    client.stop().await;
}

#[tokio::test]
async fn app_launch_with_an_invalid_payload_starts_nothing() {
    let coordinator = FakeCoordinator::start().await;
    let platform = FakePlatform::new();
    let client = start_client(client_config(coordinator.addr), platform.clone());
    let (mut connection, _) = coordinator.accept().await;

    for (id, payload) in [
        ("no-path", json!({"arguments": ["x"]})),
        ("empty-path", json!({"path": "  "})),
        ("bad-arguments", json!({"path": "app.exe", "arguments": "not a list"})),
    ] {
        connection.send_job(id, "app.launch", payload).await;
        connection.expect_started(id).await;
        let result = connection.expect_result(id).await;
        assert_failed(&result, reason::INVALID_PAYLOAD);
        assert!(result.message.is_some());
    }
    assert!(platform.launches().is_empty());
    client.stop().await;
}

// ------------------------------------------------------------ setting.apply

#[tokio::test]
async fn setting_apply_hands_the_validated_value_to_the_platform() {
    let coordinator = FakeCoordinator::start().await;
    let platform = FakePlatform::new();
    let client = start_client(client_config(coordinator.addr), platform.clone());
    let (mut connection, _) = coordinator.accept().await;

    let settings: [(&str, Value); 3] = [
        ("audio.volume", json!(35)),
        ("audio.mute", json!(true)),
        ("time.zone", json!("W. Europe Standard Time")),
    ];
    for (index, (setting, value)) in settings.iter().enumerate() {
        let id = format!("t{index}");
        connection.send_job(&id, "setting.apply", json!({"setting": setting, "value": value})).await;
        connection.expect_started(&id).await;
        let result = connection.expect_result(&id).await;
        assert_succeeded(&result, &format!("set {setting} to {value}"));
    }
    let expected: Vec<(String, Value)> =
        settings.iter().map(|(setting, value)| (setting.to_string(), value.clone())).collect();
    assert_eq!(platform.applied(), expected);
    client.stop().await;
}

#[tokio::test]
async fn setting_apply_rejects_a_value_the_catalogue_does_not_allow() {
    let coordinator = FakeCoordinator::start().await;
    let platform = FakePlatform::new();
    let client = start_client(client_config(coordinator.addr), platform.clone());
    let (mut connection, _) = coordinator.accept().await;

    for (id, payload) in [
        ("too-loud", json!({"setting": "audio.volume", "value": 101})),
        ("not-a-bool", json!({"setting": "audio.mute", "value": "yes"})),
        ("empty-zone", json!({"setting": "time.zone", "value": ""})),
        ("no-value", json!({"setting": "audio.volume"})),
    ] {
        connection.send_job(id, "setting.apply", payload).await;
        connection.expect_started(id).await;
        let result = connection.expect_result(id).await;
        assert_failed(&result, reason::INVALID_PAYLOAD);
        assert!(result.message.is_some(), "{result:?}");
    }
    // The catalogue's own explanation reaches the operator.
    connection.send_job("t", "setting.apply", json!({"setting": "audio.volume", "value": 101})).await;
    connection.expect_started("t").await;
    let message = connection.expect_result("t").await.message.unwrap();
    assert_eq!(message, protocol::settings::validate("audio.volume", &json!(101)).unwrap_err());
    assert!(platform.applied().is_empty());
    client.stop().await;
}

#[tokio::test]
async fn a_setting_this_client_does_not_know_fails_as_unsupported() {
    let coordinator = FakeCoordinator::start().await;
    let platform = FakePlatform::new();
    let client = start_client(client_config(coordinator.addr), platform.clone());
    let (mut connection, _) = coordinator.accept().await;

    // As a newer coordinator might send.
    connection
        .send_job("t1", "setting.apply", json!({"setting": "display.night_light", "value": true}))
        .await;
    connection.expect_started("t1").await;
    let result = connection.expect_result("t1").await;
    assert_failed(&result, reason::UNSUPPORTED);
    assert!(result.message.unwrap().contains("display.night_light"));
    assert!(platform.applied().is_empty());
    client.stop().await;
}

#[tokio::test]
async fn setting_apply_failures_are_reported_with_a_reason() {
    let coordinator = FakeCoordinator::start().await;
    let platform = FakePlatform::new();
    platform.on_apply("desktop.wallpaper", Err(PlatformError::NoUserSession("no one is logged in".into())));
    platform.on_apply("time.zone", Err(PlatformError::Unsupported("no time zones here".into())));
    platform.on_apply("display.brightness", Err(PlatformError::Failed("no built-in display".into())));
    let client = start_client(client_config(coordinator.addr), platform.clone());
    let (mut connection, _) = coordinator.accept().await;

    for (id, setting, value, reason) in [
        ("session", "desktop.wallpaper", json!(r"C:\Fake\wallpaper.jpg"), reason::NO_USER_SESSION),
        ("unsupported", "time.zone", json!("UTC"), reason::UNSUPPORTED),
        ("error", "display.brightness", json!(50), reason::ERROR),
    ] {
        connection.send_job(id, "setting.apply", json!({"setting": setting, "value": value})).await;
        connection.expect_started(id).await;
        let result = connection.expect_result(id).await;
        assert_failed(&result, reason);
        if id == "error" {
            assert_eq!(result.message.as_deref(), Some("no built-in display"));
        }
    }
    client.stop().await;
}

// ------------------------------------------------------------ script.run as the user

#[tokio::test]
async fn a_script_run_as_the_user_reaches_the_platform_as_such() {
    let coordinator = FakeCoordinator::start().await;
    let platform = FakePlatform::new();
    let client = start_client(client_config(coordinator.addr), platform.clone());
    let (mut connection, _) = coordinator.accept().await;

    connection
        .send_job("t1", "script.run", json!({"shell": "powershell", "body": "out as user", "run_as": "user"}))
        .await;
    connection.expect_started("t1").await;
    connection.expect_output("t1", 1, protocol::OutputStream::Stdout, "as user\n").await;
    assert_eq!(connection.expect_result("t1").await.status, JobStatus::Succeeded);
    assert_eq!(platform.runs()[0].run_as, RunAs::User);

    // Nobody logged in.
    connection
        .send_job("t2", "script.run", json!({"shell": "powershell", "body": "no-session", "run_as": "user"}))
        .await;
    connection.expect_started("t2").await;
    assert_failed(&connection.expect_result("t2").await, reason::NO_USER_SESSION);
    client.stop().await;
}

// ------------------------------------------------------------ power

#[tokio::test]
async fn power_reports_native_acceptance_then_waits_for_ack_before_another_job() {
    let coordinator = FakeCoordinator::start().await;
    let platform = FakePlatform::new();
    let mut config = client_config(coordinator.addr);
    // Long enough that only the acknowledgement can release the next job.
    config.power_ack_timeout = Duration::from_secs(20);
    let client = start_client(config, platform.clone());
    let (mut connection, _) = coordinator.accept().await;

    connection.send_job("t1", "power", json!({"action": "restart"})).await;
    connection.send_script("after", "out ran after the power job").await;
    connection.expect_started("t1").await;
    assert_succeeded(&connection.expect_result("t1").await, "restart scheduled by Windows");

    // The OS accepted the request before success was reported. No next job
    // starts until the coordinator has stored that result.
    assert_eq!(connection.try_recv(QUIET).await, None);
    let calls = power_calls(&platform, 1).await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, PowerAction::Restart);
    connection.ack("t1").await;

    // The worker carries on (on a device that did not actually restart).
    connection.expect_started("after").await;
    client.stop().await;
}

#[tokio::test]
async fn power_result_ack_timeout_does_not_block_the_worker_forever() {
    let coordinator = FakeCoordinator::start().await;
    let platform = FakePlatform::new();
    // `client_config` waits 200 ms for the acknowledgement.
    let client = start_client(client_config(coordinator.addr), platform.clone());
    let (mut connection, _) = coordinator.accept().await;

    connection.send_job("t1", "power", json!({"action": "shutdown"})).await;
    connection.send_script("after", "out ran after the power job").await;
    connection.expect_started("t1").await;
    assert_succeeded(&connection.expect_result("t1").await, "shutdown scheduled by Windows");
    let reported_at = Instant::now();

    let calls = power_calls(&platform, 1).await;
    assert_eq!(calls.len(), 1, "never acted");
    assert_eq!(calls[0].0, PowerAction::Shutdown);
    connection.expect_started("after").await;
    let waited = reported_at.elapsed();
    // The result was received a little after the timer started.
    assert!(waited >= Duration::from_millis(100), "next job started after only {waited:?}");
    assert!(waited < Duration::from_secs(3), "next job started after {waited:?}");
    client.stop().await;
}

#[tokio::test]
async fn power_is_carried_out_once_even_if_the_job_is_delivered_again() {
    let coordinator = FakeCoordinator::start().await;
    let platform = FakePlatform::new();
    let client = start_client(client_config(coordinator.addr), platform.clone());
    let (mut connection, _) = coordinator.accept().await;

    let sleep = json!({"action": "sleep"});
    connection.send_job("t1", "power", sleep.clone()).await;
    connection.expect_started("t1").await;
    assert_succeeded(&connection.expect_result("t1").await, "sleep request completed");
    // Again before the acknowledgement...
    connection.send_job("t1", "power", sleep.clone()).await;
    connection.ack("t1").await;
    assert_eq!(power_calls(&platform, 1).await.len(), 1);
    // ...after it...
    connection.send_job("t1", "power", sleep.clone()).await;
    assert_eq!(connection.try_recv(QUIET).await, None);
    connection.drop_abruptly();

    // ...and on a new connection, as after the device woke up.
    let (mut connection, _) = coordinator.accept().await;
    connection.send_job("t1", "power", sleep).await;
    assert_eq!(connection.try_recv(QUIET).await, None);
    assert_eq!(platform.power_calls().len(), 1);
    client.stop().await;
}

#[tokio::test]
async fn power_on_a_platform_without_it_fails_before_reporting_success() {
    let coordinator = FakeCoordinator::start().await;
    let platform = FakePlatform::new();
    platform.without_power();
    let client = start_client(client_config(coordinator.addr), platform.clone());
    let (mut connection, _) = coordinator.accept().await;

    connection.send_job("t1", "power", json!({"action": "restart"})).await;
    connection.expect_started("t1").await;
    assert_failed(&connection.expect_result("t1").await, reason::UNSUPPORTED);

    connection.send_job("t2", "power", json!({"action": "hibernate"})).await;
    connection.expect_started("t2").await;
    assert_failed(&connection.expect_result("t2").await, reason::INVALID_PAYLOAD);

    tokio::time::sleep(QUIET).await;
    assert!(platform.power_calls().is_empty());
    client.stop().await;
}

#[tokio::test]
async fn native_power_failure_after_successful_preflight_is_reported_to_the_operator() {
    let coordinator = FakeCoordinator::start().await;
    let platform = FakePlatform::new();
    platform.fail_power(PlatformError::Failed("native power request rejected".into()));
    let client = start_client(client_config(coordinator.addr), platform.clone());
    let (mut connection, _) = coordinator.accept().await;
    for action in ["restart", "shutdown", "sleep"] {
        connection.send_job(action, "power", json!({"action": action})).await;
        connection.expect_started(action).await;
        let result = connection.expect_result(action).await;
        assert_failed(&result, reason::ERROR);
        assert_eq!(result.message.as_deref(), Some("native power request rejected"));
        connection.ack(action).await;
    }
    assert_eq!(platform.power_calls().len(), 3);
    client.stop().await;
}

#[tokio::test]
async fn sleep_waits_for_native_return_and_survives_disconnect_cancel_and_redelivery() {
    let coordinator = FakeCoordinator::start().await;
    let platform = FakePlatform::new();
    platform.hold_power();
    let client = start_client(client_config(coordinator.addr), platform.clone());
    let (mut connection, _) = coordinator.accept().await;
    let payload = json!({"action": "sleep"});
    connection.send_job("sleep", "power", payload.clone()).await;
    connection.expect_started("sleep").await;
    assert_eq!(power_calls(&platform, 1).await.len(), 1);
    assert_eq!(connection.try_recv(QUIET).await, None, "success before native return");
    connection.drop_abruptly();

    let (mut connection, _) = coordinator.accept().await;
    connection.expect_started("sleep").await;
    connection.send_job("sleep", "power", payload).await;
    connection.cancel("sleep").await;
    assert_eq!(connection.try_recv(QUIET).await, None);
    assert_eq!(platform.power_calls().len(), 1);
    platform.release();
    assert_succeeded(&connection.expect_result("sleep").await, "sleep request completed");
    connection.ack("sleep").await;
    client.stop().await;
}

#[tokio::test]
async fn interrupted_native_sleep_is_reported_after_restart_without_repeating_it() {
    let coordinator = FakeCoordinator::start().await;
    let state = tempfile::tempdir().unwrap();
    let platform = FakePlatform::new();
    platform.hold_power();
    let client = start_client_with_state(coordinator.addr, state.path(), platform.clone());
    let (mut connection, _) = coordinator.accept().await;
    let payload = json!({"action": "sleep"});
    connection.send_job("sleep", "power", payload.clone()).await;
    connection.expect_started("sleep").await;
    assert_eq!(power_calls(&platform, 1).await.len(), 1);
    client.stop().await;
    connection.drop_abruptly();

    let client = start_client_with_state(coordinator.addr, state.path(), platform.clone());
    let (mut connection, _) = coordinator.accept().await;
    connection.expect_started("sleep").await;
    assert_failed(&connection.expect_result("sleep").await, reason::INTERRUPTED);
    connection.send_job("sleep", "power", payload).await;
    connection.ack("sleep").await;
    assert_eq!(connection.try_recv(QUIET).await, None);
    assert_eq!(platform.power_calls().len(), 1);
    client.stop().await;
}
