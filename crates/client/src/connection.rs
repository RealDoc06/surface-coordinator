//! One connection to the coordinator: connect, handshake, then exchange
//! messages until the connection ends.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use protocol::handshake::{self, Role};
use protocol::{ClientMessage, Hello, ServerMessage};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

use crate::jobs::{Job, JobManager};
use crate::platform::SystemInfo;
use crate::ClientConfig;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// How a connection attempt ended.
pub enum Ended {
    /// The client is shutting down.
    Shutdown,
    /// The connection failed or was lost; reconnect after a delay.
    Lost {
        reason: String,
        /// How long the session lasted after `welcome`, if it got that far.
        connected_for: Option<Duration>,
    },
}

fn lost(reason: impl Into<String>) -> Ended {
    Ended::Lost { reason: reason.into(), connected_for: None }
}

async fn send(socket: &mut Socket, message: &ClientMessage) -> Result<(), String> {
    socket
        .send(Message::text(message.to_json()))
        .await
        .map_err(|error| format!("send failed: {error}"))
}

/// Next message of a known type. Unknown types and malformed messages are
/// skipped; an error means the connection is gone.
async fn receive(socket: &mut Socket) -> Result<ServerMessage, String> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => match ServerMessage::from_json(text.as_str()) {
                Ok(ServerMessage::Unknown) => tracing::debug!("ignoring message of unknown type"),
                Ok(message) => return Ok(message),
                Err(error) => tracing::debug!(%error, "ignoring malformed message"),
            },
            Some(Ok(Message::Close(_))) | None => return Err("closed by coordinator".into()),
            Some(Ok(_)) => {}
            Some(Err(error)) => return Err(format!("connection error: {error}")),
        }
    }
}

/// Connects, authenticates and serves one session.
pub async fn run(
    config: &ClientConfig,
    system: &SystemInfo,
    jobs: &JobManager,
    shutdown: &CancellationToken,
) -> Ended {
    let url = format!(
        "{}{}",
        config.coordinator_url.trim_end_matches('/'),
        protocol::CLIENT_WS_PATH
    );
    let handshake = async {
        let (mut socket, _) = tokio_tungstenite::connect_async(url.as_str())
            .await
            .map_err(|error| format!("connect failed: {error}"))?;
        let heartbeat = authenticate(config, system, &mut socket).await?;
        Ok::<_, String>((socket, heartbeat))
    };
    let connected = tokio::select! {
        _ = shutdown.cancelled() => return Ended::Shutdown,
        connected = tokio::time::timeout(config.handshake_timeout, handshake) => connected,
    };
    let (socket, heartbeat) = match connected {
        Ok(Ok(connected)) => connected,
        Ok(Err(reason)) => return lost(reason),
        Err(_) => return lost("handshake timed out"),
    };
    tracing::info!(%url, "connected to coordinator");

    let since = Instant::now();
    let ended = session(socket, heartbeat, jobs, shutdown).await;
    jobs.detach();
    match ended {
        Ended::Lost { reason, .. } => Ended::Lost { reason, connected_for: Some(since.elapsed()) },
        Ended::Shutdown => Ended::Shutdown,
    }
}

/// Runs the handshake and returns the heartbeat interval from `welcome`.
async fn authenticate(
    config: &ClientConfig,
    system: &SystemInfo,
    socket: &mut Socket,
) -> Result<Duration, String> {
    let client_nonce = handshake::generate_nonce();
    let hello = ClientMessage::Hello(Hello {
        protocol_version: protocol::PROTOCOL_VERSION,
        device_id: config.device_id.clone(),
        hostname: system.hostname.clone(),
        os: system.os.clone(),
        client_version: config.client_version.clone(),
        key_id: config.key_id.clone(),
        nonce: client_nonce.clone(),
    });
    send(socket, &hello).await?;

    let server_nonce = match receive(socket).await? {
        ServerMessage::Challenge { nonce, mac } => {
            // The coordinator must prove it holds the key before this
            // client proves anything to it.
            let genuine = handshake::verify_mac(
                &config.key_secret,
                Role::Coordinator,
                &client_nonce,
                &nonce,
                &mac,
            );
            if !genuine {
                let _ = socket.close(None).await;
                return Err("handshake failed: the coordinator did not prove knowledge of the \
                            enrollment key (wrong key or wrong coordinator)"
                    .into());
            }
            nonce
        }
        ServerMessage::Reject { reason } => {
            return Err(format!("handshake rejected by coordinator: {reason}"))
        }
        other => return Err(format!("handshake failed: expected challenge, got {other:?}")),
    };

    let mac = handshake::compute_mac(&config.key_secret, Role::Client, &client_nonce, &server_nonce);
    send(socket, &ClientMessage::Auth { mac }).await?;
    match receive(socket).await? {
        ServerMessage::Welcome { heartbeat_interval_ms } => {
            let interval = match heartbeat_interval_ms {
                0 => Duration::from_secs(protocol::DEFAULT_HEARTBEAT_INTERVAL_SECS),
                ms => Duration::from_millis(ms),
            };
            Ok(interval)
        }
        ServerMessage::Reject { reason } => {
            Err(format!("handshake rejected by coordinator: {reason}"))
        }
        other => Err(format!("handshake failed: expected welcome, got {other:?}")),
    }
}

async fn session(
    mut socket: Socket,
    heartbeat: Duration,
    jobs: &JobManager,
    shutdown: &CancellationToken,
) -> Ended {
    // Job messages reach this connection through the channel; attaching
    // replays whatever the coordinator has not acknowledged yet.
    let (outbound, mut outbound_rx) = mpsc::unbounded_channel();
    jobs.attach(outbound);

    let mut ticker = tokio::time::interval_at(Instant::now() + heartbeat, heartbeat);
    let silence_limit = heartbeat * protocol::MISSED_HEARTBEATS_BEFORE_OFFLINE;
    let mut last_heard = Instant::now();

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                let _ = socket.close(None).await;
                return Ended::Shutdown;
            }
            _ = ticker.tick() => {
                if last_heard.elapsed() >= silence_limit {
                    let _ = socket.close(None).await;
                    return lost("coordinator stopped responding");
                }
                if let Err(reason) = send(&mut socket, &ClientMessage::Heartbeat).await {
                    return lost(reason);
                }
            }
            Some(message) = outbound_rx.recv() => {
                if let Err(reason) = send(&mut socket, &message).await {
                    return lost(reason);
                }
            }
            received = receive(&mut socket) => {
                let message = match received {
                    Ok(message) => message,
                    Err(reason) => return lost(reason),
                };
                last_heard = Instant::now();
                match message {
                    ServerMessage::Job { job_target_id, job_id: _, kind, payload } => {
                        jobs.accept(Job { job_target_id, kind, payload });
                    }
                    ServerMessage::JobResultAck { job_target_id } => jobs.acknowledge(&job_target_id),
                    ServerMessage::HeartbeatAck => {}
                    // Handshake messages have no meaning here.
                    ServerMessage::Challenge { .. }
                    | ServerMessage::Welcome { .. }
                    | ServerMessage::Reject { .. }
                    | ServerMessage::Unknown => {}
                }
            }
        }
    }
}
