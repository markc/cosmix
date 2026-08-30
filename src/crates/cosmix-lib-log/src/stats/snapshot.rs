//! In-process snapshot reader (plan §3.4 — `local_snapshot()`).
//!
//! Reads the installed [`StatsRecorder`]'s `Registry` and projects it
//! into the public `Snapshot` shape.
//!
//! # Why labels are always raw here
//!
//! Per plan §3.4, `local_snapshot()` returns `SeriesLabels::Raw` for
//! every series regardless of the family's classification. The
//! reasoning is trust-domain: the caller is *inside* the process and
//! already has full access to whatever data the recorder holds; there
//! is no information gain from hashing. The hash-redaction discipline
//! applies only to the JSONL on-disk path (slice 3) and the
//! cross-process Bus verb (slice 5), where the caller is *not*
//! in-process.
//!
//! # Histogram percentile computation
//!
//! Percentiles are computed by sorting the bucket's accumulated
//! `f64` values and indexing at `((n-1) * p).round()`. This is the
//! cheap "good enough for v1" approach — accurate for well-behaved
//! distributions, fast on the ~thousands-of-samples scale a single
//! family typically holds, and produces stable output across
//! identical inputs. Heavier estimators (HDR, t-digest) ride in v2
//! per plan §3.4; v1 trades exactness on multi-million-sample
//! distributions for zero-allocation simplicity.

use crate::stats::recorder::{RecorderInner, shared};
use crate::stats::types::{
    HistogramSummary, MetricFamily, MetricKind, Series, SeriesLabels, SeriesValue, Snapshot,
};
use metrics_util::AtomicBucket;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Read the current process's recorder registry into a `Snapshot`.
///
/// Returns an empty snapshot (no service, no metrics) if no recorder
/// has been installed in this process — the `--stats=off` / pre-init
/// / test-without-install case.
///
/// Available on every build (core and citizen); the recorder is
/// core-resident, per plan §3.4.
pub fn local_snapshot() -> Snapshot {
    let Some(inner) = shared() else {
        return Snapshot {
            service: String::new(),
            captured_at: chrono::Utc::now(),
            metrics: Vec::new(),
        };
    };
    snapshot_from_inner(&inner)
}

/// Build a snapshot from a specific `RecorderInner`. Exists primarily
/// for unit tests that construct a recorder without installing it
/// globally (slice 7 also relies on this seam for the local-recorder
/// helper).
pub(crate) fn snapshot_from_inner(inner: &Arc<RecorderInner>) -> Snapshot {
    // Key families by (name, kind) — not name alone — so the same
    // metric name registered as multiple kinds (legal under
    // `metrics::Recorder`) produces one MetricFamily per kind, never
    // a single family with a mismatched `kind` field vs `SeriesValue`
    // variants. (Codex round-2 MINOR fix.)
    let mut by_key: HashMap<(String, MetricKind), MetricFamily> = HashMap::new();

    inner.registry.visit_counters(|key, counter| {
        let labels = labels_from_key(key);
        let value = SeriesValue::Counter(counter.load(Ordering::Acquire));
        push_series(
            &mut by_key,
            key.name(),
            MetricKind::Counter,
            labels,
            value,
            inner,
        );
    });
    inner.registry.visit_gauges(|key, gauge| {
        let labels = labels_from_key(key);
        let bits = gauge.load(Ordering::Acquire);
        let value = SeriesValue::Gauge(f64::from_bits(bits));
        push_series(
            &mut by_key,
            key.name(),
            MetricKind::Gauge,
            labels,
            value,
            inner,
        );
    });
    inner.registry.visit_histograms(|key, bucket| {
        let labels = labels_from_key(key);
        let summary = summarise_bucket_for_rollup(bucket);
        let value = SeriesValue::Histogram(summary);
        push_series(
            &mut by_key,
            key.name(),
            MetricKind::Histogram,
            labels,
            value,
            inner,
        );
    });
    // Built-in process gauges live in the OUT-OF-BAND side map
    // (`RecorderInner.built_in_gauges`), not the metrics Registry —
    // see `process_gauges.rs` for why. Surface them here as
    // no-label gauges so `local_snapshot()` callers (and v2 Bus
    // verb slice) see the same union the roll-up driver fans out
    // to sinks. A missing name means the most recent rollup's
    // procfs read for that gauge failed; consumers see no series
    // rather than a stale value (Codex round-5 MAJOR fix).
    for (name, value) in inner.built_in_gauge_snapshot() {
        push_series(
            &mut by_key,
            name,
            MetricKind::Gauge,
            BTreeMap::new(),
            SeriesValue::Gauge(value),
            inner,
        );
    }

    let mut metrics: Vec<MetricFamily> = by_key.into_values().collect();
    // Name first (operator-meaningful primary sort), kind second
    // (stable tiebreaker for the rare same-name-different-kind case).
    metrics.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| compare_kind(&a.kind, &b.kind))
    });
    for family in &mut metrics {
        family
            .series
            .sort_by(|a, b| compare_series_labels(&a.labels, &b.labels));
    }
    Snapshot {
        service: inner.identity.clone(),
        captured_at: chrono::Utc::now(),
        metrics,
    }
}

fn compare_kind(a: &MetricKind, b: &MetricKind) -> std::cmp::Ordering {
    // Total ordering on MetricKind for the (name, kind) tiebreaker
    // sort. The variant order here is arbitrary but stable.
    fn rank(k: &MetricKind) -> u8 {
        match k {
            MetricKind::Counter => 0,
            MetricKind::Gauge => 1,
            MetricKind::Histogram => 2,
        }
    }
    rank(a).cmp(&rank(b))
}

fn labels_from_key(key: &metrics::Key) -> BTreeMap<String, String> {
    key.labels()
        .map(|l| (l.key().to_string(), l.value().to_string()))
        .collect()
}

fn push_series(
    by_key: &mut HashMap<(String, MetricKind), MetricFamily>,
    name: &str,
    kind: MetricKind,
    labels: BTreeMap<String, String>,
    value: SeriesValue,
    inner: &RecorderInner,
) {
    let entry = by_key.entry((name.to_string(), kind)).or_insert_with(|| {
        let description = inner.descriptions.read().ok().and_then(|d| {
            d.get(&(name.to_string(), kind))
                .and_then(|d| d.text.clone())
        });
        MetricFamily {
            name: name.to_string(),
            kind,
            description,
            series: Vec::new(),
        }
    });
    entry.series.push(Series {
        labels: SeriesLabels::Raw(labels),
        value,
    });
}

fn compare_series_labels(a: &SeriesLabels, b: &SeriesLabels) -> std::cmp::Ordering {
    use SeriesLabels::*;
    match (a, b) {
        (Raw(l), Raw(r)) => {
            let lv: Vec<_> = l.iter().collect();
            let rv: Vec<_> = r.iter().collect();
            lv.cmp(&rv)
        }
        (Hash(l), Hash(r)) => l.cmp(r),
        // Local snapshots never produce Hash; this ordering is for
        // future cross-surface use (JSONL/Bus) and is arbitrary but
        // total.
        (Raw(_), Hash(_)) => std::cmp::Ordering::Less,
        (Hash(_), Raw(_)) => std::cmp::Ordering::Greater,
    }
}

/// Compute count/sum/p50/p95/p99 over the histogram's accumulated
/// samples. Shared by `local_snapshot` and the roll-up driver; the
/// `_for_rollup` suffix exists to make the crate-internal call site
/// distinct from the historical private name.
pub(crate) fn summarise_bucket_for_rollup(bucket: &Arc<AtomicBucket<f64>>) -> HistogramSummary {
    let mut values = bucket.data();
    let count = values.len() as u64;
    let sum: f64 = values.iter().copied().sum();
    if values.is_empty() {
        return HistogramSummary {
            count: 0,
            sum: 0.0,
            p50: 0.0,
            p95: 0.0,
            p99: 0.0,
        };
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let pick = |p: f64| -> f64 {
        let n = values.len();
        let idx = ((n as f64 - 1.0) * p).round() as usize;
        values[idx.min(n - 1)]
    };
    HistogramSummary {
        count,
        sum,
        p50: pick(0.50),
        p95: pick(0.95),
        p99: pick(0.99),
    }
}
