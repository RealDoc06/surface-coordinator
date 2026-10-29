//! `client.update`: download a new client executable from the coordinator,
//! verify it, put it in place and restart; then, on the following starts,
//! complete the update or roll it back.
//!
//! Installing (the job, in the worker):
//!
//! 1. Download `/updates/client/<version>/<target>` from the coordinator of
//!    the current session to `client.update.tmp` beside the executable;
//!    check size and SHA-256, then the signature (`.sig`) against the key
//!    built into this client.
//! 2. Check that the signed executable can start as the expected version.
//!    Write the pending-update marker, [`MARKER_FILE`], then replace the
//!    executable (the running one becomes `client.old.exe`).
//! 3. Leave the job unfinished (the ledger marks it as awaiting the restart),
//!    note "restarting" in its output, and restart.
//!
//! Starting (before connecting, see [`on_start`]), when there is a marker:
//!
//! - On the new version: count the start. Up to [`MAX_STARTS`] starts, the
//!   update is completed by the first handshake: the old executable and the
//!   marker are deleted and the job succeeds. The next start puts the old
//!   executable back and restarts. A process that stays alive without a
//!   handshake also restores the old version after its confirmation deadline.
//! - On the previous version after such a rollback: the job fails with
//!   `rolled_back`.
//!
//! The executable itself is only touched through the platform.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use protocol::update::{self, Version, VerifyError};
use protocol::{reason, ClientUpdatePayload, JobResult, OutputStream};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::jobs::{self, JobManager, Outcome, WorkerContext};
use crate::platform::{OutputChunk, Platform};

/// The pending-update marker in the state directory.
pub const MARKER_FILE: &str = "update-pending.json";
/// Where the download goes, beside the executable: the same volume, so that
/// moving it into place is a rename.
pub const DOWNLOAD_FILE: &str = "client.update.tmp";
/// Starts of the new version without a completed handshake before the
/// previous version is restored.
pub const MAX_STARTS: u32 = 3;
/// A signature is 88 base64 characters; anything much longer is not one.
const MAX_SIGNATURE_BYTES: u64 = 4096;

/// What the next starts need to know about an update in progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    pub job_target_id: String,
    pub previous_version: String,
    pub new_version: String,
    /// Starts of the new version so far.
    pub starts: u32,
    /// The previous version has been put back.
    #[serde(default, skip_serializing_if = "is_false")]
    pub rolled_back: bool,
    /// Why this provisional version was rolled back, when the health
    /// deadline rather than repeated starts triggered recovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Reads the marker. A corrupt one is set aside: it cannot be acted on.
pub fn read_marker(state_dir: &Path) -> io::Result<Option<Marker>> {
    let path = state_dir.join(MARKER_FILE);
    match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(marker) => Ok(Some(marker)),
            Err(error) => {
                tracing::error!(%error, path = %path.display(), "update marker is corrupt; ignoring it");
                let _ = std::fs::rename(&path, path.with_extension("json.corrupt"));
                Ok(None)
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Flushes the recovery information before atomically replacing the marker.
pub fn write_marker(state_dir: &Path, marker: &Marker) -> io::Result<()> {
    let path = state_dir.join(MARKER_FILE);
    let temporary = path.with_extension("json.tmp");
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all(&serde_json::to_vec_pretty(marker)?)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary, &path)
}

fn delete_marker(state_dir: &Path) {
    let path = state_dir.join(MARKER_FILE);
    match std::fs::remove_file(&path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            tracing::error!(%error, path = %path.display(), "cannot delete the update marker")
        }
        _ => {}
    }
}

/// The HTTP base URL of the coordinator at the WebSocket base URL `url`.
pub(crate) fn http_base(url: &str) -> Option<String> {
    let url = url.trim_end_matches('/');
    let (scheme, rest) = url.split_once("://")?;
    let scheme = match scheme.to_ascii_lowercase().as_str() {
        "ws" => "http",
        "wss" => "https",
        _ => return None,
    };
    Some(format!("{scheme}://{rest}"))
}

// ------------------------------------------------------------ installing

/// Runs a `client.update` job. Returns [`Outcome::Restart`] once the new
/// executable is in place.
pub(crate) async fn install<P: Platform>(
    jobs: &JobManager,
    platform: &P,
    context: &WorkerContext,
    id: &str,
    payload: ClientUpdatePayload,
) -> Outcome {
    match try_install(jobs, platform, context, id, payload).await {
        Ok(outcome) => outcome,
        Err(result) => result.into(),
    }
}

async fn try_install<P: Platform>(
    jobs: &JobManager,
    platform: &P,
    context: &WorkerContext,
    id: &str,
    payload: ClientUpdatePayload,
) -> Result<Outcome, JobResult> {
    let fail = |reason: &str, message: String| jobs::failed(id, reason, Some(message));
    let settings = &context.update;
    let _lifecycle = settings.lifecycle.lock().await;

    let version: Version =
        payload.version.parse().map_err(|error: update::ParseVersionError| fail(reason::INVALID_PAYLOAD, error.to_string()))?;
    let expected = update::parse_sha256_hex(&payload.sha256).ok_or_else(|| {
        fail(reason::INVALID_PAYLOAD, format!("{:?} is not a hex SHA-256 digest", payload.sha256))
    })?;
    let target = payload.target.as_str();
    if update::current_target() != Some(target) {
        let this = update::current_target().unwrap_or("a platform without client updates");
        return Err(fail(reason::UNSUPPORTED, format!("the update is for {target}; this client is for {this}")));
    }
    if settings.public_key.is_empty() {
        return Err(fail(reason::UNSUPPORTED, "this client build has no update key".into()));
    }

    let running: Version = settings.client_version.parse().map_err(|_| {
        fail(
            reason::ERROR,
            format!("the running version {:?} cannot be compared with {version}", settings.client_version),
        )
    })?;
    if version == running {
        return Ok(jobs::succeeded(id, format!("already on version {version}")).into());
    }
    if version < running && !payload.downgrade {
        return Err(fail(
            reason::DOWNGRADE_REFUSED,
            format!("version {version} is older than the running version {running} and the job does not allow a downgrade"),
        ));
    }
    let state_dir = settings
        .state_dir
        .as_deref()
        .ok_or_else(|| fail(reason::UNSUPPORTED, "updating needs a state directory".into()))?;
    let exe = platform.current_exe().map_err(|error| jobs::platform_failure(id, error))?;
    // Whichever coordinator this client is talking to, the gateway of the
    // hotspot included.
    let base = context.network.session_url().as_deref().and_then(http_base).ok_or_else(|| {
        fail(reason::DOWNLOAD_FAILED, "there is no coordinator to download the update from".into())
    })?;

    let http = reqwest::Client::builder()
        .connect_timeout(settings.download_timeout)
        .read_timeout(settings.download_timeout)
        .build()
        .map_err(|error| fail(reason::ERROR, format!("cannot set up the download: {}", describe(&error))))?;
    let url = format!("{base}/updates/client/{version}/{target}");
    let mut download = Download { path: exe.with_file_name(DOWNLOAD_FILE), keep: false };
    tracing::info!(job_target_id = %id, %url, "downloading client update");
    let sha256 = download.fetch(&http, &url, payload.size).await.map_err(|(reason, message)| fail(reason, message))?;
    if sha256 != expected {
        return Err(fail(
            reason::BAD_HASH,
            format!("the download's SHA-256 is {} instead of {}", hex(&sha256), payload.sha256.to_ascii_lowercase()),
        ));
    }
    let signature = fetch_signature(&http, &format!("{url}.sig")).await.map_err(|(reason, message)| fail(reason, message))?;
    match update::verify(&settings.public_key, &version.to_string(), target, &sha256, signature.trim()) {
        Ok(()) => {}
        Err(VerifyError::BadPublicKey) => {
            return Err(fail(reason::ERROR, "the update key built into this client is not valid".into()))
        }
        Err(error) => return Err(fail(reason::BAD_SIGNATURE, error.to_string())),
    }

    platform.validate_update(download.path.clone(), version.to_string()).await
        .map_err(|error| jobs::platform_failure(id, error))?;

    // Replacing the executable is the point of no return.
    if !jobs.commit(id) {
        return Err(jobs::cancelled(id));
    }
    // Before the swap: should the client stop right after it, the next
    // start must not take the job for interrupted (see `on_start`).
    jobs.await_restart(id).map_err(|error| fail(reason::ERROR, format!(
        "cannot persist the update restart state: {error}; version {running} is still in place"
    )))?;
    let marker = Marker {
        job_target_id: id.to_owned(),
        previous_version: settings.client_version.clone(),
        new_version: version.to_string(),
        starts: 0,
        rolled_back: false,
        failure: None,
    };
    // Persist recovery information before changing the executable. A crash
    // after a successful swap must not strand the new version without it.
    if let Err(error) = write_marker(state_dir, &marker) {
        return Err(fail(reason::ERROR, format!(
            "cannot write the update marker: {error}; version {running} is still in place"
        )));
    }
    download.keep = true;
    if let Err(error) = platform.swap_executable(download.path.clone()).await {
        download.keep = false;
        delete_marker(state_dir);
        return Err(jobs::platform_failure(id, error));
    }

    tracing::info!(job_target_id = %id, %version, "client update installed; restarting");
    // Not replayed after the restart; the connection sends it before it closes.
    let note = format!("restarting to complete the update to {version}\n");
    jobs.output(id, OutputChunk { stream: OutputStream::Stdout, data: note });
    Ok(Outcome::Restart)
}

/// The downloaded file, deleted unless kept: a failed or cancelled job
/// leaves nothing behind.
struct Download {
    path: PathBuf,
    keep: bool,
}

impl Drop for Download {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

type Failure = (&'static str, String);

impl Download {
    /// Streams `url` into the file, never more than `size` bytes, and
    /// returns the SHA-256 of what was received.
    async fn fetch(&mut self, http: &reqwest::Client, url: &str, size: u64) -> Result<[u8; 32], Failure> {
        let failed = |message: String| (reason::DOWNLOAD_FAILED, message);
        let mut response =
            http.get(url).send().await.map_err(|error| failed(format!("cannot download {url}: {}", describe(&error))))?;
        if !response.status().is_success() {
            return Err(failed(format!("downloading {url} failed: HTTP {}", response.status())));
        }
        // A plain file, written in place: a tokio file could still be open
        // in a background thread when a cancelled job is dropped, and the
        // file must go at once.
        let mut file = std::fs::File::create(&self.path)
            .map_err(|error| (reason::ERROR, format!("cannot create {}: {error}", self.path.display())))?;
        let mut hasher = Sha256::new();
        let mut received = 0u64;
        while let Some(chunk) =
            response.chunk().await.map_err(|error| failed(format!("downloading {url} failed: {}", describe(&error))))?
        {
            received += chunk.len() as u64;
            if received > size {
                return Err((reason::BAD_HASH, format!("the download is larger than the announced {size} bytes")));
            }
            hasher.update(&chunk);
            file.write_all(&chunk)
                .map_err(|error| (reason::ERROR, format!("cannot write {}: {error}", self.path.display())))?;
        }
        if received < size {
            return Err(failed(format!("the download ended after {received} of {size} bytes")));
        }
        file.sync_all()
            .map_err(|error| (reason::ERROR, format!("cannot write {}: {error}", self.path.display())))?;
        Ok(hasher.finalize().into())
    }
}

async fn fetch_signature(http: &reqwest::Client, url: &str) -> Result<String, Failure> {
    let failed = |message: String| (reason::DOWNLOAD_FAILED, message);
    let mut response = http
        .get(url)
        .send()
        .await
        .map_err(|error| failed(format!("cannot download the signature {url}: {}", describe(&error))))?;
    if !response.status().is_success() {
        return Err(failed(format!("downloading the signature {url} failed: HTTP {}", response.status())));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| failed(format!("downloading the signature {url} failed: {}", describe(&error))))?
    {
        bytes.extend_from_slice(&chunk);
        if bytes.len() as u64 > MAX_SIGNATURE_BYTES {
            return Err((reason::BAD_SIGNATURE, "the signature file is too large to be a signature".into()));
        }
    }
    String::from_utf8(bytes).map_err(|_| (reason::BAD_SIGNATURE, "the signature file is not text".into()))
}

/// A reqwest error with its causes, which say what actually went wrong.
fn describe(error: &reqwest::Error) -> String {
    let mut text = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

// ------------------------------------------------------------ starting

/// What the client does after [`on_start`].
pub(crate) enum Startup {
    /// Connect as usual; with a completion, finish the update after the
    /// first handshake.
    Continue(Option<Completion>),
    /// The previous version has been put back and a restart requested:
    /// do not connect.
    Restart,
}

/// An update that completes with the first handshake or rolls back at its
/// confirmation deadline.
pub(crate) struct Completion {
    state_dir: PathBuf,
    job_target_id: String,
    new_version: String,
}

impl Completion {
    /// Call once a handshake has completed.
    pub(crate) async fn complete<P: Platform>(self, platform: &P, jobs: &JobManager) {
        tracing::info!(version = %self.new_version, "client update completed");
        // Keep the marker, backup, and lifecycle lock until success is durable.
        // A restart during a disk fault can then complete the same update again
        // instead of reporting an interrupted job or accepting another update.
        persist_result(jobs, jobs::succeeded(&self.job_target_id, format!("updated to {}", self.new_version))).await;
        if let Err(error) = platform.remove_old_executable().await {
            tracing::warn!(%error, "cannot remove the previous executable");
        }
        delete_marker(&self.state_dir);
    }

    /// A new process can stay alive forever without ever connecting. The
    /// crash-loop counter cannot catch that failure, so provisional updates
    /// also have a deadline. A coincident long outage may conservatively
    /// restore a valid update; ordinary confirmed clients never roll back.
    pub(crate) async fn timed_out<P: Platform>(self, platform: &P, jobs: &JobManager, timeout: Duration) {
        let mut marker = match read_marker(&self.state_dir) {
            Ok(Some(marker)) if marker.job_target_id == self.job_target_id && !marker.rolled_back => marker,
            _ => return,
        };
        marker.failure = Some(format!(
            "version {} did not confirm a coordinator connection within {} s",
            marker.new_version, timeout.as_secs(),
        ));
        tracing::error!(version = %marker.new_version, "update confirmation timed out; restoring previous version");
        let _ = restore_and_restart(platform, jobs, &self.state_dir, marker).await;
    }
}

async fn persist_result(jobs: &JobManager, result: JobResult) {
    jobs.finish(result);
    while let Err(error) = jobs.persist() {
        tracing::warn!(%error, "cannot persist update result; retaining recovery information");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Acts on the pending-update marker, if any; call on every start before
/// connecting. Also settles jobs left awaiting a restart without a marker.
pub(crate) async fn on_start<P: Platform>(
    platform: &P,
    jobs: &JobManager,
    client_version: &str,
    state_dir: Option<&Path>,
) -> Startup {
    let Some(state_dir) = state_dir else { return Startup::Continue(None) };
    let startup = match read_marker(state_dir) {
        Ok(Some(marker)) => resume(platform, jobs, client_version, state_dir, marker).await,
        Ok(None) => Startup::Continue(None),
        Err(error) => {
            tracing::error!(%error, "cannot read the update marker");
            Startup::Continue(None)
        }
    };
    // The client stopped after an update had marked its job but before it
    // wrote the marker: that update never took place.
    let pending = read_marker(state_dir).ok().flatten().map(|marker| marker.job_target_id);
    for id in jobs.awaiting_restart() {
        if Some(&id) != pending.as_ref() {
            jobs.finish(jobs::failed(
                &id,
                reason::INTERRUPTED,
                Some("the client restarted before the update was installed".into()),
            ));
        }
    }
    startup
}

async fn resume<P: Platform>(
    platform: &P,
    jobs: &JobManager,
    client_version: &str,
    state_dir: &Path,
    mut marker: Marker,
) -> Startup {
    let id = marker.job_target_id.clone();
    let (previous, new) = (marker.previous_version.clone(), marker.new_version.clone());

    if !marker.rolled_back && client_version == new {
        marker.starts += 1;
        if let Err(error) = write_marker(state_dir, &marker) {
            tracing::error!(%error, "cannot count this start in the update marker");
        }
        if marker.starts <= MAX_STARTS {
            tracing::info!(version = %new, start = marker.starts, "completing the client update once a coordinator is reached");
            let completion = Completion { state_dir: state_dir.to_owned(), job_target_id: id, new_version: new };
            return Startup::Continue(Some(completion));
        }
        tracing::error!("version {new} did not complete a handshake in {MAX_STARTS} starts; restoring version {previous}");
        return restore_and_restart(platform, jobs, state_dir, marker).await;
    }

    if marker.rolled_back && client_version == previous {
        tracing::warn!("the update to {new} was rolled back");
        persist_result(jobs, jobs::failed(
            &id,
            reason::ROLLED_BACK,
            Some(format!("{}; version {previous} restored", failure_detail(&marker))),
        )).await;
        if let Err(error) = platform.remove_old_executable().await {
            tracing::warn!(%error, "cannot remove the executable of the failed version");
        }
        delete_marker(state_dir);
        return Startup::Continue(None);
    }

    tracing::error!(
        running = client_version,
        previous = %previous,
        new = %new,
        rolled_back = marker.rolled_back,
        "the update marker matches neither the running version nor the expected one; dropping it"
    );
    persist_result(jobs, jobs::failed(
        &id,
        reason::ERROR,
        Some(format!("the update from {previous} to {new} did not take effect: version {client_version} is running")),
    )).await;
    delete_marker(state_dir);
    Startup::Continue(None)
}

fn failure_detail(marker: &Marker) -> String {
    marker.failure.clone().unwrap_or_else(|| format!(
        "version {} did not complete a handshake in {MAX_STARTS} starts", marker.new_version
    ))
}

async fn restore_and_restart<P: Platform>(
    platform: &P,
    jobs: &JobManager,
    state_dir: &Path,
    mut marker: Marker,
) -> Startup {
    if let Err(error) = platform.restore_executable().await {
        tracing::error!(%error, "cannot restore the previous client version");
        persist_result(jobs, jobs::failed(
            &marker.job_target_id,
            reason::ERROR,
            Some(format!("{}, and restoring version {} failed: {error}",
                failure_detail(&marker), marker.previous_version)),
        )).await;
        delete_marker(state_dir);
        return Startup::Continue(None);
    }
    marker.rolled_back = true;
    if let Err(error) = write_marker(state_dir, &marker) {
        tracing::error!(%error, "cannot record the rollback in the update marker");
    }
    match platform.request_restart() {
        Ok(()) => Startup::Restart,
        Err(error) => {
            tracing::error!(%error, "cannot restart; previous client runs from the next start");
            Startup::Continue(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_http_base_follows_the_websocket_scheme() {
        assert_eq!(http_base("ws://127.0.0.1:8080").as_deref(), Some("http://127.0.0.1:8080"));
        assert_eq!(http_base("wss://coordinator.example/").as_deref(), Some("https://coordinator.example"));
        assert_eq!(http_base("WSS://coordinator.example/sc").as_deref(), Some("https://coordinator.example/sc"));
        assert_eq!(http_base("ws://[fe80::1]:47800").as_deref(), Some("http://[fe80::1]:47800"));
        assert_eq!(http_base("http://already.example"), None);
        assert_eq!(http_base("no scheme"), None);
    }

    #[test]
    fn the_marker_round_trips_and_omits_rolled_back_until_set() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_marker(dir.path()).unwrap(), None);
        let mut marker = Marker {
            job_target_id: "t1".into(),
            previous_version: "1.0.0".into(),
            new_version: "1.1.0".into(),
            starts: 0,
            rolled_back: false,
            failure: None,
        };
        write_marker(dir.path(), &marker).unwrap();
        let text = std::fs::read_to_string(dir.path().join(MARKER_FILE)).unwrap();
        assert!(!text.contains("rolled_back"), "{text}");
        assert_eq!(read_marker(dir.path()).unwrap(), Some(marker.clone()));

        marker.rolled_back = true;
        write_marker(dir.path(), &marker).unwrap();
        assert_eq!(read_marker(dir.path()).unwrap(), Some(marker));
        assert!(!dir.path().join("update-pending.json.tmp").exists());
    }

    #[test]
    fn a_corrupt_marker_is_set_aside() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(MARKER_FILE), b"{ not json").unwrap();
        assert_eq!(read_marker(dir.path()).unwrap(), None);
        assert!(dir.path().join("update-pending.json.corrupt").exists());
    }
}
