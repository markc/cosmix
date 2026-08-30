//! Tests for verifier profile controls (`foreman task set --verifier`).

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use cosmix_foreman::ledger::{ClaimToken, Ledger, TaskControls, TaskStatus};
use cosmix_foreman::verify::{builtin_profile_names, lookup_profile};

mod support;

fn foreman(db: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_foreman"));
    cmd.arg("--db")
        .arg(db)
        .args(args)
        .env(
            "FOREMAN_VERIFY_LANE",
            db.parent().unwrap().join("verify.lock"),
        )
        .env("FOREMAN_VERIFY_LANE_WAIT_SECS", "30");
    cmd
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn task_set_verifier_updates_profile_and_files_finding() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();

    // Add a task with the default rust profile
    let id = ledger
        .add_task_scoped(
            "gate edit",
            "spec",
            "impl",
            "high",
            &[],
            TaskControls {
                verifier_profile: "rust",
                crates: &[],
                operator_driven_reason: None,
            },
        )
        .unwrap();

    // Change to compositor profile
    let result = foreman(&db, &["task", "set", "1", "--verifier", "compositor"])
        .output()
        .expect("task set command");
    assert!(
        result.status.success(),
        "task set --verifier failed: stdout={} stderr={}",
        stdout(&result),
        stderr(&result)
    );
    assert!(stdout(&result).contains("verifier profile set to 'compositor'"));

    // Verify the profile was updated
    let task = ledger.task(id).unwrap().unwrap();
    assert_eq!(task.verifier_profile, "compositor");

    // Verify a finding was filed
    let findings = ledger.open_findings(10).unwrap();
    assert!(!findings.is_empty(), "a finding should be filed");
    let finding = &findings[0];
    assert_eq!(finding.2, "info", "finding should be info severity");
    assert_eq!(
        finding.3, "verifier profile changed for task 1",
        "finding title should match"
    );
    assert!(finding.4.contains("changed from 'rust' to 'compositor'"));
}

#[test]
fn task_set_verifier_refuses_invalid_profile() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();

    let id = ledger
        .add_task_scoped(
            "gate edit",
            "spec",
            "impl",
            "high",
            &[],
            TaskControls {
                verifier_profile: "rust",
                crates: &[],
                operator_driven_reason: None,
            },
        )
        .unwrap();

    // Try to set an invalid profile
    let result = foreman(&db, &["task", "set", "1", "--verifier", "bogus"])
        .output()
        .expect("task set command");
    assert!(
        !result.status.success(),
        "invalid profile should be rejected"
    );
    assert!(
        stderr(&result).contains("unknown verifier profile"),
        "error should list valid profiles: {}",
        stderr(&result)
    );
    assert!(
        stderr(&result).contains("rust, compositor, none"),
        "error should list all valid profiles: {}",
        stderr(&result)
    );

    // Verify the profile was NOT changed
    let task = ledger.task(id).unwrap().unwrap();
    assert_eq!(task.verifier_profile, "rust");
}

#[test]
fn task_set_verifier_refuses_while_running() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();

    let id = ledger
        .add_task_scoped(
            "gate edit",
            "spec",
            "impl",
            "high",
            &[],
            TaskControls {
                verifier_profile: "rust",
                crates: &[],
                operator_driven_reason: None,
            },
        )
        .unwrap();

    // Claim the task to put it in claimed state
    let task = ledger.claim_task(id, "agent@test").unwrap();
    assert_eq!(task.status, TaskStatus::Claimed.as_db_str());

    // Transition to running
    ledger.set_status(id, TaskStatus::Running).unwrap();

    // Verify the task is now running
    let task = ledger.task(id).unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Running.as_db_str());

    // Try to change profile while running
    let result = foreman(&db, &["task", "set", "1", "--verifier", "compositor"])
        .output()
        .expect("task set command");
    assert!(!result.status.success(), "should refuse while running");
    assert!(
        stderr(&result).contains("cannot change verifier profile while task 1 is running"),
        "error should explain why: {}",
        stderr(&result)
    );
}

#[test]
fn task_set_verifier_refuses_while_landing() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();

    let id = ledger
        .add_task_scoped(
            "gate edit",
            "spec",
            "impl",
            "high",
            &[],
            TaskControls {
                verifier_profile: "rust",
                crates: &[],
                operator_driven_reason: None,
            },
        )
        .unwrap();

    // Put task in landing state
    ledger.set_status(id, TaskStatus::Landing).unwrap();

    // Try to change profile while landing
    let result = foreman(&db, &["task", "set", "1", "--verifier", "compositor"])
        .output()
        .expect("task set command");
    assert!(!result.status.success(), "should refuse while landing");
    assert!(
        stderr(&result).contains("cannot change verifier profile while task 1 is landing"),
        "error should explain why: {}",
        stderr(&result)
    );
}

#[test]
fn task_set_verifier_allows_on_queued_task() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();

    let id = ledger
        .add_task_scoped(
            "gate edit",
            "spec",
            "impl",
            "high",
            &[],
            TaskControls {
                verifier_profile: "rust",
                crates: &[],
                operator_driven_reason: None,
            },
        )
        .unwrap();

    // Task is queued by default, should allow change
    let result = foreman(&db, &["task", "set", "1", "--verifier", "none"])
        .output()
        .expect("task set command");
    assert!(
        result.status.success(),
        "should allow change on queued task: {}",
        stderr(&result)
    );

    let task = ledger.task(id).unwrap().unwrap();
    assert_eq!(task.verifier_profile, "none");
}

#[test]
fn task_set_verifier_canonicalises_empty_alias_in_row_output_and_finding() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let id = ledger
        .add_task_scoped(
            "legacy gate edit",
            "spec",
            "impl",
            "high",
            &[],
            TaskControls {
                verifier_profile: "",
                crates: &[],
                operator_driven_reason: None,
            },
        )
        .unwrap();

    let result = foreman(&db, &["task", "set", "1", "--verifier", ""])
        .output()
        .expect("task set command");
    assert!(
        result.status.success(),
        "empty rust alias should canonicalise: {}",
        stderr(&result)
    );
    assert!(stdout(&result).contains("verifier profile set to 'rust'"));
    assert_eq!(ledger.task(id).unwrap().unwrap().verifier_profile, "rust");

    let findings = ledger.open_findings(10).unwrap();
    assert_eq!(findings.len(), 1);
    assert!(findings[0].4.contains("changed from 'rust' to 'rust'"));
}

#[test]
fn builtin_profile_table_has_unique_lookupable_names() {
    let mut unique = HashSet::new();
    for &name in builtin_profile_names() {
        assert!(unique.insert(name), "duplicate built-in profile {name:?}");
        let profile = lookup_profile(name)
            .unwrap_or_else(|error| panic!("listed profile {name:?} was rejected: {error:#}"));
        assert_eq!(profile.name, name, "listed name must already be canonical");
    }
}

#[test]
fn task_add_help_lists_all_builtin_profiles() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let result = foreman(&db, &["task", "add", "--help"])
        .output()
        .expect("task add --help");

    let help = stdout(&result);
    for name in builtin_profile_names() {
        assert!(
            help.contains(name),
            "task add help omitted built-in profile {name:?}: {help}"
        );
    }
}

#[test]
fn tier_zero_green_output_names_the_profile() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    ledger
        .add_task("green gate", "spec", "impl", "low", &[], "none")
        .unwrap();
    let fake_codex = tmp.path().join("fake-codex");
    support::write_executable(
        &fake_codex,
        "#!/bin/sh\n\
         echo '{\"type\":\"thread.started\",\"thread_id\":\"green-gate\"}'\n\
         echo '{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}'\n",
    );

    let result = foreman(
        &db,
        &[
            "run",
            "--task",
            "1",
            "--agent",
            "codex",
            "--workdir",
            tmp.path().to_str().unwrap(),
            "--no-governor",
        ],
    )
    .env("FOREMAN_CODEX_BIN", &fake_codex)
    .output()
    .expect("foreman run");
    assert!(
        result.status.success(),
        "green run failed: stdout={} stderr={}",
        stdout(&result),
        stderr(&result)
    );
    assert!(
        stdout(&result).contains("tier-0 green (profile: none)"),
        "green output did not name the profile: {}",
        stdout(&result)
    );
}

#[test]
fn refinery_landed_output_names_the_profile() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(&repo)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    std::fs::create_dir(&repo).unwrap();
    git(&["init", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "base\n").unwrap();
    git(&["add", "README.md"]);
    git(&["commit", "-m", "base"]);
    git(&["checkout", "-b", "task/1"]);
    std::fs::write(repo.join("landed.txt"), "landed\n").unwrap();
    git(&["add", "landed.txt"]);
    git(&["commit", "-m", "task"]);
    git(&["checkout", "main"]);

    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let id = ledger
        .add_task("green landing", "spec", "impl", "low", &[], "none")
        .unwrap();
    let claimed = ledger.claim_task(id, "test").unwrap();
    ledger
        .set_task_workspace(
            id,
            ClaimToken {
                owner: "test",
                generation: claimed.attempt,
            },
            None,
            Some("task/1"),
        )
        .unwrap();
    ledger.finish_task(id, "test", "done").unwrap();

    let result = foreman(
        &db,
        &[
            "refine",
            "--repo",
            repo.to_str().unwrap(),
            "--subdir",
            ".",
            "--tier",
            "0",
        ],
    )
    .output()
    .expect("foreman refine");
    assert!(
        result.status.success(),
        "green refine failed: stdout={} stderr={}",
        stdout(&result),
        stderr(&result)
    );
    assert!(
        stdout(&result).contains("task 1 [task/1]: landed (profile: none)"),
        "landing output did not name the profile: {}",
        stdout(&result)
    );
}
