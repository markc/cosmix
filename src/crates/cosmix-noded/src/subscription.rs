//! Subscription broker — shared registry for `topic.*` and `ui.subscribe`.
//!
//! Implements the topic pub/sub primitive defined in
//! `src/_doc/2026-04-10-topic-pubsub-v1.md`. Maintains a unified subscription
//! table for both topic subscriptions (data channels with cached latest value)
//! and UI event subscriptions (event filters for Mix `on` handlers). The
//! registry is shared under the hood so peer-disconnect cleanup walks a single
//! reverse index. On the wire the two families are distinct (§ 4.2 of the
//! delta).
//!
//! The broker treats topic payloads as **opaque bytes** (§ 2.1 of the delta).
//! It parses the outer `topic.publish` wrapper to extract the body but never
//! interprets the body's content — it only injects reserved routing headers
//! (`topic`, `topic_seq`, `topic_stale`, `topic_op`, `broker_origin`) and
//! forwards the annotated inner message verbatim.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cosmix_bus::bus::{self, BusMessage};
use tokio::sync::{RwLock, mpsc};

/// Maximum cached snapshot body size in bytes (§ 3.11.2).
pub const MAX_SNAPSHOT_BYTES: usize = 1024 * 1024;

/// Grace period after producer disconnect before a stale snapshot is purged
/// (§ 10.3.1, aligned with the 60s panel orphan timeout).
pub const STALE_SNAPSHOT_TTL: Duration = Duration::from_secs(60);

/// Janitor sweep interval — how often we check for stale snapshots past TTL.
pub const JANITOR_INTERVAL: Duration = Duration::from_secs(10);

pub(crate) const BROKER_ORIGIN_HEADER: &str = "broker_origin";

/// Reserved header names the broker injects into deliveries. Any producer-
/// supplied headers with these names are unconditionally overwritten
/// (§ 3.11.2 security property).
pub const RESERVED_HEADERS: &[&str] = &[
    "topic",
    "topic_seq",
    "topic_stale",
    "topic_op",
    BROKER_ORIGIN_HEADER,
];

// ── Public types ──

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BrokerOrigin {
    Local,
    Mesh,
}

impl BrokerOrigin {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Mesh => "mesh",
        }
    }
}

pub(crate) fn strip_broker_origin(message: &mut BusMessage) -> bool {
    let before = message.headers.len();
    message
        .headers
        .retain(|name, _| !name.eq_ignore_ascii_case(BROKER_ORIGIN_HEADER));
    before != message.headers.len()
}

pub(crate) fn stamp_broker_origin(message: &mut BusMessage, origin: BrokerOrigin) {
    strip_broker_origin(message);
    message.set(BROKER_ORIGIN_HEADER, origin.as_str());
}

/// Subscription identifier. Format is debuggable:
/// - Topic subs: `"<peer>::topic::<name>"`
/// - UI event subs: `"<peer>::ui::<source>::<action>"`
pub type SubscriptionId = String;

#[derive(Debug, Clone)]
pub enum SubKind {
    Topic {
        name: String,
    },
    /// Stub in v1 — registered in the shared table but no event routing yet.
    /// See § 6 Phase A of the delta. Fields are stored but unused until
    /// ui.event routing is wired.
    #[allow(dead_code)]
    UiEvent {
        source: String,
        action: Option<String>,
    },
}

/// Per-subscription delivery filter applied at fan-out time.
///
/// SPEC 12 §15.5 reserves `<svc>.props.records.changed` and
/// `<svc>.props.audit` as per-service topics shared across every
/// namespace owned by that service. Without a filter, a subscriber
/// granted access to one namespace under `maild` would receive events
/// from every namespace under `maild` (a cross-namespace leak across
/// authorisation scopes). The filter is matched against the parsed
/// JSON body's top-level `namespace` field, extracted once per publish
/// — see [`SubscriptionBroker::publish`].
///
/// Currently only `namespace` is supported; the struct stays open for
/// future fields (e.g. per-key scoping) without a wire break.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BodyFilter {
    pub namespace: String,
}

#[derive(Clone)]
pub struct Subscription {
    pub id: SubscriptionId,
    pub peer: String,
    pub kind: SubKind,
    pub tx: mpsc::Sender<String>,
    /// `None` = receive every publish to this topic.
    /// `Some(BodyFilter{namespace})` = receive only publishes whose
    /// body JSON's top-level `namespace` field equals the filter.
    pub filter: Option<BodyFilter>,
}

/// Metadata exposed by `topic.list`.
#[derive(Debug, Clone)]
pub struct TopicInfo {
    pub name: String,
    pub subscribers: usize,
    pub has_snapshot: bool,
    pub snapshot_seq: u64,
    pub snapshot_size: usize,
    pub last_publisher: Option<String>,
    pub stale: bool,
}

#[derive(Debug)]
pub enum PublishError {
    ReservedName,
    PayloadTooLarge {
        #[allow(dead_code)]
        size: usize,
        limit: usize,
    },
    MalformedPayload,
}

impl PublishError {
    pub fn error_body(&self) -> String {
        match self {
            PublishError::ReservedName => r#"{"error": "reserved_name"}"#.to_string(),
            PublishError::PayloadTooLarge { limit, .. } => {
                format!(r#"{{"error": "payload_too_large", "limit": {}}}"#, limit)
            }
            PublishError::MalformedPayload => r#"{"error": "malformed_payload"}"#.to_string(),
        }
    }
}

/// One notification the broker wants the caller to deliver.
/// Returned from operations that may cross a 0↔N subscriber transition.
///
/// `target_tx` is the direct outbound channel for the target peer, captured
/// at the moment the notification was built. This is load-bearing for
/// anonymous publishers: they're not in the broker's registered-services map,
/// so a registry lookup by `target_peer` would miss and the notification
/// would be silently dropped. Carrying the Sender on the Notification
/// itself lets `dispatch_notifications` deliver directly without any
/// lookup, and works uniformly for both registered and anonymous peers.
///
/// The `target_peer` string is retained for diagnostics and logging —
/// `target_tx` is the delivery mechanism, `target_peer` is the identity.
#[derive(Debug, Clone)]
pub struct Notification {
    pub target_peer: String,
    pub target_tx: tokio::sync::mpsc::Sender<String>,
    pub wire: String,
}

// ── Internal state ──

struct CachedSnapshot {
    /// The annotated inner Bus message (with `topic` + `topic_seq` headers
    /// already injected). Cloned and re-rendered per delivery so we can
    /// toggle the `topic_stale` header without mutating the cache.
    body: BusMessage,
    seq: u64,
    published_by: String,
    #[allow(dead_code)]
    published_at: Instant,
    /// `Some` when the publisher has disconnected; janitor purges after TTL.
    stale_since: Option<Instant>,
    size: usize,
    /// Extracted once from the inner body's JSON `namespace` field at
    /// publish time. Used by [`SubscriptionBroker::subscribe_topic_filtered`]
    /// to skip snapshot replay when a filter is set and doesn't match.
    /// `None` if the body wasn't JSON or had no top-level `namespace`.
    body_namespace: Option<String>,
}

struct TopicState {
    snapshot: Option<CachedSnapshot>,
    /// Service name or synthesized anon id of the most recent publisher.
    last_publisher: Option<String>,
    /// Outbound channel of the last publisher, for `topic.active`/`topic.idle`
    /// push-back. `None` if the publisher has disconnected.
    last_publisher_tx: Option<mpsc::Sender<String>>,
    /// Monotonic per-topic publish counter (§ 3.11.1).
    next_seq: u64,
}

struct BrokerInner {
    subscriptions: HashMap<SubscriptionId, Subscription>,
    by_peer: HashMap<String, HashSet<SubscriptionId>>,
}

/// The shared subscription broker.
pub struct SubscriptionBroker {
    inner: RwLock<BrokerInner>,
    topics: RwLock<HashMap<String, TopicState>>,
}

impl Default for SubscriptionBroker {
    fn default() -> Self {
        Self::new()
    }
}

impl SubscriptionBroker {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(BrokerInner {
                subscriptions: HashMap::new(),
                by_peer: HashMap::new(),
            }),
            topics: RwLock::new(HashMap::new()),
        }
    }

    // ── Identity helpers ──

    fn topic_sub_id(peer: &str, topic: &str, filter: Option<&BodyFilter>) -> SubscriptionId {
        // Length-prefix every free-form segment (peer, topic, namespace).
        // Topic names AND peer ids are both free-form UTF-8 (`noded.register`
        // accepts the `from` value as-is, and topic names only forbid a
        // leading `$` — § 3.11.1). A pure-delimiter scheme would let a
        // crafted peer or topic value reconstruct another (peer, topic,
        // filter) tuple's id. Format:
        //   unfiltered: "<lp>:<peer>::topic::<lt>:<topic>"
        //   filtered:   "<lp>:<peer>::topic::<lt>:<topic>::ns::<ln>:<ns>"
        // The `<len>:` prefix forces equality to consume exactly `<len>`
        // bytes of free-form content per segment before the next
        // structural delimiter — collisions are impossible.
        match filter {
            None => format!("{}:{peer}::topic::{}:{topic}", peer.len(), topic.len(),),
            Some(f) => format!(
                "{}:{peer}::topic::{}:{topic}::ns::{}:{}",
                peer.len(),
                topic.len(),
                f.namespace.len(),
                f.namespace,
            ),
        }
    }

    /// Extract the top-level `namespace` field from a JSON body, if any.
    /// Used at publish time to index a record once for filtered fan-out
    /// rather than re-parsing per subscriber per delivery.
    fn extract_body_namespace(body: &str) -> Option<String> {
        let v: serde_json::Value = serde_json::from_str(body).ok()?;
        v.get("namespace")?.as_str().map(|s| s.to_string())
    }

    fn ui_event_sub_id(peer: &str, source: &str, action: Option<&str>) -> SubscriptionId {
        match action {
            Some(a) => format!("{peer}::ui::{source}::{a}"),
            None => format!("{peer}::ui::{source}::*"),
        }
    }

    // ── `topic.publish` ──

    /// Publish a snapshot to a topic. Returns `(seq, delivered_count, notifications)`
    /// on success. The caller is responsible for dispatching notifications (it
    /// has access to the peer registry; the broker does not).
    pub async fn publish(
        &self,
        name: &str,
        inner_body: &str,
        from: &str,
        from_tx: mpsc::Sender<String>,
        retain: bool,
    ) -> Result<(u64, usize, Vec<Notification>), PublishError> {
        self.publish_with_origin(name, inner_body, from, from_tx, BrokerOrigin::Local, retain)
            .await
    }

    /// Publish with the origin computed from the publisher's connection.
    /// The canonical stamped envelope is shared by live fan-out and retained
    /// replay, so replay preserves the publish-time origin.
    pub(crate) async fn publish_with_origin(
        &self,
        name: &str,
        inner_body: &str,
        from: &str,
        from_tx: mpsc::Sender<String>,
        origin: BrokerOrigin,
        retain: bool,
    ) -> Result<(u64, usize, Vec<Notification>), PublishError> {
        if name.starts_with('$') {
            return Err(PublishError::ReservedName);
        }
        if inner_body.len() > MAX_SNAPSHOT_BYTES {
            return Err(PublishError::PayloadTooLarge {
                size: inner_body.len(),
                limit: MAX_SNAPSHOT_BYTES,
            });
        }

        let mut inner = match bus::parse(inner_body) {
            Ok(m) => m,
            Err(_) => return Err(PublishError::MalformedPayload),
        };

        // Extract the body's namespace field once per publish for
        // filtered fan-out (§ SPEC 12 §15.5 — `<svc>.props.records.changed`
        // and `<svc>.props.audit` are per-service topics whose payloads
        // span every namespace; filtered subscribers receive only their
        // namespace). Generic topics with non-JSON bodies just get
        // `None` and skip filter matching — non-filtered subscribers
        // always receive them regardless.
        let body_namespace = Self::extract_body_namespace(&inner.body);

        // Allocate seq, inject reserved headers, optionally cache. All under
        // the topics write lock so publishes to the same topic serialize.
        let (seq, wire) = {
            let mut topics = self.topics.write().await;
            let state = topics
                .entry(name.to_string())
                .or_insert_with(|| TopicState {
                    snapshot: None,
                    last_publisher: None,
                    last_publisher_tx: None,
                    next_seq: 0,
                });
            state.next_seq += 1;
            let seq = state.next_seq;
            state.last_publisher = Some(from.to_string());
            state.last_publisher_tx = Some(from_tx.clone());

            // Security property (§ 3.11.2): broker always wins on routing headers.
            for h in RESERVED_HEADERS {
                inner.headers.remove(*h);
            }
            stamp_broker_origin(&mut inner, origin);
            inner.set("topic", name);
            inner.set("topic_seq", &seq.to_string());

            let wire = inner.to_wire();

            if retain {
                let size = wire.len();
                state.snapshot = Some(CachedSnapshot {
                    body: inner.clone(),
                    seq,
                    published_by: from.to_string(),
                    published_at: Instant::now(),
                    stale_since: None,
                    size,
                    body_namespace: body_namespace.clone(),
                });
            } else if let Some(snap) = state.snapshot.as_mut() {
                // retain=false doesn't update the cache, but if the live
                // publisher is the same as the cached one, clear any stale flag.
                if snap.published_by == from {
                    snap.stale_since = None;
                }
            }

            (seq, wire)
        };

        // Fan out to current subscribers. Track subscribers to prune on closed
        // channels. Topic.active transitions are handled only on subscribe
        // (not publish), so no notifications here.
        let mut delivered = 0usize;
        let mut to_prune: Vec<SubscriptionId> = Vec::new();
        {
            let inner_state = self.inner.read().await;
            for sub in inner_state.subscriptions.values() {
                if let SubKind::Topic { name: topic_name } = &sub.kind
                    && topic_name == name
                {
                    // Apply per-subscription filter. Mismatch is a
                    // silent skip (NOT a delivery failure) for the
                    // payload — but we still reap dead-tx subs here so
                    // a subscriber whose connection closed before any
                    // matching publish doesn't linger forever and block
                    // a later re-grant via idempotency. Closed-tx prune
                    // happens regardless of filter match; live txs on
                    // mismatch just skip silently.
                    if let Some(filter) = &sub.filter
                        && body_namespace.as_deref() != Some(filter.namespace.as_str())
                    {
                        if sub.tx.is_closed() {
                            to_prune.push(sub.id.clone());
                        }
                        continue;
                    }
                    match sub.tx.try_send(wire.clone()) {
                        Ok(()) => delivered += 1,
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            tracing::warn!(
                                peer = %sub.peer,
                                topic = %name,
                                "Topic delivery dropped: subscriber outbound full"
                            );
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            to_prune.push(sub.id.clone());
                        }
                    }
                }
            }
        }

        let notifications = if !to_prune.is_empty() {
            // Guarded prune (SPEC 12 C10b): a concurrent re-subscribe
            // could have replaced any pruned id with a fresh live tx
            // between our fan-out detecting Closed and this remove.
            // The guard skips those; `actually_pruned` tells us which
            // topics still had a dead-sub deletion succeed and should
            // be checked for the 1→0 idle notification.
            let actually_pruned = self.prune_dead_subscriptions(&to_prune).await;
            self.idle_notifications_for_pruned_topics(&actually_pruned)
                .await
        } else {
            Vec::new()
        };

        Ok((seq, delivered, notifications))
    }

    // ── `topic.subscribe` ──

    /// Subscribe a peer to a topic. Idempotent per § 3.11.3.
    /// Returns `(subscription_id, replayed, seq, notifications)`.
    ///
    /// Thin wrapper around [`Self::subscribe_topic_filtered`] with no
    /// filter — every publish to `name` is delivered to this subscriber.
    pub async fn subscribe_topic(
        &self,
        name: &str,
        peer: &str,
        tx: mpsc::Sender<String>,
    ) -> (SubscriptionId, bool, u64, Vec<Notification>) {
        self.subscribe_topic_filtered(name, peer, tx, None).await
    }

    /// Subscribe a peer to a topic with an optional [`BodyFilter`].
    ///
    /// When `filter` is `Some(BodyFilter{namespace})`, only publishes
    /// whose body JSON has a top-level `namespace` field equal to the
    /// filter value are delivered to this subscriber. A subscriber
    /// granted access to a single property namespace under a per-service
    /// reserved topic (SPEC 12 §15.5) uses this form to avoid receiving
    /// events from other namespaces owned by the same service.
    ///
    /// A `(peer, name)` pair can hold multiple distinct filtered
    /// subscriptions (one per namespace) — the broker keys them on
    /// `(peer, name, filter)`. `unsubscribe_topic(name, peer)` removes
    /// every filter variant for that pair in one call; see its docs.
    ///
    /// Cached snapshot replay is also filter-aware: a `retain: true`
    /// snapshot whose body namespace doesn't match the filter is not
    /// replayed to the new subscriber. The 0→1 `topic.active` notice is
    /// driven by overall subscriber count (across all filter variants),
    /// not per-filter count — publishers don't reason about filter scope.
    pub async fn subscribe_topic_filtered(
        &self,
        name: &str,
        peer: &str,
        tx: mpsc::Sender<String>,
        filter: Option<BodyFilter>,
    ) -> (SubscriptionId, bool, u64, Vec<Notification>) {
        let id = Self::topic_sub_id(peer, name, filter.as_ref());

        // Idempotency: if the peer is already subscribed with this exact
        // filter AND the existing tx is still live, return the existing
        // subscription with no replay and no notifications. Different
        // filters on the same (peer, name) produce different ids and
        // are independent subscriptions.
        //
        // Dead-tx race (SPEC 12 C10b MAJOR): if the original target
        // disconnected between `subscribe_grant`'s registry lookup and
        // here, or any time after a prior grant, the entry may still be
        // present (publish-side prune only fires on a delivery attempt,
        // which mismatched-namespace publishes skip — pruned alongside
        // matches now, but a topic with no traffic at all leaves the
        // dead sub in place). A bare idempotency return would hand the
        // caller back a stale tx and silently drop every future event.
        // Re-grant on the same `(peer, name, filter)` must therefore
        // fall through and re-insert with the fresh tx.
        {
            let inner = self.inner.read().await;
            if let Some(existing) = inner.subscriptions.get(&id)
                && !existing.tx.is_closed()
            {
                return (id, false, 0, Vec::new());
            }
        }

        // Compute pre-count, insert, compute post-count — all under one lock
        // so the 0→1 transition detection is atomic. Pre-count is across
        // every filter variant: publishers see "this topic has a
        // subscriber" regardless of filtering.
        let (pre_count, _post_count) = {
            let mut inner = self.inner.write().await;
            let pre = inner
                .subscriptions
                .values()
                .filter(|s| matches!(&s.kind, SubKind::Topic { name: n } if n == name))
                .count();
            inner.subscriptions.insert(
                id.clone(),
                Subscription {
                    id: id.clone(),
                    peer: peer.to_string(),
                    kind: SubKind::Topic {
                        name: name.to_string(),
                    },
                    tx: tx.clone(),
                    filter: filter.clone(),
                },
            );
            inner
                .by_peer
                .entry(peer.to_string())
                .or_default()
                .insert(id.clone());
            (pre, pre + 1)
        };

        // Replay cached snapshot (if any) and detect 0→1 transition for
        // topic.active notification to the last publisher. Filter check
        // gates only the replay — the active-transition notification
        // still fires because the topic now has at least one subscriber.
        let (replay, seq, active_notice) = {
            let topics = self.topics.read().await;
            if let Some(state) = topics.get(name) {
                let replay_wire = state.snapshot.as_ref().and_then(|snap| {
                    if let Some(f) = &filter
                        && snap.body_namespace.as_deref() != Some(f.namespace.as_str())
                    {
                        return None;
                    }
                    let mut msg = snap.body.clone();
                    if snap.stale_since.is_some() {
                        msg.set("topic_stale", "true");
                    }
                    Some(msg.to_wire())
                });
                let seq = state.snapshot.as_ref().map(|s| s.seq).unwrap_or(0);

                let notification = if pre_count == 0 {
                    state.last_publisher.as_ref().and_then(|pub_id| {
                        state.last_publisher_tx.as_ref().and_then(|pub_tx| {
                            if pub_tx.is_closed() {
                                None
                            } else {
                                let notice = build_topic_notice("topic.active", name, 1);
                                Some(Notification {
                                    target_peer: pub_id.clone(),
                                    target_tx: pub_tx.clone(),
                                    wire: notice.to_wire(),
                                })
                            }
                        })
                    })
                } else {
                    None
                };

                (replay_wire, seq, notification)
            } else {
                (None, 0, None)
            }
        };

        // Push the replay to the new subscriber. If the channel was
        // closed at this point (target peer disconnected after the
        // verb-level close-check, before the snapshot try_send) the
        // sub is dead-on-arrival: the publish hot loop only prunes
        // *during* a matching publish, so on a low-traffic or
        // never-matching topic the dead sub would persist
        // indefinitely. Evict here and suppress the `topic.active`
        // notification — the publisher must not learn of a
        // "subscriber" that will never receive an idle.
        //
        // The no-replay case (state.snapshot is None) is intentionally
        // NOT pre-empted by an `is_closed()` check: existing tests
        // model "dead from birth" subs via `let (tx, _) = channel()`
        // and rely on durability until the janitor sweeps. The janitor
        // tick is the catch-all for that path; this only handles the
        // narrow race where replay is the very first delivery attempt
        // and tells us authoritatively that the channel is dead.
        if let Some(wire) = &replay
            && matches!(
                tx.try_send(wire.clone()),
                Err(mpsc::error::TrySendError::Closed(_))
            )
        {
            // Guarded prune: a concurrent re-subscribe could have
            // replaced our entry with a fresh live tx between our
            // insert (above) and this `try_send` failure (the
            // intervening write-lock release allows it). The guarded
            // variant skips the remove if the entry's tx is no longer
            // closed.
            self.prune_dead_subscriptions(std::slice::from_ref(&id))
                .await;
            // After the prune attempt the entry may be in one of three
            // states (rev 7 — Codex MAJOR):
            //   (a) absent: prune removed it (genuinely dead), OR a
            //       concurrent unsubscribe/remove_peer removed it.
            //       In both sub-cases there is no live subscriber, so
            //       any topic.active we computed must be suppressed —
            //       the unsubscribe/remove path is responsible for the
            //       paired topic.idle, and emitting active here would
            //       produce an unpaired or out-of-order transition.
            //   (b) present, tx live: guarded skip — a concurrent
            //       re-grant for the same (peer, topic, filter) won
            //       the race and the topic genuinely has a live
            //       subscriber. The active_notice we computed under
            //       pre_count==0 still correctly describes the
            //       transition, so pass it through.
            //   (c) present, tx closed: race lost a second time — the
            //       entry has been replaced by yet another dead tx
            //       (extremely unlikely; janitor will sweep). Treat
            //       like (a): suppress active to avoid an unpaired
            //       transition. The janitor's eventual sweep will emit
            //       the right idle.
            let entry_live = {
                let inner = self.inner.read().await;
                inner
                    .subscriptions
                    .get(&id)
                    .is_some_and(|s| !s.tx.is_closed())
            };
            let notifications = if entry_live {
                active_notice.map(|n| vec![n]).unwrap_or_default()
            } else {
                Vec::new()
            };
            return (id, false, 0, notifications);
        }

        let notifications = active_notice.map(|n| vec![n]).unwrap_or_default();
        (id, replay.is_some(), seq, notifications)
    }

    // ── `topic.unsubscribe` ──

    /// Unsubscribe a peer from a topic. Idempotent.
    /// Returns notifications (potentially a `topic.idle` if count hit 0).
    ///
    /// **Filter-aware removal:** if the peer holds multiple filtered
    /// subscriptions to `name` (one per namespace, registered via
    /// [`Self::subscribe_topic_filtered`]), this call removes *all* of
    /// them. A subscriber that wants per-filter removal must
    /// re-subscribe to the filters it still wants after the
    /// unsubscribe. The rationale is that the v1 wire surface
    /// (`topic.unsubscribe { topic }`) takes no filter argument, and a
    /// silent partial unsubscribe (where some filters survive) would
    /// surprise callers far more than a clean "you are now off this
    /// topic" semantics.
    pub async fn unsubscribe_topic(&self, name: &str, peer: &str) -> Vec<Notification> {
        // Scan + remove under a single write lock. Holding the lock
        // across both phases closes the race where a concurrent
        // `subscribe_topic_filtered` for the same (peer, topic) could
        // insert a new filter variant between a read-only scan and a
        // subsequent write — the new variant would survive the
        // "remove all" contract.
        let (removed_any, post_count_zero) = {
            let mut inner = self.inner.write().await;
            let to_remove: Vec<SubscriptionId> = inner
                .by_peer
                .get(peer)
                .map(|ids| {
                    ids.iter()
                        .filter_map(|id| {
                            let sub = inner.subscriptions.get(id)?;
                            match &sub.kind {
                                SubKind::Topic { name: n } if n == name => Some(id.clone()),
                                _ => None,
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();

            if to_remove.is_empty() {
                return Vec::new();
            }

            for id in &to_remove {
                inner.subscriptions.remove(id);
            }
            if let Some(peer_subs) = inner.by_peer.get_mut(peer) {
                for id in &to_remove {
                    peer_subs.remove(id);
                }
                if peer_subs.is_empty() {
                    inner.by_peer.remove(peer);
                }
            }
            let zero = inner
                .subscriptions
                .values()
                .filter(|s| matches!(&s.kind, SubKind::Topic { name: n } if n == name))
                .count()
                == 0;
            (true, zero)
        };
        let _ = removed_any;

        if post_count_zero {
            let mut topics = self.topics.write().await;
            let mut notifications = Vec::new();
            if let Some(state) = topics.get(name)
                && let (Some(pub_id), Some(pub_tx)) =
                    (&state.last_publisher, &state.last_publisher_tx)
                && !pub_tx.is_closed()
            {
                let notice = build_topic_notice("topic.idle", name, 0);
                notifications.push(Notification {
                    target_peer: pub_id.clone(),
                    target_tx: pub_tx.clone(),
                    wire: notice.to_wire(),
                });
            }
            if let Some(state) = topics.get(name)
                && state.snapshot.is_none()
            {
                topics.remove(name);
                tracing::info!(
                    topic = %name,
                    "Removing dead TopicState (no snapshot, unsubscribe→0)"
                );
            }
            return notifications;
        }

        Vec::new()
    }

    // ── `topic.subscriber_count` ──

    pub async fn subscriber_count(&self, name: &str) -> usize {
        let inner = self.inner.read().await;
        inner
            .subscriptions
            .values()
            .filter(|s| matches!(&s.kind, SubKind::Topic { name: n } if n == name))
            .count()
    }

    // ── `topic.list` ──

    /// Aggregate stats for SPEC 07 noded.props.* surface: (active topic
    /// count, total retained snapshot bytes). Active = has subscribers
    /// or has retained snapshot.
    pub async fn props_summary(&self) -> (u64, u64) {
        let topics = self.topics.read().await;
        let inner = self.inner.read().await;
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for sub in inner.subscriptions.values() {
            if let SubKind::Topic { name } = &sub.kind {
                *counts.entry(name.as_str()).or_insert(0) += 1;
            }
        }
        let mut active: u64 = 0;
        let mut bytes: u64 = 0;
        for (name, state) in topics.iter() {
            let subs = counts.get(name.as_str()).copied().unwrap_or(0);
            let has_snap = state.snapshot.is_some();
            if subs > 0 || has_snap {
                active += 1;
            }
            if let Some(snap) = &state.snapshot {
                bytes += snap.size as u64;
            }
        }
        for name in counts.keys() {
            if !topics.contains_key(*name) {
                active += 1;
            }
        }
        (active, bytes)
    }

    pub async fn list(&self, prefix: Option<&str>) -> Vec<TopicInfo> {
        let topics = self.topics.read().await;
        let inner = self.inner.read().await;

        let mut counts: HashMap<&str, usize> = HashMap::new();
        for sub in inner.subscriptions.values() {
            if let SubKind::Topic { name } = &sub.kind {
                *counts.entry(name.as_str()).or_insert(0) += 1;
            }
        }

        let mut out = Vec::new();
        for (name, state) in topics.iter() {
            if let Some(p) = prefix
                && !name.starts_with(p)
            {
                continue;
            }
            let subscribers = counts.get(name.as_str()).copied().unwrap_or(0);
            let (has_snapshot, snapshot_seq, snapshot_size, stale) = match &state.snapshot {
                Some(snap) => (true, snap.seq, snap.size, snap.stale_since.is_some()),
                None => (false, 0, 0, false),
            };
            out.push(TopicInfo {
                name: name.clone(),
                subscribers,
                has_snapshot,
                snapshot_seq,
                snapshot_size,
                last_publisher: state.last_publisher.clone(),
                stale,
            });
        }

        // Also include subscribed topics that have no snapshot yet (edge case:
        // subscribe happened before any publish).
        for (name, count) in counts {
            if !out.iter().any(|t| t.name == name) {
                if let Some(p) = prefix
                    && !name.starts_with(p)
                {
                    continue;
                }
                out.push(TopicInfo {
                    name: name.to_string(),
                    subscribers: count,
                    has_snapshot: false,
                    snapshot_seq: 0,
                    snapshot_size: 0,
                    last_publisher: None,
                    stale: false,
                });
            }
        }

        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    // ── `topic.clear` ──

    /// Clear a topic's cached snapshot. If `notify` is true and there are
    /// subscribers, fan out a final delivery with `topic_op: delete`.
    ///
    /// Filter-aware: a filtered subscriber whose `BodyFilter` namespace
    /// does not match the cleared snapshot's namespace is skipped — it
    /// never saw the snapshot on replay (see
    /// [`Self::subscribe_topic_filtered`]) so it must not receive a
    /// dangling delete notice for content it cannot observe. An
    /// unfiltered subscriber, and a filtered one whose filter matches
    /// the cleared snapshot's namespace, both receive the delete.
    pub async fn clear(&self, name: &str, notify: bool) -> (usize, Vec<Notification>) {
        let prepared: Option<(String, Option<String>)> = {
            let mut topics = self.topics.write().await;
            let Some(state) = topics.get_mut(name) else {
                return (0, Vec::new());
            };
            // Capture the cleared snapshot's namespace BEFORE dropping
            // the snapshot, so the delete fan-out below can match it
            // against subscriber filters.
            let cleared_namespace = state
                .snapshot
                .as_ref()
                .and_then(|s| s.body_namespace.clone());
            let had_snapshot = state.snapshot.is_some();
            state.snapshot = None;
            if !notify || !had_snapshot {
                return (0, Vec::new());
            }
            let mut msg = BusMessage::new();
            msg.set("command", "ui.noop"); // placeholder inner command
            msg.set("topic", name);
            msg.set("topic_op", "delete");
            msg.set("topic_seq", &state.next_seq.to_string());
            Some((msg.to_wire(), cleared_namespace))
        };

        let Some((wire, cleared_namespace)) = prepared else {
            return (0, Vec::new());
        };

        let mut delivered = 0usize;
        let mut to_prune: Vec<SubscriptionId> = Vec::new();
        {
            let inner = self.inner.read().await;
            for sub in inner.subscriptions.values() {
                if let SubKind::Topic { name: topic_name } = &sub.kind
                    && topic_name == name
                {
                    if let Some(filter) = &sub.filter
                        && cleared_namespace.as_deref() != Some(filter.namespace.as_str())
                    {
                        // Filter didn't match the cleared snapshot's
                        // namespace; subscriber never saw the snapshot,
                        // skip the delete notice. Still reap dead-tx
                        // subs here so a filtered subscriber whose
                        // connection silently closed doesn't survive
                        // a clear-with-mismatched-namespace (SPEC 12
                        // C10b — same gap as the publish hot loop).
                        if sub.tx.is_closed() {
                            to_prune.push(sub.id.clone());
                        }
                        continue;
                    }
                    match sub.tx.try_send(wire.clone()) {
                        Ok(()) => delivered += 1,
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            tracing::warn!(peer = %sub.peer, topic = %name, "topic.clear delete delivery dropped: full");
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            to_prune.push(sub.id.clone());
                        }
                    }
                }
            }
        }
        let notifications = if !to_prune.is_empty() {
            // Guarded prune (SPEC 12 C10b) — see `publish()` for the
            // concurrent re-subscribe race rationale.
            let actually_pruned = self.prune_dead_subscriptions(&to_prune).await;
            self.idle_notifications_for_pruned_topics(&actually_pruned)
                .await
        } else {
            Vec::new()
        };

        (delivered, notifications)
    }

    // ── UI event stubs (§ 6 Phase A) ──

    /// Stub for `ui.subscribe`. Registers the subscription in the shared
    /// registry but emits no deliveries — event routing is not wired in v1.
    /// Callers should return RC 5 to signal "registered but not yet functional."
    pub async fn subscribe_ui_event(
        &self,
        peer: &str,
        source: &str,
        action: Option<&str>,
        tx: mpsc::Sender<String>,
    ) -> SubscriptionId {
        let id = Self::ui_event_sub_id(peer, source, action);
        let mut inner = self.inner.write().await;
        inner.subscriptions.insert(
            id.clone(),
            Subscription {
                id: id.clone(),
                peer: peer.to_string(),
                kind: SubKind::UiEvent {
                    source: source.to_string(),
                    action: action.map(str::to_string),
                },
                tx,
                filter: None,
            },
        );
        inner
            .by_peer
            .entry(peer.to_string())
            .or_default()
            .insert(id.clone());
        id
    }

    pub async fn unsubscribe_ui_event(&self, peer: &str, source: &str, action: Option<&str>) {
        let id = Self::ui_event_sub_id(peer, source, action);
        let mut inner = self.inner.write().await;
        if inner.subscriptions.remove(&id).is_some()
            && let Some(peer_subs) = inner.by_peer.get_mut(peer)
        {
            peer_subs.remove(&id);
            if peer_subs.is_empty() {
                inner.by_peer.remove(peer);
            }
        }
    }

    // ── Peer lifecycle ──

    /// Remove the subscriptions a *specific connection* registered under
    /// `peer`, and mark stale any topics that same connection last
    /// published. Returns the `topic.idle` notifications that fire as its
    /// subscriptions go away.
    ///
    /// `owner` is the connection's outbound channel. Teardown is
    /// **channel-scoped**: only subscriptions whose `tx.same_channel(owner)`
    /// — and a `last_publisher_tx` on the same channel — are touched. The
    /// `peer` string alone is *not* a sufficient key. A peer name can be
    /// re-registered by a different connection (after `route_local` prunes a
    /// half-dead tx's stale registry entry, or a citizen restarts under
    /// systemd) while the old socket is still readable enough to drive a
    /// disconnect or `noded.deregister`. A string-keyed wipe would then tear
    /// down the *new* owner's topic/subscription state — the SPEC 12 §15.5
    /// impersonation window, the same class the registry's `same_channel`
    /// guard already closes for the request registry. Scoping by the owning
    /// channel makes both the WS-close path and `noded.deregister`
    /// authoritative for *their* connection only and a no-op for any name a
    /// newer connection now owns.
    pub async fn remove_peer(&self, peer: &str, owner: &mpsc::Sender<String>) -> Vec<Notification> {
        // Step 1: remove only THIS connection's subscriptions under `peer`;
        // leave any a newer connection registered under the same name. Track
        // affected topics for the 1→0 idle tally in step 3.
        let affected_topics: HashSet<String> = {
            let mut inner = self.inner.write().await;
            // Snapshot the id set so the by_peer borrow ends before we
            // mutate subscriptions / by_peer below.
            let ids: Vec<SubscriptionId> = inner
                .by_peer
                .get(peer)
                .map(|s| s.iter().cloned().collect())
                .unwrap_or_default();
            let mut topics = HashSet::new();
            let mut retained: HashSet<SubscriptionId> = HashSet::new();
            for id in ids {
                let ours = inner
                    .subscriptions
                    .get(&id)
                    .map(|s| s.tx.same_channel(owner))
                    .unwrap_or(false);
                if ours {
                    if let Some(sub) = inner.subscriptions.remove(&id)
                        && let SubKind::Topic { name } = sub.kind
                    {
                        topics.insert(name);
                    }
                } else {
                    // Belongs to a different connection that re-registered
                    // this name — must survive our teardown.
                    retained.insert(id);
                }
            }
            if retained.is_empty() {
                inner.by_peer.remove(peer);
            } else {
                inner.by_peer.insert(peer.to_string(), retained);
            }
            topics
        };

        // Step 2: mark snapshots stale only where THIS connection was the
        // last publisher. A `last_publisher` string match with a different
        // `last_publisher_tx` channel means a newer connection re-published
        // under the same name — leaving its snapshot/tx intact is required,
        // not optional. (`last_publisher_tx == None` ⇒ that publisher was
        // already torn down and stale-marked, so skipping is a no-op.)
        let now = Instant::now();
        {
            let mut topics = self.topics.write().await;
            for state in topics.values_mut() {
                if state.last_publisher.as_deref() == Some(peer)
                    && state
                        .last_publisher_tx
                        .as_ref()
                        .map(|t| t.same_channel(owner))
                        .unwrap_or(false)
                {
                    if let Some(snap) = state.snapshot.as_mut()
                        && snap.stale_since.is_none()
                    {
                        snap.stale_since = Some(now);
                    }
                    // Drop the publisher tx — it's dead, so is_closed() would
                    // be true anyway, but nil-ing avoids confusion elsewhere.
                    state.last_publisher_tx = None;
                }
            }
        }

        // Step 3: for each affected topic whose subscriber count went to 0,
        // fire topic.idle to the last publisher (if still alive), and drop
        // the TopicState entry if it has no snapshot left to keep alive.
        let mut notifications = Vec::new();
        {
            let inner = self.inner.read().await;
            let mut topics = self.topics.write().await;
            let mut dead: Vec<String> = Vec::new();
            for topic_name in &affected_topics {
                let count = inner
                    .subscriptions
                    .values()
                    .filter(|s| matches!(&s.kind, SubKind::Topic { name: n } if n == topic_name))
                    .count();
                if count == 0
                    && let Some(state) = topics.get(topic_name)
                {
                    if let (Some(pub_id), Some(pub_tx)) =
                        (&state.last_publisher, &state.last_publisher_tx)
                        && !pub_tx.is_closed()
                    {
                        let notice = build_topic_notice("topic.idle", topic_name, 0);
                        notifications.push(Notification {
                            target_peer: pub_id.clone(),
                            target_tx: pub_tx.clone(),
                            wire: notice.to_wire(),
                        });
                    }
                    if state.snapshot.is_none() {
                        dead.push(topic_name.clone());
                    }
                }
            }
            for name in &dead {
                topics.remove(name);
                tracing::info!(
                    topic = %name,
                    "Removing dead TopicState (no snapshot, peer disconnect→0)"
                );
            }
        }

        notifications
    }

    // ── Janitor (stale snapshot purge) ──

    /// Walk topics and purge snapshots whose stale_since exceeded the TTL.
    /// After purge, drops any TopicState that has no snapshot and no subscribers
    /// — preserving the seq counter or `last_publisher` across purge+republish
    /// would mislead future subscribers (they couldn't distinguish "missed N
    /// messages" from "topic was purged and re-created"). Matches MQTT's
    /// retained-message-expiry semantics: purge means the topic no longer exists.
    pub async fn janitor_tick(&self) -> Vec<Notification> {
        let now = Instant::now();
        let mut notifications: Vec<Notification> = Vec::new();

        // SPEC 12 C10b — sweep dead-tx subscriptions first. The
        // publish hot-loop prunes on `try_send(...Closed)`, but a sub
        // on a low-traffic topic (or one whose every publish fails the
        // namespace filter then-mismatch-prune runs only when
        // mismatched publishes occur) can otherwise persist
        // indefinitely. The janitor is the catch-all that guarantees
        // dead subs eventually leave the registry even if no publish
        // arrives. Done before the counts tally so dead subs don't
        // inflate the per-topic count and keep a dead TopicState
        // alive.
        //
        // When pruning drives a topic from N>0 to 0 subscribers, emit
        // the matching `topic.idle` so the publisher sees a clean
        // 0→1→0 cycle paired with the original `topic.active`. Without
        // this, a publisher that received `topic.active` and then had
        // its only subscriber die silently would be stuck in "active"
        // forever (the 1→0 transition is otherwise only fired by
        // explicit unsubscribe / peer disconnect / clear-with-notify).
        let dead_ids: Vec<SubscriptionId> = {
            let inner = self.inner.read().await;
            inner
                .subscriptions
                .iter()
                .filter_map(|(id, sub)| {
                    if sub.tx.is_closed() {
                        Some(id.clone())
                    } else {
                        None
                    }
                })
                .collect()
        };
        if !dead_ids.is_empty() {
            tracing::info!(
                count = dead_ids.len(),
                "Janitor sweeping dead-tx subscriptions"
            );
            // Guarded prune: ids collected under inner.read are
            // re-validated as still-closed under inner.write to avoid
            // a concurrent re-grant having replaced them with a fresh
            // live tx (SPEC 12 C10b race).
            let actually_pruned = self.prune_dead_subscriptions(&dead_ids).await;
            notifications.extend(
                self.idle_notifications_for_pruned_topics(&actually_pruned)
                    .await,
            );
        }

        // Acquire inner.read first (matches the inner-before-topics order used
        // by remove_peer) to tally subscriber counts per topic.
        let counts: HashMap<String, usize> = {
            let inner = self.inner.read().await;
            let mut c: HashMap<String, usize> = HashMap::new();
            for sub in inner.subscriptions.values() {
                if let SubKind::Topic { name } = &sub.kind {
                    *c.entry(name.clone()).or_insert(0) += 1;
                }
            }
            c
        };
        let mut topics = self.topics.write().await;
        topics.retain(|name, state| {
            if let Some(snap) = &state.snapshot
                && let Some(since) = snap.stale_since
                && now.duration_since(since) > STALE_SNAPSHOT_TTL
            {
                state.snapshot = None;
                tracing::info!(
                    topic = %name,
                    "Purged stale topic snapshot past orphan timeout"
                );
            }
            let subs = counts.get(name).copied().unwrap_or(0);
            if state.snapshot.is_none() && subs == 0 {
                tracing::info!(
                    topic = %name,
                    "Removing dead TopicState (no snapshot, no subscribers)"
                );
                return false;
            }
            true
        });

        notifications
    }

    // ── Internals ──

    /// SPEC 12 C10b — guarded variant for dead-tx pruning. Only
    /// removes the entry if its current `tx` is still closed under
    /// the write lock. A concurrent re-subscribe between the prune
    /// caller's `is_closed()` check and this write-lock acquisition
    /// would have replaced the dead entry with a fresh live tx; this
    /// guard avoids deleting that live subscription.
    ///
    /// Returns the set of topic names whose subscriptions were
    /// actually removed (some ids may have been re-grant-replaced and
    /// thus skipped), so callers can compute the correct post-prune
    /// idle notifications.
    async fn prune_dead_subscriptions(&self, ids: &[SubscriptionId]) -> HashSet<String> {
        let mut inner = self.inner.write().await;
        let mut affected_topics: HashSet<String> = HashSet::new();
        for id in ids {
            // Verify the entry is still dead before removing. A live
            // re-grant would have refreshed the tx between collection
            // and now — those entries are skipped to preserve the
            // freshly-bound subscriber.
            let still_dead = inner
                .subscriptions
                .get(id)
                .is_some_and(|s| s.tx.is_closed());
            if !still_dead {
                continue;
            }
            if let Some(sub) = inner.subscriptions.remove(id) {
                if let SubKind::Topic { name } = &sub.kind {
                    affected_topics.insert(name.clone());
                }
                if let Some(peer_subs) = inner.by_peer.get_mut(&sub.peer) {
                    peer_subs.remove(id);
                    if peer_subs.is_empty() {
                        inner.by_peer.remove(&sub.peer);
                    }
                }
            }
        }
        affected_topics
    }

    /// SPEC 12 C10b — for each topic in `topic_names`, if the post-prune
    /// subscriber count is 0 and the last publisher's tx is still alive,
    /// build a `topic.idle` notification. This is the shared tail of
    /// every dead-tx prune path (publish hot loop, clear, janitor) so
    /// callers that already saw `topic.active` see a paired `topic.idle`
    /// regardless of which path reaped the dead sub.
    ///
    /// Caller must have already removed the dead subscriptions before
    /// invoking this — counts are recomputed against the post-prune
    /// `inner` state. Lock order matches `remove_peer`:
    /// `inner.read` → `topics.read`.
    async fn idle_notifications_for_pruned_topics(
        &self,
        topic_names: &HashSet<String>,
    ) -> Vec<Notification> {
        if topic_names.is_empty() {
            return Vec::new();
        }
        let inner = self.inner.read().await;
        let topics = self.topics.read().await;
        let mut out = Vec::new();
        for topic_name in topic_names {
            let live_count = inner
                .subscriptions
                .values()
                .filter(|s| matches!(&s.kind, SubKind::Topic { name: n } if n == topic_name))
                .count();
            if live_count == 0
                && let Some(state) = topics.get(topic_name)
                && let (Some(pub_id), Some(pub_tx)) =
                    (&state.last_publisher, &state.last_publisher_tx)
                && !pub_tx.is_closed()
            {
                let notice = build_topic_notice("topic.idle", topic_name, 0);
                out.push(Notification {
                    target_peer: pub_id.clone(),
                    target_tx: pub_tx.clone(),
                    wire: notice.to_wire(),
                });
            }
        }
        out
    }
}

// ── Helpers ──

fn build_topic_notice(command: &str, name: &str, subscribers: usize) -> BusMessage {
    let mut msg = BusMessage::new();
    msg.set("command", command);
    msg.set("from", "noded");
    msg.set("name", name);
    msg.set("subscribers", &subscribers.to_string());
    msg
}

/// Synthesize a connection-scoped anonymous identity per § 3.11.1.
/// Format: `anon-<hex_nonce>-<unix_seconds>`.
pub fn synth_anon_id() -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let nanos = now.subsec_nanos();
    let secs = now.as_secs();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    // Mix time + counter to reduce collision across concurrent connects.
    let nonce = nanos.wrapping_add(counter).wrapping_mul(2654435761);
    format!("anon-{:08x}-{}", nonce, secs)
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_inner_body(target: &str, count: u32) -> String {
        format!("---\ncommand: ui.batch\n---\n[{{\"target\":\"{target}\",\"count\":{count}}}]\n")
    }

    async fn drain(rx: &mut mpsc::Receiver<String>, n: usize) -> Vec<String> {
        let mut out = Vec::new();
        for _ in 0..n {
            match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
                Ok(Some(m)) => out.push(m),
                _ => break,
            }
        }
        out
    }

    #[tokio::test]
    async fn publish_then_subscribe_replays_snapshot() {
        let broker = SubscriptionBroker::new();
        let (pub_tx, _pub_rx) = mpsc::channel::<String>(16);
        let (sub_tx, mut sub_rx) = mpsc::channel::<String>(16);

        let body = sample_inner_body("sysmon", 1);
        let (seq, delivered, notes) = broker
            .publish("sysmon.metrics", &body, "producer", pub_tx.clone(), true)
            .await
            .unwrap();
        assert_eq!(seq, 1);
        assert_eq!(delivered, 0);
        assert!(notes.is_empty());

        let (id, replayed, replay_seq, notes) = broker
            .subscribe_topic("sysmon.metrics", "viewer", sub_tx)
            .await;
        assert!(id.contains("viewer"));
        assert!(replayed);
        assert_eq!(replay_seq, 1);
        // 0→1 transition should have emitted topic.active to the publisher.
        assert_eq!(notes.len(), 1);
        assert!(notes[0].wire.contains("topic.active"));
        assert_eq!(notes[0].target_peer, "producer");

        let msgs = drain(&mut sub_rx, 1).await;
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].contains("topic: sysmon.metrics"));
        assert!(msgs[0].contains("topic_seq: 1"));
    }

    #[tokio::test]
    async fn subscribe_then_publish_delivers() {
        let broker = SubscriptionBroker::new();
        let (pub_tx, _pub_rx) = mpsc::channel::<String>(16);
        let (sub_tx, mut sub_rx) = mpsc::channel::<String>(16);

        broker
            .subscribe_topic("sysmon.metrics", "viewer", sub_tx)
            .await;

        let body = sample_inner_body("sysmon", 5);
        let (_seq, delivered, _notes) = broker
            .publish("sysmon.metrics", &body, "producer", pub_tx, true)
            .await
            .unwrap();
        assert_eq!(delivered, 1);

        let msgs = drain(&mut sub_rx, 1).await;
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].contains("topic: sysmon.metrics"));
    }

    #[tokio::test]
    async fn subscribe_is_idempotent() {
        let broker = SubscriptionBroker::new();
        let (pub_tx, _pub_rx) = mpsc::channel::<String>(16);
        let (sub_tx, mut sub_rx) = mpsc::channel::<String>(16);

        broker
            .publish("t", &sample_inner_body("x", 1), "p", pub_tx.clone(), true)
            .await
            .unwrap();

        let (id1, replayed1, _, _) = broker.subscribe_topic("t", "v", sub_tx.clone()).await;
        let (id2, replayed2, _, notes2) = broker.subscribe_topic("t", "v", sub_tx.clone()).await;
        assert_eq!(id1, id2);
        assert!(replayed1);
        assert!(!replayed2, "second subscribe should not replay");
        assert!(
            notes2.is_empty(),
            "second subscribe should not emit topic.active"
        );

        // Only one replay delivered despite two subscribe calls.
        let msgs = drain(&mut sub_rx, 2).await;
        assert_eq!(msgs.len(), 1);

        assert_eq!(broker.subscriber_count("t").await, 1);
    }

    /// SPEC 12 C10b MAJOR regression (rev 4) — the guarded prune helper
    /// must NOT remove an entry whose tx has been refreshed to live
    /// between collection and removal. Simulates the race: dead-tx sub
    /// is inserted, then re-granted with a fresh tx (entry replaced),
    /// then a delayed prune call arrives with the original (now-stale)
    /// id list. Without the guard the fresh live subscription would be
    /// deleted out from under the legitimate subscriber.
    #[tokio::test]
    async fn prune_dead_subscriptions_skips_resubscribed_entries() {
        let broker = SubscriptionBroker::new();

        // Step 1: insert with dead tx.
        let (sub_tx_dead, sub_rx_dead) = mpsc::channel::<String>(16);
        let (id1, _, _, _) = broker.subscribe_topic("t", "v", sub_tx_dead).await;
        drop(sub_rx_dead);

        // Step 2: re-grant with live tx. The dead-tx-replace branch in
        // `subscribe_topic_filtered` overwrites the entry under the
        // same id.
        let (sub_tx_live, _sub_rx_live) = mpsc::channel::<String>(16);
        let (id2, _, _, _) = broker.subscribe_topic("t", "v", sub_tx_live).await;
        assert_eq!(id1, id2);

        // Step 3: simulate a delayed prune that thought the entry was
        // still dead. With the guard, the entry survives.
        let pruned = broker
            .prune_dead_subscriptions(std::slice::from_ref(&id1))
            .await;
        assert!(
            pruned.is_empty(),
            "guarded prune must not delete a re-granted live entry"
        );
        assert_eq!(broker.subscriber_count("t").await, 1);
    }

    /// SPEC 12 C10b MAJOR regression — re-subscribing to a
    /// `(peer, topic, filter)` whose previous tx is closed must NOT
    /// short-circuit on idempotency. The dead entry is replaced with
    /// the fresh tx so the second subscriber actually receives events.
    #[tokio::test]
    async fn resubscribe_replaces_closed_tx() {
        let broker = SubscriptionBroker::new();
        let (pub_tx, _pub_rx) = mpsc::channel::<String>(16);

        // First subscriber, then drop its receiver to close the tx.
        let (sub_tx_dead, sub_rx_dead) = mpsc::channel::<String>(16);
        let (id1, _, _, _) = broker.subscribe_topic("t", "v", sub_tx_dead).await;
        drop(sub_rx_dead);

        // Re-subscribe with a fresh tx. Same id (same peer+topic+filter)
        // — the dead entry must be replaced, not idempotent-returned.
        let (sub_tx_live, mut sub_rx_live) = mpsc::channel::<String>(16);
        let (id2, _, _, _) = broker.subscribe_topic("t", "v", sub_tx_live).await;
        assert_eq!(id1, id2, "same key, same id");

        // Publish: the fresh subscriber must receive it. Pre-fix this
        // would deliver to the dead tx (Closed → prune) and the new
        // receiver would see nothing.
        let body = sample_inner_body("x", 1);
        let (_seq, delivered, _) = broker
            .publish("t", &body, "p", pub_tx, false)
            .await
            .unwrap();
        assert_eq!(delivered, 1, "fresh tx must receive the publish");
        let msgs = drain(&mut sub_rx_live, 1).await;
        assert_eq!(msgs.len(), 1);
    }

    /// SPEC 12 C10b MAJOR regression (rev 5) — when the replay-Closed
    /// branch fires on a genuinely-dead entry (no concurrent re-grant
    /// raced in), the guarded prune deletes the entry and the
    /// `topic.active` notification MUST be suppressed: there is no real
    /// subscriber to announce, and emitting an active without a paired
    /// idle would corrupt the publisher's transition accounting.
    #[tokio::test]
    async fn replay_closed_genuine_dead_tx_suppresses_active_notice() {
        let broker = SubscriptionBroker::new();
        let (pub_tx, mut pub_rx) = mpsc::channel::<String>(16);

        // Snapshot must exist so the replay try_send is attempted at all.
        broker
            .publish("t", &sample_inner_body("x", 1), "p", pub_tx.clone(), true)
            .await
            .unwrap();
        // Drop the publisher rx — but the broker holds its own clone via
        // last_publisher_tx; that's fine, the test asserts on returned
        // notifications, not on what reaches pub_rx.
        let _ = pub_rx.try_recv();

        // Subscribe with a closed-from-start tx so the replay try_send
        // hits Closed immediately and prune runs.
        let (sub_tx_dead, sub_rx_dead) = mpsc::channel::<String>(16);
        drop(sub_rx_dead);
        let (id, replayed, _seq, notes) = broker.subscribe_topic("t", "v", sub_tx_dead).await;

        assert!(!replayed, "replay try_send failed Closed");
        assert!(
            notes.is_empty(),
            "active_notice MUST be suppressed when entry was genuinely dead and pruned",
        );
        assert_eq!(
            broker.subscriber_count("t").await,
            0,
            "guarded prune deleted the dead entry",
        );
        // And the entry truly is gone from inner.subscriptions.
        let inner = broker.inner.read().await;
        assert!(!inner.subscriptions.contains_key(&id));
    }

    /// SPEC 12 C10b MAJOR regression (rev 6/7) — the post-prune
    /// classification in `subscribe_topic_filtered`'s replay-Closed
    /// branch must distinguish three cases:
    ///   (a) entry absent — prune removed it OR concurrent unsubscribe
    ///       did. Suppress active_notice in both sub-cases (the remover
    ///       path emits the paired idle).
    ///   (b) entry present, tx live — concurrent re-grant won; emit
    ///       the preserved active_notice so the publisher learns of
    ///       the live subscriber.
    ///   (c) entry present, tx closed — pathological double-race;
    ///       suppress active and let the janitor sweep.
    ///
    /// This test exercises case (b)'s post-state directly: it sets up
    /// a topic with last_publisher state, an entry whose tx is live,
    /// and asserts that `prune_dead_subscriptions` skips while the
    /// inner-read classification reports `entry_live = true`. The
    /// `subscribe_topic_filtered` fix keys its active_notice emission
    /// off `entry_live`, not off `pruned.is_empty()` (which conflates
    /// case (a) and (b)).
    #[tokio::test]
    async fn replay_closed_guard_skip_post_state_classification() {
        let broker = SubscriptionBroker::new();
        let (pub_tx, _pub_rx) = mpsc::channel::<String>(16);
        broker
            .publish("t", &sample_inner_body("x", 1), "p", pub_tx, true)
            .await
            .unwrap();

        // Case (b): entry present with live tx.
        let (sub_tx_live, _sub_rx_live) = mpsc::channel::<String>(16);
        let (id, _, _, _) = broker.subscribe_topic("t", "v", sub_tx_live).await;

        let pruned = broker
            .prune_dead_subscriptions(std::slice::from_ref(&id))
            .await;
        assert!(pruned.is_empty(), "guarded prune skips on live tx");

        let entry_live = {
            let inner = broker.inner.read().await;
            inner
                .subscriptions
                .get(&id)
                .is_some_and(|s| !s.tx.is_closed())
        };
        assert!(
            entry_live,
            "case (b): live entry must classify as live — active_notice will be emitted"
        );
        assert_eq!(broker.subscriber_count("t").await, 1);
    }

    /// SPEC 12 C10b MAJOR regression (rev 7) — case (a) of the
    /// replay-Closed post-prune classification: if the entry is
    /// already absent (e.g. concurrent unsubscribe or remove_peer
    /// removed it between our insert and our prune), the preserved
    /// `topic.active` MUST be suppressed. Otherwise the publisher gets
    /// an unpaired active — the unsubscribe/remove path already
    /// emitted the paired idle.
    #[tokio::test]
    async fn replay_closed_entry_absent_suppresses_active_notice() {
        let broker = SubscriptionBroker::new();
        let (pub_tx, _pub_rx) = mpsc::channel::<String>(16);
        broker
            .publish("t", &sample_inner_body("x", 1), "p", pub_tx, true)
            .await
            .unwrap();

        // Build an id that doesn't exist in the broker (mimicking the
        // post-state where a concurrent unsubscribe removed our entry).
        let absent_id: SubscriptionId = "ghost::topic::t".to_string();

        let pruned = broker
            .prune_dead_subscriptions(std::slice::from_ref(&absent_id))
            .await;
        assert!(
            pruned.is_empty(),
            "guarded prune returns empty for absent id (same as for live)"
        );

        let entry_live = {
            let inner = broker.inner.read().await;
            inner
                .subscriptions
                .get(&absent_id)
                .is_some_and(|s| !s.tx.is_closed())
        };
        assert!(
            !entry_live,
            "case (a): absent entry must classify as NOT live — active_notice will be suppressed"
        );
    }

    /// SPEC 12 C10b MAJOR regression — namespace-mismatched publishes
    /// must still reap dead-tx subscriptions so they don't linger
    /// indefinitely on filtered topics that never see matching traffic
    /// (which would otherwise hold idempotency captive for a re-grant).
    #[tokio::test]
    async fn mismatched_filter_publish_prunes_dead_tx() {
        let broker = SubscriptionBroker::new();
        let (pub_tx, _pub_rx) = mpsc::channel::<String>(16);

        let (sub_tx_dead, sub_rx_dead) = mpsc::channel::<String>(16);
        let filter = BodyFilter {
            namespace: "accounts".to_string(),
        };
        let (id, _, _, _) = broker
            .subscribe_topic_filtered(
                "maild.props.records.changed",
                "client-w",
                sub_tx_dead,
                Some(filter),
            )
            .await;
        drop(sub_rx_dead);
        assert_eq!(
            broker.subscriber_count("maild.props.records.changed").await,
            1,
            "subscription present before mismatched publish"
        );

        // Publish a non-matching namespace. The fan-out skips delivery,
        // but the prune branch must still detect the closed tx.
        let body = r#"---
command: maild.props.records.changed
---
{"namespace":"themes","records":[]}
"#;
        broker
            .publish("maild.props.records.changed", body, "maild", pub_tx, false)
            .await
            .unwrap();

        assert_eq!(
            broker.subscriber_count("maild.props.records.changed").await,
            0,
            "dead subscription must be pruned even on mismatched-filter publish",
        );

        // And: idempotency is no longer blocked — a fresh subscribe
        // produces the same id and inserts cleanly.
        let (sub_tx_live, _sub_rx_live) = mpsc::channel::<String>(16);
        let filter = BodyFilter {
            namespace: "accounts".to_string(),
        };
        let (id2, _, _, _) = broker
            .subscribe_topic_filtered(
                "maild.props.records.changed",
                "client-w",
                sub_tx_live,
                Some(filter),
            )
            .await;
        assert_eq!(id, id2);
    }

    #[tokio::test]
    async fn unsubscribe_emits_idle() {
        let broker = SubscriptionBroker::new();
        let (pub_tx, mut pub_rx) = mpsc::channel::<String>(16);
        let (sub_tx, _sub_rx) = mpsc::channel::<String>(16);

        broker
            .publish("t", &sample_inner_body("x", 1), "p", pub_tx, true)
            .await
            .unwrap();
        let (_, _, _, active) = broker.subscribe_topic("t", "v", sub_tx).await;
        assert_eq!(active.len(), 1);
        // Dispatch the active notice to its target (we are the test harness).
        // In real code the broker would do this.
        let _ = drain(&mut pub_rx, 1).await;

        let idle = broker.unsubscribe_topic("t", "v").await;
        assert_eq!(idle.len(), 1);
        assert!(idle[0].wire.contains("topic.idle"));
        assert_eq!(idle[0].target_peer, "p");
    }

    #[tokio::test]
    async fn reserved_name_rejected() {
        let broker = SubscriptionBroker::new();
        let (tx, _rx) = mpsc::channel::<String>(16);
        let err = broker
            .publish("$stats", &sample_inner_body("x", 1), "p", tx, true)
            .await
            .unwrap_err();
        assert!(matches!(err, PublishError::ReservedName));
    }

    #[tokio::test]
    async fn oversized_payload_rejected() {
        let broker = SubscriptionBroker::new();
        let (tx, _rx) = mpsc::channel::<String>(16);
        let huge = "a".repeat(MAX_SNAPSHOT_BYTES + 1);
        let err = broker.publish("t", &huge, "p", tx, true).await.unwrap_err();
        assert!(matches!(err, PublishError::PayloadTooLarge { .. }));
    }

    #[tokio::test]
    async fn producer_supplied_reserved_headers_overwritten() {
        let broker = SubscriptionBroker::new();
        let (pub_tx, _pub_rx) = mpsc::channel::<String>(16);
        let (sub_tx, mut sub_rx) = mpsc::channel::<String>(16);

        broker.subscribe_topic("t", "v", sub_tx).await;

        // Attempt to pre-seed topic_seq=9999 and topic=evil.
        let malicious =
            "---\ncommand: ui.batch\ntopic: evil\ntopic_seq: 9999\n---\n[]\n".to_string();
        let (seq, _, _) = broker
            .publish("t", &malicious, "p", pub_tx, true)
            .await
            .unwrap();
        assert_eq!(seq, 1);

        let msgs = drain(&mut sub_rx, 1).await;
        assert_eq!(msgs.len(), 1);
        // Broker values must win.
        assert!(msgs[0].contains("topic: t"));
        assert!(msgs[0].contains("topic_seq: 1"));
        assert!(!msgs[0].contains("topic: evil"));
        assert!(!msgs[0].contains("topic_seq: 9999"));
    }

    #[tokio::test]
    async fn mesh_publish_overwrites_hostile_origin_for_live_and_retained_delivery() {
        let broker = SubscriptionBroker::new();
        let (pub_tx, _pub_rx) = mpsc::channel::<String>(16);
        let (live_tx, mut live_rx) = mpsc::channel::<String>(16);
        broker.subscribe_topic("t", "live", live_tx).await;

        let mut hostile = BusMessage::new();
        hostile.set("command", "props.changed");
        hostile.set("broker_origin", "local");
        hostile.set("Broker_Origin", "local");
        hostile.body = r#"{"path":"state"}"#.into();
        broker
            .publish_with_origin(
                "t",
                &hostile.to_wire(),
                "remote-publisher",
                pub_tx,
                BrokerOrigin::Mesh,
                true,
            )
            .await
            .unwrap();

        let live = bus::parse(&drain(&mut live_rx, 1).await.remove(0)).unwrap();
        assert_eq!(live.get(BROKER_ORIGIN_HEADER), Some("mesh"));
        assert_eq!(
            live.headers
                .keys()
                .filter(|name| name.eq_ignore_ascii_case(BROKER_ORIGIN_HEADER))
                .count(),
            1
        );

        let (replay_tx, mut replay_rx) = mpsc::channel::<String>(16);
        let (_, replayed, _, _) = broker.subscribe_topic("t", "replay", replay_tx).await;
        assert!(replayed);
        let replay = bus::parse(&drain(&mut replay_rx, 1).await.remove(0)).unwrap();
        assert_eq!(replay.get(BROKER_ORIGIN_HEADER), Some("mesh"));
        assert_eq!(
            replay
                .headers
                .keys()
                .filter(|name| name.eq_ignore_ascii_case(BROKER_ORIGIN_HEADER))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn retain_false_does_not_cache() {
        let broker = SubscriptionBroker::new();
        let (pub_tx, _pub_rx) = mpsc::channel::<String>(16);
        let (sub_tx, mut sub_rx) = mpsc::channel::<String>(16);

        let (seq, delivered, _) = broker
            .publish("t", &sample_inner_body("x", 1), "p", pub_tx, false)
            .await
            .unwrap();
        assert_eq!(seq, 1);
        assert_eq!(delivered, 0);

        // Subscribe — no replay because nothing was cached.
        let (_, replayed, _, _) = broker.subscribe_topic("t", "v", sub_tx).await;
        assert!(!replayed);
        assert_eq!(drain(&mut sub_rx, 1).await.len(), 0);
    }

    #[tokio::test]
    async fn retain_false_still_increments_seq() {
        let broker = SubscriptionBroker::new();
        let (pub_tx, _pub_rx) = mpsc::channel::<String>(16);

        let (s1, _, _) = broker
            .publish("t", &sample_inner_body("x", 1), "p", pub_tx.clone(), false)
            .await
            .unwrap();
        let (s2, _, _) = broker
            .publish("t", &sample_inner_body("x", 2), "p", pub_tx.clone(), true)
            .await
            .unwrap();
        let (s3, _, _) = broker
            .publish("t", &sample_inner_body("x", 3), "p", pub_tx, false)
            .await
            .unwrap();
        assert_eq!((s1, s2, s3), (1, 2, 3));
    }

    #[tokio::test]
    async fn remove_peer_marks_publisher_topics_stale() {
        let broker = SubscriptionBroker::new();
        let (pub_tx, _pub_rx) = mpsc::channel::<String>(16);
        let (sub_tx, mut sub_rx) = mpsc::channel::<String>(16);

        broker
            .publish("t", &sample_inner_body("x", 1), "p", pub_tx.clone(), true)
            .await
            .unwrap();

        broker.remove_peer("p", &pub_tx).await;

        // New subscriber should receive replay with topic_stale: true
        broker.subscribe_topic("t", "v", sub_tx).await;
        let msgs = drain(&mut sub_rx, 1).await;
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].contains("topic_stale: true"));
    }

    #[tokio::test]
    async fn clear_removes_snapshot() {
        let broker = SubscriptionBroker::new();
        let (pub_tx, _pub_rx) = mpsc::channel::<String>(16);
        let (sub_tx, mut sub_rx) = mpsc::channel::<String>(16);

        broker
            .publish("t", &sample_inner_body("x", 1), "p", pub_tx, true)
            .await
            .unwrap();

        let (delivered, _) = broker.clear("t", true).await;
        // No subscribers before clear, so no delivery
        assert_eq!(delivered, 0);

        let (_, replayed, _, _) = broker.subscribe_topic("t", "v", sub_tx).await;
        assert!(!replayed, "snapshot was cleared, no replay expected");
        assert_eq!(drain(&mut sub_rx, 1).await.len(), 0);
    }

    #[tokio::test]
    async fn clear_with_subscribers_notifies_delete() {
        let broker = SubscriptionBroker::new();
        let (pub_tx, _pub_rx) = mpsc::channel::<String>(16);
        let (sub_tx, mut sub_rx) = mpsc::channel::<String>(16);

        broker
            .publish("t", &sample_inner_body("x", 1), "p", pub_tx, true)
            .await
            .unwrap();
        broker.subscribe_topic("t", "v", sub_tx).await;
        // drain the replay
        drain(&mut sub_rx, 1).await;

        let (delivered, _) = broker.clear("t", true).await;
        assert_eq!(delivered, 1);
        let msgs = drain(&mut sub_rx, 1).await;
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].contains("topic_op: delete"));
    }

    #[tokio::test]
    async fn list_reflects_topics_and_counts() {
        let broker = SubscriptionBroker::new();
        let (pub_tx, _pub_rx) = mpsc::channel::<String>(16);
        // Keep the receiver alive: subscribe_topic_filtered evicts on
        // replay-time `TrySendError::Closed` (C10b dead-sub guard), and
        // a publish-then-subscribe sequence with retain=true triggers
        // exactly that replay path.
        let (sub_tx, _sub_rx) = mpsc::channel::<String>(16);

        broker
            .publish(
                "sysmon.metrics",
                &sample_inner_body("x", 1),
                "p",
                pub_tx.clone(),
                true,
            )
            .await
            .unwrap();
        broker.subscribe_topic("sysmon.metrics", "v", sub_tx).await;

        let infos = broker.list(None).await;
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].name, "sysmon.metrics");
        assert_eq!(infos[0].subscribers, 1);
        assert!(infos[0].has_snapshot);
        assert_eq!(infos[0].snapshot_seq, 1);
        assert_eq!(infos[0].last_publisher.as_deref(), Some("p"));
        assert!(!infos[0].stale);

        let filtered = broker.list(Some("mail.")).await;
        assert!(filtered.is_empty());
    }

    #[tokio::test]
    async fn anon_id_format() {
        let id = synth_anon_id();
        assert!(id.starts_with("anon-"));
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[1].len(), 8);
        // Third part is a decimal unix timestamp
        assert!(parts[2].parse::<u64>().is_ok());
    }

    #[tokio::test]
    async fn anon_ids_unique_across_concurrent_calls() {
        let a = synth_anon_id();
        let b = synth_anon_id();
        let c = synth_anon_id();
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
    }

    // ── Ghost TopicState removal ──
    //
    // TopicState entries with snapshot=None AND subscribers=0 are dead — they
    // must be dropped so `topic.list` doesn't show zombie rows. Three trigger
    // sites: unsubscribe→0, peer disconnect→0, janitor snapshot purge.

    #[tokio::test]
    async fn unsubscribe_to_zero_drops_entry_when_no_snapshot() {
        let broker = SubscriptionBroker::new();
        let (pub_tx, _pub_rx) = mpsc::channel::<String>(16);
        let (sub_tx, _sub_rx) = mpsc::channel::<String>(16);

        // retain=false: publish creates TopicState but no snapshot.
        broker
            .publish("t", &sample_inner_body("x", 1), "p", pub_tx, false)
            .await
            .unwrap();
        broker.subscribe_topic("t", "v", sub_tx).await;
        broker.unsubscribe_topic("t", "v").await;

        assert!(
            broker.list(None).await.is_empty(),
            "dead topic (no snapshot, no subscribers) should not appear in list"
        );
    }

    #[tokio::test]
    async fn unsubscribe_to_zero_keeps_entry_when_snapshot_exists() {
        let broker = SubscriptionBroker::new();
        let (pub_tx, _pub_rx) = mpsc::channel::<String>(16);
        let (sub_tx, _sub_rx) = mpsc::channel::<String>(16);

        broker
            .publish("t", &sample_inner_body("x", 1), "p", pub_tx, true)
            .await
            .unwrap();
        broker.subscribe_topic("t", "v", sub_tx).await;
        broker.unsubscribe_topic("t", "v").await;

        let infos = broker.list(None).await;
        assert_eq!(infos.len(), 1, "retained snapshot keeps entry alive");
        assert!(infos[0].has_snapshot);
        assert_eq!(infos[0].subscribers, 0);
    }

    #[tokio::test]
    async fn remove_peer_drops_entry_when_no_snapshot() {
        let broker = SubscriptionBroker::new();
        let (pub_tx, _pub_rx) = mpsc::channel::<String>(16);
        let (sub_tx, _sub_rx) = mpsc::channel::<String>(16);

        broker
            .publish("t", &sample_inner_body("x", 1), "p", pub_tx, false)
            .await
            .unwrap();
        broker.subscribe_topic("t", "v", sub_tx.clone()).await;
        broker.remove_peer("v", &sub_tx).await;

        assert!(
            broker.list(None).await.is_empty(),
            "dead topic after subscriber disconnect should not appear in list"
        );
    }

    #[tokio::test]
    async fn janitor_sweeps_dead_entries() {
        let broker = SubscriptionBroker::new();
        let (pub_tx, _pub_rx) = mpsc::channel::<String>(16);

        // retain=false publish with nobody subscribing → dead entry immediately.
        broker
            .publish("t", &sample_inner_body("x", 1), "p", pub_tx, false)
            .await
            .unwrap();
        assert_eq!(
            broker.list(None).await.len(),
            1,
            "entry exists before janitor"
        );

        broker.janitor_tick().await;
        assert!(
            broker.list(None).await.is_empty(),
            "janitor should sweep dead TopicState (no snapshot, no subscribers)"
        );
    }

    /// SPEC 12 C10b — janitor must catch dead-tx subscriptions even on
    /// topics that never see a matching publish (which is how the
    /// publish-hot-loop prunes). Without this sweep, a subscriber whose
    /// connection silently died on a low-traffic filtered topic would
    /// remain in the registry forever, holding an empty TopicState
    /// alive and blocking re-grants via idempotency.
    #[tokio::test]
    async fn janitor_sweeps_dead_tx_subscriptions() {
        let broker = SubscriptionBroker::new();
        let (sub_tx, sub_rx) = mpsc::channel::<String>(16);
        let filter = BodyFilter {
            namespace: "accounts".to_string(),
        };
        broker
            .subscribe_topic_filtered(
                "maild.props.records.changed",
                "client-w",
                sub_tx,
                Some(filter),
            )
            .await;
        // Subscriber disconnects without any publish ever arriving.
        drop(sub_rx);

        assert_eq!(
            broker.subscriber_count("maild.props.records.changed").await,
            1,
            "subscription present before janitor"
        );

        broker.janitor_tick().await;

        assert_eq!(
            broker.subscriber_count("maild.props.records.changed").await,
            0,
            "janitor must reap dead-tx subscription even with no traffic"
        );
    }

    /// SPEC 12 C10b — when the publish hot loop's dead-tx prune drives
    /// a topic from 1 to 0 subscribers, the publisher must see the
    /// paired `topic.idle` in the returned notifications.
    #[tokio::test]
    async fn publish_emits_idle_when_prune_drives_count_to_zero() {
        let broker = SubscriptionBroker::new();
        let (pub_tx, mut pub_rx) = mpsc::channel::<String>(16);
        let (sub_tx, sub_rx) = mpsc::channel::<String>(16);

        broker
            .publish("t", &sample_inner_body("x", 1), "p", pub_tx.clone(), true)
            .await
            .unwrap();
        let (_, _, _, active) = broker.subscribe_topic("t", "v", sub_tx).await;
        assert_eq!(active.len(), 1);
        let _ = drain(&mut pub_rx, 1).await;

        drop(sub_rx);
        let (_, _delivered, notices) = broker
            .publish("t", &sample_inner_body("x", 2), "p", pub_tx, false)
            .await
            .unwrap();

        assert_eq!(
            notices.len(),
            1,
            "publish hot-loop prune-to-zero must emit topic.idle"
        );
        assert!(notices[0].wire.contains("topic.idle"));
        assert_eq!(notices[0].target_peer, "p");
    }

    /// SPEC 12 C10b — symmetric idle pairing for the `clear()` path.
    #[tokio::test]
    async fn clear_emits_idle_when_prune_drives_count_to_zero() {
        let broker = SubscriptionBroker::new();
        let (pub_tx, mut pub_rx) = mpsc::channel::<String>(16);
        let (sub_tx, sub_rx) = mpsc::channel::<String>(16);

        broker
            .publish("t", &sample_inner_body("x", 1), "p", pub_tx, true)
            .await
            .unwrap();
        let (_, _, _, active) = broker.subscribe_topic("t", "v", sub_tx).await;
        assert_eq!(active.len(), 1);
        let _ = drain(&mut pub_rx, 1).await;

        drop(sub_rx);
        let (_delivered, notices) = broker.clear("t", true).await;

        assert_eq!(
            notices.len(),
            1,
            "clear() prune-to-zero must emit topic.idle"
        );
        assert!(notices[0].wire.contains("topic.idle"));
        assert_eq!(notices[0].target_peer, "p");
    }

    /// SPEC 12 C10b — when the janitor's dead-tx sweep drives a topic
    /// from N>0 to 0 subscribers, the publisher must receive the
    /// matching `topic.idle` so the active/idle pairing isn't broken.
    /// Without this notification, a publisher that saw `topic.active`
    /// and then had its sole subscriber die silently would remain stuck
    /// in "active" forever.
    #[tokio::test]
    async fn janitor_emits_idle_when_dead_tx_sweep_drives_count_to_zero() {
        let broker = SubscriptionBroker::new();
        let (pub_tx, mut pub_rx) = mpsc::channel::<String>(16);
        let (sub_tx, sub_rx) = mpsc::channel::<String>(16);

        // Publish first so there's a last_publisher_tx captured for the
        // idle notification target.
        broker
            .publish("t", &sample_inner_body("x", 1), "p", pub_tx, true)
            .await
            .unwrap();
        let (_, _, _, active) = broker.subscribe_topic("t", "v", sub_tx).await;
        assert_eq!(active.len(), 1, "0→1 fires topic.active");
        // Drain the active so the next recv'd frame is unambiguous.
        let _ = drain(&mut pub_rx, 1).await;

        // Subscriber dies silently.
        drop(sub_rx);
        let notices = broker.janitor_tick().await;

        assert_eq!(
            notices.len(),
            1,
            "janitor must emit topic.idle when sweep drives count to 0"
        );
        assert!(notices[0].wire.contains("topic.idle"));
        assert_eq!(notices[0].target_peer, "p");
    }

    /// SPEC 12 C10b — `clear()` must reap dead-tx subscribers even when
    /// they would have been filter-skipped on the cleared snapshot's
    /// namespace. Otherwise a clear-with-notify on a filtered topic
    /// leaves dead subs durable.
    #[tokio::test]
    async fn clear_prunes_dead_tx_filter_mismatched_subscriber() {
        let broker = SubscriptionBroker::new();
        let (pub_tx, _pub_rx) = mpsc::channel::<String>(16);

        // Cache a snapshot in namespace "accounts" so clear() will fire
        // a delete with cleared_namespace = Some("accounts").
        broker
            .publish(
                "maild.props.records.changed",
                r#"---
command: maild.props.records.changed
---
{"namespace":"accounts","records":[]}
"#,
                "maild",
                pub_tx,
                true,
            )
            .await
            .unwrap();

        // Subscriber filters on "themes" — will NOT match the cleared
        // snapshot's namespace. Drop receiver to make it dead-on-arrival.
        let (sub_tx, sub_rx) = mpsc::channel::<String>(16);
        broker
            .subscribe_topic_filtered(
                "maild.props.records.changed",
                "client-w",
                sub_tx,
                Some(BodyFilter {
                    namespace: "themes".to_string(),
                }),
            )
            .await;
        drop(sub_rx);

        broker.clear("maild.props.records.changed", true).await;

        assert_eq!(
            broker.subscriber_count("maild.props.records.changed").await,
            0,
            "clear() must prune dead-tx subscribers on filter mismatch",
        );
    }

    // ── Coverage-by-composition-gap audit (v1.2) ──
    //
    // The v1.1 bug escaped five review gates because every broker test used
    // registered-style peer IDs. These tests exercise the same state transitions
    // with synth_anon_id()-format peer strings, locking in that the broker's
    // notification path is peer-name-agnostic (notifications carry target_tx
    // directly rather than looking it up in a registry).
    //
    // Matrix: {registered, anonymous} × {publisher, subscriber} × {relevant transitions}
    // Registered cases are covered by the tests above; these add the anon half.

    #[tokio::test]
    async fn anon_publisher_0_to_1_subscribe_notifies() {
        let broker = SubscriptionBroker::new();
        let anon = synth_anon_id();
        let (pub_tx, mut pub_rx) = mpsc::channel::<String>(16);
        let (sub_tx, _sub_rx) = mpsc::channel::<String>(16);

        broker
            .publish("t", &sample_inner_body("x", 1), &anon, pub_tx, true)
            .await
            .unwrap();
        let (_, _, _, notes) = broker.subscribe_topic("t", "v", sub_tx).await;

        assert_eq!(notes.len(), 1, "0→1 should fire topic.active");
        assert!(notes[0].wire.contains("topic.active"));
        assert_eq!(notes[0].target_peer, anon, "target is the anon publisher");
        // target_tx must be usable — the v1.1 bug was that anon peers failed
        // registry lookup, dropping the notification. Prove delivery works.
        notes[0].target_tx.try_send(notes[0].wire.clone()).unwrap();
        let msgs = drain(&mut pub_rx, 1).await;
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].contains("topic.active"));
    }

    #[tokio::test]
    async fn anon_publisher_1_to_0_unsubscribe_notifies() {
        let broker = SubscriptionBroker::new();
        let anon = synth_anon_id();
        let (pub_tx, mut pub_rx) = mpsc::channel::<String>(16);
        let (sub_tx, _sub_rx) = mpsc::channel::<String>(16);

        broker
            .publish("t", &sample_inner_body("x", 1), &anon, pub_tx, true)
            .await
            .unwrap();
        broker.subscribe_topic("t", "v", sub_tx).await;
        drain(&mut pub_rx, 1).await; // drain the active notice the broker would deliver

        let idle = broker.unsubscribe_topic("t", "v").await;
        assert_eq!(idle.len(), 1);
        assert!(idle[0].wire.contains("topic.idle"));
        assert_eq!(idle[0].target_peer, anon);
        idle[0].target_tx.try_send(idle[0].wire.clone()).unwrap();
        let msgs = drain(&mut pub_rx, 1).await;
        assert_eq!(msgs.len(), 1);
    }

    #[tokio::test]
    async fn anon_subscriber_disconnect_notifies_publisher() {
        let broker = SubscriptionBroker::new();
        let anon_sub = synth_anon_id();
        let (pub_tx, mut pub_rx) = mpsc::channel::<String>(16);
        let (sub_tx, _sub_rx) = mpsc::channel::<String>(16);

        broker
            .publish("t", &sample_inner_body("x", 1), "p", pub_tx, true)
            .await
            .unwrap();
        broker.subscribe_topic("t", &anon_sub, sub_tx.clone()).await;
        drain(&mut pub_rx, 1).await;

        // Anon subscriber disconnects.
        let idle = broker.remove_peer(&anon_sub, &sub_tx).await;
        assert_eq!(idle.len(), 1, "1→0 from disconnect should fire topic.idle");
        assert_eq!(idle[0].target_peer, "p");
        idle[0].target_tx.try_send(idle[0].wire.clone()).unwrap();
        let msgs = drain(&mut pub_rx, 1).await;
        assert_eq!(msgs.len(), 1);
    }

    #[tokio::test]
    async fn anon_publisher_disconnect_marks_stale() {
        let broker = SubscriptionBroker::new();
        let anon = synth_anon_id();
        let (pub_tx, _pub_rx) = mpsc::channel::<String>(16);
        let (sub_tx, mut sub_rx) = mpsc::channel::<String>(16);

        broker
            .publish("t", &sample_inner_body("x", 1), &anon, pub_tx.clone(), true)
            .await
            .unwrap();
        broker.remove_peer(&anon, &pub_tx).await;

        broker.subscribe_topic("t", "v", sub_tx).await;
        let msgs = drain(&mut sub_rx, 1).await;
        assert_eq!(msgs.len(), 1);
        assert!(
            msgs[0].contains("topic_stale: true"),
            "anon publisher disconnect must mark snapshot stale like registered"
        );
    }

    #[tokio::test]
    async fn remove_peer_is_channel_scoped_across_reregistration() {
        // SPEC 12 §15.5 impersonation window, subscription side. An old
        // connection and a new connection both use the peer name `svc`
        // (citizen restart under systemd / route_local prune of a half-dead
        // tx). Tearing down the OLD connection — via WS-close or
        // noded.deregister — must touch only ITS channel: the new
        // connection's subscription and its republished snapshot must
        // survive. A string-keyed remove_peer would wipe both.
        let broker = SubscriptionBroker::new();
        let (old_tx, _old_rx) = mpsc::channel::<String>(16);
        let (new_tx, mut new_rx) = mpsc::channel::<String>(16);

        // Old conn: owns its own-only topic, and is last publisher of
        // "shared".
        broker
            .subscribe_topic("old_only", "svc", old_tx.clone())
            .await;
        broker
            .publish(
                "shared",
                &sample_inner_body("v1", 1),
                "svc",
                old_tx.clone(),
                true,
            )
            .await
            .unwrap();

        // New conn re-registers `svc`: subscribes "shared" and republishes
        // it, so "shared"'s last_publisher_tx is now the NEW channel and the
        // (peer,topic) subscription id now maps to the NEW tx.
        broker
            .subscribe_topic("shared", "svc", new_tx.clone())
            .await;
        broker
            .publish(
                "shared",
                &sample_inner_body("v2", 2),
                "svc",
                new_tx.clone(),
                true,
            )
            .await
            .unwrap();

        // Old connection tears down — channel-scoped to old_tx.
        broker.remove_peer("svc", &old_tx).await;

        // New conn's "shared" subscription survived: a fresh publish reaches
        // it and the snapshot was NOT collaterally marked stale.
        broker
            .publish(
                "shared",
                &sample_inner_body("v3", 3),
                "svc",
                new_tx.clone(),
                true,
            )
            .await
            .unwrap();
        let msgs = drain(&mut new_rx, 8).await;
        assert!(
            msgs.iter().any(|m| m.contains("v3")),
            "new owner's subscription must survive old connection teardown"
        );
        assert!(
            !msgs.iter().any(|m| m.contains("topic_stale: true")),
            "new owner's republished snapshot must not be marked stale"
        );

        // Old conn's own-only topic WAS channel-matched and is gone.
        let infos = broker.list(None).await;
        assert!(
            !infos.iter().any(|i| i.name == "old_only"),
            "old connection's own subscription should be torn down"
        );
    }

    #[tokio::test]
    async fn anon_publisher_shows_in_topic_list() {
        let broker = SubscriptionBroker::new();
        let anon = synth_anon_id();
        let (pub_tx, _pub_rx) = mpsc::channel::<String>(16);

        broker
            .publish("t", &sample_inner_body("x", 1), &anon, pub_tx, true)
            .await
            .unwrap();

        let infos = broker.list(None).await;
        assert_eq!(infos.len(), 1);
        assert_eq!(
            infos[0].last_publisher.as_deref(),
            Some(anon.as_str()),
            "topic.list must preserve anon publisher IDs verbatim"
        );
        assert!(infos[0].has_snapshot);
    }

    #[tokio::test]
    async fn anon_subscriber_receives_clear_notification() {
        let broker = SubscriptionBroker::new();
        let anon_sub = synth_anon_id();
        let (pub_tx, _pub_rx) = mpsc::channel::<String>(16);
        let (sub_tx, mut sub_rx) = mpsc::channel::<String>(16);

        broker
            .publish("t", &sample_inner_body("x", 1), "p", pub_tx, true)
            .await
            .unwrap();
        broker.subscribe_topic("t", &anon_sub, sub_tx).await;
        drain(&mut sub_rx, 1).await; // snapshot replay

        let (delivered, _) = broker.clear("t", true).await;
        assert_eq!(delivered, 1, "anon subscriber should receive clear notify");
        let msgs = drain(&mut sub_rx, 1).await;
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].contains("topic_op: delete"));
    }

    // ── C10a — Body-filter (per-namespace) subscriptions ──
    //
    // SPEC 12 §15.5 reserves `<svc>.props.records.changed` /
    // `<svc>.props.audit` as per-service topics whose payloads span
    // every namespace under the service. Filtered subscriptions
    // (`subscribe_topic_filtered` with `BodyFilter{namespace}`) gate
    // delivery on the body JSON's top-level `namespace` field so a
    // subscriber granted access to one namespace cannot observe events
    // from sibling namespaces under the same service. The cross-
    // namespace isolation property is enforced at the BROKER, not at
    // the granter — see the rev 2 plan doc.

    /// Build an Bus inner body with `namespace` in the JSON payload —
    /// matches the shape `project_records_changed` produces.
    fn ns_body(namespace: &str, key: &str) -> String {
        format!(
            "---\ncommand: ui.batch\n---\n{{\"namespace\":\"{namespace}\",\"key\":\"{key}\"}}\n"
        )
    }

    #[tokio::test]
    async fn filtered_subscriber_only_receives_matching_namespace() {
        let broker = SubscriptionBroker::new();
        let (pub_tx, _pub_rx) = mpsc::channel::<String>(16);
        let (sub_tx, mut sub_rx) = mpsc::channel::<String>(16);

        broker
            .subscribe_topic_filtered(
                "maild.props.records.changed",
                "v",
                sub_tx,
                Some(BodyFilter {
                    namespace: "maild.accounts".to_string(),
                }),
            )
            .await;

        // Matching namespace — should deliver.
        broker
            .publish(
                "maild.props.records.changed",
                &ns_body("maild.accounts", "alpha@h"),
                "maild",
                pub_tx.clone(),
                false,
            )
            .await
            .unwrap();
        // Non-matching namespace under the same service — must be
        // silently filtered out at the broker.
        broker
            .publish(
                "maild.props.records.changed",
                &ns_body("maild.themes", "dark"),
                "maild",
                pub_tx.clone(),
                false,
            )
            .await
            .unwrap();
        // Second matching publish — should deliver.
        broker
            .publish(
                "maild.props.records.changed",
                &ns_body("maild.accounts", "beta@h"),
                "maild",
                pub_tx,
                false,
            )
            .await
            .unwrap();

        let msgs = drain(&mut sub_rx, 5).await;
        assert_eq!(
            msgs.len(),
            2,
            "filtered subscriber should receive 2 maild.accounts publishes only, got {msgs:?}",
        );
        for m in &msgs {
            assert!(
                m.contains(r#""namespace":"maild.accounts""#),
                "wrong namespace leaked into filtered stream: {m}",
            );
        }
    }

    #[tokio::test]
    async fn unfiltered_subscriber_still_receives_every_namespace() {
        // Regression guard: filter support must not change behaviour for
        // existing unfiltered topic subscribers (those that came in via
        // the legacy `subscribe_topic`).
        let broker = SubscriptionBroker::new();
        let (pub_tx, _pub_rx) = mpsc::channel::<String>(16);
        let (sub_tx, mut sub_rx) = mpsc::channel::<String>(16);

        broker
            .subscribe_topic("maild.props.records.changed", "v", sub_tx)
            .await;

        broker
            .publish(
                "maild.props.records.changed",
                &ns_body("maild.accounts", "a"),
                "maild",
                pub_tx.clone(),
                false,
            )
            .await
            .unwrap();
        broker
            .publish(
                "maild.props.records.changed",
                &ns_body("maild.themes", "t"),
                "maild",
                pub_tx,
                false,
            )
            .await
            .unwrap();

        let msgs = drain(&mut sub_rx, 5).await;
        assert_eq!(
            msgs.len(),
            2,
            "unfiltered subscriber must still see all publishes regardless of namespace",
        );
    }

    #[tokio::test]
    async fn distinct_filters_coexist_for_same_peer_topic() {
        let broker = SubscriptionBroker::new();
        let (pub_tx, _pub_rx) = mpsc::channel::<String>(16);
        let (sub_a_tx, mut sub_a_rx) = mpsc::channel::<String>(16);
        let (sub_b_tx, mut sub_b_rx) = mpsc::channel::<String>(16);

        // Same peer "v" subscribes twice with different filters. Each
        // gets its own outbound channel here purely so we can drain
        // separately in the test; in production both would be on the
        // peer's single outbound, and the filter discriminator keeps
        // the two subscriptions distinct in the registry.
        broker
            .subscribe_topic_filtered(
                "t",
                "v",
                sub_a_tx,
                Some(BodyFilter {
                    namespace: "a".into(),
                }),
            )
            .await;
        broker
            .subscribe_topic_filtered(
                "t",
                "v",
                sub_b_tx,
                Some(BodyFilter {
                    namespace: "b".into(),
                }),
            )
            .await;

        broker
            .publish("t", &ns_body("a", "k"), "p", pub_tx.clone(), false)
            .await
            .unwrap();
        broker
            .publish("t", &ns_body("b", "k"), "p", pub_tx, false)
            .await
            .unwrap();

        let a_msgs = drain(&mut sub_a_rx, 2).await;
        let b_msgs = drain(&mut sub_b_rx, 2).await;
        assert_eq!(a_msgs.len(), 1, "filter A must only see ns=a");
        assert!(a_msgs[0].contains(r#""namespace":"a""#));
        assert_eq!(b_msgs.len(), 1, "filter B must only see ns=b");
        assert!(b_msgs[0].contains(r#""namespace":"b""#));
    }

    #[tokio::test]
    async fn unsubscribe_topic_removes_all_filter_variants() {
        // C10a contract: `topic.unsubscribe { topic }` is filter-aware
        // by way of "remove all filters for this (peer, topic) pair".
        // The wire surface takes no filter, so a partial unsubscribe
        // (leaving sibling-namespace filters live) would silently leak
        // events the caller thought they were off.
        let broker = SubscriptionBroker::new();
        let (pub_tx, _pub_rx) = mpsc::channel::<String>(16);
        let (sub_a_tx, mut sub_a_rx) = mpsc::channel::<String>(16);
        let (sub_b_tx, mut sub_b_rx) = mpsc::channel::<String>(16);

        broker
            .subscribe_topic_filtered(
                "t",
                "v",
                sub_a_tx,
                Some(BodyFilter {
                    namespace: "a".into(),
                }),
            )
            .await;
        broker
            .subscribe_topic_filtered(
                "t",
                "v",
                sub_b_tx,
                Some(BodyFilter {
                    namespace: "b".into(),
                }),
            )
            .await;
        assert_eq!(
            broker.subscriber_count("t").await,
            2,
            "both filter variants count toward subscriber_count",
        );

        // Single unsubscribe call removes BOTH filter variants.
        broker.unsubscribe_topic("t", "v").await;
        assert_eq!(
            broker.subscriber_count("t").await,
            0,
            "unsubscribe_topic must remove every filter variant in one call",
        );

        // Verify no further deliveries reach either filtered channel.
        broker
            .publish("t", &ns_body("a", "k"), "p", pub_tx.clone(), false)
            .await
            .unwrap();
        broker
            .publish("t", &ns_body("b", "k"), "p", pub_tx, false)
            .await
            .unwrap();
        assert!(
            drain(&mut sub_a_rx, 2).await.is_empty(),
            "filter A must not receive publishes after unsubscribe",
        );
        assert!(
            drain(&mut sub_b_rx, 2).await.is_empty(),
            "filter B must not receive publishes after unsubscribe",
        );
    }

    #[tokio::test]
    async fn unsubscribe_topic_removes_unfiltered_alongside_filtered() {
        // Mixed case: peer holds one unfiltered + one filtered
        // subscription to the same topic. A single unsubscribe drops
        // both.
        let broker = SubscriptionBroker::new();
        let (un_tx, mut un_rx) = mpsc::channel::<String>(16);
        let (filt_tx, mut filt_rx) = mpsc::channel::<String>(16);
        let (pub_tx, _pub_rx) = mpsc::channel::<String>(16);

        broker.subscribe_topic("t", "v", un_tx).await;
        broker
            .subscribe_topic_filtered(
                "t",
                "v",
                filt_tx,
                Some(BodyFilter {
                    namespace: "a".into(),
                }),
            )
            .await;
        assert_eq!(broker.subscriber_count("t").await, 2);

        broker.unsubscribe_topic("t", "v").await;
        assert_eq!(broker.subscriber_count("t").await, 0);

        broker
            .publish("t", &ns_body("a", "k"), "p", pub_tx, false)
            .await
            .unwrap();
        assert!(drain(&mut un_rx, 2).await.is_empty());
        assert!(drain(&mut filt_rx, 2).await.is_empty());
    }

    #[tokio::test]
    async fn filtered_replay_skips_non_matching_snapshot() {
        // Retained snapshot for namespace=a; new subscriber filters
        // namespace=b → snapshot must NOT be replayed.
        let broker = SubscriptionBroker::new();
        let (pub_tx, _pub_rx) = mpsc::channel::<String>(16);
        let (sub_tx, mut sub_rx) = mpsc::channel::<String>(16);

        broker
            .publish("t", &ns_body("a", "k"), "p", pub_tx, true)
            .await
            .unwrap();

        let (_, replayed, _, _) = broker
            .subscribe_topic_filtered(
                "t",
                "v",
                sub_tx,
                Some(BodyFilter {
                    namespace: "b".into(),
                }),
            )
            .await;
        assert!(
            !replayed,
            "snapshot for ns=a must not replay to ns=b filter"
        );
        assert!(drain(&mut sub_rx, 1).await.is_empty());
    }

    #[tokio::test]
    async fn filtered_replay_delivers_matching_snapshot() {
        let broker = SubscriptionBroker::new();
        let (pub_tx, _pub_rx) = mpsc::channel::<String>(16);
        let (sub_tx, mut sub_rx) = mpsc::channel::<String>(16);

        broker
            .publish("t", &ns_body("a", "k"), "p", pub_tx, true)
            .await
            .unwrap();

        let (_, replayed, _, _) = broker
            .subscribe_topic_filtered(
                "t",
                "v",
                sub_tx,
                Some(BodyFilter {
                    namespace: "a".into(),
                }),
            )
            .await;
        assert!(replayed, "snapshot must replay when filter matches");
        let msgs = drain(&mut sub_rx, 1).await;
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].contains(r#""namespace":"a""#));
    }

    #[tokio::test]
    async fn clear_with_notify_filters_by_cleared_snapshot_namespace() {
        // C10a: a filtered subscriber whose namespace doesn't match the
        // cleared snapshot's namespace must NOT receive the delete
        // notice — they never saw the snapshot on replay.
        let broker = SubscriptionBroker::new();
        let (pub_tx, _pub_rx) = mpsc::channel::<String>(16);
        let (sub_match_tx, mut sub_match_rx) = mpsc::channel::<String>(16);
        let (sub_mismatch_tx, mut sub_mismatch_rx) = mpsc::channel::<String>(16);
        let (sub_unfilt_tx, mut sub_unfilt_rx) = mpsc::channel::<String>(16);

        broker
            .publish("t", &ns_body("a", "k"), "p", pub_tx, true)
            .await
            .unwrap();
        broker
            .subscribe_topic_filtered(
                "t",
                "vm",
                sub_match_tx,
                Some(BodyFilter {
                    namespace: "a".into(),
                }),
            )
            .await;
        broker
            .subscribe_topic_filtered(
                "t",
                "vn",
                sub_mismatch_tx,
                Some(BodyFilter {
                    namespace: "b".into(),
                }),
            )
            .await;
        broker.subscribe_topic("t", "vu", sub_unfilt_tx).await;
        // Drain replays (only sub_match and sub_unfilt got one).
        drain(&mut sub_match_rx, 2).await;
        drain(&mut sub_unfilt_rx, 2).await;
        assert!(drain(&mut sub_mismatch_rx, 1).await.is_empty());

        let (delivered, _) = broker.clear("t", true).await;
        // Matched filter and unfiltered each get the delete; mismatch
        // does not.
        assert_eq!(delivered, 2);

        assert_eq!(drain(&mut sub_match_rx, 2).await.len(), 1);
        assert_eq!(drain(&mut sub_unfilt_rx, 2).await.len(), 1);
        assert!(
            drain(&mut sub_mismatch_rx, 2).await.is_empty(),
            "filter-mismatch subscriber must not receive clear notice",
        );
    }

    #[tokio::test]
    async fn unsubscribe_topic_emits_idle_when_multiple_filter_variants_removed_at_once() {
        // Single peer holds two filtered subscriptions on the same
        // (peer, topic). A single unsubscribe must drive the topic to
        // 0 subscribers and fire exactly one topic.idle (1→0 transition
        // is per-topic, not per-filter).
        let broker = SubscriptionBroker::new();
        let (pub_tx, mut pub_rx) = mpsc::channel::<String>(16);
        let (sub_a_tx, _) = mpsc::channel::<String>(16);
        let (sub_b_tx, _) = mpsc::channel::<String>(16);

        broker
            .publish("t", &ns_body("a", "k"), "p", pub_tx, true)
            .await
            .unwrap();
        broker
            .subscribe_topic_filtered(
                "t",
                "v",
                sub_a_tx,
                Some(BodyFilter {
                    namespace: "a".into(),
                }),
            )
            .await;
        broker
            .subscribe_topic_filtered(
                "t",
                "v",
                sub_b_tx,
                Some(BodyFilter {
                    namespace: "b".into(),
                }),
            )
            .await;
        drain(&mut pub_rx, 1).await; // initial topic.active for first sub

        let notes = broker.unsubscribe_topic("t", "v").await;
        assert_eq!(
            notes.len(),
            1,
            "exactly one topic.idle on 2→0, regardless of filter variant count",
        );
        assert!(notes[0].wire.contains("topic.idle"));
        assert_eq!(broker.subscriber_count("t").await, 0);
    }

    #[tokio::test]
    async fn topic_sub_id_does_not_collide_on_delimiter_in_topic_name() {
        // Codex MAJOR (C10a rev 2): topic names are free-form UTF-8, so
        // a topic literally named "t::ns::a" must not produce the same
        // SubscriptionId as a filtered subscription to topic "t" with
        // namespace "a". Length-prefix encoding makes collisions
        // impossible — pin the property here.
        let unfiltered_evil = SubscriptionBroker::topic_sub_id("v", "t::ns::a", None);
        let filtered_normal = SubscriptionBroker::topic_sub_id(
            "v",
            "t",
            Some(&BodyFilter {
                namespace: "a".into(),
            }),
        );
        assert_ne!(
            unfiltered_evil, filtered_normal,
            "topic name containing ::ns:: must not collide with filtered id",
        );
        // And a side-by-side roundtrip: same (peer, topic, filter)
        // produces the same id; different filters produce different ids.
        let f_a = BodyFilter {
            namespace: "a".into(),
        };
        let f_b = BodyFilter {
            namespace: "b".into(),
        };
        assert_eq!(
            SubscriptionBroker::topic_sub_id("v", "t", Some(&f_a)),
            SubscriptionBroker::topic_sub_id("v", "t", Some(&f_a)),
        );
        assert_ne!(
            SubscriptionBroker::topic_sub_id("v", "t", Some(&f_a)),
            SubscriptionBroker::topic_sub_id("v", "t", Some(&f_b)),
        );
        // And the unfiltered id is distinct from any filtered id for
        // the same (peer, topic).
        assert_ne!(
            SubscriptionBroker::topic_sub_id("v", "t", None),
            SubscriptionBroker::topic_sub_id("v", "t", Some(&f_a)),
        );
        // Codex re-review caught the peer segment was still
        // unprefixed: peer `"a"` with topic `"b::topic::1:c"`
        // previously collided with peer `"a::topic::13:b"`, topic
        // `"c"` because the peer segment was a raw prefix. Length-
        // prefixing the peer fixes it.
        let evil_peer = SubscriptionBroker::topic_sub_id("a::topic::13:b", "c", None);
        let normal_peer = SubscriptionBroker::topic_sub_id("a", "b::topic::1:c", None);
        assert_ne!(
            evil_peer, normal_peer,
            "crafted peer string must not reconstruct another (peer, topic) id",
        );
    }

    #[tokio::test]
    async fn filtered_subscribe_idempotent_per_filter() {
        let broker = SubscriptionBroker::new();
        let (sub_tx, mut sub_rx) = mpsc::channel::<String>(16);
        let (pub_tx, _pub_rx) = mpsc::channel::<String>(16);

        broker
            .publish("t", &ns_body("a", "k"), "p", pub_tx, true)
            .await
            .unwrap();

        let filter = BodyFilter {
            namespace: "a".into(),
        };
        let (id1, r1, _, _) = broker
            .subscribe_topic_filtered("t", "v", sub_tx.clone(), Some(filter.clone()))
            .await;
        let (id2, r2, _, n2) = broker
            .subscribe_topic_filtered("t", "v", sub_tx.clone(), Some(filter))
            .await;
        assert_eq!(id1, id2, "same (peer, topic, filter) must yield same id");
        assert!(r1);
        assert!(!r2, "second subscribe with same filter must not replay");
        assert!(n2.is_empty(), "second subscribe must not fire topic.active");
        // Only one replay reaches the subscriber.
        let msgs = drain(&mut sub_rx, 2).await;
        assert_eq!(msgs.len(), 1);
        assert_eq!(broker.subscriber_count("t").await, 1);
    }
}
