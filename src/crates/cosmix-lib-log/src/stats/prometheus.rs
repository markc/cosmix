//! Redaction-first Prometheus child recorder (plan §3.3.2 + §8.1).
//!
//! `PrometheusChild` wraps a `metrics-exporter-prometheus`
//! `PrometheusRecorder` behind the substrate's label-sensitivity
//! classification. The substrate `StatsRecorder` slots this child into
//! its register hot path via `set_prometheus_child`; every
//! `register_*` call that survives the per-metric cardinality cap
//! (plan §3.3.1 + §3.3.2 cap-ordering rule) is also registered on the
//! child after Restricted-family labels have been rewritten to a
//! single `labels_hash` label.
//!
//! # Why a custom `Recorder` shape and not `FanoutBuilder`
//!
//! `metrics_util::layers::FanoutBuilder(StatsRecorder, PrometheusRecorder)`
//! would deliver the *raw* `metrics::Key` (label values included) to
//! the Prometheus exporter on every macro call — the substrate's
//! classification table never gets the chance to rewrite labels before
//! the Prometheus registry stores them. The plan §3.3.2 cap on raw
//! user-controlled bytes would then be JSONL+Bus only and `/metrics`
//! would become the operator-visible cardinality leak the
//! classification exists to prevent. The shipped topology is
//! *redaction-first*: `StatsRecorder` is the only global recorder,
//! and the Prometheus child sees redacted `Key`s only.
//!
//! # Why a separate Counter handle fan-out (rather than wrapping
//! # `metrics::Counter::from_arc` at construction time)
//!
//! Operationally: any `metrics::Counter` (and `Gauge` / `Histogram`)
//! handle a caller acquires from `register_*` keeps pointing at the
//! exact backing `metrics::CounterFn` it was created with. The macros
//! re-register each expansion, but daemon code commonly *also*
//! caches handles in a struct field or `OnceLock` to avoid the macro
//! registry-lookup on every increment. Any handle obtained *before*
//! `attach_prometheus()` will never fan out to the Prometheus child —
//! it was minted as a primary-only handle. Fan-out at *register* time
//! (when `attach_prometheus` runs *before* any daemon-side `counter!()`
//! by construction — the Bus register path runs first) preserves the
//! invariant for any subsequent cached handle. Codex round-3 MINOR.
//!
//! The same operational rule covers internal recorder paths that
//! bypass the `Recorder::register_*` trait surface (cardinality-drops
//! counter, process gauges): they fan out via [`Self::fan_internal_counter`]
//! and [`Self::fan_internal_gauge`] at the same callsites that update
//! the primary, so `/metrics` mirrors JSONL/Bus for built-ins too
//! (Codex round-3 MAJOR).

use crate::stats::classify::sensitivity_of;
use crate::stats::labels_hash::labels_hash;
use crate::stats::types::LabelSensitivity;
use metrics::{
    Counter, Gauge, Histogram, Key, KeyName, Label, Level, Metadata, Recorder, SharedString, Unit,
};
use metrics_exporter_prometheus::{PrometheusHandle, PrometheusRecorder};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Synthetic `Metadata` used when the substrate fans out internal
/// recorder paths (cardinality-drops counter, process gauges) to the
/// Prometheus child. These paths bypass the `Recorder::register_*`
/// trait surface and so do not carry per-callsite metadata; the
/// Prometheus recorder does not consume `Metadata` for storage, so
/// supplying a fixed `Substrate / INFO` value is sufficient and
/// consistent across periods.
const SUBSTRATE_META: Metadata<'static> = Metadata::new(
    "cosmix_log::stats",
    Level::INFO,
    Some("cosmix_lib_log::stats"),
);

/// Redaction-first wrapper around a `PrometheusRecorder`. Held by the
/// substrate `StatsRecorder` (one slot, set via
/// `set_prometheus_child`) and consulted on every `register_*` call
/// that passed the per-metric cardinality cap.
pub struct PrometheusChild {
    inner: PrometheusRecorder,
}

impl PrometheusChild {
    /// Wrap an existing `PrometheusRecorder` for fan-out from
    /// `StatsRecorder`. The recorder must not be installed as the
    /// global `metrics::Recorder` — the substrate `StatsRecorder`
    /// owns that role.
    pub fn new(inner: PrometheusRecorder) -> Self {
        Self { inner }
    }

    /// Render the current Prometheus exposition output. Test-only
    /// affordance; production scrapes go through the HTTP listener
    /// (`PrometheusBuilder::with_http_listener` + `build()` + the
    /// returned `ExporterFuture`).
    pub fn handle(&self) -> PrometheusHandle {
        self.inner.handle()
    }

    /// Rewrite `key`'s labels for the Prometheus child per the
    /// per-family `LabelSensitivity` classification (plan §3.3.2):
    /// - `Safe` families pass through unchanged.
    /// - `Restricted` families collapse to a single
    ///   `{"labels_hash": <16-hex>}` label using the canonical
    ///   `labels_hash` encoding (§4.1) — the same digest that
    ///   appears on JSONL `labels_hash` records and in
    ///   `SeriesLabels::Hash` Bus responses. Operators correlate
    ///   across surfaces by hash.
    ///
    /// The unclassified default is `Restricted`, matching
    /// `classify::sensitivity_of` — a family that hasn't been
    /// declared `Safe` cannot leak raw label bytes through
    /// `/metrics`.
    fn redact(&self, key: &Key) -> Key {
        if sensitivity_of(key.name()) == LabelSensitivity::Safe {
            return key.clone();
        }
        let map: BTreeMap<String, String> = key
            .labels()
            .map(|l| (l.key().to_string(), l.value().to_string()))
            .collect();
        let digest = labels_hash(&map);
        Key::from_parts(
            key.name().to_string(),
            vec![Label::new("labels_hash", digest)],
        )
    }

    pub(crate) fn register_counter(&self, key: &Key, metadata: &Metadata<'_>) -> Counter {
        let redacted = self.redact(key);
        self.inner.register_counter(&redacted, metadata)
    }

    pub(crate) fn register_gauge(&self, key: &Key, metadata: &Metadata<'_>) -> Gauge {
        let redacted = self.redact(key);
        self.inner.register_gauge(&redacted, metadata)
    }

    pub(crate) fn register_histogram(&self, key: &Key, metadata: &Metadata<'_>) -> Histogram {
        let redacted = self.redact(key);
        self.inner.register_histogram(&redacted, metadata)
    }

    pub(crate) fn describe_counter(
        &self,
        name: KeyName,
        unit: Option<Unit>,
        description: SharedString,
    ) {
        self.inner.describe_counter(name, unit, description);
    }

    pub(crate) fn describe_gauge(
        &self,
        name: KeyName,
        unit: Option<Unit>,
        description: SharedString,
    ) {
        self.inner.describe_gauge(name, unit, description);
    }

    pub(crate) fn describe_histogram(
        &self,
        name: KeyName,
        unit: Option<Unit>,
        description: SharedString,
    ) {
        self.inner.describe_histogram(name, unit, description);
    }

    /// Fan an internal recorder-owned counter increment to the
    /// Prometheus child. Used by `RecorderInner::record_drop` for the
    /// built-in cardinality-drops counter, which writes directly into
    /// `inner.registry` (bypassing the public `Recorder::register_counter`
    /// path the substrate fan-out hooks into). Codex round-3 MAJOR.
    ///
    /// Redaction obeys the same per-family classification used by the
    /// register-time path — the drops counter is intentionally
    /// `Restricted` (see `classify::classify_built_in_metrics`), so
    /// the `metric=<rejected_family>` label collapses to a
    /// `labels_hash` on `/metrics`.
    pub(crate) fn fan_internal_counter(&self, key: &Key, value: u64) {
        let counter = self.register_counter(key, &SUBSTRATE_META);
        counter.increment(value);
    }

    /// Fan an internal recorder-owned gauge write to the Prometheus
    /// child. Used by `process_gauges::update_process_gauges` for the
    /// built-in `cosmix_process_*` gauges, which live in a recorder
    /// side map (not the `metrics::Registry`) so the public
    /// `register_gauge` fan-out path never sees them. Re-registering
    /// the gauge per period is correct because the underlying
    /// `PrometheusRecorder` caches handles by `Key` hash, so each
    /// period writes through the same backing storage. Codex round-3
    /// MAJOR.
    pub(crate) fn fan_internal_gauge(&self, key: &Key, value: f64) {
        let gauge = self.register_gauge(key, &SUBSTRATE_META);
        gauge.set(value);
    }
}

/// Composite `metrics::CounterFn` that drives both the substrate
/// `AtomicU64` and the redaction-wrapped Prometheus counter for the
/// same `(metric, labels)` pair. Built once per surviving
/// `register_counter` call when a `PrometheusChild` is attached.
pub(crate) struct FanCounter {
    pub(crate) primary: Counter,
    pub(crate) secondary: Counter,
}

impl metrics::CounterFn for FanCounter {
    fn increment(&self, value: u64) {
        self.primary.increment(value);
        self.secondary.increment(value);
    }

    fn absolute(&self, value: u64) {
        self.primary.absolute(value);
        self.secondary.absolute(value);
    }
}

/// Composite `metrics::GaugeFn` — see `FanCounter` for the cache
/// rationale. `set` writes the value to both backends; `increment`
/// and `decrement` apply the same delta to both.
pub(crate) struct FanGauge {
    pub(crate) primary: Gauge,
    pub(crate) secondary: Gauge,
}

impl metrics::GaugeFn for FanGauge {
    fn increment(&self, value: f64) {
        self.primary.increment(value);
        self.secondary.increment(value);
    }

    fn decrement(&self, value: f64) {
        self.primary.decrement(value);
        self.secondary.decrement(value);
    }

    fn set(&self, value: f64) {
        self.primary.set(value);
        self.secondary.set(value);
    }
}

/// Composite `metrics::HistogramFn` — see `FanCounter` for the cache
/// rationale. Each `record` call delivers the same sample to both
/// histograms; Prometheus reads its own internal bucket counts via
/// `/metrics`, the substrate computes percentiles from
/// `metrics_util::AtomicBucket` for Bus/JSONL.
pub(crate) struct FanHistogram {
    pub(crate) primary: Histogram,
    pub(crate) secondary: Histogram,
}

impl metrics::HistogramFn for FanHistogram {
    fn record(&self, value: f64) {
        self.primary.record(value);
        self.secondary.record(value);
    }
}

/// Build the composite Counter for a `(substrate, prometheus)` pair.
/// Returned as a `metrics::Counter` so the recorder's hot path stays
/// type-uniform whether a Prometheus child is attached or not.
pub(crate) fn fan_counter(primary: Counter, secondary: Counter) -> Counter {
    Counter::from_arc(Arc::new(FanCounter { primary, secondary }))
}

/// Build the composite Gauge for a `(substrate, prometheus)` pair.
pub(crate) fn fan_gauge(primary: Gauge, secondary: Gauge) -> Gauge {
    Gauge::from_arc(Arc::new(FanGauge { primary, secondary }))
}

/// Build the composite Histogram for a `(substrate, prometheus)` pair.
pub(crate) fn fan_histogram(primary: Histogram, secondary: Histogram) -> Histogram {
    Histogram::from_arc(Arc::new(FanHistogram { primary, secondary }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::classify::classify;
    use metrics_exporter_prometheus::PrometheusBuilder;

    fn metadata() -> Metadata<'static> {
        Metadata::new("test", metrics::Level::INFO, None)
    }

    fn build_child() -> PrometheusChild {
        PrometheusChild::new(PrometheusBuilder::new().build_recorder())
    }

    #[test]
    fn safe_family_passes_labels_through() {
        classify("prom_safe_family", LabelSensitivity::Safe);
        let child = build_child();
        let key = Key::from_parts(
            "prom_safe_family",
            vec![Label::new("kind", "click"), Label::new("user", "ada")],
        );
        let c = child.register_counter(&key, &metadata());
        c.increment(3);
        let rendered = child.handle().render();
        assert!(
            rendered.contains("prom_safe_family"),
            "metric name missing from /metrics output: {rendered}"
        );
        assert!(
            rendered.contains("kind=\"click\""),
            "Safe family must keep raw label `kind`: {rendered}"
        );
        assert!(
            rendered.contains("user=\"ada\""),
            "Safe family must keep raw label `user`: {rendered}"
        );
        assert!(
            !rendered.contains("labels_hash="),
            "Safe family must not emit a labels_hash label: {rendered}"
        );
    }

    #[test]
    fn restricted_family_collapses_to_labels_hash() {
        // Default classification is Restricted — leave the family
        // unclassified to assert the safer-side default.
        let child = build_child();
        let key = Key::from_parts(
            "prom_restricted_family",
            vec![Label::new("email", "user@example.com")],
        );
        let c = child.register_counter(&key, &metadata());
        c.increment(1);
        let rendered = child.handle().render();
        assert!(
            rendered.contains("prom_restricted_family"),
            "metric name missing: {rendered}"
        );
        assert!(
            !rendered.contains("user@example.com"),
            "Restricted family must NOT leak raw label value: {rendered}"
        );
        assert!(
            !rendered.contains("email=\""),
            "Restricted family must NOT expose the raw label key: {rendered}"
        );
        assert!(
            rendered.contains("labels_hash=\""),
            "Restricted family must emit a single labels_hash label: {rendered}"
        );
    }

    #[test]
    fn redact_matches_canonical_labels_hash() {
        // The on-wire digest in /metrics must equal the digest the
        // JSONL writer / Bus verb would emit for the same label-set
        // — operators correlate across surfaces by hash.
        let child = build_child();
        let key = Key::from_parts(
            "prom_corr_family",
            vec![Label::new("a", "1"), Label::new("b", "2")],
        );
        let expected = labels_hash(&BTreeMap::from([
            ("a".to_string(), "1".to_string()),
            ("b".to_string(), "2".to_string()),
        ]));
        child.register_counter(&key, &metadata()).increment(1);
        let rendered = child.handle().render();
        assert!(
            rendered.contains(&format!("labels_hash=\"{expected}\"")),
            "expected labels_hash={expected} in: {rendered}"
        );
    }

    #[test]
    fn fan_internal_counter_increments_show_on_metrics() {
        // Codex round-3 MAJOR: built-in cardinality-drops counter
        // bypasses `Recorder::register_counter`. `fan_internal_counter`
        // is the path that mirrors it onto /metrics. Classify the
        // family Safe so the assertion can match the raw label.
        classify("prom_internal_safe_counter", LabelSensitivity::Safe);
        let child = build_child();
        let key = Key::from_parts(
            "prom_internal_safe_counter",
            vec![Label::new("metric", "some_rejected_family")],
        );
        child.fan_internal_counter(&key, 2);
        child.fan_internal_counter(&key, 3);
        let rendered = child.handle().render();
        assert!(
            rendered.contains("prom_internal_safe_counter"),
            "internal counter must appear on /metrics: {rendered}"
        );
        // Same backing handle across calls => increments accumulate.
        assert!(
            rendered.contains("prom_internal_safe_counter{metric=\"some_rejected_family\"} 5"),
            "increments via fan_internal_counter should accumulate to 5: {rendered}"
        );
    }

    #[test]
    fn fan_internal_gauge_writes_show_on_metrics() {
        // Codex round-3 MAJOR: built-in process gauges live in a
        // recorder side map. `fan_internal_gauge` is the path that
        // mirrors them onto /metrics. Process-gauge family names are
        // classified Safe at recorder build time; classify here so the
        // test doesn't depend on global state from another test.
        classify("prom_internal_gauge", LabelSensitivity::Safe);
        let child = build_child();
        let key = Key::from_name("prom_internal_gauge");
        child.fan_internal_gauge(&key, 1.0);
        // Re-write the same key with a fresh value — the prometheus
        // recorder caches by key, so the second write should overwrite,
        // not double-emit.
        child.fan_internal_gauge(&key, 42.0);
        let rendered = child.handle().render();
        assert!(
            rendered.contains("prom_internal_gauge 42"),
            "expected the most recent gauge write to win: {rendered}"
        );
    }

    #[test]
    fn empty_label_set_restricted_still_redacts() {
        // A Restricted family with no labels still surfaces under the
        // labels_hash convention — the digest of an empty BTreeMap is
        // stable and operators can match it against
        // `labels_hash([])` from the canonical encoding.
        let child = build_child();
        let key = Key::from_parts("prom_restricted_empty", Vec::<Label>::new());
        child.register_counter(&key, &metadata()).increment(1);
        let rendered = child.handle().render();
        let expected = labels_hash(&BTreeMap::new());
        assert!(
            rendered.contains(&format!("labels_hash=\"{expected}\"")),
            "empty-label Restricted family should still expose labels_hash={expected}: {rendered}"
        );
    }
}
