//! Subscriber loop driving [`crate::cache::TrustGrantsCache`] from a
//! [`crate::wgd_records::WgdClient`] (plan §3.1 steps 1–4).
//!
//! One tokio task per wgd namespace (`trusted_meshes`, `grants`,
//! `cross_mesh_exposable`). Each task:
//!
//! 1. `props.list` → capture records + `nseq` high-water mark.
//! 2. Rebuild this namespace's slice of [`crate::cache::Snapshot`]
//!    from the listing (atomic per-namespace replace).
//! 3. Signal first-seed completion; once all three namespaces have
//!    signalled the cache flips to ready.
//! 4. `props.watch since=<nseq>` → apply each `Upsert` / `Delete` to
//!    the namespace's slice.
//! 5. On stream end / per-event error / list failure → exponential
//!    backoff, then back to step 1. Reseed is a full re-list because
//!    a disconnect-window gap in the change stream can't be bridged
//!    otherwise (plan §3.1 step 4).
//!
//! The subscriber holds a flat `BTreeMap<key, Record>` per namespace
//! locally; the `Snapshot` is *derived* from that on each apply.
//! Deletes carry only `RecordKey`, and `Grant.mesh_fqdn` /
//! `cross_mesh_exposable.cap_namespace_prefix` live inside the record
//! body — so without a key→bucket index, a delete event has nothing
//! to point at. Deriving from the flat map sidesteps the
//! bookkeeping entirely.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use cosmix_props::{Record, RecordKey};
use futures_util::StreamExt;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::warn;

use crate::cache::{Snapshot, TrustGrantsCache};
use crate::caps::{CapabilitySet, Grant, TrustedMesh};
use crate::wgd_records::{
    WatchEvent, WgdClient, parse_exposable_entry, parse_grant, parse_trusted_mesh,
};

/// Fully-qualified `props.list` / `props.watch` namespace names. Kept
/// public so callers can wire matching SPEC-12 `NamespaceSpec`s into
/// wgd without re-typing the strings (plan §§4, 5, 9).
pub const NS_TRUSTED_MESHES: &str = "wgd.trusted_meshes";
pub const NS_GRANTS: &str = "wgd.grants";
pub const NS_CROSS_MESH_EXPOSABLE: &str = "wgd.cross_mesh_exposable";

/// All namespaces the subscriber follows, in plan §3.1 order.
pub const NAMESPACES: [&str; 3] = [NS_TRUSTED_MESHES, NS_GRANTS, NS_CROSS_MESH_EXPOSABLE];

const INITIAL_BACKOFF: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, Copy)]
enum Kind {
    Trusted,
    Grants,
    Exposable,
}

impl Kind {
    fn namespace(self) -> &'static str {
        match self {
            Self::Trusted => NS_TRUSTED_MESHES,
            Self::Grants => NS_GRANTS,
            Self::Exposable => NS_CROSS_MESH_EXPOSABLE,
        }
    }
}

/// Readiness handle returned by [`TrustGrantsCache::start`]. Flips to
/// `true` once every namespace has completed its first `props.list`
/// (plan §3.1 step 3).
///
/// Consumers gate request admission on `wait_ready` — plan §3.1 step
/// 4: the HTTPS listener replies 503 while not-ready, so the cache
/// can never serve an empty snapshot as if it were authoritative.
#[derive(Debug, Clone)]
pub struct Ready {
    rx: watch::Receiver<bool>,
}

impl Ready {
    pub fn is_ready(&self) -> bool {
        *self.rx.borrow()
    }

    /// Resolves once the cache is ready. Cheap to call repeatedly.
    pub async fn wait_ready(&mut self) {
        if *self.rx.borrow_and_update() {
            return;
        }
        loop {
            if self.rx.changed().await.is_err() {
                // sender dropped — the subscriber tasks have all
                // exited; nothing more to wait for.
                return;
            }
            if *self.rx.borrow_and_update() {
                return;
            }
        }
    }
}

/// Owns the three background subscriber tasks. Dropping it aborts
/// them; production callers normally hold it for the verifier's
/// lifetime.
#[derive(Debug)]
pub struct SubscriberHandle {
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for SubscriberHandle {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl TrustGrantsCache {
    /// Start the subscriber tasks against `client` and return the
    /// cache, a readiness handle, and an owner for the background
    /// tasks. Plan §3.1 entry point.
    pub fn start(client: Arc<dyn WgdClient>) -> (Self, Ready, SubscriberHandle) {
        let cache = Self::for_testing(Snapshot::default());
        let (tx, rx) = watch::channel(false);
        let pending = Arc::new(AtomicU32::new(3));
        let tx = Arc::new(tx);

        let mut tasks = Vec::with_capacity(3);
        for kind in [Kind::Trusted, Kind::Grants, Kind::Exposable] {
            let cache_c = cache.clone();
            let client_c = Arc::clone(&client);
            let pending_c = Arc::clone(&pending);
            let tx_c = Arc::clone(&tx);
            tasks.push(tokio::spawn(async move {
                run_namespace(kind, cache_c, client_c, pending_c, tx_c).await;
            }));
        }
        (cache, Ready { rx }, SubscriberHandle { tasks })
    }
}

/// One namespace's worth of local state: the flat record set this
/// task has observed. The `Snapshot` slice owned by this namespace is
/// derived from `records` on every apply.
#[derive(Default)]
struct NsState {
    /// Keyed by `RecordKey.key` (PK string).
    records: BTreeMap<String, Record>,
}

impl NsState {
    fn replace_from(&mut self, records: Vec<Record>) {
        self.records.clear();
        for r in records {
            self.records.insert(r.key.key.clone(), r);
        }
    }

    fn upsert(&mut self, r: Record) {
        self.records.insert(r.key.key.clone(), r);
    }

    fn delete(&mut self, k: &RecordKey) {
        self.records.remove(&k.key);
    }
}

async fn run_namespace(
    kind: Kind,
    cache: TrustGrantsCache,
    client: Arc<dyn WgdClient>,
    pending: Arc<AtomicU32>,
    ready_tx: Arc<watch::Sender<bool>>,
) {
    let ns = kind.namespace();
    let mut state = NsState::default();
    let mut first_seeded = false;
    let mut backoff = INITIAL_BACKOFF;

    loop {
        match client.list_records(ns).await {
            Ok(listing) => {
                state.replace_from(listing.records);
                apply_namespace_slice(&cache, kind, &state);
                backoff = INITIAL_BACKOFF;

                match client.watch_records(ns, listing.nseq).await {
                    Ok(mut stream) => {
                        // Plan §3.1 step 3 ready requires three legs:
                        // `list`, `watch established`, and
                        // `replay-caught-up`. The first two are now
                        // satisfied; the third lands when the stream
                        // yields `WatchEvent::CaughtUp` below. Holding
                        // the ready countdown until then avoids the
                        // window where replay events are in flight
                        // but the snapshot does not yet reflect them.
                        //
                        // `marker_nseq` tracks the boundary seen on
                        // *this* watch only — per SPEC-12 §5.5
                        // v0.2.1 a same-`nseq` redelivery on the
                        // same watch is ignored, a different-`nseq`
                        // marker is a protocol violation that
                        // triggers a reseed. `first_seeded` is
                        // task-lifetime and only flips the global
                        // readiness countdown once.
                        let mut marker_nseq: Option<cosmix_props::Nseq> = None;
                        while let Some(event) = stream.next().await {
                            match event {
                                Ok(WatchEvent::Upsert(r)) => {
                                    state.upsert(r);
                                    apply_namespace_slice(&cache, kind, &state);
                                }
                                Ok(WatchEvent::Delete(k)) => {
                                    state.delete(&k);
                                    apply_namespace_slice(&cache, kind, &state);
                                }
                                Ok(WatchEvent::CaughtUp { nseq }) => {
                                    match marker_nseq {
                                        Some(prev) if prev.0 == nseq.0 => {
                                            // Redelivery on the same
                                            // watch (§5.5 v0.2.1): ignore.
                                        }
                                        Some(prev) => {
                                            warn!(
                                                namespace = ns,
                                                prev_nseq = prev.0,
                                                new_nseq = nseq.0,
                                                "wgd watch emitted second caught_up with different nseq on same watch; reseeding (protocol violation)"
                                            );
                                            break;
                                        }
                                        None => {
                                            marker_nseq = Some(nseq);
                                            if !first_seeded {
                                                first_seeded = true;
                                                if pending.fetch_sub(1, Ordering::AcqRel) == 1 {
                                                    let _ = ready_tx.send(true);
                                                }
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!(
                                        namespace = ns,
                                        error = %e,
                                        "wgd watch error; reseeding"
                                    );
                                    break;
                                }
                            }
                        }
                        warn!(namespace = ns, "wgd watch stream ended; reseeding");
                    }
                    Err(e) => {
                        warn!(
                            namespace = ns,
                            error = %e,
                            "wgd watch establish failed; reseeding"
                        );
                    }
                }
            }
            Err(e) => {
                warn!(
                    namespace = ns,
                    error = %e,
                    "wgd list failed; will retry"
                );
            }
        }

        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Re-derive this namespace's slice of the shared [`Snapshot`] from
/// the task-local flat record map. Other slices are left untouched —
/// each task only writes its own slice, so per-namespace mutations
/// never race.
fn apply_namespace_slice(cache: &TrustGrantsCache, kind: Kind, state: &NsState) {
    cache.mutate(|snap| match kind {
        Kind::Trusted => derive_trusted(state, &mut snap.trusted),
        Kind::Grants => derive_grants(state, &mut snap.grants_by_mesh),
        Kind::Exposable => derive_exposable(state, &mut snap.exposable),
    });
}

fn derive_trusted(state: &NsState, out: &mut std::collections::HashMap<String, TrustedMesh>) {
    out.clear();
    for record in state.records.values() {
        match parse_trusted_mesh(record) {
            Ok(t) => {
                out.insert(t.mesh_fqdn.clone(), t);
            }
            Err(e) => warn!(
                namespace = NS_TRUSTED_MESHES,
                key = %record.key.key,
                error = %e,
                "skipping malformed trusted_mesh record"
            ),
        }
    }
}

fn derive_grants(state: &NsState, out: &mut std::collections::HashMap<String, Vec<Grant>>) {
    out.clear();
    for record in state.records.values() {
        match parse_grant(record) {
            Ok(g) => out.entry(g.mesh_fqdn.clone()).or_default().push(g),
            Err(e) => warn!(
                namespace = NS_GRANTS,
                key = %record.key.key,
                error = %e,
                "skipping malformed grant record"
            ),
        }
    }
}

fn derive_exposable(state: &NsState, out: &mut std::collections::HashMap<String, CapabilitySet>) {
    out.clear();
    for record in state.records.values() {
        match parse_exposable_entry(record) {
            Ok((ns_id, cap)) => {
                out.entry(ns_id).or_default().insert(cap);
            }
            Err(e) => warn!(
                namespace = NS_CROSS_MESH_EXPOSABLE,
                key = %record.key.key,
                error = %e,
                "skipping malformed cross_mesh_exposable record"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wgd_records::{Listing, WatchStream, WgdError};
    use async_trait::async_trait;
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use chrono::{DateTime, TimeZone, Utc};
    use cosmix_props::{NamespaceName, Nseq, PropValue, Version};
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::Mutex;
    use std::time::Duration;
    use tokio::sync::broadcast;
    use tokio::time::timeout;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap()
    }

    fn rfc3339(t: DateTime<Utc>) -> String {
        t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }

    fn rk(ns: &str, key: &str) -> RecordKey {
        RecordKey::collection(NamespaceName::new(ns).unwrap(), key)
    }

    fn obj(pairs: &[(&str, PropValue)]) -> PropValue {
        let mut m = BTreeMap::new();
        for (k, v) in pairs {
            m.insert((*k).to_owned(), v.clone());
        }
        PropValue::Object(m)
    }

    fn trusted_record(fqdn: &str, nseq: u64) -> Record {
        Record {
            key: rk("wgd.trusted_meshes", fqdn),
            value: obj(&[
                ("mesh_fqdn", PropValue::String(fqdn.into())),
                (
                    "current_pubkey",
                    PropValue::String(STANDARD.encode([0u8; 32])),
                ),
                ("current_selector", PropValue::String("2026q3".into())),
                ("last_verified_at", PropValue::String(rfc3339(now()))),
                ("max_pubkey_age_secs", PropValue::UInt(7 * 86400)),
                ("enabled", PropValue::Bool(true)),
            ]),
            version: Version(1),
            nseq: Nseq(nseq),
            lifecycle: None,
        }
    }

    fn grant_record(grant_id: &str, mesh: &str, cap: &str, nseq: u64) -> Record {
        Record {
            key: rk("wgd.grants", grant_id),
            value: obj(&[
                ("grant_id", PropValue::String(grant_id.into())),
                ("mesh_fqdn", PropValue::String(mesh.into())),
                (
                    "capabilities",
                    PropValue::List(vec![PropValue::String(cap.into())]),
                ),
                (
                    "valid_from",
                    PropValue::String(rfc3339(now() - chrono::Duration::days(1))),
                ),
                (
                    "valid_until",
                    PropValue::String(rfc3339(now() + chrono::Duration::days(30))),
                ),
                ("enabled", PropValue::Bool(true)),
            ]),
            version: Version(1),
            nseq: Nseq(nseq),
            lifecycle: None,
        }
    }

    fn exposable_record(owning_service: &str, cap: &str, nseq: u64) -> Record {
        let pk = format!("{}:{}", owning_service, cap);
        let prefix = cap
            .split_once(':')
            .map(|(_, rest)| rest.to_owned())
            .unwrap_or_else(|| cap.to_owned());
        Record {
            key: rk("wgd.cross_mesh_exposable", &pk),
            value: obj(&[
                ("pk", PropValue::String(pk.clone())),
                ("owning_service", PropValue::String(owning_service.into())),
                ("cap_token", PropValue::String(cap.into())),
                ("cap_namespace_prefix", PropValue::String(prefix)),
                ("registered_at", PropValue::String(rfc3339(now()))),
                ("lifetime_kind", PropValue::String("ephemeral".into())),
            ]),
            version: Version(1),
            nseq: Nseq(nseq),
            lifecycle: None,
        }
    }

    /// Per-namespace state in the fake. `history` is a SPEC-12 §5.5
    /// stand-in: every observable mutation (upsert/delete) is appended
    /// with its `nseq` so `watch_records(since)` can replay anything
    /// `nseq > since` *before* attaching the live broadcast. Without
    /// this the fake would silently miss the list↔watch gap bug class
    /// the real SPEC-12 §5.5 seam is designed to prevent.
    #[derive(Default)]
    struct FakeNs {
        records: BTreeMap<String, Record>,
        nseq: u64,
        bus: Option<broadcast::Sender<(Nseq, WatchEvent)>>,
        history: Vec<(Nseq, WatchEvent)>,
        fail_list_count: u32,
        fail_watch_count: u32,
    }

    impl FakeNs {
        fn new() -> Self {
            Self {
                bus: Some(broadcast::channel(64).0),
                ..Default::default()
            }
        }

        fn ensure_bus(&mut self) -> broadcast::Sender<(Nseq, WatchEvent)> {
            if self.bus.is_none() {
                self.bus = Some(broadcast::channel(64).0);
            }
            self.bus.as_ref().unwrap().clone()
        }

        fn record_event(&mut self, nseq: Nseq, event: WatchEvent) {
            self.history.push((nseq, event.clone()));
            let bus = self.ensure_bus();
            let _ = bus.send((nseq, event));
        }
    }

    struct FakeWgdInner {
        ns: BTreeMap<&'static str, FakeNs>,
    }

    /// In-process [`WgdClient`] fake used by the subscriber tests.
    /// Carries one [`FakeNs`] per `wgd.*` namespace plus knobs for
    /// injecting transport failures and forcing watch-stream
    /// disconnects.
    pub struct FakeWgdClient {
        inner: Arc<Mutex<FakeWgdInner>>,
    }

    impl FakeWgdClient {
        pub fn new() -> Self {
            let mut ns = BTreeMap::new();
            ns.insert(NS_TRUSTED_MESHES, FakeNs::new());
            ns.insert(NS_GRANTS, FakeNs::new());
            ns.insert(NS_CROSS_MESH_EXPOSABLE, FakeNs::new());
            Self {
                inner: Arc::new(Mutex::new(FakeWgdInner { ns })),
            }
        }

        fn with<R>(&self, f: impl FnOnce(&mut FakeWgdInner) -> R) -> R {
            let mut g = self.inner.lock().expect("fake mutex poisoned");
            f(&mut g)
        }

        pub fn seed(&self, namespace: &'static str, records: Vec<Record>) {
            self.with(|i| {
                let ns = i.ns.get_mut(namespace).expect("unknown namespace");
                ns.records.clear();
                for r in records {
                    ns.records.insert(r.key.key.clone(), r);
                }
                if let Some(max) = ns.records.values().map(|r| r.nseq.0).max() {
                    ns.nseq = max;
                }
            });
        }

        pub fn upsert(&self, namespace: &'static str, record: Record) {
            self.with(|i| {
                let ns = i.ns.get_mut(namespace).expect("unknown namespace");
                ns.nseq = ns.nseq.max(record.nseq.0);
                ns.records.insert(record.key.key.clone(), record.clone());
                ns.record_event(record.nseq, WatchEvent::Upsert(record));
            });
        }

        pub fn delete(&self, namespace: &'static str, key: &str, new_nseq: u64) {
            self.with(|i| {
                let ns = i.ns.get_mut(namespace).expect("unknown namespace");
                ns.nseq = ns.nseq.max(new_nseq);
                ns.records.remove(key);
                let rkey = RecordKey::collection(NamespaceName::new(namespace).unwrap(), key);
                ns.record_event(Nseq(new_nseq), WatchEvent::Delete(rkey));
            });
        }

        pub fn disconnect(&self, namespace: &'static str) {
            self.with(|i| {
                let ns = i.ns.get_mut(namespace).expect("unknown namespace");
                // Drop the current sender — outstanding receivers see
                // RecvError::Closed and the subscriber reseeds.
                ns.bus = None;
            });
        }

        pub fn fail_next_list(&self, namespace: &'static str, n: u32) {
            self.with(|i| {
                i.ns.get_mut(namespace)
                    .expect("unknown namespace")
                    .fail_list_count += n;
            });
        }

        pub fn fail_next_watch(&self, namespace: &'static str, n: u32) {
            self.with(|i| {
                i.ns.get_mut(namespace)
                    .expect("unknown namespace")
                    .fail_watch_count += n;
            });
        }
    }

    #[async_trait]
    impl WgdClient for FakeWgdClient {
        async fn list_records(&self, namespace: &str) -> Result<Listing, WgdError> {
            let mut guard = self.inner.lock().expect("fake mutex poisoned");
            let ns = guard
                .ns
                .get_mut(namespace)
                .ok_or_else(|| WgdError::Transport(format!("unknown namespace {namespace}")))?;
            if ns.fail_list_count > 0 {
                ns.fail_list_count -= 1;
                return Err(WgdError::Transport("injected list failure".into()));
            }
            let records: Vec<Record> = ns.records.values().cloned().collect();
            Ok(Listing {
                records,
                nseq: Nseq(ns.nseq),
            })
        }

        async fn watch_records(
            &self,
            namespace: &str,
            since: Nseq,
        ) -> Result<WatchStream, WgdError> {
            // SPEC-12 §5.5 contract: replay every commit with `nseq >
            // since` *before* delivering live events, so the
            // list↔watch handover loses nothing. Snapshot history and
            // subscribe under the same lock — any subsequent
            // `record_event` is ordered after `subscribe`, so the
            // live broadcast picks up everything beyond the snapshot.
            // Live events still get nseq-deduped against the replay
            // tail to guard against the broadcast buffer racing the
            // snapshot.
            let (replay, rx, caught_up_nseq) = {
                let mut guard = self.inner.lock().expect("fake mutex poisoned");
                let ns = guard
                    .ns
                    .get_mut(namespace)
                    .ok_or_else(|| WgdError::Transport(format!("unknown namespace {namespace}")))?;
                if ns.fail_watch_count > 0 {
                    ns.fail_watch_count -= 1;
                    return Err(WgdError::Transport("injected watch failure".into()));
                }
                let rx = ns.ensure_bus().subscribe();
                let replay: VecDeque<(Nseq, WatchEvent)> = ns
                    .history
                    .iter()
                    .filter(|(n, _)| n.0 > since.0)
                    .cloned()
                    .collect();
                // SPEC-12 §5.5 v0.2.1: caught_up `nseq` is the
                // namespace high-water mark observed at install
                // time — NOT the caller's `since`. Sampled inside
                // the same lock that snapshotted the replay slice.
                let caught_up_nseq = Nseq(ns.nseq);
                (replay, rx, caught_up_nseq)
            };

            let stream = futures_util::stream::unfold(
                FakeWatchState::Replay(replay, rx, caught_up_nseq),
                |state| async move {
                    match state {
                        FakeWatchState::Replay(mut q, rx, caught_up_nseq) => {
                            if let Some((_, ev)) = q.pop_front() {
                                Some((Ok(ev), FakeWatchState::Replay(q, rx, caught_up_nseq)))
                            } else {
                                // Plan §3.1 step 3 / SPEC-12 §5.5
                                // v0.2.1: emit the caught-up marker
                                // exactly once after replay drains
                                // (even when replay was empty), then
                                // transition to live forwarding with
                                // `caught_up_nseq` as the dedup
                                // baseline.
                                Some((
                                    Ok(WatchEvent::CaughtUp {
                                        nseq: caught_up_nseq,
                                    }),
                                    FakeWatchState::Live(rx, caught_up_nseq),
                                ))
                            }
                        }
                        FakeWatchState::Live(rx, last) => pump_live(rx, last).await,
                    }
                },
            );
            Ok(Box::pin(stream))
        }
    }

    /// State machine for [`FakeWgdClient::watch_records`]'s stream:
    /// drain the replay snapshot, emit `CaughtUp`, then forward live
    /// broadcast events. The trailing `Nseq` in `Replay` is the
    /// namespace high-water mark captured at install time
    /// (`caught_up_nseq` per SPEC-12 §5.5 v0.2.1); it becomes the
    /// dedup baseline in `Live` after the marker is emitted.
    enum FakeWatchState {
        Replay(
            VecDeque<(Nseq, WatchEvent)>,
            broadcast::Receiver<(Nseq, WatchEvent)>,
            Nseq,
        ),
        Live(broadcast::Receiver<(Nseq, WatchEvent)>, Nseq),
    }

    async fn pump_live(
        mut rx: broadcast::Receiver<(Nseq, WatchEvent)>,
        mut last: Nseq,
    ) -> Option<(Result<WatchEvent, WgdError>, FakeWatchState)> {
        loop {
            match rx.recv().await {
                Ok((n, ev)) => {
                    if n.0 <= last.0 {
                        continue; // already emitted via replay
                    }
                    last = n;
                    return Some((Ok(ev), FakeWatchState::Live(rx, last)));
                }
                Err(broadcast::error::RecvError::Closed) => return None,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    return Some((
                        Err(WgdError::Transport(format!("watch lagged by {n}"))),
                        FakeWatchState::Live(rx, last),
                    ));
                }
            }
        }
    }

    async fn wait_until<F: FnMut() -> bool>(mut cond: F) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !cond() {
            if std::time::Instant::now() >= deadline {
                panic!("condition not satisfied within timeout");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn ready_after_initial_seed_on_all_three_namespaces() {
        let fake = FakeWgdClient::new();
        fake.seed(NS_TRUSTED_MESHES, vec![trusted_record("channie.net", 1)]);
        fake.seed(
            NS_GRANTS,
            vec![grant_record(
                "01HXG1",
                "channie.net",
                "props.read:maild.accounts",
                1,
            )],
        );
        fake.seed(
            NS_CROSS_MESH_EXPOSABLE,
            vec![exposable_record("maild", "props.read:maild.accounts", 1)],
        );

        let (cache, mut ready, _handle) =
            TrustGrantsCache::start(Arc::new(fake) as Arc<dyn WgdClient>);
        timeout(Duration::from_secs(2), ready.wait_ready())
            .await
            .expect("ready timed out");

        let h = cache.handle();
        assert!(h.trusted_mesh("channie.net").is_some());
        assert_eq!(h.grants_for_mesh("channie.net").len(), 1);
        assert_eq!(h.exposable_for_namespace("maild.accounts").len(), 1);
    }

    #[tokio::test]
    async fn watch_upsert_updates_handle() {
        let fake = Arc::new(FakeWgdClient::new());
        let (cache, mut ready, _handle) =
            TrustGrantsCache::start(Arc::clone(&fake) as Arc<dyn WgdClient>);
        timeout(Duration::from_secs(2), ready.wait_ready())
            .await
            .expect("ready timed out");

        let h = cache.handle();
        assert!(h.trusted_mesh("channie.net").is_none());

        fake.upsert(NS_TRUSTED_MESHES, trusted_record("channie.net", 2));
        wait_until(|| h.trusted_mesh("channie.net").is_some()).await;
    }

    #[tokio::test]
    async fn watch_delete_removes_grant_via_bucket_search() {
        let fake = Arc::new(FakeWgdClient::new());
        fake.seed(
            NS_GRANTS,
            vec![grant_record(
                "01HXG1",
                "channie.net",
                "props.read:maild.accounts",
                1,
            )],
        );
        let (cache, mut ready, _handle) =
            TrustGrantsCache::start(Arc::clone(&fake) as Arc<dyn WgdClient>);
        timeout(Duration::from_secs(2), ready.wait_ready())
            .await
            .expect("ready timed out");

        let h = cache.handle();
        assert_eq!(h.grants_for_mesh("channie.net").len(), 1);

        fake.delete(NS_GRANTS, "01HXG1", 2);
        wait_until(|| h.grants_for_mesh("channie.net").is_empty()).await;
    }

    #[tokio::test]
    async fn disconnect_triggers_reseed_and_picks_up_changes_during_outage() {
        let fake = Arc::new(FakeWgdClient::new());
        fake.seed(NS_TRUSTED_MESHES, vec![trusted_record("channie.net", 1)]);
        let (cache, mut ready, _handle) =
            TrustGrantsCache::start(Arc::clone(&fake) as Arc<dyn WgdClient>);
        timeout(Duration::from_secs(2), ready.wait_ready())
            .await
            .expect("ready timed out");

        let h = cache.handle();
        assert!(h.trusted_mesh("channie.net").is_some());

        // Disconnect, mutate state while disconnected, expect the
        // reseed re-list to surface the new record.
        fake.disconnect(NS_TRUSTED_MESHES);
        fake.with(|i| {
            let ns = i.ns.get_mut(NS_TRUSTED_MESHES).unwrap();
            ns.records
                .insert("example.com".into(), trusted_record("example.com", 3));
            ns.nseq = 3;
        });

        wait_until(|| h.trusted_mesh("example.com").is_some()).await;
        // Original record survives the reseed too.
        assert!(h.trusted_mesh("channie.net").is_some());
    }

    #[tokio::test]
    async fn list_failure_is_retried_until_success() {
        let fake = Arc::new(FakeWgdClient::new());
        fake.seed(NS_TRUSTED_MESHES, vec![trusted_record("channie.net", 1)]);
        // Fail the first two list attempts on the trusted_meshes
        // namespace; the other two namespaces seed normally so the
        // ready watch only flips after the retry succeeds.
        fake.fail_next_list(NS_TRUSTED_MESHES, 2);

        let (cache, mut ready, _handle) =
            TrustGrantsCache::start(Arc::clone(&fake) as Arc<dyn WgdClient>);
        timeout(Duration::from_secs(3), ready.wait_ready())
            .await
            .expect("ready timed out under retry");

        assert!(cache.handle().trusted_mesh("channie.net").is_some());
    }

    #[tokio::test]
    async fn watch_establish_failure_triggers_reseed_and_recovers() {
        // The watch establish path is a third failure mode distinct
        // from list-failure and mid-stream disconnect: list succeeds,
        // watch_records returns Err, the loop must back off + reseed.
        let fake = Arc::new(FakeWgdClient::new());
        fake.seed(NS_TRUSTED_MESHES, vec![trusted_record("channie.net", 1)]);
        fake.fail_next_watch(NS_TRUSTED_MESHES, 2);

        let (cache, mut ready, _handle) =
            TrustGrantsCache::start(Arc::clone(&fake) as Arc<dyn WgdClient>);
        // Ready waits through the injected watch failures: the first
        // list seeds the snapshot, but readiness needs `CaughtUp`
        // from a successful watch, so the cache only flips ready
        // once the injected-failure counter drains and a later
        // `watch_records` returns Ok and yields the marker.
        timeout(Duration::from_secs(3), ready.wait_ready())
            .await
            .expect("ready timed out");
        assert!(cache.handle().trusted_mesh("channie.net").is_some());

        // After the injected failures drain, an upsert should land
        // via the eventually-established watch.
        fake.upsert(NS_TRUSTED_MESHES, trusted_record("example.com", 5));
        let h = cache.handle();
        wait_until(|| h.trusted_mesh("example.com").is_some()).await;
    }

    #[tokio::test]
    async fn replay_path_delivers_events_between_list_and_watch() {
        // Exercises the fake's SPEC-12 §5.5 replay seam. The fake's
        // history is the only mechanism by which the subscriber sees
        // an event that the wgd server committed after `props.list`
        // captured `nseq` but before the `props.watch since=nseq`
        // subscription took effect. The real subscriber drives this
        // path on every initial-seed; here we exercise it directly
        // by calling `watch_records` after upserts have populated
        // history.
        let fake = FakeWgdClient::new();
        fake.upsert(NS_TRUSTED_MESHES, trusted_record("channie.net", 1));
        fake.upsert(NS_TRUSTED_MESHES, trusted_record("example.com", 2));

        let mut stream = fake
            .watch_records(NS_TRUSTED_MESHES, Nseq(0))
            .await
            .expect("watch_records");
        let ev1 = stream.next().await.expect("first replay").expect("ok");
        let ev2 = stream.next().await.expect("second replay").expect("ok");
        let ev3 = stream.next().await.expect("caught-up marker").expect("ok");
        match (&ev1, &ev2, &ev3) {
            (
                WatchEvent::Upsert(a),
                WatchEvent::Upsert(b),
                WatchEvent::CaughtUp { nseq: marker },
            ) => {
                assert_eq!(a.nseq, Nseq(1));
                assert_eq!(b.nseq, Nseq(2));
                // SPEC-12 §5.5 v0.2.1: caught_up nseq is the
                // namespace high-water mark observed at watch
                // install time, not the caller's `since`.
                assert_eq!(*marker, Nseq(2));
            }
            other => {
                panic!("expected Upsert(1), Upsert(2), CaughtUp, got {:?}", other)
            }
        }

        // since=1 skips the first replay entry; CaughtUp still trails.
        let mut stream2 = fake
            .watch_records(NS_TRUSTED_MESHES, Nseq(1))
            .await
            .expect("watch_records");
        let ev = stream2.next().await.expect("replay").expect("ok");
        match ev {
            WatchEvent::Upsert(r) => assert_eq!(r.nseq, Nseq(2)),
            other => panic!("expected Upsert(nseq=2), got {:?}", other),
        }
        let marker = stream2.next().await.expect("caught-up").expect("ok");
        match marker {
            WatchEvent::CaughtUp { nseq } => assert_eq!(nseq, Nseq(2)),
            other => panic!("expected CaughtUp(2), got {:?}", other),
        }

        // since=2 produces an immediate CaughtUp with no replay; the
        // marker still carries the namespace high-water mark (2),
        // never the caller's since.
        let mut stream3 = fake
            .watch_records(NS_TRUSTED_MESHES, Nseq(2))
            .await
            .expect("watch_records");
        let marker = stream3.next().await.expect("caught-up").expect("ok");
        match marker {
            WatchEvent::CaughtUp { nseq } => assert_eq!(nseq, Nseq(2)),
            other => panic!("expected CaughtUp(2), got {:?}", other),
        }

        // since >= current_nseq (caller-side bug) MUST still receive a
        // marker carrying the *server-observed* high-water mark, not
        // the caller's since. SPEC-12 §5.5 v0.2.1.
        let mut stream4 = fake
            .watch_records(NS_TRUSTED_MESHES, Nseq(99))
            .await
            .expect("watch_records");
        let marker = stream4.next().await.expect("caught-up").expect("ok");
        match marker {
            WatchEvent::CaughtUp { nseq } => assert_eq!(nseq, Nseq(2)),
            other => panic!(
                "expected CaughtUp(2) clamped to high-water, got {:?}",
                other
            ),
        }
    }

    #[tokio::test]
    async fn malformed_record_is_skipped_other_records_survive() {
        let fake = Arc::new(FakeWgdClient::new());
        // Good record + a malformed one (missing `mesh_fqdn`).
        let bad = Record {
            key: rk("wgd.trusted_meshes", "bad.example"),
            value: obj(&[("enabled", PropValue::Bool(true))]),
            version: Version(1),
            nseq: Nseq(1),
            lifecycle: None,
        };
        fake.seed(
            NS_TRUSTED_MESHES,
            vec![trusted_record("channie.net", 1), bad],
        );

        let (cache, mut ready, _handle) =
            TrustGrantsCache::start(Arc::clone(&fake) as Arc<dyn WgdClient>);
        timeout(Duration::from_secs(2), ready.wait_ready())
            .await
            .expect("ready timed out");

        let h = cache.handle();
        assert!(h.trusted_mesh("channie.net").is_some());
        assert!(h.trusted_mesh("bad.example").is_none());
    }
}
