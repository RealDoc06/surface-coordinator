//! "Move to this computer's Wi-Fi": the routes against a fake network.

use std::net::SocketAddr;
use std::sync::Arc;

use manager::embedded::{Embedded, Options};
use manager::hotspot;
use manager::wifi::{Unsupported, WifiNetwork, WifiSource};
use serde_json::{json, Value};

struct OnOffice;

impl WifiSource for OnOffice {
    fn current(&self) -> Result<WifiNetwork, String> {
        Ok(WifiNetwork { ssid: "Office-Net-Fixture".into(), password: Some("fake-office-passphrase".into()) })
    }
}

async fn start(wifi: Arc<dyn WifiSource>) -> (Embedded, tempfile::TempDir, reqwest::Client, String) {
    let data_dir = tempfile::tempdir().unwrap();
    let options = Options {
        client_bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        hotspot: Arc::new(hotspot::Unsupported),
        wifi,
    };
    let embedded = Embedded::start(data_dir.path(), options).await.unwrap();
    let token = embedded.entry_url().split_once("#token=").unwrap().1.to_owned();
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("Authorization", format!("Bearer {token}").parse().unwrap());
    let http = reqwest::Client::builder().default_headers(headers).build().unwrap();
    let base = format!("{}/api/v1/manager", embedded.origin());
    (embedded, data_dir, http, base)
}

#[tokio::test]
async fn the_network_is_described_without_its_password() {
    let (embedded, _dir, http, base) = start(Arc::new(OnOffice)).await;
    let text = http.get(format!("{base}/wifi")).send().await.unwrap().text().await.unwrap();
    assert!(!text.contains("fake-office-passphrase"), "{text}");
    let wifi: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(wifi["available"], true);
    assert_eq!(wifi["ssid"], "Office-Net-Fixture");
    assert_eq!(wifi["secured"], true);
    // Devices are sent to this computer's address on the client port.
    let url = wifi["coordinator_url"].as_str().unwrap();
    assert!(url.starts_with("ws://"), "{url}");
    assert!(url.ends_with(&format!(":{}", embedded.client_addr().port())), "{url}");
    assert!(!url.contains("127.0.0.1"), "{url}");

    let unauthenticated = reqwest::get(format!("{base}/wifi")).await.unwrap();
    assert_eq!(unauthenticated.status(), 401);
    embedded.stop().await;
}

#[tokio::test]
async fn moving_creates_a_job_the_coordinator_accepts() {
    let (embedded, _dir, http, base) = start(Arc::new(OnOffice)).await;
    // No devices are enrolled, so the coordinator refuses for lack of
    // targets, which it only does after accepting the payload: the SSID,
    // the password and the coordinator address the manager filled in.
    let response =
        http.post(format!("{base}/wifi/move")).json(&json!({"targets": {"type": "all"}})).send().await.unwrap();
    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "no_targets");

    let response = http
        .post(format!("{base}/wifi/move"))
        .json(&json!({"targets": {"type": "devices", "device_ids": ["nope"]}}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.json::<Value>().await.unwrap()["error"]["code"], "unknown_device");
    embedded.stop().await;
}

#[tokio::test]
async fn without_a_readable_network_the_button_has_nothing_to_offer() {
    let (embedded, _dir, http, base) = start(Arc::new(Unsupported)).await;
    let wifi: Value = http.get(format!("{base}/wifi")).send().await.unwrap().json().await.unwrap();
    assert_eq!(wifi["available"], false);
    assert!(wifi["reason"].as_str().unwrap().contains("cannot read"));

    let response =
        http.post(format!("{base}/wifi/move")).json(&json!({"targets": {"type": "all"}})).send().await.unwrap();
    assert_eq!(response.status(), 409);
    assert_eq!(response.json::<Value>().await.unwrap()["error"]["code"], "wifi_unavailable");
    embedded.stop().await;
}
