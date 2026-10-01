//! Shared server state: configuration, storage and the live connections.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use protocol::{job_kind, ServerMessage};
use tokio::sync::{broadcast, mpsc};
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
    pub outbound: mpsc::UnboundedSender<ServerMessage>,
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
        outbound: mpsc::UnboundedSender<ServerMessage>,
        restricted: bool,
    ) -> Connection {
        let connection = Connection {
            id: self.next_connection_id.fetch_add(1, Ordering::Relaxed),
            outbound,
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
    pub fn signal_cancel(&self, device_id: &str, job_target_id: &str) -> bool {
        self.connection(device_id).is_some_and(|connection| {
            connection
                .outbound
                .send(ServerMessage::JobCancel { job_target_id: job_target_id.to_owned() })
                .is_ok()
        })
    }

    /// Sends pending job targets to a device if it is connected. With
    /// `include_unresolved`, targets it may already have are sent again. A
    /// restricted connection is only sent `client.update`; everything else
    /// stays queued until the device returns on a supported version.
    pub async fn dispatch(&self, device_id: &str, include_unresolved: bool) {
        let Some(connection) = self.connection(device_id) else {
            return;
        };
        let device = device_id.to_owned();
        let only_kind = connection.restricted.then_some(job_kind::CLIENT_UPDATE);
        let taken = self
            .store
            .call(move |conn| store::take_dispatchable(conn, &device, include_unresolved, only_kind))
            .await;
        match taken {
            Ok((dispatches, changes)) => {
                for dispatch in dispatches {
                    tracing::debug!(device_id, job_target_id = %dispatch.job_target_id, "dispatching job");
                    // If the connection is gone the target stays `sent` and
                    // is delivered again when the device reconnects.
                    let _ = connection.outbound.send(ServerMessage::Job {
                        job_target_id: dispatch.job_target_id,
                        job_id: dispatch.job_id,
                        kind: dispatch.kind,
                        payload: dispatch.payload,
                    });
                }
                for change in changes {
                    self.publish(Event::JobTargetUpdated { job_id: change.job_id, target: change.target });
                }
            }
            Err(error) => tracing::error!(device_id, %error, "failed to load jobs to dispatch"),
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
