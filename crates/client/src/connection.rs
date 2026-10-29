//! One connection to the coordinator: connect, handshake, then exchange
//! messages until the connection ends.

use std::time::Duration;

use futures_util::{Sink, SinkExt, Stream, StreamExt};
use protocol::handshake::{self, Role};
use protocol::{ClientMessage, Hello, NetworkInfo, ServerMessage};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::{Error as SocketError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

use crate::jobs::{Job, JobManager};
use crate::network::Network;
use crate::platform::SystemInfo;
use crate::ClientConfig;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

const CLOSE_GRACE: Duration = Duration::from_secs(1);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

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

async fn send(
    socket: &mut (impl Sink<Message, Error = SocketError> + Unpin),
    message: &ClientMessage,
) -> Result<(), String> {
    socket
        .send(Message::text(message.to_json()))
        .await
        .map_err(|error| format!("send failed: {error}"))
}

/// Next message of a known type. Unknown types and malformed messages are
/// skipped; an error means the connection is gone.
async fn receive(
    socket: &mut (impl Stream<Item = Result<Message, SocketError>> + Unpin),
) -> Result<ServerMessage, String> {
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

/// What one connection attempt needs besides the configuration.
pub struct Attempt<'a> {
    /// Base URL of the coordinator.
    pub url: &'a str,
    /// Reported in `hello`.
    pub network_info: Option<NetworkInfo>,
    pub system: &'a SystemInfo,
    pub jobs: &'a JobManager,
    pub network: &'a Network,
}

/// Connects, authenticates and serves one session.
pub async fn run(config: &ClientConfig, attempt: Attempt<'_>, shutdown: &CancellationToken) -> Ended {
    let Attempt { url: base_url, network_info, system, jobs, network } = attempt;
    let url = format!("{}{}", base_url.trim_end_matches('/'), protocol::CLIENT_WS_PATH);
    let handshake = async {
        // Job events and acknowledgements are small, latency-sensitive frames.
        let (mut socket, _) = tokio_tungstenite::connect_async_with_config(url.as_str(), None, true)
            .await
            .map_err(|error| format!("connect failed: {error}"))?;
        let heartbeat = authenticate(config, system, network_info, &mut socket).await?;
        Ok::<_, String>((socket, heartbeat))
    };
    let connected = tokio::select! {
        biased;
        _ = shutdown.cancelled() => return Ended::Shutdown,
        _ = network.reconnect_requested() => return lost("reconnecting after a network change"),
        connected = tokio::time::timeout(config.handshake_timeout, handshake) => connected,
    };
    let (socket, heartbeat) = match connected {
        Ok(Ok(connected)) => connected,
        Ok(Err(reason)) => return lost(reason),
        Err(_) => return lost("handshake timed out"),
    };
    tracing::info!(%url, "connected to coordinator");

    let since = Instant::now();
    network.mark_connected(base_url);
    let ended = session(socket, heartbeat, jobs, network, shutdown).await;
    jobs.detach();
    network.mark_disconnected();
    match ended {
        Ended::Lost { reason, .. } => Ended::Lost { reason, connected_for: Some(since.elapsed()) },
        Ended::Shutdown => Ended::Shutdown,
    }
}

/// Runs the handshake and returns the heartbeat interval from `welcome`.
async fn authenticate(
    config: &ClientConfig,
    system: &SystemInfo,
    network: Option<NetworkInfo>,
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
        network,
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
                let _ = tokio::time::timeout(CLOSE_GRACE, socket.close(None)).await;
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
        ServerMessage::Welcome { heartbeat_interval_ms, restricted } => {
            if restricted {
                // The coordinator only sends `client.update` jobs to this
                // client until it is updated; the session works as usual.
                tracing::warn!("the coordinator does not support this client's protocol version; it will only be offered updates");
            }
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
    socket: impl Sink<Message, Error = SocketError>
        + Stream<Item = Result<Message, SocketError>>
        + Unpin,
    heartbeat: Duration,
    jobs: &JobManager,
    network: &Network,
    shutdown: &CancellationToken,
) -> Ended {
    // Job messages reach this connection through the channel; attaching
    // replays whatever the coordinator has not acknowledged yet.
    let (outbound, mut outbound_rx) = mpsc::unbounded_channel();
    jobs.attach(outbound);

    let silence_limit = heartbeat * protocol::MISSED_HEARTBEATS_BEFORE_OFFLINE;
    let mut last_heard = Instant::now();
    let (mut writer, mut reader) = socket.split();

    let ended = {
        // Keep a write alive while reading acknowledgements and cancellations.
        // Recreating this future on every received message would drop the job
        // message it has already taken from the queue.
        let writing = write_messages(&mut writer, &mut outbound_rx, heartbeat);
        tokio::pin!(writing);
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break Ended::Shutdown,
                // A network change must interrupt even a blocked socket write.
                _ = network.reconnect_requested() => {
                    break lost("reconnecting after a network change");
                }
                _ = tokio::time::sleep_until(last_heard + silence_limit) => {
                    break lost("coordinator stopped responding");
                }
                reason = &mut writing => break lost(reason),
                received = receive(&mut reader) => {
                    let message = match received {
                        Ok(message) => message,
                        Err(reason) => break lost(reason),
                    };
                    last_heard = Instant::now();
                    match message {
                        ServerMessage::Job { job_target_id, job_id: _, kind, payload } => {
                            jobs.accept(Job { job_target_id, kind, payload });
                        }
                        ServerMessage::JobResultAck { job_target_id } => jobs.acknowledge(&job_target_id),
                        ServerMessage::HeartbeatAck => {}
                        ServerMessage::JobCancel { job_target_id } => jobs.cancel(&job_target_id),
                        // Handshake messages have no meaning here.
                        ServerMessage::Challenge { .. }
                        | ServerMessage::Welcome { .. }
                        | ServerMessage::Reject { .. }
                        | ServerMessage::Unknown => {}
                    }
                }
            }
        }
    };

    // On shutdown, try to send queued results (notably the update/restart
    // notice) before closing. A dead network must never hold shutdown or
    // reconnection open indefinitely.
    let close = async {
        if matches!(ended, Ended::Shutdown) {
            while let Ok(message) = outbound_rx.try_recv() {
                if send(&mut writer, &message).await.is_err() {
                    return;
                }
            }
        }
        let _ = writer.close().await;
    };
    let _ = tokio::time::timeout(CLOSE_GRACE, close).await;
    ended
}

async fn write_messages(
    socket: &mut (impl Sink<Message, Error = SocketError> + Unpin),
    outbound: &mut mpsc::UnboundedReceiver<ClientMessage>,
    heartbeat: Duration,
) -> String {
    let mut ticker = tokio::time::interval_at(Instant::now() + heartbeat, heartbeat);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let message = tokio::select! {
            _ = ticker.tick() => ClientMessage::Heartbeat,
            Some(message) = outbound.recv() => message,
        };
        match tokio::time::timeout(WRITE_TIMEOUT, send(socket, &message)).await {
            Ok(Ok(())) => {}
            Ok(Err(reason)) => return reason,
            Err(_) => return "send timed out".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncReadExt;
    use tokio_tungstenite::tungstenite::protocol::Role as SocketRole;

    use super::*;
    use crate::platform::OutputChunk;

    #[tokio::test]
    async fn a_network_change_interrupts_a_stalled_handshake() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let mut config =
            ClientConfig::new(Some(url.clone()), "test-key", "test-secret", "test-device");
        config.handshake_timeout = Duration::from_secs(30);
        let system = SystemInfo { hostname: "test-host".into(), os: "test-os".into() };
        let (jobs, _queue) = JobManager::new(None).unwrap();
        let network = Network::load(Some(url.clone()), None).unwrap();
        let shutdown = CancellationToken::new();
        let attempt = Attempt {
            url: &url,
            network_info: None,
            system: &system,
            jobs: &jobs,
            network: &network,
        };

        let coordinator = async {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            assert!(matches!(socket.next().await, Some(Ok(Message::Text(_)))));
            // The old coordinator never sends a challenge. A new network
            // should take effect without waiting out the handshake timeout.
            network.reconnect_now();
            let _ = socket.next().await;
        };
        let (ended, ()) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(run(&config, attempt, &shutdown), coordinator)
        })
        .await
        .expect("a network change interrupts authentication promptly");
        assert!(
            matches!(ended, Ended::Lost { connected_for: None, reason } if reason == "reconnecting after a network change")
        );
    }

    #[tokio::test]
    async fn blocked_output_keeps_commands_and_session_deadlines_responsive() {
        enum Stop {
            Shutdown,
            Reconnect,
            Silence,
        }

        for stop in [Stop::Shutdown, Stop::Reconnect, Stop::Silence] {
            // A small memory socket guarantees backpressure without relying
            // on platform-dependent TCP buffer sizes or changing the network.
            let (client, coordinator) = tokio::io::duplex(64);
            let socket = WebSocketStream::from_raw_socket(client, SocketRole::Client, None).await;
            let mut coordinator =
                WebSocketStream::from_raw_socket(coordinator, SocketRole::Server, None).await;
            let (jobs, mut queue) = JobManager::new(None).unwrap();
            let network = Network::load(None, None).unwrap();
            let shutdown = CancellationToken::new();
            let heartbeat = match stop {
                Stop::Silence => Duration::from_millis(100),
                _ => Duration::from_secs(30),
            };

            let peer = async {
                coordinator
                    .send(Message::text(
                        ServerMessage::Job {
                            job_target_id: "first".into(),
                            job_id: "job-first".into(),
                            kind: "script.run".into(),
                            payload: serde_json::json!({}),
                        }
                        .to_json(),
                    ))
                    .await
                    .unwrap();
                assert_eq!(queue.recv().await.unwrap().job_target_id, "first");
                jobs.output(
                    "first",
                    OutputChunk { stream: protocol::OutputStream::Stdout, data: "x".repeat(1024) },
                );

                // Observe the first byte, then stop reading: the client must
                // remain blocked while flushing the rest of this frame.
                coordinator.get_mut().read_exact(&mut [0u8; 1]).await.unwrap();
                coordinator
                    .send(Message::text(
                        ServerMessage::Job {
                            job_target_id: "second".into(),
                            job_id: "job-second".into(),
                            kind: "script.run".into(),
                            payload: serde_json::json!({}),
                        }
                        .to_json(),
                    ))
                    .await
                    .unwrap();
                assert_eq!(queue.recv().await.unwrap().job_target_id, "second");
                match stop {
                    Stop::Shutdown => shutdown.cancel(),
                    Stop::Reconnect => network.reconnect_now(),
                    Stop::Silence => {}
                }
                // Keep both halves open without reading any more output.
            };
            let (ended, ()) = tokio::time::timeout(Duration::from_secs(3), async {
                tokio::join!(session(socket, heartbeat, &jobs, &network, &shutdown), peer)
            })
            .await
            .expect(
                "a blocked write must not stop reads, shutdown, reconnect, or silence detection",
            );
            match (stop, ended) {
                (Stop::Shutdown, Ended::Shutdown) => {}
                (Stop::Reconnect, Ended::Lost { reason, .. }) => {
                    assert_eq!(reason, "reconnecting after a network change");
                }
                (Stop::Silence, Ended::Lost { reason, .. }) => {
                    assert_eq!(reason, "coordinator stopped responding");
                }
                _ => panic!("session ended for the wrong reason"),
            }
        }
    }
}
