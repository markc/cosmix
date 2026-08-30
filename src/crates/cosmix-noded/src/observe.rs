//! SPEC 02 §4.2 broker observation extension.
//!
//! Observation is deliberately separate from topic pub/sub: it has no
//! retention, is owned by one broker connection, and uses an independent
//! drop-oldest ring so routing never awaits an observer.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use cosmix_bus::bus::{BusMessage, BusTarget};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

pub(crate) const DEFAULT_CAPACITY: usize = 1024;
pub(crate) const MIN_CAPACITY: usize = 64;
pub(crate) const MAX_CAPACITY: usize = 4096;
pub(crate) const BYTE_LIMIT: usize = 8 * 1024 * 1024;
pub(crate) const PAYLOAD_LIMIT: usize = 64 * 1024;
const MAX_CONNECTION_SUBSCRIPTIONS: usize = 4;
const MAX_BROKER_SUBSCRIPTIONS: usize = 16;
const MAX_VERB_FILTERS: usize = 32;
const MAX_VERB_BYTES: usize = 128;
const MAX_SERVICE_FILTERS: usize = 64;
const MAX_SERVICE_BYTES: usize = 128;
const DRAIN_INTERVAL: Duration = Duration::from_millis(2);
/// Per-subscription work budget on each 2 ms drainer wake. This permits
/// batching without allowing one subscriber to monopolise the broker task.
pub(crate) const MAX_DRAIN_BYTES_PER_TICK: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Direction {
    Local,
    MeshIn,
    MeshOut,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Outcome {
    Delivered,
    BrokerHandled,
    Rejected,
    Dropped,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BodyMode {
    #[default]
    None,
    Redacted,
}

impl BodyMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Redacted => "redacted",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Observation<'a> {
    pub direction: Direction,
    pub outcome: Outcome,
    pub message: &'a BusMessage,
    pub canonical_size: Option<usize>,
    pub correlation_id: Option<&'a str>,
}

impl<'a> Observation<'a> {
    pub(crate) fn from_message(
        direction: Direction,
        outcome: Outcome,
        message: &'a BusMessage,
        canonical_wire: &str,
        correlation_id: Option<&'a str>,
    ) -> Self {
        Self {
            direction,
            outcome,
            message,
            canonical_size: Some(canonical_wire.len()),
            correlation_id,
        }
    }

    /// Borrow a canonical envelope whose wire form is not otherwise needed by
    /// routing. Its size is rendered lazily only after at least one filter
    /// matches.
    pub(crate) fn canonical(
        direction: Direction,
        outcome: Outcome,
        message: &'a BusMessage,
        correlation_id: Option<&'a str>,
    ) -> Self {
        Self {
            direction,
            outcome,
            message,
            canonical_size: None,
            correlation_id,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
struct ObserveFilter {
    verbs: Vec<String>,
    services: Vec<String>,
    directions: Vec<Direction>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct StartRequest {
    filter: ObserveFilter,
    body: BodyMode,
    capacity: usize,
}

impl Default for StartRequest {
    fn default() -> Self {
        Self {
            filter: ObserveFilter::default(),
            body: BodyMode::None,
            capacity: DEFAULT_CAPACITY,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct StartResponse<'a> {
    subscription_id: &'a str,
    filter: &'a ObserveFilter,
    body: &'static str,
    capacity: usize,
    byte_limit: usize,
    redaction_policy: &'static str,
}

#[derive(Clone, Debug, Serialize)]
struct EventBody {
    seq: u64,
    ts: String,
    direction: Direction,
    outcome: Outcome,
    message_type: &'static str,
    from: Option<String>,
    to: Option<String>,
    verb: Option<String>,
    size: usize,
    correlation_id: Option<String>,
    rc: Option<i64>,
    dropped_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload: Option<RedactedPayload>,
    payload_omitted: Option<&'static str>,
}

#[derive(Clone, Debug, Serialize)]
struct RedactedPayload {
    headers: BTreeMap<String, String>,
    body: Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct QueuedEvent {
    wire_prefix: String,
    wire_suffix: String,
    accounted_bytes: usize,
}

impl QueuedEvent {
    fn render(&self, dropped_count: u64) -> String {
        let mut wire = String::with_capacity(
            self.wire_prefix
                .len()
                .saturating_add(self.wire_suffix.len())
                .saturating_add(20),
        );
        wire.push_str(&self.wire_prefix);
        wire.push_str(&dropped_count.to_string());
        wire.push_str(&self.wire_suffix);
        wire
    }
}

struct Subscription {
    id: String,
    owner_tx: mpsc::Sender<String>,
    filter: ObserveFilter,
    body: BodyMode,
    capacity: usize,
    queue: VecDeque<QueuedEvent>,
    queued_bytes: usize,
    dropped_since_delivery: u64,
    next_seq: u64,
}

#[derive(Default)]
struct Inner {
    subscriptions: Vec<Subscription>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ObserveError {
    Unauthorised,
    InvalidArgs,
    FilterInvalid,
    LimitExceeded,
    Unavailable,
}

impl ObserveError {
    pub(crate) fn rc(self) -> &'static str {
        match self {
            Self::Unavailable => "20",
            _ => "10",
        }
    }

    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::Unauthorised => "observe_unauthorised",
            Self::InvalidArgs => "observe_invalid_args",
            Self::FilterInvalid => "observe_filter_invalid",
            Self::LimitExceeded => "observe_limit_exceeded",
            Self::Unavailable => "observe_unavailable",
        }
    }
}

pub(crate) struct ObserveManager {
    allowed_services: Vec<String>,
    next_id: AtomicU64,
    active_subscriptions: AtomicUsize,
    inner: Mutex<Inner>,
}

impl ObserveManager {
    pub(crate) fn new(allowed_services: Vec<String>) -> Arc<Self> {
        let allowed_services = allowed_services
            .into_iter()
            .filter(|pattern| {
                let valid = validate_glob(pattern, MAX_SERVICE_BYTES);
                if !valid {
                    tracing::warn!(
                        pattern,
                        "ignoring invalid [observe].allowed_services pattern"
                    );
                }
                valid
            })
            .collect();
        Arc::new(Self {
            allowed_services,
            next_id: AtomicU64::new(1),
            active_subscriptions: AtomicUsize::new(0),
            inner: Mutex::new(Inner::default()),
        })
    }

    pub(crate) fn spawn_drainer(self: &Arc<Self>) {
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(DRAIN_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                manager.drain_once();
            }
        });
    }

    /// End-to-end observation hot-path gate. Callers must check this before
    /// doing work whose sole consumer is [`Self::observe`] (cloning an
    /// envelope, retaining a second wire string, or canonicalising metadata).
    /// With no subscriptions the complete routing-path cost is one relaxed
    /// atomic load.
    #[inline]
    pub(crate) fn is_active(&self) -> bool {
        self.active_subscriptions.load(Ordering::Relaxed) != 0
    }

    pub(crate) fn start(
        &self,
        service_name: Option<&str>,
        same_node: bool,
        owner_tx: &mpsc::Sender<String>,
        request_id: Option<&str>,
        body: &str,
    ) -> Result<String, ObserveError> {
        self.authorise(service_name, same_node)?;
        let mut request = parse_start_request(body)?;
        normalise_filter(&mut request.filter)?;
        request.capacity = request.capacity.clamp(MIN_CAPACITY, MAX_CAPACITY);

        let id = format!("observe-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let mut inner = lock(&self.inner);
        if inner.subscriptions.len() >= MAX_BROKER_SUBSCRIPTIONS
            || inner
                .subscriptions
                .iter()
                .filter(|subscription| subscription.owner_tx.same_channel(owner_tx))
                .count()
                >= MAX_CONNECTION_SUBSCRIPTIONS
        {
            return Err(ObserveError::LimitExceeded);
        }
        let response = StartResponse {
            subscription_id: &id,
            filter: &request.filter,
            body: request.body.as_str(),
            capacity: request.capacity,
            byte_limit: BYTE_LIMIT,
            redaction_policy: "observe-v1",
        };
        let response_body =
            serde_json::to_string(&response).map_err(|_| ObserveError::Unavailable)?;
        let response_wire =
            response_wire("noded.observe.start", request_id, "0", None, &response_body);
        if owner_tx.try_send(response_wire).is_err() {
            return Err(ObserveError::Unavailable);
        }
        // Insert only after the acknowledgement is in the connection FIFO.
        // The same mutex serialises drain/stop, making this ordering a fence.
        inner.subscriptions.push(Subscription {
            id: id.clone(),
            owner_tx: owner_tx.clone(),
            filter: request.filter,
            body: request.body,
            capacity: request.capacity,
            queue: VecDeque::new(),
            queued_bytes: 0,
            dropped_since_delivery: 0,
            next_seq: 1,
        });
        self.active_subscriptions.fetch_add(1, Ordering::Release);
        Ok(id)
    }

    pub(crate) async fn stop(
        &self,
        owner_tx: &mpsc::Sender<String>,
        request_id: Option<&str>,
        subscription_id: &str,
    ) -> bool {
        let stopped = {
            let mut inner = lock(&self.inner);
            let owned = inner.subscriptions.iter().position(|subscription| {
                subscription.id == subscription_id && subscription.owner_tx.same_channel(owner_tx)
            });
            let stopped = owned.is_some();
            if let Some(index) = owned {
                // Remove and purge while holding the same mutex used by
                // observe/drain. Once released no event for this id can enter
                // the connection FIFO.
                inner.subscriptions.remove(index);
                self.active_subscriptions.fetch_sub(1, Ordering::Release);
            }
            stopped
        };
        let body = format!(r#"{{"stopped":{stopped}}}"#);
        let wire = response_wire("noded.observe.stop", request_id, "0", None, &body);
        // Responses are not droppable observer traffic. Await ordinary channel
        // capacity after the ring has been fenced; already-queued events remain
        // before this ACK in FIFO order and none can follow it.
        let _ = owner_tx.send(wire).await;
        stopped
    }

    pub(crate) fn remove_owner(&self, owner_tx: &mpsc::Sender<String>) {
        let mut inner = lock(&self.inner);
        let before = inner.subscriptions.len();
        inner
            .subscriptions
            .retain(|subscription| !subscription.owner_tx.same_channel(owner_tx));
        let removed = before.saturating_sub(inner.subscriptions.len());
        if removed > 0 {
            self.active_subscriptions
                .fetch_sub(removed, Ordering::Release);
        }
    }

    pub(crate) fn observe(&self, observation: Observation<'_>) {
        // The zero-subscriber production hot path is exactly one relaxed
        // atomic load: no clone, allocation, serialisation, or mutex.
        if !self.is_active() {
            return;
        }
        if is_observer_control_or_event(observation.message) {
            return;
        }

        struct Target {
            id: String,
            mode: BodyMode,
            seq: u64,
        }
        // Match borrowed metadata before cloning anything. Reserve each matched
        // sequence while snapshotting; gaps are permitted if the subscription
        // is stopped before the prepared event is enqueued.
        let targets: Vec<Target> = {
            let mut inner = lock(&self.inner);
            inner
                .subscriptions
                .iter_mut()
                .filter(|subscription| matches_filter(&subscription.filter, &observation))
                .map(|subscription| {
                    let seq = subscription.next_seq;
                    subscription.next_seq = subscription.next_seq.wrapping_add(1);
                    Target {
                        id: subscription.id.clone(),
                        mode: subscription.body,
                        seq,
                    }
                })
                .collect()
        };
        if targets.is_empty() {
            return;
        }

        let metadata = metadata_event(&observation);
        let redacted = targets
            .iter()
            .any(|target| target.mode == BodyMode::Redacted)
            .then(|| redacted_event(&observation));

        // All JSON and Bus wire serialisation happens outside the observation
        // mutex. The queued template leaves only dropped_count interpolation
        // for the drainer.
        let prepared: Vec<(String, QueuedEvent)> = targets
            .into_iter()
            .filter_map(|target| {
                let mut event = match target.mode {
                    BodyMode::None => metadata.clone(),
                    BodyMode::Redacted => redacted.clone().unwrap_or_else(|| metadata.clone()),
                };
                event.seq = target.seq;
                event_wire_template(&target.id, &event)
                    .ok()
                    .map(|queued| (target.id, queued))
            })
            .collect();

        let mut inner = lock(&self.inner);
        for (id, queued) in prepared {
            let Some(subscription) = inner
                .subscriptions
                .iter_mut()
                .find(|subscription| subscription.id == id)
            else {
                continue;
            };
            let accounted_bytes = queued.accounted_bytes;
            if accounted_bytes > BYTE_LIMIT {
                subscription.dropped_since_delivery =
                    subscription.dropped_since_delivery.saturating_add(1);
                continue;
            }
            while subscription.queue.len() >= subscription.capacity
                || subscription.queued_bytes.saturating_add(accounted_bytes) > BYTE_LIMIT
            {
                let Some(evicted) = subscription.queue.pop_front() else {
                    break;
                };
                subscription.queued_bytes = subscription
                    .queued_bytes
                    .saturating_sub(evicted.accounted_bytes);
                subscription.dropped_since_delivery =
                    subscription.dropped_since_delivery.saturating_add(1);
            }
            subscription.queued_bytes = subscription.queued_bytes.saturating_add(accounted_bytes);
            subscription.queue.push_back(queued);
        }
    }

    fn authorise<'a>(
        &self,
        service_name: Option<&'a str>,
        same_node: bool,
    ) -> Result<&'a str, ObserveError> {
        let service = service_name
            .filter(|service| !service.is_empty())
            .ok_or(ObserveError::Unauthorised)?;
        if !same_node
            || !self
                .allowed_services
                .iter()
                .any(|pattern| anchored_glob_matches(pattern, service))
        {
            return Err(ObserveError::Unauthorised);
        }
        Ok(service)
    }

    fn drain_once(&self) {
        struct DrainBatch {
            id: String,
            dropped_count: u64,
            events: Vec<QueuedEvent>,
        }
        struct RenderedBatch {
            snapshot: DrainBatch,
            wires: Vec<String>,
        }

        // Clone a bounded front slice under the mutex, then perform every
        // string rendering operation after releasing it. A concurrent enqueue
        // may evict a snapshotted front; the validation pass below detects
        // that and leaves the live queue untouched for the next tick.
        let snapshots: Vec<DrainBatch> = {
            let inner = lock(&self.inner);
            inner
                .subscriptions
                .iter()
                .filter_map(|subscription| {
                    let mut bytes = 0usize;
                    let events: Vec<QueuedEvent> = subscription
                        .queue
                        .iter()
                        .take_while(|event| {
                            if bytes > 0
                                && bytes.saturating_add(event.accounted_bytes)
                                    > MAX_DRAIN_BYTES_PER_TICK
                            {
                                return false;
                            }
                            bytes = bytes.saturating_add(event.accounted_bytes);
                            true
                        })
                        .cloned()
                        .collect();
                    (!events.is_empty()).then(|| DrainBatch {
                        id: subscription.id.clone(),
                        dropped_count: subscription.dropped_since_delivery,
                        events,
                    })
                })
                .collect()
        };
        let rendered: Vec<RenderedBatch> = snapshots
            .into_iter()
            .map(|snapshot| {
                let wires = snapshot
                    .events
                    .iter()
                    .enumerate()
                    .map(|(index, event)| {
                        event.render(if index == 0 {
                            snapshot.dropped_count
                        } else {
                            0
                        })
                    })
                    .collect();
                RenderedBatch { snapshot, wires }
            })
            .collect();

        let mut inner = lock(&self.inner);
        let mut closed = Vec::new();
        for batch in rendered {
            let Some((index, subscription)) = inner
                .subscriptions
                .iter_mut()
                .enumerate()
                .find(|(_, subscription)| subscription.id == batch.snapshot.id)
            else {
                continue;
            };
            if subscription.dropped_since_delivery != batch.snapshot.dropped_count
                || !subscription
                    .queue
                    .iter()
                    .take(batch.snapshot.events.len())
                    .eq(batch.snapshot.events.iter())
            {
                continue;
            }
            for wire in batch.wires {
                match subscription.owner_tx.try_send(wire) {
                    Ok(()) => {
                        let delivered = subscription.queue.pop_front().expect("front exists");
                        subscription.queued_bytes = subscription
                            .queued_bytes
                            .saturating_sub(delivered.accounted_bytes);
                        subscription.dropped_since_delivery = 0;
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => break,
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        closed.push(index);
                        break;
                    }
                }
            }
        }
        for index in closed.into_iter().rev() {
            inner.subscriptions.remove(index);
            self.active_subscriptions.fetch_sub(1, Ordering::Release);
        }
    }

    #[cfg(test)]
    fn subscription_count(&self) -> usize {
        lock(&self.inner).subscriptions.len()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn parse_start_request(body: &str) -> Result<StartRequest, ObserveError> {
    if body.trim().is_empty() {
        return Ok(StartRequest::default());
    }
    serde_json::from_str(body).map_err(|_| ObserveError::InvalidArgs)
}

fn normalise_filter(filter: &mut ObserveFilter) -> Result<(), ObserveError> {
    if filter.verbs.len() > MAX_VERB_FILTERS || filter.services.len() > MAX_SERVICE_FILTERS {
        return Err(ObserveError::LimitExceeded);
    }
    if filter
        .verbs
        .iter()
        .any(|pattern| !validate_glob(pattern, MAX_VERB_BYTES))
        || filter.services.iter().any(|service| {
            service.is_empty()
                || service.len() > MAX_SERVICE_BYTES
                || !service
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        })
    {
        return Err(ObserveError::FilterInvalid);
    }
    filter.verbs.sort();
    filter.verbs.dedup();
    filter.services.sort();
    filter.services.dedup();
    if filter.directions.is_empty() {
        filter.directions = vec![Direction::Local, Direction::MeshIn, Direction::MeshOut];
    } else {
        filter.directions.sort_by_key(|direction| match direction {
            Direction::Local => 0,
            Direction::MeshIn => 1,
            Direction::MeshOut => 2,
        });
        filter.directions.dedup();
    }
    Ok(())
}

fn validate_glob(pattern: &str, max_bytes: usize) -> bool {
    !pattern.is_empty()
        && pattern.len() <= max_bytes
        && pattern
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'*'))
}

pub(crate) fn anchored_glob_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let (mut p, mut v, mut star, mut retry) = (0usize, 0usize, None, 0usize);
    while v < value.len() {
        if p < pattern.len() && pattern[p] == value[v] {
            p += 1;
            v += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            retry = v;
        } else if let Some(star_at) = star {
            p = star_at + 1;
            retry += 1;
            v = retry;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

fn matches_filter(filter: &ObserveFilter, observation: &Observation<'_>) -> bool {
    if !filter.directions.contains(&observation.direction) {
        return false;
    }
    let verb = observation.message.command_name().unwrap_or("");
    if !filter.verbs.is_empty()
        && !filter
            .verbs
            .iter()
            .any(|pattern| anchored_glob_matches(pattern, verb))
    {
        return false;
    }
    filter.services.is_empty()
        || filter
            .services
            .iter()
            .any(|service| endpoint_matches(observation.message, service))
}

fn endpoint_matches(message: &BusMessage, service: &str) -> bool {
    [message.from_addr(), message.to_addr()]
        .into_iter()
        .flatten()
        .any(|endpoint| {
            if endpoint == service {
                return true;
            }
            match BusTarget::parse(endpoint) {
                Some(BusTarget::Local(address)) => {
                    address.service.as_deref().unwrap_or("noded") == service
                }
                _ => false,
            }
        })
}

fn metadata_event(observation: &Observation<'_>) -> EventBody {
    EventBody {
        seq: 0,
        ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        direction: observation.direction,
        outcome: observation.outcome,
        message_type: classify_message_type(observation.message),
        from: observation.message.from_addr().map(ToString::to_string),
        to: observation.message.to_addr().map(ToString::to_string),
        verb: observation.message.command_name().map(ToString::to_string),
        size: observation
            .canonical_size
            .unwrap_or_else(|| observation.message.to_wire().len()),
        correlation_id: observation.correlation_id.map(ToString::to_string),
        rc: observation
            .message
            .get("rc")
            .and_then(|value| value.parse().ok()),
        dropped_count: 0,
        payload: None,
        payload_omitted: Some("disabled"),
    }
}

fn redacted_event(observation: &Observation<'_>) -> EventBody {
    let mut event = metadata_event(observation);
    if !payload_policy_allows(observation) {
        event.payload_omitted = Some("policy");
        return event;
    }
    let captured_bytes = observation.message.body.len()
        + observation
            .message
            .headers
            .iter()
            .map(|(name, value)| name.len().saturating_add(value.len()))
            .sum::<usize>();
    if captured_bytes > PAYLOAD_LIMIT {
        event.payload_omitted = Some("oversize");
        return event;
    }
    let body = if observation.message.body.trim().is_empty() {
        Value::Null
    } else {
        let Ok(mut value) = serde_json::from_str::<Value>(&observation.message.body) else {
            event.payload_omitted = Some("opaque");
            return event;
        };
        if !matches!(value, Value::Object(_) | Value::Array(_)) {
            event.payload_omitted = Some("opaque");
            return event;
        }
        redact_value(&mut value);
        value
    };
    let headers = observation
        .message
        .headers
        .iter()
        .map(|(name, value)| {
            (
                name.clone(),
                if denied_field(name) {
                    "[REDACTED]".to_string()
                } else {
                    value.clone()
                },
            )
        })
        .collect();
    event.payload = Some(RedactedPayload { headers, body });
    event.payload_omitted = None;
    event
}

#[derive(Clone, Copy)]
struct PayloadPolicyRule {
    service: &'static str,
    verb: &'static str,
}

/// Conservative whole-payload omissions applied before field redaction.
/// Entries are deliberately data, not conditionals, so deployments can grow
/// this into a configured policy without changing the evaluation order.
const PAYLOAD_POLICY_DENY: &[PayloadPolicyRule] = &[
    PayloadPolicyRule {
        service: "noded",
        verb: "noded.register",
    },
    PayloadPolicyRule {
        service: "noded",
        verb: "noded.admit.*",
    },
    PayloadPolicyRule {
        service: "*",
        verb: "*.register",
    },
    PayloadPolicyRule {
        service: "*",
        verb: "*.register.*",
    },
    PayloadPolicyRule {
        service: "*",
        verb: "register",
    },
    PayloadPolicyRule {
        service: "*",
        verb: "register.*",
    },
    PayloadPolicyRule {
        service: "*",
        verb: "*.registration",
    },
    PayloadPolicyRule {
        service: "*",
        verb: "*.registration.*",
    },
    PayloadPolicyRule {
        service: "*",
        verb: "registration",
    },
    PayloadPolicyRule {
        service: "*",
        verb: "registration.*",
    },
    PayloadPolicyRule {
        service: "*",
        verb: "*.auth",
    },
    PayloadPolicyRule {
        service: "*",
        verb: "*.auth.*",
    },
    PayloadPolicyRule {
        service: "*",
        verb: "auth",
    },
    PayloadPolicyRule {
        service: "*",
        verb: "auth.*",
    },
    PayloadPolicyRule {
        service: "*",
        verb: "*.login",
    },
    PayloadPolicyRule {
        service: "*",
        verb: "*.login.*",
    },
    PayloadPolicyRule {
        service: "*",
        verb: "login",
    },
    PayloadPolicyRule {
        service: "*",
        verb: "login.*",
    },
    PayloadPolicyRule {
        service: "*",
        verb: "*.token",
    },
    PayloadPolicyRule {
        service: "*",
        verb: "*.token.*",
    },
    PayloadPolicyRule {
        service: "*",
        verb: "token",
    },
    PayloadPolicyRule {
        service: "*",
        verb: "token.*",
    },
];

fn payload_policy_allows(observation: &Observation<'_>) -> bool {
    let verb = observation.message.command_name().unwrap_or("");
    !PAYLOAD_POLICY_DENY.iter().any(|rule| {
        anchored_glob_matches(rule.verb, verb)
            && (rule.service == "*"
                || endpoint_matches(observation.message, rule.service)
                || verb
                    .strip_prefix(rule.service)
                    .is_some_and(|suffix| suffix.starts_with('.')))
    })
}

fn redact_value(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if denied_field(key) {
                    *value = Value::String("[REDACTED]".to_string());
                } else {
                    redact_value(value);
                }
            }
        }
        Value::Array(values) => values.iter_mut().for_each(redact_value),
        _ => {}
    }
}

fn denied_field(field: &str) -> bool {
    let normalised: String = field
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    [
        "credential",
        "authorisation",
        "authorization",
        "cookie",
        "password",
        "token",
        "apikey",
        "privatekey",
        "signature",
    ]
    .iter()
    .any(|denied| normalised.contains(denied))
}

fn classify_message_type(message: &BusMessage) -> &'static str {
    match message.message_type() {
        Some("response") => "response",
        Some("event") => "event",
        Some("stream") => "stream",
        _ => "request",
    }
}

fn event_wire_template(
    subscription_id: &str,
    event: &EventBody,
) -> Result<QueuedEvent, serde_json::Error> {
    let mut event = event.clone();
    event.dropped_count = u64::MAX;
    let body = serde_json::to_string(&event)?;
    let mut message = BusMessage::new()
        .with_header("type", "event")
        .with_header("from", "noded")
        .with_header("command", "noded.observe.event")
        .with_header("subscription_id", subscription_id);
    message.body = body;
    let wire = message.to_wire();
    let marker = format!(r#""dropped_count":{}"#, u64::MAX);
    let marker_start = wire
        .find(&marker)
        .expect("serialised observation contains dropped_count")
        + r#""dropped_count":"#.len();
    let marker_end = marker_start + u64::MAX.to_string().len();
    let accounted_bytes = wire.len();
    Ok(QueuedEvent {
        wire_prefix: wire[..marker_start].to_string(),
        wire_suffix: wire[marker_end..].to_string(),
        accounted_bytes,
    })
}

fn is_observer_control_or_event(message: &BusMessage) -> bool {
    let command = message.command_name();
    let is_event = command == Some("noded.observe.event")
        && message.message_type() == Some("event")
        && message.from_addr() == Some("noded");
    let is_control = matches!(command, Some("noded.observe.start" | "noded.observe.stop"))
        && message.message_type() != Some("response")
        && addressed_to_noded(message);
    is_event || is_control
}

fn addressed_to_noded(message: &BusMessage) -> bool {
    match message.to_addr() {
        None | Some("noded") => true,
        Some(target) => match BusTarget::parse(target) {
            Some(BusTarget::Local(address)) => {
                address.service.as_deref().unwrap_or("noded") == "noded"
            }
            _ => false,
        },
    }
}

pub(crate) fn response_wire(
    command: &str,
    request_id: Option<&str>,
    rc: &str,
    error: Option<&str>,
    body: &str,
) -> String {
    let mut response = BusMessage::new()
        .with_header("type", "response")
        .with_header("from", "noded")
        .with_header("command", command)
        .with_header("rc", rc);
    if let Some(id) = request_id {
        response.set("id", id);
    }
    if let Some(error) = error {
        response.set("error", error);
    }
    response.body = body.to_string();
    response.to_wire()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(command: &str) -> BusMessage {
        BusMessage::new()
            .with_header("type", "request")
            .with_header("from", "studio-bevy-7")
            .with_header("to", "musicd")
            .with_header("command", command)
            .with_header("id", "caller-42")
    }

    fn observe_command(manager: &ObserveManager, command: &str) {
        let message = message(command);
        manager.observe(Observation::from_message(
            Direction::Local,
            Outcome::Delivered,
            &message,
            &message.to_wire(),
            Some("caller-42"),
        ));
    }

    fn manager() -> Arc<ObserveManager> {
        ObserveManager::new(vec!["tower-bevy-*".into()])
    }

    fn start(manager: &ObserveManager, tx: &mpsc::Sender<String>, body: &str) -> String {
        manager
            .start(Some("tower-bevy-9"), true, tx, Some("start-1"), body)
            .unwrap()
    }

    #[test]
    fn authorisation_requires_registered_local_allowlisted_service() {
        let manager = manager();
        assert_eq!(
            manager.authorise(None, true),
            Err(ObserveError::Unauthorised)
        );
        assert_eq!(
            manager.authorise(Some("tower-bevy-9"), false),
            Err(ObserveError::Unauthorised)
        );
        assert_eq!(
            manager.authorise(Some("studio-bevy-9"), true),
            Err(ObserveError::Unauthorised)
        );
        assert_eq!(
            manager.authorise(Some("tower-bevy-9"), true),
            Ok("tower-bevy-9")
        );
    }

    #[test]
    fn filter_validation_enforces_limits_and_anchored_globs() {
        assert!(anchored_glob_matches("maild.*", "maild.account.list"));
        assert!(!anchored_glob_matches("maild.*", "xmaild.account.list"));
        assert!(!anchored_glob_matches("maild.*", "maild"));
        let too_many = serde_json::json!({
            "filter": {"verbs": (0..33).map(|n| format!("svc.{n}")).collect::<Vec<_>>()}
        })
        .to_string();
        let mut parsed = parse_start_request(&too_many).unwrap();
        assert_eq!(
            normalise_filter(&mut parsed.filter),
            Err(ObserveError::LimitExceeded)
        );
        let invalid = r#"{"filter":{"verbs":["maild.[secret]"]}}"#;
        let mut parsed = parse_start_request(invalid).unwrap();
        assert_eq!(
            normalise_filter(&mut parsed.filter),
            Err(ObserveError::FilterInvalid)
        );
    }

    #[test]
    fn capacity_is_clamped_to_contract_bounds() {
        let manager = manager();
        let (tx, mut rx) = mpsc::channel(8);
        start(&manager, &tx, r#"{"capacity":1}"#);
        let response = cosmix_bus::bus::parse(&rx.try_recv().unwrap()).unwrap();
        let body: Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(body["capacity"], MIN_CAPACITY);
    }

    #[test]
    fn ring_drops_oldest_and_reports_evictions_on_next_delivery() {
        let manager = manager();
        let (tx, mut rx) = mpsc::channel(8);
        let id = start(
            &manager,
            &tx,
            r#"{"filter":{"verbs":["musicd.*"]},"capacity":64}"#,
        );
        let _ack = rx.try_recv().unwrap();
        for index in 0..66 {
            observe_command(&manager, &format!("musicd.event.{index}"));
        }
        manager.drain_once();
        let wire = rx.try_recv().unwrap();
        let event = cosmix_bus::bus::parse(&wire).unwrap();
        assert_eq!(event.get("subscription_id"), Some(id.as_str()));
        let body: Value = serde_json::from_str(&event.body).unwrap();
        assert_eq!(body["seq"], 3);
        assert_eq!(body["dropped_count"], 2);
    }

    #[test]
    fn drainer_batches_ready_events_in_one_wake() {
        let manager = manager();
        let (tx, mut rx) = mpsc::channel(32);
        start(&manager, &tx, r#"{"capacity":64}"#);
        let _ack = rx.try_recv().unwrap();
        for index in 0..10 {
            observe_command(&manager, &format!("musicd.event.{index}"));
        }

        manager.drain_once();

        let delivered = std::iter::from_fn(|| rx.try_recv().ok()).count();
        assert_eq!(delivered, 10, "drainer regressed to one event per wake");
    }

    #[test]
    fn byte_ceiling_evicts_oldest_even_below_event_capacity() {
        let manager = manager();
        let (tx, mut rx) = mpsc::channel(8);
        start(&manager, &tx, r#"{"capacity":4096}"#);
        let _ack = rx.try_recv().unwrap();
        for index in 0..2500 {
            let mut message = message("musicd.large");
            message.set("from", &format!("studio-{index}-{}", "x".repeat(4096)));
            manager.observe(Observation::from_message(
                Direction::Local,
                Outcome::Delivered,
                &message,
                &message.to_wire(),
                Some(&index.to_string()),
            ));
        }
        let inner = lock(&manager.inner);
        let subscription = &inner.subscriptions[0];
        assert!(subscription.queued_bytes <= BYTE_LIMIT);
        assert!(subscription.queue.len() < 2500);
        assert!(subscription.dropped_since_delivery > 0);
    }

    #[tokio::test]
    async fn stop_is_a_fifo_fence_and_non_owner_leaks_no_existence() {
        let manager = manager();
        let (tx, mut rx) = mpsc::channel(8);
        let (other_tx, mut other_rx) = mpsc::channel(8);
        let id = start(&manager, &tx, "{}");
        let _start_ack = rx.try_recv().unwrap();
        observe_command(&manager, "musicd.play");
        assert!(!manager.stop(&other_tx, Some("x"), &id).await);
        let other = cosmix_bus::bus::parse(&other_rx.try_recv().unwrap()).unwrap();
        assert_eq!(other.body, r#"{"stopped":false}"#);
        assert!(manager.stop(&tx, Some("stop-1"), &id).await);
        manager.drain_once();
        let stop = cosmix_bus::bus::parse(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(stop.body, r#"{"stopped":true}"#);
        assert!(rx.try_recv().is_err(), "purged event must not follow stop");
    }

    #[tokio::test]
    async fn stop_ack_waits_for_normal_reply_capacity_instead_of_dropping() {
        let manager = manager();
        let (tx, mut rx) = mpsc::channel(1);
        let id = start(&manager, &tx, "{}");
        let stop = manager.stop(&tx, Some("stop-1"), &id);
        tokio::pin!(stop);
        assert!(
            tokio::time::timeout(Duration::from_millis(5), &mut stop)
                .await
                .is_err(),
            "full reply channel must backpressure the ACK, not discard it"
        );
        let _start_ack = rx.recv().await.unwrap();
        assert!(stop.await);
        let response = cosmix_bus::bus::parse(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(response.body, r#"{"stopped":true}"#);
    }

    #[test]
    fn disconnect_removes_every_owned_subscription() {
        let manager = manager();
        assert!(!manager.is_active());
        let (tx, mut rx) = mpsc::channel(8);
        start(&manager, &tx, "{}");
        start(&manager, &tx, "{}");
        let _ = rx.try_recv();
        let _ = rx.try_recv();
        assert_eq!(manager.subscription_count(), 2);
        assert!(manager.is_active());
        manager.remove_owner(&tx);
        assert_eq!(manager.subscription_count(), 0);
        assert!(!manager.is_active());
    }

    #[test]
    fn redaction_is_recursive_case_insensitive_and_opaque_safe() {
        let mut message = message("maild.account.set");
        message.set("Authorization", "Bearer secret");
        message.body = serde_json::json!({
            "Password": "secret",
            "nested": {"api_key": "secret", "safe": "visible"},
        })
        .to_string();
        let observation = Observation::from_message(
            Direction::Local,
            Outcome::Delivered,
            &message,
            &message.to_wire(),
            Some("caller-42"),
        );
        let event = redacted_event(&observation);
        let payload = event.payload.unwrap();
        assert_eq!(payload.headers["Authorization"], "[REDACTED]");
        assert_eq!(payload.body["Password"], "[REDACTED]");
        assert_eq!(payload.body["nested"]["api_key"], "[REDACTED]");
        assert_eq!(payload.body["nested"]["safe"], "visible");

        message.body = "secret bearer bytes".into();
        let opaque = Observation::from_message(
            Direction::Local,
            Outcome::Delivered,
            &message,
            &message.to_wire(),
            Some("caller-42"),
        );
        assert_eq!(redacted_event(&opaque).payload_omitted, Some("opaque"));

        message.body = "x".repeat(PAYLOAD_LIMIT + 1);
        let oversize = Observation::from_message(
            Direction::Local,
            Outcome::Delivered,
            &message,
            &message.to_wire(),
            Some("caller-42"),
        );
        assert_eq!(redacted_event(&oversize).payload_omitted, Some("oversize"));

        message.body = "{}".into();
        message.set("x-large", &"x".repeat(PAYLOAD_LIMIT));
        let oversize = Observation::from_message(
            Direction::Local,
            Outcome::Delivered,
            &message,
            &message.to_wire(),
            Some("caller-42"),
        );
        assert_eq!(redacted_event(&oversize).payload_omitted, Some("oversize"));
    }

    #[test]
    fn policy_omits_known_authentication_families_before_field_redaction() {
        for command in [
            "noded.register",
            "noded.admit.response",
            "session.auth.login",
            "registration.begin",
            "token.rotate",
        ] {
            let mut message = message(command);
            if command.starts_with("noded.") {
                message.set("to", "noded");
            }
            message.body = r#"{"safe":"still sensitive as a whole"}"#.into();
            let observation = Observation::from_message(
                Direction::Local,
                Outcome::Delivered,
                &message,
                &message.to_wire(),
                Some("caller-42"),
            );
            let event = redacted_event(&observation);
            assert!(event.payload.is_none(), "{command}");
            assert_eq!(event.payload_omitted, Some("policy"), "{command}");
        }
    }

    #[test]
    fn only_real_observe_control_and_event_frames_are_suppressed() {
        let manager = manager();
        let (tx, mut rx) = mpsc::channel(8);
        start(&manager, &tx, "{}");
        let _ = rx.try_recv();
        let mut control = message("noded.observe.start");
        control.set("to", "noded");
        manager.observe(Observation::from_message(
            Direction::Local,
            Outcome::BrokerHandled,
            &control,
            &control.to_wire(),
            Some("caller-42"),
        ));
        let event = BusMessage::new()
            .with_header("type", "event")
            .with_header("from", "noded")
            .with_header("command", "noded.observe.event");
        manager.observe(Observation::from_message(
            Direction::Local,
            Outcome::Delivered,
            &event,
            &event.to_wire(),
            None,
        ));
        observe_command(&manager, "noded.observe.hidden");
        manager.drain_once();
        let observed = cosmix_bus::bus::parse(&rx.try_recv().unwrap()).unwrap();
        let body: Value = serde_json::from_str(&observed.body).unwrap();
        assert_eq!(body["verb"], "noded.observe.hidden");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn supplied_correlation_id_survives_internal_wire_id() {
        let manager = manager();
        let (tx, mut rx) = mpsc::channel(8);
        start(&manager, &tx, "{}");
        let _ = rx.try_recv();
        let mut response = message("musicd.play");
        response.set("type", "response");
        response.set("id", "noded-99");
        manager.observe(Observation::from_message(
            Direction::Local,
            Outcome::Delivered,
            &response,
            &response.to_wire(),
            Some("caller-42"),
        ));
        manager.drain_once();
        let event = cosmix_bus::bus::parse(&rx.try_recv().unwrap()).unwrap();
        let body: Value = serde_json::from_str(&event.body).unwrap();
        assert_eq!(body["correlation_id"], "caller-42");
    }
}
