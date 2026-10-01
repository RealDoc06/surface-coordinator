//! Device detail and removal, enrollment keys, rejected connections, job
//! paging, the web UI and the separate UI listener.

mod common;

use std::time::Duration;

use common::{wait_until, FakeClient, HandshakeOptions, TestServer, API_TOKEN};
use coordinator_core::{AuthMode, Capabilities, Config};
use serde_json::{json, Value};

#[tokio::test]
async fn device_detail_lists_its_recent_jobs() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let mut client = FakeClient::connect(&server, &key, "device-a").await;
    let first = server.create_script_job(json!({"type": "all"})).await;
    let second = server.create_script_job(json!({"type": "all"})).await;
    let (target_id, ..) = client.expect_job().await;
    client.expect_job().await;
    client.started(&target_id).await;
    client.output(&target_id, 1, "not in the listing\n").await;
    client.result(&target_id, 0).await;
    client.expect_result_ack(&target_id).await;

    let (status, body) = server.get("/api/v1/devices/device-a").await;
    assert_eq!(status, 200);
    assert_eq!(body["device"]["id"], "device-a");
    assert_eq!(body["device"]["online"], true);
    let targets = body["targets"].as_array().unwrap();
    assert_eq!(targets.len(), 2);
    // Newest first, each with its job, none with output.
    assert_eq!(targets[0]["job"]["id"], second["id"]);
    assert_eq!(targets[1]["job"]["id"], first["id"]);
    assert_eq!(targets[1]["job"]["kind"], "script.run");
    assert_eq!(targets[1]["state"], "succeeded");
    assert_eq!(targets[1]["device_id"], "device-a");
    assert!(targets[1].get("output").is_none());

    let (status, body) = server.get("/api/v1/devices/unknown").await;
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "not_found");
}

#[tokio::test]
async fn forgetting_a_device_removes_it_its_history_and_its_connection() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let mut a = FakeClient::connect(&server, &key, "device-a").await;
    let _b = FakeClient::connect(&server, &key, "device-b").await;
    let only_a = server.create_script_job(json!({"type": "devices", "device_ids": ["device-a"]})).await;
    let both = server.create_script_job(json!({"type": "all"})).await;

    let response = server
        .http
        .delete(server.url("/api/v1/devices/device-a"))
        .bearer_auth(API_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    assert!(a.closed_by_server().await);
    assert!(server.device("device-a").await.is_none());

    // The job that only targeted it is gone; the shared one keeps the other target.
    let (status, _) = server.get(&format!("/api/v1/jobs/{}", only_a["id"].as_str().unwrap())).await;
    assert_eq!(status, 404);
    let job = server.job(both["id"].as_str().unwrap()).await;
    assert_eq!(job["targets"].as_array().unwrap().len(), 1);
    assert_eq!(job["targets"][0]["device_id"], "device-b");

    let response = server
        .http
        .delete(server.url("/api/v1/devices/device-a"))
        .bearer_auth(API_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);

    // The device may enroll again and starts from scratch.
    let _again = FakeClient::connect(&server, &key, "device-a").await;
    assert!(server.is_online("device-a").await);
}

#[tokio::test]
async fn enrollment_keys_can_be_created_exported_imported_and_revoked() {
    let server = TestServer::start().await;

    let (status, body) = server.post("/api/v1/enrollment-keys", json!({"label": "Classroom A"})).await;
    assert_eq!(status, 201, "{body}");
    let id = body["key"]["id"].as_str().unwrap().to_owned();
    let secret = body["key"]["secret"].as_str().unwrap().to_owned();
    assert_eq!(secret.len(), 64);
    assert_eq!(body["key"]["label"], "Classroom A");
    assert!(body["key"]["revoked_at"].is_null());

    // A device can enroll with the key the API just created.
    let key = coordinator_core::EnrollmentKey {
        id: id.clone(),
        label: String::new(),
        secret: secret.clone(),
        created: 0,
        revoked: None,
    };
    let _client = FakeClient::connect(&server, &key, "device-a").await;

    // Listings never contain secrets and count the devices using each key.
    let (_, body) = server.get("/api/v1/enrollment-keys").await;
    let keys = body["keys"].as_array().unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0]["id"], id.as_str());
    assert_eq!(keys[0]["device_count"], 1);
    assert!(keys[0].get("secret").is_none());

    // Export is explicit.
    let (status, body) = server.get(&format!("/api/v1/enrollment-keys/{id}/secret")).await;
    assert_eq!(status, 200);
    assert_eq!(body, json!({"id": id, "secret": secret}));
    let (status, _) = server.get("/api/v1/enrollment-keys/nope/secret").await;
    assert_eq!(status, 404);

    // Import into a second coordinator, as the manager does for hotspot mode.
    let other = TestServer::start().await;
    let (status, body) = other
        .post("/api/v1/enrollment-keys", json!({"label": "from remote", "id": id, "secret": secret}))
        .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["key"]["id"], id.as_str());
    let _client = FakeClient::connect(&other, &key, "device-a").await;
    let (status, body) = other
        .post("/api/v1/enrollment-keys", json!({"label": "again", "id": id, "secret": secret}))
        .await;
    assert_eq!(status, 409);
    assert_eq!(body["error"]["code"], "key_exists");
    for bad in [
        json!({"label": "x", "id": id}),
        json!({"label": "x", "id": "short", "secret": secret}),
        json!({"label": "x", "id": "valid-id-123", "secret": "too short"}),
    ] {
        let (status, body) = other.post("/api/v1/enrollment-keys", bad.clone()).await;
        assert_eq!(status, 400, "{bad}");
        assert_eq!(body["error"]["code"], "invalid_key");
    }

    // Revoked keys are refused at the next handshake.
    let (status, _) = server.post(&format!("/api/v1/enrollment-keys/{id}/revoke"), json!({})).await;
    assert_eq!(status, 204);
    let (_, body) = server.get("/api/v1/enrollment-keys").await;
    assert!(body["keys"][0]["revoked_at"].as_i64().is_some());
    assert!(FakeClient::handshake(server.addr, HandshakeOptions::new(&key, "device-b")).await.is_err());
    let (status, _) = server.post("/api/v1/enrollment-keys/nope/revoke", json!({})).await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn rejected_handshakes_are_recorded_for_the_operator() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let (_, body) = server.get("/api/v1/rejected-connections").await;
    assert!(body["rejected"].as_array().unwrap().is_empty());

    let mut wrong_secret = HandshakeOptions::new(&key, "device-wrong-secret");
    wrong_secret.secret = "fake-wrong-secret-fixture";
    wrong_secret.skip_verify = true;
    assert!(FakeClient::handshake(server.addr, wrong_secret).await.is_err());
    let mut unknown_key = HandshakeOptions::new(&key, "device-unknown-key");
    unknown_key.key_id = "no-such-key";
    assert!(FakeClient::handshake(server.addr, unknown_key).await.is_err());

    assert!(
        wait_until(|| async {
            server.get("/api/v1/rejected-connections").await.1["rejected"].as_array().unwrap().len() == 2
        })
        .await
    );
    let (_, body) = server.get("/api/v1/rejected-connections").await;
    let rejected = body["rejected"].as_array().unwrap();
    // Newest first.
    assert_eq!(rejected[0]["device_id"], "device-unknown-key");
    assert_eq!(rejected[0]["key_id"], "no-such-key");
    assert_eq!(rejected[0]["hostname"], "host-device-unknown-key");
    assert!(rejected[0]["reason"].as_str().unwrap().contains("unknown enrollment key"));
    assert!(rejected[0]["peer"].as_str().unwrap().starts_with("127.0.0.1:"));
    assert!(rejected[0]["at"].as_i64().unwrap() > 0);
    assert_eq!(rejected[1]["device_id"], "device-wrong-secret");
    assert_eq!(rejected[1]["key_id"], key.id.as_str());
    assert!(rejected[1]["reason"].as_str().unwrap().contains("MAC"));

    // A successful handshake leaves no trace here.
    let _ok = FakeClient::connect(&server, &key, "device-ok").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(server.get("/api/v1/rejected-connections").await.1["rejected"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn job_listing_pages_backwards_in_time() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let _client = FakeClient::connect(&server, &key, "device-a").await;
    let mut ids = Vec::new();
    for _ in 0..5 {
        ids.push(server.create_script_job(json!({"type": "all"})).await["id"].as_str().unwrap().to_owned());
        tokio::time::sleep(Duration::from_millis(3)).await;
    }
    ids.reverse();

    let (_, body) = server.get("/api/v1/jobs?limit=2").await;
    let page: Vec<&str> = body["jobs"].as_array().unwrap().iter().map(|j| j["id"].as_str().unwrap()).collect();
    assert_eq!(page, ids[0..2]);
    let before = body["jobs"][1]["created_at"].as_i64().unwrap();
    let (_, body) = server.get(&format!("/api/v1/jobs?limit=2&before={before}")).await;
    let page: Vec<&str> = body["jobs"].as_array().unwrap().iter().map(|j| j["id"].as_str().unwrap()).collect();
    assert_eq!(page, ids[2..4]);
    let (_, body) = server.get("/api/v1/jobs").await;
    assert_eq!(body["jobs"].as_array().unwrap().len(), 5);
}

#[tokio::test]
async fn the_web_ui_is_served_with_a_fallback_to_the_application() {
    let ui = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(ui.path().join("assets")).unwrap();
    std::fs::write(ui.path().join("index.html"), "<!doctype html><title>test ui</title>").unwrap();
    std::fs::write(ui.path().join("assets/app-abc123.js"), "console.log('app')").unwrap();
    let ui_dir = ui.path().to_owned();
    let server = TestServer::start_with(move |c| c.ui_dir = Some(ui_dir)).await;

    let get = |path: &'static str| {
        let request = server.http.get(server.url(path));
        async move { request.send().await.unwrap() }
    };

    for path in ["/", "/devices", "/jobs/some-id", "/index.html"] {
        let response = get(path).await;
        assert_eq!(response.status(), 200, "{path}");
        assert_eq!(response.headers()["content-type"], "text/html; charset=utf-8");
        assert_eq!(response.headers()["cache-control"], "no-cache");
        assert_eq!(response.headers()["x-frame-options"], "DENY");
        assert!(response.headers().contains_key("content-security-policy"));
        assert!(response.text().await.unwrap().contains("test ui"), "{path}");
    }

    let response = get("/assets/app-abc123.js").await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "text/javascript; charset=utf-8");
    assert!(response.headers()["cache-control"].to_str().unwrap().contains("immutable"));
    assert_eq!(response.text().await.unwrap(), "console.log('app')");

    // Unknown API routes are JSON errors, never the application page.
    let response = get("/api/v1/no-such-route").await;
    assert_eq!(response.status(), 404);
    assert_eq!(response.json::<Value>().await.unwrap()["error"]["code"], "not_found");

    // Nothing outside the UI directory can be read through it.
    for path in ["/..%2f..%2fCargo.toml", "/assets/..%2f..%2fsecret"] {
        let response = get(path).await;
        assert!(response.text().await.unwrap().contains("test ui"), "{path}");
    }
}

#[tokio::test]
async fn a_page_is_embedded_when_no_ui_directory_is_given() {
    let server = TestServer::start().await;
    let response = server.http.get(server.url("/")).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "text/html; charset=utf-8");
    assert!(response.text().await.unwrap().contains("Surface Coordinator"));
}

#[tokio::test]
async fn with_a_separate_ui_listener_clients_and_operators_use_different_ports() {
    let data_dir = tempfile::tempdir().unwrap();
    let mut config =
        Config::new("127.0.0.1:0".parse().unwrap(), data_dir.path(), AuthMode::Token(API_TOKEN.into()));
    config.ui_bind = Some("127.0.0.1:0".parse().unwrap());
    config.capabilities = Capabilities { hotspot: true };
    let coordinator = coordinator_core::start(config).await.unwrap();
    let (clients, ui) = (coordinator.local_addr(), coordinator.ui_addr());
    assert_ne!(clients, ui);
    let http = reqwest::Client::new();

    // The API and UI answer only on the UI listener.
    let response = http.get(format!("http://{ui}/api/v1/session")).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.json::<Value>().await.unwrap()["capabilities"]["hotspot"], true);
    assert_eq!(http.get(format!("http://{ui}/")).send().await.unwrap().status(), 200);
    assert_eq!(http.get(format!("http://{clients}/api/v1/session")).send().await.unwrap().status(), 404);
    assert_eq!(http.get(format!("http://{clients}/")).send().await.unwrap().status(), 404);

    // Clients connect on the client listener only.
    let key = coordinator_core::admin::create_enrollment_key(data_dir.path(), "k").unwrap();
    let _client = FakeClient::handshake(clients, HandshakeOptions::new(&key, "device-a")).await.unwrap();
    let devices: Value = http
        .get(format!("http://{ui}/api/v1/devices"))
        .bearer_auth(API_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(devices["devices"][0]["online"], true);
    assert_eq!(
        http.get(format!("http://{ui}{}", protocol::CLIENT_WS_PATH)).send().await.unwrap().status(),
        404
    );
    coordinator.stop().await;
}

#[tokio::test]
async fn embedder_routes_are_served_behind_authentication() {
    use axum::routing::get;
    let extra = axum::Router::new().route("/hotspot", get(|| async { "hotspot status" }));
    let server = TestServer::start_with(move |c| c.extra_api_routes = Some(extra)).await;

    let response = server.http.get(server.url("/api/v1/manager/hotspot")).send().await.unwrap();
    assert_eq!(response.status(), 401);
    let response = server
        .http
        .get(server.url("/api/v1/manager/hotspot"))
        .bearer_auth(API_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "hotspot status");
}
