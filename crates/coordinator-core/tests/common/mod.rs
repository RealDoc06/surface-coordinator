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
    /// Keeps cookies, like a browser.
    pub http: reqwest::Client,
    accounts: bool,
}

pub fn test_config(data_dir: &std::path::Path) -> Config {
    let mut config = Config::new(
        "127.0.0.1:0".parse().unwrap(),
        data_dir,
        AuthMode::Token(API_TOKEN.into()),
    );
    config.expiry_sweep_interval = Duration::from_millis(50);
    // Tests never contact a real release source; the update tests turn
    // checking on against a local fake server with a throwaway key.
    config.updates.enabled = false;
    config.update_public_key = None;
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
        let accounts = matches!(config.auth, AuthMode::Accounts);
        let coordinator = coordinator_core::start(config).await.unwrap();
        let addr = coordinator.local_addr();
        TestServer { coordinator: Some(coordinator), data_dir, addr, http: Self::client(), accounts }
    }

    /// A server in accounts mode with one account. Nobody is logged in.
    pub async fn start_accounts(username: &str, password: &str) -> Self {
        let server = Self::start_with(|c| c.auth = AuthMode::Accounts).await;
        admin::create_account(server.data_dir.path(), username, password).unwrap();
        server
    }

    /// An HTTP client with its own cookie jar.
    pub fn client() -> reqwest::Client {
        reqwest::Client::builder().cookie_store(true).build().unwrap()
    }

    /// Stops the server (if it is running) and starts a new one on the same
    /// data directory.
    pub async fn restart(&mut self) {
        self.restart_with(|_| {}).await;
    }

    /// Like [`restart`](Self::restart), with the configuration adjusted
    /// after the defaults (and the auth mode the server started with).
    pub async fn restart_with(&mut self, adjust: impl FnOnce(&mut Config)) {
        if let Some(coordinator) = self.coordinator.take() {
            coordinator.stop().await;
        }
        let mut config = test_config(self.data_dir.path());
        if self.accounts {
            config.auth = AuthMode::Accounts;
        }
        adjust(&mut config);
        let coordinator = coordinator_core::start(config).await.unwrap();
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

    /// Any method, with the API token and an optional JSON body. The body of
    /// an empty response (204) comes back as `null`.
    pub async fn request(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut request = self.http.request(method, self.url(path)).bearer_auth(API_TOKEN);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    pub async fn put(&self, path: &str, body: Value) -> (u16, Value) {
        self.request(reqwest::Method::PUT, path, Some(body)).await
    }

    pub async fn patch(&self, path: &str, body: Value) -> (u16, Value) {
        self.request(reqwest::Method::PATCH, path, Some(body)).await
    }

    pub async fn delete(&self, path: &str) -> (u16, Value) {
        self.request(reqwest::Method::DELETE, path, None).await
    }

    /// Creates a group and returns it.
    pub async fn create_group(&self, name: &str) -> Value {
        let (status, body) = self.post("/api/v1/groups", json!({"name": name})).await;
        assert_eq!(status, 201, "create group failed: {body}");
        body["group"].clone()
    }

    /// Replaces a group's members and returns the group.
    pub async fn set_members(&self, group_id: &str, device_ids: &[&str]) -> Value {
        let (status, body) = self
            .put(&format!("/api/v1/groups/{group_id}/members"), json!({"device_ids": device_ids}))
            .await;
        assert_eq!(status, 200, "set members failed: {body}");
        body["group"].clone()
    }

    /// Creates a job of any kind; returns the status and body.
    pub async fn create_job(&self, kind: &str, payload: Value, targets: Value) -> (u16, Value) {
        self.post("/api/v1/jobs", json!({"kind": kind, "payload": payload, "targets": targets})).await
    }

    /// Cancels a job and returns the status and body.
    pub async fn cancel(&self, job_id: &str) -> (u16, Value) {
        self.post(&format!("/api/v1/jobs/{job_id}/cancel"), json!({})).await
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

    /// Waits until the target of a job on `device_id` is in `state`.
    pub async fn wait_for_target_state_of(&self, job_id: &str, device_id: &str, state: &str) -> Value {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let job = self.job(job_id).await;
            let target = job["targets"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["device_id"] == device_id)
                .unwrap_or_else(|| panic!("no target for {device_id}"))
                .clone();
            if target["state"] == state {
                return target;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "target of {device_id} never reached state {state}; last seen: {target}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
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

// ---------------------------------------------------------------- client updates

/// A throwaway Ed25519 key pair, generated per test.
pub struct Keys {
    pub signing: String,
    pub public: String,
}

impl Keys {
    pub fn generate() -> Keys {
        use base64::Engine;
        let signing = base64::engine::general_purpose::STANDARD.encode(rand::random::<[u8; 32]>());
        let public = protocol::update::public_key_for(&signing).unwrap();
        Keys { signing, public }
    }

    pub fn sign(&self, version: &str, target: &str, artifact: &[u8]) -> String {
        use protocol::update;
        let sha256 = update::parse_sha256_hex(&update::sha256_hex(artifact)).unwrap();
        update::sign(&self.signing, version, target, &sha256).unwrap()
    }
}

/// Bytes standing in for the client executable of `version`; large enough
/// to arrive in several chunks.
pub fn artifact(version: &str) -> Vec<u8> {
    let mut bytes = format!("MZ fake client {version}\n").into_bytes();
    bytes.extend((0..200_000u32).map(|i| (i % 251) as u8));
    bytes
}

impl TestServer {
    /// `POST /api/v1/updates/upload` with the given query, signature header
    /// (if any) and body.
    pub async fn upload_raw(&self, query: &[(&str, &str)], signature: Option<&str>, body: Vec<u8>) -> (u16, Value) {
        let mut request = self
            .http
            .post(self.url("/api/v1/updates/upload"))
            .query(query)
            .bearer_auth(API_TOKEN)
            .header("content-type", "application/octet-stream")
            .body(body);
        if let Some(signature) = signature {
            request = request.header("x-signature", signature);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    /// Uploads a correctly signed release of `version` and returns `Updates`.
    pub async fn upload_release(&self, keys: &Keys, version: &str) -> Value {
        let bytes = artifact(version);
        let signature = keys.sign(version, protocol::update::TARGET_WINDOWS_X86_64, &bytes);
        let query = [("version", version), ("target", protocol::update::TARGET_WINDOWS_X86_64)];
        let (status, body) = self.upload_raw(&query, Some(&signature), bytes).await;
        assert_eq!(status, 201, "upload failed: {body}");
        body
    }

    pub async fn updates(&self) -> Value {
        let (status, body) = self.get("/api/v1/updates").await;
        assert_eq!(status, 200, "{body}");
        body
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
    /// What the welcome said: only `client.update` jobs will come.
    pub restricted: bool,
}

pub struct HandshakeOptions<'a> {
    pub key_id: &'a str,
    /// Secret the client uses for both verification and its own MAC.
    pub secret: &'a str,
    pub device_id: &'a str,
    pub protocol_version: u32,
    /// Reported in hello.
    pub client_version: &'a str,
    /// Skip verifying the coordinator, as a hostile client would.
    pub skip_verify: bool,
    /// Network reported in hello.
    pub network: Option<protocol::NetworkInfo>,
}

impl<'a> HandshakeOptions<'a> {
    pub fn new(key: &'a EnrollmentKey, device_id: &'a str) -> Self {
        HandshakeOptions {
            key_id: &key.id,
            secret: &key.secret,
            device_id,
            protocol_version: protocol::PROTOCOL_VERSION,
            client_version: "0.0.0-test",
            skip_verify: false,
            network: None,
        }
    }
}

/// Connects a device reporting `version` as its client version.
pub async fn connect_as(server: &TestServer, key: &EnrollmentKey, id: &str, version: &str) -> FakeClient {
    let mut options = HandshakeOptions::new(key, id);
    options.client_version = version;
    FakeClient::handshake(server.addr, options).await.expect("handshake")
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
            client_version: options.client_version.to_owned(),
            key_id: options.key_id.to_owned(),
            nonce: client_nonce.clone(),
            network: options.network.clone(),
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
            Some(ServerMessage::Welcome { heartbeat_interval_ms, restricted }) => Ok(FakeClient {
                socket,
                device_id: options.device_id.to_owned(),
                heartbeat_interval_ms,
                restricted,
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

    /// Waits for a `job.cancel` and returns the target it names.
    pub async fn expect_cancel(&mut self) -> String {
        loop {
            match self.recv().await {
                Some(ServerMessage::JobCancel { job_target_id }) => return job_target_id,
                Some(ServerMessage::HeartbeatAck) => continue,
                other => panic!("expected job.cancel, got {other:?}"),
            }
        }
    }

    /// Reports a target as stopped on request, the way a real client answers `job.cancel`.
    pub async fn result_cancelled(&mut self, target_id: &str) {
        self.send(ClientMessage::JobResult(JobResult {
            job_target_id: target_id.to_owned(),
            status: JobStatus::Failed,
            exit_code: None,
            reason: Some(protocol::reason::CANCELLED.to_owned()),
            message: Some("cancelled by the operator".into()),
        }))
        .await;
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
