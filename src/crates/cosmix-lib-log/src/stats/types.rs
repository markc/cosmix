//! Public snapshot types (plan §3.4).
//!
//! Pure data — no I/O, no recorder state, no thread-locals. The
//! `local_snapshot()` reader (slice 2b), the `disk_snapshot()` reader
//! (slice 3), and the `<svc>.stats.snapshot` Bus verb (slice 5) all
//! return these types verbatim; the JSONL line schema in §4.1 is the
//! serialized form of `Series` + producer metadata.

use std::collections::BTreeMap;

/// Frozen v1 snapshot shape returned by every read path
/// (`local_snapshot`, `disk_snapshot`, Bus verb).
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub service: String,
    pub captured_at: chrono::DateTime<chrono::Utc>,
    pub metrics: Vec<MetricFamily>,
}

/// One metric family — counter, gauge, or histogram — and every
/// label-set currently active under it.
#[derive(Debug, Clone)]
pub struct MetricFamily {
    pub name: String,
    pub kind: MetricKind,
    pub description: Option<String>,
    pub series: Vec<Series>,
}

/// One label-set's current observation.
#[derive(Debug, Clone)]
pub struct Series {
    pub labels: SeriesLabels,
    pub value: SeriesValue,
}

/// Label representation on a `Series`. Exactly one variant is
/// populated per series; the variant matches the owning family's
/// `LabelSensitivity` at snapshot time and the JSONL XOR invariant
/// (§4.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeriesLabels {
    /// Family is `Safe`, or family is `Restricted` and the Bus
    /// caller holds `stats.snapshot:raw-labels:<svc>`, or the
    /// snapshot is in-process (`local_snapshot()`).
    Raw(BTreeMap<String, String>),
    /// Family is `Restricted` and the snapshot path requires
    /// redaction (JSONL aggregation; cross-process Bus without the
    /// restricted cap). The string is the canonical 16-hex-char
    /// FxHash digest defined in §4.1 (the [`crate::stats::labels_hash`]
    /// helper).
    Hash(String),
}

/// What kind of metric the family records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MetricKind {
    Counter,
    Gauge,
    Histogram,
}

/// A single series' current value. The variant matches the family's
/// `MetricKind`; mismatched pairings are a bug in the recorder and
/// never reach the reader.
#[derive(Debug, Clone)]
pub enum SeriesValue {
    Counter(u64),
    Gauge(f64),
    Histogram(HistogramSummary),
}

/// Histogram summary persisted to JSONL (per §4.1) and returned by
/// the snapshot. The full HDR/t-digest sketch lives in memory only;
/// v2 may persist sketches alongside.
#[derive(Debug, Clone, PartialEq)]
pub struct HistogramSummary {
    pub count: u64,
    pub sum: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
}

/// Label-sensitivity classification (plan §3.3.2). Controls whether
/// snapshot/JSONL paths expose label *values* verbatim or write a
/// hashed form.
///
/// Default for an unclassified family is `Restricted` — the safer
/// side. Every metric family declared by `cosmix-lib-log`,
/// `cosmix-mix`, and `cosmix-lib-mix` MUST call
/// [`crate::stats::classify`] at startup before any record is
/// written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelSensitivity {
    /// Labels are bounded enums known to be non-sensitive (verdict,
    /// rcode, level, phase, target_root, kind). JSONL records and
    /// Bus snapshot responses write label values verbatim; Loki /
    /// Prometheus shipping is safe by construction.
    Safe,
    /// Labels may carry user-controlled bytes that cross trust
    /// boundaries. JSONL records carry `labels_hash` in place of raw
    /// `labels`; the `<svc>.stats.snapshot` Bus verb returns the raw
    /// labels only to callers holding
    /// `stats.snapshot:raw-labels:<svc>`.
    Restricted,
}
