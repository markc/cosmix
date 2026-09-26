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
//! mesh-open `blobd.props.get {path:"lane"}` (or `blobd-<name>.props.get`
//! when the reference carries an instance) addressed through the local
//! noded as `blobd[.<instance>].<node>`; on `Service 'blobd' not
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

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

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
/// Production wires noded: a mesh-open `blobd.props.get {path:"lane"}`
/// addressed `blobd[.<instance>].<node>` through the local broker.
/// Every failure is "this source is unreachable" — the caller falls
/// back.
pub trait Resolver: Send + Sync {
    fn lane_url<'a>(
        &'a self,
        node: &'a str,
        instance: Option<&'a str>,
    ) -> BoxFuture<'a, Result<String, String>>;
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
/// lives across reconnects. `None` between connections — a fetch
/// completing then drops its events with a log line (the `blob.stat`
/// transition still happened), and resolution falls back.
#[derive(Clone, Default)]
pub struct ClientSlot(Arc<RwLock<Option<Arc<NodedClient>>>>);

impl ClientSlot {
    pub fn set(&self, client: Option<Arc<NodedClient>>) {
        *self.0.write().unwrap() = client;
    }

    fn get(&self) -> Option<Arc<NodedClient>> {
        self.0.read().unwrap().clone()
    }
}

// ---- Production wiring (noded through the client slot) ----

/// Resolves lane URLs with `blobd.props.get` through the local noded.
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
            let service = match instance {
                Some(name) => format!("blobd-{name}"),
                None => "blobd".to_string(),
            };
            let to = format!("{service}.{node}");
            let reply = tokio::time::timeout(
                BUS_CALL_TIMEOUT,
                client.call_typed(&to, "blobd.props.get", json!({"path": "lane"})),
            )
            .await
            .map_err(|_| format!("blobd.props.get on {to} timed out"))?
            .map_err(|e| format!("blobd.props.get on {to}: {e}"))?;
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
/// connection; between connections the event is dropped with a log
/// line (best-effort, exactly like the citizen's own publish path).
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
                eprintln!(
                    "cosmix-blobd: fetch event on {} dropped (no broker connection)",
                    event.topic
                );
                return;
            };
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
        })
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
            return Attempt::ok(None, size, "application/octet-stream".to_string());
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
                SourceOutcome::Fetched { size, mime } => {
                    return Attempt::ok(Some(target.node), size, mime);
                }
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
                    SourceOutcome::Fetched { size, mime } => {
                        return Attempt::ok(Some(peer.clone()), size, mime);
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

        // Quota before the first byte; a lying (or absent) length meets
        // the same limit as the mid-stream counter below.
        let cap = match self.store.upload_cap(owner) {
            Ok(cap) => cap,
            Err(error) => return SourceOutcome::Local(error.to_string()),
        };
        if let Some(len) = response.content_length()
            && len > cap
        {
            return SourceOutcome::Quota(format!(
                "quota: body of {len} bytes exceeds the remaining cap of {cap}"
            ));
        }
        let mime = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.split(';').next().unwrap_or(v).trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "application/octet-stream".to_string());

        let started = SystemTime::now();
        let (tx, rx) = mpsc::channel::<Frame>(PUMP_FRAMES);
        let pump = tokio::spawn(pump_response(response, tx, cap));
        let reader_store = Arc::clone(&self.store);
        let expected = *hash;
        let landed = tokio::task::spawn_blocking(move || {
            blob::put_reader(&reader_store.blobs_root(), ChannelReader::new(rx))
        })
        .await;
        let _ = pump.await;

        match landed {
            Ok(Ok((landed, size))) => {
                if landed != expected {
                    // The bytes landed under their own (wrong) hash;
                    // remove that entry when this fetch created it and
                    // nothing pins or describes it.
                    self.store.discard_recently_created(&landed, started);
                    return SourceOutcome::Verify { landed };
                }
                SourceOutcome::Fetched { size, mime }
            }
            Ok(Err(error)) => {
                // The abort reason rode inside the io::Error that
                // stopped the stream; put_reader has already deleted
                // the staging file by the time it surfaces here.
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
    async fn complete(self: &Arc<Self>, hash: BlobHash, attempt: Attempt, owners: Vec<String>) {
        let id = reference::blob_id(&hash);
        let mut events = Vec::new();
        if attempt.outcome == FetchOutcome::Ok {
            let before = self.props_input();
            let newly_pinned = self
                .store
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
                .unwrap_or_default();
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
}

impl Attempt {
    fn ok(origin_used: Option<String>, size: u64, mime: String) -> Self {
        Self {
            outcome: FetchOutcome::Ok,
            origin_used,
            size: Some(size),
            mime: Some(mime),
            error: None,
        }
    }

    fn terminal(source: SourceOutcome) -> Self {
        let (outcome, error) = match source {
            SourceOutcome::Verify { landed } => (
                FetchOutcome::VerifyFailed,
                format!(
                    "hash mismatch: the body hashes to {}, not the requested id",
                    reference::blob_id(&landed)
                ),
            ),
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
        }
    }
}

/// What one source said.
#[derive(Debug)]
enum SourceOutcome {
    Fetched {
        size: u64,
        mime: String,
    },
    /// Bytes landed under the wrong hash; terminal, never retried.
    Verify {
        landed: BlobHash,
    },
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
/// channel inside an `io::Error` so `put_reader`'s error path (which
/// deletes the staging file) runs before the classifier sees it.
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
}
