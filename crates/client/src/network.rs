//! Network state shared by the connection loop and the job worker.
//!
//! A `wifi.join` job runs in the worker but has to steer the connection: it
//! may switch the coordinator, close the current session, and later send the
//! loop back to searching for the hotspot. This is where the two meet.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use tokio::sync::{watch, Notify};

const FILE_NAME: &str = "network.json";

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    /// Coordinator given by a `wifi.join` job; replaces the configured one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    coordinator_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Status {
    /// Number of sessions established so far.
    generation: u64,
    connected: bool,
}

pub struct Network {
    configured_url: Option<String>,
    path: Option<PathBuf>,
    override_url: Mutex<Option<String>>,
    /// Base URL of the coordinator of the latest session, which may be the
    /// hotspot's gateway rather than any configured URL.
    session_url: Mutex<Option<String>>,
    status: watch::Sender<Status>,
    /// Wakes the connection loop for the two requests below.
    wake: Notify,
    reconnect: AtomicBool,
    /// Failed handoff's SSID. Recovery may leave that network, but must not
    /// undo a different network selected by the user in the meantime.
    search: Mutex<Option<String>>,
    handoffs: AtomicUsize,
    /// Scanning or joining while another join is under way would undo it.
    wifi: tokio::sync::Mutex<()>,
}

impl Network {
    /// Network state with the stored coordinator override from `state_dir`,
    /// if any. Without a state directory the override lives in memory only.
    pub fn load(configured_url: Option<String>, state_dir: Option<&Path>) -> io::Result<Network> {
        let (path, file) = match state_dir {
            Some(dir) => {
                std::fs::create_dir_all(dir)?;
                let path = dir.join(FILE_NAME);
                let file = read(&path)?;
                (Some(path), file)
            }
            None => (None, File::default()),
        };
        Ok(Network {
            configured_url,
            path,
            override_url: Mutex::new(file.coordinator_url),
            session_url: Mutex::new(None),
            status: watch::Sender::new(Status { generation: 0, connected: false }),
            wake: Notify::new(),
            reconnect: AtomicBool::new(false),
            search: Mutex::new(None),
            handoffs: AtomicUsize::new(0),
            wifi: tokio::sync::Mutex::new(()),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Option<String>> {
        self.override_url.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The coordinator to connect to: the stored override, else the
    /// configured one.
    pub fn coordinator_url(&self) -> Option<String> {
        self.lock().clone().or_else(|| self.configured_url.clone())
    }

    pub fn override_url(&self) -> Option<String> {
        self.lock().clone()
    }

    /// Replaces the override and stores it. A failure to store is logged:
    /// the override still holds until the client restarts.
    pub fn set_override(&self, url: Option<String>) {
        let mut current = self.lock();
        *current = url;
        let Some(path) = &self.path else { return };
        let write = || -> io::Result<()> {
            let bytes = serde_json::to_vec(&File { coordinator_url: current.clone() })?;
            let temporary = path.with_extension("json.tmp");
            std::fs::write(&temporary, bytes)?;
            std::fs::rename(&temporary, path)
        };
        if let Err(error) = write() {
            tracing::error!(%error, path = %path.display(), "failed to save the network state");
        }
    }

    /// A session has been established (`welcome` received) with the
    /// coordinator at `url`.
    pub fn mark_connected(&self, url: &str) {
        *self.session_url.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(url.to_owned());
        // A coordinator was reached; an earlier request to search is moot.
        self.search.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take();
        self.status.send_modify(|status| {
            status.generation += 1;
            status.connected = true;
        });
    }

    pub fn mark_disconnected(&self) {
        self.status.send_modify(|status| status.connected = false);
    }

    /// Base URL of the coordinator of the latest session, kept after the
    /// session ends: `client.update` downloads from there.
    pub fn session_url(&self) -> Option<String> {
        self.session_url.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    /// Increases with every session established.
    pub fn generation(&self) -> u64 {
        self.status.borrow().generation
    }

    /// Whether a session is up. Only tests ask so far.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_connected(&self) -> bool {
        self.status.borrow().connected
    }

    /// Resolves once a session newer than `generation` has been
    /// established, even if it has ended again since.
    pub async fn wait_for_connection_after(&self, generation: u64) {
        let mut status = self.status.subscribe();
        // The sender lives as long as `self`, so this cannot fail.
        let _ = status.wait_for(|status| status.generation > generation).await;
    }

    /// Ends the current session at once and makes the loop reconnect
    /// without waiting.
    pub fn reconnect_now(&self) {
        self.reconnect.store(true, Ordering::SeqCst);
        self.wake.notify_waiters();
    }

    /// Recovers a failed handoff on its next round. Only the named network
    /// may be left; another active network belongs to the user.
    pub fn search_now(&self, failed_ssid: &str) {
        *self.search.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(failed_ssid.to_owned());
        self.wake.notify_waiters();
    }

    /// Called by the loop at the start of each round: a reconnect asked for
    /// earlier is what this round does. Returns the failed handoff's SSID
    /// when recovery was requested.
    pub(crate) fn start_round(&self) -> Option<String> {
        self.reconnect.store(false, Ordering::SeqCst);
        self.search.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take()
    }

    async fn until(&self, condition: impl Fn() -> bool) {
        loop {
            let notified = self.wake.notified();
            tokio::pin!(notified);
            // Registered before the check, so a request made in between
            // still wakes this waiter.
            notified.as_mut().enable();
            if condition() {
                return;
            }
            notified.await;
        }
    }

    /// Resolves when [`Network::reconnect_now`] has been called this round.
    pub(crate) async fn reconnect_requested(&self) {
        self.until(|| self.reconnect.load(Ordering::SeqCst)).await;
    }

    /// Resolves when the loop should stop waiting between attempts.
    pub(crate) async fn interrupted(&self) {
        self.until(|| {
            self.reconnect.load(Ordering::SeqCst)
                || self.search.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).is_some()
        })
            .await;
    }

    /// While the returned guard lives the loop does not search for the
    /// hotspot: a handoff in progress decides itself when to give up on the
    /// new network, and the hotspot is visible from it.
    pub(crate) fn begin_handoff(&self) -> Handoff<'_> {
        self.handoffs.fetch_add(1, Ordering::SeqCst);
        Handoff(self)
    }

    pub(crate) fn handoff_in_progress(&self) -> bool {
        self.handoffs.load(Ordering::SeqCst) > 0
    }

    /// Held while scanning or joining.
    pub(crate) async fn wifi_lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.wifi.lock().await
    }
}

pub(crate) struct Handoff<'a>(&'a Network);

impl Drop for Handoff<'_> {
    fn drop(&mut self) {
        self.0.handoffs.fetch_sub(1, Ordering::SeqCst);
    }
}

fn read(path: &Path) -> io::Result<File> {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<File>(&bytes) {
            Ok(file) => Ok(file),
            Err(error) => {
                // Falling back to the configured coordinator is better than
                // not running; keep the file for inspection.
                tracing::error!(%error, path = %path.display(), "network state is corrupt; ignoring it");
                let _ = std::fs::rename(path, path.with_extension("json.corrupt"));
                Ok(File::default())
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(File::default()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;

    const CONFIGURED: &str = "wss://configured.example";
    const STORED: &str = "wss://stored.example";
    const SOON: Duration = Duration::from_secs(5);

    #[test]
    fn the_override_takes_precedence_and_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let network = Network::load(Some(CONFIGURED.into()), Some(dir.path())).unwrap();
        assert_eq!(network.coordinator_url().as_deref(), Some(CONFIGURED));
        assert_eq!(network.override_url(), None);

        network.set_override(Some(STORED.into()));
        assert_eq!(network.coordinator_url().as_deref(), Some(STORED));
        let reloaded = Network::load(Some(CONFIGURED.into()), Some(dir.path())).unwrap();
        assert_eq!(reloaded.override_url().as_deref(), Some(STORED));
        // Also when nothing is configured.
        let reloaded = Network::load(None, Some(dir.path())).unwrap();
        assert_eq!(reloaded.coordinator_url().as_deref(), Some(STORED));

        network.set_override(None);
        let reloaded = Network::load(Some(CONFIGURED.into()), Some(dir.path())).unwrap();
        assert_eq!(reloaded.coordinator_url().as_deref(), Some(CONFIGURED));
        assert!(!dir.path().join("network.json.tmp").exists());
    }

    #[test]
    fn without_a_state_directory_the_override_is_kept_in_memory() {
        let network = Network::load(None, None).unwrap();
        assert_eq!(network.coordinator_url(), None);
        network.set_override(Some(STORED.into()));
        assert_eq!(network.coordinator_url().as_deref(), Some(STORED));
    }

    #[test]
    fn a_corrupt_file_is_set_aside() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), b"{ not json").unwrap();
        let network = Network::load(Some(CONFIGURED.into()), Some(dir.path())).unwrap();
        assert_eq!(network.coordinator_url().as_deref(), Some(CONFIGURED));
        assert!(dir.path().join("network.json.corrupt").exists());
    }

    #[tokio::test]
    async fn waiting_for_a_connection_needs_a_newer_session() {
        let network = Arc::new(Network::load(None, None).unwrap());
        assert_eq!(network.session_url(), None);
        network.mark_connected(CONFIGURED);
        let before = network.generation();
        assert!(network.is_connected());
        assert_eq!(network.session_url().as_deref(), Some(CONFIGURED));
        // The session that is already up does not count.
        let wait = tokio::time::timeout(Duration::from_millis(100), network.wait_for_connection_after(before));
        assert!(wait.await.is_err());

        let waiter = tokio::spawn({
            let network = network.clone();
            async move { network.wait_for_connection_after(before).await }
        });
        network.mark_disconnected();
        assert!(!network.is_connected());
        network.mark_connected(STORED);
        // Even a session that ended before the waiter looked counts.
        network.mark_disconnected();
        tokio::time::timeout(SOON, waiter).await.unwrap().unwrap();
        // The latest session's coordinator, even after it ended.
        assert_eq!(network.session_url().as_deref(), Some(STORED));
    }

    #[tokio::test]
    async fn a_reconnect_request_wakes_the_session_and_lasts_for_the_round() {
        let network = Arc::new(Network::load(None, None).unwrap());
        let session = tokio::spawn({
            let network = network.clone();
            async move { network.reconnect_requested().await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        network.reconnect_now();
        tokio::time::timeout(SOON, session).await.unwrap().unwrap();
        // The wait at the end of the round is skipped too...
        tokio::time::timeout(SOON, network.interrupted()).await.unwrap();
        // ...and the next round starts afresh.
        assert_eq!(network.start_round(), None);
        let wait = tokio::time::timeout(Duration::from_millis(100), network.interrupted());
        assert!(wait.await.is_err());
    }

    #[tokio::test]
    async fn a_search_request_wakes_the_loop_until_taken() {
        let network = Arc::new(Network::load(None, None).unwrap());
        let waiting = tokio::spawn({
            let network = network.clone();
            async move { network.interrupted().await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        network.search_now("Office");
        tokio::time::timeout(SOON, waiting).await.unwrap().unwrap();
        // It does not end a session.
        let session = tokio::time::timeout(Duration::from_millis(100), network.reconnect_requested());
        assert!(session.await.is_err());
        assert_eq!(network.start_round().as_deref(), Some("Office"));
        assert_eq!(network.start_round(), None);

        // A coordinator reached in the meantime makes the request moot.
        network.search_now("Office");
        network.mark_connected(CONFIGURED);
        assert_eq!(network.start_round(), None);
    }

    #[test]
    fn a_handoff_is_in_progress_while_its_guard_lives() {
        let network = Network::load(None, None).unwrap();
        assert!(!network.handoff_in_progress());
        let first = network.begin_handoff();
        let second = network.begin_handoff();
        drop(first);
        assert!(network.handoff_in_progress());
        drop(second);
        assert!(!network.handoff_in_progress());
    }
}
