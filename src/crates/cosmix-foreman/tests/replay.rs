//! Golden vendor-stream replay through the complete runner path.
//!
//! Each test creates two scratch ledgers, seeds the same task, then runs the
//! same captured stdout lines, line-to-line deltas and exit status through
//! the real driver spawn/parser/session path and the runner's
//! claim→lower→stream→record→disposition path. It compares every column in
//! the final `tasks`, `runs`, and `events` rows after removing only the
//! field named in [`EXCLUSIONS`]. A mismatch reports the first table, row and
//! field that diverged.
//!
//! This proves that pinned driver output and supplied time reproduce the
//! terminal ledger projection for these paths. It does not prove that a live
//! vendor will emit the same stream, that tool side effects outside the
//! ledger are replayable, or that exact deadline-boundary scheduling is
//! deterministic.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};
use cosmix_foreman::clock::RunClock;
use cosmix_foreman::driver::claude::ClaudeDriver;
use cosmix_foreman::driver::codex::CodexDriver;
use cosmix_foreman::executor::{Budget, Executor, StopReason};
use cosmix_foreman::ledger::Ledger;
use cosmix_foreman::replay::CapturedStream;
use cosmix_foreman::runner::{RunOptions, run_task_with_clock};
use rusqlite::types::ValueRef;
use serde_json::{Map, Value, json};

#[derive(Clone, Copy)]
enum ExclusionTarget {
    LedgerField {
        table: &'static str,
        field: &'static str,
    },
    NonLedgerInput {
        name: &'static str,
    },
}

struct Exclusion {
    target: ExclusionTarget,
    rationale: &'static str,
}

/// Honest ledger-visible nondeterminism. Do not add a field here merely to
/// turn a red diff green: first name the source and why identity is neither
/// possible nor desirable.
const EXCLUSIONS: &[Exclusion] = &[
    Exclusion {
        target: ExclusionTarget::LedgerField {
            table: "tasks",
            field: "created_at",
        },
        rationale: "the task is test-fixture seed data created before the replay clock owns the claim-to-disposition path",
    },
    Exclusion {
        target: ExclusionTarget::NonLedgerInput {
            name: "runner.claimant_pid",
        },
        rationale: "claim ownership deliberately identifies the live process; terminal disposition clears claimed_by, so no differing value survives in the compared rows",
    },
    Exclusion {
        target: ExclusionTarget::NonLedgerInput {
            name: "prompt.findings_nonce",
        },
        rationale: "the anti-forgery fence must be freshly minted per attempt; pinned vendor stdout makes it prompt-only and it is never stored in the terminal ledger projection",
    },
];

struct ReplayClock {
    base: DateTime<Utc>,
    state: Mutex<ReplayTime>,
}

struct ReplayTime {
    now: Duration,
    line_deltas: VecDeque<Duration>,
}

impl ReplayClock {
    fn from_capture(capture: &CapturedStream) -> Self {
        Self {
            base: "2026-08-22T00:00:00Z".parse().expect("fixed replay epoch"),
            state: Mutex::new(ReplayTime {
                now: Duration::ZERO,
                line_deltas: capture
                    .lines
                    .iter()
                    .map(|line| Duration::from_millis(line.after_ms))
                    .collect(),
            }),
        }
    }

    fn assert_consumed(&self, fixture: &str) {
        let state = self.state.lock().unwrap();
        assert!(
            state.line_deltas.is_empty(),
            "{fixture}: replay ended with {} captured line deltas unused",
            state.line_deltas.len()
        );
    }
}

impl RunClock for ReplayClock {
    fn monotonic(&self) -> Duration {
        self.state.lock().unwrap().now
    }

    fn wall_now(&self) -> DateTime<Utc> {
        let now = self.state.lock().unwrap().now;
        self.base + chrono::Duration::from_std(now).expect("fixture duration fits chrono")
    }

    fn line_arrived(&self) {
        let mut state = self.state.lock().unwrap();
        let delta = state
            .line_deltas
            .pop_front()
            .expect("driver produced more raw lines than the capture contains");
        state.now += delta;
    }

    fn timeout_elapsed(&self, wait: Duration) {
        self.state.lock().unwrap().now += wait;
    }

    fn sleep(&self, _duration: Duration) {
        // Child/pipe reaping needs only a scheduling yield for these finite
        // fixture processes. It must not invent elapsed replay time: all
        // ledger-visible duration comes from captured line deltas.
        std::thread::yield_now();
    }
}

#[derive(Clone, Copy)]
enum Lane {
    Claude,
    Codex,
}

fn manifest_dir() -> PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")))
}

fn fixture_path(name: &str) -> PathBuf {
    manifest_dir().join("testdata/replay").join(name)
}

fn fixture_executor(lane: Lane, fixture: &Path) -> Box<dyn Executor> {
    let program = env!("CARGO_BIN_EXE_foreman-stream-fixture");
    let fixture = fixture.to_string_lossy().into_owned();
    match lane {
        Lane::Claude => Box::new(
            ClaudeDriver::new()
                .with_program(program)
                .with_env("FOREMAN_REPLAY_FIXTURE", fixture)
                .with_env("FOREMAN_REPLAY_FAST", "1"),
        ),
        Lane::Codex => Box::new(
            CodexDriver::new()
                .with_program(program)
                .with_env("FOREMAN_REPLAY_FIXTURE", fixture)
                .with_env("FOREMAN_REPLAY_FAST", "1"),
        ),
    }
}

fn run_once(
    db: &Path,
    workdir: &Path,
    fixture_name: &str,
    lane: Lane,
    budget: Budget,
) -> (Value, StopReason, &'static str) {
    let fixture = fixture_path(fixture_name);
    let capture = CapturedStream::load(&fixture).unwrap();
    let expected_lane = match lane {
        Lane::Claude => "claude",
        Lane::Codex => "codex",
    };
    assert_eq!(capture.lane, expected_lane, "{fixture_name}: lane metadata");
    let clock = ReplayClock::from_capture(&capture);
    let ledger = Ledger::open(db).unwrap();
    let task = ledger
        .add_task(
            "fixture task",
            "fixture specification with no identifying text",
            "impl",
            "low",
            &[],
            "none",
        )
        .unwrap();
    let executor = fixture_executor(lane, &fixture);
    let options = RunOptions {
        workdir: workdir.to_path_buf(),
        budget,
        stall_secs: 60,
        verify: false,
        ..Default::default()
    };
    let report = run_task_with_clock(&ledger, task, executor.as_ref(), &options, &clock).unwrap();
    let captured_duration: u64 = capture.lines.iter().map(|line| line.after_ms).sum();
    assert_eq!(
        report.duration_ms,
        i64::try_from(captured_duration).unwrap(),
        "{fixture_name}: runner duration must come from captured line deltas"
    );
    clock.assert_consumed(fixture_name);
    drop(ledger);
    (ledger_snapshot(db), report.outcome.stop, report.task_status)
}

fn assert_replay(
    fixture_name: &str,
    lane: Lane,
    budget: Budget,
    expected_stop: StopReason,
    expected_task: &str,
) {
    let tmp = tempfile::tempdir().unwrap();
    let workdir = tmp.path().join("worktree");
    std::fs::create_dir(&workdir).unwrap();
    let first = run_once(
        &tmp.path().join("first.db"),
        &workdir,
        fixture_name,
        lane,
        budget.clone(),
    );
    let second = run_once(
        &tmp.path().join("second.db"),
        &workdir,
        fixture_name,
        lane,
        budget,
    );
    assert_eq!(first.1, expected_stop, "{fixture_name}: first stop");
    assert_eq!(second.1, expected_stop, "{fixture_name}: second stop");
    assert_eq!(first.2, expected_task, "{fixture_name}: first task status");
    assert_eq!(
        second.2, expected_task,
        "{fixture_name}: second task status"
    );

    let left = apply_exclusions(first.0);
    let right = apply_exclusions(second.0);
    if let Some((path, left, right)) = first_difference(&left, &right, "ledger") {
        panic!("{fixture_name}: replay ledger differs at {path}\nfirst: {left}\nsecond: {right}");
    }
}

fn apply_exclusions(mut snapshot: Value) -> Value {
    for exclusion in EXCLUSIONS {
        assert!(
            !exclusion.rationale.trim().is_empty(),
            "every exclusion needs a rationale"
        );
        match exclusion.target {
            ExclusionTarget::LedgerField { table, field } => {
                let rows = snapshot
                    .get_mut(table)
                    .and_then(Value::as_array_mut)
                    .unwrap_or_else(|| panic!("excluded table {table} is absent"));
                for row in rows {
                    let removed = row
                        .as_object_mut()
                        .expect("snapshot rows are objects")
                        .remove(field);
                    assert!(
                        removed.is_some(),
                        "excluded field {table}.{field} is absent"
                    );
                }
            }
            ExclusionTarget::NonLedgerInput { name } => {
                assert!(!name.trim().is_empty(), "non-ledger exclusion needs a name");
            }
        }
    }
    snapshot
}

fn ledger_snapshot(path: &Path) -> Value {
    let connection = rusqlite::Connection::open(path).unwrap();
    json!({
        "tasks": table_rows(&connection, "tasks"),
        "runs": table_rows(&connection, "runs"),
        "events": table_rows(&connection, "events"),
    })
}

fn table_rows(connection: &rusqlite::Connection, table: &str) -> Value {
    let mut columns = Vec::new();
    let mut info = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .unwrap();
    let names = info.query_map([], |row| row.get::<_, String>(1)).unwrap();
    for name in names {
        columns.push(name.unwrap());
    }
    assert!(!columns.is_empty(), "snapshot table {table} has no columns");

    let mut statement = connection
        .prepare(&format!("SELECT * FROM {table} ORDER BY id"))
        .unwrap();
    let rows = statement
        .query_map([], |row| {
            let mut object = Map::new();
            for (index, name) in columns.iter().enumerate() {
                object.insert(name.clone(), sqlite_value(row.get_ref(index)?));
            }
            Ok(Value::Object(object))
        })
        .unwrap();
    Value::Array(rows.map(|row| row.unwrap()).collect())
}

fn sqlite_value(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(value) => value.into(),
        ValueRef::Real(value) => serde_json::Number::from_f64(value)
            .map(Value::Number)
            .expect("SQLite stored a non-finite REAL"),
        ValueRef::Text(value) => String::from_utf8_lossy(value).into_owned().into(),
        ValueRef::Blob(value) => Value::Array(value.iter().map(|byte| (*byte).into()).collect()),
    }
}

fn first_difference(left: &Value, right: &Value, path: &str) -> Option<(String, Value, Value)> {
    match (left, right) {
        (Value::Object(left), Value::Object(right)) => {
            let mut keys: Vec<_> = left.keys().chain(right.keys()).collect();
            keys.sort_unstable();
            keys.dedup();
            for key in keys {
                let child = format!("{path}.{key}");
                match (left.get(key), right.get(key)) {
                    (Some(left), Some(right)) => {
                        if let Some(diff) = first_difference(left, right, &child) {
                            return Some(diff);
                        }
                    }
                    (left, right) => {
                        return Some((
                            child,
                            left.cloned().unwrap_or(Value::Null),
                            right.cloned().unwrap_or(Value::Null),
                        ));
                    }
                }
            }
            None
        }
        (Value::Array(left), Value::Array(right)) => {
            if left.len() != right.len() {
                return Some((
                    format!("{path}.length"),
                    left.len().into(),
                    right.len().into(),
                ));
            }
            for (index, (left, right)) in left.iter().zip(right).enumerate() {
                if let Some(diff) = first_difference(left, right, &format!("{path}[{index}]")) {
                    return Some(diff);
                }
            }
            None
        }
        _ if left == right => None,
        _ => Some((path.to_string(), left.clone(), right.clone())),
    }
}

#[test]
fn claude_done_replays_identically() {
    assert_replay(
        "claude-done.stream.jsonl",
        Lane::Claude,
        Budget::default(),
        StopReason::Done,
        "done",
    );
}

#[test]
fn claude_budget_kill_replays_identically() {
    assert_replay(
        "claude-budget.stream.jsonl",
        Lane::Claude,
        Budget {
            max_output_tokens: Some(100),
            ..Default::default()
        },
        StopReason::BudgetCeiling,
        "bounced",
    );
}

#[test]
fn claude_mid_stream_death_replays_identically() {
    assert_replay(
        "claude-death.stream.jsonl",
        Lane::Claude,
        Budget::default(),
        StopReason::Error,
        "failed",
    );
}

#[test]
fn codex_done_replays_identically() {
    assert_replay(
        "codex-done.stream.jsonl",
        Lane::Codex,
        Budget::default(),
        StopReason::Done,
        "done",
    );
}

#[test]
fn codex_budget_kill_replays_identically() {
    assert_replay(
        "codex-budget.stream.jsonl",
        Lane::Codex,
        Budget {
            max_output_tokens: Some(100),
            ..Default::default()
        },
        StopReason::BudgetCeiling,
        "bounced",
    );
}

#[test]
fn codex_mid_stream_death_replays_identically() {
    assert_replay(
        "codex-death.stream.jsonl",
        Lane::Codex,
        Budget::default(),
        StopReason::Error,
        "failed",
    );
}

#[test]
fn capture_wrapper_records_lines_deltas_and_exit_status() {
    let tmp = tempfile::tempdir().unwrap();
    let fixture = tmp.path().join("capture.stream.jsonl");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_foreman-stream-fixture"))
        .args(["-c", "printf 'one\\ntwo\\n'; exit 7"])
        .env("FOREMAN_CAPTURE_VENDOR_BIN", "/bin/sh")
        .env("FOREMAN_CAPTURE_FIXTURE", &fixture)
        .env("FOREMAN_CAPTURE_LANE", "claude")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(output.stdout, b"one\ntwo\n");
    let capture = CapturedStream::load(&fixture).unwrap();
    assert_eq!(capture.lane, "claude");
    assert_eq!(
        capture
            .lines
            .iter()
            .map(|line| line.stdout.as_str())
            .collect::<Vec<_>>(),
        ["one", "two"]
    );
    assert_eq!(capture.exit_code, Some(7));
    assert_eq!(capture.exit_signal, None);
}

#[test]
fn malformed_capture_cannot_overflow_time_or_forge_an_exit() {
    let tmp = tempfile::tempdir().unwrap();
    let excessive = tmp.path().join("excessive.stream.jsonl");
    std::fs::write(
        &excessive,
        concat!(
            "{\"record\":\"meta\",\"version\":1,\"lane\":\"claude\"}\n",
            "{\"record\":\"line\",\"after_ms\":18446744073709551615,\"stdout\":\"{}\"}\n",
            "{\"record\":\"exit\",\"code\":0,\"signal\":null}\n",
        ),
    )
    .unwrap();
    let error = CapturedStream::load(&excessive).unwrap_err().to_string();
    assert!(
        error.contains("i64 millisecond range"),
        "unexpected excessive-time error: {error}"
    );

    let invalid_exit = tmp.path().join("invalid-exit.stream.jsonl");
    std::fs::write(
        &invalid_exit,
        concat!(
            "{\"record\":\"meta\",\"version\":1,\"lane\":\"codex\"}\n",
            "{\"record\":\"exit\",\"code\":999,\"signal\":null}\n",
        ),
    )
    .unwrap();
    let error = CapturedStream::load(&invalid_exit).unwrap_err().to_string();
    assert!(
        error.contains("outside 0..=255"),
        "unexpected invalid-exit error: {error}"
    );
}
