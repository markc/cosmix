//! Built-in process gauges (plan §3 "built-in counters" group).
//!
//! `update_process_gauges` is invoked at the top of every
//! `perform_rollup` call (after `rollup_lock` is held) so the
//! recorder's `built_in_gauges` side map carries fresh readings
//! for `cosmix_process_uptime_seconds`, `cosmix_process_memory_kb`,
//! and `cosmix_process_open_fds` whenever a sink reads the
//! period.
//!
//! # Why a side map (Codex round-5 MAJOR)
//!
//! These gauges do NOT live in the metrics `Registry`. A registry
//! entry persists across roll-ups and would emit the *last
//! successful* reading every period thereafter — even when the
//! current period's procfs read failed. By rewriting a side map
//! each period (clear-then-insert-only-on-success), a failed
//! read drops the entry and the consumer (`read_current`,
//! `local_snapshot`) emits no record for the missing name. The
//! operator sees the series go dark, which is the truth, instead
//! of a stale value frozen at the last good sample.
//!
//! # Procfs is best-effort
//!
//! `/proc/self/status` and `/proc/self/fd/` are Linux-only and
//! may also be unavailable inside sandboxes (e.g. seccomp). We
//! silently skip the failing gauge rather than polluting the
//! period with placeholder zeros — operators reading a series
//! with no recent samples can tell the gauge went dark, while a
//! synthetic `0.0` would lie.

use crate::stats::recorder::RecorderInner;
use std::sync::Arc;

/// Refresh the three built-in process gauges into `inner`'s
/// `built_in_gauges` side map. Called by the roll-up driver
/// immediately before `read_current` so the same period sees
/// fresh built-ins across every fan-out sink.
///
/// Every successful procfs read inserts a fresh entry; failed
/// reads drop the entry. The map is `.clear()`ed first so a
/// previously-admitted gauge whose procfs read goes dark in this
/// period cannot leak a stale value into the emission.
pub(crate) fn update_process_gauges(inner: &Arc<RecorderInner>) {
    let mut g = inner
        .built_in_gauges
        .lock()
        .expect("stats built_in_gauges Mutex poisoned");
    g.clear();

    let uptime = inner.process_start.elapsed().as_secs_f64();
    g.insert("cosmix_process_uptime_seconds", uptime);
    fan_to_prometheus(inner, "cosmix_process_uptime_seconds", uptime);

    if let Some(rss_kb) = read_vm_rss_kb() {
        let v = rss_kb as f64;
        g.insert("cosmix_process_memory_kb", v);
        fan_to_prometheus(inner, "cosmix_process_memory_kb", v);
    }

    if let Some(open_fds) = count_open_fds() {
        let v = open_fds as f64;
        g.insert("cosmix_process_open_fds", v);
        fan_to_prometheus(inner, "cosmix_process_open_fds", v);
    }

    if let Some(cpu_s) = read_cpu_seconds_total() {
        g.insert("cosmix_process_cpu_seconds_total", cpu_s);
        fan_to_prometheus(inner, "cosmix_process_cpu_seconds_total", cpu_s);
    }
}

/// Codex round-3 MAJOR: built-in process gauges live in the recorder
/// side map and never pass through `Recorder::register_gauge`, so the
/// register-time fan-out path cannot mirror them to the Prometheus
/// child. Forward the same `(name, value)` pair to the child at the
/// same callsite as the side-map insert so `/metrics` sees the same
/// readings JSONL/Bus do, and a failed procfs read in this period
/// (which skips the `g.insert`) also skips the Prometheus write —
/// preserving the "series went dark" semantic the side map exists for.
///
/// No-op on builds without the `prometheus` feature.
#[cfg(feature = "prometheus")]
fn fan_to_prometheus(inner: &Arc<RecorderInner>, name: &'static str, value: f64) {
    if let Some(child) = inner.prometheus_child.get() {
        let key = metrics::Key::from_name(name);
        child.fan_internal_gauge(&key, value);
    }
}

#[cfg(not(feature = "prometheus"))]
fn fan_to_prometheus(_inner: &Arc<RecorderInner>, _name: &'static str, _value: f64) {}

/// Cumulative CPU time of this process (ALL threads, user+system) in
/// SECONDS, from `/proc/self/stat` fields 14+15 (utime+stime).
///
/// NOT `/proc/self/schedstat` — that file is main-thread-only, which
/// under-reported a 12-thread embed daemon by ~4 orders of magnitude
/// on the 2026-07-25 indexd canary (0.06s reported vs ~366s cgroup).
/// utime/stime are in USER_HZ ticks, which the kernel FIXES at 100
/// for the procfs interface regardless of CONFIG_HZ, so /100.0 is
/// correct on Linux without a `sysconf(_SC_CLK_TCK)` (libc) call.
/// Excludes reaped children (cutime/cstime) — this series is "this
/// process", while cgroup CPUUsageNSec also covers live children;
/// samplers comparing the two should expect cgroup >= this.
///
/// `_total` suffix: monotone counter by Prometheus convention (it
/// rides the built-in gauge side map like the other process
/// built-ins; consumers must treat it as a counter series).
fn read_cpu_seconds_total() -> Option<f64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // Field 2 (comm) is parenthesised and may contain spaces/parens;
    // fields 3+ start after the LAST ')'.
    let after_comm = &stat[stat.rfind(')')? + 1..];
    let mut fields = after_comm.split_whitespace();
    // after_comm fields: [0]=state(3) ... utime is overall field 14 →
    // index 11 here; stime field 15 → index 12.
    let utime = fields.nth(11)?.parse::<u64>().ok()?;
    let stime = fields.next()?.parse::<u64>().ok()?;
    const USER_HZ: f64 = 100.0;
    Some((utime + stime) as f64 / USER_HZ)
}

/// Parse `VmRSS:` (in kilobytes per kernel convention — the
/// `kB` suffix is literal regardless of system page size) from
/// `/proc/self/status`. Returns `None` on any IO/parse error;
/// the caller skips the gauge.
fn read_vm_rss_kb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            // Format: "VmRSS:\t  12345 kB"
            let mut parts = rest.split_whitespace();
            let value = parts.next()?;
            return value.parse::<u64>().ok();
        }
    }
    None
}

/// Count entries in `/proc/self/fd/`. The directory itself is a
/// view of the process's file-descriptor table, so its entry
/// count is the FD count. Returns `None` on the first per-entry
/// IO error rather than silently undercounting — a partial walk
/// would lie about FD pressure (Codex round-5 MINOR fix). Caller
/// then skips the gauge for this period.
fn count_open_fds() -> Option<u64> {
    let entries = std::fs::read_dir("/proc/self/fd").ok()?;
    let mut count: u64 = 0;
    for entry in entries {
        // Surface the error rather than `.ok()?`-collapse, so a
        // mid-walk failure goes dark for this period instead of
        // reporting (count so far).
        entry.ok()?;
        count += 1;
    }
    // `read_dir` itself opens an FD that gets counted; subtract
    // it so the gauge reflects FDs the application holds, not
    // the measurement scaffolding.
    Some(count.saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::recorder::StatsRecorderBuilder;
    use crate::stats::snapshot::snapshot_from_inner;
    use crate::stats::types::{MetricKind, SeriesValue};

    #[test]
    fn builtin_gauges_are_populated_after_update() {
        let rec = StatsRecorderBuilder::new("process-gauges-test").build();
        update_process_gauges(&rec.inner);
        let snap = snapshot_from_inner(&rec.inner);
        let names: Vec<_> = snap
            .metrics
            .iter()
            .filter(|f| f.kind == MetricKind::Gauge)
            .map(|f| f.name.as_str())
            .collect();
        assert!(
            names.contains(&"cosmix_process_uptime_seconds"),
            "uptime gauge missing; saw {names:?}"
        );
        // On non-Linux or sandboxed CI, memory_kb and open_fds may be
        // skipped — the uptime gauge is the only one guaranteed.
        for family in &snap.metrics {
            if family.name == "cosmix_process_uptime_seconds" {
                let series = &family.series[0];
                if let SeriesValue::Gauge(v) = &series.value {
                    assert!(*v >= 0.0);
                }
            }
        }
    }

    #[test]
    fn failed_read_clears_prior_value() {
        // Anti-regression for Codex round-5 MAJOR: simulate a
        // mid-life procfs failure by hand-poisoning the side map
        // with a stale value and confirming that a fresh
        // `update_process_gauges` call rewrites only the names
        // it can read, dropping any name it could NOT read.
        let rec = StatsRecorderBuilder::new("process-gauges-test").build();
        // Pre-poison with a name that update_process_gauges
        // never writes — it must be dropped by the clear().
        {
            let mut g = rec.inner.built_in_gauges.lock().unwrap();
            g.insert("ghost_gauge_from_a_prior_period", 999.0);
            g.insert("cosmix_process_uptime_seconds", 0.0);
        }
        update_process_gauges(&rec.inner);
        let snap = rec.inner.built_in_gauge_snapshot();
        assert!(
            !snap
                .iter()
                .any(|(n, _)| *n == "ghost_gauge_from_a_prior_period"),
            "stale gauge from a prior period must not survive the clear"
        );
        assert!(
            snap.iter()
                .any(|(n, _)| *n == "cosmix_process_uptime_seconds"),
            "uptime must be re-inserted after the clear"
        );
    }

    #[test]
    fn vm_rss_kb_parses_when_proc_available() {
        if !std::path::Path::new("/proc/self/status").exists() {
            return;
        }
        let kb = read_vm_rss_kb().expect("VmRSS present on Linux");
        assert!(
            kb > 0,
            "VmRSS should be non-zero for a running test process"
        );
    }

    #[test]
    fn open_fds_counts_at_least_stdio() {
        if !std::path::Path::new("/proc/self/fd").exists() {
            return;
        }
        let n = count_open_fds().expect("/proc/self/fd readable on Linux");
        assert!(n >= 3, "expected at least stdin/stdout/stderr, got {n}");
    }

    #[test]
    fn cpu_seconds_total_is_monotone_and_counts_all_threads() {
        if !std::path::Path::new("/proc/self/stat").exists() {
            return; // non-Linux / restricted sandbox: gauge goes dark, by design
        }
        let a = read_cpu_seconds_total().expect("/proc/self/stat readable on Linux");
        // Burn several USER_HZ ticks (10ms each) on a SPAWNED thread —
        // the whole point of the /proc/self/stat source is counting
        // threads other than main (the schedstat regression this
        // replaces under-reported a 12-thread daemon ~4 orders of
        // magnitude).
        std::thread::spawn(|| {
            let t0 = std::time::Instant::now();
            let mut x = 0u64;
            while t0.elapsed() < std::time::Duration::from_millis(60) {
                x = std::hint::black_box(x.wrapping_add(31));
            }
        })
        .join()
        .unwrap();
        let b = read_cpu_seconds_total().unwrap();
        assert!(b >= a, "cpu counter went backwards: {a} -> {b}");
        assert!(
            b - a >= 0.01,
            "spawned-thread burn invisible ({a} -> {b}); is the source thread-local again?"
        );
    }
}
