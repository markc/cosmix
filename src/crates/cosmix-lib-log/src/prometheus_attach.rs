//! `LogHandle::attach_prometheus` backing module (plan §8.1).
//!
//! Builds a `PrometheusRecorder` + `ExporterFuture` from
//! `PrometheusBuilder::with_http_listener(addr).build()`, wraps the
//! recorder in the redaction-first [`PrometheusChild`] shim, slots
//! it into the already-installed substrate `StatsRecorder` via
//! `set_prometheus_child_on_installed`, and spawns the exporter
//! future on the caller's Tokio runtime. `PrometheusBuilder::install`
//! is deliberately *not* used — it would try to take the global
//! `metrics::Recorder` slot the substrate `StatsRecorder` already
//! owns, defeating the redaction-first policy facade.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use metrics_exporter_prometheus::PrometheusBuilder;
// Use the `metrics-util` version that `metrics-exporter-prometheus`
// itself depends on (currently 0.20), not the workspace pin (0.18).
// `PrometheusBuilder::idle_timeout` resolves `MetricKindMask` against
// its own dep tree; the two versions are distinct types to the type
// system even when their wire shape matches. See the `prometheus`
// feature in this crate's Cargo.toml for the alias rationale.
use metrics_util_prometheus::MetricKindMask;

/// Idle timeout for `Gauge`-kind metrics on the Prometheus exporter.
///
/// Codex round-4 MAJOR: when a built-in process gauge's procfs read
/// fails for a period, the recorder side map drops the entry (so
/// JSONL/Bus correctly emit no record), but a previously-registered
/// gauge in the Prometheus registry would otherwise keep rendering
/// its last value indefinitely (`idle_timeout` defaults to `None` in
/// `PrometheusBuilder::new`). Setting an idle timeout scoped to
/// `Gauge` causes the exporter to evict gauges that have not been
/// `set` for the timeout window, restoring the "series went dark"
/// semantic on `/metrics`.
///
/// Codex round-5 MAJOR: the timeout must be larger than the longest
/// allowed `stats.interval_seconds`, otherwise a successful rollup at
/// a high-interval setting cannot refresh the gauge before the
/// exporter declares it idle (a false-dark regression). The timeout
/// is derived from [`crate::stats::INTERVAL_SECONDS_CEILING`] as
/// `2× CEILING + 60 s` (jitter buffer for rollup-loop scheduling
/// skew and sink fanout under load), so bumping the ceiling
/// propagates here automatically — there is no second constant to
/// keep aligned. Both the substrate `before_set` and the
/// `init::install_stats_recorder` CLI/defaults path enforce the
/// matching upper bound on `interval_seconds` so this exporter never
/// sees a cadence it cannot survive.
///
/// Counter and histogram metrics are deliberately excluded from the
/// mask: a counter whose increments stop is *not* stale, its last
/// total is still the correct cumulative value.
const PROMETHEUS_GAUGE_IDLE_TIMEOUT: Duration =
    Duration::from_secs(crate::stats::INTERVAL_SECONDS_CEILING * 2 + 60);

use crate::handle::LogError;
use crate::stats::{
    PrometheusAttachError, PrometheusChild, precheck_prometheus_attachable,
    set_prometheus_child_on_installed,
};

/// Build the Prometheus exporter, attach it to the installed
/// `StatsRecorder`, and spawn the HTTP listener future on the
/// current Tokio runtime. Must be called with a runtime in scope —
/// `PrometheusBuilder::build()` spawns its upkeep task with
/// `tokio::spawn` regardless of whether we then spawn the
/// listener future.
pub(crate) async fn attach(addr: SocketAddr) -> Result<(), LogError> {
    // Codex round-3 MAJOR: `PrometheusBuilder::build()` unconditionally
    // spawns a background upkeep task before returning. Errors out of
    // `build()` (bind failure) or out of `set_prometheus_child_on_installed`
    // (no recorder, slot already occupied) would otherwise leak the
    // task for the lifetime of the process. Pre-check the two
    // known-bad install states before we ever call `build()`.
    map_attach_err(precheck_prometheus_attachable())?;

    let (recorder, exporter) = PrometheusBuilder::new()
        .with_http_listener(addr)
        .idle_timeout(MetricKindMask::GAUGE, Some(PROMETHEUS_GAUGE_IDLE_TIMEOUT))
        .build()
        .map_err(|e| {
            LogError::InvalidStats(format!("Prometheus exporter build failed for {addr}: {e}"))
        })?;

    let child = Arc::new(PrometheusChild::new(recorder));
    // The post-build install can still race a concurrent attach call
    // (caller misuse — the contract is attach-once), in which case
    // the upkeep task from this `build()` leaks for the process
    // lifetime. The pre-check above covers the non-race failure modes.
    map_attach_err(set_prometheus_child_on_installed(child))?;

    // Plan §8.1: the exporter future is the long-running listener;
    // spawn it on the daemon's runtime so it lives until the
    // process exits. Result is ignored — a listener crash logs
    // through tracing, the recorder keeps serving JSONL/Bus.
    tokio::spawn(async move {
        if let Err(e) = exporter.await {
            // `ExporterError` from metrics-exporter-prometheus 0.18
            // does not implement `Display`; fall back to `Debug`.
            tracing::error!(
                target: "cosmix_log::stats",
                error = ?e,
                addr = %addr,
                "Prometheus HTTP exporter exited with error"
            );
        }
    });

    Ok(())
}

/// Map a `PrometheusAttachError` from the recorder layer to a
/// `LogError::InvalidStats` with the same operator-facing message
/// shape as before the round-3 refactor. Used by both the precheck
/// and the post-build install so a caller cannot tell which side
/// rejected the attach (the diagnostic is in the message text).
fn map_attach_err(result: Result<(), PrometheusAttachError>) -> Result<(), LogError> {
    match result {
        Ok(()) => Ok(()),
        Err(PrometheusAttachError::NoRecorder) => Err(LogError::InvalidStats(
            "no StatsRecorder is installed; --stats=on must be enabled before \
             attach_prometheus"
                .into(),
        )),
        Err(PrometheusAttachError::AlreadyAttached) => Err(LogError::InvalidStats(
            "a PrometheusChild is already attached to this process's StatsRecorder".into(),
        )),
    }
}
