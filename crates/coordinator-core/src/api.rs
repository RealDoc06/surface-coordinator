//! The HTTP API used by the UI. The contract is `docs/api.md`.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use protocol::{job_kind, ScriptRunPayload, WifiJoinPayload};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::{self, Identity};
use crate::events::{self, Event};
use crate::state::AppState;
use crate::store::{self, CreateJobError, Targets};
use crate::AuthMode;

const DEFAULT_LISTED_JOBS: u32 = 50;
const MAX_LISTED_JOBS: u32 = 200;
const DEVICE_TARGETS: u32 = 50;
const MAX_TIMEOUT_SECS: u64 = 24 * 60 * 60;
const MAX_LABEL_LEN: usize = 100;
const MAX_SSID_BYTES: usize = 32;
const MIN_WIFI_PASSWORD_CHARS: usize = 8;
const MAX_WIFI_PASSWORD_CHARS: usize = 63;
const MAX_COORDINATOR_URL_BYTES: usize = 2048;

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    let open = Router::new()
        .route("/health", get(health))
        .route("/session", get(session))
        .route("/auth/login", post(login))
        .route("/auth/token", post(token_login))
        .route("/auth/logout", post(logout));

    let mut protected = Router::new()
        .route("/devices", get(list_devices))
        .route("/devices/{id}", get(get_device).delete(delete_device))
        .route("/jobs", get(list_jobs).post(create_job))
        .route("/jobs/{id}", get(get_job))
        .route("/enrollment-keys", get(list_keys).post(create_key))
        .route("/enrollment-keys/{id}/secret", get(export_key))
        .route("/enrollment-keys/{id}/revoke", post(revoke_key))
        .route("/rejected-connections", get(list_rejected))
        .route("/accounts", get(list_accounts).post(create_account))
        .route("/accounts/{id}", axum::routing::delete(delete_account))
        .route("/accounts/{id}/password", post(set_password))
        .route("/events", get(events::upgrade));
    if let Some(extra) = state.config.extra_api_routes.clone() {
        // Routes supplied by the embedder (the manager), behind the same
        // authentication as everything else.
        protected = protected.nest_service("/manager", extra);
    }
    let protected =
        protected.route_layer(middleware::from_fn_with_state(state, auth::require_auth));

    Router::new()
        .nest("/api/v1", open.merge(protected))
        .layer(middleware::from_fn(auth::same_origin))
}

// ---------------------------------------------------------------- errors

pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> ApiError {
        ApiError { status, code, message: message.into() }
    }

    fn bad_request(code: &'static str, message: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::BAD_REQUEST, code, message)
    }

    fn not_found(what: &str) -> ApiError {
        ApiError::new(StatusCode::NOT_FOUND, "not_found", format!("no such {what}"))
    }

    pub fn unauthorized() -> ApiError {
        ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", "authentication is required")
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
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", "internal error")
    }
}

type ApiResult<T = Json<Value>> = Result<T, ApiError>;

fn no_content() -> Response {
    StatusCode::NO_CONTENT.into_response()
}

// ---------------------------------------------------------------- session

async fn health() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol_version": protocol::PROTOCOL_VERSION,
    }))
}

async fn session(State(state): State<Arc<AppState>>, headers: HeaderMap) -> ApiResult {
    let identity = auth::identify(&state, &headers).await?;
    let accounts_mode = matches!(state.config.auth, AuthMode::Accounts);
    let account = identity
        .as_ref()
        .and_then(|identity| identity.account.as_ref())
        .map(|account| json!({"id": account.id, "username": account.username}));
    Ok(Json(json!({
        "authenticated": identity.is_some(),
        "auth_mode": if accounts_mode { "accounts" } else { "token" },
        "account": account,
        "capabilities": {
            "accounts": accounts_mode,
            "hotspot": state.config.capabilities.hotspot,
        },
        "version": env!("CARGO_PKG_VERSION"),
        "protocol_version": protocol::PROTOCOL_VERSION,
    })))
}

fn with_cookie(mut response: Response, cookie: String) -> Response {
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

async fn open_session(state: &AppState, account_id: Option<String>) -> Result<String, crate::Error> {
    let (token, hash) = auth::new_session_token();
    let expires = store::now_ms() + auth::SESSION_LIFETIME.as_millis() as i64;
    state
        .store
        .call(move |conn| store::create_session(conn, &hash, account_id.as_deref(), expires))
        .await?;
    Ok(auth::session_cookie(&token, state.config.secure_cookies))
}

#[derive(Deserialize)]
struct Login {
    username: String,
    password: String,
}

async fn login(State(state): State<Arc<AppState>>, Json(request): Json<Login>) -> ApiResult<Response> {
    if !matches!(state.config.auth, AuthMode::Accounts) {
        return Err(ApiError::bad_request("wrong_auth_mode", "this coordinator does not use accounts"));
    }
    if !state.login_throttle.allowed(&request.username) {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "too_many_attempts",
            "too many failed attempts; try again in a little while",
        ));
    }
    let username = request.username.clone();
    let found = state.store.call(move |conn| store::get_account_for_login(conn, &username)).await?;
    let password = request.password;
    // Verify even when there is no such account, so the response time does
    // not reveal which usernames exist.
    let (account, hash) = match found {
        Some((account, hash)) => (Some(account), hash),
        None => (None, state.dummy_password_hash.clone()),
    };
    let correct = tokio::task::spawn_blocking(move || auth::verify_password(&password, &hash))
        .await
        .unwrap_or(false);
    match account {
        Some(account) if correct => {
            state.login_throttle.record_success(&request.username);
            let cookie = open_session(&state, Some(account.id.clone())).await?;
            tracing::info!(username = %account.username, "login");
            let body = Json(json!({"account": {"id": account.id, "username": account.username}}));
            Ok(with_cookie(body.into_response(), cookie))
        }
        _ => {
            state.login_throttle.record_failure(&request.username);
            tracing::warn!(username = %request.username, "failed login");
            Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                "invalid_credentials",
                "wrong username or password",
            ))
        }
    }
}

#[derive(Deserialize)]
struct TokenLogin {
    token: String,
}

async fn token_login(
    State(state): State<Arc<AppState>>,
    Json(request): Json<TokenLogin>,
) -> ApiResult<Response> {
    let AuthMode::Token(expected) = &state.config.auth else {
        return Err(ApiError::bad_request("wrong_auth_mode", "this coordinator uses accounts"));
    };
    if !auth::token_matches(expected, &request.token) {
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "invalid_token", "the token is not valid"));
    }
    let cookie = open_session(&state, None).await?;
    Ok(with_cookie(no_content(), cookie))
}

async fn logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> ApiResult<Response> {
    if let Some(token) = auth::session_token_from(&headers) {
        let hash = auth::hash_token(token);
        state.store.call(move |conn| store::delete_session(conn, &hash)).await?;
    }
    Ok(with_cookie(no_content(), auth::clear_session_cookie(state.config.secure_cookies)))
}

// ---------------------------------------------------------------- accounts

fn require_accounts_mode(state: &AppState) -> Result<(), ApiError> {
    match state.config.auth {
        AuthMode::Accounts => Ok(()),
        AuthMode::Token(_) => Err(ApiError::not_found("route")),
    }
}

async fn list_accounts(State(state): State<Arc<AppState>>) -> ApiResult {
    require_accounts_mode(&state)?;
    let accounts = state.store.call(|conn| store::list_accounts(conn)).await?;
    Ok(Json(json!({ "accounts": accounts })))
}

fn check_password(password: &str) -> Result<(), ApiError> {
    if auth::valid_password(password) {
        Ok(())
    } else {
        Err(ApiError::bad_request(
            "weak_password",
            format!("the password must have at least {} characters", auth::MIN_PASSWORD_LEN),
        ))
    }
}

async fn create_account(
    State(state): State<Arc<AppState>>,
    Json(request): Json<Login>,
) -> ApiResult<Response> {
    require_accounts_mode(&state)?;
    if !auth::valid_username(&request.username) {
        return Err(ApiError::bad_request(
            "invalid_username",
            "usernames are 1 to 64 characters: letters, digits, '.', '_' and '-'",
        ));
    }
    check_password(&request.password)?;
    let created = state
        .store
        .call(move |conn| {
            let hash = auth::hash_password(&request.password);
            store::create_account(conn, &request.username, &hash)
        })
        .await?;
    match created {
        Some(account) => {
            tracing::info!(username = %account.username, "account created");
            Ok((StatusCode::CREATED, Json(json!({ "account": account }))).into_response())
        }
        None => Err(ApiError::new(StatusCode::CONFLICT, "username_taken", "that username is taken")),
    }
}

#[derive(Deserialize)]
struct NewPassword {
    password: String,
}

async fn set_password(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(request): Json<NewPassword>,
) -> ApiResult<Response> {
    require_accounts_mode(&state)?;
    check_password(&request.password)?;
    let changed = state
        .store
        .call(move |conn| {
            let hash = auth::hash_password(&request.password);
            store::set_account_password(conn, &id, &hash)
        })
        .await?;
    if changed { Ok(no_content()) } else { Err(ApiError::not_found("account")) }
}

async fn delete_account(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<Identity>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    require_accounts_mode(&state)?;
    if identity.account.as_ref().is_some_and(|account| account.id == id) {
        return Err(ApiError::bad_request("cannot_delete_self", "you cannot delete your own account"));
    }
    let deleted = state.store.call(move |conn| store::delete_account(conn, &id)).await?;
    if deleted { Ok(no_content()) } else { Err(ApiError::not_found("account")) }
}

// ---------------------------------------------------------------- devices

async fn list_devices(State(state): State<Arc<AppState>>) -> ApiResult {
    let mut devices = state.store.call(|conn| store::list_devices(conn)).await?;
    for device in &mut devices {
        state.apply_presence(device);
    }
    Ok(Json(json!({ "devices": devices })))
}

async fn get_device(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult {
    let found = state
        .store
        .call(move |conn| {
            let Some(device) = store::get_device(conn, &id)? else { return Ok(None) };
            let targets = store::list_device_targets(conn, &id, DEVICE_TARGETS)?;
            Ok(Some((device, targets)))
        })
        .await?;
    let Some((mut device, targets)) = found else {
        return Err(ApiError::not_found("device"));
    };
    state.apply_presence(&mut device);
    Ok(Json(json!({ "device": device, "targets": targets })))
}

async fn delete_device(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult<Response> {
    let _presence = state.presence.lock().await;
    // Hang up first; the connection's own teardown then finds nothing to do.
    state.drop_connection(&id);
    let device_id = id.clone();
    let deleted = state.store.call(move |conn| store::delete_device(conn, &device_id)).await?;
    if !deleted {
        return Err(ApiError::not_found("device"));
    }
    tracing::info!(device_id = %id, "device forgotten");
    state.publish(Event::DeviceRemoved { device_id: id });
    Ok(no_content())
}

// ---------------------------------------------------------------- jobs

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
            Ok(serde_json::to_value(script).expect("payload serialises"))
        }
        job_kind::WIFI_JOIN => {
            let wifi: WifiJoinPayload = serde_json::from_value(payload)
                .map_err(|e| ApiError::bad_request("invalid_payload", e.to_string()))?;
            // The 802.11 limits: an SSID is up to 32 bytes, a WPA passphrase
            // 8 to 63 characters.
            if wifi.ssid.is_empty() || wifi.ssid.len() > MAX_SSID_BYTES {
                return Err(ApiError::bad_request(
                    "invalid_payload",
                    format!("ssid must be 1 to {MAX_SSID_BYTES} bytes"),
                ));
            }
            if let Some(password) = &wifi.password {
                if !(MIN_WIFI_PASSWORD_CHARS..=MAX_WIFI_PASSWORD_CHARS).contains(&password.chars().count()) {
                    return Err(ApiError::bad_request(
                        "invalid_payload",
                        format!(
                            "password must be {MIN_WIFI_PASSWORD_CHARS} to {MAX_WIFI_PASSWORD_CHARS} \
                             characters; omit it for an open network"
                        ),
                    ));
                }
            }
            if let Some(url) = &wifi.coordinator_url {
                if !(url.starts_with("ws://") || url.starts_with("wss://")) {
                    return Err(ApiError::bad_request(
                        "invalid_payload",
                        "coordinator_url must start with ws:// or wss://",
                    ));
                }
                if url.len() > MAX_COORDINATOR_URL_BYTES {
                    return Err(ApiError::bad_request(
                        "invalid_payload",
                        format!("coordinator_url must be at most {MAX_COORDINATOR_URL_BYTES} bytes"),
                    ));
                }
            }
            Ok(serde_json::to_value(wifi).expect("payload serialises"))
        }
        other => Err(ApiError::bad_request(
            "unsupported_kind",
            format!("job kind {other:?} is not supported by this coordinator"),
        )),
    }
}

async fn create_job(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<Identity>,
    Json(request): Json<CreateJob>,
) -> ApiResult<Response> {
    let payload = validate_payload(&request.kind, request.payload)?;
    let targets = match request.targets {
        TargetSelector::All => Targets::All,
        TargetSelector::Devices { device_ids } => Targets::Devices(device_ids),
    };
    let kind = request.kind;
    let created_by = identity.username().map(str::to_owned);
    let created = state
        .store
        .call(move |conn| store::create_job(conn, &kind, &payload, created_by.as_deref(), &targets))
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
    state.publish(Event::JobCreated { job: job.clone() });
    for target in &job.targets {
        state.dispatch(&target.device_id, false).await;
    }
    Ok((StatusCode::CREATED, Json(json!({ "job": job }))).into_response())
}

#[derive(Deserialize)]
struct JobsQuery {
    limit: Option<u32>,
    before: Option<i64>,
}

async fn list_jobs(State(state): State<Arc<AppState>>, Query(query): Query<JobsQuery>) -> ApiResult {
    let limit = query.limit.unwrap_or(DEFAULT_LISTED_JOBS).clamp(1, MAX_LISTED_JOBS);
    let jobs = state.store.call(move |conn| store::list_jobs(conn, limit, query.before)).await?;
    Ok(Json(json!({ "jobs": jobs })))
}

async fn get_job(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult {
    let job = state.store.call(move |conn| store::get_job(conn, &id, true)).await?;
    match job {
        Some(job) => Ok(Json(json!({ "job": job }))),
        None => Err(ApiError::not_found("job")),
    }
}

// ---------------------------------------------------------------- enrollment keys

async fn list_keys(State(state): State<Arc<AppState>>) -> ApiResult {
    let keys = state.store.call(|conn| store::list_key_summaries(conn)).await?;
    Ok(Json(json!({ "keys": keys })))
}

#[derive(Deserialize)]
struct CreateKey {
    #[serde(default)]
    label: String,
    id: Option<String>,
    secret: Option<String>,
}

fn valid_key_id(id: &str) -> bool {
    (8..=64).contains(&id.len())
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn valid_key_secret(secret: &str) -> bool {
    (32..=256).contains(&secret.len()) && secret.bytes().all(|b| b.is_ascii_graphic())
}

async fn create_key(
    State(state): State<Arc<AppState>>,
    Json(request): Json<CreateKey>,
) -> ApiResult<Response> {
    let label = request.label.trim().to_owned();
    if label.chars().count() > MAX_LABEL_LEN {
        return Err(ApiError::bad_request("invalid_key", "the label is too long"));
    }
    let key = match (request.id, request.secret) {
        (None, None) => state.store.call(move |conn| store::create_enrollment_key(conn, &label)).await?,
        (Some(id), Some(secret)) => {
            if !valid_key_id(&id) || !valid_key_secret(&secret) {
                return Err(ApiError::bad_request(
                    "invalid_key",
                    "the key id or secret is not in the expected format",
                ));
            }
            state
                .store
                .call(move |conn| store::import_enrollment_key(conn, &id, &secret, &label))
                .await?
                .ok_or_else(|| {
                    ApiError::new(StatusCode::CONFLICT, "key_exists", "a key with this id already exists")
                })?
        }
        _ => {
            return Err(ApiError::bad_request(
                "invalid_key",
                "to import a key, give both its id and its secret",
            ))
        }
    };
    tracing::info!(key_id = %key.id, "enrollment key added");
    let body = json!({"key": {
        "id": key.id,
        "label": key.label,
        "created_at": key.created,
        "revoked_at": key.revoked,
        "device_count": 0,
        "secret": key.secret,
    }});
    Ok((StatusCode::CREATED, Json(body)).into_response())
}

async fn export_key(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult {
    let key = state.store.call(move |conn| store::get_enrollment_key(conn, &id)).await?;
    match key {
        Some(key) => Ok(Json(json!({"id": key.id, "secret": key.secret}))),
        None => Err(ApiError::not_found("enrollment key")),
    }
}

async fn revoke_key(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult<Response> {
    let outcome = state
        .store
        .call(move |conn| {
            if store::get_enrollment_key(conn, &id)?.is_none() {
                return Ok(false);
            }
            store::revoke_enrollment_key(conn, &id)?;
            Ok(true)
        })
        .await?;
    if outcome { Ok(no_content()) } else { Err(ApiError::not_found("enrollment key")) }
}

// ---------------------------------------------------------------- rejected connections

async fn list_rejected(State(state): State<Arc<AppState>>) -> ApiResult {
    let rejected = state.store.call(|conn| store::list_rejected(conn)).await?;
    Ok(Json(json!({ "rejected": rejected })))
}

/// Refreshes a device's presence fields from the live connection table.
impl AppState {
    pub fn apply_presence(&self, device: &mut store::Device) {
        // The connection table is the authority on who is online; heartbeats
        // refresh last-seen in memory only.
        match self.connection(&device.id) {
            Some(connection) => {
                device.online = true;
                device.last_seen_at = connection.last_seen.load(Ordering::Relaxed);
            }
            None => device.online = false,
        }
    }
}
