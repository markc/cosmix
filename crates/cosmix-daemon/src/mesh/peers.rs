use cosmix_port::amp::{AmpAddress, AmpMessage};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, RwLock};
use tokio_tungstenite::connect_async;
use tracing::{info, warn};

use crate::daemon::config::PeerConfig;
use super::connection;

/// Reconnection backoff: 1s, 2s, 4s, 8s, 16s, 30s max.
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Timeout for request/response correlation.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Live state of a peer connection.
#[derive(Debug, Clone)]
pub struct PeerState {
    pub name: String,
    pub wg_ip: String,
    pub connected: bool,
    pub last_seen: Option<Instant>,
    pub outbound_tx: Option<mpsc::Sender<AmpMessage>>,
    /// Remote ports advertised by this peer.
    pub remote_ports: Vec<RemotePortInfo>,
}

/// Info about a port on a remote node.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RemotePortInfo {
    pub name: String,
    pub commands: Vec<String>,
}

/// Pending response waiters, keyed by request ID.
pub type PendingMap = Arc<RwLock<HashMap<String, oneshot::Sender<AmpMessage>>>>;

/// Manages all peer connections.
pub struct PeerManager {
    node_name: String,
    node_wg_ip: String,
    pub peers: Arc<RwLock<HashMap<String, PeerState>>>,
    /// Inbound messages from all peers, consumed by the router.
    pub inbound_tx: mpsc::Sender<(String, AmpMessage)>,
    /// Pending request/response correlation.
    pub pending: PendingMap,
}

impl PeerManager {
    pub fn new(
        node_name: String,
        node_wg_ip: String,
        inbound_tx: mpsc::Sender<(String, AmpMessage)>,
    ) -> Self {
        Self {
            node_name,
            node_wg_ip,
            peers: Arc::new(RwLock::new(HashMap::new())),
            inbound_tx,
            pending: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Returns the node name for this mesh node.
    pub fn node_name(&self) -> &str {
        &self.node_name
    }

    /// Start outbound connections to all configured peers.
    pub async fn connect_to_peers(&self, peer_configs: &HashMap<String, PeerConfig>) {
        for (name, config) in peer_configs.iter() {
            let peer_name: String = name.clone();
            let peer_ip: String = config.wg_ip.clone();
            let peer_port: u16 = config.port;
            let node_name = self.node_name.clone();
            let node_wg_ip = self.node_wg_ip.clone();
            let peers = self.peers.clone();
            let inbound_tx = self.inbound_tx.clone();

            tokio::spawn(async move {
                let mut backoff = INITIAL_BACKOFF;

                loop {
                    info!(peer = %peer_name, ip = %peer_ip, "connecting");

                    let url = format!("ws://{}:{}/mesh", peer_ip, peer_port);
                    match connect_async(&url).await {
                        Ok((ws_stream, _)) => {
                            info!(peer = %peer_name, "connected");
                            backoff = INITIAL_BACKOFF;

                            let (outbound_tx, outbound_rx) = mpsc::channel(256);

                            {
                                let mut peers = peers.write().await;
                                peers.insert(
                                    peer_name.clone(),
                                    PeerState {
                                        name: peer_name.clone(),
                                        wg_ip: peer_ip.clone(),
                                        connected: true,
                                        last_seen: Some(Instant::now()),
                                        outbound_tx: Some(outbound_tx.clone()),
                                        remote_ports: Vec::new(),
                                    },
                                );
                            }

                            // Send hello
                            let hello = AmpMessage::new()
                                .with_header("amp", "1")
                                .with_header("type", "event")
                                .with_header("from", &format!("{}.amp", node_name))
                                .with_header("command", "hello")
                                .with_header("args", &format!(r#"{{"wg_ip":"{}"}}"#, node_wg_ip));
                            let _ = outbound_tx.send(hello).await;

                            // Run connection (blocks until disconnect)
                            connection::run_connection(
                                peer_name.clone(),
                                ws_stream,
                                outbound_rx,
                                inbound_tx.clone(),
                            )
                            .await;

                            // Mark disconnected
                            {
                                let mut peers = peers.write().await;
                                if let Some(state) = peers.get_mut(&peer_name) {
                                    state.connected = false;
                                    state.outbound_tx = None;
                                    state.remote_ports.clear();
                                }
                            }
                        }
                        Err(e) => {
                            warn!(peer = %peer_name, error = %e, "connection failed");
                        }
                    }

                    info!(peer = %peer_name, backoff_secs = backoff.as_secs(), "reconnecting");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            });
        }
    }

    /// Send an AMP message to a specific peer.
    pub async fn send_to(&self, peer_name: &str, msg: AmpMessage) -> Result<(), String> {
        let peers = self.peers.read().await;
        if let Some(state) = peers.get(peer_name) {
            if let Some(tx) = &state.outbound_tx {
                tx.send(msg)
                    .await
                    .map_err(|_| format!("send channel closed for {peer_name}"))
            } else {
                Err(format!("peer {peer_name} not connected"))
            }
        } else {
            Err(format!("unknown peer: {peer_name}"))
        }
    }

    /// Route a message by its `to:` address.
    pub async fn route(&self, msg: AmpMessage) -> Result<(), String> {
        let to = msg
            .to_addr()
            .ok_or_else(|| "message has no 'to' address".to_string())?;

        let target_node = AmpAddress::parse(to)
            .ok_or_else(|| format!("invalid address: {to}"))?
            .node;

        self.send_to(&target_node, msg).await
    }

    /// Send a request and wait for a correlated response.
    pub async fn request(&self, peer_name: &str, mut msg: AmpMessage) -> Result<AmpMessage, String> {
        // Generate request ID
        let id = uuid::Uuid::new_v4().to_string();
        msg.set("id", &id);
        msg.set("type", "request");

        // Register pending waiter
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.write().await;
            pending.insert(id.clone(), tx);
        }

        // Send the message
        self.send_to(peer_name, msg).await?;

        // Wait for response with timeout
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => {
                // Sender dropped — clean up
                self.pending.write().await.remove(&id);
                Err("response channel closed".to_string())
            }
            Err(_) => {
                // Timeout — clean up
                self.pending.write().await.remove(&id);
                Err(format!("request to {peer_name} timed out after {}s", REQUEST_TIMEOUT.as_secs()))
            }
        }
    }

    /// Get status of all peers.
    pub async fn status(&self) -> Vec<PeerStatus> {
        let peers = self.peers.read().await;
        peers
            .values()
            .map(|s| PeerStatus {
                name: s.name.clone(),
                wg_ip: s.wg_ip.clone(),
                connected: s.connected,
                last_seen_secs: s.last_seen.map(|t| t.elapsed().as_secs()),
                remote_ports: s.remote_ports.clone(),
            })
            .collect()
    }
}

#[derive(Debug, serde::Serialize)]
pub struct PeerStatus {
    pub name: String,
    pub wg_ip: String,
    pub connected: bool,
    pub last_seen_secs: Option<u64>,
    pub remote_ports: Vec<RemotePortInfo>,
}
