//! Client updates: the release cache fed from a (fake) release source, the
//! download routes for clients, `client.update` jobs, and clients on a
//! protocol version this coordinator does not support.

mod common;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use base64::Engine;
use common::{wait_until, FakeClient, HandshakeOptions, TestServer, API_TOKEN, WAIT};
use coordinator_core::Config;
use futures_util::StreamExt;
use protocol::update::{self, TARGET_WINDOWS_X86_64};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

const SOURCE: &str = "fixture-owner/fixture-repo";

// ---------------------------------------------------------------- fake release source

/// A throwaway Ed25519 key pair, generated per test.
struct Keys {
    signing: String,
    public: String,
}

impl Keys {
    fn generate() -> Keys {
        let signing = base64::engine::general_purpose::STANDARD.encode(rand::random::<[u8; 32]>());
        let public = update::public_key_for(&signing).unwrap();
        Keys { signing, public }
    }

    fn sign(&self, version: &str, target: &str, artifact: &[u8]) -> String {
        let sha256 = update::parse_sha256_hex(&update::sha256_hex(artifact)).unwrap();
        update::sign(&self.signing, version, target, &sha256).unwrap()
    }
}

/// Bytes standing in for the client executable of `version`; large enough
/// to arrive in several chunks.
fn artifact(version: &str) -> Vec<u8> {
    let mut bytes = format!("MZ fake client {version}\n").into_bytes();
    bytes.extend((0..200_000u32).map(|i| (i % 251) as u8));
    bytes
}

fn artifact_name(version: &str) -> String {
    format!("surface-coordinator-client-{version}-windows-x86_64.exe")
}

#[derive(Default)]
struct Published {
    /// Body of `releases/latest`; `None` answers 404 like a repository
    /// without releases.
    latest: Option<Value>,
    assets: HashMap<String, Vec<u8>>,
    /// Paths requested, in order.
    requests: Vec<String>,
    /// Headers of the last `releases/latest` request.
    api_headers: Option<HeaderMap>,
}

/// Stands in for the GitHub releases API and its asset downloads.
struct FakeReleases {
    addr: SocketAddr,
    published: Arc<Mutex<Published>>,
    keys: Keys,
}

type Shared = Arc<Mutex<Published>>;

async fn latest(
    State(published): State<Shared>,
    Path((owner, repo)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let mut published = published.lock().unwrap();
    published.requests.push(format!("/repos/{owner}/{repo}/releases/latest"));
    published.api_headers = Some(headers);
    match &published.latest {
        Some(release) if format!("{owner}/{repo}") == SOURCE => Json(release.clone()).into_response(),
        _ => (StatusCode::NOT_FOUND, Json(json!({"message": "Not Found"}))).into_response(),
    }
}

async fn asset(State(published): State<Shared>, Path(name): Path<String>) -> Response {
    let mut published = published.lock().unwrap();
    published.requests.push(format!("/assets/{name}"));
    match published.assets.get(&name) {
        Some(bytes) => bytes.clone().into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

impl FakeReleases {
    async fn start() -> FakeReleases {
        let published: Shared = Arc::default();
        let app = Router::new()
            .route("/repos/{owner}/{repo}/releases/latest", get(latest))
            .route("/assets/{name}", get(asset))
            .with_state(published.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        FakeReleases { addr, published, keys: Keys::generate() }
    }

    /// Points a coordinator at this source and trusts its key.
    fn configure(&self, config: &mut Config) {
        config.updates.enabled = true;
        config.updates.api_base = format!("http://{}", self.addr);
        config.updates.source = SOURCE.into();
        config.update_public_key = Some(self.keys.public.clone());
    }

    fn asset_url(&self, name: &str) -> String {
        format!("http://{}/assets/{name}", self.addr)
    }

    /// Publishes a correctly signed release.
    fn publish(&self, version: &str, notes: &str) -> Vec<u8> {
        let bytes = artifact(version);
        let signature = self.keys.sign(version, TARGET_WINDOWS_X86_64, &bytes);
        self.publish_with(version, notes, bytes.clone(), &signature);
        bytes
    }

    /// Publishes a release with the given artifact and signature, plus an
    /// unrelated asset as real releases have.
    fn publish_with(&self, version: &str, notes: &str, bytes: Vec<u8>, signature: &str) {
        let name = artifact_name(version);
        let sig_name = format!("{name}.sig");
        let release = json!({
            "tag_name": format!("v{version}"),
            "name": format!("Surface Coordinator {version}"),
            "body": notes,
            "assets": [
                {"name": "manager-setup.exe", "browser_download_url": self.asset_url("manager-setup.exe")},
                {"name": name, "browser_download_url": self.asset_url(&name)},
                {"name": sig_name, "browser_download_url": self.asset_url(&sig_name)},
            ],
        });
        let mut published = self.published.lock().unwrap();
        published.assets.insert(name, bytes);
        published.assets.insert(sig_name, signature.as_bytes().to_vec());
        published.latest = Some(release);
    }

    fn publish_raw(&self, release: Value) {
        self.published.lock().unwrap().latest = Some(release);
    }

    fn requests(&self) -> Vec<String> {
        self.published.lock().unwrap().requests.clone()
    }

    fn downloads_of(&self, name: &str) -> usize {
        let path = format!("/assets/{name}");
        self.requests().iter().filter(|request| **request == path).count()
    }

    fn api_header(&self, name: &str) -> Option<String> {
        let published = self.published.lock().unwrap();
        published.api_headers.as_ref()?.get(name).map(|value| value.to_str().unwrap().to_owned())
    }
}

/// Starts a coordinator that checks `releases` and waits for the check it
/// makes at start-up to finish, so a test sees a settled state.
async fn start_checking(releases: &FakeReleases) -> TestServer {
    let server = TestServer::start_with(|config| releases.configure(config)).await;
    wait_for_first_check(&server).await;
    server
}

async fn wait_for_first_check(server: &TestServer) {
    let checked = wait_until(|| async { !updates(server).await["last_checked_at"].is_null() }).await;
    assert!(checked, "the start-up check never finished");
}

async fn updates(server: &TestServer) -> Value {
    let (status, body) = server.get("/api/v1/updates").await;
    assert_eq!(status, 200, "{body}");
    body
}

async fn check(server: &TestServer) -> (u16, Value) {
    server.post("/api/v1/updates/check", json!({})).await
}

fn one_device(id: &str) -> Value {
    json!({"type": "devices", "device_ids": [id]})
}

async fn connect_as(server: &TestServer, key: &coordinator_core::EnrollmentKey, id: &str, version: &str) -> FakeClient {
    let mut options = HandshakeOptions::new(key, id);
    options.client_version = version;
    FakeClient::handshake(server.addr, options).await.expect("handshake")
}

/// Every file below a directory, recursively.
fn files_below(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(files_below(&path));
            } else {
                found.push(path);
            }
        }
    }
    found
}

/// Sends a request with exactly this path, which an HTTP client library
/// would normalise first; returns the status code.
async fn raw_get_status(addr: SocketAddr, path: &str) -> u16 {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(WAIT, stream.read_to_end(&mut response)).await.unwrap().unwrap();
    let response = String::from_utf8_lossy(&response);
    response.split(' ').nth(1).unwrap().parse().unwrap()
}

// ---------------------------------------------------------------- the release cache

#[tokio::test]
async fn a_check_caches_the_latest_release_and_lists_it() {
    let releases = FakeReleases::start().await;
    let bytes = releases.publish("1.2.3", "Fixes the thing.");
    let server = start_checking(&releases).await;

    let (status, body) = check(&server).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["enabled"], true);
    assert_eq!(body["source"], SOURCE);
    assert!(body["last_checked_at"].as_i64().unwrap() > 0);
    assert!(body["last_error"].is_null(), "{body}");
    let latest = &body["latest"];
    assert_eq!(latest["version"], "1.2.3");
    assert_eq!(latest["target"], "windows-x86_64");
    assert_eq!(latest["size"], bytes.len());
    assert_eq!(latest["sha256"], update::sha256_hex(&bytes));
    assert_eq!(latest["notes"], "Fixes the thing.");
    assert!(latest["cached_at"].as_i64().unwrap() > 0);
    assert_eq!(body["cached"], json!([latest.clone()]));
    assert_eq!(updates(&server).await, body);

    // Asked the way the GitHub API expects.
    assert!(releases.requests().contains(&format!("/repos/{SOURCE}/releases/latest")));
    let agent = releases.api_header("user-agent").unwrap();
    assert!(agent.starts_with("surface-coordinator/"), "{agent}");
    assert_eq!(releases.api_header("accept").unwrap(), "application/vnd.github+json");

    // The verified files are in the data directory.
    let dir = server.data_dir.path().join("updates").join("1.2.3");
    assert_eq!(std::fs::read(dir.join("client-windows-x86_64.exe")).unwrap(), bytes);
    let signature = std::fs::read_to_string(dir.join("client-windows-x86_64.exe.sig")).unwrap();
    assert_eq!(signature.trim(), releases.keys.sign("1.2.3", TARGET_WINDOWS_X86_64, &bytes));
}

#[tokio::test]
async fn checking_again_for_a_cached_version_downloads_nothing() {
    let releases = FakeReleases::start().await;
    releases.publish("1.2.3", "");
    let server = start_checking(&releases).await;
    let first = updates(&server).await;
    assert_eq!(first["latest"]["version"], "1.2.3");

    for _ in 0..2 {
        let (status, body) = check(&server).await;
        assert_eq!(status, 200, "{body}");
        assert!(body["last_checked_at"].as_i64() >= first["last_checked_at"].as_i64());
        assert_eq!(body["cached"], first["cached"]);
    }
    assert_eq!(releases.downloads_of(&artifact_name("1.2.3")), 1);
    assert_eq!(releases.downloads_of(&format!("{}.sig", artifact_name("1.2.3"))), 1);
    assert_eq!(
        releases.requests().iter().filter(|r| r.ends_with("/releases/latest")).count(),
        3,
        "the start-up check and the two asked for"
    );
}

#[tokio::test]
async fn a_newer_release_is_cached_alongside_and_listed_first_and_the_ui_is_told_to_refetch() {
    let releases = FakeReleases::start().await;
    releases.publish("1.2.3", "old");
    let server = start_checking(&releases).await;
    let mut events = Events::open(&server).await;

    // 1.10.0 is newer than 1.9.0 and 1.2.3: versions compare numerically.
    releases.publish("1.10.0", "new");
    let (status, body) = check(&server).await;
    assert_eq!(status, 200, "{body}");
    let versions: Vec<&str> = body["cached"].as_array().unwrap().iter().map(|r| r["version"].as_str().unwrap()).collect();
    assert_eq!(versions, ["1.10.0", "1.2.3"]);
    assert_eq!(body["latest"]["version"], "1.10.0");
    assert_eq!(body["latest"]["notes"], "new");
    events.next_of("resync").await;

    releases.publish("1.9.0", "older than the newest");
    let (status, body) = check(&server).await;
    assert_eq!(status, 200, "{body}");
    let versions: Vec<&str> = body["cached"].as_array().unwrap().iter().map(|r| r["version"].as_str().unwrap()).collect();
    assert_eq!(versions, ["1.10.0", "1.9.0", "1.2.3"]);
    assert_eq!(body["latest"]["version"], "1.10.0");
}

/// Asserts the check fails, nothing new is cached and the error is kept.
async fn assert_check_fails(server: &TestServer, mentions: &str) {
    let (status, body) = check(server).await;
    assert_eq!(status, 502, "{body}");
    assert_eq!(body["error"]["code"], "update_check_failed");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.to_lowercase().contains(mentions), "{message:?} should mention {mentions:?}");
    let state = updates(server).await;
    assert_eq!(state["last_error"], message);
    assert!(state["last_checked_at"].as_i64().is_some());
    assert_eq!(state["cached"], json!([]));
    assert!(state["latest"].is_null());
    assert!(files_below(&server.data_dir.path().join("updates")).is_empty(), "nothing unverified is kept");
}

#[tokio::test]
async fn a_release_with_a_bad_signature_is_not_cached() {
    let releases = FakeReleases::start().await;
    let signature = releases.keys.sign("1.2.3", TARGET_WINDOWS_X86_64, b"some other file");
    releases.publish_with("1.2.3", "", artifact("1.2.3"), &signature);
    let server = start_checking(&releases).await;
    assert_check_fails(&server, "signature").await;

    // Signed with a key the coordinator does not trust.
    let stranger = Keys::generate();
    let bytes = artifact("1.2.3");
    releases.publish_with("1.2.3", "", bytes.clone(), &stranger.sign("1.2.3", TARGET_WINDOWS_X86_64, &bytes));
    assert_check_fails(&server, "signature").await;

    // Not a signature at all.
    releases.publish_with("1.2.3", "", bytes, "not base64 at all");
    assert_check_fails(&server, "signature").await;
    assert!(server.get("/api/v1/updates").await.1["enabled"] == true);
}

#[tokio::test]
async fn a_signature_made_for_another_version_or_target_is_refused() {
    let releases = FakeReleases::start().await;
    let bytes = artifact("1.2.3");
    releases.publish_with("1.2.3", "", bytes.clone(), &releases.keys.sign("1.2.4", TARGET_WINDOWS_X86_64, &bytes));
    let server = start_checking(&releases).await;
    assert_check_fails(&server, "signature").await;

    releases.publish_with("1.2.3", "", bytes.clone(), &releases.keys.sign("1.2.3", "linux-x86_64", &bytes));
    assert_check_fails(&server, "signature").await;

    // Once a correct release appears it is cached and the error is gone.
    releases.publish("1.2.3", "");
    let (status, body) = check(&server).await;
    assert_eq!(status, 200, "{body}");
    assert!(body["last_error"].is_null());
    assert_eq!(body["latest"]["version"], "1.2.3");
}

#[tokio::test]
async fn a_release_without_client_assets_is_an_error() {
    let releases = FakeReleases::start().await;
    releases.publish_raw(json!({
        "tag_name": "v1.2.3",
        "body": "manager only",
        "assets": [{"name": "manager-setup.exe", "browser_download_url": releases.asset_url("manager-setup.exe")}],
    }));
    let server = start_checking(&releases).await;
    assert_check_fails(&server, &artifact_name("1.2.3")).await;

    // The artifact without its signature is not enough either.
    let name = artifact_name("1.2.3");
    releases.publish_raw(json!({
        "tag_name": "v1.2.3",
        "body": null,
        "assets": [{"name": name, "browser_download_url": releases.asset_url(&name)}],
    }));
    assert_check_fails(&server, ".sig").await;
    assert_eq!(releases.downloads_of(&name), 0, "nothing is downloaded without its signature");
}

#[tokio::test]
async fn a_release_whose_tag_is_not_a_version_is_an_error() {
    let releases = FakeReleases::start().await;
    releases.publish_raw(json!({"tag_name": "nightly", "body": "", "assets": []}));
    let server = start_checking(&releases).await;
    assert_check_fails(&server, "nightly").await;
}

#[tokio::test]
async fn a_missing_asset_download_is_an_error() {
    let releases = FakeReleases::start().await;
    releases.publish("1.2.3", "");
    releases.published.lock().unwrap().assets.remove(&artifact_name("1.2.3"));
    let server = start_checking(&releases).await;
    assert_check_fails(&server, "404").await;
}

#[tokio::test]
async fn an_unreachable_release_source_fails_the_check_but_not_the_coordinator() {
    // A port nothing listens on any more.
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
    let keys = Keys::generate();
    let server = TestServer::start_with(|config| {
        config.updates.enabled = true;
        config.updates.api_base = format!("http://{closed}");
        config.updates.source = SOURCE.into();
        config.update_public_key = Some(keys.public.clone());
    })
    .await;
    wait_for_first_check(&server).await;

    let (status, body) = check(&server).await;
    assert_eq!(status, 502, "{body}");
    assert_eq!(body["error"]["code"], "update_check_failed");
    assert!(!updates(&server).await["last_error"].is_null());

    // Everything else carries on.
    let key = server.new_key();
    let mut client = FakeClient::connect(&server, &key, "device-a").await;
    server.create_script_job(one_device("device-a")).await;
    assert_eq!(client.expect_job().await.2, "script.run");
}

#[tokio::test]
async fn without_a_key_or_when_turned_off_nothing_is_checked() {
    let releases = FakeReleases::start().await;
    releases.publish("1.2.3", "");

    let no_key = TestServer::start_with(|config| {
        releases.configure(config);
        config.update_public_key = None;
    })
    .await;
    let turned_off = TestServer::start_with(|config| {
        releases.configure(config);
        config.updates.enabled = false;
    })
    .await;

    for server in [&no_key, &turned_off] {
        let body = updates(server).await;
        assert_eq!(body["enabled"], false, "{body}");
        assert!(body["source"].is_null());
        assert!(body["last_checked_at"].is_null());
        assert!(body["latest"].is_null());
        assert_eq!(body["cached"], json!([]));

        let (status, body) = check(server).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["enabled"], false);
        assert!(body["last_checked_at"].is_null());
    }
    // Not at start-up, not on demand.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(releases.requests(), Vec::<String>::new());
}

#[tokio::test]
async fn the_update_routes_need_authentication() {
    let releases = FakeReleases::start().await;
    let server = start_checking(&releases).await;
    let anonymous = TestServer::client();
    assert_eq!(anonymous.get(server.url("/api/v1/updates")).send().await.unwrap().status(), 401);
    assert_eq!(anonymous.post(server.url("/api/v1/updates/check")).send().await.unwrap().status(), 401);
}

#[tokio::test]
async fn the_cache_survives_a_restart_and_entries_whose_files_are_gone_are_dropped() {
    let releases = FakeReleases::start().await;
    releases.publish("1.2.3", "first");
    let mut server = start_checking(&releases).await;
    releases.publish("1.3.0", "second");
    assert_eq!(check(&server).await.0, 200);
    let before = updates(&server).await;
    assert_eq!(before["cached"].as_array().unwrap().len(), 2);

    // Restarted without update checks: the cache is still listed and served.
    server.restart_with(|config| config.updates.enabled = false).await;
    let after = updates(&server).await;
    assert_eq!(after["cached"], before["cached"]);
    assert_eq!(after["latest"]["version"], "1.3.0");
    let response = reqwest::get(server.url("/updates/client/1.2.3/windows-x86_64")).await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.unwrap().to_vec(), artifact("1.2.3"));

    // A version whose file disappeared is forgotten at the next start.
    std::fs::remove_file(server.data_dir.path().join("updates/1.3.0/client-windows-x86_64.exe")).unwrap();
    server.restart_with(|config| config.updates.enabled = false).await;
    let after = updates(&server).await;
    let versions: Vec<&str> = after["cached"].as_array().unwrap().iter().map(|r| r["version"].as_str().unwrap()).collect();
    assert_eq!(versions, ["1.2.3"]);
    assert_eq!(after["latest"]["version"], "1.2.3");
    let response = reqwest::get(server.url("/updates/client/1.3.0/windows-x86_64")).await.unwrap();
    assert_eq!(response.status(), 404);
}

// ---------------------------------------------------------------- downloads for clients

#[tokio::test]
async fn the_download_routes_serve_exactly_the_cached_artifact_and_signature() {
    let releases = FakeReleases::start().await;
    let bytes = releases.publish("1.2.3", "");
    let server = start_checking(&releases).await;
    let signature = releases.keys.sign("1.2.3", TARGET_WINDOWS_X86_64, &bytes);

    // No authentication: clients fetch these.
    let anonymous = reqwest::Client::new();
    let response = anonymous.get(server.url("/updates/client/1.2.3/windows-x86_64")).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-length"], bytes.len().to_string().as_str());
    assert_eq!(response.bytes().await.unwrap().to_vec(), bytes);

    let response = anonymous.get(server.url("/updates/client/1.2.3/windows-x86_64.sig")).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    assert_eq!(text.trim(), signature);
    // What a client does with it.
    let sha256 = update::parse_sha256_hex(&update::sha256_hex(&bytes)).unwrap();
    assert!(update::verify(&releases.keys.public, "1.2.3", TARGET_WINDOWS_X86_64, &sha256, &text).is_ok());

    for path in [
        "/updates/client/9.9.9/windows-x86_64",
        "/updates/client/9.9.9/windows-x86_64.sig",
        "/updates/client/1.2.3/linux-x86_64",
        "/updates/client/1.2.3/windows-x86_64.exe",
        "/updates/client/1.2.3/client-windows-x86_64.exe",
        "/updates/client/1.2/windows-x86_64",
        "/updates/client/v1.2.3/windows-x86_64",
        "/updates/client/01.2.3/windows-x86_64",
        "/updates/client/1.2.3/windows-x86_64.sig.sig",
        "/updates/client/1.2.3/..%2F..%2Fcoordinator.db",
        "/updates/client/1.2.3/%2E%2E",
        "/updates/client/1.2.3/..%5C..%5Ccoordinator.db",
        "/updates/client/..%2Fupdates%2F1.2.3/windows-x86_64",
        "/updates/client/1.2.3%2Fwindows-x86_64",
        "/updates/client/1.2.3",
        "/updates/client",
    ] {
        let response = anonymous.get(server.url(path)).send().await.unwrap();
        assert_eq!(response.status(), 404, "{path}");
    }
    // Paths an HTTP library would normalise away, sent as they are.
    for path in [
        "/updates/client/1.2.3/../../coordinator.db",
        "/updates/client/../../coordinator.db",
        "/updates/client/1.2.3/./windows-x86_64",
    ] {
        assert_eq!(raw_get_status(server.addr, path).await, 404, "{path}");
    }
}

#[tokio::test]
async fn downloads_are_served_where_clients_connect_and_not_with_the_ui() {
    let releases = FakeReleases::start().await;
    let bytes = releases.publish("1.2.3", "");
    let server = TestServer::start_with(|config| {
        releases.configure(config);
        config.ui_bind = Some("127.0.0.1:0".parse().unwrap());
    })
    .await;
    let ui_addr = server.coordinator.as_ref().unwrap().ui_addr();
    let checked = wait_until(|| async {
        let response = server.http.get(format!("http://{ui_addr}/api/v1/updates")).bearer_auth(API_TOKEN).send().await;
        response.unwrap().json::<Value>().await.unwrap()["latest"]["version"] == "1.2.3"
    })
    .await;
    assert!(checked);

    let response = reqwest::get(server.url("/updates/client/1.2.3/windows-x86_64")).await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.unwrap().to_vec(), bytes);
    let response = reqwest::get(format!("http://{ui_addr}/updates/client/1.2.3/windows-x86_64")).await.unwrap();
    assert_eq!(response.status(), 404);
}

// ---------------------------------------------------------------- client.update jobs

#[tokio::test]
async fn a_client_update_job_needs_a_cached_version() {
    let releases = FakeReleases::start().await;
    releases.publish("1.2.3", "");
    let server = start_checking(&releases).await;
    let key = server.new_key();
    let _client = connect_as(&server, &key, "device-a", "1.0.0").await;

    let (status, body) = server.create_job("client.update", json!({"version": "9.9.9"}), one_device("device-a")).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["code"], "invalid_payload");
    assert_eq!(body["error"]["message"], "version 9.9.9 is not cached");

    for payload in [json!({}), json!({"version": "latest"}), json!({"version": 1}), json!({"version": "1.2.3", "downgrade": "yes"})] {
        let (status, body) = server.create_job("client.update", payload.clone(), one_device("device-a")).await;
        assert_eq!(status, 400, "{payload} -> {body}");
        assert_eq!(body["error"]["code"], "invalid_payload", "{payload} -> {body}");
    }
    assert_eq!(server.get("/api/v1/jobs").await.1["jobs"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn a_client_update_job_carries_the_cached_artifact_details_to_the_device() {
    let releases = FakeReleases::start().await;
    let bytes = releases.publish("1.2.3", "");
    let server = start_checking(&releases).await;
    let key = server.new_key();
    let mut client = connect_as(&server, &key, "device-a", "1.0.0").await;

    let expected = json!({
        "version": "1.2.3",
        "target": "windows-x86_64",
        "size": bytes.len(),
        "sha256": update::sha256_hex(&bytes),
        "downgrade": false,
    });
    let (status, body) = server.create_job("client.update", json!({"version": "1.2.3"}), one_device("device-a")).await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["job"]["kind"], "client.update");
    assert_eq!(body["job"]["payload"], expected);
    let (_, job_id, kind, payload) = client.expect_job().await;
    assert_eq!(job_id, body["job"]["id"].as_str().unwrap());
    assert_eq!(kind, "client.update");
    assert_eq!(payload, expected);
    let parsed: protocol::ClientUpdatePayload = serde_json::from_value(payload).unwrap();
    assert_eq!(parsed.size, bytes.len() as u64);

    // The downgrade flag is passed on.
    let (status, _) = server
        .create_job("client.update", json!({"version": "1.2.3", "downgrade": true}), one_device("device-a"))
        .await;
    assert_eq!(status, 201);
    assert_eq!(client.expect_job().await.3["downgrade"], true);
}

#[tokio::test]
async fn a_device_already_on_the_version_succeeds_at_once() {
    let releases = FakeReleases::start().await;
    releases.publish("1.2.3", "");
    let server = start_checking(&releases).await;
    let key = server.new_key();
    let mut current = connect_as(&server, &key, "device-current", "1.2.3").await;
    connect_as(&server, &key, "device-old", "1.0.0").await.disconnect().await;
    assert!(wait_until(|| async { !server.is_online("device-old").await }).await);

    let (status, body) = server.create_job("client.update", json!({"version": "1.2.3"}), json!({"type": "all"})).await;
    assert_eq!(status, 201, "{body}");
    let targets = body["job"]["targets"].as_array().unwrap();
    let find = |device: &str| targets.iter().find(|t| t["device_id"] == device).unwrap().clone();
    let done = find("device-current");
    assert_eq!(done["state"], "succeeded");
    assert_eq!(done["message"], "already on version 1.2.3");
    assert!(done["finished_at"].as_i64().is_some());
    assert!(done["exit_code"].is_null());
    assert_eq!(find("device-old")["state"], "queued");

    // Nothing is sent to the device that has it already.
    assert!(current.try_recv(Duration::from_millis(200)).await.is_none());
    let job_id = body["job"]["id"].as_str().unwrap();
    let stored = server.wait_for_target_state_of(job_id, "device-current", "succeeded").await;
    assert_eq!(stored["message"], "already on version 1.2.3");
}

#[tokio::test]
async fn update_available_marks_devices_older_than_the_newest_cached_version() {
    let releases = FakeReleases::start().await;
    // Nothing published yet: the start-up check finds no release.
    let server = start_checking(&releases).await;
    assert!(!updates(&server).await["last_error"].is_null());
    let key = server.new_key();
    let old = connect_as(&server, &key, "device-old", "1.0.0").await;
    let _current = connect_as(&server, &key, "device-current", "1.2.3").await;
    let _newer = connect_as(&server, &key, "device-newer", "2.0.0").await;
    let _odd = connect_as(&server, &key, "device-odd", "0.0.0-test").await;
    for device in server.devices().await {
        assert_eq!(device["update_available"], false, "nothing cached: {device}");
    }

    releases.publish("1.2.3", "");
    assert_eq!(check(&server).await.0, 200);
    let available = |id: &'static str| {
        let server = &server;
        async move { server.device(id).await.unwrap()["update_available"].clone() }
    };
    assert_eq!(available("device-old").await, true);
    assert_eq!(available("device-current").await, false);
    assert_eq!(available("device-newer").await, false);
    assert_eq!(available("device-odd").await, false, "a version that does not parse is not compared");
    let (_, body) = server.get("/api/v1/devices/device-old").await;
    assert_eq!(body["device"]["update_available"], true);

    // Offline devices are marked too, and events carry the field.
    let mut events = Events::open(&server).await;
    old.disconnect().await;
    let event = events.next_of("device.updated").await;
    assert_eq!(event["device"]["id"], "device-old");
    assert_eq!(event["device"]["online"], false);
    assert_eq!(event["device"]["update_available"], true);
    assert_eq!(event["device"]["restricted"], false);
    assert_eq!(event["device"]["protocol_version"], protocol::PROTOCOL_VERSION);
}

// ---------------------------------------------------------------- restricted clients

#[tokio::test]
async fn a_client_on_an_unsupported_protocol_version_only_gets_client_updates() {
    let releases = FakeReleases::start().await;
    let bytes = releases.publish("1.2.3", "");
    let server = start_checking(&releases).await;
    let key = server.new_key();
    let unsupported = protocol::PROTOCOL_VERSION + 1;
    let mut events = Events::open(&server).await;

    let mut options = HandshakeOptions::new(&key, "device-a");
    options.protocol_version = unsupported;
    options.client_version = "0.9.0";
    let mut client = FakeClient::handshake(server.addr, options).await.expect("welcomed");
    assert!(client.restricted);
    let event = events.next_of("device.updated").await;
    assert_eq!(event["device"]["restricted"], true);
    assert_eq!(event["device"]["protocol_version"], unsupported);
    assert_eq!(event["device"]["update_available"], true);
    let device = server.device("device-a").await.unwrap();
    assert_eq!(device["restricted"], true);
    assert_eq!(device["protocol_version"], unsupported);

    // Anything but an update waits.
    let script = server.create_script_job(one_device("device-a")).await;
    let script_id = script["id"].as_str().unwrap();
    assert!(client.try_recv(Duration::from_millis(200)).await.is_none());
    assert_eq!(server.single_target(script_id).await["state"], "queued");

    let (status, body) = server.create_job("client.update", json!({"version": "1.2.3"}), one_device("device-a")).await;
    assert_eq!(status, 201, "{body}");
    let (update_target, _, kind, payload) = client.expect_job().await;
    assert_eq!(kind, "client.update");
    assert_eq!(payload["sha256"], update::sha256_hex(&bytes));
    assert_eq!(payload["size"], bytes.len());
    assert_eq!(payload["target"], "windows-x86_64");
    assert_eq!(server.single_target(script_id).await["state"], "queued");

    // Reconnecting on the same old version still holds the script back.
    client.disconnect().await;
    assert!(wait_until(|| async { !server.is_online("device-a").await }).await);
    let mut options = HandshakeOptions::new(&key, "device-a");
    options.protocol_version = unsupported;
    let mut client = FakeClient::handshake(server.addr, options).await.unwrap();
    let (again, _, kind, _) = client.expect_job().await;
    assert_eq!((again.as_str(), kind.as_str()), (update_target.as_str(), "client.update"));
    assert!(client.try_recv(Duration::from_millis(200)).await.is_none());

    // The updated client speaks the supported version: the script arrives,
    // and the update target is delivered again for it to settle.
    client.disconnect().await;
    assert!(wait_until(|| async { !server.is_online("device-a").await }).await);
    let mut client = connect_as(&server, &key, "device-a", "1.2.3").await;
    assert!(!client.restricted);
    let (script_target, job_id, kind, _) = client.expect_job().await;
    assert_eq!(kind, "script.run");
    assert_eq!(job_id, script_id);
    assert_eq!(client.expect_job().await.0, update_target);
    client.result(&script_target, 0).await;
    client.expect_result_ack(&script_target).await;
    let device = server.device("device-a").await.unwrap();
    assert_eq!(device["restricted"], false);
    assert_eq!(device["protocol_version"], protocol::PROTOCOL_VERSION);
    assert_eq!(device["update_available"], false);
}

// ---------------------------------------------------------------- events

struct Events {
    socket: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
}

impl Events {
    async fn open(server: &TestServer) -> Events {
        let mut request = format!("ws://{}/api/v1/events", server.addr).into_client_request().unwrap();
        request.headers_mut().insert("Authorization", format!("Bearer {API_TOKEN}").parse().unwrap());
        let (socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        Events { socket }
    }

    /// Next event of the given type, skipping others.
    async fn next_of(&mut self, kind: &str) -> Value {
        loop {
            let message = tokio::time::timeout(WAIT, self.socket.next())
                .await
                .expect("timed out waiting for an event")
                .expect("event stream ended")
                .unwrap();
            if let Message::Text(text) = message {
                let event: Value = serde_json::from_str(text.as_str()).unwrap();
                if event["type"] == kind {
                    return event;
                }
            }
        }
    }
}
