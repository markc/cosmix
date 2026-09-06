//! noded module — WebSocket message broker for the cosmix appmesh.
//!
//! Routes Bus messages between local services and bridges to remote
//! mesh nodes over WireGuard.
//!
//! Per-peer outbound channels are **bounded** (see `PEER_OUTBOUND_BUFFER`).
//! A slow consumer whose WebSocket cannot drain at publish rate causes
//! subsequent messages to that peer to be dropped rather than buffered
//! without bound — this prevents topic broadcast (Phase A of the topic
//! pub/sub rollout) from exposing the broker to OOM via a single stalled
//! subscriber. See `src/_doc/2026-04-10-topic-pubsub-v1.md` § 4.4.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use anyhow::Result;
use arc_swap::ArcSwap;
use axum::Router;
use axum::extract::ConnectInfo;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use base64::Engine as _;
use cosmix_bus::bus::{self, BusMessage, BusTarget};
use cosmix_config::node::AdmissionMode;
use cosmix_mesh::{MeshConfig, MeshInbound, MeshPeers, PeerConfig, ReconcileReport};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{RwLock, mpsc, oneshot};

use crate::observe::{
    Direction as ObserveDirection, Observation, ObserveError, ObserveManager,
    Outcome as ObserveOutcome,
};
use crate::subscription::{
    self, BrokerOrigin, JANITOR_INTERVAL, Notification, SubscriptionBroker, TopicInfo,
    stamp_broker_origin, strip_broker_origin,
};

// ── Constants ──

/// Per-peer outbound channel capacity. Bounded to prevent unbounded memory
/// growth when a WebSocket peer cannot keep up with message delivery rate.
/// Messages exceeding this depth are dropped with a tracing warning; the
/// peer remains connected. 256 is generous for typical routing (1–10 msg/s)
/// and leaves plenty of headroom for bursty topic fan-out in Phase A.
const PEER_OUTBOUND_BUFFER: usize = 256;
/// Max size of a single inbound WebSocket message (the mesh Bus ingress
/// path) before tungstenite rejects it — bounding the buffer+parse a
/// mesh peer can force. 16 MiB is generous over the ~1 MiB norm for the
/// largest legitimate Bus bodies (`subscription::MAX_SNAPSHOT_BYTES`);
/// axum's default is 64 MiB. Mirrors `cosmix_bus::bus::MAX_MESSAGE_BYTES` so
/// neither the native-socket nor the WebSocket ingress is unbounded.
const WS_MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

/// Coalescing window for backpressure-drop warnings. A flood of dropped
/// messages produces at most one tracing event per window per call site,
/// with a rolling drop count. Prevents 14k WARN/sec scenarios that crash
/// observability tooling and consume tracing-appender RAM.
const WARN_COALESCE_MS: u64 = 1000;

// Per-call-site rate-limiting state for drop warnings. Each pair tracks
// (last_warn_unix_ms, dropped_since_last_warn). Static so the limiter is
// process-wide; granularity is per-call-site, not per-target — under a
// flood you see one combined warn per second per site, not one per
// affected service.
static ROUTE_DROP_LAST_MS: AtomicU64 = AtomicU64::new(0);
static ROUTE_DROP_COUNT: AtomicU64 = AtomicU64::new(0);
static TAP_DROP_LAST_MS: AtomicU64 = AtomicU64::new(0);
static TAP_DROP_COUNT: AtomicU64 = AtomicU64::new(0);

/// Emit a tracing warning at most once per `WARN_COALESCE_MS` window.
/// `dropped` is incremented every call; the warn message receives the
/// rolling count and resets it. `mk` builds the message string lazily so
/// we don't pay format cost on the suppressed path.
fn warn_drop(last_at_ms: &AtomicU64, dropped: &AtomicU64, mk: impl FnOnce(u64) -> String) {
    dropped.fetch_add(1, Ordering::Relaxed);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let prev = last_at_ms.load(Ordering::Relaxed);
    if now.saturating_sub(prev) >= WARN_COALESCE_MS
        && last_at_ms
            .compare_exchange(prev, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        let n = dropped.swap(0, Ordering::Relaxed);
        tracing::warn!("{}", mk(n));
    }
}

// ── State ──

/// One registered service: its outbound delivery channel plus the build
/// provenance + binding metadata it supplied at `noded.register`
/// (SPEC 02 §4.1; `cosmix-lib-bus::service_info`).
/// `info.name` mirrors the registry key.
#[derive(Clone)]
pub(crate) struct ServiceEntry {
    tx: mpsc::Sender<String>,
    info: cosmix_bus::ServiceInfo,
}

impl ServiceEntry {
    /// True iff this entry's delivery channel is `other`. Used by the
    /// SPEC 12 §15.5 same-channel registration/dereg guards so a stale
    /// socket can't strip a newer owner's registration.
    fn same_channel(&self, other: &mpsc::Sender<String>) -> bool {
        self.tx.same_channel(other)
    }
}

pub(crate) type Registry = Arc<RwLock<HashMap<String, ServiceEntry>>>;
type PendingResponses = Arc<PendingResponseTable>;
type TapSubscribers = Arc<RwLock<Vec<mpsc::Sender<String>>>>;

/// One in-flight request awaiting its response. The broker re-keys every
/// inbound request by a broker-local id (see [`PendingResponseTable`])
/// because caller-supplied request ids are local to the caller process —
/// two anonymous clients (e.g. two `mix -c` shells, two MCP one-shots)
/// both start their `NodedClient::next_id` atomic at 1 and collide on
/// `id=1`. Pre-fix the second caller's `caller_tx` overwrote the first
/// and the first's reply was logged as "Dropping orphan response (no
/// pending caller)"; with the rewrite the broker maps `broker_id ↔
/// (caller_tx, caller_id)` and restores the caller's original id on the
/// response wire before delivering it back. SPEC 18 Phase 2 WS5
/// uncovered this against concurrent `mix -c` fan-outs.
struct PendingResponse {
    caller_tx: mpsc::Sender<String>,
    /// Exact recipient connection; a reused service name is not reply authority.
    responder_tx: mpsc::Sender<String>,
    caller_id: String,
    /// Original caller correlation captured before the broker rewrites `id`.
    /// Kept as an explicit observation field so a future correlation surface
    /// can diverge from the transport reply id without mining rewritten wire.
    observer_correlation_id: String,
    /// Registered caller identity at dispatch time, if any.
    caller_service: Option<String>,
    /// The §14 message class of the registered request (`rpc`/`event`/`stream`),
    /// captured from the Bus `type` at register so the `delivery.inflight_fate`
    /// telemetry labels an abandoned request by its real class, not a guess.
    class: &'static str,
}

/// Broker-local correlation table for pending request → response
/// dispatch. Keyed by `broker_id` (a `noded-<u64>` prefix so taps and
/// debug logs can distinguish broker-rewritten ids from caller ids at
/// a glance), value is the caller's `Sender` plus the caller's
/// original id for wire restoration. See [`PendingResponse`] for the
/// historical bug this fixes.
///
/// `RwLock` rather than `Mutex` because the response path is the hot
/// reader; insert/remove are bursty but short. Removal is always done
/// under the write lock in a single step ([`Self::take`]); the prior
/// shape's get-then-write split was race-prone under duplicate ids.
struct PendingResponseTable {
    next_id: AtomicU64,
    map: RwLock<HashMap<String, PendingResponse>>,
}

impl PendingResponseTable {
    fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            map: RwLock::new(HashMap::new()),
        }
    }

    /// Register a pending request and rewrite `msg`'s `id` header in
    /// place to a broker-local id. Returns `Some(broker_id)` if the
    /// message carried an id (insertion done), `None` if the message
    /// was id-less (fire-and-forget — no reply correlation needed).
    /// Caller must re-serialise `msg` *after* calling this if the
    /// previous wire bytes are still in use.
    async fn register(
        &self,
        msg: &mut BusMessage,
        caller_tx: &mpsc::Sender<String>,
        caller_service: Option<&str>,
        responder_tx: &mpsc::Sender<String>,
    ) -> Option<String> {
        let caller_id = msg.get("id")?.to_string();
        // §14 class from the Bus `type` (request/unset → rpc; the table only
        // ever holds id-bearing messages, so id-less fire-and-forget `control`
        // frames never enter it). Captured here so churn telemetry is honest
        // even though `register` does not itself enforce request-only.
        let class = match msg.message_type() {
            Some("event") => "event",
            Some("stream") => "stream",
            _ => "rpc",
        };
        // Wrap after 2^64 routed requests per broker process is
        // structurally unreachable (≈10^9 req/s for 580 years); the id
        // is process-local and not stable API, so a hypothetical wrap
        // collides only with an entry already long-removed. No special
        // handling beyond `Relaxed` ordering on a u64 counter.
        let n = self.next_id.fetch_add(1, Ordering::Relaxed);
        let broker_id = format!("noded-{n}");
        self.map.write().await.insert(
            broker_id.clone(),
            PendingResponse {
                caller_tx: caller_tx.clone(),
                responder_tx: responder_tx.clone(),
                observer_correlation_id: caller_id.clone(),
                caller_id,
                caller_service: caller_service.map(ToString::to_string),
                class,
            },
        );
        msg.set("id", &broker_id);
        Some(broker_id)
    }

    /// Atomically take and remove a pending entry by broker_id. Returns
    /// `None` if the id was not in the table (orphan response, or
    /// already taken by a peer cleanup path). Single write-lock
    /// acquisition — there is no read-then-write split, so duplicate
    /// take attempts behave as last-writer-wins exactly once.
    async fn take(&self, broker_id: &str) -> Option<PendingResponse> {
        self.map.write().await.remove(broker_id)
    }

    /// An incorrect responder must neither complete nor consume the request.
    async fn take_response(
        &self,
        broker_id: &str,
        responder_tx: &mpsc::Sender<String>,
    ) -> Option<PendingResponse> {
        let mut pending = self.map.write().await;
        if pending
            .get(broker_id)
            .is_some_and(|entry| entry.responder_tx.same_channel(responder_tx))
        {
            pending.remove(broker_id)
        } else {
            None
        }
    }

    /// Remove and return every pending entry whose caller is `caller_tx`
    /// (channel identity, not name), as `(broker_id, entry)` pairs. Used at
    /// session drop to enumerate the requests this session had in flight —
    /// each is an abandoned reply (the caller's `rx` dies with the connection),
    /// so the SPEC 13 §14 / §7.8 B3 churn instrumentation records one
    /// `delivery.inflight_fate` per pair. Single write-lock acquisition,
    /// mirroring [`Self::take`]; O(n) over the (small) pending set, on the cold
    /// disconnect path only.
    async fn drain_for_channel(
        &self,
        caller_tx: &mpsc::Sender<String>,
    ) -> Vec<(String, PendingResponse)> {
        let mut map = self.map.write().await;
        let ids: Vec<String> = map
            .iter()
            .filter(|(_, p)| p.caller_tx.same_channel(caller_tx))
            .map(|(bid, _)| bid.clone())
            .collect();
        ids.into_iter()
            .filter_map(|bid| map.remove(&bid).map(|p| (bid, p)))
            .collect()
    }
}

#[derive(Clone)]
struct AppState {
    registry: Registry,
    pending_responses: PendingResponses,
    tap_subscribers: TapSubscribers,
    observe: Arc<ObserveManager>,
    mesh: Arc<MeshPeers>,
    node_name: String,
    broker: Arc<SubscriptionBroker>,
    /// SPEC 07 props surface — captured at broker startup, immutable for L1.
    bind: String,
    log_level: String,
    started_at: String,
    started: Instant,
    /// Resolved `_spec/` directory; `None` if not located at startup —
    /// `spec.get` returns an error, `world.specs.*` topics aren't seeded.
    spec_dir: Option<Arc<PathBuf>>,
    spec_release: Option<Arc<crate::spec_release::SpecRelease>>,
    /// SPEC 07 §3 props change bus: cached snapshot + per-path emit caps.
    change_bus: Arc<crate::props::ChangeBus>,
    /// SPEC 13 authority + D1.4 routing plane. Posture and the route table are
    /// one lock-free snapshot so admission, dispatch, `noded.inventory`, and
    /// `noded.peers` cannot observe different epochs. `#[derive(Clone)]` shares
    /// the one cell via the outer `Arc`.
    authority: Arc<ArcSwap<crate::routing::RoutingAuthority>>,
    /// Serialises authorised delivery with authority publication. Never held
    /// across an await; already enqueued work is outside revocation's boundary.
    delivery_fence: Arc<std::sync::RwLock<()>>,
    /// Start-immutable `/etc` roster used only when no Verified posture has
    /// ever been accepted since boot.
    etc_roster: Arc<Vec<PeerConfig>>,
    /// Start-immutable configured local WG address. It is warning-only for a
    /// signed self-address mismatch; §9a `wg_bound` owns bind enforcement.
    wg_ip: String,
    /// Actual start-immutable broker listener port, read back from the bound
    /// socket. Compared with signed self membership as a warning-only
    /// consistency check.
    listener_port: u16,
    /// Tracks whether the last signed self endpoint diverged from the bound
    /// listener, so convergence is emitted only after a real divergence.
    listener_endpoint_diverged: Arc<AtomicBool>,
    /// SPEC 13 §9a D2 admission posture (2-c-1). Per-node operator policy; `off`
    /// (default) sends no challenge — behaviour-neutral. `observe` challenges +
    /// verdicts + logs but never refuses. Start-immutable.
    admission_mode: AdmissionMode,
    /// SPEC 13 §9a B1 self-check (2-c-2b) — is the listener bound to this node's
    /// own WG/mesh IP? Start-immutable (the bind never changes at runtime). A
    /// fail-closed input to the enforce gate: a non-WG bind cannot hold the WG
    /// trust boundary, so under `enforce` every inter-node session is refused.
    wg_bound: bool,
    /// SPEC 13 §9a (2-c-1b) — the per-session outstanding-challenge table for
    /// broker-side D2 admission.
    challenge_table: Arc<crate::admission::ChallengeTable>,
    /// SPEC 13 §9a (2-c-2c) — live ENFORCE-admitted inter-node sessions, keyed
    /// by session_id, so an inventory reload that revokes a member can close
    /// exactly that member's session (the §5.5 membership-recheck teardown).
    /// Populated only on a successful enforce admit; same-node and refused
    /// sessions are never tracked (no teardown). Empty in off/observe.
    live_sessions: Arc<RwLock<HashMap<String, GatedSession>>>,
}

struct PendingMeshObservation {
    manager: Arc<ObserveManager>,
    request: BusMessage,
    canonical_wire: String,
    correlation_id: Option<String>,
}

/// SPEC 13 §9a (2-c-2c) — a live enforce-admitted inter-node session, tracked so
/// a revocation reload can tear it down. `close` is the lever: the read loop
/// `select!`s on it, so `notify_one()` breaks the loop and drops the socket.
#[derive(Clone)]
struct GatedSession {
    /// The cryptographically-proven member name established at admission — the
    /// key re-checked against the new inventory snapshot on reload.
    claimed_source_node: String,
    /// The peer's source address, for the §17 `admission.refused{reload:true}`
    /// correlator on teardown (the watcher has no per-session socket).
    source_ip: String,
    /// The session's outbound channel — used to drain its in-flight rpcs for the
    /// `delivery.inflight_fate{cause:reload-teardown}` records (draining here
    /// claims them, so the natural-drop path emits no duplicate session-churn).
    tx: mpsc::Sender<String>,
    /// Triggered by the reload watcher to break the session's read loop.
    close: Arc<tokio::sync::Notify>,
}

/// SPEC 13 §9a (2-c-2b) — the per-session admission record built from the admit
/// response(s) on one socket, consulted by the register-time enforce gate. NOT a
/// bool (a Codex BLOCKER): storing the proven NODE NAME binds the registered
/// `bridge-<node>` identity to the proven one, closing prove-as-X /
/// register-as-Y; storing the detail refuses with the REAL reason; `response_seen`
/// separates "no proof received" from "a bad proof received".
#[derive(Debug, Default, Clone)]
struct SessionAdmission {
    /// `Some(claimed_source_node)` iff the most recent admit response verified
    /// would-admit; `None` otherwise (no/bad/mismatched proof).
    admitted_node: Option<String>,
    /// The §17 detail of the most recent would-refuse response, so the gate can
    /// refuse with the real reason (e.g. `bad-credential-signature`) not a
    /// blanket `no-proof`.
    last_detail: Option<&'static str>,
    /// Whether ANY admit response was processed on this socket.
    response_seen: bool,
}

impl SessionAdmission {
    /// Fold one admit-response outcome into the record. A would-admit sets the
    /// proven node; a would-refuse clears it and records the detail. (With one
    /// challenge per socket there is normally one response; a pathological extra
    /// response can only DOWNGRADE the record — fail-safe.)
    fn apply(&mut self, outcome: AdmitOutcome) {
        self.response_seen = true;
        match outcome {
            AdmitOutcome::Admit(node) => {
                self.admitted_node = Some(node);
            }
            AdmitOutcome::Refuse(detail) => {
                self.admitted_node = None;
                self.last_detail = Some(detail);
            }
        }
    }
}

/// The verdict of one `noded.admit.response` — returned by
/// [`process_admit_response`] and folded into [`SessionAdmission`].
enum AdmitOutcome {
    /// would-admit: the proof verified for this (proven) member name.
    Admit(String),
    /// would-refuse: the §17 detail of why.
    Refuse(&'static str),
}

// ── Entry point ──

pub struct RunConfig {
    pub listen: String,
    pub node: String,
    pub wg_ip: String,
    pub mesh_config_path: Option<String>,
    pub spec_dir: Option<PathBuf>,
    pub admission_mode: AdmissionMode,
    pub observe_allowed_services: Vec<String>,
}

pub async fn run(config: RunConfig, ready_tx: oneshot::Sender<()>) -> Result<()> {
    let RunConfig {
        listen,
        node,
        wg_ip,
        mesh_config_path,
        spec_dir,
        admission_mode,
        observe_allowed_services,
    } = config;
    // Validate before listener/readiness or background work. A configured but
    // invalid public release must never fall back to legacy directory discovery.
    let spec_release = crate::spec_release::SpecRelease::from_env()?.map(Arc::new);
    let spec_dir = if spec_release.is_some() {
        None
    } else {
        spec_dir
    };
    let mut mesh_config = if let Some(ref path) = mesh_config_path {
        MeshConfig::load(path)?
    } else {
        MeshConfig::load_default(&node)
    };
    crate::routing::validate_node_name(&mesh_config.node_name, &node)?;
    let etc_roster = Arc::new(mesh_config.peers.clone());
    // SPEC 13 §9a (2-c-1c) — inject this node's d2 admission seed so the bridge
    // prover can sign. `None` (no seed file yet — ceremony Part B) ⇒ the bridge
    // registers without proving (the broker logs `unproven:no-proof`).
    mesh_config.d2_seed = load_d2_seed(&d2_seed_path());

    tracing::info!(
        node = %mesh_config.node_name,
        peers = mesh_config.peers.len(),
        "Mesh config loaded"
    );

    // SPEC 13 1b-c/2-c-0b — load + verify the cached signed inventory against
    // the provisioned genesis key (fail-closed). Emit the §17 authority-plane
    // accept/reject event, then atomically pair the posture with its D1.4 route
    // table so the §7.7 reload watcher can hot-swap one coherent snapshot.
    let initial_posture =
        crate::authority::load_and_verify(&crate::authority::AuthorityPaths::default());
    emit_posture_event(&initial_posture, false, false);
    let (admit_verified, admit_epoch) = match &initial_posture {
        crate::authority::Posture::Verified(a) => (true, a.epoch),
        crate::authority::Posture::Unverified { .. } => (false, 0),
    };
    let listener = tokio::net::TcpListener::bind(&listen).await?;
    let listener_port = listener.local_addr()?.port();
    // SPEC 13 §9a B1 self-check — is the listener bound to our own WG IP?
    // Start-immutable; computed once and shared with the reload watcher.
    let wg_bound = bind_is_wg(&listen, &wg_ip);
    let initial_authority = crate::routing::RoutingAuthority::new(
        initial_posture,
        &node,
        &wg_ip,
        listener_port,
        etc_roster.as_ref(),
    );
    let listener_endpoint_diverged = Arc::new(AtomicBool::new(false));
    emit_listener_endpoint_state(
        &initial_authority,
        &node,
        listener_port,
        &listener_endpoint_diverged,
        false,
    );

    let (mesh_incoming_tx, mut mesh_incoming_rx) = mpsc::unbounded_channel::<MeshInbound>();
    let mesh = Arc::new(MeshPeers::new(mesh_config, mesh_incoming_tx));
    mesh.reconcile_endpoints(
        initial_authority.routes.desired_endpoints(),
        initial_authority.revision(),
    )
    .await;
    let authority = Arc::new(ArcSwap::from_pointee(initial_authority));

    // SPEC 13 §9a (slice 2-c-1a) — emit the D2 admission posture so the
    // configured mode, the fail-closed-bind self-check, the trust root, and the
    // prover-incapable flag (no d2 seed yet — ceremony Part B) are observable
    // before any challenge is wired. No wire change: 2-c-1a is behaviour-neutral.
    emit_admission_posture_event(
        admission_mode,
        &listen,
        wg_bound,
        admit_verified,
        load_d2_seed(&d2_seed_path()).is_none(),
        admit_epoch,
        false,
    );

    let registry: Registry = Arc::new(RwLock::new(HashMap::new()));
    let observe = ObserveManager::new(observe_allowed_services);
    observe.spawn_drainer();

    // Deliver messages from remote hubs to local services
    let registry_for_mesh = registry.clone();
    let observe_for_mesh = observe.clone();
    let mesh_for_incoming = mesh.clone();
    tokio::spawn(async move {
        while let Some(inbound) = mesh_incoming_rx.recv().await {
            if !mesh_for_incoming
                .inbound_generation_is_current(&inbound.peer, inbound.connection_generation)
                .await
            {
                tracing::debug!(
                    peer = %inbound.peer,
                    connection_generation = %inbound.connection_generation,
                    "Dropping inbound frame from retired mesh connection"
                );
                continue;
            }
            let peer = inbound.peer;
            let connection_generation = inbound.connection_generation;
            let mut msg = inbound.message;
            // Load once before any observation-only allocation. When false,
            // mesh ingress still canonicalises and serialises exactly what
            // delivery requires, but retains no correlation string or second
            // wire copy for observation.
            let observing = observe_for_mesh.is_active();
            let target = msg.to_addr().unwrap_or("").to_string();
            let correlation_id = if observing {
                msg.get("id").map(ToString::to_string)
            } else {
                None
            };
            // Mesh ingress has no authenticated remote-service identity yet.
            // Canonicalise before route classification so both accepted and
            // route-rejected observations describe the broker-owned envelope.
            msg.headers.remove("from");
            stamp_broker_origin(&mut msg, BrokerOrigin::Mesh);
            // SPEC 01 §4 cross-mesh routing is reserved-but-refused at the
            // parser until federation transport exists; if a remote hub
            // attempts to relay a `to: <local>@<fqdn>` address through this
            // node, drop it here rather than dispatching the local half.
            let parsed = BusTarget::parse(&target);
            if matches!(parsed, Some(BusTarget::CrossMesh { .. })) {
                tracing::warn!(
                    target = %target,
                    "Mesh ingress refused: cross-mesh routing not implemented"
                );
                if observing {
                    observe_for_mesh.observe(Observation::canonical(
                        ObserveDirection::MeshIn,
                        ObserveOutcome::Rejected,
                        &msg,
                        correlation_id.as_deref(),
                    ));
                }
                continue;
            }
            let service = match parsed {
                // `<node>.bus` is the broker form per SPEC 01 §4.1 — the
                // service is implicit `noded`. Without this default the
                // dispatch would try to deliver to a local service named
                // after the node, which is never registered.
                Some(BusTarget::Local(addr)) => {
                    addr.service.clone().unwrap_or_else(|| "noded".to_string())
                }
                None => {
                    // Mirror the egress malformed-address logic: a parser
                    // rejection on a target that *looks* like an Bus
                    // address (contains `.` or `@`) must not fall through
                    // to local-service lookup — otherwise a remote (or
                    // older) broker relaying a SPEC-01-removed shape like
                    // `a.b.c.d.bus` or `maild@delta` could deliver to a
                    // locally-registered service with that literal name.
                    // We drop/log on ingress rather than synthesizing an
                    // error reply because the message originated on a
                    // remote noded — the local service isn't the right
                    // entity to send error responses back to that remote
                    // and there is no authenticated remote-service
                    // identity for one yet (SPEC 10 territory).
                    if target.contains('.') || target.contains('@') {
                        tracing::warn!(
                            target = %target,
                            "Mesh ingress refused: malformed Bus target"
                        );
                        if observing {
                            observe_for_mesh.observe(Observation::canonical(
                                ObserveDirection::MeshIn,
                                ObserveOutcome::Rejected,
                                &msg,
                                correlation_id.as_deref(),
                            ));
                        }
                        continue;
                    }
                    target.clone()
                }
                // `BusTarget::CrossMesh` is already drained by the
                // earlier `matches!` above; this arm is unreachable but
                // satisfies the matcher exhaustiveness check.
                Some(BusTarget::CrossMesh { .. }) => {
                    unreachable!("cross-mesh ingress already short-circuited above")
                }
            };

            let target_tx = {
                let registry = registry_for_mesh.read().await;
                registry.get(&service).map(|entry| entry.tx.clone())
            };
            let mut observed_wire = None;
            let outcome = if let Some(target_tx) = target_tx {
                let canonical_wire = msg.to_wire();
                let delivery_wire = if observing {
                    observed_wire = Some(canonical_wire.clone());
                    canonical_wire
                } else {
                    canonical_wire
                };
                let Some(_inbound_fence) = mesh_for_incoming
                    .validate_inbound(&peer, connection_generation)
                    .await
                else {
                    tracing::debug!(
                        peer = %peer,
                        connection_generation = %connection_generation,
                        "Dropping inbound frame retired before final delivery"
                    );
                    continue;
                };
                match target_tx.try_send(delivery_wire) {
                    Ok(()) => ObserveOutcome::Delivered,
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        tracing::warn!(
                            target = %service,
                            "Mesh→local delivery dropped: peer outbound full"
                        );
                        ObserveOutcome::Dropped
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        tracing::debug!(
                            target = %service,
                            "Mesh→local delivery failed: peer disconnected"
                        );
                        ObserveOutcome::Rejected
                    }
                }
            } else {
                tracing::debug!(target = %service, "No local service for incoming mesh message");
                ObserveOutcome::Rejected
            };
            if observing {
                if let Some(canonical_wire) = observed_wire.as_deref() {
                    observe_for_mesh.observe(Observation::from_message(
                        ObserveDirection::MeshIn,
                        outcome,
                        &msg,
                        canonical_wire,
                        correlation_id.as_deref(),
                    ));
                } else {
                    observe_for_mesh.observe(Observation::canonical(
                        ObserveDirection::MeshIn,
                        outcome,
                        &msg,
                        correlation_id.as_deref(),
                    ));
                }
            }
        }
    });

    let pending_responses: PendingResponses = Arc::new(PendingResponseTable::new());
    let tap_subscribers: TapSubscribers = Arc::new(RwLock::new(Vec::new()));
    let broker = Arc::new(SubscriptionBroker::new());

    // Janitor: periodically purge stale topic snapshots past the orphan timeout
    // grace period. See src/_doc/2026-04-10-topic-pubsub-v1.md § 10.3.1.
    //
    // SPEC 12 C10b — janitor also reaps dead-tx subscriptions and may
    // emit `topic.idle` notifications when its sweep drives a topic's
    // count from N>0 to 0. Each `Notification` carries the target
    // peer's `Sender` directly (captured at notification-build time),
    // so we deliver inline without needing the registry.
    let broker_for_janitor = broker.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(JANITOR_INTERVAL);
        interval.tick().await; // skip the immediate first tick
        loop {
            interval.tick().await;
            for notice in broker_for_janitor.janitor_tick().await {
                let _ = notice.target_tx.try_send(notice.wire);
            }
        }
    });

    let started = Instant::now();
    let started_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let log_level = std::env::var("RUST_LOG").unwrap_or_else(|_| "cosmix_noded=info".to_string());

    let spec_dir_arc = spec_dir.map(Arc::new);
    let change_bus = crate::props::ChangeBus::new(broker.clone());

    let state = AppState {
        registry,
        pending_responses,
        tap_subscribers,
        observe,
        mesh,
        node_name: node.clone(),
        broker: broker.clone(),
        bind: listen.to_string(),
        log_level,
        started_at: started_at.clone(),
        started,
        spec_dir: spec_dir_arc.clone(),
        spec_release,
        change_bus: change_bus.clone(),
        authority: authority.clone(),
        delivery_fence: Arc::new(std::sync::RwLock::new(())),
        etc_roster,
        wg_ip,
        listener_port,
        listener_endpoint_diverged,
        admission_mode,
        wg_bound,
        challenge_table: Arc::new(crate::admission::ChallengeTable::new()),
        live_sessions: Arc::new(RwLock::new(HashMap::new())),
    };

    // Seed the change bus with the L1 snapshot so the first mutation
    // produces a diff against real state, not against `None`. Also seed
    // the retained `world.noded` topic so the first subscriber to attach
    // (even before any mutation) receives a snapshot.
    let initial = crate::props::collect(
        started,
        &state.started_at,
        &state.bind,
        &state.node_name,
        &state.log_level,
        &state.registry,
        &state.broker,
    )
    .await;
    change_bus.seed(&initial).await;
    change_bus.publish_world_unchecked(&initial).await;

    // Drainer: every 1s, flush any cap-blocked world.noded publish so
    // rapid back-to-back mutations don't leave the retained snapshot
    // permanently stale.
    let bus_for_drain = change_bus.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(1000));
        ticker.tick().await;
        loop {
            ticker.tick().await;
            bus_for_drain.drain_pending().await;
        }
    });

    // SPEC 13 §7.7 (2-c-0b) — hot-reload the authority posture on
    // `inventory.signed` change. off/observe grandfather every live session
    // (outside the §14 gate); under enforce (2-c-2c) a revocation delta closes
    // the affected member's live sessions (the §5.5 membership-recheck teardown).
    spawn_inventory_reload_watcher(state.clone());

    // SPEC 07 §5.2 — seed `world.specs.<n>` retained topics at startup.
    if let Some(dir) = spec_dir_arc.as_deref() {
        seed_world_specs(&broker, dir).await;
    }

    let app = Router::new()
        .route("/ws", axum::routing::get(ws_handler))
        .with_state(state);

    tracing::info!(node = %node, "Broker listening on ws://{}", listen);

    // Signal that the listener is bound and ready
    let _ = ready_tx.send(());

    // SPEC 13 §9a (2-c-2a) — serve WITH connect-info so each accepted socket
    // carries its peer's source address. The source IP is the trustworthy
    // correlator on a would-refuse (the claimed `source_node` is unverified
    // there) and the input to the same-node-origin classifier the enforce gate
    // (2-c-2b) uses — never the proof, only the local-vs-network boundary.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;

    Ok(())
}

/// SPEC 07 §5.2 — at startup, scan `_spec/` and publish each chapter as a
/// retained `world.specs.NN` topic. Frontmatter scalars become headers,
/// the prose is the body. File-watch is deferred (out of scope per plan).
async fn seed_world_specs(broker: &Arc<SubscriptionBroker>, spec_dir: &std::path::Path) {
    let chapters = crate::spec::available_chapters(spec_dir);
    if chapters.is_empty() {
        tracing::warn!(
            ?spec_dir,
            "No spec chapters found; world.specs.* not seeded"
        );
        return;
    }
    // Broker-internal publisher: synthesize an outbound channel and absorb it.
    // The broker stores `last_publisher_tx` for `topic.idle` push-back, but
    // the broker itself never disconnects, so the channel just discards.
    let (sink_tx, mut sink_rx) = mpsc::channel::<String>(8);
    tokio::spawn(async move {
        while sink_rx.recv().await.is_some() {
            // discard
        }
    });

    let mut seeded = 0usize;
    for n in &chapters {
        let path = match crate::spec::find_chapter(spec_dir, *n) {
            Some(p) => p,
            None => continue,
        };
        let msg = match crate::spec::load_spec_file(&path) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(?path, error = %e, "spec load failed; skipping");
                continue;
            }
        };
        let topic = format!("world.specs.{:02}", n);
        let wire = msg.to_wire();
        match broker
            .publish(&topic, &wire, "noded", sink_tx.clone(), true)
            .await
        {
            Ok((_, _, notices)) => {
                // SPEC 12 C10b — propagate any dead-tx-pair idle notices.
                for n in notices {
                    let _ = n.target_tx.try_send(n.wire);
                }
                seeded += 1;
            }
            Err(e) => {
                tracing::warn!(topic = %topic, error = ?e, "spec topic publish failed");
            }
        }
    }
    tracing::info!(
        seeded,
        total = chapters.len(),
        "world.specs.* retained topics"
    );
}

// ── WebSocket handler ──

/// Emit the §17 authority-plane accept/reject event for a posture. Shared by
/// startup (§7.1) and reload (§7.7) so both speak one event shape. `reload`
/// flags the reload path; `kept_last_good` marks a rejected reload that was
/// NOT applied (the don't-downgrade case).
fn emit_posture_event(posture: &crate::authority::Posture, reload: bool, kept_last_good: bool) {
    match posture {
        crate::authority::Posture::Verified(a) => tracing::info!(
            event = "inventory.accepted",
            epoch = a.epoch,
            recovery_generation = a.recovery_generation,
            via_recovery = a.via_recovery,
            hash = %a.hash,
            members = a.members.len(),
            verified_by = ?a.verified_by,
            reload = reload,
            "signed inventory accepted as trust root (§7.1/§7.7)"
        ),
        crate::authority::Posture::Unverified { reason } => tracing::warn!(
            event = "inventory.rejected",
            reason = %reason,
            reload = reload,
            kept_last_good = kept_last_good,
            "no verified trust root from this load — fail-closed authority posture (§7.7)"
        ),
    }
}

/// Emit the structured §9 listener/signed-self endpoint consistency state.
/// The listener is start-immutable; this is a truthful state transition event,
/// not an implicit promise to rebind it during reload.
fn emit_listener_endpoint_state(
    authority: &crate::routing::RoutingAuthority,
    node: &str,
    listener_port: u16,
    diverged: &AtomicBool,
    reload: bool,
) {
    let Some(signed_port) = authority.signed_self_noded_port() else {
        return;
    };
    if signed_port != listener_port {
        diverged.store(true, Ordering::Release);
        tracing::warn!(
            event = "listener.endpoint_diverged",
            node,
            signed_noded_port = signed_port,
            listener_noded_port = listener_port,
            epoch = authority.revision().epoch,
            reload,
            "signed self endpoint differs from the bound listener; routing remains enabled"
        );
    } else if diverged.swap(false, Ordering::AcqRel) {
        tracing::info!(
            event = "listener.endpoint_converged",
            node,
            signed_noded_port = signed_port,
            listener_noded_port = listener_port,
            epoch = authority.revision().epoch,
            reload,
            "signed self endpoint has converged with the bound listener"
        );
    }
}

/// The §9a runtime read contract for this node's d2 admission seed.
fn d2_seed_path() -> std::path::PathBuf {
    std::path::PathBuf::from("/etc/cosmix/noded/d2.seed")
}

/// The outcome of reading the d2 admission seed file (SPEC 13 §9a, 2-c-2a). The
/// point of the type is to distinguish the **normal** pre-ceremony "absent"
/// state from a **present-but-unreadable** file — the misconfiguration that
/// silently disabled proving until the observe-flip surfaced it (a root-only
/// `0600` seed is unreadable by the unprivileged `cosmix-noded` user). Absent
/// is silent; unreadable/malformed are LOUD.
#[derive(Debug, PartialEq, Eq)]
enum SeedRead {
    Loaded([u8; 32]),
    /// `NotFound` — the seed has not been provisioned yet (ceremony Part B).
    Absent,
    /// Present but not readable (e.g. `PermissionDenied`). Carries the io
    /// `ErrorKind` text for the warning.
    Unreadable(String),
    /// Readable but not a base64 32-byte seed.
    Malformed,
}

/// Pure classifier (testable without root or the filesystem): map a read result
/// to a [`SeedRead`]. `NotFound` → `Absent` (silent, normal pre-ceremony); any
/// other IO error (notably `PermissionDenied`) → `Unreadable` (loud); a
/// readable-but-non-base64-32 body → `Malformed` (loud).
fn classify_seed_read(raw: std::io::Result<String>) -> SeedRead {
    match raw {
        Ok(s) => match base64::engine::general_purpose::STANDARD
            .decode(s.trim())
            .ok()
            .and_then(|b| <[u8; 32]>::try_from(b).ok())
        {
            Some(seed) => SeedRead::Loaded(seed),
            None => SeedRead::Malformed,
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => SeedRead::Absent,
        Err(e) => SeedRead::Unreadable(e.kind().to_string()),
    }
}

/// Read this node's `kind:"d2"` admission seed (SPEC 13 §9a, slice 2-c-1) —
/// base64 32-byte at `/etc/cosmix/noded/d2.seed`. The corrected perms are
/// **`root:cosmix-noded 0640`** (NOT root-only `0600`): noded runs as the
/// unprivileged `cosmix-noded` user, so a root-only seed is unreadable and the
/// node is silently prover-incapable. Returns `None` on missing/unreadable/
/// malformed: the node then cannot PROVE itself to a peer (the prover-incapable
/// state), which is NOT an error — the seed is provisioned by the d2 key
/// ceremony Part B, which may not have run. Distinct from the wg key (D0/D2
/// decoupling). **2-c-2a:** a present-but-unreadable seed is now LOUD (a
/// `tracing::warn!` naming the fix) instead of a silent `None`, closing the
/// legibility gap that hid the perms bug.
fn load_d2_seed(path: &std::path::Path) -> Option<[u8; 32]> {
    match classify_seed_read(std::fs::read_to_string(path)) {
        SeedRead::Loaded(seed) => Some(seed),
        // Normal pre-ceremony state — the seed simply isn't provisioned yet.
        SeedRead::Absent => None,
        SeedRead::Malformed => {
            tracing::warn!(
                path = %path.display(),
                "d2 admission seed present but malformed (expected base64 32-byte) — node cannot prove itself (§9a)"
            );
            None
        }
        SeedRead::Unreadable(kind) => {
            tracing::warn!(
                path = %path.display(),
                error = %kind,
                "d2 admission seed present but UNREADABLE — noded runs as the unprivileged cosmix-noded user; the seed must be root:cosmix-noded 0640 (§9a). Node is prover-incapable until fixed."
            );
            None
        }
    }
}

/// SPEC 13 §9a fail-closed bind (B1 self-check): the broker's listener must be
/// bound to this node's **own WG/mesh IP** (`wg_ip`), never `0.0.0.0`/any (which
/// exposes the boundary to off-mesh hosts) and never some other concrete
/// public/LAN/loopback address (which is not the WG interface). The listener is
/// `wg_ip:port` by construction, so this enforces that and guards a `--listen`
/// override; an unparseable bind or `wg_ip` fails closed.
fn bind_is_wg(bind: &str, wg_ip: &str) -> bool {
    let bind_ip = match bind.parse::<std::net::SocketAddr>() {
        Ok(addr) => addr.ip(),
        Err(_) => return false,
    };
    if bind_ip.is_unspecified() {
        return false;
    }
    match wg_ip.parse::<std::net::IpAddr>() {
        Ok(wg) => bind_ip == wg,
        Err(_) => false,
    }
}

fn admission_mode_str(m: AdmissionMode) -> &'static str {
    match m {
        AdmissionMode::Off => "off",
        AdmissionMode::Observe => "observe",
        AdmissionMode::Enforce => "enforce",
    }
}

/// SPEC 13 §9a fail-closed predicate (2-c-2b): under `enforce`, does the node
/// fail to hold the WG trust boundary — and so MUST refuse every inter-node
/// session (never fall to legacy-open)? Returns the §17 refuse-all reason, or
/// `None` when enforce can run normally. The SINGLE source of truth for the two
/// fail-closed conditions, checked trust-root-before-bind; `admission_effective`
/// and the register-time gate both consult it (a typed predicate, never a string
/// compare on the effective-posture text). `None` for off/observe.
fn enforce_refuses_all(
    configured: AdmissionMode,
    wg_bound: bool,
    verified: bool,
) -> Option<&'static str> {
    if configured != AdmissionMode::Enforce {
        return None;
    }
    if !verified {
        return Some("no-verified-trust-root");
    }
    if !wg_bound {
        return Some("non-wg-bind");
    }
    None
}

/// Effective posture + reason from the configured mode and the two §9a
/// fail-closed conditions. `off`/`observe` are unaffected (observe never
/// refuses); `enforce` degrades to `refuse-all` when the node cannot hold the
/// trust boundary (no verified trust root, or a non-WG bind).
fn admission_effective(
    configured: AdmissionMode,
    wg_bound: bool,
    verified: bool,
) -> (&'static str, &'static str) {
    match configured {
        AdmissionMode::Off => ("off", ""),
        AdmissionMode::Observe => ("observe", ""),
        AdmissionMode::Enforce => match enforce_refuses_all(configured, wg_bound, verified) {
            Some(reason) => ("refuse-all", reason),
            None => ("enforce", ""),
        },
    }
}

/// SPEC 13 §9a §4 (2-c-2b) — the local-vs-network boundary, NOT the proof. A
/// session is **same-node** (ungated; a local citizen registering a service)
/// iff its source is loopback or this broker's OWN bind IP. Every other WG `/24`
/// address (the hub, any peer) is **inter-node** and gated. This never
/// identifies WHICH member and never substitutes for the Ed25519 signature, so
/// it does not reintroduce source-IP-as-proof / per-`/32` pinning. It is sound
/// only because under the deployed WG path a remote peer's source is its
/// WG-authenticated `/24` IP — never loopback, never the broker's own bind IP (a
/// peer can't forge another's tunnel IP nor source from loopback). The non-WG
/// bind that could see spoofable off-mesh source IPs is exactly the
/// [`enforce_refuses_all`] `non-wg-bind` fail-closed condition.
fn is_same_node_origin(src: std::net::IpAddr, bind: &str) -> bool {
    if src.is_loopback() {
        return true;
    }
    bind.parse::<std::net::SocketAddr>()
        .map(|a| a.ip() == src)
        .unwrap_or(false)
}

/// The register-time enforce decision (SPEC 13 §9a, 2-c-2b) for one
/// `noded.register` under `mesh.admission=enforce`. Pure over its inputs so the
/// entire security matrix is unit-testable — a loopback test client would
/// otherwise always classify same-node and never exercise the gated path.
#[derive(Debug, PartialEq, Eq)]
enum RegisterGate {
    /// Same-node origin (local citizen) — proceed, emit no admission event.
    Ungated,
    /// Inter-node with a proof bound to the registered identity — emit
    /// `admission.admitted` and proceed. Carries the PROVEN node name.
    Admit(String),
    /// Inter-node, refused — emit `admission.refused`; send rc=10; do NOT
    /// register. `synth_observed` requests a synthetic `admission.observed` to
    /// cover a session that never answered (only set when no admit response was
    /// processed, since `process_admit_response` already emitted one otherwise —
    /// avoids a double observed).
    Refuse {
        detail: &'static str,
        source_node: String,
        synth_observed: bool,
    },
}

/// The pure §9a register-time enforce gate (2-c-2b). Order: same-node → ungated;
/// then inter-node fail-closed ([`enforce_refuses_all`]); then the
/// identity-binding check — a plain service name (no `bridge-` prefix) is a
/// remote dodge (§4 → `no-proof`), else the registered `bridge-<node>` identity
/// MUST equal the PROVEN one (closing prove-as-X / register-as-Y). Detail
/// vocabulary is the ratified §17 set only.
#[allow(clippy::too_many_arguments)]
fn register_gate_decision(
    source_ip: std::net::IpAddr,
    bind: &str,
    configured: AdmissionMode,
    wg_bound: bool,
    verified: bool,
    from: &str,
    adm: &SessionAdmission,
) -> RegisterGate {
    // Off/observe never reach the gate (the caller guards on Enforce), but be
    // explicit: only enforce gates.
    if configured != AdmissionMode::Enforce {
        return RegisterGate::Ungated;
    }
    // §4 same-node citizens are ungated (a service has no d2 identity).
    if is_same_node_origin(source_ip, bind) {
        return RegisterGate::Ungated;
    }
    // Inter-node → gated. Emit the synthetic observed only when no admit
    // response was processed (else process_admit_response already emitted one).
    let synth = !adm.response_seen;
    let claimed = from.strip_prefix("bridge-");
    // source_node for the §17 events: the claimed bare name, else the raw from.
    let source_node = claimed.unwrap_or(from).to_string();

    // Fail-closed first: a node that cannot hold the WG boundary refuses every
    // inter-node session (NOT fall to legacy-open).
    if let Some(reason) = enforce_refuses_all(configured, wg_bound, verified) {
        return RegisterGate::Refuse {
            detail: reason,
            source_node,
            synth_observed: synth,
        };
    }

    match claimed {
        // A remote peer registering a PLAIN service name to dodge the gate (§4).
        None => RegisterGate::Refuse {
            detail: "no-proof",
            source_node,
            synth_observed: synth,
        },
        // The registered identity matches the PROVEN one → admit.
        Some(c) if adm.admitted_node.as_deref() == Some(c) => RegisterGate::Admit(c.to_string()),
        // No admit response was ever processed → no-proof.
        Some(_) if !adm.response_seen => RegisterGate::Refuse {
            detail: "no-proof",
            source_node,
            synth_observed: synth,
        },
        // A response proved a DIFFERENT member (prove-as-X / register-as-Y — the
        // Codex BLOCKER the identity-binding closes).
        Some(_) if adm.admitted_node.is_some() => RegisterGate::Refuse {
            detail: "name-mismatch",
            source_node,
            synth_observed: synth,
        },
        // A response was seen and would-refused — surface its real reason.
        Some(_) => RegisterGate::Refuse {
            detail: adm.last_detail.unwrap_or("no-proof"),
            source_node,
            synth_observed: synth,
        },
    }
}

/// Emit the §17 `admission.posture` authority-plane event — the operator's
/// single source of truth for "is this node challenging, refusing, or
/// prover-blocked, and why" (SPEC 13 §9a/§17). `prover_incapable` = the node
/// cannot prove ITSELF to a peer (no d2 seed) — distinct from its verifier role,
/// which runs regardless of the seed.
#[allow(clippy::too_many_arguments)]
fn emit_admission_posture_event(
    configured: AdmissionMode,
    bind: &str,
    wg_bound: bool,
    verified: bool,
    prover_incapable: bool,
    epoch: u64,
    reload: bool,
) {
    let (effective, reason) = admission_effective(configured, wg_bound, verified);
    tracing::info!(
        event = "admission.posture",
        configured = admission_mode_str(configured),
        effective = effective,
        reason = reason,
        bind = %bind,
        wg_bound = wg_bound,
        trust_root = if verified { "verified" } else { "unverified" },
        prover_incapable = prover_incapable,
        epoch = epoch,
        reload = reload,
        "D2 admission posture (§9a)"
    );
}

/// Emit the §17 `admission.observed` verdict for one session (SPEC 13 §9a,
/// 2-c-1b). OBSERVE-only — the verdict is logged, never enforced. `source_node`
/// is the CLAIMED (cryptographically unverified on a `would-refuse`) name;
/// `canon_digest` lets an operator tell a transcript-divergence bug (uniform
/// bad-credential-signature with a stable digest) from "no d2 keys yet".
#[allow(clippy::too_many_arguments)]
fn emit_admission_observed(
    state: &AppState,
    verdict: &str,
    detail: &str,
    source_node: &str,
    source_ip: &str,
    session_id: &str,
    epoch: u64,
    canon_digest: &str,
) {
    tracing::info!(
        event = "admission.observed",
        verdict = verdict,
        detail = detail,
        source_node = source_node,
        source_ip = source_ip,
        session_id = session_id,
        posture = admission_mode_str(state.admission_mode),
        epoch = epoch,
        canon_digest = canon_digest,
        "D2 admission verdict (observe; §9a)"
    );
}

/// Emit the §17 `admission.admitted` event (SPEC 13 §9a, 2-c-2b) — the positive
/// audit trail for an ENFORCE accept. Unlike `admission.observed`, `source_node`
/// here is the cryptographically PROVEN member name (bound to the registered
/// `bridge-<node>` identity).
fn emit_admission_admitted(
    state: &AppState,
    source_node: &str,
    source_ip: &str,
    epoch: u64,
    session_id: &str,
) {
    tracing::info!(
        event = "admission.admitted",
        source_node = source_node,
        source_ip = source_ip,
        session_id = session_id,
        posture = admission_mode_str(state.admission_mode),
        epoch = epoch,
        "D2 admission admitted (enforce; §9a)"
    );
}

/// Emit the §17 `admission.refused` event (SPEC 13 §9a, 2-c-2b/2-c-2c). `detail`
/// is the ratified §17 reason; `source_node` is the CLAIMED (unverified) name —
/// `source_ip` is the trustworthy correlator. `reload=true` marks a teardown of
/// an already-admitted session whose member was revoked on inventory reload
/// (2-c-2c, §5.5); `reload=false` is a register-time refusal (2-c-2b).
#[allow(clippy::too_many_arguments)]
fn emit_admission_refused(
    state: &AppState,
    detail: &str,
    source_node: &str,
    source_ip: &str,
    epoch: u64,
    session_id: &str,
    reload: bool,
) {
    tracing::info!(
        event = "admission.refused",
        detail = detail,
        source_node = source_node,
        source_ip = source_ip,
        session_id = session_id,
        posture = admission_mode_str(state.admission_mode),
        epoch = epoch,
        reload = reload,
        "D2 admission refused (enforce; §9a)"
    );
}

/// SPEC 13 §9a (2-c-1b/2-c-2b) — handle a `noded.admit.response`: verify the id
/// answers THIS socket's challenge, take the single-use outstanding challenge,
/// reconstruct the transcript from its stored raw bytes and the response wire,
/// run `admit()` against the claimed member from the SAME authority snapshot, and
/// emit the §17 `admission.observed` verdict. ALWAYS emits the observed event and
/// returns the [`AdmitOutcome`] the caller folds into the per-session record
/// (2-c-2b) — `None` only for an uncorrelatable (no-id) frame, which is ignored.
/// This still never refuses the session itself; the enforce refusal is applied at
/// the register-time gate from the folded record.
async fn process_admit_response(
    state: &AppState,
    msg: &BusMessage,
    session_id: &str,
    source_ip: &str,
    own_challenge_id: Option<&str>,
) -> Option<AdmitOutcome> {
    let id = match msg.get("id") {
        // A `noded.admit.response` with no `id` is uncorrelatable garbage —
        // ignore it silently (HEAD behaviour), touching neither the verdict
        // stream nor the per-session record.
        None => return None,
        Some(i) => i.to_string(),
    };
    // Defence-in-depth: a response MUST answer the challenge issued on ITS OWN
    // socket, not some other session's. The challenge table is keyed by the Bus
    // `id` globally; without this bind, a response quoting another socket's id
    // would `take` that entry and populate THIS session's record from it. The
    // crypto still requires the member's d2 signature over the stored nonce, so
    // this is not the sole defence — but it keeps the "prove against your own
    // challenge" invariant from resting on the no-relay assumption. A mismatch
    // (incl. no challenge issued on this socket) is stale-or-replayed.
    if own_challenge_id != Some(id.as_str()) {
        emit_admission_observed(
            state,
            "would-refuse",
            "stale-or-replayed-challenge",
            "?",
            source_ip,
            session_id,
            0,
            "",
        );
        return Some(AdmitOutcome::Refuse("stale-or-replayed-challenge"));
    }
    // Single-use: take the challenge (also the replay / no-entry defence).
    let challenge = match state.challenge_table.take(&id).await {
        Some(c) => c,
        None => {
            emit_admission_observed(
                state,
                "would-refuse",
                "stale-or-replayed-challenge",
                "?",
                source_ip,
                session_id,
                0,
                "",
            );
            return Some(AdmitOutcome::Refuse("stale-or-replayed-challenge"));
        }
    };
    // One authority snapshot for mesh / epoch / member (design §5.4).
    let snap = state.authority.load();
    let a = match &snap.posture {
        crate::authority::Posture::Verified(a) => a,
        crate::authority::Posture::Unverified { .. } => {
            emit_admission_observed(
                state,
                "would-refuse",
                "no-verified-trust-root",
                "?",
                source_ip,
                session_id,
                0,
                "",
            );
            return Some(AdmitOutcome::Refuse("no-verified-trust-root"));
        }
    };
    let body: serde_json::Value =
        serde_json::from_str(&msg.body).unwrap_or(serde_json::Value::Null);
    let proof =
        match crate::admission::reconstruct_proof(&challenge, &body, &a.mesh, &state.node_name) {
            Ok(p) => p,
            Err(e) => {
                emit_admission_observed(
                    state,
                    "would-refuse",
                    e.detail(),
                    "?",
                    source_ip,
                    session_id,
                    a.epoch,
                    "",
                );
                return Some(AdmitOutcome::Refuse(e.detail()));
            }
        };
    let digest = crate::admission::canon_digest(&proof.transcript);
    let (verdict, detail) = match a.members_full.get(&proof.claimed_source_node) {
        None => ("would-refuse", "no-current-d2-credential"),
        Some(member) => {
            match cosmix_mesh_trust::admission::admit(
                member,
                &proof.transcript,
                &proof.signature,
                a.epoch,
            ) {
                Ok(()) => ("would-admit", ""),
                Err(e) => ("would-refuse", crate::admission::verdict_detail(&e)),
            }
        }
    };
    emit_admission_observed(
        state,
        verdict,
        detail,
        &proof.claimed_source_node,
        source_ip,
        session_id,
        a.epoch,
        &digest,
    );
    Some(if verdict == "would-admit" {
        AdmitOutcome::Admit(proof.claimed_source_node)
    } else {
        AdmitOutcome::Refuse(detail)
    })
}

/// The §7.7 don't-downgrade rule: a freshly-loaded `Unverified` posture MUST
/// NOT replace a live `Verified` one — a bad/partial push keeps last-known-good.
/// Every other transition (a verified advance, or recovering from Unverified)
/// is applied.
fn reload_keeps_last_good(
    new: &crate::authority::Posture,
    current: &crate::authority::Posture,
) -> bool {
    matches!(new, crate::authority::Posture::Unverified { .. })
        && matches!(current, crate::authority::Posture::Verified(_))
}

/// SPEC 13 §7.7 (slice 2-c-0b) — watch the local `inventory.signed` cache and
/// hot-reload the authority posture + route table on change, reusing the SAME
/// [`crate::authority::load_and_verify`] (no second verify path) and **never
/// downgrading** a `Verified` posture to `Unverified` on a bad/partial push.
/// In `off`/`observe` this grandfathers every live session — it swaps the
/// in-memory snapshot consumed by admission, routing, `noded.inventory`, and
/// `noded.peers`. D1.4 reconciles outbound transports before publishing that
/// snapshot. The deployed `enforce` path below also performs 2-c-2c inbound
/// revocation teardown,
/// despite older §14 planning text that still calls activation blocked. D1.4
/// does not widen it. Watch failures are non-fatal: the node keeps
/// the start-time posture and logs a warning. **Known limitation:** if the
/// watched parent dir is *removed and recreated*, the inotify watch is not
/// re-established and reload goes quiet until the daemon restarts (the error is
/// logged). `/var/lib/cosmix/noded` is stable system state, so this is an
/// operational anomaly, not a steady-state concern; the file being replaced
/// in place (the `install`/scp writer) is handled by watching the dir.
fn spawn_inventory_reload_watcher(state: AppState) {
    use notify::{EventKind, RecursiveMode, Watcher};

    let paths = crate::authority::AuthorityPaths::default();
    let Some(dir) = paths.signed.parent().map(|p| p.to_path_buf()) else {
        tracing::warn!("inventory.signed has no parent dir; reload watcher disabled");
        return;
    };
    // The cache filename, to filter dir events down to OUR file.
    let target = paths.signed.file_name().map(|n| n.to_os_string());

    // notify runs its callback on its own (sync) thread; bridge a coalesced
    // unit signal to the async task (we re-read `paths` on fire, so the event
    // payload is not needed). The unbounded tokio sender is safe to call from
    // the non-runtime watcher thread.
    let (sig_tx, mut sig_rx) = mpsc::unbounded_channel::<()>();
    let target_cb = target.clone();
    let mut watcher = match notify::recommended_watcher(
        move |res: notify::Result<notify::Event>| {
            let ev = match res {
                Ok(ev) => ev,
                // Surface a watch error (incl. the kernel dropping the watch if the
                // parent dir is removed) rather than swallow it — reload may be
                // degraded until the daemon restarts.
                Err(e) => {
                    tracing::warn!(error = %e, "inventory reload watch error — reload may be degraded until restart");
                    return;
                }
            };
            // The `install`/scp writer produces Create/Modify on the target name; a
            // future temp+rename writer produces a Create of the same name in the
            // dir. Other kinds (access/remove) are ignored. We watch the PARENT dir
            // so an inode-replacing writer is still caught.
            let touches_target = match &target_cb {
                Some(t) => ev
                    .paths
                    .iter()
                    .any(|p| p.file_name() == Some(t.as_os_str())),
                None => true,
            };
            if touches_target && matches!(ev.kind, EventKind::Create(_) | EventKind::Modify(_)) {
                let _ = sig_tx.send(());
            }
        },
    ) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(error = %e, "inventory reload watcher init failed; reload disabled");
            return;
        }
    };
    if let Err(e) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
        tracing::warn!(dir = %dir.display(), error = %e, "inventory reload watch failed; reload disabled");
        return;
    }

    tokio::spawn(async move {
        // Hold the watcher for the task's life (dropping it stops watching).
        let _watcher = watcher;
        while sig_rx.recv().await.is_some() {
            // Debounce: coalesce the create+modify+chmod burst AND let an
            // in-place write finish before we read (half-write protection — a
            // truncated read fails closed to Unverified and is kept-last-good
            // anyway, so this is belt-and-braces).
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            while sig_rx.try_recv().is_ok() {}

            apply_inventory_reload(&state, crate::authority::load_and_verify(&paths)).await;
        }
    });
}

/// Apply one verified reload as a transport-and-authority transaction. The
/// transport fence precedes authority publication; inbound enforce teardown
/// completes before the §17 applied events make the new state externally
/// legible. A kept-last-good load returns before touching transport state.
async fn apply_inventory_reload(state: &AppState, new: crate::authority::Posture) {
    let current = state.authority.load();
    let keep = reload_keeps_last_good(&new, &current.posture);
    drop(current);
    if keep {
        emit_posture_event(&new, true, true);
        return;
    }

    let next = Arc::new(crate::routing::RoutingAuthority::new(
        new,
        &state.node_name,
        &state.wg_ip,
        state.listener_port,
        state.etc_roster.as_ref(),
    ));
    let (verified, epoch) = match &next.posture {
        crate::authority::Posture::Verified(accepted) => (true, accepted.epoch),
        crate::authority::Posture::Unverified { .. } => (false, 0),
    };

    // D1.4 §7.7 ordering fence: transport first, then the coherent authority
    // snapshot, then inbound session teardown, then observable applied events.
    let report = state
        .mesh
        .reconcile_endpoints(next.routes.desired_endpoints(), next.revision())
        .await;
    {
        let _fence = state
            .delivery_fence
            .write()
            .expect("delivery fence poisoned");
        state.authority.store(next.clone());
    }
    if state.admission_mode == AdmissionMode::Enforce
        && let crate::authority::Posture::Verified(accepted) = &next.posture
    {
        reload_revocation_teardown(state, accepted).await;
    }

    emit_posture_event(&next.posture, true, false);
    emit_routing_reload_applied(&report, next.revision());
    emit_admission_posture_event(
        state.admission_mode,
        &state.bind,
        state.wg_bound,
        verified,
        load_d2_seed(&d2_seed_path()).is_none(),
        epoch,
        true,
    );
    emit_listener_endpoint_state(
        &next,
        &state.node_name,
        state.listener_port,
        &state.listener_endpoint_diverged,
        true,
    );
}

fn emit_routing_reload_applied(report: &ReconcileReport, revision: cosmix_mesh::AuthorityRevision) {
    tracing::info!(
        event = "routing.reload_applied",
        epoch = revision.epoch,
        recovery_generation = revision.recovery_generation,
        added = report.added,
        removed = report.removed,
        endpoint_changed = report.endpoint_changed,
        connections_retired = report.connections_retired.len(),
        connections_retained = report.connections_retained,
        "signed routing reload applied after transport fencing"
    );
    for retired in &report.connections_retired {
        tracing::info!(
            event = "mesh.connection.retired",
            peer = %retired.peer,
            endpoint = %retired.endpoint,
            connection_generation = %retired.connection_generation,
            reason = retired.reason.as_str(),
            epoch = revision.epoch,
            "outbound mesh connection retired by routing reload"
        );
    }
    for inflight in &report.inflight_failed {
        tracing::info!(
            event = "delivery.inflight_fate",
            direction = "outbound",
            class = %inflight.class,
            cause = "reload-teardown",
            fate = "fail-fast",
            id = %inflight.message_id,
            peer = %inflight.peer,
            session_id = %inflight.connection_generation,
            epoch = revision.epoch,
            "outbound in-flight request failed by routing reload"
        );
    }
}

/// SPEC 13 §5.5/§9a (2-c-2c) — the membership re-check for an already-admitted
/// session on inventory reload. The session's `claimed_source_node` was
/// cryptographically established at admission; this is NOT a stored-signature
/// re-verify (every inventory change bumps the epoch, so `admit()` would
/// spuriously `EpochMismatch` every session — §5.5). Instead it asks: is the
/// proven member still `active` + `bus:true` + present + holding a current `d2`
/// credential at the NEW epoch? Returns the §17 revoke detail if the session
/// must close, or `None` if the member is still admissible (incl. a key-rotation
/// overlap where `select_d2_pubkeys` returns both keys, and a plain epoch bump
/// with the credential still valid — both grandfather). A removed member maps to
/// `source-tombstoned` (no §17 "removed" value exists; it is no longer a member).
fn reload_revoke_detail(
    member: Option<&serde_json::Value>,
    new_epoch: u64,
) -> Option<&'static str> {
    let Some(m) = member else {
        // No longer in the inventory at all — treat as tombstoned for audit.
        return Some("source-tombstoned");
    };
    if m.get("status").and_then(serde_json::Value::as_str) != Some("active") {
        return Some("source-tombstoned");
    }
    if m.get("bus").and_then(serde_json::Value::as_bool) != Some(true) {
        return Some("source-bus-false");
    }
    if cosmix_mesh_trust::admission::select_d2_pubkeys(m, new_epoch).is_empty() {
        return Some("no-current-d2-credential");
    }
    None
}

/// SPEC 13 §5.5 (2-c-2c) — close every live enforce-admitted session whose
/// member failed the [`reload_revoke_detail`] re-check against the new snapshot
/// `a`. For each: drain its in-flight rpcs and emit one
/// `delivery.inflight_fate{cause:reload-teardown}` each (draining here CLAIMS
/// the entries, so the natural session-drop path emits no duplicate
/// session-churn), emit `admission.refused{reload:true}`, then notify the
/// session's close lever (the read loop breaks → the socket drops). A
/// still-admissible member is untouched (grandfather).
async fn reload_revocation_teardown(state: &AppState, a: &crate::authority::Accepted) {
    // Snapshot the victims under the read lock, then release it before the
    // awaited drains/emits/closes (don't hold the lock across awaits, and a
    // closing session's own cleanup also wants the write lock).
    let victims: Vec<(String, GatedSession, &'static str)> = {
        let live = state.live_sessions.read().await;
        live.iter()
            .filter_map(|(sid, s)| {
                reload_revoke_detail(a.members_full.get(&s.claimed_source_node), a.epoch)
                    .map(|detail| (sid.clone(), s.clone(), detail))
            })
            .collect()
    };
    for (sid, s, detail) in victims {
        // Drain THIS session's in-flight rpcs first (claims them so the
        // natural-drop drain finds none → no double session-churn emit).
        for (broker_id, p) in state.pending_responses.drain_for_channel(&s.tx).await {
            tracing::info!(
                event = "delivery.inflight_fate",
                class = p.class,
                cause = "reload-teardown",
                id = %broker_id,
                caller_id = %p.caller_id,
                fate = "silent-drop",
                duplicate_suppressed = false,
                peer = %s.claimed_source_node,
                session_id = %sid,
                epoch = a.epoch,
                "in-flight request dropped on enforce revocation teardown (§14/§5.5)"
            );
        }
        emit_admission_refused(
            state,
            detail,
            &s.claimed_source_node,
            &s.source_ip,
            a.epoch,
            &sid,
            true,
        );
        // Break the session's read loop; its own cleanup removes the registry
        // entry + the live_sessions entry. Remove here too so a second reload
        // before the loop wakes doesn't re-process it.
        s.close.notify_one();
        state.live_sessions.write().await.remove(&sid);
    }
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let source_ip = peer.ip();
    // Bound the per-message / per-frame size a mesh peer can send before
    // we buffer + parse it. axum's defaults (64 MiB message / 16 MiB
    // frame) are far larger than any legitimate Bus control frame; the
    // ~1 MiB norm (MAX_SNAPSHOT_BYTES) leaves generous headroom at 16/8
    // MiB. Mirrors the native-transport cap (cosmix-lib-bus
    // MAX_MESSAGE_BYTES) so neither ingress path is unbounded.
    ws.max_message_size(WS_MAX_MESSAGE_BYTES)
        .max_frame_size(bus::WS_MAX_FRAME_BYTES)
        .on_upgrade(move |socket| handle_socket(socket, state, source_ip))
}

async fn handle_socket(socket: WebSocket, state: AppState, source_ip: std::net::IpAddr) {
    let (mut ws_sink, mut ws_stream) = socket.split();
    // The source address as a string for §17 event fields (2-c-2a) — the
    // trustworthy correlator on a would-refuse and the same-node classifier
    // input for the enforce gate (2-c-2b).
    let source_ip_str = source_ip.to_string();

    let (tx, mut rx) = mpsc::channel::<String>(PEER_OUTBOUND_BUFFER);

    let send_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if ws_sink.send(Message::Text(msg.into())).await.is_err() {
                break;
            }
        }
    });

    // Synthesized connection-scoped identity for anonymous publishers
    // (§ 3.11.1 of the topic pub/sub delta). Used as the publisher identity
    // for topic operations when the peer has not called `noded.register`.
    // Dies with the connection; not federated.
    let anon_id = subscription::synth_anon_id();
    let mut service_name: Option<String> = None;

    // SPEC 13 §9a (2-c-2b) — the per-session admission record, folded from each
    // admit response and consulted by the register-time enforce gate below.
    let mut session_adm = SessionAdmission::default();

    // SPEC 13 §9a (2-c-2c) — the close lever for the enforce revocation
    // teardown. The read loop `select!`s on it; the reload watcher calls
    // `notify_one()` to break the loop and drop a revoked member's session.
    let close_signal = Arc::new(tokio::sync::Notify::new());

    // SPEC 13 §9a (2-c-1b) — broker-speaks-first: when admission is enabled the
    // broker's FIRST frame is a D2 challenge. Non-blocking + additive — an
    // un-upgraded peer ignores the unknown `noded.admit.challenge` request and
    // registers as before; the read loop never awaits a response. The challenge
    // rides the per-session `tx`, so it serialises ahead of any later reply.
    // `challenge_id` is held only to reap the entry on socket close.
    let mut challenge_id: Option<String> = None;
    if state.admission_mode != AdmissionMode::Off {
        // Snapshot the epoch + mesh, then DROP the authority guard before the
        // awaited `issue()` (don't pin the Arc across the await).
        let inputs = match &state.authority.load().posture {
            crate::authority::Posture::Verified(a) => Some((a.epoch, a.mesh.clone())),
            crate::authority::Posture::Unverified { .. } => None,
        };
        if let Some((epoch, mesh)) = inputs
            && let Some(issued) = state
                .challenge_table
                .issue(epoch, &mesh, &state.node_name)
                .await
        {
            let mut frame = BusMessage::new()
                .with_header("command", "noded.admit.challenge")
                .with_header("type", "request")
                .with_header("from", &state.node_name)
                .with_header("id", &issued.id);
            frame.body = issued.body;
            let _ = tx.try_send(frame.to_wire());
            challenge_id = Some(issued.id);
        }
    }

    loop {
        // SPEC 13 §9a (2-c-2c) — read the next frame OR honour a reload-teardown
        // close. `biased` checks the close first so a pending teardown wins
        // promptly. A fresh `notified()` future each iteration is safe: a
        // `notify_one()` that lands between iterations stores one permit the next
        // `notified()` consumes immediately (no missed close).
        let msg = tokio::select! {
            biased;
            _ = close_signal.notified() => break,
            next = ws_stream.next() => match next {
                Some(Ok(m)) => m,
                // Stream closed or errored — same as the old `while let` exit.
                _ => break,
            },
        };
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Close(_) => break,
            _ => continue,
        };

        let mut bus_msg = match bus::parse(&text) {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!("Invalid Bus message: {e}");
                let err = BusMessage::new()
                    .with_header("rc", "10")
                    .with_header("error", &format!("Invalid Bus message: {e}"));
                let _ = tx.try_send(err.to_wire());
                continue;
            }
        };

        // SPEC 13 §9a (2-c-1b) — the D2 admission response. It carries
        // `type:response`, so it MUST be intercepted by command HERE, before the
        // pending-response orphan path below would `take()`-and-drop it.
        if bus_msg.command_name() == Some("noded.admit.response") {
            let outcome = process_admit_response(
                &state,
                &bus_msg,
                &anon_id,
                &source_ip_str,
                challenge_id.as_deref(),
            )
            .await;
            let observing = state.observe.is_active();
            let observed_outcome = if observing {
                Some(if matches!(outcome, Some(AdmitOutcome::Admit(_))) {
                    ObserveOutcome::BrokerHandled
                } else {
                    ObserveOutcome::Rejected
                })
            } else {
                None
            };
            if let Some(outcome) = outcome {
                session_adm.apply(outcome);
            }
            if let Some(observed_outcome) = observed_outcome {
                canonicalize_connection_from(&mut bus_msg, service_name.as_deref());
                state.observe.observe(Observation::canonical(
                    ObserveDirection::Local,
                    observed_outcome,
                    &bus_msg,
                    bus_msg.get("id"),
                ));
            }
            continue;
        }

        // Check if this is a response to a pending request. Responses MUST be
        // correlated by id AND recipient connection — never fall through to the command dispatcher,
        // because a `type=response` whose `command` matches a broker verb (e.g.
        // `topic.publish`) would otherwise be re-dispatched as a fresh request,
        // closing a feedback loop with the original publisher (observed
        // indexd↔noded amplification fixed 2026-05-04).
        //
        // The `id` on the response wire is the broker-local `noded-<u64>`
        // rewritten by `route_local` on the forward leg. We `take()` the
        // pending entry (single-step under the write lock), then restore the
        // caller's original id on the message before forwarding so the caller
        // correlates against the id it issued — not the broker's internal one.
        if bus_msg.message_type() == Some("response") {
            if let Some(id) = bus_msg.get("id").map(|s| s.to_string()) {
                if let Some(pending) = state.pending_responses.take_response(&id, &tx).await {
                    let wire = canonicalize_correlated_response(
                        &mut bus_msg,
                        &pending.caller_id,
                        service_name.as_deref(),
                        pending.caller_service.as_deref(),
                        broker_origin_for_delivery(source_ip, &state.bind),
                    );
                    if state.observe.is_active() {
                        let outcome = match pending.caller_tx.try_send(wire.clone()) {
                            Ok(()) => ObserveOutcome::Delivered,
                            Err(mpsc::error::TrySendError::Full(_)) => ObserveOutcome::Dropped,
                            Err(mpsc::error::TrySendError::Closed(_)) => ObserveOutcome::Rejected,
                        };
                        state.observe.observe(Observation::from_message(
                            ObserveDirection::Local,
                            outcome,
                            &bus_msg,
                            &wire,
                            Some(&pending.observer_correlation_id),
                        ));
                    } else {
                        let _ = pending.caller_tx.try_send(wire);
                    }
                    continue;
                }
                tracing::debug!(
                    id = %id,
                    cmd = bus_msg.command_name().unwrap_or("?"),
                    from = bus_msg.from_addr().unwrap_or("?"),
                    "Dropping orphan response (no pending caller)"
                );
            } else {
                tracing::debug!(
                    cmd = bus_msg.command_name().unwrap_or("?"),
                    from = bus_msg.from_addr().unwrap_or("?"),
                    "Dropping orphan response (no id)"
                );
            }
            if state.observe.is_active() {
                canonicalize_connection_from(&mut bus_msg, service_name.as_deref());
                state.observe.observe(Observation::canonical(
                    ObserveDirection::Local,
                    ObserveOutcome::Rejected,
                    &bus_msg,
                    bus_msg.get("id"),
                ));
            }
            continue;
        }

        let target = bus_msg.to_addr().unwrap_or("noded").to_string();

        // Resolve target shape BEFORE deciding whether to canonicalise
        // `from`. `noded.*` admin commands (notably `noded.register`)
        // read the caller-supplied `from` header as their parameter
        // payload — rewriting it before `handle_noded_command` would
        // (a) break anonymous registration (strips the requested name)
        // and (b) prevent a registered connection from re-registering
        // under a different alias (overwrites the requested name with
        // the current registration). Codex C10c-pre rev-2 MAJOR. Both
        // the plain `to: noded` spelling and the Bus-address spelling
        // `to: noded.<node>.bus` resolve to local noded; the only
        // difference is whether `BusTarget::parse` matches. So we
        // walk the address resolution once, then dispatch.
        enum Route<'a> {
            LocalNoded,
            LocalService(&'a str),
            RemoteMesh { target: crate::routing::RouteTarget },
            UnknownMeshNode { node: String },
            CrossMeshRefused { addr: String },
            MalformedAddress { addr: String },
        }
        let parsed = BusTarget::parse(&target);
        let route: Route<'_> = if target == "noded" {
            Route::LocalNoded
        } else {
            match &parsed {
                Some(BusTarget::CrossMesh { .. }) => Route::CrossMeshRefused {
                    addr: target.clone(),
                },
                Some(BusTarget::Local(addr)) => {
                    if !addr.is_for_node(&state.node_name) {
                        let node = addr.node.clone();
                        let resolved = state.authority.load().routes.resolve(&node);
                        if let Some(target) = resolved {
                            Route::RemoteMesh { target }
                        } else {
                            Route::UnknownMeshNode { node }
                        }
                    } else {
                        // `<node>.bus` is the broker form per SPEC 01
                        // §4.1: implicit service is `noded`. Without
                        // this default the broker form would dispatch
                        // to a (never-registered) service literally
                        // named after the node.
                        let local_service = addr.service.as_deref().unwrap_or("noded");
                        if local_service == "noded" {
                            Route::LocalNoded
                        } else {
                            Route::LocalService(local_service)
                        }
                    }
                }
                None => {
                    // Per SPEC 01 §4.1, a bare label (no `.` and no `@`)
                    // is local-broker registry shorthand. Anything else
                    // that fails to parse is a malformed Bus address —
                    // refuse rather than falling back to local-service
                    // lookup, which would let an arbitrarily-named
                    // service (e.g. one that registered as `a.b.c.d.bus`
                    // or the legacy `maild@delta`) collect mis-shaped
                    // traffic that the parser already rejected.
                    if target.contains('.') || target.contains('@') {
                        Route::MalformedAddress {
                            addr: target.clone(),
                        }
                    } else {
                        Route::LocalService(&target)
                    }
                }
            }
        };

        // Canonicalise `from` ONLY for paths that hand the message off
        // to another service. SPEC 12 §15.5 / C10c BLOCKER: the wire
        // `from` is caller-supplied; the broker-authenticated identity
        // is `service_name` (set under noded.register's collision
        // gate, noded.rs:748) for registered peers, else absent. A
        // peer "alice" could otherwise send `from: webd` and trigger
        // any `service_name`-keyed auth policy (e.g. SPEC 12 property
        // caps) to authorise as webd, or have noded grant webd a live
        // subscription the real webd never asked for. Registered
        // connections get `from: <service_name>`; anonymous
        // connections have the header removed entirely so the
        // receiving service's `service_name` resolves to `None`.
        //
        // SPEC 13 §9a (2-c-2d) — set when the enforce gate refuses an inter-node
        // register; the session is then fully CLOSED below (not merely denied a
        // name), so a non-member/tombstoned peer can't keep an open socket to hit
        // un-authed builtins or route as anonymous.
        let mut close_after_refuse = false;
        match route {
            Route::LocalNoded => {
                // SPEC 13 §9a (2-c-2b) — the register-time enforce gate. Per
                // register (a connection can re-register / alias), and ONLY
                // under `enforce`: off/observe never refuse. The gate lives HERE
                // (not in handle_noded_command) because the source_ip + the
                // per-session admission record are session-scoped. Same-node
                // citizens are ungated; an inter-node bridge must have proved
                // the member it now registers as.
                let mut refused = false;
                if state.admission_mode == AdmissionMode::Enforce
                    && bus_msg.command_name() == Some("noded.register")
                {
                    let from = bus_msg.from_addr().unwrap_or("").to_string();
                    let (verified, epoch) = match &state.authority.load().posture {
                        crate::authority::Posture::Verified(a) => (true, a.epoch),
                        crate::authority::Posture::Unverified { .. } => (false, 0),
                    };
                    match register_gate_decision(
                        source_ip,
                        &state.bind,
                        state.admission_mode,
                        state.wg_bound,
                        verified,
                        &from,
                        &session_adm,
                    ) {
                        RegisterGate::Ungated => {
                            tracing::debug!(
                                from = %from,
                                "enforce: same-node register, ungated (§4 local citizen)"
                            );
                        }
                        RegisterGate::Admit(node) => {
                            emit_admission_admitted(&state, &node, &source_ip_str, epoch, &anon_id);
                            // SPEC 13 §5.5 (2-c-2c) — track this live gated
                            // session so a revocation reload can close it. Keyed
                            // by session_id; a re-register refreshes the entry.
                            state.live_sessions.write().await.insert(
                                anon_id.clone(),
                                GatedSession {
                                    claimed_source_node: node,
                                    source_ip: source_ip_str.clone(),
                                    tx: tx.clone(),
                                    close: close_signal.clone(),
                                },
                            );
                        }
                        RegisterGate::Refuse {
                            detail,
                            source_node,
                            synth_observed,
                        } => {
                            // A session that never answered gets its observed
                            // record HERE (process_admit_response only fires on
                            // a response). unproven = no proof; would-refuse =
                            // a proof was seen but rejected.
                            if synth_observed {
                                let verdict = if detail == "no-proof" {
                                    "unproven"
                                } else {
                                    "would-refuse"
                                };
                                emit_admission_observed(
                                    &state,
                                    verdict,
                                    detail,
                                    &source_node,
                                    &source_ip_str,
                                    &anon_id,
                                    epoch,
                                    "",
                                );
                            }
                            emit_admission_refused(
                                &state,
                                detail,
                                &source_node,
                                &source_ip_str,
                                epoch,
                                &anon_id,
                                false,
                            );
                            let mut resp = BusMessage::new()
                                .with_header("type", "response")
                                .with_header("from", "noded")
                                .with_header("rc", "10")
                                .with_header("error", &format!("admission refused: {detail}"));
                            if let Some(id) = bus_msg.get("id") {
                                resp.set("id", id);
                            }
                            if let Some(service) = service_name.as_deref() {
                                resp.set("to", service);
                            }
                            let observing = state.observe.is_active();
                            if observing {
                                canonicalize_connection_from(&mut bus_msg, service_name.as_deref());
                                state.observe.observe(Observation::canonical(
                                    ObserveDirection::Local,
                                    ObserveOutcome::Rejected,
                                    &bus_msg,
                                    bus_msg.get("id"),
                                ));
                            }
                            let response_wire = resp.to_wire();
                            if observing {
                                let response_outcome = match tx.try_send(response_wire.clone()) {
                                    Ok(()) => ObserveOutcome::Delivered,
                                    Err(mpsc::error::TrySendError::Full(_)) => {
                                        ObserveOutcome::Dropped
                                    }
                                    Err(mpsc::error::TrySendError::Closed(_)) => {
                                        ObserveOutcome::Rejected
                                    }
                                };
                                state.observe.observe(Observation::from_message(
                                    ObserveDirection::Local,
                                    response_outcome,
                                    &resp,
                                    &response_wire,
                                    bus_msg.get("id"),
                                ));
                            } else {
                                let _ = tx.try_send(response_wire);
                            }
                            refused = true;
                            // 2-c-2d: full isolation — close the session, don't
                            // just deny the name (the §9a finding).
                            close_after_refuse = true;
                        }
                    }
                }
                if !refused {
                    let mut command_outcome = ObserveOutcome::BrokerHandled;
                    handle_noded_command(
                        &bus_msg,
                        &tx,
                        &state,
                        &mut service_name,
                        &anon_id,
                        source_ip,
                        &mut command_outcome,
                    )
                    .await;
                    // The command handler consumes the caller's original
                    // `from` (notably for register). Rewriting it afterwards
                    // exists only to produce canonical observation metadata,
                    // so zero-subscriber command handling stops at this one
                    // relaxed load.
                    if state.observe.is_active() {
                        match service_name.as_deref() {
                            Some(service) => bus_msg.set("from", service),
                            None => {
                                bus_msg.headers.remove("from");
                            }
                        }
                        state.observe.observe(Observation::canonical(
                            ObserveDirection::Local,
                            command_outcome,
                            &bus_msg,
                            bus_msg.get("id"),
                        ));
                    }
                }
            }
            Route::LocalService(local_service) => {
                let mesh_from = bus_msg
                    .get(crate::subscription::MESH_FROM_HEADER)
                    .map(str::to_owned);
                let observing = state.observe.is_active();
                let correlation_id = if observing {
                    bus_msg.get("id").map(ToString::to_string)
                } else {
                    None
                };
                canonicalize_routed_from_in_place(&mut bus_msg, service_name.as_deref());
                let origin = broker_origin_for_delivery(source_ip, &state.bind);
                stamp_broker_origin(&mut bus_msg, origin);
                let canonical_text = bus_msg.to_wire();
                // `route_local` mutates `bus_msg`'s `id` to the broker-local
                // rewrite. Use the wire bytes it returns (id-rewritten) for
                // the tap so observers see exactly what the target service
                // saw — not the caller's pre-rewrite id, which would
                // disagree with the response wire the broker emits.
                let route_result = route_local(
                    &state,
                    local_service,
                    &mut bus_msg,
                    &tx,
                    service_name.as_deref(),
                    source_ip,
                    &session_adm,
                    mesh_from.as_deref(),
                )
                .await;
                let observed_wire = route_result
                    .forwarded_wire
                    .as_deref()
                    .unwrap_or(&canonical_text);
                broadcast_tap(
                    &state.tap_subscribers,
                    observed_wire,
                    route_result.target_tx.as_ref(),
                )
                .await;
                if observing {
                    state.observe.observe(Observation::from_message(
                        ObserveDirection::Local,
                        route_result.outcome,
                        &bus_msg,
                        observed_wire,
                        correlation_id.as_deref(),
                    ));
                }
            }
            Route::RemoteMesh { target } => {
                canonicalize_routed_from_in_place(&mut bus_msg, service_name.as_deref());
                // Only a local registered source acquires a direct-hop service
                // assertion. A relay cannot borrow this node's clipboard grant.
                if is_same_node_origin(source_ip, &state.bind)
                    && let Some(service) = service_name.as_deref()
                    && valid_service_name(service)
                    && state
                        .registry
                        .read()
                        .await
                        .get(service)
                        .is_some_and(|entry| entry.same_channel(&tx))
                {
                    bus_msg.set(crate::subscription::MESH_FROM_HEADER, service);
                }
                // The request clone and canonical wire have no delivery
                // consumer: retain them only while observation is active.
                // With no subscribers this relaxed load is the complete
                // observation cost of mesh egress.
                let pending_observation = if state.observe.is_active() {
                    Some(PendingMeshObservation {
                        manager: state.observe.clone(),
                        request: bus_msg.clone(),
                        canonical_wire: bus_msg.to_wire(),
                        correlation_id: bus_msg.get("id").map(ToString::to_string),
                    })
                } else {
                    None
                };
                let tx_clone = tx.clone();
                let mesh = state.mesh.clone();
                let peer = target.into_peer();
                let msg_id = bus_msg.get("id").map(|s| s.to_string());
                tokio::spawn(async move {
                    match mesh.call(peer, bus_msg).await {
                        Ok(mut resp) => {
                            if let Some(observation) = pending_observation.as_ref() {
                                observation.manager.observe(Observation::from_message(
                                    ObserveDirection::MeshOut,
                                    ObserveOutcome::Delivered,
                                    &observation.request,
                                    &observation.canonical_wire,
                                    observation.correlation_id.as_deref(),
                                ));
                            }
                            stamp_broker_origin(&mut resp, BrokerOrigin::Mesh);
                            let response_wire = resp.to_wire();
                            if let Some(observation) = pending_observation.as_ref() {
                                let outcome = match tx_clone.try_send(response_wire.clone()) {
                                    Ok(()) => ObserveOutcome::Delivered,
                                    Err(mpsc::error::TrySendError::Full(_)) => {
                                        ObserveOutcome::Dropped
                                    }
                                    Err(mpsc::error::TrySendError::Closed(_)) => {
                                        ObserveOutcome::Rejected
                                    }
                                };
                                observation.manager.observe(Observation::from_message(
                                    ObserveDirection::MeshIn,
                                    outcome,
                                    &resp,
                                    &response_wire,
                                    observation.correlation_id.as_deref(),
                                ));
                            } else {
                                let _ = tx_clone.try_send(response_wire);
                            }
                        }
                        Err(e) => {
                            if let Some(observation) = pending_observation.as_ref() {
                                observation.manager.observe(Observation::from_message(
                                    ObserveDirection::MeshOut,
                                    ObserveOutcome::Rejected,
                                    &observation.request,
                                    &observation.canonical_wire,
                                    observation.correlation_id.as_deref(),
                                ));
                            }
                            let mut err = BusMessage::new()
                                .with_header("rc", "10")
                                .with_header("type", "response")
                                .with_header("error", &format!("Mesh bridge error: {e}"));
                            if let Some(ref id) = msg_id {
                                err.set("id", id);
                            }
                            if let Some(observation) = pending_observation.as_ref() {
                                deliver_observed_response(
                                    &observation.manager,
                                    &tx_clone,
                                    &err,
                                    ObserveDirection::MeshIn,
                                    msg_id.as_deref(),
                                );
                            } else {
                                let _ = tx_clone.try_send(err.to_wire());
                            }
                        }
                    }
                });
            }
            Route::UnknownMeshNode { node } => {
                let mut err = BusMessage::new()
                    .with_header("rc", "10")
                    .with_header("type", "response")
                    .with_header("error", &format!("Unknown mesh node: '{node}'"));
                if let Some(id) = bus_msg.get("id") {
                    err.set("id", id);
                }
                observe_rejected_local_route(&state, &mut bus_msg, service_name.as_deref());
                deliver_observed_response(
                    &state.observe,
                    &tx,
                    &err,
                    ObserveDirection::Local,
                    bus_msg.get("id"),
                );
            }
            Route::CrossMeshRefused { addr } => {
                // SPEC 01 §4.2 cross-mesh routing is reserved-but-refused at
                // both ends until federation transport (and the SPEC 10
                // remote-service identity model) is designed. Refuse at the
                // egress router with a clear error so callers don't silently
                // get fallback delivery to a same-named local service.
                let mut err = BusMessage::new()
                    .with_header("rc", "10")
                    .with_header("type", "response")
                    .with_header(
                        "error",
                        &format!("cross-mesh routing not implemented: '{addr}'"),
                    );
                if let Some(id) = bus_msg.get("id") {
                    err.set("id", id);
                }
                observe_rejected_local_route(&state, &mut bus_msg, service_name.as_deref());
                deliver_observed_response(
                    &state.observe,
                    &tx,
                    &err,
                    ObserveDirection::Local,
                    bus_msg.get("id"),
                );
            }
            Route::MalformedAddress { addr } => {
                // The target *looks* like an Bus address (it contains
                // `.` or `@`) but the parser rejected it. Refuse rather
                // than falling back to local-service lookup — see the
                // route-resolution comment above.
                let mut err = BusMessage::new()
                    .with_header("rc", "10")
                    .with_header("type", "response")
                    .with_header("error", &format!("Invalid Bus target: '{addr}'"));
                if let Some(id) = bus_msg.get("id") {
                    err.set("id", id);
                }
                observe_rejected_local_route(&state, &mut bus_msg, service_name.as_deref());
                deliver_observed_response(
                    &state.observe,
                    &tx,
                    &err,
                    ObserveDirection::Local,
                    bus_msg.get("id"),
                );
            }
        }
        // SPEC 13 §9a (2-c-2d) — a refused inter-node register fully isolates the
        // session: close the socket (break the read loop → cleanup below) instead
        // of leaving it open to un-authed builtins / anonymous routing. The rc=10
        // was already queued on `tx`; the awaits in cleanup let `send_task` flush
        // it before the socket drops (best-effort, as for any close).
        if close_after_refuse {
            tracing::debug!("enforce: closing refused inter-node session (§9a 2-c-2d)");
            break;
        }
    }

    // SPEC 13 §5.5 (2-c-2c) — drop this session from the live gated set. A
    // reload teardown that selected this session already drained its pending +
    // removed it (so the §14 drain below finds none → no duplicate fate emit);
    // a natural disconnect removes it here. Harmless no-op for an untracked
    // (same-node / observe / never-admitted) session.
    state.live_sessions.write().await.remove(&anon_id);

    // SPEC 13 §14 / §7.8 B3 — delivery-on-reconnect instrumentation (slice
    // 2-c-0, posture `off`). Any request this session had in flight to a local
    // service is abandoned on drop: its reply will never reach the now-dead
    // caller (silent-drop). Record one `delivery.inflight_fate{cause:session-churn}`
    // per abandoned request so the §14 natural-churn delivery contract is
    // characterised before B3 closes. The per-entry `class` (captured at
    // register) is reported as-is — the table holds only id-bearing messages
    // (id-less fire-and-forget `control` frames never enter it). `cause` is
    // session-churn only (no admission-refused / reload-teardown until enforce,
    // §14); `id` is the broker-local `noded-<u64>` correlator; idempotency is
    // unsupported in 2-c-0, so the schema's `idempotency_key` is omitted.
    //
    // Known bounded undercount: if a reply `take()`s an entry in the narrow
    // window between this session's read loop exiting and this drain running,
    // that entry is gone from the table and not counted here — its reply is
    // discarded when `send_task` is aborted below, but the caller's receiver is
    // still alive in that window so the responder cannot reliably distinguish it
    // (a `TrySendError::Closed` only appears after the abort). Detecting it would
    // need an explicit per-session closed flag in every pending entry — outsized
    // for a microsecond race in best-effort characterisation telemetry, so it is
    // documented rather than chased. The dominant population (an rpc in flight
    // when the caller drops, reply not yet arrived) is captured reliably here.
    let peer_label = service_name.as_deref().unwrap_or(&anon_id);
    let drop_epoch = match &state.authority.load().posture {
        crate::authority::Posture::Verified(a) => a.epoch,
        crate::authority::Posture::Unverified { .. } => 0,
    };
    for (broker_id, p) in state.pending_responses.drain_for_channel(&tx).await {
        tracing::info!(
            event = "delivery.inflight_fate",
            class = p.class,
            cause = "session-churn",
            id = %broker_id,
            caller_id = %p.caller_id,
            fate = "silent-drop",
            duplicate_suppressed = false,
            peer = %peer_label,
            session_id = %anon_id,
            epoch = drop_epoch,
            "in-flight request abandoned on session drop (§14 B3 churn instrumentation)"
        );
    }

    // SPEC 02 §4.2: observation is connection-owned, never retained across a
    // reconnect. This also purges every queued event before the socket writer
    // is aborted below.
    state.observe.remove_owner(&tx);

    // Broker cleanup: use the peer's stable identity (registered service name
    // if any, else the synthesized anon id) so subscription removal matches
    // what was used on registration, AND scope teardown to THIS connection's
    // channel (`&tx`) so a name a newer connection re-registered (after a
    // route_local prune or citizen restart) is not collaterally torn down —
    // the SPEC 12 §15.5 impersonation window on the subscription side.
    // Dispatch any topic.idle notifications that fire as a result.
    let peer_id = service_name.as_deref().unwrap_or(&anon_id);
    let idle_notices = state.broker.remove_peer(peer_id, &tx).await;
    dispatch_notifications(&state, &idle_notices).await;

    if let Some(name) = &service_name {
        // Same-channel guard: a new connection may already have taken
        // over this name (after the prior route_local cleanup removed
        // our stale entry and another peer legitimately re-registered).
        // Removing by name alone would silently strip the new owner —
        // reopening the SPEC 12 §15.5 impersonation window.
        let mut reg_w = state.registry.write().await;
        let removed = if reg_w
            .get(name)
            .map(|cur| cur.same_channel(&tx))
            .unwrap_or(false)
        {
            reg_w.remove(name).is_some()
        } else {
            false
        };
        drop(reg_w);
        if removed {
            tracing::info!("Service '{}' disconnected", name);
            emit_props_change(&state, &format!("disconnect:{name}")).await;
        }
    }

    // SPEC 13 §9a (2-c-1b) — free this session's outstanding challenge if it
    // was never answered (a no-op if already taken). Bounds the table.
    if let Some(id) = &challenge_id {
        state.challenge_table.reap(id).await;
    }

    send_task.abort();
}

/// SPEC 07 §3 — re-collect the noded props snapshot and let the change bus
/// emit `props.changed` events for any leaves that moved since the last
/// observation. Called at every known mutation site (register, disconnect).
async fn emit_props_change(state: &AppState, cause: &str) {
    let snap = crate::props::collect(
        state.started,
        &state.started_at,
        &state.bind,
        &state.node_name,
        &state.log_level,
        &state.registry,
        &state.broker,
    )
    .await;
    state.change_bus.observe(&snap, cause).await;
}

/// Route broker-emitted notifications (topic.active / topic.idle) to their
/// target peers. Looks up each target in the registry; for anonymous
/// publishers the target_peer is their synthesized anon id, which will NOT
/// be in the registry (anonymous peers aren't registered) — those
/// notifications are dropped silently, which is correct because the anon
/// peer's connection is addressed by the tx handle the broker already holds
/// via last_publisher_tx, not via the registry. Registered publishers hit
/// the registry path.
async fn dispatch_notifications(_state: &AppState, notices: &[Notification]) {
    if notices.is_empty() {
        return;
    }
    // Deliver directly via the notification's captured Sender. This works
    // uniformly for registered services AND anonymous publishers — both
    // have their outbound channel captured in `target_tx` at the moment
    // the notification was built by the broker, so no registry lookup is
    // needed. The previous implementation used `registry.get(target_peer)`
    // which silently dropped notifications for anonymous publishers (they
    // are never in the registered-services map), breaking topic.idle and
    // topic.active delivery for the entire anonymous-publisher use case.
    //
    // `try_send` may fail on slow-consumer (channel full) or closed-peer;
    // both are logged but non-fatal. The sender is an unbounded/bounded
    // mpsc::Sender and a failure here means the target peer is either
    // dropped or wedged — in either case there's nothing more this path
    // can do, and the peer will be cleaned up on its next disconnect.
    for notice in notices {
        if let Err(e) = notice.target_tx.try_send(notice.wire.clone()) {
            tracing::warn!(
                target_peer = %notice.target_peer,
                error = ?e,
                "Failed to dispatch notification to peer"
            );
        }
    }
}

/// SPEC 12 §15.5 / C10c BLOCKER — defense-in-depth wire-trust gate.
///
/// Rewrites the `from` header to the broker-authenticated connection
/// identity before the message is forwarded to another service:
///
/// - `Some(peer)`: connection registered via `noded.register`; the
///   wire `from` (whatever the caller put there) is replaced with
///   `peer`. The receiving service can then trust
///   `IncomingCommand.from` as the authenticated identity.
/// - `None`: anonymous connection (no `noded.register`); the `from`
///   header is **removed entirely** rather than rewritten to the
///   synthesised `anon-*` id. Two reasons:
///   1. The receiving service's `PeerIdentity::service_name` is set
///      from `IncomingCommand.from` (e.g. `cosmix-maild::bus::mod`'s
///      `dispatch_props`); leaving an `anon-*` string there would
///      look like a registered service to `service_name`-keyed auth
///      policies and to the C10c granter's `target_peer` derivation.
///   2. The right primitive for "give me a not-empty identity for
///      this anonymous connection" is the connection-scoped `anon_id`
///      already used by broker-internal operations
///      (`subscription::synth_anon_id`); rewriting `from` to that
///      would conflate transport identity with application identity.
///
/// Re-serialises `msg` to a fresh wire string so `route_local`'s raw
/// forward and `broadcast_tap`'s observers all see the canonical
/// `from`. The original `text` is returned only when the message
/// already lacked a `from` header and we're in the anonymous case —
/// avoids a redundant round-trip through `to_wire` for the common
/// well-formed-anonymous case.
#[cfg(test)]
fn canonicalize_routed_from(
    msg: &mut BusMessage,
    peer_id: Option<&str>,
    original_text: &str,
) -> String {
    let origin_removed = strip_broker_origin(msg);
    match peer_id {
        Some(reg) => {
            msg.set("from", reg);
            msg.to_wire()
        }
        None => {
            if msg.headers.remove("from").is_some() || origin_removed {
                msg.to_wire()
            } else {
                original_text.to_string()
            }
        }
    }
}

/// Canonicalise routed identity without serialising. Mesh routing consumes an
/// [`BusMessage`] rather than wire text, so serialising here would exist solely
/// to feed observation. Callers that need observation serialise only after
/// their [`ObserveManager::is_active`] gate.
fn canonicalize_routed_from_in_place(msg: &mut BusMessage, peer_id: Option<&str>) {
    strip_broker_origin(msg);
    match peer_id {
        Some(service) => msg.set("from", service),
        None => {
            msg.headers.remove("from");
        }
    }
}

fn canonicalize_connection_from(message: &mut BusMessage, service_name: Option<&str>) {
    strip_broker_origin(message);
    match service_name {
        Some(service) => message.set("from", service),
        None => {
            message.headers.remove("from");
        }
    }
}

fn broker_origin_for_delivery(source_ip: std::net::IpAddr, bind: &str) -> BrokerOrigin {
    if is_same_node_origin(source_ip, bind) {
        BrokerOrigin::Local
    } else {
        BrokerOrigin::Mesh
    }
}

/// Caller holds delivery_fence through enqueue and the registry read lock
/// proving that the registered bridge still belongs to this connection.
fn admitted_delivery_peer(
    state: &AppState,
    source_ip: std::net::IpAddr,
    registered: Option<&str>,
    admission: &SessionAdmission,
) -> Option<String> {
    let authority = state.authority.load();
    let crate::authority::Posture::Verified(accepted) = &authority.posture else {
        return None;
    };
    let RegisterGate::Admit(peer) = register_gate_decision(
        source_ip,
        &state.bind,
        state.admission_mode,
        state.wg_bound,
        true,
        registered?,
        admission,
    ) else {
        return None;
    };
    reload_revoke_detail(accepted.members_full.get(&peer), accepted.epoch)
        .is_none()
        .then_some(peer)
}

fn observe_rejected_local_route(
    state: &AppState,
    message: &mut BusMessage,
    service_name: Option<&str>,
) {
    if !state.observe.is_active() {
        return;
    }
    canonicalize_connection_from(message, service_name);
    state.observe.observe(Observation::canonical(
        ObserveDirection::Local,
        ObserveOutcome::Rejected,
        message,
        message.get("id"),
    ));
}

fn canonicalize_correlated_response(
    message: &mut BusMessage,
    caller_id: &str,
    responder_service: Option<&str>,
    caller_service: Option<&str>,
    responder_origin: BrokerOrigin,
) -> String {
    message.set("id", caller_id);
    match responder_service {
        Some(service) => message.set("from", service),
        None => {
            message.headers.remove("from");
        }
    }
    match caller_service {
        Some(service) => message.set("to", service),
        None => {
            message.headers.remove("to");
        }
    }
    stamp_broker_origin(message, responder_origin);
    message.to_wire()
}

fn deliver_observed_response(
    observe: &ObserveManager,
    target: &mpsc::Sender<String>,
    response: &BusMessage,
    direction: ObserveDirection,
    correlation_id: Option<&str>,
) -> ObserveOutcome {
    let observing = observe.is_active();
    let wire = response.to_wire();
    if observing {
        let outcome = match target.try_send(wire.clone()) {
            Ok(()) => ObserveOutcome::Delivered,
            Err(mpsc::error::TrySendError::Full(_)) => ObserveOutcome::Dropped,
            Err(mpsc::error::TrySendError::Closed(_)) => ObserveOutcome::Rejected,
        };
        observe.observe(Observation::from_message(
            direction,
            outcome,
            response,
            &wire,
            correlation_id,
        ));
        outcome
    } else {
        match target.try_send(wire) {
            Ok(()) => ObserveOutcome::Delivered,
            Err(mpsc::error::TrySendError::Full(_)) => ObserveOutcome::Dropped,
            Err(mpsc::error::TrySendError::Closed(_)) => ObserveOutcome::Rejected,
        }
    }
}

/// Route a message to a registered local service. Returns the `Sender` the
/// broker tried to deliver to (whether `Ok` or `Full` — both reference the same
/// peer's channel) so the caller can pass it to `broadcast_tap` as a
/// dedup-exclude, plus the wire bytes actually delivered (id-rewritten when
/// the message carried an `id`, identical to the caller's wire otherwise) so
/// the tap observes what the target observed. The wire is only returned on
/// successful delivery (`Ok`); on `Full`/`Closed` the target did *not* receive
/// these bytes, so the wire is `None` and the caller falls back to the
/// canonical pre-route wire for the tap — observers must never see bytes the
/// target didn't. Returns `(None, None)` when the service is unknown or just
/// disconnected.
///
/// `msg` is mutated in place: a successful registry lookup with an `id`
/// rewrites `id` to a broker-local `noded-<u64>`. The response path
/// (`handle_socket`'s response branch) takes the entry by that broker id and
/// restores the caller's original id on the reply wire before forwarding —
/// so multiple anonymous callers (each starting `NodedClient::next_id` at 1)
/// don't collide on the broker's pending-response map. See
/// [`PendingResponse`] for the SPEC 18 Phase 2 WS5 incident that uncovered
/// this.
struct LocalRouteResult {
    target_tx: Option<mpsc::Sender<String>>,
    forwarded_wire: Option<String>,
    outcome: ObserveOutcome,
}

#[allow(clippy::too_many_arguments)]
async fn route_local(
    state: &AppState,
    service: &str,
    msg: &mut BusMessage,
    caller_tx: &mpsc::Sender<String>,
    caller_service: Option<&str>,
    source_ip: std::net::IpAddr,
    admission: &SessionAdmission,
    mesh_from: Option<&str>,
) -> LocalRouteResult {
    let registry = &state.registry;
    let pending_responses = &state.pending_responses;
    let observe = &state.observe;
    let reg = registry.read().await;
    if let Some(target_tx) = reg.get(service).map(|e| e.tx.clone()) {
        // Register pending BEFORE rewriting the wire bytes so the
        // broker_id we insert under matches the id we serialise into the
        // forwarded wire. `register` is a no-op (returns `None`) for
        // id-less messages — fire-and-forget skips the correlation table
        // entirely.
        let broker_id = pending_responses
            .register(msg, caller_tx, caller_service, &target_tx)
            .await;
        let (wire, delivery) = {
            let _fence = state
                .delivery_fence
                .read()
                .expect("delivery fence poisoned");
            let owns_registration = caller_service
                .and_then(|name| reg.get(name))
                .is_some_and(|entry| entry.same_channel(caller_tx));
            if owns_registration
                && let Some(origin_service) = mesh_from.filter(|name| valid_service_name(name))
                && let Some(peer) =
                    admitted_delivery_peer(state, source_ip, caller_service, admission)
            {
                msg.set(crate::subscription::BROKER_PEER_HEADER, &peer);
                msg.set(crate::subscription::BROKER_SERVICE_HEADER, origin_service);
            }
            let wire = msg.to_wire();
            let delivery = target_tx.try_send(wire.clone());
            (wire, delivery)
        };
        match delivery {
            Ok(()) => LocalRouteResult {
                target_tx: Some(target_tx),
                forwarded_wire: Some(wire),
                outcome: ObserveOutcome::Delivered,
            },
            Err(mpsc::error::TrySendError::Full(_)) => {
                // Slow consumer — keep the peer registered, drop this message.
                // Coalesced warn (1/sec/call-site) prevents the warn itself
                // from becoming a flood when the queue stays saturated.
                let svc = service.to_string();
                warn_drop(&ROUTE_DROP_LAST_MS, &ROUTE_DROP_COUNT, |n| {
                    format!(
                        "route_local: outbound full; dropped {n} messages in last 1s (latest target='{svc}')"
                    )
                });
                // Take the pending entry back so its `caller_id` is the
                // only authoritative source for the synthesised reply —
                // `msg`'s `id` is now the broker-local rewrite. If
                // registration didn't insert anything (id-less message)
                // there's nothing to take and no reply id to attach.
                let caller_id = match broker_id {
                    Some(ref bid) => pending_responses.take(bid).await.map(|p| p.caller_id),
                    None => None,
                };
                let mut err = BusMessage::new()
                    .with_header("rc", "20")
                    .with_header("type", "response")
                    .with_header(
                        "error",
                        &format!("Service '{service}' overloaded (outbound buffer full)"),
                    );
                if let Some(id) = caller_id {
                    err.set("id", &id);
                }
                drop(reg);
                deliver_observed_response(
                    observe,
                    caller_tx,
                    &err,
                    ObserveDirection::Local,
                    err.get("id"),
                );
                // Target didn't receive the wire — tap should fall back to the
                // canonical pre-route bytes, not these undelivered ones.
                LocalRouteResult {
                    target_tx: Some(target_tx),
                    forwarded_wire: None,
                    outcome: ObserveOutcome::Dropped,
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // Peer disconnected — unregister and report to caller.
                // Only remove if the entry STILL points at the channel we
                // just observed closed; otherwise a newer registration may
                // have replaced it (e.g. the old peer reconnected and
                // re-registered under the same name), and removing by name
                // alone would silently strip the new owner — reopening the
                // SPEC 12 §15.5 impersonation window the registration gate
                // is meant to close.
                drop(reg);
                let mut reg_w = registry.write().await;
                if reg_w
                    .get(service)
                    .map(|cur| cur.same_channel(&target_tx))
                    .unwrap_or(false)
                {
                    reg_w.remove(service);
                }
                drop(reg_w);
                let caller_id = match broker_id {
                    Some(ref bid) => pending_responses.take(bid).await.map(|p| p.caller_id),
                    None => None,
                };
                let mut err = BusMessage::new()
                    .with_header("rc", "10")
                    .with_header("type", "response")
                    .with_header("error", &format!("Service '{service}' disconnected"));
                if let Some(id) = caller_id {
                    err.set("id", &id);
                }
                deliver_observed_response(
                    observe,
                    caller_tx,
                    &err,
                    ObserveDirection::Local,
                    err.get("id"),
                );
                LocalRouteResult {
                    target_tx: None,
                    forwarded_wire: Some(wire),
                    outcome: ObserveOutcome::Rejected,
                }
            }
        }
    } else {
        drop(reg);
        let mut err = BusMessage::new()
            .with_header("rc", "10")
            .with_header("type", "response")
            .with_header("error", &format!("Service '{service}' not found"));
        if let Some(id) = msg.get("id") {
            err.set("id", id);
        }
        deliver_observed_response(
            observe,
            caller_tx,
            &err,
            ObserveDirection::Local,
            err.get("id"),
        );
        LocalRouteResult {
            target_tx: None,
            forwarded_wire: None,
            outcome: ObserveOutcome::Rejected,
        }
    }
}

/// Broadcast `raw` to every tap subscriber, except `exclude` if its channel
/// matches one of the subscribers — that peer is the routed-to target and
/// already received the message via `route_local`. The dedup avoids the
/// double-enqueue pattern that crashed the box on 2026-04-28: when the
/// `log` peer registered as a service AND subscribed to `noded.tap`, the
/// same bounded mpsc(256) saw two `try_send` attempts per `to: log`
/// message, halving its effective capacity and producing paired
/// drop-warnings at thousands per second when the queue stalled.
async fn broadcast_tap(
    tap_subscribers: &TapSubscribers,
    raw: &str,
    exclude: Option<&mpsc::Sender<String>>,
) {
    let taps = tap_subscribers.read().await;
    if taps.is_empty() {
        return;
    }
    let mut disconnected = Vec::new();
    for (i, tap_tx) in taps.iter().enumerate() {
        if let Some(ex) = exclude
            && tap_tx.same_channel(ex)
        {
            continue;
        }
        match tap_tx.try_send(raw.to_string()) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                // Slow tap subscriber — drop this frame, keep the subscription.
                // Taps are observability; losing frames is acceptable. Coalesce
                // warns to 1/sec/call-site to bound tracing overhead under flood.
                warn_drop(&TAP_DROP_LAST_MS, &TAP_DROP_COUNT, |n| {
                    format!("broadcast_tap: outbound full; dropped {n} frames in last 1s")
                });
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                disconnected.push(i);
            }
        }
    }
    drop(taps);
    if !disconnected.is_empty() {
        let mut taps = tap_subscribers.write().await;
        for i in disconnected.into_iter().rev() {
            taps.remove(i);
        }
    }
}

// ── Hub internal commands ──

/// Build `noded.peers` from the exact active routing snapshot. Verified posture
/// reports signed ActiveBus members; an Unverified boot reports the immutable
/// `/etc` compatibility fallback. The per-peer JSON shape remains unchanged.
fn noded_peers_body(
    node_name: &str,
    bind: &str,
    listener_port: u16,
    authority: &crate::routing::RoutingAuthority,
) -> serde_json::Value {
    let peers: Vec<serde_json::Value> = authority
        .routes
        .peers()
        .map(|p| {
            serde_json::json!({
                "name": p.name(),
                "wg_ip": p.mesh_ip(),
                "port": p.noded_port(),
            })
        })
        .collect();
    let self_addr = bind.parse::<std::net::SocketAddr>().ok();
    let self_wg_ip = match self_addr {
        Some(sa) => Some(sa.ip().to_string()),
        None => bind.rsplit_once(':').map(|(h, _)| h.to_string()),
    };
    let mut body = serde_json::json!({
        "node": node_name,
        "wg_ip": self_wg_ip,
        "port": listener_port,
        "peers": peers,
        "source": authority.routes.source_label(),
    });
    if let crate::authority::Posture::Verified(a) = &authority.posture {
        body["authority"] = serde_json::json!({
            "epoch": a.epoch,
            "hash": a.hash,
            "self": authority.self_eligibility_label(),
            "routing_view": a.routing_view.iter().map(crate::authority::RoutingMember::to_json).collect::<Vec<_>>(),
        });
    }
    body
}

async fn handle_noded_command(
    msg: &BusMessage,
    tx: &mpsc::Sender<String>,
    state: &AppState,
    service_name: &mut Option<String>,
    anon_id: &str,
    source_ip: std::net::IpAddr,
    observed_outcome: &mut ObserveOutcome,
) {
    struct OutcomeGuard<'a> {
        rejected: std::sync::atomic::AtomicBool,
        output: &'a mut ObserveOutcome,
    }
    impl Drop for OutcomeGuard<'_> {
        fn drop(&mut self) {
            *self.output = if self.rejected.load(std::sync::atomic::Ordering::Relaxed) {
                ObserveOutcome::Rejected
            } else {
                ObserveOutcome::BrokerHandled
            };
        }
    }
    let outcome = OutcomeGuard {
        rejected: std::sync::atomic::AtomicBool::new(false),
        output: observed_outcome,
    };
    // Peer identity for broker operations: registered service name if any,
    // else the connection-scoped synthesized anon id (§ 3.11.1).
    let peer_id: String = service_name.as_deref().unwrap_or(anon_id).to_string();

    let command = msg.command_name().unwrap_or("");
    let msg_id = msg.get("id");

    let respond = |rc: &str| -> BusMessage {
        if rc != "0" {
            outcome
                .rejected
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let mut resp = BusMessage::new()
            .with_header("type", "response")
            .with_header("from", "noded")
            .with_header("rc", rc);
        if let Some(id) = msg_id {
            resp.set("id", id);
        }
        resp
    };

    match command {
        "noded.register" => {
            let from = match msg.from_addr() {
                Some(f) => f.to_string(),
                None => {
                    let mut resp = respond("10");
                    resp.set("error", "noded.register requires 'from' header");
                    let _ = tx.try_send(resp.to_wire());
                    return;
                }
            };
            if !valid_service_name(&from) {
                let mut resp = respond("10");
                resp.set(
                    "error",
                    "noded.register 'from' must match ^[a-z][a-z0-9-]{1,30}$",
                );
                let _ = tx.try_send(resp.to_wire());
                return;
            }

            // Build the registry record from the provenance the citizen
            // supplied in the register body (all optional — absent for an
            // old citizen) plus the broker-stamped binding time.
            let prov: cosmix_bus::RegisterProvenance = if msg.body.trim().is_empty() {
                cosmix_bus::RegisterProvenance::default()
            } else {
                serde_json::from_str(&msg.body).unwrap_or_else(|e| {
                    // A new citizen sent a non-empty but malformed body —
                    // register anyway (name-only) but don't lose it silently.
                    tracing::warn!(
                        service = %from,
                        error = %e,
                        "noded.register: malformed provenance body, registering without it"
                    );
                    cosmix_bus::RegisterProvenance::default()
                })
            };
            let info = cosmix_bus::ServiceInfo {
                name: from.clone(),
                binary: prov.binary,
                version: prov.version,
                git_sha: prov.git_sha,
                git_dirty: prov.git_dirty,
                build_time: prov.build_time,
                pid: prov.pid,
                started_at: prov.started_at,
                registered_at: Some(
                    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                ),
                schema_version: cosmix_bus::SCHEMA_VERSION,
                meta: prov.meta,
            };

            // Atomic check-collision / remove-old / insert-new under a
            // single write lock. The collision check happens BEFORE
            // removing any prior name this connection held, so a
            // rejection leaves both the registry and `service_name`
            // unchanged — otherwise a failed re-register would silently
            // strip the connection's registered owner while
            // `service_name` (the value the reserved-topic gates read)
            // still points at the dropped name, defeating SPEC 12 §15.5.
            let alias_changed = service_name
                .as_ref()
                .is_some_and(|old_name| old_name != &from);
            {
                let mut reg = state.registry.write().await;
                // Reject if some OTHER connection holds the target name.
                // The same-name-same-connection refresh case is a no-op
                // (same_channel == true) and permitted; we fall through
                // and re-insert our own tx below.
                if let Some(existing) = reg.get(&from)
                    && !existing.same_channel(tx)
                {
                    drop(reg);
                    let mut resp = respond("10");
                    resp.set(
                        "error",
                        &format!("service name '{from}' is already registered"),
                    );
                    let _ = tx.try_send(resp.to_wire());
                    return;
                }
                // Collision check passed. Now drop any prior name this
                // connection held under a different alias, then install
                // the new one.
                if let Some(old_name) = service_name.as_ref()
                    && old_name != &from
                {
                    // Same-channel guard (SPEC 12 §15.5): only drop our
                    // prior name if the registry entry STILL points at
                    // THIS connection. A stale connection whose old entry
                    // was already pruned (e.g. route_local closed-channel
                    // removal) and re-owned by a NEW connection must not
                    // strip that new owner — that would reopen the
                    // impersonation window the dereg/disconnect/closed
                    // paths close (Codex P1 BLOCKER).
                    if reg
                        .get(old_name)
                        .map(|cur| cur.same_channel(tx))
                        .unwrap_or(false)
                    {
                        reg.remove(old_name);
                    }
                }
                reg.insert(
                    from.clone(),
                    ServiceEntry {
                        tx: tx.clone(),
                        info,
                    },
                );
            }
            if alias_changed {
                // Observe authority is bound to the allowlisted registered
                // name. Re-registering under an alias cannot retain it.
                state.observe.remove_owner(tx);
            }
            *service_name = Some(from.clone());
            tracing::info!("Service '{}' registered", from);

            let mut resp = respond("0");
            resp.set("command", "noded.register");
            resp.body = format!(r#"{{"registered": "{}"}}"#, from);
            let _ = tx.try_send(resp.to_wire());

            emit_props_change(state, &format!("register:{from}")).await;
        }

        "noded.deregister" => {
            // SPEC 18 §3.5 graceful shutdown: a citizen removes its own
            // registered name and awaits this response BEFORE exiting, so
            // the broker never routes a request to a dead name in the race
            // window between process exit and WS-close detection (the
            // implicit dereg at ~L820 is too late for that guarantee).
            // Operates solely on the name THIS connection registered under
            // (`service_name`) — it cannot target another connection's name.
            // Idempotent: deregistering when unregistered, or when the name
            // was already reclaimed by another connection, is RC 0 — the
            // caller's intent ("my name must not be bound to my connection")
            // holds either way.
            //
            // Correctness rests on `remove_peer` being CHANNEL-SCOPED (it
            // takes our `tx` and only touches subscriptions/last-publisher
            // state on that exact channel — see its doc). That is what makes
            // a stale dereg from an old half-alive socket safe even if a new
            // connection has already re-registered `name` (route_local prune
            // or citizen restart, e.g. statecache under systemd): our
            // teardown removes only OUR subs; the new owner's are untouched.
            // The registry removal is independently same-channel guarded
            // (SPEC 12 §15.5). `service_name.take()` clears our broker
            // identity so the eventual WS-close is a clean no-op under the
            // anon id — and even if it weren't cleared, the channel-scoped
            // remove_peer at WS-close could not collaterally tear down a
            // newer owner. remove_peer-before-registry-removal ordering is
            // retained as defence-in-depth, not as the load-bearing
            // mechanism (channel scoping is).
            match service_name.take() {
                Some(name) => {
                    state.observe.remove_owner(tx);
                    // Authoritative channel-scoped teardown of OUR subs,
                    // mirroring the disconnect path (~L635).
                    let idle_notices = state.broker.remove_peer(&name, tx).await;
                    dispatch_notifications(state, &idle_notices).await;
                    {
                        let mut reg = state.registry.write().await;
                        if reg
                            .get(&name)
                            .map(|cur| cur.same_channel(tx))
                            .unwrap_or(false)
                        {
                            reg.remove(&name);
                        }
                    }
                    tracing::info!("Service '{}' deregistered", name);
                    let mut resp = respond("0");
                    resp.set("command", "noded.deregister");
                    resp.body = format!(r#"{{"deregistered": "{name}"}}"#);
                    let _ = tx.try_send(resp.to_wire());
                    emit_props_change(state, &format!("deregister:{name}")).await;
                }
                None => {
                    state.observe.remove_owner(tx);
                    let mut resp = respond("0");
                    resp.set("command", "noded.deregister");
                    resp.body = r#"{"deregistered": null}"#.to_string();
                    let _ = tx.try_send(resp.to_wire());
                }
            }
        }

        "noded.list" => {
            // Version-discovery contract: returns [ServiceInfo] objects
            // (was [name] strings before the 2026-06-01 reshape). Sorted
            // by name for stable, legible output. New clients dual-parse
            // (cosmix_bus::ServiceInfo tolerates both shapes) so a new
            // client tolerates an old broker during the client-first
            // rollout (§9).
            let mut services: Vec<cosmix_bus::ServiceInfo> = {
                let reg = state.registry.read().await;
                reg.values().map(|e| e.info.clone()).collect()
            };
            services.sort_by(|a, b| a.name.cmp(&b.name));
            let body = serde_json::to_string(&services).unwrap_or_else(|_| "[]".to_string());

            let mut resp = respond("0");
            resp.set("command", "noded.list");
            resp.body = body;
            let _ = tx.try_send(resp.to_wire());
        }

        "noded.info" => {
            // Version-discovery contract: node identity + the local
            // broker's own build, COMPUTED ON READ (so uptime_s /
            // service_count are live, not a stored snapshot).
            let service_count = { state.registry.read().await.len() as u16 };
            let bi = cosmix_buildinfo::build_info!();
            let noded_self = cosmix_bus::ServiceInfo {
                name: "noded".to_string(),
                binary: Some(bi.pkg.to_string()),
                version: Some(bi.version.to_string()),
                git_sha: Some(bi.git_sha.to_string()),
                git_dirty: Some(bi.git_dirty),
                build_time: Some(bi.build_time.to_string()),
                pid: Some(std::process::id()),
                started_at: Some(state.started_at.clone()),
                registered_at: None,
                schema_version: cosmix_bus::SCHEMA_VERSION,
                meta: Default::default(),
            };
            let info = cosmix_bus::NodeInfo {
                node: state.node_name.clone(),
                // `bind` is `<host>:<port>`; the host is the WG IP. Parse
                // as SocketAddr so IPv6 (`[fd00::1]:p`) yields `fd00::1`,
                // not `[fd00::1]`; fall back to rsplit for any non-addr
                // bind string.
                wg_ip: state
                    .bind
                    .parse::<std::net::SocketAddr>()
                    .map(|sa| sa.ip().to_string())
                    .ok()
                    .or_else(|| state.bind.rsplit_once(':').map(|(h, _)| h.to_string())),
                mesh: None,
                noded: Some(noded_self),
                uptime_s: Some(state.started.elapsed().as_secs()),
                service_count: Some(service_count),
                schema_version: cosmix_bus::SCHEMA_VERSION,
                meta: Default::default(),
            };
            let body = serde_json::to_string(&info).unwrap_or_else(|_| "{}".to_string());

            let mut resp = respond("0");
            resp.set("command", "noded.info");
            resp.body = body;
            let _ = tx.try_send(resp.to_wire());
        }

        "noded.ping" => {
            // Extensions map advertises broker availability per
            // 2026-04-10-topic-pubsub-v1.md § 2.6. Clients detect broker
            // support by reading `extensions.topic` from the ping response
            // rather than probing `topic.list` and checking for RC 10.
            let mut resp = respond("0");
            resp.set("command", "noded.ping");
            resp.body =
                r#"{"pong": true, "extensions": {"core": "1.0", "topic": "1.0", "observe": "1.0"}}"#.to_string();
            let _ = tx.try_send(resp.to_wire());
        }

        "noded.peers" => {
            // One snapshot supplies both the active peers and their authority
            // provenance, so an inventory reload cannot mix epochs in one reply.
            let authority = state.authority.load();
            let body = noded_peers_body(
                &state.node_name,
                &state.bind,
                state.listener_port,
                authority.as_ref(),
            );

            let mut resp = respond("0");
            resp.set("command", "noded.peers");
            resp.body = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());
            let _ = tx.try_send(resp.to_wire());
        }

        "noded.inventory" => {
            // SPEC 13 1b-c authority plane: report the verified signed
            // inventory (epoch, hash, verify_keys, member set) or the
            // fail-closed `unverified` posture + cause. This is the
            // authoritative membership view (§7.1). The same atomically-loaded
            // runtime snapshot also owns the active D1.4 route table.
            let mut resp = respond("0");
            resp.set("command", "noded.inventory");
            resp.body = serde_json::to_string(&state.authority.load().posture.to_json())
                .unwrap_or_else(|_| "{}".to_string());
            let _ = tx.try_send(resp.to_wire());
        }

        "noded.observe.start" => {
            let registered = if let Some(name) = service_name.as_deref() {
                let registry = state.registry.read().await;
                registry
                    .get(name)
                    .filter(|entry| entry.same_channel(tx))
                    .map(|_| name)
            } else {
                None
            };
            if let Err(error) = state.observe.start(
                registered,
                is_same_node_origin(source_ip, &state.bind),
                tx,
                msg_id,
                &msg.body,
            ) {
                outcome
                    .rejected
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                let body = format!(r#"{{"error":"{}"}}"#, error.code());
                let wire = crate::observe::response_wire(
                    "noded.observe.start",
                    msg_id,
                    error.rc(),
                    Some(error.code()),
                    &body,
                );
                let _ = tx.try_send(wire);
            }
        }

        "noded.observe.stop" => {
            let subscription_id = serde_json::from_str::<serde_json::Value>(&msg.body)
                .ok()
                .and_then(|body| {
                    body.get("subscription_id")
                        .and_then(serde_json::Value::as_str)
                        .map(ToString::to_string)
                });
            if let Some(subscription_id) = subscription_id {
                state.observe.stop(tx, msg_id, &subscription_id).await;
            } else {
                let error = ObserveError::InvalidArgs;
                outcome
                    .rejected
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                let body = format!(r#"{{"error":"{}"}}"#, error.code());
                let wire = crate::observe::response_wire(
                    "noded.observe.stop",
                    msg_id,
                    error.rc(),
                    Some(error.code()),
                    &body,
                );
                let _ = tx.try_send(wire);
            }
        }

        "noded.tap" => {
            // `noded.tap` streams a live copy of EVERY inter-service message
            // routed on this node (headers + bodies) to the subscriber — mail
            // content, session/auth tokens carried in bodies, props values. It
            // therefore bypasses all per-service read authorization and must be
            // reachable only by same-node citizens (e.g. the logger), never by a
            // remote mesh peer. Gate it with the same same-node-origin test the
            // register enforce gate uses (`is_same_node_origin`): a remote WG
            // peer's source is its authenticated `/24` IP — never loopback, never
            // the broker's own bind IP — so it can't forge a local origin. Unlike
            // `noded.register` this is NOT a session-scoped decision, so the gate
            // lives here in the command handler rather than the routing layer.
            if !is_same_node_origin(source_ip, &state.bind) {
                tracing::warn!(
                    source_ip = %source_ip,
                    peer = %peer_id,
                    "refused noded.tap from off-node origin (would mirror all inter-service traffic)"
                );
                let mut resp = respond("10");
                resp.set("command", "noded.tap");
                resp.set("error", "noded.tap is restricted to same-node origin");
                let _ = tx.try_send(resp.to_wire());
                return;
            }
            state.tap_subscribers.write().await.push(tx.clone());
            tracing::info!(source_ip = %source_ip, "Tap subscriber added");

            let mut resp = respond("0");
            resp.set("command", "noded.tap");
            resp.body = r#"{"tapping": true}"#.to_string();
            let _ = tx.try_send(resp.to_wire());
        }

        // ── Topic pub/sub (see 2026-04-10-topic-pubsub-v1.md § 3.11) ──
        "topic.publish" => {
            let name = match msg.get("name") {
                Some(n) if !n.is_empty() => n.to_string(),
                _ => {
                    let mut resp = respond("10");
                    resp.set("command", "topic.publish");
                    resp.set("error", "topic.publish requires 'name' header");
                    let _ = tx.try_send(resp.to_wire());
                    return;
                }
            };
            // SPEC 12 §15.5: reserved property-substrate topics
            // accept publishes only from the owning service.
            if !crate::props_reservation::may_publish(&name, &peer_id) {
                let mut resp = respond("10");
                resp.set("command", "topic.publish");
                resp.set(
                    "error",
                    "topic_reserved: only the registered owning service may publish to <svc>.props.changed, <svc>.props.records.changed, or <svc>.props.audit (SPEC 12 §15.5)",
                );
                let _ = tx.try_send(resp.to_wire());
                return;
            }
            // retain defaults to true per § 3.11.2
            let retain = msg.get("retain").map(|v| v != "false").unwrap_or(true);
            // SPEC 12 §15.5 defense-in-depth (Codex C10b BLOCKER fix):
            // for reserved topics, strip every caller-supplied Bus
            // routing header from the inner so granted subscribers
            // can't be tricked into dispatching the fan-out frame as
            // an arbitrary command from a forged `from`. The
            // legitimate publisher already emits this shape; we
            // enforce it broker-side instead of trusting publishers.
            let canonical_body =
                crate::props_reservation::canonicalize_reserved_inner(&name, &msg.body);
            let body_ref: &str = canonical_body.as_deref().unwrap_or(&msg.body);
            match state
                .broker
                .publish_with_origin(
                    &name,
                    body_ref,
                    &peer_id,
                    tx.clone(),
                    broker_origin_for_delivery(source_ip, &state.bind),
                    retain,
                )
                .await
            {
                Ok((seq, delivered, notices)) => {
                    let mut resp = respond("0");
                    resp.set("command", "topic.publish");
                    resp.body = format!(r#"{{"seq": {seq}, "delivered": {delivered}}}"#);
                    let _ = tx.try_send(resp.to_wire());
                    dispatch_notifications(state, &notices).await;
                }
                Err(e) => {
                    let mut resp = respond("10");
                    resp.set("command", "topic.publish");
                    resp.body = e.error_body();
                    let _ = tx.try_send(resp.to_wire());
                }
            }
        }

        "topic.subscribe" => {
            let name = match msg.get("name") {
                Some(n) if !n.is_empty() => n.to_string(),
                _ => {
                    let mut resp = respond("10");
                    resp.set("command", "topic.subscribe");
                    resp.set("error", "topic.subscribe requires 'name' header");
                    let _ = tx.try_send(resp.to_wire());
                    return;
                }
            };
            // SPEC 12 §15.5: reserved property-substrate topics
            // refuse subscribe from every peer — including the
            // owning service. Subscribers go through
            // <svc>.props.watch / <svc>.props.audit.watch where
            // capability is re-checked.
            if !crate::props_reservation::may_subscribe(&name) {
                let mut resp = respond("10");
                resp.set("command", "topic.subscribe");
                resp.set(
                    "error",
                    "topic_reserved: <svc>.props.records.changed and <svc>.props.audit are not directly subscribable; use <svc>.props.watch / <svc>.props.audit.watch (SPEC 12 §15.5)",
                );
                let _ = tx.try_send(resp.to_wire());
                return;
            }
            let (sub_id, replayed, seq, notices) = state
                .broker
                .subscribe_topic(&name, &peer_id, tx.clone())
                .await;
            let mut resp = respond("0");
            resp.set("command", "topic.subscribe");
            resp.body = format!(
                r#"{{"subscription_id": "{sub_id}", "replayed": {replayed}, "seq": {seq}}}"#
            );
            let _ = tx.try_send(resp.to_wire());
            dispatch_notifications(state, &notices).await;
        }

        "topic.unsubscribe" => {
            let name = match msg.get("name") {
                Some(n) if !n.is_empty() => n.to_string(),
                _ => {
                    let mut resp = respond("10");
                    resp.set("command", "topic.unsubscribe");
                    resp.set("error", "topic.unsubscribe requires 'name' header");
                    let _ = tx.try_send(resp.to_wire());
                    return;
                }
            };
            let notices = state.broker.unsubscribe_topic(&name, &peer_id).await;
            let mut resp = respond("0");
            resp.set("command", "topic.unsubscribe");
            resp.body = "{}".to_string();
            let _ = tx.try_send(resp.to_wire());
            dispatch_notifications(state, &notices).await;
        }

        "topic.subscriber_count" => {
            let name = match msg.get("name") {
                Some(n) if !n.is_empty() => n.to_string(),
                _ => {
                    let mut resp = respond("10");
                    resp.set("command", "topic.subscriber_count");
                    resp.set("error", "topic.subscriber_count requires 'name' header");
                    let _ = tx.try_send(resp.to_wire());
                    return;
                }
            };
            // SPEC 12 §15.5: only the owning service may read the
            // count for a reserved topic — subscriber cardinality is
            // a side channel non-owners must not observe.
            if !crate::props_reservation::may_read_count(&name, &peer_id) {
                let mut resp = respond("10");
                resp.set("command", "topic.subscriber_count");
                resp.set(
                    "error",
                    "topic_reserved: only the owning service may read subscriber_count for <svc>.props.records.changed or <svc>.props.audit (SPEC 12 §15.5)",
                );
                let _ = tx.try_send(resp.to_wire());
                return;
            }
            let count = state.broker.subscriber_count(&name).await;
            let mut resp = respond("0");
            resp.set("command", "topic.subscriber_count");
            resp.body = format!(r#"{{"count": {count}}}"#);
            let _ = tx.try_send(resp.to_wire());
        }

        "topic.list" => {
            let prefix = msg.get("prefix").filter(|s| !s.is_empty());
            let infos = state.broker.list(prefix).await;
            // SPEC 12 §15.5: reserved topics are visible only to
            // their owning service. The broker returns everything;
            // the handler filters here so the rule lives next to
            // the other reservation gates rather than leaking into
            // the broker.
            let visible: Vec<_> = infos
                .into_iter()
                .filter(|i| crate::props_reservation::visible_in_list(&i.name, &peer_id))
                .collect();
            let body = format_topic_list(&visible);
            let mut resp = respond("0");
            resp.set("command", "topic.list");
            resp.body = body;
            let _ = tx.try_send(resp.to_wire());
        }

        "topic.clear" => {
            let name = match msg.get("name") {
                Some(n) if !n.is_empty() => n.to_string(),
                _ => {
                    let mut resp = respond("10");
                    resp.set("command", "topic.clear");
                    resp.set("error", "topic.clear requires 'name' header");
                    let _ = tx.try_send(resp.to_wire());
                    return;
                }
            };
            // SPEC 12 §15.5: only the owning service may clear a
            // reserved topic's retained snapshot.
            if !crate::props_reservation::may_clear(&name, &peer_id) {
                let mut resp = respond("10");
                resp.set("command", "topic.clear");
                resp.set(
                    "error",
                    "topic_reserved: only the owning service may clear <svc>.props.records.changed or <svc>.props.audit (SPEC 12 §15.5)",
                );
                let _ = tx.try_send(resp.to_wire());
                return;
            }
            let notify = msg.get("notify").map(|v| v != "false").unwrap_or(true);
            let (delivered, idle_notices) = state.broker.clear(&name, notify).await;
            let mut resp = respond("0");
            resp.set("command", "topic.clear");
            resp.body = format!(r#"{{"delivered": {delivered}}}"#);
            let _ = tx.try_send(resp.to_wire());
            // SPEC 12 C10b — clear-prune may have driven the topic to
            // zero subscribers (only dead txs remained); dispatch the
            // paired topic.idle so the publisher's view doesn't get
            // stuck in "active".
            dispatch_notifications(state, &idle_notices).await;
        }

        // ── UI event subscribe stubs (§ 6 Phase A — registered but not routed) ──
        //
        // These land in the shared subscription registry so the "one registry
        // under the hood" claim from the delta is honored, but no ui.event
        // routing is wired in v1. We return RC 5 (warning) with a body noting
        // the subscription was accepted but event delivery is pending.
        "ui.subscribe" => {
            // Parse "source: ..." and optional "action: ..." from the body's
            // key:value shape per spec § 3.9.
            let (source, action) = parse_ui_sub_body(&msg.body);
            let source = match source {
                Some(s) => s,
                None => {
                    let mut resp = respond("10");
                    resp.set("command", "ui.subscribe");
                    resp.set("error", "ui.subscribe requires 'source' in body");
                    let _ = tx.try_send(resp.to_wire());
                    return;
                }
            };
            let sub_id = state
                .broker
                .subscribe_ui_event(&peer_id, &source, action.as_deref(), tx.clone())
                .await;
            let mut resp = respond("5");
            resp.set("command", "ui.subscribe");
            resp.body = format!(
                r#"{{"subscription_id": "{sub_id}", "warning": "registered_but_not_routed"}}"#
            );
            let _ = tx.try_send(resp.to_wire());
        }

        "ui.unsubscribe" => {
            let (source, action) = parse_ui_sub_body(&msg.body);
            let source = match source {
                Some(s) => s,
                None => {
                    let mut resp = respond("10");
                    resp.set("command", "ui.unsubscribe");
                    resp.set("error", "ui.unsubscribe requires 'source' in body");
                    let _ = tx.try_send(resp.to_wire());
                    return;
                }
            };
            state
                .broker
                .unsubscribe_ui_event(&peer_id, &source, action.as_deref())
                .await;
            let mut resp = respond("0");
            resp.set("command", "ui.unsubscribe");
            resp.body = "{}".to_string();
            let _ = tx.try_send(resp.to_wire());
        }

        // SPEC 12 §15.5 / C10 — privileged subscribe-on-behalf verb.
        //
        // The owning service of a reserved props topic (per
        // `props_reservation::reserved_owner`) uses this to subscribe
        // a third-party peer to a per-namespace slice of its
        // `<svc>.props.records.changed` or `<svc>.props.audit` topic.
        // The lower-level `topic.subscribe` is reservation-gated and
        // rejects every peer; the watch verbs in `cosmix-lib-props-store`
        // call this verb after their own capability re-check so the
        // live-fan-out wire becomes reachable from
        // `<svc>.props.watch` / `<svc>.props.audit.watch`.
        //
        // Auth model — confused-deputy guard:
        //   * Granter identity is the connection-state `peer_id`
        //     (registered service name set by `noded.register`, else
        //     synthesised anon id). NEVER the caller-supplied `from`
        //     header — `from` is the Bus routing field, the registry
        //     gate on `noded.register` is what binds a name to this
        //     specific tx channel.
        //   * `target_peer` is caller-supplied but is only used as a
        //     lookup key into the registry. The actual mpsc::Sender
        //     the broker fans out to is the one the target peer
        //     itself registered. A malicious owner can therefore
        //     cause unsolicited fan-out to *some other registered
        //     service* (DoS on target), but cannot impersonate the
        //     target or read their traffic. The scope of damage is
        //     bounded to the owner's own reserved topics.
        //   * Anonymous peers can't be `target_peer` — they aren't
        //     in the registry (no `noded.register`), so the lookup
        //     fails with `target_peer_not_connected` (rev-2 Q4).
        "noded.props.subscribe_grant" => {
            let topic = match msg.get("topic") {
                Some(t) if !t.is_empty() => t.to_string(),
                _ => {
                    let mut resp = respond("10");
                    resp.set("command", "noded.props.subscribe_grant");
                    resp.set("error", "subscribe_grant requires 'topic' header");
                    let _ = tx.try_send(resp.to_wire());
                    return;
                }
            };
            let target_peer = match msg.get("target_peer") {
                Some(p) if !p.is_empty() => p.to_string(),
                _ => {
                    let mut resp = respond("10");
                    resp.set("command", "noded.props.subscribe_grant");
                    resp.set("error", "subscribe_grant requires 'target_peer' header");
                    let _ = tx.try_send(resp.to_wire());
                    return;
                }
            };
            let namespace = match msg.get("namespace") {
                Some(n) if !n.is_empty() => n.to_string(),
                _ => {
                    let mut resp = respond("10");
                    resp.set("command", "noded.props.subscribe_grant");
                    resp.set(
                        "error",
                        "subscribe_grant requires 'namespace' header (no wildcard in v1)",
                    );
                    let _ = tx.try_send(resp.to_wire());
                    return;
                }
            };
            // Authorization: caller must be the topic's reserved
            // owner. Non-reserved topics are not grantable through
            // this verb — peers can `topic.subscribe` them directly.
            let owner = match crate::props_reservation::reserved_owner(&topic) {
                Some(o) => o,
                None => {
                    let mut resp = respond("10");
                    resp.set("command", "noded.props.subscribe_grant");
                    resp.set(
                        "error",
                        "subscribe_grant is only valid for reserved \
                         <svc>.props.records.changed or <svc>.props.audit topics",
                    );
                    let _ = tx.try_send(resp.to_wire());
                    return;
                }
            };
            if owner != peer_id {
                let mut resp = respond("10");
                resp.set("command", "noded.props.subscribe_grant");
                resp.set(
                    "error",
                    "subscribe_grant denied: only the topic's owning service may grant",
                );
                let _ = tx.try_send(resp.to_wire());
                return;
            }
            // Look up the target's authenticated outbound channel.
            // Clone the Sender out of the registry under a read lock
            // so we don't hold it across the broker call. We then
            // explicitly reject a closed sender — the registry's
            // disconnect cleanup is itself eventual (a peer drop runs
            // `remove_peer` which only fires after the connection task
            // notices closure), so a window exists where the registry
            // still holds a Sender whose receiver has been dropped.
            // Returning the grant without the close-check would insert
            // a dead subscription that the broker can never deliver
            // through, and idempotency would resurrect it on every
            // re-grant. Fail loudly here so the caller re-grants after
            // the target reconnects (the registry refresh) instead.
            let target_tx = {
                let reg = state.registry.read().await;
                reg.get(&target_peer).map(|e| e.tx.clone())
            };
            let target_tx = match target_tx {
                Some(t) if !t.is_closed() => t,
                _ => {
                    let mut resp = respond("10");
                    resp.set("command", "noded.props.subscribe_grant");
                    resp.set("error", "target_peer_not_connected");
                    let _ = tx.try_send(resp.to_wire());
                    return;
                }
            };
            let filter = subscription::BodyFilter {
                namespace: namespace.clone(),
            };
            let (sub_id, _replayed, _seq, notices) = state
                .broker
                .subscribe_topic_filtered(&topic, &target_peer, target_tx, Some(filter))
                .await;
            let mut resp = respond("0");
            resp.set("command", "noded.props.subscribe_grant");
            resp.body = format!(
                r#"{{"subscription_id":"{}","namespace":"{}"}}"#,
                escape_json_string(&sub_id),
                escape_json_string(&namespace),
            );
            let _ = tx.try_send(resp.to_wire());
            dispatch_notifications(state, &notices).await;
        }

        // SPEC 07 §2 — uniform property surface (L1).
        // `noded.props.{get,list,describe}` against the consolidated
        // node daemon's observable state.
        c if c.starts_with("noded.props.") => {
            let suffix = &c["noded.props.".len()..];
            let snapshot = crate::props::collect(
                state.started,
                &state.started_at,
                &state.bind,
                &state.node_name,
                &state.log_level,
                &state.registry,
                &state.broker,
            )
            .await;
            // Accept args as either an `args` header (string-encoded JSON,
            // per SPEC 07 §2 examples) or as the message body (RPC-style
            // call from Mix/cosmix-lib-client, which puts named args in the
            // body). Header takes precedence.
            let args_json = crate::props::parse_args(msg.get("args")).or_else(|| {
                if msg.body.is_empty() {
                    None
                } else {
                    serde_json::from_str(&msg.body).ok()
                }
            });
            // L1: no in-mesh trust check yet → always honour `sensitive: true`
            // by redacting. Broker-level reveal-from-mesh enforcement lands in
            // SPEC 07 §7.2 follow-up work.
            let resp_inner = cosmix_props::bus::dispatch_props(
                &snapshot,
                suffix,
                args_json.as_ref(),
                /* redact_sensitive = */ true,
            );
            let mut resp = respond(&resp_inner.rc.to_string());
            resp.set("command", c);
            resp.body = resp_inner.body;
            let _ = tx.try_send(resp.to_wire());
        }

        // SPEC 07 §3 — subscribe to property change events. Sugar over
        // `topic.subscribe name=noded.props.changed` so callers don't have
        // to know the topic name.
        "props.watch" => {
            let topic = crate::props::PROPS_CHANGED_TOPIC;
            let (sub_id, replayed, seq, notices) = state
                .broker
                .subscribe_topic(topic, &peer_id, tx.clone())
                .await;
            let mut resp = respond("0");
            resp.set("command", "props.watch");
            resp.body = format!(
                r#"{{"subscription_id": "{sub_id}", "topic": "{topic}", "replayed": {replayed}, "seq": {seq}}}"#
            );
            let _ = tx.try_send(resp.to_wire());
            dispatch_notifications(state, &notices).await;
        }

        // SPEC 07 §5.1 — spec distribution.
        "spec.get" | "spec.v2.get" => {
            let args_json = crate::props::parse_args(msg.get("args")).or_else(|| {
                if msg.body.is_empty() {
                    None
                } else {
                    serde_json::from_str(&msg.body).ok()
                }
            });
            let dir = state.spec_dir.as_deref().map(|p| p.as_path());
            let result = match (state.spec_release.as_deref(), command) {
                (Some(release), "spec.v2.get") => release.get_v2(args_json.as_ref()),
                (Some(release), _) => release.get_legacy(args_json.as_ref()),
                (None, "spec.v2.get") => crate::spec_release::error("release_unavailable"),
                (None, _) => crate::spec::dispatch_spec_get(dir, args_json.as_ref()),
            };
            let mut resp = respond(&result.rc.to_string());
            resp.set("command", command);
            for (k, v) in &result.headers {
                resp.set(k, v);
            }
            if let Some(err) = &result.error {
                resp.set("error", err);
            }
            resp.body = result.body;
            let _ = tx.try_send(resp.to_wire());
        }

        _ => {
            let mut resp = respond("10");
            resp.set("error", &format!("Unknown noded command: '{command}'"));
            let _ = tx.try_send(resp.to_wire());
        }
    }
}

fn valid_service_name(name: &str) -> bool {
    (2..=31).contains(&name.len())
        && name
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// Parse a `ui.subscribe` body (`key: value` lines per § 3.9). Returns
/// `(source, action)`.
fn parse_ui_sub_body(body: &str) -> (Option<String>, Option<String>) {
    let mut source = None;
    let mut action = None;
    for line in body.lines() {
        let line = line.trim();
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim();
            let v = v.trim().to_string();
            match k {
                "source" => source = Some(v),
                "action" => action = Some(v),
                _ => {}
            }
        }
    }
    (source, action)
}

/// Format `TopicInfo` entries as a JSON array for `topic.list` responses.
fn format_topic_list(infos: &[TopicInfo]) -> String {
    let parts: Vec<String> = infos
        .iter()
        .map(|info| {
            let last_pub = match &info.last_publisher {
                Some(p) => format!(r#""{}""#, escape_json_string(p)),
                None => "null".to_string(),
            };
            format!(
                r#"{{"name":"{}","subscribers":{},"has_snapshot":{},"snapshot_seq":{},"snapshot_size":{},"last_publisher":{},"stale":{}}}"#,
                escape_json_string(&info.name),
                info.subscribers,
                info.has_snapshot,
                info.snapshot_seq,
                info.snapshot_size,
                last_pub,
                info.stale,
            )
        })
        .collect();
    format!("[{}]", parts.join(","))
}

fn escape_json_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    //! Regression test for the 2026-04-28 noded log-feedback meltdown.
    //!
    //! Reproduces the failure shape: a peer that is **registered as a
    //! service** AND **subscribed to noded.tap** on the same connection
    //! has its bounded outbound mpsc(256) referenced twice by the broker.
    //! Floods at this peer used to fire one WARN per dropped message
    //! (route_local) plus another per dropped tap (broadcast_tap),
    //! starving journald and the broker itself.
    //!
    //! With the fix: drops are deduped (route_local return value tells
    //! broadcast_tap to skip the same Sender) and remaining drops are
    //! coalesced to ≤1 WARN per second per axis.

    use std::collections::HashMap;
    use std::net::{Ipv4Addr, TcpListener};
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use arc_swap::ArcSwap;
    use cosmix_config::node::AdmissionMode;
    use cosmix_mesh::{DEFAULT_NODED_PORT, MeshConfig, MeshPeers, PeerConfig};
    use tokio::sync::{RwLock, mpsc};
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::{Context, SubscriberExt};
    use tracing_subscriber::util::SubscriberInitExt;

    use super::{
        AppState, PendingResponseTable, apply_inventory_reload, emit_listener_endpoint_state,
    };
    use crate::observe::ObserveManager;
    use crate::subscription::SubscriptionBroker;

    #[derive(Default)]
    struct EventNameVisitor {
        name: Option<String>,
    }

    impl tracing::field::Visit for EventNameVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "event" {
                self.name = Some(format!("{value:?}").trim_matches('"').to_string());
            }
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            if field.name() == "event" {
                self.name = Some(value.to_string());
            }
        }
    }

    struct RecordEventNames(Arc<StdMutex<Vec<String>>>);

    impl<S: tracing::Subscriber> Layer<S> for RecordEventNames {
        fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
            let mut visitor = EventNameVisitor::default();
            event.record(&mut visitor);
            if let Some(name) = visitor.name {
                self.0.lock().unwrap().push(name);
            }
        }
    }

    struct CountWarns(Arc<AtomicU64>);

    impl<S: tracing::Subscriber> Layer<S> for CountWarns {
        fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
            if *event.metadata().level() == tracing::Level::WARN {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Pick a free ephemeral port on the loopback. Returns `None` when
    /// the test environment can't bind to localhost (sandboxed CI seen
    /// in the wild rejects with `PermissionDenied`); callers should
    /// `eprintln!` and `return` rather than panic.
    fn pick_port() -> Option<u16> {
        match TcpListener::bind((Ipv4Addr::LOCALHOST, 0)) {
            Ok(l) => l.local_addr().ok().map(|a| a.port()),
            Err(e) => {
                eprintln!("pick_port: bind to 127.0.0.1:0 failed ({e}); skipping test");
                None
            }
        }
    }

    #[test]
    fn noded_peers_reports_active_signed_table_or_complete_etc_fallback() {
        use super::noded_peers_body;
        use crate::authority::{Accepted, Posture, RoutingMember};
        use crate::routing::RoutingAuthority;
        use cosmix_mesh::PeerConfig;

        let etc_roster = vec![PeerConfig {
            name: "beta".into(),
            mesh_ip: "192.0.2.99".parse().unwrap(),
            noded_port: 4300,
        }];
        let verified_posture = Posture::Verified(Accepted {
            epoch: 7,
            recovery_generation: 0,
            via_recovery: false,
            mesh: "example.internal".into(),
            hash: "authority-hash".into(),
            verified_by: vec![],
            verify_keys: vec![],
            members: vec![],
            routing_view: vec![
                RoutingMember::ActiveBus {
                    name: "alpha".into(),
                    mesh_ip: "192.0.2.5".parse().unwrap(),
                    noded_port: 4200,
                },
                RoutingMember::ActiveBus {
                    name: "beta".into(),
                    mesh_ip: "192.0.2.6".parse().unwrap(),
                    noded_port: 4300,
                },
            ],
            members_full: std::collections::BTreeMap::new(),
        });
        let verified =
            RoutingAuthority::new(verified_posture, "alpha", "192.0.2.5", 4200, &etc_roster);

        let body = noded_peers_body("alpha", "192.0.2.5:0", 4200, &verified);
        // Existing fields and per-peer shape remain byte-semantically compatible.
        assert_eq!(body["node"], "alpha");
        assert_eq!(body["wg_ip"], "192.0.2.5");
        assert_eq!(body["port"], 4200);
        assert_eq!(body["peers"][0]["name"], "beta");
        assert_eq!(body["peers"][0]["wg_ip"], "192.0.2.6");
        assert_eq!(body["peers"][0]["port"], 4300);
        assert_eq!(body["source"], "signed-inventory");
        assert_eq!(body["authority"]["epoch"], 7);
        assert_eq!(body["authority"]["hash"], "authority-hash");
        assert_eq!(body["authority"]["self"], "active-bus");
        assert_eq!(body["authority"]["routing_view"][0]["class"], "active-bus");
        assert_eq!(body["authority"]["routing_view"][0]["noded_port"], 4200);

        let removed_self = RoutingAuthority::new(
            Posture::Verified(Accepted {
                epoch: 8,
                recovery_generation: 0,
                via_recovery: false,
                mesh: "example.internal".into(),
                hash: "removed-self-hash".into(),
                verified_by: vec![],
                verify_keys: vec![],
                members: vec![],
                routing_view: vec![RoutingMember::ActiveBus {
                    name: "beta".into(),
                    mesh_ip: "192.0.2.6".parse().unwrap(),
                    noded_port: 4300,
                }],
                members_full: std::collections::BTreeMap::new(),
            }),
            "alpha",
            "192.0.2.5",
            4200,
            &etc_roster,
        );
        let removed_body = noded_peers_body("alpha", "192.0.2.5:0", 4200, &removed_self);
        assert_eq!(removed_body["peers"], serde_json::json!([]));
        assert_eq!(removed_body["authority"]["self"], "missing");

        let unverified = RoutingAuthority::new(
            Posture::Unverified {
                reason: "migration".into(),
            },
            "alpha",
            "192.0.2.5",
            4200,
            &etc_roster,
        );
        let fallback = noded_peers_body("alpha", "192.0.2.5:0", 4200, &unverified);
        assert_eq!(fallback["source"], "etc-roster-fallback");
        assert_eq!(fallback["peers"][0]["wg_ip"], "192.0.2.99");
        assert_eq!(fallback["peers"][0]["port"], 4300);
        assert!(fallback.get("authority").is_none());
    }

    /// SPEC 13 §14 / §7.8 B3 (slice 2-c-0): `drain_for_channel` enumerates +
    /// removes only the calling session's in-flight entries, leaving other
    /// sessions' entries intact, and preserves each entry's captured §14
    /// `class` + caller identity — the population the `delivery.inflight_fate`
    /// churn instrumentation records at session drop.
    #[tokio::test]
    async fn drain_for_channel_scopes_to_one_session() {
        use super::PendingResponseTable;
        use cosmix_bus::bus::BusMessage;
        use tokio::sync::mpsc;

        let table = PendingResponseTable::new();
        let (tx_a, _rx_a) = mpsc::channel::<String>(4);
        let (tx_b, _rx_b) = mpsc::channel::<String>(4);

        // Two rpc requests (no `type` header → class `rpc`) from session A.
        for id in ["a1", "a2"] {
            let mut m = BusMessage::new();
            m.set("id", id);
            assert!(
                table
                    .register(&mut m, &tx_a, Some("caller-a"), &tx_a)
                    .await
                    .is_some()
            );
        }
        // An id-bearing `event` from session B — class must be captured as
        // `event`, not assumed `rpc` (register does not enforce request-only).
        let mut mb = BusMessage::new();
        mb.set("type", "event");
        mb.set("id", "b1");
        assert!(
            table
                .register(&mut mb, &tx_b, Some("caller-b"), &tx_b)
                .await
                .is_some()
        );

        // Draining A returns A's two entries — caller ids preserved, class
        // `rpc` — and leaves B untouched.
        let drained_a = table.drain_for_channel(&tx_a).await;
        assert_eq!(drained_a.len(), 2);
        assert!(drained_a.iter().all(|(_, p)| p.class == "rpc"));
        let mut a_ids: Vec<String> = drained_a.iter().map(|(_, p)| p.caller_id.clone()).collect();
        a_ids.sort();
        assert_eq!(a_ids, vec!["a1".to_string(), "a2".to_string()]);

        // B's entry survived A's drain; its captured class is `event`.
        let drained_b = table.drain_for_channel(&tx_b).await;
        assert_eq!(drained_b.len(), 1);
        assert_eq!(drained_b[0].1.caller_id, "b1");
        assert_eq!(drained_b[0].1.class, "event");

        // Table is now empty; a re-drain of a drained channel is a no-op.
        assert!(table.drain_for_channel(&tx_a).await.is_empty());
    }

    /// SPEC 13 §9a B1 self-check (2-c-1a): a `0.0.0.0`/`::`/unparseable bind is
    /// not WG-bound (fails closed); a specific mesh IP is.
    #[test]
    fn bind_is_wg_requires_own_wg_ip() {
        use super::bind_is_wg;
        let wg = "192.0.2.5";
        assert!(bind_is_wg("192.0.2.5:4200", wg), "bound to own wg_ip");
        assert!(!bind_is_wg("0.0.0.0:4200", wg), "0.0.0.0 exposes off-mesh");
        assert!(!bind_is_wg("[::]:4200", wg), ":: exposes off-mesh");
        assert!(
            !bind_is_wg("127.0.0.1:4200", wg),
            "loopback is not the wg ip"
        );
        assert!(
            !bind_is_wg("192.168.1.5:4200", wg),
            "a LAN/public ip is not wg"
        );
        assert!(!bind_is_wg("not-an-addr", wg), "unparseable fails closed");
        assert!(
            !bind_is_wg("192.0.2.5:4200", "bad-ip"),
            "bad wg_ip fails closed"
        );
    }

    /// SPEC 13 §9a (2-c-1a): the d2-seed read returns `None` (prover-incapable,
    /// not an error) on missing/short/non-base64, `Some` on a valid 32-byte seed.
    #[test]
    fn load_d2_seed_none_on_missing_or_malformed() {
        use super::load_d2_seed;
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD;
        assert!(load_d2_seed(std::path::Path::new("/nonexistent/d2.seed")).is_none());
        let dir = std::env::temp_dir();
        let good = dir.join("cosmix_test_d2_good.seed");
        std::fs::write(&good, b64.encode([7u8; 32])).unwrap();
        assert_eq!(load_d2_seed(&good), Some([7u8; 32]));
        std::fs::remove_file(&good).ok();
        let short = dir.join("cosmix_test_d2_short.seed");
        std::fs::write(&short, b64.encode([7u8; 16])).unwrap();
        assert!(load_d2_seed(&short).is_none());
        std::fs::remove_file(&short).ok();
        let bad = dir.join("cosmix_test_d2_bad.seed");
        std::fs::write(&bad, "!!! not base64 !!!").unwrap();
        assert!(load_d2_seed(&bad).is_none());
        std::fs::remove_file(&bad).ok();
    }

    /// SPEC 13 §9a (2-c-2a): the seed-read classifier distinguishes the normal
    /// pre-ceremony **absent** state (silent `None`) from a present-but-
    /// **unreadable** file (the perms bug — LOUD) and a malformed body. Tested
    /// purely (no root / no filesystem) so the `PermissionDenied` arm is
    /// covered regardless of the test user.
    #[test]
    fn classify_seed_read_distinguishes_absent_from_unreadable() {
        use super::{SeedRead, classify_seed_read};
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD;
        // NotFound → Absent (silent — normal before the d2 ceremony).
        assert_eq!(
            classify_seed_read(Err(std::io::Error::from(std::io::ErrorKind::NotFound))),
            SeedRead::Absent
        );
        // Any other IO error (esp. PermissionDenied) → Unreadable (LOUD).
        match classify_seed_read(Err(std::io::Error::from(
            std::io::ErrorKind::PermissionDenied,
        ))) {
            SeedRead::Unreadable(_) => {}
            other => panic!("expected Unreadable, got {other:?}"),
        }
        // Readable + valid base64-32 → Loaded.
        assert_eq!(
            classify_seed_read(Ok(b64.encode([7u8; 32]))),
            SeedRead::Loaded([7u8; 32])
        );
        // Readable but wrong length / not base64 → Malformed (LOUD).
        assert_eq!(
            classify_seed_read(Ok(b64.encode([7u8; 16]))),
            SeedRead::Malformed
        );
        assert_eq!(
            classify_seed_read(Ok("!!! not base64 !!!".into())),
            SeedRead::Malformed
        );
    }

    /// SPEC 13 §9a (2-c-1a): observe never refuses; enforce degrades to
    /// `refuse-all` (fail closed) on no-trust-root or a non-WG bind.
    #[test]
    fn admission_effective_fail_closed() {
        use super::admission_effective;
        use cosmix_config::node::AdmissionMode::{Enforce, Observe, Off};
        assert_eq!(admission_effective(Off, true, true), ("off", ""));
        assert_eq!(admission_effective(Observe, false, false), ("observe", ""));
        assert_eq!(admission_effective(Enforce, true, true), ("enforce", ""));
        assert_eq!(
            admission_effective(Enforce, true, false),
            ("refuse-all", "no-verified-trust-root")
        );
        assert_eq!(
            admission_effective(Enforce, false, true),
            ("refuse-all", "non-wg-bind")
        );
    }

    /// SPEC 13 §9a §4 (2-c-2b): the same-node classifier is loopback OR the
    /// broker's OWN bind IP — every other WG `/24` address (hub, any peer,
    /// arbitrary) is inter-node. This is the local-vs-network boundary, never
    /// the proof.
    #[test]
    fn same_node_origin_is_loopback_or_own_bind_only() {
        use super::is_same_node_origin;
        let bind = "192.0.2.5:4200"; // this node's own WG bind
        assert!(is_same_node_origin("127.0.0.1".parse().unwrap(), bind));
        assert!(is_same_node_origin("::1".parse().unwrap(), bind));
        assert!(
            is_same_node_origin("192.0.2.5".parse().unwrap(), bind),
            "own bind IP"
        );
        // hub, another member, an arbitrary /24 address → all inter-node.
        assert!(
            !is_same_node_origin("192.0.2.1".parse().unwrap(), bind),
            "hub"
        );
        assert!(
            !is_same_node_origin("192.0.2.7".parse().unwrap(), bind),
            "peer"
        );
        assert!(
            !is_same_node_origin("192.0.2.99".parse().unwrap(), bind),
            "arbitrary"
        );
        // An unparseable bind classifies a non-loopback source as inter-node
        // (fail-closed: never accidentally same-node).
        assert!(!is_same_node_origin(
            "192.0.2.7".parse().unwrap(),
            "bad-bind"
        ));
    }

    /// SPEC 13 §9a (2-c-2b): the fail-closed predicate — `None` for off/observe
    /// and a healthy enforce; the §17 reason (trust-root before bind) otherwise.
    #[test]
    fn enforce_refuses_all_predicate() {
        use super::enforce_refuses_all;
        use cosmix_config::node::AdmissionMode::{Enforce, Observe, Off};
        assert_eq!(enforce_refuses_all(Off, false, false), None);
        assert_eq!(enforce_refuses_all(Observe, false, false), None);
        assert_eq!(enforce_refuses_all(Enforce, true, true), None);
        assert_eq!(
            enforce_refuses_all(Enforce, true, false),
            Some("no-verified-trust-root")
        );
        assert_eq!(
            enforce_refuses_all(Enforce, false, true),
            Some("non-wg-bind")
        );
        // Trust-root is checked before bind (same order as admission_effective).
        assert_eq!(
            enforce_refuses_all(Enforce, false, false),
            Some("no-verified-trust-root")
        );
    }

    /// SPEC 13 §9a (2-c-2b) — THE security matrix as a pure-function test (a
    /// loopback integration client would always classify same-node and never
    /// reach the gated path). Covers every row of the design's enforce table.
    #[test]
    fn register_gate_security_matrix() {
        use super::{RegisterGate, SessionAdmission, register_gate_decision};
        use cosmix_config::node::AdmissionMode::{Enforce, Observe, Off};

        let bind = "192.0.2.5:4200";
        let loopback: std::net::IpAddr = "127.0.0.1".parse().unwrap();
        let inter: std::net::IpAddr = "192.0.2.7".parse().unwrap(); // a peer
        let none = SessionAdmission::default(); // no proof seen
        let proved = |n: &str| SessionAdmission {
            admitted_node: Some(n.to_string()),
            last_detail: None,
            response_seen: true,
        };
        let refused = |d: &'static str| SessionAdmission {
            admitted_node: None,
            last_detail: Some(d),
            response_seen: true,
        };

        // off/observe never gate (caller also guards, but the fn is explicit).
        assert_eq!(
            register_gate_decision(inter, bind, Off, true, true, "bridge-delta", &none),
            RegisterGate::Ungated
        );
        assert_eq!(
            register_gate_decision(inter, bind, Observe, true, true, "bridge-delta", &none),
            RegisterGate::Ungated
        );

        // same-node (loopback / own bind) + NO proof → ungated (local citizen).
        assert_eq!(
            register_gate_decision(loopback, bind, Enforce, true, true, "webd", &none),
            RegisterGate::Ungated
        );
        assert_eq!(
            register_gate_decision(
                "192.0.2.5".parse().unwrap(),
                bind,
                Enforce,
                true,
                true,
                "webd",
                &none
            ),
            RegisterGate::Ungated
        );

        // inter-node + no proof → refused no-proof, with a synthetic observed.
        assert_eq!(
            register_gate_decision(inter, bind, Enforce, true, true, "bridge-delta", &none),
            RegisterGate::Refuse {
                detail: "no-proof",
                source_node: "delta".into(),
                synth_observed: true
            }
        );

        // inter-node + a bad-sig response → refused with the REAL reason, no
        // synthetic observed (process_admit_response already emitted one).
        assert_eq!(
            register_gate_decision(
                inter,
                bind,
                Enforce,
                true,
                true,
                "bridge-delta",
                &refused("bad-credential-signature")
            ),
            RegisterGate::Refuse {
                detail: "bad-credential-signature",
                source_node: "delta".into(),
                synth_observed: false
            }
        );

        // inter-node + valid proof for the registered node → admit (proven name).
        assert_eq!(
            register_gate_decision(
                inter,
                bind,
                Enforce,
                true,
                true,
                "bridge-delta",
                &proved("delta")
            ),
            RegisterGate::Admit("delta".into())
        );

        // prove-as-delta / register-as-bridge-beta → name-mismatch (the BLOCKER).
        assert_eq!(
            register_gate_decision(
                inter,
                bind,
                Enforce,
                true,
                true,
                "bridge-beta",
                &proved("delta")
            ),
            RegisterGate::Refuse {
                detail: "name-mismatch",
                source_node: "beta".into(),
                synth_observed: false
            }
        );

        // inter-node registering a PLAIN service name (no bridge- prefix) →
        // no-proof (a remote dodge, §4).
        assert_eq!(
            register_gate_decision(inter, bind, Enforce, true, true, "webd", &none),
            RegisterGate::Refuse {
                detail: "no-proof",
                source_node: "webd".into(),
                synth_observed: true
            }
        );

        // fail-closed: Unverified trust root → refuse-all every inter-node.
        assert_eq!(
            register_gate_decision(
                inter,
                bind,
                Enforce,
                true,
                false,
                "bridge-delta",
                &proved("delta")
            ),
            RegisterGate::Refuse {
                detail: "no-verified-trust-root",
                source_node: "delta".into(),
                synth_observed: false
            }
        );
        // fail-closed: non-WG bind → refuse-all every inter-node.
        assert_eq!(
            register_gate_decision(
                inter,
                bind,
                Enforce,
                false,
                true,
                "bridge-delta",
                &proved("delta")
            ),
            RegisterGate::Refuse {
                detail: "non-wg-bind",
                source_node: "delta".into(),
                synth_observed: false
            }
        );
    }

    /// SPEC 13 §7.7 (slice 2-c-0b): the reload swap rule keeps last-known-good
    /// ONLY when a fresh `Unverified` would replace a live `Verified` — every
    /// other transition (verified advance, or recovery from Unverified) applies.
    #[test]
    fn reload_keeps_last_good_only_on_downgrade() {
        use super::reload_keeps_last_good;
        use crate::authority::{Accepted, Posture};

        let verified = || {
            Posture::Verified(Accepted {
                epoch: 1,
                recovery_generation: 0,
                via_recovery: false,
                mesh: "bus".into(),
                hash: "h".into(),
                verified_by: vec![],
                verify_keys: vec![],
                members: vec![],
                routing_view: vec![],
                members_full: std::collections::BTreeMap::new(),
            })
        };
        let unverified = || Posture::Unverified {
            reason: "bad push".into(),
        };

        // The one keep-last-good case: a bad push over a live trust root.
        assert!(reload_keeps_last_good(&unverified(), &verified()));
        // A valid (re-)load over a verified root is applied (monotonicity in
        // load_and_verify guards a real rollback).
        assert!(!reload_keeps_last_good(&verified(), &verified()));
        // No good to keep — apply (stays fail-closed).
        assert!(!reload_keeps_last_good(&unverified(), &unverified()));
        // Recovery from a fail-closed boot is applied.
        assert!(!reload_keeps_last_good(&verified(), &unverified()));
    }

    fn reload_posture(
        epoch: u64,
        routing_view: Vec<crate::authority::RoutingMember>,
    ) -> crate::authority::Posture {
        crate::authority::Posture::Verified(crate::authority::Accepted {
            epoch,
            recovery_generation: 0,
            via_recovery: false,
            mesh: "bus".into(),
            hash: format!("hash-{epoch}"),
            verified_by: vec![],
            verify_keys: vec![],
            members: vec![],
            routing_view,
            members_full: std::collections::BTreeMap::new(),
        })
    }

    fn active_route(
        name: &str,
        mesh_ip: std::net::IpAddr,
        noded_port: u16,
    ) -> crate::authority::RoutingMember {
        crate::authority::RoutingMember::ActiveBus {
            name: name.into(),
            mesh_ip,
            noded_port,
        }
    }

    async fn reload_test_state(
        posture: crate::authority::Posture,
        etc_roster: Vec<PeerConfig>,
    ) -> AppState {
        let listener_port = DEFAULT_NODED_PORT;
        let authority_value = crate::routing::RoutingAuthority::new(
            posture,
            "alpha",
            "127.0.0.1",
            listener_port,
            &etc_roster,
        );
        let (mesh_tx, _mesh_rx) = mpsc::unbounded_channel();
        let mesh = Arc::new(MeshPeers::new(
            MeshConfig {
                node_name: "alpha".into(),
                peers: etc_roster.clone(),
                d2_seed: None,
            },
            mesh_tx,
        ));
        mesh.reconcile_endpoints(
            authority_value.routes.desired_endpoints(),
            authority_value.revision(),
        )
        .await;
        let broker = Arc::new(SubscriptionBroker::new());
        let started = Instant::now();
        AppState {
            registry: Arc::new(RwLock::new(HashMap::new())),
            pending_responses: Arc::new(PendingResponseTable::new()),
            tap_subscribers: Arc::new(RwLock::new(Vec::new())),
            observe: ObserveManager::new(Vec::new()),
            mesh,
            node_name: "alpha".into(),
            broker: broker.clone(),
            bind: format!("127.0.0.1:{listener_port}"),
            log_level: "info".into(),
            started_at: chrono::Utc::now().to_rfc3339(),
            started,
            spec_dir: None,
            spec_release: None,
            change_bus: crate::props::ChangeBus::new(broker),
            authority: Arc::new(ArcSwap::from_pointee(authority_value)),
            delivery_fence: Arc::new(std::sync::RwLock::new(())),
            etc_roster: Arc::new(etc_roster),
            wg_ip: "127.0.0.1".into(),
            listener_port,
            listener_endpoint_diverged: Arc::new(AtomicBool::new(false)),
            admission_mode: AdmissionMode::Off,
            wg_bound: true,
            challenge_table: Arc::new(crate::admission::ChallengeTable::new()),
            live_sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    #[tokio::test]
    async fn spec_release_dispatch_preserves_wire_correlation() {
        let ip = "127.0.0.1".parse().unwrap();
        let mut state = reload_test_state(
            reload_posture(1, vec![active_route("alpha", ip, DEFAULT_NODED_PORT)]),
            vec![],
        )
        .await;
        let fixture = crate::spec_release::tests::Fixture::new();
        state.spec_release = Some(Arc::new(fixture.load().unwrap()));
        for (command, args, rc) in [
            ("spec.get", serde_json::json!({"name":"04-wire"}), "0"),
            ("spec.get", serde_json::json!({"chapter":1.9}), "10"),
            ("spec.v2.get", serde_json::json!({"document":"wire"}), "0"),
        ] {
            let mut request = BusMessage::new()
                .with_header("command", command)
                .with_header("id", "request-123");
            request.body = args.to_string();
            let (tx, mut rx) = mpsc::channel(1);
            let mut service = None;
            let mut outcome = super::ObserveOutcome::BrokerHandled;
            super::handle_noded_command(
                &request,
                &tx,
                &state,
                &mut service,
                "anon",
                ip,
                &mut outcome,
            )
            .await;
            let wire = rx.recv().await.unwrap();
            let response = cosmix_bus::bus::parse(&wire).unwrap();
            assert_eq!(response.get("id"), Some("request-123"));
            assert_eq!(response.get("type"), Some("response"));
            assert_eq!(response.get("from"), Some("noded"));
            assert_eq!(response.get("command"), Some(command));
            assert_eq!(response.get("rc"), Some(rc));
            if command == "spec.get" && rc == "0" {
                assert!(wire.ends_with("---\n# Wire\n"));
                // Existing Bus parsing trims trailing prose whitespace. V2
                // protects the original bytes inside a JSON string instead.
                assert_eq!(response.body, "# Wire");
            }
        }
    }

    #[tokio::test]
    async fn reload_publishes_before_accepted_and_applied_events() {
        let ip: std::net::IpAddr = "127.0.0.1".parse().unwrap();
        let state = reload_test_state(
            reload_posture(1, vec![active_route("alpha", ip, DEFAULT_NODED_PORT)]),
            vec![],
        )
        .await;
        let names = Arc::new(StdMutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(RecordEventNames(names.clone()));
        let _guard = tracing::subscriber::set_default(subscriber);

        apply_inventory_reload(
            &state,
            reload_posture(2, vec![active_route("alpha", ip, DEFAULT_NODED_PORT)]),
        )
        .await;

        assert_eq!(state.authority.load().revision().epoch, 2);
        let names = names.lock().unwrap();
        let accepted = names
            .iter()
            .position(|name| name == "inventory.accepted")
            .expect("inventory.accepted event");
        let applied = names
            .iter()
            .position(|name| name == "routing.reload_applied")
            .expect("routing.reload_applied event");
        assert!(accepted < applied, "events: {names:?}");
    }

    #[tokio::test]
    async fn kept_last_good_performs_zero_transport_reconciliation() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let accepted_server = accepted.clone();
        let (messages_tx, mut messages_rx) = mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            accepted_server.fetch_add(1, Ordering::SeqCst);
            let mut websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            websocket
                .send(tokio_tungstenite::tungstenite::Message::Ping(
                    Vec::new().into(),
                ))
                .await
                .unwrap();
            assert!(websocket.next().await.is_some(), "registration frame");
            while let Some(Ok(_)) = websocket.next().await {
                let _ = messages_tx.send(());
            }
        });
        let self_ip: std::net::IpAddr = "127.0.0.1".parse().unwrap();
        let beta = PeerConfig {
            name: "beta".into(),
            mesh_ip: endpoint.ip(),
            noded_port: endpoint.port(),
        };
        let state = reload_test_state(
            reload_posture(
                1,
                vec![
                    active_route("alpha", self_ip, DEFAULT_NODED_PORT),
                    active_route("beta", endpoint.ip(), endpoint.port()),
                ],
            ),
            vec![],
        )
        .await;
        state
            .mesh
            .send(beta.clone(), BusMessage::new())
            .await
            .unwrap();
        messages_rx.recv().await.expect("first message");

        apply_inventory_reload(
            &state,
            crate::authority::Posture::Unverified {
                reason: "bad push".into(),
            },
        )
        .await;

        state.mesh.send(beta, BusMessage::new()).await.unwrap();
        messages_rx.recv().await.expect("second message");
        assert_eq!(accepted.load(Ordering::SeqCst), 1);
        assert_eq!(state.authority.load().revision().epoch, 1);
        server.abort();
    }

    #[test]
    fn listener_divergence_and_convergence_are_stateful_events() {
        let ip: std::net::IpAddr = "127.0.0.1".parse().unwrap();
        let diverged_authority = crate::routing::RoutingAuthority::new(
            reload_posture(1, vec![active_route("alpha", ip, 4300)]),
            "alpha",
            "127.0.0.1",
            DEFAULT_NODED_PORT,
            &[],
        );
        let converged_authority = crate::routing::RoutingAuthority::new(
            reload_posture(2, vec![active_route("alpha", ip, DEFAULT_NODED_PORT)]),
            "alpha",
            "127.0.0.1",
            DEFAULT_NODED_PORT,
            &[],
        );
        let state = AtomicBool::new(false);
        let names = Arc::new(StdMutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(RecordEventNames(names.clone()));
        let _guard = tracing::subscriber::set_default(subscriber);

        emit_listener_endpoint_state(
            &diverged_authority,
            "alpha",
            DEFAULT_NODED_PORT,
            &state,
            false,
        );
        emit_listener_endpoint_state(
            &diverged_authority,
            "alpha",
            DEFAULT_NODED_PORT,
            &state,
            true,
        );
        emit_listener_endpoint_state(
            &converged_authority,
            "alpha",
            DEFAULT_NODED_PORT,
            &state,
            true,
        );
        emit_listener_endpoint_state(
            &converged_authority,
            "alpha",
            DEFAULT_NODED_PORT,
            &state,
            true,
        );

        let names = names.lock().unwrap();
        assert_eq!(
            names
                .iter()
                .filter(|name| *name == "listener.endpoint_diverged")
                .count(),
            2
        );
        assert_eq!(
            names
                .iter()
                .filter(|name| *name == "listener.endpoint_converged")
                .count(),
            1
        );
    }

    /// SPEC 13 §5.5 (2-c-2c) — the reload membership-recheck matrix. A live
    /// session closes iff its proven member is no longer admissible at the NEW
    /// epoch (tombstoned / bus:false / removed / d2 credential lapsed); a
    /// still-valid member — including a key-rotation overlap and a plain epoch
    /// bump with the credential still in-window — grandfathers (returns None).
    #[test]
    fn reload_revoke_detail_recheck_matrix() {
        use super::reload_revoke_detail;
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD;
        let pk = b64.encode([3u8; 32]);
        let d2 = |from: u64, until: serde_json::Value| serde_json::json!({ "kind": "d2", "pubkey": pk, "from_epoch": from, "until_epoch": until });
        let member = |status: &str, bus: bool, creds: serde_json::Value| serde_json::json!({ "name": "delta", "status": status, "bus": bus, "credentials": creds });
        let open = || serde_json::json!([d2(1, serde_json::Value::Null)]);

        // Removed entirely (absent from the new inventory) → tombstoned.
        assert_eq!(reload_revoke_detail(None, 5), Some("source-tombstoned"));
        // Status revoked (tombstone) → source-tombstoned.
        assert_eq!(
            reload_revoke_detail(Some(&member("revoked", true, open())), 5),
            Some("source-tombstoned")
        );
        // bus:false → source-bus-false.
        assert_eq!(
            reload_revoke_detail(Some(&member("active", false, open())), 5),
            Some("source-bus-false")
        );
        // Credential window lapsed ([1,4), new epoch 5) → no-current-d2-credential.
        assert_eq!(
            reload_revoke_detail(
                Some(&member(
                    "active",
                    true,
                    serde_json::json!([d2(1, serde_json::json!(4))])
                )),
                5
            ),
            Some("no-current-d2-credential")
        );
        // Still valid (open-ended cred) → grandfather (None).
        assert_eq!(
            reload_revoke_detail(Some(&member("active", true, open())), 5),
            None
        );
        // Epoch bump, credential STILL valid ([1,9), epoch 5) → grandfather.
        assert_eq!(
            reload_revoke_detail(
                Some(&member(
                    "active",
                    true,
                    serde_json::json!([d2(1, serde_json::json!(9))])
                )),
                5
            ),
            None
        );
        // Key-rotation overlap: outgoing [1,6) + incoming [5,∞), epoch 5 — both
        // cover it → untouched (the §5.5/§6.1 grandfather, NOT a close).
        assert_eq!(
            reload_revoke_detail(
                Some(&member(
                    "active",
                    true,
                    serde_json::json!([
                        d2(1, serde_json::json!(6)),
                        d2(5, serde_json::Value::Null)
                    ])
                )),
                5
            ),
            None
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn flood_to_self_subscribed_peer_is_bounded() {
        let warns = Arc::new(AtomicU64::new(0));
        let _guard = tracing_subscriber::registry()
            .with(CountWarns(warns.clone()))
            .set_default();

        let Some(port) = pick_port() else { return };
        let listen = format!("127.0.0.1:{port}");
        let url = format!("ws://{listen}/ws");

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let listen_for_run = listen.clone();
        tokio::spawn(async move {
            let _ = super::run(
                super::RunConfig {
                    listen: listen_for_run,
                    node: "test-node".into(),
                    wg_ip: "127.0.0.1".into(),
                    mesh_config_path: None,
                    spec_dir: None,
                    admission_mode: cosmix_config::node::AdmissionMode::Off,
                    observe_allowed_services: Vec::new(),
                },
                ready_tx,
            )
            .await;
        });
        ready_rx.await.expect("broker failed to bind");

        // Mimic the pre-fix logger: one connection that is BOTH a registered
        // service and a noded.tap subscriber. Crucially we never drain its
        // incoming queue, so its outbound mpsc(256) saturates almost
        // immediately under flood.
        let victim = cosmix_client::NodedClient::connect("victim", &url)
            .await
            .expect("victim connect");
        victim
            .call("noded", "noded.tap", serde_json::Value::Null)
            .await
            .expect("noded.tap subscribe");

        let flooder = cosmix_client::NodedClient::connect_anonymous(&url)
            .await
            .expect("flooder connect");

        // Fire-and-forget flood; we don't care about responses.
        for _ in 0..10_000 {
            let _ = flooder
                .send("victim", "noop", serde_json::Value::Null)
                .await;
        }

        // Allow the broker to drain queued work and the WARN_COALESCE_MS
        // window (1s) to flush its trailing summary line.
        tokio::time::sleep(Duration::from_millis(2500)).await;

        // Pre-fix this hit thousands. Post-fix the broker emits at most a
        // handful: one route_local-drop summary per second (≤3) and zero
        // tap-drop summaries (deduped against the routed peer). Also tolerate
        // a few unrelated info/debug-adjacent warns from startup.
        let n = warns.load(Ordering::Relaxed);
        assert!(
            n < 50,
            "expected coalesced WARNs, got {n} — drop dedup or rate-limit regressed"
        );

        // Broker still routes for fresh callers — proves the props/control
        // plane wasn't starved by the flood.
        let probe = cosmix_client::NodedClient::connect_anonymous(&url)
            .await
            .expect("probe connect");
        let pong = tokio::time::timeout(
            Duration::from_secs(2),
            probe.call("noded", "noded.ping", serde_json::Value::Null),
        )
        .await
        .expect("noded.ping timed out — broker starved")
        .expect("noded.ping failed");
        assert!(pong.get("pong").is_some(), "missing pong: {pong:?}");
    }

    /// SPEC 13 §9a (2-c-1b) — broker-speaks-first old-client interop (design §8
    /// MAJOR). An `observe` broker sends a `noded.admit.challenge` as its first
    /// frame; a client that does not know the verb MUST ignore it and still
    /// function (the challenge is fire-and-forget; the read loop never awaits a
    /// response). On a host with a verified inventory the challenge is actually
    /// sent (exercising the path); otherwise the broker is Unverified and this
    /// is a baseline — either way the session must work.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn observe_mode_does_not_break_an_old_client() {
        let Some(port) = pick_port() else { return };
        let listen = format!("127.0.0.1:{port}");
        let url = format!("ws://{listen}/ws");

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let listen_for_run = listen.clone();
        tokio::spawn(async move {
            let _ = super::run(
                super::RunConfig {
                    listen: listen_for_run,
                    node: "test-node".into(),
                    wg_ip: "127.0.0.1".into(),
                    mesh_config_path: None,
                    spec_dir: None,
                    admission_mode: cosmix_config::node::AdmissionMode::Observe,
                    observe_allowed_services: Vec::new(),
                },
                ready_tx,
            )
            .await;
        });
        ready_rx.await.expect("broker failed to bind");

        // An "old" client: connects + registers, never handles the challenge.
        let client = cosmix_client::NodedClient::connect("oldclient", &url)
            .await
            .expect("old client connect");
        let pong = tokio::time::timeout(
            Duration::from_secs(2),
            client.call("noded", "noded.ping", serde_json::Value::Null),
        )
        .await
        .expect("noded.ping timed out — broker-speaks-first broke the session")
        .expect("noded.ping failed");
        assert!(pong.get("pong").is_some(), "missing pong: {pong:?}");
    }

    // ─────────────────────────────────────────────────────────────
    // SPEC 12 / C10b — `noded.props.subscribe_grant` verb tests
    //
    // These exercise the verb's parsing, auth gate, and broker call
    // via a raw WebSocket round-trip. `cosmix-lib-client::call()`
    // can't be used directly because the verb requires custom Bus
    // headers (`topic`, `target_peer`, `namespace`) and inspecting
    // the response body's `subscription_id`/`namespace` payload.
    // ─────────────────────────────────────────────────────────────

    use cosmix_bus::bus::{self as bus_mod, BusMessage};
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    /// Open a raw WebSocket to the broker, return the sink/stream pair.
    async fn raw_connect(
        url: &str,
    ) -> (
        futures_util::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            WsMessage,
        >,
        futures_util::stream::SplitStream<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
        >,
    ) {
        let (ws, _resp) = tokio_tungstenite::connect_async(url)
            .await
            .expect("raw ws connect");
        ws.split()
    }

    /// Send a raw Bus message and wait for a response with the matching
    /// id. Anything else (orphan or unrelated traffic) is skipped.
    async fn raw_call(
        sink: &mut futures_util::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            WsMessage,
        >,
        stream: &mut futures_util::stream::SplitStream<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
        >,
        req: BusMessage,
        id: &str,
    ) -> BusMessage {
        sink.send(WsMessage::Text(req.to_wire().into()))
            .await
            .expect("ws send");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let frame = tokio::time::timeout(remaining, stream.next())
                .await
                .expect("raw_call response timeout")
                .expect("stream closed")
                .expect("ws frame");
            if let WsMessage::Text(text) = frame {
                let parsed = match bus_mod::parse(&text) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if parsed.message_type() == Some("response")
                    && parsed.get("id").map(|i| i == id).unwrap_or(false)
                {
                    return parsed;
                }
            }
        }
    }

    /// Register a service name on a raw connection by issuing
    /// `noded.register` and awaiting rc=0.
    async fn raw_register(
        sink: &mut futures_util::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            WsMessage,
        >,
        stream: &mut futures_util::stream::SplitStream<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
        >,
        name: &str,
    ) {
        let id = format!("reg-{name}");
        let req = BusMessage::new()
            .with_header("command", "noded.register")
            .with_header("from", name)
            .with_header("to", "noded")
            .with_header("type", "request")
            .with_header("id", &id);
        let resp = raw_call(sink, stream, req, &id).await;
        assert_eq!(
            resp.get("rc"),
            Some("0"),
            "register {name} failed: {:?}",
            resp.get("error")
        );
    }

    /// Spawn the broker on a free loopback port and return the ws URL.
    async fn spawn_broker() -> Option<String> {
        spawn_broker_with_observe(Vec::new()).await
    }

    async fn spawn_broker_with_observe(observe_allowed_services: Vec<String>) -> Option<String> {
        let port = pick_port()?;
        let listen = format!("127.0.0.1:{port}");
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let listen_for_run = listen.clone();
        tokio::spawn(async move {
            let _ = super::run(
                super::RunConfig {
                    listen: listen_for_run,
                    node: "test-node".into(),
                    wg_ip: "127.0.0.1".into(),
                    mesh_config_path: None,
                    spec_dir: None,
                    admission_mode: cosmix_config::node::AdmissionMode::Off,
                    observe_allowed_services,
                },
                ready_tx,
            )
            .await;
        });
        ready_rx.await.expect("broker bind");
        Some(format!("ws://{listen}/ws"))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn observe_wire_gate_and_event_delivery_follow_registered_allowlist() {
        let Some(url) = spawn_broker_with_observe(vec!["tower-bevy-*".into()]).await else {
            return;
        };

        let anonymous = cosmix_client::NodedClient::connect_anonymous(&url)
            .await
            .expect("anonymous connect");
        let denied = anonymous
            .call(
                "noded",
                "noded.observe.start",
                serde_json::json!({"body":"none"}),
            )
            .await
            .unwrap_err();
        assert!(denied.to_string().contains("observe_unauthorised"));

        let studio = cosmix_client::NodedClient::connect("studio-bevy-7", &url)
            .await
            .expect("studio register");
        let denied = studio
            .call(
                "noded",
                "noded.observe.start",
                serde_json::json!({"body":"none"}),
            )
            .await
            .unwrap_err();
        assert!(denied.to_string().contains("observe_unauthorised"));

        let tower = cosmix_client::NodedClient::connect("tower-bevy-7", &url)
            .await
            .expect("tower register");
        let started = tower
            .call(
                "noded",
                "noded.observe.start",
                serde_json::json!({
                    "filter":{"verbs":["noded.list"],"directions":["local"]},
                    "body":"none",
                    "capacity":64
                }),
            )
            .await
            .expect("observe start");
        let subscription_id = started["subscription_id"]
            .as_str()
            .expect("subscription id")
            .to_string();
        let mut incoming = tower.incoming_async().await.expect("observer incoming");

        let sender = cosmix_client::NodedClient::connect("sender", &url)
            .await
            .expect("sender register");
        let ping = sender
            .call("noded", "noded.ping", serde_json::Value::Null)
            .await
            .expect("noded ping");
        assert_eq!(ping["extensions"]["observe"], "1.0");
        sender
            .call("noded", "noded.list", serde_json::Value::Null)
            .await
            .expect("generate observed command");
        let event = tokio::time::timeout(Duration::from_secs(2), incoming.recv())
            .await
            .expect("observe event timeout")
            .expect("observe event");
        assert_eq!(event.command, "noded.observe.event");
        assert_eq!(
            event.header("subscription_id"),
            Some(subscription_id.as_str())
        );
        let body: serde_json::Value = serde_json::from_str(&event.body).unwrap();
        assert_eq!(body["verb"], "noded.list");
        assert_eq!(body["outcome"], "broker_handled");

        let stopped = tower
            .call(
                "noded",
                "noded.observe.stop",
                serde_json::json!({"subscription_id":subscription_id}),
            )
            .await
            .expect("observe stop");
        assert_eq!(stopped["stopped"], true);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn observe_covers_rejected_broker_response_and_route_outcomes_truthfully() {
        let Some(url) = spawn_broker_with_observe(vec!["tower-bevy-*".into()]).await else {
            return;
        };

        let _bob = cosmix_client::NodedClient::connect("bob", &url)
            .await
            .expect("bob register");
        let (mut alice_sink, mut alice_stream) = raw_connect(&url).await;
        raw_register(&mut alice_sink, &mut alice_stream, "alice").await;

        let tower = cosmix_client::NodedClient::connect("tower-bevy-coverage", &url)
            .await
            .expect("tower register");
        tower
            .call(
                "noded",
                "noded.observe.start",
                serde_json::json!({"body":"none","capacity":64}),
            )
            .await
            .expect("observe start");
        let mut incoming = tower.incoming_async().await.expect("observer incoming");

        // Collision: the request's claimed `from:bob` must never become its
        // observed identity; this socket is canonically `alice`.
        let collision = BusMessage::new()
            .with_header("command", "noded.register")
            .with_header("from", "bob")
            .with_header("to", "noded")
            .with_header("type", "request")
            .with_header("id", "collision-1");
        assert_eq!(
            raw_call(&mut alice_sink, &mut alice_stream, collision, "collision-1")
                .await
                .get("rc"),
            Some("10")
        );

        let unknown_command = BusMessage::new()
            .with_header("command", "noded.no_such_command")
            .with_header("from", "alice")
            .with_header("to", "noded")
            .with_header("type", "request")
            .with_header("id", "unknown-command-1");
        assert_eq!(
            raw_call(
                &mut alice_sink,
                &mut alice_stream,
                unknown_command,
                "unknown-command-1"
            )
            .await
            .get("rc"),
            Some("10")
        );

        // Both response-only branches are broker outcomes, even though
        // neither has a live caller correlation.
        for response in [
            BusMessage::new()
                .with_header("command", "citizen.orphan")
                .with_header("from", "alice")
                .with_header("type", "response")
                .with_header("id", "orphan-1"),
            BusMessage::new()
                .with_header("command", "noded.admit.response")
                .with_header("from", "alice")
                .with_header("type", "response")
                .with_header("id", "admit-orphan-1"),
        ] {
            alice_sink
                .send(WsMessage::Text(response.to_wire().into()))
                .await
                .expect("send orphan response");
        }

        let missing_route = BusMessage::new()
            .with_header("command", "ghost.read")
            .with_header("from", "alice")
            .with_header("to", "ghost")
            .with_header("type", "request")
            .with_header("id", "missing-route-1");
        assert_eq!(
            raw_call(
                &mut alice_sink,
                &mut alice_stream,
                missing_route,
                "missing-route-1"
            )
            .await
            .get("rc"),
            Some("10")
        );

        let mut collision_canonical = false;
        let mut failed_command_rejected = false;
        let mut orphan_rejected = false;
        let mut admit_rejected = false;
        let mut route_request_rejected = false;
        let mut route_response_delivered = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let Ok(Some(event)) = tokio::time::timeout(remaining, incoming.recv()).await else {
                break;
            };
            if event.command != "noded.observe.event" {
                continue;
            }
            let body: serde_json::Value = serde_json::from_str(&event.body).unwrap();
            let verb = body["verb"].as_str();
            let correlation = body["correlation_id"].as_str();
            collision_canonical |= verb == Some("noded.register")
                && correlation == Some("collision-1")
                && body["outcome"] == "rejected"
                && body["from"] == "alice";
            failed_command_rejected |=
                verb == Some("noded.no_such_command") && body["outcome"] == "rejected";
            orphan_rejected |= verb == Some("citizen.orphan")
                && body["message_type"] == "response"
                && body["outcome"] == "rejected";
            admit_rejected |= verb == Some("noded.admit.response")
                && body["message_type"] == "response"
                && body["outcome"] == "rejected";
            route_request_rejected |= correlation == Some("missing-route-1")
                && body["message_type"] == "request"
                && body["outcome"] == "rejected";
            route_response_delivered |= correlation == Some("missing-route-1")
                && body["message_type"] == "response"
                && body["outcome"] == "delivered";
            if collision_canonical
                && failed_command_rejected
                && orphan_rejected
                && admit_rejected
                && route_request_rejected
                && route_response_delivered
            {
                break;
            }
        }
        assert!(
            collision_canonical,
            "collision used caller-claimed identity"
        );
        assert!(
            failed_command_rejected,
            "failed broker command looked handled"
        );
        assert!(orphan_rejected, "orphan response was not observed");
        assert!(admit_rejected, "admit response was not observed");
        assert!(route_request_rejected, "rejected route request was omitted");
        assert!(
            route_response_delivered,
            "synthetic rejected-route response was omitted"
        );
    }

    fn grant_msg(
        id: &str,
        from: &str,
        topic: &str,
        target_peer: Option<&str>,
        namespace: Option<&str>,
    ) -> BusMessage {
        let mut m = BusMessage::new()
            .with_header("command", "noded.props.subscribe_grant")
            .with_header("from", from)
            .with_header("to", "noded")
            .with_header("type", "request")
            .with_header("id", id)
            .with_header("topic", topic);
        if let Some(p) = target_peer {
            m = m.with_header("target_peer", p);
        }
        if let Some(n) = namespace {
            m = m.with_header("namespace", n);
        }
        m
    }

    fn props_changed_publish(id: &str, from: &str, topic: &str) -> BusMessage {
        let mut inner = BusMessage::new();
        inner.set("command", "props.changed");
        inner.body =
            r#"{"path":"notifications.n1.state","old":"queued","new":"shown"}"#.to_string();
        let mut outer = BusMessage::new()
            .with_header("command", "topic.publish")
            .with_header("from", from)
            .with_header("to", "noded")
            .with_header("type", "request")
            .with_header("id", id)
            .with_header("name", topic)
            .with_header("retain", "false");
        outer.body = inner.to_wire();
        outer
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn props_changed_publish_requires_registered_topic_owner() {
        let Some(url) = spawn_broker().await else {
            return;
        };
        let (mut owner_sink, mut owner_stream) = raw_connect(&url).await;
        raw_register(&mut owner_sink, &mut owner_stream, "interact").await;
        let (mut foreign_sink, mut foreign_stream) = raw_connect(&url).await;
        raw_register(&mut foreign_sink, &mut foreign_stream, "musicd").await;

        let denied = raw_call(
            &mut foreign_sink,
            &mut foreign_stream,
            props_changed_publish("foreign-publish", "interact", "interact.props.changed"),
            "foreign-publish",
        )
        .await;
        assert_eq!(denied.get("rc"), Some("10"));
        assert!(denied.get("error").unwrap_or("").contains("topic_reserved"));

        let accepted = raw_call(
            &mut owner_sink,
            &mut owner_stream,
            props_changed_publish("owner-publish", "musicd", "interact.props.changed"),
            "owner-publish",
        )
        .await;
        assert_eq!(accepted.get("rc"), Some("0"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subscribe_grant_rejects_non_owner() {
        let Some(url) = spawn_broker().await else {
            return;
        };
        let (mut s_webd, mut r_webd) = raw_connect(&url).await;
        raw_register(&mut s_webd, &mut r_webd, "webd").await;
        // Pre-register target so the registry-lookup gate isn't what
        // rejects us — we want the auth gate to fire.
        let (mut s_tgt, mut r_tgt) = raw_connect(&url).await;
        raw_register(&mut s_tgt, &mut r_tgt, "client-x").await;

        let id = "g-1";
        let req = grant_msg(
            id,
            "webd",
            "maild.props.records.changed",
            Some("client-x"),
            Some("accounts"),
        );
        let resp = raw_call(&mut s_webd, &mut r_webd, req, id).await;
        assert_eq!(resp.get("rc"), Some("10"));
        let err = resp.get("error").unwrap_or("");
        assert!(
            err.contains("only the topic's owning service"),
            "wrong error for non-owner: {err}",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subscribe_grant_rejects_unknown_target() {
        let Some(url) = spawn_broker().await else {
            return;
        };
        let (mut s, mut r) = raw_connect(&url).await;
        raw_register(&mut s, &mut r, "maild").await;

        let id = "g-2";
        let req = grant_msg(
            id,
            "maild",
            "maild.props.records.changed",
            Some("ghost-peer"),
            Some("accounts"),
        );
        let resp = raw_call(&mut s, &mut r, req, id).await;
        assert_eq!(resp.get("rc"), Some("10"));
        assert_eq!(resp.get("error"), Some("target_peer_not_connected"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subscribe_grant_requires_namespace() {
        let Some(url) = spawn_broker().await else {
            return;
        };
        let (mut s_maild, mut r_maild) = raw_connect(&url).await;
        raw_register(&mut s_maild, &mut r_maild, "maild").await;
        let (mut s_tgt, mut r_tgt) = raw_connect(&url).await;
        raw_register(&mut s_tgt, &mut r_tgt, "client-y").await;

        let id = "g-3";
        let req = grant_msg(
            id,
            "maild",
            "maild.props.records.changed",
            Some("client-y"),
            None,
        );
        let resp = raw_call(&mut s_maild, &mut r_maild, req, id).await;
        assert_eq!(resp.get("rc"), Some("10"));
        let err = resp.get("error").unwrap_or("");
        assert!(err.contains("namespace"), "wrong error: {err}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subscribe_grant_rejects_non_reserved_topic() {
        let Some(url) = spawn_broker().await else {
            return;
        };
        let (mut s, mut r) = raw_connect(&url).await;
        raw_register(&mut s, &mut r, "maild").await;
        let (mut s_tgt, mut r_tgt) = raw_connect(&url).await;
        raw_register(&mut s_tgt, &mut r_tgt, "client-z").await;

        let id = "g-4";
        let req = grant_msg(
            id,
            "maild",
            // Plain topic — not a reserved <svc>.props.{audit,records.changed}.
            "maild.heartbeat",
            Some("client-z"),
            Some("anything"),
        );
        let resp = raw_call(&mut s, &mut r, req, id).await;
        assert_eq!(resp.get("rc"), Some("10"));
        let err = resp.get("error").unwrap_or("");
        assert!(err.contains("reserved"), "wrong error: {err}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subscribe_grant_happy_path_delivers_filtered_events() {
        let Some(url) = spawn_broker().await else {
            return;
        };
        // Owner connection — both grants AND publishes.
        let (mut s_owner, mut r_owner) = raw_connect(&url).await;
        raw_register(&mut s_owner, &mut r_owner, "maild").await;
        // Target connection — receives fan-out frames.
        let (mut s_tgt, mut r_tgt) = raw_connect(&url).await;
        raw_register(&mut s_tgt, &mut r_tgt, "client-w").await;

        // Grant.
        let id = "g-5";
        let req = grant_msg(
            id,
            "maild",
            "maild.props.records.changed",
            Some("client-w"),
            Some("accounts"),
        );
        let resp = raw_call(&mut s_owner, &mut r_owner, req, id).await;
        assert_eq!(
            resp.get("rc"),
            Some("0"),
            "grant failed: {:?}",
            resp.get("error")
        );
        let body: serde_json::Value = serde_json::from_str(&resp.body).expect("json body");
        assert!(body.get("subscription_id").is_some());
        assert_eq!(
            body.get("namespace").and_then(|v| v.as_str()),
            Some("accounts")
        );

        // Publish — matching namespace. Broker expects the publish
        // payload to be a wire-encoded inner BusMessage (matching
        // C9 `NodedPropsPublisher` shape), not raw JSON. Inner MUST
        // carry at least one header (here `command`) — `to_wire` of a
        // header-less message produces `---\n---\n<body>\n` which
        // `bus::parse` cannot split (no `\n---\n` separator), so the
        // body would be silently dropped and the body-namespace
        // filter would never match.
        let pub_id = "p-1";
        let mut inner = BusMessage::new();
        inner.set("command", "maild.props.records.changed");
        inner.body = r#"{"namespace":"accounts","records":[{"key":"alice","nseq":1}]}"#.to_string();
        let mut pub_req = BusMessage::new()
            .with_header("command", "topic.publish")
            .with_header("from", "maild")
            .with_header("to", "noded")
            .with_header("type", "request")
            .with_header("id", pub_id)
            .with_header("name", "maild.props.records.changed")
            .with_header("retain", "false");
        pub_req.body = inner.to_wire();
        let pub_resp = raw_call(&mut s_owner, &mut r_owner, pub_req, pub_id).await;
        assert_eq!(pub_resp.get("rc"), Some("0"));

        // Target should receive a non-response frame carrying the
        // published body. Drain frames briefly until we see one.
        let mut got_event = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let frame = match tokio::time::timeout(remaining, r_tgt.next()).await {
                Ok(Some(Ok(WsMessage::Text(t)))) => t,
                _ => break,
            };
            let parsed = match bus_mod::parse(&frame) {
                Ok(m) => m,
                Err(_) => continue,
            };
            // Fan-out delivers the publish BusMessage body verbatim;
            // the request type stays "request", not "response".
            if parsed.body.contains(r#""namespace":"accounts""#) {
                got_event = true;
                break;
            }
        }
        assert!(
            got_event,
            "target_peer did not receive matching-namespace fan-out",
        );

        // Publish — non-matching namespace; target must NOT receive.
        let pub2_id = "p-2";
        let mut inner2 = BusMessage::new();
        inner2.set("command", "maild.props.records.changed");
        inner2.body = r#"{"namespace":"themes","records":[{"key":"dark","nseq":2}]}"#.to_string();
        let mut pub2_req = BusMessage::new()
            .with_header("command", "topic.publish")
            .with_header("from", "maild")
            .with_header("to", "noded")
            .with_header("type", "request")
            .with_header("id", pub2_id)
            .with_header("name", "maild.props.records.changed")
            .with_header("retain", "false");
        pub2_req.body = inner2.to_wire();
        let pub2_resp = raw_call(&mut s_owner, &mut r_owner, pub2_req, pub2_id).await;
        assert_eq!(pub2_resp.get("rc"), Some("0"));

        // Brief wait — if the broker were going to leak the cross-
        // namespace event, it would arrive within the fan-out tick.
        let leak_window = tokio::time::sleep(Duration::from_millis(150));
        tokio::pin!(leak_window);
        loop {
            tokio::select! {
                _ = &mut leak_window => break,
                frame = r_tgt.next() => {
                    let frame = match frame {
                        Some(Ok(WsMessage::Text(t))) => t,
                        _ => break,
                    };
                    let parsed = match bus_mod::parse(&frame) {
                        Ok(m) => m,
                        Err(_) => continue,
                    };
                    assert!(
                        !parsed.body.contains(r#""namespace":"themes""#),
                        "cross-namespace leak: target received themes event",
                    );
                }
            }
        }
    }

    // ── SPEC 12 §15.5 / C10c-pre wire-trust gate ──────────────────────────
    // The router rewrites every routed message's `from` to the
    // broker-authenticated connection identity (or strips it for
    // anonymous connections). The unit tests below cover the pure
    // canonicaliser; the wire-level regression below proves the
    // canonicaliser is on the routing hot path before any service
    // sees the message.

    #[test]
    fn canonicalize_from_registered_overwrites_spoofed_header() {
        // Connection registered as "alice" tries to send `from: webd`.
        // The canonicaliser must overwrite to the authenticated identity.
        let mut msg = BusMessage::new();
        msg.set("from", "webd");
        msg.set("to", "maild");
        msg.set("command", "maild.props.watch");
        let original = msg.to_wire();

        let canonical = super::canonicalize_routed_from(&mut msg, Some("alice"), &original);
        assert_eq!(msg.get("from"), Some("alice"));

        let reparsed = bus_mod::parse(&canonical).expect("canonical reparses");
        assert_eq!(reparsed.get("from"), Some("alice"));
        assert_eq!(reparsed.get("to"), Some("maild"));
        assert_eq!(reparsed.get("command"), Some("maild.props.watch"));
    }

    #[test]
    fn broker_origin_stamp_overwrites_every_client_spelling() {
        let mut msg = BusMessage::new();
        msg.set("broker_origin", "local");
        msg.set("Broker_Origin", "local");
        super::stamp_broker_origin(&mut msg, super::BrokerOrigin::Mesh);
        assert_eq!(msg.get("broker_origin"), Some("mesh"));
        assert_eq!(
            msg.headers
                .keys()
                .filter(|name| name.eq_ignore_ascii_case("broker_origin"))
                .count(),
            1
        );
    }

    #[test]
    fn broker_origin_comes_from_socket_location_not_registered_name_shape() {
        let bind = "192.0.2.5:4200";
        assert_eq!(
            super::broker_origin_for_delivery("127.0.0.1".parse().unwrap(), bind),
            super::BrokerOrigin::Local
        );
        assert_eq!(
            super::broker_origin_for_delivery("192.0.2.99".parse().unwrap(), bind),
            super::BrokerOrigin::Mesh
        );
    }

    #[test]
    fn correlated_response_overwrites_hostile_origin_from_responder_socket() {
        let mut response = BusMessage::new()
            .with_header("type", "response")
            .with_header("id", "noded-7")
            .with_header("broker_origin", "local")
            .with_header("Broker_Origin", "local");
        let wire = super::canonicalize_correlated_response(
            &mut response,
            "caller-9",
            Some("responder"),
            Some("caller"),
            super::broker_origin_for_delivery("192.0.2.99".parse().unwrap(), "192.0.2.5:4200"),
        );
        let delivered = bus_mod::parse(&wire).unwrap();
        assert_eq!(delivered.get("id"), Some("caller-9"));
        assert_eq!(delivered.get("from"), Some("responder"));
        assert_eq!(delivered.get("to"), Some("caller"));
        assert_eq!(delivered.get("broker_origin"), Some("mesh"));
        assert_eq!(
            delivered
                .headers
                .keys()
                .filter(|name| name.eq_ignore_ascii_case("broker_origin"))
                .count(),
            1
        );
    }

    #[test]
    fn canonicalize_from_registered_same_name_is_noop_shape() {
        // Common case: registered service "webd" sending `from: webd`.
        // The header value doesn't change; the to_wire round-trip still
        // produces an equivalent message (header order is BTreeMap-stable).
        let mut msg = BusMessage::new();
        msg.set("from", "webd");
        msg.set("to", "maild");
        msg.set("command", "maild.props.set");
        let original = msg.to_wire();
        let canonical = super::canonicalize_routed_from(&mut msg, Some("webd"), &original);
        assert_eq!(msg.get("from"), Some("webd"));
        let reparsed = bus_mod::parse(&canonical).expect("reparse");
        assert_eq!(reparsed.get("from"), Some("webd"));
    }

    #[test]
    fn canonicalize_from_anonymous_strips_spoofed_header() {
        // Unregistered (anonymous) connection sends `from: webd`. The
        // canonicaliser MUST remove the header entirely — leaving an
        // anon-* placeholder there would look like a registered service
        // to downstream auth-policy resolvers keyed on service_name.
        let mut msg = BusMessage::new();
        msg.set("from", "webd");
        msg.set("to", "maild");
        msg.set("command", "maild.props.watch");
        let original = msg.to_wire();
        let canonical = super::canonicalize_routed_from(&mut msg, None, &original);
        assert_eq!(msg.get("from"), None, "from header must be removed");
        let reparsed = bus_mod::parse(&canonical).expect("reparse");
        assert_eq!(reparsed.get("from"), None);
        assert_eq!(reparsed.get("to"), Some("maild"));
    }

    #[test]
    fn canonicalize_from_anonymous_without_header_short_circuits() {
        // Well-formed anonymous client that didn't set `from` at all —
        // the canonicaliser short-circuits to the original text rather
        // than paying for a redundant to_wire round-trip.
        let mut msg = BusMessage::new();
        msg.set("to", "maild");
        msg.set("command", "maild.props.list");
        let original = msg.to_wire();
        let canonical = super::canonicalize_routed_from(&mut msg, None, &original);
        assert_eq!(canonical, original, "short-circuit returns original text");
        assert_eq!(msg.get("from"), None);
    }

    async fn mesh_delivery_test_state() -> AppState {
        use base64::Engine as _;
        let mut posture = reload_posture(1, vec![]);
        let crate::authority::Posture::Verified(accepted) = &mut posture else {
            unreachable!()
        };
        accepted.members_full.insert("beta".into(), serde_json::json!({
            "name":"beta", "status":"active", "bus":true,
            "credentials":[{"kind":"d2", "pubkey":base64::engine::general_purpose::STANDARD.encode([3u8;32]),
                "from_epoch":1, "until_epoch":null}]
        }));
        let mut state = reload_test_state(posture, vec![]).await;
        state.admission_mode = AdmissionMode::Enforce;
        state
    }

    #[tokio::test]
    async fn mesh_delivery_identity_requires_proof_owner_and_direct_source() {
        for case in [
            "valid",
            "off",
            "observe",
            "local",
            "no-proof",
            "wrong-proof",
            "wrong-owner",
            "relay",
            "bad-source",
            "non-wg",
            "revoked",
        ] {
            let mut state = mesh_delivery_test_state().await;
            let (caller, _caller_rx) = mpsc::channel(4);
            let (other, _other_rx) = mpsc::channel(4);
            let (target, mut target_rx) = mpsc::channel(4);
            let mut admission = super::SessionAdmission {
                admitted_node: Some("beta".into()),
                response_seen: true,
                last_detail: None,
            };
            match case {
                "off" => state.admission_mode = AdmissionMode::Off,
                "observe" => state.admission_mode = AdmissionMode::Observe,
                "no-proof" => admission.admitted_node = None,
                "wrong-proof" => admission.admitted_node = Some("gamma".into()),
                "non-wg" => state.wg_bound = false,
                "revoked" => apply_inventory_reload(&state, reload_posture(2, vec![])).await,
                _ => {}
            }
            {
                let mut registry = state.registry.write().await;
                registry.insert(
                    "bridge-beta".into(),
                    super::ServiceEntry {
                        tx: if case == "wrong-owner" {
                            other
                        } else {
                            caller.clone()
                        },
                        info: Default::default(),
                    },
                );
                registry.insert(
                    "desktop".into(),
                    super::ServiceEntry {
                        tx: target,
                        info: Default::default(),
                    },
                );
            }
            let mut request = BusMessage::new()
                .with_header("id", "original")
                .with_header("from", "bridge-beta")
                .with_header("broker_origin", "mesh");
            let source = if case == "local" {
                "127.0.0.1"
            } else {
                "192.0.2.2"
            }
            .parse()
            .unwrap();
            let origin_service = match case {
                "relay" => None,
                "bad-source" => Some("nested.beta.bus"),
                _ => Some("desktopctl"),
            };
            super::route_local(
                &state,
                "desktop",
                &mut request,
                &caller,
                Some("bridge-beta"),
                source,
                &admission,
                origin_service,
            )
            .await;
            let delivered = bus_mod::parse(&target_rx.recv().await.unwrap()).unwrap();
            assert_eq!(
                delivered.get("broker_peer"),
                if case == "valid" { Some("beta") } else { None },
                "{case}"
            );
            assert_eq!(
                delivered.get("broker_service"),
                if case == "valid" {
                    Some("desktopctl")
                } else {
                    None
                },
                "{case}"
            );
        }
    }

    #[tokio::test]
    async fn reload_while_delivery_waits_cannot_stamp_old_authority() {
        let state = mesh_delivery_test_state().await;
        let (caller, _caller_rx) = mpsc::channel(4);
        let (target, mut target_rx) = mpsc::channel(4);
        {
            let mut registry = state.registry.write().await;
            registry.insert(
                "bridge-beta".into(),
                super::ServiceEntry {
                    tx: caller.clone(),
                    info: Default::default(),
                },
            );
            registry.insert(
                "desktop".into(),
                super::ServiceEntry {
                    tx: target,
                    info: Default::default(),
                },
            );
        }
        let pending_guard = state.pending_responses.map.write().await;
        let task_state = state.clone();
        let task = tokio::spawn(async move {
            let mut request = BusMessage::new()
                .with_header("id", "original")
                .with_header("broker_origin", "mesh");
            let admission = super::SessionAdmission {
                admitted_node: Some("beta".into()),
                response_seen: true,
                last_detail: None,
            };
            super::route_local(
                &task_state,
                "desktop",
                &mut request,
                &caller,
                Some("bridge-beta"),
                "192.0.2.2".parse().unwrap(),
                &admission,
                Some("desktopctl"),
            )
            .await;
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            while state.registry.try_write().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        apply_inventory_reload(&state, reload_posture(2, vec![])).await;
        drop(pending_guard);
        task.await.unwrap();
        let delivered = bus_mod::parse(&target_rx.recv().await.unwrap()).unwrap();
        assert_eq!(delivered.get("broker_peer"), None);
        assert_eq!(delivered.get("broker_service"), None);
    }

    #[test]
    fn reserved_mesh_identity_is_stripped_case_insensitively() {
        let mut msg = BusMessage::new();
        for name in [
            "broker_peer",
            "BROKER_PEER",
            "broker_service",
            "Broker_Service",
            "mesh_from",
            "Mesh_From",
        ] {
            msg.headers.insert(name.into(), "forged".into());
        }
        super::stamp_broker_origin(&mut msg, super::BrokerOrigin::Local);
        assert_eq!(msg.headers.len(), 1);
        assert_eq!(msg.get("broker_origin"), Some("local"));
    }

    #[tokio::test]
    async fn pending_reply_requires_exact_recipient_connection() {
        let table = PendingResponseTable::new();
        let (caller, _caller_rx) = mpsc::channel(4);
        let (recipient, _recipient_rx) = mpsc::channel(4);
        let (replacement, _replacement_rx) = mpsc::channel(4);
        let mut message = BusMessage::new().with_header("id", "original");
        let id = table
            .register(&mut message, &caller, Some("caller"), &recipient)
            .await
            .unwrap();
        assert!(table.take_response(&id, &caller).await.is_none());
        assert!(table.take_response(&id, &replacement).await.is_none());
        let accepted = table.take_response(&id, &recipient.clone()).await.unwrap();
        assert_eq!(accepted.caller_id, "original");
        assert!(table.take_response(&id, &recipient).await.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forged_wire_response_cannot_consume_another_services_request() {
        let url = spawn_broker().await.expect("live broker required");
        let (mut caller, mut caller_rx) = raw_connect(&url).await;
        let (mut owner, mut owner_rx) = raw_connect(&url).await;
        let (mut intruder, mut intruder_rx) = raw_connect(&url).await;
        raw_register(&mut owner, &mut owner_rx, "clipboard").await;
        raw_register(&mut intruder, &mut intruder_rx, "intruder").await;
        let request = BusMessage::new()
            .with_header("to", "clipboard")
            .with_header("command", "desktop.capabilities")
            .with_header("id", "caller-id");
        caller
            .send(WsMessage::Text(request.to_wire().into()))
            .await
            .unwrap();
        let frame = tokio::time::timeout(Duration::from_secs(3), owner_rx.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let WsMessage::Text(text) = frame else {
            panic!("expected routed request")
        };
        let routed = bus_mod::parse(&text).unwrap();
        let response = BusMessage::new()
            .with_header("type", "response")
            .with_header("id", routed.get("id").unwrap())
            .with_header("rc", "0");
        intruder
            .send(WsMessage::Text(response.to_wire().into()))
            .await
            .unwrap();
        // An ordered ping proves the forged response was processed first.
        let barrier = raw_call(
            &mut intruder,
            &mut intruder_rx,
            BusMessage::new()
                .with_header("to", "noded")
                .with_header("command", "noded.ping")
                .with_header("id", "barrier"),
            "barrier",
        )
        .await;
        assert_eq!(barrier.get("rc"), Some("0"));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), caller_rx.next())
                .await
                .is_err()
        );
        owner
            .send(WsMessage::Text(response.to_wire().into()))
            .await
            .unwrap();
        let frame = tokio::time::timeout(Duration::from_secs(3), caller_rx.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let WsMessage::Text(text) = frame else {
            panic!("expected legitimate reply")
        };
        let reply = bus_mod::parse(&text).unwrap();
        assert_eq!(reply.get("id"), Some("caller-id"));
        assert_eq!(reply.get("from"), Some("clipboard"));
    }

    /// SPEC 12 §15.5 wire-trust regression — a peer "alice" sending
    /// `from: webd` to maild's reserved address must NOT reach maild
    /// with a spoofed identity. The receiving connection (acting as
    /// maild) checks the `from` header to assert it matches the
    /// authenticated origin ("alice"), not the spoofed value ("webd").
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn routed_from_is_canonicalised_to_authenticated_peer_id() {
        let Some(url) = spawn_broker().await else {
            return;
        };

        // Peer A: registers as "alice".
        let (mut s_alice, mut r_alice) = raw_connect(&url).await;
        raw_register(&mut s_alice, &mut r_alice, "alice").await;

        // Peer B: registers as "maild" so it is reachable by name and
        // can observe incoming routed messages.
        let (mut s_maild, mut r_maild) = raw_connect(&url).await;
        raw_register(&mut s_maild, &mut r_maild, "maild").await;

        // Alice tries to impersonate webd to maild.
        let mut spoof = BusMessage::new();
        spoof.set("from", "webd");
        spoof.set("to", "maild");
        spoof.set("command", "maild.props.watch");
        spoof.set("id", "spoof-1");
        spoof.set("type", "request");
        spoof.set("namespace", "accounts");

        s_alice
            .send(WsMessage::Text(spoof.to_wire().into()))
            .await
            .expect("alice send");

        // maild observes the routed inbound. Expect `from: alice`,
        // never `from: webd`. Match the inbound by `command` +
        // `namespace` rather than the caller's original `id`: the
        // broker rewrites `id` to `noded-<n>` on the forward leg (and
        // restores the caller's id on the response leg) to keep
        // multiple anonymous callers' requests from colliding on the
        // pending-response table — see `PendingResponseTable`.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let frame = tokio::time::timeout(remaining, r_maild.next())
                .await
                .expect("inbound timeout at maild");
            let text = match frame {
                Some(Ok(WsMessage::Text(t))) => t.to_string(),
                Some(Ok(_)) => continue,
                Some(Err(e)) => panic!("ws error: {e}"),
                None => panic!("stream closed before inbound"),
            };
            let parsed = match bus_mod::parse(&text) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if parsed.get("command") != Some("maild.props.watch")
                || parsed.get("namespace") != Some("accounts")
            {
                continue;
            }
            assert_eq!(
                parsed.get("from"),
                Some("alice"),
                "spoofed from=webd must be rewritten to authenticated identity",
            );
            assert_eq!(
                parsed.get("broker_origin"),
                Some("local"),
                "loopback-routed delivery must carry the broker-owned local marker",
            );
            // Belt-and-braces: pin the broker-rewrite format directly.
            // The wire id on the forward leg must be `noded-<n>`; the
            // caller's original id (`spoof-1`) is restored only on the
            // response leg. Checking `starts_with("noded-")` is a
            // stronger assertion than `assert_ne!(.., "spoof-1")` —
            // the latter would silently pass if the rewrite produced
            // any other distinct id (e.g. accidentally dropped, set
            // to empty, etc).
            let observed_id = parsed.get("id").expect("forwarded id");
            assert!(
                observed_id.starts_with("noded-"),
                "forwarded id must be broker-rewritten as `noded-<n>`, got {observed_id:?}",
            );
            return;
        }
    }

    /// C10c-pre rev-2 MAJOR — the Bus-address spelling of `to: noded`
    /// (`to: noded.<node>.bus`) is a valid alias for the plain
    /// `to: noded` and MUST also bypass `from`-canonicalisation so
    /// `noded.register` can read the requested name from the wire
    /// header. Without the fix, anonymous registration via this
    /// spelling lost the requested name and registered as "" or
    /// failed; registered connections re-registering under a new
    /// alias had their wire `from` overwritten with the current
    /// registration. This test pins both spellings.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn noded_register_works_via_bus_address_spelling() {
        let Some(url) = spawn_broker().await else {
            return;
        };

        // Anonymous connection issues noded.register via the
        // Bus-address spelling rather than the plain `to: noded`.
        let (mut sink, mut stream) = raw_connect(&url).await;
        let id = "reg-bus-1";
        let req = BusMessage::new()
            .with_header("command", "noded.register")
            .with_header("from", "newcomer")
            .with_header("to", "noded.test-node.bus")
            .with_header("type", "request")
            .with_header("id", id);
        let resp = raw_call(&mut sink, &mut stream, req, id).await;
        assert_eq!(
            resp.get("rc"),
            Some("0"),
            "Bus-address noded.register failed: {:?}",
            resp.get("error"),
        );
        assert!(
            resp.body.contains(r#""registered": "newcomer""#),
            "registration did not echo the requested name: body={}",
            resp.body,
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn noded_register_rejects_names_outside_spec_10_grammar() {
        let Some(url) = spawn_broker().await else {
            return;
        };
        let (mut sink, mut stream) = raw_connect(&url).await;
        let id = "invalid-reg-1";
        let request = BusMessage::new()
            .with_header("command", "noded.register")
            .with_header("from", "Invalid_Name")
            .with_header("to", "noded")
            .with_header("type", "request")
            .with_header("id", id);
        let response = raw_call(&mut sink, &mut stream, request, id).await;
        assert_eq!(response.get("rc"), Some("10"));
        assert_eq!(
            response.get("error"),
            Some("noded.register 'from' must match ^[a-z][a-z0-9-]{1,30}$")
        );

        raw_register(&mut sink, &mut stream, "valid-after-refusal").await;
    }

    /// Anonymous connections must NOT be able to set ANY `from`
    /// header on routed traffic — the canonicaliser strips it
    /// entirely so the receiving service sees `from` absent and
    /// resolves `PeerIdentity::service_name` to `None`. This is the
    /// guard that lets C10c's `<svc>.props.watch` reject anonymous
    /// callers before reaching the granter.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn routed_from_is_stripped_for_anonymous_peer() {
        let Some(url) = spawn_broker().await else {
            return;
        };

        // Peer A: NEVER calls noded.register.
        let (mut s_anon, _r_anon) = raw_connect(&url).await;

        // Peer B: registers as "maild".
        let (mut s_maild, mut r_maild) = raw_connect(&url).await;
        raw_register(&mut s_maild, &mut r_maild, "maild").await;

        let mut spoof = BusMessage::new();
        spoof.set("from", "webd");
        spoof.set("to", "maild");
        spoof.set("command", "maild.props.watch");
        spoof.set("id", "anon-spoof-1");
        spoof.set("type", "request");
        spoof.set("namespace", "accounts");

        s_anon
            .send(WsMessage::Text(spoof.to_wire().into()))
            .await
            .expect("anon send");

        // Match by command + namespace, not the caller's `id`: the
        // broker rewrites `id` on the forward leg to avoid the
        // pending-response collision across multiple anonymous callers
        // (see `PendingResponseTable`).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let frame = tokio::time::timeout(remaining, r_maild.next())
                .await
                .expect("inbound timeout at maild");
            let text = match frame {
                Some(Ok(WsMessage::Text(t))) => t.to_string(),
                Some(Ok(_)) => continue,
                Some(Err(e)) => panic!("ws error: {e}"),
                None => panic!("stream closed before inbound"),
            };
            let parsed = match bus_mod::parse(&text) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if parsed.get("command") != Some("maild.props.watch")
                || parsed.get("namespace") != Some("accounts")
            {
                continue;
            }
            assert_eq!(
                parsed.get("from"),
                None,
                "anonymous peer's spoofed from must be stripped entirely",
            );
            // Pin the broker-rewrite format directly (see the
            // `routed_from_is_canonicalised_to_authenticated_peer_id`
            // test for the rationale — `assert_ne!` would silently
            // pass on a dropped or empty id).
            let observed_id = parsed.get("id").expect("forwarded id");
            assert!(
                observed_id.starts_with("noded-"),
                "forwarded id must be broker-rewritten as `noded-<n>`, got {observed_id:?}",
            );
            return;
        }
    }
}
