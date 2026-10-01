//! The HTTP API used by the UI (and, until there is one, by hand).

use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::extract::{Path, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use protocol::{job_kind, RunAs, ScriptRunPayload};
use serde::Deserialize;
use serde_json::{json, Value};
use subtle::ConstantTimeEq;

use crate::state::AppState;
use crate::store::{self, CreateJobError, Targets};
use crate::AuthMode;

const MAX_LISTED_JOBS: u32 = 200;
const MAX_TIMEOUT_SECS: u64 = 24 * 60 * 60;

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    let authenticated = Router::new()
        .route("/devices", get(list_devices))
        .route("/jobs", get(list_jobs).post(create_job))
        .route("/jobs/{id}", get(get_job))
        .route_layer(middleware::from_fn_with_state(state, require_auth));
    let api = Router::new().route("/health", get(health)).merge(authenticated);
    Router::new().nest("/api/v1", api)
}

struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn bad_request(code: &'static str, message: impl Into<String>) -> ApiError {
        ApiError { status: StatusCode::BAD_REQUEST, code, message: message.into() }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({"error": {"code": self.code, "message": self.message}});
        (self.status, Json(body)).into_response()
    }
}

impl From<crate::Error> for ApiError {
    fn from(error: crate::Error) -> ApiError {
        tracing::error!(%error, "API request failed");
        ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal",
            message: "internal error".into(),
        }
    }
}

async fn require_auth(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    let AuthMode::Token(expected) = &state.config.auth;
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let authorised =
        presented.is_some_and(|token| bool::from(token.as_bytes().ct_eq(expected.as_bytes())));
    if !authorised {
        return ApiError {
            status: StatusCode::UNAUTHORIZED,
            code: "unauthorized",
            message: "a valid bearer token is required".into(),
        }
        .into_response();
    }
    next.run(request).await
}

async fn health() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol_version": protocol::PROTOCOL_VERSION,
    }))
}

async fn list_devices(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    let mut devices = state.store.call(|conn| store::list_devices(conn)).await?;
    // The connection table is the authority on who is online; heartbeats
    // refresh last-seen in memory only.
    for device in &mut devices {
        match state.connection(&device.id) {
            Some(connection) => {
                device.online = true;
                device.last_seen_at = connection.last_seen.load(Ordering::Relaxed);
            }
            None => device.online = false,
        }
    }
    Ok(Json(json!({ "devices": devices })))
}

#[derive(Deserialize)]
struct CreateJob {
    kind: String,
    payload: Value,
    targets: TargetSelector,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum TargetSelector {
    All,
    Devices { device_ids: Vec<String> },
}

/// Checks a payload for its kind and returns it with defaults filled in.
fn validate_payload(kind: &str, payload: Value) -> Result<Value, ApiError> {
    match kind {
        job_kind::SCRIPT_RUN => {
            let script: ScriptRunPayload = serde_json::from_value(payload)
                .map_err(|e| ApiError::bad_request("invalid_payload", e.to_string()))?;
            if script.body.trim().is_empty() {
                return Err(ApiError::bad_request("invalid_payload", "body must not be empty"));
            }
            if script.timeout_secs == 0 || script.timeout_secs > MAX_TIMEOUT_SECS {
                return Err(ApiError::bad_request(
                    "invalid_payload",
                    format!("timeout_secs must be between 1 and {MAX_TIMEOUT_SECS}"),
                ));
            }
            let _: RunAs = script.run_as;
            Ok(serde_json::to_value(script).expect("payload serialises"))
        }
        other => Err(ApiError::bad_request(
            "unsupported_kind",
            format!("job kind {other:?} is not supported by this coordinator"),
        )),
    }
}

async fn create_job(
    State(state): State<Arc<AppState>>,
    Json(request): Json<CreateJob>,
) -> Result<Response, ApiError> {
    let payload = validate_payload(&request.kind, request.payload)?;
    let targets = match request.targets {
        TargetSelector::All => Targets::All,
        TargetSelector::Devices { device_ids } => Targets::Devices(device_ids),
    };
    let kind = request.kind;
    let created = state
        .store
        .call(move |conn| store::create_job(conn, &kind, &payload, Some("api-token"), &targets))
        .await?;
    let job = match created {
        Ok(job) => job,
        Err(CreateJobError::UnknownDevice(id)) => {
            return Err(ApiError::bad_request("unknown_device", format!("no device with id {id:?}")))
        }
        Err(CreateJobError::NoTargets) => {
            return Err(ApiError::bad_request("no_targets", "the job would have no target devices"))
        }
    };
    tracing::info!(job_id = %job.id, kind = %job.kind, targets = job.targets.len(), "job created");
    for target in &job.targets {
        state.dispatch(&target.device_id, false).await;
    }
    Ok((StatusCode::CREATED, Json(json!({ "job": job }))).into_response())
}

async fn list_jobs(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    let jobs = state.store.call(|conn| store::list_jobs(conn, MAX_LISTED_JOBS)).await?;
    Ok(Json(json!({ "jobs": jobs })))
}

async fn get_job(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let job = state.store.call(move |conn| store::get_job(conn, &id, true)).await?;
    match job {
        Some(job) => Ok(Json(json!({ "job": job }))),
        None => Err(ApiError {
            status: StatusCode::NOT_FOUND,
            code: "not_found",
            message: "no such job".into(),
        }),
    }
}
