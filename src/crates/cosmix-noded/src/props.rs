//! SPEC 07 §2 property surface for cosmix-noded (L1 conformance).
//!
//! Exposes config, lifecycle, services, and topics as a uniform
//! `PropTree`. Wired into the broker dispatch via `handle_props_command`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use cosmix_props::{PropDescribe, PropPath, PropTree, PropType, PropValue, tree::build_snapshot};
use serde_json::Value as Json;
use tokio::sync::{Mutex, mpsc};
use tokio::time::Instant;

use crate::subscription::SubscriptionBroker;

/// SPEC 07 §7.1 — at most 10 events per second per path.
const CHANGE_CAP_INTERVAL: Duration = Duration::from_millis(100);

/// SPEC 07 §7.1 — `world.<daemon>` retained republishes capped at 1 Hz.
const WORLD_REPUBLISH_MIN: Duration = Duration::from_millis(1000);

/// Topic carrying noded property change events (SPEC 07 §3).
pub const PROPS_CHANGED_TOPIC: &str = "noded.props.changed";

/// Retained topic carrying the full noded property snapshot (SPEC 07 §3 L3).
pub const WORLD_NODED_TOPIC: &str = "world.noded";

/// Snapshot data the props tree reads from. Captured at construction
/// time so the trait impl can be `&self` without holding async locks.
///
/// For L1 the impl rebuilds the snapshot per `props.get` from a fresh
/// `NodedPropsSource::collect` call. L2/L3 will cache + invalidate on
/// `props.changed` events.
pub struct NodedPropsSnapshot {
    pub bind: String,
    pub node_name: String,
    pub log_level: String,
    pub started_at: String,
    pub uptime_s: u64,
    pub services_registered: Vec<String>,
    pub topics_active: u64,
    pub topics_snapshot_bytes: u64,
}

impl NodedPropsSnapshot {
    pub fn snapshot_value(&self) -> PropValue {
        build_snapshot([
            (
                PropPath::new("config.bind").unwrap(),
                PropValue::from(self.bind.clone()),
            ),
            (
                PropPath::new("config.node_name").unwrap(),
                PropValue::from(self.node_name.clone()),
            ),
            (
                PropPath::new("config.log_level").unwrap(),
                PropValue::from(self.log_level.clone()),
            ),
            (
                PropPath::new("lifecycle.started_at").unwrap(),
                PropValue::from(self.started_at.clone()),
            ),
            (
                PropPath::new("lifecycle.uptime_s").unwrap(),
                PropValue::from(self.uptime_s),
            ),
            (
                PropPath::new("lifecycle.health").unwrap(),
                PropValue::from("ok"),
            ),
            (
                PropPath::new("lifecycle.props_level").unwrap(),
                PropValue::from("L3"),
            ),
            (
                PropPath::new("services.registered").unwrap(),
                PropValue::List(
                    self.services_registered
                        .iter()
                        .map(|s| PropValue::from(s.clone()))
                        .collect(),
                ),
            ),
            (
                PropPath::new("services.count").unwrap(),
                PropValue::from(self.services_registered.len() as u64),
            ),
            (
                PropPath::new("topics.active").unwrap(),
                PropValue::from(self.topics_active),
            ),
            (
                PropPath::new("topics.snapshot_bytes").unwrap(),
                PropValue::from(self.topics_snapshot_bytes),
            ),
        ])
    }
}

impl PropTree for NodedPropsSnapshot {
    fn snapshot(&self) -> PropValue {
        self.snapshot_value()
    }

    fn list(&self) -> Vec<PropPath> {
        all_paths()
    }

    fn describe(&self, path: &PropPath) -> Option<PropDescribe> {
        describe_path(path)
    }
}

/// All defined leaf paths for noded's L1 surface.
fn all_paths() -> Vec<PropPath> {
    [
        "config.bind",
        "config.node_name",
        "config.log_level",
        "lifecycle.started_at",
        "lifecycle.uptime_s",
        "lifecycle.health",
        "lifecycle.props_level",
        "services.registered",
        "services.count",
        "topics.active",
        "topics.snapshot_bytes",
    ]
    .into_iter()
    .map(|s| PropPath::new(s).unwrap())
    .collect()
}

fn describe_path(path: &PropPath) -> Option<PropDescribe> {
    use PropType::*;
    match path.as_str() {
        "config.bind" => Some(
            PropDescribe::leaf(
                path.clone(),
                String,
                "WireGuard interface address and port the broker binds to.",
            )
            .with_format("host:port"),
        ),
        "config.node_name" => Some(PropDescribe::leaf(
            path.clone(),
            String,
            "This node's mesh-visible name.",
        )),
        "config.log_level" => Some(PropDescribe::leaf(
            path.clone(),
            String,
            "Tracing log level (info, debug, trace, warn, error).",
        )),
        "lifecycle.started_at" => Some(
            PropDescribe::leaf(path.clone(), String, "RFC 3339 timestamp of process start.")
                .with_format("rfc3339"),
        ),
        "lifecycle.uptime_s" => Some(
            PropDescribe::leaf(path.clone(), Number, "Seconds since process start.")
                .with_transient(true),
        ),
        "lifecycle.health" => Some(PropDescribe::leaf(
            path.clone(),
            String,
            "Coarse health classification (ok | degraded | failing).",
        )),
        "lifecycle.props_level" => Some(PropDescribe::leaf(
            path.clone(),
            String,
            "SPEC 07 conformance level (L0 | L1 | L2 | L3).",
        )),
        "services.registered" => Some(PropDescribe::leaf(
            path.clone(),
            List,
            "Service names currently registered on this node.",
        )),
        "services.count" => Some(PropDescribe::leaf(
            path.clone(),
            Number,
            "Length of services.registered.",
        )),
        "topics.active" => Some(PropDescribe::leaf(
            path.clone(),
            Number,
            "Topic count currently retained or actively subscribed.",
        )),
        "topics.snapshot_bytes" => Some(PropDescribe::leaf(
            path.clone(),
            Number,
            "Total bytes of retained topic snapshots in memory.",
        )),
        _ => None,
    }
}

/// Collect a fresh snapshot from live AppState.
pub async fn collect(
    started: std::time::Instant,
    started_iso: &str,
    bind: &str,
    node_name: &str,
    log_level: &str,
    registry: &crate::noded::Registry,
    broker: &Arc<SubscriptionBroker>,
) -> NodedPropsSnapshot {
    let services_registered: Vec<String> = {
        let r = registry.read().await;
        let mut keys: Vec<String> = r.keys().cloned().collect();
        keys.sort();
        keys
    };

    let (topics_active, topics_snapshot_bytes) = broker.props_summary().await;

    NodedPropsSnapshot {
        bind: bind.to_string(),
        node_name: node_name.to_string(),
        log_level: log_level.to_string(),
        started_at: started_iso.to_string(),
        uptime_s: started.elapsed().as_secs(),
        services_registered,
        topics_active,
        topics_snapshot_bytes,
    }
}

/// Parse the optional `args` JSON header into a `serde_json::Value`.
pub fn parse_args(s: Option<&str>) -> Option<Json> {
    s.and_then(|raw| serde_json::from_str(raw).ok())
}

/// SPEC 07 §3 — props change bus.
///
/// Holds the last-emitted snapshot and per-path emit timestamps so that
/// callers can drop a fresh snapshot in and have the diff fan out as
/// `props.changed` events on `noded.props.changed`. Per-path 10 Hz cap
/// per §7.1; transient leaves (`describe().transient`) are always
/// suppressed because they would otherwise flood the topic.
///
/// A change the cap suppresses is not dropped: it arms a trailing emit
/// for that path, fired once the cap interval expires, so the last value
/// of a burst always reaches watchers.
pub struct ChangeBus {
    me: Weak<ChangeBus>,
    broker: Arc<SubscriptionBroker>,
    sink_tx: mpsc::Sender<String>,
    last: Mutex<Option<PropValue>>,
    /// Lock order: `last_emit` before `trailing`, everywhere.
    last_emit: Mutex<HashMap<String, Instant>>,
    /// Per-path trailing emit armed by a cap-suppressed change. At most
    /// one per path; later suppressed changes overwrite `new`.
    trailing: Mutex<HashMap<String, Trailer>>,
    trailer_gen: AtomicU64,
    last_world_publish: Mutex<Option<Instant>>,
    /// Pre-redacted snapshot stashed when `publish_world` is blocked by
    /// the 1 Hz cap. The drainer publishes it at the next allowed tick.
    pending_world: Mutex<Option<PropValue>>,
}

/// A deferred `props.changed` for one path. `old` is the value watchers
/// last received, `new` the latest suppressed value. `generation` ties
/// the entry to the one timer allowed to fire it.
struct Trailer {
    generation: u64,
    path: PropPath,
    old: PropValue,
    new: PropValue,
    cause: String,
}

impl ChangeBus {
    pub fn new(broker: Arc<SubscriptionBroker>) -> Arc<Self> {
        let (sink_tx, mut sink_rx) = mpsc::channel::<String>(8);
        tokio::spawn(async move { while sink_rx.recv().await.is_some() {} });
        Arc::new_cyclic(|me| Self {
            me: me.clone(),
            broker,
            sink_tx,
            last: Mutex::new(None),
            last_emit: Mutex::new(HashMap::new()),
            trailing: Mutex::new(HashMap::new()),
            trailer_gen: AtomicU64::new(0),
            last_world_publish: Mutex::new(None),
            pending_world: Mutex::new(None),
        })
    }

    /// Seed the cache without emitting events. Call once after the props
    /// surface is fully constructed but before any mutations.
    pub async fn seed(&self, snapshot: &NodedPropsSnapshot) {
        *self.last.lock().await = Some(snapshot.snapshot_value());
    }

    /// SPEC 07 §3 (L3) — publish the full redacted snapshot as a retained
    /// `world.noded` message. Capped at 1 Hz per §7.1; mutations beyond
    /// the cap stash the latest snapshot for the drainer to publish on
    /// the next allowed tick (coalescing semantics, not drop-on-overflow).
    pub async fn publish_world(&self, snapshot: &NodedPropsSnapshot) {
        let val = snapshot.redacted_snapshot();
        let now = Instant::now();
        let mut g = self.last_world_publish.lock().await;
        if let Some(prev) = *g
            && now.duration_since(prev) < WORLD_REPUBLISH_MIN
        {
            *self.pending_world.lock().await = Some(val);
            return;
        }
        *g = Some(now);
        drop(g);
        self.publish_world_value(&val).await;
    }

    /// Cap-bypassing publish for the startup seed. The first call must
    /// always succeed so a peer that subscribes before any mutation
    /// receives a real snapshot rather than nothing. Does not stamp the
    /// cap clock, so the next mutation can still publish freely.
    pub async fn publish_world_unchecked(&self, snapshot: &NodedPropsSnapshot) {
        let val = snapshot.redacted_snapshot();
        self.publish_world_value(&val).await;
    }

    /// Drainer tick: if a publish was deferred because of the cap,
    /// publish the latest pending snapshot now. Run from a 1 Hz interval
    /// task; the mutex makes the read+take atomic.
    pub async fn drain_pending(&self) {
        let val = self.pending_world.lock().await.take();
        if let Some(val) = val {
            *self.last_world_publish.lock().await = Some(Instant::now());
            self.publish_world_value(&val).await;
        }
    }

    async fn publish_world_value(&self, val: &PropValue) {
        let event = cosmix_props::publish::build_world_message("noded", val);

        match self
            .broker
            .publish(
                WORLD_NODED_TOPIC,
                &event.to_wire(),
                "noded",
                self.sink_tx.clone(),
                true,
            )
            .await
        {
            Ok((_seq, _delivered, notices)) => {
                // SPEC 12 C10b — dead-tx prune in the publish hot loop
                // may emit `topic.idle` if it drives the count to zero.
                for n in notices {
                    let _ = n.target_tx.try_send(n.wire);
                }
            }
            Err(e) => {
                tracing::warn!(error = ?e, "world.noded publish failed");
            }
        }
    }

    /// Diff `new` against the cached snapshot, emit one `props.changed`
    /// event per leaf change (subject to the 10 Hz cap), and update the
    /// cache. `cause` is logged in the event body. After all events fire,
    /// republish `world.noded` (capped at 1 Hz) so the retained snapshot
    /// stays fresh.
    pub async fn observe(&self, snapshot: &NodedPropsSnapshot, cause: &str) {
        let new_val = snapshot.snapshot_value();
        let old_val = {
            let mut g = self.last.lock().await;
            let prev = g.clone();
            *g = Some(new_val.clone());
            prev
        };
        let Some(old_val) = old_val else { return };
        let diffs = cosmix_props::diff(&old_val, &new_val);
        if diffs.is_empty() {
            return;
        }

        let now = Instant::now();
        let mut last_emit = self.last_emit.lock().await;
        let mut trailing = self.trailing.lock().await;
        for (path, old, new) in diffs {
            if snapshot
                .describe(&path)
                .map(|d| d.transient)
                .unwrap_or(false)
            {
                continue;
            }
            if let Some(prev_t) = last_emit.get(path.as_str())
                && now.duration_since(*prev_t) < CHANGE_CAP_INTERVAL
            {
                // Suppressed: arm (or refresh) the path's trailing emit
                // so the final value of the burst is not lost.
                if let Some(t) = trailing.get_mut(path.as_str()) {
                    t.new = new;
                    t.cause = cause.to_string();
                } else {
                    let generation = self.trailer_gen.fetch_add(1, Ordering::Relaxed);
                    let key = path.as_str().to_string();
                    self.arm_trailer(key.clone(), generation, *prev_t + CHANGE_CAP_INTERVAL);
                    trailing.insert(
                        key,
                        Trailer {
                            generation,
                            path,
                            old,
                            new,
                            cause: cause.to_string(),
                        },
                    );
                }
                continue;
            }
            // A normal emit supersedes any pending trailer; its `old` is
            // what watchers last saw, not the suppressed intermediate.
            let old = match trailing.remove(path.as_str()) {
                Some(t) => t.old,
                None => old,
            };
            if old == new {
                continue;
            }
            last_emit.insert(path.as_str().to_string(), now);
            self.publish_changed(&path, &old, &new, cause).await;
        }
        drop(trailing);
        drop(last_emit);

        self.publish_world(snapshot).await;
    }

    /// One-shot timer for a trailing emit. Holds only a weak ref so a
    /// dropped bus does not stay alive for a pending trailer.
    fn arm_trailer(&self, key: String, generation: u64, deadline: Instant) {
        let me = self.me.clone();
        tokio::spawn(async move {
            tokio::time::sleep_until(deadline).await;
            if let Some(bus) = me.upgrade() {
                bus.fire_trailer(&key, generation).await;
            }
        });
    }

    /// Fire the trailer for `key` if it is still the one this timer armed.
    /// A normal emit that got there first removed it; a later re-arm
    /// carries a different generation. A burst that reverted to the value
    /// watchers already hold emits nothing.
    async fn fire_trailer(&self, key: &str, generation: u64) {
        let mut last_emit = self.last_emit.lock().await;
        let mut trailing = self.trailing.lock().await;
        if trailing.get(key).map(|t| t.generation) != Some(generation) {
            return;
        }
        let Some(t) = trailing.remove(key) else { return };
        drop(trailing);
        if t.old == t.new {
            return;
        }
        last_emit.insert(key.to_string(), Instant::now());
        self.publish_changed(&t.path, &t.old, &t.new, &t.cause).await;
    }

    async fn publish_changed(
        &self,
        path: &PropPath,
        old: &PropValue,
        new: &PropValue,
        cause: &str,
    ) {
        let event = cosmix_props::publish::build_props_changed_message(path, old, new, cause);

        match self
            .broker
            .publish(
                PROPS_CHANGED_TOPIC,
                &event.to_wire(),
                "noded",
                self.sink_tx.clone(),
                false,
            )
            .await
        {
            Ok((_seq, _delivered, notices)) => {
                for n in notices {
                    let _ = n.target_tx.try_send(n.wire);
                }
            }
            Err(e) => {
                tracing::warn!(path = %path, error = ?e, "props.changed publish failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_bus::bus;

    fn snap(log_level: &str) -> NodedPropsSnapshot {
        NodedPropsSnapshot {
            bind: "127.0.0.1:0".to_string(),
            node_name: "alpha".to_string(),
            log_level: log_level.to_string(),
            started_at: "2026-01-01T00:00:00Z".to_string(),
            uptime_s: 0,
            services_registered: Vec::new(),
            topics_active: 0,
            topics_snapshot_bytes: 0,
        }
    }

    /// Bus seeded at `info`, with a watcher on `noded.props.changed`.
    async fn setup() -> (Arc<ChangeBus>, mpsc::Receiver<String>) {
        let broker = Arc::new(SubscriptionBroker::new());
        let (tx, rx) = mpsc::channel(64);
        broker
            .subscribe_topic(PROPS_CHANGED_TOPIC, "watcher", tx)
            .await;
        let bus = ChangeBus::new(broker);
        bus.seed(&snap("info")).await;
        (bus, rx)
    }

    /// `(path, old, new)` of the next event, or `None` if nothing arrives
    /// within 5 s of (paused, auto-advancing) time.
    async fn next_event(rx: &mut mpsc::Receiver<String>) -> Option<(String, Json, Json)> {
        let wire = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .ok()??;
        let msg = bus::parse(&wire).unwrap();
        let body: Json = serde_json::from_str(&msg.body).unwrap();
        Some((
            body["path"].as_str().unwrap().to_string(),
            body["old"].clone(),
            body["new"].clone(),
        ))
    }

    fn ev(old: &str, new: &str) -> Option<(String, Json, Json)> {
        Some(("config.log_level".to_string(), Json::from(old), Json::from(new)))
    }

    #[tokio::test(start_paused = true)]
    async fn burst_inside_cap_ends_with_final_value() {
        let (bus, mut rx) = setup().await;
        bus.observe(&snap("debug"), "t").await;
        assert_eq!(next_event(&mut rx).await, ev("info", "debug"));

        tokio::time::advance(Duration::from_millis(30)).await;
        bus.observe(&snap("warn"), "t").await;
        tokio::time::advance(Duration::from_millis(30)).await;
        bus.observe(&snap("error"), "t").await;
        assert!(rx.try_recv().is_err(), "suppressed changes must not emit early");

        let t0 = Instant::now();
        assert_eq!(next_event(&mut rx).await, ev("debug", "error"));
        assert!(t0.elapsed() <= CHANGE_CAP_INTERVAL);
        assert_eq!(next_event(&mut rx).await, None);
    }

    #[tokio::test(start_paused = true)]
    async fn normal_emit_beats_trailer_without_duplicate() {
        let (bus, mut rx) = setup().await;
        bus.observe(&snap("debug"), "t").await;
        assert_eq!(next_event(&mut rx).await, ev("info", "debug"));

        tokio::time::advance(Duration::from_millis(30)).await;
        bus.observe(&snap("warn"), "t").await;
        assert!(bus.trailing.lock().await.contains_key("config.log_level"));

        // Race: the cap window has lapsed for `observe` but the trailer's
        // timer has not run yet. Backdate the stamp rather than advance
        // the clock, which would let the timer win.
        bus.last_emit.lock().await.insert(
            "config.log_level".to_string(),
            Instant::now() - Duration::from_millis(200),
        );
        bus.observe(&snap("error"), "t").await;
        assert_eq!(next_event(&mut rx).await, ev("debug", "error"));
        assert_eq!(next_event(&mut rx).await, None);
    }

    #[tokio::test(start_paused = true)]
    async fn burst_that_reverts_emits_nothing() {
        let (bus, mut rx) = setup().await;
        bus.observe(&snap("debug"), "t").await;
        assert_eq!(next_event(&mut rx).await, ev("info", "debug"));

        tokio::time::advance(Duration::from_millis(30)).await;
        bus.observe(&snap("warn"), "t").await;
        tokio::time::advance(Duration::from_millis(30)).await;
        bus.observe(&snap("debug"), "t").await;
        assert_eq!(next_event(&mut rx).await, None);
        assert!(bus.trailing.lock().await.is_empty());
    }
}
