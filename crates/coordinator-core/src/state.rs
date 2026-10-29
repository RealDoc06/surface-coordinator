//! Shared server state: configuration, storage and the live connections.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{broadcast, Notify};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::auth::{self, LoginThrottle};
use crate::events::{Event, EVENT_BUFFER};
use crate::store::{self, Store};
use crate::updates::UpdateState;
use crate::Config;

pub struct AppState {
    pub config: Config,
    pub store: Store,
    /// Cancelled when the server stops.
    pub shutdown: CancellationToken,
    /// Connection and background tasks, awaited on stop.
    pub tasks: TaskTracker,
    /// Live events for the UI.
    pub events: broadcast::Sender<Event>,
    pub login_throttle: LoginThrottle,
    /// Bounds memory-hard password work independently of the database lock.
    pub password_work: Arc<tokio::sync::Semaphore>,
    /// Wakes UI sockets when a session may have been revoked.
    pub auth_changes: tokio::sync::watch::Sender<()>,
    /// Verified against when a login names an unknown account, so that
    /// attempt takes as long as one for a real account.
    pub dummy_password_hash: String,
    connections: Mutex<HashMap<String, Connection>>,
    next_connection_id: AtomicU64,
    /// Serialises online/offline transitions so the database flag cannot
    /// end up contradicting the connection table.
    pub presence: tokio::sync::Mutex<()>,
    /// The client release cache and its checks.
    pub updates: UpdateState,
}

/// An authenticated client connection.
#[derive(Clone)]
pub struct Connection {
    pub id: u64,
    /// Coalesces queued work into one wakeup; payloads stay in SQLite until sent.
    pub pending: Arc<Notify>,
    /// Unix milliseconds of the last message received.
    pub last_seen: Arc<AtomicI64>,
    /// Cancelled to make the connection task close the socket.
    pub close: CancellationToken,
    /// The client spoke a protocol version this coordinator does not
    /// support: it is only sent `client.update` jobs.
    pub restricted: bool,
}

impl AppState {
    pub fn new(config: Config, store: Store, updates: UpdateState) -> AppState {
        AppState {
            config,
            store,
            shutdown: CancellationToken::new(),
            tasks: TaskTracker::new(),
            events: broadcast::channel(EVENT_BUFFER).0,
            login_throttle: LoginThrottle::default(),
            password_work: Arc::new(tokio::sync::Semaphore::new(2)),
            auth_changes: tokio::sync::watch::channel(()).0,
            dummy_password_hash: auth::hash_password("no account has this password"),
            connections: Mutex::new(HashMap::new()),
            next_connection_id: AtomicU64::new(1),
            presence: tokio::sync::Mutex::new(()),
            updates,
        }
    }

    fn connections(&self) -> std::sync::MutexGuard<'_, HashMap<String, Connection>> {
        self.connections.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Sends an event to every connected UI. Nobody listening is fine.
    pub fn publish(&self, event: Event) {
        let _ = self.events.send(event);
    }

    /// Publishes the current state of a device.
    pub async fn publish_device(&self, device_id: &str) {
        let id = device_id.to_owned();
        match self.store.call(move |conn| store::get_device(conn, &id)).await {
            Ok(Some(mut device)) => {
                self.apply_live_state(&mut device);
                self.publish(Event::DeviceUpdated { device });
            }
            Ok(None) => {}
            Err(error) => tracing::error!(device_id, %error, "failed to load device for an event"),
        }
    }

    /// Registers a connection for a device, closing any previous one.
    pub fn register(
        &self,
        device_id: &str,
        restricted: bool,
    ) -> Connection {
        let connection = Connection {
            id: self.next_connection_id.fetch_add(1, Ordering::Relaxed),
            pending: Arc::new(Notify::new()),
            last_seen: Arc::new(AtomicI64::new(store::now_ms())),
            close: self.shutdown.child_token(),
            restricted,
        };
        if let Some(previous) = self.connections().insert(device_id.to_owned(), connection.clone()) {
            previous.close.cancel();
        }
        connection
    }

    /// Removes a connection if it is still the device's current one.
    /// Returns whether it was.
    pub fn unregister(&self, device_id: &str, connection_id: u64) -> bool {
        let mut connections = self.connections();
        match connections.get(device_id) {
            Some(current) if current.id == connection_id => {
                connections.remove(device_id);
                true
            }
            _ => false,
        }
    }

    /// Closes and forgets a device's connection, if it has one.
    pub fn drop_connection(&self, device_id: &str) {
        if let Some(connection) = self.connections().remove(device_id) {
            connection.close.cancel();
        }
    }

    pub fn connection(&self, device_id: &str) -> Option<Connection> {
        self.connections().get(device_id).cloned()
    }

    /// Asks a device to stop a job target. Returns false if the device has no
    /// live connection, or the connection is already winding down and will
    /// not deliver the message.
    pub fn signal_cancel(&self, device_id: &str, _job_target_id: &str) -> bool {
        self.connection(device_id).is_some_and(|connection| {
            if connection.close.is_cancelled() {
                return false;
            }
            connection.pending.notify_one();
            true
        })
    }

    /// Wakes a connected device without copying its backlog into memory or
    /// making the API wait for a database round trip for every target.
    pub fn dispatch(&self, device_id: &str) {
        if let Some(connection) = self.connection(device_id) {
            connection.pending.notify_one();
        }
    }

    /// Fills in a device's fields that are not stored: presence from the
    /// live connection table, and whether an update is available from the
    /// release cache.
    pub fn apply_live_state(&self, device: &mut store::Device) {
        // The connection table is the authority on who is online; heartbeats
        // refresh last-seen in memory only.
        match self.connection(&device.id) {
            Some(connection) => {
                device.online = true;
                device.restricted = connection.restricted;
                device.last_seen_at = connection.last_seen.load(Ordering::Relaxed);
            }
            None => {
                device.online = false;
                device.restricted = false;
            }
        }
        device.update_available = self.updates.is_newer_than(&device.client_version);
    }
}
