//! Task 30 acceptance: foreman as an independent executable driving any
//! project repo, proven against a THROWAWAY second repository — not an
//! argument that it would work.
//!
//! `second_project_lands_end_to_end_without_touching_the_first` is the test
//! that matters: it runs one project ("cmctl/cos shape" — explicit flags,
//! no `--project`) and a second, unrelated project (registered only via a
//! manifest) through dispatch → tier-0 → landing, then proves the first
//! project's ledger, worktree, locks, and integration branch are byte-for-byte
//! untouched by the second project's run. Everything happens under one
//! `tempfile::tempdir()` this test owns; nothing here ever opens a path
//! under `~/.cmctl/.foreman/`.

use std::collections::BTreeMap;
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Command, Output};

mod support;

#[test]
fn runbook_says_project_policy_denial_leaves_the_ladder_unchanged() {
    let runbook = include_str!("../../../../docs/cos/foreman.md");
    assert!(runbook.contains("leaves the quality ladder unchanged"));
    assert!(runbook.contains("automatically advance to another rung"));
    assert!(!runbook.contains(
        "advances the escalation ladder so MCP-only fleets can reach a later eligible rung"
    ));
}

fn manifest_dir() -> String {
    std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| env!("CARGO_MANIFEST_DIR").to_string())
}

fn fixture(name: &str) -> String {
    format!("{}/testdata/{name}", manifest_dir())
}

/// Every child gets a deliberate allow-list. In particular, fleet unit
/// markers and state selectors (`FOREMAN_*`), Cargo target overrides, and
/// host HOME/XDG configuration never leak into the throwaway projects.
fn child(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(program);
    command
        .env_clear()
        .env(
            "PATH",
            "/opt/cosmix/bin:/usr/bin:/usr/local/bin:/usr/sbin:/bin",
        )
        .env("HOME", "/nonexistent")
        .env("XDG_CONFIG_HOME", "/nonexistent")
        .env("XDG_CACHE_HOME", "/nonexistent")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("LC_ALL", "C");
    if let Some(rustup_home) = std::env::var_os("RUSTUP_HOME").or_else(|| {
        std::env::var_os("HOME").map(|home| Path::new(&home).join(".rustup").into_os_string())
    }) {
        // rustc/cargo are rustup shims on the fleet host. Point only at the
        // installed toolchains; HOME and Cargo/XDG configuration stay isolated.
        command.env("RUSTUP_HOME", rustup_home);
    }
    command
}

fn foreman_command(scope: &Path) -> Command {
    let mut command = child(env!("CARGO_BIN_EXE_foreman"));
    command
        .env("FOREMAN_VERIFY_LANE", scope.join("verify.lock"))
        .env("FOREMAN_VERIFY_LANE_WAIT_SECS", "30");
    command
}

fn foreman(args: &[&str]) -> Output {
    let scope = args
        .windows(2)
        .find(|pair| pair[0] == "--project" || pair[0] == "--db")
        .and_then(|pair| Path::new(pair[1]).parent())
        .expect("test foreman child must name --project or --db");
    foreman_command(scope)
        .args(args)
        .output()
        .expect("run foreman")
}

/// Hold the real host lane for the acceptance test. Under the refinery proof
/// wrapper an ancestor already owns it; otherwise this test takes it itself
/// and writes the same owner stamp Foreman writes.
struct HostLaneGuard {
    _file: Option<std::fs::File>,
    held_by_this_tree: bool,
}

fn hold_host_verify_lane() -> HostLaneGuard {
    let path = cosmix_foreman::config::FleetPolicy::host_verify_lane_path();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .unwrap();
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let me = std::process::id() as i64;
        let held_by_this_tree = cosmix_foreman::procutil::flock_holders(&path)
            .into_iter()
            .any(|pid| cosmix_foreman::procutil::process_is_ancestor(pid, me));
        return HostLaneGuard {
            _file: None,
            held_by_this_tree,
        };
    }
    let pid = std::process::id() as i64;
    let body = format!(
        "pid={pid}\npid_start={}\nacquired_at=test\n",
        cosmix_foreman::procutil::starttime(pid).unwrap()
    );
    file.set_len(0).unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();
    file.write_all(body.as_bytes()).unwrap();
    file.flush().unwrap();
    HostLaneGuard {
        _file: Some(file),
        held_by_this_tree: true,
    }
}

fn git(repo: &Path, args: &[&str]) {
    let out = child("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("git command");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_out(repo: &Path, args: &[&str]) -> String {
    let out = child("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("git command");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn git_ref_exists(repo: &Path, reference: &str) -> bool {
    child("git")
        .args(["show-ref", "--verify", reference])
        .current_dir(repo)
        .output()
        .expect("git show-ref")
        .status
        .success()
}

/// A minimal, deliberately trivial Rust crate: `cargo fmt --check`,
/// `cargo clippy --all-targets -- -D warnings`, and `cargo test` all pass
/// unmodified — the "rust" built-in verifier profile (task 29's shape)
/// verifies it with no project-specific argv at all.
fn init_trivial_crate(repo: &Path, pkg: &str) {
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{pkg}\"\nversion = \"0.1.0\"\nedition = \"2024\"\nlicense = \"MIT\"\n"
        ),
    )
    .unwrap();
    std::fs::write(
        repo.join("src/lib.rs"),
        "pub fn answer() -> i32 {\n    42\n}\n\n\
         #[test]\n\
         fn answer_is_42() {\n    assert_eq!(answer(), 42);\n}\n",
    )
    .unwrap();
    // Tier 1 (the refinery's landing gate) runs `cargo deny check` when it's
    // installed on the host; without a config it falls back to a default
    // that rejects every license as "not explicitly allowed". This is not
    // project-specific verifier plumbing — it's the same file any real repo
    // ships to make cargo-deny's default reject-everything policy usable.
    std::fs::write(repo.join("deny.toml"), "[licenses]\nallow = [\"MIT\"]\n").unwrap();
    std::fs::write(repo.join("verifier.marker"), "project verifier\n").unwrap();
    std::fs::write(repo.join(".gitignore"), "target/\nCargo.lock\n").unwrap();
    git(repo, &["init", "-b", "main"]);
    git(repo, &["config", "user.email", "test@example.com"]);
    git(repo, &["config", "user.name", "Test"]);
    git(repo, &["add", "."]);
    git(repo, &["commit", "-m", "initial"]);
}

fn ledger_rows(path: &Path) -> BTreeMap<String, Vec<Vec<String>>> {
    let connection = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .unwrap();
    let mut tables = connection
        .prepare(
            "SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .unwrap();
    let names: Vec<String> = tables
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let mut snapshot = BTreeMap::new();
    for table in names {
        let mut columns = connection
            .prepare(&format!(
                "SELECT name FROM pragma_table_info('{table}') ORDER BY cid"
            ))
            .unwrap();
        let columns: Vec<String> = columns
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let projection = columns
            .iter()
            .map(|column| format!("quote(\"{column}\")"))
            .collect::<Vec<_>>()
            .join(", ");
        let mut statement = connection
            .prepare(&format!(
                "SELECT {projection} FROM \"{table}\" ORDER BY rowid"
            ))
            .unwrap();
        let rows = statement
            .query_map([], |row| {
                (0..columns.len())
                    .map(|index| row.get(index))
                    .collect::<rusqlite::Result<Vec<String>>>()
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        snapshot.insert(table, rows);
    }
    snapshot
}

fn ledger_files(path: &Path) -> BTreeMap<String, Vec<u8>> {
    let parent = path.parent().unwrap();
    let prefix = path.file_name().unwrap().to_string_lossy();
    std::fs::read_dir(parent)
        .unwrap()
        .map(Result::unwrap)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.starts_with(prefix.as_ref())
                .then(|| (name, std::fs::read(entry.path()).unwrap()))
        })
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
struct GitSnapshot {
    integration: String,
    worktree_head: String,
    worktree_status: String,
    registrations: String,
    git_pointer: Vec<u8>,
}

fn git_snapshot(repo: &Path, integration: &str, worktree: &Path) -> GitSnapshot {
    GitSnapshot {
        integration: git_out(repo, &["rev-parse", integration]),
        worktree_head: git_out(worktree, &["symbolic-ref", "HEAD"]),
        worktree_status: git_out(worktree, &["status", "--porcelain=v1"]),
        registrations: git_out(repo, &["worktree", "list", "--porcelain"]),
        git_pointer: std::fs::read(worktree.join(".git")).unwrap(),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct LockSnapshot {
    bytes: Vec<u8>,
    inode: u64,
    mode: u32,
    available: bool,
}

fn lock_snapshot(path: &Path) -> LockSnapshot {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    // SAFETY: flock receives a live file descriptor and no pointer arguments.
    let available = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    if available {
        // SAFETY: releases the lock acquired immediately above on the same fd.
        assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) }, 0);
    }
    let metadata = file.metadata().unwrap();
    LockSnapshot {
        bytes: std::fs::read(path).unwrap(),
        inode: metadata.ino(),
        mode: metadata.mode(),
        available,
    }
}

/// A stand-in implementation agent: real git work (a content change that
/// keeps fmt/clippy/test green, committed to the task branch) followed by
/// replaying a captured successful completion stream — the same recipe
/// a real implementation agent uses, driven here through the real `foreman`
/// binary instead of the library directly. Package versioning is deliberately
/// left to the refinery.
fn write_agent_script(path: &Path) {
    support::write_executable(
        path,
        "#!/bin/sh\nset -eu\n\
         if [ -n \"${EXPECT_PROJECT:-}\" ]; then\n\
           settings=\n\
           while [ \"$#\" -gt 0 ]; do\n\
            if [ \"$1\" = \"--settings\" ]; then settings=$2; shift 2; else shift; fi\n\
           done\n\
           test -n \"$settings\"\n\
           grep -F -- \"--project\" \"$settings\" >/dev/null\n\
           grep -F -- \"$EXPECT_PROJECT\" \"$settings\" >/dev/null\n\
         fi\n\
         printf '\npub fn agent_change() {}\n' >> src/lib.rs\n\
         git add src/lib.rs\n\
         git commit -m 'implement change'\n\
         cat \"$FAKE_STREAM\"\n",
    );
}

/// The five things the task's acceptance criterion asks for, all in one
/// walk: (1) a throwaway repo with its own trivial crate and (task 29's)
/// verifier profile, (2) registered as a project via the manifest, (3) one
/// task run through it end to end — dispatch, tier-0, landing, (4) the
/// first project's ledger/worktree/locks/integration branch untouched, (5)
/// the existing cmctl/cos-shaped invocation (explicit flags, no
/// `--project`) still works unchanged.
#[test]
fn second_project_lands_end_to_end_without_touching_the_first() {
    let tmp = tempfile::tempdir().unwrap();
    let host_lane = hold_host_verify_lane();
    // Both integration repositories are siblings. Project B's manifest root
    // must keep its worktree and lock out of this shared repo parent.
    let repos = tmp.path().join("repos");
    std::fs::create_dir(&repos).unwrap();

    // ---- Project A: the cmctl/cos invocation shape — explicit flags only,
    // never `--project`. This is what the operator's systemd units do
    // today and must keep doing unchanged. ----
    let dir_a = tmp.path().join("project-a");
    std::fs::create_dir_all(&dir_a).unwrap();
    let repo_a = repos.join("project-a");
    init_trivial_crate(&repo_a, "project-a-demo");
    let db_a = dir_a.join("ledger.db");

    // The exact live defect: a real child Foreman with no private lane is a
    // descendant of the host-lane holder. It must refuse before its 30s
    // contention bound, naming both the holder and the remedy.
    let started = std::time::Instant::now();
    let refused = child(env!("CARGO_BIN_EXE_foreman"))
        .args([
            "--db",
            db_a.to_str().unwrap(),
            "verify",
            "--dir",
            repo_a.to_str().unwrap(),
            "--profile",
            "rust",
            "--tier",
            "0",
        ])
        .env(
            "FOREMAN_VERIFY_LANE_WAIT_SECS",
            if host_lane.held_by_this_tree {
                "30"
            } else {
                // A separate live fleet run can legitimately own the host
                // lane while this suite runs. It is not our ancestor, so it
                // exercises the bounded-holder diagnostic rather than the
                // ancestor deadlock refusal.
                "1"
            },
        )
        .output()
        .expect("run unscoped child foreman");
    assert!(!refused.status.success(), "unscoped child must refuse");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "ancestor contention must refuse immediately, not wait: {:?}",
        started.elapsed()
    );
    let refusal = String::from_utf8_lossy(&refused.stderr);
    if host_lane.held_by_this_tree {
        assert!(
            refusal.contains("would deadlock on the host verify lane held by pid")
                && refusal.contains("set FOREMAN_VERIFY_LANE"),
            "refusal must name the holder and remedy: {refusal}"
        );
    } else {
        assert!(
            refusal.contains("verify lane acquisition timed out after 1s")
                && refusal.contains("blocked on pid"),
            "unrelated contention must still be bounded and name its holder: {refusal}"
        );
    }

    assert!(
        foreman(&["--db", db_a.to_str().unwrap(), "init"])
            .status
            .success()
    );
    let add_a = foreman(&[
        "--db",
        db_a.to_str().unwrap(),
        "task",
        "add",
        "project-a task",
        "--spec",
        "{}",
    ]);
    assert!(add_a.status.success(), "{add_a:?}");

    let agent_a = dir_a.join("fake-agent");
    write_agent_script(&agent_a);
    let dispatch_a = foreman_command(&dir_a)
        .args([
            "--db",
            db_a.to_str().unwrap(),
            "dispatch",
            "--max-tasks",
            "1",
            "--workdir",
            repo_a.to_str().unwrap(),
            "--branch-template",
            "task/{id}",
            "--integration",
            "main",
            "--subdir",
            ".",
        ])
        .env("FOREMAN_LADDER", "claude")
        .env("FOREMAN_CLAUDE_BIN", &agent_a)
        .env("FAKE_STREAM", fixture("claude-ok.jsonl"))
        .output()
        .expect("dispatch A");
    let dispatch_a_stdout = String::from_utf8_lossy(&dispatch_a.stdout).into_owned();
    assert!(
        dispatch_a.status.success(),
        "project A dispatch must succeed: stdout={dispatch_a_stdout} stderr={}",
        String::from_utf8_lossy(&dispatch_a.stderr)
    );
    assert!(
        dispatch_a_stdout.contains("ran 1") && !dispatch_a_stdout.contains("bounced 1"),
        "project A's task must run tier-0 and reach done: stdout={dispatch_a_stdout} stderr={}",
        String::from_utf8_lossy(&dispatch_a.stderr)
    );

    let refine_a = foreman(&[
        "--db",
        db_a.to_str().unwrap(),
        "refine",
        "--repo",
        repo_a.to_str().unwrap(),
        "--integration",
        "main",
        "--subdir",
        ".",
        "--tier",
        "0",
    ]);
    let refine_a_stdout = String::from_utf8_lossy(&refine_a.stdout).into_owned();
    assert!(
        refine_a.status.success() && refine_a_stdout.contains("1/1 landed"),
        "project A must land: stdout={refine_a_stdout} stderr={}",
        String::from_utf8_lossy(&refine_a.stderr)
    );
    let worktree_a = repos.join("task-1");
    assert!(
        worktree_a.is_dir(),
        "project A's task-1 worktree must exist as a sibling of its own repo"
    );
    let lock_a = repos.join("clone.lock");
    assert!(lock_a.is_file(), "project A's clone.lock must exist");
    let verify_lock_a = dir_a.join("verify.lock");
    assert!(
        verify_lock_a.is_file(),
        "project A's verifier lock and owner stamp must exist"
    );
    let rows_a_before_b = ledger_rows(&db_a);
    let files_a_before_b = ledger_files(&db_a);
    let git_a_before_b = git_snapshot(&repo_a, "main", &worktree_a);
    let lock_a_before_b = lock_snapshot(&lock_a);
    let verify_lock_a_before_b = lock_snapshot(&verify_lock_a);
    let verify_owner_a_before_b = std::fs::read_to_string(&verify_lock_a).unwrap();

    // ---- Project B: a throwaway repo, registered ONLY via a project
    // manifest — no --repo, --integration, --subdir, --db, or --verifier
    // flag anywhere in its invocations. ----
    let dir_b = tmp.path().join("project-b");
    std::fs::create_dir_all(&dir_b).unwrap();
    let repo_b = repos.join("project-b");
    init_trivial_crate(&repo_b, "project-b-demo");
    let remote_b = dir_b.join("publish.git");
    git(&dir_b, &["init", "--bare", remote_b.to_str().unwrap()]);
    git(
        &repo_b,
        &["remote", "add", "publish", remote_b.to_str().unwrap()],
    );
    git(&repo_b, &["push", "publish", "main:refs/heads/main"]);
    let manifest_b = dir_b.join("project.mix");
    let root_b = dir_b.join(".foreman-project-project-b");
    let db_b = root_b.join("ledger.db");
    let cache_b = root_b.join("cache");
    std::fs::create_dir_all(&cache_b).unwrap();
    std::fs::write(
        &manifest_b,
        format!(
            "name: \"project-b\"\n\
             repo: \"{}\"\n\
             db: \"{}\"\n\
             cache_dir: \"{}\"\n\
             integration: \"main\"\n\
             branch_template: \"task/{{id}}\"\n\
             worktree_template: \"task-{{id}}\"\n\
             package_manifest_template: \"Cargo.toml\"\n\
             verifier: \"project\"\n\
             profiles: {{ project: {{ cwd: \".\",\n\
             \x20\x20tier0: [[\"cargo\", \"test\", \"--quiet\"],\n\
             \x20\x20\x20\x20{{ argv: [\"sh\", \"-c\", \"test -f verifier.marker\"], opaque: true }}],\n\
             \x20\x20tier1: [],\n\
             \x20\x20tier2: []\n\
             }} }}\n\
             landing_tier: 0\n\
             landing_review: false\n\
             push_remote: \"publish\"\n\
             lanes: {{ claude: {{ credentials: [\"PROJECT_B_PUBLISH_TOKEN\"] }} }}\n\
             instruction_pack: \"This is a throwaway demo crate proving foreman \
             can drive a second, non-cosmix repository end to end.\"\n",
            repo_b.display(),
            db_b.display(),
            cache_b.display(),
        ),
    )
    .unwrap();
    let project_b_flag = ["--project", manifest_b.to_str().unwrap()];

    assert!(
        foreman(&[project_b_flag[0], project_b_flag[1], "init"])
            .status
            .success()
    );
    let add_b = foreman(&[
        project_b_flag[0],
        project_b_flag[1],
        "task",
        "add",
        "project-b task",
        "--spec",
        "{}",
    ]);
    assert!(add_b.status.success(), "{add_b:?}");

    let verify_b = foreman(&[
        project_b_flag[0],
        project_b_flag[1],
        "verify",
        "--tier",
        "0",
    ]);
    assert!(
        verify_b.status.success(),
        "manifest-only verify must use project repo/profile: {}",
        String::from_utf8_lossy(&verify_b.stderr)
    );

    let agent_b = dir_b.join("fake-agent");
    write_agent_script(&agent_b);
    let dispatch_b = foreman_command(&dir_b)
        .args([
            project_b_flag[0],
            project_b_flag[1],
            "dispatch",
            "--max-tasks",
            "1",
            "--policy",
        ])
        .env("FOREMAN_LADDER", "claude")
        .env("FOREMAN_CLAUDE_BIN", &agent_b)
        .env("FAKE_STREAM", fixture("claude-ok.jsonl"))
        .env("EXPECT_PROJECT", &manifest_b)
        .env("PROJECT_B_PUBLISH_TOKEN", "fixture-token")
        .output()
        .expect("dispatch B");
    let dispatch_b_stdout = String::from_utf8_lossy(&dispatch_b.stdout).into_owned();
    assert!(
        dispatch_b.status.success(),
        "project B dispatch (manifest-only) must succeed: stdout={dispatch_b_stdout} stderr={}",
        String::from_utf8_lossy(&dispatch_b.stderr)
    );
    assert!(
        dispatch_b_stdout.contains("ran 1") && !dispatch_b_stdout.contains("bounced 1"),
        "project B's task must run tier-0 and reach done: {dispatch_b_stdout}"
    );

    let refine_b = foreman_command(&dir_b)
        .args([project_b_flag[0], project_b_flag[1], "refine"])
        .env("FOREMAN_LANDING_GATE", "sh -c exit 99")
        .env("PROJECT_B_PUBLISH_TOKEN", "fixture-token")
        .output()
        .expect("refine B");
    let refine_b_stdout = String::from_utf8_lossy(&refine_b.stdout).into_owned();
    assert!(
        refine_b.status.success() && refine_b_stdout.contains("1/1 landed"),
        "project B must land using only manifest-supplied repo/integration/profile/gate: \
         stdout={refine_b_stdout} stderr={}",
        String::from_utf8_lossy(&refine_b.stderr)
    );
    assert_eq!(
        git_out(&remote_b, &["rev-parse", "refs/heads/main"]),
        git_out(&repo_b, &["rev-parse", "refs/heads/main"]),
        "manifest push_remote must receive the exact landed integration tip"
    );
    let push_rows = cosmix_foreman::ledger::Ledger::open(&db_b)
        .unwrap()
        .push_intents_for_attempt(1, 1)
        .unwrap();
    assert_eq!(push_rows.len(), 2);
    assert_eq!(
        push_rows[0].outcome,
        cosmix_foreman::ledger::PushIntentOutcome::Succeeded
    );
    assert_eq!(
        push_rows[1].outcome,
        cosmix_foreman::ledger::PushIntentOutcome::Succeeded
    );
    assert_eq!(
        push_rows[1].kind,
        cosmix_foreman::ledger::PushIntentKind::Delete
    );
    assert!(
        !git_ref_exists(&remote_b, "refs/heads/task/1"),
        "the landed task's remote branch must be absent after pruning"
    );
    assert!(
        root_b.join("clone.lock").is_file(),
        "project B clone.lock must live under its per-manifest root"
    );
    assert!(
        root_b.join("task-1").is_dir(),
        "project B worktree must live under its per-manifest root"
    );
    // ==== Isolation: project A's ledger, worktree, locks, and integration
    // branch are untouched by everything project B just did. ====

    let files_a_after_b = ledger_files(&db_a);
    let git_a_after_b = git_snapshot(&repo_a, "main", &worktree_a);
    let lock_a_after_b = lock_snapshot(&lock_a);
    let verify_lock_a_after_b = lock_snapshot(&verify_lock_a);
    let verify_owner_a_after_b = std::fs::read_to_string(&verify_lock_a).unwrap();
    assert_eq!(
        files_a_before_b, files_a_after_b,
        "every project A SQLite file must remain byte-identical while B runs"
    );
    assert_eq!(
        git_a_before_b, git_a_after_b,
        "project A integration/worktree HEADs, status, registration and .git pointer must be unchanged"
    );
    assert_eq!(
        lock_a_before_b, lock_a_after_b,
        "project A's clone lock inode, bytes and availability must be unchanged"
    );
    assert_eq!(
        verify_lock_a_before_b, verify_lock_a_after_b,
        "project A's verifier lock inode, bytes and availability must be unchanged"
    );
    assert_eq!(
        verify_owner_a_before_b, verify_owner_a_after_b,
        "project A's verifier lane owner stamp must be unchanged"
    );
    let rows_a_after_b = ledger_rows(&db_a);
    assert_eq!(
        rows_a_before_b, rows_a_after_b,
        "every row of every project A ledger table must remain unchanged"
    );
    let log_a = git_out(&repo_a, &["log", "--oneline", "main"]);
    assert!(
        !log_a.contains("project-b"),
        "project A's history must never mention project B: {log_a}"
    );

    let tasks_a = foreman(&["--db", db_a.to_str().unwrap(), "task", "list", "--json"]);
    let tasks_a: serde_json::Value = serde_json::from_slice(&tasks_a.stdout).unwrap();
    let tasks_a = tasks_a.as_array().unwrap();
    assert_eq!(
        tasks_a.len(),
        1,
        "project A's ledger must still hold exactly its own one task: {tasks_a:?}"
    );
    assert_eq!(tasks_a[0]["status"], "landed");

    let tasks_b = foreman(&[
        project_b_flag[0],
        project_b_flag[1],
        "task",
        "list",
        "--json",
    ]);
    let tasks_b: serde_json::Value = serde_json::from_slice(&tasks_b.stdout).unwrap();
    let tasks_b = tasks_b.as_array().unwrap();
    assert_eq!(
        tasks_b.len(),
        1,
        "project B's ledger must hold exactly its own one task: {tasks_b:?}"
    );
    assert_eq!(tasks_b[0]["status"], "landed");

    // Worktrees: legacy A retains its repo-parent location while manifest B
    // uses its manifest-derived project root, despite the repos being
    // siblings and both tasks having id 1.
    assert!(worktree_a.is_dir(), "project A's worktree must still exist");
    assert!(
        std::fs::read_to_string(worktree_a.join("Cargo.toml"))
            .unwrap()
            .contains("project-a-demo"),
        "project A's worktree must still hold project A's own crate"
    );
    let worktree_b = root_b.join("task-1");
    assert!(
        worktree_b.is_dir(),
        "project B's task-1 worktree must exist below its manifest root"
    );
    assert!(
        std::fs::read_to_string(worktree_b.join("Cargo.toml"))
            .unwrap()
            .contains("project-b-demo"),
        "project B's worktree must hold project B's own crate"
    );

    // B has its own available sibling lock; A's exact lock state was
    // compared above rather than merely comparing paths selected by the test.
    let lock_b = root_b.join("clone.lock");
    assert!(lock_b.is_file(), "project B's clone.lock must exist");
    assert!(lock_snapshot(&lock_b).available);
    assert!(
        !dir_a.join("project.mix").exists(),
        "project B's manifest must never appear under project A's directory"
    );

    // A's byte/row equality above proves separate-ledger isolation rather
    // than relying on a tautological inequality between configured paths.
    assert!(db_a.is_file());
    assert!(db_b.is_file());
}

#[test]
fn omitted_push_remote_is_a_stated_noop() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_trivial_crate(&repo, "push-noop-demo");
    let manifest = tmp.path().join("project.mix");
    std::fs::write(
        &manifest,
        format!(
            "name: \"push-noop\"\nrepo: {:?}\ndb: \"ledger.db\"\ncache_dir: \"cache\"\nlanding_tier: 0\ninstruction_pack: \"Project rules.\"\n",
            repo
        ),
    )
    .unwrap();

    let output = foreman_command(tmp.path())
        .args(["--project", manifest.to_str().unwrap(), "refine"])
        .output()
        .expect("refine no-op project");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("push_remote is not configured; remote update is a no-op"),
        "{stdout}"
    );
}

/// A manifest's `verifier` supplies the default only when `--verifier` is
/// omitted; an explicit flag always wins. No agent spawn needed — this is
/// the flag-precedence contract in isolation.
#[test]
fn manifest_verifier_default_applies_only_when_flag_omitted() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_trivial_crate(&repo, "verifier-default");
    let manifest = tmp.path().join("project.mix");
    let root = tmp.path().join(".foreman-project-demo");
    let db = root.join("ledger.db");
    let cache = root.join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(
        &manifest,
        format!(
            "name: \"demo\"\nrepo: \"{}\"\ndb: \"{}\"\ncache_dir: \"{}\"\ninstruction_pack: \"Project rules.\"\nverifier: \"none\"\n",
            repo.display(),
            db.display(),
            cache.display()
        ),
    )
    .unwrap();
    let flag = ["--project", manifest.to_str().unwrap()];

    assert!(foreman(&[flag[0], flag[1], "init"]).status.success());
    assert!(
        foreman(&[flag[0], flag[1], "task", "add", "no flag", "--spec", "{}",])
            .status
            .success()
    );
    assert!(
        foreman(&[
            flag[0],
            flag[1],
            "task",
            "add",
            "explicit flag",
            "--spec",
            "{}",
            "--verifier",
            "rust",
        ])
        .status
        .success()
    );

    let show = |id: &str| -> serde_json::Value {
        let out = foreman(&[flag[0], flag[1], "task", "show", id]);
        assert!(out.status.success(), "{out:?}");
        serde_json::from_slice(&out.stdout).unwrap()
    };
    assert_eq!(
        show("1")["verifier_profile"],
        "none",
        "no --verifier flag: the manifest's default must apply"
    );
    assert_eq!(
        show("2")["verifier_profile"],
        "rust",
        "an explicit --verifier flag must win over the manifest's default"
    );
}

#[test]
fn run_verify_and_gc_take_project_defaults() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_trivial_crate(&repo, "run-defaults-demo");
    git(&repo, &["branch", "-m", "trunk"]);
    git(&repo, &["checkout", "-b", "decoy-head"]);
    std::fs::write(
        repo.join("decoy.txt"),
        "repository HEAD is not integration\n",
    )
    .unwrap();
    git(&repo, &["add", "decoy.txt"]);
    git(&repo, &["commit", "-m", "diverge HEAD from integration"]);
    let manifest = tmp.path().join("project.mix");
    let root = tmp.path().join(".foreman-project-run-defaults");
    let db = root.join("ledger.db");
    let cache = root.join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(
        &manifest,
        format!(
            "name: \"run-defaults\"\nrepo: \"{}\"\ndb: \"{}\"\ncache_dir: \"{}\"\n\
             integration: \"trunk\"\nworktree_template: \"run-{{id}}\"\nverifier: \"none\"\ninstruction_pack: \"Project rules.\"\n",
            repo.display(),
            db.display(),
            cache.display()
        ),
    )
    .unwrap();
    let flag = ["--project", manifest.to_str().unwrap()];
    assert!(foreman(&[flag[0], flag[1], "init"]).status.success());
    assert!(
        foreman(&[flag[0], flag[1], "task", "add", "run task", "--spec", "{}"])
            .status
            .success()
    );
    let agent = tmp.path().join("fake-agent");
    write_agent_script(&agent);
    let run = foreman_command(tmp.path())
        .args([
            flag[0], flag[1], "run", "--task", "1", "--agent", "claude", "--branch", "topic/1",
        ])
        .env("FOREMAN_CLAUDE_BIN", &agent)
        .env("FAKE_STREAM", fixture("claude-ok.jsonl"))
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "run --project must use repo/integration from the manifest: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    let worktree = root.join("run-1");
    assert!(worktree.is_dir());
    assert_eq!(git_out(&worktree, &["branch", "--show-current"]), "topic/1");
    let ancestor = child("git")
        .args(["merge-base", "--is-ancestor", "trunk", "topic/1"])
        .current_dir(&repo)
        .status()
        .unwrap();
    assert!(
        ancestor.success(),
        "run branch must be based on manifest integration"
    );
    let decoy_ancestor = child("git")
        .args(["merge-base", "--is-ancestor", "decoy-head", "topic/1"])
        .current_dir(&repo)
        .status()
        .unwrap();
    assert!(
        !decoy_ancestor.success(),
        "first dispatch must not create the task branch from repository HEAD"
    );

    let verify = foreman(&[flag[0], flag[1], "verify", "--tier", "0"]);
    assert!(
        verify.status.success(),
        "verify --project must resolve repo/profile"
    );
    let gc = foreman_command(tmp.path())
        .args([flag[0], flag[1], "gc-cache", "--max-gb", "1"])
        .env_remove("CARGO_TARGET_DIR")
        .output()
        .unwrap();
    assert!(
        gc.status.success(),
        "gc-cache --project must use cache_dir without ambient fallback: {}",
        String::from_utf8_lossy(&gc.stderr)
    );
}

#[test]
fn manifest_name_and_repository_history_are_bound_to_the_ledger_on_open() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_trivial_crate(&repo, "identity-demo");
    let manifest = tmp.path().join("alpha.mix");
    let root = tmp.path().join(".foreman-alpha-alpha");
    let db = root.join("ledger.db");
    let cache = root.join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    let body = |repository: &Path| {
        format!(
            "name: \"alpha\"\nrepo: \"{}\"\ndb: \"{}\"\ncache_dir: \"{}\"\ninstruction_pack: \"Project rules.\"\n",
            repository.display(),
            db.display(),
            cache.display()
        )
    };
    std::fs::write(&manifest, body(&repo)).unwrap();

    let init = foreman(&["--project", manifest.to_str().unwrap(), "init"]);
    assert!(init.status.success(), "{init:?}");
    let connection = rusqlite::Connection::open(&db).unwrap();
    let stored: (String, String) = connection
        .query_row(
            "SELECT name, repository_identity FROM project_identity WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(stored.0, "alpha");
    assert_eq!(
        stored.1,
        format!(
            "git-root:{}",
            git_out(&repo, &["rev-list", "--max-parents=0", "HEAD"])
        )
    );
    drop(connection);

    let unrelated = tmp.path().join("unrelated");
    init_trivial_crate(&unrelated, "unrelated-identity");
    std::fs::write(&manifest, body(&unrelated)).unwrap();
    let wrong_repo = foreman(&[
        "--project",
        manifest.to_str().unwrap(),
        "task",
        "list",
        "--json",
    ]);
    assert!(!wrong_repo.status.success(), "{wrong_repo:?}");
    let stderr = String::from_utf8_lossy(&wrong_repo.stderr);
    assert!(
        stderr.contains("repository") && stderr.contains("refusing manifest"),
        "{stderr}"
    );

    let moved = tmp.path().join("moved-repo");
    std::fs::rename(&repo, &moved).unwrap();
    std::fs::write(&manifest, body(&moved)).unwrap();
    let reopened = foreman(&["--project", manifest.to_str().unwrap(), "init"]);
    assert!(
        reopened.status.success(),
        "moving the same repository must retain identity: {}",
        String::from_utf8_lossy(&reopened.stderr)
    );
}

#[test]
fn populated_unbound_ledger_requires_explicit_migration_but_empty_ledger_can_stamp() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_trivial_crate(&repo, "legacy-identity");

    let populated_root = tmp.path().join(".foreman-populated-legacy");
    let populated_db = populated_root.join("populated.db");
    let populated_cache = populated_root.join("cache");
    std::fs::create_dir_all(&populated_cache).unwrap();
    assert!(
        foreman(&["--db", populated_db.to_str().unwrap(), "init"])
            .status
            .success()
    );
    assert!(
        foreman(&[
            "--db",
            populated_db.to_str().unwrap(),
            "task",
            "add",
            "legacy task",
            "--spec",
            "{}",
        ])
        .status
        .success()
    );
    let populated_manifest = tmp.path().join("populated.mix");
    std::fs::write(
        &populated_manifest,
        format!(
            "name: \"legacy\"\nrepo: \"{}\"\ndb: \"{}\"\ncache_dir: \"{}\"\ninstruction_pack: \"Project rules.\"\n",
            repo.display(),
            populated_db.display(),
            populated_cache.display()
        ),
    )
    .unwrap();
    let refused = foreman(&["--project", populated_manifest.to_str().unwrap(), "init"]);
    assert!(!refused.status.success(), "{refused:?}");
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("legacy state")
            && stderr.contains("no project identity")
            && stderr.contains("migrate it explicitly"),
        "{stderr}"
    );

    let partial_root = tmp.path().join(".foreman-partial-legacy");
    let partial_db = partial_root.join("partial.db");
    let partial_cache = partial_root.join("cache");
    std::fs::create_dir_all(&partial_cache).unwrap();
    let partial = rusqlite::Connection::open(&partial_db).unwrap();
    partial
        .execute_batch(
            "CREATE TABLE project_identity (
                singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                name TEXT NOT NULL UNIQUE
             );
             INSERT INTO project_identity (singleton, name) VALUES (1, 'legacy');",
        )
        .unwrap();
    drop(partial);
    assert!(
        foreman(&["--db", partial_db.to_str().unwrap(), "init"])
            .status
            .success()
    );
    assert!(
        foreman(&[
            "--db",
            partial_db.to_str().unwrap(),
            "task",
            "add",
            "partially stamped task",
            "--spec",
            "{}",
        ])
        .status
        .success()
    );
    let partial_manifest = tmp.path().join("partial.mix");
    std::fs::write(
        &partial_manifest,
        format!(
            "name: \"legacy\"\nrepo: \"{}\"\ndb: \"{}\"\ncache_dir: \"{}\"\ninstruction_pack: \"Project rules.\"\n",
            repo.display(),
            partial_db.display(),
            partial_cache.display()
        ),
    )
    .unwrap();
    let partial_refused = foreman(&["--project", partial_manifest.to_str().unwrap(), "init"]);
    assert!(!partial_refused.status.success(), "{partial_refused:?}");
    let stderr = String::from_utf8_lossy(&partial_refused.stderr);
    assert!(
        stderr.contains("repository identity is unstamped")
            && stderr.contains("migrate it explicitly"),
        "{stderr}"
    );

    let empty_root = tmp.path().join(".foreman-empty-empty");
    let empty_db = empty_root.join("empty.db");
    let empty_cache = empty_root.join("cache");
    std::fs::create_dir_all(&empty_cache).unwrap();
    assert!(
        foreman(&["--db", empty_db.to_str().unwrap(), "init"])
            .status
            .success()
    );
    let empty_manifest = tmp.path().join("empty.mix");
    std::fs::write(
        &empty_manifest,
        format!(
            "name: \"empty\"\nrepo: \"{}\"\ndb: \"{}\"\ncache_dir: \"{}\"\ninstruction_pack: \"Project rules.\"\n",
            repo.display(),
            empty_db.display(),
            empty_cache.display()
        ),
    )
    .unwrap();
    let adopted = foreman(&["--project", empty_manifest.to_str().unwrap(), "init"]);
    assert!(
        adopted.status.success(),
        "an empty schema-only ledger may be stamped on first project open: {}",
        String::from_utf8_lossy(&adopted.stderr)
    );
}

#[test]
fn manifest_repo_and_integration_cannot_be_overridden() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_trivial_crate(&repo, "authority-demo");
    git(&repo, &["branch", "-m", "trunk"]);
    let other = tmp.path().join("other");
    init_trivial_crate(&other, "wrong-repo");
    let manifest = tmp.path().join("project.mix");
    let root = tmp.path().join(".foreman-project-authority");
    let db = root.join("ledger.db");
    let cache = root.join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(
        &manifest,
        format!(
            "name: \"authority\"\nrepo: \"{}\"\ndb: \"{}\"\ncache_dir: \"{}\"\nintegration: \"trunk\"\nverifier: \"none\"\nlanding_tier: 0\ninstruction_pack: \"Project rules.\"\n",
            repo.display(),
            db.display(),
            cache.display()
        ),
    )
    .unwrap();
    let project = manifest.to_str().unwrap();
    assert!(foreman(&["--project", project, "init"]).status.success());

    let matching = foreman(&[
        "--project",
        project,
        "refine",
        "--repo",
        repo.to_str().unwrap(),
        "--integration",
        "trunk",
    ]);
    assert!(
        matching.status.success(),
        "matching assertions remain accepted: {}",
        String::from_utf8_lossy(&matching.stderr)
    );
    let ambient_sibling = foreman_command(tmp.path())
        .args(["--project", project, "refine"])
        .env("FOREMAN_SIBLING_REPOS", &other)
        .output()
        .unwrap();
    assert!(
        ambient_sibling.status.success(),
        "project mode must not fetch or fast-forward ambient fleet sibling clones: {}",
        String::from_utf8_lossy(&ambient_sibling.stderr)
    );
    let wrong_repo = foreman(&[
        "--project",
        project,
        "refine",
        "--repo",
        other.to_str().unwrap(),
    ]);
    assert!(!wrong_repo.status.success());
    assert!(
        String::from_utf8_lossy(&wrong_repo.stderr).contains("fixes --repo"),
        "{}",
        String::from_utf8_lossy(&wrong_repo.stderr)
    );
    let wrong_integration = foreman(&["--project", project, "refine", "--integration", "main"]);
    assert!(!wrong_integration.status.success());
    assert!(
        String::from_utf8_lossy(&wrong_integration.stderr).contains("fixes --integration"),
        "{}",
        String::from_utf8_lossy(&wrong_integration.stderr)
    );
}

/// `refine` without `--repo` and without an active `--project` manifest
/// fails with a clear error rather than silently defaulting to the cwd.
#[test]
fn refine_without_repo_or_project_fails_clearly() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    assert!(
        foreman(&["--db", db.to_str().unwrap(), "init"])
            .status
            .success()
    );

    let out = foreman(&["--db", db.to_str().unwrap(), "refine"]);
    assert!(!out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--repo") && stderr.contains("--project"),
        "the error should point at both ways to supply a repo: {stderr}"
    );
}
