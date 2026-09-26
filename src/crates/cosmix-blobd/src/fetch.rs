//! `blob.fetch`: cross-node pull over the byte lane, early reply,
//! completion through the `blob.fetched` event.
//!
//! The verb **replies immediately** — never a deferred reply: the mesh
//! response timeout is 30 s (`cosmix-lib-mesh/src/peer.rs`) and a
//! multi-GiB pull outlives it. Completion is the `blob.fetched` event
//! (`retain: false`) plus the `blob.stat` transition `present:false →
//! true`.
//!
//! **Single-flight per hash:** a second `blob.fetch` for an in-flight
//! hash joins it, adds its pin on completion, and never starts a
//! second download.
//!
//! **Resolution:** `from` if given, else the reference's `origin` —
//! advisory, first try only. The node's lane URL comes from a
//! mesh-open `blob.props.get {path:"lane"}` (the verb namespace is
//! `blob.*`; the service is `blobd`, addressed `blobd[.<instance>].<node>`
//! through the local noded); on `Service 'blobd' not
//! found` / `disconnected` (both rc=10, discriminated by message text)
//! or a lane 404, the fetch fans `blob.has {blobs:[hash]}` out over
//! `noded.peers` (all peers, one round, bounded concurrency 4) and
//! fetches from the first `present`. `verify_failed` is terminal —
//! never retried against another peer.
//!
//! **Bounded:** at most `fetch_max_concurrent` (default 2) downloads
//! run; beyond that the verb still replies `accepted` and the fetch
//! queues in-process up to `fetch_queue_max` (default 32) — above
//! that the verb replies rc 10 `busy`. Never an unbounded queue. A
//! stalled body aborts after a 30 s idle read timeout. The resolver
//! and peer roster sit behind small traits so tests inject them and
//! production wires noded; no unit test touches the Bus.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use cosmix_bus::PortReply;
use cosmix_client::NodedClient;
use cosmix_mds::blob;
use cosmix_mds::types::BlobHash;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::{Semaphore, mpsc};

use crate::citizen::{BusEvent, TOPIC_PINNED, domain_event};
use crate::core::reference;
use crate::core::store::Store;
use crate::lane::{ChannelReader, Frame};
use crate::props::{PropsInput, props_diff_events};

/// The completion event's topic, published `retain: false`.
pub const TOPIC_FETCHED: &str = "blob.fetched";

/// Idle read timeout on a lane response: no body bytes for this long
/// aborts the fetch (staging deleted) — the lane's upload bound,
/// mirrored.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// Frames buffered between the async response pump and the blocking
/// CAS writer (the lane's bound: backpressure, not throughput).
const PUMP_FRAMES: usize = 4;
/// TCP connect timeout for a lane GET — an unresponsive peer must not
/// hold a fetch slot for the mesh timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// `blob.has` fan-out concurrency bound (one round over the roster).
const FANOUT_CONCURRENCY: usize = 4;
/// Timeout around props/has Bus calls: above the 30 s mesh response
/// timeout, below the client's 60 s safety net, so a hung hop becomes
/// `origin_unreachable` instead of parking a slot.
const BUS_CALL_TIMEOUT: Duration = Duration::from_secs(35);
/// Timeout around one event publication (the citizen's own bound).
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(60);
/// Backoff before the single record_fetch retry in the completion
/// path (F2): enough for a transient db hiccup to clear, short enough
/// not to hold the fetch task.
const PIN_RETRY_DELAY: Duration = Duration::from_millis(250);

pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The `blob.fetched` outcome taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchOutcome {
    /// Fetched, verified, pinned.
    Ok,
    /// No source was reachable at all — the origin would not resolve
    /// and no peer answered `blob.has`.
    OriginUnreachable,
    /// Sources answered and none holds the blob (a lane 404 or a
    /// peer's `has: false`).
    NotFoundAnywhere,
    /// The bytes a source served do not hash to the requested id.
    /// Terminal — never retried against another peer.
    VerifyFailed,
    /// The owner or total cap would be exceeded (declared
    /// `Content-Length` or the mid-stream counter).
    Quota,
    /// A holder was reached but the transfer or local ingest failed.
    Io,
}

impl FetchOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::OriginUnreachable => "origin_unreachable",
            Self::NotFoundAnywhere => "not_found_anywhere",
            Self::VerifyFailed => "verify_failed",
            Self::Quota => "quota",
            Self::Io => "io",
        }
    }
}

/// The first-try source: a node name (never an IP) plus the remote
/// instance when the reference carries one.
#[derive(Debug, Clone)]
pub struct FetchTarget {
    pub node: String,
    pub instance: Option<String>,
}

/// Resolves a node's byte-lane base URL (`http://<wg-ip>:<port>`).
/// Production wires noded: a mesh-open `blob.props.get` (the citizen's
/// `blob.*` namespace — the service is `blobd`, the verb never carries
/// the `blobd.` prefix) addressed `blobd[.<instance>].<node>` through
/// the local broker. Every failure is "this source is unreachable" —
/// the caller falls back.
pub trait Resolver: Send + Sync {
    fn lane_url<'a>(
        &'a self,
        node: &'a str,
        instance: Option<&'a str>,
    ) -> BoxFuture<'a, Result<String, String>>;
}

/// The props verb [`NodedResolver`] sends — `blob.props.get`, the
/// citizen's own namespace (the service is `blobd`, the verb is not
/// `blobd.props.get`; B1). A shared constant so the citizen test can
/// push the resolver's exact command string through
/// [`crate::citizen::Citizen::dispatch`] and the two can never drift.
pub(crate) const RESOLVER_PROPS_VERB: &str = "blob.props.get";

/// The `to` address [`NodedResolver`] uses: `blobd.<node>`, or
/// `blobd-<name>.<node>` when the reference carries an instance (the
/// same service name `Config::service_name` mints for a named
/// instance).
pub(crate) fn resolver_to(node: &str, instance: Option<&str>) -> String {
    match instance {
        Some(name) => format!("blobd-{name}.{node}"),
        None => format!("blobd.{node}"),
    }
}

/// The fallback roster. Production wires noded: `noded.peers` for the
/// list, mesh-open `blob.has` per peer for presence. `has` takes
/// `self: Arc<Self>` so the fan-out can run it bounded-concurrent on
/// the runtime.
pub trait PeerSource: Send + Sync {
    fn peers(&self) -> BoxFuture<'static, Result<Vec<String>, String>>;
    fn has(
        self: Arc<Self>,
        node: String,
        hash: BlobHash,
    ) -> BoxFuture<'static, Result<bool, String>>;
}

/// Where completion events go. Production: `topic.publish` with
/// `retain: false` through the live broker connection — noded's
/// `topic.publish` defaults to `retain: true`, which would replay the
/// last event to a late subscriber.
pub trait EventSink: Send + Sync {
    fn publish<'a>(&'a self, event: BusEvent) -> BoxFuture<'a, ()>;
}

/// The broker connection of the moment, shared by the production
/// resolver, peer source and event sink: blobd's own `NodedClient`
/// exists per connection (it reconnects), while the fetch machinery
/// lives across reconnects. `None` between connections — resolution
/// falls back, and events published then are buffered (bounded, F7)
/// for replay on the next connection.
#[derive(Clone, Default)]
pub struct ClientSlot {
    client: Arc<RwLock<Option<Arc<NodedClient>>>>,
    pending: Arc<PendingEvents>,
}

impl ClientSlot {
    pub fn set(&self, client: Option<Arc<NodedClient>>) {
        match client {
            Some(client) => {
                *self.client.write().unwrap() = Some(Arc::clone(&client));
                // F7: replay what was published while disconnected,
                // oldest first, before anything new publishes.
                let backlog = self.pending.drain();
                if !backlog.is_empty() {
                    tokio::spawn(async move {
                        for event in &backlog {
                            publish_event_via(&client, event).await;
                        }
                    });
                }
            }
            None => *self.client.write().unwrap() = None,
        }
    }

    fn get(&self) -> Option<Arc<NodedClient>> {
        self.client.read().unwrap().clone()
    }

    fn buffer(&self, event: BusEvent) {
        self.pending.push(event);
    }

    #[cfg(test)]
    fn pending_len(&self) -> usize {
        self.pending.len()
    }

    #[cfg(test)]
    fn drain_pending_for_test(&self) -> Vec<BusEvent> {
        self.pending.drain()
    }
}

/// Events published while no broker connection exists, held for
/// replay on the next connection (F7). Bounded at 256 — a long
/// partition must not grow an unbounded queue — and beyond the bound
/// the OLDEST drop with a log line (recovery for a missed
/// `blob.fetched` is `blob.stat` present + the waiter's timeout).
#[derive(Default)]
pub(crate) struct PendingEvents {
    queue: Mutex<VecDeque<BusEvent>>,
}

impl PendingEvents {
    const CAP: usize = 256;

    pub(crate) fn push(&self, event: BusEvent) {
        let mut queue = self.queue.lock().unwrap();
        let dropped = if queue.len() >= Self::CAP {
            queue.pop_front()
        } else {
            None
        };
        queue.push_back(event);
        drop(queue);
        if let Some(dropped) = dropped {
            eprintln!(
                "cosmix-blobd: fetch event backlog full ({}); dropping the oldest {} \
                 (recovery: blob.stat present + the waiter's timeout)",
                Self::CAP,
                dropped.topic
            );
        }
    }

    pub(crate) fn drain(&self) -> Vec<BusEvent> {
        self.queue.lock().unwrap().drain(..).collect()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.queue.lock().unwrap().len()
    }
}

// ---- Production wiring (noded through the client slot) ----

/// Resolves lane URLs with `blob.props.get` (the citizen's `blob.*`
/// verb namespace) through the local noded.
pub struct NodedResolver {
    client: ClientSlot,
}

impl NodedResolver {
    fn new(client: ClientSlot) -> Self {
        Self { client }
    }
}

impl Resolver for NodedResolver {
    fn lane_url<'a>(
        &'a self,
        node: &'a str,
        instance: Option<&'a str>,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let client = self
                .client
                .get()
                .ok_or_else(|| "not connected to the broker".to_string())?;
            let to = resolver_to(node, instance);
            let reply = tokio::time::timeout(
                BUS_CALL_TIMEOUT,
                client.call_typed(&to, RESOLVER_PROPS_VERB, json!({"path": "lane"})),
            )
            .await
            .map_err(|_| format!("{RESOLVER_PROPS_VERB} on {to} timed out"))?
            .map_err(|e| format!("{RESOLVER_PROPS_VERB} on {to}: {e}"))?;
            match reply {
                PortReply::Ok { value, .. } => {
                    let bind = value
                        .get("bind")
                        .and_then(Value::as_str)
                        .ok_or_else(|| format!("{to} props lane carries no bind"))?;
                    Ok(format!("http://{bind}"))
                }
                // "Service 'blobd' not found" / "disconnected" / "Unknown
                // mesh node" / "Mesh bridge error" — all rc=10, all
                // first-try-unreachable for this source.
                PortReply::AppError { message, .. } => Err(message),
            }
        })
    }
}

/// The fallback roster from `noded.peers` + mesh-open `blob.has`.
pub struct NodedPeers {
    client: ClientSlot,
}

impl NodedPeers {
    fn new(client: ClientSlot) -> Self {
        Self { client }
    }
}

impl PeerSource for NodedPeers {
    fn peers(&self) -> BoxFuture<'static, Result<Vec<String>, String>> {
        let client = self.client.get();
        Box::pin(async move {
            let client = client.ok_or_else(|| "not connected to the broker".to_string())?;
            let reply = tokio::time::timeout(
                BUS_CALL_TIMEOUT,
                client.call_typed("noded", "noded.peers", json!({})),
            )
            .await
            .map_err(|_| "noded.peers timed out".to_string())?
            .map_err(|e| format!("noded.peers: {e}"))?;
            match reply {
                PortReply::Ok { value, .. } => {
                    let mut out = Vec::new();
                    if let Some(peers) = value.get("peers").and_then(Value::as_array) {
                        for peer in peers {
                            if let Some(name) = peer.get("name").and_then(Value::as_str) {
                                out.push(name.to_string());
                            }
                        }
                    }
                    Ok(out)
                }
                PortReply::AppError { message, .. } => Err(message),
            }
        })
    }

    fn has(
        self: Arc<Self>,
        node: String,
        hash: BlobHash,
    ) -> BoxFuture<'static, Result<bool, String>> {
        Box::pin(async move {
            let client = self
                .client
                .get()
                .ok_or_else(|| "not connected to the broker".to_string())?;
            let id = reference::blob_id(&hash);
            let to = format!("blobd.{node}");
            let reply = tokio::time::timeout(
                BUS_CALL_TIMEOUT,
                client.call_typed(&to, "blob.has", json!({"blobs": [id]})),
            )
            .await
            .map_err(|_| format!("blob.has on {to} timed out"))?
            .map_err(|e| format!("blob.has on {to}: {e}"))?;
            match reply {
                PortReply::Ok { value, .. } => Ok(value
                    .get("present")
                    .and_then(Value::as_array)
                    .is_some_and(|present| {
                        present.iter().any(|v| v.as_str() == Some(id.as_str()))
                    })),
                PortReply::AppError { message, .. } => Err(message),
            }
        })
    }
}

/// Publishes completion events `retain: false` through the live
/// connection; between connections the event is buffered (bounded,
/// see [`PendingEvents`]) and replayed on reconnect — best-effort
/// with a log line only if the backlog overflows (F7).
pub struct BusSink {
    client: ClientSlot,
}

impl BusSink {
    fn new(client: ClientSlot) -> Self {
        Self { client }
    }
}

impl EventSink for BusSink {
    fn publish<'a>(&'a self, event: BusEvent) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let Some(client) = self.client.get() else {
                self.client.buffer(event);
                return;
            };
            publish_event_via(&client, &event).await
        })
    }
}

/// One `retain: false` `topic.publish` through a live connection —
/// the shared home of the publication both the live sink and the
/// reconnect replay use. Never fails the caller: a lost broker logs
/// and continues (the `blob.stat` transition still happened).
async fn publish_event_via(client: &Arc<NodedClient>, event: &BusEvent) {
    let headers = std::collections::BTreeMap::from([
        ("name".to_string(), event.topic.to_string()),
        // noded's topic.publish defaults retain: true; these
        // events must never replay to a late subscriber.
        ("retain".to_string(), "false".to_string()),
    ]);
    let wire = event.message.to_wire();
    match tokio::time::timeout(
        PUBLISH_TIMEOUT,
        client.send_with_headers("noded", "topic.publish", &headers, &wire),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => eprintln!(
            "cosmix-blobd: fetch publish on {} failed (continuing): {error}",
            event.topic
        ),
        Err(_) => eprintln!(
            "cosmix-blobd: fetch publish on {} timed out (continuing)",
            event.topic
        ),
    }
}

// ---- The fetcher ----

/// Bounds for the fetch machinery, from config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchConfig {
    /// Concurrent downloads; beyond it a fetch queues.
    pub max_concurrent: usize,
    /// Queue depth; a fetch beyond it is refused rc 10 `busy`.
    pub queue_max: usize,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            max_concurrent: crate::core::config::DEFAULT_FETCH_MAX_CONCURRENT,
            queue_max: crate::core::config::DEFAULT_FETCH_QUEUE_MAX,
        }
    }
}

impl FetchConfig {
    pub fn from_config(cfg: &crate::core::config::Config) -> Self {
        Self {
            max_concurrent: cfg.fetch_max_concurrent,
            queue_max: cfg.fetch_queue_max,
        }
    }
}

/// What [`Fetcher::submit`] decided — the verb reply's `in_flight`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// This call started the download (a slot was free).
    Started,
    /// A fetch for this hash was already in the system; the caller
    /// joined it (its pin lands on completion) or this call queued
    /// behind the concurrency bound.
    Joined,
    /// The queue is full; rc 10 `busy`.
    Busy,
}

/// Live gauges and lifetime counters (`fetch.*` props).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FetchGauges {
    pub in_flight: u64,
    pub queued: u64,
    pub completed: u64,
    pub failed: u64,
}

struct Entry {
    /// Pin owners: the submitter plus every joiner.
    owners: Vec<String>,
    target: Option<FetchTarget>,
    /// True until the task holds a concurrency slot.
    queued: bool,
}

#[derive(Default)]
struct FetchState {
    entries: HashMap<BlobHash, Entry>,
    /// Admitted entries — the `fetch.in_flight` gauge. Counted at
    /// submit under the same lock that decides admission, so the
    /// verb's queued/started reply is exact, not a race against task
    /// startup.
    running: usize,
}

struct Inner {
    store: Arc<Store>,
    lane_bind: Option<SocketAddr>,
    instance: String,
    resolver: Arc<dyn Resolver>,
    peers: Arc<dyn PeerSource>,
    sink: Arc<dyn EventSink>,
    client: ClientSlot,
    /// Built on first use inside the runtime (construction itself
    /// must not require one — the citizen tests build a `Fetcher`
    /// outside any runtime). `Client` is a cheap `Arc` clone.
    http: OnceLock<reqwest::Client>,
    state: Mutex<FetchState>,
    slots: Semaphore,
    jobs_tx: mpsc::UnboundedSender<BlobHash>,
    jobs_rx: tokio::sync::Mutex<Option<mpsc::UnboundedReceiver<BlobHash>>>,
    config: FetchConfig,
    completed: AtomicU64,
    failed: AtomicU64,
}

/// The fetch machinery: submit from the (synchronous) verb dispatch,
/// download on the runtime, complete through the event sink.
/// Cheap to clone (`Arc` inside); construction never touches the
/// network.
#[derive(Clone)]
pub struct Fetcher(Arc<Inner>);

impl Fetcher {
    pub fn new(
        store: Arc<Store>,
        config: FetchConfig,
        lane_bind: Option<SocketAddr>,
        instance: String,
        resolver: Arc<dyn Resolver>,
        peers: Arc<dyn PeerSource>,
        sink: Arc<dyn EventSink>,
    ) -> Self {
        let (jobs_tx, jobs_rx) = mpsc::unbounded_channel();
        Self(Arc::new(Inner {
            store,
            lane_bind,
            instance,
            resolver,
            peers,
            sink,
            client: ClientSlot::default(),
            http: OnceLock::new(),
            state: Mutex::new(FetchState::default()),
            slots: Semaphore::new(config.max_concurrent),
            jobs_tx,
            jobs_rx: tokio::sync::Mutex::new(Some(jobs_rx)),
            config,
            completed: AtomicU64::new(0),
            failed: AtomicU64::new(0),
        }))
    }

    /// The production wiring: noded-addressed resolution, `noded.peers`
    /// fallback and Bus publication, all through the live connection
    /// slot [`Fetcher::set_client`] keeps current.
    pub fn production(
        store: Arc<Store>,
        cfg: &crate::core::config::Config,
        lane_bind: Option<SocketAddr>,
        instance: &str,
    ) -> Self {
        let client = ClientSlot::default();
        Self::new(
            store,
            FetchConfig::from_config(cfg),
            lane_bind,
            instance.to_string(),
            Arc::new(NodedResolver::new(client.clone())),
            Arc::new(NodedPeers::new(client.clone())),
            Arc::new(BusSink::new(client)),
        )
    }

    /// Submit a fetch. Synchronous and quick (a map insert and a
    /// channel send) — the verb reply is immediate by construction.
    /// Admission is decided under the state lock: a slot free → this
    /// call started the download; the concurrency bound met → it
    /// queues (bounded); the queue full → `Busy`.
    pub fn submit(
        &self,
        hash: BlobHash,
        target: Option<FetchTarget>,
        owner: String,
    ) -> SubmitOutcome {
        let mut state = self.0.state.lock().unwrap();
        if let Some(entry) = state.entries.get_mut(&hash) {
            entry.owners.push(owner);
            return SubmitOutcome::Joined;
        }
        let slot_free = state.running < self.0.config.max_concurrent;
        if !slot_free {
            let queued_count = state.entries.values().filter(|e| e.queued).count();
            if queued_count >= self.0.config.queue_max {
                return SubmitOutcome::Busy;
            }
        }
        if slot_free {
            state.running += 1;
        }
        state.entries.insert(
            hash,
            Entry {
                owners: vec![owner],
                target,
                queued: !slot_free,
            },
        );
        drop(state);
        let _ = self.0.jobs_tx.send(hash);
        if slot_free {
            SubmitOutcome::Started
        } else {
            SubmitOutcome::Joined
        }
    }

    /// Spawn the dispatcher (once, inside the runtime): one task per
    /// admitted fetch, each holding a concurrency slot for its whole
    /// download.
    pub async fn spawn_dispatcher(&self) {
        let Some(mut jobs_rx) = self.0.jobs_rx.lock().await.take() else {
            return;
        };
        let inner = Arc::clone(&self.0);
        tokio::spawn(async move {
            while let Some(hash) = jobs_rx.recv().await {
                let inner = Arc::clone(&inner);
                tokio::spawn(async move {
                    inner.run_fetch(hash).await;
                });
            }
        });
    }

    /// The current broker connection for the production resolver, peer
    /// source and event sink; `None` between connections.
    pub fn set_client(&self, client: Option<Arc<NodedClient>>) {
        self.0.client.set(client);
    }

    /// `fetch.in_flight` / `fetch.queued` gauges and lifetime
    /// `fetch.completed` / `fetch.failed` counters.
    pub fn gauges(&self) -> FetchGauges {
        let state = self.0.state.lock().unwrap();
        FetchGauges {
            in_flight: state.running as u64,
            queued: state.entries.values().filter(|e| e.queued).count() as u64,
            completed: self.0.completed.load(Ordering::Relaxed),
            failed: self.0.failed.load(Ordering::Relaxed),
        }
    }
}

impl Inner {
    /// One admitted fetch: slot, resolution chain, ingest, completion
    /// events. Runs as its own task; the entry exists for its whole
    /// lifetime (single-flight), removed only here.
    async fn run_fetch(self: Arc<Self>, hash: BlobHash) {
        // A queued fetch waits here; the permit is held for the whole
        // download.
        let _permit = self.slots.acquire().await;
        let (target, owner, was_queued) = {
            let mut state = self.state.lock().unwrap();
            let Some(entry) = state.entries.get_mut(&hash) else {
                return; // defensive: jobs and entries are 1:1
            };
            // Admission already counted a submit-time slot; a queued
            // entry is promoted when its permit actually frees up.
            let was_queued = entry.queued;
            entry.queued = false;
            (entry.target.clone(), entry.owners[0].clone(), was_queued)
        };
        if was_queued {
            self.state.lock().unwrap().running += 1;
        }

        let attempt = self.execute(hash, target, owner).await;

        // Completion: take the entry (its owners froze at this point —
        // later fetches for the same hash start fresh).
        let owners = {
            let mut state = self.state.lock().unwrap();
            match state.entries.remove(&hash) {
                Some(entry) => {
                    if !entry.queued {
                        state.running = state.running.saturating_sub(1);
                    }
                    entry.owners
                }
                None => Vec::new(),
            }
        };

        self.complete(hash, attempt, owners).await;
    }

    /// The resolution chain: origin first try, then the bounded
    /// `blob.has` fan-out over the peer roster, then classification.
    async fn execute(
        self: &Arc<Self>,
        hash: BlobHash,
        target: Option<FetchTarget>,
        owner: String,
    ) -> Attempt {
        // The bytes may have landed while this fetch queued (a
        // concurrent put or fetch): nothing to move.
        if matches!(blob::exists(&self.store.blobs_root(), &hash), Ok(true)) {
            let size = blob::size(&self.store.blobs_root(), &hash).unwrap_or(0);
            return Attempt::ok(
                None,
                size,
                "application/octet-stream".to_string(),
                None,
            );
        }

        let mut answered_no = false; // a reachable source said "not present"
        let mut holder_failed = None; // a holder was reached but the transfer failed

        if let Some(target) = target
            && let Ok(url) = self
                .resolver
                .lane_url(&target.node, target.instance.as_deref())
                .await
        {
            match self.try_source(&url, &hash, &owner).await {
                SourceOutcome::Fetched {
                    size,
                    mime,
                    reservation,
                } => return Attempt::ok(Some(target.node), size, mime, Some(reservation)),
                terminal @ (SourceOutcome::Verify { .. }
                | SourceOutcome::Quota(_)
                | SourceOutcome::Local(_)) => return Attempt::terminal(terminal),
                SourceOutcome::Lane404 => answered_no = true,
                // The origin served 200 and then failed — it
                // demonstrably holds the bytes; that is an `io`
                // outcome if nothing else serves them.
                SourceOutcome::StreamErr(error) => holder_failed = Some(error),
                SourceOutcome::NetErr(_) => {}
            }
        }

        // Fallback: one round of `blob.has` over the roster, bounded.
        let peers = self.peers.peers().await.unwrap_or_default();
        let mut holders = Vec::new();
        for chunk in peers.chunks(FANOUT_CONCURRENCY) {
            let mut round = tokio::task::JoinSet::new();
            for peer in chunk {
                let peer = peer.clone();
                let source = Arc::clone(&self.peers);
                round.spawn(async move {
                    let result = source.has(peer.clone(), hash).await;
                    (peer, result)
                });
            }
            let mut by_peer: HashMap<String, Result<bool, String>> = HashMap::new();
            while let Some(joined) = round.join_next().await {
                if let Ok((peer, result)) = joined {
                    by_peer.insert(peer, result);
                }
            }
            for peer in chunk {
                match by_peer.get(peer.as_str()) {
                    Some(Ok(true)) => holders.push(peer.clone()),
                    Some(Ok(false)) => answered_no = true,
                    Some(Err(_)) | None => {} // unreachable peer: not an answer
                }
            }
        }

        for peer in &holders {
            match self.resolver.lane_url(peer, None).await {
                Ok(url) => match self.try_source(&url, &hash, &owner).await {
                    SourceOutcome::Fetched {
                        size,
                        mime,
                        reservation,
                    } => {
                        return Attempt::ok(Some(peer.clone()), size, mime, Some(reservation))
                    }
                    terminal @ (SourceOutcome::Verify { .. }
                    | SourceOutcome::Quota(_)
                    | SourceOutcome::Local(_)) => return Attempt::terminal(terminal),
                    // A live holder's lane no longer serves it (a stale
                    // has): keep trying the remaining holders.
                    SourceOutcome::Lane404 => answered_no = true,
                    // The holder claimed the bytes (has: true) but the
                    // transfer failed — `io` if nothing else serves.
                    SourceOutcome::StreamErr(error) | SourceOutcome::NetErr(error) => {
                        holder_failed = Some(error)
                    }
                },
                Err(_) => continue,
            }
        }

        // Exhausted. Classify.
        if let Some(error) = holder_failed {
            Attempt::failed(
                FetchOutcome::Io,
                format!("a holder was reached but the transfer failed: {error}"),
            )
        } else if answered_no {
            Attempt::failed(
                FetchOutcome::NotFoundAnywhere,
                "no reachable node holds the blob".to_string(),
            )
        } else {
            Attempt::failed(
                FetchOutcome::OriginUnreachable,
                "no source was reachable: the origin would not resolve \
                 (or was not given) and no peer answered"
                    .to_string(),
            )
        }
    }

    /// One `GET http://<lane>/blob/<hex>`: quota-checked from
    /// `Content-Length` before the first byte, streamed into mds
    /// staging (never buffered whole), the landed hash verified.
    async fn try_source(
        self: &Arc<Self>,
        url: &str,
        hash: &BlobHash,
        owner: &str,
    ) -> SourceOutcome {
        let hex = blob::hex(hash);
        let target = format!("{url}/blob/{hex}");
        let http = self
            .http
            .get_or_init(|| {
                reqwest::Client::builder()
                    .connect_timeout(CONNECT_TIMEOUT)
                    .build()
                    .expect("build the fetch HTTP client")
            })
            .clone();
        let response = match http.get(&target).send().await {
            Ok(response) => response,
            Err(error) => return SourceOutcome::NetErr(format!("GET {target}: {error}")),
        };
        match response.status() {
            reqwest::StatusCode::OK => {}
            reqwest::StatusCode::NOT_FOUND => return SourceOutcome::Lane404,
            status => {
                return SourceOutcome::NetErr(format!("lane answered {status} for {target}"));
            }
        }

        // Quota before the first byte, reserved at admission (M3): the
        // declared Content-Length, or the whole remaining room when
        // absent — concurrent downloads for one owner can no longer
        // each spend the same headroom. The reservation travels with
        // the attempt and releases when the pin settles.
        let reservation = match self.store.reserve_upload(owner, response.content_length()) {
            Ok(reservation) => reservation,
            Err(e @ crate::core::store::StoreError::QuotaOwner { .. }) => {
                return SourceOutcome::Quota(e.to_string())
            }
            Err(e @ crate::core::store::StoreError::QuotaTotal { .. }) => {
                return SourceOutcome::Quota(e.to_string())
            }
            Err(e) => return SourceOutcome::Local(e.to_string()),
        };
        let cap = reservation.cap();
        let mime = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.split(';').next().unwrap_or(v).trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "application/octet-stream".to_string());

        let (tx, rx) = mpsc::channel::<Frame>(PUMP_FRAMES);
        let pump = tokio::spawn(pump_response(response, tx, cap));
        let reader_store = Arc::clone(&self.store);
        let expected = *hash;
        // The reservation rides inside the write task (its release
        // must outlast the stream) and comes back out on success, so
        // it can outlive this frame and settle after the pin lands.
        let landed = tokio::task::spawn_blocking(move || {
            let landed =
                blob::put_reader_expect(&reader_store.blobs_root(), ChannelReader::new(rx), &expected);
            (landed, reservation)
        })
        .await;
        let _ = pump.await;

        match landed {
            // put_reader_expect compared before committing, so a
            // success here is verified: the landed hash is the
            // requested one.
            Ok((Ok((_landed, size)), reservation)) => SourceOutcome::Fetched {
                size,
                mime,
                reservation,
            },
            Ok((Err(cosmix_mds::Error::BlobCorrupt(message)), _reservation)) => {
                // The body did not hash to the requested id. Nothing
                // ever entered the CAS — mds removed the staging
                // before any commit — so there is nothing to undo.
                SourceOutcome::Verify(message)
            }
            Ok((Err(error), _reservation)) => {
                // The abort reason rode inside the io::Error that
                // stopped the stream; put_reader_expect has already
                // deleted the staging file by the time it surfaces
                // here.
                match abort_kind(&error) {
                    Some(FetchAbort::Cap) => SourceOutcome::Quota(
                        "quota: the body passed the remaining cap mid-stream".to_string(),
                    ),
                    _ => SourceOutcome::StreamErr(error.to_string()),
                }
            }
            Err(join) => SourceOutcome::Local(format!("fetch write task failed: {join}")),
        }
    }

    /// Ingest on success, counters, and the completion events: one
    /// `blob.pinned` per newly pinned owner, the props diff, then
    /// `blob.fetched` last — it is the completion signal.
    ///
    /// The pin is the completion's contract: `ok` is never published
    /// without it (F2). A failed `record_fetch` is retried once after
    /// a short backoff (a transient db hiccup); a second failure turns
    /// the attempt into outcome `io` with the error, so every joined
    /// caller learns the truth instead of trusting an unpinned blob.
    async fn complete(self: &Arc<Self>, hash: BlobHash, mut attempt: Attempt, owners: Vec<String>) {
        let id = reference::blob_id(&hash);
        let mut events = Vec::new();
        if attempt.outcome == FetchOutcome::Ok {
            // Settle the admission's quota reservation after the pins
            // account the real size (M3); on every other path it has
            // already dropped with the attempt's failure.
            let reservation = attempt.reservation.take();
            let before = self.props_input();
            let pin_result = match self.store.record_fetch(
                &hash,
                attempt.size.unwrap_or(0),
                attempt
                    .mime
                    .as_deref()
                    .unwrap_or("application/octet-stream"),
                attempt.origin_used.as_deref().unwrap_or("unknown"),
                &owners,
            ) {
                Ok(pinned) => Ok(pinned),
                Err(first) => {
                    eprintln!(
                        "cosmix-blobd: record_fetch for {id} failed ({first}); retrying once"
                    );
                    tokio::time::sleep(PIN_RETRY_DELAY).await;
                    self.store
                        .record_fetch(
                            &hash,
                            attempt.size.unwrap_or(0),
                            attempt
                                .mime
                                .as_deref()
                                .unwrap_or("application/octet-stream"),
                            attempt.origin_used.as_deref().unwrap_or("unknown"),
                            &owners,
                        )
                        .map_err(|second| format!("{first}; retry: {second}"))
                }
            };
            match pin_result {
                Ok(newly_pinned) => {
                    for owner in &newly_pinned {
                        events.push(domain_event(
                            TOPIC_PINNED,
                            json!({"blob": id, "owner": owner}),
                        ));
                    }
                    self.completed.fetch_add(1, Ordering::Relaxed);
                    let after = self.props_input();
                    events.extend(props_diff_events(&before, &after));
                    events.push(domain_event(
                        TOPIC_FETCHED,
                        json!({
                            "blob": id,
                            "outcome": attempt.outcome.as_str(),
                            "origin_used": attempt.origin_used,
                            "size": attempt.size,
                        }),
                    ));
                }
                Err(error) => {
                    self.failed.fetch_add(1, Ordering::Relaxed);
                    events.push(domain_event(
                        TOPIC_FETCHED,
                        json!({
                            "blob": id,
                            "outcome": FetchOutcome::Io.as_str(),
                            "origin_used": Value::Null,
                            "error": format!("pin failed after the download landed: {error}"),
                        }),
                    ));
                }
            }
            drop(reservation);
        } else {
            self.failed.fetch_add(1, Ordering::Relaxed);
            events.push(domain_event(
                TOPIC_FETCHED,
                json!({
                    "blob": id,
                    "outcome": attempt.outcome.as_str(),
                    "origin_used": Value::Null,
                    "error": attempt.error,
                }),
            ));
        }
        for event in events {
            self.sink.publish(event).await;
        }
    }

    /// A full props snapshot for the completion diff (the same shape
    /// the citizen serves); store failures degrade to defaults — a
    /// props glitch must not eat the `blob.fetched` event.
    fn props_input(&self) -> PropsInput {
        let (counts_blobs, counts_pins) = self.store.counts().unwrap_or((0, 0));
        let quota = self.store.quota_report(None).unwrap_or_default();
        let (in_flight, queued) = {
            let state = self.state.lock().unwrap();
            (
                state.running as u64,
                state.entries.values().filter(|e| e.queued).count() as u64,
            )
        };
        PropsInput {
            lane_bind: self
                .lane_bind
                .as_ref()
                .map(|a| a.to_string())
                .unwrap_or_default(),
            lane_port: self.lane_bind.map(|a| a.port()).unwrap_or(0),
            root: self.store.root().display().to_string(),
            instance: self.instance.clone(),
            counts_blobs,
            counts_pins,
            quota_total_used: quota.total.used,
            quota_total_limit: quota.total.limit,
            generation: self.store.generation(),
            fetch_in_flight: in_flight,
            fetch_queued: queued,
            fetch_completed: self.completed.load(Ordering::Relaxed),
            fetch_failed: self.failed.load(Ordering::Relaxed),
        }
    }
}

// ---- Attempts and sources ----

/// The result of the resolution chain for one hash.
struct Attempt {
    outcome: FetchOutcome,
    origin_used: Option<String>,
    size: Option<u64>,
    mime: Option<String>,
    error: Option<String>,
    /// The download's quota reservation (M3), held from admission
    /// until the pins account the real size; `None` when the bytes
    /// were already present.
    reservation: Option<crate::core::store::Reservation>,
}

impl Attempt {
    fn ok(
        origin_used: Option<String>,
        size: u64,
        mime: String,
        reservation: Option<crate::core::store::Reservation>,
    ) -> Self {
        Self {
            outcome: FetchOutcome::Ok,
            origin_used,
            size: Some(size),
            mime: Some(mime),
            error: None,
            reservation,
        }
    }

    fn terminal(source: SourceOutcome) -> Self {
        let (outcome, error) = match source {
            SourceOutcome::Verify(message) => (FetchOutcome::VerifyFailed, message),
            SourceOutcome::Quota(error) => (FetchOutcome::Quota, error),
            SourceOutcome::Local(error) => (FetchOutcome::Io, error),
            other => (FetchOutcome::Io, format!("{other:?}")),
        };
        Self::failed(outcome, error)
    }

    fn failed(outcome: FetchOutcome, error: String) -> Self {
        Self {
            outcome,
            origin_used: None,
            size: None,
            mime: None,
            error: Some(error),
            reservation: None,
        }
    }
}

/// What one source said.
#[derive(Debug)]
enum SourceOutcome {
    Fetched {
        size: u64,
        mime: String,
        /// The admission's quota reservation (M3), carried to
        /// `complete` so it releases only after the pins account the
        /// real size.
        reservation: crate::core::store::Reservation,
    },
    /// Bytes did not hash to the requested id; terminal, never
    /// retried. mds's `put_reader_expect` rejected them before any
    /// commit, so no CAS entry exists under either hash; the message
    /// names both.
    Verify(String),
    /// Over the owner/total cap; terminal (the bytes are the same size
    /// from any peer).
    Quota(String),
    /// A live lane answered 404 — try the next source.
    Lane404,
    /// The source never served a body (connect refused, an odd
    /// status) — try the next source.
    NetErr(String),
    /// The body started and failed mid-stream — the source
    /// demonstrably holds the bytes.
    StreamErr(String),
    /// Local store failure; terminal.
    Local(String),
}

/// Why a fetch stream was aborted mid-body. Travels through the pump
/// channel inside an `io::Error` so `put_reader_expect`'s error path
/// (which deletes the staging file) runs before the classifier sees
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FetchAbort {
    Cap,
    Idle,
}

impl std::fmt::Display for FetchAbort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cap => write!(f, "quota: the body passed the remaining cap mid-stream"),
            Self::Idle => write!(f, "lane read idle for over 30s"),
        }
    }
}

impl std::error::Error for FetchAbort {}

fn abort_kind(error: &cosmix_mds::Error) -> Option<FetchAbort> {
    match error {
        cosmix_mds::Error::Io(io) => io.get_ref().and_then(|r| r.downcast_ref().copied()),
        _ => None,
    }
}

/// Pump the lane response into the channel the blocking CAS writer
/// reads, enforcing the mid-stream cap and the idle timeout (the
/// lane's own upload pump, mirrored).
async fn pump_response(response: reqwest::Response, tx: mpsc::Sender<Frame>, cap: u64) {
    let mut count: u64 = 0;
    let mut stream = response.bytes_stream();
    loop {
        let item = match tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await {
            Err(_) => {
                let _ = tx
                    .send(Frame::Abort(io::Error::other(FetchAbort::Idle)))
                    .await;
                return;
            }
            Ok(None) => {
                let _ = tx.send(Frame::Eof).await;
                return;
            }
            Ok(Some(Err(error))) => {
                let _ = tx
                    .send(Frame::Abort(io::Error::other(format!(
                        "lane body: {error}"
                    ))))
                    .await;
                return;
            }
            Ok(Some(Ok(bytes))) => bytes,
        };
        if item.is_empty() {
            continue;
        }
        count += item.len() as u64;
        if count > cap {
            let _ = tx
                .send(Frame::Abort(io::Error::other(FetchAbort::Cap)))
                .await;
            return;
        }
        if tx.send(Frame::Data(item)).await.is_err() {
            return; // reader gone; it owns the error surface now
        }
    }
}

// ---- Test support (injected resolver / roster / sink) ----

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::collections::HashSet;
    use std::time::Instant;

    /// A static node → lane-URL map; nodes in `unreachable` always
    /// fail (the `Service 'blobd' not found` shape, from the
    /// fetcher's point of view).
    #[derive(Default)]
    pub(crate) struct MapResolver {
        urls: HashMap<String, String>,
        unreachable: HashSet<String>,
    }

    impl MapResolver {
        pub(crate) fn map(mut self, node: &str, url: impl Into<String>) -> Self {
            self.urls.insert(node.to_string(), url.into());
            self
        }

        pub(crate) fn unreachable(mut self, node: &str) -> Self {
            self.unreachable.insert(node.to_string());
            self
        }
    }

    impl Resolver for MapResolver {
        fn lane_url<'a>(
            &'a self,
            node: &'a str,
            _instance: Option<&'a str>,
        ) -> BoxFuture<'a, Result<String, String>> {
            Box::pin(std::future::ready(if self.unreachable.contains(node) {
                Err(format!(
                    "Service 'blobd' not found on {node} (test resolver)"
                ))
            } else {
                self.urls
                    .get(node)
                    .cloned()
                    .ok_or_else(|| format!("no lane mapped for {node} (test resolver)"))
            }))
        }
    }

    /// Always unreachable — for citizen tests that never fetch.
    pub(crate) struct NullResolver;

    impl Resolver for NullResolver {
        fn lane_url<'a>(
            &'a self,
            node: &'a str,
            _instance: Option<&'a str>,
        ) -> BoxFuture<'a, Result<String, String>> {
            Box::pin(std::future::ready(Err(format!(
                "Service 'blobd' not found on {node} (null resolver)"
            ))))
        }
    }

    /// A fixed roster whose `blob.has` answers by checking the peer's
    /// real store — exactly what production's mesh-open `blob.has`
    /// would say.
    #[derive(Default)]
    pub(crate) struct ListPeers {
        peers: Vec<String>,
        stores: HashMap<String, Arc<Store>>,
    }

    impl ListPeers {
        pub(crate) fn with_peer(mut self, node: &str, store: Arc<Store>) -> Self {
            self.peers.push(node.to_string());
            self.stores.insert(node.to_string(), store);
            self
        }
    }

    impl PeerSource for ListPeers {
        fn peers(&self) -> BoxFuture<'static, Result<Vec<String>, String>> {
            Box::pin(std::future::ready(Ok(self.peers.clone())))
        }

        fn has(
            self: Arc<Self>,
            node: String,
            hash: BlobHash,
        ) -> BoxFuture<'static, Result<bool, String>> {
            Box::pin(std::future::ready(Ok(self
                .stores
                .get(&node)
                .map(|store| blob::exists(&store.blobs_root(), &hash).unwrap_or(false))
                .unwrap_or(false))))
        }
    }

    /// An empty roster — for citizen tests that never fetch.
    pub(crate) struct NullPeers;

    impl PeerSource for NullPeers {
        fn peers(&self) -> BoxFuture<'static, Result<Vec<String>, String>> {
            Box::pin(std::future::ready(Ok(Vec::new())))
        }

        fn has(
            self: Arc<Self>,
            _node: String,
            _hash: BlobHash,
        ) -> BoxFuture<'static, Result<bool, String>> {
            Box::pin(std::future::ready(Ok(false)))
        }
    }

    /// Records completion events; tests await them by topic.
    #[derive(Clone, Default)]
    pub(crate) struct TestSink(Arc<Mutex<Vec<BusEvent>>>);

    impl EventSink for TestSink {
        fn publish<'a>(&'a self, event: BusEvent) -> BoxFuture<'a, ()> {
            self.0.lock().unwrap().push(event);
            Box::pin(std::future::ready(()))
        }
    }

    impl TestSink {
        /// Wait (polling) for the newest event on `topic` whose body
        /// matches `pred`; `None` on timeout.
        pub(crate) async fn wait(
            &self,
            topic: &str,
            pred: impl Fn(&Value) -> bool,
            timeout: Duration,
        ) -> Option<Value> {
            let deadline = Instant::now() + timeout;
            loop {
                let found = {
                    let events = self.0.lock().unwrap();
                    events.iter().rev().find_map(|event| {
                        if event.topic != topic {
                            return None;
                        }
                        let value: Value = serde_json::from_str(&event.message.body).ok()?;
                        pred(&value).then_some(value)
                    })
                };
                if let Some(value) = found {
                    return Some(value);
                }
                if Instant::now() >= deadline {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }

        /// Every event body seen on `topic`, in order.
        pub(crate) fn bodies(&self, topic: &str) -> Vec<Value> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event.topic == topic)
                .map(|event| serde_json::from_str(&event.message.body).unwrap_or(Value::Null))
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{ListPeers, MapResolver, TestSink};
    use super::*;
    use crate::citizen::Citizen;
    use crate::core::store::{PutOptions, StoreOptions};
    use crate::lane::test_support::{counted_lane, options_for, pseudo_random};
    use cosmix_client::IncomingCommand;
    use std::collections::BTreeMap;
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// A citizen + fetcher pair over `store`, with the injected
    /// resolver, roster and recording sink, and the dispatcher running.
    async fn fetch_setup(
        store: Arc<Store>,
        lane_bind: Option<SocketAddr>,
        resolver: MapResolver,
        peers: ListPeers,
    ) -> (Citizen, Fetcher, TestSink) {
        let sink = TestSink::default();
        let fetcher = Fetcher::new(
            Arc::clone(&store),
            FetchConfig::default(),
            lane_bind,
            "default".into(),
            Arc::new(resolver),
            Arc::new(peers),
            Arc::new(sink.clone()),
        );
        let citizen = Citizen::new(
            store,
            "blobd".into(),
            "default".into(),
            lane_bind,
            Arc::new(fetcher.clone()),
        );
        fetcher.spawn_dispatcher().await;
        (citizen, fetcher, sink)
    }

    /// A plain store (no lane) named `origin`.
    fn bare_store(origin: &str) -> (TempDir, Arc<Store>) {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path(), options_for(origin)).unwrap();
        (dir, Arc::new(store))
    }

    fn command(verb: &str, from: &str, args: Value) -> IncomingCommand {
        let mut headers = BTreeMap::new();
        headers.insert("args".into(), args.to_string());
        IncomingCommand {
            from: from.into(),
            command: verb.into(),
            id: Some("1".into()),
            args: Value::Null,
            body: String::new(),
            headers,
        }
    }

    fn tmp_is_empty(store: &Store) -> bool {
        match std::fs::read_dir(store.blobs_root().join(".tmp")) {
            Ok(mut entries) => entries.next().is_none(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => true,
            Err(_) => false,
        }
    }

    /// Read one HTTP/1.1 request head (through the blank line).
    async fn read_head(stream: &mut TcpStream) -> String {
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            assert_eq!(
                stream.read(&mut byte).await.unwrap(),
                1,
                "peer closed mid-head"
            );
            buf.push(byte[0]);
            if buf.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    // ---- B1: the resolver's exact command must be a citizen verb ----

    #[test]
    fn resolver_props_command_dispatches_through_the_citizen() {
        // B1 was exactly this drift: the resolver sent `blobd.props.get`,
        // the citizen routes only `blob.*`, and every remote resolution
        // answered "unknown blob verb". Push the resolver's command
        // string through Citizen::dispatch so the two can never drift
        // again.
        let (_dir, store) = bare_store("B");
        let lane: SocketAddr = "10.42.0.9:4210".parse().unwrap();
        let fetcher = Fetcher::new(
            Arc::clone(&store),
            FetchConfig::default(),
            Some(lane),
            "default".into(),
            Arc::new(crate::fetch::test_support::NullResolver),
            Arc::new(crate::fetch::test_support::NullPeers),
            Arc::new(TestSink::default()),
        );
        let citizen = Citizen::new(
            store,
            "blobd".into(),
            "default".into(),
            Some(lane),
            Arc::new(fetcher),
        );
        let (rc, body, _) = citizen.dispatch(&command(
            RESOLVER_PROPS_VERB,
            "maild",
            json!({"path": "lane"}),
        ));
        assert_eq!(rc, 0, "{body}");
        let reply: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(reply["bind"], "10.42.0.9:4210");

        // The addressing side of the same contract: instance targets
        // carry the service name Config::service_name mints.
        assert_eq!(resolver_to("alpha", None), "blobd.alpha");
        assert_eq!(resolver_to("alpha", Some("two")), "blobd-two.alpha");
        assert_eq!(
            resolver_to("alpha", Some("two")),
            format!(
                "{}.alpha",
                crate::core::config::Config::parse("name: two")
                    .unwrap()
                    .service_name()
            )
        );
    }

    // ---- The main lane: A holds ≥ 32 MiB, B fetches ----

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn fetch_pulls_32mib_from_a_lane_to_b_and_pins() {
        // Node A: a real lane holding a ≥ 32 MiB blob (above
        // MAX_MESSAGE_BYTES 16 MiB, so it can never ride a Bus frame).
        let (dir_a, store_a, addr_a, _lane_a) = counted_lane(options_for("A")).await;
        let bytes = pseudo_random(32 * 1024 * 1024 + 123, 0x5EED);
        let src = dir_a.path().join("shot.png");
        std::fs::write(&src, &bytes).unwrap();
        let outcome = store_a.put(&src, &PutOptions::new("filesd")).unwrap();
        let hash = outcome.reference.hash;
        assert_eq!(hash, blob::hash_bytes(&bytes));
        let id = reference::blob_id(&hash);

        // Node B: a store plus a citizen whose resolver maps A's node
        // name to its loopback lane.
        let (_dir_b, store_b) = bare_store("B");
        let resolver = MapResolver::default().map("A", format!("http://{addr_a}"));
        let (citizen, fetcher, sink) =
            fetch_setup(Arc::clone(&store_b), None, resolver, ListPeers::default()).await;

        // The immediate reply: accepted, not present, not joined.
        let (rc, body, _) = citizen.dispatch(&command(
            "blob.fetch",
            "maild",
            json!({"blob": outcome.reference.to_json()}),
        ));
        assert_eq!(rc, 0, "{body}");
        let reply: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(reply["accepted"], true);
        assert_eq!(reply["blob"], id.as_str());
        assert_eq!(reply["origin"], "A");
        assert_eq!(reply["in_flight"], false);
        assert_eq!(reply["present"], false);

        // Completion: blob.fetched ok from A.
        let event = sink
            .wait(
                TOPIC_FETCHED,
                |v| v["blob"] == id.as_str(),
                Duration::from_secs(60),
            )
            .await
            .expect("blob.fetched ok");
        assert_eq!(event["outcome"], "ok");
        assert_eq!(event["origin_used"], "A");
        assert_eq!(event["size"], bytes.len() as u64);

        // The stat transition, byte equality, the pin, the attrs.
        let stat = store_b.stat(&hash).unwrap();
        assert!(stat.present);
        assert_eq!(stat.pins, vec!["maild".to_string()]);
        assert_eq!(stat.mime.as_deref(), Some("image/png"));
        assert_eq!(stat.origin.as_deref(), Some("A"));
        assert_eq!(blob::get(&store_b.blobs_root(), &hash).unwrap(), bytes);
        assert_eq!(
            blob::hash_bytes(&blob::get(&store_b.blobs_root(), &hash).unwrap()),
            hash
        );

        // Quota accounted for the fetching owner; gauges settled.
        let quota = store_b.quota_report(None).unwrap();
        assert_eq!(quota.owners["maild"].used, bytes.len() as u64);
        assert_eq!(quota.total.used, bytes.len() as u64);
        let gauges = fetcher.gauges();
        assert_eq!(
            (
                gauges.in_flight,
                gauges.queued,
                gauges.completed,
                gauges.failed
            ),
            (0, 0, 1, 0)
        );
        assert!(tmp_is_empty(&store_b));

        // The blob.pinned event rode along (what §4.3's replicate
        // policy would trigger on), retain:false by construction.
        let pinned = sink.bodies(crate::citizen::TOPIC_PINNED);
        assert_eq!(pinned.len(), 1);
        assert_eq!(pinned[0]["owner"], "maild");
    }

    // ---- Single-flight: a second fetch joins, one GET ----

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn second_fetch_of_an_in_flight_hash_joins_it() {
        // A scripted origin that serves one byte, then holds until
        // released — a download slow enough to join deterministically.
        let bytes = pseudo_random(64 * 1024, 0x51E6);
        let hash = blob::hash_bytes(&bytes);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicU64::new(0));
        let got_get = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        {
            let hits = Arc::clone(&hits);
            let got_get = Arc::clone(&got_get);
            let release = Arc::clone(&release);
            tokio::spawn(async move {
                while let Ok((mut stream, _)) = listener.accept().await {
                    let head = read_head(&mut stream).await;
                    if !head.starts_with("GET") {
                        continue;
                    }
                    hits.fetch_add(1, Ordering::Relaxed);
                    got_get.notify_one();
                    let body = bytes.clone();
                    let release = Arc::clone(&release);
                    tokio::spawn(async move {
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        stream.write_all(head.as_bytes()).await.unwrap();
                        stream.write_all(&body[..1]).await.unwrap();
                        release.notified().await;
                        stream.write_all(&body[1..]).await.unwrap();
                    });
                }
            });
        }

        let (_dir_b, store_b) = bare_store("B");
        let id = reference::blob_id(&hash);
        let resolver = MapResolver::default().map("A", format!("http://{addr}"));
        let (citizen, fetcher, sink) =
            fetch_setup(Arc::clone(&store_b), None, resolver, ListPeers::default()).await;

        // First fetch starts the download (in_flight: false)…
        let (rc, body, _) = citizen.dispatch(&command(
            "blob.fetch",
            "maild",
            json!({"blob": id, "from": "A"}),
        ));
        assert_eq!(rc, 0, "{body}");
        let reply: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(reply["in_flight"], false);
        assert_eq!(reply["present"], false);

        // …the origin is mid-body when the second fetch joins.
        got_get.notified().await;
        let (rc, body, _) = citizen.dispatch(&command(
            "blob.fetch",
            "filesd",
            json!({"blob": id, "from": "A"}),
        ));
        assert_eq!(rc, 0, "{body}");
        let reply: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(reply["in_flight"], true);
        assert_eq!(reply["present"], false);

        release.notify_one();
        let event = sink
            .wait(
                TOPIC_FETCHED,
                |v| v["blob"] == id.as_str(),
                Duration::from_secs(30),
            )
            .await
            .expect("blob.fetched ok");
        assert_eq!(event["outcome"], "ok");

        // Exactly one GET hit the origin; both joiners pinned.
        assert_eq!(hits.load(Ordering::Relaxed), 1);
        let stat = store_b.stat(&hash).unwrap();
        assert!(stat.present);
        assert!(stat.pins.contains(&"maild".to_string()));
        assert!(stat.pins.contains(&"filesd".to_string()));
        assert_eq!(fetcher.gauges().completed, 1);
    }

    // ---- Origin unreachable → fan-out finds it on C ----

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn unreachable_origin_falls_back_to_a_peer() {
        // C (a real lane) holds the blob; D is a peer that does not.
        let (dir_c, store_c, addr_c, lane_c) = counted_lane(options_for("C")).await;
        let (_dir_d, store_d) = bare_store("D");
        let bytes = pseudo_random(256 * 1024, 0xF11A);
        let src = dir_c.path().join("data.bin");
        std::fs::write(&src, &bytes).unwrap();
        let outcome = store_c.put(&src, &PutOptions::new("filesd")).unwrap();
        let hash = outcome.reference.hash;
        let id = reference::blob_id(&hash);

        let (_dir_b, store_b) = bare_store("B");
        let resolver = MapResolver::default()
            .unreachable("A")
            .map("C", format!("http://{addr_c}"));
        let peers = ListPeers::default()
            .with_peer("C", Arc::clone(&store_c))
            .with_peer("D", Arc::clone(&store_d));
        let (citizen, _fetcher, sink) =
            fetch_setup(Arc::clone(&store_b), None, resolver, peers).await;

        // The reference says A; A is down; C answers the fan-out.
        let (rc, body, _) = citizen.dispatch(&command(
            "blob.fetch",
            "maild",
            json!({"blob": outcome.reference.to_json()}),
        ));
        assert_eq!(rc, 0, "{body}");
        let event = sink
            .wait(
                TOPIC_FETCHED,
                |v| v["blob"] == id.as_str(),
                Duration::from_secs(30),
            )
            .await
            .expect("blob.fetched ok");
        assert_eq!(event["outcome"], "ok");
        assert_eq!(event["origin_used"], "C");

        assert_eq!(lane_c.gets(), 1);
        let stat = store_b.stat(&hash).unwrap();
        assert!(stat.present);
        assert_eq!(stat.origin.as_deref(), Some("C"));
        assert_eq!(blob::get(&store_b.blobs_root(), &hash).unwrap(), bytes);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn nobody_holds_it_not_found_anywhere() {
        let (_dir_c, store_c) = bare_store("C");
        let (_dir_d, store_d) = bare_store("D");
        let (_dir_b, store_b) = bare_store("B");
        let resolver = MapResolver::default().unreachable("A");
        let peers = ListPeers::default()
            .with_peer("C", store_c)
            .with_peer("D", store_d);
        let (citizen, fetcher, sink) =
            fetch_setup(Arc::clone(&store_b), None, resolver, peers).await;

        let bytes = pseudo_random(4096, 0x0FF);
        let hash = blob::hash_bytes(&bytes);
        let id = reference::blob_id(&hash);
        let (rc, body, _) = citizen.dispatch(&command(
            "blob.fetch",
            "maild",
            json!({"blob": id, "from": "A"}),
        ));
        assert_eq!(rc, 0, "{body}");

        let event = sink
            .wait(
                TOPIC_FETCHED,
                |v| v["blob"] == id.as_str(),
                Duration::from_secs(30),
            )
            .await
            .expect("blob.fetched");
        assert_eq!(event["outcome"], "not_found_anywhere");
        assert_eq!(event["origin_used"], Value::Null);
        assert!(event["error"].as_str().is_some());

        // The CAS is untouched and the counters landed on `failed`.
        assert!(!blob::exists(&store_b.blobs_root(), &hash).unwrap());
        assert!(tmp_is_empty(&store_b));
        let gauges = fetcher.gauges();
        assert_eq!(
            (
                gauges.in_flight,
                gauges.queued,
                gauges.completed,
                gauges.failed
            ),
            (0, 0, 0, 1)
        );
    }

    // ---- Wrong bytes: verify_failed, terminal, no peer retry ----

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn wrong_bytes_verify_failed_is_terminal_and_never_retried() {
        // A hostile origin serves bytes that do not hash to the
        // requested id; peer C holds the real bytes and must never be
        // asked (verify is terminal, never retried).
        let real = pseudo_random(128 * 1024, 0x0BAD);
        let hash = blob::hash_bytes(&real);
        let id = reference::blob_id(&hash);
        let hostile = pseudo_random(128 * 1024, 0xD1CE);
        let hostile_hash = blob::hash_bytes(&hostile);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hostile_addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let _ = read_head(&mut stream).await;
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    hostile.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(&hostile).await;
            }
        });

        let (dir_c, store_c, addr_c, lane_c) = counted_lane(options_for("C")).await;
        let src = dir_c.path().join("real.bin");
        std::fs::write(&src, &real).unwrap();
        store_c.put(&src, &PutOptions::new("filesd")).unwrap();

        let (_dir_b, store_b) = bare_store("B");
        let resolver = MapResolver::default()
            .map("A", format!("http://{hostile_addr}"))
            .map("C", format!("http://{addr_c}"));
        let peers = ListPeers::default().with_peer("C", Arc::clone(&store_c));
        let (citizen, _fetcher, sink) =
            fetch_setup(Arc::clone(&store_b), None, resolver, peers).await;

        let (rc, body, _) = citizen.dispatch(&command(
            "blob.fetch",
            "maild",
            json!({"blob": id, "from": "A"}),
        ));
        assert_eq!(rc, 0, "{body}");
        let event = sink
            .wait(
                TOPIC_FETCHED,
                |v| v["blob"] == id.as_str(),
                Duration::from_secs(30),
            )
            .await
            .expect("blob.fetched");
        assert_eq!(event["outcome"], "verify_failed");
        assert!(event["error"].as_str().unwrap().contains("hash mismatch"));

        // Terminal: the peer that holds the real bytes was never
        // asked, and neither hash has a CAS entry (put_reader_expect
        // rejected the body before any commit; staging is empty).
        assert_eq!(lane_c.gets(), 0, "verify_failed must not retry a peer");
        assert!(!blob::exists(&store_b.blobs_root(), &hash).unwrap());
        assert!(!blob::exists(&store_b.blobs_root(), &hostile_hash).unwrap());
        assert!(tmp_is_empty(&store_b));
        assert_eq!(store_b.quota_report(None).unwrap().total.used, 0);
    }

    // ---- F2: a failed pin publishes io, never a pinless ok ----

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_failed_record_fetch_publishes_io_never_a_pinless_ok() {
        // The download lands and then record_fetch fails every time:
        // the completion must be outcome io — the old code swallowed
        // the failure (.unwrap_or_default()) and published `ok` for a
        // blob no pin protected, so the first blob.gc swept it.
        let (dir_a, store_a, addr_a, _lane_a) = counted_lane(options_for("A")).await;
        let bytes = pseudo_random(64 * 1024, 0x71A1);
        let src = dir_a.path().join("pinfail.bin");
        std::fs::write(&src, &bytes).unwrap();
        let outcome = store_a.put(&src, &PutOptions::new("filesd")).unwrap();
        let hash = outcome.reference.hash;
        let id = reference::blob_id(&hash);

        let (_dir_b, store_b) = bare_store("B");
        store_b.fail_record_fetch.store(true, Ordering::Relaxed);
        let resolver = MapResolver::default().map("A", format!("http://{addr_a}"));
        let (citizen, fetcher, sink) =
            fetch_setup(Arc::clone(&store_b), None, resolver, ListPeers::default()).await;

        let (rc, body, _) = citizen.dispatch(&command(
            "blob.fetch",
            "maild",
            json!({"blob": id, "from": "A"}),
        ));
        assert_eq!(rc, 0, "{body}");
        let event = sink
            .wait(
                TOPIC_FETCHED,
                |v| v["blob"] == id.as_str(),
                Duration::from_secs(30),
            )
            .await
            .expect("blob.fetched fired");
        assert_eq!(event["outcome"], "io");
        assert!(event["error"].as_str().unwrap().contains("pin"));
        assert_eq!(event["origin_used"], Value::Null);

        // Never an ok, never a pinned event; the bytes sit unpinned
        // (GC's grace covers them) and the counter landed on failed.
        assert!(
            sink.bodies(TOPIC_FETCHED)
                .iter()
                .all(|v| v["outcome"] != "ok")
        );
        assert!(sink.bodies(crate::citizen::TOPIC_PINNED).is_empty());
        assert!(blob::exists(&store_b.blobs_root(), &hash).unwrap());
        assert!(store_b.stat(&hash).unwrap().pins.is_empty());
        let gauges = fetcher.gauges();
        assert_eq!((gauges.in_flight, gauges.queued, gauges.completed, gauges.failed), (0, 0, 0, 1));

        // Recovery: with the injection off, a re-fetch finds the bytes
        // present and pins them — the reply is the completion.
        store_b.fail_record_fetch.store(false, Ordering::Relaxed);
        let (rc, body, _) = citizen.dispatch(&command("blob.fetch", "maild", json!({"blob": id})));
        assert_eq!(rc, 0, "{body}");
        let reply: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(reply["present"], true);
        assert_eq!(store_b.stat(&hash).unwrap().pins, vec!["maild".to_string()]);
    }

    // ---- F7: events published while disconnected wait for reconnect ----

    #[test]
    fn pending_events_hold_order_and_bound() {
        let pending = PendingEvents::default();
        for i in 0..300 {
            pending.push(domain_event(TOPIC_FETCHED, json!({"i": i})));
        }
        assert_eq!(pending.len(), PendingEvents::CAP, "the backlog is bounded");
        let drained = pending.drain();
        // The oldest 44 dropped; replay order is still publication order.
        assert_eq!(
            drained.first().and_then(|e| {
                serde_json::from_str::<Value>(&e.message.body).ok()
            }),
            Some(json!({"i": 44}))
        );
        assert_eq!(
            drained.last().and_then(|e| {
                serde_json::from_str::<Value>(&e.message.body).ok()
            }),
            Some(json!({"i": 299}))
        );
        assert!(pending.drain().is_empty(), "drain empties the backlog");
    }

    #[tokio::test]
    async fn bus_sink_buffers_while_disconnected() {
        // With no connection the event is held for the next
        // set(Some), not dropped; the drain the reconnect performs
        // (ClientSlot::set's replay) picks it up. Publishing it needs
        // a live broker, which a unit test never touches — the live
        // delivery is the hub gate's arms 1/2/3/9.
        let slot = ClientSlot::default();
        let sink = BusSink::new(slot.clone());
        sink.publish(domain_event(TOPIC_FETCHED, json!({"i": 1}))).await;
        sink.publish(domain_event(TOPIC_FETCHED, json!({"i": 2}))).await;
        assert_eq!(slot.pending_len(), 2);
        let backlog = slot.drain_pending_for_test();
        assert_eq!(backlog.len(), 2);
        assert_eq!(backlog[0].topic, TOPIC_FETCHED);
        assert_eq!(
            serde_json::from_str::<Value>(&backlog[0].message.body).unwrap(),
            json!({"i": 1})
        );
    }

    // ---- Quota: refused from Content-Length before the first byte ----

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn quota_refused_from_content_length_before_any_byte() {
        // The origin declares 1 MiB and never sends a body: a
        // mid-stream implementation would hang on the 30 s idle
        // timeout instead, so a prompt `quota` proves the pre-check.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let _ = read_head(&mut stream).await;
                let head = "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                            Content-Length: 1048576\r\nConnection: close\r\n\r\n";
                let _ = stream.write_all(head.as_bytes()).await;
                // No body, ever.
            }
        });

        let capped = StoreOptions {
            owner_limits: BTreeMap::from([("maild".to_string(), 4 * 1024)]),
            ..options_for("B")
        };
        let dir = TempDir::new().unwrap();
        let store_b = Arc::new(Store::open(dir.path(), capped).unwrap());

        let bytes = pseudo_random(1024, 0x900D);
        let hash = blob::hash_bytes(&bytes);
        let id = reference::blob_id(&hash);
        let resolver = MapResolver::default().map("A", format!("http://{addr}"));
        let (citizen, fetcher, sink) =
            fetch_setup(Arc::clone(&store_b), None, resolver, ListPeers::default()).await;

        let (rc, body, _) = citizen.dispatch(&command(
            "blob.fetch",
            "maild",
            json!({"blob": id, "from": "A"}),
        ));
        assert_eq!(rc, 0, "{body}");
        let event = sink
            .wait(
                TOPIC_FETCHED,
                |v| v["blob"] == id.as_str(),
                // Well under the 30 s idle bound: the refusal must be
                // prompt because no byte is ever read.
                Duration::from_secs(10),
            )
            .await
            .expect("blob.fetched quota");
        assert_eq!(event["outcome"], "quota");
        assert!(event["error"].as_str().unwrap().contains("quota"));

        assert!(!blob::exists(&store_b.blobs_root(), &hash).unwrap());
        assert!(tmp_is_empty(&store_b));
        assert_eq!(store_b.quota_report(None).unwrap().owners["maild"].used, 0);
        assert_eq!(fetcher.gauges().failed, 1);
    }

    // ---- The queue bound: busy, never an unbounded queue ----

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn beyond_the_queue_bound_the_verb_answers_busy() {
        // Two slots held by gated downloads, a queue of one, and a
        // fourth fetch refused busy. The release is a watch latch so a
        // download that starts after the release still completes.
        let (release_tx, release_rx) = tokio::sync::watch::channel(false);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        {
            let release_rx = release_rx.clone();
            tokio::spawn(async move {
                while let Ok((mut stream, _)) = listener.accept().await {
                    let _ = read_head(&mut stream).await;
                    let mut released = release_rx.clone();
                    tokio::spawn(async move {
                        let head = "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                                    Content-Length: 10\r\nConnection: close\r\n\r\n";
                        let _ = stream.write_all(head.as_bytes()).await;
                        let _ = stream.write_all(b"x").await;
                        while !*released.borrow_and_update() {
                            if released.changed().await.is_err() {
                                return;
                            }
                        }
                        let _ = stream.write_all(b"123456789").await;
                    });
                }
            });
        }

        let (_dir_b, store_b) = bare_store("B");
        let resolver = MapResolver::default().map("A", format!("http://{addr}"));
        let sink = TestSink::default();
        let config = FetchConfig {
            max_concurrent: 2,
            queue_max: 1,
        };
        let fetcher = Fetcher::new(
            Arc::clone(&store_b),
            config,
            None,
            "default".into(),
            Arc::new(resolver),
            Arc::new(ListPeers::default()),
            Arc::new(sink.clone()),
        );
        let citizen = Citizen::new(
            Arc::clone(&store_b),
            "blobd".into(),
            "default".into(),
            None,
            Arc::new(fetcher.clone()),
        );
        fetcher.spawn_dispatcher().await;

        let id_of = |seed: u64| reference::blob_id(&blob::hash_bytes(&pseudo_random(64, seed)));
        let mut ids = Vec::new();
        for seed in [0x11, 0x22, 0x33, 0x44] {
            let id = id_of(seed);
            ids.push(id.clone());
            let (rc, body, _) = citizen.dispatch(&command(
                "blob.fetch",
                "maild",
                json!({"blob": id, "from": "A"}),
            ));
            match seed {
                0x11 | 0x22 => {
                    assert_eq!(rc, 0, "{body}");
                    assert_eq!(
                        serde_json::from_str::<Value>(&body).unwrap()["in_flight"],
                        false
                    );
                }
                0x33 => {
                    // Queued behind the two running downloads.
                    assert_eq!(rc, 0, "{body}");
                    assert_eq!(
                        serde_json::from_str::<Value>(&body).unwrap()["in_flight"],
                        true
                    );
                }
                _ => {
                    // The queue (depth 1) is full: busy, rc 10.
                    assert_eq!(rc, 10, "{body}");
                    assert!(body.contains("busy"), "{body}");
                }
            }
        }

        // Release: all three admitted fetches complete (the queued
        // hash lands too, and no bytes hash to their claimed ids —
        // verify_failed, which is exactly the terminal path).
        release_tx.send(true).unwrap();
        for id in &ids[..3] {
            let event = sink
                .wait(
                    TOPIC_FETCHED,
                    |v| v["blob"] == id.as_str(),
                    Duration::from_secs(30),
                )
                .await
                .expect("blob.fetched for an admitted hash");
            assert_eq!(event["outcome"], "verify_failed");
        }
        let gauges = fetcher.gauges();
        assert_eq!(
            (
                gauges.in_flight,
                gauges.queued,
                gauges.completed,
                gauges.failed
            ),
            (0, 0, 0, 3)
        );
    }
}
