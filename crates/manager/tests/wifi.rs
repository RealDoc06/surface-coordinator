//! "Move to this computer's Wi-Fi": the routes against a fake network.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::http::HeaderMap;
use axum::Json;
use manager::embedded::{Embedded, Options};
use manager::hotspot;
use manager::wifi::{self, Unsupported, WifiNetwork, WifiService, WifiSource};
use serde_json::{json, Value};

struct OnOffice;

impl WifiSource for OnOffice {
    fn current(&self) -> Result<WifiNetwork, String> {
        Ok(WifiNetwork {
            ssid: "Office-Net-Fixture".into(),
            password: Some("fake-office-passphrase".into()),
        })
    }
}

const ONLINE: &str = "https://coordinator.example.org:8443";

fn move_request(targets: Value, destination: &Value) -> Value {
    json!({
        "targets": targets,
        "expected_ssid": destination["ssid"],
        "expected_coordinator_url": destination["coordinator_url"],
    })
}

async fn start(
    wifi: Arc<dyn WifiSource>,
) -> (Embedded, tempfile::TempDir, reqwest::Client, String) {
    start_with(wifi, None).await
}

async fn start_with(
    wifi: Arc<dyn WifiSource>,
    online_coordinator: Option<&str>,
) -> (Embedded, tempfile::TempDir, reqwest::Client, String) {
    let data_dir = tempfile::tempdir().unwrap();
    let options = Options {
        client_bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        hotspot: Arc::new(hotspot::Unsupported),
        wifi,
        online_coordinator: online_coordinator.map(str::to_owned),
    };
    let embedded = Embedded::start(data_dir.path(), options).await.unwrap();
    let token = embedded
        .entry_url()
        .split_once("#token=")
        .unwrap()
        .1
        .to_owned();
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("Authorization", format!("Bearer {token}").parse().unwrap());
    let http = reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap();
    let base = format!("{}/api/v1/manager", embedded.origin());
    (embedded, data_dir, http, base)
}

#[tokio::test]
async fn the_network_is_described_without_its_password() {
    let (embedded, _dir, http, base) = start(Arc::new(OnOffice)).await;
    let text = http
        .get(format!("{base}/wifi"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!text.contains("fake-office-passphrase"), "{text}");
    let wifi: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(wifi["available"], true);
    assert_eq!(wifi["ssid"], "Office-Net-Fixture");
    assert_eq!(wifi["secured"], true);
    assert_eq!(wifi["coordinator"], "local");
    // Devices are sent to this computer's address on the client port.
    let url = wifi["coordinator_url"].as_str().unwrap();
    assert!(url.starts_with("ws://"), "{url}");
    assert!(
        url.ends_with(&format!(":{}", embedded.client_addr().port())),
        "{url}"
    );
    assert!(!url.contains("127.0.0.1"), "{url}");

    let unauthenticated = reqwest::get(format!("{base}/wifi")).await.unwrap();
    assert_eq!(unauthenticated.status(), 401);
    embedded.stop().await;
}

#[tokio::test]
async fn moving_creates_a_job_the_coordinator_accepts() {
    let (embedded, _dir, http, base) = start(Arc::new(OnOffice)).await;
    let destination: Value = http
        .get(format!("{base}/wifi"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // No devices are enrolled, so the coordinator refuses for lack of
    // targets, which it only does after accepting the payload: the SSID,
    // the password and the coordinator address the manager filled in.
    let response = http
        .post(format!("{base}/wifi/move"))
        .json(&move_request(json!({"type": "all"}), &destination))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "no_targets");

    let response = http
        .post(format!("{base}/wifi/move"))
        .json(&move_request(
            json!({"type": "devices", "device_ids": ["nope"]}),
            &destination,
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["code"],
        "unknown_device"
    );
    embedded.stop().await;
}

#[tokio::test]
async fn without_a_readable_network_the_button_has_nothing_to_offer() {
    let (embedded, _dir, http, base) = start(Arc::new(Unsupported)).await;
    let wifi: Value = http
        .get(format!("{base}/wifi"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(wifi["available"], false);
    assert!(wifi["reason"].as_str().unwrap().contains("cannot read"));

    let response = http
        .post(format!("{base}/wifi/move"))
        .json(&json!({
            "targets": {"type": "all"},
            "expected_ssid": "Office-Net-Fixture",
            "expected_coordinator_url": "wss://coordinator.example.org:8443",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 409);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["code"],
        "wifi_unavailable"
    );
    embedded.stop().await;
}

#[tokio::test]
async fn with_an_online_coordinator_devices_are_sent_there() {
    let (embedded, _dir, http, base) = start_with(Arc::new(OnOffice), Some(ONLINE)).await;
    let wifi: Value = http
        .get(format!("{base}/wifi"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(wifi["available"], true);
    assert_eq!(wifi["coordinator"], "online");
    assert_eq!(
        wifi["coordinator_url"],
        "wss://coordinator.example.org:8443"
    );
    // Whether this computer is reachable on that network does not matter.
    assert_eq!(wifi["warning"], Value::Null);

    // Back to this computer once the operator no longer uses one.
    embedded.set_online_coordinator(None);
    let wifi: Value = http
        .get(format!("{base}/wifi"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(wifi["coordinator"], "local");
    assert!(
        wifi["coordinator_url"]
            .as_str()
            .unwrap()
            .starts_with("ws://"),
        "{wifi}"
    );
    embedded.stop().await;
}

/// A stand-in for the coordinator's `POST /api/v1/jobs` that keeps the
/// authorization header and the job it was sent. Returns its origin.
async fn job_recorder(jobs: Arc<Mutex<Vec<(String, Value)>>>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new().route(
        "/api/v1/jobs",
        axum::routing::post(
            move |headers: HeaderMap, Json(job): Json<Value>| async move {
                let authorization = headers["authorization"].to_str().unwrap().to_owned();
                jobs.lock().unwrap().push((authorization, job));
                (
                    axum::http::StatusCode::CREATED,
                    Json(json!({"job": {"id": "job-1"}})),
                )
            },
        ),
    );
    tokio::spawn(async move { axum::serve(listener, app).await });
    origin
}

#[tokio::test]
async fn the_job_carries_the_online_coordinators_address() {
    let jobs = Arc::new(Mutex::new(Vec::new()));
    let coordinator = job_recorder(jobs.clone()).await;
    let service = Arc::new(WifiService::new(Arc::new(OnOffice)));
    service.set_online_coordinator(Some(ONLINE));
    service.connect(coordinator, "fake-local-token".into(), 8081);

    // The routes alone, without the coordinator's authentication in front.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, wifi::router(service)).await });

    let targets = json!({"type": "devices", "device_ids": ["surface-1"]});
    let response = reqwest::Client::new()
        .post(format!("{base}/wifi/move"))
        .json(&json!({
            "targets": targets,
            "expected_ssid": "Office-Net-Fixture",
            "expected_coordinator_url": "wss://coordinator.example.org:8443",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    assert_eq!(
        response.json::<Value>().await.unwrap()["job"]["id"],
        "job-1"
    );

    let jobs = jobs.lock().unwrap();
    let [(authorization, job)] = jobs.as_slice() else {
        panic!("{jobs:?}")
    };
    assert_eq!(authorization, "Bearer fake-local-token");
    assert_eq!(
        *job,
        json!({
            "kind": "wifi.join",
            "payload": {
                "ssid": "Office-Net-Fixture",
                "password": "fake-office-passphrase",
                "coordinator_url": "wss://coordinator.example.org:8443",
            },
            "targets": targets,
        })
    );
}

struct ChangingNetwork(Mutex<WifiNetwork>);

impl WifiSource for ChangingNetwork {
    fn current(&self) -> Result<WifiNetwork, String> {
        Ok(self.0.lock().unwrap().clone())
    }
}

#[tokio::test]
async fn a_changed_network_or_coordinator_requires_a_new_confirmation() {
    let jobs = Arc::new(Mutex::new(Vec::new()));
    let coordinator = job_recorder(jobs.clone()).await;
    let source = Arc::new(ChangingNetwork(Mutex::new(WifiNetwork {
        ssid: "Reviewed network".into(),
        password: Some("fake-reviewed-password".into()),
    })));
    let service = Arc::new(WifiService::new(source.clone()));
    service.set_online_coordinator(Some(ONLINE));
    service.connect(coordinator, "fake-local-token".into(), 8081);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = wifi::router(service.clone());
    tokio::spawn(async move { axum::serve(listener, router).await });
    let http = reqwest::Client::new();
    let targets = json!({"type": "devices", "device_ids": ["surface-1"]});

    let reviewed: Value = http
        .get(format!("{base}/wifi"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    source.0.lock().unwrap().ssid = "New network".into();
    let changed = http
        .post(format!("{base}/wifi/move"))
        .json(&move_request(targets.clone(), &reviewed))
        .send()
        .await
        .unwrap();
    assert_eq!(changed.status(), 409);
    assert_eq!(
        changed.json::<Value>().await.unwrap()["error"]["code"],
        "wifi_changed"
    );
    assert!(jobs.lock().unwrap().is_empty());

    let refreshed: Value = http
        .get(format!("{base}/wifi"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    service.set_online_coordinator(Some("https://new-coordinator.example.org"));
    let changed = http
        .post(format!("{base}/wifi/move"))
        .json(&move_request(targets.clone(), &refreshed))
        .send()
        .await
        .unwrap();
    assert_eq!(changed.status(), 409);
    assert_eq!(
        changed.json::<Value>().await.unwrap()["error"]["code"],
        "wifi_changed"
    );
    assert!(jobs.lock().unwrap().is_empty());

    let refreshed: Value = http
        .get(format!("{base}/wifi"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let accepted = http
        .post(format!("{base}/wifi/move"))
        .json(&move_request(targets.clone(), &refreshed))
        .send()
        .await
        .unwrap();
    assert_eq!(accepted.status(), 201);
    assert_eq!(jobs.lock().unwrap()[0].1["payload"]["ssid"], "New network");
    assert_eq!(
        jobs.lock().unwrap()[0].1["payload"]["coordinator_url"],
        "wss://new-coordinator.example.org"
    );

    let unconfirmed = http
        .post(format!("{base}/wifi/move"))
        .json(&json!({"targets": targets}))
        .send()
        .await
        .unwrap();
    assert_eq!(unconfirmed.status(), 422);
    assert_eq!(jobs.lock().unwrap().len(), 1);
}
