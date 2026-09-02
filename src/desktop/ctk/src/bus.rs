//! Tokio-owned Bus client bridged into Bevy through bounded channels.
//!
//! The worker thread is the only place that touches [`cosmix_client::SupervisedClient`].
//! Bevy systems submit semantic calls and drain owned messages without awaiting or blocking.
//!
//! # Independent broker connections
//!
//! A single broker WebSocket delivers every frame FIFO, so RPC replies queue
//! behind high-rate subscription publications (a 60 Hz meter stream head-of-line
//! blocks a fader-write ack by tens of milliseconds). The worker therefore owns
//! two always-on [`SupervisedClient`]s and one opt-in observation client:
//!
//! * **control** — the broker-verified writing identity ([`BusBridgeConfig::service_name`],
//!   the `source_id` peers attribute our writes to). It carries **no** subscriptions,
//!   so its socket only ever moves request/reply traffic and acks return in
//!   low-single-digit milliseconds.
//! * **telemetry** — a separate `…-sub` identity that carries **all** subscriptions
//!   (meters/changed/applied) and **never** issues writes.
//! * **observation** — opt-in `…-observe` identity with its own request queue,
//!   inbound ring, generation and drop counter. Broker traffic therefore never
//!   shares a socket or queue with operator RPCs.
//!
//! The control/telemetry split is exposed to Bevy as one combined
//! [`BusConnectionState`] (`Connected` only when both planes are up) and one
//! monotonic connection generation. Observation has its own connection state,
//! generation, events and bounded message drain.
//!
//! Because a reconnect interleaves pre- and post-reconnect frames on the one
//! telemetry stream, the worker **fences** telemetry the instant it detects a
//! per-plane generation change: it drops the latest-wins mailbox and forwards
//! nothing until the new combined epoch is committed and announced. Every
//! forwarded message is therefore provably tagged with the epoch of the socket
//! it arrived on, and the fenced window is re-established by the adapter's
//! reconnect snapshot.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use bevy::app::{App, Plugin, PreStartup};
use bevy::ecs::resource::Resource;
use bevy::ecs::system::{Commands, Res};
use cosmix_bus::RegisterProvenance;
use cosmix_client::{ConnState, SupervisedClient, SupervisedError};
use flume::{Receiver, Sender, TrySendError};
use tokio::task::JoinSet;

const MAX_INFLIGHT_CALLS: usize = 32;
const MAX_OBSERVE_INFLIGHT_CALLS: usize = 4;
const OBSERVATION_REQUEST_CAPACITY: usize = 16;
/// A broker capture is capped at 64 KiB before wrapping, but JSON escaping can
/// expand hostile-yet-valid strings by roughly 6× and the observation event
/// copies headers into its envelope. One MiB safely admits the contract's
/// worst case and matches noded's per-subscription drainer work bound.
const MAX_OBSERVATION_MESSAGE_BYTES: usize = 1024 * 1024;
const OBSERVATION_STOP_FLUSH: std::time::Duration = std::time::Duration::from_millis(150);

/// Inbound `app.*` requests buffered toward the Bevy answering system. Small
/// on purpose: control verbs are low-rate operator/agent traffic, and a full
/// queue answers RC 11 busy immediately rather than accumulating latency.
const INBOUND_REQUEST_CAPACITY: usize = 16;

/// Calls accepted beyond [`MAX_INFLIGHT_CALLS`] and parked for launch as
/// slots free. Intake is never gated (a full pipeline must not delay
/// `Respond`/`Shutdown`); a call past this bound fails fast instead.
const MAX_PENDING_CALLS: usize = 64;

/// How long the worker waits, on shutdown, for in-flight reply writes to flush
/// before it deregisters and exits. Bounds the app.quit-style
/// answer-then-exit window without letting a wedged socket hang teardown.
const RESPOND_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_millis(250);

/// In-flight response tasks. Past this a reply is dropped (the peer times
/// out) — the bound exists so a request flood against a blocked socket can
/// not grow the task set without limit.
const MAX_INFLIGHT_RESPONDS: usize = 32;

/// In-flight fire-and-forget topic publishes. The outbound request channel is
/// already bounded; this second bound prevents a blocked socket from turning
/// every dequeued publish into another retained task.
const MAX_INFLIGHT_PUBLISHES: usize = 16;

/// A fire-and-forget publish must never hold the control sink indefinitely.
const PUBLISH_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

/// Largest inbound request body forwarded to the app. The body is parsed as
/// JSON on the Bevy thread; an unbounded payload is a frame-time attack.
const MAX_INBOUND_BODY_BYTES: usize = 64 * 1024;

/// Sentinel published into the shared committed-generation atomic while an
/// epoch fence is pending. No real combined generation is ever 0 (they start
/// at 1), so every stamped request fails the drain filter until the commit
/// publishes the new epoch.
const GENERATION_FENCED: u64 = 0;

type WorkerWake = Arc<dyn Fn() + Send + Sync>;

fn no_op_wake() -> WorkerWake {
    Arc::new(|| {})
}

/// Event-loop wake callback used when CTK is hosted without winit.
#[derive(Clone)]
pub struct BusWorkerWake(WorkerWake);

impl std::fmt::Debug for BusWorkerWake {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("BusWorkerWake(..)")
    }
}

impl BusWorkerWake {
    pub fn new(callback: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self(callback)
    }
}

#[derive(Clone)]
struct WakeSender<T> {
    inner: Sender<T>,
    wake: WorkerWake,
}

impl<T> WakeSender<T> {
    fn new(inner: Sender<T>, wake: WorkerWake) -> Self {
        Self { inner, wake }
    }

    fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        let result = self.inner.try_send(value);
        if result.is_ok() {
            (self.wake)();
        }
        result
    }

    async fn send_async(&self, value: T) -> Result<(), flume::SendError<T>> {
        let result = self.inner.send_async(value).await;
        if result.is_ok() {
            (self.wake)();
        }
        result
    }
}

#[derive(Default)]
struct SemanticInboxes {
    #[cfg(feature = "theme")]
    theme_changed: Mutex<Option<BusMessage>>,
}

impl SemanticInboxes {
    fn clear(&self) {
        #[cfg(feature = "theme")]
        self.theme_changed.lock().unwrap().take();
    }

    #[cfg(feature = "theme")]
    fn enqueue_theme_changed(&self, message: BusMessage, wake: &WorkerWake) {
        *self.theme_changed.lock().unwrap() = Some(message);
        wake();
    }

    #[cfg(feature = "theme")]
    fn drain_theme_changed(&self) -> Option<BusMessage> {
        self.theme_changed.lock().unwrap().take()
    }
}

#[cfg(feature = "theme")]
fn route_theme_changed_delivery(
    inboxes: &SemanticInboxes,
    message: BusMessage,
    wake: &WorkerWake,
) -> bool {
    if !crate::theme_sync::is_valid_local_delivery(&message) {
        return false;
    }
    inboxes.enqueue_theme_changed(message, wake);
    true
}

/// One immutable process identity shared by every CTK bridge plane and cloned
/// into every supervised reconnect. Fallback only: version/git/build fields
/// identify the CTK bridge build, because a Rust dependency cannot inspect
/// its caller crate's `CARGO_PKG_*` values. Apps MUST override
/// [`BusBridgeConfig::provenance`] with [`provenance_from_build`] fed by
/// `cosmix_buildinfo::build_info!()` expanded in the app crate, so the
/// version-discovery surface reports the app's own version, not ctk's.
static PROCESS_PROVENANCE: OnceLock<RegisterProvenance> = OnceLock::new();

fn process_provenance() -> RegisterProvenance {
    PROCESS_PROVENANCE
        .get_or_init(|| provenance_from_build(cosmix_buildinfo::build_info!()))
        .clone()
}

/// Build registration provenance from a caller-supplied [`cosmix_buildinfo::BuildInfo`].
///
/// Call `cosmix_buildinfo::build_info!()` **in the app crate** (macro
/// expansion captures the invoking crate's `CARGO_PKG_*` values and the
/// consumer repo's git stamp from its `build.rs` `emit()`), pass the result
/// here, and assign it to [`BusBridgeConfig::provenance`] once at startup —
/// `started_at` is stamped now, and the bridge re-sends the same value on
/// every reconnect.
pub fn provenance_from_build(build: cosmix_buildinfo::BuildInfo) -> RegisterProvenance {
    let binary = std::env::current_exe()
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| build.pkg.to_string());
    RegisterProvenance::from_parts(
        &binary,
        build.version,
        build.git_sha,
        build.git_dirty,
        build.build_time,
        cosmix_buildinfo::now_rfc3339(),
    )
}

/// Loopback fallback when the node has no readable `node.conf.mix`.
const FALLBACK_NODED_URL: &str = "ws://127.0.0.1:4200/ws";

/// Resolve the broker WebSocket URL the way every other native citizen does:
/// from `node.conf.mix` (`ws://{wg_ip}:{port}/ws`), falling back to loopback
/// only when the node is unconfigured. Desktop apps use this as the default
/// when no explicit `--noded-url` is supplied — a mesh node's broker binds
/// its WG address, not loopback, so a hardcoded `127.0.0.1` default can
/// never connect there.
///
/// Same semantics as `cosmix_config::client_helpers::resolve_noded_url`,
/// but diagnostics go to stderr: apps call this before Bevy installs its
/// tracing subscriber, so a `tracing::warn!` here would be silently lost —
/// exactly when the user most needs to know why the app is dialling
/// loopback on a configured mesh node (e.g. an unreadable config file).
pub fn resolve_noded_url() -> String {
    match cosmix_config::node::load_node_config() {
        Ok(Some(config)) => config.noded_url(),
        Ok(None) => {
            eprintln!("ctk: node.conf.mix not found; falling back to {FALLBACK_NODED_URL}");
            FALLBACK_NODED_URL.into()
        }
        Err(error) => {
            eprintln!(
                "ctk: failed to load node.conf.mix ({error:#}); falling back to {FALLBACK_NODED_URL}"
            );
            FALLBACK_NODED_URL.into()
        }
    }
}

#[derive(Resource, Clone, Debug)]
pub struct BusBridgeConfig {
    pub service_name: String,
    pub noded_url: String,
    /// Immutable process/build identity sent on both registrations and every
    /// reconnect. Constructed once per process so `started_at` cannot drift.
    pub provenance: RegisterProvenance,
    pub subscriptions: Vec<String>,
    /// Additional directed command prefixes admitted to the exclusive service port.
    pub inbound_prefixes: Vec<String>,
    /// Explicit non-winit event-loop wake. When absent, CTK retains its winit fallback.
    pub worker_wake: Option<BusWorkerWake>,
    /// Topics whose newest message replaces the previous one instead of queuing.
    pub latest_topics: Vec<String>,
    pub outbound_capacity: usize,
    pub event_capacity: usize,
    pub message_capacity: usize,
    pub max_messages_per_frame: usize,
    /// Opt-in third websocket used exclusively for broker observation.
    pub observation: bool,
    pub observation_capacity: usize,
    pub max_observation_messages_per_frame: usize,
}

impl BusBridgeConfig {
    pub fn new(service_name: impl Into<String>, noded_url: impl Into<String>) -> Self {
        Self {
            service_name: service_name.into(),
            noded_url: noded_url.into(),
            provenance: process_provenance(),
            subscriptions: Vec::new(),
            inbound_prefixes: Vec::new(),
            worker_wake: None,
            latest_topics: Vec::new(),
            outbound_capacity: 64,
            event_capacity: 128,
            message_capacity: 512,
            max_messages_per_frame: 128,
            observation: false,
            observation_capacity: 2048,
            max_observation_messages_per_frame: 256,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BusConnectionState {
    #[default]
    Connecting,
    Connected,
    Disconnected,
    ShuttingDown,
    Fatal,
}

impl From<ConnState> for BusConnectionState {
    fn from(value: ConnState) -> Self {
        match value {
            ConnState::Connecting => Self::Connecting,
            ConnState::Connected => Self::Connected,
            ConnState::Disconnected => Self::Disconnected,
            ConnState::ShuttingDown => Self::ShuttingDown,
            ConnState::Fatal => Self::Fatal,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BusMessage {
    pub connection_generation: u64,
    pub from: String,
    pub command: String,
    pub body: String,
    pub headers: BTreeMap<String, String>,
}

impl BusMessage {
    pub fn topic(&self) -> Option<&str> {
        self.headers.get("topic").map(String::as_str)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BusReply {
    pub rc: u8,
    pub body: String,
    pub result: Option<String>,
}

/// An inbound `app.*` request another mesh citizen directed at this app's
/// control port (the ARexx model: every app is an addressable command port).
///
/// Drained by the `app_control` answering system; replies go back through
/// [`BusBridge::try_respond`], which carries `connection_generation` so the
/// worker can drop a response whose epoch has been reconnected away — the
/// peer's correlation is gone and the answer may describe pre-resync state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboundRequest {
    pub connection_generation: u64,
    pub from: String,
    pub command: String,
    pub headers: BTreeMap<String, String>,
    pub body: String,
    /// The sender's correlation id. `None` = fire-and-forget send: apply the
    /// effect (if any) but never answer — a reply without an id is dropped as
    /// an orphan at the peer anyway.
    pub reply_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BusBridgeEvent {
    Connection {
        state: BusConnectionState,
        generation: u64,
    },
    Reply {
        request_id: u64,
        result: Result<BusReply, String>,
    },
    DroppedMessages(usize),
    ObservationConnection {
        state: BusConnectionState,
        generation: u64,
    },
    ObservationReply {
        request_id: u64,
        result: Result<BusReply, String>,
    },
    ObservationDroppedMessages(usize),
    Fatal(String),
}

#[cfg_attr(test, derive(Debug))]
enum WorkerRequest {
    Call {
        request_id: u64,
        to: String,
        command: String,
        headers: BTreeMap<String, String>,
        body: String,
    },
    Publish {
        to: String,
        command: String,
        headers: BTreeMap<String, String>,
        body: String,
        /// The committed combined epoch when Bevy queued this publish.
        generation: u64,
    },
    Respond {
        to: String,
        command: String,
        id: String,
        rc: u8,
        body: String,
        /// The combined epoch the request was forwarded under; a response
        /// stamped with a stale epoch is dropped instead of sent.
        generation: u64,
    },
    Shutdown,
}

#[cfg_attr(test, derive(Debug))]
enum ObservationRequest {
    Call {
        request_id: u64,
        to: String,
        command: String,
        headers: BTreeMap<String, String>,
        body: String,
    },
    StopFlush {
        request_id: u64,
        body: String,
    },
}

#[derive(Resource)]
pub struct BusBridge {
    requests: Sender<WorkerRequest>,
    observation_requests: Sender<ObservationRequest>,
    events: Receiver<BusBridgeEvent>,
    messages: Receiver<BusMessage>,
    observation_messages: Receiver<BusMessage>,
    inbound: Receiver<InboundRequest>,
    /// The worker's committed combined epoch, shared so the Bevy side can
    /// refuse to EXECUTE a request whose epoch has been reconnected away
    /// (the worker would only have dropped its response). Holds
    /// [`GENERATION_FENCED`] while an epoch fence is pending, so no inbound
    /// request executes mid-resync at all.
    committed_generation: Arc<AtomicU64>,
    latest_messages: Arc<Mutex<HashMap<String, BusMessage>>>,
    semantic_inboxes: Arc<SemanticInboxes>,
    /// Same wake the worker fires on delivery. The capped drains re-arm it
    /// when a frame cannot clear the queue, so a burst larger than the
    /// per-frame cap never strands messages waiting for an unrelated event
    /// (the worker's per-send wakes have already been coalesced away by then).
    wake: WorkerWake,
    max_messages_per_frame: usize,
    max_observation_messages_per_frame: usize,
    service_name: String,
    /// Signalled by the worker once it has drained in-flight replies and
    /// deregistered on shutdown. [`Drop`] waits on it (bounded) so a verb that
    /// answers and then triggers app exit (e.g. `app.quit`) can flush its ack
    /// before the detached worker thread dies with the process.
    shutdown_done: Receiver<()>,
    /// Set once [`Self::mixer_transport`] hands the telemetry streams to the
    /// mixer seam (`mixer` feature). The bridge's own drain methods then
    /// panic instead of silently competing for the shared flume queues.
    telemetry_taken: bool,
}

impl BusBridge {
    /// Guard for the telemetry drain methods once a mixer transport owns the
    /// streams (see [`Self::mixer_transport`]).
    fn assert_telemetry_owned(&self) {
        assert!(
            !self.telemetry_taken,
            "BusBridge telemetry streams are owned by the mixer transport — drain via the seam"
        );
    }

    /// The broker-registered identity this bridge writes AS — the `source_id`
    /// peers see on our accepted writes (own-write attribution).
    pub fn service_name(&self) -> &str {
        &self.service_name
    }

    pub fn try_call(
        &self,
        request_id: u64,
        to: impl Into<String>,
        command: impl Into<String>,
        headers: BTreeMap<String, String>,
        body: impl Into<String>,
    ) -> Result<(), String> {
        self.requests
            .try_send(WorkerRequest::Call {
                request_id,
                to: to.into(),
                command: command.into(),
                headers,
                body: body.into(),
            })
            .map_err(|error| match error {
                TrySendError::Full(_) => "Bus bridge outbound channel is full".to_string(),
                TrySendError::Disconnected(_) => "Bus bridge worker has stopped".to_string(),
            })
    }

    /// Publish an inner Bus envelope to a broker topic without owning a reply
    /// correlation. The broker still sends its ordinary acknowledgement, but
    /// the client drops that orphan response instead of putting it on any Bevy
    /// queue.
    pub fn try_publish_topic(
        &self,
        name: impl Into<String>,
        retain: bool,
        inner_wire: impl Into<String>,
    ) -> Result<(), String> {
        let mut headers = BTreeMap::new();
        headers.insert("name".to_string(), name.into());
        headers.insert("retain".to_string(), retain.to_string());
        self.requests
            .try_send(WorkerRequest::Publish {
                to: "noded".to_string(),
                command: "topic.publish".to_string(),
                headers,
                body: inner_wire.into(),
                generation: self.committed_generation.load(Ordering::Acquire),
            })
            .map_err(|error| match error {
                TrySendError::Full(_) => "Bus bridge outbound channel is full".to_string(),
                TrySendError::Disconnected(_) => "Bus bridge worker has stopped".to_string(),
            })
    }

    pub fn try_observe_call(
        &self,
        request_id: u64,
        to: impl Into<String>,
        command: impl Into<String>,
        headers: BTreeMap<String, String>,
        body: impl Into<String>,
    ) -> Result<(), String> {
        self.observation_requests
            .try_send(ObservationRequest::Call {
                request_id,
                to: to.into(),
                command: command.into(),
                headers,
                body: body.into(),
            })
            .map_err(|error| match error {
                TrySendError::Full(_) => "Bus observation outbound channel is full".to_string(),
                TrySendError::Disconnected(_) => "Bus observation worker has stopped".to_string(),
            })
    }

    /// Queue a shutdown-only observation stop. The worker drains this request
    /// ahead of disconnect with a 150 ms response bound.
    pub fn try_observe_stop_flush(
        &self,
        request_id: u64,
        body: impl Into<String>,
    ) -> Result<(), String> {
        self.observation_requests
            .try_send(ObservationRequest::StopFlush {
                request_id,
                body: body.into(),
            })
            .map_err(|error| match error {
                TrySendError::Full(_) => "Bus observation stop-flush channel is full".to_string(),
                TrySendError::Disconnected(_) => "Bus observation worker has stopped".to_string(),
            })
    }

    pub fn drain_events(&self) -> impl Iterator<Item = BusBridgeEvent> + '_ {
        self.assert_telemetry_owned();
        self.events.try_iter()
    }

    pub fn drain_messages(&self) -> impl Iterator<Item = BusMessage> + '_ {
        self.assert_telemetry_owned();
        if self.messages.len() > self.max_messages_per_frame {
            // This frame's capped drain cannot clear the queue and the
            // worker's sends already spent their wakes: re-arm one so the
            // remainder is drained by a real follow-up update, not by
            // whenever the next unrelated event happens to tick the loop.
            (self.wake)();
        }
        self.messages.try_iter().take(self.max_messages_per_frame)
    }

    pub fn drain_observation_messages(&self) -> impl Iterator<Item = BusMessage> + '_ {
        if self.observation_messages.len() > self.max_observation_messages_per_frame {
            (self.wake)();
        }
        self.observation_messages
            .try_iter()
            .take(self.max_observation_messages_per_frame)
    }

    pub fn drain_latest_messages(&self) -> Vec<BusMessage> {
        self.assert_telemetry_owned();
        let mut latest = self.latest_messages.lock().unwrap();
        std::mem::take(&mut *latest).into_values().collect()
    }

    #[cfg(feature = "theme")]
    pub(crate) fn drain_theme_changed(&self) -> Option<BusMessage> {
        self.semantic_inboxes.drain_theme_changed()
    }

    /// Drain inbound `app.*` requests directed at this app's control port.
    ///
    /// Requests forwarded under an epoch that has since been reconnected
    /// away are filtered out here: their responses would be dropped by the
    /// worker anyway, and a stale `set` must not execute against freshly
    /// resynced state.
    ///
    /// INVARIANT (conventional since this went `pub` for app-owned substrate
    /// services): each app has exactly ONE inbound drain + reply owner —
    /// either the [`AppPortPlugin`](crate::app_control) router OR one
    /// app-owned service system, never both in the same `App`. Two drainers
    /// steal each other's requests nondeterministically; a registered verb
    /// handler holds the [`InboundRequest`] (for params/provenance) but must
    /// never reach the correlation-bearing response path, or it could
    /// double-answer one id or steal another request's inbound.
    pub fn drain_inbound(&self) -> impl Iterator<Item = InboundRequest> + '_ {
        let current = self.committed_generation.load(Ordering::Acquire);
        self.inbound
            .try_iter()
            .filter(move |request| request.connection_generation == current)
    }

    /// Answer a drained [`InboundRequest`]. A fire-and-forget send (no
    /// correlation id) is silently a no-op — there is nothing to answer.
    ///
    /// Caller: the app's single inbound drain + reply owner only (see
    /// `drain_inbound`'s invariant).
    pub fn try_respond(
        &self,
        request: &InboundRequest,
        rc: u8,
        body: impl Into<String>,
    ) -> Result<(), String> {
        let Some(id) = request.reply_id.clone() else {
            return Ok(());
        };
        self.requests
            .try_send(WorkerRequest::Respond {
                to: request.from.clone(),
                command: request.command.clone(),
                id,
                rc,
                body: body.into(),
                generation: request.connection_generation,
            })
            .map_err(|error| match error {
                TrySendError::Full(_) => "Bus bridge outbound channel is full".to_string(),
                TrySendError::Disconnected(_) => "Bus bridge worker has stopped".to_string(),
            })
    }

    /// Discard every ordinary and latest-wins message currently queued.
    ///
    /// Normal frame drains stay capped; authority-epoch recovery is the rare
    /// case where retaining any pre-snapshot backlog is less safe than draining
    /// it synchronously.
    pub fn discard_messages(&self) {
        self.assert_telemetry_owned();
        self.messages.try_iter().for_each(drop);
        self.latest_messages.lock().unwrap().clear();
        self.semantic_inboxes.clear();
    }

    pub fn try_shutdown(&self) {
        let _ = self.requests.try_send(WorkerRequest::Shutdown);
    }
}

impl Drop for BusBridge {
    fn drop(&mut self) {
        self.try_shutdown();
        // Wait, bounded, for the worker to flush any in-flight reply (the
        // app.quit ack) and deregister. Without this the process exits and the
        // detached worker thread is killed mid-send, so a verb that both
        // answers and triggers exit loses its ack. Bounded so a wedged worker
        // can't hang teardown; a ready channel (tests, already-exited worker)
        // returns instantly.
        let _ = self
            .shutdown_done
            .recv_timeout(std::time::Duration::from_millis(300));
    }
}

#[cfg(test)]
pub(crate) struct TestBusPeer {
    inbound: Sender<InboundRequest>,
    requests: Receiver<WorkerRequest>,
    #[cfg(feature = "theme")]
    semantic_inboxes: Arc<SemanticInboxes>,
}

#[cfg(test)]
pub(crate) struct TestBusResponse {
    pub command: String,
    pub rc: u8,
    pub body: String,
}

#[cfg(all(test, feature = "theme"))]
pub(crate) struct TestBusPublish {
    pub to: String,
    pub command: String,
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

#[cfg(test)]
impl TestBusPeer {
    pub fn send(&self, request: InboundRequest) {
        self.inbound.send(request).expect("test inbound is open");
    }

    pub fn drain_responses(&self) -> Vec<TestBusResponse> {
        self.requests
            .try_iter()
            .filter_map(|request| match request {
                WorkerRequest::Respond {
                    command, rc, body, ..
                } => Some(TestBusResponse { command, rc, body }),
                WorkerRequest::Call { .. }
                | WorkerRequest::Publish { .. }
                | WorkerRequest::Shutdown => None,
            })
            .collect()
    }

    #[cfg(feature = "theme")]
    pub fn drain_publishes(&self) -> Vec<TestBusPublish> {
        self.requests
            .try_iter()
            .filter_map(|request| match request {
                WorkerRequest::Publish {
                    to,
                    command,
                    headers,
                    body,
                    ..
                } => Some(TestBusPublish {
                    to,
                    command,
                    headers,
                    body,
                }),
                WorkerRequest::Call { .. }
                | WorkerRequest::Respond { .. }
                | WorkerRequest::Shutdown => None,
            })
            .collect()
    }

    #[cfg(feature = "theme")]
    pub fn deliver_theme_changed(&self, message: BusMessage) {
        route_theme_changed_delivery(&self.semantic_inboxes, message, &no_op_wake());
    }
}

#[cfg(test)]
pub(crate) fn test_bridge(service_name: &str) -> (BusBridge, TestBusPeer) {
    let (request_tx, request_rx) = flume::bounded(16);
    let (observation_request_tx, _observation_request_rx) = flume::bounded(4);
    let (_event_tx, event_rx) = flume::bounded(16);
    let (_message_tx, message_rx) = flume::bounded(16);
    let (_observation_message_tx, observation_message_rx) = flume::bounded(16);
    let (inbound_tx, inbound_rx) = flume::bounded(16);
    // Pre-seed the done channel so Drop (no worker here) returns instantly
    // instead of blocking the full grace on every dropped test bridge.
    let (shutdown_done_tx, shutdown_done_rx) = flume::bounded(1);
    let _ = shutdown_done_tx.send(());
    let semantic_inboxes = Arc::new(SemanticInboxes::default());
    (
        BusBridge {
            requests: request_tx,
            observation_requests: observation_request_tx,
            events: event_rx,
            messages: message_rx,
            observation_messages: observation_message_rx,
            inbound: inbound_rx,
            committed_generation: Arc::new(AtomicU64::new(1)),
            latest_messages: Arc::new(Mutex::new(HashMap::new())),
            semantic_inboxes: semantic_inboxes.clone(),
            wake: no_op_wake(),
            max_messages_per_frame: 16,
            max_observation_messages_per_frame: 16,
            service_name: service_name.into(),
            shutdown_done: shutdown_done_rx,
            telemetry_taken: false,
        },
        TestBusPeer {
            inbound: inbound_tx,
            requests: request_rx,
            #[cfg(feature = "theme")]
            semantic_inboxes,
        },
    )
}

pub struct BusBridgePlugin {
    config: BusBridgeConfig,
}

impl BusBridgePlugin {
    pub fn new(config: BusBridgeConfig) -> Self {
        Self { config }
    }
}

impl Plugin for BusBridgePlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(self.config.clone())
            .add_systems(PreStartup, start_bridge);
    }
}

pub(crate) fn start_bridge(
    mut commands: Commands,
    config: Res<BusBridgeConfig>,
    #[cfg(feature = "theme")] event_loop_proxy: Option<Res<bevy::winit::EventLoopProxyWrapper>>,
) {
    let (request_tx, request_rx) = flume::bounded(config.outbound_capacity.max(1));
    let (observation_request_tx, observation_request_rx) =
        flume::bounded(OBSERVATION_REQUEST_CAPACITY);
    let (event_tx, event_rx) = flume::bounded(config.event_capacity.max(1));
    let (message_tx, message_rx) = flume::bounded(config.message_capacity.max(1));
    let (observation_message_tx, observation_message_rx) =
        flume::bounded(config.observation_capacity.max(1));
    let (inbound_tx, inbound_rx) = flume::bounded(INBOUND_REQUEST_CAPACITY);
    // Capacity 1: the worker sends exactly one shutdown-done signal and Drop
    // reads at most one; a rendezvous is unnecessary.
    let (shutdown_done_tx, shutdown_done_rx) = flume::bounded(1);
    let latest_messages = Arc::new(Mutex::new(HashMap::new()));
    let semantic_inboxes = Arc::new(SemanticInboxes::default());
    let committed_generation = Arc::new(AtomicU64::new(1));
    let worker_config = config.clone();
    let worker_latest = latest_messages.clone();
    let worker_semantic = semantic_inboxes.clone();
    let worker_generation = committed_generation.clone();
    #[cfg(feature = "theme")]
    let worker_wake: WorkerWake = config.worker_wake.clone().map_or_else(
        || {
            event_loop_proxy.map_or_else(no_op_wake, |proxy| {
                let proxy = (**proxy).clone();
                Arc::new(move || {
                    let _ = proxy.send_event(bevy::winit::WinitUserEvent::WakeUp);
                })
            })
        },
        |wake| wake.0,
    );
    #[cfg(not(feature = "theme"))]
    let worker_wake = config
        .worker_wake
        .clone()
        .map_or_else(no_op_wake, |wake| wake.0);

    let bridge_wake = worker_wake.clone();
    thread::Builder::new()
        .name(format!("ctk-bus-{}", worker_config.service_name))
        .spawn(move || {
            worker_main(WorkerMainParams {
                config: worker_config,
                requests: request_rx,
                observation_requests: observation_request_rx,
                events: WakeSender::new(event_tx, worker_wake.clone()),
                messages: WakeSender::new(message_tx, worker_wake.clone()),
                observation_messages: WakeSender::new(observation_message_tx, worker_wake.clone()),
                inbound: WakeSender::new(inbound_tx, worker_wake.clone()),
                committed_generation: worker_generation,
                latest_messages: worker_latest,
                semantic_inboxes: worker_semantic,
                wake: worker_wake,
                shutdown_done: shutdown_done_tx,
            })
        })
        .expect("spawn CTK Bus worker");

    commands.insert_resource(BusBridge {
        requests: request_tx,
        observation_requests: observation_request_tx,
        events: event_rx,
        messages: message_rx,
        observation_messages: observation_message_rx,
        inbound: inbound_rx,
        committed_generation,
        latest_messages,
        semantic_inboxes,
        wake: bridge_wake,
        max_messages_per_frame: config.max_messages_per_frame.max(1),
        max_observation_messages_per_frame: config.max_observation_messages_per_frame.max(1),
        service_name: config.service_name.clone(),
        shutdown_done: shutdown_done_rx,
        telemetry_taken: false,
    });
}

struct WorkerMainParams {
    config: BusBridgeConfig,
    requests: Receiver<WorkerRequest>,
    observation_requests: Receiver<ObservationRequest>,
    events: WakeSender<BusBridgeEvent>,
    messages: WakeSender<BusMessage>,
    observation_messages: WakeSender<BusMessage>,
    inbound: WakeSender<InboundRequest>,
    committed_generation: Arc<AtomicU64>,
    latest_messages: Arc<Mutex<HashMap<String, BusMessage>>>,
    semantic_inboxes: Arc<SemanticInboxes>,
    wake: WorkerWake,
    shutdown_done: Sender<()>,
}

fn worker_main(params: WorkerMainParams) {
    let WorkerMainParams {
        config,
        requests,
        observation_requests,
        events,
        messages,
        observation_messages,
        inbound,
        committed_generation,
        latest_messages,
        semantic_inboxes,
        wake,
        shutdown_done,
    } = params;
    // Fires on EVERY exit path so a dropping BusBridge never waits the full
    // grace on a worker that has already stopped (runtime build failure here,
    // or a clean drain-and-deregister after worker_loop returns).
    let _guard = SendOnDrop(shutdown_done);
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("ctk-bus-tokio")
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = events.try_send(BusBridgeEvent::Fatal(error.to_string()));
            return;
        }
    };
    runtime.block_on(worker_loop(WorkerLoopParams {
        config,
        requests,
        observation_requests,
        events,
        messages,
        observation_messages,
        inbound,
        committed_generation,
        latest_messages,
        semantic_inboxes,
        wake,
    }));
}

/// Signals the [`BusBridge`] shutdown-done channel from `Drop`, so the worker
/// covers every return path (including a `worker_loop` panic unwinding through
/// `block_on`) without a `let _ =` at each one.
struct SendOnDrop(Sender<()>);

impl Drop for SendOnDrop {
    fn drop(&mut self) {
        let _ = self.0.try_send(());
    }
}

/// Fold the two plane states into the single [`BusConnectionState`] the Bevy
/// side observes. `Connected` is reported only when **both** planes are up;
/// otherwise the worst-of-the-two is returned so any plane drop reads as a
/// disconnect and drives a full resync. One combined state, no per-plane
/// distinction visible to consumers.
fn combined_connection_state(control: ConnState, telemetry: ConnState) -> BusConnectionState {
    let planes = [
        BusConnectionState::from(control),
        BusConnectionState::from(telemetry),
    ];
    // Priority, worst first: any Fatal/ShuttingDown/Disconnected/Connecting
    // plane demotes the combined state; only two Connected planes are Connected.
    for level in [
        BusConnectionState::Fatal,
        BusConnectionState::ShuttingDown,
        BusConnectionState::Disconnected,
        BusConnectionState::Connecting,
    ] {
        if planes.contains(&level) {
            return level;
        }
    }
    BusConnectionState::Connected
}

struct WorkerLoopParams {
    config: BusBridgeConfig,
    requests: Receiver<WorkerRequest>,
    observation_requests: Receiver<ObservationRequest>,
    events: WakeSender<BusBridgeEvent>,
    messages: WakeSender<BusMessage>,
    observation_messages: WakeSender<BusMessage>,
    inbound: WakeSender<InboundRequest>,
    committed_generation: Arc<AtomicU64>,
    latest_messages: Arc<Mutex<HashMap<String, BusMessage>>>,
    semantic_inboxes: Arc<SemanticInboxes>,
    wake: WorkerWake,
}

async fn worker_loop(params: WorkerLoopParams) {
    let WorkerLoopParams {
        config,
        requests,
        observation_requests,
        events,
        messages,
        observation_messages,
        inbound,
        committed_generation,
        latest_messages,
        semantic_inboxes,
        wake,
    } = params;
    // CONTROL plane: the broker-verified writing identity. Every
    // WorkerRequest::Call is issued here and it carries NO subscriptions, so
    // RPC replies are never head-of-line-blocked behind telemetry publications.
    let control = Arc::new(
        match connect_supervised_plane(&config.service_name, &config.noded_url, &config.provenance)
            .await
        {
            Ok(client) => client,
            Err(error) => {
                let _ = events
                    .send_async(BusBridgeEvent::Fatal(error.to_string()))
                    .await;
                return;
            }
        },
    );

    // TELEMETRY plane: a separate `…-sub` identity that carries ALL
    // subscriptions and never issues a write. Keeping the 60 Hz meter/changed
    // stream off the control socket is the whole point of the split.
    let telemetry_name = format!("{}-sub", config.service_name);
    let telemetry = match connect_supervised_plane(
        &telemetry_name,
        &config.noded_url,
        &config.provenance,
    )
    .await
    {
        Ok(client) => client,
        Err(error) => {
            control.shutdown().await;
            let _ = events
                .send_async(BusBridgeEvent::Fatal(error.to_string()))
                .await;
            return;
        }
    };

    if let Err(error) = install_subscriptions(&telemetry, &config.subscriptions).await {
        control.shutdown().await;
        telemetry.shutdown().await;
        let _ = events.send_async(BusBridgeEvent::Fatal(error)).await;
        return;
    }

    let Some(mut incoming) = telemetry.incoming() else {
        control.shutdown().await;
        telemetry.shutdown().await;
        let _ = events
            .send_async(BusBridgeEvent::Fatal(
                "Bus incoming stream already taken".into(),
            ))
            .await;
        return;
    };

    // The control plane has no subscriptions and no inbound verb surface, so
    // its incoming stream should stay empty. Take and discard it anyway: a
    // stray directed request would otherwise accumulate unbounded in the
    // supervisor's forward buffer, and this keeps "control carries no
    // telemetry" an executable invariant rather than an assumption.
    let mut control_incoming = control.incoming();

    // OBSERVATION plane: opt-in, independently registered and independently
    // buffered. It is never part of the control/telemetry combined epoch, so
    // a traffic flood or observer reconnect cannot delay or invalidate RPCs.
    let observation = if config.observation {
        let observation_name = format!("{}-observe", config.service_name);
        match connect_supervised_plane(&observation_name, &config.noded_url, &config.provenance)
            .await
        {
            Ok(client) => Some(Arc::new(client)),
            Err(error) => {
                let _ = events
                    .send_async(BusBridgeEvent::ObservationConnection {
                        state: BusConnectionState::Fatal,
                        generation: 0,
                    })
                    .await;
                bevy::log::error!("Bus observation connection failed: {error}");
                None
            }
        }
    } else {
        None
    };
    let mut observation_incoming = observation.as_ref().and_then(|client| client.incoming());
    let mut last_observation_gen = observation
        .as_ref()
        .map_or(0, |client| client.connection_generation());
    let mut last_observation_state = observation
        .as_ref()
        .map_or(BusConnectionState::Disconnected, |client| {
            BusConnectionState::from(client.state())
        });
    if observation.is_some() {
        let _ = events
            .send_async(BusBridgeEvent::ObservationConnection {
                state: last_observation_state,
                generation: last_observation_gen,
            })
            .await;
    }

    // One combined generation exposed to Bevy: start at 1 (matching a single
    // fresh connection) and advance when EITHER plane reconnects. Per-plane
    // generations are tracked only to detect those reconnects.
    let mut last_control_gen = control.connection_generation();
    let mut last_telemetry_gen = telemetry.connection_generation();
    let mut combined_generation = 1u64;
    let mut epoch_pending = false;
    let mut last_state = combined_connection_state(control.state(), telemetry.state());
    let _ = events
        .send_async(BusBridgeEvent::Connection {
            state: last_state,
            generation: combined_generation,
        })
        .await;
    let mut state_tick = tokio::time::interval(std::time::Duration::from_millis(50));
    state_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut dropped = 0usize;
    let mut observation_dropped = 0usize;
    let mut calls = JoinSet::new();
    let mut observation_calls = JoinSet::new();
    // Calls accepted while the pipeline is at MAX_INFLIGHT_CALLS, launched
    // as slots free. Intake stays ungated so a saturated call pipeline can
    // never delay Respond or Shutdown behind in-flight call timeouts.
    let mut pending_calls: VecDeque<PendingCall> = VecDeque::new();
    // Outbound answers to inbound app-control requests. A failed respond is
    // deliberately not surfaced: it only happens across a disconnect, where
    // the peer's correlation is already gone.
    let mut responds: JoinSet<()> = JoinSet::new();
    // Fire-and-forget topic writes. Kept separate from responses so a burst of
    // invalidations cannot consume the bounded answer budget.
    let mut publishes: JoinSet<()> = JoinSet::new();

    loop {
        tokio::select! {
            request = requests.recv_async() => {
                match request {
                    Ok(WorkerRequest::Call { request_id, to, command, headers, body }) => {
                        // While the epoch fence is pending, no call may launch
                        // (or park for launch) on the reconnected socket: its
                        // content predates the resync. Fail it back instead.
                        if epoch_pending {
                            if events
                                .send_async(BusBridgeEvent::Reply {
                                    request_id,
                                    result: Err("connection epoch changed".to_string()),
                                })
                                .await
                                .is_err()
                            {
                                break;
                            }
                        } else if calls.len() < MAX_INFLIGHT_CALLS {
                            spawn_call(&mut calls, &control, request_id, to, command, headers, body);
                        } else if pending_calls.len() < MAX_PENDING_CALLS {
                            pending_calls.push_back((request_id, to, command, headers, body));
                        } else if events
                            .send_async(BusBridgeEvent::Reply {
                                request_id,
                                result: Err("Bus bridge call queue is full".to_string()),
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(WorkerRequest::Publish {
                        to,
                        command,
                        headers,
                        body,
                        generation,
                    }) => {
                        if request_generation_is_current(
                            generation,
                            combined_generation,
                            epoch_pending,
                        ) {
                            spawn_publish(
                                &mut publishes,
                                &control,
                                to,
                                command,
                                headers,
                                body,
                            );
                        }
                    }
                    Ok(WorkerRequest::Respond { to, command, id, rc, body, generation }) => {
                        // Only answer under the epoch the request was drained
                        // in: after a reconnect the peer's correlation is gone
                        // and the answer may describe pre-resync state.
                        if request_generation_is_current(
                            generation,
                            combined_generation,
                            epoch_pending,
                        ) {
                            spawn_respond(&mut responds, &control, to, command, id, rc, body);
                        }
                    }
                    Ok(WorkerRequest::Shutdown) | Err(_) => {
                        calls.abort_all();
                        observation_calls.abort_all();
                        publishes.abort_all();
                        if let Some(client) = observation.as_ref() {
                            // The observation request lane is independent from
                            // control, so select ordering cannot guarantee a
                            // just-queued exit stop was read first. Drain any
                            // stop-flush requests explicitly before deregister.
                            for request in observation_requests.try_iter() {
                                if let ObservationRequest::StopFlush { request_id, body } = request {
                                    let result = flush_observation_stop(client, body).await;
                                    let _ = events
                                        .send_async(BusBridgeEvent::ObservationReply {
                                            request_id,
                                            result,
                                        })
                                        .await;
                                }
                            }
                        }
                        // Let in-flight replies finish their socket write
                        // before tearing the connection down: a verb that both
                        // answers and triggers app exit (app.quit) would
                        // otherwise race its own ack. FIFO on `requests`
                        // guarantees a same-frame Respond is drained here before
                        // Shutdown. Bounded so a wedged write can't hang exit.
                        let _ = tokio::time::timeout(RESPOND_DRAIN_GRACE, async {
                            while responds.join_next().await.is_some() {}
                        })
                        .await;
                        // Deregister BOTH planes cleanly.
                        let _ = control.deregister().await;
                        let _ = telemetry.deregister().await;
                        if let Some(client) = observation.as_ref() {
                            let _ = client.deregister().await;
                        }
                        break;
                    }
                }
            }
            request = observation_requests.recv_async() => {
                match request {
                    Ok(ObservationRequest::Call { request_id, to, command, headers, body }) => {
                        if let Some(client) = observation.as_ref() {
                            if observation_calls.len() < MAX_OBSERVE_INFLIGHT_CALLS {
                                spawn_observation_call(
                                    &mut observation_calls,
                                    client,
                                    request_id,
                                    to,
                                    command,
                                    headers,
                                    body,
                                );
                            } else {
                                let _ = events
                                    .send_async(BusBridgeEvent::ObservationReply {
                                        request_id,
                                        result: Err("Bus observation call queue is full".into()),
                                    })
                                    .await;
                            }
                        } else {
                            let _ = events
                                .send_async(BusBridgeEvent::ObservationReply {
                                    request_id,
                                    result: Err("Bus observation connection is unavailable".into()),
                                })
                                .await;
                        }
                    }
                    Ok(ObservationRequest::StopFlush { request_id, body }) => {
                        let result = if let Some(client) = observation.as_ref() {
                            flush_observation_stop(client, body).await
                        } else {
                            Err("Bus observation connection is unavailable".into())
                        };
                        let _ = events
                            .send_async(BusBridgeEvent::ObservationReply { request_id, result })
                            .await;
                    }
                    Err(_) => {}
                }
            }
            completed = observation_calls.join_next(), if !observation_calls.is_empty() => {
                match completed {
                    Some(Ok((request_id, result))) => match events
                            .send_async(BusBridgeEvent::ObservationReply { request_id, result })
                            .await
                    {
                        Ok(()) => {}
                        Err(_) => break,
                    },
                    Some(Err(error)) if !error.is_cancelled() => {
                        bevy::log::error!("Bus observation call task failed: {error}");
                    }
                    _ => {}
                }
            }
            completed = calls.join_next(), if !calls.is_empty() => {
                while !epoch_pending && calls.len() < MAX_INFLIGHT_CALLS {
                    let Some((request_id, to, command, headers, body)) = pending_calls.pop_front()
                    else {
                        break;
                    };
                    spawn_call(&mut calls, &control, request_id, to, command, headers, body);
                }
                match completed {
                    // A result that raced the fence (completed before the
                    // abort, reaped after) is dropped with it: the adapter
                    // discards its request tracking at resync, and forwarding
                    // would resurrect a pre-resync outcome under a new epoch.
                    Some(Ok(_)) if epoch_pending => {}
                    Some(Ok((request_id, result))) => {
                        // A snapshot reply completed after ordinary inbound
                        // overflow must never overtake the overflow notice.
                        // Mixer adapters invalidate such an in-flight snapshot
                        // before they consider its reply.
                        if dropped > 0 {
                            if events
                                .send_async(BusBridgeEvent::DroppedMessages(dropped))
                                .await
                                .is_err()
                            {
                                break;
                            }
                            dropped = 0;
                        }
                        let event_channel_closed = events
                            .send_async(BusBridgeEvent::Reply { request_id, result })
                            .await
                            .is_err();
                        if event_channel_closed {
                            break;
                        }
                    }
                    Some(Err(error)) if !error.is_cancelled() => {
                        let _ = events
                            .send_async(BusBridgeEvent::Fatal(format!("Bus call task failed: {error}")))
                            .await;
                    }
                    _ => {}
                }
            }
            command = incoming.recv() => {
                let Some(command) = command else { break };
                // Epoch fence. If either plane's generation has advanced past
                // the last committed epoch, a reconnect is in flight: the
                // socket this frame came off cannot be attributed to the
                // current combined epoch, and pre-reconnect backlog may be
                // interleaved with post-reconnect frames in this one stream.
                // The moment we detect the change we fence — record the new
                // per-plane generations, mark the epoch pending, and drop the
                // latest-wins mailbox (its buffered frames belong to the
                // pre-reconnect epoch). While pending we forward NOTHING; the
                // state tick commits + announces the new combined epoch once
                // both planes are Connected again, and the adapter's reconnect
                // snapshot re-establishes authoritative state. This makes every
                // enqueued message provably tagged with the epoch of the socket
                // it arrived on — no frame is ever forwarded under a stale or a
                // prematurely-advanced generation.
                let control_gen = control.connection_generation();
                let telemetry_gen = telemetry.connection_generation();
                if control_gen != last_control_gen || telemetry_gen != last_telemetry_gen {
                    last_control_gen = control_gen;
                    last_telemetry_gen = telemetry_gen;
                    if !epoch_pending {
                        epoch_pending = true;
                        enter_epoch_fence(
                            &latest_messages,
                            &semantic_inboxes,
                            &committed_generation,
                            EpochFenceTasks {
                                calls: &mut calls,
                                pending_calls: &mut pending_calls,
                                responds: &mut responds,
                                publishes: &mut publishes,
                            },
                            &events,
                        );
                    }
                }
                if !epoch_pending {
                    let message = BusMessage {
                        connection_generation: combined_generation,
                        from: command.from,
                        command: command.command,
                        body: command.body,
                        headers: command.headers,
                    };
                    #[cfg(feature = "theme")]
                    if message.topic() == Some(crate::theme_sync::THEME_CHANGED_TOPIC) {
                        route_theme_changed_delivery(&semantic_inboxes, message, &wake);
                        continue;
                    }
                    if message.topic().is_some_and(|topic| {
                        config.latest_topics.iter().any(|latest| latest == topic)
                    }) {
                        let topic = message.topic().unwrap().to_string();
                        latest_messages.lock().unwrap().insert(topic, message);
                        wake();
                    } else if messages.try_send(message).is_err() {
                        dropped = dropped.saturating_add(1);
                        // Usually publish the critical notice immediately. If the
                        // bounded event channel is full, the reply branch above
                        // flushes it with backpressure before any later reply.
                        if events
                            .try_send(BusBridgeEvent::DroppedMessages(dropped))
                            .is_ok()
                        {
                            dropped = 0;
                        }
                    }
                }
                // else: fenced — discard until the epoch commits.
            }
            observation_command = async {
                observation_incoming.as_mut().unwrap().recv().await
            }, if observation_incoming.is_some() => {
                let Some(command) = observation_command else {
                    observation_incoming = None;
                    if last_observation_state != BusConnectionState::Disconnected {
                        last_observation_state = BusConnectionState::Disconnected;
                        let _ = events
                            .send_async(BusBridgeEvent::ObservationConnection {
                                state: last_observation_state,
                                generation: last_observation_gen,
                            })
                            .await;
                    }
                    continue;
                };
                let Some(client) = observation.as_ref() else {
                    continue;
                };
                let generation = client.connection_generation();
                let state = BusConnectionState::from(client.state());
                if generation != last_observation_gen || state != last_observation_state {
                    last_observation_gen = generation;
                    last_observation_state = state;
                    let _ = events
                        .send_async(BusBridgeEvent::ObservationConnection {
                            state,
                            generation,
                        })
                        .await;
                }
                if observation_frame_is_dropped(&command) {
                    observation_dropped = observation_dropped.saturating_add(1);
                } else {
                    let message = BusMessage {
                        connection_generation: generation,
                        from: command.from,
                        command: command.command,
                        body: command.body,
                        headers: command.headers,
                    };
                    if observation_messages.try_send(message).is_err() {
                        observation_dropped = observation_dropped.saturating_add(1);
                    }
                }
                if observation_dropped > 0
                    && events
                        .try_send(BusBridgeEvent::ObservationDroppedMessages(
                            observation_dropped,
                        ))
                        .is_ok()
                {
                    observation_dropped = 0;
                }
            }
            // The control plane's inbound stream carries the app-control
            // surface: directed `app.*` requests from other mesh citizens
            // (request/reply traffic — exactly what the split reserves this
            // socket for; telemetry stays on the `-sub` plane). A `None`
            // means the control supervisor stopped (fatal/shutdown), so wind
            // the worker down like a telemetry close.
            control_command = async { control_incoming.as_mut().unwrap().recv().await },
                if control_incoming.is_some() => {
                let Some(command) = control_command else {
                    break;
                };
                // Same epoch fence as the telemetry branch: without this
                // check, a request arriving on a freshly-reconnected control
                // socket BEFORE the 50ms tick notices would be stamped with
                // the closing epoch — mis-attributed in exactly the way the
                // fence exists to prevent.
                let control_gen = control.connection_generation();
                let telemetry_gen = telemetry.connection_generation();
                if control_gen != last_control_gen || telemetry_gen != last_telemetry_gen {
                    last_control_gen = control_gen;
                    last_telemetry_gen = telemetry_gen;
                    if !epoch_pending {
                        epoch_pending = true;
                        enter_epoch_fence(
                            &latest_messages,
                            &semantic_inboxes,
                            &committed_generation,
                            EpochFenceTasks {
                                calls: &mut calls,
                                pending_calls: &mut pending_calls,
                                responds: &mut responds,
                                publishes: &mut publishes,
                            },
                            &events,
                        );
                    }
                }
                // Correlation controls only whether AppPortPlugin can reply;
                // it never decides whether an app verb's side effect executes.
                // This is load-bearing for fire-and-forget menu/action dispatch
                // (`app.transport.*`, `app.song.load`, `app.quit`, etc.).
                // Oversized bodies are refused before they can reach the
                // Bevy-thread JSON parse; an id-less sender simply cannot be
                // told about that refusal.
                if command.body.len() > MAX_INBOUND_BODY_BYTES {
                    if let Some(id) = command.id {
                        let (rc, body) = app_body_too_large_error(&command.command);
                        spawn_respond(
                            &mut responds,
                            &control,
                            command.from,
                            command.command,
                            id,
                            rc,
                            body,
                        );
                    }
                } else if is_inbound_verb(&command.command, &config.inbound_prefixes) {
                    if epoch_pending {
                        // Mid-reconnect the app is resyncing; answer busy
                        // rather than hold a request across an epoch. (An
                        // id-less send is simply dropped.)
                        if let Some(id) = command.id {
                            let (rc, body) = app_delivery_error(&command.command, true);
                            spawn_respond(
                                &mut responds,
                                &control,
                                command.from,
                                command.command,
                                id,
                                rc,
                                body,
                            );
                        }
                    } else {
                        let request = InboundRequest {
                            connection_generation: combined_generation,
                            from: command.from,
                            command: command.command,
                            headers: command.headers,
                            body: command.body,
                            reply_id: command.id,
                        };
                        if let Err(TrySendError::Full(request)) = inbound.try_send(request) {
                            if let Some(id) = request.reply_id {
                                let (rc, body) = app_delivery_error(&request.command, false);
                                spawn_respond(
                                    &mut responds,
                                    &control,
                                    request.from,
                                    request.command,
                                    id,
                                    rc,
                                    body,
                                );
                            }
                        }
                    }
                } else if let Some(id) = command.id {
                    // A good citizen answers what it cannot serve; unknown
                    // id-less sends are discarded.
                    spawn_respond(
                        &mut responds,
                        &control,
                        command.from,
                        command.command,
                        id,
                        10,
                        r#"{"error":"unknown command"}"#.to_string(),
                    );
                }
            }
            // Reap completed responds; outcomes are intentionally ignored
            // (see the JoinSet's declaration).
            _ = responds.join_next(), if !responds.is_empty() => {}
            // Publish tasks log their own timeout/send failures.
            _ = publishes.join_next(), if !publishes.is_empty() => {}
            _ = state_tick.tick() => {
                let state = combined_connection_state(control.state(), telemetry.state());
                let control_gen = control.connection_generation();
                let telemetry_gen = telemetry.connection_generation();
                // A reconnect on EITHER plane advances the shared epoch, but we
                // only COMMIT the advance once both planes are Connected again:
                // an epoch announced mid-outage is useless (a consumer cannot
                // resync while a plane is down) and mirrors the single-connection
                // "Connected && generation changed" gate. This is the same fence
                // as the forward branch, here as a backstop for a reconnect that
                // lands while telemetry is quiet (no frame to trigger the check
                // there): entering the fence clears the latest-wins mailbox so no
                // pre-reconnect frame survives, and forwarding stays suppressed
                // until the commit below.
                if control_gen != last_control_gen || telemetry_gen != last_telemetry_gen {
                    last_control_gen = control_gen;
                    last_telemetry_gen = telemetry_gen;
                    if !epoch_pending {
                        epoch_pending = true;
                        enter_epoch_fence(
                            &latest_messages,
                            &semantic_inboxes,
                            &committed_generation,
                            EpochFenceTasks {
                                calls: &mut calls,
                                pending_calls: &mut pending_calls,
                                responds: &mut responds,
                                publishes: &mut publishes,
                            },
                            &events,
                        );
                    }
                }
                let mut announce = false;
                if state == BusConnectionState::Connected && epoch_pending {
                    // Before lifting the fence, purge EVERY persistent inbound
                    // queue. The outward incoming channels are one mpsc each
                    // that SURVIVES reconnects (supervised.rs: "taken once …
                    // the receiver survives"), so old-socket backlog can still
                    // be buffered here; anything drained after the lift would
                    // be stamped with the NEW generation. The old sockets are
                    // dead by commit time (a generation only advances after a
                    // completed reconnect), so everything arriving after this
                    // purge is provably new-socket.
                    while incoming.try_recv().is_ok() {}
                    if let Some(control_rx) = control_incoming.as_mut() {
                        while control_rx.try_recv().is_ok() {}
                    }
                    latest_messages.lock().unwrap().clear();
                    semantic_inboxes.clear();
                    // Commit + announce the new combined epoch and lift the
                    // fence: subsequent frames forward under this generation.
                    combined_generation = combined_generation.saturating_add(1);
                    committed_generation.store(combined_generation, Ordering::Release);
                    epoch_pending = false;
                    announce = true;
                    bevy::log::info!(
                        generation = combined_generation,
                        "Bus epoch fence committed: both planes up, forwarding resumed"
                    );
                }
                if state != last_state {
                    announce = true;
                }
                if announce {
                    last_state = state;
                    if events.send_async(BusBridgeEvent::Connection {
                        state,
                        generation: combined_generation,
                    }).await.is_err() {
                        break;
                    }
                }
                if dropped > 0 && events.try_send(BusBridgeEvent::DroppedMessages(dropped)).is_ok() {
                    dropped = 0;
                }
                if let Some(client) = observation.as_ref() {
                    let state = BusConnectionState::from(client.state());
                    let generation = client.connection_generation();
                    if state != last_observation_state || generation != last_observation_gen {
                        last_observation_state = state;
                        last_observation_gen = generation;
                        if events
                            .send_async(BusBridgeEvent::ObservationConnection {
                                state,
                                generation,
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
                if observation_dropped > 0
                    && events
                        .try_send(BusBridgeEvent::ObservationDroppedMessages(
                            observation_dropped,
                        ))
                        .is_ok()
                {
                    observation_dropped = 0;
                }
            }
        }
    }
}

async fn connect_supervised_plane(
    service_name: &str,
    noded_url: &str,
    provenance: &RegisterProvenance,
) -> Result<SupervisedClient, SupervisedError> {
    SupervisedClient::connect_supervised_with_provenance(
        service_name,
        noded_url,
        Some(provenance.clone()),
    )
    .await
}

fn is_app_verb(command: &str) -> bool {
    command.starts_with("app.")
        || matches!(
            command,
            "action.invoke" | "actions.list" | "actions.describe"
        )
}

fn is_inbound_verb(command: &str, prefixes: &[String]) -> bool {
    is_app_verb(command) || prefixes.iter().any(|prefix| command.starts_with(prefix))
}

fn is_observe_event(command: &str) -> bool {
    command == "noded.observe.event"
}

fn observation_frame_is_dropped(command: &cosmix_client::IncomingCommand) -> bool {
    !is_observe_event(&command.command)
        || observation_frame_size(command) > MAX_OBSERVATION_MESSAGE_BYTES
}

fn observation_frame_size(command: &cosmix_client::IncomingCommand) -> usize {
    command
        .body
        .len()
        .saturating_add(command.from.len())
        .saturating_add(command.command.len())
        .saturating_add(
            command
                .headers
                .iter()
                .map(|(name, value)| name.len().saturating_add(value.len()))
                .sum::<usize>(),
        )
}

fn is_action_verb(command: &str) -> bool {
    matches!(
        command,
        "action.invoke" | "actions.list" | "actions.describe"
    )
}

fn app_delivery_error(command: &str, resyncing: bool) -> (u8, String) {
    if is_action_verb(command) {
        let id = if resyncing {
            "resyncing"
        } else {
            "delivery_queue_full"
        };
        let message = if resyncing {
            "action port is resyncing after reconnect"
        } else {
            "action delivery queue is full"
        };
        return (
            10,
            format!(r#"{{"error":{{"id":"{id}","message":"{message}"}}}}"#),
        );
    }
    if resyncing {
        (11, r#"{"error":"resyncing after reconnect"}"#.to_string())
    } else {
        (11, r#"{"error":"busy"}"#.to_string())
    }
}

fn app_body_too_large_error(command: &str) -> (u8, String) {
    if is_action_verb(command) {
        return (
            10,
            r#"{"error":{"id":"body_too_large","message":"action request body is too large"}}"#
                .to_string(),
        );
    }
    (10, r#"{"error":"body too large"}"#.to_string())
}

/// A queued `WorkerRequest::Call`: `(request_id, to, command, headers, body)`.
type PendingCall = (u64, String, String, BTreeMap<String, String>, String);

struct EpochFenceTasks<'a> {
    calls: &'a mut JoinSet<(u64, Result<BusReply, String>)>,
    pending_calls: &'a mut VecDeque<PendingCall>,
    responds: &'a mut JoinSet<()>,
    publishes: &'a mut JoinSet<()>,
}

/// Everything entering the epoch fence tears down, in one place (three call
/// sites: the telemetry branch, the control branch, and the tick backstop).
///
/// * latest-wins mailbox — its frames belong to the closing epoch;
/// * semantic inboxes — their events belong to the closing epoch;
/// * in-flight responses — composed under the closing epoch;
/// * in-flight publishes — stamped under the closing epoch;
/// * in-flight AND parked calls — an old revisioned write must not launch on
///   the reconnected socket after the adapter has resynced (parked calls fail
///   back to Bevy; in-flight aborts surface as cancelled joins, filtered);
/// * the shared committed generation — [`GENERATION_FENCED`] keeps the Bevy
///   side from executing ANY queued inbound request until the commit.
fn enter_epoch_fence(
    latest_messages: &Mutex<HashMap<String, BusMessage>>,
    semantic_inboxes: &SemanticInboxes,
    committed_generation: &AtomicU64,
    tasks: EpochFenceTasks<'_>,
    events: &WakeSender<BusBridgeEvent>,
) {
    let EpochFenceTasks {
        calls,
        pending_calls,
        responds,
        publishes,
    } = tasks;
    // One structured event per fence entry (never per frame) — the fence's
    // internals have no other log surface, and the counts answer "what was
    // torn down" for a post-incident read (e.g. failed_pending > 0 means a
    // call was parked at the drop and failed back).
    bevy::log::warn!(
        cleared_messages = latest_messages.lock().unwrap().len(),
        inflight_calls = calls.len(),
        inflight_responds = responds.len(),
        inflight_publishes = publishes.len(),
        failed_pending = pending_calls.len(),
        "Bus epoch fence entered: telemetry fenced, in-flight work torn down"
    );
    latest_messages.lock().unwrap().clear();
    semantic_inboxes.clear();
    committed_generation.store(GENERATION_FENCED, Ordering::Release);
    // REPLACE the JoinSets rather than abort_all(): dropping a JoinSet aborts
    // its tasks AND discards already-completed buffered results — a fast
    // reconnect can enter and commit the fence within one tick, and a
    // pre-abort completion still buffered would otherwise be reaped as a
    // current reply after the fence lifts.
    *responds = JoinSet::new();
    *publishes = JoinSet::new();
    *calls = JoinSet::new();
    while let Some((request_id, ..)) = pending_calls.pop_front() {
        // Best-effort: a full event channel during a reconnect just means
        // the adapter's resync (which discards its request tracking anyway)
        // arrives before the failure notice would have.
        let _ = events.try_send(BusBridgeEvent::Reply {
            request_id,
            result: Err("connection epoch changed".to_string()),
        });
    }
}

fn request_generation_is_current(
    generation: u64,
    committed_generation: u64,
    epoch_pending: bool,
) -> bool {
    !epoch_pending && generation != GENERATION_FENCED && generation == committed_generation
}

/// Launch one RPC on the control plane. Callers enforce
/// [`MAX_INFLIGHT_CALLS`] before spawning.
fn spawn_call(
    calls: &mut JoinSet<(u64, Result<BusReply, String>)>,
    control: &Arc<SupervisedClient>,
    request_id: u64,
    to: String,
    command: String,
    headers: BTreeMap<String, String>,
    body: String,
) {
    let client = Arc::clone(control);
    calls.spawn(async move {
        let result = client
            .call_with_headers_raw(&to, &command, &headers, &body)
            .await
            .map(|(rc, body, result)| BusReply { rc, body, result })
            .map_err(|error| error.to_string());
        (request_id, result)
    });
}

fn spawn_observation_call(
    calls: &mut JoinSet<(u64, Result<BusReply, String>)>,
    observation: &Arc<SupervisedClient>,
    request_id: u64,
    to: String,
    command: String,
    headers: BTreeMap<String, String>,
    body: String,
) {
    let client = Arc::clone(observation);
    calls.spawn(async move {
        let result = client
            .call_with_headers_raw(&to, &command, &headers, &body)
            .await
            .map(|(rc, body, result)| BusReply { rc, body, result })
            .map_err(|error| error.to_string());
        (request_id, result)
    });
}

async fn flush_observation_stop(
    observation: &Arc<SupervisedClient>,
    body: String,
) -> Result<BusReply, String> {
    tokio::time::timeout(
        OBSERVATION_STOP_FLUSH,
        observation.call_with_headers_raw("noded", "noded.observe.stop", &BTreeMap::new(), &body),
    )
    .await
    .map_err(|_| "observation stop flush timed out".to_string())?
    .map(|(rc, body, result)| BusReply { rc, body, result })
    .map_err(|error| error.to_string())
}

/// Launch one fire-and-forget topic write without blocking the worker select
/// loop. Epoch fences replace this JoinSet, shutdown aborts it, and the timeout
/// bounds a live socket whose sink lock or write remains backpressured.
fn spawn_publish(
    publishes: &mut JoinSet<()>,
    control: &Arc<SupervisedClient>,
    to: String,
    command: String,
    headers: BTreeMap<String, String>,
    body: String,
) {
    if publishes.len() >= MAX_INFLIGHT_PUBLISHES {
        bevy::log::warn!("Bus topic publish dropped: publish task limit reached");
        return;
    }
    let client = Arc::clone(control);
    publishes.spawn(async move {
        match tokio::time::timeout(
            PUBLISH_WRITE_TIMEOUT,
            client.send_with_headers(&to, &command, &headers, &body),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => bevy::log::warn!("Bus topic publish failed: {error}"),
            Err(_) => bevy::log::warn!("Bus topic publish timed out"),
        }
    });
}

/// Answer an inbound request on the control connection without blocking the
/// worker select loop. Errors are dropped: a respond only fails across a
/// disconnect, where the peer's correlation is already dead. Past
/// [`MAX_INFLIGHT_RESPONDS`] the reply is dropped instead of spawned (the
/// peer times out) so a request flood against a blocked socket cannot grow
/// the task set without bound.
fn spawn_respond(
    responds: &mut JoinSet<()>,
    control: &Arc<SupervisedClient>,
    to: String,
    command: String,
    id: String,
    rc: u8,
    body: String,
) {
    if responds.len() >= MAX_INFLIGHT_RESPONDS {
        return;
    }
    let client = Arc::clone(control);
    responds.spawn(async move {
        let _ = client
            .respond_parts(&to, &command, Some(&id), rc, &body)
            .await;
    });
}

async fn install_subscriptions(client: &SupervisedClient, topics: &[String]) -> Result<(), String> {
    for topic in topics {
        let mut headers = BTreeMap::new();
        headers.insert("name".to_string(), topic.clone());
        loop {
            let failed_generation = client.connection_generation();
            match client
                .call_with_headers_raw("noded", "topic.subscribe", &headers, "")
                .await
            {
                Ok((0, _, _)) => {
                    client.subscription_registry().record(topic);
                    break;
                }
                Ok((rc, body, error)) => {
                    let detail = error.filter(|value| !value.is_empty()).unwrap_or(body);
                    return Err(format!("subscribe {topic} rejected (RC {rc}): {detail}"));
                }
                Err(SupervisedError::Disconnected | SupervisedError::Transport(_)) => {
                    wait_for_connection_after(client, failed_generation).await?;
                }
                Err(error) => return Err(format!("subscribe {topic}: {error}")),
            }
        }
    }
    Ok(())
}

async fn wait_for_connection_after(
    client: &SupervisedClient,
    failed_generation: u64,
) -> Result<(), String> {
    loop {
        match client.state() {
            ConnState::Connected if client.connection_generation() != failed_generation => {
                return Ok(());
            }
            // A transport call can fail without taking down an otherwise
            // healthy socket. Retry it after a short backoff; broker-level
            // rejections were already returned above as an explicit RC.
            ConnState::Connected => {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                return Ok(());
            }
            ConnState::Fatal | ConnState::ShuttingDown => {
                return Err("Bus client stopped while installing subscriptions".into());
            }
            _ => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }
}

/// The Bus implementation of the mixer transport seam: the two-connection
/// worker above, presented through [`crate::transport::MixerTransport`]. All
/// mixer-wire codecs (JSON write/snapshot bodies, base64 A.6 meter frames,
/// changed/applied JSON events) live HERE — the pipeline above the seam never
/// sees a byte of wire encoding. Behavior is the pre-seam pump's, verbatim:
/// same commands, same decode-error strings, same drain order and caps.
#[cfg(feature = "mixer")]
mod mixer_transport {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use base64::Engine as _;
    use cosmix_mixer_schema::{
        DspApplied, MeterFrame, MixerSnapshotResponse, WriteRequest, WriteResponse,
    };
    use flume::{Receiver, Sender};

    use super::{BusBridge, BusBridgeEvent, BusMessage, WorkerRequest};
    use crate::mixer::{APPLIED_TOPIC, CHANGED_TOPIC, METERS_TOPIC, TRANSPORT_POSITION_PATH};
    use crate::transport::{
        ChangedEvent, MixerConnectionState, MixerTransport, TransportEvent, TransportMessage,
        TransportReply,
    };

    impl From<super::BusConnectionState> for MixerConnectionState {
        fn from(value: super::BusConnectionState) -> Self {
            match value {
                super::BusConnectionState::Connecting => Self::Connecting,
                super::BusConnectionState::Connected => Self::Connected,
                super::BusConnectionState::Disconnected => Self::Disconnected,
                super::BusConnectionState::ShuttingDown => Self::ShuttingDown,
                super::BusConnectionState::Fatal => Self::Fatal,
            }
        }
    }

    /// Which issue method produced a request id — the decode selector for its
    /// eventual reply. Entries for calls torn down by an epoch fence never get
    /// a reply, so the map is cleared on every committed reconnect (mirroring
    /// the pipeline's own `pending.clear()`), and ids are never reused.
    enum RequestKind {
        Write,
        Snapshot,
        Position,
    }

    /// [`MixerTransport`] over the Bus bridge worker. Holds clones of the same
    /// channel handles the [`BusBridge`] resource holds; this transport is the
    /// SOLE drainer of the event/message channels (the bridge resource keeps
    /// serving the `app_control` inbound surface, a separate channel).
    pub struct BusTransport {
        requests: Sender<WorkerRequest>,
        events: Receiver<BusBridgeEvent>,
        messages: Receiver<BusMessage>,
        latest_messages: Arc<Mutex<HashMap<String, BusMessage>>>,
        max_messages_per_frame: usize,
        service_name: String,
        kinds: HashMap<u64, RequestKind>,
    }

    impl BusBridge {
        /// Build the mixer-transport view of this bridge, transferring
        /// EXCLUSIVE ownership of the telemetry streams to it: from this call
        /// on, the bridge's own `drain_events`/`drain_messages`/
        /// `drain_latest_messages`/`discard_messages` panic instead of
        /// silently competing (flume receivers share one queue — a second
        /// drainer would steal replies and wedge in-flight paths). Panics if
        /// called twice. The `app_control` inbound surface is a separate
        /// channel and stays on the bridge.
        pub fn mixer_transport(&mut self) -> BusTransport {
            assert!(
                !self.telemetry_taken,
                "BusBridge::mixer_transport called twice — the transport owns the telemetry streams"
            );
            self.telemetry_taken = true;
            BusTransport {
                requests: self.requests.clone(),
                events: self.events.clone(),
                messages: self.messages.clone(),
                latest_messages: self.latest_messages.clone(),
                max_messages_per_frame: self.max_messages_per_frame,
                service_name: self.service_name.clone(),
                kinds: HashMap::new(),
            }
        }
    }

    impl BusTransport {
        fn try_call(&self, request_id: u64, command: &str, body: String) -> Result<(), String> {
            self.requests
                .try_send(WorkerRequest::Call {
                    request_id,
                    to: "musicd".to_string(),
                    command: command.to_string(),
                    headers: BTreeMap::new(),
                    body,
                })
                .map_err(|error| match error {
                    flume::TrySendError::Full(_) => {
                        "Bus bridge outbound channel is full".to_string()
                    }
                    flume::TrySendError::Disconnected(_) => {
                        "Bus bridge worker has stopped".to_string()
                    }
                })
        }

        fn decode_reply(
            kind: RequestKind,
            reply: super::BusReply,
        ) -> Result<TransportReply, String> {
            match kind {
                RequestKind::Snapshot => {
                    if reply.rc != 0 {
                        return Err(format!(
                            "snapshot rejected (RC {}): {}",
                            reply.rc, reply.body
                        ));
                    }
                    serde_json::from_str::<MixerSnapshotResponse>(&reply.body)
                        .map(TransportReply::Snapshot)
                        .map_err(|error| format!("snapshot decode: {error}"))
                }
                // The write outcome (accepted/rejected/busy) is encoded in the
                // body; rc is deliberately ignored, as the pre-seam pump did.
                RequestKind::Write => Ok(TransportReply::Write(
                    serde_json::from_str::<WriteResponse>(&reply.body)
                        .map_err(|error| error.to_string()),
                )),
                RequestKind::Position => {
                    if reply.rc != 0 {
                        return Err(format!("position poll rejected (RC {})", reply.rc));
                    }
                    serde_json::from_str::<f64>(reply.body.trim())
                        .map(TransportReply::Position)
                        .map_err(|error| format!("position decode: {error}"))
                }
            }
        }

        fn decode_message(message: BusMessage) -> Option<TransportMessage> {
            let generation = message.connection_generation;
            match message.topic() {
                Some(METERS_TOPIC) => Some(
                    match base64::engine::general_purpose::STANDARD
                        .decode(message.body.trim())
                        .map_err(|error| format!("meter base64 decode: {error}"))
                        .and_then(|bytes| {
                            MeterFrame::try_from(bytes.as_slice())
                                .map_err(|error| format!("meter frame decode: {error}"))
                        }) {
                        Ok(frame) => TransportMessage::Meter { generation, frame },
                        Err(error) => TransportMessage::Malformed { generation, error },
                    },
                ),
                Some(CHANGED_TOPIC) => {
                    Some(match serde_json::from_str::<ChangedEvent>(&message.body) {
                        Ok(event) => TransportMessage::Changed { generation, event },
                        Err(error) => TransportMessage::Malformed {
                            generation,
                            error: format!("mixer.changed decode: {error}"),
                        },
                    })
                }
                Some(APPLIED_TOPIC) => {
                    Some(match serde_json::from_str::<DspApplied>(&message.body) {
                        Ok(applied) => TransportMessage::Applied {
                            generation,
                            applied,
                        },
                        Err(error) => TransportMessage::Malformed {
                            generation,
                            error: format!("mixer.applied decode: {error}"),
                        },
                    })
                }
                _ => None,
            }
        }
    }

    impl MixerTransport for BusTransport {
        fn service_name(&self) -> &str {
            &self.service_name
        }

        fn issue_write(&mut self, request_id: u64, request: &WriteRequest) -> Result<(), String> {
            let body = serde_json::to_string(request).map_err(|error| error.to_string())?;
            self.try_call(request_id, "musicd.props.set", body)?;
            self.kinds.insert(request_id, RequestKind::Write);
            Ok(())
        }

        fn request_snapshot(&mut self, request_id: u64) -> Result<(), String> {
            self.try_call(request_id, "musicd.mixer.snapshot", String::new())?;
            self.kinds.insert(request_id, RequestKind::Snapshot);
            Ok(())
        }

        fn request_position(&mut self, request_id: u64) -> Result<(), String> {
            let body = serde_json::json!({ "path": TRANSPORT_POSITION_PATH }).to_string();
            self.try_call(request_id, "musicd.props.get", body)?;
            self.kinds.insert(request_id, RequestKind::Position);
            Ok(())
        }

        fn poll_events(&mut self, out: &mut Vec<TransportEvent>) {
            out.clear();
            for event in self.events.try_iter() {
                match event {
                    BusBridgeEvent::Connection { state, generation } => {
                        if state == super::BusConnectionState::Connected {
                            // Calls torn down by the epoch fence never reply;
                            // drop their decode selectors (ids are never
                            // reused, so this can only forget dead entries).
                            // A same-generation authority reset (the pipeline
                            // abandoning pending without a reconnect) leaves
                            // selectors behind until their replies arrive or
                            // time out — bounded and misattribution-free, so
                            // deliberately not swept there.
                            self.kinds.clear();
                        }
                        out.push(TransportEvent::Connection {
                            state: state.into(),
                            generation,
                        });
                    }
                    BusBridgeEvent::Reply { request_id, result } => {
                        let Some(kind) = self.kinds.remove(&request_id) else {
                            continue;
                        };
                        // Completion observed at drain (the worker finished on
                        // another thread; this per-frame drain is the
                        // observation point) — stamped BEFORE decoding, so a
                        // batch with several replies never charges one write's
                        // ack with its neighbours' (or its own) decode cost.
                        // Pre-seam the stamp sat at the pump's reconcile, also
                        // before body parse; this is the same instant minus
                        // the decode of earlier events in the batch.
                        let observed = Instant::now();
                        let result = result.and_then(|reply| Self::decode_reply(kind, reply));
                        out.push(TransportEvent::Reply {
                            request_id,
                            result,
                            completed_at: Some(observed),
                        });
                    }
                    BusBridgeEvent::DroppedMessages(count) => {
                        out.push(TransportEvent::DroppedMessages(count));
                    }
                    BusBridgeEvent::ObservationConnection { .. }
                    | BusBridgeEvent::ObservationReply { .. }
                    | BusBridgeEvent::ObservationDroppedMessages(_) => {}
                    BusBridgeEvent::Fatal(error) => {
                        out.push(TransportEvent::Fatal(error));
                    }
                }
            }
        }

        fn poll_messages(&mut self, out: &mut Vec<TransportMessage>) {
            out.clear();
            // Ordinary queue first (capped per frame), then latest-wins
            // (meters) appended — the pre-seam pump's exact drain order.
            // Decode happens HERE, after the pump has already reconciled this
            // frame's replies, so decode cost never inflates a measured ack.
            let ordinary: Vec<BusMessage> = self
                .messages
                .try_iter()
                .take(self.max_messages_per_frame)
                .collect();
            let latest: Vec<BusMessage> = {
                let mut guard = self.latest_messages.lock().unwrap();
                std::mem::take(&mut *guard).into_values().collect()
            };
            for message in ordinary.into_iter().chain(latest) {
                if let Some(decoded) = Self::decode_message(message) {
                    out.push(decoded);
                }
            }
        }

        fn discard_backlog(&mut self) {
            self.messages.try_iter().for_each(drop);
            self.latest_messages.lock().unwrap().clear();
        }
    }
}

#[cfg(feature = "mixer")]
pub use mixer_transport::BusTransport;

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio_tungstenite::tungstenite::Message;

    #[cfg(feature = "theme")]
    #[test]
    fn only_valid_local_theme_delivery_replaces_inbox_and_wakes() {
        let inboxes = SemanticInboxes::default();
        let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let wake_count = wakes.clone();
        let wake: WorkerWake = Arc::new(move || {
            wake_count.fetch_add(1, Ordering::SeqCst);
        });
        let valid = BusMessage {
            connection_generation: 1,
            from: "peer-sub".into(),
            command: crate::theme_sync::THEME_CHANGED_TOPIC.into(),
            body: r#"{"scheme":"forest","mode":"dark"}"#.into(),
            headers: BTreeMap::from([
                (
                    "topic".into(),
                    crate::theme_sync::THEME_CHANGED_TOPIC.into(),
                ),
                ("broker_origin".into(), "local".into()),
            ]),
        };
        assert!(route_theme_changed_delivery(&inboxes, valid.clone(), &wake));
        let mut malformed = valid.clone();
        malformed.body = "{not-json".into();
        assert!(!route_theme_changed_delivery(&inboxes, malformed, &wake));
        let mut mesh_origin = valid.clone();
        mesh_origin
            .headers
            .insert("broker_origin".into(), "mesh".into());
        assert!(!route_theme_changed_delivery(&inboxes, mesh_origin, &wake));

        assert_eq!(wakes.load(Ordering::SeqCst), 1);
        assert_eq!(inboxes.drain_theme_changed().unwrap().body, valid.body);
    }

    fn register_reply(request: &cosmix_bus::bus::BusMessage) -> String {
        let mut reply = cosmix_bus::bus::BusMessage::new()
            .with_header("type", "response")
            .with_header("command", "noded.register")
            .with_header("from", "noded")
            .with_header("rc", "0");
        if let Some(id) = request.get("id") {
            reply = reply.with_header("id", id);
        }
        reply.to_wire()
    }

    #[test]
    fn observation_cap_counts_oversized_idless_events_as_dropped() {
        let oversized = cosmix_client::IncomingCommand {
            from: "noded".into(),
            command: "noded.observe.event".into(),
            id: None,
            args: serde_json::Value::Null,
            body: "x".repeat(MAX_OBSERVATION_MESSAGE_BYTES + 1),
            headers: BTreeMap::new(),
        };
        assert!(observation_frame_is_dropped(&oversized));

        // `\u0000` is the six-byte JSON spelling of one captured control
        // byte. Fill a syntactically valid worst-case-escaped event to within
        // one escape unit of the cap. A contract-maximum 64 KiB raw capture is
        // smaller than this boundary case but already far beyond the old
        // 96 KiB limit.
        let mut worst_case_escaped = cosmix_client::IncomingCommand {
            from: "noded".into(),
            command: "noded.observe.event".into(),
            id: None,
            args: serde_json::Value::Null,
            body: String::new(),
            headers: BTreeMap::from([("subscription_id".into(), "observe-1".into())]),
        };
        let fixed = observation_frame_size(&worst_case_escaped);
        let prefix = r#"{"payload":{"body":""#;
        let suffix = r#""}}"#;
        let escape_count = (MAX_OBSERVATION_MESSAGE_BYTES
            .saturating_sub(fixed)
            .saturating_sub(prefix.len())
            .saturating_sub(suffix.len()))
            / 6;
        worst_case_escaped.body = format!("{prefix}{}{suffix}", r"\u0000".repeat(escape_count));
        let size = observation_frame_size(&worst_case_escaped);
        assert!(size <= MAX_OBSERVATION_MESSAGE_BYTES);
        assert!(MAX_OBSERVATION_MESSAGE_BYTES - size < 6);
        assert!(!observation_frame_is_dropped(&worst_case_escaped));
    }

    #[test]
    fn supervised_connect_registration_carries_process_provenance() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build test runtime");
        runtime.block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind test broker");
            let url = format!("ws://{}/ws", listener.local_addr().unwrap());
            let (body_tx, body_rx) = oneshot::channel();
            let (release_tx, release_rx) = oneshot::channel();
            let broker = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.expect("accept bridge");
                let socket = tokio_tungstenite::accept_async(stream)
                    .await
                    .expect("accept websocket");
                let (mut sink, mut source) = socket.split();
                let text = source
                    .next()
                    .await
                    .expect("registration frame")
                    .expect("valid registration frame")
                    .into_text()
                    .expect("text registration");
                let request = cosmix_bus::bus::parse(&text).expect("parse noded.register request");
                assert_eq!(request.get("command"), Some("noded.register"));
                let _ = body_tx.send(request.body.clone());
                sink.send(Message::Text(register_reply(&request).into()))
                    .await
                    .expect("answer registration");
                let _ = release_rx.await;
            });

            let config = BusBridgeConfig::new("ctk-provenance-test", &url);
            let client = connect_supervised_plane(
                &config.service_name,
                &config.noded_url,
                &config.provenance,
            )
            .await
            .expect("supervised connect");
            let sent: RegisterProvenance =
                serde_json::from_str(&body_rx.await.expect("captured registration body"))
                    .expect("registration provenance JSON");
            assert_eq!(sent.pid, Some(std::process::id()));
            assert!(
                sent.started_at
                    .as_deref()
                    .is_some_and(|value| !value.is_empty()),
                "started_at must be present"
            );
            assert!(
                sent.binary
                    .as_deref()
                    .is_some_and(|value| !value.is_empty()),
                "binary must be present"
            );
            assert_eq!(sent.version.as_deref(), Some(env!("CARGO_PKG_VERSION")));
            assert!(sent.git_sha.is_some());
            assert!(sent.build_time.is_some());
            assert_eq!(sent, config.provenance);

            client.shutdown().await;
            let _ = release_tx.send(());
            broker.await.expect("test broker exits");
        });
    }

    fn request(generation: u64, command: &str) -> InboundRequest {
        InboundRequest {
            connection_generation: generation,
            from: "tester".into(),
            command: command.into(),
            headers: BTreeMap::new(),
            body: String::new(),
            reply_id: Some("7".into()),
        }
    }

    /// An [`BusBridge`] wired to test-held channel ends — no worker thread.
    fn test_bridge(
        committed: u64,
    ) -> (
        BusBridge,
        Sender<InboundRequest>,
        Receiver<WorkerRequest>,
        Arc<AtomicU64>,
    ) {
        let (request_tx, request_rx) = flume::bounded(8);
        let (observation_request_tx, _observation_request_rx) = flume::bounded(4);
        let (_event_tx, event_rx) = flume::bounded::<BusBridgeEvent>(8);
        let (_message_tx, message_rx) = flume::bounded::<BusMessage>(8);
        let (_observation_message_tx, observation_message_rx) = flume::bounded::<BusMessage>(8);
        let (inbound_tx, inbound_rx) = flume::bounded(8);
        let (shutdown_done_tx, shutdown_done_rx) = flume::bounded(1);
        let _ = shutdown_done_tx.send(());
        let committed_generation = Arc::new(AtomicU64::new(committed));
        let semantic_inboxes = Arc::new(SemanticInboxes::default());
        let bridge = BusBridge {
            requests: request_tx,
            observation_requests: observation_request_tx,
            events: event_rx,
            messages: message_rx,
            observation_messages: observation_message_rx,
            inbound: inbound_rx,
            committed_generation: committed_generation.clone(),
            latest_messages: Arc::new(Mutex::new(HashMap::new())),
            semantic_inboxes,
            wake: no_op_wake(),
            max_messages_per_frame: 8,
            max_observation_messages_per_frame: 8,
            service_name: "test-app".into(),
            shutdown_done: shutdown_done_rx,
            telemetry_taken: false,
        };
        (bridge, inbound_tx, request_rx, committed_generation)
    }

    /// The fence's tear-down contract, the part every Codex round leaned on:
    /// one call empties the mailbox, publishes the fenced sentinel, replaces
    /// (not merely aborts) every task JoinSet so no pre-fence completion stays
    /// reapable, and fails every parked call back to Bevy.
    #[test]
    fn epoch_fence_tears_down_every_in_flight_surface() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build test runtime");
        runtime.block_on(async {
            let latest_messages = Mutex::new(HashMap::from([(
                "musicd.mixer.meters".to_string(),
                BusMessage {
                    connection_generation: 3,
                    from: "musicd".into(),
                    command: "publish".into(),
                    body: String::new(),
                    headers: BTreeMap::new(),
                },
            )]));
            let committed_generation = AtomicU64::new(3);
            let mut calls: JoinSet<(u64, Result<BusReply, String>)> = JoinSet::new();
            // One never-resolving in-flight call and one ALREADY-COMPLETED
            // one: the completed result must be discarded by the fence too
            // (a fast enter+commit would otherwise reap it as current).
            calls.spawn(async { std::future::pending().await });
            calls.spawn(async { (41, Err("done before the fence".to_string())) });
            let mut responds: JoinSet<()> = JoinSet::new();
            responds.spawn(async { std::future::pending().await });
            let mut publishes: JoinSet<()> = JoinSet::new();
            publishes.spawn(async { std::future::pending().await });
            let pre_fence_publish_generation = 3;
            let mut pending_calls: VecDeque<PendingCall> = VecDeque::from([
                (
                    91,
                    "musicd".into(),
                    "musicd.props.set".into(),
                    BTreeMap::new(),
                    "{}".into(),
                ),
                (
                    92,
                    "musicd".into(),
                    "musicd.props.get".into(),
                    BTreeMap::new(),
                    String::new(),
                ),
            ]);
            let (event_tx, event_rx) = flume::bounded(8);
            let event_tx = WakeSender::new(event_tx, no_op_wake());
            let semantic_inboxes = SemanticInboxes::default();
            #[cfg(feature = "theme")]
            semantic_inboxes.enqueue_theme_changed(
                BusMessage {
                    connection_generation: 3,
                    from: "peer".into(),
                    command: crate::theme_sync::THEME_CHANGED_TOPIC.into(),
                    body: r#"{"scheme":"forest","mode":"light"}"#.into(),
                    headers: BTreeMap::from([
                        (
                            "topic".into(),
                            crate::theme_sync::THEME_CHANGED_TOPIC.into(),
                        ),
                        ("broker_origin".into(), "local".into()),
                    ]),
                },
                &no_op_wake(),
            );

            enter_epoch_fence(
                &latest_messages,
                &semantic_inboxes,
                &committed_generation,
                EpochFenceTasks {
                    calls: &mut calls,
                    pending_calls: &mut pending_calls,
                    responds: &mut responds,
                    publishes: &mut publishes,
                },
                &event_tx,
            );

            assert!(latest_messages.lock().unwrap().is_empty());
            #[cfg(feature = "theme")]
            assert!(semantic_inboxes.drain_theme_changed().is_none());
            assert_eq!(
                committed_generation.load(Ordering::Acquire),
                GENERATION_FENCED
            );
            assert!(calls.is_empty(), "calls JoinSet was replaced empty");
            assert!(responds.is_empty(), "responds JoinSet was replaced empty");
            assert!(publishes.is_empty(), "publishes JoinSet was replaced empty");
            assert!(pending_calls.is_empty());
            assert!(
                !request_generation_is_current(
                    pre_fence_publish_generation,
                    GENERATION_FENCED,
                    true,
                ),
                "the fence drops a queued publish"
            );
            assert!(
                !request_generation_is_current(pre_fence_publish_generation, 4, false),
                "lifting the fence must not send an old-generation publish"
            );
            let failed: Vec<u64> = event_rx
                .try_iter()
                .map(|event| match event {
                    BusBridgeEvent::Reply { request_id, result } => {
                        assert_eq!(result, Err("connection epoch changed".to_string()));
                        request_id
                    }
                    other => panic!("unexpected event: {other:?}"),
                })
                .collect();
            assert_eq!(failed, [91, 92]);
        });
    }

    #[test]
    fn publish_is_stamped_with_the_current_committed_generation() {
        let (bridge, _inbound_tx, requests, committed) = test_bridge(7);
        bridge
            .try_publish_topic(
                "theme.changed",
                false,
                "---\ncommand: theme.changed\n---\n{}",
            )
            .unwrap();
        let request = requests.try_recv().unwrap();
        let WorkerRequest::Publish { generation, .. } = request else {
            panic!("expected publish request");
        };
        assert_eq!(generation, 7);

        committed.store(GENERATION_FENCED, Ordering::Release);
        bridge
            .try_publish_topic(
                "theme.changed",
                false,
                "---\ncommand: theme.changed\n---\n{}",
            )
            .unwrap();
        let request = requests.try_recv().unwrap();
        let WorkerRequest::Publish { generation, .. } = request else {
            panic!("expected publish request");
        };
        assert_eq!(generation, GENERATION_FENCED);
        assert!(!request_generation_is_current(generation, 8, false));
    }

    /// `drain_inbound` executes nothing stamped outside the committed epoch —
    /// and consumes it, so a stale request never survives to a later drain.
    #[test]
    fn inbound_drain_filters_stale_and_fenced_epochs() {
        let (bridge, inbound_tx, _request_rx, committed) = test_bridge(2);
        inbound_tx.send(request(1, "app.controls.set")).unwrap();
        inbound_tx.send(request(2, "app.describe")).unwrap();
        inbound_tx.send(request(1, "app.controls.get")).unwrap();

        let drained: Vec<String> = bridge
            .drain_inbound()
            .map(|request| request.command)
            .collect();
        assert_eq!(drained, ["app.describe"]);

        // While fenced, even correctly-stamped requests are dropped: no real
        // epoch equals the sentinel, so the filter rejects everything...
        inbound_tx.send(request(2, "app.controls.set")).unwrap();
        committed.store(GENERATION_FENCED, Ordering::Release);
        assert_eq!(bridge.drain_inbound().count(), 0);

        // ...and dropping is consumption — lifting the fence does not
        // resurrect what was queued across it.
        committed.store(2, Ordering::Release);
        assert_eq!(bridge.drain_inbound().count(), 0);
    }

    /// Fire-and-forget requests never produce a response; correlated ones
    /// carry their stamped epoch so the worker can drop a stale answer.
    #[test]
    fn try_respond_is_correlation_gated_and_epoch_stamped() {
        let (bridge, _inbound_tx, request_rx, _committed) = test_bridge(2);

        let mut fire_and_forget = request(2, "app.controls.set");
        fire_and_forget.reply_id = None;
        bridge.try_respond(&fire_and_forget, 0, "{}").unwrap();
        assert!(request_rx.try_recv().is_err(), "no response without an id");

        bridge
            .try_respond(&request(2, "app.controls.get"), 0, r#"{"value":1}"#)
            .unwrap();
        match request_rx.try_recv().unwrap() {
            WorkerRequest::Respond {
                to,
                command,
                id,
                rc,
                body,
                generation,
            } => {
                assert_eq!(to, "tester");
                assert_eq!(command, "app.controls.get");
                assert_eq!(id, "7");
                assert_eq!(rc, 0);
                assert_eq!(body, r#"{"value":1}"#);
                assert_eq!(generation, 2);
            }
            other => panic!("unexpected worker request: {other:?}"),
        }
    }

    #[test]
    fn idless_action_commands_reach_the_app_registry_class() {
        for command in [
            "app.controls.set",
            "app.transport.play",
            "app.transport.stop",
            "app.song.load",
            "app.quit",
            "action.invoke",
            "actions.list",
            "actions.describe",
        ] {
            assert!(is_app_verb(command), "{command}");
        }
        assert!(!is_app_verb("props.changed"));
        assert!(is_observe_event("noded.observe.event"));
        assert!(!is_observe_event("noded.observe.start"));
        assert!(!is_observe_event("noded.observe.event.evil"));
    }

    #[test]
    fn action_delivery_failures_use_spec02_rc10_without_changing_app_verbs() {
        for verb in ["action.invoke", "actions.list", "actions.describe"] {
            let (rc, body) = app_delivery_error(verb, true);
            assert_eq!(rc, 10);
            assert!(body.contains(r#""id":"resyncing""#));
            let (rc, body) = app_delivery_error(verb, false);
            assert_eq!(rc, 10);
            assert!(body.contains(r#""id":"delivery_queue_full""#));
        }
        assert_eq!(app_delivery_error("app.controls.set", true).0, 11);
        assert_eq!(app_delivery_error("app.controls.set", false).0, 11);
    }

    #[test]
    fn oversized_action_body_uses_a_stable_error_identifier() {
        for verb in ["action.invoke", "actions.list", "actions.describe"] {
            let (rc, body) = app_body_too_large_error(verb);
            assert_eq!(rc, 10);
            assert!(body.contains(r#""id":"body_too_large""#));
        }
        let (rc, body) = app_body_too_large_error("app.controls.set");
        assert_eq!(rc, 10);
        assert_eq!(body, r#"{"error":"body too large"}"#);
    }

    #[test]
    fn successful_worker_delivery_requests_one_coalesced_wake() {
        let (tx, rx) = flume::bounded(2);
        let wakes = Arc::new(AtomicU64::new(0));
        let observed = Arc::clone(&wakes);
        let sender = WakeSender::new(
            tx,
            Arc::new(move || {
                observed.fetch_add(1, Ordering::Relaxed);
            }),
        );
        sender.try_send(1_u8).unwrap();
        assert_eq!(rx.try_recv(), Ok(1));
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
    }

    /// A burst larger than the per-frame cap has already spent its per-send
    /// wakes by the time the frame drains: the capped drain must re-arm the
    /// wake itself, or the remainder waits for an unrelated event.
    #[test]
    fn capped_drain_rearms_the_wake_for_the_undrained_remainder() {
        let (message_tx, message_rx) = flume::bounded::<BusMessage>(8);
        let (request_tx, _request_rx) = flume::bounded(1);
        let (observation_request_tx, _orx) = flume::bounded(1);
        let (_event_tx, event_rx) = flume::bounded(1);
        let (_observation_message_tx, observation_message_rx) = flume::bounded(1);
        let (_inbound_tx, inbound_rx) = flume::bounded(1);
        let (shutdown_done_tx, shutdown_done_rx) = flume::bounded(1);
        let _ = shutdown_done_tx.send(());
        let wakes = Arc::new(AtomicU64::new(0));
        let observed = Arc::clone(&wakes);
        let bridge = BusBridge {
            requests: request_tx,
            observation_requests: observation_request_tx,
            events: event_rx,
            messages: message_rx,
            observation_messages: observation_message_rx,
            inbound: inbound_rx,
            committed_generation: Arc::new(AtomicU64::new(1)),
            latest_messages: Arc::new(Mutex::new(HashMap::new())),
            semantic_inboxes: Arc::new(SemanticInboxes::default()),
            wake: Arc::new(move || {
                observed.fetch_add(1, Ordering::Relaxed);
            }),
            max_messages_per_frame: 2,
            max_observation_messages_per_frame: 2,
            service_name: "test-app".into(),
            shutdown_done: shutdown_done_rx,
            telemetry_taken: false,
        };
        let message = || BusMessage {
            connection_generation: 1,
            from: "peer".into(),
            command: "publish".into(),
            body: String::new(),
            headers: BTreeMap::new(),
        };
        for _ in 0..3 {
            message_tx.send(message()).unwrap();
        }
        assert_eq!(bridge.drain_messages().count(), 2);
        assert_eq!(wakes.load(Ordering::Relaxed), 1, "over-cap drain re-arms");
        assert_eq!(bridge.drain_messages().count(), 1);
        assert_eq!(
            wakes.load(Ordering::Relaxed),
            1,
            "an under-cap drain never self-wakes"
        );
    }
}
