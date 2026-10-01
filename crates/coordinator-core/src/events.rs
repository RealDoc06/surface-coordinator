//! Live events for the UI.

use std::sync::Arc;

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

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum Event {
    #[serde(rename = "device.updated")]
    DeviceUpdated { device: Device },
    #[serde(rename = "device.removed")]
    DeviceRemoved { device_id: String },
    #[serde(rename = "job.created")]
    JobCreated { job: Job },
    #[serde(rename = "job.target.updated")]
    JobTargetUpdated { job_id: String, target: JobTarget },
    #[serde(rename = "job.output")]
    JobOutput { job_id: String, job_target_id: String, data: String, truncated: bool },
    #[serde(rename = "connection.rejected")]
    ConnectionRejected { rejected: Rejected },
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
    ws.on_upgrade(move |socket| async move {
        let tracker = state.tasks.clone();
        let _ = tracker.track_future(forward(state, socket, events)).await;
    })
}

/// The next event for a consumer. One that fell so far behind that events
/// were dropped gets `resync` in their place.
async fn next_event(events: &mut tokio::sync::broadcast::Receiver<Event>) -> Option<Event> {
    match events.recv().await {
        Ok(event) => Some(event),
        Err(RecvError::Lagged(_)) => Some(Event::Resync),
        Err(RecvError::Closed) => None,
    }
}

async fn forward(
    state: Arc<AppState>,
    socket: WebSocket,
    mut events: tokio::sync::broadcast::Receiver<Event>,
) {
    let (mut sink, mut stream) = socket.split();
    loop {
        let event = tokio::select! {
            _ = state.shutdown.cancelled() => break,
            received = next_event(&mut events) => match received {
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
        if sink.send(Message::Text(json.into())).await.is_err() {
            break;
        }
    }
    let _ = sink.close().await;
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
        for id in ["a", "b", "c", "d"] {
            sender.send(removed(id)).unwrap();
        }
        // "a" and "b" were dropped: the consumer learns that first, then
        // gets what is still buffered and whatever follows.
        assert_eq!(id_of(next_event(&mut receiver).await), "resync");
        assert_eq!(id_of(next_event(&mut receiver).await), "c");
        assert_eq!(id_of(next_event(&mut receiver).await), "d");
        sender.send(removed("e")).unwrap();
        assert_eq!(id_of(next_event(&mut receiver).await), "e");
        drop(sender);
        assert!(next_event(&mut receiver).await.is_none());
    }

    #[test]
    fn events_carry_their_type_tag() {
        let json = serde_json::to_value(Event::Resync).unwrap();
        assert_eq!(json, serde_json::json!({"type": "resync"}));
        let json = serde_json::to_value(removed("x")).unwrap();
        assert_eq!(json, serde_json::json!({"type": "device.removed", "device_id": "x"}));
    }
}
