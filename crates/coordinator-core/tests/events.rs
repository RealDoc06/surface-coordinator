//! The live event stream for the UI.

mod common;

use std::time::Duration;

use common::{FakeClient, HandshakeOptions, TestServer, API_TOKEN, WAIT};
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

struct Events {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

async fn connect(server: &TestServer, origin: Option<&str>, token: Option<&str>) -> Result<Events, u16> {
    let mut request = format!("ws://{}/api/v1/events", server.addr).into_client_request().unwrap();
    if let Some(token) = token {
        request.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
    }
    if let Some(origin) = origin {
        request.headers_mut().insert("Origin", origin.parse().unwrap());
    }
    match tokio_tungstenite::connect_async(request).await {
        Ok((socket, _)) => Ok(Events { socket }),
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => Err(response.status().as_u16()),
        Err(other) => panic!("unexpected error: {other}"),
    }
}

impl Events {
    async fn open(server: &TestServer) -> Events {
        connect(server, None, Some(API_TOKEN)).await.expect("events socket opens")
    }

    async fn next(&mut self) -> Value {
        loop {
            let message = tokio::time::timeout(WAIT, self.socket.next())
                .await
                .expect("timed out waiting for an event")
                .expect("event stream ended")
                .unwrap();
            if let Message::Text(text) = message {
                return serde_json::from_str(text.as_str()).unwrap();
            }
        }
    }

    /// Next event of the given type, skipping others.
    async fn next_of(&mut self, kind: &str) -> Value {
        loop {
            let event = self.next().await;
            if event["type"] == kind {
                return event;
            }
        }
    }

    async fn nothing_within(&mut self, within: Duration) -> bool {
        tokio::time::timeout(within, self.socket.next()).await.is_err()
    }
}

#[tokio::test]
async fn the_event_stream_requires_authentication_and_the_same_origin() {
    let server = TestServer::start().await;
    assert_eq!(connect(&server, None, None).await.err(), Some(401));
    assert_eq!(connect(&server, None, Some("wrong")).await.err(), Some(401));
    assert_eq!(connect(&server, Some("https://evil.example"), Some(API_TOKEN)).await.err(), Some(403));
    let own_origin = format!("http://{}", server.addr);
    assert!(connect(&server, Some(&own_origin), Some(API_TOKEN)).await.is_ok());
}

#[tokio::test]
async fn devices_coming_and_going_are_announced() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let mut events = Events::open(&server).await;

    let client = FakeClient::connect(&server, &key, "device-a").await;
    let event = events.next().await;
    assert_eq!(event["type"], "device.updated");
    assert_eq!(event["device"]["id"], "device-a");
    assert_eq!(event["device"]["online"], true);
    assert_eq!(event["device"]["hostname"], "host-device-a");

    client.disconnect().await;
    let event = events.next().await;
    assert_eq!(event["type"], "device.updated");
    assert_eq!(event["device"]["id"], "device-a");
    assert_eq!(event["device"]["online"], false);

    let response = server
        .http
        .delete(server.url("/api/v1/devices/device-a"))
        .bearer_auth(API_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    assert_eq!(events.next().await, json!({"type": "device.removed", "device_id": "device-a"}));
}

#[tokio::test]
async fn device_updates_carry_the_reported_network() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let mut events = Events::open(&server).await;

    let mut options = HandshakeOptions::new(&key, "device-a");
    options.network =
        Some(protocol::NetworkInfo { ssid: Some("SC-ad74046f".into()), ip: Some("192.168.137.23".into()) });
    let _client = FakeClient::handshake(server.addr, options).await.unwrap();
    let event = events.next_of("device.updated").await;
    assert_eq!(event["device"]["network"], json!({"ssid": "SC-ad74046f", "ip": "192.168.137.23"}));
}

#[tokio::test]
async fn a_job_is_followed_from_creation_to_result_and_output_events_rebuild_the_stored_output() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let mut client = FakeClient::connect(&server, &key, "device-a").await;
    let mut events = Events::open(&server).await;

    let job = server.create_script_job(json!({"type": "all"})).await;
    let job_id = job["id"].as_str().unwrap();
    let event = events.next().await;
    assert_eq!(event["type"], "job.created");
    assert_eq!(event["job"]["id"], job_id);
    assert_eq!(event["job"]["targets"][0]["state"], "queued");
    assert!(event["job"]["targets"][0].get("output").is_none());

    let event = events.next().await;
    assert_eq!(event["type"], "job.target.updated");
    assert_eq!(event["job_id"], job_id);
    assert_eq!(event["target"]["state"], "sent");
    assert_eq!(event["target"]["device_id"], "device-a");

    let (target_id, ..) = client.expect_job().await;
    client.started(&target_id).await;
    let event = events.next().await;
    assert_eq!(event["type"], "job.target.updated");
    assert_eq!(event["target"]["state"], "running");
    assert!(event["target"]["started_at"].as_i64().is_some());

    // Output, including a repeated chunk and enough to hit the cap.
    let mut rebuilt = String::new();
    client.output(&target_id, 1, "first\n").await;
    client.output(&target_id, 1, "first\n").await;
    client.output(&target_id, 2, "second\n").await;
    // Multi-byte characters: offsets count code points, not bytes.
    client.output(&target_id, 3, "caf\u{e9} \u{1f600}\n").await;
    let big = "x\u{e9}".repeat(300 * 1024);
    client.output(&target_id, 4, &big).await;
    client.output(&target_id, 5, &big).await;
    client.output(&target_id, 6, "dropped\n").await;
    client.result(&target_id, 0).await;

    let mut truncated = false;
    let finished = loop {
        let event = events.next().await;
        match event["type"].as_str().unwrap() {
            "job.output" => {
                assert_eq!(event["job_id"], job_id);
                assert_eq!(event["job_target_id"], target_id.as_str());
                assert!(!truncated, "no output events after the truncating one");
                assert_eq!(
                    event["offset"].as_u64().unwrap(),
                    rebuilt.chars().count() as u64,
                    "offset is the number of code points stored before this chunk"
                );
                rebuilt.push_str(event["data"].as_str().unwrap());
                truncated = event["truncated"].as_bool().unwrap();
            }
            "job.target.updated" => break event,
            other => panic!("unexpected event {other}"),
        }
    };
    assert!(truncated);
    assert_eq!(finished["target"]["state"], "succeeded");
    assert_eq!(finished["target"]["exit_code"], 0);
    assert_eq!(finished["target"]["output_truncated"], true);
    assert!(finished["target"].get("output").is_none());

    // What the events delivered is exactly what is stored.
    let stored = server.single_target(job_id).await;
    assert_eq!(stored["output"].as_str().unwrap(), rebuilt);

    // A repeated result changes nothing and announces nothing.
    client.result(&target_id, 0).await;
    client.expect_result_ack(&target_id).await;
    assert!(events.nothing_within(Duration::from_millis(200)).await);
}

#[tokio::test]
async fn expiry_and_rejected_connections_are_announced() {
    let server = TestServer::start_with(|c| c.queued_expiry = Duration::from_millis(200)).await;
    let key = server.new_key();
    FakeClient::connect(&server, &key, "device-a").await.disconnect().await;
    let mut events = Events::open(&server).await;

    let job = server.create_script_job(json!({"type": "all"})).await;
    let event = events.next_of("job.target.updated").await;
    assert_eq!(event["job_id"], job["id"]);
    assert_eq!(event["target"]["state"], "expired");

    let mut options = HandshakeOptions::new(&key, "device-b");
    options.key_id = "no-such-key";
    assert!(FakeClient::handshake(server.addr, options).await.is_err());
    let event = events.next_of("connection.rejected").await;
    assert_eq!(event["rejected"]["device_id"], "device-b");
    assert_eq!(event["rejected"]["key_id"], "no-such-key");
}

#[tokio::test]
async fn the_wifi_password_is_masked_in_job_events() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let mut client = FakeClient::connect(&server, &key, "device-a").await;
    let mut events = Events::open(&server).await;

    let (status, body) = server
        .post(
            "/api/v1/jobs",
            json!({
                "kind": "wifi.join",
                "payload": {"ssid": "fake-office", "password": "fake-passphrase"},
                "targets": {"type": "all"},
            }),
        )
        .await;
    assert_eq!(status, 201, "{body}");
    let event = events.next_of("job.created").await;
    assert_eq!(event["job"]["payload"], json!({"ssid": "fake-office", "password": "********"}));
    assert!(!event.to_string().contains("fake-passphrase"));
    assert_eq!(client.expect_job().await.3["password"], "fake-passphrase");
}

#[tokio::test]
async fn a_presumed_handoff_is_announced() {
    let server = TestServer::start_with(|c| c.handoff_grace = Duration::from_millis(200)).await;
    let key = server.new_key();
    let mut client = FakeClient::connect(&server, &key, "device-a").await;
    let mut events = Events::open(&server).await;

    let (status, body) = server
        .post(
            "/api/v1/jobs",
            json!({"kind": "wifi.join", "payload": {"ssid": "fake-office"}, "targets": {"type": "all"}}),
        )
        .await;
    assert_eq!(status, 201, "{body}");
    let (target_id, ..) = client.expect_job().await;
    client.started(&target_id).await;
    loop {
        let event = events.next_of("job.target.updated").await;
        if event["target"]["state"] == "running" {
            break;
        }
    }

    client.disconnect().await;
    let event = loop {
        let event = events.next_of("job.target.updated").await;
        if event["target"]["state"] == "succeeded" {
            break event;
        }
    };
    assert_eq!(event["job_id"], body["job"]["id"]);
    assert_eq!(event["target"]["id"], target_id.as_str());
    assert!(event["target"]["exit_code"].is_null());
    assert!(event["target"]["message"].as_str().unwrap().contains("presumed to have joined"));
}
