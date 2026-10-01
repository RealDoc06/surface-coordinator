//! `app.launch`, `setting.apply` and `power` jobs, group targets, and the
//! settings catalogue.

mod common;

use std::time::Duration;

use common::{FakeClient, TestServer};
use serde_json::{json, Value};

fn one_device(id: &str) -> Value {
    json!({"type": "devices", "device_ids": [id]})
}

async fn assert_invalid(server: &TestServer, kind: &str, payload: Value) -> String {
    let (status, body) = server.create_job(kind, payload.clone(), json!({"type": "all"})).await;
    assert_eq!(status, 400, "{kind} {payload} -> {body}");
    assert_eq!(body["error"]["code"], "invalid_payload", "{kind} {payload} -> {body}");
    body["error"]["message"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn an_app_launch_job_is_dispatched_with_its_normalised_payload() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let mut client = FakeClient::connect(&server, &key, "device-a").await;

    let (status, body) = server
        .create_job(
            "app.launch",
            json!({"path": "C:\\Fake\\App.exe", "arguments": ["--flag", "two words"]}),
            one_device("device-a"),
        )
        .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["job"]["kind"], "app.launch");
    let (_, job_id, kind, payload) = client.expect_job().await;
    assert_eq!(job_id, body["job"]["id"].as_str().unwrap());
    assert_eq!(kind, "app.launch");
    assert_eq!(payload, json!({"path": "C:\\Fake\\App.exe", "arguments": ["--flag", "two words"]}));

    // Arguments are optional and stored as an empty list.
    let (status, body) = server.create_job("app.launch", json!({"path": "notepad.exe"}), one_device("device-a")).await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["job"]["payload"], json!({"path": "notepad.exe", "arguments": []}));
    assert_eq!(client.expect_job().await.3, json!({"path": "notepad.exe", "arguments": []}));
}

#[tokio::test]
async fn app_launch_payloads_are_validated() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let _client = FakeClient::connect(&server, &key, "device-a").await;

    let invalid = [
        json!({}),
        json!({"path": ""}),
        json!({"path": "a".repeat(1025)}),
        json!({"path": 5}),
        json!({"path": "a\u{0}b"}),
        json!({"path": "app.exe", "arguments": "--flag"}),
        json!({"path": "app.exe", "arguments": [1]}),
        json!({"path": "app.exe", "arguments": vec!["x"; 65]}),
        json!({"path": "app.exe", "arguments": ["x".repeat(1025)]}),
        json!({"path": "app.exe", "arguments": ["fine", "a\u{0}b"]}),
        json!("app.exe"),
    ];
    for payload in invalid {
        assert_invalid(&server, "app.launch", payload).await;
    }

    // The limits themselves are allowed; they count characters, not bytes.
    let arguments: Vec<String> = (0..64).map(|_| "\u{e9}".repeat(1024)).collect();
    let (status, body) = server
        .create_job("app.launch", json!({"path": "\u{e9}".repeat(1024), "arguments": arguments}), json!({"type": "all"}))
        .await;
    assert_eq!(status, 201, "{}", body.to_string().chars().take(300).collect::<String>());

    let (_, body) = server.get("/api/v1/jobs").await;
    assert_eq!(body["jobs"].as_array().unwrap().len(), 1, "rejected jobs are not stored");
}

#[tokio::test]
async fn a_setting_apply_job_is_dispatched_with_its_payload() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let mut client = FakeClient::connect(&server, &key, "device-a").await;

    for payload in [
        json!({"setting": "audio.volume", "value": 40}),
        json!({"setting": "audio.mute", "value": true}),
        json!({"setting": "time.zone", "value": "W. Europe Standard Time"}),
    ] {
        let (status, body) = server.create_job("setting.apply", payload.clone(), one_device("device-a")).await;
        assert_eq!(status, 201, "{payload} -> {body}");
        assert_eq!(body["job"]["payload"], payload);
        let (_, _, kind, received) = client.expect_job().await;
        assert_eq!(kind, "setting.apply");
        assert_eq!(received, payload);
    }
}

#[tokio::test]
async fn setting_apply_payloads_are_validated_against_the_catalogue() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let _client = FakeClient::connect(&server, &key, "device-a").await;

    let invalid = [
        json!({"setting": "no.such.setting", "value": 1}),
        json!({"setting": "audio.volume", "value": "loud"}),
        json!({"setting": "audio.volume", "value": 101}),
        json!({"setting": "audio.volume", "value": -1}),
        json!({"setting": "audio.volume", "value": 1.5}),
        json!({"setting": "audio.mute", "value": 1}),
        json!({"setting": "time.zone", "value": "z".repeat(65)}),
        json!({"setting": "time.zone", "value": 5}),
        json!({"setting": "windows_update.pause_days", "value": 36}),
        json!({"setting": "audio.volume"}),
        json!({"value": 5}),
    ];
    for payload in invalid {
        let message = assert_invalid(&server, "setting.apply", payload.clone()).await;
        if let (Some(setting), Some(value)) = (payload.get("setting").and_then(Value::as_str), payload.get("value")) {
            // The catalogue's own explanation is what the operator sees.
            assert_eq!(message, protocol::settings::validate(setting, value).unwrap_err(), "{payload}");
        }
    }
}

#[tokio::test]
async fn a_power_job_is_dispatched_with_its_action() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let mut client = FakeClient::connect(&server, &key, "device-a").await;

    for action in ["restart", "shutdown", "sleep"] {
        let (status, body) = server.create_job("power", json!({"action": action}), one_device("device-a")).await;
        assert_eq!(status, 201, "{body}");
        let (_, _, kind, payload) = client.expect_job().await;
        assert_eq!(kind, "power");
        assert_eq!(payload, json!({"action": action}));
    }
}

#[tokio::test]
async fn power_payloads_are_validated() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let _client = FakeClient::connect(&server, &key, "device-a").await;
    for payload in [json!({}), json!({"action": "hibernate"}), json!({"action": 1}), json!("restart")] {
        assert_invalid(&server, "power", payload).await;
    }
    assert_eq!(server.get("/api/v1/jobs").await.1["jobs"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn a_group_target_reaches_only_the_members_at_the_time_of_creation() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let mut a = FakeClient::connect(&server, &key, "device-a").await;
    let mut b = FakeClient::connect(&server, &key, "device-b").await;
    let mut c = FakeClient::connect(&server, &key, "device-c").await;
    let group = server.create_group("Classroom").await;
    let group_id = group["id"].as_str().unwrap();
    server.set_members(group_id, &["device-a", "device-c"]).await;

    let job = server.create_script_job(json!({"type": "group", "group_id": group_id})).await;
    let devices: Vec<&str> =
        job["targets"].as_array().unwrap().iter().map(|t| t["device_id"].as_str().unwrap()).collect();
    assert_eq!(devices, ["device-a", "device-c"]);
    a.expect_job().await;
    c.expect_job().await;
    assert!(b.try_recv(Duration::from_millis(200)).await.is_none());

    // Changing the group afterwards does not change the job.
    server.set_members(group_id, &["device-b"]).await;
    let stored = server.job(job["id"].as_str().unwrap()).await;
    assert_eq!(stored["targets"].as_array().unwrap().len(), 2);
    let job = server.create_script_job(json!({"type": "group", "group_id": group_id})).await;
    assert_eq!(job["targets"].as_array().unwrap().len(), 1);
    assert_eq!(job["targets"][0]["device_id"], "device-b");
    b.expect_job().await;
}

#[tokio::test]
async fn an_offline_group_member_has_its_target_queued() {
    let server = TestServer::start().await;
    let key = server.new_key();
    FakeClient::connect(&server, &key, "device-a").await.disconnect().await;
    let group = server.create_group("Cart").await;
    let group_id = group["id"].as_str().unwrap();
    server.set_members(group_id, &["device-a"]).await;

    let (status, body) = server
        .create_job("power", json!({"action": "restart"}), json!({"type": "group", "group_id": group_id}))
        .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["job"]["targets"][0]["state"], "queued");
}

#[tokio::test]
async fn an_unknown_or_empty_group_is_refused() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let _client = FakeClient::connect(&server, &key, "device-a").await;
    let empty = server.create_group("Empty").await;

    let payload = json!({"shell": "powershell", "body": "Get-Date"});
    let (status, body) = server
        .create_job("script.run", payload.clone(), json!({"type": "group", "group_id": "no-such-group"}))
        .await;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], "unknown_group");

    let (status, body) = server
        .create_job("script.run", payload, json!({"type": "group", "group_id": empty["id"]}))
        .await;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], "no_targets");
    assert_eq!(server.get("/api/v1/jobs").await.1["jobs"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn the_settings_catalogue_is_served_as_the_protocol_defines_it() {
    let server = TestServer::start().await;
    let (status, body) = server.get("/api/v1/settings-catalogue").await;
    assert_eq!(status, 200);
    assert_eq!(body["settings"], serde_json::to_value(protocol::settings::catalogue()).unwrap());

    let settings = body["settings"].as_array().unwrap();
    assert!(settings.len() >= 12);
    let volume = settings.iter().find(|s| s["id"] == "audio.volume").unwrap();
    assert_eq!(volume["scope"], "user");
    assert_eq!(volume["value"], json!({"type": "integer", "min": 0, "max": 100, "unit": "%"}));
    assert!(volume["label"].is_string() && volume["description"].is_string());

    // Like every other protected route, it needs a login.
    let response = TestServer::client().get(server.url("/api/v1/settings-catalogue")).send().await.unwrap();
    assert_eq!(response.status(), 401);
}
