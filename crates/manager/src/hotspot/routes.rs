//! The hotspot routes of `docs/api.md`, served by the embedded coordinator
//! under `/api/v1/manager` behind its own authentication.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{Control, HotspotControl};

/// The hotspot as the UI sees it (`Hotspot` in `docs/api.md`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Hotspot {
    pub control: Control,
    pub state: Phase,
    pub key_id: Option<String>,
    pub ssid: Option<String>,
    pub passphrase: Option<String>,
    pub client_port: u16,
    pub error: Option<String>,
    pub instructions: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    #[default]
    Off,
    Starting,
    On,
    Error,
}

/// The network of the enrollment key the hotspot was started for.
#[derive(Debug, Clone)]
struct Network {
    key_id: String,
    ssid: String,
    passphrase: String,
}

#[derive(Debug, Default)]
struct Status {
    phase: Phase,
    /// Kept after a failure: the hotspot may be half on, and the operator
    /// still sees what it was meant to be.
    network: Option<Network>,
    error: Option<String>,
}

/// Turns the hotspot on and off for an enrollment key and remembers how
/// that went.
pub struct HotspotService {
    control: Arc<dyn HotspotControl>,
    /// Data directory of the embedded coordinator, holding the keys.
    data_dir: PathBuf,
    /// Filled in once the coordinator listens: the routes go into its
    /// configuration before it starts, and tests let it pick a free port.
    client_port: AtomicU16,
    status: Mutex<Status>,
    /// Start and stop take seconds and must not interleave. Reading the
    /// status does not wait for them.
    operation: tokio::sync::Mutex<()>,
}

impl HotspotService {
    pub fn new(control: Arc<dyn HotspotControl>, data_dir: impl Into<PathBuf>, client_port: u16) -> HotspotService {
        HotspotService {
            control,
            data_dir: data_dir.into(),
            client_port: AtomicU16::new(client_port),
            status: Mutex::new(Status::default()),
            operation: tokio::sync::Mutex::new(()),
        }
    }

    pub fn control(&self) -> Control {
        self.control.control()
    }

    pub fn set_client_port(&self, port: u16) {
        self.client_port.store(port, Ordering::Relaxed);
    }

    pub fn hotspot(&self) -> Hotspot {
        let status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        let control = self.control.control();
        let network = status.network.as_ref();
        Hotspot {
            control,
            state: status.phase,
            key_id: network.map(|n| n.key_id.clone()),
            ssid: network.map(|n| n.ssid.clone()),
            passphrase: network.map(|n| n.passphrase.clone()),
            client_port: self.client_port.load(Ordering::Relaxed),
            error: status.error.clone(),
            instructions: match network {
                Some(n) if control == Control::Manual => self.control.instructions(&n.ssid, &n.passphrase),
                _ => Vec::new(),
            },
        }
    }

    fn set(&self, phase: Phase, network: Option<Network>, error: Option<String>) {
        *self.status.lock().unwrap_or_else(|e| e.into_inner()) = Status { phase, network, error };
    }

    /// Turns the hotspot on for the enrollment key `key_id`, or switches it
    /// to that key.
    pub async fn start(&self, key_id: &str) -> Result<Hotspot, HotspotError> {
        let network = self.network_for(key_id).await?;
        let _operation = self.operation.lock().await;
        if self.control.control() == Control::Manual {
            // The operator turns it on; the manager only shows how.
            self.set(Phase::On, Some(network), None);
            return Ok(self.hotspot());
        }
        self.set(Phase::Starting, Some(network.clone()), None);
        match self.control.start(&network.ssid, &network.passphrase).await {
            Ok(()) => {
                tracing::info!(ssid = %network.ssid, "hotspot started");
                self.set(Phase::On, Some(network), None);
                Ok(self.hotspot())
            }
            Err(message) => {
                tracing::warn!(ssid = %network.ssid, %message, "hotspot did not start");
                self.set(Phase::Error, Some(network), Some(message.clone()));
                Err(HotspotError::failed(message))
            }
        }
    }

    /// Turns the hotspot off if the manager turned it on. One that is off
    /// here is left alone: the operator may be using Mobile Hotspot for
    /// something else.
    pub async fn stop(&self) -> Result<Hotspot, HotspotError> {
        let _operation = self.operation.lock().await;
        let (phase, network) = {
            let status = self.status.lock().unwrap_or_else(|e| e.into_inner());
            (status.phase, status.network.clone())
        };
        if phase != Phase::Off && self.control.control() != Control::Manual {
            if let Err(message) = self.control.stop().await {
                tracing::warn!(%message, "hotspot did not stop");
                self.set(Phase::Error, network, Some(message.clone()));
                return Err(HotspotError::failed(message));
            }
            tracing::info!("hotspot stopped");
        }
        self.set(Phase::Off, None, None);
        Ok(self.hotspot())
    }

    /// Stops the hotspot with the embedded coordinator. Failures can only be
    /// logged: the coordinator stops anyway.
    pub async fn shutdown(&self) {
        if let Err(error) = self.stop().await {
            tracing::error!(message = %error.message, "the hotspot could not be stopped");
        }
    }

    async fn network_for(&self, key_id: &str) -> Result<Network, HotspotError> {
        let data_dir = self.data_dir.clone();
        let keys = tokio::task::spawn_blocking(move || coordinator_core::admin::list_enrollment_keys(&data_dir))
            .await
            .map_err(|error| HotspotError::internal(error.to_string()))?
            .map_err(|error| HotspotError::internal(error.to_string()))?;
        let key = keys
            .into_iter()
            .find(|key| key.id == key_id)
            .ok_or_else(|| HotspotError::new(StatusCode::NOT_FOUND, "not_found", "no such enrollment key"))?;
        if key.revoked.is_some() {
            return Err(HotspotError::new(
                StatusCode::BAD_REQUEST,
                "key_revoked",
                "this enrollment key is revoked; devices enrolled with it cannot connect",
            ));
        }
        Ok(Network {
            ssid: protocol::hotspot::ssid(&key.id),
            passphrase: protocol::hotspot::passphrase(&key.secret),
            key_id: key.id,
        })
    }
}

/// An error in the coordinator's shape:
/// `{"error": {"code": "...", "message": "..."}}`.
#[derive(Debug)]
pub struct HotspotError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl HotspotError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> HotspotError {
        HotspotError { status, code, message: message.into() }
    }

    fn failed(message: String) -> HotspotError {
        HotspotError::new(StatusCode::BAD_GATEWAY, "hotspot_failed", message)
    }

    fn internal(detail: String) -> HotspotError {
        tracing::error!(%detail, "hotspot request failed");
        HotspotError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", "internal error")
    }
}

impl IntoResponse for HotspotError {
    fn into_response(self) -> Response {
        let body = json!({"error": {"code": self.code, "message": self.message}});
        (self.status, Json(body)).into_response()
    }
}

pub fn router(service: Arc<HotspotService>) -> Router {
    Router::new()
        .route("/hotspot", get(show))
        .route("/hotspot/start", post(start))
        .route("/hotspot/stop", post(stop))
        .with_state(service)
}

#[derive(Deserialize)]
struct StartRequest {
    key_id: String,
}

async fn show(State(service): State<Arc<HotspotService>>) -> Json<Hotspot> {
    Json(service.hotspot())
}

async fn start(
    State(service): State<Arc<HotspotService>>,
    body: Result<Json<StartRequest>, JsonRejection>,
) -> Result<Json<Hotspot>, HotspotError> {
    let Json(request) =
        body.map_err(|rejection| HotspotError::new(StatusCode::BAD_REQUEST, "bad_request", rejection.body_text()))?;
    // Finish even if the browser goes away, so the state never stays
    // "starting".
    tokio::spawn(async move { service.start(&request.key_id).await })
        .await
        .map_err(|error| HotspotError::internal(error.to_string()))?
        .map(Json)
}

async fn stop(State(service): State<Arc<HotspotService>>) -> Result<Json<Hotspot>, HotspotError> {
    tokio::spawn(async move { service.stop().await })
        .await
        .map_err(|error| HotspotError::internal(error.to_string()))?
        .map(Json)
}
