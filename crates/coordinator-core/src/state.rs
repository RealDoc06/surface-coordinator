//! Shared server state: configuration, storage and the live connections.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use protocol::ServerMessage;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::store::{self, Store};
use crate::Config;

pub struct AppState {
    pub config: Config,
    pub store: Store,
    /// Cancelled when the server stops.
    pub shutdown: CancellationToken,
    /// Connection and background tasks, awaited on stop.
    pub tasks: TaskTracker,
    connections: Mutex<HashMap<String, Connection>>,
    next_connection_id: AtomicU64,
    /// Serialises online/offline transitions so the database flag cannot
    /// end up contradicting the connection table.
    pub presence: tokio::sync::Mutex<()>,
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
}

impl AppState {
    pub fn new(config: Config, store: Store) -> AppState {
        AppState {
            config,
            store,
            shutdown: CancellationToken::new(),
            tasks: TaskTracker::new(),
            connections: Mutex::new(HashMap::new()),
            next_connection_id: AtomicU64::new(1),
            presence: tokio::sync::Mutex::new(()),
        }
    }

    fn connections(&self) -> std::sync::MutexGuard<'_, HashMap<String, Connection>> {
        self.connections.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Registers a connection for a device, closing any previous one.
    pub fn register(
        &self,
        device_id: &str,
        outbound: mpsc::UnboundedSender<ServerMessage>,
    ) -> Connection {
        let connection = Connection {
            id: self.next_connection_id.fetch_add(1, Ordering::Relaxed),
            outbound,
            last_seen: Arc::new(AtomicI64::new(store::now_ms())),
            close: self.shutdown.child_token(),
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

    pub fn connection(&self, device_id: &str) -> Option<Connection> {
        self.connections().get(device_id).cloned()
    }

    /// Sends pending job targets to a device if it is connected. With
    /// `include_unresolved`, targets it may already have are sent again.
    pub async fn dispatch(&self, device_id: &str, include_unresolved: bool) {
        let Some(connection) = self.connection(device_id) else {
            return;
        };
        let device = device_id.to_owned();
        let dispatches = self
            .store
            .call(move |conn| store::take_dispatchable(conn, &device, include_unresolved))
            .await;
        match dispatches {
            Ok(dispatches) => {
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
            }
            Err(error) => tracing::error!(device_id, %error, "failed to load jobs to dispatch"),
        }
    }
}
