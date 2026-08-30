//! Task 24: dispatch and refine exit codes distinguish sweep outcome from
//! harness health. Bounces, parks, and rung refusals are normal business and
//! exit 0; genuine harness faults (bad env, ledger errors) exit non-zero.

use std::path::Path;
use std::process::Command;

use cosmix_foreman::ledger::Ledger;

mod support;

fn foreman(db: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_foreman"));
    command
        .arg("--db")
        .arg(db)
        .env(
            "FOREMAN_VERIFY_LANE",
            db.parent().unwrap().join("verify.lock"),
        )
        .env("FOREMAN_VERIFY_LANE_WAIT_SECS", "30");
    command
}

/// A genuine harness fault (unparseable FOREMAN_LADDER) exits non-zero.
#[test]
fn dispatch_bad_ladder_env_exits_nonzero() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");

    // Initialise the ledger.
    let out = foreman(&db).arg("init").output().expect("foreman init");
    assert!(out.status.success(), "init must succeed: {:?}", out);

    // Add a task.
    let out = foreman(&db)
        .args(["task", "add", "test task", "--spec", "{}"])
        .output()
        .expect("adding task");
    assert!(out.status.success(), "task add must succeed: {:?}", out);

    // Dispatch with a malformed ladder env.
    let out = foreman(&db)
        .args(["dispatch", "--max-tasks", "1"])
        .env("FOREMAN_LADDER", "invalid,,,,rung")
        .output()
        .expect("dispatch command");

    assert!(
        !out.status.success(),
        "malformed FOREMAN_LADDER must cause non-zero exit"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("FOREMAN_LADDER") || stderr.contains("unknown agent"),
        "stderr should mention the ladder problem: {stderr}"
    );
}

/// Refine with no landable tasks exits 0 — no work to do is a normal outcome,
/// not a harness fault.
#[test]
fn refine_no_landable_tasks_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let db = tmp.path().join("ledger.db");

    // Initialise a git repo with an initial commit.
    Command::new("git")
        .args(["init", repo.to_str().unwrap()])
        .status()
        .expect("git init");
    Command::new("git")
        .args([
            "-C",
            repo.to_str().unwrap(),
            "config",
            "user.email",
            "test@example.com",
        ])
        .status()
        .expect("git config");
    Command::new("git")
        .args(["-C", repo.to_str().unwrap(), "config", "user.name", "Test"])
        .status()
        .expect("git config");
    let dummy_file = repo.join("README.md");
    std::fs::write(&dummy_file, "# test").expect("write dummy file");
    Command::new("git")
        .args(["-C", repo.to_str().unwrap(), "add", "README.md"])
        .status()
        .expect("git add");
    Command::new("git")
        .args(["-C", repo.to_str().unwrap(), "commit", "-m", "initial"])
        .status()
        .expect("git commit");

    // Initialise the ledger.
    let out = foreman(&db).arg("init").output().expect("foreman init");
    assert!(out.status.success(), "init must succeed: {:?}", out);

    // Refine with no landable tasks must exit 0.
    let out = foreman(&db)
        .args([
            "refine",
            "--repo",
            repo.to_str().unwrap(),
            "--subdir",
            ".",
            "--tier",
            "1",
        ])
        .output()
        .expect("refine command");

    assert!(
        out.status.success(),
        "refine with no landable tasks must exit 0: {:?}",
        out
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("0/0") || stdout.contains("landed"),
        "stdout should report landing results: {stdout}"
    );
}

/// A genuine harness fault during refine (bad repo path) exits non-zero.
#[test]
fn refine_bad_repo_exits_nonzero() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");

    // Initialise the ledger.
    let out = foreman(&db).arg("init").output().expect("foreman init");
    assert!(out.status.success(), "init must succeed: {:?}", out);

    // Refine with a non-existent repo path.
    let out = foreman(&db)
        .args([
            "refine",
            "--repo",
            "/nonexistent/path/that/does/not/exist",
            "--subdir",
            ".",
            "--tier",
            "1",
        ])
        .output()
        .expect("refine command");

    assert!(
        !out.status.success(),
        "bad repo path must cause non-zero exit"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("repo") || stderr.contains("not found") || stderr.contains("No such file"),
        "stderr should mention the repo problem: {stderr}"
    );
}

/// A dispatch sweep whose task bounces exits 0 — a bounce is a task outcome,
/// not a harness fault.
#[test]
fn dispatch_task_bounce_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let workdir = tmp.path().join("workdir");
    std::fs::create_dir_all(&workdir).unwrap();

    // Initialise the ledger.
    let out = foreman(&db).arg("init").output().expect("foreman init");
    assert!(out.status.success(), "init must succeed: {:?}", out);

    // Add a task.
    let out = foreman(&db)
        .args(["task", "add", "bounce test", "--spec", "{}"])
        .output()
        .expect("adding task");
    assert!(out.status.success(), "task add must succeed: {:?}", out);

    // Set up a fake codex binary that bounces (exit 2, which codex uses for task failures).
    let fake_codex = tmp.path().join("fake-codex");
    support::write_executable(
        &fake_codex,
        "#!/bin/sh\necho '{\"type\":\"turn.failed\",\"error\":{\"message\":\"task failed\"}}'\nexit 2\n",
    );

    // Dispatch with a ladder that uses codex — the task will bounce.
    let out = foreman(&db)
        .args([
            "dispatch",
            "--max-tasks",
            "1",
            "--workdir",
            workdir.to_str().unwrap(),
        ])
        .env("FOREMAN_LADDER", "codex")
        .env("FOREMAN_CODEX_BIN", fake_codex.to_str().unwrap())
        .output()
        .expect("dispatch command");

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "dispatch with a bouncing task must exit 0: stdout={stdout}, stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("bounced 1") || stdout.contains("bounce"),
        "stdout should report a bounce: {stdout}"
    );
    assert!(
        stdout.contains("sweep complete"),
        "stdout should include the sweep summary: {stdout}"
    );
}

#[test]
fn infrastructure_refusal_frees_the_sweep_slot_for_later_work() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
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

    let ledger = Ledger::open(&db).unwrap();
    let first = ledger
        .add_task("squatted worktree", "spec", "impl", "low", &[], "none")
        .unwrap();
    let second = ledger
        .add_task("runnable task", "spec", "impl", "low", &[], "none")
        .unwrap();
    std::fs::create_dir(tmp.path().join(format!("task-{first}"))).unwrap();

    let fake_codex = tmp.path().join("fake-codex");
    support::write_executable(
        &fake_codex,
        "#!/bin/sh\n\
         echo '{\"type\":\"thread.started\",\"thread_id\":\"slot-test\"}'\n\
         echo '{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}'\n",
    );

    let out = foreman(&db)
        .args([
            "dispatch",
            "--max-tasks",
            "1",
            "--workdir",
            repo.to_str().unwrap(),
            "--branch-template",
            "task/{id}",
            "--no-verify",
        ])
        .env("FOREMAN_LADDER", "codex")
        .env("FOREMAN_CODEX_BIN", fake_codex.to_str().unwrap())
        .output()
        .expect("dispatch command");

    assert!(
        !out.status.success(),
        "the recorded infrastructure fault still makes this sweep unhealthy"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&format!("task {first} refused")),
        "{stderr}"
    );
    assert!(
        stdout.contains(&format!("dispatch: task {second} ")),
        "the later task did not use the freed slot: {stdout}"
    );
    assert_eq!(
        stdout.matches(&format!("dispatch: task {second} ")).count(),
        1,
        "the later task should run once: {stdout}"
    );

    let ledger = Ledger::open(&db).unwrap();
    let refused = ledger.task(first).unwrap().unwrap();
    assert_eq!(refused.infra_refusals, 1);
    assert!(refused.dispatch_after.is_some());
    assert_eq!(refused.ladder_failures, 0);
    assert_eq!(ledger.task(second).unwrap().unwrap().status, "done");
}

/// Refine with a task that fails pre-land verification exits 0 — a verification
/// failure is a task outcome, not a harness fault.
#[test]
fn refine_verify_failure_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let db = tmp.path().join("ledger.db");

    // Initialise a git repo with an initial commit.
    Command::new("git")
        .args(["init", repo.to_str().unwrap()])
        .status()
        .expect("git init");
    Command::new("git")
        .args([
            "-C",
            repo.to_str().unwrap(),
            "config",
            "user.email",
            "test@example.com",
        ])
        .status()
        .expect("git config");
    Command::new("git")
        .args(["-C", repo.to_str().unwrap(), "config", "user.name", "Test"])
        .status()
        .expect("git config");
    let dummy_file = repo.join("README.md");
    std::fs::write(&dummy_file, "# test\n").expect("write dummy file");
    Command::new("git")
        .args(["-C", repo.to_str().unwrap(), "add", "README.md"])
        .status()
        .expect("git add");
    Command::new("git")
        .args(["-C", repo.to_str().unwrap(), "commit", "-m", "initial"])
        .status()
        .expect("git commit");

    // Initialise the ledger.
    let out = foreman(&db).arg("init").output().expect("foreman init");
    assert!(out.status.success(), "init must succeed: {:?}", out);

    // Add a task and mark it done with a branch (simulating a completed run).
    let out = foreman(&db)
        .args(["task", "add", "verify fail test", "--spec", "{}"])
        .output()
        .expect("adding task");
    assert!(out.status.success(), "task add must succeed: {:?}", out);

    // Create a branch for the task in the repo that will fail verification.
    Command::new("git")
        .args(["-C", repo.to_str().unwrap(), "checkout", "-b", "task/1"])
        .status()
        .expect("git checkout");
    // Make a change that will fail cargo fmt --check.
    std::fs::write(&dummy_file, "bad    formatting\n").expect("write bad file");
    Command::new("git")
        .args(["-C", repo.to_str().unwrap(), "add", "README.md"])
        .status()
        .expect("git add");
    Command::new("git")
        .args(["-C", repo.to_str().unwrap(), "commit", "-m", "bad fmt"])
        .status()
        .expect("git commit");
    Command::new("git")
        .args(["-C", repo.to_str().unwrap(), "checkout", "main"])
        .status()
        .expect("git checkout");

    // Mark the task as done with the branch using SQL (simulating a successful run).
    let sqlite = Command::new("sqlite3")
        .arg(&db)
        .arg("UPDATE tasks SET status = 'done', branch = 'task/1' WHERE id = 1;")
        .output()
        .expect("sqlite3 update");
    assert!(
        sqlite.status.success(),
        "task update must succeed: {:?}",
        sqlite
    );

    // Refine — the pre-land verification will fail, but the process must exit 0.
    let out = foreman(&db)
        .args([
            "refine",
            "--repo",
            repo.to_str().unwrap(),
            "--subdir",
            ".",
            "--tier",
            "1",
        ])
        .output()
        .expect("refine command");

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "refine with verification failure must exit 0: stdout={stdout}, stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("0/1") || stdout.contains("landed"),
        "stdout should report landing results: {stdout}"
    );
}

fn sqlite(db: &Path, sql: &str) -> String {
    Command::new("sqlite3")
        .arg(db)
        .arg(sql)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .expect("running sqlite3")
}

/// Task 94: a dispatch supervisor that dies mid-run leaves its claim
/// `running` forever with no reaper to notice — the phantom-claim gap. This
/// proves the whole pipeline end-to-end against a REAL claim holder: a
/// genuine `foreman dispatch` process claims the task through the
/// production claim path (so the claim's `claim_pid` is that process's own,
/// trusted pid — never forged), is SIGKILLed while the claim is still
/// `running` — no signal handler, no cleanup, no chance to release it,
/// exactly like a crashed supervisor — and the very next `dispatch` sweep
/// must reap it: requeue it, file a finding naming the dead claimant, and
/// leave its ladder position untouched, since the task did nothing wrong.
#[test]
fn dispatch_reaps_a_phantom_claim_left_by_a_killed_supervisor_and_does_not_charge_the_ladder() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let workdir = tmp.path().join("workdir");
    std::fs::create_dir_all(&workdir).unwrap();

    let out = foreman(&db).arg("init").output().expect("foreman init");
    assert!(out.status.success(), "init must succeed: {:?}", out);

    let out = foreman(&db)
        .args(["task", "add", "phantom claim", "--spec", "{}"])
        .output()
        .expect("adding task");
    assert!(out.status.success(), "task add must succeed: {:?}", out);

    // A fake codex vendor binary that hangs once spawned — standing in for
    // an agent turn genuinely in progress when the supervisor dies, so this
    // test has a window to kill `foreman dispatch` while the claim is
    // `running`.
    let fake_codex = tmp.path().join("fake-codex");
    support::write_executable(&fake_codex, "#!/bin/sh\nsleep 60\n");

    let mut supervisor = foreman(&db)
        .args([
            "dispatch",
            "--max-tasks",
            "1",
            "--workdir",
            workdir.to_str().unwrap(),
        ])
        .env("FOREMAN_LADDER", "codex")
        .env("FOREMAN_CODEX_BIN", &fake_codex)
        .spawn()
        .expect("spawning dispatch supervisor");
    let supervisor_pid = supervisor.id();

    // Poll for the claim: the production claim path commits `running` +
    // `claimed_by` before the (hung) vendor process is even spawned.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let claimant = loop {
        assert!(
            std::time::Instant::now() < deadline,
            "dispatch never claimed the task before the deadline"
        );
        let status = sqlite(&db, "SELECT status FROM tasks WHERE id = 1;");
        let claimed_by = sqlite(
            &db,
            "SELECT COALESCE(claimed_by, '') FROM tasks WHERE id = 1;",
        );
        if status == "running" && !claimed_by.is_empty() {
            break claimed_by;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert_eq!(
        claimant,
        format!("codex@{supervisor_pid}"),
        "the claim must be held by the real dispatch process's own pid"
    );

    // Kill it exactly like a crashed supervisor: SIGKILL, no cleanup, no
    // chance to release the claim.
    supervisor.kill().expect("killing the dispatch supervisor");
    supervisor.wait().expect("reaping the killed supervisor"); // fully gone: pid provably dead

    // The claim's lease (several hours, CLAIM_LEASE_SECS) has not naturally
    // expired yet — back-date only the lease, the one field the ledger's own
    // API deliberately provides no way to forge (see ledger.rs's
    // `backdate_lease` test helper), never the claim identity or status.
    let backdated = sqlite(
        &db,
        "UPDATE tasks SET lease_until = '2000-01-01T00:00:00+00:00' WHERE id = 1; \
         SELECT changes();",
    );
    assert_eq!(
        backdated, "1",
        "backdating the lease must touch exactly the one task row"
    );

    // --max-tasks 0 dispatches nothing — it isolates the reap sweep, which
    // runs unconditionally before the dispatch loop, from any further
    // routing this task might otherwise pick up in the same invocation.
    let out = foreman(&db)
        .args(["dispatch", "--max-tasks", "0"])
        .output()
        .expect("dispatch command");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "reaping a dead claim must exit 0: stdout={stdout}, stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("reaped a dead claim"),
        "stdout should report the reap: {stdout}"
    );

    assert_eq!(
        sqlite(&db, "SELECT status FROM tasks WHERE id = 1;"),
        "queued",
        "the reaped task must be requeued"
    );
    assert_eq!(
        sqlite(
            &db,
            "SELECT COALESCE(claimed_by, 'NULL') FROM tasks WHERE id = 1;"
        ),
        "NULL",
        "the dead claim must be cleared"
    );
    assert_eq!(
        sqlite(&db, "SELECT ladder_failures FROM tasks WHERE id = 1;"),
        "0",
        "a reaped claim must never charge the task's ladder position"
    );

    let finding = sqlite(
        &db,
        "SELECT body FROM findings WHERE task_id = 1 AND reason_code = 'dead_claim_reaped';",
    );
    assert!(
        finding.contains(&claimant),
        "the finding should name the dead claimant `{claimant}`: {finding}"
    );
    // The reap's evidence, recorded where an operator reading the ledger
    // will find it: which pid was observed gone, and how long the claim had
    // been held (its AGE — the lease overdue time alone hides the whole
    // lease window, which here is six hours).
    assert!(
        finding.contains(&format!("pid {supervisor_pid}")),
        "the finding should name the pid observed absent: {finding}"
    );
    assert!(
        finding.contains("held for "),
        "the finding should report the claim's age, recorded at claim time \
         by the production claim path: {finding}"
    );
    assert!(
        finding.contains("observed absent at"),
        "the finding should record the liveness observation the reap acted \
         on, not just its conclusion: {finding}"
    );
}

/// The reaper's own write failing must NOT read as a green sweep. A claim
/// the sweep proved dead but could not release is still the phantom
/// `running` task this whole fix exists to end, so `dispatch` must report
/// it and exit non-zero — a harness fault, the same rule as every other
/// ledger fault in the sweep — while leaving the claim exactly as it found
/// it: still claimed, no finding saying it was released. Once the fault
/// clears, the next sweep reaps it normally. The fault is a SQLite trigger
/// that aborts the reap's requeue write: a real storage-layer failure hit
/// by a real `foreman dispatch` process, not a hook inside the binary.
#[test]
fn dispatch_exits_nonzero_when_a_dead_claim_cannot_be_reaped_and_leaves_it_claimed() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");

    let out = foreman(&db).arg("init").output().expect("foreman init");
    assert!(out.status.success(), "init must succeed: {:?}", out);
    let out = foreman(&db)
        .args(["task", "add", "unreapable phantom", "--spec", "{}"])
        .output()
        .expect("adding task");
    assert!(out.status.success(), "task add must succeed: {:?}", out);

    // A provably dead pid: spawned, exited, fully waited.
    let mut child = Command::new("true").spawn().expect("spawning `true`");
    let dead_pid = child.id();
    child.wait().expect("waiting for `true`");

    // A dead claim in exactly the shape the production claim path leaves
    // behind when its supervisor dies: `running`, a `kind@pid` claimant, the
    // trusted `claim_pid` column set, the lease long expired.
    let claimant = format!("codex@{dead_pid}");
    let planted = sqlite(
        &db,
        &format!(
            "UPDATE tasks SET status = 'running', claimed_by = '{claimant}', \
             claim_pid = {dead_pid}, attempt = 1, \
             lease_until = '2000-01-01T00:00:00+00:00', \
             claimed_at = '1999-12-31T18:00:00+00:00' WHERE id = 1; \
             SELECT changes();"
        ),
    );
    assert_eq!(planted, "1", "planting the dead claim must touch one row");

    // The injected fault: the requeue write — the first statement of the
    // reap's transaction — is refused by the storage layer.
    sqlite(
        &db,
        "CREATE TRIGGER inject_reap_fault BEFORE UPDATE OF status ON tasks \
         WHEN OLD.status = 'running' AND NEW.status = 'queued' \
         BEGIN SELECT RAISE(ABORT, 'injected storage fault'); END;",
    );

    let out = foreman(&db)
        .args(["dispatch", "--max-tasks", "0"])
        .output()
        .expect("dispatch command");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "a dead claim the sweep could not release is a harness fault and must \
         exit non-zero: stdout={stdout}, stderr={stderr}"
    );
    assert!(
        stderr.contains("could not be reaped") && stderr.contains("injected storage fault"),
        "stderr must name the stranded claim and the write's own error: {stderr}"
    );
    assert!(
        !stdout.contains("reaped a dead claim"),
        "a reap that did not happen must not be reported as one: {stdout}"
    );
    // Left exactly as found — not half-reaped, and no finding claiming a
    // release that never committed.
    assert_eq!(
        sqlite(&db, "SELECT status FROM tasks WHERE id = 1;"),
        "running"
    );
    assert_eq!(
        sqlite(&db, "SELECT claimed_by FROM tasks WHERE id = 1;"),
        claimant
    );
    assert_eq!(
        sqlite(
            &db,
            "SELECT COUNT(*) FROM findings WHERE task_id = 1 AND reason_code = 'dead_claim_reaped';"
        ),
        "0"
    );

    // The fault clears; it is still expired and still dead, so the next
    // sweep reaps it — and THAT sweep is healthy.
    sqlite(&db, "DROP TRIGGER inject_reap_fault;");
    let out = foreman(&db)
        .args(["dispatch", "--max-tasks", "0"])
        .output()
        .expect("dispatch command");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "once the write goes through the sweep is healthy: stdout={stdout}, stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("reaped a dead claim"), "{stdout}");
    assert_eq!(
        sqlite(&db, "SELECT status FROM tasks WHERE id = 1;"),
        "queued"
    );
    assert_eq!(
        sqlite(
            &db,
            "SELECT COUNT(*) FROM findings WHERE task_id = 1 AND reason_code = 'dead_claim_reaped';"
        ),
        "1"
    );
    assert_eq!(
        sqlite(&db, "SELECT ladder_failures FROM tasks WHERE id = 1;"),
        "0",
        "neither the failed sweep nor the successful one may charge the ladder"
    );
}
