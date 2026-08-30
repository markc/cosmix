use std::path::Path;
use std::process::Command;

use cosmix_foreman::executor::{AgentKind, RunOutcome, StopReason, Usage};
use cosmix_foreman::ledger::{
    ClaimToken, DepsError, FindingReason, GenericState, Ledger, ReviewFindingInsert,
    ReviewRunRecord, StoredRunOutcome, StoredStatus, TaskControls, TaskStatus,
};
use cosmix_foreman::refinery::{self, RefineOptions};
/// The exact pre-versioning schema a live fleet ledger carries: no
/// `user_version`, and none of the columns added after first release
/// (`verifier_profile`, `ladder_failures`, `lease_until`, `runs.result`,
/// `runs.error`, `reservations.pid`/`pid_start`).
const LEGACY_FLEET_SCHEMA: &str = "CREATE TABLE tasks (
                id INTEGER PRIMARY KEY,
                title TEXT NOT NULL,
                spec TEXT NOT NULL,
                kind TEXT NOT NULL DEFAULT 'impl',
                risk TEXT NOT NULL DEFAULT 'low',
                status TEXT NOT NULL DEFAULT 'queued',
                deps TEXT NOT NULL DEFAULT '[]',
                claimed_by TEXT,
                worktree TEXT,
                branch TEXT,
                attempt INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE runs (
                id INTEGER PRIMARY KEY,
                task_id INTEGER NOT NULL REFERENCES tasks(id),
                agent TEXT NOT NULL,
                model TEXT,
                session_ref TEXT,
                tokens_in INTEGER NOT NULL DEFAULT 0,
                tokens_out INTEGER NOT NULL DEFAULT 0,
                cost_usd REAL,
                verdict TEXT,
                duration_ms INTEGER,
                started_at TEXT NOT NULL
            );
            CREATE TABLE findings (
                id INTEGER PRIMARY KEY,
                task_id INTEGER REFERENCES tasks(id),
                severity TEXT NOT NULL DEFAULT 'info',
                title TEXT NOT NULL,
                body TEXT NOT NULL DEFAULT '',
                filed_by TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'open',
                created_at TEXT NOT NULL
            );
            CREATE TABLE events (
                id INTEGER PRIMARY KEY,
                run_id INTEGER NOT NULL REFERENCES runs(id),
                seq INTEGER NOT NULL,
                kind TEXT NOT NULL,
                payload TEXT NOT NULL,
                at TEXT NOT NULL
            );
            CREATE TABLE reservations (
                id INTEGER PRIMARY KEY,
                claimant TEXT NOT NULL,
                task_id INTEGER,
                usd REAL NOT NULL,
                tokens INTEGER NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE TABLE verifications (
                id INTEGER PRIMARY KEY,
                task_id INTEGER NOT NULL REFERENCES tasks(id),
                tier INTEGER NOT NULL DEFAULT 0,
                pass INTEGER NOT NULL,
                report TEXT NOT NULL,
                at TEXT NOT NULL
            );";

fn add_task(ledger: &Ledger, title: &str) -> i64 {
    ledger
        .add_task(title, "spec", "impl", "low", &[], "none")
        .unwrap()
}

fn user_version(path: &Path) -> i64 {
    rusqlite::Connection::open(path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn historical_acp_run_rows_remain_readable() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let task_id = add_task(&ledger, "historical ACP run");
    let (_, run_id) = ledger
        .start_attempt(task_id, "legacy-worker", None, None, "acp", None)
        .unwrap();

    let runs = ledger.recent_runs(1).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].id, run_id);
    assert_eq!(runs[0].agent, "acp");
}

#[test]
fn stale_same_name_claim_token_cannot_finish_new_generation() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let id = add_task(&ledger, "claim generations");

    let (first, _) = ledger
        .start_attempt(id, "worker", Some("/work/one"), None, "codex", None)
        .unwrap();
    assert_eq!(first.attempt, 1);
    ledger.requeue_task(id, true).unwrap();
    let (second, _) = ledger
        .start_attempt(id, "worker", Some("/work/two"), None, "codex", None)
        .unwrap();
    assert_eq!(second.attempt, 2);

    assert!(
        ledger
            .finish_task_claimed(
                id,
                ClaimToken {
                    owner: "worker",
                    generation: first.attempt,
                },
                "done",
            )
            .is_err()
    );
    let still_current = ledger.task(id).unwrap().unwrap();
    assert_eq!(still_current.status, "running");
    assert_eq!(still_current.attempt, second.attempt);

    ledger
        .finish_task_claimed(
            id,
            ClaimToken {
                owner: "worker",
                generation: second.attempt,
            },
            "done",
        )
        .unwrap();
    assert_eq!(ledger.task(id).unwrap().unwrap().status, "done");
}

#[test]
fn start_attempt_commits_claim_workspace_run_and_running_together() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let id = add_task(&ledger, "atomic start");

    let (task, run_id) = ledger
        .start_attempt(
            id,
            "worker",
            Some("/work/task-1"),
            Some("task/1"),
            "claude",
            Some("sonnet"),
        )
        .unwrap();

    assert_eq!(task.status, "running");
    assert_eq!(task.claimed_by.as_deref(), Some("worker"));
    assert_eq!(task.worktree.as_deref(), Some("/work/task-1"));
    assert_eq!(task.branch.as_deref(), Some("task/1"));
    let runs = ledger.recent_runs(10).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].id, run_id);
    assert_eq!(runs[0].task_id, id);
    assert_eq!(runs[0].role, "implement");
    assert_eq!(runs[0].agent, "claude");
    assert_eq!(runs[0].model.as_deref(), Some("sonnet"));
}

#[test]
fn infrastructure_disposition_and_backoff_roll_back_as_one_transaction() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let id = add_task(&ledger, "atomic infrastructure disposition");
    let error = anyhow::anyhow!("vendor unavailable");
    assert_eq!(
        ledger
            .note_infra_refusal(id, &error, 99, 100)
            .unwrap()
            .unwrap()
            .count,
        1
    );
    assert_eq!(
        ledger
            .note_infra_refusal(id, &error, 99, 100)
            .unwrap()
            .unwrap()
            .count,
        2
    );
    let (task, run) = ledger
        .start_attempt(id, "worker", None, None, "claude", None)
        .unwrap();
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch("DROP TABLE findings")
        .unwrap();

    assert!(
        ledger
            .finish_task_classified(
                id,
                ClaimToken {
                    owner: "worker",
                    generation: task.attempt,
                },
                run,
                "failed",
                Some(FindingReason::InfraRefusal),
            )
            .is_err()
    );

    let task = ledger.task(id).unwrap().unwrap();
    assert_eq!(task.status, "running");
    assert_eq!(task.claimed_by.as_deref(), Some("worker"));
    assert_eq!(task.infra_refusals, 2);
    assert!(!ledger.recent_runs(1).unwrap()[0].ladder_charge);
}

#[test]
fn post_run_infrastructure_parking_does_not_charge_the_ladder() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let id = add_task(&ledger, "post-run infrastructure park");
    let error = anyhow::anyhow!("persistent harness outage");
    for expected in 1..=9 {
        let disposition = ledger
            .note_infra_refusal(id, &error, 99, 100)
            .unwrap()
            .unwrap();
        assert_eq!(disposition.count, expected);
        assert!(!disposition.parked);
    }

    let (task, run) = ledger
        .start_attempt(id, "worker", None, None, "claude", None)
        .unwrap();
    let charged = ledger
        .finish_task_classified(
            id,
            ClaimToken {
                owner: "worker",
                generation: task.attempt,
            },
            run,
            "failed",
            Some(FindingReason::InfraRefusal),
        )
        .unwrap();

    assert!(!charged);
    let parked = ledger.task(id).unwrap().unwrap();
    assert_eq!(parked.status, "parked");
    assert_eq!(parked.infra_refusals, 10);
    assert_eq!(parked.ladder_failures, 0);
    assert!(!ledger.recent_runs(1).unwrap()[0].ladder_charge);
}

#[test]
fn malformed_dependency_json_fails_closed() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let id = add_task(&ledger, "corrupt deps");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute("UPDATE tasks SET deps = 'not json' WHERE id = ?1", [id])
            .unwrap();
    }
    assert!(ledger.task(id).is_err());
}

/// The exact value here tracks `ledger::SCHEMA_VERSION` (currently 18: version
/// 2 added `findings.reason_code`, version 3 added honest run outcomes, and
/// version 4 added `verifications.attempt`; version 5 added structured task
/// crate designations; version 6 added `tasks.operator_driven`; version 7
/// added the `agent_abandoned_background` run quality; version 8 added the
/// bounded `tasks.background_abandonments` counter; version 9 added the
/// project identity singleton; version 10 bound it to repository history;
/// version 11 added nullable per-task budgets; version 12 records each run's
/// dollar reservation for conservative unknown-cost accounting; version 13
/// corrects reconstructable historical Codex input folds; version 14
/// atomically attributes charges and adds reason-specific routing
/// counters/backoff; version 15 adds nullable task bump intent; version 16
/// adds finding resolution text and timestamps; version 17 releases stale
/// operator-driven flags on already-landed tasks; version 18 adds the durable
/// push-intent journal) —
/// a fresh database always lands on whatever the current build's version is.
const CURRENT_SCHEMA_VERSION: i64 = 18;

#[test]
fn fresh_database_is_current_schema_version() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("fresh.db");
    Ledger::open(&db).unwrap();
    assert_eq!(user_version(&db), CURRENT_SCHEMA_VERSION);
}

#[test]
fn v14_to_v15_adds_nullable_task_bump_without_rewriting_intent() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("v14.db");
    {
        let ledger = Ledger::open(&db).unwrap();
        add_task(&ledger, "pre-bump-intent task");
    }
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "ALTER TABLE tasks DROP COLUMN bump;
             PRAGMA user_version = 14;",
        )
        .unwrap();
    }

    let ledger = Ledger::open(&db).unwrap();
    assert_eq!(user_version(&db), CURRENT_SCHEMA_VERSION);
    let task = ledger.task(1).unwrap().unwrap();
    assert_eq!(task.bump, None);
    assert_eq!(task.effective_version_bump().unwrap().as_str(), "patch");
    assert_eq!(task.version_bump_source(), "derived");
}

#[test]
fn v15_to_v16_adds_finding_resolution_audit_fields() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("v15.db");
    {
        let ledger = Ledger::open(&db).unwrap();
        let task = add_task(&ledger, "pre-resolution finding");
        ledger
            .file_finding_reasoned(
                Some(task),
                "major",
                "historical finding",
                "preserve the original evidence",
                "fixture",
                FindingReason::Operator,
            )
            .unwrap();
    }
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "ALTER TABLE findings DROP COLUMN resolution;
             ALTER TABLE findings DROP COLUMN resolved_at;
             PRAGMA user_version = 15;",
        )
        .unwrap();
    }

    drop(Ledger::open(&db).unwrap());
    assert_eq!(user_version(&db), CURRENT_SCHEMA_VERSION);
    let conn = rusqlite::Connection::open(&db).unwrap();
    let finding: (String, String, String, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT status, title, body, resolution, resolved_at
             FROM findings WHERE task_id = 1",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(finding.0, "open");
    assert_eq!(finding.1, "historical finding");
    assert_eq!(finding.2, "preserve the original evidence");
    assert_eq!(finding.3, None);
    assert_eq!(finding.4, None);
}

#[test]
fn v16_to_v17_releases_stale_landed_operator_reservations_with_a_finding() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("v16.db");
    {
        let ledger = Ledger::open(&db).unwrap();
        add_task(&ledger, "historically landed reservation");
    }
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "UPDATE tasks
             SET status = 'landed', operator_driven = 1,
                 updated_at = '2026-08-01T02:03:04Z'
             WHERE id = 1;
             PRAGMA user_version = 16;",
        )
        .unwrap();
    }

    let ledger = Ledger::open(&db).unwrap();
    assert_eq!(user_version(&db), CURRENT_SCHEMA_VERSION);
    assert!(!ledger.task(1).unwrap().unwrap().operator_driven);
    let conn = rusqlite::Connection::open(&db).unwrap();
    let release: (String, String, String, String, String) = conn
        .query_row(
            "SELECT severity, filed_by, reason_code, status, created_at
             FROM findings WHERE task_id = 1 AND reason_code = 'operator_released'",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        release,
        (
            "info".into(),
            "schema-17-migration".into(),
            "operator_released".into(),
            "resolved".into(),
            "2026-08-01T02:03:04Z".into(),
        )
    );
}

#[test]
fn v15_to_v16_reclassifies_policy_events_and_reconciles_terminal_findings() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("v15-findings.db");
    {
        let ledger = Ledger::open(&db).unwrap();
        let landed = add_task(&ledger, "already landed");
        let bounced = add_task(&ledger, "still bounced");
        ledger.set_task_status(landed, "landed").unwrap();
        ledger.set_task_status(bounced, "bounced").unwrap();
        for task in [landed, bounced] {
            ledger
                .file_finding_reasoned(
                    Some(task),
                    "blocker",
                    "policy escalation",
                    "the policy gate refused an unresolvable shell write",
                    "policy-gate",
                    FindingReason::PolicyDenied,
                )
                .unwrap();
            ledger
                .file_finding_reasoned(
                    Some(task),
                    "blocker",
                    "distinct blocker",
                    "operator action is still required while this task is active",
                    "fixture",
                    FindingReason::Operator,
                )
                .unwrap();
        }
    }
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "ALTER TABLE findings DROP COLUMN resolution;
             ALTER TABLE findings DROP COLUMN resolved_at;
             PRAGMA user_version = 15;",
        )
        .unwrap();
    }

    drop(Ledger::open(&db).unwrap());
    let conn = rusqlite::Connection::open(&db).unwrap();
    let landed_rows: Vec<(String, String, Option<String>)> = conn
        .prepare(
            "SELECT severity, status, resolution FROM findings
             WHERE task_id = 1 ORDER BY id",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(landed_rows.len(), 2);
    assert_eq!(landed_rows[0].0, "info");
    assert!(landed_rows.iter().all(|row| row.1 == "resolved"));
    assert!(
        landed_rows
            .iter()
            .all(|row| { row.2.as_deref() == Some("task 1 landed (schema 16 reconciliation)") })
    );

    let bounced_rows: Vec<(String, String, Option<String>)> = conn
        .prepare(
            "SELECT severity, status, resolution FROM findings
             WHERE task_id = 2 ORDER BY id",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        bounced_rows,
        vec![
            ("info".into(), "open".into(), None),
            ("blocker".into(), "open".into(), None),
        ]
    );
    let open_blockers: i64 = conn
        .query_row(
            "SELECT count(*) FROM findings WHERE status = 'open' AND severity = 'blocker'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(open_blockers, 1, "only the distinct active blocker remains");
}

#[test]
fn landing_resolves_open_findings_with_audit_reason_but_bounce_does_not() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("terminal-findings.db");
    let ledger = Ledger::open(&db).unwrap();
    let landed = add_task(&ledger, "landing fixture");
    let bounced = add_task(&ledger, "bounce fixture");

    for task in [landed, bounced] {
        ledger
            .file_finding_reasoned(
                Some(task),
                "blocker",
                "fixture blocker",
                "original evidence remains intact",
                "fixture",
                FindingReason::Operator,
            )
            .unwrap();
        let (_claimed, run) = ledger
            .start_attempt(task, "worker", None, None, "claude", None)
            .unwrap();
        ledger.finish_task(task, "worker", "done").unwrap();
        assert!(ledger.transition_if(task, "done", "landing").unwrap());
        assert!(
            ledger
                .finish_landing_classified(
                    task,
                    if task == landed { "landed" } else { "bounced" },
                    Some(run),
                    (task == bounced).then_some(FindingReason::VerifierRed),
                )
                .unwrap()
                .0
        );
    }

    let conn = rusqlite::Connection::open(&db).unwrap();
    let landed_finding: (String, String, String, Option<String>) = conn
        .query_row(
            "SELECT status, body, resolution, resolved_at
             FROM findings WHERE task_id = ?1 AND title = 'fixture blocker'",
            [landed],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(landed_finding.0, "resolved");
    assert_eq!(landed_finding.1, "original evidence remains intact");
    assert_eq!(landed_finding.2, "task 1 landed");
    assert!(landed_finding.3.is_some());

    let bounced_finding: (String, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT status, resolution, resolved_at
             FROM findings WHERE task_id = ?1 AND title = 'fixture blocker'",
            [bounced],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(bounced_finding, ("open".into(), None, None));
    let open_blockers: i64 = conn
        .query_row(
            "SELECT count(*) FROM findings WHERE status = 'open' AND severity = 'blocker'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(open_blockers, 1, "only the bounced task remains actionable");
}

#[test]
fn injected_wall_time_alone_decides_backoff_admission() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("backoff-clock.db");
    let ledger = Ledger::open(&db).unwrap();
    let task = add_task(&ledger, "clocked backoff");
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE tasks SET dispatch_after = '2050-01-01T00:00:30+00:00' WHERE id = ?1",
            [task],
        )
        .unwrap();

    assert!(
        ledger
            .ready_tasks_at(None, "2050-01-01T00:00:29Z".parse().unwrap())
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        ledger
            .ready_tasks_at(None, "2050-01-01T00:00:30Z".parse().unwrap())
            .unwrap()
            .into_iter()
            .map(|task| task.id)
            .collect::<Vec<_>>(),
        [task]
    );
}

#[test]
fn infrastructure_backoff_writer_normalises_utc_to_z() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("backoff-normalised.db")).unwrap();
    let task = add_task(&ledger, "normalised backoff");
    ledger
        .note_infra_refusal(task, &anyhow::anyhow!("temporary outage"), 99, 100)
        .unwrap();
    let after = ledger.task(task).unwrap().unwrap().dispatch_after.unwrap();
    assert!(after.ends_with('Z'), "{after}");
    assert!(!after.ends_with("+00:00"), "{after}");
}

#[test]
fn v6_outcome_triggers_upgrade_for_abandoned_background_quality() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("v6.db");
    let run_id = {
        let ledger = Ledger::open(&db).unwrap();
        let task = add_task(&ledger, "background quality migration");
        ledger
            .start_run(task, AgentKind::Claude, None, None)
            .unwrap()
    };
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "DROP TRIGGER trg_runs_outcomes_ins;
             DROP TRIGGER trg_runs_outcomes_upd;
             CREATE TRIGGER trg_runs_outcomes_upd
             BEFORE UPDATE OF role, delivery, quality ON runs
             WHEN NEW.quality NOT IN ('unknown', 'branch_contract_failed')
             BEGIN
                 SELECT RAISE(ABORT, 'version-6 quality allow-list');
             END;
             PRAGMA user_version = 6;",
        )
        .unwrap();
    }

    let ledger = Ledger::open(&db).unwrap();
    ledger
        .set_run_quality(run_id, "agent_abandoned_background")
        .unwrap();
    assert_eq!(user_version(&db), CURRENT_SCHEMA_VERSION);
    assert_eq!(
        ledger.recent_runs(1).unwrap()[0].quality,
        "agent_abandoned_background"
    );
}

#[test]
fn v7_adds_bounded_background_abandonment_counter() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("v7.db");
    {
        let ledger = Ledger::open(&db).unwrap();
        add_task(&ledger, "background counter migration");
    }
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "ALTER TABLE tasks DROP COLUMN background_abandonments;
             PRAGMA user_version = 7;",
        )
        .unwrap();
    }

    let ledger = Ledger::open(&db).unwrap();
    assert_eq!(user_version(&db), CURRENT_SCHEMA_VERSION);
    assert_eq!(ledger.task(1).unwrap().unwrap().background_abandonments, 0);
}

#[test]
fn v10_to_v11_adds_nullable_task_budget_without_rewriting_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("v10.db");
    {
        let ledger = Ledger::open(&db).unwrap();
        add_task(&ledger, "budget migration");
    }
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "ALTER TABLE tasks DROP COLUMN budget_usd;
             PRAGMA user_version = 10;",
        )
        .unwrap();
    }

    let ledger = Ledger::open(&db).unwrap();
    assert_eq!(user_version(&db), CURRENT_SCHEMA_VERSION);
    assert_eq!(ledger.task(1).unwrap().unwrap().budget_usd, None);
}

#[test]
fn v11_to_v12_adds_nullable_run_reservation_without_rewriting_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("v11.db");
    let run_id = {
        let ledger = Ledger::open(&db).unwrap();
        let task = add_task(&ledger, "reservation migration");
        ledger
            .start_run(task, AgentKind::Claude, None, None)
            .unwrap()
    };
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "DROP TRIGGER trg_runs_reservation_nonneg_ins;
             DROP TRIGGER trg_runs_reservation_nonneg_upd;
             ALTER TABLE runs DROP COLUMN reserved_usd;
             PRAGMA user_version = 11;",
        )
        .unwrap();
    }

    let ledger = Ledger::open(&db).unwrap();
    assert_eq!(user_version(&db), CURRENT_SCHEMA_VERSION);
    let run = ledger
        .recent_runs(10)
        .unwrap()
        .into_iter()
        .find(|run| run.id == run_id)
        .unwrap();
    assert_eq!(run.reserved_usd, None);
}

#[test]
fn v12_to_v13_corrects_only_reconstructable_codex_input_folds() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("v12.db");
    {
        let ledger = Ledger::open(&db).unwrap();
        let task = add_task(&ledger, "codex fold migration");
        drop(ledger);

        let conn = rusqlite::Connection::open(&db).unwrap();
        for (id, agent, tokens_in, fresh, cache_read) in [
            // Old Codex fold: input=100, cached=40, stored total=140.
            (1, "codex", 140, None, Some(40)),
            // No captured component: correction cannot be proven.
            (2, "codex", 77, None, None),
            // Inconsistent component: correction would make fresh negative.
            (3, "codex", 50, None, Some(40)),
            // Other lanes never used the Codex fold.
            (4, "claude", 140, Some(60), Some(40)),
        ] {
            conn.execute(
                "INSERT INTO runs
                 (id, task_id, agent, tokens_in, fresh_input_tokens,
                  cache_read_input_tokens, started_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, '2026-08-25')",
                rusqlite::params![id, task, agent, tokens_in, fresh, cache_read],
            )
            .unwrap();
        }
        conn.pragma_update(None, "user_version", 12).unwrap();
    }

    let ledger = Ledger::open(&db).unwrap();
    let mut runs = ledger.recent_runs(10).unwrap();
    runs.sort_by_key(|run| run.id);
    assert_eq!(
        (runs[0].tokens_in, runs[0].fresh_input_tokens),
        (100, Some(60))
    );
    assert_eq!((runs[1].tokens_in, runs[1].fresh_input_tokens), (77, None));
    assert_eq!((runs[2].tokens_in, runs[2].fresh_input_tokens), (50, None));
    assert_eq!(
        (runs[3].tokens_in, runs[3].fresh_input_tokens),
        (140, Some(60))
    );
    assert_eq!(user_version(&db), CURRENT_SCHEMA_VERSION);
    drop(ledger);

    // The version stamp makes a second open an idempotence proof.
    let ledger = Ledger::open(&db).unwrap();
    let corrected = ledger
        .recent_runs(10)
        .unwrap()
        .into_iter()
        .find(|run| run.id == 1)
        .unwrap();
    assert_eq!(
        (corrected.tokens_in, corrected.fresh_input_tokens),
        (100, Some(60))
    );
}

#[test]
fn v13_to_v14_adds_attempt_classification_and_routing_columns() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("v13.db");
    {
        let ledger = Ledger::open(&db).unwrap();
        add_task(&ledger, "classification migration");
    }
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "ALTER TABLE tasks DROP COLUMN review_rejections;
             ALTER TABLE tasks DROP COLUMN branch_contract_failures;
             ALTER TABLE tasks DROP COLUMN dispatch_after;
             ALTER TABLE runs DROP COLUMN attempt;
             ALTER TABLE runs DROP COLUMN ladder_charge;
             ALTER TABLE runs DROP COLUMN ladder_charge_reason;
             PRAGMA user_version = 13;",
        )
        .unwrap();
    }

    let ledger = Ledger::open(&db).unwrap();
    assert_eq!(user_version(&db), CURRENT_SCHEMA_VERSION);
    let task = ledger.task(1).unwrap().unwrap();
    assert_eq!(task.review_rejections, 0);
    assert_eq!(task.branch_contract_failures, 0);
    assert_eq!(task.dispatch_after, None);
}

#[test]
fn review_findings_and_verdict_commit_as_one_batch() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("review.db")).unwrap();
    let task = add_task(&ledger, "atomic review evidence");
    let codex = ledger
        .start_review_run(task, AgentKind::Codex, Some("codex-review"))
        .unwrap();
    let claude = ledger
        .start_review_run(task, AgentKind::Claude, Some("opus"))
        .unwrap();
    let implementation = ledger
        .start_run(
            task,
            AgentKind::Codex,
            Some("implementer"),
            Some("implement"),
        )
        .unwrap();
    let arms = [
        ReviewRunRecord {
            run_id: codex,
            approve: false,
            delivered: true,
        },
        ReviewRunRecord {
            run_id: claude,
            approve: true,
            delivered: true,
        },
    ];
    let valid = ReviewFindingInsert {
        run_id: codex,
        severity: "major".into(),
        file: "src/lib.rs".into(),
        line: 7,
        title: "Real defect".into(),
        body: "The typed finding must roll back with the batch.".into(),
        filed_by: "merge-review:codex".into(),
    };
    let invalid = ReviewFindingInsert {
        run_id: claude,
        severity: "warning".into(),
        file: "src/lib.rs".into(),
        line: 9,
        title: "Invalid severity".into(),
        body: "This forces the second insert to fail.".into(),
        filed_by: "merge-review:claude".into(),
    };

    assert!(
        ledger
            .record_review_verification(
                task,
                Some(implementation),
                false,
                "batch evidence",
                &arms,
                &[valid.clone(), invalid],
            )
            .is_err()
    );
    assert!(ledger.task_findings_detailed(task).unwrap().is_empty());
    assert!(ledger.latest_verification(task).unwrap().is_none());
    assert!(
        ledger
            .recent_runs(3)
            .unwrap()
            .iter()
            .all(|run| run.quality == "unknown")
    );

    let (_, finding_ids) = ledger
        .record_review_verification(
            task,
            Some(implementation),
            false,
            "batch evidence",
            &arms,
            &[valid],
        )
        .unwrap();
    assert_eq!(finding_ids.len(), 1);
    assert!(ledger.latest_verification(task).unwrap().is_some());
    let runs = ledger.recent_runs(3).unwrap();
    assert!(
        runs.iter()
            .any(|run| run.id == codex && run.quality == "review_rejected")
    );
    assert!(
        runs.iter()
            .any(|run| run.id == claude && run.quality == "review_approved")
    );
    assert!(
        runs.iter()
            .any(|run| run.id == implementation && run.quality == "review_rejected")
    );
}

#[test]
fn delivered_reject_marks_implementation_quality_when_sibling_is_undelivered() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("mixed-review.db")).unwrap();
    let task = add_task(&ledger, "mixed review evidence");
    let rejecting = ledger
        .start_review_run(task, AgentKind::Claude, Some("opus"))
        .unwrap();
    let failed = ledger
        .start_review_run(task, AgentKind::Codex, Some("codex-review"))
        .unwrap();
    let implementation = ledger
        .start_run(
            task,
            AgentKind::Codex,
            Some("implementer"),
            Some("implement"),
        )
        .unwrap();

    ledger
        .record_review_verification(
            task,
            Some(implementation),
            false,
            "mixed delivery batch",
            &[
                ReviewRunRecord {
                    run_id: rejecting,
                    approve: false,
                    delivered: true,
                },
                ReviewRunRecord {
                    run_id: failed,
                    approve: false,
                    delivered: false,
                },
            ],
            &[],
        )
        .unwrap();

    let runs = ledger.recent_runs(3).unwrap();
    assert!(
        runs.iter()
            .any(|run| run.id == rejecting && run.quality == "review_rejected")
    );
    assert!(
        runs.iter()
            .any(|run| run.id == failed && run.quality == "unknown")
    );
    assert!(
        runs.iter()
            .any(|run| run.id == implementation && run.quality == "review_rejected")
    );
}

#[test]
fn task_crate_designations_are_structured_and_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let id = ledger
        .add_task_scoped(
            "policy work",
            "free-form prose",
            "impl",
            "low",
            &[],
            TaskControls {
                verifier_profile: "none",
                crates: &["cosmix-foreman".to_string()],
                operator_driven_reason: None,
            },
        )
        .unwrap();
    assert_eq!(ledger.task(id).unwrap().unwrap().crates, ["cosmix-foreman"]);
    assert!(
        ledger
            .add_task_scoped(
                "bad scope",
                "spec",
                "impl",
                "low",
                &[],
                TaskControls {
                    verifier_profile: "none",
                    crates: &["../other".to_string()],
                    operator_driven_reason: None,
                },
            )
            .is_err()
    );
}

#[test]
fn unversioned_fleet_database_is_adopted_in_place() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("fleet.db");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(LEGACY_FLEET_SCHEMA).unwrap();
        assert_eq!(user_version(&db), 0);
    }

    Ledger::open(&db).unwrap();
    assert_eq!(user_version(&db), CURRENT_SCHEMA_VERSION);
}

/// A real fleet DB already opened once by a pre-`reason_code` foreman build
/// sits at `user_version = 1`, not 0 — a fresh-adoption test alone cannot
/// catch a bug that only fires on THIS path. It did: the per-column ALTER
/// loop already adds `findings.reason_code` for any starting version, so a
/// second explicit `ALTER TABLE findings ADD COLUMN reason_code` gated on
/// `version == 1` duplicated it and errored with "duplicate column name" —
/// caught here, fixed by dropping that redundant arm. Existing rows must
/// not be backfilled with a guess: they land on the schema's own DEFAULT,
/// `'unknown'`.
#[test]
fn v1_fleet_database_upgrades_reason_code_column_without_erroring() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("fleet.db");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(LEGACY_FLEET_SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO tasks (id, title, spec, created_at, updated_at)
             VALUES (1, 't', 'spec', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO findings (task_id, severity, title, body, filed_by, created_at)
             VALUES (1, 'major', 'pre-migration finding', 'body', 'runner', '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        // LEGACY_FLEET_SCHEMA already lacks every column added after first
        // release (including `reason_code`, added later still) — pinning
        // user_version to 1 is what simulates "already adopted once", the
        // realistic starting point for a live fleet DB today.
        conn.pragma_update(None, "user_version", 1).unwrap();
    }

    let ledger = Ledger::open(&db).unwrap();
    assert_eq!(user_version(&db), CURRENT_SCHEMA_VERSION);

    let reason_code: String = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT reason_code FROM findings WHERE task_id = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(reason_code, "unknown");

    // The row itself is untouched otherwise.
    let findings = ledger.task_findings(1).unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].2, "pre-migration finding");
}

#[test]
fn outcome_migration_backfills_only_derivable_history_and_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("fleet-copy.db");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(LEGACY_FLEET_SCHEMA).unwrap();
        // Version 2 is task 37's deployed schema generation. The idempotent
        // per-column loop below also makes this deliberately sparse fixture
        // representative without duplicating the complete then-current DDL.
        conn.pragma_update(None, "user_version", 2).unwrap();
        conn.execute(
            "INSERT INTO tasks
             (id, title, spec, kind, risk, status, deps, attempt, created_at, updated_at)
             VALUES (1, 'legacy', 'spec', 'impl', 'low', 'done', '[]', 1, '2026-08-20', '2026-08-20')",
            [],
        )
        .unwrap();
        for (id, model, verdict) in [
            (1, Some("sonnet"), Some("done")),
            (2, Some("opus"), Some("budget_ceiling")),
            (3, Some("sonnet"), Some("error")),
            (4, Some("sonnet"), None),
            (5, Some("merge-review"), Some("done")),
        ] {
            conn.execute(
                "INSERT INTO runs
                 (id, task_id, agent, model, verdict, duration_ms, started_at)
                 VALUES (?1, 1, 'claude', ?2, ?3, 10, '2026-08-20')",
                rusqlite::params![id, model, verdict],
            )
            .unwrap();
        }
    }

    let ledger = Ledger::open(&db).unwrap();
    let mut runs = ledger.recent_runs(10).unwrap();
    runs.sort_by_key(|run| run.id);
    assert_eq!(runs[0].role, "implement");
    assert_eq!(runs[0].delivery, "delivered");
    assert_eq!(runs[1].delivery, "resource_exhausted");
    assert_eq!(runs[2].delivery, "unknown", "generic error is ambiguous");
    assert_eq!(runs[3].delivery, "unknown", "unfinished stays unknown");
    assert_eq!(runs[4].role, "review");
    assert_eq!(
        runs[4].model, None,
        "the destroyed real model is not guessed"
    );
    assert_eq!(runs[4].quality, "unknown");
    drop(ledger);

    // A second open is the re-run proof: in particular, the unfinished row
    // must not be inferred from defaults introduced by the first migration.
    let ledger = Ledger::open(&db).unwrap();
    let unfinished = ledger
        .recent_runs(10)
        .unwrap()
        .into_iter()
        .find(|run| run.id == 4)
        .unwrap();
    assert_eq!(unfinished.delivery, "unknown");
    assert_eq!(user_version(&db), CURRENT_SCHEMA_VERSION);
}

#[test]
fn v3_migration_adds_attempt_without_guessing_legacy_verification_generation() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("fleet.db");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(LEGACY_FLEET_SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO tasks (id, title, spec, attempt, created_at, updated_at)
             VALUES (1, 't', 'spec', 7, '2026-01-01T00:00:00Z',
                     '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO verifications (task_id, tier, pass, report, at)
             VALUES (1, 1, 0, '{\"tip\":\"old\"}', '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "ALTER TABLE verifications ADD COLUMN run_id INTEGER REFERENCES runs(id)",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 3).unwrap();
    }

    let ledger = Ledger::open(&db).unwrap();
    assert_eq!(user_version(&db), CURRENT_SCHEMA_VERSION);
    ledger
        .record_verification(1, 0, true, r#"{"pass":true}"#)
        .unwrap();

    let conn = rusqlite::Connection::open(&db).unwrap();
    let legacy_attempt: Option<i64> = conn
        .query_row("SELECT attempt FROM verifications WHERE id = 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    let new_attempt: Option<i64> = conn
        .query_row("SELECT attempt FROM verifications WHERE id = 2", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(legacy_attempt, None);
    assert_eq!(new_attempt, Some(7));
}

#[test]
fn v5_to_v6_migration_adds_operator_driven_idempotently() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("fleet.db");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(LEGACY_FLEET_SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO tasks (id, title, spec, created_at, updated_at)
             VALUES (46, 'gate edit', 'policy.rs', '2026-08-23', '2026-08-23')",
            [],
        )
        .unwrap();
        conn.execute(
            "ALTER TABLE tasks ADD COLUMN crates TEXT NOT NULL DEFAULT '[]'",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 5).unwrap();
    }

    let ledger = Ledger::open(&db).unwrap();
    assert!(!ledger.task(46).unwrap().unwrap().operator_driven);
    assert_eq!(user_version(&db), CURRENT_SCHEMA_VERSION);
    drop(ledger);

    let ledger = Ledger::open(&db).unwrap();
    assert!(!ledger.task(46).unwrap().unwrap().operator_driven);
    assert_eq!(user_version(&db), CURRENT_SCHEMA_VERSION);
}

#[test]
fn run_verification_links_both_run_and_current_attempt() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let id = add_task(&ledger, "dual verification linkage");
    let (task, run_id) = ledger
        .start_attempt(id, "worker", None, None, "codex", Some("model"))
        .unwrap();

    ledger
        .record_run_verification(id, run_id, 0, true, r#"{"pass":true}"#)
        .unwrap();

    let conn = rusqlite::Connection::open(&db).unwrap();
    let linkage: (Option<i64>, Option<i64>) = conn
        .query_row(
            "SELECT run_id, attempt FROM verifications WHERE task_id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(linkage, (Some(run_id), Some(task.attempt)));
    assert_eq!(ledger.recent_runs(1).unwrap()[0].quality, "tier_0_passed");
}

#[test]
fn future_schema_version_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("future.db");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
    }
    let err = match Ledger::open(&db) {
        Ok(_) => panic!("future schema must be refused"),
        Err(err) => err,
    };
    assert!(format!("{err:#}").contains("user_version 99"), "{err:#}");
}

#[test]
fn add_task_rejects_missing_future_and_duplicate_dependencies() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();

    let missing = ledger
        .add_task("missing", "spec", "impl", "low", &[0], "none")
        .unwrap_err();
    assert!(matches!(
        missing.downcast_ref::<DepsError>(),
        Some(DepsError::Missing(0))
    ));

    let future = ledger
        .add_task("future", "spec", "impl", "low", &[1], "none")
        .unwrap_err();
    assert!(matches!(
        future.downcast_ref::<DepsError>(),
        Some(DepsError::Future(1))
    ));

    let base = add_task(&ledger, "base");
    let duplicate = ledger
        .add_task("duplicate", "spec", "impl", "low", &[base, base], "none")
        .unwrap_err();
    assert!(matches!(
        duplicate.downcast_ref::<DepsError>(),
        Some(DepsError::Duplicate(id)) if *id == base
    ));
}

#[test]
fn overflowing_run_tokens_are_refused_and_trigger_blocks_negative_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let task_id = add_task(&ledger, "overflow");
    let run_id = ledger
        .store_run_start(task_id, "codex", Some("model"), None)
        .unwrap();
    let outcome = StoredRunOutcome {
        stop: "done".into(),
        result: None,
        error: None,
        input_tokens: u64::MAX,
        fresh_input_tokens: None,
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        output_tokens: 1,
        cost_usd: None,
        session_ref: None,
    };
    assert!(ledger.store_run_finish(run_id, &outcome, 1).is_err());
    assert_eq!(ledger.recent_runs(1).unwrap()[0].tokens_in, 0);

    let conn = rusqlite::Connection::open(&db).unwrap();
    assert!(
        conn.execute("UPDATE runs SET tokens_out = -1 WHERE id = ?1", [run_id])
            .is_err()
    );
}

#[test]
fn resume_uses_only_the_latest_resource_exhausted_implementation_run() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let task_id = add_task(&ledger, "resume");
    assert!(
        ledger
            .store_run_start(task_id + 999, "codex", None, None)
            .is_err(),
        "a missing task must not return a stale last_insert_rowid"
    );

    let exhausted = ledger
        .store_run_start(task_id, "codex", Some("model"), None)
        .unwrap();
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE runs SET delivery = 'resource_exhausted', session_ref = 'session-1'
             WHERE id = ?1",
            [exhausted],
        )
        .unwrap();
    assert_eq!(
        ledger
            .latest_resumable_session(task_id, "codex", Some("model"))
            .unwrap()
            .as_deref(),
        Some("session-1")
    );
    // A ladder climb keeps the agent and changes the model. That is a rung
    // CHANGE, and `runner.rs`'s same-rung guard refuses to resume across it;
    // this lookup, which preconfigures the driver before the runner is even
    // constructed, has to agree or the guard is bypassed.
    assert_eq!(
        ledger
            .latest_resumable_session(task_id, "codex", Some("a-bigger-model"))
            .unwrap(),
        None,
        "a model change is a rung change: it must not resume the prior model's session"
    );
    assert_eq!(
        ledger
            .latest_resumable_session(task_id, "claude", Some("model"))
            .unwrap(),
        None,
        "an agent change must not resume another lane's session"
    );

    let delivered = ledger
        .store_run_start(task_id, "codex", Some("model"), None)
        .unwrap();
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE runs SET delivery = 'delivered', session_ref = 'session-2'
             WHERE id = ?1",
            [delivered],
        )
        .unwrap();
    assert_eq!(
        ledger
            .latest_resumable_session(task_id, "codex", Some("model"))
            .unwrap(),
        None,
        "an older exhausted session must not leak into later attempts"
    );
}

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn landing_recovery_stops_at_malformed_newest_report() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    std::fs::write(repo.join("base.txt"), "base\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "base"]);
    let tip = git(&repo, &["rev-parse", "HEAD"]).trim().to_string();

    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let id = add_task(&ledger, "interrupted landing");
    ledger.set_task_status(id, "landing").unwrap();
    let older_green = serde_json::json!({
        "tip": tip,
        "report": { "pass": true }
    });
    ledger
        .record_verification(id, 1, true, &older_green.to_string())
        .unwrap();
    ledger
        .record_verification(id, 1, false, "{malformed newest row")
        .unwrap();

    let err = refinery::refine(
        &ledger,
        &RefineOptions {
            repo,
            project_root: None,
            integration: "main".into(),
            subdir: ".".into(),
            tier: 0,
            review: false,
            db,
            echo: false,
            fleet_policy: None,
            profiles: Vec::new(),
            project_pack: String::new(),
            landing_gate: None,
            lane_policy: None,
        },
    )
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("malformed verification-report JSON"),
        "{err:#}"
    );
    assert_eq!(ledger.task(id).unwrap().unwrap().status, "landing");
}

#[test]
fn landing_recovery_ignores_old_attempt_red_after_new_tipless_report() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    std::fs::write(repo.join("base.txt"), "base\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "base"]);
    let tip = git(&repo, &["rev-parse", "HEAD"]).trim().to_string();

    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let id = add_task(&ledger, "fresh landing after old rejection");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'landing', attempt = 2, ladder_failures = 1
             WHERE id = ?1",
            [id],
        )
        .unwrap();
        let old_red = serde_json::json!({ "tip": tip, "report": { "pass": false } });
        conn.execute(
            "INSERT INTO verifications (task_id, attempt, tier, pass, report, at)
             VALUES (?1, 1, 1, 0, ?2, '2026-08-20T00:00:00Z')",
            rusqlite::params![id, old_red.to_string()],
        )
        .unwrap();
    }
    // This is the current attempt's tier-0 row. It has no tip because no
    // landing step ran before the crash.
    ledger
        .record_verification(id, 0, true, r#"{"pass":true}"#)
        .unwrap();

    let reports = refinery::refine(
        &ledger,
        &RefineOptions {
            repo,
            project_root: None,
            integration: "main".into(),
            subdir: ".".into(),
            tier: 0,
            review: false,
            db,
            echo: false,
            fleet_policy: None,
            profiles: Vec::new(),
            project_pack: String::new(),
            landing_gate: None,
            lane_policy: None,
        },
    )
    .unwrap();
    assert!(reports.is_empty());
    let task = ledger.task(id).unwrap().unwrap();
    // Mutation pin: removing the attempt fence makes recovery skip the
    // tip-less current row and select the prior attempt's red tip.
    assert_eq!(task.status, "done");
    assert_eq!(task.ladder_failures, 1);
}

#[test]
fn landing_recovery_keeps_current_attempt_red_as_a_bounce() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    std::fs::write(repo.join("base.txt"), "base\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "base"]);
    let tip = git(&repo, &["rev-parse", "HEAD"]).trim().to_string();

    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let id = add_task(&ledger, "current landing rejection");
    let (first, first_run) = ledger
        .start_attempt(id, "agent", None, None, "claude", None)
        .unwrap();
    ledger
        .finish_task_classified(
            id,
            ClaimToken {
                owner: "agent",
                generation: first.attempt,
            },
            first_run,
            "bounced",
            Some(cosmix_foreman::ledger::FindingReason::VerifierRed),
        )
        .unwrap();
    let (second, second_run) = ledger
        .start_attempt(id, "agent", None, None, "claude", None)
        .unwrap();
    ledger.finish_task(id, "agent", "done").unwrap();
    ledger.set_task_status(id, "landing").unwrap();
    let red = serde_json::json!({ "tip": tip, "report": { "pass": false } });
    ledger
        .record_run_verification(id, second_run, 1, false, &red.to_string())
        .unwrap();

    refinery::refine(
        &ledger,
        &RefineOptions {
            repo,
            project_root: None,
            integration: "main".into(),
            subdir: ".".into(),
            tier: 0,
            review: false,
            db,
            echo: false,
            fleet_policy: None,
            profiles: Vec::new(),
            project_pack: String::new(),
            landing_gate: None,
            lane_policy: None,
        },
    )
    .unwrap();
    let task = ledger.task(id).unwrap().unwrap();
    assert_eq!(task.status, "bounced");
    assert_eq!(task.attempt, second.attempt);
    assert_eq!(task.ladder_failures, 2);
    let charges = ledger.task_attempt_charges(id).unwrap();
    assert!(charges.iter().any(|charge| {
        charge.run_id == second_run
            && charge.charged
            && charge.reason.as_deref() == Some("verifier_red")
    }));
    assert!(
        ledger
            .open_findings(10)
            .unwrap()
            .iter()
            .any(|finding| finding.3 == "refinery recovered a recorded red landing"),
        "crash recovery must leave the next attempt a durable red-verdict handoff"
    );
}

#[test]
fn migrated_v13_null_attempt_run_is_charged_during_red_landing_recovery() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    std::fs::write(repo.join("base.txt"), "base\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "base"]);
    let tip = git(&repo, &["rev-parse", "HEAD"]).trim().to_string();

    let db = tmp.path().join("ledger.db");
    let run_id;
    {
        let ledger = Ledger::open(&db).unwrap();
        let id = add_task(&ledger, "v13 interrupted landing");
        let (_task, run) = ledger
            .start_attempt(id, "agent", None, None, "claude", None)
            .unwrap();
        run_id = run;
        ledger.finish_task(id, "agent", "done").unwrap();
        ledger.set_task_status(id, "landing").unwrap();
        let red = serde_json::json!({ "tip": tip, "report": { "pass": false } });
        ledger
            .record_run_verification(id, run_id, 1, false, &red.to_string())
            .unwrap();
    }
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "ALTER TABLE tasks DROP COLUMN review_rejections;
             ALTER TABLE tasks DROP COLUMN branch_contract_failures;
             ALTER TABLE tasks DROP COLUMN dispatch_after;
             ALTER TABLE runs DROP COLUMN attempt;
             ALTER TABLE runs DROP COLUMN ladder_charge;
             ALTER TABLE runs DROP COLUMN ladder_charge_reason;
             PRAGMA user_version = 13;",
        )
        .unwrap();
    }
    let ledger = Ledger::open(&db).unwrap();
    refinery::refine(
        &ledger,
        &RefineOptions {
            repo,
            project_root: None,
            integration: "main".into(),
            subdir: ".".into(),
            tier: 0,
            review: false,
            db,
            echo: false,
            fleet_policy: None,
            profiles: Vec::new(),
            project_pack: String::new(),
            landing_gate: None,
            lane_policy: None,
        },
    )
    .unwrap();
    let task = ledger.task(1).unwrap().unwrap();
    assert_eq!(task.status, "bounced");
    assert_eq!(task.ladder_failures, 1);
    let charge = ledger
        .recent_runs(10)
        .unwrap()
        .into_iter()
        .find(|run| run.id == run_id)
        .unwrap();
    assert!(charge.ladder_charge);
    assert_eq!(charge.ladder_charge_reason.as_deref(), Some("verifier_red"));
}

#[test]
fn migrated_null_attempt_run_is_not_charged_across_mcp_bounce_and_operator_land() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    std::fs::write(repo.join("base.txt"), "base\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "base"]);
    git(&repo, &["branch", "task/migrated"]);
    let db = tmp.path().join("ledger.db");
    let run_id;
    {
        let ledger = Ledger::open(&db).unwrap();
        let id = add_task(&ledger, "migrated then MCP bounced");
        let (task, run) = ledger
            .start_attempt(id, "legacy-agent", None, None, "claude", None)
            .unwrap();
        run_id = run;
        ledger
            .set_task_workspace(
                id,
                ClaimToken {
                    owner: "legacy-agent",
                    generation: task.attempt,
                },
                None,
                Some("task/migrated"),
            )
            .unwrap();
        ledger.finish_task(id, "legacy-agent", "done").unwrap();
    }
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "ALTER TABLE tasks DROP COLUMN review_rejections;
             ALTER TABLE tasks DROP COLUMN branch_contract_failures;
             ALTER TABLE tasks DROP COLUMN dispatch_after;
             ALTER TABLE runs DROP COLUMN attempt;
             ALTER TABLE runs DROP COLUMN ladder_charge;
             ALTER TABLE runs DROP COLUMN ladder_charge_reason;
             PRAGMA user_version = 13;",
        )
        .unwrap();
    }

    let ledger = Ledger::open(&db).unwrap();
    ledger.requeue_task(1, false).unwrap();
    let claimed = ledger.claim_task(1, "mcp-agent").unwrap();
    assert_eq!(claimed.attempt, 2);
    ledger
        .set_task_workspace(
            1,
            ClaimToken {
                owner: "mcp-agent",
                generation: claimed.attempt,
            },
            None,
            Some("task/migrated"),
        )
        .unwrap();
    assert!(
        !ledger
            .finish_agent_bounce(1, "mcp-agent", claimed.attempt, "retry landing", 3)
            .unwrap()
    );
    ledger.land_task(1, &repo).unwrap();
    assert!(ledger.transition_if(1, "done", "landing").unwrap());
    assert_eq!(
        ledger.latest_implementation_run(1).unwrap(),
        None,
        "an MCP self-bounce plus operator land must not inherit the migrated run"
    );
    let (moved, charged) = ledger
        .finish_landing_classified(1, "bounced", Some(run_id), Some(FindingReason::VerifierRed))
        .unwrap();
    assert!(moved);
    assert!(!charged);
    assert_eq!(ledger.task(1).unwrap().unwrap().ladder_failures, 0);
    let legacy = ledger
        .recent_runs(10)
        .unwrap()
        .into_iter()
        .find(|run| run.id == run_id)
        .unwrap();
    assert!(!legacy.ladder_charge);
    assert_eq!(legacy.ladder_charge_reason, None);
}

/// Item 13: every status string this column has ever stored decomposes into
/// generic state + extension and reassembles to the SAME bytes — that byte
/// identity is what lets the decomposition land before extraction without
/// rewriting a single fleet row.
#[test]
fn legacy_statuses_round_trip_byte_exactly() {
    for legacy in [
        "queued", "claimed", "running", "done", "bounced", "failed", "parked", "landing", "landed",
        "retired",
    ] {
        let stored = StoredStatus::from_db_str(legacy).expect("legacy status decodes");
        assert_eq!(stored.as_db_str(), legacy, "{legacy} must round-trip");
        let typed: TaskStatus = legacy.parse().unwrap();
        assert_eq!(typed.stored(), stored);
        assert_eq!(typed.as_db_str(), legacy);
    }
    assert_eq!(
        TaskStatus::Bounced.stored().state,
        GenericState::Ready,
        "a bounced task is retry fuel, not a terminal state"
    );
    assert_eq!(TaskStatus::Landing.stored().state, GenericState::Running);
    assert_eq!(TaskStatus::Landed.stored().state, GenericState::Done);
    assert_eq!(TaskStatus::Landed.stored().extension, Some("landed"));
    assert_eq!(TaskStatus::Retired.stored().state, GenericState::Blocked);
    assert_eq!(TaskStatus::Retired.stored().extension, Some("retired"));
    assert_eq!(TaskStatus::Queued.stored().extension, None);
    // A status nobody writes is corruption, not a new state.
    assert!(StoredStatus::from_db_str("half-landed").is_err());
}

/// Item 8: the workspace pointer is what the refinery LANDS, so a stale
/// attempt must not be able to rewrite it — same generation guard as a
/// terminal transition, proven through force-requeue + same-name reclaim.
#[test]
fn stale_generation_cannot_rewrite_workspace_or_mark_running() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let id = add_task(&ledger, "claim-scoped writes");

    let first = ledger.claim_task(id, "worker").unwrap();
    ledger.requeue_task(id, true).unwrap();
    let second = ledger.claim_task(id, "worker").unwrap();
    assert!(second.attempt > first.attempt);

    let stale = ClaimToken {
        owner: "worker",
        generation: first.attempt,
    };
    assert!(
        ledger
            .set_task_workspace(id, stale, Some("/old/work"), Some("task/old"))
            .is_err(),
        "a dead attempt must not point the refinery at its own branch"
    );
    assert!(ledger.mark_running(id, stale).is_err());
    let row = ledger.task(id).unwrap().unwrap();
    assert_eq!(row.branch, None, "the stale write must not have landed");
    assert_eq!(row.worktree, None);
    assert_eq!(row.status, "claimed");

    let live = ClaimToken {
        owner: "worker",
        generation: second.attempt,
    };
    ledger
        .set_task_workspace(id, live, Some("/new/work"), Some("task/new"))
        .unwrap();
    ledger.mark_running(id, live).unwrap();
    let row = ledger.task(id).unwrap().unwrap();
    assert_eq!(row.branch.as_deref(), Some("task/new"));
    assert_eq!(row.status, "running");
}

#[test]
fn stale_generation_cannot_file_sccache_bypass_findings() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let id = add_task(&ledger, "claim-scoped sccache finding");

    let first = ledger.claim_task(id, "worker").unwrap();
    ledger.requeue_task(id, true).unwrap();
    let second = ledger.claim_task(id, "worker").unwrap();
    let bodies = vec!["observed incident".to_string()];

    let stale = ledger.file_sccache_bypass_findings_claimed(
        id,
        ClaimToken {
            owner: "worker",
            generation: first.attempt,
        },
        &bodies,
        "runner",
    );
    assert!(stale.is_err(), "a dead attempt must not file its incident");
    assert!(ledger.task_findings(id).unwrap().is_empty());

    ledger
        .file_sccache_bypass_findings_claimed(
            id,
            ClaimToken {
                owner: "worker",
                generation: second.attempt,
            },
            &bodies,
            "runner",
        )
        .unwrap();
    let findings = ledger.task_findings(id).unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].1, "info");
    assert_eq!(findings[0].2, "sccache bypassed during verifier step");
}

/// Item 9: a cycle cannot form through ordinary adds (a dep must already
/// exist), so the cyclic case is tested against a HOSTILE existing graph —
/// hand-edited rows, exactly the shape a corrupted or externally-written
/// ledger has. The add must refuse loudly, never silently.
#[test]
fn add_task_rejects_dependencies_into_a_cyclic_graph() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let one = add_task(&ledger, "one");
    let two = add_task(&ledger, "two");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "UPDATE tasks SET deps = ?1 WHERE id = ?2",
            (format!("[{two}]"), one),
        )
        .unwrap();
        conn.execute(
            "UPDATE tasks SET deps = ?1 WHERE id = ?2",
            (format!("[{one}]"), two),
        )
        .unwrap();
    }
    let err = ledger
        .add_task("into the cycle", "spec", "impl", "low", &[one], "none")
        .unwrap_err();
    assert!(
        err.downcast_ref::<DepsError>()
            .is_some_and(|e| matches!(e, DepsError::Cyclic(_))),
        "expected a typed cyclic-dependency error, got {err:#}"
    );
}

/// Acceptance: "the live fleet DB opens in place". The adoption test above
/// proves an EMPTY legacy schema is versioned in place; this one proves the
/// rows a real fleet ledger holds survive it — legacy statuses that the
/// generic-state split must still read (item 13's compatibility read path),
/// and a pre-existing deps array that the now fail-closed decoder must
/// still accept (item 5). Together with `future_schema_version_is_refused`
/// these are the testable proxy for the live DB, which no test may touch.
#[test]
fn legacy_fleet_rows_survive_schema_adoption() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("fleet.db");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(LEGACY_FLEET_SCHEMA).unwrap();
        // One row per legacy status string, plus a real deps array on the
        // last so the fail-closed decoder is exercised against stored JSON
        // this schema wrote before the decoder existed.
        for (i, status) in [
            "queued", "claimed", "running", "done", "bounced", "failed", "parked", "landing",
            "landed",
        ]
        .iter()
        .enumerate()
        {
            let id = i as i64 + 1;
            let deps = if *status == "landed" { "[1,2]" } else { "[]" };
            conn.execute(
                "INSERT INTO tasks (id, title, spec, kind, risk, status, deps,
                                    created_at, updated_at)
                 VALUES (?1, ?2, 'spec', 'impl', 'low', ?3, ?4, '2026-01-01T00:00:00Z',
                         '2026-01-01T00:00:00Z')",
                rusqlite::params![id, format!("legacy {status}"), status, deps],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO runs (task_id, agent, model, tokens_in, tokens_out, cost_usd,
                               verdict, duration_ms, started_at)
             VALUES (4, 'claude', 'sonnet', 6000000, 1234, 2.5, 'done', 1,
                     '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        assert_eq!(user_version(&db), 0);
    }

    let ledger = Ledger::open(&db).unwrap();
    assert_eq!(user_version(&db), CURRENT_SCHEMA_VERSION);

    // Every row still reads, with its status byte-identical: adoption must
    // not rewrite a single stored status.
    let tasks = ledger.tasks(None, false).unwrap();
    assert_eq!(tasks.len(), 9, "every legacy row must still be readable");
    for t in &tasks {
        assert_eq!(
            t.title,
            format!("legacy {}", t.status),
            "status must survive adoption byte-exactly"
        );
        // The compatibility read path must give every stored status meaning.
        let stored = StoredStatus::from_db_str(&t.status)
            .unwrap_or_else(|e| panic!("legacy status {:?} must decode: {e}", t.status));
        assert_eq!(stored.as_db_str(), t.status);
    }
    let landed = tasks.iter().find(|t| t.status == "landed").unwrap();
    assert_eq!(
        landed.deps,
        vec![1, 2],
        "a pre-existing deps array must still decode"
    );

    // The back-filled columns arrived with their defaults, not by dropping
    // and recreating the table (which would have taken the rows with it).
    assert_eq!(landed.verifier_profile, "rust");
    assert!(!landed.operator_driven);
    let runs = ledger.recent_runs(10).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].role, "implement");
    assert_eq!(
        ledger.implementing_agent_for_task(4).unwrap().as_deref(),
        Some("claude"),
        "a legacy successful run remains implementer-family evidence"
    );

    // The folded tokens_in written before the breakdown columns existed
    // survives untouched; the new columns arrive NULL (unknown), not 0 --
    // this schema adoption never backfills a breakdown it never observed.
    assert_eq!(runs[0].tokens_in, 6_000_000);
    assert_eq!(runs[0].tokens_out, 1234);
    assert_eq!(runs[0].cost_usd, Some(2.5));
    assert_eq!(runs[0].fresh_input_tokens, None);
    assert_eq!(runs[0].cache_read_input_tokens, None);
    assert_eq!(runs[0].cache_creation_input_tokens, None);
}

#[test]
fn delivery_void_fraction_counts_unknown_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();

    // Empty ledger: zero void fraction
    let void = ledger.delivery_void_fraction().unwrap();
    assert_eq!(void.contributing_runs, 0);
    assert_eq!(void.unknown_runs, 0);
    assert_eq!(void.fraction, 0.0);

    // Add a run with known delivery
    let task = ledger
        .add_task("t", "spec", "impl", "low", &[], "none")
        .unwrap();
    let run = ledger
        .start_run(task, AgentKind::Claude, None, None)
        .unwrap();
    ledger
        .finish_run(
            run,
            &RunOutcome {
                stop: StopReason::Done,
                result: None,
                error: None,
                usage: Usage {
                    input_tokens: 100,
                    fresh_input_tokens: None,
                    output_tokens: 50,
                    cost_usd: Some(0.1),
                    cache_read_input_tokens: None,
                    cache_creation_input_tokens: None,
                },
                session_ref: None,
                terminal_session_ref: None,
                usage_observed: true,
                output_observed: true,
                resume_failure: None,
            },
            1000,
        )
        .unwrap();

    // One run with known delivery: zero void fraction
    let void = ledger.delivery_void_fraction().unwrap();
    assert_eq!(void.contributing_runs, 1);
    assert_eq!(void.unknown_runs, 0);
    assert_eq!(void.fraction, 0.0);

    // Start a run but never finish it, exactly as a hard kill leaves it.
    let _run2 = ledger
        .start_run(task, AgentKind::Codex, None, None)
        .unwrap();

    // Two runs, one unknown: 50% void fraction
    let void = ledger.delivery_void_fraction().unwrap();
    assert_eq!(void.contributing_runs, 2);
    assert_eq!(void.unknown_runs, 1);
    assert!(
        (void.fraction - 0.5).abs() < 0.001,
        "void fraction should be 0.5"
    );
}

// Task 65a: Retire functionality tests

#[test]
fn retire_task_succeeds_for_queued_task() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let id = add_task(&ledger, "queued task");

    ledger.retire_task(id, "no longer needed").unwrap();
    let task = ledger.task(id).unwrap().unwrap();
    assert_eq!(task.status, "retired");

    assert!(ledger.task_findings(id).unwrap().is_empty());
    let audit: (String, String, Option<String>) = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT status, title, resolution FROM findings WHERE task_id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(audit.0, "resolved");
    assert_eq!(audit.1, "task 1 retired");
    assert_eq!(audit.2.as_deref(), Some("task 1 retired: no longer needed"));
}

#[test]
fn retire_task_refuses_running_task() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let id = add_task(&ledger, "running task");

    ledger
        .start_attempt(id, "worker", Some("/work/task-1"), None, "claude", None)
        .unwrap();

    let result = ledger.retire_task(id, "try to retire running");
    assert!(result.is_err());
    assert!(
        result
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("cannot retire task")
    );
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("while it is running")
    );

    let task = ledger.task(id).unwrap().unwrap();
    assert_eq!(task.status, "running");
}

#[test]
fn retire_task_refuses_claimed_task() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let id = add_task(&ledger, "claimed task");
    ledger.claim_task(id, "worker").unwrap();

    let error = ledger
        .retire_task(id, "claim is still live")
        .unwrap_err()
        .to_string();
    assert!(error.contains("claimed by worker (claimed)"), "{error}");

    let task = ledger.task(id).unwrap().unwrap();
    assert_eq!(task.status, "claimed");
    assert_eq!(task.claimed_by.as_deref(), Some("worker"));
    assert!(ledger.task_findings(id).unwrap().is_empty());
}

#[test]
fn claimant_cannot_finish_a_retired_task() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let id = add_task(&ledger, "terminal fence");
    let claimed = ledger.claim_task(id, "worker").unwrap();

    rusqlite::Connection::open(&db)
        .unwrap()
        .execute("UPDATE tasks SET status = 'retired' WHERE id = ?1", [id])
        .unwrap();

    assert!(ledger.finish_task(id, "worker", "done").is_err());
    assert!(
        ledger
            .finish_task_claimed(
                id,
                ClaimToken {
                    owner: "worker",
                    generation: claimed.attempt,
                },
                "done",
            )
            .is_err()
    );
    let task = ledger.task(id).unwrap().unwrap();
    assert_eq!(task.status, "retired");
    assert_eq!(task.claimed_by.as_deref(), Some("worker"));
}

#[test]
fn retire_task_refuses_landing_task() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let id = add_task(&ledger, "landing task");

    // Move task through proper lifecycle: queued -> running -> done -> landing
    // Start a run to get to running status
    ledger
        .start_attempt(id, "worker", Some("/work/task-1"), None, "claude", None)
        .unwrap();
    // Manually complete the run and unclaim to move to done status
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'done', claimed_by = NULL WHERE id = ?1",
            [id],
        )
        .unwrap();
    }
    // Now transition from done to landing (requires unclaimed task)
    ledger
        .transition(id, TaskStatus::Done, TaskStatus::Landing)
        .unwrap();

    let result = ledger.retire_task(id, "try to retire landing");
    assert!(result.is_err());
    assert!(
        result
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("cannot retire task")
    );
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("while it is landing")
    );

    let task = ledger.task(id).unwrap().unwrap();
    assert_eq!(task.status, "landing");
}

#[test]
fn retire_task_creates_finding_with_reason() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let id = add_task(&ledger, "task with reason");
    let reason = "deprecated feature replaced by better approach";

    ledger.retire_task(id, reason).unwrap();

    assert!(ledger.task_findings(id).unwrap().is_empty());
    let finding: (String, String, String, Option<String>) = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT status, title, body, resolution FROM findings WHERE task_id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(finding.0, "resolved");
    assert_eq!(finding.1, "task 1 retired");
    assert_eq!(finding.2, reason);
    assert_eq!(
        finding.3.as_deref(),
        Some("task 1 retired: deprecated feature replaced by better approach")
    );
}

#[test]
fn retired_task_excluded_from_default_list() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let id1 = add_task(&ledger, "active task");
    let id2 = add_task(&ledger, "retired task");

    ledger.retire_task(id2, "completed").unwrap();

    let tasks = ledger.tasks(None, false).unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].id, id1);
}

#[test]
fn retired_task_included_in_list_with_all_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let id1 = add_task(&ledger, "active task");
    let id2 = add_task(&ledger, "retired task");

    ledger.retire_task(id2, "completed").unwrap();

    let tasks = ledger.tasks(None, true).unwrap();
    assert_eq!(tasks.len(), 2);
    let ids: Vec<_> = tasks.iter().map(|t| t.id).collect();
    assert!(ids.contains(&id1));
    assert!(ids.contains(&id2));
}

#[test]
fn task_list_skips_unknown_status_by_default_and_all_preserves_it() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let visible = add_task(&ledger, "visible task");
    let unknown = add_task(&ledger, "unknown task");
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'deprecated' WHERE id = ?1",
            [unknown],
        )
        .unwrap();

    let listed = ledger.tasks(None, false).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, visible);

    let all = ledger.tasks(None, true).unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[1].id, unknown);
    assert_eq!(all[1].status, "deprecated");
}

#[test]
fn task_retire_cli_requires_long_reason() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let id = add_task(&ledger, "cli retirement");
    drop(ledger);

    let positional = Command::new(env!("CARGO_BIN_EXE_foreman"))
        .arg("--db")
        .arg(&db)
        .args(["task", "retire", "1", "obsolete"])
        .env("FOREMAN_VERIFY_LANE", tmp.path().join("verify.lock"))
        .env("FOREMAN_VERIFY_LANE_WAIT_SECS", "30")
        .output()
        .unwrap();
    assert!(!positional.status.success());

    let flagged = Command::new(env!("CARGO_BIN_EXE_foreman"))
        .arg("--db")
        .arg(&db)
        .args(["task", "retire", "1", "--reason", "obsolete"])
        .env("FOREMAN_VERIFY_LANE", tmp.path().join("verify.lock"))
        .env("FOREMAN_VERIFY_LANE_WAIT_SECS", "30")
        .output()
        .unwrap();
    assert!(
        flagged.status.success(),
        "{}",
        String::from_utf8_lossy(&flagged.stderr)
    );
    assert_eq!(
        Ledger::open(&db).unwrap().task(id).unwrap().unwrap().status,
        "retired"
    );
}

#[test]
fn task_add_budget_round_trips_through_task_show() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let add = Command::new(env!("CARGO_BIN_EXE_foreman"))
        .arg("--db")
        .arg(&db)
        .args([
            "task", "add", "budgeted", "--spec", "spec", "--budget", "7.25",
        ])
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "{}",
        String::from_utf8_lossy(&add.stderr)
    );

    let show = Command::new(env!("CARGO_BIN_EXE_foreman"))
        .arg("--db")
        .arg(&db)
        .args(["task", "show", "1"])
        .output()
        .unwrap();
    assert!(
        show.status.success(),
        "{}",
        String::from_utf8_lossy(&show.stderr)
    );
    let task: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(task["budget_usd"], 7.25);
}

#[test]
fn task_bump_add_set_and_show_report_explicit_and_derived_intent() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");

    let explicit = Command::new(env!("CARGO_BIN_EXE_foreman"))
        .arg("--db")
        .arg(&db)
        .args([
            "task", "add", "explicit", "--spec", "spec", "--risk", "low", "--kind", "impl",
            "--bump", "minor",
        ])
        .output()
        .unwrap();
    assert!(
        explicit.status.success(),
        "{}",
        String::from_utf8_lossy(&explicit.stderr)
    );

    let derived = Command::new(env!("CARGO_BIN_EXE_foreman"))
        .arg("--db")
        .arg(&db)
        .args([
            "task", "add", "derived", "--spec", "spec", "--risk", "high", "--kind", "impl",
        ])
        .output()
        .unwrap();
    assert!(derived.status.success());

    let show = |id: &str| {
        let output = Command::new(env!("CARGO_BIN_EXE_foreman"))
            .arg("--db")
            .arg(&db)
            .args(["task", "show", id])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };

    let first = show("1");
    assert_eq!(first["bump"], "minor");
    assert_eq!(first["effective_bump"], "minor");
    assert_eq!(first["bump_source"], "explicit");

    let second = show("2");
    assert_eq!(second["bump"], serde_json::Value::Null);
    assert_eq!(second["effective_bump"], "minor");
    assert_eq!(second["bump_source"], "derived");

    let set = Command::new(env!("CARGO_BIN_EXE_foreman"))
        .arg("--db")
        .arg(&db)
        .args(["task", "set", "2", "--bump", "patch"])
        .output()
        .unwrap();
    assert!(
        set.status.success(),
        "{}",
        String::from_utf8_lossy(&set.stderr)
    );
    let corrected = show("2");
    assert_eq!(corrected["bump"], "patch");
    assert_eq!(corrected["effective_bump"], "patch");
    assert_eq!(corrected["bump_source"], "explicit");
}

// Task 65b: Unknown status handling tests

#[test]
fn ready_tasks_skips_unknown_status_with_finding() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let id = add_task(&ledger, "corrupt status task");

    // Inject an unknown status directly into the database
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'future_status' WHERE id = ?1",
            [id],
        )
        .unwrap();
    }

    let tasks = ledger.ready_tasks(None).unwrap();
    assert_eq!(tasks.len(), 0);

    let findings = ledger.task_findings(id).unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].1, "warn");
    assert!(findings[0].2.contains("unknown status"));
    assert!(findings[0].3.contains("future_status"));

    let reason: String = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT reason_code FROM findings WHERE task_id = ?1",
            [id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(reason, "unknown_status");
}

#[test]
fn unknown_status_finding_not_duplicated_on_second_call() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let id = add_task(&ledger, "duplicate test task");

    // Inject an unknown status directly into the database
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'another_unknown' WHERE id = ?1",
            [id],
        )
        .unwrap();
    }

    // First call creates the finding
    let tasks1 = ledger.ready_tasks(None).unwrap();
    assert_eq!(tasks1.len(), 0);
    let findings1 = ledger.task_findings(id).unwrap();
    assert_eq!(findings1.len(), 1);

    // Second call should not create a duplicate finding
    let tasks2 = ledger.ready_tasks(None).unwrap();
    assert_eq!(tasks2.len(), 0);
    let findings2 = ledger.task_findings(id).unwrap();
    assert_eq!(findings2.len(), 1); // Still only one finding
}

#[test]
fn resolved_unknown_status_finding_does_not_silence_the_row() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let id = add_task(&ledger, "resolved finding task");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE tasks SET status = 'still_unknown' WHERE id = ?1",
        [id],
    )
    .unwrap();

    assert!(ledger.ready_tasks(None).unwrap().is_empty());
    conn.execute(
        "UPDATE findings SET status = 'resolved' WHERE task_id = ?1",
        [id],
    )
    .unwrap();
    assert!(ledger.ready_tasks(None).unwrap().is_empty());

    let (all_findings, open_findings): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), SUM(status = 'open') FROM findings
             WHERE task_id = ?1 AND reason_code = 'unknown_status'",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(all_findings, 2);
    assert_eq!(open_findings, 1);
}

#[test]
fn operator_driven_tasks_handles_unknown_status() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let id = add_task(&ledger, "operator unknown status");

    // Inject an unknown status directly into the database
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'bogus_status' WHERE id = ?1",
            [id],
        )
        .unwrap();
    }

    let tasks = ledger.operator_driven_tasks(None).unwrap();
    assert_eq!(tasks.len(), 0);

    let findings = ledger.task_findings(id).unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].1, "warn");
    assert!(findings[0].2.contains("unknown status"));
}
