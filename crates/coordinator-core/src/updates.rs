//! Client updates: the cache of verified client releases, fed from the
//! release source (the GitHub repository's latest release) or uploaded by an
//! operator, the API that shows and refreshes it, and the downloads clients
//! install from.
//!
//! Nothing reaches the cache unverified: an artifact is written to a
//! temporary file, its signature checked against the update public key, and
//! only then moved into `<data dir>/updates/<version>/` and recorded. Only
//! recorded files are served.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use axum::body::Body;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::StreamExt;
use protocol::update::{self, Version, TARGET_WINDOWS_X86_64};
use protocol::ClientUpdatePayload;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

use crate::api::{ApiError, ApiResult};
use crate::events::Event;
use crate::rollout::{self, Rollout};
use crate::state::AppState;
use crate::store::{self, ClientRelease, Store};
use crate::{Config, Error};

/// The repository whose releases are checked unless configured otherwise.
pub const DEFAULT_SOURCE: &str = "RealDoc06/surface-coordinator";
pub const DEFAULT_API_BASE: &str = "https://api.github.com";
pub const DEFAULT_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Targets this coordinator caches and serves. Paths are only ever built
/// from these, never from a request.
const TARGETS: &[&str] = &[TARGET_WINDOWS_X86_64];

const MAX_ARTIFACT_BYTES: u64 = 200 * 1024 * 1024;
const MAX_SIGNATURE_BYTES: u64 = 4 * 1024;
const MAX_UPLOAD_NOTES_CHARS: usize = 4000;
/// A release description with its asset list; far smaller in practice.
const MAX_RELEASE_BYTES: u64 = 4 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const ARTIFACT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const SMALL_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

const UPDATES_DIR: &str = "updates";
/// Downloads in progress, in the updates directory so the final move stays
/// on one file system. Left-overs of an interrupted run are removed at start.
const TEMP_PREFIX: &str = ".download-";

#[derive(Debug, Clone)]
pub struct UpdateConfig {
    /// Check the release source periodically and on request. Without an
    /// update public key nothing is checked either way. Uploads do not
    /// depend on this.
    pub enabled: bool,
    /// `owner/repository` on GitHub.
    pub source: String,
    /// Base URL of the GitHub API; tests point it at a local fake.
    pub api_base: String,
    pub check_interval: Duration,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        UpdateConfig {
            enabled: true,
            source: DEFAULT_SOURCE.to_owned(),
            api_base: DEFAULT_API_BASE.to_owned(),
            check_interval: DEFAULT_CHECK_INTERVAL,
        }
    }
}

/// The update public key compiled into this build from
/// `release-keys/client-update.pub`, if the file existed.
pub fn built_in_public_key() -> Option<String> {
    let key = env!("SC_CLIENT_UPDATE_PUBKEY").trim();
    (!key.is_empty()).then(|| key.to_owned())
}

fn public_key(config: &Config) -> Option<&str> {
    config.update_public_key.as_deref().map(str::trim).filter(|key| !key.is_empty())
}

/// Whether the release source is checked: turned on, and there is a key to
/// verify releases with.
pub fn checking(config: &Config) -> bool {
    config.updates.enabled && public_key(config).is_some()
}

/// Refuses configurations that would build odd URLs or never tick.
pub fn validate_config(config: &Config) -> Result<(), Error> {
    if !checking(config) {
        return Ok(());
    }
    let updates = &config.updates;
    let part = |text: &str| {
        !text.is_empty()
            && text != "."
            && text != ".."
            && text.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    match updates.source.split_once('/') {
        Some((owner, repo)) if part(owner) && part(repo) => {}
        _ => {
            return Err(Error::InvalidConfig(format!(
                "the update source must be owner/repository, not {:?}",
                updates.source
            )))
        }
    }
    if !(updates.api_base.starts_with("https://") || updates.api_base.starts_with("http://")) {
        return Err(Error::InvalidConfig("the update API base must be an http(s) URL".into()));
    }
    if updates.check_interval.is_zero() {
        return Err(Error::InvalidConfig("the update check interval must not be zero".into()));
    }
    Ok(())
}

// ---------------------------------------------------------------- state

#[derive(Default)]
struct Status {
    last_checked_at: Option<i64>,
    last_error: Option<String>,
}

/// The release cache's state that lives in memory.
pub struct UpdateState {
    status: Mutex<Status>,
    /// One check or upload storing at a time: a request arriving during a
    /// check waits for it and then finds the release cached, and two uploads
    /// of one version cannot both be moved into place.
    check: tokio::sync::Mutex<()>,
    /// Newest cached version. Every view of a device needs it, so it is kept
    /// here rather than queried each time.
    newest: Mutex<Option<Version>>,
    /// Present only when checking is enabled.
    http: Option<reqwest::Client>,
    /// The stored auto-update setting, mirrored so the sweep need not read it.
    auto_update: AtomicBool,
    /// One rollout step at a time, so two runs cannot both pick a canary or
    /// both target the same devices.
    pub(crate) rollout: tokio::sync::Mutex<()>,
}

impl UpdateState {
    /// `auto_update` is the stored setting (see [`store::setting::AUTO_UPDATE`]).
    pub fn new(config: &Config, newest: Option<Version>, auto_update: bool) -> Result<UpdateState, Error> {
        let http = if checking(config) {
            let client = reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .user_agent(format!("surface-coordinator/{}", env!("CARGO_PKG_VERSION")))
                .build()
                .map_err(|error| Error::Internal(format!("cannot create the HTTP client: {error}")))?;
            Some(client)
        } else {
            None
        };
        Ok(UpdateState {
            status: Mutex::default(),
            check: tokio::sync::Mutex::new(()),
            newest: Mutex::new(newest),
            http,
            auto_update: AtomicBool::new(auto_update),
            rollout: tokio::sync::Mutex::new(()),
        })
    }

    fn status(&self) -> std::sync::MutexGuard<'_, Status> {
        self.status.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn auto_update(&self) -> bool {
        self.auto_update.load(Ordering::Relaxed)
    }

    pub(crate) fn newest(&self) -> Option<Version> {
        *self.newest.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn set_newest(&self, version: Option<Version>) {
        *self.newest.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = version;
    }

    /// Whether a client reporting `client_version` is older than the newest
    /// cached release. A version that does not parse is never older.
    pub fn is_newer_than(&self, client_version: &str) -> bool {
        match (self.newest(), client_version.parse::<Version>()) {
            (Some(newest), Ok(running)) => running < newest,
            _ => false,
        }
    }
}

fn newest_of(releases: &[ClientRelease]) -> Option<Version> {
    releases.iter().filter_map(|release| release.version.parse().ok()).max()
}

// ---------------------------------------------------------------- files

fn known_target(text: &str) -> Option<&'static str> {
    TARGETS.iter().copied().find(|target| *target == text)
}

fn updates_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(UPDATES_DIR)
}

fn artifact_path(data_dir: &Path, version: &Version, target: &'static str) -> PathBuf {
    updates_dir(data_dir).join(version.to_string()).join(format!("client-{target}.exe"))
}

fn signature_path(data_dir: &Path, version: &Version, target: &'static str) -> PathBuf {
    updates_dir(data_dir).join(version.to_string()).join(format!("client-{target}.exe.sig"))
}

/// The release asset names CI publishes.
fn asset_name(version: &Version, target: &str) -> String {
    format!("surface-coordinator-client-{version}-{target}.exe")
}

/// Brings the cache index in line with the files at start: forgets entries
/// whose files are gone (or truncated) and removes interrupted downloads.
/// Returns the newest version left.
pub fn load_cache(store: &Store, data_dir: &Path) -> Result<Option<Version>, Error> {
    if let Ok(entries) = std::fs::read_dir(updates_dir(data_dir)) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(TEMP_PREFIX) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let data_dir = data_dir.to_owned();
    store.call_blocking(|conn| {
        let mut kept = Vec::new();
        for release in store::list_client_releases(conn)? {
            let complete = match (release.version.parse::<Version>(), known_target(&release.target)) {
                (Ok(version), Some(target)) => {
                    let artifact = std::fs::metadata(artifact_path(&data_dir, &version, target));
                    artifact.is_ok_and(|meta| meta.is_file() && meta.len() == release.size)
                        && signature_path(&data_dir, &version, target).is_file()
                }
                _ => false,
            };
            if complete {
                kept.push(release);
            } else {
                tracing::warn!(version = %release.version, target = %release.target,
                    "cached client release is missing its files; forgetting it");
                store::delete_client_release(conn, &release.version, &release.target)?;
            }
        }
        Ok(newest_of(&kept))
    })
}

/// Removes a file when dropped unless it was kept.
struct TempFile {
    path: PathBuf,
    keep: bool,
}

impl TempFile {
    /// A fresh name in `dir` (the updates directory); nothing is created yet.
    fn new(dir: &Path) -> TempFile {
        TempFile { path: dir.join(format!("{TEMP_PREFIX}{}", uuid::Uuid::new_v4().simple())), keep: false }
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

// ---------------------------------------------------------------- checking

/// What the GitHub API returns for a release; only the fields used.
#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

/// An error and its causes in one line; reqwest's own text stops at
/// "error sending request".
fn describe(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

async fn get(http: &reqwest::Client, url: &str, timeout: Duration) -> Result<reqwest::Response, String> {
    let response = http
        .get(url)
        .timeout(timeout)
        .send()
        .await
        .map_err(|error| format!("could not fetch {url}: {}", describe(&error)))?;
    if !response.status().is_success() {
        return Err(format!("fetching {url} failed: the server answered {}", response.status()));
    }
    Ok(response)
}

/// Reads a whole response, refusing more than `max` bytes.
async fn read_capped(mut response: reqwest::Response, url: &str, max: u64) -> Result<Vec<u8>, String> {
    let too_large = || format!("{url} is larger than the {max} byte limit");
    if response.content_length().is_some_and(|length| length > max) {
        return Err(too_large());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| format!("reading {url} failed: {}", describe(&e)))? {
        if (body.len() + chunk.len()) as u64 > max {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Streams a download into `path`, hashing it on the way. Returns the size
/// and SHA-256.
async fn download_to(http: &reqwest::Client, url: &str, path: &Path, max: u64) -> Result<(u64, [u8; 32]), String> {
    let mut response = get(http, url, ARTIFACT_TIMEOUT).await?;
    let too_large = || format!("{url} is larger than the {max} byte limit");
    if response.content_length().is_some_and(|length| length > max) {
        return Err(too_large());
    }
    let write_failed = |error: std::io::Error| format!("could not write the download to {}: {error}", path.display());
    let mut file = tokio::fs::File::create(path).await.map_err(write_failed)?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    while let Some(chunk) = response.chunk().await.map_err(|e| format!("downloading {url} failed: {}", describe(&e)))? {
        size += chunk.len() as u64;
        if size > max {
            return Err(too_large());
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await.map_err(write_failed)?;
    }
    file.sync_all().await.map_err(write_failed)?;
    Ok((size, hasher.finalize().into()))
}

/// Reads the latest release and caches its client artifact if it is new.
/// Returns the release if it was added to the cache.
async fn fetch_latest(state: &AppState, http: &reqwest::Client) -> Result<Option<ClientRelease>, String> {
    let config = &state.config;
    let Some(key) = public_key(config) else { return Ok(None) };
    let url = format!(
        "{}/repos/{}/releases/latest",
        config.updates.api_base.trim_end_matches('/'),
        config.updates.source
    );
    let response = http
        .get(&url)
        .header(header::ACCEPT, "application/vnd.github+json")
        .timeout(SMALL_REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|error| format!("could not reach the release source {url}: {}", describe(&error)))?;
    if !response.status().is_success() {
        return Err(format!("the release source {url} answered {}", response.status()));
    }
    let body = read_capped(response, &url, MAX_RELEASE_BYTES).await?;
    let release: GithubRelease = serde_json::from_slice(&body)
        .map_err(|error| format!("the release from {url} is not in the expected format: {error}"))?;
    let tag = release.tag_name.as_str();
    let version: Version = tag
        .strip_prefix('v')
        .unwrap_or(tag)
        .parse()
        .map_err(|_| format!("the latest release's tag {tag:?} is not a version of the form vMAJOR.MINOR.PATCH"))?;
    // The canonical spelling is what is signed, stored and served.
    let version_text = version.to_string();
    let target = TARGET_WINDOWS_X86_64;

    let lookup = version_text.clone();
    let cached = state
        .store
        .call(move |conn| store::get_client_release(conn, &lookup, target))
        .await
        .map_err(|error| format!("could not read the update cache: {error}"))?;
    if cached.is_some() {
        return Ok(None);
    }

    let name = asset_name(&version, target);
    let signature_name = format!("{name}.sig");
    let find = |wanted: &str| {
        release.assets.iter().find(|asset| asset.name == wanted).map(|asset| asset.browser_download_url.clone())
    };
    let artifact_url =
        find(&name).ok_or_else(|| format!("release {version} has no client artifact (no asset named {name})"))?;
    let signature_url = find(&signature_name)
        .ok_or_else(|| format!("release {version} has no client signature (no asset named {signature_name})"))?;

    let signature = read_capped(get(http, &signature_url, SMALL_REQUEST_TIMEOUT).await?, &signature_url, MAX_SIGNATURE_BYTES)
        .await?;
    let signature = String::from_utf8(signature)
        .map_err(|_| format!("the signature of release {version} is not base64 text"))?
        .trim()
        .to_owned();

    let dir = updates_dir(&config.data_dir);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|error| format!("could not create {}: {error}", dir.display()))?;
    let mut temp = TempFile::new(&dir);
    let (size, sha256) = download_to(http, &artifact_url, &temp.path, MAX_ARTIFACT_BYTES).await?;
    update::verify(key, &version_text, target, &sha256, &signature)
        .map_err(|error| format!("the signature of release {version} does not verify: {error}"))?;
    let verified = Verified { version, target, size, sha256, signature, notes: release.body };
    store_verified(state, &mut temp, verified).await.map(Some)
}

/// An artifact in a temporary file whose signature has been checked.
struct Verified {
    version: Version,
    target: &'static str,
    size: u64,
    sha256: [u8; 32],
    signature: String,
    notes: Option<String>,
}

/// Moves a verified artifact into the cache and records it; until it is
/// recorded it is not served. Downloaded and uploaded releases end up the
/// same way. The caller holds the check lock.
async fn store_verified(state: &AppState, temp: &mut TempFile, verified: Verified) -> Result<ClientRelease, String> {
    let Verified { version, target, size, sha256, signature, notes } = verified;
    let data_dir = &state.config.data_dir;
    let artifact = artifact_path(data_dir, &version, target);
    let move_failed = |error: std::io::Error| format!("could not store release {version}: {error}");
    if let Some(parent) = artifact.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(move_failed)?;
    }
    tokio::fs::write(signature_path(data_dir, &version, target), format!("{}\n", signature.trim()))
        .await
        .map_err(move_failed)?;
    tokio::fs::rename(&temp.path, &artifact).await.map_err(move_failed)?;
    temp.keep = true;

    let release = ClientRelease {
        version: version.to_string(),
        target: target.to_owned(),
        size,
        sha256: hex::encode(sha256),
        cached_at: store::now_ms(),
        notes: notes.filter(|notes| !notes.trim().is_empty()),
    };
    let record = release.clone();
    state
        .store
        .call(move |conn| store::insert_client_release(conn, &record))
        .await
        .map_err(|error| format!("could not record release {version}: {error}"))?;
    Ok(release)
}

/// After a release was added to the cache: devices' `update_available` may
/// have changed, and the rollout may have a new version to roll out.
async fn release_cached(state: &AppState, release: &ClientRelease) {
    tracing::info!(version = %release.version, target = %release.target, size = release.size,
        "client release verified and cached");
    match state.store.call(|conn| store::list_client_releases(conn)).await {
        Ok(releases) => state.updates.set_newest(newest_of(&releases)),
        Err(error) => tracing::error!(%error, "failed to reload the update cache"),
    }
    state.publish(Event::Resync);
    rollout::run(state).await;
}

/// Checks the release source now. Returns why it failed; the reason is also
/// kept for the API. Does nothing when checking is disabled.
pub async fn check(state: &AppState) -> Result<(), String> {
    let Some(http) = &state.updates.http else { return Ok(()) };
    let outcome = {
        let _one_at_a_time = state.updates.check.lock().await;
        let outcome = fetch_latest(state, http).await;
        let mut status = state.updates.status();
        status.last_checked_at = Some(store::now_ms());
        status.last_error = outcome.as_ref().err().cloned();
        outcome
    };
    match outcome {
        Ok(Some(release)) => {
            release_cached(state, &release).await;
            Ok(())
        }
        Ok(None) => Ok(()),
        Err(reason) => {
            tracing::warn!(%reason, "client update check failed");
            Err(reason)
        }
    }
}

/// Checks at start and then every `check_interval` until the server stops.
pub async fn check_periodically(state: std::sync::Arc<AppState>) {
    let mut ticker = tokio::time::interval(state.config.updates.check_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = state.shutdown.cancelled() => return,
            _ = ticker.tick() => {}
        }
        // A download can take minutes; stopping does not wait for it.
        tokio::select! {
            _ = state.shutdown.cancelled() => return,
            _ = check(&state) => {}
        }
    }
}

// ---------------------------------------------------------------- API

/// `Updates` in `docs/api.md`.
#[derive(Serialize)]
pub struct Updates {
    enabled: bool,
    checking: bool,
    auto_update: bool,
    source: Option<String>,
    last_checked_at: Option<i64>,
    last_error: Option<String>,
    latest: Option<ClientRelease>,
    cached: Vec<ClientRelease>,
    rollout: Option<Rollout>,
}

async fn snapshot(state: &AppState) -> Result<Updates, Error> {
    let cached = state.store.call(|conn| store::list_client_releases(conn)).await?;
    let rollout = rollout::status(state).await?;
    let checking = checking(&state.config);
    let status = state.updates.status();
    Ok(Updates {
        enabled: public_key(&state.config).is_some(),
        checking,
        auto_update: state.updates.auto_update(),
        // Not shown while nothing is checked, so it does not look as if it were.
        source: checking.then(|| state.config.updates.source.clone()),
        last_checked_at: status.last_checked_at,
        last_error: status.last_error.clone(),
        latest: cached.first().cloned(),
        cached,
        rollout,
    })
}

/// `GET /api/v1/updates`.
pub async fn get_updates(State(state): State<std::sync::Arc<AppState>>) -> ApiResult<Json<Updates>> {
    Ok(Json(snapshot(&state).await?))
}

/// `POST /api/v1/updates/check`.
pub async fn check_now(State(state): State<std::sync::Arc<AppState>>) -> ApiResult<Json<Updates>> {
    if let Err(reason) = check(&state).await {
        return Err(ApiError::new(StatusCode::BAD_GATEWAY, "update_check_failed", reason));
    }
    Ok(Json(snapshot(&state).await?))
}

#[derive(Deserialize)]
pub struct UploadQuery {
    // Missing parameters are reported like wrong ones, in the API's format.
    #[serde(default)]
    version: String,
    #[serde(default)]
    target: String,
    notes: Option<String>,
}

/// `POST /api/v1/updates/upload?version=..&target=..[&notes=..]`: a client
/// executable signed with the update key, as the body, with its signature in
/// `X-Signature`. Stored exactly like a release fetched from the source.
pub async fn upload(
    State(state): State<std::sync::Arc<AppState>>,
    Query(query): Query<UploadQuery>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let UploadRequest { key, version, target, signature } = match upload_request(&state, &query, &headers).await {
        Ok(request) => request,
        Err(error) => {
            // Read what the client sends before answering: a connection
            // closed on an unread body can reach it as a reset instead of
            // this answer. Not worth it for a body announced as too large.
            if error.status != StatusCode::PAYLOAD_TOO_LARGE {
                discard(body).await;
            }
            return Err(error);
        }
    };
    let bad_signature = |message: String| ApiError::bad_request("bad_signature", message);
    let exists = || ApiError::new(StatusCode::CONFLICT, "version_exists", format!("version {version} is already cached"));

    // Streamed to disk and hashed on the way; never held in memory.
    let dir = updates_dir(&state.config.data_dir);
    let internal = |error: std::io::Error| ApiError::from(Error::from(error));
    tokio::fs::create_dir_all(&dir).await.map_err(internal)?;
    let mut temp = TempFile::new(&dir);
    let mut file = tokio::fs::File::create(&temp.path).await.map_err(internal)?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| ApiError::bad_request("bad_request", format!("reading the upload failed: {error}")))?;
        size += chunk.len() as u64;
        if size > MAX_ARTIFACT_BYTES {
            return Err(too_large());
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await.map_err(internal)?;
    }
    file.sync_all().await.map_err(internal)?;
    drop(file);
    let sha256: [u8; 32] = hasher.finalize().into();
    update::verify(key, &version.to_string(), target, &sha256, &signature)
        .map_err(|error| bad_signature(format!("the signature does not verify: {error}")))?;

    let notes = query.notes.map(|notes| notes.chars().take(MAX_UPLOAD_NOTES_CHARS).collect());
    let release = {
        let _one_at_a_time = state.updates.check.lock().await;
        // Another upload or a check may have cached it meanwhile.
        if is_cached(&state, &version, target).await? {
            return Err(exists());
        }
        let verified = Verified { version, target, size, sha256, signature, notes };
        store_verified(&state, &mut temp, verified).await.map_err(|reason| ApiError::from(Error::Internal(reason)))?
    };
    release_cached(&state, &release).await;
    Ok((StatusCode::CREATED, Json(snapshot(&state).await?)).into_response())
}

/// An upload whose query and headers make sense; the body is still unread.
struct UploadRequest<'a> {
    key: &'a str,
    version: Version,
    target: &'static str,
    signature: String,
}

/// Everything about an upload that can be refused before reading its body.
async fn upload_request<'a>(
    state: &'a AppState,
    query: &UploadQuery,
    headers: &HeaderMap,
) -> Result<UploadRequest<'a>, ApiError> {
    let Some(key) = public_key(&state.config) else {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "updates_unavailable",
            "no update key is configured, so uploads cannot be verified",
        ));
    };
    let version: Version = query.version.parse().map_err(|_| {
        ApiError::bad_request("invalid_version", "version must be of the form major.minor.patch")
    })?;
    let Some(target) = known_target(&query.target) else {
        return Err(ApiError::bad_request(
            "unsupported_target",
            format!("target must be one of: {}", TARGETS.join(", ")),
        ));
    };
    let signature = headers
        .get("x-signature")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().to_owned())
        .ok_or_else(|| {
            ApiError::bad_request("bad_signature", "the X-Signature header with the base64 signature is missing")
        })?;
    if is_cached(state, &version, target).await? {
        return Err(ApiError::new(StatusCode::CONFLICT, "version_exists", format!("version {version} is already cached")));
    }
    let announced = headers.get(header::CONTENT_LENGTH).and_then(|value| value.to_str().ok()?.parse::<u64>().ok());
    if announced.is_some_and(|length| length > MAX_ARTIFACT_BYTES) {
        return Err(too_large());
    }
    Ok(UploadRequest { key, version, target, signature })
}

fn too_large() -> ApiError {
    ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "too_large", format!("the upload exceeds {MAX_ARTIFACT_BYTES} bytes"))
}

/// Reads and drops a request body, up to the upload limit.
async fn discard(body: Body) {
    let mut stream = body.into_data_stream();
    let mut read = 0u64;
    while let Some(Ok(chunk)) = stream.next().await {
        read += chunk.len() as u64;
        if read > MAX_ARTIFACT_BYTES {
            return;
        }
    }
}

async fn is_cached(state: &AppState, version: &Version, target: &'static str) -> Result<bool, Error> {
    let version = version.to_string();
    let found = state.store.call(move |conn| store::get_client_release(conn, &version, target)).await?;
    Ok(found.is_some())
}

#[derive(Deserialize)]
pub struct AutoUpdateRequest {
    enabled: bool,
}

/// `POST /api/v1/updates/auto`: turns the automatic rollout on or off.
/// Turning it on rolls out an already cached release right away.
pub async fn set_auto_update(
    State(state): State<std::sync::Arc<AppState>>,
    Json(request): Json<AutoUpdateRequest>,
) -> ApiResult<Json<Updates>> {
    {
        // Under the rollout lock, so a run in progress finishes with the
        // setting it started with.
        let _rollout = state.updates.rollout.lock().await;
        let value = if request.enabled { "true" } else { "false" };
        state.store.call(move |conn| store::set_setting(conn, store::setting::AUTO_UPDATE, value)).await?;
        state.updates.auto_update.store(request.enabled, Ordering::Relaxed);
    }
    tracing::info!(enabled = request.enabled, "automatic client update rollout changed");
    rollout::run(&state).await;
    Ok(Json(snapshot(&state).await?))
}

#[derive(Deserialize)]
struct ClientUpdateRequest {
    version: String,
    #[serde(default)]
    downgrade: bool,
}

/// Turns the `client.update` payload the UI sends into the one devices get,
/// with the details of the cached artifact.
pub async fn client_update_payload(state: &AppState, payload: Value) -> Result<ClientUpdatePayload, ApiError> {
    let invalid = |message: String| ApiError::bad_request("invalid_payload", message);
    let request: ClientUpdateRequest = serde_json::from_value(payload).map_err(|e| invalid(e.to_string()))?;
    let version: Version = request
        .version
        .parse()
        .map_err(|_| invalid("version must be of the form major.minor.patch".to_owned()))?;
    let lookup = version.to_string();
    let release = state
        .store
        .call(move |conn| store::get_client_release(conn, &lookup, TARGET_WINDOWS_X86_64))
        .await?
        .ok_or_else(|| invalid(format!("version {version} is not cached")))?;
    Ok(ClientUpdatePayload {
        version: release.version,
        target: release.target,
        size: release.size,
        sha256: release.sha256,
        downgrade: request.downgrade,
    })
}

// ---------------------------------------------------------------- downloads

/// `GET /updates/client/{version}/{target}` and `.../{target}.sig` on the
/// client listener. Serves only what the cache index records; the path is
/// built from the parsed version and a known target, never from the request.
pub async fn download(
    State(state): State<std::sync::Arc<AppState>>,
    path: Result<UrlPath<(String, String)>, PathRejection>,
) -> Response {
    let not_found = || ApiError::not_found("update").into_response();
    let Ok(UrlPath((version_text, file))) = path else { return not_found() };
    let Ok(version) = version_text.parse::<Version>() else { return not_found() };
    // One URL per file: `01.2.3` is not another name for `1.2.3`.
    if version.to_string() != version_text {
        return not_found();
    }
    let (target_text, is_signature) = match file.strip_suffix(".sig") {
        Some(target) => (target, true),
        None => (file.as_str(), false),
    };
    let Some(target) = known_target(target_text) else { return not_found() };

    let recorded = state.store.call(move |conn| store::get_client_release(conn, &version_text, target)).await;
    match recorded {
        Ok(Some(_)) => {}
        Ok(None) => return not_found(),
        Err(error) => return ApiError::from(error).into_response(),
    }
    let path = if is_signature {
        signature_path(&state.config.data_dir, &version, target)
    } else {
        artifact_path(&state.config.data_dir, &version, target)
    };
    let Ok(file) = tokio::fs::File::open(&path).await else { return not_found() };
    let Ok(metadata) = file.metadata().await else { return not_found() };

    let mut response = Body::from_stream(ReaderStream::new(file)).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(metadata.len()));
    let content_type = if is_signature { "text/plain; charset=utf-8" } else { "application/octet-stream" };
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    response
}
