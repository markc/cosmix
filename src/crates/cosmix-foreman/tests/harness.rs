//! End-to-end harness tests: fake vendor CLIs (a /bin/sh cat of a captured
//! fixture stream) driven through the real Session plumbing, the ledger, and
//! the runner. Proves the Phase-0 exit criterion — one task, one driver,
//! budget caps, normalized events + usage landing in SQLite.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use cosmix_foreman::driver::claude::ClaudeDriver;
use cosmix_foreman::driver::codex::CodexDriver;
use cosmix_foreman::executor::{AgentEvent, Budget, Executor, StopReason, Workspace};
use cosmix_foreman::ledger::{ClaimToken, FindingReason, Ledger, TaskControls};
use cosmix_foreman::runner::{RunOptions, run_task};
use cosmix_foreman::verify::run_commands;

mod support;

/// This crate's manifest directory, resolved when the test *runs* rather than
/// baked in by `env!`. Cargo exports `CARGO_MANIFEST_DIR` into the test process
/// too, and that value names the tree cargo is actually running in; `env!`
/// records whichever tree last *compiled* the binary. The two diverge when one
/// `CARGO_TARGET_DIR` is shared across several git worktrees of this repo —
/// cargo's dep-info paths are workspace-relative, so an artefact built in a
/// sibling worktree is judged fresh and rerun here, still pointing at that
/// tree's testdata. Falls back to the compile-time value outside cargo.
fn manifest_dir() -> String {
    std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| env!("CARGO_MANIFEST_DIR").to_string())
}

fn fixture(name: &str) -> String {
    format!("{}/testdata/{name}", manifest_dir())
}

/// All fake vendor CLIs are written once, before any test forks — writing a
/// script while a concurrent test's fork+exec is in flight fails ETXTBSY
/// (the forked child transiently holds the write fd open). Write-once
/// avoids RE-writing a live executable; `write_executable` makes even the
/// first write safe, because OnceLock init is lazy and can race another
/// test's in-flight fork like any other write.
struct Fixtures {
    _dir: tempfile::TempDir,
    /// Cats $FAKE_STREAM and exits $FAKE_EXIT.
    agent: PathBuf,
    /// Reports huge usage then hangs — for the budget-kill path.
    hanging: PathBuf,
    /// Writes to stderr and fails.
    noisy: PathBuf,
}

fn fixtures() -> &'static Fixtures {
    static FIXTURES: std::sync::OnceLock<Fixtures> = std::sync::OnceLock::new();
    FIXTURES.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, body: &str| -> PathBuf {
            let path = dir.path().join(name);
            support::write_executable(&path, body);
            path
        };
        Fixtures {
            agent: write("fake-agent", "#!/bin/sh\ncat \"$FAKE_STREAM\"\nexit \"${FAKE_EXIT:-0}\"\n"),
            hanging: write(
                "fake-hang",
                "#!/bin/sh\n\
                 echo '{\"type\":\"thread.started\",\"thread_id\":\"t-hang\"}'\n\
                 echo '{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":10,\"output_tokens\":99999}}'\n\
                 sleep 600\n\
                 touch \"$HANG_MARKER\"\n",
            ),
            noisy: write("fake-noisy", "#!/bin/sh\necho 'auth expired' >&2\nexit 1\n"),
            _dir: dir,
        }
    })
}

fn drain(session: &mut cosmix_foreman::executor::Session) -> Vec<AgentEvent> {
    let mut out = Vec::new();
    while let Ok(Some(ev)) = session.next_event(Duration::from_secs(10)) {
        out.push(ev);
    }
    out
}

/// Sentinel marking the process an owned-helper test is allowed to run in.
const HELPER_ENV: &str = "COSMIX_FOREMAN_HARNESS_HELPER";

/// Run `name` (an `#[ignore]`d test below) in a process of its own and fail
/// loudly if the scenario fails. Two scenarios need that:
/// `agent_sessions_are_subscription_only_by_default` gives its helper child
/// ambient API keys to prove the drivers scrub them, and
/// `glm_lane_pins_the_auto_compact_window_and_claude_lane_does_not`
/// asserts variables are ABSENT from a child that inherits this process's
/// environment, an assertion the ambient shell can break: the fleet's GLM
/// lane pins `CLAUDE_CODE_AUTO_COMPACT_WINDOW`/`CLAUDE_CODE_MAX_OUTPUT_TOKENS`
/// in its own terminal, and a suite run from such a terminal inherits them.
/// `scrub` names variables the helper process must not inherit (the shim
/// configures the child `Command`; the parent's own environment is never
/// mutated). The owned helper process (`--exact name --test-threads=1`) gives
/// each scenario a process it owns, so the rest of this binary needs no env
/// serialization at all.
fn run_owned_helper(name: &str, scrub: &[&str], configure: impl FnOnce(&mut Command)) {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args([
        "--exact",
        name,
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ])
    .env(HELPER_ENV, name);
    for var in scrub {
        cmd.env_remove(var);
    }
    configure(&mut cmd);
    let out = cmd.output().expect("spawn owned helper test process");
    assert!(
        out.status.success(),
        "owned helper {name} failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn assert_owned_helper(name: &str) {
    assert_eq!(
        std::env::var(HELPER_ENV).as_deref(),
        Ok(name),
        "this test must only run inside the process spawned for it"
    );
}

fn git(repo: &std::path::Path, args: &[&str]) -> String {
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

fn init_repo(repo: &std::path::Path, branch: &str) {
    std::fs::create_dir(repo).unwrap();
    git(repo, &["init", "-b", branch]);
    std::fs::write(repo.join("base.txt"), "base\n").unwrap();
    git(repo, &["add", "."]);
    git(repo, &["commit", "-m", "base"]);
}

#[test]
fn claude_session_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let driver = ClaudeDriver::new()
        .with_program(fixtures().agent.to_str().unwrap())
        .with_env("FAKE_STREAM", fixture("claude-ok.jsonl"));
    let ws = Workspace {
        dir: tmp.path().to_path_buf(),
        verify_subdir: None,
    };
    let mut session = driver
        .start("do the thing", &ws, &Budget::default())
        .unwrap();
    let events = drain(&mut session);
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Text { .. })));
    let outcome = session.wait().unwrap();
    assert_eq!(outcome.stop, StopReason::Done);
    assert_eq!(outcome.usage.cost_usd, Some(0.0421));
}

#[test]
fn claude_with_env_cannot_override_pinned_cargo_target_dir_in_child() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("src")).unwrap();
    let probe = tmp.path().join("claude-env-probe");
    support::write_executable(
        &probe,
        "#!/bin/sh\nprintf '%s' \"${CARGO_TARGET_DIR-unset}\" > \"$PROBE_OUT\"\n\
         cat \"$FAKE_STREAM\"\n",
    );
    let out = tmp.path().join("seen");
    let driver = ClaudeDriver::new()
        .with_program(probe.to_str().unwrap())
        .with_env("FAKE_STREAM", fixture("claude-ok.jsonl"))
        .with_env("PROBE_OUT", out.to_str().unwrap())
        .with_env("CARGO_TARGET_DIR", "/x");
    let ws = Workspace {
        dir: tmp.path().to_path_buf(),
        verify_subdir: Some("src".into()),
    };
    let mut session = driver.start("probe", &ws, &Budget::default()).unwrap();
    let _ = drain(&mut session);
    let _ = session.wait().unwrap();
    assert_eq!(
        std::fs::read_to_string(out).unwrap(),
        tmp.path()
            .canonicalize()
            .unwrap()
            .join("src/target")
            .display()
            .to_string()
    );
}

#[test]
fn codex_with_env_cannot_override_pinned_cargo_target_dir_in_child() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("src")).unwrap();
    let probe = tmp.path().join("codex-env-probe");
    support::write_executable(
        &probe,
        "#!/bin/sh\nprintf '%s' \"${CARGO_TARGET_DIR-unset}\" > \"$PROBE_OUT\"\n\
         cat \"$FAKE_STREAM\"\n",
    );
    let out = tmp.path().join("seen");
    let driver = CodexDriver::new()
        .with_program(probe.to_str().unwrap())
        .with_env("FAKE_STREAM", fixture("codex-ok.jsonl"))
        .with_env("PROBE_OUT", out.to_str().unwrap())
        .with_env("CARGO_TARGET_DIR", "/x");
    let ws = Workspace {
        dir: tmp.path().to_path_buf(),
        verify_subdir: Some("src".into()),
    };
    let mut session = driver.start("probe", &ws, &Budget::default()).unwrap();
    let _ = drain(&mut session);
    let _ = session.wait().unwrap();
    assert_eq!(
        std::fs::read_to_string(out).unwrap(),
        tmp.path()
            .canonicalize()
            .unwrap()
            .join("src/target")
            .display()
            .to_string()
    );
}

#[test]
fn driver_build_warms_the_exact_target_the_verifier_reuses() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("src");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    std::fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"driver-warm-probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(workspace.join("src/main.rs"), "fn main() {}\n").unwrap();
    let agent = tmp.path().join("agent");
    support::write_executable(
        &agent,
        "#!/bin/sh\nset -eu\ncd src\ncargo build\ncat \"$FAKE_STREAM\"\n",
    );
    let driver = ClaudeDriver::new()
        .with_program(agent.to_str().unwrap())
        .with_env("FAKE_STREAM", fixture("claude-ok.jsonl"));
    let ws = Workspace {
        dir: tmp.path().to_path_buf(),
        verify_subdir: Some("src".into()),
    };
    let mut session = driver.start("warm", &ws, &Budget::default()).unwrap();
    let _ = drain(&mut session);
    assert_eq!(session.wait().unwrap().stop, StopReason::Done);

    let artifact = workspace.join("target/debug/driver-warm-probe");
    let before = std::fs::metadata(&artifact).unwrap().modified().unwrap();
    let report = run_commands(
        "driver-warm-reuse",
        &[vec!["cargo".into(), "build".into(), "--verbose".into()]],
        &workspace,
    )
    .unwrap();
    assert!(report.pass, "verifier build must pass: {report:?}");
    assert_eq!(
        report.target_dir.as_deref(),
        workspace.join("target").to_str()
    );
    assert_eq!(
        std::fs::metadata(&artifact).unwrap().modified().unwrap(),
        before
    );
    assert!(
        report.steps[0].tail.contains("Fresh"),
        "{:?}",
        report.steps[0]
    );
}

#[test]
fn claude_and_glm_classify_dirty_abandoned_background_bash_with_bounded_retry() {
    for (lane, driver) in [
        (
            "claude",
            ClaudeDriver::new()
                .with_program(fixtures().agent.to_str().unwrap())
                .with_env("FAKE_STREAM", fixture("claude-background-abandoned.jsonl")),
        ),
        (
            "glm",
            ClaudeDriver::glm("zk-test")
                .with_program(fixtures().agent.to_str().unwrap())
                .with_env("FAKE_STREAM", fixture("claude-background-abandoned.jsonl")),
        ),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        init_repo(&repo, "task/56");
        std::fs::write(repo.join("wip.txt"), "uncommitted\n").unwrap();
        let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
        let task = ledger
            .add_task("background trap", "run tests", "impl", "low", &[], "none")
            .unwrap();
        let opts = RunOptions {
            workdir: repo,
            branch: Some("task/56".into()),
            verify: false,
            ..Default::default()
        };

        // One informed retry is free; repeating the exact mechanism parks
        // without consuming the task-quality escalation ladder.
        for expected_status in ["queued", "parked"] {
            let report = run_task(&ledger, task, &driver, &opts).unwrap();
            assert_eq!(report.task_status, expected_status, "{lane}");
            assert_eq!(report.outcome.stop, StopReason::Error, "{lane}");
            assert!(
                report
                    .outcome
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .starts_with("agent_abandoned_background"),
                "{lane}: {:?}",
                report.outcome.error
            );
        }

        let task_row = ledger.task(task).unwrap().unwrap();
        assert_eq!(task_row.status, "parked", "{lane}");
        assert_eq!(task_row.background_abandonments, 2, "{lane}");
        assert_eq!(
            task_row.ladder_failures, 0,
            "{lane}: harness abandonment must not charge a ladder rung"
        );
        let runs = ledger.recent_runs(2).unwrap();
        assert_eq!(runs.len(), 2, "{lane}");
        for run in runs {
            assert_eq!(run.delivery, "harness_error", "{lane}");
            assert_eq!(run.quality, "agent_abandoned_background", "{lane}");
        }
        let findings = ledger.task_findings(task).unwrap();
        assert_eq!(findings.len(), 1, "{lane}");
        assert_eq!(findings[0].1, "blocker", "{lane}");
        assert!(findings[0].2.contains("background Bash"), "{lane}");
        assert!(findings[0].3.contains("run_in_background"), "{lane}");
        assert!(findings[0].3.contains("single-turn `claude -p`"), "{lane}");
        assert!(findings[0].3.contains("repeated 2 consecutive"), "{lane}");

        ledger.requeue_task(task, false).unwrap();
        let retried = ledger.task(task).unwrap().unwrap();
        assert_eq!(retried.status, "queued", "{lane}");
        assert_eq!(retried.background_abandonments, 0, "{lane}");
        assert!(ledger.task_findings(task).unwrap().is_empty(), "{lane}");
    }
}

#[test]
fn committed_run_with_teardown_background_signal_is_not_abandonment() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo, "task/56");
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let task = ledger
        .add_task("committed work", "run tests", "impl", "low", &[], "none")
        .unwrap();
    let driver = ClaudeDriver::new()
        .with_program(fixtures().agent.to_str().unwrap())
        .with_env(
            "FAKE_STREAM",
            fixture("claude-background-killed-after-result.jsonl"),
        );
    let opts = RunOptions {
        workdir: repo,
        branch: Some("task/56".into()),
        verify: false,
        ..Default::default()
    };

    let report = run_task(&ledger, task, &driver, &opts).unwrap();

    assert_eq!(report.task_status, "done");
    assert_eq!(report.outcome.stop, StopReason::Done);
    assert_eq!(report.outcome.error, None);
    let task_row = ledger.task(task).unwrap().unwrap();
    assert_eq!(task_row.background_abandonments, 0);
    let run = ledger.recent_runs(1).unwrap().remove(0);
    assert_eq!(run.delivery, "delivered");
    assert_eq!(run.quality, "unknown");
    assert!(ledger.task_findings(task).unwrap().is_empty());
}

#[test]
fn completed_background_bash_is_not_abandonment() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let task = ledger
        .add_task(
            "completed background",
            "run tests",
            "impl",
            "low",
            &[],
            "none",
        )
        .unwrap();
    let driver = ClaudeDriver::new()
        .with_program(fixtures().agent.to_str().unwrap())
        .with_env("FAKE_STREAM", fixture("claude-background-completed.jsonl"));
    let opts = RunOptions {
        workdir: tmp.path().to_path_buf(),
        verify: false,
        ..Default::default()
    };

    let report = run_task(&ledger, task, &driver, &opts).unwrap();

    assert_eq!(report.task_status, "done");
    assert_eq!(report.outcome.stop, StopReason::Done);
    let run = ledger.recent_runs(1).unwrap().remove(0);
    assert_eq!(run.delivery, "delivered");
    assert_eq!(run.quality, "unknown");
    assert!(ledger.task_findings(task).unwrap().is_empty());
}

#[test]
fn claude_budget_exit_two_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let driver = ClaudeDriver::new()
        .with_program(fixtures().agent.to_str().unwrap())
        .with_env("FAKE_STREAM", fixture("claude-budget.jsonl"))
        .with_env("FAKE_EXIT", "2");
    let ws = Workspace {
        dir: tmp.path().to_path_buf(),
        verify_subdir: None,
    };
    // Exit 2 reads as a budget ceiling only because a budget was set.
    let budget = Budget {
        max_turns: Some(2),
        ..Default::default()
    };
    let mut session = driver.start("p", &ws, &budget).unwrap();
    drain(&mut session);
    let outcome = session.wait().unwrap();
    assert_eq!(outcome.stop, StopReason::BudgetCeiling);
}

#[test]
fn codex_failure_carries_stderr_tail() {
    let tmp = tempfile::tempdir().unwrap();
    let driver = CodexDriver::new().with_program(fixtures().noisy.to_str().unwrap());
    let ws = Workspace {
        dir: tmp.path().to_path_buf(),
        verify_subdir: None,
    };
    let mut session = driver.start("p", &ws, &Budget::default()).unwrap();
    drain(&mut session);
    let outcome = session.wait().unwrap();
    assert_eq!(outcome.stop, StopReason::Error);
    assert!(
        outcome
            .error
            .as_deref()
            .unwrap_or("")
            .contains("auth expired"),
        "stderr tail should be in the error, got {:?}",
        outcome.error
    );
}

#[test]
fn ledger_claim_is_atomic_and_deps_gate() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let dep = ledger
        .add_task("dep", "spec", "impl", "low", &[], "none")
        .unwrap();
    let t = ledger
        .add_task("t", "spec", "impl", "low", &[dep], "none")
        .unwrap();

    // Dep not done → claim refused.
    assert!(ledger.claim_task(t, "a").is_err());
    ledger.claim_task(dep, "a").unwrap();
    ledger.set_task_status(dep, "done").unwrap();

    let claimed = ledger.claim_task(t, "a").unwrap();
    assert_eq!(claimed.status, "claimed");
    assert_eq!(claimed.attempt, 1);
    // Second claim loses.
    assert!(ledger.claim_task(t, "b").is_err());

    // Bounce releases the claim; a re-claim bumps attempt.
    ledger.set_task_status(t, "bounced").unwrap();
    let reclaimed = ledger.claim_task(t, "b").unwrap();
    assert_eq!(reclaimed.attempt, 2);
    assert_eq!(reclaimed.claimed_by.as_deref(), Some("b"));
}

#[test]
fn runner_records_run_events_and_dispositions_task() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let task = ledger
        .add_task("smoke", "run the fixture", "impl", "low", &[], "none")
        .unwrap();

    let driver = ClaudeDriver::new()
        .with_program(fixtures().agent.to_str().unwrap())
        .with_env("FAKE_STREAM", fixture("claude-ok.jsonl"));
    let opts = RunOptions {
        workdir: tmp.path().to_path_buf(),
        stall_secs: 10,
        ..Default::default()
    };
    let report = run_task(&ledger, task, &driver, &opts).unwrap();

    assert_eq!(report.task_status, "done");
    assert_eq!(report.outcome.stop, StopReason::Done);
    assert_eq!(ledger.task(task).unwrap().unwrap().status, "done");

    let runs = ledger.recent_runs(5).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].tokens_in, 42);
    assert_eq!(runs[0].fresh_input_tokens, Some(42));
    assert_eq!(runs[0].cache_read_input_tokens, None);
    assert_eq!(runs[0].cache_creation_input_tokens, None);
    assert_eq!(runs[0].tokens_out, 14);
    assert_eq!(runs[0].cost_usd, Some(0.0421));
    assert_eq!(runs[0].verdict.as_deref(), Some("done"));
    assert_eq!(runs[0].role, "implement");
    assert_eq!(runs[0].delivery, "delivered");
    assert_eq!(runs[0].quality, "tier_0_passed");
    assert_eq!(runs[0].session_ref.as_deref(), Some("sess-1"));
    assert!((ledger.total_spend_usd().unwrap() - 0.0421).abs() < 1e-9);
}

#[test]
fn fixture_agent_changes_own_crate_dependencies_and_finishes_without_policy_finding() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join("src/crates/fixture-crate/src")).unwrap();
    let manifest = repo.join("src/crates/fixture-crate/Cargo.toml");
    std::fs::write(
        &manifest,
        "[package]\nname = \"fixture-crate\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nold-pin = \"1\"\nremove-me = \"1\"\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("src/crates/fixture-crate/src/lib.rs"),
        "pub const READY: bool = true;\n",
    )
    .unwrap();
    for args in [
        &["init", "-b", "main"][..],
        &["config", "user.email", "fixture@example.com"][..],
        &["config", "user.name", "Policy Fixture"][..],
        &["add", "."][..],
        &["commit", "-m", "fixture base"][..],
        &["checkout", "-b", "task/1"][..],
    ] {
        let output = Command::new("git")
            .args(args)
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let task = ledger
        .add_task_scoped(
            "fixture dependency maintenance",
            "Add, remove, and re-pin dependencies; do not modify other-crate",
            "impl",
            "low",
            &[],
            TaskControls {
                verifier_profile: "none",
                crates: &["fixture-crate".to_string()],
                operator_driven_reason: None,
            },
        )
        .unwrap();
    assert_eq!(task, 1, "fixture script pins task/1");

    let agent = tmp.path().join("manifest-edit-agent");
    support::write_executable(
        &agent,
        r#"#!/bin/sh
set -eu
payload='{"tool_name":"Write","tool_input":{"file_path":"'"$PWD"'/src/crates/fixture-crate/Cargo.toml","content":"[package]\nname = \"fixture-crate\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nold-pin = \"2\"\nadd-me = \"1\"\n"}}'
printf '%s' "$payload" | "$FIXTURE_FOREMAN" --db "$FIXTURE_DB" policy-check --task 1 --worktree "$PWD" --provider anthropic --branch task/1 --integration-base "$(git rev-parse main)"
cat > src/crates/fixture-crate/Cargo.toml <<'MANIFEST'
[package]
name = "fixture-crate"
version = "0.1.0"
edition = "2024"

[dependencies]
old-pin = "2"
add-me = "1"
MANIFEST
git add src/crates/fixture-crate/Cargo.toml
git commit -m 'maintain fixture dependencies'
cat "$FAKE_STREAM"
"#,
    );

    let driver = ClaudeDriver::new()
        .with_program(agent.to_str().unwrap())
        .with_env("FIXTURE_FOREMAN", env!("CARGO_BIN_EXE_foreman"))
        .with_env("FIXTURE_DB", db.to_string_lossy())
        .with_env(
            "FOREMAN_VERIFY_LANE",
            tmp.path().join("verify.lock").to_string_lossy(),
        )
        .with_env("FOREMAN_VERIFY_LANE_WAIT_SECS", "30")
        .with_env("FAKE_STREAM", fixture("claude-ok.jsonl"));
    let opts = RunOptions {
        workdir: repo.clone(),
        branch: Some("task/1".into()),
        verify: false,
        stall_secs: 10,
        ..Default::default()
    };
    let report = run_task(&ledger, task, &driver, &opts).unwrap();

    assert_eq!(report.task_status, "done");
    let landed = std::fs::read_to_string(manifest).unwrap();
    assert!(landed.contains("old-pin = \"2\""));
    assert!(landed.contains("add-me = \"1\""));
    assert!(!landed.contains("remove-me"));
    assert!(
        ledger.task_findings(task).unwrap().is_empty(),
        "an allowed scoped manifest edit must not file a policy finding"
    );
}

#[test]
fn runner_records_vendor_cache_breakdown_in_events_and_run_total() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let stream = tmp.path().join("claude-cache.jsonl");
    std::fs::write(
        &stream,
        concat!(
            "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"cache-run\"}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,",
            "\"result\":\"done\",\"session_id\":\"cache-run\",\"usage\":{",
            "\"input_tokens\":1000,\"cache_read_input_tokens\":4000,",
            "\"cache_creation_input_tokens\":500,\"output_tokens\":2000}}\n"
        ),
    )
    .unwrap();
    let ledger = Ledger::open(&db).unwrap();
    let task = ledger
        .add_task("cache telemetry", "spec", "impl", "low", &[], "none")
        .unwrap();
    let driver = ClaudeDriver::new()
        .with_program(fixtures().agent.to_str().unwrap())
        .with_env("FAKE_STREAM", stream.to_str().unwrap());
    let report = run_task(
        &ledger,
        task,
        &driver,
        &RunOptions {
            workdir: tmp.path().to_path_buf(),
            stall_secs: 10,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(report.outcome.stop, StopReason::Done);

    let row = ledger.recent_runs(1).unwrap().remove(0);
    assert_eq!(row.fresh_input_tokens, Some(1000));
    assert_eq!(row.cache_read_input_tokens, Some(4000));
    assert_eq!(row.cache_creation_input_tokens, Some(500));
    assert_eq!(
        row.tokens_in,
        row.fresh_input_tokens.unwrap()
            + row.cache_read_input_tokens.unwrap()
            + row.cache_creation_input_tokens.unwrap(),
        "the stored folded total must remain exactly the sum of its components"
    );

    let payload: String = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT payload FROM events WHERE run_id = ?1 AND kind = 'usage' \
             ORDER BY seq DESC LIMIT 1",
            [report.run_id],
            |row| row.get(0),
        )
        .unwrap();
    let event: AgentEvent = serde_json::from_str(&payload).unwrap();
    let AgentEvent::Usage { usage } = event else {
        panic!("queried usage event decoded as another event kind");
    };
    assert_eq!(usage.fresh_input_tokens, Some(1000));
    assert_eq!(usage.cache_read_input_tokens, Some(4000));
    assert_eq!(usage.cache_creation_input_tokens, Some(500));
    assert_eq!(usage.input_tokens, 5500);
}

#[test]
fn historical_usage_event_payloads_still_decode_as_unknown() {
    // Every `usage` event written before this change carries only the folded
    // total. Those rows stay readable, and the absent components decode as
    // unknown rather than failing the read or inventing a zero.
    let legacy =
        r#"{"kind":"usage","usage":{"input_tokens":6000000,"output_tokens":1234,"cost_usd":2.5}}"#;
    let event: AgentEvent = serde_json::from_str(legacy).unwrap();
    let AgentEvent::Usage { usage } = event else {
        panic!("legacy usage payload decoded as another event kind");
    };
    assert_eq!(usage.input_tokens, 6_000_000);
    assert_eq!(usage.output_tokens, 1234);
    assert_eq!(usage.fresh_input_tokens, None);
    assert_eq!(usage.cache_read_input_tokens, None);
    assert_eq!(usage.cache_creation_input_tokens, None);
}

#[test]
fn runner_records_the_rebased_base_as_the_run_s_first_event() {
    // The clean half of the provisioning rebase: "the gate was green on
    // task N" is worth nothing unless the trail also says WHICH integration
    // commit that attempt's branch sat on. Seq 0, ahead of the agent
    // stream, because `events.run_id` is NOT NULL — the run is the only
    // place a per-attempt fact like this can live.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("l.db");
    let ledger = Ledger::open(&db).unwrap();
    let task = ledger
        .add_task("rebased", "spec", "impl", "low", &[], "none")
        .unwrap();

    let driver = ClaudeDriver::new()
        .with_program(fixtures().agent.to_str().unwrap())
        .with_env("FAKE_STREAM", fixture("claude-ok.jsonl"));
    let opts = RunOptions {
        workdir: tmp.path().to_path_buf(),
        stall_secs: 10,
        branch: Some("task/1".into()),
        rebased_onto: Some("cafef00dcafef00dcafef00dcafef00dcafef00d".into()),
        verify: false,
        ..Default::default()
    };
    run_task(&ledger, task, &driver, &opts).unwrap();

    let run = ledger.recent_runs(1).unwrap().remove(0);
    let conn = rusqlite::Connection::open(&db).unwrap();
    let (kind, payload): (String, String) = conn
        .query_row(
            "SELECT kind, payload FROM events WHERE run_id = ?1 ORDER BY seq LIMIT 1",
            [run.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(kind, "rebase", "the base of record must lead the trail");
    assert!(
        payload.contains("cafef00dcafef00dcafef00dcafef00dcafef00d") && payload.contains("task/1"),
        "the event must name the base AND the branch it applies to: {payload}"
    );
}

#[test]
fn runner_kills_session_on_output_token_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let task = ledger
        .add_task("hang", "spec", "impl", "low", &[], "none")
        .unwrap();

    let marker_path = tmp.path().join("hang-completed.txt");
    let driver = CodexDriver::new()
        .with_program(fixtures().hanging.to_str().unwrap())
        .with_env("HANG_MARKER", marker_path.to_str().unwrap());
    let opts = RunOptions {
        workdir: tmp.path().to_path_buf(),
        budget: Budget {
            max_output_tokens: Some(100),
            ..Default::default()
        },
        stall_secs: 10,
        ..Default::default()
    };
    let report = run_task(&ledger, task, &driver, &opts).unwrap();

    // The mechanism assertion: killed by budget ceiling, not natural exit.
    assert_eq!(report.outcome.stop, StopReason::BudgetCeiling);
    assert_eq!(report.task_status, "bounced");

    // Prove the session was killed, not completed: the marker file must NOT exist.
    // If the fixture ran to completion, it would have touched this file after the 600s sleep.
    assert!(
        !marker_path.exists(),
        "fixture must be killed before it writes the completion marker"
    );

    // The kill point is an output-token cap and the breakdown work does not
    // touch outputs — this fixture and these assertions are unchanged from
    // before it, which is the whole point of asserting them here.
    assert_eq!(report.outcome.usage.output_tokens, 99_999);
    assert_eq!(report.outcome.usage.input_tokens, 10);
    let row = ledger.recent_runs(1).unwrap().remove(0);
    assert_eq!(row.tokens_in, 10, "the folded input total must not move");
    // A bounced task is claimable again (escalation ladder feeds on this).
    assert!(ledger.claim_task(task, "next-tier").is_ok());
}

#[test]
fn runner_fails_task_on_spawn_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let task = ledger
        .add_task("t", "spec", "impl", "low", &[], "none")
        .unwrap();

    let driver = ClaudeDriver::new().with_program("/nonexistent/claude-binary");
    let opts = RunOptions {
        workdir: tmp.path().to_path_buf(),
        ..Default::default()
    };
    let report = run_task(&ledger, task, &driver, &opts).unwrap();
    assert_eq!(report.task_status, "failed");
    assert_eq!(report.outcome.stop, StopReason::Error);
    assert_eq!(ledger.task(task).unwrap().unwrap().status, "failed");
    let runs = ledger.recent_runs(1).unwrap();
    assert_eq!(runs[0].verdict.as_deref(), Some("error"));
    assert_eq!(runs[0].delivery, "harness_error");
    assert_eq!(runs[0].quality, "unknown");
}

#[test]
fn claude_args_carry_budget_flags() {
    let d = ClaudeDriver::new().with_model(Some("opus".into()));
    let budget = Budget {
        max_turns: Some(7),
        max_budget_usd: Some(1.5),
        ..Default::default()
    };
    let args = d.build_args("hello", &budget);
    let joined = args.join(" ");
    assert!(joined.contains("-p hello"));
    assert!(joined.contains("--output-format stream-json"));
    assert!(
        joined.contains("--verbose"),
        "stream-json in print mode requires --verbose"
    );
    assert!(joined.contains("--model opus"));
    assert!(joined.contains("--max-turns 7"));
    assert!(joined.contains("--max-budget-usd 1.5"));
    assert!(
        joined.contains("--permission-mode bypassPermissions"),
        "agentic-first: the unattended path is the default"
    );
    assert!(joined.contains("--append-system-prompt"));
    assert!(joined.contains("single-turn headless session"));
    assert!(joined.contains("Never use Bash run_in_background"));
    assert!(joined.contains("foreground with an explicit timeout"));
    assert!(joined.contains("commit all work before the final message"));

    let strict = ClaudeDriver::new().with_permission_mode(Some("plan".into()));
    let joined = strict.build_args("hello", &Budget::default()).join(" ");
    assert!(
        joined.contains("--permission-mode plan"),
        "guard rails stay opt-in-able"
    );
}

#[test]
fn codex_args_shape() {
    let d = CodexDriver::new().with_model(Some("gpt-5.6-sol".into()));
    let args = d.build_args("hello world", &Budget::default());
    assert_eq!(args[0], "exec");
    assert!(args.contains(&"--json".to_string()));
    assert!(args.contains(&"-m".to_string()));
    let n = args.len();
    assert_eq!(args[n - 1], "hello world", "prompt goes last");
    assert_eq!(
        args[n - 2],
        "--",
        "dash-leading prompts must not parse as flags"
    );
    let joined = args.join(" ");
    assert!(
        joined.contains("--sandbox workspace-write"),
        "agentic-first: read-only exec 'succeeds' having written nothing"
    );

    let read_only = CodexDriver::new().with_sandbox("read-only");
    let joined = read_only.build_args("review", &Budget::default()).join(" ");
    assert!(joined.contains("--sandbox read-only"));
    assert!(!joined.contains("--sandbox workspace-write"));
}

#[test]
fn codex_refuses_unenforceable_budgets() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = Workspace {
        dir: tmp.path().to_path_buf(),
        verify_subdir: None,
    };
    let d = CodexDriver::new().with_program(fixtures().agent.to_str().unwrap());
    for budget in [
        Budget {
            max_turns: Some(3),
            ..Default::default()
        },
        Budget {
            max_budget_usd: Some(1.0),
            ..Default::default()
        },
    ] {
        match d.start("p", &ws, &budget) {
            Ok(_) => panic!("a cap that cannot be enforced must be refused, not accepted"),
            Err(err) => assert!(format!("{err:#}").contains("cannot enforce"), "{err:#}"),
        }
    }
}

#[test]
fn glm_refuses_fantasy_dollar_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = Workspace {
        dir: tmp.path().to_path_buf(),
        verify_subdir: None,
    };
    let d = ClaudeDriver::glm("zk-test").with_program(fixtures().agent.to_str().unwrap());
    let budget = Budget {
        max_budget_usd: Some(1.0),
        ..Default::default()
    };
    match d.start("p", &ws, &budget) {
        Ok(_) => panic!("glm dollar budget caps a fictional number; must be refused"),
        Err(err) => assert!(format!("{err:#}").contains("Anthropic-priced"), "{err:#}"),
    }
}

/// The GLM lane must tell the CLI's auto-compactor the window it is really
/// working in. Under Z.ai's remap the CLI believes it runs a Claude-5 name
/// with a 1M window and never compacts; the endpoint then refuses the
/// request ("The model has reached its context window limit" — 6 of 7 GLM
/// attempts on 2026-08-22, zero compaction events). The native claude lane
/// must NOT carry the pin: its window is the model's own.
#[test]
fn glm_lane_pins_the_auto_compact_window_and_claude_lane_does_not() {
    run_owned_helper(
        "glm_lane_pins_the_auto_compact_window_and_claude_lane_does_not_owned_process",
        &[
            "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
            "CLAUDE_CODE_MAX_OUTPUT_TOKENS",
        ],
        |_| {},
    );
}

/// The claude-lane half below asserts the pin is ABSENT from the probe
/// child's environment. A probe child inherits this binary's environment,
/// and the fleet's GLM lane — which pins exactly these variables for its own
/// CLI — can be the terminal a suite run starts from; the inherited pin would
/// make the native lane look guilty of the GLM lane's pinning. The scrubbed
/// owned helper keeps the assertion about the driver, not about where the
/// operator launched the suite from.
#[test]
#[ignore = "run only in the process spawned by the parent test of the same name"]
fn glm_lane_pins_the_auto_compact_window_and_claude_lane_does_not_owned_process() {
    assert_owned_helper(
        "glm_lane_pins_the_auto_compact_window_and_claude_lane_does_not_owned_process",
    );
    let tmp = tempfile::tempdir().unwrap();
    let probe = tmp.path().join("env-probe");
    support::write_executable(
        &probe,
        "#!/bin/sh\nprintenv CLAUDE_CODE_AUTO_COMPACT_WINDOW > \"$PROBE_OUT\" 2>&1 || true\n\
         printenv CLAUDE_CODE_MAX_OUTPUT_TOKENS >> \"$PROBE_OUT\" 2>&1 || true\nexit 0\n",
    );
    let out = tmp.path().join("seen.txt");
    let ws = cosmix_foreman::executor::Workspace {
        dir: tmp.path().to_path_buf(),
        verify_subdir: None,
    };
    let budget = Budget::default();

    let glm = ClaudeDriver::glm("zk-test")
        .with_program(probe.to_str().unwrap())
        .with_env("PROBE_OUT", out.to_str().unwrap());
    let mut s = glm.start("p", &ws, &budget).unwrap();
    let _ = drain(&mut s);
    let seen = std::fs::read_to_string(&out).unwrap_or_default();
    assert_eq!(
        seen.trim(),
        format!(
            "{}\n{}",
            cosmix_foreman::driver::claude::ZAI_AUTO_COMPACT_WINDOW,
            cosmix_foreman::driver::claude::ZAI_MAX_OUTPUT_TOKENS
        ),
        "glm lane must pin CLAUDE_CODE_AUTO_COMPACT_WINDOW and \
         CLAUDE_CODE_MAX_OUTPUT_TOKENS for the agent; saw {seen:?}"
    );

    std::fs::write(&out, "").unwrap();
    let claude = ClaudeDriver::new()
        .with_program(probe.to_str().unwrap())
        .with_env("PROBE_OUT", out.to_str().unwrap());
    let mut s = claude.start("p", &ws, &budget).unwrap();
    let _ = drain(&mut s);
    let seen = std::fs::read_to_string(&out).unwrap_or_default();
    assert!(
        seen.trim().is_empty(),
        "native claude lane must not carry the GLM window pin; saw {seen:?}"
    );
}

/// THE RETRY LOOP. Every gate writes its verdict to `findings`, but for
/// most of this fleet's life the next agent was handed only the original
/// spec — so it rediscovered the wall or hit it again, and the arcs that
/// converged did so because the operator pasted the verdict into the spec
/// by hand. The prompt must carry the prior verdicts, or the loop is not a
/// loop.
#[test]
fn retry_prompt_carries_the_previous_verdicts() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let task = ledger
        .add_task("retry", "the original spec", "impl", "low", &[], "none")
        .unwrap();
    ledger
        .file_finding(
            Some(task),
            "major",
            "tier-0 red after agent run",
            "assertion failed: ledger_rejects_cyclic_deps (the-actual-reason)",
            "runner",
        )
        .unwrap();

    // The fake agent records the prompt it was given.
    let seen = tmp.path().join("prompt.txt");
    let probe = tmp.path().join("prompt-probe");
    support::write_executable(
        &probe,
        "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done > \"$PROMPT_OUT\"\nexit 0\n",
    );

    let driver = ClaudeDriver::new()
        .with_program(probe.to_str().unwrap())
        .with_env("PROMPT_OUT", seen.to_str().unwrap());
    let opts = RunOptions {
        workdir: tmp.path().to_path_buf(),
        stall_secs: 10,
        ..Default::default()
    };
    let _ = run_task(&ledger, task, &driver, &opts);

    let prompt = std::fs::read_to_string(&seen).unwrap_or_default();
    assert!(
        prompt.contains("the-actual-reason"),
        "the retry prompt must carry the prior verdict; got:\n{prompt}"
    );
    assert!(
        prompt.contains("the original spec"),
        "and must still carry the spec itself"
    );
}

/// A vendor API key in the environment silently outranks a subscription
/// login, converting an unattended fleet's whole day into a metered bill
/// with no signal in the ledger (the CLIs report list-price cost either
/// way). The scrub is the only thing standing between the operator and
/// that surprise, so pin it: the child must NOT see the key.
#[test]
fn agent_sessions_are_subscription_only_by_default() {
    run_owned_helper(
        "agent_sessions_are_subscription_only_by_default_owned_process",
        &[],
        |command| {
            command
                .env("ANTHROPIC_API_KEY", "sk-ant-must-not-reach-the-agent")
                .env("OPENAI_API_KEY", "sk-oai-must-not-reach-the-agent");
        },
    );
}

#[test]
#[ignore = "run only in the process spawned by the parent test of the same name"]
fn agent_sessions_are_subscription_only_by_default_owned_process() {
    assert_owned_helper("agent_sessions_are_subscription_only_by_default_owned_process");
    let tmp = tempfile::tempdir().unwrap();
    let probe = tmp.path().join("env-probe");
    support::write_executable(
        &probe,
        "#!/bin/sh\nprintenv ANTHROPIC_API_KEY > \"$PROBE_OUT\" 2>&1 || true\n\
         printenv OPENAI_API_KEY >> \"$PROBE_OUT\" 2>&1 || true\nexit 0\n",
    );
    let out = tmp.path().join("seen.txt");
    let ws = cosmix_foreman::executor::Workspace {
        dir: tmp.path().to_path_buf(),
        verify_subdir: None,
    };
    let budget = Budget::default();
    let claude = ClaudeDriver::new()
        .with_program(probe.to_str().unwrap())
        .with_env("PROBE_OUT", out.to_str().unwrap());
    let mut s = claude.start("p", &ws, &budget).unwrap();
    let _ = drain(&mut s);
    let seen = std::fs::read_to_string(&out).unwrap_or_default();
    assert!(
        !seen.contains("must-not-reach-the-agent"),
        "claude lane leaked an API key to the agent: {seen}"
    );

    std::fs::write(&out, "").unwrap();
    let codex = cosmix_foreman::driver::codex::CodexDriver::new()
        .with_program(probe.to_str().unwrap())
        .with_env("PROBE_OUT", out.to_str().unwrap());
    let mut s = codex.start("p", &ws, &budget).unwrap();
    let _ = drain(&mut s);
    let seen = std::fs::read_to_string(&out).unwrap_or_default();
    assert!(
        !seen.contains("must-not-reach-the-agent"),
        "codex lane leaked an API key to the agent: {seen}"
    );
}

#[test]
fn verify_subdir_moves_the_tier0_verifier() {
    // cos-shaped repo: the buildable workspace lives in src/, the repo root
    // has no Cargo.toml. Without verify_subdir the rust profile is red at
    // the root no matter how good the agent's work is (this exact shape
    // bounced the live fleet's first three green runs); with it, tier-0
    // runs where the workspace actually is.
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("src/src")).unwrap();
    std::fs::write(
        tmp.path().join("src/Cargo.toml"),
        "[package]\nname = \"mini\"\nversion = \"0.0.1\"\nedition = \"2021\"\n",
    )
    .unwrap();
    // Non-empty: `cargo fmt --check` rejects a zero-byte lib.rs.
    std::fs::write(tmp.path().join("src/src/lib.rs"), "// mini\n").unwrap();

    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let mk_driver = || {
        ClaudeDriver::new()
            .with_program(fixtures().agent.to_str().unwrap())
            .with_env("FAKE_STREAM", fixture("claude-ok.jsonl"))
    };

    let rootward = ledger
        .add_task("no-subdir", "spec", "impl", "low", &[], "rust")
        .unwrap();
    let opts = RunOptions {
        workdir: tmp.path().to_path_buf(),
        stall_secs: 10,
        ..Default::default()
    };
    let report = run_task(&ledger, rootward, &mk_driver(), &opts).unwrap();
    assert_eq!(
        report.task_status, "bounced",
        "tier-0 at a Cargo.toml-less root must be red"
    );

    let subbed = ledger
        .add_task("subdir", "spec", "impl", "low", &[], "rust")
        .unwrap();
    let opts = RunOptions {
        workdir: tmp.path().to_path_buf(),
        stall_secs: 10,
        verify_subdir: Some("src".into()),
        ..Default::default()
    };
    let report = run_task(&ledger, subbed, &mk_driver(), &opts).unwrap();
    let verification = ledger.verification_reports(subbed, 1).unwrap();
    assert_eq!(
        report.task_status, "done",
        "verify_subdir must move tier-0 into the workspace: {verification:?}"
    );

    // Laundering: replace the subdir with a symlink to an unrelated green
    // workspace OUTSIDE the worktree. The contract checks see a clean tree;
    // containment must refuse the verify dir and fail the task.
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(outside.path().join("green/src")).unwrap();
    std::fs::write(
        outside.path().join("green/Cargo.toml"),
        "[package]\nname = \"green\"\nversion = \"0.0.1\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(outside.path().join("green/src/lib.rs"), "// green\n").unwrap();
    std::fs::remove_dir_all(tmp.path().join("src")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("green"), tmp.path().join("src")).unwrap();

    let laundered = ledger
        .add_task("laundered", "spec", "impl", "low", &[], "rust")
        .unwrap();
    let opts = RunOptions {
        workdir: tmp.path().to_path_buf(),
        stall_secs: 10,
        verify_subdir: Some("src".into()),
        ..Default::default()
    };
    let report = run_task(&ledger, laundered, &mk_driver(), &opts).unwrap();
    assert_eq!(
        report.task_status, "failed",
        "a subdir symlink escaping the worktree must be refused, not verified"
    );
}

#[test]
fn post_run_infra_refusals_accumulate_across_claims_until_a_noninfra_disposition() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let task = ledger
        .add_task("infra-test", "spec", "impl", "low", &[], "none")
        .unwrap();
    let err = anyhow::anyhow!("worktree provisioning refused");

    assert_eq!(
        ledger
            .note_infra_refusal(task, &err, 3, 10)
            .unwrap()
            .unwrap()
            .count,
        1
    );

    for expected in 2..=3 {
        let (claimed, run) = ledger
            .start_attempt(task, "test-claimant", None, None, "claude", None)
            .unwrap();
        assert_eq!(claimed.infra_refusals, expected - 1);
        ledger
            .finish_task_classified(
                task,
                ClaimToken {
                    owner: "test-claimant",
                    generation: claimed.attempt,
                },
                run,
                "failed",
                Some(FindingReason::InfraRefusal),
            )
            .unwrap();
    }
    assert_eq!(ledger.task(task).unwrap().unwrap().infra_refusals, 3);
    assert!(
        !ledger.open_findings(10).unwrap().is_empty(),
        "the third consecutive refusal must file the threshold finding"
    );

    let (claimed, run) = ledger
        .start_attempt(task, "test-claimant", None, None, "claude", None)
        .unwrap();
    ledger
        .finish_task_classified(
            task,
            ClaimToken {
                owner: "test-claimant",
                generation: claimed.attempt,
            },
            run,
            "done",
            None,
        )
        .unwrap();
    let completed = ledger.task(task).unwrap().unwrap();
    assert_eq!(completed.infra_refusals, 0);
    assert_eq!(completed.dispatch_after, None);
}

#[test]
fn repeated_worktree_failure_files_exactly_one_finding() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let task = ledger
        .add_task("infra-finding", "spec", "impl", "low", &[], "none")
        .unwrap();

    // A real git clone with a squatting task-N directory forces the same
    // pre-claim worktree refusal that dispatch receives from launch().
    let repo = tmp.path().join("integration");
    std::fs::create_dir(&repo).unwrap();
    for args in [
        &["init", "-b", "main"][..],
        &["config", "user.name", "test"][..],
        &["config", "user.email", "test@example.com"][..],
    ] {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );
    }
    std::fs::write(repo.join("base.txt"), "base\n").unwrap();
    for args in [&["add", "."][..], &["commit", "-m", "base"][..]] {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );
    }
    std::fs::create_dir(tmp.path().join(format!("task-{task}"))).unwrap();

    // Threshold passed explicitly — the production knob is process-global, and
    // a test that read it would change answer for an operator who exports it.
    let threshold = 3;
    for attempt in 1..=5 {
        let error = cosmix_foreman::refinery::ensure_task_worktree(
            &repo,
            task,
            &format!("task/{task}"),
            None,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("not this clone's worktree"),
            "test must force worktree provisioning to refuse: {error:#}"
        );
        assert_eq!(
            ledger
                .note_infra_refusal(task, &error, threshold, 10)
                .unwrap()
                .unwrap()
                .count,
            attempt
        );
        assert_eq!(
            ledger.open_findings(10).unwrap().len(),
            usize::from(attempt >= threshold),
            "the third refusal files once and later refusals do not duplicate it"
        );
    }

    let findings = ledger.open_findings(10).unwrap();
    assert_eq!(findings.len(), 1);
    let (_, finding_task, severity, title, body) = &findings[0];
    assert_eq!(*finding_task, Some(task));
    assert_eq!(severity, "major");
    assert!(title.contains(&format!("task {task}: infra-finding")));
    assert!(body.contains("Last error:"));
    assert!(body.contains("not this clone's worktree"));
}

#[test]
fn consecutive_infra_refusals_park_without_charging_the_ladder() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let task = ledger
        .add_task("broken environment", "spec", "impl", "low", &[], "none")
        .unwrap();
    let refusal = "task worktree exists but belongs to another clone; remove it before redispatch";
    let error = anyhow::anyhow!(refusal);

    for count in 1..=4 {
        let disposition = ledger
            .note_infra_refusal(task, &error, 3, 4)
            .unwrap()
            .unwrap();
        assert_eq!(disposition.count, count);
        assert_eq!(disposition.parked, count == 4);
    }

    let parked = ledger.task(task).unwrap().unwrap();
    assert_eq!(parked.status, "parked");
    assert_eq!(parked.infra_refusals, 4);
    assert_eq!(parked.ladder_failures, 0);
    assert_eq!(parked.dispatch_after, None);

    let findings = ledger.open_findings(10).unwrap();
    assert_eq!(findings.len(), 1);
    let (_, finding_task, severity, _, body) = &findings[0];
    assert_eq!(*finding_task, Some(task));
    assert_eq!(severity, "blocker");
    assert!(body.ends_with(refusal), "{body}");

    ledger.requeue_task(task, false).unwrap();
    let requeued = ledger.task(task).unwrap().unwrap();
    assert_eq!(requeued.status, "queued");
    assert_eq!(requeued.infra_refusals, 0);
    assert_eq!(requeued.dispatch_after, None);
    assert!(ledger.open_findings(10).unwrap().is_empty());
}

#[test]
fn one_infra_refusal_then_success_resets_the_sequence() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let task = ledger
        .add_task("transient environment", "spec", "impl", "low", &[], "none")
        .unwrap();
    let refusal = anyhow::anyhow!("temporary worktree provisioning outage");
    let disposition = ledger
        .note_infra_refusal(task, &refusal, 3, 4)
        .unwrap()
        .unwrap();
    assert_eq!(disposition.count, 1);
    assert!(!disposition.parked);

    let (claimed, run) = ledger
        .start_attempt(task, "successful-dispatch", None, None, "claude", None)
        .unwrap();
    assert!(
        !ledger
            .finish_task_classified(
                task,
                ClaimToken {
                    owner: "successful-dispatch",
                    generation: claimed.attempt,
                },
                run,
                "done",
                None,
            )
            .unwrap(),
        "a successful dispatch must not charge the ladder"
    );

    let completed = ledger.task(task).unwrap().unwrap();
    assert_eq!(completed.status, "done");
    assert_eq!(completed.infra_refusals, 0);
    assert_eq!(completed.dispatch_after, None);
    assert_eq!(completed.ladder_failures, 0);
}

#[test]
fn infra_refusal_returns_none_when_task_moved_on() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let task = ledger
        .add_task("moved-task", "spec", "impl", "low", &[], "none")
        .unwrap();

    // Claimed, so no longer dispatchable: a refusal racing the claim must not
    // resurrect a counter the claim just cleared.
    ledger.claim_task(task, "test").unwrap();

    let err = anyhow::anyhow!("worktree creation failed");
    assert_eq!(ledger.note_infra_refusal(task, &err, 3, 10).unwrap(), None);
    assert!(ledger.open_findings(10).unwrap().is_empty());
}

#[test]
fn infra_refusals_finding_knob_is_validated() {
    use cosmix_foreman::ledger::{
        infra_refusal_finding_threshold, infra_refusal_park_threshold,
        parse_infra_refusals_finding, parse_infra_refusals_park,
    };

    assert_eq!(parse_infra_refusals_finding("5").unwrap(), 5);
    assert_eq!(parse_infra_refusals_finding(" 7 ").unwrap(), 7);
    for bad in ["0", "-1", "", "three", "3.5"] {
        assert!(
            parse_infra_refusals_finding(bad).is_err(),
            "{bad:?} must be an error, not a silent default"
        );
        assert!(
            parse_infra_refusals_park(bad).is_err(),
            "{bad:?} must be an error, not a silent default"
        );
    }
    // Only meaningful when the operator has not exported the knob.
    if std::env::var_os("FOREMAN_INFRA_REFUSALS_FINDING").is_none() {
        assert_eq!(infra_refusal_finding_threshold().unwrap(), 3);
    }
    if std::env::var_os("FOREMAN_INFRA_REFUSALS_PARK").is_none() {
        assert_eq!(infra_refusal_park_threshold().unwrap(), 10);
    }
}
