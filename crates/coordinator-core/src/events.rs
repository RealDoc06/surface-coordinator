//! Live events for the UI.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::sync::broadcast::error::RecvError;

use crate::api::ApiError;
use crate::auth;
use crate::state::AppState;
use crate::store::{Device, Job, JobTarget, Rejected};

/// Events a slow consumer may fall behind by before it is told to resync.
pub const EVENT_BUFFER: usize = 1024;
const AUTH_CHECK_INTERVAL: Duration = Duration::from_secs(30);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum Event {
    /// Sent per socket so an idle UI can detect a silent network failure.
    #[serde(rename = "heartbeat")]
    Heartbeat,
    #[serde(rename = "device.updated")]
    DeviceUpdated { device: Device },
    #[serde(rename = "device.removed")]
    DeviceRemoved { device_id: String },
    #[serde(rename = "job.created")]
    JobCreated { job: Job },
    #[serde(rename = "job.target.updated")]
    JobTargetUpdated { job_id: String, target: JobTarget },
    #[serde(rename = "job.output")]
    JobOutput { job_id: String, job_target_id: String, data: String, truncated: bool, offset: u64 },
    #[serde(rename = "connection.rejected")]
    ConnectionRejected { rejected: Rejected },
    /// A group was created, renamed, deleted or had its members changed, or
    /// a device that belonged to a group was forgotten.
    #[serde(rename = "groups.changed")]
    GroupsChanged,
    /// Events were dropped; the UI should refetch what it shows.
    #[serde(rename = "resync")]
    Resync,
}

/// `GET /api/v1/events`: upgrades to a WebSocket that receives every event.
pub async fn upgrade(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    // A page on another site could open this socket with the user's cookie.
    if !auth::is_same_origin(&headers) {
        return ApiError::new(
            axum::http::StatusCode::FORBIDDEN,
            "cross_origin",
            "requests from other origins are not allowed",
        )
        .into_response();
    }
    // Subscribe before the upgrade completes so nothing is missed in between.
    let events = state.events.subscribe();
    let auth_changes = state.auth_changes.subscribe();
    // The UI sends only control frames, so it never needs large incoming messages.
    ws.max_message_size(1024).max_frame_size(1024).on_upgrade(move |socket| async move {
        let tracker = state.tasks.clone();
        let _ = tracker.track_future(forward(state, socket, events, headers, auth_changes)).await;
    })
}

/// The next event or an idle heartbeat. A consumer that fell behind gets
/// `resync` in place of dropped events.
async fn next_event(
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    heartbeat: &mut tokio::time::Interval,
) -> Option<Event> {
    let received = tokio::select! {
        biased;
        received = events.recv() => received,
        _ = heartbeat.tick() => return Some(Event::Heartbeat),
    };
    match received {
        Ok(event) => Some(event),
        Err(RecvError::Lagged(_)) => Some(Event::Resync),
        Err(RecvError::Closed) => None,
    }
}

fn heartbeat_interval() -> tokio::time::Interval {
    let mut interval = tokio::time::interval_at(
        tokio::time::Instant::now() + HEARTBEAT_INTERVAL,
        HEARTBEAT_INTERVAL,
    );
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval
}

async fn forward(
    state: Arc<AppState>,
    socket: WebSocket,
    mut events: tokio::sync::broadcast::Receiver<Event>,
    headers: HeaderMap,
    mut auth_changes: tokio::sync::watch::Receiver<()>,
) {
    let (mut sink, mut stream) = socket.split();
    // The first tick also closes the race between HTTP authentication and upgrade.
    let mut auth_check = tokio::time::interval(AUTH_CHECK_INTERVAL);
    auth_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut heartbeat = heartbeat_interval();
    loop {
        let event = tokio::select! {
            biased;
            _ = state.shutdown.cancelled() => break,
            _ = auth_changes.changed() => {
                if !session_is_current(&state, &headers).await {
                    break;
                }
                continue;
            },
            _ = auth_check.tick() => {
                // Periodic checks cover expiry and revocation by the local admin CLI.
                if !session_is_current(&state, &headers).await {
                    break;
                }
                continue;
            },
            received = next_event(&mut events, &mut heartbeat) => match received {
                Some(event) => event,
                None => break,
            },
            incoming = stream.next() => match incoming {
                // The UI sends nothing; this only notices it going away.
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => continue,
            },
        };
        let json = serde_json::to_string(&event).expect("events always serialise");
        let sent = tokio::select! {
            biased;
            _ = state.shutdown.cancelled() => break,
            sent = tokio::time::timeout(SEND_TIMEOUT, sink.send(Message::Text(json.into()))) => sent,
        };
        if !matches!(sent, Ok(Ok(()))) {
            break;
        }
    }
    let _ = tokio::time::timeout(CLOSE_TIMEOUT, sink.close()).await;
}

async fn session_is_current(state: &AppState, headers: &HeaderMap) -> bool {
    tokio::select! {
        biased;
        _ = state.shutdown.cancelled() => false,
        identity = auth::identify(state, headers) => matches!(identity, Ok(Some(_))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn removed(id: &str) -> Event {
        Event::DeviceRemoved { device_id: id.to_owned() }
    }

    fn id_of(event: Option<Event>) -> String {
        match event {
            Some(Event::DeviceRemoved { device_id }) => device_id,
            Some(Event::Resync) => "resync".to_owned(),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_consumer_that_falls_behind_is_told_to_resync_and_then_continues() {
        let (sender, mut receiver) = tokio::sync::broadcast::channel(2);
        let mut heartbeat = heartbeat_interval();
        for id in ["a", "b", "c", "d"] {
            sender.send(removed(id)).unwrap();
        }
        // "a" and "b" were dropped: the consumer learns that first, then
        // gets what is still buffered and whatever follows.
        assert_eq!(id_of(next_event(&mut receiver, &mut heartbeat).await), "resync");
        assert_eq!(id_of(next_event(&mut receiver, &mut heartbeat).await), "c");
        assert_eq!(id_of(next_event(&mut receiver, &mut heartbeat).await), "d");
        sender.send(removed("e")).unwrap();
        assert_eq!(id_of(next_event(&mut receiver, &mut heartbeat).await), "e");
        drop(sender);
        assert!(next_event(&mut receiver, &mut heartbeat).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn an_idle_socket_gets_regular_heartbeats_without_broadcasting_them() {
        let (sender, mut receiver) = tokio::sync::broadcast::channel(2);
        let mut other_socket = sender.subscribe();
        let mut heartbeat = heartbeat_interval();
        let started = tokio::time::Instant::now();
        for beat in 1..=3 {
            let event = next_event(&mut receiver, &mut heartbeat).await.unwrap();
            assert_eq!(serde_json::to_value(event).unwrap(), serde_json::json!({"type": "heartbeat"}));
            assert_eq!(started.elapsed(), HEARTBEAT_INTERVAL * beat);
            assert!(matches!(other_socket.try_recv(), Err(tokio::sync::broadcast::error::TryRecvError::Empty)));
        }
        sender.send(removed("a")).unwrap();
        assert_eq!(id_of(next_event(&mut receiver, &mut heartbeat).await), "a");
        assert_eq!(started.elapsed(), HEARTBEAT_INTERVAL * 3);
        drop(sender);
        assert!(next_event(&mut receiver, &mut heartbeat).await.is_none());
    }

    #[tokio::test]
    async fn revalidation_rejects_an_expired_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::Config::new(
            "127.0.0.1:0".parse().unwrap(), dir.path(), crate::AuthMode::Token("fixture-token".into()),
        );
        config.updates.enabled = false;
        let store = crate::store::Store::open(dir.path()).unwrap();
        let updates = crate::updates::UpdateState::new(&config, None, false).unwrap();
        let state = AppState::new(config, store, updates);
        let (token, hash) = auth::new_session_token();
        let mut headers = HeaderMap::new();
        headers.insert("cookie", format!("{}={token}", auth::SESSION_COOKIE).parse().unwrap());
        state.store.call(move |conn| {
            crate::store::create_session(conn, &hash, None, crate::store::now_ms() + 60_000)
        }).await.unwrap();
        assert!(session_is_current(&state, &headers).await);
        state.store.call(|conn| conn.execute("UPDATE sessions SET expires = 0", [])).await.unwrap();
        assert!(!session_is_current(&state, &headers).await);
    }

    #[test]
    fn events_carry_their_type_tag() {
        let json = serde_json::to_value(Event::Resync).unwrap();
        assert_eq!(json, serde_json::json!({"type": "resync"}));
        let json = serde_json::to_value(Event::GroupsChanged).unwrap();
        assert_eq!(json, serde_json::json!({"type": "groups.changed"}));
        let json = serde_json::to_value(removed("x")).unwrap();
        assert_eq!(json, serde_json::json!({"type": "device.removed", "device_id": "x"}));
    }
}
