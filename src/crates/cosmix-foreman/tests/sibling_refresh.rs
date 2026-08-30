//! Refinery sibling-dependency refresh against real throwaway Git clones.

use std::path::{Path, PathBuf};
use std::process::Command;

use cosmix_foreman::ledger::{ClaimToken, Ledger};
use cosmix_foreman::refinery::{self, RefineOptions};

const HELPER_ENV: &str = "COSMIX_FOREMAN_SIBLING_REFRESH_HELPER";
const HELPER_ROOT_ENV: &str = "COSMIX_FOREMAN_SIBLING_REFRESH_ROOT";

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} in {}: {}",
        repo.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn write(repo: &Path, name: &str, content: &str) {
    std::fs::write(repo.join(name), content).unwrap();
}

fn git_repo(path: PathBuf) -> PathBuf {
    std::fs::create_dir(&path).unwrap();
    git(&path, &["init", "-b", "main"]);
    git(&path, &["config", "user.name", "t"]);
    git(&path, &["config", "user.email", "t@t"]);
    write(&path, "base.txt", "base\n");
    git(&path, &["add", "."]);
    git(&path, &["commit", "-m", "base"]);
    path
}

fn opts(repo: PathBuf, db: PathBuf) -> RefineOptions {
    RefineOptions {
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
    }
}

#[test]
fn sibling_refresh_fast_forwards_and_divergence_stops_the_queue() {
    let root = tempfile::tempdir().unwrap();
    let name = "sibling_refresh_fast_forwards_and_divergence_stops_the_queue_owned_process";
    let sibling = root.path().join("sibling");
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            name,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(HELPER_ENV, name)
        .env(HELPER_ROOT_ENV, root.path())
        .env("FOREMAN_SIBLING_REPOS", &sibling)
        .output()
        .expect("spawn owned sibling-refresh helper");
    assert!(
        out.status.success(),
        "owned helper failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
#[ignore = "run only in the owned helper process"]
fn sibling_refresh_fast_forwards_and_divergence_stops_the_queue_owned_process() {
    assert_eq!(
        std::env::var(HELPER_ENV).as_deref(),
        Ok("sibling_refresh_fast_forwards_and_divergence_stops_the_queue_owned_process")
    );
    let root = PathBuf::from(std::env::var_os(HELPER_ROOT_ENV).unwrap());
    let integration = git_repo(root.join("integration"));
    let upstream = git_repo(root.join("upstream"));
    let remote = root.join("remote.git");
    git(
        &root,
        &[
            "clone",
            "--bare",
            upstream.to_str().unwrap(),
            remote.to_str().unwrap(),
        ],
    );
    git(
        &upstream,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    let sibling = root.join("sibling");
    // Clone from the bare repository. The implementation uses @{u} (the
    // upstream branch) which works correctly regardless of whether the bare
    // repository sets up HEAD.
    git(
        &root,
        &["clone", remote.to_str().unwrap(), sibling.to_str().unwrap()],
    );

    // The installed sibling clone starts stale, then refinery preflight must
    // fetch and fast-forward it before looking at an otherwise empty queue.
    write(&upstream, "remote.txt", "remote advance\n");
    git(&upstream, &["add", "."]);
    git(&upstream, &["commit", "-m", "remote advance"]);
    git(&upstream, &["push", "origin", "main"]);
    let remote_tip = git(&upstream, &["rev-parse", "HEAD"]);

    let db = root.join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    refinery::refine(&ledger, &opts(integration.clone(), db.clone())).unwrap();
    assert_eq!(git(&sibling, &["rev-parse", "HEAD"]), remote_tip);

    // Give the clone and origin different children of the refreshed tip.
    // The failed ff is infrastructure: refine must return before changing a
    // queued task to landing/bounced or filing a task finding.
    write(&sibling, "local.txt", "local advance\n");
    git(&sibling, &["add", "."]);
    git(&sibling, &["commit", "-m", "local advance"]);
    let local_tip = git(&sibling, &["rev-parse", "HEAD"]);
    write(&upstream, "other.txt", "other remote advance\n");
    git(&upstream, &["add", "."]);
    git(&upstream, &["commit", "-m", "other remote advance"]);
    git(&upstream, &["push", "origin", "main"]);

    let task = ledger
        .add_task("queued", "spec", "impl", "low", &[], "none")
        .unwrap();
    let claim = ledger.claim_task(task, "worker").unwrap();
    ledger
        .set_task_workspace(
            task,
            ClaimToken {
                owner: "worker",
                generation: claim.attempt,
            },
            None,
            Some("task/queued"),
        )
        .unwrap();
    ledger.finish_task(task, "worker", "done").unwrap();

    let error = refinery::refine(&ledger, &opts(integration, db)).unwrap_err();
    let error = format!("{error:#}");
    assert!(error.contains("INFRA"), "{error}");
    assert!(error.contains("has diverged from its upstream"), "{error}");
    assert!(error.contains(&sibling.display().to_string()), "{error}");
    assert_eq!(git(&sibling, &["rev-parse", "HEAD"]), local_tip);
    assert_eq!(ledger.task(task).unwrap().unwrap().status, "done");
    assert!(ledger.open_findings(10).unwrap().is_empty());
}
