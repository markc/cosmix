//! The timer-facing `gc-scratch` surface through the real binary.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use cosmix_foreman::ledger::{Ledger, TaskControls};

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn repository(fleet: &Path) -> PathBuf {
    let repo = fleet.join("workdir");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["config", "user.name", "test"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    std::fs::write(repo.join(".gitignore"), "target/\n").unwrap();
    std::fs::write(repo.join("tracked"), "base\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "base"]);
    repo
}

fn terminal_fixture(fleet: &Path, status: &str) -> (PathBuf, PathBuf, i64, Vec<PathBuf>) {
    let repo = repository(fleet);
    let db = fleet.join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let id = ledger
        .add_task_scoped(
            "scratch CLI",
            "spec",
            "impl",
            "low",
            &[],
            TaskControls {
                verifier_profile: "none",
                crates: &[],
                operator_driven_reason: None,
            },
        )
        .unwrap();
    let branch = format!("task/{id}");
    git(&repo, &["branch", &branch]);
    let worktree = fleet.join(format!("task-{id}"));
    git(
        &repo,
        &["worktree", "add", worktree.to_str().unwrap(), &branch],
    );
    ledger
        .start_attempt(
            id,
            "fixture",
            Some(worktree.to_str().unwrap()),
            Some(&branch),
            "codex",
            None,
        )
        .unwrap();
    ledger.finish_task(id, "fixture", "done").unwrap();
    ledger.set_task_status(id, status).unwrap();

    let targets = vec![
        worktree.join("src/target"),
        fleet.join(format!("task-{id}-target")),
    ];
    for target in &targets {
        std::fs::create_dir_all(target).unwrap();
        std::fs::write(target.join("artifact"), vec![1_u8; 8192]).unwrap();
    }
    (repo, db, id, targets)
}

fn foreman(db: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_foreman"))
        .arg("--db")
        .arg(db)
        .args(args)
        .output()
        .expect("spawning foreman")
}

#[test]
fn dry_run_and_real_sweep_report_allocated_before_after() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, db, id, targets) = terminal_fixture(tmp.path(), "landed");
    for cache in ["target", "target-refine"] {
        let deps = tmp.path().join(cache).join("debug/deps");
        std::fs::create_dir_all(&deps).unwrap();
        std::fs::write(deps.join("hot-cache-entry"), vec![2_u8; 8192]).unwrap();
    }
    let fleet = tmp.path().to_str().unwrap();
    let repo_arg = repo.to_str().unwrap();
    let common = [
        "gc-scratch",
        "--fleet-dir",
        fleet,
        "--repo",
        repo_arg,
        "--terminal-age-hours",
        "0",
        "--as-of",
        "2030-01-02T03:04:05Z",
    ];

    let mut dry_args = common.to_vec();
    dry_args.push("--dry-run");
    let dry = foreman(&db, &dry_args);
    let dry_stdout = String::from_utf8_lossy(&dry.stdout);
    assert!(
        dry.status.success(),
        "{}",
        String::from_utf8_lossy(&dry.stderr)
    );
    assert!(dry_stdout.contains(" -> "), "{dry_stdout}");
    assert!(dry_stdout.contains("dry-run"), "{dry_stdout}");
    assert!(
        dry_stdout.contains("replay with --as-of 2030-01-02T03:04:05+00:00"),
        "{dry_stdout}"
    );
    assert!(
        dry_stdout.contains("toward the 160 GiB cap"),
        "default shared-cache bound must run without an opt-in: {dry_stdout}"
    );
    assert!(dry_stdout.contains("sweep total:"), "{dry_stdout}");
    assert!(targets.iter().all(|target| target.is_dir()));

    let mut real_args = common.to_vec();
    real_args.push("--confirm");
    let real = foreman(&db, &real_args);
    let real_stdout = String::from_utf8_lossy(&real.stdout);
    assert!(
        real.status.success(),
        "{}",
        String::from_utf8_lossy(&real.stderr)
    );
    assert!(real_stdout.contains(" -> 0 B"), "{real_stdout}");
    assert!(real_stdout.contains("reclaimed"), "{real_stdout}");
    assert!(targets.iter().all(|target| !target.exists()));
    assert!(
        ["target", "target-refine"]
            .iter()
            .all(|cache| tmp.path().join(cache).is_dir()),
        "under-cap shared caches remain warm"
    );
    assert!(tmp.path().join(format!("task-{id}")).is_dir());
    assert!(
        tmp.path().join(format!("task-{id}/tracked")).is_file(),
        "tracked file survives"
    );
    let branch_name = format!("task/{id}");
    let branch = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["branch", "--list", branch_name.as_str()])
        .output()
        .unwrap();
    assert!(
        !branch.stdout.is_empty(),
        "scratch sweep must retain the task branch"
    );
}

#[test]
fn periodic_backstop_assets_invoke_the_native_sweep() {
    let service = include_str!("../../../_etc/systemd-user/foreman-gc-scratch.service");
    let timer = include_str!("../../../_etc/systemd-user/foreman-gc-scratch.timer");
    let environment = include_str!("../../../_etc/cosmix/foreman-gc-scratch.env.example");

    assert!(service.contains("EnvironmentFile=%h/.config/cosmix/foreman-gc-scratch.env"));
    assert!(service.contains("--project ${FOREMAN_PROJECT} gc-scratch"));
    assert!(
        service.contains("gc-scratch --confirm"),
        "the installed timer must pass --confirm itself; nothing else may run \
         gc-scratch unattended"
    );
    assert!(timer.contains("OnCalendar=daily"));
    assert!(timer.contains("Persistent=true"));
    assert!(environment.contains("FOREMAN_PROJECT=/absolute/path/to/project.mix"));
}

/// The bare command name — what a caller poking at this binary types first,
/// with neither `--dry-run` nor `--confirm` — must be a safe no-op against
/// the live fleet, not an immediate delete. Only the installed timer's
/// `ExecStart` (asserted above) and an operator who deliberately passes
/// `--confirm` may cause a real deletion.
#[test]
fn bare_invocation_without_confirm_or_dry_run_refuses_and_deletes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, db, _id, targets) = terminal_fixture(tmp.path(), "landed");
    let fleet = tmp.path().to_str().unwrap();
    let repo_arg = repo.to_str().unwrap();

    let bare = foreman(
        &db,
        &[
            "gc-scratch",
            "--fleet-dir",
            fleet,
            "--repo",
            repo_arg,
            "--terminal-age-hours",
            "0",
        ],
    );

    assert!(
        !bare.status.success(),
        "a bare invocation must refuse, not delete"
    );
    let stderr = String::from_utf8_lossy(&bare.stderr);
    assert!(stderr.contains("--confirm"), "{stderr}");
    assert!(
        targets.iter().all(|target| target.is_dir()),
        "nothing may be deleted without --confirm"
    );
}
