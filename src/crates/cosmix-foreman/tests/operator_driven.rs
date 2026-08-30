use std::path::Path;
use std::process::{Command, Output};

use cosmix_foreman::ledger::{Ledger, TaskControls};

mod support;

fn foreman(db: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_foreman"))
        .arg("--db")
        .arg(db)
        .args(args)
        .env(
            "FOREMAN_VERIFY_LANE",
            db.parent().unwrap().join("verify.lock"),
        )
        .env("FOREMAN_VERIFY_LANE_WAIT_SECS", "30")
        .output()
        .expect("foreman command")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn operator_driven_cli_skips_dispatch_runs_explicitly_and_can_be_cleared() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let workdir = tmp.path().join("workdir");
    std::fs::create_dir(&workdir).unwrap();

    let added = foreman(
        &db,
        &[
            "task",
            "add",
            "gate edit",
            "--spec",
            "edit cosmix-foreman/src/policy.rs",
            "--verifier",
            "none",
            "--operator-driven",
            "--reason",
            "foreman gate requires an explicit operator run",
        ],
    );
    assert!(
        added.status.success(),
        "task add failed: {}",
        stderr(&added)
    );

    let list = foreman(&db, &["task", "list"]);
    assert!(stdout(&list).contains("[operator-driven]"));
    let show = foreman(&db, &["task", "show", "1"]);
    let shown: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(shown["operator_driven"], true);
    let reservation: (i64, String, String, String) = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT task_id, body, filed_by, reason_code FROM findings
             WHERE task_id = 1 AND reason_code = 'operator_reserved'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        reservation,
        (
            1,
            "foreman gate requires an explicit operator run".into(),
            "operator".into(),
            "operator_reserved".into(),
        )
    );

    let pinned = foreman(
        &db,
        &[
            "dispatch",
            "--task",
            "1",
            "--dry-run",
            "--workdir",
            workdir.to_str().unwrap(),
        ],
    );
    assert!(!pinned.status.success());
    assert!(
        stderr(&pinned).contains("task 1 not ready: operator-driven"),
        "dispatch refusal was not explicit: {}",
        stderr(&pinned)
    );

    let queue = foreman(
        &db,
        &[
            "dispatch",
            "--dry-run",
            "--workdir",
            workdir.to_str().unwrap(),
        ],
    );
    assert!(queue.status.success(), "queue summary: {}", stderr(&queue));
    assert!(
        stdout(&queue).contains("operator-driven: 1"),
        "queue summary must name reserved tasks: {}",
        stdout(&queue)
    );

    let requeued = foreman(&db, &["task", "requeue", "1"]);
    assert!(requeued.status.success(), "requeue: {}", stderr(&requeued));
    assert!(
        Ledger::open(&db)
            .unwrap()
            .task(1)
            .unwrap()
            .unwrap()
            .operator_driven
    );

    let fake_codex = tmp.path().join("fake-codex");
    support::write_executable(
        &fake_codex,
        "#!/bin/sh\n\
         echo '{\"type\":\"thread.started\",\"thread_id\":\"operator-run\"}'\n\
         echo '{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}'\n",
    );
    let run = Command::new(env!("CARGO_BIN_EXE_foreman"))
        .arg("--db")
        .arg(&db)
        .args([
            "run",
            "--task",
            "1",
            "--agent",
            "codex",
            "--workdir",
            workdir.to_str().unwrap(),
            "--no-governor",
            "--no-verify",
        ])
        .env("FOREMAN_CODEX_BIN", &fake_codex)
        .env("FOREMAN_VERIFY_LANE", tmp.path().join("verify.lock"))
        .env("FOREMAN_VERIFY_LANE_WAIT_SECS", "30")
        .output()
        .expect("operator run");
    assert!(
        run.status.success(),
        "explicit run must claim the flagged task: stdout={} stderr={}",
        stdout(&run),
        stderr(&run)
    );
    let task = Ledger::open(&db).unwrap().task(1).unwrap().unwrap();
    assert_eq!(task.status, "done");
    assert!(task.operator_driven);

    assert!(foreman(&db, &["task", "requeue", "1"]).status.success());
    let cleared = foreman(
        &db,
        &[
            "task",
            "set",
            "1",
            "--operator-driven=false",
            "--reason",
            "safe for unattended dispatch",
        ],
    );
    assert!(cleared.status.success(), "clear flag: {}", stderr(&cleared));
    let task = Ledger::open(&db).unwrap().task(1).unwrap().unwrap();
    assert!(!task.operator_driven);

    let enabled = foreman(
        &db,
        &[
            "task",
            "set",
            "1",
            "--operator-driven",
            "--reason",
            "operator must inspect this run",
        ],
    );
    assert!(
        enabled.status.success(),
        "bare flag should mean true: {}",
        stderr(&enabled)
    );
    assert!(
        Ledger::open(&db)
            .unwrap()
            .task(1)
            .unwrap()
            .unwrap()
            .operator_driven
    );
    assert!(
        foreman(
            &db,
            &[
                "task",
                "set",
                "1",
                "--operator-driven=false",
                "--reason",
                "inspection complete",
            ],
        )
        .status
        .success()
    );

    let dispatchable = foreman(
        &db,
        &[
            "dispatch",
            "--task",
            "1",
            "--dry-run",
            "--workdir",
            workdir.to_str().unwrap(),
        ],
    );
    assert!(
        dispatchable.status.success(),
        "cleared task should dispatch: {}",
        stderr(&dispatchable)
    );
    assert!(stdout(&dispatchable).contains("dispatch: task 1"));
    assert!(
        stdout(&dispatchable).contains("profile: none"),
        "dispatch decision must name the verifier profile: {}",
        stdout(&dispatchable)
    );
}

#[test]
fn operator_driven_cli_requires_reasons_for_reserve_and_release() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");

    let add_without_reason = foreman(
        &db,
        &[
            "task",
            "add",
            "unexplained",
            "--spec",
            "spec",
            "--operator-driven",
        ],
    );
    assert!(!add_without_reason.status.success());
    assert!(
        stderr(&add_without_reason).contains("requires a non-blank --reason"),
        "{}",
        stderr(&add_without_reason)
    );

    let added = foreman(&db, &["task", "add", "ordinary", "--spec", "spec"]);
    assert!(added.status.success(), "{}", stderr(&added));
    let reserve_without_reason = foreman(&db, &["task", "set", "1", "--operator-driven"]);
    assert!(!reserve_without_reason.status.success());
    assert!(
        stderr(&reserve_without_reason).contains("requires a non-blank --reason"),
        "{}",
        stderr(&reserve_without_reason)
    );

    assert!(
        foreman(
            &db,
            &[
                "task",
                "set",
                "1",
                "--operator-driven",
                "--reason",
                "await operator review",
            ],
        )
        .status
        .success()
    );
    let release_without_reason = foreman(&db, &["task", "set", "1", "--operator-driven=false"]);
    assert!(!release_without_reason.status.success());
    assert!(
        stderr(&release_without_reason).contains("requires a non-blank --reason"),
        "{}",
        stderr(&release_without_reason)
    );
}

#[test]
fn unexplained_reservation_is_marked_in_dispatch_and_status_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let workdir = tmp.path().join("workdir");
    std::fs::create_dir(&workdir).unwrap();
    let added = foreman(
        &db,
        &[
            "task",
            "add",
            "legacy reservation",
            "--spec",
            "spec",
            "--verifier",
            "none",
        ],
    );
    assert!(added.status.success(), "{}", stderr(&added));
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute("UPDATE tasks SET operator_driven = 1 WHERE id = 1", [])
        .unwrap();

    let queue = foreman(
        &db,
        &[
            "dispatch",
            "--dry-run",
            "--workdir",
            workdir.to_str().unwrap(),
        ],
    );
    assert!(queue.status.success(), "{}", stderr(&queue));
    assert!(
        stdout(&queue).contains("operator-driven: 1 [UNEXPLAINED]"),
        "{}",
        stdout(&queue)
    );

    let status = foreman(&db, &["status", "--json"]);
    assert!(status.status.success(), "{}", stderr(&status));
    let snapshot: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(snapshot["operator_driven"][0]["task_id"], 1);
    assert_eq!(
        snapshot["operator_driven"][0]["reservation_explained"],
        false
    );
}

#[test]
fn unattended_claim_refuses_operator_driven_without_consuming_an_attempt() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let id = ledger
        .add_task_scoped(
            "gate edit",
            "spec",
            "impl",
            "high",
            &[],
            TaskControls {
                verifier_profile: "none",
                crates: &[],
                operator_driven_reason: Some("operator integration test"),
            },
        )
        .unwrap();

    let error = ledger.claim_task(id, "dispatch@test").unwrap_err();
    assert_eq!(format!("{error:#}"), "task 1 not ready: operator-driven");
    assert_eq!(ledger.task(id).unwrap().unwrap().attempt, 0);

    let (claimed, _) = ledger
        .start_attempt(id, "operator@test", None, None, "codex", None)
        .unwrap();
    assert_eq!(claimed.attempt, 1);
    assert!(claimed.operator_driven);
}
