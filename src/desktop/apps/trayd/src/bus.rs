//! Event-driven Bus bridge owned by trayd.
//!
//! D-Bus clients hold sender-bound leases. A dedicated Tokio thread owns two
//! independently supervised broker connections while at least one lease is
//! active: a low-volume roster plane and a fenced observation plane.

use std::collections::{BTreeSet, HashMap, VecDeque};
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver as PublishReceiver, SyncSender, TrySendError};
#[cfg(test)]
use std::sync::Condvar;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use cosmix_bus::RegisterProvenance;
use cosmix_client::{IncomingCommand, NodedClient};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, watch};
use zbus::blocking::{Connection as BusConnection, MessageIterator, Proxy as BusProxy};
use zbus::message::Type as MessageType;
use zbus::MatchRule;

const LEASE_TTL: Duration = Duration::from_secs(10 * 60);
const RECONNECT_BACKSTOP: Duration = Duration::from_secs(5 * 60);
const NODED_UNIT: &str = "cosmix-noded.service";
const OBSERVE_CAPACITY: usize = 1024;
const TRAFFIC_RING_CAPACITY: usize = 2048;
const SNAPSHOT_EVENT_LIMIT: usize = 128;
const SNAPSHOT_BYTE_LIMIT: usize = 512 * 1024;
const SIGNAL_EVENT_LIMIT: usize = 64;
const SIGNAL_BYTE_LIMIT: usize = 256 * 1024;
const PAYLOAD_BYTE_LIMIT: usize = 16 * 1024;
const OBSERVATION_ENVELOPE_LIMIT: usize = 1024 * 1024;
const REFRESH_QUEUE_CAPACITY: usize = 8;
const MAX_FILTER_BYTES: usize = 128;
const WORLD_NODED_TOPIC: &str = "world.noded";
#[cfg(not(test))]
const LIFECYCLE_READY_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(test)]
const LIFECYCLE_READY_TIMEOUT: Duration = Duration::from_millis(500);

pub(crate) type WireTraffic = (
    u64,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    bool,
    i64,
    u64,
    u64,
    String,
    String,
);
pub(crate) type WireNode = (String, String, bool, String);
#[derive(serde::Serialize, zbus::zvariant::Type)]
pub(crate) struct WireBusSnapshot {
    revision: u64,
    state: String,
    error: String,
    observing: bool,
    filter_epoch: u64,
    effective_directions: Vec<String>,
    effective_verbs: Vec<String>,
    body_mode: String,
    inventory_posture: String,
    nodes: Vec<WireNode>,
    local_services: Vec<String>,
    traffic: Vec<WireTraffic>,
    server_dropped: u64,
    bridge_dropped: u64,
}

#[derive(Debug, zbus::DBusError, PartialEq, Eq)]
#[zbus(prefix = "dev.cosmix.trayd.Error", impl_display = true)]
pub(crate) enum BusError {
    UnknownBusSession(String),
    BadBusFilter(String),
    BusUnavailable(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LeaseFilter {
    directions: Vec<String>,
    verb: String,
    body: BodyMode,
}

impl LeaseFilter {
    fn parse(directions: Vec<String>, verb: String, body_mode: String) -> Result<Self, BusError> {
        if directions.is_empty() {
            return Err(BusError::BadBusFilter(
                "at least one direction is required".into(),
            ));
        }
        let mut directions = directions;
        if directions
            .iter()
            .any(|direction| !matches!(direction.as_str(), "local" | "mesh_in" | "mesh_out"))
        {
            return Err(BusError::BadBusFilter(
                "directions must be local, mesh_in, or mesh_out".into(),
            ));
        }
        directions.sort();
        directions.dedup();
        if !valid_glob(&verb) {
            return Err(BusError::BadBusFilter(
                "verb_glob must be a non-empty anchored Bus glob of at most 128 bytes".into(),
            ));
        }
        let body = BodyMode::parse(&body_mode)?;
        Ok(Self {
            directions,
            verb,
            body,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum BodyMode {
    #[default]
    None,
    Redacted,
}

impl BodyMode {
    fn parse(value: &str) -> Result<Self, BusError> {
        match value {
            "none" => Ok(Self::None),
            "redacted" => Ok(Self::Redacted),
            _ => Err(BusError::BadBusFilter(
                "body_mode must be none or redacted".into(),
            )),
        }
    }

    const fn wire(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Redacted => "redacted",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct EffectiveFilter {
    directions: Vec<String>,
    verbs: Vec<String>,
    body: BodyMode,
}

impl EffectiveFilter {
    fn from_leases(leases: impl Iterator<Item = LeaseFilter>) -> Self {
        let mut directions = BTreeSet::new();
        let mut verbs = BTreeSet::new();
        let mut body = BodyMode::None;
        for filter in leases {
            directions.extend(filter.directions);
            verbs.insert(filter.verb);
            if filter.body == BodyMode::Redacted {
                body = BodyMode::Redacted;
            }
        }
        Self {
            directions: directions.into_iter().collect(),
            verbs: verbs.into_iter().collect(),
            body,
        }
    }

    fn start_body(&self) -> String {
        json!({
            "filter": {
                "verbs": self.verbs,
                "services": [],
                "directions": self.directions,
            },
            "body": self.body.wire(),
            "capacity": OBSERVE_CAPACITY,
        })
        .to_string()
    }
}

fn valid_glob(pattern: &str) -> bool {
    !pattern.is_empty()
        && pattern.len() <= MAX_FILTER_BYTES
        && pattern
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'*'))
}

#[derive(Clone, Debug)]
struct Lease {
    owner: String,
    filter: LeaseFilter,
    expires_at: Instant,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DesiredState {
    active: bool,
    activation: u64,
    filter_epoch: u64,
    filter: EffectiveFilter,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct NodeRecord {
    name: String,
    mesh_ip: String,
    bus: bool,
    status: String,
}

impl NodeRecord {
    fn wire(&self) -> WireNode {
        (
            self.name.clone(),
            self.mesh_ip.clone(),
            self.bus,
            self.status.clone(),
        )
    }
}

#[derive(Debug, Deserialize)]
struct Inventory {
    posture: String,
    #[serde(default)]
    members: Vec<NodeRecord>,
}

#[derive(Clone, Debug, Deserialize)]
struct ObserveStartReply {
    subscription_id: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ObservedEvent {
    seq: u64,
    ts: String,
    direction: String,
    outcome: String,
    message_type: String,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    verb: Option<String>,
    size: u64,
    #[serde(default)]
    correlation_id: Option<String>,
    #[serde(default)]
    rc: Option<i64>,
    #[serde(default)]
    dropped_count: u64,
    #[serde(default)]
    payload: Option<Value>,
    #[serde(default)]
    payload_omitted: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TrafficEvent {
    publish_id: u64,
    seq: u64,
    timestamp: String,
    direction: String,
    outcome: String,
    message_type: String,
    from: String,
    to: String,
    verb: String,
    correlation_id: String,
    rc: Option<i64>,
    size: u64,
    broker_dropped: u64,
    payload_json: String,
    payload_omitted: String,
}

impl TrafficEvent {
    fn from_observed(event: ObservedEvent) -> Self {
        let (payload_json, payload_omitted) = match event.payload {
            Some(payload) => {
                let encoded = serde_json::to_string(&payload).unwrap_or_default();
                if encoded.len() <= PAYLOAD_BYTE_LIMIT {
                    (encoded, event.payload_omitted.unwrap_or_default())
                } else {
                    (String::new(), "plasmoid_limit".into())
                }
            }
            None => (String::new(), event.payload_omitted.unwrap_or_default()),
        };
        Self {
            publish_id: 0,
            seq: event.seq,
            timestamp: event.ts,
            direction: event.direction,
            outcome: event.outcome,
            message_type: event.message_type,
            from: event.from.unwrap_or_default(),
            to: event.to.unwrap_or_default(),
            verb: event.verb.unwrap_or_default(),
            correlation_id: event.correlation_id.unwrap_or_default(),
            rc: event.rc,
            size: event.size,
            broker_dropped: event.dropped_count,
            payload_json,
            payload_omitted,
        }
    }

    fn wire(&self) -> WireTraffic {
        (
            self.seq,
            self.timestamp.clone(),
            self.direction.clone(),
            self.outcome.clone(),
            self.message_type.clone(),
            self.from.clone(),
            self.to.clone(),
            self.verb.clone(),
            self.correlation_id.clone(),
            self.rc.is_some(),
            self.rc.unwrap_or_default(),
            self.size,
            self.broker_dropped,
            self.payload_json.clone(),
            self.payload_omitted.clone(),
        )
    }

    fn estimated_bytes(&self) -> usize {
        // Includes a conservative allowance for the D-Bus struct, scalar
        // alignment, string length fields and terminating NULs.
        128usize
            .saturating_add(self.timestamp.len())
            .saturating_add(self.direction.len())
            .saturating_add(self.outcome.len())
            .saturating_add(self.message_type.len())
            .saturating_add(self.from.len())
            .saturating_add(self.to.len())
            .saturating_add(self.verb.len())
            .saturating_add(self.correlation_id.len())
            .saturating_add(self.payload_json.len())
            .saturating_add(self.payload_omitted.len())
    }
}

struct BusState {
    leases: HashMap<String, Lease>,
    desired: DesiredState,
    revision: u64,
    status: String,
    roster_error: String,
    observe_error: String,
    lifecycle_error: String,
    roster_connected: bool,
    observe_connected: bool,
    observing: bool,
    roster_generation: u64,
    observe_generation: u64,
    subscription_id: Option<String>,
    inventory_posture: String,
    nodes: Vec<NodeRecord>,
    local_services: Vec<String>,
    traffic: VecDeque<TrafficEvent>,
    next_publish_id: u64,
    last_batch_publish_id: u64,
    last_changed_revision: u64,
    server_dropped: u64,
    transport_dropped: u64,
    malformed_dropped: u64,
    ring_dropped: u64,
}

impl Default for BusState {
    fn default() -> Self {
        Self {
            leases: HashMap::new(),
            desired: DesiredState::default(),
            revision: 0,
            status: "idle".into(),
            roster_error: String::new(),
            observe_error: String::new(),
            lifecycle_error: String::new(),
            roster_connected: false,
            observe_connected: false,
            observing: false,
            roster_generation: 0,
            observe_generation: 0,
            subscription_id: None,
            inventory_posture: "unknown".into(),
            nodes: Vec::new(),
            local_services: Vec::new(),
            traffic: VecDeque::new(),
            next_publish_id: 1,
            last_batch_publish_id: 0,
            last_changed_revision: 0,
            server_dropped: 0,
            transport_dropped: 0,
            malformed_dropped: 0,
            ring_dropped: 0,
        }
    }
}

impl BusState {
    fn active(&self) -> bool {
        !self.leases.is_empty()
    }

    fn error(&self) -> String {
        [
            self.roster_error.as_str(),
            self.observe_error.as_str(),
            self.lifecycle_error.as_str(),
        ]
        .into_iter()
        .filter(|error| !error.is_empty())
        .collect::<Vec<_>>()
        .join("; ")
    }

    fn bridge_dropped(&self) -> u64 {
        self.transport_dropped
            .saturating_add(self.malformed_dropped)
            .saturating_add(self.ring_dropped)
    }

    fn bump(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }

    fn recompute_status(&mut self) {
        if !self.active() {
            if !self.roster_connected && !self.observe_connected {
                self.status = "idle".into();
            }
            return;
        }
        self.status = if self.roster_connected
            && self.observe_connected
            && self.observing
            && self.lifecycle_error.is_empty()
        {
            "connected"
        } else if !self.error().is_empty() {
            "degraded"
        } else {
            "connecting"
        }
        .into();
    }

    fn recompute_desired(&mut self, was_active: bool, previous: &EffectiveFilter) -> bool {
        let active = self.active();
        let filter =
            EffectiveFilter::from_leases(self.leases.values().map(|lease| lease.filter.clone()));
        let changed = active != was_active || &filter != previous;
        if !changed {
            return false;
        }
        if !was_active && active {
            self.desired.activation = self.desired.activation.saturating_add(1);
            self.roster_connected = false;
            self.observe_connected = false;
            self.traffic.clear();
            self.nodes.clear();
            self.local_services.clear();
            self.server_dropped = 0;
            self.transport_dropped = 0;
            self.malformed_dropped = 0;
            self.ring_dropped = 0;
            self.roster_error.clear();
            self.observe_error.clear();
            self.inventory_posture = "unknown".into();
        }
        // Traffic is scoped to one effective filter. A last-close or any union
        // change fences already-collected rows as well as the live observe
        // subscription, so a later client cannot inherit stale metadata.
        self.traffic.clear();
        self.last_batch_publish_id = self.next_publish_id.saturating_sub(1);
        self.server_dropped = 0;
        self.transport_dropped = 0;
        self.malformed_dropped = 0;
        self.ring_dropped = 0;
        self.desired.active = active;
        self.desired.filter = filter;
        self.desired.filter_epoch = self.desired.filter_epoch.saturating_add(1);
        self.observing = false;
        self.subscription_id = None;
        if active {
            self.status = "connecting".into();
        } else if !self.roster_connected && !self.observe_connected {
            self.status = "idle".into();
        } else {
            self.status = "stopping".into();
        }
        self.bump();
        true
    }

    fn open(&mut self, owner: String, filter: LeaseFilter, now: Instant) -> (String, bool) {
        let was_active = self.active();
        let previous = self.desired.filter.clone();
        let session_id = uuid::Uuid::new_v4().to_string();
        self.leases.insert(
            session_id.clone(),
            Lease {
                owner,
                filter,
                expires_at: now + LEASE_TTL,
            },
        );
        let changed = self.recompute_desired(was_active, &previous);
        (session_id, changed)
    }

    fn update(
        &mut self,
        owner: &str,
        session_id: &str,
        filter: LeaseFilter,
        now: Instant,
    ) -> Result<bool, BusError> {
        let was_active = self.active();
        let previous = self.desired.filter.clone();
        let lease = self
            .leases
            .get_mut(session_id)
            .filter(|lease| lease.owner == owner)
            .ok_or_else(|| unknown_session(session_id))?;
        lease.filter = filter;
        lease.expires_at = now + LEASE_TTL;
        Ok(self.recompute_desired(was_active, &previous))
    }

    fn keep_alive(&mut self, owner: &str, session_id: &str, now: Instant) -> Result<(), BusError> {
        let lease = self
            .leases
            .get_mut(session_id)
            .filter(|lease| lease.owner == owner)
            .ok_or_else(|| unknown_session(session_id))?;
        lease.expires_at = now + LEASE_TTL;
        Ok(())
    }

    fn close(&mut self, owner: &str, session_id: &str) -> Result<bool, BusError> {
        if self
            .leases
            .get(session_id)
            .is_none_or(|lease| lease.owner != owner)
        {
            return Err(unknown_session(session_id));
        }
        let was_active = self.active();
        let previous = self.desired.filter.clone();
        self.leases.remove(session_id);
        Ok(self.recompute_desired(was_active, &previous))
    }

    fn remove_owner(&mut self, owner: &str) -> bool {
        let was_active = self.active();
        let previous = self.desired.filter.clone();
        self.leases.retain(|_, lease| lease.owner != owner);
        self.recompute_desired(was_active, &previous)
    }

    fn expire(&mut self, now: Instant) -> bool {
        let was_active = self.active();
        let previous = self.desired.filter.clone();
        self.leases.retain(|_, lease| lease.expires_at > now);
        self.recompute_desired(was_active, &previous)
    }

    fn next_expiry(&self) -> Option<Instant> {
        self.leases.values().map(|lease| lease.expires_at).min()
    }

    fn owns(&self, owner: &str, session_id: &str) -> bool {
        self.leases
            .get(session_id)
            .is_some_and(|lease| lease.owner == owner)
    }

    fn accepts_observation(&self, activation: u64, generation: u64, subscription: &str) -> bool {
        self.active()
            && self.desired.activation == activation
            && self.observe_generation == generation
            && self.subscription_id.as_deref() == Some(subscription)
            && self.observing
    }

    fn push_traffic(&mut self, mut event: TrafficEvent) {
        event.publish_id = self.next_publish_id;
        self.next_publish_id = self.next_publish_id.saturating_add(1);
        self.server_dropped = self.server_dropped.saturating_add(event.broker_dropped);
        if self.traffic.len() == TRAFFIC_RING_CAPACITY {
            self.traffic.pop_front();
            self.ring_dropped = self.ring_dropped.saturating_add(1);
        }
        self.traffic.push_back(event);
        self.bump();
    }

    fn wire_snapshot(&self) -> WireBusSnapshot {
        WireBusSnapshot {
            revision: self.revision,
            state: self.status.clone(),
            error: self.error(),
            observing: self.observing,
            filter_epoch: self.desired.filter_epoch,
            effective_directions: self.desired.filter.directions.clone(),
            effective_verbs: self.desired.filter.verbs.clone(),
            body_mode: self.desired.filter.body.wire().into(),
            inventory_posture: self.inventory_posture.clone(),
            nodes: self.nodes.iter().map(NodeRecord::wire).collect(),
            local_services: self.local_services.clone(),
            traffic: bounded_snapshot_events(&self.traffic),
            server_dropped: self.server_dropped,
            bridge_dropped: self.bridge_dropped(),
        }
    }
}

fn unknown_session(session_id: &str) -> BusError {
    BusError::UnknownBusSession(format!("unknown Bus session: {session_id}"))
}

struct WorkerChannels {
    desired: watch::Sender<DesiredState>,
    expiry: watch::Sender<Option<Instant>>,
    refresh: mpsc::Sender<u64>,
    noded_lifecycle: broadcast::Sender<()>,
}

pub(crate) struct BusPublication {
    pub revision: u64,
    pub filter_epoch: u64,
    pub events: Vec<WireTraffic>,
    pub server_dropped: u64,
    pub bridge_dropped: u64,
}

pub(crate) struct BusController {
    state: Mutex<BusState>,
    worker: Mutex<Option<WorkerChannels>>,
    startup: Mutex<()>,
    lifecycle_watcher_running: AtomicBool,
    publish_tx: SyncSender<()>,
    publish_rx: Mutex<Option<PublishReceiver<()>>>,
    #[cfg(test)]
    test_worker_receivers: Mutex<Option<TestWorkerReceivers>>,
    #[cfg(test)]
    test_open_gate: Mutex<Option<Arc<TestOpenGate>>>,
    #[cfg(test)]
    test_notification_gate: Mutex<Option<Arc<TestOpenGate>>>,
    #[cfg(test)]
    test_rollback_gate: Mutex<Option<Arc<TestOpenGate>>>,
    #[cfg(test)]
    test_lifecycle_start_gate: Mutex<Option<Arc<TestOpenGate>>>,
    #[cfg(test)]
    test_lifecycle_ready_gate: Mutex<Option<Arc<TestOpenGate>>>,
    #[cfg(test)]
    test_lifecycle_start_failures: AtomicUsize,
    #[cfg(test)]
    test_lifecycle_start_attempts: AtomicUsize,
    #[cfg(test)]
    test_worker_start_attempts: AtomicUsize,
}

#[cfg(test)]
struct TestWorkerReceivers {
    _desired: watch::Receiver<DesiredState>,
    _expiry: watch::Receiver<Option<Instant>>,
    _refresh: mpsc::Receiver<u64>,
}

#[cfg(test)]
pub(crate) struct TestOpenGate {
    entered: Mutex<bool>,
    entered_changed: Condvar,
    released: Mutex<bool>,
    released_changed: Condvar,
}

#[cfg(test)]
impl TestOpenGate {
    pub(crate) fn new() -> Self {
        Self {
            entered: Mutex::new(false),
            entered_changed: Condvar::new(),
            released: Mutex::new(false),
            released_changed: Condvar::new(),
        }
    }

    pub(crate) fn block(&self) {
        *self
            .entered
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
        self.entered_changed.notify_all();
        let released = self
            .released
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        drop(
            self.released_changed
                .wait_while(released, |released| !*released)
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
    }

    pub(crate) fn wait_until_entered(&self) {
        let entered = self
            .entered
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (entered, timeout) = self
            .entered_changed
            .wait_timeout_while(entered, Duration::from_secs(5), |entered| !*entered)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(
            *entered && !timeout.timed_out(),
            "test gate was not entered"
        );
    }

    pub(crate) fn release(&self) {
        *self
            .released
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
        self.released_changed.notify_all();
    }
}

impl BusController {
    pub(crate) fn new() -> Arc<Self> {
        let (publish_tx, publish_rx) = sync_channel(1);
        Arc::new(Self {
            state: Mutex::new(BusState::default()),
            worker: Mutex::new(None),
            startup: Mutex::new(()),
            lifecycle_watcher_running: AtomicBool::new(false),
            publish_tx,
            publish_rx: Mutex::new(Some(publish_rx)),
            #[cfg(test)]
            test_worker_receivers: Mutex::new(None),
            #[cfg(test)]
            test_open_gate: Mutex::new(None),
            #[cfg(test)]
            test_notification_gate: Mutex::new(None),
            #[cfg(test)]
            test_rollback_gate: Mutex::new(None),
            #[cfg(test)]
            test_lifecycle_start_gate: Mutex::new(None),
            #[cfg(test)]
            test_lifecycle_ready_gate: Mutex::new(None),
            #[cfg(test)]
            test_lifecycle_start_failures: AtomicUsize::new(0),
            #[cfg(test)]
            test_lifecycle_start_attempts: AtomicUsize::new(0),
            #[cfg(test)]
            test_worker_start_attempts: AtomicUsize::new(0),
        })
    }

    #[cfg(test)]
    pub(crate) fn new_test() -> Arc<Self> {
        let controller = Self::new();
        let (desired, desired_rx) = watch::channel(DesiredState::default());
        let (expiry, expiry_rx) = watch::channel(None);
        let (refresh, refresh_rx) = mpsc::channel(REFRESH_QUEUE_CAPACITY);
        let (noded_lifecycle, _) = broadcast::channel(1);
        *controller.lock_worker() = Some(WorkerChannels {
            desired,
            expiry,
            refresh,
            noded_lifecycle,
        });
        controller
            .lifecycle_watcher_running
            .store(true, Ordering::Release);
        *controller
            .test_worker_receivers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(TestWorkerReceivers {
            _desired: desired_rx,
            _expiry: expiry_rx,
            _refresh: refresh_rx,
        });
        controller
    }

    #[cfg(test)]
    pub(crate) fn block_next_open(&self) -> Arc<TestOpenGate> {
        let gate = Arc::new(TestOpenGate::new());
        *self
            .test_open_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(&gate));
        gate
    }

    #[cfg(test)]
    fn block_next_worker_notification(&self) -> Arc<TestOpenGate> {
        let gate = Arc::new(TestOpenGate::new());
        *self
            .test_notification_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(&gate));
        gate
    }

    #[cfg(test)]
    fn block_next_rollback(&self) -> Arc<TestOpenGate> {
        let gate = Arc::new(TestOpenGate::new());
        *self
            .test_rollback_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(&gate));
        gate
    }

    #[cfg(test)]
    fn block_next_lifecycle_start(&self) -> Arc<TestOpenGate> {
        let gate = Arc::new(TestOpenGate::new());
        *self
            .test_lifecycle_start_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(&gate));
        gate
    }

    #[cfg(test)]
    fn block_next_lifecycle_readiness(&self) -> Arc<TestOpenGate> {
        let gate = Arc::new(TestOpenGate::new());
        *self
            .test_lifecycle_ready_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(&gate));
        gate
    }

    #[cfg(test)]
    fn fail_next_lifecycle_start(&self) {
        self.test_lifecycle_start_failures
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn take_publish_receiver(&self) -> PublishReceiver<()> {
        self.publish_rx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .expect("Bus publisher receiver is taken once")
    }

    pub(crate) fn open(
        self: &Arc<Self>,
        owner: String,
        directions: Vec<String>,
        verb: String,
        body_mode: String,
    ) -> Result<String, BusError> {
        let filter = LeaseFilter::parse(directions, verb, body_mode)?;
        self.ensure_worker()?;
        #[cfg(test)]
        if let Some(gate) = self
            .test_open_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            gate.block();
        }
        let rollback_owner = owner.clone();
        let (session_id, changed, notified) = {
            let mut state = self.lock_state();
            let (session_id, changed) = state.open(owner, filter, Instant::now());
            let notified =
                self.notify_worker(state.next_expiry(), changed.then(|| state.desired.clone()));
            (session_id, changed, notified)
        };
        if let Err(error) = notified {
            #[cfg(test)]
            if let Some(gate) = self
                .test_rollback_gate
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
            {
                gate.block();
            }
            if let Err(rollback_error) = self.rollback_open(&rollback_owner, &session_id) {
                eprintln!(
                    "cosmix-trayd: cannot roll back failed Bus lease {session_id}: {rollback_error}"
                );
            }
            return Err(error);
        }
        if changed {
            self.schedule_publish();
        }
        Ok(session_id)
    }

    pub(crate) fn update(
        &self,
        owner: &str,
        session_id: &str,
        directions: Vec<String>,
        verb: String,
        body_mode: String,
    ) -> Result<(), BusError> {
        let filter = LeaseFilter::parse(directions, verb, body_mode)?;
        let (changed, notified) = {
            let mut state = self.lock_state();
            let changed = state.update(owner, session_id, filter, Instant::now())?;
            let notified =
                self.notify_worker(state.next_expiry(), changed.then(|| state.desired.clone()));
            (changed, notified)
        };
        notified?;
        if changed {
            self.schedule_publish();
        }
        Ok(())
    }

    pub(crate) fn keep_alive(&self, owner: &str, session_id: &str) -> Result<(), BusError> {
        let notified = {
            let mut state = self.lock_state();
            state.keep_alive(owner, session_id, Instant::now())?;
            self.notify_worker(state.next_expiry(), None)
        };
        notified
    }

    pub(crate) fn close(&self, owner: &str, session_id: &str) -> Result<(), BusError> {
        let (changed, notified) = {
            let mut state = self.lock_state();
            let changed = state.close(owner, session_id)?;
            let notified =
                self.notify_worker(state.next_expiry(), changed.then(|| state.desired.clone()));
            (changed, notified)
        };
        notified?;
        if changed {
            self.schedule_publish();
        }
        Ok(())
    }

    pub(crate) fn refresh_roster(&self, owner: &str, session_id: &str) -> Result<(), BusError> {
        let activation = {
            let state = self.lock_state();
            if !state.owns(owner, session_id) {
                return Err(unknown_session(session_id));
            }
            state.desired.activation
        };
        let worker = self
            .lock_worker()
            .as_ref()
            .ok_or_else(|| BusError::BusUnavailable("Bus worker is not running".into()))?
            .refresh
            .clone();
        worker.try_send(activation).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => {
                BusError::BusUnavailable("Bus roster refresh queue is full".into())
            }
            mpsc::error::TrySendError::Closed(_) => {
                BusError::BusUnavailable("Bus worker has stopped".into())
            }
        })
    }

    pub(crate) fn owner_lost(&self, owner: &str) {
        let changed = {
            let mut state = self.lock_state();
            let changed = state.remove_owner(owner);
            let _ = self.notify_worker(state.next_expiry(), changed.then(|| state.desired.clone()));
            changed
        };
        if changed {
            self.schedule_publish();
        }
    }

    pub(crate) fn snapshot(&self) -> WireBusSnapshot {
        self.lock_state().wire_snapshot()
    }

    pub(crate) fn active(&self) -> bool {
        self.lock_state().active()
    }

    pub(crate) fn status(&self) -> String {
        self.lock_state().status.clone()
    }

    pub(crate) fn revision(&self) -> u64 {
        self.lock_state().revision
    }

    pub(crate) fn take_publication(&self) -> Option<BusPublication> {
        let mut state = self.lock_state();
        let events = bounded_signal_events(&state.traffic, state.last_batch_publish_id);
        let newest_publish_id = events
            .last()
            .map(|event| event.0)
            .unwrap_or(state.last_batch_publish_id);
        let changed = state.revision != state.last_changed_revision;
        if !changed && events.is_empty() {
            return None;
        }
        state.last_changed_revision = state.revision;
        state.last_batch_publish_id = newest_publish_id;
        let more = state
            .traffic
            .back()
            .is_some_and(|event| event.publish_id > newest_publish_id);
        let publication = BusPublication {
            revision: state.revision,
            filter_epoch: state.desired.filter_epoch,
            events: events.into_iter().map(|(_, event)| event).collect(),
            server_dropped: state.server_dropped,
            bridge_dropped: state.bridge_dropped(),
        };
        drop(state);
        if more {
            self.schedule_publish();
        }
        Some(publication)
    }

    fn ensure_worker(self: &Arc<Self>) -> Result<(), BusError> {
        let _startup = self
            .startup
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let existing_lifecycle = self
            .lock_worker()
            .as_ref()
            .map(|worker| worker.noded_lifecycle.clone());
        if let Some(noded_tx) = existing_lifecycle {
            if !self.lifecycle_watcher_running.load(Ordering::Acquire) {
                let readiness = self.start_lifecycle_watcher(noded_tx);
                let _ = readiness.recv_timeout(LIFECYCLE_READY_TIMEOUT);
            }
            return Ok(());
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| {
                BusError::BusUnavailable(format!("cannot create Bus runtime: {error}"))
            })?;
        let (desired_tx, desired_rx) = watch::channel(DesiredState::default());
        let (expiry_tx, expiry_rx) = watch::channel(None);
        let (refresh_tx, refresh_rx) = mpsc::channel(REFRESH_QUEUE_CAPACITY);
        let (noded_tx, _) = broadcast::channel(16);
        let controller = Arc::clone(self);
        let noded_url = cosmix_config::client_helpers::resolve_noded_url();
        let provenance = process_provenance();
        let worker_noded_tx = noded_tx.clone();
        #[cfg(test)]
        self.test_worker_start_attempts
            .fetch_add(1, Ordering::Relaxed);
        thread::Builder::new()
            .name("cosmix-trayd-bus".into())
            .spawn(move || {
                runtime.block_on(async move {
                    tokio::join!(
                        roster_supervisor(
                            Arc::clone(&controller),
                            desired_rx.clone(),
                            refresh_rx,
                            worker_noded_tx.subscribe(),
                            noded_url.clone(),
                            provenance.clone(),
                        ),
                        observe_supervisor(
                            Arc::clone(&controller),
                            desired_rx,
                            worker_noded_tx.subscribe(),
                            noded_url,
                            provenance,
                        ),
                        lease_reaper(controller, expiry_rx),
                    );
                });
            })
            .map_err(|error| {
                BusError::BusUnavailable(format!("cannot start Bus worker: {error}"))
            })?;

        // Startup has three coupled constraints: watcher startup and its
        // bounded readiness wait must precede first publication; watcher
        // startup must hold neither state nor worker because its spawn-failure
        // path takes state; and startup serialises concurrent callers so only
        // one watcher and Bus thread are created. A readiness timeout permits
        // publication because the five-minute reconnect backstop remains; in
        // that case publication means watcher start attempted, not subscription
        // live, and one lifecycle edge can still be lost.
        let readiness = self.start_lifecycle_watcher(noded_tx.clone());
        let _ = readiness.recv_timeout(LIFECYCLE_READY_TIMEOUT);
        *self.lock_worker() = Some(WorkerChannels {
            desired: desired_tx,
            expiry: expiry_tx,
            refresh: refresh_tx,
            noded_lifecycle: noded_tx,
        });
        Ok(())
    }

    fn start_lifecycle_watcher(
        self: &Arc<Self>,
        noded_tx: broadcast::Sender<()>,
    ) -> PublishReceiver<()> {
        let (ready_tx, ready_rx) = sync_channel(1);
        self.lifecycle_watcher_running
            .store(true, Ordering::Release);
        #[cfg(test)]
        self.test_lifecycle_start_attempts
            .fetch_add(1, Ordering::Relaxed);
        #[cfg(test)]
        if let Some(gate) = self
            .test_lifecycle_start_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            gate.block();
        }
        #[cfg(test)]
        if self.test_lifecycle_start_failures.load(Ordering::Relaxed) > 0 {
            self.test_lifecycle_start_failures
                .fetch_sub(1, Ordering::Relaxed);
            self.lifecycle_watcher_running
                .store(false, Ordering::Release);
            self.lifecycle_watch_failed(
                "cannot start noded lifecycle watcher: test failure".into(),
            );
            return ready_rx;
        }
        #[cfg(test)]
        if let Some(gate) = self
            .test_lifecycle_ready_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let watcher_controller = Arc::clone(self);
            if let Err(error) = thread::Builder::new()
                .name("cosmix-trayd-test-noded-lifecycle".into())
                .spawn(move || {
                    let _running = LifecycleWatcherRunning::new(Arc::clone(&watcher_controller));
                    gate.block();
                    watcher_controller.lifecycle_watch_ready();
                    let _ = ready_tx.send(());
                })
            {
                self.lifecycle_watcher_running
                    .store(false, Ordering::Release);
                self.lifecycle_watch_failed(format!(
                    "cannot start test noded lifecycle watcher: {error}"
                ));
            }
            return ready_rx;
        }
        start_noded_lifecycle_watcher(Arc::clone(self), noded_tx, ready_tx);
        ready_rx
    }

    fn notify_worker(
        &self,
        expiry: Option<Instant>,
        desired: Option<DesiredState>,
    ) -> Result<(), BusError> {
        #[cfg(test)]
        if let Some(gate) = self
            .test_notification_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            gate.block();
        }
        let worker = self.lock_worker();
        let worker = worker
            .as_ref()
            .ok_or_else(|| BusError::BusUnavailable("Bus worker is not running".into()))?;
        worker
            .expiry
            .send(expiry)
            .map_err(|_| BusError::BusUnavailable("Bus worker has stopped".into()))?;
        if let Some(desired) = desired {
            worker
                .desired
                .send(desired)
                .map_err(|_| BusError::BusUnavailable("Bus worker has stopped".into()))?;
        }
        Ok(())
    }

    fn rollback_open(&self, owner: &str, session_id: &str) -> Result<(), BusError> {
        let changed = {
            let mut state = self.lock_state();
            let Some(lease) = state.leases.get(session_id) else {
                return Ok(());
            };
            if lease.owner != owner {
                return Err(unknown_session(session_id));
            }
            let changed = state.close(owner, session_id)?;
            let _ = self.notify_worker(state.next_expiry(), changed.then(|| state.desired.clone()));
            changed
        };
        if changed {
            self.schedule_publish();
        }
        Ok(())
    }

    fn expire_leases(&self, now: Instant) {
        let changed = {
            let mut state = self.lock_state();
            let changed = state.expire(now);
            let _ = self.notify_worker(state.next_expiry(), changed.then(|| state.desired.clone()));
            changed
        };
        if changed {
            self.schedule_publish();
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, BusState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_worker(&self) -> MutexGuard<'_, Option<WorkerChannels>> {
        self.worker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn schedule_publish(&self) {
        match self.publish_tx.try_send(()) {
            Ok(()) | Err(TrySendError::Full(())) => {}
            Err(TrySendError::Disconnected(())) => {}
        }
    }

    fn lifecycle_watch_ready(&self) {
        let mut state = self.lock_state();
        if state.lifecycle_error.is_empty() {
            return;
        }
        state.lifecycle_error.clear();
        state.recompute_status();
        state.bump();
        drop(state);
        self.schedule_publish();
    }

    fn lifecycle_watch_failed(&self, message: String) {
        let mut state = self.lock_state();
        let message = concise(&message);
        if state.lifecycle_error == message {
            return;
        }
        state.lifecycle_error = message;
        state.recompute_status();
        state.bump();
        drop(state);
        self.schedule_publish();
    }

    fn roster_connecting(&self, activation: u64) {
        let mut state = self.lock_state();
        if !current_activation(&state, activation) {
            return;
        }
        state.roster_connected = false;
        state.roster_error.clear();
        state.recompute_status();
        state.bump();
        drop(state);
        self.schedule_publish();
    }

    fn roster_connected(&self, activation: u64, generation: u64) {
        let mut state = self.lock_state();
        if !current_activation(&state, activation) {
            return;
        }
        state.roster_connected = true;
        state.roster_generation = generation;
        state.roster_error.clear();
        state.recompute_status();
        state.bump();
        drop(state);
        self.schedule_publish();
    }

    fn roster_connection_failed(&self, activation: u64, message: String) {
        let mut state = self.lock_state();
        if !current_activation(&state, activation) {
            return;
        }
        state.roster_connected = false;
        state.roster_error = concise(&message);
        state.recompute_status();
        state.bump();
        drop(state);
        self.schedule_publish();
    }

    fn roster_request_failed(&self, activation: u64, generation: u64, message: String) {
        let mut state = self.lock_state();
        if !current_roster(&state, activation, generation) {
            return;
        }
        state.roster_error = concise(&message);
        state.recompute_status();
        state.bump();
        drop(state);
        self.schedule_publish();
    }

    fn roster_stopped(&self, activation: u64) {
        let mut state = self.lock_state();
        if state.desired.activation != activation {
            return;
        }
        state.roster_connected = false;
        state.recompute_status();
        state.bump();
        drop(state);
        self.schedule_publish();
    }

    fn install_inventory(&self, activation: u64, generation: u64, body: &str) {
        let parsed = serde_json::from_str::<Inventory>(body);
        let mut state = self.lock_state();
        if !current_roster(&state, activation, generation) {
            return;
        }
        match parsed {
            Ok(mut inventory) => {
                state.inventory_posture = inventory.posture.clone();
                if inventory.posture == "verified" {
                    // Tombstoned records are closed leases kept for the trust
                    // layer, not roster members; the tray shows the live mesh.
                    inventory
                        .members
                        .retain(|member| member.status != "tombstoned");
                    inventory
                        .members
                        .sort_by(|left, right| left.name.cmp(&right.name));
                    state.nodes = inventory.members;
                    state.roster_error.clear();
                } else {
                    state.nodes.clear();
                }
            }
            Err(error) => {
                state.inventory_posture = "unknown".into();
                state.nodes.clear();
                state.roster_error = concise(&format!("invalid noded.inventory: {error}"));
            }
        }
        state.recompute_status();
        state.bump();
        drop(state);
        self.schedule_publish();
    }

    fn install_local_services(&self, activation: u64, generation: u64, body: &str) {
        let parsed = serde_json::from_str::<Value>(body);
        let mut state = self.lock_state();
        if !current_roster(&state, activation, generation) {
            return;
        }
        match parsed {
            Ok(value) => {
                let mut services = value
                    .pointer("/services/registered")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .filter(|service| !service.is_empty() && service.len() <= MAX_FILTER_BYTES)
                    .map(ToString::to_string)
                    .collect::<Vec<_>>();
                services.sort();
                services.dedup();
                state.local_services = services;
            }
            Err(error) => {
                state.roster_error = concise(&format!("invalid world.noded snapshot: {error}"));
                state.recompute_status();
            }
        }
        state.bump();
        drop(state);
        self.schedule_publish();
    }

    fn observe_connecting(&self, activation: u64) {
        let mut state = self.lock_state();
        if !current_activation(&state, activation) {
            return;
        }
        state.observe_connected = false;
        state.observing = false;
        state.subscription_id = None;
        state.observe_error.clear();
        state.recompute_status();
        state.bump();
        drop(state);
        self.schedule_publish();
    }

    fn observe_connected(&self, activation: u64, generation: u64) {
        let mut state = self.lock_state();
        if !current_activation(&state, activation) {
            return;
        }
        state.observe_connected = true;
        state.observe_generation = generation;
        state.observe_error.clear();
        state.recompute_status();
        state.bump();
        drop(state);
        self.schedule_publish();
    }

    fn observe_started(
        &self,
        activation: u64,
        generation: u64,
        filter_epoch: u64,
        subscription_id: String,
    ) {
        let mut state = self.lock_state();
        if !current_activation(&state, activation)
            || state.desired.filter_epoch != filter_epoch
            || state.observe_generation != generation
        {
            return;
        }
        state.traffic.clear();
        state.last_batch_publish_id = state.next_publish_id.saturating_sub(1);
        state.subscription_id = Some(subscription_id);
        state.observing = true;
        state.observe_error.clear();
        state.recompute_status();
        state.bump();
        drop(state);
        self.schedule_publish();
    }

    fn begin_observe_stop(&self, activation: u64, generation: u64, subscription_id: &str) {
        let mut state = self.lock_state();
        if current_activation(&state, activation)
            && state.observe_generation == generation
            && state.subscription_id.as_deref() == Some(subscription_id)
        {
            state.subscription_id = None;
            state.observing = false;
            state.recompute_status();
            state.bump();
            drop(state);
            self.schedule_publish();
        }
    }

    fn observe_failed(&self, activation: u64, generation: u64, message: String) {
        let mut state = self.lock_state();
        if !current_activation(&state, activation) || state.observe_generation != generation {
            return;
        }
        state.observing = false;
        state.subscription_id = None;
        state.observe_error = concise(&message);
        state.recompute_status();
        state.bump();
        drop(state);
        self.schedule_publish();
    }

    fn observe_connection_failed(&self, activation: u64, message: String) {
        let mut state = self.lock_state();
        if !current_activation(&state, activation) {
            return;
        }
        state.observe_connected = false;
        state.observing = false;
        state.subscription_id = None;
        state.observe_error = concise(&message);
        state.recompute_status();
        state.bump();
        drop(state);
        self.schedule_publish();
    }

    fn observe_disconnected(&self, activation: u64, generation: u64, message: String) {
        let mut state = self.lock_state();
        if state.desired.activation != activation || state.observe_generation != generation {
            return;
        }
        state.observe_connected = false;
        state.observing = false;
        state.subscription_id = None;
        if state.active() {
            state.observe_error = concise(&message);
        }
        state.recompute_status();
        state.bump();
        drop(state);
        self.schedule_publish();
    }

    fn observe_stopped(&self, activation: u64) {
        let mut state = self.lock_state();
        if state.desired.activation != activation {
            return;
        }
        state.observe_connected = false;
        state.observing = false;
        state.subscription_id = None;
        state.recompute_status();
        state.bump();
        drop(state);
        self.schedule_publish();
    }

    fn observation_message(&self, activation: u64, generation: u64, command: IncomingCommand) {
        if command.command != "noded.observe.event" {
            return;
        }
        let Some(subscription_id) = command.headers.get("subscription_id").cloned() else {
            self.record_unscoped_malformed(activation, generation);
            return;
        };
        {
            let state = self.lock_state();
            if !state.accepts_observation(activation, generation, &subscription_id) {
                return;
            }
        }
        if observation_frame_size(&command) > OBSERVATION_ENVELOPE_LIMIT {
            self.record_bridge_drop(activation, generation, &subscription_id, false);
            return;
        }
        let event = match serde_json::from_str::<ObservedEvent>(&command.body) {
            Ok(event) => TrafficEvent::from_observed(event),
            Err(_) => {
                self.record_bridge_drop(activation, generation, &subscription_id, true);
                return;
            }
        };
        if event.estimated_bytes() > SIGNAL_BYTE_LIMIT {
            self.record_bridge_drop(activation, generation, &subscription_id, false);
            return;
        }
        let mut state = self.lock_state();
        if !state.accepts_observation(activation, generation, &subscription_id) {
            return;
        }
        state.push_traffic(event);
        drop(state);
        self.schedule_publish();
    }

    fn record_unscoped_malformed(&self, activation: u64, generation: u64) {
        let mut state = self.lock_state();
        if !current_activation(&state, activation)
            || state.observe_generation != generation
            || !state.observing
        {
            return;
        }
        state.malformed_dropped = state.malformed_dropped.saturating_add(1);
        state.bump();
        drop(state);
        self.schedule_publish();
    }

    fn record_bridge_drop(
        &self,
        activation: u64,
        generation: u64,
        subscription_id: &str,
        malformed: bool,
    ) {
        let mut state = self.lock_state();
        if !state.accepts_observation(activation, generation, subscription_id) {
            return;
        }
        if malformed {
            state.malformed_dropped = state.malformed_dropped.saturating_add(1);
        } else {
            state.transport_dropped = state.transport_dropped.saturating_add(1);
        }
        state.bump();
        drop(state);
        self.schedule_publish();
    }
}

fn current_activation(state: &BusState, activation: u64) -> bool {
    state.active() && state.desired.activation == activation
}

fn current_roster(state: &BusState, activation: u64, generation: u64) -> bool {
    current_activation(state, activation)
        && state.roster_connected
        && state.roster_generation == generation
}

fn bounded_snapshot_events(events: &VecDeque<TrafficEvent>) -> Vec<WireTraffic> {
    let mut selected = Vec::new();
    let mut bytes = 0usize;
    for event in events.iter().rev().take(SNAPSHOT_EVENT_LIMIT) {
        let event_bytes = event.estimated_bytes();
        if !selected.is_empty() && bytes.saturating_add(event_bytes) > SNAPSHOT_BYTE_LIMIT {
            break;
        }
        bytes = bytes.saturating_add(event_bytes);
        selected.push(event.wire());
    }
    selected.reverse();
    selected
}

fn bounded_signal_events(
    events: &VecDeque<TrafficEvent>,
    after_publish_id: u64,
) -> Vec<(u64, WireTraffic)> {
    let mut selected = Vec::new();
    let mut bytes = 0usize;
    for event in events
        .iter()
        .filter(|event| event.publish_id > after_publish_id)
        .take(SIGNAL_EVENT_LIMIT)
    {
        let event_bytes = event.estimated_bytes();
        if !selected.is_empty() && bytes.saturating_add(event_bytes) > SIGNAL_BYTE_LIMIT {
            break;
        }
        bytes = bytes.saturating_add(event_bytes);
        selected.push((event.publish_id, event.wire()));
    }
    selected
}

fn observation_frame_size(command: &IncomingCommand) -> usize {
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

fn process_provenance() -> RegisterProvenance {
    let build = cosmix_buildinfo::build_info!();
    RegisterProvenance::from_parts(
        "cosmix-trayd",
        build.version,
        build.git_sha,
        build.git_dirty,
        build.build_time,
        cosmix_buildinfo::now_rfc3339(),
    )
}

struct LifecycleWatcherRunning {
    controller: Arc<BusController>,
}

impl LifecycleWatcherRunning {
    fn new(controller: Arc<BusController>) -> Self {
        Self { controller }
    }
}

impl Drop for LifecycleWatcherRunning {
    fn drop(&mut self) {
        self.controller
            .lifecycle_watcher_running
            .store(false, Ordering::Release);
    }
}

fn start_noded_lifecycle_watcher(
    controller: Arc<BusController>,
    events: broadcast::Sender<()>,
    readiness: SyncSender<()>,
) {
    let watcher_controller = Arc::clone(&controller);
    if let Err(error) = thread::Builder::new()
        .name("cosmix-trayd-noded-lifecycle".into())
        .spawn(move || {
            let _running = LifecycleWatcherRunning::new(Arc::clone(&watcher_controller));
            let connection = match BusConnection::system() {
                Ok(connection) => connection,
                Err(error) => {
                    watcher_controller
                        .lifecycle_watch_failed(format!("cannot watch noded lifecycle: {error}"));
                    return;
                }
            };
            let rule = match MatchRule::builder()
                .msg_type(MessageType::Signal)
                .sender("org.freedesktop.systemd1")
                .and_then(|builder| builder.interface("org.freedesktop.systemd1.Manager"))
                .and_then(|builder| builder.member("JobRemoved"))
                .map(|builder| builder.build())
            {
                Ok(rule) => rule,
                Err(error) => {
                    watcher_controller.lifecycle_watch_failed(format!(
                        "cannot build noded lifecycle match: {error}"
                    ));
                    return;
                }
            };
            let mut signals = match MessageIterator::for_match_rule(rule, &connection, Some(64)) {
                Ok(signals) => signals,
                Err(error) => {
                    watcher_controller.lifecycle_watch_failed(format!(
                        "cannot install noded lifecycle match: {error}"
                    ));
                    return;
                }
            };
            if let Err(error) = subscribe_systemd_manager(&connection) {
                watcher_controller.lifecycle_watch_failed(error);
                return;
            }
            watcher_controller.lifecycle_watch_ready();
            let _ = readiness.send(());
            for message in &mut signals {
                let message = match message {
                    Ok(message) => message,
                    Err(error) => {
                        watcher_controller.lifecycle_watch_failed(format!(
                            "noded lifecycle subscription failed: {error}"
                        ));
                        return;
                    }
                };
                let Ok((_job_id, _job_path, unit, _result)) =
                    message
                        .body()
                        .deserialize::<(u32, zbus::zvariant::OwnedObjectPath, String, String)>()
                else {
                    continue;
                };
                if unit == NODED_UNIT {
                    let _ = events.send(());
                }
            }
            watcher_controller.lifecycle_watch_failed("noded lifecycle subscription ended".into());
        })
    {
        controller
            .lifecycle_watcher_running
            .store(false, Ordering::Release);
        controller.lifecycle_watch_failed(format!("cannot start noded lifecycle watcher: {error}"));
    }
}

fn subscribe_systemd_manager(connection: &BusConnection) -> Result<(), String> {
    let proxy = BusProxy::new(
        connection,
        "org.freedesktop.systemd1",
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
    )
    .map_err(|error| format!("cannot create systemd manager proxy: {error}"))?;
    invoke_systemd_subscribe(|member| {
        proxy
            .call::<_, _, ()>(member, &())
            .map_err(|error| format!("cannot subscribe to systemd jobs: {error}"))
    })
}

fn invoke_systemd_subscribe(call: impl FnOnce(&str) -> Result<(), String>) -> Result<(), String> {
    call("Subscribe")
}

async fn lease_reaper(
    controller: Arc<BusController>,
    mut expiry_rx: watch::Receiver<Option<Instant>>,
) {
    loop {
        let deadline = *expiry_rx.borrow();
        let Some(deadline) = deadline else {
            if expiry_rx.changed().await.is_err() {
                return;
            }
            continue;
        };
        tokio::select! {
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                controller.expire_leases(Instant::now());
            }
            changed = expiry_rx.changed() => {
                if changed.is_err() {
                    return;
                }
            }
        }
    }
}

async fn roster_supervisor(
    controller: Arc<BusController>,
    mut desired_rx: watch::Receiver<DesiredState>,
    mut refresh_rx: mpsc::Receiver<u64>,
    mut noded_rx: broadcast::Receiver<()>,
    noded_url: String,
    provenance: RegisterProvenance,
) {
    let service_name = format!("trayd-roster-{}", std::process::id());
    let mut generation = 0u64;
    loop {
        let desired = wait_until_active(&mut desired_rx).await;
        let Some(desired) = desired else {
            return;
        };
        while refresh_rx.try_recv().is_ok() {}
        let activation = desired.activation;
        controller.roster_connecting(activation);
        let client = match NodedClient::connect_with_provenance(
            &service_name,
            &noded_url,
            Some(provenance.clone()),
        )
        .await
        {
            Ok(client) => client,
            Err(error) => {
                controller.roster_connection_failed(
                    activation,
                    format!("roster connection failed: {error}"),
                );
                if !reconnect_wait(&mut desired_rx, &mut noded_rx, activation).await {
                    continue;
                }
                continue;
            }
        };
        generation = generation.saturating_add(1);
        controller.roster_connected(activation, generation);
        let Some(mut incoming) = client.incoming_async().await else {
            client.close().await;
            controller.roster_connection_failed(
                activation,
                "roster connection has no receive stream".into(),
            );
            let _ = reconnect_wait(&mut desired_rx, &mut noded_rx, activation).await;
            continue;
        };
        refresh_inventory(&controller, &client, activation, generation).await;
        if let Err(error) = subscribe_world(&client).await {
            close_client(&client, Some(WORLD_NODED_TOPIC)).await;
            controller.roster_connection_failed(
                activation,
                format!("world.noded subscription failed: {error}"),
            );
            let _ = reconnect_wait(&mut desired_rx, &mut noded_rx, activation).await;
            continue;
        }
        let mut reconnect = false;
        loop {
            tokio::select! {
                changed = desired_rx.changed() => {
                    if changed.is_err() {
                        close_client(&client, Some(WORLD_NODED_TOPIC)).await;
                        return;
                    }
                    let now = desired_rx.borrow().clone();
                    if !now.active || now.activation != activation {
                        close_client(&client, Some(WORLD_NODED_TOPIC)).await;
                        controller.roster_stopped(activation);
                        break;
                    }
                }
                refresh = refresh_rx.recv() => {
                    if refresh == Some(activation) {
                        refresh_inventory(&controller, &client, activation, generation).await;
                    }
                }
                command = incoming.recv() => {
                    match command {
                        Some(command) if command.headers.get("topic").map(String::as_str)
                            == Some(WORLD_NODED_TOPIC) => {
                                controller.install_local_services(
                                    activation,
                                    generation,
                                    &command.body,
                                );
                            }
                        Some(_) => {}
                        None => {
                            controller.roster_connection_failed(
                                activation,
                                "roster connection disconnected".into(),
                            );
                            reconnect = true;
                            break;
                        }
                    }
                }
            }
        }
        client.close().await;
        if reconnect {
            let _ = reconnect_wait(&mut desired_rx, &mut noded_rx, activation).await;
        }
    }
}

async fn observe_supervisor(
    controller: Arc<BusController>,
    mut desired_rx: watch::Receiver<DesiredState>,
    mut noded_rx: broadcast::Receiver<()>,
    noded_url: String,
    provenance: RegisterProvenance,
) {
    let service_name = format!("trayd-observe-{}", std::process::id());
    let mut generation = 0u64;
    loop {
        let desired = wait_until_active(&mut desired_rx).await;
        let Some(desired) = desired else {
            return;
        };
        let activation = desired.activation;
        controller.observe_connecting(activation);
        let client = match NodedClient::connect_with_provenance(
            &service_name,
            &noded_url,
            Some(provenance.clone()),
        )
        .await
        {
            Ok(client) => client,
            Err(error) => {
                controller.observe_connection_failed(
                    activation,
                    format!("observation connection failed: {error}"),
                );
                if !reconnect_wait(&mut desired_rx, &mut noded_rx, activation).await {
                    continue;
                }
                continue;
            }
        };
        generation = generation.saturating_add(1);
        controller.observe_connected(activation, generation);
        let Some(mut incoming) = client.incoming_async().await else {
            client.close().await;
            controller.observe_disconnected(
                activation,
                generation,
                "observation connection has no receive stream".into(),
            );
            let _ = reconnect_wait(&mut desired_rx, &mut noded_rx, activation).await;
            continue;
        };

        let mut subscription: Option<String> = None;
        let mut applied_filter_epoch = 0u64;
        let mut failed_filter_epoch = 0u64;
        let mut reconnect = false;
        loop {
            let desired = desired_rx.borrow().clone();
            if !desired.active || desired.activation != activation {
                stop_observation(
                    &controller,
                    &client,
                    &mut incoming,
                    activation,
                    generation,
                    subscription.take(),
                )
                .await;
                close_client(&client, None).await;
                controller.observe_stopped(activation);
                break;
            }
            if applied_filter_epoch != desired.filter_epoch
                && failed_filter_epoch != desired.filter_epoch
            {
                stop_observation(
                    &controller,
                    &client,
                    &mut incoming,
                    activation,
                    generation,
                    subscription.take(),
                )
                .await;
                match start_observation(&client, &desired.filter).await {
                    Ok(id) => {
                        applied_filter_epoch = desired.filter_epoch;
                        failed_filter_epoch = 0;
                        subscription = Some(id.clone());
                        controller.observe_started(
                            activation,
                            generation,
                            desired.filter_epoch,
                            id,
                        );
                    }
                    Err(error) => {
                        failed_filter_epoch = desired.filter_epoch;
                        controller.observe_failed(activation, generation, error);
                    }
                }
            }

            tokio::select! {
                changed = desired_rx.changed() => {
                    if changed.is_err() {
                        stop_observation(
                            &controller,
                            &client,
                            &mut incoming,
                            activation,
                            generation,
                            subscription.take(),
                        ).await;
                        close_client(&client, None).await;
                        return;
                    }
                }
                command = incoming.recv() => {
                    match command {
                        Some(command) => controller.observation_message(
                            activation,
                            generation,
                            command,
                        ),
                        None => {
                            controller.observe_disconnected(
                                activation,
                                generation,
                                "observation connection disconnected".into(),
                            );
                            reconnect = true;
                            break;
                        }
                    }
                }
            }
        }
        client.close().await;
        if reconnect {
            let _ = reconnect_wait(&mut desired_rx, &mut noded_rx, activation).await;
        }
    }
}

async fn wait_until_active(desired_rx: &mut watch::Receiver<DesiredState>) -> Option<DesiredState> {
    loop {
        let desired = desired_rx.borrow().clone();
        if desired.active {
            return Some(desired);
        }
        if desired_rx.changed().await.is_err() {
            return None;
        }
    }
}

async fn reconnect_wait(
    desired_rx: &mut watch::Receiver<DesiredState>,
    noded_rx: &mut broadcast::Receiver<()>,
    activation: u64,
) -> bool {
    tokio::select! {
        // A completed cosmix-noded systemd job is the normal reconnect edge.
        // The only clock path is the project-mandated five-minute backstop.
        event = noded_rx.recv() => !matches!(event, Err(broadcast::error::RecvError::Closed)),
        _ = tokio::time::sleep(RECONNECT_BACKSTOP) => true,
        changed = desired_rx.changed() => {
            changed.is_ok()
                && desired_rx.borrow().active
                && desired_rx.borrow().activation == activation
        }
    }
}

async fn refresh_inventory(
    controller: &BusController,
    client: &NodedClient,
    activation: u64,
    generation: u64,
) {
    match client
        .call_with_headers_raw(
            "noded",
            "noded.inventory",
            &std::collections::BTreeMap::new(),
            "",
        )
        .await
    {
        Ok((0, body, _)) => controller.install_inventory(activation, generation, &body),
        Ok((rc, body, error)) => controller.roster_request_failed(
            activation,
            generation,
            format!("noded.inventory rc={rc}: {}", error.unwrap_or(body)),
        ),
        Err(error) => controller.roster_request_failed(
            activation,
            generation,
            format!("noded.inventory failed: {error}"),
        ),
    }
}

async fn subscribe_world(client: &NodedClient) -> Result<(), String> {
    let headers =
        std::collections::BTreeMap::from([("name".to_string(), WORLD_NODED_TOPIC.to_string())]);
    client
        .call_with_headers("noded", "topic.subscribe", &headers, "")
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

async fn unsubscribe_world(client: &NodedClient) {
    let headers =
        std::collections::BTreeMap::from([("name".to_string(), WORLD_NODED_TOPIC.to_string())]);
    let _ = client
        .call_with_headers("noded", "topic.unsubscribe", &headers, "")
        .await;
}

async fn start_observation(
    client: &NodedClient,
    filter: &EffectiveFilter,
) -> Result<String, String> {
    match client
        .call_with_headers_raw(
            "noded",
            "noded.observe.start",
            &std::collections::BTreeMap::new(),
            &filter.start_body(),
        )
        .await
    {
        Ok((0, body, _)) => serde_json::from_str::<ObserveStartReply>(&body)
            .map(|reply| reply.subscription_id)
            .map_err(|error| format!("invalid noded.observe.start reply: {error}")),
        Ok((rc, body, error)) => Err(format!(
            "noded.observe.start rc={rc}: {}",
            error.unwrap_or(body)
        )),
        Err(error) => Err(format!("noded.observe.start failed: {error}")),
    }
}

async fn stop_observation(
    controller: &BusController,
    client: &NodedClient,
    incoming: &mut mpsc::UnboundedReceiver<IncomingCommand>,
    activation: u64,
    generation: u64,
    subscription_id: Option<String>,
) {
    let Some(subscription_id) = subscription_id else {
        return;
    };
    controller.begin_observe_stop(activation, generation, &subscription_id);
    let body = json!({ "subscription_id": subscription_id }).to_string();
    let _ = client
        .call_with_headers_raw(
            "noded",
            "noded.observe.stop",
            &std::collections::BTreeMap::new(),
            &body,
        )
        .await;
    while incoming.try_recv().is_ok() {}
}

async fn close_client(client: &NodedClient, topic: Option<&str>) {
    if topic == Some(WORLD_NODED_TOPIC) && client.is_connected() {
        unsubscribe_world(client).await;
    }
    if client.is_connected() {
        let _ = client.deregister().await;
    }
    client.close().await;
}

fn concise(message: &str) -> String {
    let single_line = message.split_whitespace().collect::<Vec<_>>().join(" ");
    const LIMIT: usize = 240;
    if single_line.chars().count() <= LIMIT {
        return single_line;
    }
    let mut shortened = single_line.chars().take(LIMIT).collect::<String>();
    shortened.push('…');
    shortened
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn join_with_timeout<T>(handle: thread::JoinHandle<T>, context: &str) -> thread::Result<T> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !handle.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            handle.is_finished(),
            "{context} did not finish within five seconds"
        );
        handle.join()
    }

    fn filter(direction: &str, verb: &str, body: &str) -> LeaseFilter {
        LeaseFilter::parse(vec![direction.into()], verb.into(), body.into()).unwrap()
    }

    fn observed(seq: u64) -> String {
        json!({
            "seq": seq,
            "ts": "2026-07-28T00:00:00Z",
            "direction": "local",
            "outcome": "delivered",
            "message_type": "request",
            "from": "alpha-service",
            "to": "noded",
            "verb": "noded.inventory",
            "size": 128,
            "correlation_id": "example-1",
            "rc": null,
            "dropped_count": 2,
            "payload_omitted": "disabled"
        })
        .to_string()
    }

    fn command(subscription: &str, seq: u64) -> IncomingCommand {
        IncomingCommand {
            from: "noded".into(),
            command: "noded.observe.event".into(),
            id: None,
            args: Value::Null,
            body: observed(seq),
            headers: BTreeMap::from([("subscription_id".into(), subscription.into())]),
        }
    }

    #[test]
    fn filter_union_is_sorted_deduplicated_and_redacted_wins() {
        let now = Instant::now();
        let mut state = BusState::default();
        state.open(":1.10".into(), filter("mesh_out", "maild.*", "none"), now);
        state.open(":1.11".into(), filter("local", "noded.*", "redacted"), now);
        state.open(":1.12".into(), filter("local", "maild.*", "none"), now);
        assert_eq!(state.desired.filter.directions, ["local", "mesh_out"]);
        assert_eq!(state.desired.filter.verbs, ["maild.*", "noded.*"]);
        assert_eq!(state.desired.filter.body, BodyMode::Redacted);
    }

    #[test]
    fn owner_loss_removes_only_that_senders_leases_and_stops_last() {
        let now = Instant::now();
        let mut state = BusState::default();
        state.open(":1.20".into(), filter("local", "*", "none"), now);
        state.open(":1.21".into(), filter("mesh_in", "*", "none"), now);
        assert!(state.remove_owner(":1.20"));
        assert!(state.active());
        assert_eq!(state.desired.filter.directions, ["mesh_in"]);
        assert!(state.remove_owner(":1.21"));
        assert!(!state.active());
        assert_eq!(state.status, "idle");
        assert!(state.subscription_id.is_none());
    }

    #[test]
    fn lease_mutations_are_sender_bound() {
        let now = Instant::now();
        let mut state = BusState::default();
        let (session, _) = state.open(":1.22".into(), filter("local", "*", "none"), now);
        assert!(matches!(
            state.update(":1.23", &session, filter("mesh_in", "*", "none"), now),
            Err(BusError::UnknownBusSession(_))
        ));
        assert!(matches!(
            state.keep_alive(":1.23", &session, now),
            Err(BusError::UnknownBusSession(_))
        ));
        assert!(matches!(
            state.close(":1.23", &session),
            Err(BusError::UnknownBusSession(_))
        ));
        assert_eq!(state.desired.filter.directions, ["local"]);
    }

    #[test]
    fn ten_minute_expiry_is_a_backstop_and_keepalive_renews() {
        let now = Instant::now();
        let mut state = BusState::default();
        let (session, _) = state.open(":1.30".into(), filter("local", "*", "none"), now);
        state
            .keep_alive(":1.30", &session, now + Duration::from_secs(5 * 60))
            .unwrap();
        assert_eq!(
            state.next_expiry(),
            Some(now + Duration::from_secs(15 * 60))
        );
        assert!(!state.expire(now + Duration::from_secs(10 * 60)));
        assert!(state.active());
        assert!(state.expire(now + Duration::from_secs(15 * 60)));
        assert!(!state.active());
    }

    #[test]
    fn lease_reaper_fires_at_the_supplied_one_shot_deadline() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let controller = BusController::new();
            let deadline = Instant::now() + Duration::from_millis(20);
            {
                let mut state = controller.lock_state();
                let (session, _) =
                    state.open(":1.31".into(), filter("local", "*", "none"), Instant::now());
                state.leases.get_mut(&session).unwrap().expires_at = deadline;
            }
            let (expiry_tx, expiry_rx) = watch::channel(Some(deadline));
            let (desired_tx, _desired_rx) = watch::channel(DesiredState::default());
            let (refresh_tx, _refresh_rx) = mpsc::channel(1);
            let (noded_lifecycle, _) = broadcast::channel(1);
            *controller.lock_worker() = Some(WorkerChannels {
                desired: desired_tx,
                expiry: expiry_tx,
                refresh: refresh_tx,
                noded_lifecycle,
            });
            let task = tokio::spawn(lease_reaper(Arc::clone(&controller), expiry_rx));
            tokio::time::sleep(Duration::from_millis(60)).await;
            assert!(!controller.active());
            task.abort();
        });
    }

    #[test]
    fn open_rolls_back_a_lease_when_worker_notification_fails() {
        let controller = BusController::new();
        let (desired_tx, desired_rx) = watch::channel(DesiredState::default());
        let (expiry_tx, expiry_rx) = watch::channel(None);
        let (refresh_tx, _refresh_rx) = mpsc::channel(1);
        let (noded_lifecycle, _) = broadcast::channel(1);
        drop(desired_rx);
        *controller.lock_worker() = Some(WorkerChannels {
            desired: desired_tx,
            expiry: expiry_tx,
            refresh: refresh_tx,
            noded_lifecycle,
        });
        controller
            .lifecycle_watcher_running
            .store(true, Ordering::Release);

        let error = controller
            .open(
                ":1.32".into(),
                vec!["local".into()],
                "*".into(),
                "none".into(),
            )
            .expect_err("a stopped desired-state receiver must reject the open");

        assert!(matches!(error, BusError::BusUnavailable(_)));
        assert!(!controller.active());
        assert!(controller.lock_state().leases.is_empty());
        assert_eq!(*expiry_rx.borrow(), None);
    }

    #[test]
    fn lifecycle_watcher_start_is_entered_lock_free_before_worker_publication() {
        let controller = BusController::new();
        let gate = controller.block_next_lifecycle_start();
        let worker_controller = Arc::clone(&controller);
        let ensure = thread::spawn(move || worker_controller.ensure_worker());

        gate.wait_until_entered();
        let worker = controller
            .worker
            .try_lock()
            .expect("Bus worker lock was held while starting the lifecycle watcher");
        assert!(
            worker.is_none(),
            "Bus worker channels were published before the watcher start helper was entered"
        );
        drop(worker);
        let state = controller
            .state
            .try_lock()
            .expect("Bus state lock was held while starting the lifecycle watcher");
        drop(state);
        gate.release();
        join_with_timeout(ensure, "Bus worker startup")
            .expect("join Bus worker startup")
            .expect("Bus worker startup succeeds");
    }

    #[test]
    fn worker_publication_awaits_lifecycle_readiness_but_has_a_bounded_timeout() {
        let controller = BusController::new();
        let ready_gate = controller.block_next_lifecycle_readiness();
        let worker_controller = Arc::clone(&controller);
        let ensure = thread::spawn(move || worker_controller.ensure_worker());

        ready_gate.wait_until_entered();
        let worker = controller
            .worker
            .try_lock()
            .expect("Bus worker lock was held while awaiting lifecycle readiness");
        let unpublished = worker.is_none();
        drop(worker);
        ready_gate.release();
        let startup = join_with_timeout(ensure, "readiness-gated Bus worker startup");
        assert!(
            unpublished,
            "Bus worker channels were published before lifecycle readiness"
        );
        startup
            .expect("join readiness-gated Bus worker startup")
            .expect("readiness-gated Bus worker startup succeeds");
        assert!(controller.lock_worker().is_some());

        let controller = BusController::new();
        let timeout_gate = controller.block_next_lifecycle_readiness();
        let worker_controller = Arc::clone(&controller);
        let ensure = thread::spawn(move || worker_controller.ensure_worker());
        timeout_gate.wait_until_entered();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !ensure.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let timed_out_without_hanging = ensure.is_finished();
        timeout_gate.release();
        assert!(
            timed_out_without_hanging,
            "lifecycle readiness timeout did not bound Bus worker startup"
        );
        join_with_timeout(ensure, "timed-out Bus worker startup")
            .expect("join timed-out Bus worker startup")
            .expect("timed-out Bus worker startup proceeds");
        assert!(controller.lock_worker().is_some());
    }

    #[test]
    fn failed_lifecycle_watcher_start_is_retried_without_restarting_bus_worker() {
        let controller = BusController::new();
        controller.fail_next_lifecycle_start();

        controller
            .ensure_worker()
            .expect("Bus worker starts despite lifecycle watcher failure");
        assert_eq!(
            controller
                .test_lifecycle_start_attempts
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            controller
                .test_worker_start_attempts
                .load(Ordering::Relaxed),
            1
        );
        assert!(!controller.lifecycle_watcher_running.load(Ordering::Acquire));

        controller
            .ensure_worker()
            .expect("later startup retries the lifecycle watcher");
        assert_eq!(
            controller
                .test_lifecycle_start_attempts
                .load(Ordering::Relaxed),
            2,
            "the lifecycle watcher was not retried"
        );
        assert_eq!(
            controller
                .test_worker_start_attempts
                .load(Ordering::Relaxed),
            1,
            "the Bus worker was started again while retrying only its watcher"
        );
    }

    #[test]
    fn every_worker_notification_is_ordered_with_its_state_mutation() {
        #[derive(Clone, Copy, Debug)]
        enum Path {
            Open,
            Update,
            KeepAlive,
            Close,
            OwnerLost,
            ExpireLeases,
            RollbackOpen,
        }

        for path in [
            Path::Open,
            Path::Update,
            Path::KeepAlive,
            Path::Close,
            Path::OwnerLost,
            Path::ExpireLeases,
            Path::RollbackOpen,
        ] {
            let controller = BusController::new_test();
            let owner = format!(":1.33.{path:?}");
            let session_id = if matches!(path, Path::Open) {
                String::new()
            } else {
                controller
                    .open(
                        owner.clone(),
                        vec!["local".into()],
                        "*".into(),
                        "none".into(),
                    )
                    .expect("seed Bus lease")
            };
            let gate = controller.block_next_worker_notification();
            let operation_controller = Arc::clone(&controller);
            let operation = thread::spawn(move || match path {
                Path::Open => operation_controller
                    .open(owner, vec!["local".into()], "*".into(), "none".into())
                    .map(|_| ()),
                Path::Update => operation_controller.update(
                    &owner,
                    &session_id,
                    vec!["mesh_out".into()],
                    "peer.*".into(),
                    "redacted".into(),
                ),
                Path::KeepAlive => operation_controller.keep_alive(&owner, &session_id),
                Path::Close => operation_controller.close(&owner, &session_id),
                Path::OwnerLost => {
                    operation_controller.owner_lost(&owner);
                    Ok(())
                }
                Path::ExpireLeases => {
                    operation_controller.expire_leases(Instant::now() + LEASE_TTL);
                    Ok(())
                }
                Path::RollbackOpen => operation_controller.rollback_open(&owner, &session_id),
            });

            gate.wait_until_entered();
            assert!(
                controller.state.try_lock().is_err(),
                "{path:?} released Bus state before its worker notification"
            );
            gate.release();
            join_with_timeout(operation, &format!("{path:?} notification"))
                .unwrap_or_else(|_| panic!("join {path:?} notification"))
                .unwrap_or_else(|error| panic!("{path:?} notification succeeds: {error}"));

            let desired = controller
                .test_worker_receivers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_ref()
                .expect("test worker receivers remain installed")
                ._desired
                .borrow()
                .clone();
            assert_eq!(
                desired,
                controller.lock_state().desired,
                "{path:?} published the final desired state"
            );
        }
    }

    #[test]
    fn rollback_tolerates_concurrent_owner_loss() {
        let controller = BusController::new();
        let (desired_tx, desired_rx) = watch::channel(DesiredState::default());
        let (expiry_tx, _expiry_rx) = watch::channel(None);
        let (refresh_tx, _refresh_rx) = mpsc::channel(1);
        let (noded_lifecycle, _) = broadcast::channel(1);
        drop(desired_rx);
        *controller.lock_worker() = Some(WorkerChannels {
            desired: desired_tx,
            expiry: expiry_tx,
            refresh: refresh_tx,
            noded_lifecycle,
        });
        controller
            .lifecycle_watcher_running
            .store(true, Ordering::Release);
        let rollback_gate = controller.block_next_rollback();
        let open_controller = Arc::clone(&controller);
        let open = thread::spawn(move || {
            open_controller.open(
                ":1.34".into(),
                vec!["local".into()],
                "*".into(),
                "none".into(),
            )
        });

        rollback_gate.wait_until_entered();
        controller.owner_lost(":1.34");
        rollback_gate.release();
        let error = join_with_timeout(open, "rollback after concurrent owner loss")
            .expect("rollback must not panic after concurrent owner loss")
            .expect_err("failed worker notification still rejects the open");

        assert!(matches!(error, BusError::BusUnavailable(_)));
        assert!(!controller.active());
        assert!(controller.lock_state().leases.is_empty());
    }

    #[test]
    fn connection_generation_rejects_pre_reconnect_events() {
        let controller = BusController::new();
        {
            let mut state = controller.lock_state();
            state.open(":1.40".into(), filter("local", "*", "none"), Instant::now());
        }
        let activation = controller.lock_state().desired.activation;
        controller.observe_connected(activation, 1);
        controller.observe_started(activation, 1, 1, "old".into());
        controller.observation_message(activation, 1, command("old", 1));
        assert_eq!(controller.lock_state().traffic.len(), 1);

        controller.observe_disconnected(activation, 1, "broker bounced".into());
        controller.observation_message(activation, 1, command("old", 2));
        controller.observe_connected(activation, 2);
        controller.observe_started(activation, 2, 1, "new".into());
        controller.observation_message(activation, 1, command("old", 3));
        controller.observation_message(activation, 2, command("old", 4));
        controller.observation_message(activation, 2, command("new", 5));
        let state = controller.lock_state();
        assert_eq!(state.traffic.len(), 1);
        assert_eq!(state.traffic[0].seq, 5);
    }

    #[test]
    fn stop_fence_rejects_every_event_after_stop_begins() {
        let controller = BusController::new();
        {
            let mut state = controller.lock_state();
            state.open(":1.50".into(), filter("local", "*", "none"), Instant::now());
        }
        let activation = controller.lock_state().desired.activation;
        controller.observe_connected(activation, 7);
        controller.observe_started(activation, 7, 1, "fenced".into());
        controller.begin_observe_stop(activation, 7, "fenced");
        controller.observation_message(activation, 7, command("fenced", 1));
        assert!(controller.lock_state().traffic.is_empty());
        assert!(!controller.lock_state().observing);
    }

    #[test]
    fn one_oversized_event_cannot_break_the_dbus_batch_bound() {
        let controller = BusController::new();
        {
            let mut state = controller.lock_state();
            state.open(":1.51".into(), filter("local", "*", "none"), Instant::now());
        }
        let activation = controller.lock_state().desired.activation;
        controller.observe_connected(activation, 8);
        controller.observe_started(activation, 8, 1, "bounded".into());
        let mut event = serde_json::from_str::<Value>(&observed(1)).unwrap();
        event["from"] = Value::String("x".repeat(SIGNAL_BYTE_LIMIT));
        let oversized = IncomingCommand {
            from: "noded".into(),
            command: "noded.observe.event".into(),
            id: None,
            args: Value::Null,
            body: event.to_string(),
            headers: BTreeMap::from([("subscription_id".into(), "bounded".into())]),
        };
        controller.observation_message(activation, 8, oversized);
        let state = controller.lock_state();
        assert!(state.traffic.is_empty());
        assert_eq!(state.transport_dropped, 1);
    }

    #[test]
    fn traffic_ring_and_signal_batches_are_bounded() {
        let mut state = BusState::default();
        for seq in 0..(TRAFFIC_RING_CAPACITY as u64 + 5) {
            state.push_traffic(TrafficEvent::from_observed(
                serde_json::from_str(&observed(seq)).unwrap(),
            ));
        }
        assert_eq!(state.traffic.len(), TRAFFIC_RING_CAPACITY);
        assert_eq!(state.ring_dropped, 5);
        let batch = bounded_signal_events(&state.traffic, 0);
        assert!(batch.len() <= SIGNAL_EVENT_LIMIT);
        let snapshot = bounded_snapshot_events(&state.traffic);
        assert!(snapshot.len() <= SNAPSHOT_EVENT_LIMIT);
    }

    #[test]
    fn verified_inventory_is_required_before_members_are_exposed() {
        let controller = BusController::new();
        {
            let mut state = controller.lock_state();
            state.open(":1.60".into(), filter("local", "*", "none"), Instant::now());
        }
        let activation = controller.lock_state().desired.activation;
        controller.roster_connected(activation, 3);
        controller.install_inventory(
            activation,
            3,
            r#"{"posture":"unverified","members":[{"name":"alpha","mesh_ip":"192.0.2.10","bus":true,"status":"active"}]}"#,
        );
        assert!(controller.lock_state().nodes.is_empty());
        controller.install_inventory(
            activation,
            3,
            r#"{"posture":"verified","members":[{"name":"beta","mesh_ip":"198.51.100.20","bus":true,"status":"active"},{"name":"alpha","mesh_ip":"192.0.2.10","bus":false,"status":"inactive"}]}"#,
        );
        let state = controller.lock_state();
        assert_eq!(state.nodes.len(), 2);
        assert_eq!(state.nodes[0].name, "alpha");
    }

    #[test]
    fn tombstoned_members_are_filtered_from_the_roster() {
        let controller = BusController::new();
        {
            let mut state = controller.lock_state();
            state.open(":1.61".into(), filter("local", "*", "none"), Instant::now());
        }
        let activation = controller.lock_state().desired.activation;
        controller.roster_connected(activation, 3);
        controller.install_inventory(
            activation,
            3,
            r#"{"posture":"verified","members":[{"name":"alpha","mesh_ip":"192.0.2.10","bus":true,"status":"active"},{"name":"beta","mesh_ip":"198.51.100.20","bus":true,"status":"tombstoned"},{"name":"gamma","mesh_ip":"203.0.113.30","bus":false,"status":"inactive"}]}"#,
        );
        let state = controller.lock_state();
        let names: Vec<&str> = state.nodes.iter().map(|node| node.name.as_str()).collect();
        assert_eq!(names, ["alpha", "gamma"]);
    }

    #[test]
    fn systemd_lifecycle_edge_wakes_reconnect_without_a_retry_clock() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let desired = DesiredState {
                active: true,
                activation: 7,
                ..DesiredState::default()
            };
            let (_desired_tx, mut desired_rx) = watch::channel(desired);
            let (noded_tx, mut noded_rx) = broadcast::channel(2);
            noded_tx.send(()).unwrap();
            assert!(reconnect_wait(&mut desired_rx, &mut noded_rx, 7).await);
        });
        assert_eq!(RECONNECT_BACKSTOP, Duration::from_secs(5 * 60));
    }

    #[test]
    fn lease_change_wakes_reconnect_and_fences_a_stopped_activation() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (desired_tx, mut desired_rx) = watch::channel(DesiredState {
                active: true,
                activation: 8,
                ..DesiredState::default()
            });
            let (_noded_tx, mut noded_rx) = broadcast::channel(2);
            desired_tx.send(DesiredState::default()).unwrap();
            assert!(!reconnect_wait(&mut desired_rx, &mut noded_rx, 8).await);
        });
    }

    #[test]
    fn filter_change_and_last_close_clear_retained_traffic() {
        let now = Instant::now();
        let mut state = BusState::default();
        let (session, _) = state.open(":1.70".into(), filter("local", "*", "none"), now);
        state.push_traffic(TrafficEvent::from_observed(
            serde_json::from_str(&observed(1)).unwrap(),
        ));
        assert_eq!(state.traffic.len(), 1);
        state
            .update(":1.70", &session, filter("mesh_in", "*", "none"), now)
            .unwrap();
        assert!(state.traffic.is_empty());
        state.push_traffic(TrafficEvent::from_observed(
            serde_json::from_str(&observed(2)).unwrap(),
        ));
        state.close(":1.70", &session).unwrap();
        assert!(state.traffic.is_empty());
    }

    #[test]
    fn traffic_publications_carry_the_filter_epoch_fence() {
        let controller = BusController::new();
        let session = {
            let mut state = controller.lock_state();
            let (session, _) =
                state.open(":1.91".into(), filter("local", "*", "none"), Instant::now());
            state.push_traffic(TrafficEvent::from_observed(
                serde_json::from_str(&observed(1)).unwrap(),
            ));
            session
        };
        let first = controller.take_publication().expect("first publication");
        assert_eq!(first.filter_epoch, 1);

        {
            let mut state = controller.lock_state();
            state
                .update(
                    ":1.91",
                    &session,
                    filter("mesh_in", "noded.*", "none"),
                    Instant::now(),
                )
                .unwrap();
            state.push_traffic(TrafficEvent::from_observed(
                serde_json::from_str(&observed(2)).unwrap(),
            ));
        }
        let second = controller.take_publication().expect("second publication");
        assert_eq!(second.filter_epoch, 2);
    }

    #[test]
    fn systemd_subscription_loss_is_a_degraded_bus_state() {
        let mut state = BusState::default();
        state.open(":1.92".into(), filter("local", "*", "none"), Instant::now());
        state.roster_connected = true;
        state.observe_connected = true;
        state.observing = true;
        state.lifecycle_error = "systemd job subscription ended".into();
        state.recompute_status();
        assert_eq!(state.status, "degraded");
        assert!(state.error().contains("subscription ended"));
    }

    #[test]
    fn systemd_subscription_invokes_manager_subscribe() {
        let mut invoked = None;
        invoke_systemd_subscribe(|member| {
            invoked = Some(member.to_owned());
            Ok(())
        })
        .unwrap();
        assert_eq!(invoked.as_deref(), Some("Subscribe"));
    }

    #[test]
    fn typed_error_names_are_stable() {
        for (error, expected) in [
            (
                BusError::UnknownBusSession(String::new()),
                "dev.cosmix.trayd.Error.UnknownBusSession",
            ),
            (
                BusError::BadBusFilter(String::new()),
                "dev.cosmix.trayd.Error.BadBusFilter",
            ),
            (
                BusError::BusUnavailable(String::new()),
                "dev.cosmix.trayd.Error.BusUnavailable",
            ),
        ] {
            assert_eq!(zbus::DBusError::name(&error).as_str(), expected);
        }
    }
}
