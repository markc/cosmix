//! Supervised reconnecting Bus client (SPEC 18 §3.3).
//!
//! [`NodedClient`] is a one-shot dial: on transport loss its reader task
//! exits, the incoming receiver yields `None`, and any consumer (notably
//! the Mix event pump) sees a *terminal* close. That is correct for a
//! short-lived tool but fatal for a resident citizen — a single
//! `cosmix-noded` bounce cleanly kills every Mix daemon. SPEC 18 §3.3
//! names this the one gating defect for "long-running".
//!
//! [`SupervisedClient`] wraps a [`NodedClient`] and owns the
//! `connect → register → (replay subs) → pump` loop:
//!
//! * **Initial connect** uses a *bounded* budget
//!   ([`MAX_INITIAL_ATTEMPTS`]) so a misconfigured citizen fails fast
//!   (SPEC 18 §3.1 — serve mode exits non-zero on a typed fatal).
//! * **Reconnect** is *unbounded* with exponential backoff + full
//!   jitter (base 250 ms, ×2, cap 30 s — DECIDED §7-Q1) so a resident
//!   citizen waits out a long broker outage without systemd flapping.
//! * On every reconnect the [`SubscriptionRegistry`] is replayed **in
//!   recorded order** (§3.3 — re-subscribe, not just re-register, is
//!   the real gate).
//! * The wrapper owns a **replaceable** incoming stream
//!   (consult BLOCKER 2): the outward [`mpsc::UnboundedReceiver`] handed
//!   to the Mix pump (WS3) survives reconnects; only a *fatal* shutdown
//!   closes it. A transient drop is never observed as the sticky
//!   terminal `"transport_closed"`.
//! * While disconnected, every outbound call **fails fast with a typed
//!   error** ([`SupervisedError::Disconnected`]) — there is **no
//!   outbound queue** (the forbidden partial-truth buffer).
//!
//! The [`SubscriptionRegistry`] is *mutated* by WS2's
//! `subscribe`/`unsubscribe` primitive (transactionally, only after an
//! RC-0 broker subscribe). WS1 only defines the structure and replays
//! it; until WS2 lands the registry stays empty and replay is a no-op,
//! but the machinery is exercised by the reconnect test.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rand::Rng;
use tokio::sync::{Mutex as TokioMutex, RwLock, mpsc, watch};

use crate::bounded::{
    BoundedIncomingEvent, BoundedIncomingReceiver, BoundedIncomingSender, bounded_incoming_channel,
};
use crate::native::{NativeIncomingReceiver, NodedClient, RegistrationRejected};
use crate::types::IncomingCommand;

/// Bounded attempt budget for the *initial* connect+register. Exhausting
/// it is a typed fatal (SPEC 18 §3.1): a misconfigured citizen must fail
/// fast rather than spin forever against a broker that will never answer.
pub const MAX_INITIAL_ATTEMPTS: u32 = 5;

/// Backoff base (DECIDED §7-Q1).
const BACKOFF_BASE_MS: u64 = 250;
/// Backoff ceiling (DECIDED §7-Q1).
const BACKOFF_CAP_MS: u64 = 30_000;

/// Deterministic backoff *ceiling* for `attempt` (0-based): the upper
/// bound of the full-jitter window, `min(base · 2^attempt, cap)`.
/// Monotonic non-decreasing, saturating at [`BACKOFF_CAP_MS`].
///
/// Split out from [`backoff_delay`] so the (deterministic) cap schedule
/// is unit-testable independently of the (random) jitter.
fn backoff_ceiling_ms(attempt: u32) -> u64 {
    let factor = 2u64.checked_pow(attempt).unwrap_or(u64::MAX);
    let exp = BACKOFF_BASE_MS.saturating_mul(factor);
    exp.min(BACKOFF_CAP_MS)
}

/// Full-jitter backoff (AWS "full jitter"): a uniform draw from
/// `[0, backoff_ceiling_ms(attempt)]`. Full jitter — not equal/decorrelated
/// — is the DECIDED §7-Q1 choice: it decorrelates a fleet of citizens
/// reconnecting after a shared broker bounce.
fn backoff_delay(attempt: u32) -> Duration {
    let ceiling = backoff_ceiling_ms(attempt);
    // `gen_range(0..=ceiling)` is inclusive; ceiling is always > 0
    // (BACKOFF_BASE_MS > 0) so the range is non-empty.
    let ms = rand::thread_rng().gen_range(0..=ceiling);
    Duration::from_millis(ms)
}

/// Connection lifecycle state. The watch channel is also the sampled state
/// authority, so edge-triggered and sampled consumers cannot diverge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    /// Initial connect in progress (pre-`Connected`).
    Connecting = 0,
    /// Live broker connection; outbound calls permitted.
    Connected = 1,
    /// Transport lost; supervisor is backing off / reconnecting.
    /// Outbound calls fail fast with [`SupervisedError::Disconnected`].
    Disconnected = 2,
    /// Graceful shutdown requested; supervisor will not reconnect.
    ShuttingDown = 3,
    /// Initial connect budget exhausted — terminal.
    Fatal = 4,
}

/// Typed error surface for the supervised client.
///
/// SPEC 18 §3.3 requires that outbound calls while disconnected return a
/// *typed* error (so the caller can distinguish "broker is down right
/// now, do not retry into a queue" from a genuine RPC failure) and that
/// initial-connect exhaustion is a *typed fatal* (so serve mode can exit
/// non-zero per §3.1). Underlying transport/RPC failures wrap into
/// [`SupervisedError::Transport`].
#[derive(Debug)]
pub enum SupervisedError {
    /// An outbound call was attempted while the broker connection was
    /// down. There is deliberately **no** outbound queue — the caller
    /// must decide what to do, never silently buffer (SPEC 18 §3.3,
    /// `feedback_refactor_silent_noop_audit`).
    Disconnected,
    /// The supervised client is shutting down (deregister/QUIT path);
    /// new outbound work is refused.
    ShuttingDown,
    /// The *initial* connect+register budget ([`MAX_INITIAL_ATTEMPTS`])
    /// was exhausted. Terminal — serve mode must exit non-zero
    /// (SPEC 18 §3.1).
    InitialConnectFailed {
        attempts: u32,
        source: anyhow::Error,
    },
    /// An underlying [`NodedClient`] transport/RPC error on a call that
    /// *did* reach a live connection.
    Transport(anyhow::Error),
}

impl std::fmt::Display for SupervisedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SupervisedError::Disconnected => {
                write!(
                    f,
                    "broker disconnected (no outbound queue; caller must retry)"
                )
            }
            SupervisedError::ShuttingDown => write!(f, "supervised client is shutting down"),
            SupervisedError::InitialConnectFailed { attempts, source } => write!(
                f,
                "initial broker connect failed after {attempts} attempt(s): {source}"
            ),
            SupervisedError::Transport(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SupervisedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SupervisedError::InitialConnectFailed { source, .. } => Some(source.as_ref()),
            SupervisedError::Transport(e) => Some(e.as_ref()),
            _ => None,
        }
    }
}

impl SupervisedError {
    /// Return the broker's structured registration refusal when this error
    /// came from the initial connect/register path.
    pub fn registration_rejection(&self) -> Option<(u8, &str)> {
        let Self::InitialConnectFailed { source, .. } = self else {
            return None;
        };
        let rejection = source.downcast_ref::<RegistrationRejected>()?;
        Some((rejection.rc, rejection.message.as_str()))
    }
}

/// Ordered, deduplicated set of subscribed topic names.
///
/// SPEC 18 §3.3 requires the supervised client to "record every
/// `subscribe()` call as it happens … and replay the full set in
/// recorded order" on reconnect. This is that record. It is mutated by
/// WS2's `subscribe`/`unsubscribe` primitive **transactionally** —
/// [`record`](Self::record) is called only *after* an RC-0 broker
/// subscribe, [`remove`](Self::remove) only after an RC-0 unsubscribe —
/// so a never-satisfiable topic is never replayed forever, and a
/// deliberately-dropped topic is not resurrected by a later bounce.
///
/// WS1 owns the structure and the replay; WS2 owns the mutation
/// chokepoint. Cloneable handle — interior `Arc<Mutex<…>>`.
#[derive(Clone, Default)]
pub struct SubscriptionRegistry {
    inner: Arc<std::sync::Mutex<Vec<String>>>,
}

impl SubscriptionRegistry {
    /// A fresh empty registry.
    pub fn new() -> SubscriptionRegistry {
        SubscriptionRegistry::default()
    }

    /// Record a topic as subscribed. First-seen insertion order is
    /// preserved; a duplicate is a no-op (returns `false`). Order
    /// matters: §3.3 replays in recorded order.
    pub fn record(&self, topic: &str) -> bool {
        let mut g = self.inner.lock().unwrap();
        if g.iter().any(|t| t == topic) {
            return false;
        }
        g.push(topic.to_string());
        true
    }

    /// Forget a topic (after an RC-0 unsubscribe). Returns whether it
    /// was present.
    pub fn remove(&self, topic: &str) -> bool {
        let mut g = self.inner.lock().unwrap();
        if let Some(pos) = g.iter().position(|t| t == topic) {
            g.remove(pos);
            true
        } else {
            false
        }
    }

    /// Snapshot of all recorded topics in replay (recorded) order.
    pub fn snapshot(&self) -> Vec<String> {
        self.inner.lock().unwrap().clone()
    }

    /// Number of recorded topics.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().is_empty()
    }
}

/// A [`NodedClient`] under a reconnect supervisor.
///
/// Construct with [`connect_supervised`](Self::connect_supervised). Hand
/// the [`incoming`](Self::incoming) receiver to the event pump (WS3);
/// make outbound calls through the state-gated proxies. The supervisor
/// task transparently reconnects, re-registers, and replays the
/// [`SubscriptionRegistry`] underneath.
pub struct SupervisedClient {
    /// Current live connection. Swapped (write-locked, briefly) by the
    /// supervisor on reconnect; read-locked + `Arc`-cloned by outbound
    /// proxies so the network await never holds the lock.
    inner: Arc<RwLock<Arc<NodedClient>>>,
    /// Publishes connection-state transitions without sampling.
    state_tx: watch::Sender<ConnState>,
    /// Serialises terminal-state checks and watch publication.
    state_publish: Arc<std::sync::Mutex<()>>,
    /// Monotonic successful-connection generation. Starts at one for the
    /// initial connection and increments after every complete reconnect and
    /// subscription replay, so consumers cannot sample away a fast bounce.
    connection_generation: Arc<AtomicU64>,
    registry: SubscriptionRegistry,
    /// Outward replaceable incoming stream — taken once by the pump.
    incoming_rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<IncomingCommand>>>,
    /// Opt-in bounded outward incoming stream, taken once by its consumer.
    bounded_incoming_rx: std::sync::Mutex<Option<BoundedIncomingReceiver>>,
    /// Signals the supervisor to stop (no reconnect). `true` = stop.
    shutdown_tx: watch::Sender<bool>,
    supervisor: TokioMutex<Option<tokio::task::JoinHandle<()>>>,
    service_name: String,
}

/// Connection options for a supervised client.
///
/// The default preserves the pre-0.4.0 lifecycle: registration rejections are
/// retried like other failed connection attempts. A citizen whose public name
/// must never wait out a rejection can opt into a terminal state with
/// [`fatal_on_registration_rejection`](Self::fatal_on_registration_rejection).
pub struct SupervisedConnectOptions {
    service_name: String,
    noded_url: String,
    provenance: Option<cosmix_bus::RegisterProvenance>,
    fatal_on_registration_rejection: bool,
    bounded_incoming_capacity: Option<usize>,
}

impl SupervisedConnectOptions {
    /// Treat any broker registration rejection (collision or admission) as
    /// terminal. Disabled by default for compatibility with existing citizens.
    pub fn fatal_on_registration_rejection(mut self, enabled: bool) -> Self {
        self.fatal_on_registration_rejection = enabled;
        self
    }

    /// Bound every incoming subscription lane for this supervised client.
    ///
    /// The socket reader and reconnect-stable outward lane both use
    /// non-blocking `try_send`. A full lane drops the new command and exposes
    /// loss through [`BoundedIncomingEvent::Overflow`] and the receiver's
    /// cumulative overflow counter. Existing users remain unbounded unless
    /// they opt in here.
    ///
    /// # Panics
    ///
    /// Panics when `capacity` is zero.
    pub fn bounded_incoming(mut self, capacity: usize) -> Self {
        assert!(capacity > 0, "bounded incoming capacity must be non-zero");
        self.bounded_incoming_capacity = Some(capacity);
        self
    }

    /// Connect using these options.
    pub async fn connect(self) -> Result<SupervisedClient, SupervisedError> {
        SupervisedClient::connect_with_options(self).await
    }
}

impl SupervisedClient {
    /// Build a supervised connection with opt-in lifecycle policy.
    pub fn connect_options(service_name: &str, noded_url: &str) -> SupervisedConnectOptions {
        SupervisedConnectOptions {
            service_name: service_name.to_string(),
            noded_url: noded_url.to_string(),
            provenance: None,
            fatal_on_registration_rejection: false,
            bounded_incoming_capacity: None,
        }
    }

    /// Connect (bounded budget), register, and start the reconnect
    /// supervisor. Returns [`SupervisedError::InitialConnectFailed`] if
    /// the initial connect+register cannot succeed within
    /// [`MAX_INITIAL_ATTEMPTS`] — a typed fatal the serve-mode
    /// entrypoint (WS3) maps to a non-zero exit (SPEC 18 §3.1).
    pub async fn connect_supervised(
        service_name: &str,
        noded_url: &str,
    ) -> Result<SupervisedClient, SupervisedError> {
        Self::connect_options(service_name, noded_url)
            .connect()
            .await
    }

    /// Like [`connect_supervised`](Self::connect_supervised) but sends
    /// `provenance` (build version / git_sha / build_time / pid / …) on
    /// every register — re-sent across reconnects (built ONCE by the
    /// citizen so `started_at` stays the true process start). A
    /// `mix --serve` citizen uses this to report the mix binary's build
    /// (version-discovery contract).
    pub async fn connect_supervised_with_provenance(
        service_name: &str,
        noded_url: &str,
        provenance: Option<cosmix_bus::RegisterProvenance>,
    ) -> Result<SupervisedClient, SupervisedError> {
        let mut options = Self::connect_options(service_name, noded_url);
        options.provenance = provenance;
        options.connect().await
    }

    async fn connect_with_options(
        options: SupervisedConnectOptions,
    ) -> Result<SupervisedClient, SupervisedError> {
        let SupervisedConnectOptions {
            service_name,
            noded_url,
            provenance,
            fatal_on_registration_rejection,
            bounded_incoming_capacity,
        } = options;
        let (state_tx, _) = watch::channel(ConnState::Connecting);
        let state_publish = Arc::new(std::sync::Mutex::new(()));

        let mut last_err: Option<anyhow::Error> = None;
        let mut client: Option<NodedClient> = None;
        for attempt in 0..MAX_INITIAL_ATTEMPTS {
            match NodedClient::connect_with_provenance_and_capacity(
                &service_name,
                &noded_url,
                provenance.clone(),
                bounded_incoming_capacity,
            )
            .await
            {
                Ok(c) => {
                    client = Some(c);
                    break;
                }
                Err(e) => {
                    tracing::debug!(
                        event = "supervised_initial_attempt_failed",
                        service = %service_name,
                        attempt,
                        error = %e,
                        "initial broker connect attempt failed"
                    );
                    if fatal_on_registration_rejection
                        && e.downcast_ref::<RegistrationRejected>().is_some()
                    {
                        publish_state(&state_tx, &state_publish, ConnState::Fatal);
                        return Err(SupervisedError::InitialConnectFailed {
                            attempts: attempt + 1,
                            source: e,
                        });
                    }
                    last_err = Some(e);
                    // No sleep after the final attempt — fail fast.
                    if attempt + 1 < MAX_INITIAL_ATTEMPTS {
                        tokio::time::sleep(backoff_delay(attempt)).await;
                    }
                }
            }
        }

        let client = match client {
            Some(c) => c,
            None => {
                publish_state(&state_tx, &state_publish, ConnState::Fatal);
                return Err(SupervisedError::InitialConnectFailed {
                    attempts: MAX_INITIAL_ATTEMPTS,
                    source: last_err
                        .unwrap_or_else(|| anyhow::anyhow!("connect failed (no error captured)")),
                });
            }
        };

        // Take the *first* connection's incoming receiver. The
        // supervisor forwards from it (and every later one) into the
        // single outward channel below.
        let first_rx = client.take_native_incoming().await.ok_or_else(|| {
            // A fresh NodedClient always has its receiver; this only
            // fires on a programming error (someone took it first).
            SupervisedError::Transport(anyhow::anyhow!(
                "fresh NodedClient had no incoming receiver"
            ))
        })?;

        let inner = Arc::new(RwLock::new(Arc::new(client)));
        publish_state(&state_tx, &state_publish, ConnState::Connected);
        let connection_generation = Arc::new(AtomicU64::new(1));

        let (out_tx, out_rx, bounded_out_rx) = match bounded_incoming_capacity {
            Some(capacity) => {
                let (sender, receiver) = bounded_incoming_channel(capacity);
                (SupervisorOutgoing::Bounded(sender), None, Some(receiver))
            }
            None => {
                let (sender, receiver) = mpsc::unbounded_channel();
                (SupervisorOutgoing::Unbounded(sender), Some(receiver), None)
            }
        };
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        // One registry, two handles over the same `Arc<Mutex<…>>`: the
        // supervisor replays it; WS2 (via `subscription_registry()`)
        // mutates it. Clones share storage — see the
        // `registry_handle_is_shared` test.
        let registry = SubscriptionRegistry::new();

        let supervisor = tokio::spawn(supervisor_loop(SupervisorCtx {
            inner: inner.clone(),
            state_tx: state_tx.clone(),
            state_publish: state_publish.clone(),
            connection_generation: connection_generation.clone(),
            registry: registry.clone(),
            out_tx,
            shutdown_rx,
            service_name: service_name.to_string(),
            noded_url: noded_url.to_string(),
            provenance,
            fatal_on_registration_rejection,
            bounded_incoming_capacity,
            first_rx,
        }));

        Ok(SupervisedClient {
            inner,
            state_tx,
            state_publish,
            connection_generation,
            registry,
            incoming_rx: std::sync::Mutex::new(out_rx),
            bounded_incoming_rx: std::sync::Mutex::new(bounded_out_rx),
            shutdown_tx,
            supervisor: TokioMutex::new(Some(supervisor)),
            service_name,
        })
    }

    /// The shared [`SubscriptionRegistry`]. WS2's
    /// `subscribe`/`unsubscribe` primitive mutates this; the supervisor
    /// replays it on reconnect.
    pub fn subscription_registry(&self) -> SubscriptionRegistry {
        self.registry.clone()
    }

    /// Take the outward incoming stream (once). This receiver **survives
    /// reconnects** — it only yields `None` on a fatal shutdown, never
    /// on a transient drop (consult BLOCKER 2). The Mix pump (WS3) drives
    /// this.
    pub fn incoming(&self) -> Option<mpsc::UnboundedReceiver<IncomingCommand>> {
        self.incoming_rx.lock().unwrap().take()
    }

    /// Take the opt-in bounded incoming stream once.
    ///
    /// Returns `None` when this client was built with the default unbounded
    /// lane, or when the bounded receiver has already been taken.
    pub fn incoming_bounded(&self) -> Option<BoundedIncomingReceiver> {
        self.bounded_incoming_rx.lock().unwrap().take()
    }

    /// Current connection state.
    pub fn state(&self) -> ConnState {
        *self.state_tx.borrow()
    }

    /// Subscribe to connection-state transitions.
    ///
    /// The receiver starts at the current state and is notified directly by
    /// the supervisor on every edge; callers do not need to poll [`state`].
    pub fn subscribe_state(&self) -> watch::Receiver<ConnState> {
        self.state_tx.subscribe()
    }

    /// Monotonic generation of fully established connections.
    ///
    /// Unlike sampling [`state`](Self::state), this cannot miss a disconnect
    /// and reconnect that both occur between two observations. A new value
    /// means registration and recorded-subscription replay have completed on a
    /// new broker socket.
    pub fn connection_generation(&self) -> u64 {
        self.connection_generation.load(Ordering::SeqCst)
    }

    /// Whether outbound calls will be accepted right now.
    pub fn is_connected(&self) -> bool {
        self.state() == ConnState::Connected
    }

    /// The registered service name.
    pub fn service_name(&self) -> &str {
        &self.service_name
    }

    /// Gate the outbound path: typed fail-fast unless `Connected`.
    /// **No queue** — a disconnected caller gets an error, never a
    /// silent buffer (SPEC 18 §3.3).
    fn gate(&self) -> Result<(), SupervisedError> {
        match self.state() {
            ConnState::Connected => Ok(()),
            ConnState::ShuttingDown => Err(SupervisedError::ShuttingDown),
            _ => Err(SupervisedError::Disconnected),
        }
    }

    /// Snapshot the live inner client without holding the lock across
    /// the subsequent network await.
    async fn client(&self) -> Arc<NodedClient> {
        self.inner.read().await.clone()
    }

    // ── State-gated outbound proxies ──

    /// See [`NodedClient::call`].
    pub async fn call(
        &self,
        to: &str,
        command: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, SupervisedError> {
        self.gate()?;
        self.client()
            .await
            .call(to, command, args)
            .await
            .map_err(SupervisedError::Transport)
    }

    /// See [`NodedClient::call_typed`]. A gate failure (ShuttingDown /
    /// Disconnected) or an inner transport error is `Err(SupervisedError)`;
    /// a peer `rc >= 10` reply is `Ok(PortReply::AppError { rc, message })`,
    /// so a caller can map transport vs application error to distinct `$rc`
    /// bands (the Mix serve-mode rc-band contract).
    pub async fn call_typed(
        &self,
        to: &str,
        command: &str,
        args: serde_json::Value,
    ) -> Result<cosmix_bus::PortReply, SupervisedError> {
        self.gate()?;
        self.client()
            .await
            .call_typed(to, command, args)
            .await
            .map_err(SupervisedError::Transport)
    }

    /// See [`NodedClient::send`].
    pub async fn send(
        &self,
        to: &str,
        command: &str,
        args: serde_json::Value,
    ) -> Result<(), SupervisedError> {
        self.gate()?;
        self.client()
            .await
            .send(to, command, args)
            .await
            .map_err(SupervisedError::Transport)
    }

    /// See [`NodedClient::send_with_headers`].
    pub async fn send_with_headers(
        &self,
        to: &str,
        command: &str,
        headers: &std::collections::BTreeMap<String, String>,
        body: &str,
    ) -> Result<(), SupervisedError> {
        self.gate()?;
        self.client()
            .await
            .send_with_headers(to, command, headers, body)
            .await
            .map_err(SupervisedError::Transport)
    }

    /// See [`NodedClient::call_with_headers`].
    pub async fn call_with_headers(
        &self,
        to: &str,
        command: &str,
        headers: &std::collections::BTreeMap<String, String>,
        body: &str,
    ) -> Result<serde_json::Value, SupervisedError> {
        self.gate()?;
        self.client()
            .await
            .call_with_headers(to, command, headers, body)
            .await
            .map_err(SupervisedError::Transport)
    }

    /// See [`NodedClient::call_with_headers_raw`].
    pub async fn call_with_headers_raw(
        &self,
        to: &str,
        command: &str,
        headers: &std::collections::BTreeMap<String, String>,
        body: &str,
    ) -> Result<(u8, String, Option<String>), SupervisedError> {
        self.gate()?;
        self.client()
            .await
            .call_with_headers_raw(to, command, headers, body)
            .await
            .map_err(SupervisedError::Transport)
    }

    /// See [`NodedClient::respond`].
    pub async fn respond(
        &self,
        cmd: &IncomingCommand,
        rc: u8,
        body: &str,
    ) -> Result<(), SupervisedError> {
        self.gate()?;
        self.client()
            .await
            .respond(cmd, rc, body)
            .await
            .map_err(SupervisedError::Transport)
    }

    /// See [`NodedClient::respond_parts`].
    ///
    /// The neutral part-wise form used by SPEC 18 WS3 serve mode: the
    /// Mix `reply()` builtin's correlation parts (captured by the
    /// evaluator off the in-flight event) flow through the serve-mode
    /// `MixBusHandler` to here, so the citizen answers a request over
    /// its *supervised* connection. Gated like every other outbound
    /// proxy — a reply attempted while disconnected fails fast with a
    /// typed error (no queue; §3.3), surfaced to the script.
    pub async fn respond_parts(
        &self,
        to: &str,
        command: &str,
        id: Option<&str>,
        rc: u8,
        body: &str,
    ) -> Result<(), SupervisedError> {
        self.gate()?;
        self.client()
            .await
            .respond_parts(to, command, id, rc, body)
            .await
            .map_err(SupervisedError::Transport)
    }

    /// SPEC 18 Phase 2 WS3-C.7f — the C.7f shutdown drain's Phase 3
    /// synth-reply path. Like [`respond_parts`] but **deliberately
    /// bypasses [`gate`]** so a citizen that has just successfully
    /// run [`deregister`] (which atomically transitions the state to
    /// `ShuttingDown` before issuing the RPC) can still answer the
    /// pending requests that were in-flight at shutdown.
    ///
    /// **Safety / scope.** This is *not* a general-purpose escape
    /// hatch from [`gate`]. The C.7f shutdown drain is the *only*
    /// caller, and only after deregister: the supervisor is stopped,
    /// the broker registry is clean, and the WS is still live (a
    /// single `noded.deregister` RPC does not close the socket).
    /// Script-side `reply()` continues to be gated — a still-
    /// executing Class C handler body that calls `reply()` during
    /// drain Phase 1 correctly receives the typed
    /// `SupervisedError::ShuttingDown` so the script can decide.
    ///
    /// A transport error here (broker WS torn down between deregister
    /// completion and synth) wraps as [`SupervisedError::Transport`]
    /// and is counted in `ClassCDrainOutcome::synth_failed` — same as
    /// any other in-flight RPC failure, distinguishable from "we
    /// knew the socket was gone" via `synth_skipped_no_socket`.
    pub async fn respond_parts_shutdown_synth(
        &self,
        to: &str,
        command: &str,
        id: Option<&str>,
        rc: u8,
        body: &str,
    ) -> Result<(), SupervisedError> {
        self.client()
            .await
            .respond_parts(to, command, id, rc, body)
            .await
            .map_err(SupervisedError::Transport)
    }

    /// See [`NodedClient::list_services`].
    ///
    /// Service discovery for SPEC 18 WS3 serve mode (the Mix
    /// `port_exists()` / `address` reachability check). Gated: a
    /// disconnected citizen reports the typed error rather than a
    /// stale or empty list it cannot vouch for.
    pub async fn list_services(&self) -> Result<Vec<String>, SupervisedError> {
        self.gate()?;
        self.client()
            .await
            .list_services()
            .await
            .map_err(SupervisedError::Transport)
    }

    // ── Topic subscription chokepoint (SPEC 18 WS2) ──

    /// Subscribe to a Ch03 topic and record it for §3.3 reconnect
    /// replay — the single registry-mutating chokepoint (SPEC 18 plan
    /// "central design decision", Option A).
    ///
    /// **Transactional ordering (consult MAJOR 1, §3.3):** the topic is
    /// recorded into the [`SubscriptionRegistry`] **only after an RC-0
    /// broker subscribe**. A rejected subscribe (reserved name → broker
    /// `rc=10`, or any transport failure) returns the typed error and
    /// leaves the registry **unchanged** — otherwise a
    /// never-satisfiable topic would be replayed forever on every
    /// reconnect. This is the `feedback_refactor_silent_noop_audit`
    /// partial-truth shape inverted: record reflects *broker-confirmed*
    /// state, never optimistic intent.
    ///
    /// The RC-0 ⇔ `Ok` equivalence is load-bearing and depends on
    /// [`NodedClient::call_with_headers`] mapping `rc >= 10` to `Err`
    /// (it does — see its docs; the `subscribe_records_only_after_rc0`
    /// / `rejected_subscribe_leaves_registry_unchanged` integration
    /// tests pin this contract end-to-end against the stub broker so a
    /// future change to that mapping cannot silently break the
    /// guarantee).
    ///
    /// Fails fast with a typed error if not `Connected` — **no queue**
    /// (§3.3). A subscribe issued while `Disconnected` is *not* deferred;
    /// the caller (the Mix `subscribe()` builtin) surfaces the error.
    /// Recorded topics already in the registry are replayed on the next
    /// reconnect regardless.
    pub async fn subscribe_topic(&self, topic: &str) -> Result<(), SupervisedError> {
        self.gate()?;
        let mut headers = std::collections::BTreeMap::new();
        headers.insert("name".to_string(), topic.to_string());
        self.client()
            .await
            .call_with_headers("noded", "topic.subscribe", &headers, "")
            .await
            .map_err(SupervisedError::Transport)?;
        // RC-0 reached here (call_with_headers bailed on rc >= 10).
        // Record only now — a duplicate is a harmless no-op (the
        // registry dedups; replay order is first-seen).
        self.registry.record(topic);
        Ok(())
    }

    /// Unsubscribe from a Ch03 topic and forget it so a later reconnect
    /// does **not** re-subscribe a deliberately-dropped topic.
    ///
    /// Symmetric to [`subscribe_topic`](Self::subscribe_topic): the
    /// registry entry is removed **only after an RC-0 unsubscribe**. A
    /// failed unsubscribe returns the typed error and leaves the
    /// registry unchanged — the topic stays recorded (and thus still
    /// replayed), which is the safe direction: a still-recorded topic
    /// over-delivers, a wrongly-forgotten one goes silently deaf after
    /// the next bounce.
    ///
    /// Fails fast with a typed error if not `Connected` — no queue.
    pub async fn unsubscribe_topic(&self, topic: &str) -> Result<(), SupervisedError> {
        self.gate()?;
        let mut headers = std::collections::BTreeMap::new();
        headers.insert("name".to_string(), topic.to_string());
        self.client()
            .await
            .call_with_headers("noded", "topic.unsubscribe", &headers, "")
            .await
            .map_err(SupervisedError::Transport)?;
        self.registry.remove(topic);
        Ok(())
    }

    /// Stop supervising **without** deregistering. Idempotent. Used by
    /// `Drop` and as the defensive stop; WS5's graceful path uses
    /// [`deregister`](Self::deregister).
    pub async fn shutdown(&self) {
        publish_state(&self.state_tx, &self.state_publish, ConnState::ShuttingDown);
        let _ = self.shutdown_tx.send(true);
        if let Some(handle) = self.supervisor.lock().await.take() {
            let _ = handle.await;
        }
    }

    /// Stop reconnect supervision and deterministically close the current
    /// socket, including its detached reader task. Idempotent and best effort.
    pub async fn close(&self) {
        publish_state(&self.state_tx, &self.state_publish, ConnState::ShuttingDown);
        let _ = self.shutdown_tx.send(true);
        // Close the currently published socket before joining. A bounded
        // caller may abandon this method if the supervisor itself is wedged;
        // the detached native reader must not retain the registered name in
        // that case. A reconnect completing after the stop edge closes its
        // fresh client before it can swap it into `inner`.
        self.client().await.close().await;
        if let Some(handle) = self.supervisor.lock().await.take() {
            let _ = handle.await;
        }
    }

    /// Graceful deregister (SPEC 18 §3.5 building block; sequencing in
    /// WS5). Marks `ShuttingDown`, stops the supervisor so it cannot
    /// race a reconnect against the deregister, then issues
    /// [`NodedClient::deregister`] on the live connection. If already
    /// disconnected the broker has dropped this name on WS-close, so a
    /// deregister RPC has nothing to do and is reported as
    /// [`SupervisedError::Disconnected`] — the caller (WS5) treats that
    /// as "already gone, proceed to exit".
    pub async fn deregister(&self) -> Result<(), SupervisedError> {
        publish_state(&self.state_tx, &self.state_publish, ConnState::ShuttingDown);
        let _ = self.shutdown_tx.send(true);
        // Join the supervisor FIRST. Once `handle.await` returns the
        // supervisor is definitively dead and `inner` can no longer be
        // swapped — so the liveness decision below is race-free. Deciding
        // on a *pre-stop* snapshot was the BLOCKER: the supervisor could
        // complete a reconnect (re-register + swap a live client) after
        // the snapshot, leaving a live name we never `noded.deregister`.
        if let Some(handle) = self.supervisor.lock().await.take() {
            let _ = handle.await;
        }
        let client = self.client().await;
        if client.is_connected() {
            client
                .deregister()
                .await
                .map_err(SupervisedError::Transport)
        } else {
            // No live socket: the broker already dropped this name on
            // WS-close (the supervisor never swapped, or the connection
            // died). Nothing to deregister — WS5 treats this as
            // "already gone, proceed to exit".
            Err(SupervisedError::Disconnected)
        }
    }
}

impl Drop for SupervisedClient {
    fn drop(&mut self) {
        // Stop a possibly-forever-reconnecting supervisor when the
        // handle is dropped without an explicit shutdown. The supervisor
        // also notices `out_tx` send failure once traffic flows, but an
        // idle reconnect loop would otherwise leak.
        publish_state(&self.state_tx, &self.state_publish, ConnState::ShuttingDown);
        let _ = self.shutdown_tx.send(true);
    }
}

/// Everything the detached supervisor task owns.
enum SupervisorOutgoing {
    Unbounded(mpsc::UnboundedSender<IncomingCommand>),
    Bounded(BoundedIncomingSender),
}

impl SupervisorOutgoing {
    /// Returns `false` only when the outward consumer has gone away.
    fn forward(&self, event: BoundedIncomingEvent) -> bool {
        match (self, event) {
            (Self::Unbounded(sender), BoundedIncomingEvent::Command(command)) => {
                sender.send(command).is_ok()
            }
            (Self::Unbounded(_), BoundedIncomingEvent::Overflow { .. }) => {
                unreachable!("an unbounded native lane cannot overflow")
            }
            (Self::Bounded(sender), BoundedIncomingEvent::Command(command)) => {
                sender.try_send(command)
            }
            (Self::Bounded(sender), BoundedIncomingEvent::Overflow { dropped }) => {
                sender.record_overflow(dropped);
                true
            }
        }
    }
}

struct SupervisorCtx {
    inner: Arc<RwLock<Arc<NodedClient>>>,
    state_tx: watch::Sender<ConnState>,
    state_publish: Arc<std::sync::Mutex<()>>,
    connection_generation: Arc<AtomicU64>,
    registry: SubscriptionRegistry,
    out_tx: SupervisorOutgoing,
    shutdown_rx: watch::Receiver<bool>,
    service_name: String,
    noded_url: String,
    /// Build provenance re-sent on every (re)register — built once by the
    /// citizen and cloned into each reconnect, so started_at stays the
    /// true process start (version-discovery contract).
    provenance: Option<cosmix_bus::RegisterProvenance>,
    /// Opt-in terminal policy for application-level register refusals.
    fatal_on_registration_rejection: bool,
    bounded_incoming_capacity: Option<usize>,
    first_rx: NativeIncomingReceiver,
}

/// `true` once a stop has been requested (explicit shutdown, or the
/// `SupervisedClient` — and thus the watch sender — was dropped).
fn stop_requested(rx: &watch::Receiver<bool>) -> bool {
    *rx.borrow()
}

fn publish_state(
    state_tx: &watch::Sender<ConnState>,
    state_publish: &std::sync::Mutex<()>,
    next: ConnState,
) {
    let _publish = state_publish
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous = *state_tx.borrow();
    if matches!(previous, ConnState::ShuttingDown | ConnState::Fatal) {
        return;
    }
    if previous != next {
        state_tx.send_replace(next);
    }
}

async fn supervisor_loop(mut ctx: SupervisorCtx) {
    let mut current_rx = ctx.first_rx;
    loop {
        // ── Forward phase: pump inner → outward until the inner
        // connection drops or a stop is requested. ──
        loop {
            tokio::select! {
                changed = ctx.shutdown_rx.changed() => {
                    // `changed()` Err = sender dropped (SupervisedClient
                    // gone) → stop. Ok → inspect the value.
                    if changed.is_err() || stop_requested(&ctx.shutdown_rx) {
                        tracing::info!(
                            event = "supervised_stop",
                            service = %ctx.service_name,
                            "supervisor stopping (shutdown requested)"
                        );
                        return;
                    }
                }
                maybe = current_rx.recv() => {
                    match maybe {
                        Some(event) => {
                            if !ctx.out_tx.forward(event) {
                                // Pump (consumer) gone — nothing left to
                                // supervise.
                                tracing::info!(
                                    event = "supervised_stop",
                                    service = %ctx.service_name,
                                    "supervisor stopping (incoming consumer dropped)"
                                );
                                return;
                            }
                        }
                        None => break, // inner connection dropped
                    }
                }
            }
        }

        if stop_requested(&ctx.shutdown_rx) {
            return;
        }

        // ── Reconnect phase: unbounded backoff (resident citizen waits
        // out a long broker outage; systemd not flapped). ──
        publish_state(&ctx.state_tx, &ctx.state_publish, ConnState::Disconnected);
        let down_since = Instant::now();
        tracing::warn!(
            event = "supervised_disconnect",
            service = %ctx.service_name,
            "broker connection lost; reconnecting (unbounded backoff)"
        );

        let mut attempt: u32 = 0;
        let new_rx = loop {
            // Backoff sleep, cancellable by a stop request.
            let delay = backoff_delay(attempt);
            tokio::select! {
                changed = ctx.shutdown_rx.changed() => {
                    if changed.is_err() || stop_requested(&ctx.shutdown_rx) {
                        return;
                    }
                }
                _ = tokio::time::sleep(delay) => {}
            }
            if stop_requested(&ctx.shutdown_rx) {
                return;
            }

            match NodedClient::connect_with_provenance_and_capacity(
                &ctx.service_name,
                &ctx.noded_url,
                ctx.provenance.clone(),
                ctx.bounded_incoming_capacity,
            )
            .await
            {
                Ok(client) => {
                    // A stop requested *during* the connect await must
                    // not yield a swapped-in, broker-registered client
                    // (BLOCKER: deregister-vs-reconnect race).
                    // `NodedClient::connect` spawns a detached reader
                    // that owns the socket, so a bare drop would leave
                    // this freshly-registered connection LIVE while
                    // `deregister()` reports `Disconnected` — the §3.5
                    // "registry must not retain a dead name" violation.
                    // `close()` deterministically tears it down so the
                    // broker's WS-close path (WS0 channel-scoped) reaps
                    // the half-registered name; `deregister()` then
                    // decides off the post-join inner state.
                    if stop_requested(&ctx.shutdown_rx) {
                        client.close().await;
                        return;
                    }

                    // Replay the ENTIRE registry in recorded order
                    // BEFORE declaring Connected. §3.3's real gate is
                    // observable re-subscribe: a partial replay that
                    // still flips Connected is the silent-no-op
                    // partial-truth shape (`feedback_refactor_silent_noop_audit`)
                    // — the citizen would look healthy while deaf on a
                    // recorded topic. Any replay failure fails the WHOLE
                    // attempt: drop the client, stay Disconnected, back
                    // off, and retry register + full ordered replay.
                    let topics = ctx.registry.snapshot();
                    let mut replay_ok = true;
                    for topic in &topics {
                        let mut headers = std::collections::BTreeMap::new();
                        headers.insert("name".to_string(), topic.clone());
                        if let Err(e) = client
                            .call_with_headers("noded", "topic.subscribe", &headers, "")
                            .await
                        {
                            tracing::warn!(
                                event = "supervised_replay_failed",
                                service = %ctx.service_name,
                                topic = %topic,
                                error = %e,
                                "subscription replay failed; failing whole reconnect \
                                 attempt (citizen stays Disconnected, will retry)"
                            );
                            replay_ok = false;
                            break;
                        }
                    }
                    if !replay_ok {
                        // Explicitly close (a bare drop leaves the
                        // detached reader holding a LIVE broker-
                        // registered socket); broker WS-close reaps the
                        // freshly-registered name. Retry the full
                        // register+replay after backoff.
                        client.close().await;
                        attempt += 1;
                        continue;
                    }

                    match client.take_native_incoming().await {
                        Some(rx) => {
                            // Final stop check before the swap: if a stop
                            // landed after the post-connect check, do not
                            // publish a live connection. (deregister()'s
                            // post-join inner read is still the
                            // authoritative race resolver; this just
                            // avoids a needless live swap.)
                            if stop_requested(&ctx.shutdown_rx) {
                                client.close().await;
                                return;
                            }
                            *ctx.inner.write().await = Arc::new(client);
                            ctx.connection_generation.fetch_add(1, Ordering::SeqCst);
                            publish_state(&ctx.state_tx, &ctx.state_publish, ConnState::Connected);
                            tracing::info!(
                                event = "supervised_reconnect",
                                service = %ctx.service_name,
                                attempts = attempt + 1,
                                downtime_ms = down_since.elapsed().as_millis() as u64,
                                replayed_subscriptions = topics.len(),
                                "reconnected to broker (full registry replayed)"
                            );
                            break rx;
                        }
                        None => {
                            // Brand-new client with no receiver = a
                            // programming-error-shaped race; explicitly
                            // close it (bare drop would leak the live
                            // registered socket) and retry rather than
                            // swap in a deaf client.
                            client.close().await;
                            tracing::warn!(
                                event = "supervised_reconnect_raced",
                                service = %ctx.service_name,
                                "fresh connection had no incoming receiver; retrying"
                            );
                            attempt += 1;
                            continue;
                        }
                    }
                }
                Err(e) => {
                    if ctx.fatal_on_registration_rejection
                        && e.downcast_ref::<RegistrationRejected>().is_some()
                    {
                        publish_state(&ctx.state_tx, &ctx.state_publish, ConnState::Fatal);
                        tracing::debug!(
                            event = "supervised_registration_rejected",
                            service = %ctx.service_name,
                            error = %e,
                            "service registration was rejected during reconnect; supervisor stopped"
                        );
                        return;
                    }
                    tracing::debug!(
                        event = "supervised_reconnect_attempt_failed",
                        service = %ctx.service_name,
                        attempt = attempt + 1,
                        error = %e,
                        "reconnect attempt failed"
                    );
                    attempt += 1;
                    continue;
                }
            }
        };

        current_rx = new_rx;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_ceiling_is_monotonic_and_capped() {
        // base 250, ×2, cap 30_000
        assert_eq!(backoff_ceiling_ms(0), 250);
        assert_eq!(backoff_ceiling_ms(1), 500);
        assert_eq!(backoff_ceiling_ms(2), 1_000);
        assert_eq!(backoff_ceiling_ms(7), 30_000); // 250*128=32_000 → capped
        assert_eq!(backoff_ceiling_ms(8), 30_000);
        // Saturation: a huge attempt must not panic on overflow.
        assert_eq!(backoff_ceiling_ms(64), 30_000);
        assert_eq!(backoff_ceiling_ms(u32::MAX), 30_000);
        // Monotonic non-decreasing.
        let mut prev = 0;
        for a in 0..40 {
            let c = backoff_ceiling_ms(a);
            assert!(c >= prev, "ceiling regressed at attempt {a}");
            prev = c;
        }
    }

    #[test]
    fn backoff_delay_within_full_jitter_window() {
        for attempt in 0..12 {
            let ceiling = backoff_ceiling_ms(attempt);
            for _ in 0..256 {
                let d = backoff_delay(attempt).as_millis() as u64;
                assert!(
                    d <= ceiling,
                    "delay {d} exceeded full-jitter ceiling {ceiling} at attempt {attempt}"
                );
            }
        }
    }

    #[test]
    fn registry_preserves_order_and_dedups() {
        let r = SubscriptionRegistry::new();
        assert!(r.is_empty());
        assert!(r.record("a"));
        assert!(r.record("b"));
        assert!(r.record("c"));
        assert!(!r.record("b"), "duplicate must be a no-op");
        assert_eq!(r.snapshot(), vec!["a", "b", "c"]);
        assert_eq!(r.len(), 3);

        assert!(r.remove("b"));
        assert!(!r.remove("b"), "removing absent topic is false");
        assert_eq!(r.snapshot(), vec!["a", "c"]);

        // Re-recording after removal appends at the end (recorded order
        // is insertion order, not original order) — replay order must
        // reflect the *current* subscription set in the order it was
        // (re-)established.
        assert!(r.record("b"));
        assert_eq!(r.snapshot(), vec!["a", "c", "b"]);
    }

    #[test]
    fn registry_handle_is_shared() {
        let r1 = SubscriptionRegistry::new();
        let r2 = r1.clone();
        r1.record("x");
        assert_eq!(r2.snapshot(), vec!["x"], "clones share storage");
        r2.remove("x");
        assert!(r1.is_empty(), "mutation through either handle is visible");
    }

    #[test]
    fn supervised_error_is_std_error_and_typed() {
        let e = SupervisedError::Disconnected;
        let dyn_err: &dyn std::error::Error = &e;
        assert!(dyn_err.to_string().contains("disconnected"));
        // Distinguishable variants for the caller (WS3/WS5).
        assert!(matches!(
            SupervisedError::ShuttingDown,
            SupervisedError::ShuttingDown
        ));
        let fatal = SupervisedError::InitialConnectFailed {
            attempts: MAX_INITIAL_ATTEMPTS,
            source: anyhow::anyhow!("refused"),
        };
        assert!(fatal.to_string().contains("5 attempt"));
        assert!(std::error::Error::source(&fatal).is_some());
    }

    #[test]
    fn state_publication_cannot_overwrite_shutdown_with_reconnect() {
        let (state_tx, state_rx) = watch::channel(ConnState::Connected);
        let publish = Arc::new(std::sync::Mutex::new(()));

        let shutdown_tx = state_tx.clone();
        let shutdown_publish = publish.clone();
        let shutdown = std::thread::spawn(move || {
            publish_state(&shutdown_tx, &shutdown_publish, ConnState::ShuttingDown);
        });
        let reconnect_tx = state_tx.clone();
        let reconnect_publish = publish.clone();
        let reconnect = std::thread::spawn(move || {
            publish_state(&reconnect_tx, &reconnect_publish, ConnState::Connected);
        });
        shutdown.join().expect("shutdown publisher");
        reconnect.join().expect("reconnect publisher");

        assert_eq!(*state_rx.borrow(), ConnState::ShuttingDown);
    }
}
