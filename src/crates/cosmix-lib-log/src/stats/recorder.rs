//! `StatsRecorder` — the substrate's `metrics::Recorder` impl
//! (plan §3.3.1, §3.4).
//!
//! Composes `metrics_util::Registry<Key, AtomicStorage>` for atomic
//! counter/gauge/histogram storage, layered with per-metric
//! cardinality enforcement (plan §3.2 + §3.3.1 guardrails).
//!
//! # Cardinality discipline
//!
//! Every `register_*` callsite computes the canonical 64-bit FxHash
//! `id` of the label-set (the same hash family used for the wire
//! `labels_hash` digest in `labels_hash.rs`) and checks it against a
//! per-metric `HashSet<u64>`. If the metric's set already contains the
//! id, the call proceeds. If the set is full (size == cap), the call
//! returns a `Counter/Gauge/Histogram::noop()` handle — the caller
//! continues to operate on a no-op handle for the lifetime of that
//! label-set, and the built-in
//! `cosmix_stats_cardinality_drops_total{metric=<name>}` counter is
//! incremented so the drop is observable. New label-sets are silently
//! dropped instead of raising because the hot path cannot tolerate
//! `Result<Counter, _>` propagation (per §3.2 design rationale).
//!
//! # Cap bounds (plan §3.3.1)
//!
//! - Floor: 16  (lower would lose information faster than it bounds memory)
//! - Ceiling: 4096  (caps recorder memory at ~few-MB per metric family even pathological)
//! - Default: 1024
//!
//! Out-of-bounds caps passed to the builder panic at build time — this
//! is operator/test-runtime misconfiguration, not a runtime input;
//! failing loudly at startup is the right shape.
//!
//! # Recursion guard on the drops metric
//!
//! `cosmix_stats_cardinality_drops_total` is itself a metric family
//! tracked in the same cardinality registry. If its own cap is hit
//! (would require >cap distinct metric names dropping in a single
//! process, already gross misuse), `record_drop` degrades to a silent
//! no-op rather than recursing into itself.

use crate::stats::labels_hash::labels_hash;
use crate::stats::rollup::{InstalledSink, PrevState};
use crate::stats::sink::StatsSink;
use crate::stats::types::MetricKind;
use metrics::atomics::AtomicU64;
use metrics::{
    Counter, Gauge, Histogram, Key, KeyName, Label, Metadata, Recorder, SharedString, Unit,
};
use metrics_util::registry::{AtomicStorage, Registry};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

/// Lower bound on per-metric cardinality cap (plan §3.3.1).
pub const CARDINALITY_FLOOR: u32 = 16;
/// Upper bound on per-metric cardinality cap (plan §3.3.1).
pub const CARDINALITY_CEILING: u32 = 4096;
/// Default per-metric cardinality cap (plan §3.3.1).
pub const CARDINALITY_DEFAULT: u32 = 1024;
/// Name of the built-in counter exposing cap-rejection events.
pub const CARDINALITY_DROPS_METRIC: &str = "cosmix_stats_cardinality_drops_total";

/// Names reserved for the built-in process gauges written by
/// `process_gauges::update_process_gauges` into the side map
/// `RecorderInner.built_in_gauges`. Any `register_*` call under
/// one of these names returns a `noop()` handle — the substrate
/// owns the namespace, and admitting an app-side write under
/// the same name would emit duplicate `PeriodRecord`s in one
/// period (Codex round-6 MINOR fix: registry + side-map both
/// surface as `Key::from_name(name)` gauges, so a collision is
/// invisible to dedup).
pub(crate) const BUILTIN_GAUGE_RESERVED_NAMES: &[&str] = &[
    "cosmix_process_uptime_seconds",
    "cosmix_process_memory_kb",
    "cosmix_process_open_fds",
    "cosmix_process_cpu_seconds_total",
];

/// Per-metric operator description text (the second arg to
/// `metrics::describe_counter!` / `_gauge!` / `_histogram!`). Keyed
/// by `(name, MetricKind)` in `RecorderInner.descriptions` so the
/// rare same-name-different-kind family pair (legal under
/// `metrics::Recorder`) does not overwrite each other's descriptions
/// (Codex round-3 MINOR fix).
#[derive(Debug, Clone)]
pub(crate) struct MetricDescription {
    pub(crate) text: Option<String>,
}

/// Minimum spacing between successive `warn`-level cap-rejection
/// events for the *same* metric (plan §3.3.1: "one warn-level event
/// per metric per hour"). The drops counter increments on every
/// rejection regardless; the warn is the human-attention path.
const WARN_REPEAT_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Shared inner state of a `StatsRecorder`. Held behind an `Arc` so the
/// recorder itself can be moved into `metrics::set_global_recorder`
/// (which takes ownership) while `local_snapshot()` retains read
/// access via the `SHARED` slot.
pub(crate) struct RecorderInner {
    pub(crate) identity: String,
    /// Kernel hostname snapshotted at recorder-build time (from
    /// `/etc/hostname`, `"unknown"` fallback). Cached so every
    /// `PeriodSnapshot.host` field reuses the same string instead of
    /// re-reading the file every period (Codex round-4 MINOR — the
    /// JSONL contract pins this to "kernel hostname at recorder
    /// startup", §3.4).
    pub(crate) host: String,
    /// `Instant` captured at builder time. Backs the
    /// `cosmix_process_uptime_seconds` built-in gauge (plan §3 —
    /// "every binary, free, no app code needed") via
    /// `process_gauges::update_process_gauges`. This is the
    /// recorder-active duration, not the absolute process uptime;
    /// in normal init flow the recorder is built within the first
    /// few ms of `main`, so the discrepancy is negligible. v2 can
    /// switch to `/proc/self/stat` field 22 if absolute uptime is
    /// ever required.
    pub(crate) process_start: Instant,
    /// Built-in process gauges live OUT-OF-BAND of the metrics
    /// `Registry`. `update_process_gauges` rewrites this map every
    /// roll-up; failed procfs reads drop the entry, so a gauge that
    /// can't be sampled this period emits **no record** (operator
    /// sees the series go dark, not a stale value — Codex round-5
    /// MAJOR fix). The registry path is reserved for app gauges,
    /// which emit every period regardless of change.
    pub(crate) built_in_gauges: Mutex<HashMap<&'static str, f64>>,
    pub(crate) registry: Registry<Key, AtomicStorage>,
    /// Per-metric-name set of admitted `metrics::Key`s. Using `Key`
    /// directly (rather than a derived hash) guarantees the admission
    /// set is congruent with `Registry`'s own dedup: any two callsites
    /// that `Registry` would coalesce into one entry consume one
    /// admission slot, and any two that `Registry` keeps distinct
    /// consume two slots (Codex round-3 MAJOR fix — see `admit` doc).
    label_sets: RwLock<HashMap<String, HashSet<Key>>>,
    /// `Instant` of the last `warn`-level cap-rejection event emitted
    /// for each metric. Rate-limits the warn path to
    /// `WARN_REPEAT_INTERVAL` (plan §3.3.1).
    last_warn: RwLock<HashMap<String, Instant>>,
    /// Operator descriptions, keyed by `(name, kind)` (Codex round-3
    /// MINOR fix). The metrics facade's `describe_counter` /
    /// `_gauge` / `_histogram` carry the kind at the call site even
    /// though `KeyName` does not.
    pub(crate) descriptions: RwLock<HashMap<(String, MetricKind), MetricDescription>>,
    default_cap: u32,
    per_metric_caps: HashMap<String, u32>,
    /// Roll-up serialisation lock. `perform_rollup` takes this for
    /// the entire read-current → per-sink-compute → commit window,
    /// so concurrent roll-up calls cannot commit an older candidate
    /// after a newer one and rewind per-sink prev (Codex round-5
    /// MAJOR). The inner `()` is never observed; the lock is purely
    /// an ordering primitive.
    pub(crate) rollup_lock: Mutex<()>,
    /// Installed storage backends. Each entry pairs the sink with
    /// its own `PrevState` (the previous-period readings *that the
    /// sink actually accepted*). Per-sink prev is the only design
    /// that satisfies the plan §4.1 contract — "delta is the change
    /// since the previous emitted line for the same (metric,
    /// labels)" — under partial fan-out failures: a sink that
    /// rejects a period must see merged deltas spanning the missed
    /// period on its next successful accept, while sinks that
    /// accepted the period advance independently (Codex round-4
    /// MAJOR). Held behind a `Mutex` (not `RwLock`) because the
    /// only access pattern is occasional read-then-iterate by the
    /// roll-up task.
    pub(crate) sinks: Mutex<Vec<Arc<InstalledSink>>>,
    /// Optional redaction-first Prometheus child recorder (plan
    /// §3.3.2 + §8.1). Set once by the citizen-side
    /// `LogHandle::attach_prometheus` after the daemon's Tokio
    /// runtime is up; `register_*` calls that survive the
    /// per-metric cardinality cap fan out to it after label
    /// redaction. `OnceLock` because the seam is install-once
    /// (slot it, never swap it) and the hot path needs an
    /// uncontended read.
    #[cfg(feature = "prometheus")]
    pub(crate) prometheus_child: OnceLock<Arc<crate::stats::prometheus::PrometheusChild>>,
}

/// The substrate stats recorder. Constructed via [`StatsRecorderBuilder`],
/// installed via [`StatsRecorder::install`].
pub struct StatsRecorder {
    pub(crate) inner: Arc<RecorderInner>,
}

/// Builder for [`StatsRecorder`]. Sets the identity (service name
/// surfaced in `Snapshot.service` and JSONL `service` field) and any
/// per-metric cardinality overrides.
pub struct StatsRecorderBuilder {
    identity: String,
    default_cap: u32,
    per_metric_caps: HashMap<String, u32>,
}

/// Error returned by [`StatsRecorder::install`] when the global slot
/// (the `SHARED` `OnceLock` or `metrics::set_global_recorder`) is
/// already occupied.
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    /// A `StatsRecorder` was already installed in this process. Only
    /// one `install` call is permitted; tests that need a per-test
    /// recorder should use `metrics::with_local_recorder` (the
    /// thread-local installer described in plan §3.4.2).
    #[error("a StatsRecorder is already installed for this process")]
    AlreadyInstalled,
    /// `metrics::set_global_recorder` failed because some other
    /// `Recorder` (or a previously-installed StatsRecorder via a
    /// different code path) is already the global.
    #[error("metrics::set_global_recorder failed: another global recorder is already set")]
    GlobalRecorderSet,
}

impl StatsRecorderBuilder {
    /// Construct a new builder for the given service identity. The
    /// identity surfaces verbatim in `Snapshot.service` and in the
    /// JSONL `service` field (slice 3).
    pub fn new(identity: impl Into<String>) -> Self {
        Self {
            identity: identity.into(),
            default_cap: CARDINALITY_DEFAULT,
            per_metric_caps: HashMap::new(),
        }
    }

    /// Override the default cardinality cap. Panics if `cap` is
    /// outside `[CARDINALITY_FLOOR, CARDINALITY_CEILING]`.
    pub fn default_cardinality(mut self, cap: u32) -> Self {
        check_cap_bounds("<default>", cap);
        self.default_cap = cap;
        self
    }

    /// Set a per-metric cardinality cap override. Panics if `cap` is
    /// outside `[CARDINALITY_FLOOR, CARDINALITY_CEILING]`.
    pub fn cardinality_override(mut self, name: impl Into<String>, cap: u32) -> Self {
        let n = name.into();
        check_cap_bounds(&n, cap);
        self.per_metric_caps.insert(n, cap);
        self
    }

    /// Build the recorder. The returned recorder is not yet installed
    /// as the process-wide global; call [`StatsRecorder::install`].
    pub fn build(self) -> StatsRecorder {
        // Built-in metric families default to `Safe` sensitivity — see
        // `classify_built_in_metrics`'s doc for the per-family
        // reasoning. Runs at *every* recorder build because classify
        // is idempotent for matching (name, sensitivity) pairs; a
        // test that swapped the classification before our call would
        // still see the override after, which matches the
        // re-classification contract in `classify.rs`.
        crate::stats::classify::classify_built_in_metrics();
        StatsRecorder {
            inner: Arc::new(RecorderInner {
                identity: self.identity,
                host: read_kernel_hostname(),
                process_start: Instant::now(),
                built_in_gauges: Mutex::new(HashMap::new()),
                registry: Registry::atomic(),
                label_sets: RwLock::new(HashMap::new()),
                last_warn: RwLock::new(HashMap::new()),
                descriptions: RwLock::new(HashMap::new()),
                default_cap: self.default_cap,
                per_metric_caps: self.per_metric_caps,
                rollup_lock: Mutex::new(()),
                sinks: Mutex::new(Vec::new()),
                #[cfg(feature = "prometheus")]
                prometheus_child: OnceLock::new(),
            }),
        }
    }
}

fn check_cap_bounds(label: &str, cap: u32) {
    assert!(
        (CARDINALITY_FLOOR..=CARDINALITY_CEILING).contains(&cap),
        "cardinality cap for {label} = {cap} out of bounds [{CARDINALITY_FLOOR}, {CARDINALITY_CEILING}]"
    );
}

/// Process-wide handle to the installed recorder's inner state. Set
/// once by `StatsRecorder::install`, read by `local_snapshot()` (and
/// by slice-3+ JSONL/Bus read paths).
static SHARED: OnceLock<Arc<RecorderInner>> = OnceLock::new();

impl StatsRecorder {
    /// Install this recorder as the process-wide `metrics` global AND
    /// publish to the shared slot so `local_snapshot()` can find it.
    ///
    /// Returns `Err` if either slot is already occupied — the caller
    /// must decide whether that is fatal (the normal init path treats
    /// it as fatal) or expected (test helpers).
    ///
    /// # Ordering & atomicity (Codex round-1 MAJOR fix)
    ///
    /// `metrics::set_global_recorder` is attempted *first*. Only if it
    /// succeeds — guaranteeing no other recorder is the global — do
    /// we publish to `SHARED`. The reverse order would leave `SHARED`
    /// occupied (pointing at a recorder that is NOT the global) when
    /// `set_global_recorder` failed, permanently breaking the
    /// invariant *`SHARED` is set iff a `StatsRecorder` is the global
    /// recorder*. The chosen order keeps that invariant: a failed
    /// install leaves both slots in a consistent state. The
    /// `SHARED.set` after a successful `set_global_recorder` cannot
    /// fail in practice — `SHARED` is private to this crate and only
    /// written here, and a prior `SHARED.set` success requires a
    /// prior `set_global_recorder` success — which would have made
    /// our own call fail. We still surface the error rather than
    /// `expect()` so a future test seam that pokes `SHARED` directly
    /// doesn't trigger a panic in production.
    pub fn install(self) -> Result<(), InstallError> {
        let inner = Arc::clone(&self.inner);
        metrics::set_global_recorder(self).map_err(|_| InstallError::GlobalRecorderSet)?;
        SHARED
            .set(inner)
            .map_err(|_| InstallError::AlreadyInstalled)?;
        Ok(())
    }

    /// Register a storage backend with this recorder.
    ///
    /// The roll-up driver (`crate::stats::rollup::perform_rollup`)
    /// fans every period to each registered sink in registration
    /// order. Sinks added after the recorder is installed take
    /// effect on the next roll-up; sinks added before installation
    /// receive every roll-up.
    ///
    /// Idempotent only on the caller — `add_sink` does not dedup;
    /// adding the same `Arc<dyn StatsSink>` twice produces two
    /// `record_period` calls per period.
    pub fn add_sink(&self, sink: Arc<dyn StatsSink>) {
        self.inner
            .sinks
            .lock()
            .expect("stats sinks Mutex poisoned")
            .push(Arc::new(InstalledSink {
                sink,
                prev: Mutex::new(PrevState::new()),
            }));
    }
}

/// Errors returned by [`set_prometheus_child_on_installed`].
#[cfg(feature = "prometheus")]
#[derive(Debug, thiserror::Error)]
pub enum PrometheusAttachError {
    /// No `StatsRecorder` is the process global — `init()` either
    /// hasn't run yet, ran with `--stats=off`, or lost the global
    /// recorder race.
    #[error("no StatsRecorder is installed; call init() with --stats=on first")]
    NoRecorder,
    /// A `PrometheusChild` has already been attached to the
    /// installed recorder. The seam is install-once by design
    /// (plan §8.1 "follow-on attach step"); swap support is
    /// out of scope.
    #[error("a PrometheusChild is already attached to this process's StatsRecorder")]
    AlreadyAttached,
}

/// Attach a `PrometheusChild` to the process-installed
/// `StatsRecorder`. The substrate recorder fans every surviving
/// `register_counter` / `_gauge` / `_histogram` call out to the
/// child after label redaction (plan §3.3.2). Returns
/// [`PrometheusAttachError::NoRecorder`] when no recorder is
/// installed (the `--stats=off` / pre-init / test path) and
/// [`PrometheusAttachError::AlreadyAttached`] when the slot has
/// already been claimed; the install-once shape mirrors
/// `StatsRecorder::install`.
#[cfg(feature = "prometheus")]
pub fn set_prometheus_child_on_installed(
    child: Arc<crate::stats::prometheus::PrometheusChild>,
) -> Result<(), PrometheusAttachError> {
    let Some(inner) = shared() else {
        return Err(PrometheusAttachError::NoRecorder);
    };
    inner
        .prometheus_child
        .set(child)
        .map_err(|_| PrometheusAttachError::AlreadyAttached)
}

/// Precondition check used by [`crate::prometheus_attach::attach`]
/// before it calls `PrometheusBuilder::build()`.
///
/// **Codex round-3 MAJOR.** `PrometheusBuilder::build()` spawns its
/// upkeep `tokio::spawn` task before returning (see
/// `metrics-exporter-prometheus-0.18.3/src/exporter/builder.rs:555`),
/// *regardless* of whether the caller then succeeds in installing the
/// resulting recorder. If `attach` discovers it can't install the
/// child after a successful `build()` (no global recorder, slot
/// already occupied), the upkeep task is already running and there is
/// no API to cancel it — it would tick forever for the lifetime of
/// the process. This precheck folds the two known-bad states (no
/// recorder, slot occupied) into a pre-build error, so the
/// post-build install can only fail under a genuine attach-vs-attach
/// race that the caller's API contract already forbids.
///
/// A non-empty `Ok` does *not* atomically reserve the slot — that
/// would require a CAS primitive `OnceLock` does not expose. Callers
/// who race two concurrent `attach_prometheus` calls can still observe
/// `AlreadyAttached` from the post-build install; that path leaks the
/// upkeep task, but the documented contract is that `attach` is
/// called once.
#[cfg(feature = "prometheus")]
pub fn precheck_prometheus_attachable() -> Result<(), PrometheusAttachError> {
    let Some(inner) = shared() else {
        return Err(PrometheusAttachError::NoRecorder);
    };
    if inner.prometheus_child.get().is_some() {
        return Err(PrometheusAttachError::AlreadyAttached);
    }
    Ok(())
}

/// Add a sink to the process-installed recorder. Returns `false`
/// if no recorder has been installed (the `--stats=off` /
/// pre-init / test-without-install case). Used by the roll-up
/// task and the `LogHandle::shutdown` flush path.
pub fn add_sink_to_installed(sink: Arc<dyn StatsSink>) -> bool {
    if let Some(inner) = shared() {
        inner
            .sinks
            .lock()
            .expect("stats sinks Mutex poisoned")
            .push(Arc::new(InstalledSink {
                sink,
                prev: Mutex::new(PrevState::new()),
            }));
        true
    } else {
        false
    }
}

/// Read `/etc/hostname` for the cached `RecorderInner.host` field,
/// falling back to `"unknown"` (matches the Mix `hostname()`
/// builtin convention at `cosmix-lib-mix/src/builtins.rs:3556`).
fn read_kernel_hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .unwrap_or_else(|_| "unknown".into())
        .trim()
        .to_string()
}

/// Read-only access to the installed recorder's inner state. Returns
/// `None` if no recorder has been installed in this process (the
/// `--stats=off` / pre-init / test cases).
pub(crate) fn shared() -> Option<Arc<RecorderInner>> {
    SHARED.get().cloned()
}

/// Handle to the process-global installed recorder, for daemons that
/// need to drive [`crate::stats::perform_rollup`] periodically or
/// serve `<svc>.stats.snapshot` (`handle_snapshot_bus` takes
/// `&StatsRecorder`). `init()` consumes the recorder it installs, so
/// this is the only post-init path to it. `None` when stats are off
/// or init hasn't run. Cheap: clones the shared `Arc`.
pub fn installed_recorder() -> Option<StatsRecorder> {
    SHARED.get().cloned().map(|inner| StatsRecorder { inner })
}

impl RecorderInner {
    fn cap_for(&self, name: &str) -> u32 {
        self.per_metric_caps
            .get(name)
            .copied()
            .unwrap_or(self.default_cap)
    }

    /// Try to admit `key` against the per-metric cap. Returns true if
    /// the same `Key` (under `metrics::Key`'s own `Hash` + `Eq`) is
    /// already admitted, or there is room to insert it; false if the
    /// per-metric cap is exhausted.
    ///
    /// # Why `HashSet<Key>` (Codex round-3 MAJOR fix)
    ///
    /// `metrics_util::registry::Registry` dedups entries by
    /// `metrics::Key`'s own `Hash` + `Eq`. Any derived
    /// canonicalisation we hand-write — sorting label pairs by `(key,
    /// value)`, by key only, by hash — diverges from `Key::eq` on
    /// pathological inputs (duplicate label keys with permuted
    /// orderings, the 3-7 label `sort_by_key` stable-sort branch,
    /// the swap-check 2-label branch). Storing `Key` directly means
    /// the admission set is *definitionally* in lock-step with the
    /// `Registry`: every Key-set the Registry coalesces into one
    /// entry consumes one slot here; every Key-set the Registry
    /// keeps distinct consumes one slot per entry. No cap bypass
    /// vector via permutation of duplicate keys.
    ///
    /// The memory cost (an extra `Key` clone per admitted series) is
    /// bounded by the per-metric cap × number of metric names and is
    /// acceptable at the floor/ceiling defined in §3.3.1.
    fn admit(&self, key: &Key) -> bool {
        let name = key.name();
        // Hot path: already-known Key. Avoids the write lock entirely
        // for the steady-state case.
        {
            let r = self
                .label_sets
                .read()
                .expect("stats label_sets read RwLock poisoned");
            if let Some(set) = r.get(name)
                && set.contains(key)
            {
                return true;
            }
        }
        // Slow path: take the write lock once to either insert or
        // confirm rejection. Cap_for() is read inside the lock so a
        // concurrent builder-side override (which there is no API for
        // post-build, but defends the invariant in case one lands)
        // can't admit one extra past the limit.
        let admitted = {
            let mut w = self
                .label_sets
                .write()
                .expect("stats label_sets write RwLock poisoned");
            let set = w.entry(name.to_string()).or_default();
            if set.contains(key) {
                true
            } else if (set.len() as u32) >= self.cap_for(name) {
                false
            } else {
                set.insert(key.clone());
                true
            }
        };
        if !admitted {
            self.record_drop(key);
        }
        admitted
    }

    /// Record a cap rejection: bump the drops counter (subject to its
    /// own admission, with a recursion guard) and emit a rate-limited
    /// `warn` event (plan §3.3.1: "one warn-level event per metric
    /// per hour"). The warn carries the rejected metric name plus
    /// the 16-hex `labels_hash` digest — never raw label values, per
    /// §3.3.2's restricted-by-default discipline (`labels_hash.rs`).
    fn record_drop(&self, rejected: &Key) {
        let dropped_metric = rejected.name();
        // Recursion guard. The drops counter is itself a metric
        // tracked in this registry; if its own cardinality cap fills
        // (which would require >cap distinct metric names already
        // dropping in a single process), we degrade to a silent no-op
        // rather than recurse.
        if dropped_metric == CARDINALITY_DROPS_METRIC {
            return;
        }
        let drops_key = Key::from_parts(
            CARDINALITY_DROPS_METRIC,
            vec![Label::new("metric", dropped_metric.to_string())],
        );
        let admitted = {
            let mut w = self
                .label_sets
                .write()
                .expect("stats label_sets write RwLock poisoned");
            let set = w.entry(CARDINALITY_DROPS_METRIC.to_string()).or_default();
            if set.contains(&drops_key) {
                true
            } else if (set.len() as u32) >= self.cap_for(CARDINALITY_DROPS_METRIC) {
                false
            } else {
                set.insert(drops_key.clone());
                true
            }
        };
        if admitted {
            let counter: Arc<AtomicU64> =
                self.registry.get_or_create_counter(&drops_key, Arc::clone);
            counter.fetch_add(1, Ordering::Release);
            // Codex round-3 MAJOR: `record_drop` bypasses
            // `Recorder::register_counter`, so the standard fan-out
            // path never sees the drops counter. Mirror it to the
            // Prometheus child explicitly so operators see the same
            // built-in metric on `/metrics` they alert on in JSONL/Bus.
            #[cfg(feature = "prometheus")]
            if let Some(child) = self.prometheus_child.get() {
                child.fan_internal_counter(&drops_key, 1);
            }
            // Make the drops counter visible in snapshot output even
            // when no operator described it.
            let mut d = self
                .descriptions
                .write()
                .expect("stats descriptions write RwLock poisoned");
            d.entry((CARDINALITY_DROPS_METRIC.to_string(), MetricKind::Counter))
                .or_insert(MetricDescription {
                    text: Some("Per-metric cardinality cap rejections (built-in).".to_string()),
                });
        }
        // Plan §3.3.1: rate-limited warn, at most once per metric per
        // hour. The `labels_hash` is the canonical 16-hex digest used
        // across surfaces (JSONL, Bus), so an operator can grep
        // matching records for the same digest. Plan §3.3.2 says raw
        // *values* are restricted-by-default; label *keys* are
        // bounded structural names (compile-time strings from the
        // `metrics::counter!` macro family) and are safe to log —
        // including them on the warn is the single best diagnostic
        // signpost, because cardinality bugs almost always trace to a
        // specific label dimension (Codex round-4 MINOR fix).
        //
        // Round-4 MINOR #2 (Wontfix, by design): the warn digest is
        // BTreeMap-based and collapses duplicate label keys, while
        // admission uses Key::eq (duplicate-preserving). The digest's
        // job is cross-surface correlation with JSONL/Bus records
        // (§4.1) that share the same BTreeMap canonicalisation; an
        // operator who sees the warn digest can find the matching
        // wire records. The admission surface and the wire surface
        // are deliberately different invariants — diverging digests
        // would break the correlation the digest exists for.
        if self.warn_now(dropped_metric) {
            let labels_map: BTreeMap<String, String> = rejected
                .labels()
                .map(|l| (l.key().to_string(), l.value().to_string()))
                .collect();
            let labels_hash = labels_hash(&labels_map);
            // Dedup + sort label keys for stable rendering. Vec is
            // bounded by the per-Key label count (small in practice).
            let mut label_keys: Vec<&str> = rejected.labels().map(|l| l.key()).collect();
            label_keys.sort_unstable();
            label_keys.dedup();
            tracing::warn!(
                target: "cosmix_log::stats",
                metric = dropped_metric,
                labels_hash = labels_hash.as_str(),
                label_keys = ?label_keys,
                cap = self.cap_for(dropped_metric),
                "stats cardinality cap exhausted; new label-set dropped (further drops counted in cosmix_stats_cardinality_drops_total)"
            );
        }
    }

    /// Returns true and stamps `Instant::now()` iff the per-metric
    /// warn rate-limiter says it is time to emit again. The current
    /// `Instant` is recorded under the write lock so two threads
    /// racing on the same metric admit exactly one warn between
    /// them in the worst case (a brief window where neither has yet
    /// committed the stamp).
    fn warn_now(&self, metric: &str) -> bool {
        let now = Instant::now();
        let mut w = self
            .last_warn
            .write()
            .expect("stats last_warn write RwLock poisoned");
        match w.get(metric).copied() {
            Some(prev) if now.duration_since(prev) < WARN_REPEAT_INTERVAL => false,
            _ => {
                w.insert(metric.to_string(), now);
                true
            }
        }
    }

    /// Snapshot the current built-in gauge readings. Returns an
    /// empty Vec when no `update_process_gauges` call has run yet,
    /// or when every read failed this period (the procfs-skip
    /// branch in `process_gauges`). Callers (the roll-up driver
    /// and `local_snapshot`) treat absence as "series went dark"
    /// — they emit no record for the missing name — so a stale
    /// reading cannot leak past a failed read (Codex round-5
    /// MAJOR fix). Iteration order is unspecified; callers that
    /// need stable ordering must sort by name themselves.
    pub(crate) fn built_in_gauge_snapshot(&self) -> Vec<(&'static str, f64)> {
        let g = self
            .built_in_gauges
            .lock()
            .expect("stats built_in_gauges Mutex poisoned");
        g.iter().map(|(name, value)| (*name, *value)).collect()
    }
}

impl StatsRecorder {
    fn describe(&self, name: KeyName, kind: MetricKind, description: SharedString) {
        let mut d = self
            .inner
            .descriptions
            .write()
            .expect("stats descriptions write RwLock poisoned");
        d.insert(
            (name.as_str().to_string(), kind),
            MetricDescription {
                text: Some(description.into_owned()),
            },
        );
    }
}

impl Recorder for StatsRecorder {
    fn describe_counter(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        #[cfg(feature = "prometheus")]
        if let Some(child) = self.inner.prometheus_child.get() {
            child.describe_counter(key.clone(), unit, description.clone());
        }
        #[cfg(not(feature = "prometheus"))]
        let _ = unit;
        self.describe(key, MetricKind::Counter, description);
    }

    fn describe_gauge(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        #[cfg(feature = "prometheus")]
        if let Some(child) = self.inner.prometheus_child.get() {
            child.describe_gauge(key.clone(), unit, description.clone());
        }
        #[cfg(not(feature = "prometheus"))]
        let _ = unit;
        self.describe(key, MetricKind::Gauge, description);
    }

    fn describe_histogram(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        #[cfg(feature = "prometheus")]
        if let Some(child) = self.inner.prometheus_child.get() {
            child.describe_histogram(key.clone(), unit, description.clone());
        }
        #[cfg(not(feature = "prometheus"))]
        let _ = unit;
        self.describe(key, MetricKind::Histogram, description);
    }

    fn register_counter(&self, key: &Key, metadata: &Metadata<'_>) -> Counter {
        // Plan §3.3.2: cardinality cap evaluated BEFORE redaction
        // forward. A cap-rejected series must not appear on
        // /metrics either, otherwise the cap is JSONL-only and
        // operator-visible cardinality leaks through Prometheus.
        if is_reserved_builtin_name(key.name()) || !self.inner.admit(key) {
            return Counter::noop();
        }
        let primary_atomic: Arc<AtomicU64> =
            self.inner.registry.get_or_create_counter(key, Arc::clone);
        let primary = Counter::from_arc(primary_atomic);
        #[cfg(feature = "prometheus")]
        if let Some(child) = self.inner.prometheus_child.get() {
            let secondary = child.register_counter(key, metadata);
            return crate::stats::prometheus::fan_counter(primary, secondary);
        }
        #[cfg(not(feature = "prometheus"))]
        let _ = metadata;
        primary
    }

    fn register_gauge(&self, key: &Key, metadata: &Metadata<'_>) -> Gauge {
        if is_reserved_builtin_name(key.name()) || !self.inner.admit(key) {
            return Gauge::noop();
        }
        let primary_atomic: Arc<AtomicU64> =
            self.inner.registry.get_or_create_gauge(key, Arc::clone);
        let primary = Gauge::from_arc(primary_atomic);
        #[cfg(feature = "prometheus")]
        if let Some(child) = self.inner.prometheus_child.get() {
            let secondary = child.register_gauge(key, metadata);
            return crate::stats::prometheus::fan_gauge(primary, secondary);
        }
        #[cfg(not(feature = "prometheus"))]
        let _ = metadata;
        primary
    }

    fn register_histogram(&self, key: &Key, metadata: &Metadata<'_>) -> Histogram {
        if is_reserved_builtin_name(key.name()) || !self.inner.admit(key) {
            return Histogram::noop();
        }
        let primary_atomic = self.inner.registry.get_or_create_histogram(key, Arc::clone);
        let primary = Histogram::from_arc(primary_atomic);
        #[cfg(feature = "prometheus")]
        if let Some(child) = self.inner.prometheus_child.get() {
            let secondary = child.register_histogram(key, metadata);
            return crate::stats::prometheus::fan_histogram(primary, secondary);
        }
        #[cfg(not(feature = "prometheus"))]
        let _ = metadata;
        primary
    }
}

/// Returns true when `name` is in `BUILTIN_GAUGE_RESERVED_NAMES`.
/// Reserved names are owned by the substrate's built-in gauge
/// side map (`RecorderInner.built_in_gauges`); app-side
/// registrations under those names get a `noop()` handle and
/// never reach the metrics `Registry`, so the side-map record
/// and a would-be registry record cannot collide into duplicate
/// per-period emissions (Codex round-6 MINOR).
fn is_reserved_builtin_name(name: &str) -> bool {
    BUILTIN_GAUGE_RESERVED_NAMES.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::snapshot::snapshot_from_inner;
    use crate::stats::types::{SeriesLabels, SeriesValue};

    fn make() -> StatsRecorder {
        StatsRecorderBuilder::new("test").build()
    }

    fn metadata() -> Metadata<'static> {
        Metadata::new("test", metrics::Level::INFO, None)
    }

    #[test]
    fn counter_increments_visible_in_snapshot() {
        let rec = make();
        let key = Key::from_parts("test_counter", vec![Label::new("k", "v")]);
        let c = rec.register_counter(&key, &metadata());
        c.increment(7);
        let snap = snapshot_from_inner(&rec.inner);
        let family = snap
            .metrics
            .iter()
            .find(|m| m.name == "test_counter")
            .expect("counter family present");
        assert_eq!(family.series.len(), 1);
        match &family.series[0].value {
            SeriesValue::Counter(v) => assert_eq!(*v, 7),
            other => panic!("expected counter value, got {other:?}"),
        }
        match &family.series[0].labels {
            SeriesLabels::Raw(m) => {
                assert_eq!(m.get("k").map(String::as_str), Some("v"));
            }
            other => panic!("expected raw labels in local snapshot, got {other:?}"),
        }
    }

    #[test]
    fn gauge_set_visible_in_snapshot() {
        let rec = make();
        let key = Key::from_parts("test_gauge", Vec::<Label>::new());
        let g = rec.register_gauge(&key, &metadata());
        g.set(3.5);
        let snap = snapshot_from_inner(&rec.inner);
        let family = snap
            .metrics
            .iter()
            .find(|m| m.name == "test_gauge")
            .expect("gauge family present");
        match &family.series[0].value {
            SeriesValue::Gauge(v) => assert!((*v - 3.5).abs() < f64::EPSILON),
            other => panic!("expected gauge value, got {other:?}"),
        }
    }

    #[test]
    fn histogram_summary_has_percentiles() {
        let rec = make();
        let key = Key::from_parts("test_hist", Vec::<Label>::new());
        let h = rec.register_histogram(&key, &metadata());
        for v in 1..=100 {
            h.record(f64::from(v));
        }
        let snap = snapshot_from_inner(&rec.inner);
        let family = snap
            .metrics
            .iter()
            .find(|m| m.name == "test_hist")
            .expect("histogram family present");
        match &family.series[0].value {
            SeriesValue::Histogram(s) => {
                assert_eq!(s.count, 100);
                assert!((s.sum - 5050.0).abs() < f64::EPSILON);
                // p50 lands near the median; p99 lands near the tail.
                assert!(s.p50 >= 49.0 && s.p50 <= 51.0, "p50={}", s.p50);
                assert!(s.p99 >= 98.0, "p99={}", s.p99);
            }
            other => panic!("expected histogram value, got {other:?}"),
        }
    }

    #[test]
    fn cardinality_cap_drops_new_label_sets_silently() {
        let rec = StatsRecorderBuilder::new("test")
            .cardinality_override("capped_metric", 16)
            .build();
        // Fill the cap.
        for i in 0..16 {
            let k = Key::from_parts("capped_metric", vec![Label::new("i", format!("{i}"))]);
            let c = rec.register_counter(&k, &metadata());
            c.increment(1);
        }
        // The 17th label-set is dropped (returns a noop handle).
        let extra = Key::from_parts("capped_metric", vec![Label::new("i", "16")]);
        let c = rec.register_counter(&extra, &metadata());
        c.increment(99); // noop — does not appear in snapshot
        let snap = snapshot_from_inner(&rec.inner);
        let family = snap
            .metrics
            .iter()
            .find(|m| m.name == "capped_metric")
            .expect("capped_metric family");
        assert_eq!(
            family.series.len(),
            16,
            "exactly the cap's worth of admitted series"
        );
        // The drops counter has fired exactly once with metric=capped_metric.
        let drops = snap
            .metrics
            .iter()
            .find(|m| m.name == CARDINALITY_DROPS_METRIC)
            .expect("drops counter present after cap-hit");
        assert_eq!(drops.series.len(), 1);
        match &drops.series[0].value {
            SeriesValue::Counter(v) => assert_eq!(*v, 1),
            other => panic!("expected drops counter value, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_label_set_does_not_consume_budget() {
        let rec = StatsRecorderBuilder::new("test")
            .cardinality_override("dup_metric", 16)
            .build();
        let k = Key::from_parts("dup_metric", vec![Label::new("k", "v")]);
        for _ in 0..50 {
            let c = rec.register_counter(&k, &metadata());
            c.increment(1);
        }
        let snap = snapshot_from_inner(&rec.inner);
        let family = snap
            .metrics
            .iter()
            .find(|m| m.name == "dup_metric")
            .expect("dup_metric family");
        assert_eq!(family.series.len(), 1, "single label-set across 50 calls");
        match &family.series[0].value {
            SeriesValue::Counter(v) => assert_eq!(*v, 50),
            other => panic!("expected counter, got {other:?}"),
        }
        // No drops should be recorded.
        assert!(
            snap.metrics
                .iter()
                .all(|m| m.name != CARDINALITY_DROPS_METRIC)
        );
    }

    #[test]
    fn duplicate_label_keys_consume_distinct_budget_slots() {
        // Anti-regression for Codex round-2 MAJOR / round-3 MAJOR.
        // The admission set uses `metrics::Key` directly (round-3 fix),
        // so distinct-by-`Key::eq` callsites always consume distinct
        // slots. Cap-bypass via duplicate-key collisions is impossible
        // by construction: the admission set's Hash+Eq IS the
        // Registry's Hash+Eq.
        let rec = StatsRecorderBuilder::new("test")
            .cardinality_override("dup_key_metric", 16)
            .build();
        for i in 0..16 {
            let k = Key::from_parts(
                "dup_key_metric",
                vec![Label::new("a", format!("{i}")), Label::new("a", "fixed")],
            );
            rec.register_counter(&k, &metadata()).increment(1);
        }
        // The 17th should be dropped — the cap is real.
        let overflow = Key::from_parts(
            "dup_key_metric",
            vec![Label::new("a", "16"), Label::new("a", "fixed")],
        );
        rec.register_counter(&overflow, &metadata()).increment(99);
        let snap = snapshot_from_inner(&rec.inner);
        let family = snap
            .metrics
            .iter()
            .find(|m| m.name == "dup_key_metric")
            .expect("dup_key_metric family present");
        assert_eq!(
            family.series.len(),
            16,
            "16 admitted Registry series under the cap (admission ≡ Registry)"
        );
        let drops = snap
            .metrics
            .iter()
            .find(|m| m.name == CARDINALITY_DROPS_METRIC)
            .expect("drops counter present after cap-hit on duplicate-key set");
        match &drops.series[0].value {
            SeriesValue::Counter(v) => assert_eq!(*v, 1),
            other => panic!("expected drops counter value, got {other:?}"),
        }
    }

    #[test]
    fn permuted_duplicate_keys_3_labels_are_admission_distinct() {
        // Anti-regression for Codex round-3 MAJOR. With 3 labels and
        // duplicate keys, `metrics::Key::eq` uses the n<8 branch:
        // stable sort by key only, then pairwise compare. So
        // `[(a,1),(a,2),(b,x)]` and `[(a,2),(a,1),(b,x)]` are
        // DISTINCT under Key::eq (the stable sort preserves the
        // original positional order of same-key pairs). The old
        // round-2 sort-unstable-by-(key,value) admission ID treated
        // them as IDENTICAL — one admission slot, two Registry
        // entries: a cap bypass via permutation. The round-3 fix
        // (HashSet<Key>) makes admission and Registry agree by
        // construction. Cap is 16; we register both permutations and
        // the cap-budget remaining should be 14, not 15.
        let rec = StatsRecorderBuilder::new("test")
            .cardinality_override("perm_metric", 16)
            .build();
        let k_a = Key::from_parts(
            "perm_metric",
            vec![
                Label::new("a", "1"),
                Label::new("a", "2"),
                Label::new("b", "x"),
            ],
        );
        let k_b = Key::from_parts(
            "perm_metric",
            vec![
                Label::new("a", "2"),
                Label::new("a", "1"),
                Label::new("b", "x"),
            ],
        );
        // Sanity: these two Keys are distinct under metrics::Key — if
        // upstream ever changed this, the cap-bypass concern itself
        // would vanish, but the test (and any code depending on it)
        // would also misreport.
        assert_ne!(
            k_a, k_b,
            "permuted duplicate-key 3-label Keys must be distinct"
        );
        rec.register_counter(&k_a, &metadata()).increment(1);
        rec.register_counter(&k_b, &metadata()).increment(1);
        let snap = snapshot_from_inner(&rec.inner);
        let family = snap
            .metrics
            .iter()
            .find(|m| m.name == "perm_metric")
            .expect("perm_metric family present");
        assert_eq!(
            family.series.len(),
            2,
            "permuted duplicate-key Keys consume two distinct admission slots, not one"
        );
        // No drops yet — we are well under the cap of 16.
        assert!(
            snap.metrics
                .iter()
                .all(|m| m.name != CARDINALITY_DROPS_METRIC),
            "cap not hit yet, drops counter must not appear"
        );
    }

    #[test]
    #[should_panic(expected = "out of bounds")]
    fn cap_below_floor_panics_at_build_time() {
        let _ = StatsRecorderBuilder::new("test").cardinality_override("bad", 8);
    }

    #[test]
    #[should_panic(expected = "out of bounds")]
    fn cap_above_ceiling_panics_at_build_time() {
        let _ = StatsRecorderBuilder::new("test").cardinality_override("bad", 8192);
    }

    #[test]
    fn reserved_builtin_name_registration_returns_noop() {
        // Anti-regression for Codex round-6 MINOR: an app
        // registering a gauge under a reserved built-in name
        // would otherwise collide with the side-map record in
        // `read_current`, emitting two `PeriodRecord`s under
        // one `(metric, labels)` per period. The contract is:
        // reserved names yield a noop handle, never reach the
        // Registry.
        let rec = make();
        for name in BUILTIN_GAUGE_RESERVED_NAMES {
            let key = Key::from_parts(*name, Vec::<Label>::new());
            // Counter, gauge, and histogram all refuse the name.
            rec.register_counter(&key, &metadata()).increment(999);
            rec.register_gauge(&key, &metadata()).set(999.0);
            rec.register_histogram(&key, &metadata()).record(999.0);
        }
        let snap = snapshot_from_inner(&rec.inner);
        for name in BUILTIN_GAUGE_RESERVED_NAMES {
            // The reserved name MAY still appear in the snapshot
            // because the side-map path surfaces built-in gauges
            // under it. What it must NOT do is appear with the
            // would-be app values, and there must be at most one
            // series under the gauge family (no duplicates).
            let count = snap.metrics.iter().filter(|m| m.name == *name).count();
            assert!(
                count <= 1,
                "reserved built-in name {name} produced {count} families"
            );
            if let Some(family) = snap.metrics.iter().find(|m| m.name == *name) {
                assert_eq!(
                    family.series.len(),
                    1,
                    "reserved-name family must have at most one series"
                );
                if let crate::stats::types::SeriesValue::Gauge(v) = &family.series[0].value {
                    assert!(
                        (*v - 999.0).abs() > f64::EPSILON,
                        "reserved-name series value {v} must NOT match the rejected app write of 999.0"
                    );
                }
            }
        }
    }

    #[test]
    fn snapshot_describe_text_surfaces_to_family() {
        let rec = make();
        rec.describe_counter(
            KeyName::from("text_metric"),
            None,
            SharedString::from("operator description"),
        );
        let key = Key::from_parts("text_metric", Vec::<Label>::new());
        rec.register_counter(&key, &metadata()).increment(1);
        let snap = snapshot_from_inner(&rec.inner);
        let family = snap
            .metrics
            .iter()
            .find(|m| m.name == "text_metric")
            .expect("family present");
        assert_eq!(family.description.as_deref(), Some("operator description"));
    }
}
