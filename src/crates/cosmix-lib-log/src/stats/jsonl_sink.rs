//! JSONL `StatsSink` implementation (plan §4.1).
//!
//! Writes one JSON line per (metric, label-set) per roll-up period
//! to a per-process file under `<log_dir>/<identity>.stats.<class>-<pid>-<startup_ts>.jsonl.open`,
//! renamed to `.jsonl.done` on [`StatsSink::flush`].
//!
//! # Producer classes (plan §4.1 table)
//!
//! - **Daemon** (`JsonlSink::daemon`): long-running process,
//!   one file per startup, daily-rotated. Slice 3 ships the
//!   single-file per-startup form; UTC-midnight rotation is a slice
//!   4+ hook handled by the roll-up task (not this sink).
//! - **Delta** (`JsonlSink::delta`): per-process file for Mix (one
//!   per `mix -c` / `mix <file>` invocation, merged at read time by
//!   `disk_snapshot`).
//!
//! # The `.open` → `.done` rename is the only durability barrier
//!
//! While the recorder is alive the file carries `.jsonl.open`;
//! `flush()` fsyncs the file and renames it to `.jsonl.done`. POSIX
//! rename is atomic so external observers (Alloy `loki.source.file`,
//! `disk_snapshot`, indexd ingest) see either suffix, never both.
//!
//! # Filename collision retry
//!
//! Same-second restart of the same daemon (pid recycling, vanishing
//! rare) would otherwise collide on the `.open` path. The constructor
//! uses `O_CREAT|O_EXCL` and walks suffixes `-1`, `-2`, ... until
//! open succeeds; mesh-wide uniqueness is provided by the `host`
//! field embedded in every JSONL line, so filenames don't need to be
//! globally unique.
//!
//! # Byte budget (plan §3.3.1)
//!
//! - **Soft** (`byte_budget_mib`, default 256 daemon / 16 mix): once
//!   per-UTC-day bytes-written crosses the soft threshold, the sink
//!   emits a `warn` event at most once per hour and continues to
//!   write.
//! - **Hard** (1 GiB/day, fixed): once crossed, the sink stops
//!   appending until the next UTC midnight and surfaces a
//!   `level=error` event. Subsequent `record_period` calls return
//!   `Ok(())` without writing — the in-memory recorder remains
//!   correct, only the disk path pauses. The `cosmix_stats_budget_exceeded_total`
//!   counter wiring lives on the roll-up task (slice 4); this sink
//!   surfaces the budget-exceeded condition via the warn/error log
//!   stream so slice-3 standalone tests can validate it.

use crate::stats::labels_hash::labels_hash;
use crate::stats::sink::{PeriodRecord, PeriodSnapshot, PeriodValue, StatsSink};
use crate::stats::types::{LabelSensitivity, MetricKind};
use serde_json::{Number, Value};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Hard ceiling on per-process per-UTC-day JSONL bytes (plan §3.3.1).
/// Once crossed, the sink pauses disk writes until the next UTC midnight.
pub const HARD_BUDGET_BYTES: u64 = 1024 * 1024 * 1024;

/// Minimum spacing between soft-budget warn events (plan §3.3.1).
const SOFT_WARN_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Upper bound on filename-collision-retry attempts. Stops the
/// constructor from looping unboundedly if the log dir is wedged in
/// some pathological state (filesystem full, permission denied per
/// stem, etc.).
const COLLISION_RETRY_CEILING: u32 = 1024;

/// Producer class — controls the discriminator embedded in the
/// filename (`daemon-<pid>-<ts>` vs `delta-<pid>-<ts>`; plan §4.1).
#[derive(Debug, Clone, Copy)]
pub enum ProducerClass {
    /// Daemon roll-up file. Long-lived process, one file per startup.
    Daemon,
    /// Per-process delta file (Mix one-shot, cumulative-read aggregator).
    Delta,
}

impl ProducerClass {
    fn name(self) -> &'static str {
        match self {
            ProducerClass::Daemon => "daemon",
            ProducerClass::Delta => "delta",
        }
    }
}

pub struct JsonlSink {
    inner: Mutex<Inner>,
    /// Soft warn threshold, computed from the operator-mutable
    /// `byte_budget_mib`. `0` disables the soft warn (only the hard
    /// ceiling applies).
    soft_budget_bytes: u64,
}

struct Inner {
    open_path: PathBuf,
    done_path: PathBuf,
    /// `Some` while the sink is accepting writes; `None` after
    /// `flush()` has finalised the file.
    file: Option<BufWriter<File>>,
    /// Running byte count for the current UTC day. Resets at UTC
    /// midnight inside `record_period`.
    bytes_today: u64,
    /// UTC day the `bytes_today` counter is tracking.
    budget_day: chrono::NaiveDate,
    last_soft_warn: Option<Instant>,
    /// Hard-budget pause flag for the current UTC day. Resets at
    /// midnight along with `bytes_today`.
    paused: bool,
    /// `true` once `.open → .done` rename has succeeded. Set
    /// independently of `finalised` so a `flush()` that fails on the
    /// trailing parent-directory fsync can be retried without
    /// re-attempting the (now ENOENT) rename.
    renamed: bool,
    /// `true` after `flush()` has driven through every durability
    /// step (file fsync, rename, parent-directory fsync). Further
    /// `record_period` calls return an error (the sink is single-shot
    /// from flush onwards).
    finalised: bool,
}

impl JsonlSink {
    /// Construct a daemon-class sink. Filename discriminator
    /// `daemon-<pid>-<startup_ts>` per plan §4.1.
    pub fn daemon(
        log_dir: impl Into<PathBuf>,
        identity: &str,
        byte_budget_mib: u32,
    ) -> std::io::Result<Self> {
        Self::new(
            log_dir.into(),
            identity,
            ProducerClass::Daemon,
            byte_budget_mib,
        )
    }

    /// Construct a delta-class sink. Filename discriminator
    /// `delta-<pid>-<startup_ts>` per plan §4.1.
    pub fn delta(
        log_dir: impl Into<PathBuf>,
        identity: &str,
        byte_budget_mib: u32,
    ) -> std::io::Result<Self> {
        Self::new(
            log_dir.into(),
            identity,
            ProducerClass::Delta,
            byte_budget_mib,
        )
    }

    fn new(
        log_dir: PathBuf,
        identity: &str,
        class: ProducerClass,
        byte_budget_mib: u32,
    ) -> std::io::Result<Self> {
        std::fs::create_dir_all(&log_dir)?;
        let pid = std::process::id();
        let startup_ts = startup_timestamp();
        let base_stem = format!(
            "{identity}.stats.{class_name}-{pid}-{startup_ts}",
            class_name = class.name(),
        );
        let (open_path, done_path, file) = open_with_collision_retry(&log_dir, &base_stem)?;
        let writer = BufWriter::new(file);
        let today = chrono::Utc::now().date_naive();
        Ok(Self {
            inner: Mutex::new(Inner {
                open_path,
                done_path,
                file: Some(writer),
                bytes_today: 0,
                budget_day: today,
                last_soft_warn: None,
                paused: false,
                renamed: false,
                finalised: false,
            }),
            soft_budget_bytes: u64::from(byte_budget_mib).saturating_mul(1024 * 1024),
        })
    }

    /// Path of the live `.jsonl.open` file. Exposed for tests and
    /// the slice-5 `disk_snapshot` reader.
    pub fn open_path(&self) -> PathBuf {
        self.inner
            .lock()
            .expect("JsonlSink poisoned")
            .open_path
            .clone()
    }

    /// Path the file will be renamed to on `flush()`. Exposed for
    /// tests and the slice-5 `disk_snapshot` reader.
    pub fn done_path(&self) -> PathBuf {
        self.inner
            .lock()
            .expect("JsonlSink poisoned")
            .done_path
            .clone()
    }
}

fn startup_timestamp() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Open the `.jsonl.open` file with `O_CREAT|O_EXCL` and walk
/// numeric suffixes `-1`, `-2`, ... if the bare stem collides.
/// Returns `(open_path, done_path, file)`.
fn open_with_collision_retry(
    log_dir: &Path,
    base_stem: &str,
) -> std::io::Result<(PathBuf, PathBuf, File)> {
    let try_open = |suffix: Option<u32>| -> std::io::Result<(PathBuf, PathBuf, File)> {
        let stem = match suffix {
            None => base_stem.to_string(),
            Some(n) => format!("{base_stem}-{n}"),
        };
        let open_path = log_dir.join(format!("{stem}.jsonl.open"));
        let done_path = log_dir.join(format!("{stem}.jsonl.done"));
        // Pre-check the `.done` companion. If it exists we'd clobber
        // it at flush-time rename (POSIX rename overwrites). Treating
        // its presence as a collision matches the `.open` collision
        // shape, and the suffix walk recovers cleanly.
        if done_path.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                ".jsonl.done variant already exists for this stem",
            ));
        }
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&open_path)?;
        // Re-check after `O_CREAT|O_EXCL` succeeds (Codex round-1 MAJOR).
        // A racing same-stem writer can complete `.open → .done` between
        // our pre-check and our open; finding the `.done` now means a
        // future `flush()` rename would clobber theirs. Remove the
        // just-created `.open` and treat the slot as a collision so the
        // suffix walk recovers cleanly.
        if done_path.exists() {
            let _ = std::fs::remove_file(&open_path);
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                ".jsonl.done appeared between pre-check and create_new",
            ));
        }
        Ok((open_path, done_path, file))
    };
    match try_open(None) {
        Ok(v) => return Ok(v),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            tracing::warn!(
                target: "cosmix_log::stats",
                stem = base_stem,
                "JSONL filename collision on bare stem; retrying with numeric suffix",
            );
        }
        Err(e) => return Err(e),
    }
    for n in 1..=COLLISION_RETRY_CEILING {
        match try_open(Some(n)) {
            Ok(v) => return Ok(v),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!(
            "exhausted {COLLISION_RETRY_CEILING} collision-retry suffixes for stats JSONL stem {base_stem}"
        ),
    ))
}

impl StatsSink for JsonlSink {
    fn record_period(&self, period: &PeriodSnapshot) -> std::io::Result<()> {
        let mut inner = self.inner.lock().expect("JsonlSink Mutex poisoned");
        if inner.finalised {
            return Err(std::io::Error::other(
                "JsonlSink already flushed; no further records accepted",
            ));
        }
        let today = chrono::Utc::now().date_naive();
        if today != inner.budget_day {
            inner.budget_day = today;
            inner.bytes_today = 0;
            inner.paused = false;
            inner.last_soft_warn = None;
        }
        if inner.paused {
            return Ok(());
        }
        // Pre-build + pre-validate every line BEFORE touching the
        // writer (Codex round-2 MAJOR). The previous shape wrote each
        // line as it was serialized, so a kind/PeriodValue mismatch on
        // record N would leave records 1..N-1 partially written on
        // disk while the call as a whole returned Err — a partial
        // period that a downstream aggregator cannot detect. Validating
        // up-front means a serialization failure rejects the entire
        // period atomically (no bytes written, no budget accounting).
        let mut lines: Vec<String> = Vec::with_capacity(period.records.len());
        let mut payload_bytes: u64 = 0;
        for rec in &period.records {
            let line = build_line(period, rec)?;
            payload_bytes = payload_bytes.saturating_add(line.len() as u64 + 1);
            lines.push(line);
        }
        // Hard-budget pre-check (Codex round-2 MAJOR). Enforcing the
        // ceiling AFTER the write let a single big period overshoot
        // `HARD_BUDGET_BYTES` arbitrarily — the "hard 1 GiB/day"
        // contract said one thing and the code did another. Reject
        // the whole period if it would cross the line; the period is
        // dropped from the disk surface but the in-memory recorder
        // remains correct (the snapshot-derived `value` field still
        // reflects truth). Same shape as the `paused` branch above.
        let projected = inner.bytes_today.saturating_add(payload_bytes);
        if projected > HARD_BUDGET_BYTES {
            inner.paused = true;
            tracing::error!(
                target: "cosmix_log::stats",
                bytes_today = inner.bytes_today,
                period_bytes = payload_bytes,
                hard_budget = HARD_BUDGET_BYTES,
                path = %inner.open_path.display(),
                "stats JSONL period would cross the 1 GiB/day hard ceiling; dropping period and pausing disk-append until next UTC midnight",
            );
            return Ok(());
        }
        let writer = inner.file.as_mut().ok_or_else(|| {
            std::io::Error::other("JsonlSink writer slot empty (post-flush invariant violated)")
        })?;
        for line in &lines {
            writer.write_all(line.as_bytes())?;
            writer.write_all(b"\n")?;
        }
        inner.bytes_today = projected;
        let total = inner.bytes_today;
        let soft = self.soft_budget_bytes;
        if total >= HARD_BUDGET_BYTES {
            inner.paused = true;
            tracing::error!(
                target: "cosmix_log::stats",
                bytes_today = total,
                hard_budget = HARD_BUDGET_BYTES,
                path = %inner.open_path.display(),
                "stats JSONL hard ceiling (1 GiB/day) reached; disk-append paused until next UTC midnight",
            );
        } else if soft > 0 && total >= soft {
            let emit = !matches!(
                inner.last_soft_warn,
                Some(prev) if prev.elapsed() < SOFT_WARN_INTERVAL
            );
            if emit {
                inner.last_soft_warn = Some(Instant::now());
                tracing::warn!(
                    target: "cosmix_log::stats",
                    bytes_today = total,
                    soft_budget = soft,
                    "stats JSONL soft byte budget exceeded; continuing up to 1 GiB hard ceiling",
                );
            }
        }
        Ok(())
    }

    fn flush(&self) -> std::io::Result<()> {
        let mut inner = self.inner.lock().expect("JsonlSink Mutex poisoned");
        if inner.finalised {
            return Ok(());
        }
        // Step 1 — flush + fsync the file IN-PLACE via `as_mut` so
        // any error leaves the writer slot populated for a caller
        // retry (Codex round-1 MAJOR). The earlier `take()` shape
        // dropped the writer on first-step failure and a subsequent
        // `flush()` could then rename an unsynced `.open` to `.done`.
        if let Some(writer) = inner.file.as_mut() {
            writer.flush()?;
            writer.get_ref().sync_all()?;
        }
        // Step 2 — rename `.open → .done`. Skipped on retry if the
        // prior call got past this point but failed on the directory
        // fsync below; otherwise the second rename would fail ENOENT
        // and mask the original error.
        if !inner.renamed {
            std::fs::rename(&inner.open_path, &inner.done_path)?;
            inner.file = None;
            inner.renamed = true;
        }
        // Step 3 — fsync the parent directory (Codex round-2 MAJOR).
        // POSIX rename is atomic for readers but the directory entry
        // needs an explicit fsync for crash-durability; without this
        // a power loss can lose the `.done` entry despite the
        // `flush()`-is-the-durability-barrier contract.
        if let Some(parent) = inner.done_path.parent() {
            let dir = File::open(parent)?;
            dir.sync_all()?;
        }
        inner.finalised = true;
        Ok(())
    }
}

fn build_line(period: &PeriodSnapshot, rec: &PeriodRecord) -> std::io::Result<String> {
    let mut obj = serde_json::Map::with_capacity(9);
    obj.insert("ts".into(), Value::String(format_ts(period.ts)));
    obj.insert("host".into(), Value::String(period.host.clone()));
    obj.insert("service".into(), Value::String(period.service.clone()));
    obj.insert("metric".into(), Value::String(rec.metric.clone()));
    // labels XOR labels_hash, per plan §4.1 (Codex round-14 MAJOR fix).
    match rec.sensitivity {
        LabelSensitivity::Safe => {
            let labels_value = serde_json::to_value(&rec.labels)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            obj.insert("labels".into(), labels_value);
        }
        LabelSensitivity::Restricted => {
            obj.insert(
                "labels_hash".into(),
                Value::String(labels_hash(&rec.labels)),
            );
        }
    }
    obj.insert("kind".into(), Value::String(kind_str(rec.kind).into()));
    obj.insert("value".into(), value_to_json(&rec.value, rec.kind)?);
    obj.insert("delta".into(), value_to_json(&rec.delta, rec.kind)?);
    obj.insert(
        "period".into(),
        Value::Number(Number::from(period.period_seconds)),
    );
    serde_json::to_string(&Value::Object(obj)).map_err(|e| std::io::Error::other(e.to_string()))
}

fn kind_str(k: MetricKind) -> &'static str {
    match k {
        MetricKind::Counter => "counter",
        MetricKind::Gauge => "gauge",
        MetricKind::Histogram => "histogram_summary",
    }
}

fn value_to_json(v: &PeriodValue, kind: MetricKind) -> std::io::Result<Value> {
    let mismatch = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "PeriodValue variant does not match owning record's MetricKind",
        )
    };
    match (v, kind) {
        (PeriodValue::Counter(n), MetricKind::Counter) => Ok(Value::Number(Number::from(*n))),
        (PeriodValue::Gauge(x), MetricKind::Gauge) => Ok(num_or_null(*x)),
        (PeriodValue::Histogram(s), MetricKind::Histogram) => {
            let mut m = serde_json::Map::with_capacity(5);
            m.insert("count".into(), Value::Number(Number::from(s.count)));
            m.insert("sum".into(), num_or_null(s.sum));
            m.insert("p50".into(), num_or_null(s.p50));
            m.insert("p95".into(), num_or_null(s.p95));
            m.insert("p99".into(), num_or_null(s.p99));
            Ok(Value::Object(m))
        }
        _ => Err(mismatch()),
    }
}

/// JSON-safe f64 → Number. NaN / inf serialise as `null` per RFC 8259
/// (which the rest of the cosmix-mds pillar already enforces).
fn num_or_null(x: f64) -> Value {
    Number::from_f64(x)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

/// ISO-8601 UTC with millisecond precision, trailing `Z`. Matches
/// `cosmix-lib-log.md` §5 and plan §4.1 (Codex-anticipated MINOR:
/// no offset, always Z).
fn format_ts(ts: chrono::DateTime<chrono::Utc>) -> String {
    ts.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::types::HistogramSummary;
    use std::collections::BTreeMap;
    use std::io::{BufRead, BufReader};

    fn make_period(records: Vec<PeriodRecord>) -> PeriodSnapshot {
        PeriodSnapshot {
            ts: chrono::Utc.with_ymd_and_hms(2026, 5, 22, 12, 0, 0).unwrap(),
            host: "test-host".into(),
            service: "test-svc".into(),
            period_seconds: 60,
            records,
        }
    }

    fn counter_rec(
        name: &str,
        sens: LabelSensitivity,
        labels: &[(&str, &str)],
        v: u64,
        d: u64,
    ) -> PeriodRecord {
        PeriodRecord {
            metric: name.into(),
            kind: MetricKind::Counter,
            sensitivity: sens,
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            value: PeriodValue::Counter(v),
            delta: PeriodValue::Counter(d),
        }
    }

    use chrono::TimeZone;

    fn read_lines(path: &Path) -> Vec<String> {
        let f = File::open(path).expect("open .open file");
        BufReader::new(f)
            .lines()
            .map(|r| r.expect("line"))
            .collect()
    }

    #[test]
    fn safe_family_writes_labels_field_verbatim() {
        let tmp = tempfile::tempdir().unwrap();
        let sink = JsonlSink::daemon(tmp.path(), "test-svc", 256).unwrap();
        let rec = counter_rec(
            "maild_verdicts_total",
            LabelSensitivity::Safe,
            &[("verdict", "spam")],
            42,
            3,
        );
        sink.record_period(&make_period(vec![rec])).unwrap();
        let done = sink.done_path();
        sink.flush().unwrap();
        let lines = read_lines(&done);
        assert_eq!(lines.len(), 1);
        let v: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(v["metric"], "maild_verdicts_total");
        assert_eq!(v["kind"], "counter");
        assert_eq!(v["value"], 42);
        assert_eq!(v["delta"], 3);
        assert_eq!(v["period"], 60);
        assert_eq!(v["host"], "test-host");
        assert_eq!(v["service"], "test-svc");
        assert_eq!(v["labels"]["verdict"], "spam");
        assert!(
            v.get("labels_hash").is_none(),
            "Safe family must not write labels_hash"
        );
    }

    #[test]
    fn restricted_family_writes_labels_hash_xor_labels() {
        let tmp = tempfile::tempdir().unwrap();
        let sink = JsonlSink::daemon(tmp.path(), "test-svc", 256).unwrap();
        let rec = counter_rec(
            "maild_per_rule_total",
            LabelSensitivity::Restricted,
            &[("rule", "user-controlled-rule-name")],
            10,
            1,
        );
        sink.record_period(&make_period(vec![rec])).unwrap();
        let done = sink.done_path();
        sink.flush().unwrap();
        let lines = read_lines(&done);
        let v: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert!(
            v.get("labels").is_none(),
            "Restricted family must not write raw labels"
        );
        let hash = v["labels_hash"].as_str().expect("labels_hash present");
        assert_eq!(hash.len(), 16, "labels_hash is 16 hex chars");
        assert!(
            hash.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    #[test]
    fn histogram_value_and_delta_are_objects() {
        let tmp = tempfile::tempdir().unwrap();
        let sink = JsonlSink::daemon(tmp.path(), "test-svc", 256).unwrap();
        let rec = PeriodRecord {
            metric: "bus_request_duration_seconds".into(),
            kind: MetricKind::Histogram,
            sensitivity: LabelSensitivity::Safe,
            labels: BTreeMap::new(),
            value: PeriodValue::Histogram(HistogramSummary {
                count: 100,
                sum: 12.5,
                p50: 0.1,
                p95: 0.5,
                p99: 1.2,
            }),
            delta: PeriodValue::Histogram(HistogramSummary {
                count: 5,
                sum: 0.6,
                p50: 0.1,
                p95: 0.2,
                p99: 0.3,
            }),
        };
        sink.record_period(&make_period(vec![rec])).unwrap();
        let done = sink.done_path();
        sink.flush().unwrap();
        let lines = read_lines(&done);
        let v: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(v["kind"], "histogram_summary");
        assert_eq!(v["value"]["count"], 100);
        assert_eq!(v["delta"]["count"], 5);
        assert!(v["value"]["p99"].as_f64().unwrap() > 1.0);
        assert!(v["delta"]["p99"].as_f64().unwrap() < 1.0);
    }

    #[test]
    fn flush_renames_open_to_done() {
        let tmp = tempfile::tempdir().unwrap();
        let sink = JsonlSink::daemon(tmp.path(), "test-svc", 256).unwrap();
        let rec = counter_rec("c", LabelSensitivity::Safe, &[], 1, 1);
        sink.record_period(&make_period(vec![rec])).unwrap();
        let open_path = sink.open_path();
        let done_path = sink.done_path();
        assert!(open_path.exists());
        assert!(!done_path.exists());
        sink.flush().unwrap();
        assert!(!open_path.exists(), "open path must be gone after flush");
        assert!(done_path.exists(), "done path must exist after flush");
    }

    #[test]
    fn flush_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let sink = JsonlSink::daemon(tmp.path(), "test-svc", 256).unwrap();
        sink.flush().unwrap();
        // Second flush is a no-op (the file is already renamed).
        sink.flush().unwrap();
        let rec = counter_rec("c", LabelSensitivity::Safe, &[], 1, 1);
        let err = sink.record_period(&make_period(vec![rec])).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::Other);
    }

    #[test]
    fn filename_collision_retries_with_numeric_suffix() {
        // Deterministic exercise of the suffix-walk (Codex round-2 MINOR
        // fix). Driving through `JsonlSink::daemon` twice didn't
        // reliably collide because `startup_timestamp()` resolution is
        // 1 second; the prior test happened to pass even when the
        // second sink picked a fresh stem and never hit the retry loop.
        // Calling `open_with_collision_retry` with a fixed stem and
        // pre-creating the bare `.jsonl.open` forces the walk to bump
        // to `-1`, then `-2`.
        let tmp = tempfile::tempdir().unwrap();
        let stem = "test-svc.stats.daemon-1-1700000000";
        let bare_open = tmp.path().join(format!("{stem}.jsonl.open"));
        std::fs::write(&bare_open, b"").unwrap();

        let (open_path, done_path, _file) = open_with_collision_retry(tmp.path(), stem).unwrap();
        assert_eq!(
            open_path,
            tmp.path().join(format!("{stem}-1.jsonl.open")),
            "first collision must bump to -1",
        );
        assert_eq!(
            done_path,
            tmp.path().join(format!("{stem}-1.jsonl.done")),
            "-1 done path tracks -1 open path",
        );

        // Second retry: -1 is now taken (by the just-returned File),
        // so the next call must bump to -2.
        let (open_path_2, _, _file2) = open_with_collision_retry(tmp.path(), stem).unwrap();
        assert_eq!(
            open_path_2,
            tmp.path().join(format!("{stem}-2.jsonl.open")),
            "second collision must bump to -2",
        );

        // Stale `.done` companion of the bare stem also forces a bump
        // — the pre-flush rename would otherwise clobber it.
        let bare_open_3 = tmp.path().join("alt-stem.jsonl.open");
        let bare_done_3 = tmp.path().join("alt-stem.jsonl.done");
        std::fs::write(&bare_done_3, b"").unwrap();
        let (open_path_3, _, _file3) = open_with_collision_retry(tmp.path(), "alt-stem").unwrap();
        assert_eq!(
            open_path_3,
            tmp.path().join("alt-stem-1.jsonl.open"),
            "pre-existing .done must trigger the same suffix walk",
        );
        assert!(
            !bare_open_3.exists(),
            "bare .open must NOT have been left behind by the rejected open",
        );
    }

    #[test]
    fn hard_budget_pauses_disk_writes() {
        let tmp = tempfile::tempdir().unwrap();
        // Soft budget = 1 MiB; hard = 1 GiB. We can't realistically
        // write 1 GiB in a test, so we directly verify the pause
        // logic by reaching into the inner state.
        let sink = JsonlSink::daemon(tmp.path(), "test-svc", 1).unwrap();
        // Forge a state where bytes_today is just under the hard
        // ceiling and one more record will push it over. The pre-check
        // (Codex round-2 fix) rejects the entire period; no bytes land
        // on disk for this call, and the sink pauses.
        {
            let mut inner = sink.inner.lock().unwrap();
            inner.bytes_today = HARD_BUDGET_BYTES - 10;
        }
        let rec = counter_rec("c", LabelSensitivity::Safe, &[("k", "v")], 1, 1);
        sink.record_period(&make_period(vec![rec.clone()])).unwrap();
        {
            let inner = sink.inner.lock().unwrap();
            assert!(inner.paused, "projected > hard ceiling → paused");
            assert_eq!(
                inner.bytes_today,
                HARD_BUDGET_BYTES - 10,
                "rejected period must NOT bump bytes_today",
            );
        }
        // A second call with the sink already paused also writes
        // nothing, returns Ok.
        sink.record_period(&make_period(vec![rec])).unwrap();
        let done = sink.done_path();
        sink.flush().unwrap();
        let lines = read_lines(&done);
        assert!(
            lines.is_empty(),
            "no lines written: the first period was rejected by the pre-check, the second by the pause flag",
        );
    }

    #[test]
    fn partial_period_serialization_failure_writes_nothing() {
        // Atomicity guard (Codex round-2 MAJOR fix). A period whose
        // record 2 is malformed (Histogram-kind paired with Counter
        // PeriodValue) must reject the whole period — record 1 cannot
        // have been written to disk before record 2's serializer
        // failed.
        let tmp = tempfile::tempdir().unwrap();
        let sink = JsonlSink::daemon(tmp.path(), "test-svc", 256).unwrap();
        let good = counter_rec("c", LabelSensitivity::Safe, &[("k", "v")], 1, 1);
        let bad = PeriodRecord {
            metric: "h".to_string(),
            kind: MetricKind::Histogram,
            sensitivity: LabelSensitivity::Safe,
            labels: BTreeMap::new(),
            // Wrong PeriodValue variant for the Histogram kind: triggers
            // the InvalidData branch in `build_line`.
            value: PeriodValue::Counter(7),
            delta: PeriodValue::Counter(7),
        };
        let err = sink
            .record_period(&make_period(vec![good, bad]))
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        {
            let inner = sink.inner.lock().unwrap();
            assert_eq!(
                inner.bytes_today, 0,
                "rejected period must NOT touch bytes_today",
            );
        }
        let done = sink.done_path();
        sink.flush().unwrap();
        assert!(
            read_lines(&done).is_empty(),
            "no record from a partially-serialized period may reach disk",
        );
    }

    #[test]
    fn utc_midnight_rollover_resets_budget_and_unpauses() {
        let tmp = tempfile::tempdir().unwrap();
        let sink = JsonlSink::daemon(tmp.path(), "test-svc", 1).unwrap();
        {
            let mut inner = sink.inner.lock().unwrap();
            inner.bytes_today = HARD_BUDGET_BYTES;
            inner.paused = true;
            // Force the budget_day into the past so the next
            // record_period sees a UTC-day mismatch and resets.
            inner.budget_day = inner.budget_day.pred_opt().unwrap();
        }
        let rec = counter_rec("c", LabelSensitivity::Safe, &[], 1, 1);
        sink.record_period(&make_period(vec![rec])).unwrap();
        let inner = sink.inner.lock().unwrap();
        assert!(!inner.paused, "UTC-day rollover must clear the pause flag");
        // bytes_today should reflect only this period's write, not
        // the carry-over from the prior day.
        assert!(
            inner.bytes_today < 1024,
            "budget counter reset on UTC-day rollover"
        );
    }

    #[test]
    fn timestamp_is_iso8601_utc_with_milliseconds_and_z() {
        let tmp = tempfile::tempdir().unwrap();
        let sink = JsonlSink::daemon(tmp.path(), "svc", 256).unwrap();
        let rec = counter_rec("c", LabelSensitivity::Safe, &[], 1, 1);
        sink.record_period(&make_period(vec![rec])).unwrap();
        let done = sink.done_path();
        sink.flush().unwrap();
        let lines = read_lines(&done);
        let v: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        let ts = v["ts"].as_str().unwrap();
        assert!(ts.ends_with('Z'), "ts ends with Z: {ts}");
        assert!(ts.contains('T'), "ts is ISO-8601: {ts}");
        assert!(ts.contains('.'), "ts has milliseconds: {ts}");
    }

    #[test]
    fn record_period_after_flush_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let sink = JsonlSink::daemon(tmp.path(), "svc", 256).unwrap();
        sink.flush().unwrap();
        let rec = counter_rec("c", LabelSensitivity::Safe, &[], 1, 1);
        let err = sink.record_period(&make_period(vec![rec])).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::Other);
    }

    #[test]
    fn daemon_and_delta_classes_produce_distinct_discriminators() {
        let tmp = tempfile::tempdir().unwrap();
        let d = JsonlSink::daemon(tmp.path(), "svc", 256).unwrap();
        let m = JsonlSink::delta(tmp.path(), "svc", 16).unwrap();
        let d_name = d
            .open_path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let m_name = m
            .open_path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(d_name.contains(".stats.daemon-"), "{d_name}");
        assert!(m_name.contains(".stats.delta-"), "{m_name}");
    }

    #[test]
    fn period_value_mismatched_kind_errors() {
        // value_to_json is internal; verify by constructing a
        // mismatched record and ensuring build_line surfaces the
        // InvalidData error.
        let period = make_period(vec![PeriodRecord {
            metric: "x".into(),
            kind: MetricKind::Counter,
            sensitivity: LabelSensitivity::Safe,
            labels: BTreeMap::new(),
            value: PeriodValue::Gauge(1.0), // mismatched: kind=Counter
            delta: PeriodValue::Counter(0),
        }]);
        let tmp = tempfile::tempdir().unwrap();
        let sink = JsonlSink::daemon(tmp.path(), "svc", 256).unwrap();
        let err = sink.record_period(&period).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
