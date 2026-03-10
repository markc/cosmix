pub mod connection;
pub mod peers;

use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::WebSocketUpgrade;
use axum::response::IntoResponse;
use axum::Router;
use cosmix_port::amp::AmpMessage;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::daemon::config::MeshSection;
use crate::daemon::state::SharedState;
use self::connection::{KEEPALIVE_INTERVAL, READ_TIMEOUT};
use self::peers::{PeerManager, PeerState};

/// Handle returned from start_mesh, stored in DaemonState.
pub struct MeshHandle {
    pub manager: Arc<PeerManager>,
}

impl std::fmt::Debug for MeshHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeshHandle").finish()
    }
}

/// Start the mesh subsystem: peer connections + inbound message router + listener.
pub async fn start_mesh(config: &MeshSection, _state: SharedState) -> Result<MeshHandle> {
    let (inbound_tx, mut inbound_rx) = mpsc::channel::<(String, AmpMessage)>(256);

    let manager = Arc::new(PeerManager::new(
        config.node_name.clone(),
        config.wg_ip.clone(),
        inbound_tx,
    ));

    // Connect to configured peers
    manager.connect_to_peers(&config.peers).await;

    // Spawn inbound message router
    let router_manager = manager.clone();
    tokio::spawn(async move {
        while let Some((peer_name, msg)) = inbound_rx.recv().await {
            // Check if this is a response to a pending request
            if let Some(id) = msg.get("id") {
                if msg.message_type() == Some("response") {
                    let mut pending = router_manager.pending.write().await;
                    if let Some(tx) = pending.remove(id) {
                        let _ = tx.send(msg);
                        continue;
                    }
                }
            }

            // Handle other inbound messages
            let cmd = msg.command_name().unwrap_or("").to_string();
            debug!(peer = %peer_name, command = %cmd, "routing inbound message");

            match cmd.as_str() {
                "hello" => {
                    info!(peer = %peer_name, "hello received");
                }
                _ => {
                    debug!(peer = %peer_name, command = %cmd, "unhandled mesh message");
                }
            }
        }
        warn!("mesh inbound router exited");
    });

    // Start WebSocket listener for inbound connections
    let listen_port = config.listen_port;
    let wg_ip = config.wg_ip.clone();
    let listener_manager = manager.clone();
    tokio::spawn(async move {
        let app = Router::new()
            .route("/mesh", axum::routing::get({
                let mgr = listener_manager.clone();
                move |ws: WebSocketUpgrade| {
                    let mgr = mgr.clone();
                    async move { handle_ws_upgrade(ws, mgr) }
                }
            }));

        let bind_addr = format!("{}:{}", wg_ip, listen_port);
        info!(addr = %bind_addr, "mesh WebSocket listener starting");

        let listener = match tokio::net::TcpListener::bind(&bind_addr).await {
            Ok(l) => l,
            Err(e) => {
                // Fall back to 0.0.0.0 if WireGuard IP not available
                warn!(addr = %bind_addr, error = %e, "bind failed, trying 0.0.0.0");
                match tokio::net::TcpListener::bind(format!("0.0.0.0:{}", listen_port)).await {
                    Ok(l) => l,
                    Err(e2) => {
                        tracing::error!(error = %e2, "mesh listener bind failed");
                        return;
                    }
                }
            }
        };

        info!(addr = ?listener.local_addr(), "mesh WebSocket listener ready");
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(error = %e, "mesh listener error");
        }
    });

    Ok(MeshHandle { manager })
}

fn handle_ws_upgrade(ws: WebSocketUpgrade, manager: Arc<PeerManager>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_inbound_ws(socket, manager))
}

/// Handle an inbound WebSocket connection from a peer.
async fn handle_inbound_ws(socket: WebSocket, manager: Arc<PeerManager>) {
    let (mut ws_write, mut ws_read) = socket.split();

    // Wait for hello to identify the peer
    let peer_name = match tokio::time::timeout(READ_TIMEOUT, ws_read.next()).await {
        Ok(Some(Ok(Message::Text(text)))) => {
            if let Ok(msg) = cosmix_port::amp::parse(&text) {
                if msg.command_name() == Some("hello") {
                    // Extract peer name from the 'from' address
                    if let Some(from) = msg.get("from") {
                        // from is like "gcwg.amp" → extract "gcwg"
                        from.trim_end_matches(".amp").split('.').last()
                            .unwrap_or("unknown").to_string()
                    } else {
                        "unknown".to_string()
                    }
                } else {
                    warn!("inbound connection: expected hello, got {:?}", msg.command_name());
                    return;
                }
            } else {
                warn!("inbound connection: invalid AMP message");
                return;
            }
        }
        _ => {
            warn!("inbound connection: no hello received");
            return;
        }
    };

    info!(peer = %peer_name, "inbound peer connected");

    let (outbound_tx, mut outbound_rx) = mpsc::channel::<AmpMessage>(256);

    // Register/update peer state
    {
        let mut peers = manager.peers.write().await;
        let state = peers.entry(peer_name.clone()).or_insert_with(|| PeerState {
            name: peer_name.clone(),
            wg_ip: String::new(),
            connected: false,
            last_seen: None,
            outbound_tx: None,
            remote_ports: Vec::new(),
        });
        state.connected = true;
        state.last_seen = Some(Instant::now());
        state.outbound_tx = Some(outbound_tx.clone());
    }

    // Send hello back
    let hello = AmpMessage::new()
        .with_header("amp", "1")
        .with_header("type", "event")
        .with_header("from", &format!("{}.amp", manager.node_name()))
        .with_header("command", "hello");
    let _ = outbound_tx.send(hello).await;

    // Run read/write loop (same logic as outbound connections)
    let inbound_tx = manager.inbound_tx.clone();
    let mut keepalive = tokio::time::interval(KEEPALIVE_INTERVAL);
    keepalive.tick().await;

    loop {
        tokio::select! {
            Some(msg) = outbound_rx.recv() => {
                let wire = msg.to_wire();
                if ws_write.send(Message::Text(wire.into())).await.is_err() {
                    warn!(peer = %peer_name, "inbound write failed");
                    break;
                }
            }
            _ = keepalive.tick() => {
                let empty = AmpMessage::empty().to_wire();
                if ws_write.send(Message::Text(empty.into())).await.is_err() {
                    break;
                }
            }
            result = tokio::time::timeout(READ_TIMEOUT, ws_read.next()) => {
                match result {
                    Ok(Some(Ok(Message::Text(text)))) => {
                        if let Ok(msg) = cosmix_port::amp::parse(&text) {
                            if msg.is_empty_message() {
                                continue;
                            }
                            debug!(peer = %peer_name, cmd = ?msg.command_name(), "inbound message");
                            if inbound_tx.send((peer_name.clone(), msg)).await.is_err() {
                                break;
                            }
                        }
                    }
                    Ok(Some(Ok(Message::Close(_)))) => break,
                    Ok(Some(Ok(_))) => continue,
                    Ok(Some(Err(e))) => {
                        warn!(peer = %peer_name, error = %e, "inbound read error");
                        break;
                    }
                    Ok(None) => break,
                    Err(_) => {
                        warn!(peer = %peer_name, "inbound read timeout");
                        break;
                    }
                }
            }
        }
    }

    // Mark disconnected
    {
        let mut peers = manager.peers.write().await;
        if let Some(state) = peers.get_mut(&peer_name) {
            state.connected = false;
            state.outbound_tx = None;
            state.remote_ports.clear();
        }
    }

    info!(peer = %peer_name, "inbound peer disconnected");
}
