//! Hotspot routes of the embedded coordinator, against a fake hotspot.
//!
//! The real hotspot is never touched here: starting it would replace the
//! network name and password of this computer's own Mobile Hotspot.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};

use manager::embedded::{Embedded, Options};
use manager::hotspot::{BoxFuture, Control, HotspotControl};
use protocol::hotspot;
use reqwest::{Response, StatusCode};
use serde_json::{json, Value};
use tokio::sync::Notify;

#[derive(Debug, Clone, PartialEq)]
enum Call {
    Start { ssid: String, passphrase: String },
    Stop,
}

/// Records what the manager asks of the hotspot.
struct FakeControl {
    control: Control,
    calls: Mutex<Vec<Call>>,
    /// The next start fails with this message.
    start_failure: Mutex<Option<String>>,
    /// The next stop fails with this message.
    stop_failure: Mutex<Option<String>>,
    /// When set, a start waits for this before it finishes.
    start_gate: Mutex<Option<Arc<Notify>>>,
}

impl FakeControl {
    fn new(control: Control) -> Arc<FakeControl> {
        Arc::new(FakeControl {
            control,
            calls: Mutex::new(Vec::new()),
            start_failure: Mutex::new(None),
            stop_failure: Mutex::new(None),
            start_gate: Mutex::new(None),
        })
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    fn fail_next_start(&self, message: &str) {
        *self.start_failure.lock().unwrap() = Some(message.to_owned());
    }

    fn fail_next_stop(&self, message: &str) {
        *self.stop_failure.lock().unwrap() = Some(message.to_owned());
    }
}

impl HotspotControl for FakeControl {
    fn control(&self) -> Control {
        self.control
    }

    fn start(&self, ssid: &str, passphrase: &str) -> BoxFuture<Result<(), String>> {
        self.calls
            .lock()
            .unwrap()
            .push(Call::Start { ssid: ssid.to_owned(), passphrase: passphrase.to_owned() });
        let failure = self.start_failure.lock().unwrap().take();
        let gate = self.start_gate.lock().unwrap().take();
        Box::pin(async move {
            if let Some(gate) = gate {
                gate.notified().await;
            }
            failure.map_or(Ok(()), Err)
        })
    }

    fn stop(&self) -> BoxFuture<Result<(), String>> {
        self.calls.lock().unwrap().push(Call::Stop);
        let failure = self.stop_failure.lock().unwrap().take();
        Box::pin(async move { failure.map_or(Ok(()), Err) })
    }

    fn instructions(&self, ssid: &str, passphrase: &str) -> Vec<String> {
        match self.control {
            Control::Manual => vec![
                "Open the sharing settings.".to_owned(),
                format!("Name the network {ssid}."),
                format!("Use the password {passphrase}."),
            ],
            _ => Vec::new(),
        }
    }
}

/// A running embedded coordinator with a signed-in HTTP client.
struct Manager {
    embedded: Embedded,
    http: reqwest::Client,
    token: String,
    _data_dir: tempfile::TempDir,
}

impl Manager {
    async fn start(control: Arc<FakeControl>) -> Manager {
        let data_dir = tempfile::tempdir().unwrap();
        let options = Options {
            // The app uses the fixed client port; tests must not collide.
            client_bind: SocketAddr::from(([127, 0, 0, 1], 0)),
            hotspot: control,
            wifi: Arc::new(manager::wifi::Unsupported),
            online_coordinator: None,
        };
        let embedded = Embedded::start(data_dir.path(), options).await.unwrap();
        let token = embedded.entry_url().split_once("#token=").unwrap().1.to_owned();
        Manager { embedded, http: reqwest::Client::new(), token, _data_dir: data_dir }
    }

    fn data_dir(&self) -> &Path {
        self._data_dir.path()
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/v1{path}", self.embedded.origin())
    }

    async fn get(&self, path: &str) -> Response {
        self.http.get(self.url(path)).bearer_auth(&self.token).send().await.unwrap()
    }

    async fn post(&self, path: &str, body: Value) -> Response {
        self.http.post(self.url(path)).bearer_auth(&self.token).json(&body).send().await.unwrap()
    }

    async fn hotspot(&self) -> Value {
        let response = self.get("/manager/hotspot").await;
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.unwrap()
    }

    async fn start_hotspot(&self, key_id: &str) -> Response {
        self.post("/manager/hotspot/start", json!({ "key_id": key_id })).await
    }

    async fn stop_hotspot(&self) -> Response {
        self.post("/manager/hotspot/stop", json!({})).await
    }

    fn create_key(&self, label: &str) -> coordinator_core::EnrollmentKey {
        coordinator_core::admin::create_enrollment_key(self.data_dir(), label).unwrap()
    }
}

async fn error_code(response: Response) -> (StatusCode, String, String) {
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    let code = body["error"]["code"].as_str().expect("error code").to_owned();
    let message = body["error"]["message"].as_str().expect("error message").to_owned();
    (status, code, message)
}

#[tokio::test]
async fn hotspot_routes_need_authentication() {
    let manager = Manager::start(FakeControl::new(Control::Automatic)).await;
    let anonymous = reqwest::Client::new();
    let response = anonymous.get(manager.url("/manager/hotspot")).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = anonymous
        .post(manager.url("/manager/hotspot/start"))
        .json(&json!({ "key_id": "0123456789abcdef" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = anonymous.post(manager.url("/manager/hotspot/stop")).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    manager.embedded.stop().await;
}

#[tokio::test]
async fn the_session_offers_hotspot_controls_only_when_the_hotspot_can_be_controlled() {
    for (control, offered) in
        [(Control::Automatic, true), (Control::Manual, true), (Control::Unsupported, false)]
    {
        let manager = Manager::start(FakeControl::new(control)).await;
        let session: Value = manager.get("/session").await.json().await.unwrap();
        assert_eq!(session["capabilities"]["hotspot"], offered, "{control:?}");
        manager.embedded.stop().await;
    }
}

#[tokio::test]
async fn the_hotspot_starts_off_and_names_the_port_devices_connect_to() {
    let manager = Manager::start(FakeControl::new(Control::Automatic)).await;
    let hotspot = manager.hotspot().await;
    assert_eq!(
        hotspot,
        json!({
            "control": "automatic",
            "state": "off",
            "key_id": null,
            "ssid": null,
            "passphrase": null,
            "client_port": manager.embedded.client_addr().port(),
            "error": null,
            "instructions": [],
        })
    );
    manager.embedded.stop().await;
}

#[tokio::test]
async fn starting_uses_the_network_name_and_password_of_the_chosen_key() {
    let control = FakeControl::new(Control::Automatic);
    let manager = Manager::start(control.clone()).await;
    let key = manager.create_key("Lab trolley");

    let response = manager.start_hotspot(&key.id).await;
    assert_eq!(response.status(), StatusCode::OK);
    let started: Value = response.json().await.unwrap();

    let ssid = hotspot::ssid(&key.id);
    let passphrase = hotspot::passphrase(&key.secret);
    assert_eq!(control.calls(), vec![Call::Start { ssid: ssid.clone(), passphrase: passphrase.clone() }]);
    assert_eq!(started["state"], "on");
    assert_eq!(started["key_id"], key.id);
    assert_eq!(started["ssid"], ssid);
    assert_eq!(started["passphrase"], passphrase);
    assert_eq!(started["error"], Value::Null);
    assert_eq!(started["instructions"], json!([]));
    assert_eq!(manager.hotspot().await, started);
    manager.embedded.stop().await;
}

#[tokio::test]
async fn starting_for_another_key_while_on_switches_the_hotspot_to_that_key() {
    let control = FakeControl::new(Control::Automatic);
    let manager = Manager::start(control.clone()).await;
    let first = manager.create_key("First trolley");
    let second = manager.create_key("Second trolley");

    assert_eq!(manager.start_hotspot(&first.id).await.status(), StatusCode::OK);
    let response = manager.start_hotspot(&second.id).await;
    assert_eq!(response.status(), StatusCode::OK);
    let switched: Value = response.json().await.unwrap();

    assert_eq!(switched["state"], "on");
    assert_eq!(switched["key_id"], second.id);
    assert_eq!(switched["ssid"], hotspot::ssid(&second.id));
    assert_eq!(
        control.calls().last(),
        Some(&Call::Start { ssid: hotspot::ssid(&second.id), passphrase: hotspot::passphrase(&second.secret) })
    );
    manager.embedded.stop().await;
}

#[tokio::test]
async fn an_unknown_key_is_not_found() {
    let control = FakeControl::new(Control::Automatic);
    let manager = Manager::start(control.clone()).await;
    manager.create_key("Some other key");

    let (status, code, _) = error_code(manager.start_hotspot("00000000000000000000000000000000").await).await;
    assert_eq!((status, code.as_str()), (StatusCode::NOT_FOUND, "not_found"));
    assert_eq!(control.calls(), vec![]);
    assert_eq!(manager.hotspot().await["state"], "off");
    manager.embedded.stop().await;
}

#[tokio::test]
async fn a_revoked_key_is_refused() {
    let control = FakeControl::new(Control::Automatic);
    let manager = Manager::start(control.clone()).await;
    let key = manager.create_key("Retired trolley");
    assert!(coordinator_core::admin::revoke_enrollment_key(manager.data_dir(), &key.id).unwrap());

    let (status, code, _) = error_code(manager.start_hotspot(&key.id).await).await;
    assert_eq!((status, code.as_str()), (StatusCode::BAD_REQUEST, "key_revoked"));
    assert_eq!(control.calls(), vec![]);
    manager.embedded.stop().await;
}

#[tokio::test]
async fn a_malformed_start_request_is_a_bad_request() {
    let control = FakeControl::new(Control::Automatic);
    let manager = Manager::start(control.clone()).await;
    for body in [json!({}), json!({ "key_id": 7 }), json!("key")] {
        let (status, code, _) = error_code(manager.post("/manager/hotspot/start", body.clone()).await).await;
        assert_eq!((status, code.as_str()), (StatusCode::BAD_REQUEST, "bad_request"), "{body}");
    }
    let response = manager
        .http
        .post(manager.url("/manager/hotspot/start"))
        .bearer_auth(&manager.token)
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await
        .unwrap();
    let (status, code, _) = error_code(response).await;
    assert_eq!((status, code.as_str()), (StatusCode::BAD_REQUEST, "bad_request"));
    assert_eq!(control.calls(), vec![]);
    manager.embedded.stop().await;
}

#[tokio::test]
async fn a_failed_start_is_reported_until_a_start_succeeds() {
    let control = FakeControl::new(Control::Automatic);
    let manager = Manager::start(control.clone()).await;
    let key = manager.create_key("Lab trolley");

    control.fail_next_start("the Wi-Fi adapter is switched off");
    let (status, code, message) = error_code(manager.start_hotspot(&key.id).await).await;
    assert_eq!((status, code.as_str()), (StatusCode::BAD_GATEWAY, "hotspot_failed"));
    assert!(message.contains("the Wi-Fi adapter is switched off"), "{message}");
    let failed = manager.hotspot().await;
    assert_eq!(failed["state"], "error");
    assert_eq!(failed["error"], "the Wi-Fi adapter is switched off");

    let response = manager.start_hotspot(&key.id).await;
    assert_eq!(response.status(), StatusCode::OK);
    let started: Value = response.json().await.unwrap();
    assert_eq!(started["state"], "on");
    assert_eq!(started["error"], Value::Null);
    manager.embedded.stop().await;
}

#[tokio::test]
async fn the_hotspot_shows_as_starting_while_it_starts() {
    let control = FakeControl::new(Control::Automatic);
    let manager = Arc::new(Manager::start(control.clone()).await);
    let key = manager.create_key("Lab trolley");
    let gate = Arc::new(Notify::new());
    *control.start_gate.lock().unwrap() = Some(gate.clone());

    let starting = tokio::spawn({
        let manager = manager.clone();
        let key_id = key.id.clone();
        async move { manager.start_hotspot(&key_id).await.status() }
    });
    // Reading the state must not wait for a slow start.
    let mut seen = Value::Null;
    for _ in 0..200 {
        seen = manager.hotspot().await;
        if seen["state"] == "starting" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(seen["state"], "starting");
    assert_eq!(seen["key_id"], key.id);

    gate.notify_one();
    assert_eq!(starting.await.unwrap(), StatusCode::OK);
    assert_eq!(manager.hotspot().await["state"], "on");
    let manager = Arc::into_inner(manager).expect("the request has finished");
    manager.embedded.stop().await;
}

#[tokio::test]
async fn stopping_turns_the_hotspot_off_and_forgets_the_key() {
    let control = FakeControl::new(Control::Automatic);
    let manager = Manager::start(control.clone()).await;
    let key = manager.create_key("Lab trolley");
    assert_eq!(manager.start_hotspot(&key.id).await.status(), StatusCode::OK);

    let response = manager.stop_hotspot().await;
    assert_eq!(response.status(), StatusCode::OK);
    let stopped: Value = response.json().await.unwrap();
    assert_eq!(control.calls().last(), Some(&Call::Stop));
    assert_eq!(stopped["state"], "off");
    for field in ["key_id", "ssid", "passphrase", "error"] {
        assert_eq!(stopped[field], Value::Null, "{field}");
    }
    manager.embedded.stop().await;
}

#[tokio::test]
async fn stopping_while_off_leaves_the_computer_alone() {
    // The operator may use Mobile Hotspot for something else; the manager
    // only turns off what it turned on.
    let control = FakeControl::new(Control::Automatic);
    let manager = Manager::start(control.clone()).await;
    let response = manager.stop_hotspot().await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.json::<Value>().await.unwrap()["state"], "off");
    manager.embedded.stop().await;
    assert_eq!(control.calls(), vec![]);
}

#[tokio::test]
async fn a_failed_stop_is_reported_and_can_be_retried() {
    let control = FakeControl::new(Control::Automatic);
    let manager = Manager::start(control.clone()).await;
    let key = manager.create_key("Lab trolley");
    assert_eq!(manager.start_hotspot(&key.id).await.status(), StatusCode::OK);

    control.fail_next_stop("the hotspot is busy");
    let (status, code, message) = error_code(manager.stop_hotspot().await).await;
    assert_eq!((status, code.as_str()), (StatusCode::BAD_GATEWAY, "hotspot_failed"));
    assert!(message.contains("the hotspot is busy"), "{message}");
    let failed = manager.hotspot().await;
    assert_eq!(failed["state"], "error");
    assert_eq!(failed["error"], "the hotspot is busy");

    let response = manager.stop_hotspot().await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.json::<Value>().await.unwrap()["state"], "off");
    assert_eq!(control.calls().iter().filter(|call| **call == Call::Stop).count(), 2);
    manager.embedded.stop().await;
}

#[tokio::test]
async fn with_manual_control_the_operator_gets_instructions_instead() {
    let control = FakeControl::new(Control::Manual);
    let manager = Manager::start(control.clone()).await;
    let key = manager.create_key("Lab trolley");
    // Would fail if the manager tried to start it itself.
    control.fail_next_start("there is no API for this");

    let response = manager.start_hotspot(&key.id).await;
    assert_eq!(response.status(), StatusCode::OK);
    let started: Value = response.json().await.unwrap();
    let ssid = hotspot::ssid(&key.id);
    let passphrase = hotspot::passphrase(&key.secret);
    assert_eq!(started["control"], "manual");
    assert_eq!(started["state"], "on");
    assert_eq!(started["ssid"], ssid);
    assert_eq!(started["passphrase"], passphrase);
    assert_eq!(started["error"], Value::Null);
    let instructions = started["instructions"].as_array().unwrap();
    assert!(!instructions.is_empty());
    let text = instructions.iter().map(|step| step.as_str().unwrap()).collect::<Vec<_>>().join("\n");
    assert!(text.contains(&ssid) && text.contains(&passphrase), "{text}");

    let response = manager.stop_hotspot().await;
    assert_eq!(response.json::<Value>().await.unwrap()["instructions"], json!([]));
    manager.embedded.stop().await;
}

#[tokio::test]
async fn stopping_the_local_coordinator_stops_the_hotspot() {
    let control = FakeControl::new(Control::Automatic);
    let manager = Manager::start(control.clone()).await;
    let key = manager.create_key("Lab trolley");
    assert_eq!(manager.start_hotspot(&key.id).await.status(), StatusCode::OK);

    manager.embedded.stop().await;
    assert_eq!(control.calls().last(), Some(&Call::Stop));
}
