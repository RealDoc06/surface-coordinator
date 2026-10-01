//! Test harness: a fake coordinator listening on a random port and a fake
//! platform whose scripts are controlled by the test.
#![allow(dead_code)]

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use client::platform::{
    OutputChunk, Platform, PlatformError, ScriptOutcome, ScriptRequest, SystemInfo,
    WifiConnection, WifiJoinRequest,
};
use client::{BackoffConfig, Client, ClientConfig};
use futures_util::{SinkExt, StreamExt};
use protocol::handshake::{compute_mac, generate_nonce, verify_mac, Role};
use protocol::{ClientMessage, Hello, JobResult, OutputStream, PowerAction, ServerMessage};
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Semaphore};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use tokio_util::sync::CancellationToken;

/// Obviously fake fixtures; never real enrollment secrets.
pub const KEY_ID: &str = "test-key-id";
pub const SECRET: &str = "test-fixture-secret-not-a-real-key";
pub const DEVICE_ID: &str = "test-device";
pub const WAIT: Duration = Duration::from_secs(5);

// ------------------------------------------------------------ platform

/// Scripts are tiny commands, one per line of the body:
///
/// - `out <text>` / `err <text>`: emit a chunk on stdout / stderr
/// - `wait`: block until the test calls [`FakePlatform::release`]
/// - `exit <code>`: finish with this exit code
/// - `timeout`: finish as timed out
/// - `unsupported`: fail as unsupported
/// - `no-session`: fail because nobody is logged in
/// - `fail <text>`: fail with an error
///
/// A script that runs off its end exits with 0. A script stopped before its
/// end, its future dropped, is counted: see [`FakePlatform::scripts_dropped`].
///
/// Wi-Fi is unsupported unless the platform is made with
/// [`FakePlatform::with_wifi`]; then the test decides which networks are in
/// range, which one the device is on and how joining each one turns out.
///
/// Launching apps, applying settings and power actions succeed and are
/// recorded unless the test says otherwise. Nothing real happens.
pub struct FakePlatform {
    runs: Mutex<Vec<ScriptRequest>>,
    scripts_dropped: AtomicUsize,
    gate: Semaphore,
    wifi: Mutex<FakeWifi>,
    launches: Mutex<Vec<(String, Vec<String>)>>,
    launch_outcomes: Mutex<HashMap<String, Result<u32, PlatformError>>>,
    applied: Mutex<Vec<(String, Value)>>,
    apply_outcomes: Mutex<HashMap<String, Result<(), PlatformError>>>,
    /// Applying a setting waits for [`FakePlatform::release`].
    hold_applies: AtomicBool,
    power_supported: AtomicBool,
    power_calls: Mutex<Vec<(PowerAction, Instant)>>,
}

/// Counts a script whose future is dropped before it finished.
struct DropWatch<'a> {
    counter: &'a AtomicUsize,
    finished: bool,
}

impl Drop for DropWatch<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.counter.fetch_add(1, Ordering::SeqCst);
        }
    }
}

#[derive(Default)]
struct FakeWifi {
    supported: bool,
    visible: Vec<String>,
    current: Option<WifiConnection>,
    /// On success the device is then on the given connection.
    outcomes: HashMap<String, Result<WifiConnection, PlatformError>>,
    scans: Vec<Instant>,
    joins: Vec<WifiJoinRequest>,
}

impl FakePlatform {
    pub fn new() -> Arc<FakePlatform> {
        Arc::new(FakePlatform {
            runs: Mutex::new(Vec::new()),
            scripts_dropped: AtomicUsize::new(0),
            gate: Semaphore::new(0),
            wifi: Mutex::new(FakeWifi::default()),
            launches: Mutex::new(Vec::new()),
            launch_outcomes: Mutex::new(HashMap::new()),
            applied: Mutex::new(Vec::new()),
            apply_outcomes: Mutex::new(HashMap::new()),
            hold_applies: AtomicBool::new(false),
            power_supported: AtomicBool::new(true),
            power_calls: Mutex::new(Vec::new()),
        })
    }

    /// Scripts stopped before their end because their future was dropped.
    pub fn scripts_dropped(&self) -> usize {
        self.scripts_dropped.load(Ordering::SeqCst)
    }

    /// What launching `path` returns. Other paths start with pid 4242.
    pub fn on_launch(&self, path: &str, outcome: Result<u32, PlatformError>) {
        self.launch_outcomes.lock().unwrap().insert(path.to_owned(), outcome);
    }

    pub fn launches(&self) -> Vec<(String, Vec<String>)> {
        self.launches.lock().unwrap().clone()
    }

    /// What applying `setting` returns. Other settings apply fine.
    pub fn on_apply(&self, setting: &str, outcome: Result<(), PlatformError>) {
        self.apply_outcomes.lock().unwrap().insert(setting.to_owned(), outcome);
    }

    /// Makes every setting wait for [`FakePlatform::release`] before it is
    /// applied.
    pub fn hold_applies(&self) {
        self.hold_applies.store(true, Ordering::SeqCst);
    }

    pub fn applied(&self) -> Vec<(String, Value)> {
        self.applied.lock().unwrap().clone()
    }

    /// Makes power actions unsupported, as on a platform without them.
    pub fn without_power(&self) {
        self.power_supported.store(false, Ordering::SeqCst);
    }

    /// Power actions carried out, and when.
    pub fn power_calls(&self) -> Vec<(PowerAction, Instant)> {
        self.power_calls.lock().unwrap().clone()
    }

    /// A platform with Wi-Fi: nothing in range and not connected until the
    /// test says otherwise.
    pub fn with_wifi() -> Arc<FakePlatform> {
        let platform = FakePlatform::new();
        platform.wifi.lock().unwrap().supported = true;
        platform
    }

    pub fn set_visible(&self, ssids: &[&str]) {
        self.wifi.lock().unwrap().visible = ssids.iter().map(|s| s.to_string()).collect();
    }

    pub fn set_current(&self, current: Option<WifiConnection>) {
        self.wifi.lock().unwrap().current = current;
    }

    pub fn current(&self) -> Option<WifiConnection> {
        self.wifi.lock().unwrap().current.clone()
    }

    /// What joining `ssid` does. Joining a network without an outcome fails.
    pub fn on_join(&self, ssid: &str, outcome: Result<WifiConnection, PlatformError>) {
        self.wifi.lock().unwrap().outcomes.insert(ssid.to_owned(), outcome);
    }

    /// When each scan happened.
    pub fn scans(&self) -> Vec<Instant> {
        self.wifi.lock().unwrap().scans.clone()
    }

    pub fn joins(&self) -> Vec<WifiJoinRequest> {
        self.wifi.lock().unwrap().joins.clone()
    }

    fn wifi_supported(&self) -> Result<std::sync::MutexGuard<'_, FakeWifi>, PlatformError> {
        let wifi = self.wifi.lock().unwrap();
        if wifi.supported {
            Ok(wifi)
        } else {
            Err(PlatformError::Unsupported("no Wi-Fi on this fake".into()))
        }
    }

    /// Requests seen so far, in order.
    pub fn runs(&self) -> Vec<ScriptRequest> {
        self.runs.lock().unwrap().clone()
    }

    /// Lets one `wait` step continue.
    pub fn release(&self) {
        self.gate.add_permits(1);
    }
}

impl Platform for FakePlatform {
    fn system_info(&self) -> SystemInfo {
        SystemInfo { hostname: "FAKE-HOST".into(), os: "fake-os".into() }
    }

    async fn run_script(
        &self,
        request: ScriptRequest,
        output: mpsc::Sender<OutputChunk>,
    ) -> Result<ScriptOutcome, PlatformError> {
        self.runs.lock().unwrap().push(request.clone());
        let mut watch = DropWatch { counter: &self.scripts_dropped, finished: false };
        let outcome = self.interpret(&request, &output).await;
        watch.finished = true;
        outcome
    }

    async fn wifi_scan(&self) -> Result<Vec<String>, PlatformError> {
        let mut wifi = self.wifi_supported()?;
        wifi.scans.push(Instant::now());
        Ok(wifi.visible.clone())
    }

    async fn wifi_join(&self, request: WifiJoinRequest) -> Result<(), PlatformError> {
        let mut wifi = self.wifi_supported()?;
        wifi.joins.push(request.clone());
        match wifi.outcomes.get(&request.ssid).cloned() {
            Some(Ok(connection)) => {
                wifi.current = Some(connection);
                Ok(())
            }
            Some(Err(error)) => Err(error),
            None => Err(PlatformError::Failed(format!("{} is not in range", request.ssid))),
        }
    }

    async fn wifi_current(&self) -> Result<Option<WifiConnection>, PlatformError> {
        Ok(self.wifi_supported()?.current.clone())
    }

    async fn launch_app(&self, path: String, arguments: Vec<String>) -> Result<u32, PlatformError> {
        self.launches.lock().unwrap().push((path.clone(), arguments));
        self.launch_outcomes.lock().unwrap().get(&path).cloned().unwrap_or(Ok(4242))
    }

    async fn apply_setting(&self, setting: String, value: Value) -> Result<(), PlatformError> {
        if self.hold_applies.load(Ordering::SeqCst) {
            self.gate.acquire().await.unwrap().forget();
        }
        self.applied.lock().unwrap().push((setting.clone(), value));
        self.apply_outcomes.lock().unwrap().get(&setting).cloned().unwrap_or(Ok(()))
    }

    fn check_power(&self, _action: PowerAction) -> Result<(), PlatformError> {
        if self.power_supported.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(PlatformError::Unsupported("no power actions on this fake".into()))
        }
    }

    async fn power(&self, action: PowerAction) -> Result<(), PlatformError> {
        self.power_calls.lock().unwrap().push((action, Instant::now()));
        Ok(())
    }
}

impl FakePlatform {
    async fn interpret(
        &self,
        request: &ScriptRequest,
        output: &mpsc::Sender<OutputChunk>,
    ) -> Result<ScriptOutcome, PlatformError> {
        for line in request.body.lines() {
            let (command, argument) = line.split_once(' ').unwrap_or((line, ""));
            match command {
                "out" | "err" => {
                    let stream =
                        if command == "out" { OutputStream::Stdout } else { OutputStream::Stderr };
                    let _ = output.send(OutputChunk { stream, data: format!("{argument}\n") }).await;
                }
                "wait" => self.gate.acquire().await.unwrap().forget(),
                "exit" => return Ok(ScriptOutcome::Exited(argument.parse().unwrap())),
                "timeout" => return Ok(ScriptOutcome::TimedOut),
                "unsupported" => return Err(PlatformError::Unsupported("not here".into())),
                "no-session" => return Err(PlatformError::NoUserSession("nobody logged in".into())),
                "fail" => return Err(PlatformError::Failed(argument.to_owned())),
                other => panic!("unknown fake script command {other:?}"),
            }
        }
        Ok(ScriptOutcome::Exited(0))
    }
}

/// A Wi-Fi connection whose gateway is this machine, so that a
/// [`FakeCoordinator`] can play the coordinator at the gateway.
pub fn wifi_on(ssid: &str) -> WifiConnection {
    WifiConnection {
        ssid: ssid.to_owned(),
        ip: Some(IpAddr::V4(Ipv4Addr::new(192, 168, 137, 23))),
        gateway: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
    }
}

// ------------------------------------------------------------ client

pub struct RunningClient {
    pub shutdown: CancellationToken,
    pub task: JoinHandle<()>,
}

impl RunningClient {
    pub async fn stop(self) {
        self.shutdown.cancel();
        tokio::time::timeout(WAIT, self.task).await.expect("client stops promptly").unwrap();
    }
}

/// Configuration with short timings. Without Wi-Fi on the platform the
/// network fallback never gets anywhere, so only the coordinator at `addr`
/// matters.
pub fn client_config(addr: SocketAddr) -> ClientConfig {
    ClientConfig {
        coordinator_url: Some(format!("ws://{addr}")),
        key_id: KEY_ID.into(),
        key_secret: SECRET.into(),
        device_id: DEVICE_ID.into(),
        client_version: "9.9.9-test".into(),
        state_dir: None,
        backoff: BackoffConfig {
            initial: Duration::from_millis(20),
            max: Duration::from_millis(100),
            stable_after: Duration::from_secs(30),
        },
        handshake_timeout: Duration::from_secs(5),
        search_after: Duration::from_millis(300),
        search_interval: Duration::from_millis(100),
        handoff_timeout: Duration::from_millis(700),
        hotspot_port: protocol::EMBEDDED_CLIENT_PORT,
        power_ack_timeout: Duration::from_millis(200),
    }
}

/// A coordinator that cannot be reached: it hangs up on every connection
/// at once. A closed port would do too, but on Windows a refused connection
/// to localhost takes about two seconds, which would hide the timing under
/// test.
pub struct Unreachable {
    pub addr: SocketAddr,
    task: JoinHandle<()>,
}

impl Unreachable {
    pub async fn start() -> Unreachable {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });
        Unreachable { addr, task }
    }

    pub fn url(&self) -> String {
        format!("ws://{}", self.addr)
    }
}

impl Drop for Unreachable {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub fn start_client(config: ClientConfig, platform: Arc<FakePlatform>) -> RunningClient {
    let shutdown = CancellationToken::new();
    let client = Client::new(config, platform).unwrap();
    let task = tokio::spawn(client.run(shutdown.clone()));
    RunningClient { shutdown, task }
}

pub fn start_client_with_state(
    addr: SocketAddr,
    state_dir: &Path,
    platform: Arc<FakePlatform>,
) -> RunningClient {
    let mut config = client_config(addr);
    config.state_dir = Some(state_dir.to_owned());
    start_client(config, platform)
}

// ------------------------------------------------------------ coordinator

pub struct FakeCoordinator {
    listener: TcpListener,
    pub addr: SocketAddr,
    /// Announced in `welcome`.
    pub heartbeat_interval_ms: u64,
}

pub struct Connection {
    socket: WebSocketStream<TcpStream>,
    /// Heartbeats seen while waiting for other messages.
    pub heartbeats: usize,
}

impl FakeCoordinator {
    pub async fn start() -> FakeCoordinator {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        FakeCoordinator { listener, addr, heartbeat_interval_ms: 30_000 }
    }

    /// Accepts a connection without speaking.
    pub async fn accept_raw(&self) -> Connection {
        let (stream, _) = tokio::time::timeout(WAIT, self.listener.accept())
            .await
            .expect("timed out waiting for the client to connect")
            .unwrap();
        let socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        Connection { socket, heartbeats: 0 }
    }

    /// True if no connection attempt arrives within `within`.
    pub async fn no_connection_within(&self, within: Duration) -> bool {
        tokio::time::timeout(within, self.listener.accept()).await.is_err()
    }

    /// Accepts a connection and completes the handshake as a coordinator
    /// holding the right key. Returns the connection and the client's hello.
    pub async fn accept(&self) -> (Connection, Hello) {
        let mut connection = self.accept_raw().await;
        let hello = connection.expect_hello().await;
        let server_nonce = generate_nonce();
        connection
            .send(ServerMessage::Challenge {
                mac: compute_mac(SECRET, Role::Coordinator, &hello.nonce, &server_nonce),
                nonce: server_nonce.clone(),
            })
            .await;
        match connection.recv().await {
            Some(ClientMessage::Auth { mac }) => {
                assert!(
                    verify_mac(SECRET, Role::Client, &hello.nonce, &server_nonce, &mac),
                    "client sent a MAC that does not verify"
                );
            }
            other => panic!("expected auth, got {other:?}"),
        }
        connection
            .send(ServerMessage::Welcome {
                heartbeat_interval_ms: self.heartbeat_interval_ms,
                restricted: false,
            })
            .await;
        (connection, hello)
    }
}

impl Connection {
    pub async fn send(&mut self, message: ServerMessage) {
        self.socket.send(Message::text(message.to_json())).await.unwrap();
    }

    pub async fn send_text(&mut self, text: &str) {
        self.socket.send(Message::text(text.to_owned())).await.unwrap();
    }

    async fn recv_any(&mut self) -> Option<ClientMessage> {
        loop {
            match self.socket.next().await {
                Some(Ok(Message::Text(text))) => {
                    return Some(ClientMessage::from_json(text.as_str()).unwrap())
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return None,
                Some(Ok(_)) => continue,
            }
        }
    }

    /// Next message including heartbeats; `None` when the client has gone.
    pub async fn recv_with_heartbeats(&mut self) -> Option<ClientMessage> {
        tokio::time::timeout(WAIT, self.recv_any())
            .await
            .expect("timed out waiting for a message from the client")
    }

    /// Next message that is not a heartbeat; `None` when the client has gone.
    pub async fn recv(&mut self) -> Option<ClientMessage> {
        loop {
            match self.recv_with_heartbeats().await {
                Some(ClientMessage::Heartbeat) => self.heartbeats += 1,
                other => return other,
            }
        }
    }

    /// Next non-heartbeat message if one arrives within `within`.
    pub async fn try_recv(&mut self, within: Duration) -> Option<ClientMessage> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            match tokio::time::timeout_at(deadline, self.recv_any()).await {
                Ok(Some(ClientMessage::Heartbeat)) => self.heartbeats += 1,
                Ok(other) => return other,
                Err(_) => return None,
            }
        }
    }

    pub async fn expect_hello(&mut self) -> Hello {
        match self.recv().await {
            Some(ClientMessage::Hello(hello)) => hello,
            other => panic!("expected hello, got {other:?}"),
        }
    }

    pub async fn send_script(&mut self, target_id: &str, body: &str) {
        self.send_job(target_id, "script.run", json!({"shell": "powershell", "body": body})).await;
    }

    pub async fn send_job(&mut self, target_id: &str, kind: &str, payload: Value) {
        self.send(ServerMessage::Job {
            job_target_id: target_id.to_owned(),
            job_id: format!("job-of-{target_id}"),
            kind: kind.to_owned(),
            payload,
        })
        .await;
    }

    pub async fn cancel(&mut self, target_id: &str) {
        self.send(ServerMessage::JobCancel { job_target_id: target_id.to_owned() }).await;
    }

    pub async fn ack(&mut self, target_id: &str) {
        self.send(ServerMessage::JobResultAck { job_target_id: target_id.to_owned() }).await;
    }

    pub async fn expect_started(&mut self, target_id: &str) {
        assert_eq!(
            self.recv().await,
            Some(ClientMessage::JobStarted { job_target_id: target_id.to_owned() })
        );
    }

    pub async fn expect_output(&mut self, target_id: &str, seq: u64, stream: OutputStream, data: &str) {
        assert_eq!(
            self.recv().await,
            Some(ClientMessage::JobOutput {
                job_target_id: target_id.to_owned(),
                seq,
                stream,
                data: data.to_owned(),
            })
        );
    }

    pub async fn expect_result(&mut self, target_id: &str) -> JobResult {
        match self.recv().await {
            Some(ClientMessage::JobResult(result)) => {
                assert_eq!(result.job_target_id, target_id);
                result
            }
            other => panic!("expected job.result for {target_id}, got {other:?}"),
        }
    }

    /// Drops the TCP connection without a close handshake.
    pub fn drop_abruptly(self) {
        drop(self.socket);
    }

    /// True once the client has closed the connection.
    pub async fn closed_by_client(&mut self) -> bool {
        loop {
            match tokio::time::timeout(WAIT, self.recv_any()).await {
                Ok(None) => return true,
                Ok(Some(_)) => continue,
                Err(_) => return false,
            }
        }
    }
}
