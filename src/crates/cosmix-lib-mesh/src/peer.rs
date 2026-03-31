//! Mesh peer discovery and WebSocket connection management.
//!
//! Peers are discovered from a config file listing known nodes with their
//! WireGuard mesh IPs and hub ports. The MeshPeers struct manages connections
//! to remote hubs and provides message relay.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock, mpsc};
use tokio_tungstenite::tungstenite;

use cosmix_amp::amp::{self, AmpMessage};

/// Configuration for a single mesh peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerConfig {
    /// Human-readable node name (used in AMP addresses like `files.mko.amp`)
    pub name: String,
    /// WireGuard mesh IP (e.g. "172.16.2.210")
    pub mesh_ip: String,
    /// Hub WebSocket port on that node (default 4200)
    #[serde(default = "default_hub_port")]
    pub hub_port: u16,
}

fn default_hub_port() -> u16 {
    4200
}

impl PeerConfig {
    /// WebSocket URL for this peer's hub.
    pub fn hub_url(&self) -> String {
        format!("ws://{}:{}/ws", self.mesh_ip, self.hub_port)
    }
}

/// Mesh configuration loaded from TOML.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshConfig {
    /// This node's name (must match what other peers call us).
    pub node_name: String,
    /// Known peers on the mesh.
    #[serde(default)]
    pub peers: Vec<PeerConfig>,
}

impl MeshConfig {
    /// Load mesh config from a TOML file.
    pub fn load(path: &str) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Self = toml_cfg::from_str(&content)?;
        Ok(config)
    }

    /// Load from default path (~/.config/cosmix/mesh.toml) or return empty config.
    pub fn load_default(node_name: &str) -> Self {
        let path = dirs_next::config_dir()
            .map(|d| d.join("cosmix/mesh.toml"))
            .unwrap_or_default();

        if path.exists() {
            match Self::load(&path.to_string_lossy()) {
                Ok(c) => return c,
                Err(e) => tracing::warn!(error = %e, "Failed to load mesh config, using empty"),
            }
        }

        Self {
            node_name: node_name.to_string(),
            peers: Vec::new(),
        }
    }

    /// Find a peer by node name.
    pub fn find_peer(&self, node_name: &str) -> Option<&PeerConfig> {
        self.peers.iter().find(|p| p.name == node_name)
    }
}

/// A connection to a remote hub with send/receive channels.
struct RemoteHub {
    tx: mpsc::UnboundedSender<String>,
    #[allow(dead_code)]
    connected: Arc<std::sync::atomic::AtomicBool>,
}

/// Manages connections to remote mesh peers.
///
/// Used by cosmix-hub to bridge messages to remote nodes.
pub struct MeshPeers {
    config: MeshConfig,
    /// Active connections: node_name → RemoteHub
    connections: Arc<RwLock<HashMap<String, RemoteHub>>>,
    /// Pending responses: message_id → oneshot sender for the response
    pending: Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<AmpMessage>>>>,
    /// Channel to deliver incoming messages from remote hubs back to local hub
    incoming_tx: mpsc::UnboundedSender<AmpMessage>,
}

impl MeshPeers {
    /// Create a new MeshPeers manager.
    ///
    /// `incoming_tx` receives messages from remote hubs that need to be
    /// delivered to local services.
    pub fn new(config: MeshConfig, incoming_tx: mpsc::UnboundedSender<AmpMessage>) -> Self {
        Self {
            config,
            connections: Arc::new(RwLock::new(HashMap::new())),
            pending: Arc::new(Mutex::new(HashMap::new())),
            incoming_tx,
        }
    }

    /// Get this node's name.
    pub fn node_name(&self) -> &str {
        &self.config.node_name
    }

    /// Get the list of configured peer names.
    pub fn peer_names(&self) -> Vec<String> {
        self.config.peers.iter().map(|p| p.name.clone()).collect()
    }

    /// Check if a node name is a known remote peer.
    pub fn is_remote_peer(&self, node_name: &str) -> bool {
        self.config.find_peer(node_name).is_some()
    }

    /// Send a message to a remote node's hub. Returns the response.
    ///
    /// Establishes connection on first use, reuses for subsequent calls.
    pub async fn call(&self, node_name: &str, msg: AmpMessage) -> Result<AmpMessage> {
        let peer = self.config.find_peer(node_name)
            .ok_or_else(|| anyhow::anyhow!("Unknown peer: {node_name}"))?
            .clone();

        // Ensure connection exists
        self.ensure_connected(&peer).await?;

        // Set up response channel
        let msg_id = msg.get("id")
            .map(|s| s.to_string())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        self.pending.lock().await.insert(msg_id.clone(), resp_tx);

        // Send the message with the id
        let mut msg = msg;
        msg.set("id", &msg_id);

        let connections = self.connections.read().await;
        if let Some(hub) = connections.get(node_name) {
            hub.tx.send(msg.to_wire())
                .map_err(|_| anyhow::anyhow!("Send to {node_name} failed"))?;
        } else {
            self.pending.lock().await.remove(&msg_id);
            anyhow::bail!("Connection to {node_name} lost");
        }
        drop(connections);

        // Wait for response with timeout
        match tokio::time::timeout(std::time::Duration::from_secs(30), resp_rx).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(_)) => {
                self.pending.lock().await.remove(&msg_id);
                anyhow::bail!("Response channel for {node_name} closed")
            }
            Err(_) => {
                self.pending.lock().await.remove(&msg_id);
                anyhow::bail!("Timeout waiting for response from {node_name}")
            }
        }
    }

    /// Send a fire-and-forget message to a remote node.
    pub async fn send(&self, node_name: &str, msg: AmpMessage) -> Result<()> {
        let peer = self.config.find_peer(node_name)
            .ok_or_else(|| anyhow::anyhow!("Unknown peer: {node_name}"))?
            .clone();

        self.ensure_connected(&peer).await?;

        let connections = self.connections.read().await;
        if let Some(hub) = connections.get(node_name) {
            hub.tx.send(msg.to_wire())
                .map_err(|_| anyhow::anyhow!("Send to {node_name} failed"))?;
        }

        Ok(())
    }

    /// Ensure we have an active WebSocket connection to a peer's hub.
    async fn ensure_connected(&self, peer: &PeerConfig) -> Result<()> {
        // Check if already connected
        {
            let connections = self.connections.read().await;
            if connections.contains_key(&peer.name) {
                return Ok(());
            }
        }

        // Connect
        let url = peer.hub_url();
        tracing::info!(peer = %peer.name, url = %url, "Connecting to remote hub");

        let (ws_stream, _) = tokio_tungstenite::connect_async(&url).await
            .map_err(|e| anyhow::anyhow!("Failed to connect to {}: {e}", peer.name))?;

        let (mut ws_sink, mut ws_stream_rx) = ws_stream.split();

        // Channel for sending messages to this remote hub
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let connected = Arc::new(std::sync::atomic::AtomicBool::new(true));

        // Register ourselves with the remote hub
        let register_msg = AmpMessage::new()
            .with_header("command", "hub.register")
            .with_header("from", &format!("bridge-{}", self.config.node_name))
            .with_header("to", "hub");
        let _ = ws_sink.send(tungstenite::Message::Text(register_msg.to_wire().into())).await;

        // Spawn sender task
        let connected_send = connected.clone();
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if ws_sink.send(tungstenite::Message::Text(msg.into())).await.is_err() {
                    connected_send.store(false, std::sync::atomic::Ordering::Relaxed);
                    break;
                }
            }
        });

        // Spawn reader task
        let peer_name = peer.name.clone();
        let pending = self.pending.clone();
        let incoming_tx = self.incoming_tx.clone();
        let connections = self.connections.clone();
        let connected_read = connected.clone();
        tokio::spawn(async move {
            while let Some(Ok(msg)) = ws_stream_rx.next().await {
                let text = match msg {
                    tungstenite::Message::Text(t) => t.to_string(),
                    tungstenite::Message::Close(_) => break,
                    _ => continue,
                };

                let Ok(amp_msg) = amp::parse(&text) else { continue };

                // Check if this is a response to a pending request
                if let Some(id) = amp_msg.get("id") {
                    let mut pending = pending.lock().await;
                    if let Some(sender) = pending.remove(id) {
                        let _ = sender.send(amp_msg);
                        continue;
                    }
                }

                // Otherwise it's an incoming command from a remote service — deliver locally
                let _ = incoming_tx.send(amp_msg);
            }

            // Connection lost — remove from registry
            connected_read.store(false, std::sync::atomic::Ordering::Relaxed);
            connections.write().await.remove(&peer_name);
            tracing::info!(peer = %peer_name, "Remote hub disconnected");
        });

        // Store connection
        self.connections.write().await.insert(
            peer.name.clone(),
            RemoteHub { tx, connected },
        );

        tracing::info!(peer = %peer.name, "Connected to remote hub");
        Ok(())
    }
}
