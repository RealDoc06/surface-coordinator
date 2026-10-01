//! Test harness: a real server on a random port with a temporary data
//! directory, a fake client speaking the protocol over a WebSocket, and a
//! small HTTP helper.
#![allow(dead_code)]

use std::future::Future;
use std::net::SocketAddr;
use std::time::Duration;

use coordinator_core::{admin, AuthMode, Config, Coordinator, EnrollmentKey};
use futures_util::{SinkExt, StreamExt};
use protocol::handshake::{compute_mac, generate_nonce, verify_mac, Role};
use protocol::{ClientMessage, Hello, JobResult, JobStatus, OutputStream, ServerMessage};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

pub const API_TOKEN: &str = "test-api-token-fixture";
pub const WAIT: Duration = Duration::from_secs(5);

pub struct TestServer {
    pub coordinator: Option<Coordinator>,
    pub data_dir: TempDir,
    pub addr: SocketAddr,
    pub http: reqwest::Client,
}

pub fn test_config(data_dir: &std::path::Path) -> Config {
    let mut config = Config::new(
        "127.0.0.1:0".parse().unwrap(),
        data_dir,
        AuthMode::Token(API_TOKEN.into()),
    );
    config.expiry_sweep_interval = Duration::from_millis(50);
    config
}

impl TestServer {
    pub async fn start() -> Self {
        Self::start_with(|_| {}).await
    }

    pub async fn start_with(adjust: impl FnOnce(&mut Config)) -> Self {
        let data_dir = tempfile::tempdir().unwrap();
        let mut config = test_config(data_dir.path());
        adjust(&mut config);
        let coordinator = coordinator_core::start(config).await.unwrap();
        let addr = coordinator.local_addr();
        TestServer { coordinator: Some(coordinator), data_dir, addr, http: reqwest::Client::new() }
    }

    /// Stops the server and starts a new one on the same data directory.
    pub async fn restart(&mut self) {
        self.coordinator.take().unwrap().stop().await;
        let coordinator = coordinator_core::start(test_config(self.data_dir.path())).await.unwrap();
        self.addr = coordinator.local_addr();
        self.coordinator = Some(coordinator);
    }

    pub async fn stop(&mut self) {
        self.coordinator.take().unwrap().stop().await;
    }

    /// Creates an enrollment key with a secret generated at run time.
    pub fn new_key(&self) -> EnrollmentKey {
        admin::create_enrollment_key(self.data_dir.path(), "test").unwrap()
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    pub async fn get(&self, path: &str) -> (u16, Value) {
        let response = self.http.get(self.url(path)).bearer_auth(API_TOKEN).send().await.unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    pub async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        let response = self
            .http
            .post(self.url(path))
            .bearer_auth(API_TOKEN)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    pub async fn devices(&self) -> Vec<Value> {
        let (status, body) = self.get("/api/v1/devices").await;
        assert_eq!(status, 200);
        body["devices"].as_array().unwrap().clone()
    }

    pub async fn device(&self, id: &str) -> Option<Value> {
        self.devices().await.into_iter().find(|d| d["id"] == id)
    }

    pub async fn is_online(&self, id: &str) -> bool {
        self.device(id).await.is_some_and(|d| d["online"] == true)
    }

    /// Creates a `script.run` job and returns the created job object.
    pub async fn create_script_job(&self, targets: Value) -> Value {
        let (status, body) = self
            .post(
                "/api/v1/jobs",
                json!({
                    "kind": "script.run",
                    "payload": {"shell": "powershell", "body": "Write-Output 'hi'"},
                    "targets": targets,
                }),
            )
            .await;
        assert_eq!(status, 201, "create job failed: {body}");
        body["job"].clone()
    }

    pub async fn job(&self, id: &str) -> Value {
        let (status, body) = self.get(&format!("/api/v1/jobs/{id}")).await;
        assert_eq!(status, 200);
        body["job"].clone()
    }

    /// The only target of a job.
    pub async fn single_target(&self, job_id: &str) -> Value {
        let job = self.job(job_id).await;
        let targets = job["targets"].as_array().unwrap();
        assert_eq!(targets.len(), 1);
        targets[0].clone()
    }

    pub async fn wait_for_target_state(&self, job_id: &str, state: &str) -> Value {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let target = self.single_target(job_id).await;
            if target["state"] == state {
                return target;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "target never reached state {state}; last seen: {target}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

/// Polls `condition` until it holds or [`WAIT`] passes.
pub async fn wait_until<F, Fut>(mut condition: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        if condition().await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// What a handshake attempt ended with.
#[derive(Debug)]
pub enum HandshakeError {
    /// The coordinator sent `reject` with this reason.
    Rejected(String),
    /// The coordinator's own MAC did not verify.
    BadCoordinatorMac,
    /// The connection ended without a `reject`.
    Closed,
}

pub struct FakeClient {
    pub socket: Socket,
    pub device_id: String,
    pub heartbeat_interval_ms: u64,
}

pub struct HandshakeOptions<'a> {
    pub key_id: &'a str,
    /// Secret the client uses for both verification and its own MAC.
    pub secret: &'a str,
    pub device_id: &'a str,
    pub protocol_version: u32,
    /// Skip verifying the coordinator, as a hostile client would.
    pub skip_verify: bool,
}

impl<'a> HandshakeOptions<'a> {
    pub fn new(key: &'a EnrollmentKey, device_id: &'a str) -> Self {
        HandshakeOptions {
            key_id: &key.id,
            secret: &key.secret,
            device_id,
            protocol_version: protocol::PROTOCOL_VERSION,
            skip_verify: false,
        }
    }
}

pub async fn open_socket(addr: SocketAddr) -> Socket {
    let url = format!("ws://{}{}", addr, protocol::CLIENT_WS_PATH);
    let (socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    socket
}

impl FakeClient {
    pub async fn connect(server: &TestServer, key: &EnrollmentKey, device_id: &str) -> FakeClient {
        Self::handshake(server.addr, HandshakeOptions::new(key, device_id))
            .await
            .expect("handshake should succeed")
    }

    pub async fn handshake(
        addr: SocketAddr,
        options: HandshakeOptions<'_>,
    ) -> Result<FakeClient, HandshakeError> {
        let mut socket = open_socket(addr).await;
        let client_nonce = generate_nonce();
        let hello = ClientMessage::Hello(Hello {
            protocol_version: options.protocol_version,
            device_id: options.device_id.to_owned(),
            hostname: format!("host-{}", options.device_id),
            os: "windows".into(),
            client_version: "0.0.0-test".into(),
            key_id: options.key_id.to_owned(),
            nonce: client_nonce.clone(),
        });
        send_raw(&mut socket, &hello).await;

        let server_nonce = match recv_raw(&mut socket).await {
            Some(ServerMessage::Challenge { nonce, mac }) => {
                let valid =
                    verify_mac(options.secret, Role::Coordinator, &client_nonce, &nonce, &mac);
                if !valid && !options.skip_verify {
                    return Err(HandshakeError::BadCoordinatorMac);
                }
                nonce
            }
            Some(ServerMessage::Reject { reason }) => return Err(HandshakeError::Rejected(reason)),
            other => panic!("expected challenge, got {other:?}"),
        };

        let mac = compute_mac(options.secret, Role::Client, &client_nonce, &server_nonce);
        send_raw(&mut socket, &ClientMessage::Auth { mac }).await;
        match recv_raw(&mut socket).await {
            Some(ServerMessage::Welcome { heartbeat_interval_ms }) => Ok(FakeClient {
                socket,
                device_id: options.device_id.to_owned(),
                heartbeat_interval_ms,
            }),
            Some(ServerMessage::Reject { reason }) => Err(HandshakeError::Rejected(reason)),
            None => Err(HandshakeError::Closed),
            other => panic!("expected welcome, got {other:?}"),
        }
    }

    pub async fn send(&mut self, message: ClientMessage) {
        send_raw(&mut self.socket, &message).await;
    }

    pub async fn send_text(&mut self, text: &str) {
        self.socket.send(Message::text(text.to_owned())).await.unwrap();
    }

    /// Next message, or `None` if the connection closed. Panics after [`WAIT`].
    pub async fn recv(&mut self) -> Option<ServerMessage> {
        recv_raw(&mut self.socket).await
    }

    /// Next message if one arrives within `within`.
    pub async fn try_recv(&mut self, within: Duration) -> Option<ServerMessage> {
        tokio::time::timeout(within, recv_inner(&mut self.socket)).await.ok().flatten()
    }

    /// Waits for a `job` message and returns (job_target_id, job_id, kind, payload).
    pub async fn expect_job(&mut self) -> (String, String, String, Value) {
        loop {
            match self.recv().await {
                Some(ServerMessage::Job { job_target_id, job_id, kind, payload }) => {
                    return (job_target_id, job_id, kind, payload)
                }
                Some(ServerMessage::HeartbeatAck) => continue,
                other => panic!("expected job, got {other:?}"),
            }
        }
    }

    pub async fn expect_result_ack(&mut self, target_id: &str) {
        loop {
            match self.recv().await {
                Some(ServerMessage::JobResultAck { job_target_id }) => {
                    assert_eq!(job_target_id, target_id);
                    return;
                }
                Some(ServerMessage::HeartbeatAck) => continue,
                other => panic!("expected job.result_ack, got {other:?}"),
            }
        }
    }

    pub async fn started(&mut self, target_id: &str) {
        self.send(ClientMessage::JobStarted { job_target_id: target_id.to_owned() }).await;
    }

    pub async fn output(&mut self, target_id: &str, seq: u64, data: &str) {
        self.send(ClientMessage::JobOutput {
            job_target_id: target_id.to_owned(),
            seq,
            stream: OutputStream::Stdout,
            data: data.to_owned(),
        })
        .await;
    }

    pub async fn result(&mut self, target_id: &str, exit_code: i32) {
        let failed = exit_code != 0;
        self.send(ClientMessage::JobResult(JobResult {
            job_target_id: target_id.to_owned(),
            status: if failed { JobStatus::Failed } else { JobStatus::Succeeded },
            exit_code: Some(exit_code),
            reason: failed.then(|| protocol::reason::NONZERO_EXIT.to_owned()),
            message: None,
        }))
        .await;
    }

    /// True once the coordinator has closed the connection.
    pub async fn closed_by_server(&mut self) -> bool {
        loop {
            match tokio::time::timeout(WAIT, recv_inner(&mut self.socket)).await {
                Ok(None) => return true,
                Ok(Some(_)) => continue,
                Err(_) => return false,
            }
        }
    }

    pub async fn disconnect(mut self) {
        let _ = self.socket.close(None).await;
    }
}

async fn send_raw(socket: &mut Socket, message: &ClientMessage) {
    socket.send(Message::text(message.to_json())).await.unwrap();
}

async fn recv_raw(socket: &mut Socket) -> Option<ServerMessage> {
    tokio::time::timeout(WAIT, recv_inner(socket))
        .await
        .expect("timed out waiting for a message from the coordinator")
}

async fn recv_inner(socket: &mut Socket) -> Option<ServerMessage> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => {
                return Some(ServerMessage::from_json(text.as_str()).unwrap())
            }
            Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return None,
            Some(Ok(_)) => continue,
        }
    }
}
