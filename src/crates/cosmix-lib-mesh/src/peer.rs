//! Mesh peer discovery and WebSocket connection management.
//!
//! Peers are discovered from a config file listing known nodes with their
//! WireGuard mesh IPs and broker ports. The MeshPeers struct manages
//! connections to remote brokers and provides message relay.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock, RwLockReadGuard, mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite;

use base64::Engine as _;
use cosmix_bus::bus::{self, BusMessage};
use cosmix_mesh_trust::admission::{AdmissionTranscript, sign_admission_transcript};
pub use cosmix_mesh_trust::routing::DEFAULT_NODED_PORT;

/// Configuration for a single mesh peer.
///
/// `deny_unknown_fields` makes a stale config loud: any peer entry still
/// using the pre-rename `hub_port` key will fail to load instead of
/// silently falling through to the `noded_port` default and connecting
/// to the wrong port on a node that customized it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerConfig {
    /// Human-readable node name (used in Bus addresses like `files.delta.bus`)
    pub name: String,
    /// WireGuard mesh IP. Typed as `IpAddr`, not `String`: the broker
    /// hands this straight to `connect_async`, so a hostname here would
    /// make mesh routing consult DNS. Parsing at deserialize time makes
    /// that unrepresentable rather than merely discouraged.
    pub mesh_ip: IpAddr,
    /// Broker WebSocket port on that node (default 4200)
    #[serde(default = "default_noded_port")]
    pub noded_port: u16,
}

fn default_noded_port() -> u16 {
    DEFAULT_NODED_PORT
}

impl PeerConfig {
    /// Typed transport address for cache identity and IPv6-safe URL rendering.
    pub fn noded_addr(&self) -> SocketAddr {
        SocketAddr::new(self.mesh_ip, self.noded_port)
    }

    /// WebSocket URL for this peer's broker.
    pub fn noded_url(&self) -> String {
        // Rendered via SocketAddr so an IPv6 address is bracketed
        // (`[::1]:4200`); direct interpolation would emit an invalid URL.
        format!("ws://{}/ws", self.noded_addr())
    }
}

/// Mesh configuration loaded from a `mesh.conf.mix` file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshConfig {
    /// This node's name (must match what other peers call us).
    pub node_name: String,
    /// Known peers on the mesh.
    #[serde(default)]
    pub peers: Vec<PeerConfig>,
    /// SPEC 13 §9a D2 admission seed (2-c-1c) — this node's `kind:"d2"` private
    /// seed, used by the bridge prover to sign the admission transcript. NOT in
    /// `mesh.conf.mix`; injected at startup by noded from
    /// `/etc/cosmix/noded/d2.seed`. `None` ⇒ the node cannot prove itself
    /// (prover-incapable; the broker logs `unproven:no-proof`).
    #[serde(skip)]
    pub d2_seed: Option<[u8; 32]>,
}

impl MeshConfig {
    /// Load mesh config from an explicit `mesh.conf.mix` file path (the
    /// `cosmix-noded --mesh-config` path).
    ///
    /// A read or parse failure is a hard error.
    pub fn load(path: &str) -> Result<Self> {
        cosmix_config::load_conf_mix_path::<Self>(std::path::Path::new(path))
    }

    /// Load `mesh.conf.mix` from the default directory
    /// (`~/.config/cosmix/`), or return an empty config. Best-effort: a
    /// present-but-broken config warns and falls back to empty rather
    /// than silently masking it with a different file.
    pub fn load_default(node_name: &str) -> Self {
        let dir = cosmix_config::cosmix_path(cosmix_config::CosmixDir::Etc);
        let chosen = Some(dir.join("mesh.conf.mix")).filter(|p| p.exists());

        if let Some(path) = chosen {
            match cosmix_config::load_conf_mix_path::<Self>(&path) {
                Ok(c) => return c,
                Err(e) => tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    "Failed to load mesh config, using empty"
                ),
            }
        }

        Self {
            node_name: node_name.to_string(),
            peers: Vec::new(),
            d2_seed: None,
        }
    }

    /// Find a peer by node name.
    pub fn find_peer(&self, node_name: &str) -> Option<&PeerConfig> {
        self.peers.iter().find(|p| p.name == node_name)
    }
}

/// SPEC 13 §9a (2-c-1c) — build a signed `noded.admit.response` for a received
/// `noded.admit.challenge`. `claimed_source_node` is this node's BARE name
/// (e.g. `delta`), NOT the `bridge-<node>` label (the `from` header keeps the
/// label; `admit()`'s `NameMismatch` binds the bare name). Origin-only:
/// `client_ephemeral`/`channel_binding_hash` are 32 zero bytes, built from the
/// challenge body's DECODED raw bytes / native-`u64` epoch (the byte-for-byte
/// contract the broker's `reconstruct_proof` mirrors). Returns `None` if there
/// is no d2 seed (prover-incapable) or the challenge is malformed — the caller
/// then registers without proving (the broker logs `unproven:no-proof`).
fn build_admit_response(
    challenge: &BusMessage,
    node_name: &str,
    d2_seed: &Option<[u8; 32]>,
) -> Option<String> {
    let seed = d2_seed.as_ref()?;
    let id = challenge.get("id")?.to_string();
    let body: serde_json::Value = serde_json::from_str(&challenge.body).ok()?;
    let b64 = base64::engine::general_purpose::STANDARD;
    let transcript = AdmissionTranscript {
        mesh_fqdn: body.get("mesh_fqdn")?.as_str()?.to_string(),
        claimed_source_node: node_name.to_string(),
        verifying_broker_node: body.get("verifying_broker_node")?.as_str()?.to_string(),
        inventory_epoch: body.get("inventory_epoch")?.as_u64()?,
        session_id: b64.decode(body.get("session_id")?.as_str()?).ok()?,
        server_nonce: b64.decode(body.get("server_nonce")?.as_str()?).ok()?,
        client_ephemeral: vec![0u8; 32],
        channel_binding_hash: vec![0u8; 32],
    };
    let sig = sign_admission_transcript(seed, &transcript).ok()?;
    let mut resp = BusMessage::new()
        .with_header("command", "noded.admit.response")
        .with_header("type", "response")
        .with_header("from", &format!("bridge-{node_name}"))
        .with_header("to", "noded")
        .with_header("id", &id);
    resp.body = serde_json::json!({
        "claimed_source_node": node_name,
        "signed_epoch": transcript.inventory_epoch,
        "signature": b64.encode(sig),
        "client_ephemeral": b64.encode([0u8; 32]),
        "channel_binding_hash": b64.encode([0u8; 32]),
    })
    .to_string();
    Some(resp.to_wire())
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
const ADMISSION_CHALLENGE_TIMEOUT: Duration = Duration::from_secs(1);

/// The signed-routing revision whose desired endpoints are installed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AuthorityRevision {
    pub epoch: u64,
    pub recovery_generation: u64,
}

/// Why an established connection stopped being authorised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionRetireReason {
    Removed,
    EndpointChanged { replacement: SocketAddr },
}

impl ConnectionRetireReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Removed => "removed",
            Self::EndpointChanged { .. } => "endpoint-changed",
        }
    }
}

/// An established transport retired by endpoint reconciliation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetiredConnection {
    pub peer: String,
    pub endpoint: SocketAddr,
    pub connection_generation: uuid::Uuid,
    pub reason: ConnectionRetireReason,
}

/// An outbound RPC failed by endpoint reconciliation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetiredInflight {
    pub peer: String,
    pub connection_generation: uuid::Uuid,
    pub message_id: String,
    pub class: String,
}

/// The complete, synchronous effect of installing a desired endpoint set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    pub added: usize,
    pub removed: usize,
    pub endpoint_changed: usize,
    pub connections_retired: Vec<RetiredConnection>,
    pub connections_retained: usize,
    pub inflight_failed: Vec<RetiredInflight>,
}

/// An unsolicited frame received from a particular transport generation.
#[derive(Debug)]
pub struct MeshInbound {
    pub peer: String,
    pub connection_generation: uuid::Uuid,
    pub message: BusMessage,
}

/// Holds the transport read fence through the caller's final local enqueue.
pub struct MeshInboundGuard<'a> {
    _state: RwLockReadGuard<'a, TransportState>,
}

/// A connection to a remote broker with send/receive channels.
struct RemoteBroker {
    /// The exact resolved transport target this connection owns. A later
    /// inventory epoch may retain the name while changing its mesh address;
    /// such a call must not reuse this connection.
    target: SocketAddr,
    /// Distinguishes successive connections to the same named target so an old
    /// reader cannot remove a newer cache entry when it finally exits.
    generation: uuid::Uuid,
    tx: mpsc::UnboundedSender<String>,
    connected: Arc<AtomicBool>,
    cancel: watch::Sender<bool>,
}

impl RemoteBroker {
    fn retire(&self) {
        self.connected.store(false, Ordering::Release);
        self.cancel.send_replace(true);
    }
}

struct PendingCall {
    peer: String,
    connection_generation: uuid::Uuid,
    class: String,
    response: oneshot::Sender<std::result::Result<BusMessage, String>>,
}

#[derive(Clone)]
enum AttemptOutcome {
    Pending,
    Complete(std::result::Result<Arc<RemoteBroker>, Arc<str>>),
}

struct ConnectAttempt {
    id: uuid::Uuid,
    target: SocketAddr,
    cancel: watch::Sender<bool>,
    outcome: watch::Receiver<AttemptOutcome>,
}

struct TransportState {
    desired: HashMap<String, SocketAddr>,
    revision: AuthorityRevision,
    connections: HashMap<String, Arc<RemoteBroker>>,
    attempts: HashMap<String, Arc<ConnectAttempt>>,
}

/// Manages connections to remote mesh peers.
///
/// Used by cosmix-noded to bridge messages to remote nodes.
pub struct MeshPeers {
    config: MeshConfig,
    /// Desired endpoints, active connections and shared attempts have one
    /// linearisation point. Network I/O never runs while this lock is held.
    state: Arc<RwLock<TransportState>>,
    /// Pending responses: message_id → oneshot sender for the response
    pending: Arc<Mutex<HashMap<String, PendingCall>>>,
    /// Channel to deliver incoming messages from remote brokers back to local broker
    incoming_tx: mpsc::UnboundedSender<MeshInbound>,
}

impl MeshPeers {
    /// Create a new MeshPeers manager.
    ///
    /// `incoming_tx` receives messages from remote brokers that need to be
    /// delivered to local services.
    pub fn new(config: MeshConfig, incoming_tx: mpsc::UnboundedSender<MeshInbound>) -> Self {
        let desired = config
            .peers
            .iter()
            .map(|peer| (peer.name.clone(), peer.noded_addr()))
            .collect();
        Self {
            config,
            state: Arc::new(RwLock::new(TransportState {
                desired,
                revision: AuthorityRevision::default(),
                connections: HashMap::new(),
                attempts: HashMap::new(),
            })),
            pending: Arc::new(Mutex::new(HashMap::new())),
            incoming_tx,
        }
    }

    /// Get this node's name.
    pub fn node_name(&self) -> &str {
        &self.config.node_name
    }

    /// Atomically install the route authority's desired endpoint projection.
    /// Established transports and in-progress attempts which no longer match
    /// are fenced before this method returns.
    pub async fn reconcile_endpoints(
        &self,
        desired: HashMap<String, SocketAddr>,
        revision: AuthorityRevision,
    ) -> ReconcileReport {
        let mut state = self.state.write().await;
        let mut report = ReconcileReport::default();
        let mut reasons = HashMap::new();

        for (name, old_endpoint) in &state.desired {
            match desired.get(name) {
                None => {
                    report.removed += 1;
                    reasons.insert(name.clone(), ConnectionRetireReason::Removed);
                }
                Some(new_endpoint) if new_endpoint != old_endpoint => {
                    report.endpoint_changed += 1;
                    reasons.insert(
                        name.clone(),
                        ConnectionRetireReason::EndpointChanged {
                            replacement: *new_endpoint,
                        },
                    );
                }
                Some(_) => {}
            }
        }
        report.added = desired
            .keys()
            .filter(|name| !state.desired.contains_key(*name))
            .count();

        for (name, reason) in &reasons {
            if let Some(connection) = state.connections.remove(name) {
                connection.retire();
                report.connections_retired.push(RetiredConnection {
                    peer: name.clone(),
                    endpoint: connection.target,
                    connection_generation: connection.generation,
                    reason: reason.clone(),
                });
            }
            if let Some(attempt) = state.attempts.remove(name) {
                attempt.cancel.send_replace(true);
            }
        }

        state.desired = desired;
        state.revision = revision;
        report.connections_retained = state
            .connections
            .iter()
            .filter(|(name, connection)| {
                state.desired.get(*name) == Some(&connection.target)
                    && connection.connected.load(Ordering::Acquire)
            })
            .count();

        let retired_generations: HashMap<_, _> = report
            .connections_retired
            .iter()
            .map(|retired| {
                (
                    retired.connection_generation,
                    (retired.peer.clone(), retired.endpoint),
                )
            })
            .collect();

        // Sweep pending calls while STILL holding the state write guard: a
        // retired reader's exit path takes the state write lock (in
        // `remove_connection_generation`) before its own pending sweep, so
        // holding the guard here guarantees the reload's "route revoked"
        // verdict — and its `inflight_failed` record — wins that race
        // instead of a generic "connection closed" with no report entry.
        // Lock order state → pending matches every other nesting site.
        if !retired_generations.is_empty() {
            let mut pending = self.pending.lock().await;
            let message_ids: Vec<_> = pending
                .iter()
                .filter(|(_, call)| retired_generations.contains_key(&call.connection_generation))
                .map(|(id, _)| id.clone())
                .collect();
            for message_id in message_ids {
                let Some(call) = pending.remove(&message_id) else {
                    continue;
                };
                report.inflight_failed.push(RetiredInflight {
                    peer: call.peer.clone(),
                    connection_generation: call.connection_generation,
                    message_id,
                    class: call.class,
                });
                let _ = call
                    .response
                    .send(Err("route revoked by endpoint reconciliation".into()));
            }
        }

        report
    }

    /// Acquire the final-delivery fence for a queued inbound frame. `None`
    /// means the frame's connection has been retired or replaced.
    pub async fn validate_inbound(
        &self,
        peer: &str,
        connection_generation: uuid::Uuid,
    ) -> Option<MeshInboundGuard<'_>> {
        let state = self.state.read().await;
        let valid = state.connections.get(peer).is_some_and(|connection| {
            connection.generation == connection_generation
                && connection.connected.load(Ordering::Acquire)
                && state.desired.get(peer) == Some(&connection.target)
        });
        valid.then_some(MeshInboundGuard { _state: state })
    }

    /// Cheap early rejection before doing local registry work. The final
    /// enqueue must still use `validate_inbound` to close the dequeue race.
    pub async fn inbound_generation_is_current(
        &self,
        peer: &str,
        connection_generation: uuid::Uuid,
    ) -> bool {
        self.validate_inbound(peer, connection_generation)
            .await
            .is_some()
    }

    /// Send a message to a remote node's broker. Returns the response.
    ///
    /// The caller supplies the already-authorised, resolved peer. Membership
    /// policy belongs to noded's routing snapshot; this type owns transport and
    /// connection reuse only.
    pub async fn call(&self, peer: PeerConfig, msg: BusMessage) -> Result<BusMessage> {
        let node_name = peer.name.clone();
        let connection = self.ensure_connected(&peer).await?;

        // Set up response channel
        let msg_id = msg
            .get("id")
            .map(|s| s.to_string())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        // Send the message with the id
        let mut msg = msg;
        msg.set("id", &msg_id);
        let class = msg.command_name().unwrap_or("unknown").to_string();
        let (resp_tx, resp_rx) = oneshot::channel();
        self.enqueue_call(
            &peer,
            &connection,
            msg.to_wire(),
            msg_id.clone(),
            class,
            resp_tx,
        )
        .await?;

        // Wait for response with timeout
        match tokio::time::timeout(RESPONSE_TIMEOUT, resp_rx).await {
            Ok(Ok(Ok(resp))) => Ok(resp),
            Ok(Ok(Err(reason))) => anyhow::bail!("Call to {node_name} failed: {reason}"),
            Ok(Err(_)) => {
                self.remove_pending(&msg_id, connection.generation).await;
                anyhow::bail!("Response channel for {node_name} closed")
            }
            Err(_) => {
                self.remove_pending(&msg_id, connection.generation).await;
                anyhow::bail!("Timeout waiting for response from {node_name}")
            }
        }
    }

    /// Send a fire-and-forget message to a remote node.
    pub async fn send(&self, peer: PeerConfig, msg: BusMessage) -> Result<()> {
        let node_name = peer.name.clone();
        let connection = self.ensure_connected(&peer).await?;
        let state = self.state.read().await;
        self.validate_outbound_locked(&state, &peer, &connection)?;
        connection
            .tx
            .send(msg.to_wire())
            .map_err(|_| anyhow::anyhow!("Send to {node_name} failed"))?;

        Ok(())
    }

    async fn enqueue_call(
        &self,
        peer: &PeerConfig,
        connection: &Arc<RemoteBroker>,
        wire: String,
        message_id: String,
        class: String,
        response: oneshot::Sender<std::result::Result<BusMessage, String>>,
    ) -> Result<()> {
        let state = self.state.read().await;
        self.validate_outbound_locked(&state, peer, connection)?;
        let mut pending = self.pending.lock().await;
        if pending.contains_key(&message_id) {
            anyhow::bail!("Duplicate pending message id {message_id}");
        }
        pending.insert(
            message_id.clone(),
            PendingCall {
                peer: peer.name.clone(),
                connection_generation: connection.generation,
                class,
                response,
            },
        );
        if connection.tx.send(wire).is_err() {
            pending.remove(&message_id);
            anyhow::bail!("Send to {} failed", peer.name);
        }
        Ok(())
    }

    fn validate_outbound_locked(
        &self,
        state: &TransportState,
        peer: &PeerConfig,
        connection: &Arc<RemoteBroker>,
    ) -> Result<()> {
        let target = peer.noded_addr();
        let current = state.connections.get(&peer.name);
        if state.desired.get(&peer.name) != Some(&target)
            || current.is_none_or(|cached| cached.generation != connection.generation)
            || connection.target != target
            || !connection.connected.load(Ordering::Acquire)
        {
            anyhow::bail!("Route to {} is no longer authorised", peer.name);
        }
        Ok(())
    }

    async fn remove_pending(&self, message_id: &str, generation: uuid::Uuid) {
        // Take the state lock (read) before pending: during a reload's
        // critical section this serialises the caller's own timeout cleanup
        // AFTER reconcile's sweep, so a call retired at the fence always
        // gets its `inflight_failed` record even when its RESPONSE_TIMEOUT
        // expires in the same instant (the sweep's send to the caller's
        // already-dropped oneshot is harmless). Lock order state → pending,
        // as at every other nesting site.
        let _state = self.state.read().await;
        let mut pending = self.pending.lock().await;
        if pending
            .get(message_id)
            .is_some_and(|call| call.connection_generation == generation)
        {
            pending.remove(message_id);
        }
    }

    /// Ensure we have an active WebSocket connection to a peer's broker.
    async fn ensure_connected(&self, peer: &PeerConfig) -> Result<Arc<RemoteBroker>> {
        let target = peer.noded_addr();
        if let Some(connection) = self.cached_connection(peer).await {
            return Ok(connection);
        }

        let attempt = {
            let mut state = self.state.write().await;
            if state.desired.get(&peer.name) != Some(&target) {
                anyhow::bail!("Route to {} is no longer authorised", peer.name);
            }
            if let Some(connection) = state.connections.get(&peer.name).filter(|connection| {
                connection.target == target && connection.connected.load(Ordering::Acquire)
            }) {
                return Ok(connection.clone());
            }
            if let Some(attempt) = state
                .attempts
                .get(&peer.name)
                .filter(|attempt| attempt.target == target)
            {
                attempt.clone()
            } else {
                if let Some(stale) = state.attempts.remove(&peer.name) {
                    stale.cancel.send_replace(true);
                }
                let id = uuid::Uuid::new_v4();
                let (cancel, cancel_rx) = watch::channel(false);
                let (outcome_tx, outcome) = watch::channel(AttemptOutcome::Pending);
                let attempt = Arc::new(ConnectAttempt {
                    id,
                    target,
                    cancel,
                    outcome,
                });
                state.attempts.insert(peer.name.clone(), attempt.clone());
                self.spawn_connect_attempt(peer.clone(), id, cancel_rx, outcome_tx);
                attempt
            }
        };
        let result = wait_for_attempt(attempt.outcome.clone()).await;
        if result.is_err() {
            // A driver that died without publishing an outcome (panic/abort)
            // leaves its map entry behind, and every future caller would join
            // the corpse forever — reconcile only clears attempts for
            // removed/changed endpoints. Sweep it (id-guarded, so a fresh
            // replacement attempt is never touched); on a normally-failed
            // attempt this just races the driver's own idempotent cleanup.
            let mut state = self.state.write().await;
            if state
                .attempts
                .get(&peer.name)
                .is_some_and(|current| current.id == attempt.id)
            {
                state.attempts.remove(&peer.name);
            }
        }
        result
    }

    fn spawn_connect_attempt(
        &self,
        peer: PeerConfig,
        attempt_id: uuid::Uuid,
        cancel: watch::Receiver<bool>,
        outcome: watch::Sender<AttemptOutcome>,
    ) {
        let node_name = self.config.node_name.clone();
        let d2_seed = self.config.d2_seed;
        let state = self.state.clone();
        let pending = self.pending.clone();
        let incoming_tx = self.incoming_tx.clone();
        tokio::spawn(async move {
            let result = connect_and_publish(
                peer.clone(),
                attempt_id,
                cancel,
                node_name,
                d2_seed,
                state.clone(),
                pending,
                incoming_tx,
            )
            .await
            .map_err(|error| Arc::<str>::from(error.to_string()));
            outcome.send_replace(AttemptOutcome::Complete(result));
            let mut state = state.write().await;
            if state
                .attempts
                .get(&peer.name)
                .is_some_and(|attempt| attempt.id == attempt_id)
            {
                state.attempts.remove(&peer.name);
            }
        });
    }

    async fn cached_connection(&self, peer: &PeerConfig) -> Option<Arc<RemoteBroker>> {
        let target = peer.noded_addr();
        let state = self.state.read().await;
        if state.desired.get(&peer.name) != Some(&target) {
            return None;
        }
        state
            .connections
            .get(&peer.name)
            .filter(|connection| {
                connection.target == target && connection.connected.load(Ordering::Acquire)
            })
            .cloned()
    }
}

async fn wait_for_attempt(
    mut outcome: watch::Receiver<AttemptOutcome>,
) -> Result<Arc<RemoteBroker>> {
    loop {
        match outcome.borrow().clone() {
            AttemptOutcome::Pending => {}
            AttemptOutcome::Complete(Ok(connection)) => return Ok(connection),
            AttemptOutcome::Complete(Err(error)) => anyhow::bail!(error.to_string()),
        }
        if outcome.changed().await.is_err() {
            anyhow::bail!("Shared connection attempt ended without a result");
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn connect_and_publish(
    peer: PeerConfig,
    attempt_id: uuid::Uuid,
    mut attempt_cancel: watch::Receiver<bool>,
    node_name: String,
    d2_seed: Option<[u8; 32]>,
    state: Arc<RwLock<TransportState>>,
    pending: Arc<Mutex<HashMap<String, PendingCall>>>,
    incoming_tx: mpsc::UnboundedSender<MeshInbound>,
) -> Result<Arc<RemoteBroker>> {
    let target = peer.noded_addr();
    let url = peer.noded_url();
    tracing::info!(peer = %peer.name, url = %url, "Connecting to remote broker");

    let connect = tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(&url));
    let (ws_stream, _) = tokio::select! {
        biased;
        changed = attempt_cancel.changed() => {
            let _ = changed;
            anyhow::bail!("Route to {} was revoked during connection", peer.name);
        }
        result = connect => match result {
            Err(_) => {
                tracing::warn!(
                    event = "mesh.connection.failed",
                    cause = "connect-timeout",
                    peer = %peer.name,
                    endpoint = %target,
                    timeout_ms = CONNECT_TIMEOUT.as_millis() as u64,
                );
                anyhow::bail!("Connection to {} timed out after {}ms", peer.name, CONNECT_TIMEOUT.as_millis());
            }
            Ok(Err(error)) => anyhow::bail!("Failed to connect to {}: {error}", peer.name),
            Ok(Ok(connected)) => connected,
        }
    };

    let (mut ws_sink, mut ws_stream_rx) = ws_stream.split();
    let mut early_inbound = None;

    // SPEC 13 §9a (2-c-1c) — D2 admission prover. A 2-c broker's FIRST frame
    // is a `noded.admit.challenge`; respond with a signed transcript BEFORE
    // registering. A pre-2c/`off` broker sends nothing first → the bounded
    // read times out and we register-first (back-compat). Only an exact
    // challenge command is consumed here; any other first frame is delivered
    // to the normal inbound path so it is not lost. A late challenge (after
    // the timeout) is handled harmlessly by the reader (delivered, ignored)
    // — the session just goes unproven (the broker logs `no-proof`).
    let first_frame = tokio::select! {
        biased;
        changed = attempt_cancel.changed() => {
            let _ = changed;
            anyhow::bail!("Route to {} was revoked during admission", peer.name);
        }
        frame = tokio::time::timeout(ADMISSION_CHALLENGE_TIMEOUT, ws_stream_rx.next()) => frame,
    };
    match first_frame {
        // Timed out — a silent (pre-2c / `off`) broker. The connection is
        // alive; fall through and register-first (back-compat).
        Err(_) => {}
        // Closed / EOF / ws error before any frame — the connection is dead;
        // return WITHOUT storing a dead `RemoteBroker` (the next
        // `ensure_connected` reconnects).
        Ok(None) | Ok(Some(Err(_))) | Ok(Some(Ok(tungstenite::Message::Close(_)))) => {
            anyhow::bail!("Connection to {} closed during admission", peer.name);
        }
        Ok(Some(Ok(tungstenite::Message::Text(t)))) => {
            if let Ok(bus_msg) = bus::parse(&t) {
                if bus_msg.command_name() == Some("noded.admit.challenge") {
                    if let Some(resp_wire) = build_admit_response(&bus_msg, &node_name, &d2_seed) {
                        // A send failure here means the connection died —
                        // bail rather than store it. The write is bounded and
                        // cancellable like every other await on this path: a
                        // zero-window peer must not wedge the shared attempt
                        // (and every joined caller) past the connect budget.
                        let sent = tokio::select! {
                            biased;
                            changed = attempt_cancel.changed() => {
                                let _ = changed;
                                anyhow::bail!("Route to {} was revoked during admission response", peer.name);
                            }
                            result = tokio::time::timeout(
                                CONNECT_TIMEOUT,
                                ws_sink.send(tungstenite::Message::Text(resp_wire.into())),
                            ) => result,
                        };
                        match sent {
                            Err(_) => anyhow::bail!(
                                "Admission response to {} timed out after {}ms",
                                peer.name,
                                CONNECT_TIMEOUT.as_millis()
                            ),
                            Ok(Err(_)) => anyhow::bail!(
                                "Connection to {} closed during admission response",
                                peer.name
                            ),
                            Ok(Ok(())) => {}
                        }
                    }
                } else {
                    // Not a challenge — deliver as a normal inbound frame.
                    early_inbound = Some(bus_msg);
                }
            }
        }
        // A non-text first frame (ping/pong/binary) — not a challenge, not a
        // close; proceed to register-first.
        Ok(Some(Ok(_))) => {}
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let connected = Arc::new(AtomicBool::new(true));
    let generation = uuid::Uuid::new_v4();
    let (connection_cancel, mut writer_cancel) = watch::channel(false);
    let mut reader_cancel = connection_cancel.subscribe();
    let connection = Arc::new(RemoteBroker {
        target,
        generation,
        tx,
        connected: connected.clone(),
        cancel: connection_cancel,
    });

    let register_msg = BusMessage::new()
        .with_header("command", "noded.register")
        .with_header("from", &format!("bridge-{node_name}"))
        .with_header("to", "noded");
    tokio::select! {
        biased;
        changed = attempt_cancel.changed() => {
            let _ = changed;
            anyhow::bail!("Route to {} was revoked before registration", peer.name);
        }
        result = tokio::time::timeout(
            CONNECT_TIMEOUT,
            ws_sink.send(tungstenite::Message::Text(register_msg.to_wire().into())),
        ) => {
            match result {
                Err(_) => anyhow::bail!(
                    "Registration to {} timed out after {}ms",
                    peer.name,
                    CONNECT_TIMEOUT.as_millis()
                ),
                Ok(sent) => sent.map_err(|error| anyhow::anyhow!("Connection to {} closed during registration: {error}", peer.name))?,
            }
        }
    }

    {
        let mut locked = state.write().await;
        let still_current = locked.desired.get(&peer.name) == Some(&target)
            && locked
                .attempts
                .get(&peer.name)
                .is_some_and(|attempt| attempt.id == attempt_id && attempt.target == target)
            && !*attempt_cancel.borrow();
        if !still_current {
            anyhow::bail!(
                "Route to {} changed before connection publication",
                peer.name
            );
        }
        locked
            .connections
            .insert(peer.name.clone(), connection.clone());
    }

    if let Some(message) = early_inbound {
        let _ = incoming_tx.send(MeshInbound {
            peer: peer.name.clone(),
            connection_generation: generation,
            message,
        });
    }

    let connected_send = connected.clone();
    tokio::spawn(async move {
        let mut cancelled = false;
        loop {
            tokio::select! {
                biased;
                changed = writer_cancel.changed() => {
                    let _ = changed;
                    cancelled = true;
                    break;
                }
                next = rx.recv() => {
                    let Some(message) = next else { break };
                    // The flush itself must also be cancellable: §7.7 says
                    // nothing is written to a retired endpoint after
                    // `routing.reload_applied`, and a backpressured peer can
                    // hold this await open long past the reload. A cancelled
                    // partial write is fine — teardown drops the socket. (A
                    // frame whose send completed in the same poll in which
                    // cancel was not yet observable can still land — the
                    // one-frame slip inherent to cooperative cancellation,
                    // covered by §7.7's kernel-visible allowance.)
                    let flushed = tokio::select! {
                        biased;
                        changed = writer_cancel.changed() => {
                            let _ = changed;
                            cancelled = true;
                            break;
                        }
                        result = ws_sink.send(tungstenite::Message::Text(message.into())) => result,
                    };
                    if flushed.is_err() {
                        break;
                    }
                }
            }
        }
        connected_send.store(false, Ordering::Release);
        // On cancel (retirement), DROP the sink instead of closing it: a
        // graceful WebSocket close first flushes userspace-buffered frames
        // (a §7.7 fence violation on a retired endpoint) and then waits for
        // the peer's close reply — a zero-window peer would park this task
        // and hold the socket forever, leaking one task per wedged
        // retirement. Dropping both split halves (the reader also breaks on
        // cancel) closes the TCP socket without flushing. On a natural exit
        // the polite close is still attempted, but bounded — never parked
        // on a wedged peer.
        if !cancelled {
            let _ = tokio::time::timeout(CONNECT_TIMEOUT, ws_sink.close()).await;
        }
    });

    let peer_name = peer.name.clone();
    let connected_read = connected.clone();
    let state_read = state.clone();
    tokio::spawn(async move {
        loop {
            let next = tokio::select! {
                biased;
                changed = reader_cancel.changed() => {
                    let _ = changed;
                    break;
                }
                next = ws_stream_rx.next() => next,
            };
            let Some(Ok(msg)) = next else { break };
            let text = match msg {
                tungstenite::Message::Text(t) => t.to_string(),
                tungstenite::Message::Close(_) => break,
                _ => continue,
            };

            let Ok(bus_msg) = bus::parse(&text) else {
                continue;
            };

            // Check if this is a response to a pending request
            if let Some(id) = bus_msg.get("id") {
                let mut calls = pending.lock().await;
                if calls
                    .get(id)
                    .is_some_and(|call| call.connection_generation == generation)
                {
                    let call = calls.remove(id).expect("pending call checked above");
                    let _ = call.response.send(Ok(bus_msg));
                    continue;
                }
            }

            let _ = incoming_tx.send(MeshInbound {
                peer: peer_name.clone(),
                connection_generation: generation,
                message: bus_msg,
            });
        }

        connected_read.store(false, Ordering::Release);
        remove_connection_generation(&state_read, &peer_name, generation).await;
        fail_pending_generation(&pending, generation, "connection closed").await;
        tracing::info!(peer = %peer_name, "Remote broker disconnected");
    });

    tracing::info!(peer = %peer.name, target = %target, "Connected to remote broker");
    Ok(connection)
}

async fn remove_connection_generation(
    state: &RwLock<TransportState>,
    peer_name: &str,
    generation: uuid::Uuid,
) {
    let mut state = state.write().await;
    if state
        .connections
        .get(peer_name)
        .is_some_and(|connection| connection.generation == generation)
    {
        state.connections.remove(peer_name);
    }
}

async fn fail_pending_generation(
    pending: &Mutex<HashMap<String, PendingCall>>,
    generation: uuid::Uuid,
    reason: &str,
) {
    let mut pending = pending.lock().await;
    let ids: Vec<_> = pending
        .iter()
        .filter(|(_, call)| call.connection_generation == generation)
        .map(|(id, _)| id.clone())
        .collect();
    for id in ids {
        if let Some(call) = pending.remove(&id) {
            let _ = call.response.send(Err(reason.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use tokio::net::TcpListener;

    #[test]
    fn public_default_port_reexport_survives() {
        assert_eq!(crate::DEFAULT_NODED_PORT, 4200);
        assert_eq!(crate::DEFAULT_NODED_PORT, DEFAULT_NODED_PORT);
    }

    /// SPEC 13 §9a (2-c-1c) — the prover. No seed ⇒ `None` (register without
    /// proving). With a seed ⇒ a `noded.admit.response` whose `from` is the
    /// `bridge-<node>` label but whose `claimed_source_node` is the BARE name
    /// (the split `admit()`'s NameMismatch depends on).
    #[test]
    fn build_admit_response_shape_and_missing_seed() {
        let b64 = base64::engine::general_purpose::STANDARD;
        let mut ch = BusMessage::new()
            .with_header("command", "noded.admit.challenge")
            .with_header("id", "adm-1");
        ch.body = serde_json::json!({
            "mesh_fqdn": "bus",
            "verifying_broker_node": "beta",
            "inventory_epoch": 7,
            "session_id": b64.encode([1u8; 16]),
            "server_nonce": b64.encode([2u8; 32]),
        })
        .to_string();

        // No seed → prover-incapable → None.
        assert!(build_admit_response(&ch, "delta", &None).is_none());

        // With a seed → a well-shaped response.
        let wire = build_admit_response(&ch, "delta", &Some([9u8; 32])).expect("response");
        let resp = bus::parse(&wire).expect("parse");
        assert_eq!(resp.command_name(), Some("noded.admit.response"));
        assert_eq!(
            resp.get("from"),
            Some("bridge-delta"),
            "label keeps bridge- prefix"
        );
        let body: serde_json::Value = serde_json::from_str(&resp.body).unwrap();
        assert_eq!(
            body["claimed_source_node"], "delta",
            "bare node name, not bridge-"
        );
        assert_eq!(body["signed_epoch"], 7);
        // origin-only zeros + a 64-byte signature.
        assert_eq!(
            b64.decode(body["signature"].as_str().unwrap())
                .unwrap()
                .len(),
            64
        );
        assert_eq!(
            b64.decode(body["client_ephemeral"].as_str().unwrap())
                .unwrap(),
            vec![0u8; 32]
        );
    }

    // `.conf.mix` map literals separate entries with `,` (newlines are
    // suppressed inside `{ }`); the top-level body uses newlines.

    #[test]
    fn conf_mix_parses_peers_and_default_port() {
        let src = r#"
            node_name: "alpha"
            peers: [
              { name: "beta", mesh_ip: "192.0.2.2" },
              { name: "gamma", mesh_ip: "192.0.2.3", noded_port: 4300 }
            ]
        "#;
        let cfg: MeshConfig = cosmix_config::from_conf_mix_str(src).expect("parse");
        assert_eq!(cfg.node_name, "alpha");
        assert_eq!(cfg.peers.len(), 2);
        // `noded_port` defaults to 4200 when omitted.
        assert_eq!(cfg.find_peer("beta").unwrap().noded_port, 4200);
        assert_eq!(cfg.find_peer("gamma").unwrap().noded_port, 4300);
        assert_eq!(
            cfg.find_peer("gamma").unwrap().noded_url(),
            "ws://192.0.2.3:4300/ws"
        );
    }

    #[test]
    fn deny_unknown_fields_rejects_stale_hub_port() {
        // The exact regression PeerConfig's deny_unknown_fields guards:
        // a peer entry still using the pre-rename `hub_port` key must
        // error, not silently default `noded_port` to 4200 and connect
        // to the wrong port on a node that customized it.
        let src = r#"
            node_name: "alpha"
            peers: [ { name: "beta", mesh_ip: "192.0.2.2", hub_port: 9999 } ]
        "#;
        let err = cosmix_config::from_conf_mix_str::<MeshConfig>(src).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("unknown field") && msg.contains("hub_port"),
            "expected unknown-field error naming hub_port, got: {msg}"
        );
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("cosmix-mesh-cfg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn load_explicit_conf_mix_path() {
        let dir = temp_dir("explicit");
        let path = dir.join("mesh.conf.mix");
        std::fs::write(
            &path,
            "node_name: \"alpha\"\npeers: [ { name: \"beta\", mesh_ip: \"192.0.2.2\" } ]\n",
        )
        .unwrap();
        let cfg = MeshConfig::load(&path.to_string_lossy()).expect("load");
        assert_eq!(cfg.node_name, "alpha");
        assert_eq!(cfg.peers.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_path_is_error() {
        assert!(MeshConfig::load("/nonexistent/cosmix/mesh.conf.mix").is_err());
    }

    #[test]
    fn hostname_in_mesh_ip_is_rejected() {
        // The whole point of `mesh_ip: IpAddr`. A hostname here would be
        // interpolated into the broker's ws:// URL and resolved by
        // `connect_async`, silently making DNS the mesh routing
        // authority. It must fail at deserialize, not at connect.
        let src = r#"
            node_name: "alpha"
            peers: [ { name: "beta", mesh_ip: "noded.beta.example.internal" } ]
        "#;
        let err = cosmix_config::from_conf_mix_str::<MeshConfig>(src).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("invalid IP address syntax"),
            "expected the IpAddr parse to reject a hostname, got: {msg}"
        );
    }

    #[test]
    fn ipv6_peer_url_is_bracketed() {
        // Direct `format!("ws://{}:{}", ip, port)` would emit
        // `ws://::1:4200`, which is not a valid URL. SocketAddr brackets
        // it. The fleet is IPv4-only today, so only a test pins this.
        let src = r#"
            node_name: "alpha"
            peers: [ { name: "beta", mesh_ip: "2001:db8::1", noded_port: 4300 } ]
        "#;
        let cfg: MeshConfig = cosmix_config::from_conf_mix_str(src).expect("parse");
        assert_eq!(
            cfg.find_peer("beta").unwrap().noded_url(),
            "ws://[2001:db8::1]:4300/ws"
        );
    }

    fn test_mesh() -> MeshPeers {
        let (incoming_tx, _incoming_rx) = mpsc::unbounded_channel();
        MeshPeers::new(
            MeshConfig {
                node_name: "alpha".into(),
                peers: vec![],
                d2_seed: None,
            },
            incoming_tx,
        )
    }

    fn peer(name: &str, endpoint: SocketAddr) -> PeerConfig {
        PeerConfig {
            name: name.into(),
            mesh_ip: endpoint.ip(),
            noded_port: endpoint.port(),
        }
    }

    fn test_mesh_with_peer(peer: PeerConfig) -> (MeshPeers, mpsc::UnboundedReceiver<MeshInbound>) {
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
        (
            MeshPeers::new(
                MeshConfig {
                    node_name: "alpha".into(),
                    peers: vec![peer],
                    d2_seed: None,
                },
                incoming_tx,
            ),
            incoming_rx,
        )
    }

    fn test_connection(target: SocketAddr) -> Arc<RemoteBroker> {
        let (tx, _rx) = mpsc::unbounded_channel();
        let (cancel, _cancel_rx) = watch::channel(false);
        Arc::new(RemoteBroker {
            target,
            generation: uuid::Uuid::new_v4(),
            tx,
            connected: Arc::new(AtomicBool::new(true)),
            cancel,
        })
    }

    #[tokio::test]
    async fn dead_attempt_is_swept_so_the_next_caller_dials_fresh() {
        // A connect driver that dies without publishing an outcome (panic or
        // abort) drops its `outcome` sender with the map entry still in
        // place. Joiners must not inherit that corpse forever: the first
        // caller reports the dead attempt, and the NEXT caller dials fresh.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = listener.local_addr().unwrap();
        drop(listener); // closed port → a fresh dial fails with refused, fast
        let beta = peer("beta", endpoint);
        let (mesh, _incoming_rx) = test_mesh_with_peer(beta.clone());
        mesh.reconcile_endpoints(
            HashMap::from([("beta".to_string(), endpoint)]),
            AuthorityRevision::default(),
        )
        .await;
        {
            let mut state = mesh.state.write().await;
            let (cancel, _cancel_rx) = watch::channel(false);
            let (outcome_tx, outcome) = watch::channel(AttemptOutcome::Pending);
            drop(outcome_tx); // the driver died before send_replace
            state.attempts.insert(
                "beta".into(),
                Arc::new(ConnectAttempt {
                    id: uuid::Uuid::new_v4(),
                    target: endpoint,
                    cancel,
                    outcome,
                }),
            );
        }

        let first = mesh
            .call(beta.clone(), BusMessage::new())
            .await
            .expect_err("dead attempt must error");
        assert!(
            first.to_string().contains("ended without a result"),
            "first caller reports the dead attempt: {first}"
        );
        assert!(
            !mesh.state.read().await.attempts.contains_key("beta"),
            "the dead attempt entry must be swept"
        );
        let second = mesh
            .call(beta, BusMessage::new())
            .await
            .expect_err("closed port must refuse");
        assert!(
            !second.to_string().contains("ended without a result"),
            "second caller must dial fresh, not join the corpse: {second}"
        );
    }

    #[tokio::test]
    async fn cache_reuses_only_the_same_resolved_endpoint() {
        let mesh = test_mesh();
        let original = PeerConfig {
            name: "beta".into(),
            mesh_ip: "192.0.2.2".parse().unwrap(),
            noded_port: DEFAULT_NODED_PORT,
        };
        let connection = test_connection(original.noded_addr());
        {
            let mut state = mesh.state.write().await;
            state
                .desired
                .insert(original.name.clone(), original.noded_addr());
            state
                .connections
                .insert(original.name.clone(), connection.clone());
        }

        let reused = mesh.cached_connection(&original).await.unwrap();
        assert!(Arc::ptr_eq(&reused, &connection));

        let changed_ip = PeerConfig {
            mesh_ip: "192.0.2.3".parse().unwrap(),
            ..original.clone()
        };
        assert!(mesh.cached_connection(&changed_ip).await.is_none());

        let changed_port = PeerConfig {
            noded_port: 4300,
            ..original.clone()
        };
        assert!(mesh.cached_connection(&changed_port).await.is_none());
        assert!(Arc::ptr_eq(
            &mesh.cached_connection(&original).await.unwrap(),
            &connection
        ));
    }

    #[tokio::test]
    async fn disconnected_cache_entry_is_not_reused() {
        let mesh = test_mesh();
        let peer = PeerConfig {
            name: "beta".into(),
            mesh_ip: "192.0.2.2".parse().unwrap(),
            noded_port: DEFAULT_NODED_PORT,
        };
        let connection = test_connection(peer.noded_addr());
        connection.connected.store(false, Ordering::Release);
        {
            let mut state = mesh.state.write().await;
            state.desired.insert(peer.name.clone(), peer.noded_addr());
            state.connections.insert(peer.name.clone(), connection);
        }

        assert!(mesh.cached_connection(&peer).await.is_none());
    }

    #[tokio::test]
    async fn same_endpoint_survives_revision_change_with_same_generation() {
        let mesh = test_mesh();
        let endpoint: SocketAddr = "192.0.2.2:4200".parse().unwrap();
        let connection = test_connection(endpoint);
        {
            let mut state = mesh.state.write().await;
            state.desired.insert("beta".into(), endpoint);
            state.connections.insert("beta".into(), connection.clone());
        }

        let report = mesh
            .reconcile_endpoints(
                HashMap::from([("beta".into(), endpoint)]),
                AuthorityRevision {
                    epoch: 8,
                    recovery_generation: 2,
                },
            )
            .await;

        assert_eq!(
            report,
            ReconcileReport {
                connections_retained: 1,
                ..ReconcileReport::default()
            }
        );
        assert_eq!(
            mesh.state
                .read()
                .await
                .connections
                .get("beta")
                .unwrap()
                .generation,
            connection.generation
        );
    }

    #[tokio::test]
    async fn removed_and_changed_endpoints_retire_only_their_connections() {
        let mesh = test_mesh();
        let beta_old: SocketAddr = "192.0.2.2:4200".parse().unwrap();
        let beta_new: SocketAddr = "192.0.2.22:4200".parse().unwrap();
        let gamma: SocketAddr = "192.0.2.3:4200".parse().unwrap();
        let delta: SocketAddr = "192.0.2.4:4200".parse().unwrap();
        let beta_connection = test_connection(beta_old);
        let gamma_connection = test_connection(gamma);
        let delta_connection = test_connection(delta);
        {
            let mut state = mesh.state.write().await;
            state.desired = HashMap::from([
                ("beta".into(), beta_old),
                ("gamma".into(), gamma),
                ("delta".into(), delta),
            ]);
            state.connections = HashMap::from([
                ("beta".into(), beta_connection.clone()),
                ("gamma".into(), gamma_connection.clone()),
                ("delta".into(), delta_connection.clone()),
            ]);
        }

        let report = mesh
            .reconcile_endpoints(
                HashMap::from([("beta".into(), beta_new), ("gamma".into(), gamma)]),
                AuthorityRevision {
                    epoch: 9,
                    recovery_generation: 2,
                },
            )
            .await;

        assert_eq!(report.added, 0);
        assert_eq!(report.removed, 1);
        assert_eq!(report.endpoint_changed, 1);
        assert_eq!(report.connections_retired.len(), 2);
        assert_eq!(report.connections_retained, 1);
        assert!(!beta_connection.connected.load(Ordering::Acquire));
        assert!(!delta_connection.connected.load(Ordering::Acquire));
        assert!(gamma_connection.connected.load(Ordering::Acquire));
        let state = mesh.state.read().await;
        assert_eq!(state.connections.len(), 1);
        assert_eq!(
            state.connections.get("gamma").unwrap().generation,
            gamma_connection.generation
        );
    }

    #[tokio::test]
    async fn stale_caller_cannot_enqueue_or_reconnect_after_reconcile() {
        let endpoint: SocketAddr = "192.0.2.2:4200".parse().unwrap();
        let beta = peer("beta", endpoint);
        let (mesh, _incoming) = test_mesh_with_peer(beta.clone());
        let captured = test_connection(endpoint);
        mesh.state
            .write()
            .await
            .connections
            .insert("beta".into(), captured.clone());

        mesh.reconcile_endpoints(HashMap::new(), AuthorityRevision::default())
            .await;

        let state = mesh.state.read().await;
        assert!(
            mesh.validate_outbound_locked(&state, &beta, &captured)
                .is_err()
        );
        drop(state);
        assert!(
            mesh.send(beta, BusMessage::new())
                .await
                .unwrap_err()
                .to_string()
                .contains("no longer authorised")
        );
    }

    #[tokio::test]
    async fn stale_queued_inbound_is_rejected_at_dequeue() {
        let endpoint: SocketAddr = "192.0.2.2:4200".parse().unwrap();
        let beta = peer("beta", endpoint);
        let (mesh, _incoming) = test_mesh_with_peer(beta);
        let connection = test_connection(endpoint);
        mesh.state
            .write()
            .await
            .connections
            .insert("beta".into(), connection.clone());
        let inbound = MeshInbound {
            peer: "beta".into(),
            connection_generation: connection.generation,
            message: BusMessage::new(),
        };
        assert!(
            mesh.inbound_generation_is_current(&inbound.peer, inbound.connection_generation)
                .await
        );

        mesh.reconcile_endpoints(HashMap::new(), AuthorityRevision::default())
            .await;

        assert!(
            !mesh
                .inbound_generation_is_current(&inbound.peer, inbound.connection_generation)
                .await
        );
        assert!(
            mesh.validate_inbound(&inbound.peer, inbound.connection_generation)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn pending_rpc_is_failed_once_and_reported() {
        let endpoint: SocketAddr = "192.0.2.2:4200".parse().unwrap();
        let beta = peer("beta", endpoint);
        let (mesh, _incoming) = test_mesh_with_peer(beta);
        let connection = test_connection(endpoint);
        mesh.state
            .write()
            .await
            .connections
            .insert("beta".into(), connection.clone());
        let (response, receive) = oneshot::channel();
        mesh.pending.lock().await.insert(
            "rpc-1".into(),
            PendingCall {
                peer: "beta".into(),
                connection_generation: connection.generation,
                class: "props.get".into(),
                response,
            },
        );

        let report = mesh
            .reconcile_endpoints(HashMap::new(), AuthorityRevision::default())
            .await;
        assert_eq!(report.inflight_failed.len(), 1);
        assert_eq!(report.inflight_failed[0].message_id, "rpc-1");
        assert!(receive.await.unwrap().unwrap_err().contains("revoked"));

        let second = mesh
            .reconcile_endpoints(HashMap::new(), AuthorityRevision::default())
            .await;
        assert!(second.inflight_failed.is_empty());
    }

    #[tokio::test]
    async fn empty_desired_set_retires_every_connection() {
        let mesh = test_mesh();
        let beta: SocketAddr = "192.0.2.2:4200".parse().unwrap();
        let gamma: SocketAddr = "192.0.2.3:4200".parse().unwrap();
        {
            let mut state = mesh.state.write().await;
            state.desired = HashMap::from([("beta".into(), beta), ("gamma".into(), gamma)]);
            state.connections = HashMap::from([
                ("beta".into(), test_connection(beta)),
                ("gamma".into(), test_connection(gamma)),
            ]);
        }

        let report = mesh
            .reconcile_endpoints(HashMap::new(), AuthorityRevision::default())
            .await;
        assert_eq!(report.removed, 2);
        assert_eq!(report.connections_retired.len(), 2);
        assert!(mesh.state.read().await.connections.is_empty());
    }

    async fn spawn_open_websocket() -> (
        SocketAddr,
        oneshot::Receiver<()>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = listener.local_addr().unwrap();
        let (closed_tx, closed_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            websocket
                .send(tungstenite::Message::Ping(Vec::new().into()))
                .await
                .unwrap();
            assert!(websocket.next().await.is_some(), "registration frame");
            while websocket.next().await.is_some() {}
            let _ = closed_tx.send(());
        });
        (endpoint, closed_rx, task)
    }

    #[tokio::test]
    async fn retirement_stops_transport_tasks_and_closes_websocket() {
        let (endpoint, closed, server) = spawn_open_websocket().await;
        let beta = peer("beta", endpoint);
        let (mesh, _incoming) = test_mesh_with_peer(beta.clone());
        mesh.ensure_connected(&beta).await.unwrap();

        let report = mesh
            .reconcile_endpoints(HashMap::new(), AuthorityRevision::default())
            .await;
        assert_eq!(report.connections_retired.len(), 1);
        tokio::time::timeout(Duration::from_secs(1), closed)
            .await
            .expect("server observed transport closure")
            .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn late_connect_cannot_publish_after_reconcile() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = listener.local_addr().unwrap();
        let beta = peer("beta", endpoint);
        let (mesh, _incoming) = test_mesh_with_peer(beta.clone());
        let mesh = Arc::new(mesh);
        let accepted = Arc::new(tokio::sync::Notify::new());
        let accepted_server = accepted.clone();
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            accepted_server.notify_one();
            tokio::time::sleep(Duration::from_secs(2)).await;
        });
        let caller_mesh = mesh.clone();
        let caller = tokio::spawn(async move { caller_mesh.ensure_connected(&beta).await });
        accepted.notified().await;

        mesh.reconcile_endpoints(HashMap::new(), AuthorityRevision::default())
            .await;
        assert!(caller.await.unwrap().is_err());
        let state = mesh.state.read().await;
        assert!(state.connections.is_empty());
        assert!(state.attempts.is_empty());
        drop(state);
        server.abort();
    }

    #[tokio::test]
    async fn shared_attempt_timeout_fans_out_once() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = listener.local_addr().unwrap();
        let beta = peer("beta", endpoint);
        let (mesh, _incoming) = test_mesh_with_peer(beta.clone());
        let mesh = Arc::new(mesh);
        let accepted = Arc::new(AtomicUsize::new(0));
        let accepted_server = accepted.clone();
        let server = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok(Ok((stream, _))) =
                tokio::time::timeout(Duration::from_millis(5500), listener.accept()).await
            {
                accepted_server.fetch_add(1, Ordering::SeqCst);
                held.push(stream);
            }
            held
        });

        let started = tokio::time::Instant::now();
        let mut callers = Vec::new();
        for _ in 0..8 {
            let mesh = mesh.clone();
            let beta = beta.clone();
            callers.push(tokio::spawn(async move {
                mesh.ensure_connected(&beta)
                    .await
                    .err()
                    .expect("connection attempt must time out")
                    .to_string()
            }));
        }
        let mut errors = Vec::new();
        for caller in callers {
            errors.push(caller.await.unwrap());
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(4900),
            "elapsed {elapsed:?}"
        );
        assert!(elapsed < Duration::from_secs(6), "elapsed {elapsed:?}");
        assert!(errors.iter().all(|error| error == &errors[0]));
        assert!(errors[0].contains("timed out"));
        assert_eq!(accepted.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[tokio::test]
    async fn old_reader_generation_cannot_remove_replacement() {
        let target = SocketAddr::new("192.0.2.2".parse().unwrap(), DEFAULT_NODED_PORT);
        let old = test_connection(target);
        let replacement = test_connection(target);
        let state = RwLock::new(TransportState {
            desired: HashMap::from([("beta".into(), target)]),
            revision: AuthorityRevision::default(),
            connections: HashMap::from([("beta".into(), replacement.clone())]),
            attempts: HashMap::new(),
        });

        remove_connection_generation(&state, "beta", old.generation).await;
        assert_eq!(
            state
                .read()
                .await
                .connections
                .get("beta")
                .unwrap()
                .generation,
            replacement.generation
        );

        remove_connection_generation(&state, "beta", replacement.generation).await;
        assert!(!state.read().await.connections.contains_key("beta"));
    }
}
