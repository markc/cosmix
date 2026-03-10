use cosmix_port::amp::AmpMessage;
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, info, warn};

/// Keepalive interval — send empty AMP every 15 seconds.
pub(crate) const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// Read timeout — if no message received in 45s (3x keepalive), consider dead.
pub(crate) const READ_TIMEOUT: Duration = Duration::from_secs(45);

/// Handle a live outbound WebSocket connection to a peer.
///
/// Runs read and write loops concurrently. Returns when the connection drops.
/// The caller is responsible for reconnection.
pub async fn run_connection(
    peer_name: String,
    ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
    mut outbound_rx: mpsc::Receiver<AmpMessage>,
    inbound_tx: mpsc::Sender<(String, AmpMessage)>,
) {
    let (mut ws_write, mut ws_read) = ws_stream.split();

    let mut keepalive = tokio::time::interval(KEEPALIVE_INTERVAL);
    keepalive.tick().await; // first tick is immediate

    loop {
        tokio::select! {
            Some(msg) = outbound_rx.recv() => {
                let wire = msg.to_wire();
                debug!(peer = %peer_name, "sending {} bytes", wire.len());
                if ws_write.send(Message::Text(wire.into())).await.is_err() {
                    warn!(peer = %peer_name, "write failed");
                    break;
                }
            }
            _ = keepalive.tick() => {
                let empty = AmpMessage::empty().to_wire();
                if ws_write.send(Message::Text(empty.into())).await.is_err() {
                    warn!(peer = %peer_name, "keepalive write failed");
                    break;
                }
            }
            result = tokio::time::timeout(READ_TIMEOUT, ws_read.next()) => {
                match result {
                    Ok(Some(Ok(Message::Text(text)))) => {
                        if let Ok(msg) = cosmix_port::amp::parse(&text) {
                            if msg.is_empty_message() {
                                debug!(peer = %peer_name, "keepalive received");
                                continue;
                            }
                            debug!(peer = %peer_name, cmd = ?msg.command_name(), "received message");
                            if inbound_tx.send((peer_name.clone(), msg)).await.is_err() {
                                warn!(peer = %peer_name, "inbound channel closed");
                                break;
                            }
                        }
                    }
                    Ok(Some(Ok(Message::Close(_)))) => {
                        info!(peer = %peer_name, "peer sent close frame");
                        break;
                    }
                    Ok(Some(Ok(_))) => continue,
                    Ok(Some(Err(e))) => {
                        warn!(peer = %peer_name, error = %e, "read error");
                        break;
                    }
                    Ok(None) => {
                        info!(peer = %peer_name, "stream ended");
                        break;
                    }
                    Err(_) => {
                        warn!(peer = %peer_name, "read timeout ({}s)", READ_TIMEOUT.as_secs());
                        break;
                    }
                }
            }
        }
    }

    info!(peer = %peer_name, "connection ended");
}
