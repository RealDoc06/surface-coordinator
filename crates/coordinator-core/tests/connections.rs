//! Handshake, online state and heartbeats.

mod common;

use std::time::Duration;

use common::{wait_until, FakeClient, HandshakeError, HandshakeOptions, TestServer};
use coordinator_core::admin;
use protocol::{reject_reason, ClientMessage, ServerMessage};

#[tokio::test]
async fn handshake_brings_device_online_and_disconnect_takes_it_offline() {
    let server = TestServer::start().await;
    let key = server.new_key();
    assert!(server.devices().await.is_empty());

    let client = FakeClient::connect(&server, &key, "device-a").await;
    assert_eq!(client.heartbeat_interval_ms, 30_000);

    let device = server.device("device-a").await.expect("device listed");
    assert_eq!(device["online"], true);
    assert_eq!(device["hostname"], "host-device-a");
    assert_eq!(device["os"], "windows");
    assert_eq!(device["client_version"], "0.0.0-test");
    assert_eq!(device["enrollment_key_id"], key.id.as_str());
    assert!(device["last_seen_at"].as_i64().unwrap() > 0);

    client.disconnect().await;
    assert!(wait_until(|| async { !server.is_online("device-a").await }).await);
    // The device stays known after it disconnects.
    assert!(server.device("device-a").await.is_some());
}

#[tokio::test]
async fn coordinator_proves_knowledge_of_the_key() {
    // FakeClient::handshake verifies the coordinator's MAC; with the real
    // secret it passes, with a different secret the client detects it.
    let server = TestServer::start().await;
    let key = server.new_key();
    let mut options = HandshakeOptions::new(&key, "device-a");
    options.secret = "fake-wrong-secret-fixture";
    let result = FakeClient::handshake(server.addr, options).await;
    assert!(matches!(result, Err(HandshakeError::BadCoordinatorMac)));
    assert!(server.devices().await.is_empty());
}

#[tokio::test]
async fn wrong_secret_is_rejected() {
    let server = TestServer::start().await;
    let key = server.new_key();
    // A hostile client that does not bother to verify the coordinator.
    let mut options = HandshakeOptions::new(&key, "device-a");
    options.secret = "fake-wrong-secret-fixture";
    options.skip_verify = true;
    match FakeClient::handshake(server.addr, options).await {
        Err(HandshakeError::Rejected(reason)) => assert_eq!(reason, reject_reason::BAD_MAC),
        other => panic!("expected rejection, got {:?}", other.map(|_| "welcome")),
    }
    assert!(server.devices().await.is_empty());
}

#[tokio::test]
async fn replaying_the_coordinator_mac_is_rejected() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let mut socket = common::open_socket(server.addr).await;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let hello = ClientMessage::Hello(protocol::Hello {
        protocol_version: protocol::PROTOCOL_VERSION,
        device_id: "device-a".into(),
        hostname: "h".into(),
        os: "windows".into(),
        client_version: "0".into(),
        key_id: key.id.clone(),
        nonce: protocol::handshake::generate_nonce(),
    });
    socket.send(Message::text(hello.to_json())).await.unwrap();
    let challenge = socket.next().await.unwrap().unwrap();
    let ServerMessage::Challenge { mac, .. } =
        ServerMessage::from_json(challenge.to_text().unwrap()).unwrap()
    else {
        panic!("expected challenge");
    };
    socket.send(Message::text(ClientMessage::Auth { mac }.to_json())).await.unwrap();
    let reply = socket.next().await.unwrap().unwrap();
    assert_eq!(
        ServerMessage::from_json(reply.to_text().unwrap()).unwrap(),
        ServerMessage::Reject { reason: reject_reason::BAD_MAC.into() }
    );
    assert!(server.devices().await.is_empty());
}

#[tokio::test]
async fn unknown_key_id_is_rejected() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let mut options = HandshakeOptions::new(&key, "device-a");
    options.key_id = "no-such-key";
    match FakeClient::handshake(server.addr, options).await {
        Err(HandshakeError::Rejected(reason)) => assert_eq!(reason, reject_reason::UNKNOWN_KEY),
        other => panic!("expected rejection, got {:?}", other.map(|_| "welcome")),
    }
    assert!(server.devices().await.is_empty());
}

#[tokio::test]
async fn revoked_key_is_rejected() {
    let server = TestServer::start().await;
    let key = server.new_key();
    assert!(admin::revoke_enrollment_key(server.data_dir.path(), &key.id).unwrap());
    match FakeClient::handshake(server.addr, HandshakeOptions::new(&key, "device-a")).await {
        Err(HandshakeError::Rejected(reason)) => assert_eq!(reason, reject_reason::UNKNOWN_KEY),
        other => panic!("expected rejection, got {:?}", other.map(|_| "welcome")),
    }
}

#[tokio::test]
async fn unsupported_protocol_version_is_rejected() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let mut options = HandshakeOptions::new(&key, "device-a");
    options.protocol_version = protocol::PROTOCOL_VERSION + 1;
    match FakeClient::handshake(server.addr, options).await {
        Err(HandshakeError::Rejected(reason)) => {
            assert_eq!(reason, reject_reason::UNSUPPORTED_PROTOCOL)
        }
        other => panic!("expected rejection, got {:?}", other.map(|_| "welcome")),
    }
}

#[tokio::test]
async fn a_first_message_that_is_not_hello_closes_the_connection() {
    let server = TestServer::start().await;
    let mut socket = common::open_socket(server.addr).await;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    socket.send(Message::text(ClientMessage::Heartbeat.to_json())).await.unwrap();
    let reply = socket.next().await.unwrap().unwrap();
    assert_eq!(
        ServerMessage::from_json(reply.to_text().unwrap()).unwrap(),
        ServerMessage::Reject { reason: reject_reason::BAD_REQUEST.into() }
    );
}

#[tokio::test]
async fn device_goes_offline_after_three_missed_heartbeats() {
    let server =
        TestServer::start_with(|c| c.heartbeat_interval = Duration::from_millis(100)).await;
    let key = server.new_key();
    let mut client = FakeClient::connect(&server, &key, "device-a").await;
    assert!(server.is_online("device-a").await);

    // Heartbeats keep it online well past the timeout and are acknowledged.
    for _ in 0..8 {
        tokio::time::sleep(Duration::from_millis(80)).await;
        client.send(ClientMessage::Heartbeat).await;
        assert_eq!(client.recv().await, Some(ServerMessage::HeartbeatAck));
    }
    assert!(server.is_online("device-a").await);

    // Silence: the coordinator drops the connection and marks it offline.
    assert!(client.closed_by_server().await);
    assert!(wait_until(|| async { !server.is_online("device-a").await }).await);
}

#[tokio::test]
async fn unknown_and_malformed_messages_are_ignored() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let mut client = FakeClient::connect(&server, &key, "device-a").await;
    client.send_text(r#"{"type":"telemetry.report","cpu":12}"#).await;
    client.send_text("this is not json").await;
    client.send_text(r#"{"type":"job.output"}"#).await;
    // Still connected and still answering.
    client.send(ClientMessage::Heartbeat).await;
    assert_eq!(client.recv().await, Some(ServerMessage::HeartbeatAck));
    assert!(server.is_online("device-a").await);
}

#[tokio::test]
async fn a_new_connection_replaces_the_old_one_for_the_same_device() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let mut first = FakeClient::connect(&server, &key, "device-a").await;
    let mut second = FakeClient::connect(&server, &key, "device-a").await;

    assert!(first.closed_by_server().await);
    // The old connection's teardown must not mark the device offline.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(server.is_online("device-a").await);
    assert_eq!(server.devices().await.len(), 1);

    second.send(ClientMessage::Heartbeat).await;
    assert_eq!(second.recv().await, Some(ServerMessage::HeartbeatAck));
}

#[tokio::test]
async fn stop_closes_connections_and_the_listener() {
    let mut server = TestServer::start().await;
    let key = server.new_key();
    let mut client = FakeClient::connect(&server, &key, "device-a").await;
    let addr = server.addr;
    server.stop().await;
    assert!(client.closed_by_server().await);
    assert!(tokio::net::TcpStream::connect(addr).await.is_err());
}

#[tokio::test]
async fn devices_survive_a_restart_and_come_back_offline() {
    let mut server = TestServer::start().await;
    let key = server.new_key();
    let _client = FakeClient::connect(&server, &key, "device-a").await;
    assert!(server.is_online("device-a").await);

    server.restart().await;
    let device = server.device("device-a").await.expect("device persisted");
    assert_eq!(device["online"], false);

    // The same key still works against the restarted server.
    let _client = FakeClient::connect(&server, &key, "device-a").await;
    assert!(server.is_online("device-a").await);
}
