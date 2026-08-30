//! Real-CLI coverage for Foreman's durable state root. Every path is owned by
//! a temporary directory; this test must never inspect the live fleet root.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use cosmix_foreman::ledger::Ledger;
use cosmix_foreman::policy::{PolicyContext, hook_settings};
use cosmix_foreman::state::DbCreateMode;

fn foreman(cwd: &Path, args: &[&str], env: &[(&str, &Path)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_foreman"));
    command
        .env_clear()
        .env("HOME", cwd)
        .env("FOREMAN_VERIFY_LANE", cwd.join("verify.lock"))
        .env("FOREMAN_VERIFY_LANE_WAIT_SECS", "30")
        .current_dir(cwd)
        .args(args);
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().expect("run foreman")
}

fn assert_green(output: &Output) {
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn cli_db_precedence_is_flag_then_env_then_state_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("cwd");
    let flag = tmp.path().join("flag/ledger.db");
    let env_db = tmp.path().join("env/ledger.db");
    let state = tmp.path().join("state");
    let cosmix_var = tmp.path().join("var");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::create_dir(&state).unwrap();

    let flag_text = flag.to_str().unwrap();
    let output = foreman(
        &cwd,
        &["--db", flag_text, "init"],
        &[
            ("FOREMAN_DB", &env_db),
            ("STATE_DIRECTORY", &state),
            ("COSMIX_VAR", &cosmix_var),
        ],
    );
    assert_green(&output);
    assert!(flag.exists());
    assert!(!env_db.exists());

    let output = foreman(
        &cwd,
        &["init"],
        &[
            ("FOREMAN_DB", &env_db),
            ("STATE_DIRECTORY", &state),
            ("COSMIX_VAR", &cosmix_var),
        ],
    );
    assert_green(&output);
    assert!(env_db.exists());
    assert!(!state.join("ledger.db").exists());

    let output = foreman(
        &cwd,
        &["init"],
        &[("STATE_DIRECTORY", &state), ("COSMIX_VAR", &cosmix_var)],
    );
    assert_green(&output);
    assert!(state.join("ledger.db").exists());
    assert!(!cosmix_var.join("foreman/ledger.db").exists());
}

#[test]
fn default_state_uses_cosmix_var_and_never_the_process_cwd() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("cwd-that-must-not-capture-state");
    let cosmix_var = tmp.path().join("var");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::create_dir_all(cosmix_var.join("foreman")).unwrap();

    let output = foreman(&cwd, &["init"], &[("COSMIX_VAR", &cosmix_var)]);
    assert_green(&output);
    assert!(cosmix_var.join("foreman/ledger.db").exists());
    assert!(!cwd.join(".foreman/ledger.db").exists());
}

#[test]
fn implicit_state_never_creates_missing_parent_directories() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("cwd");
    let cosmix_var = tmp.path().join("missing-var");
    std::fs::create_dir(&cwd).unwrap();

    let output = foreman(&cwd, &["init"], &[("COSMIX_VAR", &cosmix_var)]);
    assert!(!output.status.success(), "{:?}", output);
    assert!(!cosmix_var.exists());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("does not exist"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn existing_legacy_ledger_and_conf_win_with_one_deprecation_note() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("cwd");
    let legacy_dir = cwd.join(".foreman");
    let legacy_db = legacy_dir.join("ledger.db");
    let cosmix_var = tmp.path().join("unused-var");
    std::fs::create_dir_all(&legacy_dir).unwrap();
    std::fs::write(
        legacy_dir.join("foreman.conf.mix"),
        "daily_budget_usd: 300\n",
    )
    .unwrap();

    let legacy_text = legacy_db.to_str().unwrap();
    assert_green(&foreman(&cwd, &["--db", legacy_text, "init"], &[]));
    let output = foreman(
        &cwd,
        &["governor", "status"],
        &[("COSMIX_VAR", &cosmix_var)],
    );
    assert_green(&output);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("daily budget: $300.00 (source: conf)"),
        "{stdout}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr.matches("deprecated cwd-relative ledger").count(), 1);
    assert!(stderr.contains(legacy_text), "{stderr}");

    let stop = foreman(
        &cwd,
        &["governor", "stop", "legacy-root-test"],
        &[("COSMIX_VAR", &cosmix_var)],
    );
    assert_green(&stop);
    assert_eq!(
        String::from_utf8_lossy(&stop.stderr)
            .matches("deprecated cwd-relative ledger")
            .count(),
        1
    );
    assert_eq!(
        std::fs::read_to_string(legacy_dir.join("STOP")).unwrap(),
        "legacy-root-test\n"
    );
    assert!(!cosmix_var.join("foreman/ledger.db").exists());
}

#[test]
fn kill_switch_remains_a_sibling_of_the_resolved_ledger() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("cwd");
    let state = tmp.path().join("state");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::create_dir(&state).unwrap();

    let output = foreman(
        &cwd,
        &["governor", "stop", "state-root-test"],
        &[("STATE_DIRECTORY", &state)],
    );
    assert_green(&output);
    assert_eq!(
        std::fs::read_to_string(state.join("STOP")).unwrap(),
        "state-root-test\n"
    );
    assert!(!cwd.join("STOP").exists());
}

#[test]
fn foreman_conf_cannot_override_the_config_resolution_ladder() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("cwd");
    let db = tmp.path().join("state/ledger.db");
    let beside = db.parent().unwrap().join("foreman.conf.mix");
    let bogus = tmp.path().join("bogus.conf.mix");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    std::fs::write(&beside, "daily_budget_usd: 123\n").unwrap();
    std::fs::write(&bogus, "this is not strict-data config\n").unwrap();

    let db_text = db.to_str().unwrap();
    let output = foreman(
        &cwd,
        &["--db", db_text, "governor", "status"],
        &[("FOREMAN_CONF", &bogus)],
    );
    assert_green(&output);
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("daily budget: $123.00 (source: conf)"),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn policy_hook_child_refuses_a_vanished_legacy_ledger() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("cwd");
    let legacy_dir = cwd.join(".foreman");
    let legacy = legacy_dir.join("ledger.db");
    let worktree = tmp.path().join("worktree");
    std::fs::create_dir_all(&legacy_dir).unwrap();
    std::fs::create_dir(&worktree).unwrap();
    let parent_ledger = Ledger::open(&legacy).unwrap();

    let settings = hook_settings(
        &PolicyContext {
            task_id: 27,
            worktree,
            branch: Some("task/27".into()),
            provider: "anthropic".into(),
            integration_base: "HEAD".into(),
            integration_branch: "main".into(),
            task_ref_template: "task/{id}".into(),
            package_manifest_template: Some("src/crates/{crate}/Cargo.toml".into()),
            restrict_manifest_edits: false,
            task_crates: Vec::new(),
        },
        &legacy,
        DbCreateMode::Never,
        None,
        Path::new(env!("CARGO_BIN_EXE_foreman")),
    );
    let hook = settings
        .pointer("/hooks/PreToolUse/0/hooks/0/command")
        .and_then(serde_json::Value::as_str)
        .unwrap();
    assert!(hook.contains("--db-create never"), "{hook}");

    std::fs::remove_file(&legacy).unwrap();
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(hook)
        .env_clear()
        .env("HOME", &cwd)
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(
        child.stdin.as_mut().unwrap(),
        br#"{"tool_name":"Read","tool_input":{}}"#,
    )
    .unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();

    assert!(!output.status.success(), "{output:?}");
    assert!(!legacy.exists(), "hook child recreated the legacy ledger");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(legacy.to_str().unwrap()), "{stderr}");
    assert!(stderr.contains("without creating it"), "{stderr}");
    drop(parent_ledger);
}

#[test]
fn project_policy_hook_child_inherits_db_creation_authority() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("state/ledger.db");
    let project = tmp.path().join("project.mix");
    let settings = hook_settings(
        &PolicyContext {
            task_id: 30,
            worktree: tmp.path().join("worktree"),
            branch: Some("change/30".into()),
            provider: "anthropic".into(),
            integration_base: "HEAD".into(),
            integration_branch: "trunk".into(),
            task_ref_template: "change/{id}".into(),
            package_manifest_template: None,
            restrict_manifest_edits: false,
            task_crates: Vec::new(),
        },
        &db,
        DbCreateMode::FileOnly,
        Some(&project),
        Path::new(env!("CARGO_BIN_EXE_foreman")),
    );
    let hook = settings
        .pointer("/hooks/PreToolUse/0/hooks/0/command")
        .and_then(serde_json::Value::as_str)
        .unwrap();
    assert!(hook.contains("--project"), "{hook}");
    assert!(hook.contains(project.to_str().unwrap()), "{hook}");
    assert!(hook.contains("--db"), "{hook}");
    assert!(hook.contains(db.to_str().unwrap()), "{hook}");
    assert!(hook.contains("--db-create file-only"), "{hook}");
}

#[test]
fn mayor_mcp_child_refuses_a_removed_implicit_state_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("cwd");
    let state = tmp.path().join("state");
    let db = state.join("ledger.db");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::create_dir(&state).unwrap();

    assert_green(&foreman(&cwd, &["init"], &[("STATE_DIRECTORY", &state)]));
    let parent_ledger = Ledger::open(&db).unwrap();
    std::fs::remove_dir_all(&state).unwrap();

    let output = foreman(
        &cwd,
        &[
            "--db",
            db.to_str().unwrap(),
            "--db-create",
            "file-only",
            "mcp",
        ],
        &[],
    );
    assert!(!output.status.success(), "{output:?}");
    assert!(!state.exists(), "MCP child recreated the state directory");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(state.to_str().unwrap()), "{stderr}");
    assert!(stderr.contains("does not exist"), "{stderr}");
    drop(parent_ledger);
}

#[test]
fn explicit_db_child_retains_parent_and_file_creation_authority() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("cwd");
    let db = tmp.path().join("new-parent/ledger.db");
    std::fs::create_dir(&cwd).unwrap();

    let output = foreman(
        &cwd,
        &[
            "--db",
            db.to_str().unwrap(),
            "--db-create",
            "parents-and-file",
            "init",
        ],
        &[],
    );
    assert_green(&output);
    assert!(db.exists());
}
