//! Stats subsystem — full P5 surface.
//!
//! Plan: `_doc/planned/cosmix-lib-log-stats.md`. This module owns
//! the frozen v1 public surface (§3.4). The pieces, by role:
//!
//! - **Recorder + classification:** `StatsRecorder` /
//!   `StatsRecorderBuilder` and the cardinality bounds; the
//!   `classify` table that pins every metric family as `Safe` or
//!   `Restricted`.
//! - **Snapshot data shape:** `Snapshot`, `MetricFamily`, `Series`,
//!   `LabelSensitivity`, `MetricKind`, and the canonical
//!   length-prefixed `labels_hash` encoding (§4.1).
//! - **Sinks (real `StatsSink` trait):** the JSONL `JsonlSink`. The
//!   cross-pillar `EventCounterLayer` and the built-in process
//!   gauges are not sinks but side rails that drive the same
//!   recorder.
//! - **Roll-up + dispatch:** `perform_rollup` / `flush_all_sinks` /
//!   `shutdown_installed_recorder`; `local_snapshot()` reads the
//!   installed recorder directly and always returns raw labels; the
//!   cap-gated `snapshot_dispatch` family that applies redaction
//!   projection backs the `<svc>.stats.snapshot` Bus verb.
//! - **Bus verb handler (`feature = "bus-handlers"`):** the
//!   `<svc>.stats.snapshot` Bus handler (`handle_snapshot_bus`),
//!   parsing the wire frame via `cosmix-lib-client`'s
//!   `IncomingCommand`. The cos-coupled property-namespace registration
//!   moved out of this crate to a cos extension crate.
//! - **Prometheus child (`feature = "prometheus"`):** the
//!   redaction-first `PrometheusChild` wrapper that pre-checks the
//!   cardinality cap before forwarding to the upstream exporter.
//!
//! # Three load-bearing design choices
//!
//! - **`Snapshot` carries its redaction posture in-band** (`types`):
//!   `SeriesLabels::{Raw, Hash}` makes whether a series is
//!   raw-labelled or hash-collapsed explicit in the data, not
//!   implicit in the call path (Codex round-16 MAJOR fix in the
//!   plan).
//! - **Default-restricted classification** (`classify`): every
//!   metric family declares `Safe` or `Restricted` at startup;
//!   unclassified families default to `Restricted` (the safer
//!   side — Codex round-14 MAJOR fix in the plan).
//! - **Canonical labels hash** (`labels_hash`): the length-prefixed
//!   FxHash encoding from §4.1. The same hash bytes appear in JSONL
//!   `labels_hash` records, in cardinality-cap warning events, and in
//!   `SeriesLabels::Hash` snapshot variants, so operators can
//!   correlate across surfaces.

#[cfg(feature = "bus-handlers")]
mod bus;
mod classify;
mod dispatch;
mod event_counter;
mod jsonl_sink;
mod labels_hash;
mod process_gauges;
#[cfg(feature = "prometheus")]
mod prometheus;
mod recorder;
mod rollup;
mod sink;
mod snapshot;
mod types;

#[cfg(feature = "bus-handlers")]
pub use bus::{SNAPSHOT_MAX_RESPONSE_BYTES, SnapshotCaps, handle_snapshot_bus};
// Canonical interval ceiling lives in `init`; re-exposed here so the
// prometheus gauge idle-timeout (and any consumer) reads it under the
// `stats::` path the original schema export used. Only the prometheus
// attach path references it today.
#[cfg(feature = "prometheus")]
pub(crate) use crate::init::INTERVAL_SECONDS_CEILING;
pub use classify::{classify, classify_default, sensitivity_of};
pub use dispatch::{LabelFilter, MetricPattern, SnapshotError, SnapshotRequest, snapshot_dispatch};
pub(crate) use event_counter::EventCounterLayer;
pub use jsonl_sink::{HARD_BUDGET_BYTES, JsonlSink, ProducerClass};
pub use labels_hash::{labels_hash, labels_hash_bytes};
#[cfg(feature = "prometheus")]
pub use prometheus::PrometheusChild;
pub use recorder::{
    CARDINALITY_CEILING, CARDINALITY_DEFAULT, CARDINALITY_DROPS_METRIC, CARDINALITY_FLOOR,
    InstallError, StatsRecorder, StatsRecorderBuilder, add_sink_to_installed, installed_recorder,
};
#[cfg(feature = "prometheus")]
pub use recorder::{
    PrometheusAttachError, precheck_prometheus_attachable, set_prometheus_child_on_installed,
};
pub use rollup::{flush_all_sinks, perform_rollup, shutdown_installed_recorder};
pub use sink::{PeriodRecord, PeriodSnapshot, PeriodValue, StatsSink};
pub use snapshot::local_snapshot;
pub use types::{
    HistogramSummary, LabelSensitivity, MetricFamily, MetricKind, Series, SeriesLabels,
    SeriesValue, Snapshot,
};
