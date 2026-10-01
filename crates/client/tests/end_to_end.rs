//! The real client against the real coordinator on localhost, running real
//! scripts. Windows only, because that is the only platform implementation.
#![cfg(windows)]

use std::future::Future;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use client::platform::windows::WindowsPlatform;
use client::{BackoffConfig, Client, ClientConfig};
use coordinator_core::{admin, AuthMode, Config, Coordinator};
use serde_json::{json, Value};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const API_TOKEN: &str = "test-api-token-fixture";
const WAIT: Duration = Duration::from_secs(30);

struct Server {
    coordinator: Coordinator,
    addr: SocketAddr,
    http: reqwest::Client,
}

impl Server {
    async fn start(data_dir: &Path) -> Server {
        let config =
            Config::new("127.0.0.1:0".parse().unwrap(), data_dir, AuthMode::Token(API_TOKEN.into()));
        let coordinator = coordinator_core::start(config).await.unwrap();
        let addr = coordinator.local_addr();
        Server { coordinator, addr, http: reqwest::Client::new() }
    }

    async fn get(&self, path: &str) -> Value {
        let url = format!("http://{}{}", self.addr, path);
        self.http.get(url).bearer_auth(API_TOKEN).send().await.unwrap().json().await.unwrap()
    }

    async fn devices(&self) -> Vec<Value> {
        self.get("/api/v1/devices").await["devices"].as_array().unwrap().clone()
    }

    async fn online(&self, device_id: &str) -> bool {
        self.devices().await.iter().any(|d| d["id"] == device_id && d["online"] == true)
    }

    /// Creates a PowerShell job for all devices; returns its id.
    async fn run_powershell(&self, body: &str) -> String {
        let url = format!("http://{}/api/v1/jobs", self.addr);
        let response = self
            .http
            .post(url)
            .bearer_auth(API_TOKEN)
            .json(&json!({
                "kind": "script.run",
                "payload": {"shell": "powershell", "body": body, "timeout_secs": 60},
                "targets": {"type": "all"},
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 201);
        let body: Value = response.json().await.unwrap();
        body["job"]["id"].as_str().unwrap().to_owned()
    }

    async fn target(&self, job_id: &str) -> Value {
        self.get(&format!("/api/v1/jobs/{job_id}")).await["job"]["targets"][0].clone()
    }

    async fn finished_target(&self, job_id: &str) -> Value {
        let finished = wait_until(|| async {
            let state = self.target(job_id).await["state"].clone();
            state == "succeeded" || state == "failed"
        })
        .await;
        let target = self.target(job_id).await;
        assert!(finished, "job did not finish: {target}");
        target
    }
}

async fn wait_until<F, Fut>(mut condition: F) -> bool
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
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

struct RunningClient {
    device_id: String,
    shutdown: CancellationToken,
    task: JoinHandle<()>,
}

impl RunningClient {
    fn start(addr: SocketAddr, key_id: &str, secret: &str, state_dir: &Path) -> RunningClient {
        let device_id = client::load_or_create_device_id(state_dir).unwrap();
        let mut config =
            ClientConfig::new(format!("ws://{addr}"), key_id, secret, device_id.clone());
        config.state_dir = Some(state_dir.to_owned());
        config.backoff = BackoffConfig {
            initial: Duration::from_millis(50),
            max: Duration::from_millis(200),
            stable_after: Duration::from_secs(30),
        };
        let platform = Arc::new(WindowsPlatform::new(state_dir.join("scripts")));
        let client = Client::new(config, platform).unwrap();
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(client.run(shutdown.clone()));
        RunningClient { device_id, shutdown, task }
    }

    async fn stop(self) {
        self.shutdown.cancel();
        self.task.await.unwrap();
    }
}

#[tokio::test]
async fn device_comes_online_runs_a_script_and_receives_a_job_queued_while_offline() {
    let data_dir = tempfile::tempdir().unwrap();
    let state_dir = tempfile::tempdir().unwrap();
    let server = Server::start(data_dir.path()).await;
    let key = admin::create_enrollment_key(data_dir.path(), "e2e").unwrap();

    let client = RunningClient::start(server.addr, &key.id, &key.secret, state_dir.path());
    let device_id = client.device_id.clone();
    assert!(wait_until(|| server.online(&device_id)).await, "device never came online");

    // A script that succeeds, with output on both streams.
    let job = server
        .run_powershell("Write-Output 'hello from the device'\n[Console]::Error.WriteLine('a warning')\nWrite-Output \"sum=$(40 + 2)\"")
        .await;
    let target = server.finished_target(&job).await;
    assert_eq!(target["state"], "succeeded");
    assert_eq!(target["exit_code"], 0);
    assert_eq!(target["device_id"], device_id.as_str());
    let output = target["output"].as_str().unwrap();
    assert!(output.contains("hello from the device\r\n"), "{output:?}");
    assert!(output.contains("a warning\r\n"), "{output:?}");
    assert!(output.contains("sum=42\r\n"), "{output:?}");

    // A script that fails.
    let job = server.run_powershell("Write-Output 'about to fail'\nexit 7").await;
    let target = server.finished_target(&job).await;
    assert_eq!(target["state"], "failed");
    assert_eq!(target["exit_code"], 7);
    assert_eq!(target["reason"], "nonzero_exit");
    assert_eq!(target["output"], "about to fail\r\n");

    // A job created while the device is offline waits for it.
    client.stop().await;
    assert!(wait_until(|| async { !server.online(&device_id).await }).await);
    let job = server.run_powershell("Write-Output 'queued while offline'").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(server.target(&job).await["state"], "queued");

    let client = RunningClient::start(server.addr, &key.id, &key.secret, state_dir.path());
    assert_eq!(client.device_id, device_id, "the device id is persisted");
    let target = server.finished_target(&job).await;
    assert_eq!(target["state"], "succeeded");
    assert_eq!(target["output"], "queued while offline\r\n");

    client.stop().await;
    server.coordinator.stop().await;
}

#[tokio::test]
async fn client_with_the_wrong_secret_never_comes_online() {
    let data_dir = tempfile::tempdir().unwrap();
    let state_dir = tempfile::tempdir().unwrap();
    let server = Server::start(data_dir.path()).await;
    let key = admin::create_enrollment_key(data_dir.path(), "e2e").unwrap();

    let wrong_secret = "f".repeat(64);
    assert_ne!(wrong_secret, key.secret);
    let client = RunningClient::start(server.addr, &key.id, &wrong_secret, state_dir.path());
    // Long enough for several attempts with the test backoff.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(server.devices().await.is_empty());
    client.stop().await;

    // The same device with the right secret is accepted.
    let client = RunningClient::start(server.addr, &key.id, &key.secret, state_dir.path());
    let device_id = client.device_id.clone();
    assert!(wait_until(|| server.online(&device_id)).await);
    client.stop().await;
    server.coordinator.stop().await;
}
