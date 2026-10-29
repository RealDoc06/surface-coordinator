//! Copying enrollment keys from the online coordinator into the local one.
//!
//! Devices are installed with keys made on the online coordinator. The local
//! coordinator only accepts them, and its hotspot only has their network,
//! if it holds the same keys (same id and secret). The secrets go from one
//! API to the other in memory: they are never logged or written anywhere
//! but the local coordinator's database.

use std::collections::HashMap;
use std::time::Duration;

use reqwest::header::COOKIE;
use reqwest::{RequestBuilder, Response, StatusCode};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::json;

use crate::remote;

/// The coordinator's session cookie, as the webview holds it after the
/// operator signed in.
pub const SESSION_COOKIE: &str = "sc_session";

/// Without internet (hotspot mode) the online coordinator is unreachable:
/// give up quickly rather than keep the operator waiting.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
const TIMEOUT: Duration = Duration::from_secs(15);

/// What a copy did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub imported: usize,
    pub already_present: usize,
    pub skipped_revoked: usize,
    pub revoked: usize,
}

/// Why keys were not copied. The messages are shown in the launcher as they
/// are, so they never contain a secret or a response body.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SyncError {
    #[error("Not signed in to the online coordinator; keys were not synced")]
    NotLoggedIn,
    #[error("The online coordinator could not be reached ({0}); keys were not synced")]
    Unreachable(String),
    #[error("The online coordinator gave an unexpected answer ({0}); keys were not synced")]
    Remote(String),
    #[error("The local coordinator did not take the keys ({0})")]
    Local(String),
}

/// `Key` of `docs/api.md`, as far as needed here.
#[derive(Deserialize)]
struct Key {
    id: String,
    #[serde(default)]
    label: String,
    revoked_at: Option<i64>,
}

#[derive(Deserialize)]
struct KeyList {
    keys: Vec<Key>,
}

#[derive(Deserialize)]
struct Secret {
    secret: String,
}

/// Copies the active keys of the online coordinator at `remote_origin`,
/// signed in with the session cookie value `remote_cookie`, into the local
/// coordinator at `local_origin` through its own API. Keys the local one
/// already has do not need their secrets fetched again. Remote revocations
/// are applied locally too, so an old cached key cannot keep enrolling devices.
pub async fn pull_keys(
    remote_origin: &str,
    remote_cookie: &str,
    local_origin: &str,
    local_token: &str,
) -> Result<SyncReport, SyncError> {
    let http = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| SyncError::Local(error.to_string()))?;
    let session = format!("{SESSION_COOKIE}={remote_cookie}");
    let remote = |path: &str| http.get(format!("{remote_origin}/api/v1{path}")).header(COOKIE, &session);
    let local_keys = format!("{local_origin}/api/v1/enrollment-keys");

    let online: KeyList = remote_json(remote("/enrollment-keys")).await?;
    let present: HashMap<String, Option<i64>> = local_json::<KeyList>(http.get(&local_keys).bearer_auth(local_token))
        .await?
        .keys
        .into_iter()
        .map(|key| (key.id, key.revoked_at))
        .collect();

    let mut report = SyncReport::default();
    for key in online.keys {
        if key.revoked_at.is_some() {
            if present.get(&key.id) == Some(&None) {
                let response = send_local(
                    http.post(format!("{local_keys}/{}/revoke", key.id)).bearer_auth(local_token),
                )
                .await?;
                if response.status() != StatusCode::NO_CONTENT {
                    return Err(SyncError::Local(response.status().to_string()));
                }
                report.revoked += 1;
            }
            report.skipped_revoked += 1;
            continue;
        }
        if present.contains_key(&key.id) {
            report.already_present += 1;
            continue;
        }
        let Secret { secret } = remote_json(remote(&format!("/enrollment-keys/{}/secret", key.id))).await?;
        let response = send_local(
            http.post(&local_keys)
                .bearer_auth(local_token)
                .json(&json!({"label": key.label, "id": key.id, "secret": secret})),
        )
        .await?;
        match response.status() {
            StatusCode::CREATED => report.imported += 1,
            // Added meanwhile, e.g. by the operator.
            StatusCode::CONFLICT => report.already_present += 1,
            status => return Err(SyncError::Local(status.to_string())),
        }
    }
    Ok(report)
}

/// A JSON answer of the online coordinator. 401 means the session is
/// missing, expired or ended.
async fn remote_json<T: DeserializeOwned>(request: RequestBuilder) -> Result<T, SyncError> {
    let response = request.send().await.map_err(|error| SyncError::Unreachable(remote::describe(&error)))?;
    match response.status() {
        status if status.is_success() => {
            // Not the decoder's message: it can quote the body.
            response.json().await.map_err(|_| SyncError::Remote("not the expected JSON".into()))
        }
        StatusCode::UNAUTHORIZED => Err(SyncError::NotLoggedIn),
        status => Err(SyncError::Remote(status.to_string())),
    }
}

async fn send_local(request: RequestBuilder) -> Result<Response, SyncError> {
    request.send().await.map_err(|error| SyncError::Local(remote::describe(&error)))
}

async fn local_json<T: DeserializeOwned>(request: RequestBuilder) -> Result<T, SyncError> {
    let response = send_local(request).await?;
    if !response.status().is_success() {
        return Err(SyncError::Local(response.status().to_string()));
    }
    response.json().await.map_err(|_| SyncError::Local("not the expected JSON".into()))
}
