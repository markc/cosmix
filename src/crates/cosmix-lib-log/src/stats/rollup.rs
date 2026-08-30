//! Roll-up driver — walks the in-memory recorder registry once
//! per period, then hands a per-sink `PeriodSnapshot` to every
//! installed [`StatsSink`] (plan §3.4, §4.1).
//!
//! # The shape of a period
//!
//! 1. Read the registry's current cumulative values into an
//!    immutable `CurrentReadings` snapshot.
//! 2. For each installed sink: take that sink's own `PrevState`
//!    lock, compute deltas against the current readings, build a
//!    `PeriodSnapshot`, hand it to the sink.
//!    - **Counter**: `delta = current - prev` (saturating). The
//!      first sighting of a key emits a one-line registration
//!      (`value == delta == current`); subsequent sightings with
//!      `delta == 0` are skipped (no churn lines on idle counters).
//!    - **Gauge**: `value = current`, `delta = current - prev`
//!      (signed via `f64` subtraction). Every gauge sample is
//!      emitted — gauges are point-in-time and consumers
//!      (`disk_snapshot`, Prometheus exposers) need the latest
//!      reading regardless of change.
//!    - **Histogram**: `value` is the cumulative summary over every
//!      sample the bucket has ever accepted; `delta` carries the
//!      count/sum diff since the previous accepted period plus the
//!      same cumulative quantiles. (Per-period quantile computation
//!      requires draining the bucket each period and maintaining a
//!      separate cumulative sample reservoir; that refinement is
//!      tracked for v2. The count/sum delta is precise.)
//! 3. If the sink's `record_period` returns `Ok`, commit the
//!    advanced prev to the sink slot; if it returns `Err`, the
//!    prev is **not** advanced — the failing sink will see merged
//!    deltas spanning the missed period on its next successful
//!    accept (Codex round-4 MAJOR fix).
//!
//! # Per-sink `PrevState`
//!
//! Each installed sink owns its own `PrevState` slot inside
//! `RecorderInner.sinks`. The plan §4.1 contract — "delta is the
//! change since the previous emitted line for the same (metric,
//! labels)" — is per-sink, not per-recorder: a sink that rejected
//! period N must see (N - already-accepted + N+1) on its next
//! accept, while sinks that accepted N advance independently. A
//! shared prev would mis-attribute deltas under any partial
//! fan-out failure.

use crate::stats::classify::sensitivity_of;
use crate::stats::process_gauges::update_process_gauges;
use crate::stats::recorder::{RecorderInner, StatsRecorder};
use crate::stats::sink::{PeriodRecord, PeriodSnapshot, PeriodValue, StatsSink};
use crate::stats::snapshot::summarise_bucket_for_rollup;
use crate::stats::types::{HistogramSummary, MetricKind};
use metrics::Key;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

/// Per-sink previous-period state. Owned by [`InstalledSink`] and
/// only advanced when that sink's `record_period` returns `Ok`.
#[derive(Default, Clone)]
pub(crate) struct PrevState {
    counters: HashMap<Key, u64>,
    gauges: HashMap<Key, f64>,
    histograms: HashMap<Key, HistogramPrev>,
}

#[derive(Clone, Copy)]
struct HistogramPrev {
    count: u64,
    sum: f64,
}

impl PrevState {
    pub(crate) fn new() -> Self {
        Self::default()
    }
}

/// A sink registered with the recorder, paired with the
/// per-sink `PrevState` the roll-up driver uses to frame its
/// deltas (plan §4.1; Codex round-4 MAJOR).
pub(crate) struct InstalledSink {
    pub(crate) sink: Arc<dyn StatsSink>,
    pub(crate) prev: Mutex<PrevState>,
}

/// Drive a single roll-up period.
///
/// `period_seconds = 0` is the on-exit / one-shot variant — the
/// JSONL `period` field carries `0` so downstream readers can
/// distinguish "this is the final line" from "this is a periodic
/// emission". The driver does not interpret `0` specially
/// otherwise; rate-style aggregation that wants to divide by
/// `period` must skip zero-period lines.
///
/// Returns the number of sinks that successfully accepted the
/// snapshot. Sinks that returned `Err` are logged at
/// `error`-level via `tracing` and counted as failed; their
/// per-sink prev is preserved so the next successful accept
/// covers both periods.
pub fn perform_rollup(recorder: &StatsRecorder, period_seconds: u32) -> usize {
    perform_rollup_inner(&recorder.inner, period_seconds)
}

pub(crate) fn perform_rollup_inner(inner: &Arc<RecorderInner>, period_seconds: u32) -> usize {
    // Roll-up-wide serialisation: read-current → per-sink-compute →
    // commit must be atomic w.r.t. other `perform_rollup` calls on
    // the same recorder, otherwise a slower call can commit an
    // older candidate after a faster call already advanced a sink's
    // prev — rewinding it and producing duplicate/missing deltas
    // next period (Codex round-5 MAJOR).
    let _rollup_guard = inner
        .rollup_lock
        .lock()
        .expect("stats rollup_lock Mutex poisoned");
    // Refresh the built-in process gauges before reading current
    // so the same `CurrentReadings` snapshot carries them to every
    // sink (plan §3 — "every binary, free, no app code needed").
    update_process_gauges(inner);
    let current = read_current(inner);
    let sinks: Vec<Arc<InstalledSink>> = {
        let guard = inner.sinks.lock().expect("stats sinks Mutex poisoned");
        guard.clone()
    };
    let ts = chrono::Utc::now();
    let mut ok_count = 0;
    for installed in &sinks {
        let mut prev_guard = installed
            .prev
            .lock()
            .expect("InstalledSink prev Mutex poisoned");
        // Compute against a *candidate* clone — only commit on Ok.
        let mut candidate = prev_guard.clone();
        let records = compute_records_from_current(&current, &mut candidate);
        let snapshot = PeriodSnapshot {
            ts,
            host: inner.host.clone(),
            service: inner.identity.clone(),
            period_seconds,
            records,
        };
        match installed.sink.record_period(&snapshot) {
            Ok(()) => {
                *prev_guard = candidate;
                ok_count += 1;
            }
            Err(e) => {
                tracing::error!(
                    target: "cosmix_log::stats",
                    err = %e,
                    "stats sink record_period failed; per-sink prev preserved for next accept"
                );
            }
        }
    }
    ok_count
}

/// Immutable snapshot of the registry's current cumulative values
/// at one instant. Built once per `perform_rollup` call and reused
/// across every installed sink so all sinks see the same period
/// boundary.
struct CurrentReadings {
    counters: Vec<(Key, u64)>,
    gauges: Vec<(Key, f64)>,
    histograms: Vec<(Key, HistogramSummary)>,
}

fn read_current(inner: &Arc<RecorderInner>) -> CurrentReadings {
    let mut counters = Vec::new();
    let mut gauges = Vec::new();
    let mut histograms = Vec::new();
    inner.registry.visit_counters(|key, counter| {
        counters.push((key.clone(), counter.load(Ordering::Acquire)));
    });
    inner.registry.visit_gauges(|key, gauge| {
        let bits = gauge.load(Ordering::Acquire);
        gauges.push((key.clone(), f64::from_bits(bits)));
    });
    // Built-in process gauges live OUT of the metrics Registry —
    // they were rewritten in this period's
    // `update_process_gauges` call. Folding them in as no-label
    // gauges here lets the existing per-sink delta-vs-prev path
    // handle them uniformly with app gauges, while the
    // clear-then-insert discipline upstream guarantees a failed
    // procfs read this period drops the entry (no stale
    // emission — Codex round-5 MAJOR fix).
    for (name, value) in inner.built_in_gauge_snapshot() {
        gauges.push((Key::from_name(name), value));
    }
    inner.registry.visit_histograms(|key, bucket| {
        histograms.push((key.clone(), summarise_bucket_for_rollup(bucket)));
    });
    CurrentReadings {
        counters,
        gauges,
        histograms,
    }
}

/// Build the `PeriodRecord` vector for one sink's period. Mutates
/// `prev` in place — the caller is responsible for committing the
/// post-call `prev` only on successful sink accept.
fn compute_records_from_current(
    current: &CurrentReadings,
    prev: &mut PrevState,
) -> Vec<PeriodRecord> {
    let mut records: Vec<PeriodRecord> = Vec::new();

    for (key, current_value) in &current.counters {
        use std::collections::hash_map::Entry;
        // Distinguish "first sighting" from "previous value was 0"
        // (Codex earlier-round MAJOR fix retained here). Using
        // `insert(..).unwrap_or(0)` collapses both into `last == 0`,
        // so a counter held at zero forever would emit every period.
        let (delta, first_sighting) = match prev.counters.entry(key.clone()) {
            Entry::Vacant(v) => {
                v.insert(*current_value);
                (*current_value, true)
            }
            Entry::Occupied(mut o) => {
                let last = *o.get();
                let d = current_value.saturating_sub(last);
                o.insert(*current_value);
                (d, false)
            }
        };
        if delta == 0 && !first_sighting {
            continue;
        }
        records.push(PeriodRecord {
            metric: key.name().to_string(),
            kind: MetricKind::Counter,
            sensitivity: sensitivity_of(key.name()),
            labels: labels_from(key),
            value: PeriodValue::Counter(*current_value),
            delta: PeriodValue::Counter(delta),
        });
    }

    for (key, current_value) in &current.gauges {
        let last = prev
            .gauges
            .insert(key.clone(), *current_value)
            .unwrap_or(0.0);
        // Gauges are point-in-time: emit every period regardless of
        // change so the last-`ts` aggregation rule in §4.4 has a
        // current reading.
        let delta = *current_value - last;
        records.push(PeriodRecord {
            metric: key.name().to_string(),
            kind: MetricKind::Gauge,
            sensitivity: sensitivity_of(key.name()),
            labels: labels_from(key),
            value: PeriodValue::Gauge(*current_value),
            delta: PeriodValue::Gauge(delta),
        });
    }

    for (key, summary) in &current.histograms {
        use std::collections::hash_map::Entry;
        let now = HistogramPrev {
            count: summary.count,
            sum: summary.sum,
        };
        let (count_delta, sum_delta, first_sighting) = match prev.histograms.entry(key.clone()) {
            Entry::Vacant(v) => {
                v.insert(now);
                (summary.count, summary.sum, true)
            }
            Entry::Occupied(mut o) => {
                let last = *o.get();
                o.insert(now);
                (
                    summary.count.saturating_sub(last.count),
                    summary.sum - last.sum,
                    false,
                )
            }
        };
        if count_delta == 0 && !first_sighting {
            continue;
        }
        // v1 carries the cumulative quantiles in the delta as well;
        // see module docstring for the v2 refinement note.
        let delta_summary = HistogramSummary {
            count: count_delta,
            sum: sum_delta,
            p50: summary.p50,
            p95: summary.p95,
            p99: summary.p99,
        };
        records.push(PeriodRecord {
            metric: key.name().to_string(),
            kind: MetricKind::Histogram,
            sensitivity: sensitivity_of(key.name()),
            labels: labels_from(key),
            value: PeriodValue::Histogram(summary.clone()),
            delta: PeriodValue::Histogram(delta_summary),
        });
    }

    records
}

/// Flush every installed sink. Called from `LogHandle::shutdown()`
/// (slice 4b/4c plumbing). Each sink's `flush()` is independent;
/// failures are logged and the next sink is flushed.
pub fn flush_all_sinks(recorder: &StatsRecorder) {
    flush_all_sinks_inner(&recorder.inner);
}

/// Drive the final on-exit roll-up + flush against the
/// process-installed recorder, if any.
///
/// Called from [`crate::LogHandle::shutdown`] (which the Mix CLI
/// invokes before every `std::process::exit` — plan §4.4). The
/// `period_seconds = 0` marker tells downstream JSONL readers this
/// is the final line for the process (plan §4.1); `flush_all_sinks`
/// then drives the `.open → .done` rename that closes the
/// durability barrier.
///
/// No-op when no recorder is installed — covers the `--stats=off`
/// path and the lost-install-race path (degraded but non-fatal per
/// `install_stats_recorder` in `init.rs`). Idempotency lives one
/// level up in `LogHandle::shutdown`'s `shutdown_done` flag; this
/// function is safe to call multiple times because both the
/// roll-up driver and `JsonlSink::flush` are themselves idempotent
/// on a finalised sink.
pub fn shutdown_installed_recorder() {
    if let Some(inner) = crate::stats::recorder::shared() {
        let _ = perform_rollup_inner(&inner, 0);
        flush_all_sinks_inner(&inner);
    }
}

pub(crate) fn flush_all_sinks_inner(inner: &Arc<RecorderInner>) {
    // Serialise against in-flight `perform_rollup` (Codex round-6
    // MAJOR). Without this, a shutdown can flush/finalise sink B
    // while the roll-up loop is still inside `record_period` on
    // sink A, racing the final period against rename(2). Holding
    // `rollup_lock` ensures the roll-up either fully completes
    // before any flush, or is queued to run *after* every sink is
    // already finalised (in which case its record_period calls
    // cleanly reject).
    let _rollup_guard = inner
        .rollup_lock
        .lock()
        .expect("stats rollup_lock Mutex poisoned");
    let sinks: Vec<Arc<InstalledSink>> = {
        let guard = inner.sinks.lock().expect("stats sinks Mutex poisoned");
        guard.clone()
    };
    for installed in &sinks {
        if let Err(e) = installed.sink.flush() {
            tracing::error!(
                target: "cosmix_log::stats",
                err = %e,
                "stats sink flush failed"
            );
        }
    }
}

fn labels_from(key: &Key) -> BTreeMap<String, String> {
    key.labels()
        .map(|l| (l.key().to_string(), l.value().to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::recorder::{StatsRecorder, StatsRecorderBuilder};
    use metrics::{Key, Label, Metadata, Recorder};
    use std::sync::Mutex as StdMutex;

    fn make() -> StatsRecorder {
        StatsRecorderBuilder::new("rollup-test").build()
    }

    fn metadata() -> Metadata<'static> {
        Metadata::new("rollup-test", metrics::Level::INFO, None)
    }

    /// Capture-everything sink — the test surrogate for JsonlSink.
    #[derive(Default)]
    struct CaptureSink {
        periods: StdMutex<Vec<PeriodSnapshot>>,
        flushed: StdMutex<bool>,
    }
    impl StatsSink for CaptureSink {
        fn record_period(&self, period: &PeriodSnapshot) -> std::io::Result<()> {
            self.periods.lock().unwrap().push(period.clone());
            Ok(())
        }
        fn flush(&self) -> std::io::Result<()> {
            *self.flushed.lock().unwrap() = true;
            Ok(())
        }
    }

    #[test]
    fn counter_delta_against_previous_period() {
        let rec = make();
        let sink = Arc::new(CaptureSink::default());
        rec.add_sink(sink.clone());
        let key = Key::from_parts("c", vec![Label::new("k", "v")]);
        let c = rec.register_counter(&key, &metadata());
        c.increment(10);
        perform_rollup(&rec, 60);
        c.increment(5);
        perform_rollup(&rec, 60);
        let periods = sink.periods.lock().unwrap();
        let r1 = periods[0].records.iter().find(|r| r.metric == "c").unwrap();
        match (&r1.value, &r1.delta) {
            (PeriodValue::Counter(v), PeriodValue::Counter(d)) => {
                assert_eq!(*v, 10);
                assert_eq!(*d, 10);
            }
            _ => panic!("expected counter record"),
        }
        let r2 = periods[1].records.iter().find(|r| r.metric == "c").unwrap();
        match (&r2.value, &r2.delta) {
            (PeriodValue::Counter(v), PeriodValue::Counter(d)) => {
                assert_eq!(*v, 15);
                assert_eq!(*d, 5);
            }
            _ => panic!("expected counter record"),
        }
    }

    #[test]
    fn counter_unchanged_after_first_sighting_is_omitted() {
        let rec = make();
        let sink = Arc::new(CaptureSink::default());
        rec.add_sink(sink.clone());
        let key = Key::from_parts("idle", Vec::<Label>::new());
        let c = rec.register_counter(&key, &metadata());
        c.increment(3);
        perform_rollup(&rec, 60);
        perform_rollup(&rec, 60);
        let periods = sink.periods.lock().unwrap();
        assert!(periods[0].records.iter().any(|r| r.metric == "idle"));
        assert!(
            !periods[1].records.iter().any(|r| r.metric == "idle"),
            "idle counter must not appear in subsequent unchanged period"
        );
    }

    #[test]
    fn gauge_emits_every_period_even_unchanged() {
        let rec = make();
        let sink = Arc::new(CaptureSink::default());
        rec.add_sink(sink.clone());
        let key = Key::from_parts("g", Vec::<Label>::new());
        let g = rec.register_gauge(&key, &metadata());
        g.set(2.0);
        perform_rollup(&rec, 60);
        perform_rollup(&rec, 60);
        let periods = sink.periods.lock().unwrap();
        assert!(periods[0].records.iter().any(|r| r.metric == "g"));
        let g2 = periods[1].records.iter().find(|r| r.metric == "g").unwrap();
        match (&g2.value, &g2.delta) {
            (PeriodValue::Gauge(v), PeriodValue::Gauge(d)) => {
                assert!((*v - 2.0).abs() < f64::EPSILON);
                assert!(d.abs() < f64::EPSILON);
            }
            _ => panic!("expected gauge record"),
        }
    }

    #[test]
    fn histogram_count_and_sum_delta() {
        let rec = make();
        let sink = Arc::new(CaptureSink::default());
        rec.add_sink(sink.clone());
        let key = Key::from_parts("h", Vec::<Label>::new());
        let h = rec.register_histogram(&key, &metadata());
        for v in 1..=10 {
            h.record(f64::from(v));
        }
        perform_rollup(&rec, 60);
        for v in 11..=15 {
            h.record(f64::from(v));
        }
        perform_rollup(&rec, 60);
        let periods = sink.periods.lock().unwrap();
        let hr1 = periods[0].records.iter().find(|r| r.metric == "h").unwrap();
        match (&hr1.value, &hr1.delta) {
            (PeriodValue::Histogram(v), PeriodValue::Histogram(d)) => {
                assert_eq!(v.count, 10);
                assert_eq!(d.count, 10);
                assert!((v.sum - 55.0).abs() < f64::EPSILON);
                assert!((d.sum - 55.0).abs() < f64::EPSILON);
            }
            _ => panic!("expected histogram record"),
        }
        let hr2 = periods[1].records.iter().find(|r| r.metric == "h").unwrap();
        match (&hr2.value, &hr2.delta) {
            (PeriodValue::Histogram(v), PeriodValue::Histogram(d)) => {
                assert_eq!(v.count, 15);
                assert_eq!(d.count, 5);
                assert!((d.sum - 65.0).abs() < f64::EPSILON);
            }
            _ => panic!("expected histogram record"),
        }
    }

    #[test]
    fn fan_out_calls_every_sink() {
        let rec = make();
        let sink_a = Arc::new(CaptureSink::default());
        let sink_b = Arc::new(CaptureSink::default());
        rec.add_sink(sink_a.clone());
        rec.add_sink(sink_b.clone());
        let key = Key::from_parts("c", Vec::<Label>::new());
        let c = rec.register_counter(&key, &metadata());
        c.increment(1);
        let ok = perform_rollup(&rec, 60);
        assert_eq!(ok, 2);
        assert_eq!(sink_a.periods.lock().unwrap().len(), 1);
        assert_eq!(sink_b.periods.lock().unwrap().len(), 1);
    }

    #[test]
    fn sink_error_does_not_stop_other_sinks() {
        struct FailingSink;
        impl StatsSink for FailingSink {
            fn record_period(&self, _p: &PeriodSnapshot) -> std::io::Result<()> {
                Err(std::io::Error::other("intentional"))
            }
            fn flush(&self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let rec = make();
        rec.add_sink(Arc::new(FailingSink));
        let good = Arc::new(CaptureSink::default());
        rec.add_sink(good.clone());
        let key = Key::from_parts("c", Vec::<Label>::new());
        rec.register_counter(&key, &metadata()).increment(1);
        let ok = perform_rollup(&rec, 60);
        assert_eq!(ok, 1, "only the good sink should be counted");
        assert_eq!(good.periods.lock().unwrap().len(), 1);
    }

    /// The load-bearing per-sink-prev property: when sink A rejects
    /// period N, A's next successful accept in period N+1 must
    /// carry a delta covering BOTH periods, while sink B (which
    /// accepted both) sees the per-period delta.
    #[test]
    fn failing_sink_recovers_with_merged_delta_on_next_accept() {
        use std::sync::atomic::{AtomicBool, Ordering as AOrdering};
        struct ToggleSink {
            fail: AtomicBool,
            seen: StdMutex<Vec<PeriodSnapshot>>,
        }
        impl StatsSink for ToggleSink {
            fn record_period(&self, p: &PeriodSnapshot) -> std::io::Result<()> {
                if self.fail.load(AOrdering::SeqCst) {
                    Err(std::io::Error::other("toggle"))
                } else {
                    self.seen.lock().unwrap().push(p.clone());
                    Ok(())
                }
            }
            fn flush(&self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let rec = make();
        let toggle = Arc::new(ToggleSink {
            fail: AtomicBool::new(true),
            seen: StdMutex::new(Vec::new()),
        });
        let good = Arc::new(CaptureSink::default());
        rec.add_sink(toggle.clone());
        rec.add_sink(good.clone());

        let key = Key::from_parts("c", Vec::<Label>::new());
        let c = rec.register_counter(&key, &metadata());

        // Period N: +10 → toggle rejects, good accepts.
        c.increment(10);
        perform_rollup(&rec, 60);
        assert_eq!(toggle.seen.lock().unwrap().len(), 0);
        assert_eq!(good.periods.lock().unwrap().len(), 1);

        // Period N+1: +5, toggle starts accepting.
        toggle.fail.store(false, AOrdering::SeqCst);
        c.increment(5);
        perform_rollup(&rec, 60);

        // toggle's first-ever accept carries the merged delta of 15.
        let toggle_seen = toggle.seen.lock().unwrap();
        assert_eq!(toggle_seen.len(), 1);
        let tr = toggle_seen[0]
            .records
            .iter()
            .find(|r| r.metric == "c")
            .unwrap();
        match (&tr.value, &tr.delta) {
            (PeriodValue::Counter(v), PeriodValue::Counter(d)) => {
                assert_eq!(*v, 15);
                assert_eq!(
                    *d, 15,
                    "merged-delta property: toggle never accepted N=10, must see 15 now"
                );
            }
            _ => panic!("expected counter record"),
        }
        // The good sink saw 10 then 5 — per-period as normal.
        let good_periods = good.periods.lock().unwrap();
        assert_eq!(good_periods.len(), 2);
        let g2 = good_periods[1]
            .records
            .iter()
            .find(|r| r.metric == "c")
            .unwrap();
        if let (PeriodValue::Counter(v), PeriodValue::Counter(d)) = (&g2.value, &g2.delta) {
            assert_eq!(*v, 15);
            assert_eq!(*d, 5);
        }
    }

    /// Concurrent `perform_rollup` calls must not interleave such
    /// that a later commit rewinds an earlier-advanced prev. Drives
    /// many parallel calls against one recorder + sink and asserts
    /// that the sum of all `delta` values equals the final
    /// `value` (Codex round-5 MAJOR).
    #[test]
    fn concurrent_rollups_do_not_rewind_prev() {
        use std::sync::Barrier;
        use std::thread;
        let rec = Arc::new(make());
        let sink = Arc::new(CaptureSink::default());
        rec.add_sink(sink.clone());
        let key = Key::from_parts("c", Vec::<Label>::new());
        let c = rec.register_counter(&key, &metadata());
        // Drive 50 increments across 4 threads; each thread also
        // performs a roll-up after every increment.
        let n_threads = 4;
        let per_thread = 50;
        let barrier = Arc::new(Barrier::new(n_threads));
        let mut handles = Vec::new();
        for _ in 0..n_threads {
            let rec = Arc::clone(&rec);
            let c = c.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                for _ in 0..per_thread {
                    c.increment(1);
                    perform_rollup(&rec, 60);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        // Final roll-up to capture any straggler.
        perform_rollup(&rec, 60);
        let periods = sink.periods.lock().unwrap();
        let mut sum_delta: u64 = 0;
        let mut last_value: u64 = 0;
        for p in periods.iter() {
            for r in &p.records {
                if r.metric == "c"
                    && let (PeriodValue::Counter(v), PeriodValue::Counter(d)) = (&r.value, &r.delta)
                {
                    sum_delta += *d;
                    last_value = *v;
                }
            }
        }
        let total: u64 = (n_threads as u64) * (per_thread as u64);
        assert_eq!(
            last_value, total,
            "final counter value must equal total increments"
        );
        assert_eq!(
            sum_delta, total,
            "sum of all per-period deltas must equal total — non-equality means prev was rewound"
        );
    }

    #[test]
    fn flush_all_sinks_flushes_every_registered_sink() {
        let rec = make();
        let a = Arc::new(CaptureSink::default());
        let b = Arc::new(CaptureSink::default());
        rec.add_sink(a.clone());
        rec.add_sink(b.clone());
        flush_all_sinks(&rec);
        assert!(*a.flushed.lock().unwrap());
        assert!(*b.flushed.lock().unwrap());
    }

    #[test]
    fn snapshot_stamps_service_identity_and_period_seconds_and_cached_host() {
        let rec = make();
        let sink = Arc::new(CaptureSink::default());
        rec.add_sink(sink.clone());
        let key = Key::from_parts("c", Vec::<Label>::new());
        rec.register_counter(&key, &metadata()).increment(1);
        perform_rollup(&rec, 30);
        let periods = sink.periods.lock().unwrap();
        let p = &periods[0];
        assert_eq!(p.service, "rollup-test");
        assert_eq!(p.period_seconds, 30);
        // Cached host matches the recorder's stored host (Codex
        // round-4 MINOR — read once at build, not per period).
        assert_eq!(p.host, rec.inner.host);
        assert!(!p.host.is_empty());
    }
}
