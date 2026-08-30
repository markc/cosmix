//! `StatsSink` trait + period-roll-up payload types (plan §3.4, §4.1).
//!
//! The trait is the v1 storage-backend seam (Codex round-14 MAJOR fix
//! in the plan): the recorder calls `record_period` once per roll-up
//! period with the (metric, label-set, value, delta) tuples that
//! changed; `flush` is the `LogHandle::shutdown()` durability barrier.
//! v2 backends (SQLite, indexd, direct-Prometheus) slot in by
//! implementing this trait without touching the recorder.
//!
//! The trait deliberately doesn't carry the in-memory recorder state
//! across the boundary — only the *period delta*, which is what every
//! v2 backend can serialize. Cross-process aggregation lives on
//! `snapshot_since` (default `Unsupported`); JSONL implements it by
//! scanning the log dir, others by querying their store directly.
//!
//! # Why `PeriodValue` mirrors `SeriesValue` but is distinct
//!
//! `SeriesValue` (in `types.rs`) is the *snapshot* shape — what a
//! reader returns at point-in-time. `PeriodValue` is the *roll-up*
//! shape — what the recorder writes once per period to disk-class
//! backends. The two coincide *shape*-wise today (both fan out
//! Counter/Gauge/Histogram identically), but the snapshot surface is
//! v1-frozen against future readers while the roll-up shape may grow
//! variants in lockstep with v2 sink backends (a v2 SQLite backend
//! may want raw observation arrays, for example). Splitting them now
//! keeps the wire-frozen `SeriesValue` from picking up backend-only
//! variants.

use crate::stats::types::{HistogramSummary, LabelSensitivity, MetricKind, Snapshot};
use std::collections::BTreeMap;

/// Storage backend for the stats subsystem. Implementations are
/// responsible for their own buffering, fsync cadence, and rotation;
/// the recorder calls `record_period` strictly once per period per
/// sink.
pub trait StatsSink: Send + Sync {
    /// Called by the roll-up task once per period with the
    /// (metric, label-set, value, delta) tuples that changed.
    fn record_period(&self, period: &PeriodSnapshot) -> std::io::Result<()>;

    /// Called by `LogHandle::shutdown()`. After return the sink must
    /// have durably committed every `record_period` it accepted
    /// (fsync, rename, etc.) and rejected any further `record_period`
    /// calls — the sink is single-shot from `flush()` onwards.
    fn flush(&self) -> std::io::Result<()>;

    /// Optional snapshot read; used by `disk_snapshot` to aggregate
    /// across processes. JSONL implements this by scanning the log
    /// dir (slice 5+); the v1 default returns `Unsupported` so v2
    /// backends that don't aggregate compile without a stub.
    fn snapshot_since(&self, _since: chrono::DateTime<chrono::Utc>) -> std::io::Result<Snapshot> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "this sink does not support cross-process aggregation",
        ))
    }
}

/// One roll-up period's worth of data, handed to every installed sink.
#[derive(Debug, Clone)]
pub struct PeriodSnapshot {
    /// Period-end timestamp (UTC, milliseconds). The JSONL `ts` field
    /// is this value formatted per `cosmix-lib-log.md` §5 ISO-8601
    /// discipline.
    pub ts: chrono::DateTime<chrono::Utc>,
    /// Kernel hostname at recorder startup (mesh-DNS convention).
    pub host: String,
    /// Service identity (matches `LogDefaults.identity`).
    pub service: String,
    /// Roll-up cadence in seconds (`0` = on-exit flush, not a periodic
    /// emission).
    pub period_seconds: u32,
    /// One entry per (metric, label-set) that changed during the
    /// period.
    pub records: Vec<PeriodRecord>,
}

/// One (metric, label-set) row in a period. Carries both the
/// cumulative `value` and this-period `delta` so JSONL aggregators
/// can compute `rate()`-style figures without joining adjacent lines.
#[derive(Debug, Clone)]
pub struct PeriodRecord {
    pub metric: String,
    pub kind: MetricKind,
    /// Label-sensitivity classification, looked up by the recorder
    /// at roll-up time from the process-wide `classify` registry
    /// (plan §3.3.2). The sink uses this to choose `labels` vs
    /// `labels_hash` per record.
    pub sensitivity: LabelSensitivity,
    pub labels: BTreeMap<String, String>,
    /// Cumulative-since-process-start value (counter total, current
    /// gauge reading, full-sketch histogram summary).
    pub value: PeriodValue,
    /// This-period delta (counter increase, gauge change since
    /// previous emitted line, histogram count/sum diff + this-period
    /// quantiles — see plan §4.1).
    pub delta: PeriodValue,
}

/// Roll-up value, parallel to `SeriesValue` but used on the writer
/// side. The variant always matches the owning record's `MetricKind`;
/// mismatched pairings are a bug in the roll-up task.
#[derive(Debug, Clone)]
pub enum PeriodValue {
    Counter(u64),
    Gauge(f64),
    Histogram(HistogramSummary),
}
