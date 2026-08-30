use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use cosmix_foreman::config::FleetPolicy;
use cosmix_foreman::executor::{AgentKind, Budget};
use cosmix_foreman::governor::Governor;
use cosmix_foreman::ledger::Ledger;

mod support;

const POLICY_ENV: &[&str] = &[
    "FOREMAN_CONF",
    "FOREMAN_LADDER",
    "FOREMAN_START_RUNG",
    "FOREMAN_LADDER_PATIENCE",
    "FOREMAN_DAILY_BUDGET_USD",
    "FOREMAN_DAILY_OUTPUT_TOKENS",
    "FOREMAN_REVIEW_MODEL",
    "FOREMAN_CODEX_REVIEW_MODEL",
    "FOREMAN_REVIEW_STALL_SECS",
    "FOREMAN_CODEX_REVIEW_STALL_SECS",
    "FOREMAN_REVIEW_OVERRIDE",
    "FOREMAN_TWO_ARM_REVIEW",
    "FOREMAN_CLAUDE_BIN",
    "FOREMAN_CODEX_BIN",
    "FOREMAN_RESERVE_USD",
    "FOREMAN_RESERVE_TOKENS",
    "FOREMAN_SIBLING_REPOS",
    "FOREMAN_TIER0_TIMEOUT_SECS",
    "FOREMAN_TIER1_TIMEOUT_SECS",
    "FOREMAN_TIER2_TIMEOUT_SECS",
    "FOREMAN_TIER2_COMMANDS",
    "FOREMAN_SCRATCH_TERMINAL_AGE_HOURS",
    "FOREMAN_SCRATCH_POOL",
    "FOREMAN_SCRATCH_PRESSURE_PERCENT",
    "FOREMAN_SCRATCH_SHARED_MAX_GB",
    "CONFIGURATION_DIRECTORY",
];

fn foreman(db: &Path, args: &[&str]) -> Output {
    foreman_with_env(db, args, &[])
}

fn foreman_with_env(db: &Path, args: &[&str], env: &[(&str, &Path)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_foreman"));
    command
        .arg("--db")
        .arg(db)
        .args(args)
        .env(
            "FOREMAN_VERIFY_LANE",
            db.parent().unwrap().join("verify.lock"),
        )
        .env("FOREMAN_VERIFY_LANE_WAIT_SECS", "30");
    for key in POLICY_ENV {
        command.env_remove(key);
    }
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().expect("run foreman")
}

fn conf_path(db: &Path) -> PathBuf {
    db.parent().unwrap().join("foreman.conf.mix")
}

#[cfg(unix)]
fn executable_script(path: &Path, body: &str) {
    support::write_executable(path, format!("#!/bin/sh\n{body}\n"));
}

#[test]
fn config_show_json_reports_values_sources_and_missing_file() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let output = foreman(&db, &["config", "show", "--json"]);
    assert!(output.status.success(), "{:?}", output);
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["conf_file"]["found"], false);
    assert_eq!(json["ladder"]["source"], "default");
    assert_eq!(json["start_rung"]["value"], 0);
    assert_eq!(json["start_rung"]["source"], "default");
    assert_eq!(json["daily_budget_usd"]["source"], "default");
    assert_eq!(json["tier_timeout_secs"]["source"]["0"], "default");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("configuration missing at"), "{stderr}");
    assert!(
        stderr.contains(conf_path(&db).to_str().unwrap()),
        "{stderr}"
    );
}

#[test]
fn config_show_json_reports_conf_reserve_sources() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    std::fs::write(
        conf_path(&db),
        "reserve_usd: 2.5\nreserve_tokens: 123456\nreview_stall_secs: 240\ncodex_review_stall_secs: 1050\n",
    )
    .unwrap();

    let output = foreman(&db, &["config", "show", "--json"]);
    assert!(output.status.success(), "{:?}", output);
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["reserve_usd"]["value"], 2.5);
    assert_eq!(json["reserve_usd"]["source"], "conf");
    assert_eq!(json["reserve_tokens"]["value"], 123456);
    assert_eq!(json["reserve_tokens"]["source"], "conf");
    assert_eq!(json["review_stall_secs"]["value"], 240);
    assert_eq!(json["review_stall_secs"]["source"], "conf");
    assert_eq!(json["codex_review_stall_secs"]["value"], 1050);
    assert_eq!(json["codex_review_stall_secs"]["source"], "conf");
}

#[test]
fn next_dispatch_reads_changed_conf_without_a_unit_change() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    std::fs::write(
        conf_path(&db),
        "ladder: [\"glm\", \"codex\"]\nstart_rung: 0\n",
    )
    .unwrap();
    assert!(foreman(&db, &["init"]).status.success());
    assert!(
        foreman(&db, &["task", "add", "route me", "--spec", "fixture"])
            .status
            .success()
    );

    let first = foreman(&db, &["dispatch", "--dry-run"]);
    assert!(first.status.success(), "{:?}", first);
    assert!(String::from_utf8_lossy(&first.stdout).contains("-> glm"));

    std::fs::write(
        conf_path(&db),
        "ladder: [\"glm\", \"codex\"]\nstart_rung: 1\n",
    )
    .unwrap();
    // Wake is best effort by contract. Its invocation also validates and
    // reads the new policy; no daemon-reload or unit installation occurs.
    assert!(foreman(&db, &["wake"]).status.success());
    let second = foreman(&db, &["dispatch", "--dry-run"]);
    assert!(second.status.success(), "{:?}", second);
    assert!(String::from_utf8_lossy(&second.stdout).contains("-> codex"));
}

#[test]
fn governor_status_reports_conf_ceiling_without_env() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    std::fs::write(conf_path(&db), "daily_budget_usd: 321\n").unwrap();
    assert!(foreman(&db, &["init"]).status.success());
    let output = foreman(&db, &["governor", "status"]);
    assert!(output.status.success(), "{:?}", output);
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("daily budget: $321.00 (source: conf)"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn configuration_directory_supplies_policy_when_ledger_has_no_conf() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("state/ledger.db");
    let config_dir = tmp.path().join("etc");
    std::fs::create_dir(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("foreman.conf.mix"),
        "daily_budget_usd: 222\n",
    )
    .unwrap();

    let output = foreman_with_env(
        &db,
        &["governor", "status"],
        &[("CONFIGURATION_DIRECTORY", &config_dir)],
    );
    assert!(output.status.success(), "{:?}", output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("daily budget: $222.00 (source: conf)"),
        "{stdout}"
    );
}

#[test]
fn conf_beside_ledger_wins_over_configuration_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("state/ledger.db");
    let config_dir = tmp.path().join("etc");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    std::fs::create_dir(&config_dir).unwrap();
    std::fs::write(conf_path(&db), "daily_budget_usd: 333\n").unwrap();
    std::fs::write(
        config_dir.join("foreman.conf.mix"),
        "daily_budget_usd: 222\n",
    )
    .unwrap();

    let output = foreman_with_env(
        &db,
        &["governor", "status"],
        &[("CONFIGURATION_DIRECTORY", &config_dir)],
    );
    assert!(output.status.success(), "{:?}", output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("daily budget: $333.00 (source: conf)"),
        "{stdout}"
    );
}

#[test]
fn conf_budget_is_shared_by_dispatch_and_review_reservations() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    std::fs::write(conf_path(&db), "daily_budget_usd: 9\n").unwrap();
    let ledger = Ledger::open(&db).unwrap();

    // These are the constructors used by the dispatch/run reservation and
    // the refinery review reservation respectively. Both resolve beside the
    // same ledger and therefore cannot disagree about the ceiling.
    let policy = FleetPolicy::load_conf_file(&conf_path(&db)).unwrap();
    let dispatch = Governor::from_policy(&db, &policy);
    let review = Governor::from_policy(&db, &policy);
    assert_eq!(dispatch.daily_budget_usd, 9.0);
    assert_eq!(review.daily_budget_usd, 9.0);

    let hold = dispatch
        .reserve(
            &ledger,
            "dispatch@test",
            None,
            &Budget {
                max_budget_usd: Some(6.0),
                max_output_tokens: Some(1),
                ..Default::default()
            },
            AgentKind::Claude,
        )
        .unwrap();
    let refused = review
        .reserve(
            &ledger,
            "review@test",
            None,
            &Budget {
                max_budget_usd: Some(4.0),
                max_output_tokens: Some(1),
                ..Default::default()
            },
            AgentKind::Claude,
        )
        .unwrap_err();
    assert!(format!("{refused:#}").contains("daily"), "{refused:#}");
    dispatch.release(&ledger, hold).unwrap();
}

#[test]
fn conf_reserve_is_used_for_uncapped_governor_reservations() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    std::fs::write(
        conf_path(&db),
        "daily_budget_usd: 20\n\
         daily_output_tokens: 1000\n\
         reserve_usd: 3\n\
         reserve_tokens: 200\n",
    )
    .unwrap();
    let ledger = Ledger::open(&db).unwrap();
    let policy = FleetPolicy::load_conf_file(&conf_path(&db)).unwrap();
    let governor = Governor::from_policy(&db, &policy);

    governor
        .reserve(
            &ledger,
            "dispatch@test",
            None,
            &Budget::default(),
            AgentKind::Claude,
        )
        .unwrap();
    let status = governor.status(&ledger).unwrap();
    assert!((status.reserved_usd - 3.0).abs() < f64::EPSILON);
    assert_eq!(status.reserved_tokens, 200);
}

#[test]
fn task_add_budget_above_resolved_daily_ceiling_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    std::fs::write(conf_path(&db), "daily_budget_usd: 6\n").unwrap();

    let output = foreman(
        &db,
        &[
            "task", "add", "too dear", "--spec", "fixture", "--budget", "7",
        ],
    );
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("task --budget $7.0000"), "{stderr}");
    assert!(
        stderr.contains("daily_budget_usd ceiling $6.0000"),
        "{stderr}"
    );
    assert!(
        Ledger::open(&db)
            .unwrap()
            .tasks(None, true)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn task_add_budget_accepts_disabled_daily_dollar_ceiling() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    std::fs::write(
        conf_path(&db),
        "daily_budget_usd: 0\nladder: [\"claude:sonnet\"]\n",
    )
    .unwrap();

    let output = foreman(
        &db,
        &[
            "task",
            "add",
            "uncapped daily budget",
            "--spec",
            "fixture",
            "--budget",
            "10",
        ],
    );
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        Ledger::open(&db)
            .unwrap()
            .task(1)
            .unwrap()
            .unwrap()
            .budget_usd,
        Some(10.0)
    );
}

#[test]
fn task_add_budget_refuses_a_ladder_with_no_dollar_metering_lane() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    std::fs::write(
        conf_path(&db),
        "daily_budget_usd: 20\nladder: [\"codex\", \"glm\"]\n",
    )
    .unwrap();

    let output = foreman(
        &db,
        &[
            "task",
            "add",
            "unmetered",
            "--spec",
            "fixture",
            "--budget",
            "10",
        ],
    );
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("requires a dollar-metering ladder rung"),
        "{stderr}"
    );
    assert!(stderr.contains("codex, glm"), "{stderr}");
}

#[test]
fn task_add_budget_rejects_when_only_metered_rungs_precede_start_rung() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    std::fs::write(
        conf_path(&db),
        "daily_budget_usd: 20\nladder: [\"claude:sonnet\", \"codex\"]\nstart_rung: 1\n",
    )
    .unwrap();

    let output = foreman(
        &db,
        &[
            "task",
            "add",
            "unreachable-meter",
            "--spec",
            "fixture",
            "--budget",
            "10",
        ],
    );
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("requires a dollar-metering ladder rung reachable from start_rung 1"),
        "{stderr}"
    );
    assert!(stderr.contains("[codex]"), "{stderr}");
    // Admission failed before any row was written.
    let shown = foreman(&db, &["task", "show", "1"]);
    assert!(!shown.status.success(), "{shown:?}");
}

#[test]
fn budgeted_task_on_mixed_ladder_refuses_non_metering_run_before_claim() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    std::fs::write(
        conf_path(&db),
        "daily_budget_usd: 20\nladder: [\"codex\", \"claude:sonnet\"]\n",
    )
    .unwrap();
    assert!(
        foreman(
            &db,
            &[
                "task", "add", "mixed", "--spec", "fixture", "--budget", "10",
            ],
        )
        .status
        .success()
    );

    let workdir = tmp.path().to_string_lossy().into_owned();
    let output = foreman(
        &db,
        &[
            "run",
            "--task",
            "1",
            "--agent",
            "codex",
            "--workdir",
            &workdir,
            "--no-verify",
        ],
    );
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot meter or enforce task --budget"),
        "{stderr}"
    );
    let ledger = Ledger::open(&db).unwrap();
    let task = ledger.task(1).unwrap().unwrap();
    assert_eq!(task.status, "queued");
    assert_eq!(task.attempt, 0);
    assert!(ledger.recent_runs(10).unwrap().is_empty());
    assert_eq!(ledger.reserved_totals().unwrap(), (0.0, 0));
}

#[cfg(unix)]
#[test]
fn narrower_explicit_cap_is_both_live_reservation_and_run_dollar_cap() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let capture = tmp.path().join("capture.txt");
    let claude = tmp.path().join("fake-claude");
    std::fs::write(
        conf_path(&db),
        "daily_budget_usd: 40\nreserve_usd: 3\nladder: [\"claude:sonnet\"]\n",
    )
    .unwrap();
    executable_script(
        &claude,
        &format!(
            "printf '%s\\n' \"$*\" > '{}'\n\
             /usr/sbin/sqlite3 '{}' \\
             \"SELECT printf('%.4f', usd) FROM reservations WHERE task_id = 1\" >> '{}'\n\
             exit 1",
            capture.display(),
            db.display(),
            capture.display()
        ),
    );
    assert!(
        foreman(
            &db,
            &[
                "task",
                "add",
                "large",
                "--spec",
                "fixture",
                "--budget",
                "20",
                "--verifier",
                "none",
            ],
        )
        .status
        .success()
    );

    let workdir = tmp.path().to_string_lossy().into_owned();
    let output = foreman_with_env(
        &db,
        &[
            "run",
            "--task",
            "1",
            "--agent",
            "claude",
            "--workdir",
            &workdir,
            "--max-budget-usd",
            "0.75",
            "--no-verify",
        ],
        &[("FOREMAN_CLAUDE_BIN", &claude)],
    );
    assert!(
        !output.status.success(),
        "the fixture exits non-zero: {output:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("without a result line"),
        "{output:?}"
    );
    let captured = std::fs::read_to_string(&capture).unwrap();
    assert!(captured.contains("--max-budget-usd 0.75"), "{captured}");
    assert!(captured.lines().any(|line| line == "0.7500"), "{captured}");

    let show = foreman(&db, &["task", "show", "1"]);
    let shown: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(shown["budget_usd"], 20.0);
    assert_eq!(shown["budget_charged_usd"], 0.75);
    assert_eq!(shown["budget_remaining_usd"], 19.25);

    let status = foreman(&db, &["status"]);
    let status = String::from_utf8_lossy(&status.stdout);
    assert!(
        status.contains("task 1 budget $20.0000, charged $0.7500, remainder $19.2500"),
        "{status}"
    );
    let status = foreman(&db, &["status", "--json"]);
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["task_budgets"][0]["budget_usd"], 20.0);
    assert_eq!(status["task_budgets"][0]["budget_charged_usd"], 0.75);
    assert_eq!(status["task_budgets"][0]["budget_remaining_usd"], 19.25);
}

#[cfg(unix)]
#[test]
fn exhausted_budget_parks_with_finding_without_burning_a_dispatch_slot() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let claude = tmp.path().join("fake-claude");
    std::fs::write(
        conf_path(&db),
        "daily_budget_usd: 20\nreserve_usd: 1\nladder: [\"claude:sonnet\"]\n",
    )
    .unwrap();
    executable_script(&claude, "exit 1");
    for title in ["exhaust budget", "next task"] {
        let mut args = vec![
            "task",
            "add",
            title,
            "--spec",
            "fixture",
            "--verifier",
            "none",
        ];
        if title == "exhaust budget" {
            args.extend(["--budget", "1"]);
        }
        let added = foreman(&db, &args);
        assert!(added.status.success(), "{added:?}");
    }

    let workdir = tmp.path().to_string_lossy().into_owned();
    let first = foreman_with_env(
        &db,
        &[
            "run",
            "--task",
            "1",
            "--agent",
            "claude",
            "--workdir",
            &workdir,
            "--no-verify",
        ],
        &[("FOREMAN_CLAUDE_BIN", &claude)],
    );
    assert!(!first.status.success(), "dead-early fixture must fail");
    let exhausted = Ledger::open(&db)
        .unwrap()
        .task_budget_remainder(1)
        .unwrap()
        .unwrap();
    assert_eq!(exhausted.charged_usd, 1.0);
    assert_eq!(exhausted.remaining_usd, 0.0);

    let dispatch = foreman_with_env(
        &db,
        &[
            "dispatch",
            "--max-tasks",
            "1",
            "--workdir",
            &workdir,
            "--no-verify",
        ],
        &[("FOREMAN_CLAUDE_BIN", &claude)],
    );
    assert!(
        dispatch.status.success(),
        "budget exhaustion is not a red dispatch unit: stdout={} stderr={}",
        String::from_utf8_lossy(&dispatch.stdout),
        String::from_utf8_lossy(&dispatch.stderr)
    );
    let output = String::from_utf8_lossy(&dispatch.stdout);
    assert!(
        output.contains("sweep complete — ran 1, bounced 1, parked 1"),
        "{output}"
    );

    let ledger = Ledger::open(&db).unwrap();
    let task = ledger.task(1).unwrap().unwrap();
    assert_eq!(task.status, "parked");
    assert_eq!(task.attempt, 1, "parking must not claim another attempt");
    assert_eq!(ledger.task(2).unwrap().unwrap().attempt, 1);
    let findings = ledger.task_findings(1).unwrap();
    assert!(
        findings
            .iter()
            .any(|finding| { finding.2.contains("$0.0000 remaining, $1.0000 required") })
    );
    let reason: String = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT reason_code FROM findings WHERE task_id = 1 AND severity = 'blocker'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(reason, "task_budget_exhausted");
    drop(ledger);

    let topped_up = foreman(&db, &["task", "set", "1", "--budget", "2"]);
    assert!(topped_up.status.success(), "{topped_up:?}");
    let show = foreman(&db, &["task", "show", "1"]);
    let shown: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(shown["budget_usd"], 2.0);
    assert_eq!(shown["budget_charged_usd"], 1.0);
    assert_eq!(shown["budget_remaining_usd"], 1.0);
    assert!(foreman(&db, &["task", "requeue", "1"]).status.success());
    assert_eq!(
        Ledger::open(&db).unwrap().task(1).unwrap().unwrap().status,
        "queued"
    );
    let finding_status: String = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT status FROM findings
             WHERE task_id = 1 AND reason_code = 'task_budget_exhausted'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(finding_status, "resolved");

    let invalid = foreman(&db, &["task", "set", "1", "--budget", "0"]);
    assert!(!invalid.status.success());
    let cleared = foreman(&db, &["task", "set", "1", "--budget", "clear"]);
    assert!(cleared.status.success(), "{cleared:?}");
    let show = foreman(&db, &["task", "show", "1"]);
    let shown: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(shown["budget_usd"], serde_json::Value::Null);
    assert_eq!(shown["budget_charged_usd"], serde_json::Value::Null);
    assert_eq!(shown["budget_remaining_usd"], serde_json::Value::Null);
}

#[test]
fn one_policy_snapshot_supplies_every_claim_in_a_sweep() {
    use std::cell::Cell;
    use std::ffi::OsString;

    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let live_usd = Cell::new("2");
    let live_tokens = Cell::new("20");
    let policy = FleetPolicy::load_with(conf_path(&db), |key| match key {
        "FOREMAN_DAILY_BUDGET_USD" => Some(OsString::from("20")),
        "FOREMAN_DAILY_OUTPUT_TOKENS" => Some(OsString::from("1000")),
        "FOREMAN_RESERVE_USD" => Some(OsString::from(live_usd.get())),
        "FOREMAN_RESERVE_TOKENS" => Some(OsString::from(live_tokens.get())),
        _ => None,
    })
    .unwrap();
    let ledger = Ledger::open(&db).unwrap();
    let governor = Governor::from_policy(&db, &policy);

    governor
        .reserve(
            &ledger,
            "dispatch@first",
            None,
            &Budget::default(),
            AgentKind::Claude,
        )
        .unwrap();
    // Model a per-child environment changing between claims. The already
    // loaded sweep policy must remain authoritative for the second claim.
    live_usd.set("7");
    live_tokens.set("70");
    governor
        .reserve(
            &ledger,
            "dispatch@second",
            None,
            &Budget::default(),
            AgentKind::Claude,
        )
        .unwrap();

    let status = governor.status(&ledger).unwrap();
    assert!((status.reserved_usd - 4.0).abs() < f64::EPSILON);
    assert_eq!(status.reserved_tokens, 40);
}

#[test]
fn invalid_conf_ladder_is_a_hard_error_naming_ladder() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    std::fs::write(conf_path(&db), "ladder: [\"not-an-agent\"]\n").unwrap();
    let output = foreman(&db, &["config", "show"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("ladder"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn out_of_range_start_rung_is_a_load_error() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    std::fs::write(
        conf_path(&db),
        "ladder: [\"codex\", \"claude:fable\"]\nstart_rung: 7\n",
    )
    .unwrap();
    let output = foreman(&db, &["config", "show"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("start_rung 7"), "{stderr}");
    assert!(stderr.contains("2-rung ladder"), "{stderr}");
}
