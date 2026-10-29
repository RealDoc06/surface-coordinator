//! The client listener: the WebSocket endpoint (handshake, heartbeats and
//! job messages) and the update downloads.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::connect_info::ConnectInfo;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use protocol::handshake::{self, Role};
use protocol::{reject_reason, ClientMessage, Hello, JobStatus, ServerMessage};
use tokio::time::Instant;

use crate::events::Event;
use crate::state::AppState;
use crate::store::{self, DeviceInfo, TargetResult};
use crate::{rollout, updates};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(1);
/// Largest message accepted from a client. Output chunks are far smaller.
const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
const MAX_ID_LEN: usize = 64;
const MAX_FIELD_LEN: usize = 255;
const MAX_RESULT_TEXT_LEN: usize = 4096;
/// How long a cancellation is repeated to a device that reconnects.
const REPEAT_CANCELS_FOR: Duration = Duration::from_secs(24 * 60 * 60);

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route(protocol::CLIENT_WS_PATH, get(upgrade))
        // Clients download updates from where they connect, without the
        // UI's authentication: the artifact is signed and public anyway.
        .route("/updates/client/{version}/{file}", get(updates::download))
}

async fn upgrade(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    ws: WebSocketUpgrade,
) -> Response {
    ws.max_message_size(MAX_MESSAGE_BYTES).max_frame_size(MAX_MESSAGE_BYTES).on_upgrade(move |socket| async move {
        let tracker = state.tasks.clone();
        // Tracked so that `stop` waits for the connection to wind down.
        let _ = tracker.track_future(serve(state, socket, peer)).await;
    })
}

type Sink = SplitSink<WebSocket, Message>;
type Stream = SplitStream<WebSocket>;

async fn send(sink: &mut Sink, message: &ServerMessage) -> Result<(), axum::Error> {
    tokio::time::timeout(WRITE_TIMEOUT, sink.send(Message::Text(message.to_json().into())))
        .await
        .map_err(|_| axum::Error::new(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "client stopped reading",
        )))?
}

async fn close(sink: &mut Sink) {
    let _ = tokio::time::timeout(CLOSE_TIMEOUT, sink.close()).await;
}

/// A stalled receiver cannot keep a replaced connection or shutdown alive.
async fn send_connected(
    connection: &crate::state::Connection,
    sink: &mut Sink,
    message: &ServerMessage,
) -> Result<(), axum::Error> {
    tokio::select! {
        biased;
        _ = connection.close.cancelled() => Err(axum::Error::new(std::io::Error::new(
            std::io::ErrorKind::ConnectionAborted,
            "connection closed by coordinator",
        ))),
        result = send(sink, message) => result,
    }
}

/// Next text frame, or `None` when the peer is gone.
async fn next_text(stream: &mut Stream) -> Option<String> {
    loop {
        match stream.next().await {
            Some(Ok(Message::Text(text))) => return Some(text.as_str().to_owned()),
            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return None,
            Some(Ok(_)) => continue,
        }
    }
}

struct Authenticated {
    hello: Hello,
    /// The hello's protocol version is not supported.
    restricted: bool,
}

async fn serve(state: Arc<AppState>, socket: WebSocket, peer: SocketAddr) {
    let (mut sink, mut stream) = socket.split();

    // The hello as received, kept so a failed attempt can be shown to the
    // operator with the device that made it.
    let mut claimed: Option<Hello> = None;
    let outcome = tokio::select! {
        _ = state.shutdown.cancelled() => return,
        outcome = tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            handshake(&state, &mut sink, &mut stream, &mut claimed),
        ) => outcome,
    };
    let (hello, restricted) = match outcome {
        Ok(Ok(Authenticated { hello, restricted })) => (hello, restricted),
        Ok(Err(failure)) => {
            tracing::warn!(%peer, device_id = failure.device_id.as_deref().unwrap_or("-"),
                reason = %failure.reason, "client handshake failed");
            if let Some(claimed) = claimed {
                record_rejected(&state, peer, claimed, failure.reason.clone()).await;
            }
            if let Some(reject) = failure.reject {
                let _ = send(&mut sink, &ServerMessage::Reject { reason: reject.to_owned() }).await;
            }
            close(&mut sink).await;
            return;
        }
        Err(_) => {
            tracing::warn!(%peer, reason = "timed out", "client handshake failed");
            close(&mut sink).await;
            return;
        }
    };

    let device_id = hello.device_id.clone();
    let connection = {
        let _presence = state.presence.lock().await;
        let stored = {
            let hello = hello.clone();
            state
                .store
                .call(move |conn| {
                    // Not part of the handshake validation: a device with an
                    // odd network name still connects, the text is just cut.
                    let network = hello.network.clone().unwrap_or_default();
                    let ssid = clip_chars(network.ssid, MAX_FIELD_LEN);
                    let ip = clip_chars(network.ip, MAX_FIELD_LEN);
                    store::upsert_device_online(
                        conn,
                        &DeviceInfo {
                            id: &hello.device_id,
                            enrollment_key_id: &hello.key_id,
                            hostname: &hello.hostname,
                            os: &hello.os,
                            client_version: &hello.client_version,
                            protocol_version: hello.protocol_version,
                            network_ssid: ssid.as_deref(),
                            network_ip: ip.as_deref(),
                        },
                    )
                })
                .await
        };
        if let Err(error) = stored {
            tracing::error!(%device_id, %error, "failed to record device");
            close(&mut sink).await;
            return;
        }
        state.register(&device_id, restricted)
    };
    if restricted {
        tracing::warn!(%peer, %device_id, hostname = %hello.hostname, protocol_version = hello.protocol_version,
            "device online with an unsupported protocol version; only client updates will be sent to it");
    } else {
        tracing::info!(%peer, %device_id, hostname = %hello.hostname, "device online");
    }
    state.publish_device(&device_id).await;

    let interval = state.config.heartbeat_interval;
    let welcome = ServerMessage::Welcome {
        heartbeat_interval_ms: u64::try_from(interval.as_millis()).unwrap_or(u64::MAX),
        restricted,
    };
    if send_connected(&connection, &mut sink, &welcome).await.is_ok() {
        // The session replays unresolved jobs and cancellations directly
        // from storage, one at a time, even when the offline backlog is large.
        connection.pending.notify_one();
        // The device may be one the update rollout is waiting for. In its own
        // task, so this connection starts serving at once.
        let rollout_state = state.clone();
        state.tasks.spawn(async move { rollout::run(&rollout_state).await });
        let ended = session(&state, &device_id, &connection, sink, stream).await;
        tracing::info!(%device_id, reason = ended, "device connection ended");
    }

    let _presence = state.presence.lock().await;
    if state.unregister(&device_id, connection.id) {
        let last_seen = connection.last_seen.load(Ordering::Relaxed);
        let id = device_id.clone();
        match state.store.call(move |conn| store::set_device_offline(conn, &id, last_seen)).await {
            Ok(changes) => {
                for change in changes {
                    state.publish(Event::JobTargetUpdated { job_id: change.job_id, target: change.target });
                }
            }
            Err(error) => tracing::error!(%device_id, %error, "failed to mark device offline"),
        }
        tracing::info!(%device_id, "device offline");
        state.publish_device(&device_id).await;
    }
}

/// Stores a refused connection attempt and tells the UI about it. Only
/// attempts that got as far as a readable hello are recorded.
async fn record_rejected(state: &AppState, peer: SocketAddr, claimed: Hello, reason: String) {
    let clip = |text: &str| text.chars().take(MAX_FIELD_LEN).collect::<String>();
    let (device_id, hostname, key_id) =
        (clip(&claimed.device_id), clip(&claimed.hostname), clip(&claimed.key_id));
    let recorded = state
        .store
        .call(move |conn| {
            store::record_rejected(
                conn,
                &peer.to_string(),
                Some(&device_id),
                Some(&hostname),
                Some(&key_id),
                &reason,
            )
        })
        .await;
    match recorded {
        Ok(rejected) => state.publish(Event::ConnectionRejected { rejected }),
        Err(error) => tracing::error!(%error, "failed to record a rejected connection"),
    }
}

struct HandshakeFailure {
    device_id: Option<String>,
    /// Logged on the coordinator.
    reason: String,
    /// Sent to the client, if anything is.
    reject: Option<&'static str>,
}

impl HandshakeFailure {
    fn new(device_id: Option<&str>, reason: impl Into<String>, reject: Option<&'static str>) -> Self {
        HandshakeFailure { device_id: device_id.map(str::to_owned), reason: reason.into(), reject }
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

async fn handshake(
    state: &AppState,
    sink: &mut Sink,
    stream: &mut Stream,
    claimed: &mut Option<Hello>,
) -> Result<Authenticated, HandshakeFailure> {
    let closed = |device: Option<&str>| HandshakeFailure::new(device, "connection closed", None);

    let text = next_text(stream).await.ok_or_else(|| closed(None))?;
    let hello = match ClientMessage::from_json(&text) {
        Ok(ClientMessage::Hello(hello)) => hello,
        _ => {
            return Err(HandshakeFailure::new(
                None,
                "first message was not a valid hello",
                Some(reject_reason::BAD_REQUEST),
            ))
        }
    };
    *claimed = Some(hello.clone());
    let device = Some(hello.device_id.as_str()).filter(|id| valid_id(id));

    if !valid_id(&hello.device_id)
        || !handshake::is_valid_nonce(&hello.nonce)
        || hello.hostname.len() > MAX_FIELD_LEN
        || hello.os.len() > MAX_FIELD_LEN
        || hello.client_version.len() > MAX_FIELD_LEN
        || hello.key_id.len() > MAX_FIELD_LEN
    {
        return Err(HandshakeFailure::new(
            device,
            "hello has malformed fields",
            Some(reject_reason::BAD_REQUEST),
        ));
    }
    // A version this coordinator does not support is still let in, so the
    // device can be updated: it is restricted to `client.update` jobs. The
    // handshake itself is the same in every version.
    let restricted = !protocol::is_supported(hello.protocol_version);

    let key_id = hello.key_id.clone();
    let key = state
        .store
        .call(move |conn| store::get_enrollment_key(conn, &key_id))
        .await
        .map_err(|error| HandshakeFailure::new(device, format!("database error: {error}"), None))?;
    let key = match key {
        Some(key) if key.revoked.is_none() => key,
        Some(_) => {
            return Err(HandshakeFailure::new(
                device,
                format!("enrollment key {} is revoked", hello.key_id),
                Some(reject_reason::UNKNOWN_KEY),
            ))
        }
        None => {
            return Err(HandshakeFailure::new(
                device,
                "unknown enrollment key id",
                Some(reject_reason::UNKNOWN_KEY),
            ))
        }
    };

    let server_nonce = handshake::generate_nonce();
    let challenge = ServerMessage::Challenge {
        mac: handshake::compute_mac(&key.secret, Role::Coordinator, &hello.nonce, &server_nonce),
        nonce: server_nonce.clone(),
    };
    send(sink, &challenge).await.map_err(|_| closed(device))?;

    // A client holding a different secret sees our MAC fail and hangs up here.
    let text = next_text(stream).await.ok_or_else(|| {
        HandshakeFailure::new(device, "client closed the connection after the challenge", None)
    })?;
    let mac = match ClientMessage::from_json(&text) {
        Ok(ClientMessage::Auth { mac }) => mac,
        _ => {
            return Err(HandshakeFailure::new(
                device,
                "expected auth after the challenge",
                Some(reject_reason::BAD_REQUEST),
            ))
        }
    };
    if !handshake::verify_mac(&key.secret, Role::Client, &hello.nonce, &server_nonce, &mac) {
        return Err(HandshakeFailure::new(
            device,
            "client MAC does not match the enrollment key",
            Some(reject_reason::BAD_MAC),
        ));
    }
    Ok(Authenticated { hello, restricted })
}

/// Per-connection replay positions, independent of the size of the backlog.
#[derive(Default, Clone, Copy)]
struct DeliveryCursor {
    job_rowid: i64,
    cancellation: (i64, i64),
}

enum Delivery {
    Cancel(store::Cancellation),
    Job(Box<(store::Dispatch, Option<store::TargetChange>)>),
}

/// The socket owns delivery order, so a cancel can never overtake the job
/// that was just loaded. Only one payload per connection is held in memory.
async fn next_delivery(
    state: &AppState,
    device_id: &str,
    connection: &crate::state::Connection,
    cursor: &mut DeliveryCursor,
) -> Result<Option<ServerMessage>, crate::Error> {
    let device = device_id.to_owned();
    let after = *cursor;
    let only_kind = connection.restricted.then_some(protocol::job_kind::CLIENT_UPDATE);
    let since = store::now_ms() - REPEAT_CANCELS_FOR.as_millis() as i64;
    let delivery = state.store.call(move |conn| {
        if let Some(cancel) = store::next_cancellation(conn, &device, since, after.cancellation)? {
            return Ok(Some(Delivery::Cancel(cancel)));
        }
        Ok(store::take_next_dispatchable(conn, &device, after.job_rowid, only_kind)?
            .map(|job| Delivery::Job(Box::new(job))))
    }).await?;
    Ok(match delivery {
        Some(Delivery::Cancel(cancel)) => {
            cursor.cancellation = (cancel.requested_at, cancel.rowid);
            Some(ServerMessage::JobCancel { job_target_id: cancel.target_id })
        }
        Some(Delivery::Job(job)) => {
            let (dispatch, change) = *job;
            cursor.job_rowid = cursor.job_rowid.max(dispatch.rowid);
            if let Some(change) = change {
                state.publish(Event::JobTargetUpdated { job_id: change.job_id, target: change.target });
            }
            Some(ServerMessage::Job {
                job_target_id: dispatch.job_target_id,
                job_id: dispatch.job_id,
                kind: dispatch.kind,
                payload: dispatch.payload,
            })
        }
        None => None,
    })
}

/// Runs an authenticated connection until it ends; returns why.
async fn session(
    state: &AppState,
    device_id: &str,
    connection: &crate::state::Connection,
    mut sink: Sink,
    mut stream: Stream,
) -> &'static str {
    let mut cursor = DeliveryCursor::default();
    let silence_limit = state.config.heartbeat_interval * protocol::MISSED_HEARTBEATS_BEFORE_OFFLINE;
    let silence = tokio::time::sleep(silence_limit);
    tokio::pin!(silence);

    let reason = loop {
        tokio::select! {
            _ = connection.close.cancelled() => break "closed by coordinator",
            _ = &mut silence => break "missed heartbeats",
            _ = connection.pending.notified() => {
                match next_delivery(state, device_id, connection, &mut cursor).await {
                    Ok(Some(message)) => {
                        if send_connected(connection, &mut sink, &message).await.is_err() {
                            break "connection lost";
                        }
                        connection.pending.notify_one();
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::error!(device_id, %error, "failed to load queued work");
                        break "database error";
                    }
                }
            }
            text = next_text(&mut stream) => {
                let Some(text) = text else { break "connection closed by client" };
                silence.as_mut().reset(Instant::now() + silence_limit);
                connection.last_seen.store(store::now_ms(), Ordering::Relaxed);
                match ClientMessage::from_json(&text) {
                    Ok(message) => {
                        match handle_message(state, device_id, message).await {
                            Ok(Some(reply)) => {
                                if send_connected(connection, &mut sink, &reply).await.is_err() {
                                    break "connection lost";
                                }
                                if matches!(reply, ServerMessage::JobResultAck { .. }) {
                                    connection.pending.notify_one();
                                }
                            }
                            Ok(None) => {}
                            Err(error) => {
                                // The client replays retained results/output
                                // when reconnecting, so do not strand a full
                                // delivery window after a transient DB error.
                                tracing::error!(device_id, %error, "failed to store a client message");
                                break "database error";
                            }
                        }
                    }
                    Err(error) => tracing::debug!(device_id, %error, "ignoring malformed message"),
                }
            }
        }
    };
    close(&mut sink).await;
    reason
}

fn clip(text: Option<String>, max: usize) -> Option<String> {
    text.map(|mut text| {
        if text.len() > max {
            let mut cut = max;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
        }
        text
    })
}

/// Cuts text to at most `max` characters.
fn clip_chars(text: Option<String>, max: usize) -> Option<String> {
    text.map(|text| match text.char_indices().nth(max) {
        Some((end, _)) => text[..end].to_owned(),
        None => text,
    })
}

/// Applies one message from an authenticated client; returns the reply.
async fn handle_message(
    state: &AppState,
    device_id: &str,
    message: ClientMessage,
) -> Result<Option<ServerMessage>, crate::Error> {
    let device = device_id.to_owned();
    match message {
        ClientMessage::Heartbeat => Ok(Some(ServerMessage::HeartbeatAck)),
        ClientMessage::JobStarted { job_target_id } => state
            .store
            .call(move |conn| store::mark_target_started(conn, &job_target_id, &device))
            .await
            .map(|change| {
                if let Some(change) = change {
                    state.publish(Event::JobTargetUpdated { job_id: change.job_id, target: change.target });
                }
                None
            }),
        ClientMessage::JobOutput { job_target_id, seq, data, stream: _ } => {
            let target_id = job_target_id.clone();
            state
                .store
                .call(move |conn| {
                    store::append_target_output(
                        conn,
                        &job_target_id,
                        &device,
                        seq,
                        &data,
                        protocol::MAX_OUTPUT_BYTES,
                    )
                })
                .await
                .map(|appended| {
                    if let Some(appended) = appended {
                        state.publish(Event::JobOutput {
                            job_id: appended.job_id,
                            job_target_id: target_id,
                            data: appended.data,
                            truncated: appended.truncated,
                            offset: appended.offset,
                        });
                    }
                    None
                })
        }
        ClientMessage::JobResult(result) => {
            let target_id = result.job_target_id.clone();
            state
                .store
                .call(move |conn| {
                    let reason = clip(result.reason, MAX_FIELD_LEN);
                    let message = clip(result.message, MAX_RESULT_TEXT_LEN);
                    store::finish_target(
                        conn,
                        &result.job_target_id,
                        &device,
                        &TargetResult {
                            succeeded: result.status == JobStatus::Succeeded,
                            exit_code: result.exit_code,
                            reason: reason.as_deref(),
                            message: message.as_deref(),
                        },
                    )
                })
                .await
                .map(|change| {
                    if let Some(change) = change {
                        state.publish(Event::JobTargetUpdated { job_id: change.job_id, target: change.target });
                    }
                    // Acknowledged even if nothing changed, including for targets
                    // this coordinator does not know: the acknowledgement only
                    // tells the client to stop re-sending the result.
                    Some(ServerMessage::JobResultAck { job_target_id: target_id })
                })
        }
        // Handshake messages after the handshake, and types this version
        // does not know, are ignored.
        ClientMessage::Hello(_) | ClientMessage::Auth { .. } | ClientMessage::Unknown => Ok(None),
    }
}
