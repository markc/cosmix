//! SPEC 12 §5.5 / §10 — **publisher half** of the live fan-out for
//! `<svc>.props.records.changed` and `<svc>.props.audit`.
//!
//! The substrate's `Runtime` already commits a durable `RecordEvent`
//! row per state transition (set, delete, Saga complete). The replay
//! verbs `<svc>.props.watch` and `<svc>.props.audit.watch` synthesise
//! events from that durable history for subscribers that supply a
//! `since_nseq`. This module adds a per-namespace task that tails
//! newly-committed events and publishes them on the two reserved
//! topics so the wire bodies are byte-identical to what the replay
//! projection would emit for the same event.
//!
//! ## Scope: publisher half of a now-complete bridge
//!
//! This module is the *publish* side. SPEC 12 §15.5 forbids direct
//! `topic.subscribe` on these reserved topics
//! (`cosmix_noded::props_reservation::may_subscribe` returns false
//! even for the owning service), so end-to-end "join live" routes
//! through the watch verbs: `<svc>.props.watch` and
//! `<svc>.props.audit.watch` (C4/C5) call the namespace's
//! [`crate::subscribe_granter::SubscribeGranter`] (C10c) to authorize
//! the per-topic subscription first, then snapshot the post-grant
//! `observed_nseq` and return the bounded replay tail. The
//! grant-then-observed order is load-bearing: any event with
//! `nseq > observed` is on the live side of the replay/live split
//! (eligible for live delivery through the broker subscription,
//! best-effort per "Publish failure handling" below); any event with
//! `nseq ≤ observed` is in the replay. In production the granter is
//! `NodedSubscribeGranter` (C10d), which routes the grant through
//! `noded.props.subscribe_grant`. Replay shape and live-publish shape
//! share the same projection helpers (`project_records_changed` /
//! `project_audit_row` in [`crate::bus::mutation`]), so a subscriber
//! resuming from a stale `since_nseq` cannot distinguish events
//! delivered live from events delivered via replay.
//!
//! ## Why a separate task per namespace
//!
//! Each namespace has its own monotonic `nseq` cursor. The runtime's
//! [`Runtime::events_signal`] is a per-runtime
//! [`tokio::sync::Notify`] that fires `notify_one` on every successful
//! commit. Keeping the cursor in the spawned task — rather than in a
//! shared map — means no cross-namespace lock contention and no
//! shared-state reasoning: the task owns its cursor for its entire
//! lifetime.
//!
//! ## Initial cursor
//!
//! The dispatcher's initial cursor is the namespace's current
//! `observed_nseq` at task startup. **Historical events stay on the
//! watch verbs' replay slice.** A subscriber that wants pre-dispatcher
//! state uses `<svc>.props.{watch,audit.watch}` with `since_nseq: 0`;
//! the live topic carries events that occur after the dispatcher
//! subscribes to its runtime. This matches SPEC 12's general "live is
//! best-effort, replay is authoritative" stance — see §5.5 on the
//! records.changed contract.
//!
//! ## Publish failure handling
//!
//! Publish errors (broker down, RPC timeout) are logged at `warn` and
//! the cursor still advances. Re-publishing on next signal would
//! either duplicate live events on the wire (bad — subscribers cannot
//! deduplicate without per-topic seen-set state, which the §5.5 wire
//! shape does not require) or starve the dispatcher (bad — the
//! `events_signal` Notify only stores one permit). The §11
//! reconciliation path + replay verbs are SPEC 12's recovery story
//! for missed-window subscribers; live is intentionally best-effort.
//!
//! ## `retain: false`
//!
//! Both topics carry per-event deltas, not snapshots. A late
//! subscriber that lands a retained "last event" from the broker
//! would see arbitrary noise; their entry point is the replay verb.
//! Matches the `maild.verdict` discipline.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::bus::mutation::{project_audit_row, project_records_changed};
use crate::record::Nseq;
use crate::runtime::Runtime;
use crate::store::{StoreError, StoreResult};

/// Transport-agnostic publish surface. Implementers wrap the cosmix
/// broker (production: `cosmix_client::NodedClient::send_with_headers`
/// against `noded`'s `topic.publish` RPC — not an intra-doc link
/// because this crate does not depend on cosmix-lib-client) or an
/// in-process recorder
/// (tests: collect topic + body pairs for assertion).
///
/// `Send + Sync` + boxed-future is the same shape as
/// [`crate::store::PropertyStore`], so callers can hold an
/// `Arc<dyn DispatchPublisher>` and pass it across the spawned task
/// boundary without static-type coupling.
pub trait DispatchPublisher: Send + Sync {
    fn publish<'a>(
        &'a self,
        topic: &'a str,
        body: String,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;
}

/// `<svc>.props.records.changed` — the §5.5 records-changed topic.
pub fn records_changed_topic(service: &str) -> String {
    format!("{service}.props.records.changed")
}

/// `<svc>.props.audit` — the §10 audit topic.
pub fn audit_topic(service: &str) -> String {
    format!("{service}.props.audit")
}

/// Handle returned by [`spawn_dispatcher`]. Holding it keeps the task
/// alive; dropping it does NOT cancel the task — call
/// [`DispatcherHandle::shutdown`] for graceful termination. The handle
/// is `!Clone` on purpose: only one shutter per dispatcher.
pub struct DispatcherHandle {
    shutdown: Arc<Notify>,
    join: JoinHandle<()>,
}

impl DispatcherHandle {
    /// Signal the dispatcher to stop and await its completion. After
    /// this returns the task has drained its current iteration and
    /// will not publish further events for this namespace.
    pub async fn shutdown(self) {
        self.shutdown.notify_one();
        // The task may already have exited (panic, runtime drop); a
        // join error is non-fatal for shutdown.
        let _ = self.join.await;
    }

    /// Test-helper: assert the underlying task has not yet exited.
    #[cfg(test)]
    pub fn is_running(&self) -> bool {
        !self.join.is_finished()
    }
}

/// Spawn a per-namespace fan-out task. One [`Runtime`] gets one
/// dispatcher; the caller-supplied publisher is shared across all
/// namespaces of a service (via `Arc::clone`).
///
/// `async` because the dispatcher's initial cursor MUST be captured
/// *before* the function returns — otherwise a commit that races the
/// spawn could advance `observed_nseq` past the cursor, and the
/// dispatcher would silently skip the event it was created to
/// publish. The caller awaits this once at startup; the returned
/// handle is the long-lived control surface.
///
/// Returns the underlying [`StoreError`] when the initial
/// `observed_nseq` snapshot cannot be read. Defaulting the cursor to
/// zero would silently violate the "pre-subscription history stays on
/// replay verbs" contract — a later signal would then publish every
/// historical event onto the live topic, duplicating rows for
/// subscribers that already replayed. Callers (typically the daemon's
/// startup wiring) decide whether to log-and-skip this namespace, fail
/// the whole startup, or retry; the dispatcher refuses to invent a
/// safe-looking default.
pub async fn spawn_dispatcher(
    runtime: Arc<Runtime>,
    publisher: Arc<dyn DispatchPublisher>,
) -> StoreResult<DispatcherHandle> {
    // Pre-spawn cursor capture: the namespace's current observed_nseq
    // *at the moment of subscription*. Pre-existing history stays on
    // the replay verbs (see module docs); live events are anything
    // with `nseq > cursor`.
    let cursor = runtime
        .store()
        .list(&runtime.spec().name)
        .await?
        .observed_nseq;

    let shutdown = Arc::new(Notify::new());
    let shutdown_inner = Arc::clone(&shutdown);
    let join = tokio::spawn(async move {
        run_dispatcher(runtime, publisher, shutdown_inner, cursor).await;
    });
    Ok(DispatcherHandle { shutdown, join })
}

async fn run_dispatcher(
    runtime: Arc<Runtime>,
    publisher: Arc<dyn DispatchPublisher>,
    shutdown: Arc<Notify>,
    initial_cursor: Nseq,
) {
    let signal = runtime.events_signal();
    let service = runtime.service().to_string();
    let spec = runtime.spec().clone();
    let ns = spec.name.clone();
    let store = Arc::clone(runtime.store());

    let records_topic = records_changed_topic(&service);
    let audit_topic_name = audit_topic(&service);
    let mut cursor = initial_cursor;

    loop {
        tokio::select! {
            biased;
            // Shutdown wins over a coincident commit so caller-driven
            // termination is deterministic even under high write
            // pressure.
            _ = shutdown.notified() => {
                tracing::debug!(
                    service = %service,
                    namespace = %ns.as_str(),
                    "dispatcher shutting down",
                );
                return;
            }
            _ = signal.notified() => {}
        }

        // Drain in a single inner loop so a burst of commits between
        // two wake-ups is processed in one pass. `events_since` is a
        // tx-bounded read; we exit the inner loop when it returns
        // empty.
        loop {
            let events = match store.events_since(&ns, cursor).await {
                Ok(rows) => rows,
                Err(StoreError::ReplayWindowExceeded {
                    since_nseq,
                    oldest_nseq,
                }) => {
                    // The dispatcher should never lag the namespace's
                    // replay window in normal operation (it wakes on
                    // every commit). If it does, log loudly and reset
                    // to the current snapshot — better to lose the
                    // gap on the live topic than to spin forever.
                    // Subscribers can catch up via the replay verbs.
                    tracing::error!(
                        service = %service,
                        namespace = %ns.as_str(),
                        cursor = since_nseq.0,
                        oldest = oldest_nseq.0,
                        "dispatcher cursor aged out of replay window; events lost on live topic — \
                         subscribers must recover via <svc>.props.watch / .audit.watch",
                    );
                    cursor = match store.list(&ns).await {
                        Ok(snap) => snap.observed_nseq,
                        Err(_) => {
                            // Leave the cursor untouched; next signal
                            // retries. Avoids infinite-loop-on-broken-
                            // store while preserving forward progress
                            // once IO recovers.
                            break;
                        }
                    };
                    break;
                }
                Err(e) => {
                    tracing::warn!(
                        service = %service,
                        namespace = %ns.as_str(),
                        error = %e,
                        "events_since failed; will retry on next signal",
                    );
                    break;
                }
            };

            if events.is_empty() {
                break;
            }

            for event in &events {
                let rc_body = project_records_changed(&service, &spec.schema, event).to_string();
                if let Err(e) = publisher.publish(&records_topic, rc_body).await {
                    tracing::warn!(
                        service = %service,
                        namespace = %ns.as_str(),
                        nseq = event.nseq.0,
                        error = %e,
                        "records.changed publish failed; advancing cursor anyway \
                         (replay verb is the recovery path)",
                    );
                }
                let audit_body = project_audit_row(&service, &spec.schema, event).to_string();
                if let Err(e) = publisher.publish(&audit_topic_name, audit_body).await {
                    tracing::warn!(
                        service = %service,
                        namespace = %ns.as_str(),
                        nseq = event.nseq.0,
                        error = %e,
                        "audit publish failed; advancing cursor anyway \
                         (audit.watch verb is the recovery path)",
                    );
                }
                cursor = event.nseq;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryStore;
    use crate::namespace::{
        Cardinality, NamespaceName, NamespaceSpec, PropertySchema, StorageBackendKind,
    };
    use crate::record::{Actor, RecordKey, Version};
    use crate::runtime::SetOpts;
    use crate::store::{MergeMode, PropertyStore};
    use cosmix_props_core::value::PropValue;
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::time::Duration;

    type EventLog = Arc<Mutex<Vec<(String, String)>>>;

    /// Recorder publisher. Stores every (topic, body) pair in order.
    struct RecPublisher {
        events: EventLog,
    }

    impl RecPublisher {
        fn new() -> (Arc<Self>, EventLog) {
            let events: EventLog = Arc::new(Mutex::new(Vec::new()));
            (
                Arc::new(Self {
                    events: Arc::clone(&events),
                }),
                events,
            )
        }
    }

    impl DispatchPublisher for RecPublisher {
        fn publish<'a>(
            &'a self,
            topic: &'a str,
            body: String,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
            let events = Arc::clone(&self.events);
            let topic = topic.to_string();
            Box::pin(async move {
                events.lock().unwrap().push((topic, body));
                Ok(())
            })
        }
    }

    fn ns_name() -> NamespaceName {
        NamespaceName::new("widgets").unwrap()
    }

    fn ns_spec() -> NamespaceSpec {
        NamespaceSpec::new(
            ns_name(),
            PropertySchema::default(),
            Cardinality::Collection {
                primary_key_field: "label".to_string(),
            },
            StorageBackendKind::Memory,
        )
    }

    fn obj(pairs: &[(&str, PropValue)]) -> PropValue {
        let mut m = BTreeMap::new();
        for (k, v) in pairs {
            m.insert((*k).to_string(), v.clone());
        }
        PropValue::Object(m)
    }

    fn opts(ts_ms: i64) -> SetOpts {
        SetOpts {
            expected_version: Some(Version::zero()),
            merge: MergeMode::Patch,
            actor: Actor::operator("test").expect("valid actor"),
            cause: None,
            ts_ms,
        }
    }

    async fn wait_for_events(
        events: &Arc<Mutex<Vec<(String, String)>>>,
        n: usize,
        max_iters: usize,
    ) -> Vec<(String, String)> {
        for _ in 0..max_iters {
            if events.lock().unwrap().len() >= n {
                return events.lock().unwrap().clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        events.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn dispatcher_publishes_records_changed_and_audit_for_each_commit() {
        let store: Arc<dyn PropertyStore> = Arc::new(MemoryStore::new("maild"));
        let runtime = Arc::new(Runtime::new("maild", ns_spec(), Arc::clone(&store)));
        let (pub_arc, events) = RecPublisher::new();
        let handle = spawn_dispatcher(Arc::clone(&runtime), pub_arc)
            .await
            .unwrap();

        let key = RecordKey::collection(ns_name(), "alpha");
        runtime
            .set(key, obj(&[("label", PropValue::from("alpha"))]), opts(0))
            .await
            .unwrap();

        let collected = wait_for_events(&events, 2, 100).await;
        assert_eq!(collected.len(), 2, "expected records.changed + audit");

        let records_topic = records_changed_topic("maild");
        let audit_topic_name = audit_topic("maild");
        assert_eq!(collected[0].0, records_topic);
        assert_eq!(collected[1].0, audit_topic_name);

        // §5.5 — records.changed body MUST NOT contain audit_digest.
        let rc: serde_json::Value = serde_json::from_str(&collected[0].1).unwrap();
        assert!(
            rc.get("audit_digest").is_none(),
            "records.changed body leaked audit_digest: {}",
            collected[0].1,
        );
        // §10 — audit body MUST contain audit_digest.
        let ar: serde_json::Value = serde_json::from_str(&collected[1].1).unwrap();
        assert!(
            ar.get("audit_digest").is_some(),
            "audit body missing audit_digest: {}",
            collected[1].1,
        );

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn dispatcher_live_body_matches_replay_body_byte_for_byte() {
        // The fan-out contract: subscribers cannot distinguish live
        // events from replay events. Drive a set, capture the live
        // publish, then re-project the same event with the same
        // helpers — bytes MUST match the live wire output.
        let store: Arc<dyn PropertyStore> = Arc::new(MemoryStore::new("maild"));
        let spec = ns_spec();
        let runtime = Arc::new(Runtime::new("maild", spec.clone(), Arc::clone(&store)));
        let (pub_arc, events) = RecPublisher::new();
        let handle = spawn_dispatcher(Arc::clone(&runtime), pub_arc)
            .await
            .unwrap();

        let key = RecordKey::collection(ns_name(), "beta");
        let outcome = runtime
            .set(key, obj(&[("label", PropValue::from("beta"))]), opts(0))
            .await
            .unwrap();

        let collected = wait_for_events(&events, 2, 100).await;
        assert_eq!(collected.len(), 2);

        let replay_rc =
            project_records_changed("maild", &spec.schema, &outcome.set_event).to_string();
        let replay_audit = project_audit_row("maild", &spec.schema, &outcome.set_event).to_string();

        assert_eq!(
            collected[0].1, replay_rc,
            "live records.changed body diverges from replay projection",
        );
        assert_eq!(
            collected[1].1, replay_audit,
            "live audit body diverges from replay projection",
        );

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn dispatcher_preserves_nseq_order_under_burst() {
        // Coalescing must not reorder. Fire several sets back-to-back
        // and assert published records.changed bodies appear in
        // monotonically increasing nseq order.
        let store: Arc<dyn PropertyStore> = Arc::new(MemoryStore::new("maild"));
        let runtime = Arc::new(Runtime::new("maild", ns_spec(), Arc::clone(&store)));
        let (pub_arc, events) = RecPublisher::new();
        let handle = spawn_dispatcher(Arc::clone(&runtime), pub_arc)
            .await
            .unwrap();

        for i in 0..5_u32 {
            let key = RecordKey::collection(ns_name(), format!("k{i}"));
            let label = format!("k{i}");
            runtime
                .set(
                    key,
                    obj(&[("label", PropValue::from(label))]),
                    opts(i as i64),
                )
                .await
                .unwrap();
        }

        let collected = wait_for_events(&events, 10, 200).await;
        assert_eq!(
            collected.len(),
            10,
            "expected 5 records.changed + 5 audit, got {}",
            collected.len(),
        );

        let records_topic = records_changed_topic("maild");
        let nseqs: Vec<u64> = collected
            .iter()
            .filter(|(t, _)| t == &records_topic)
            .map(|(_, b)| {
                let v: serde_json::Value = serde_json::from_str(b).unwrap();
                v["nseq"].as_u64().unwrap()
            })
            .collect();
        let mut sorted = nseqs.clone();
        sorted.sort();
        assert_eq!(
            nseqs, sorted,
            "records.changed nseqs out of order: {nseqs:?}"
        );

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn dispatcher_skips_pre_subscription_history() {
        // Initial cursor = current observed_nseq. A set committed
        // BEFORE the dispatcher spawns must not appear on the live
        // topic; subscribers wanting it use the replay verb.
        let store: Arc<dyn PropertyStore> = Arc::new(MemoryStore::new("maild"));
        let runtime = Arc::new(Runtime::new("maild", ns_spec(), Arc::clone(&store)));

        // Commit BEFORE spawning the dispatcher.
        runtime
            .set(
                RecordKey::collection(ns_name(), "gamma"),
                obj(&[("label", PropValue::from("gamma"))]),
                opts(0),
            )
            .await
            .unwrap();

        let (pub_arc, events) = RecPublisher::new();
        let handle = spawn_dispatcher(Arc::clone(&runtime), pub_arc)
            .await
            .unwrap();

        // Yield a few times to give the dispatcher a chance to publish
        // the historical row if it were going to (it must not).
        for _ in 0..5 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            events.lock().unwrap().is_empty(),
            "dispatcher published pre-subscription history: {:?}",
            events.lock().unwrap(),
        );

        // Now a fresh commit — must appear.
        runtime
            .set(
                RecordKey::collection(ns_name(), "delta"),
                obj(&[("label", PropValue::from("delta"))]),
                opts(1),
            )
            .await
            .unwrap();

        let collected = wait_for_events(&events, 2, 100).await;
        assert_eq!(collected.len(), 2);
        let rc: serde_json::Value = serde_json::from_str(&collected[0].1).unwrap();
        assert_eq!(rc["key"], "delta");

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn dispatcher_shutdown_stops_publish() {
        let store: Arc<dyn PropertyStore> = Arc::new(MemoryStore::new("maild"));
        let runtime = Arc::new(Runtime::new("maild", ns_spec(), Arc::clone(&store)));
        let (pub_arc, events) = RecPublisher::new();
        let handle = spawn_dispatcher(Arc::clone(&runtime), pub_arc)
            .await
            .unwrap();

        handle.shutdown().await;

        runtime
            .set(
                RecordKey::collection(ns_name(), "epsilon"),
                obj(&[("label", PropValue::from("epsilon"))]),
                opts(0),
            )
            .await
            .unwrap();

        // Give the (already-shutdown) task time to NOT publish.
        for _ in 0..5 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            events.lock().unwrap().is_empty(),
            "dispatcher kept publishing after shutdown: {:?}",
            events.lock().unwrap(),
        );
    }

    #[test]
    fn topic_names_match_spec_15_5_reservation_form() {
        // Broker reservation matches by suffix .props.records.changed /
        // .props.audit (cosmix-noded::props_reservation). Pin the topic
        // strings here so a stray rename can't desynchronise the
        // publisher from the reserved topic name.
        assert_eq!(
            records_changed_topic("maild"),
            "maild.props.records.changed"
        );
        assert_eq!(audit_topic("maild"), "maild.props.audit");
    }
}
