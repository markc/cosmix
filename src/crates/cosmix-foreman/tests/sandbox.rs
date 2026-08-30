//! End-to-end containment tests for the mount-namespace sandbox
//! (`src/sandbox.rs`, opt-in via `FOREMAN_SANDBOX=bwrap`).
//!
//! `contained_write_outside_the_bind_set_does_not_reach_the_host` drives a
//! fixture "codex" CLI through the real `CodexDriver`, proving a write
//! inside the bound set (the worktree) persists to the host while a write
//! outside it does not — with a CONTROL run (sandbox off) proving the
//! fixture's write mechanism actually reaches the host first. The spec
//! this task started from said the outside write should "fail"; it
//! doesn't — bwrap materializes bind mountpoints inside its own tmpfs, so
//! an unbound path still accepts the write, it just never leaves the
//! namespace. The correct assertion is NON-PERSISTENCE, and a test that
//! only checked "the write call didn't error" would be green against no
//! sandbox at all — hence the control.
//!
//! `escaping_the_namespace_with_umount_is_denied...` proves the mechanism
//! that makes this different from a bare `unshare -r`: the sandboxed
//! process holds no capability that would let it unmount its own
//! containment and read what it hid.

use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::Duration;

use cosmix_foreman::driver::claude::ClaudeDriver;
use cosmix_foreman::driver::codex::CodexDriver;
use cosmix_foreman::executor::{AgentKind, Budget, Executor, StopReason, Workspace};
use cosmix_foreman::ledger::Ledger;
use cosmix_foreman::policy::{LEGACY_PACKAGE_MANIFEST_TEMPLATE, PolicyContext, hook_settings};
use cosmix_foreman::sandbox::{HookMounts, ProjectHookMounts, SandboxSpec, wrap};
use cosmix_foreman::state::DbCreateMode;
use tempfile::TempDir;

mod support;

const HELPER_ENV: &str = "COSMIX_FOREMAN_SANDBOX_HELPER";

fn bwrap_installed() -> bool {
    ["/usr/bin/bwrap", "/bin/bwrap"]
        .into_iter()
        .any(|p| Path::new(p).exists())
}

/// Keep the synthetic home out of `/tmp`: `wrap()` deliberately overlays
/// `/tmp` with its own scratch tmpfs, which would otherwise shadow `$HOME`
/// and let the non-persistence assertion pass even if the `$HOME` tmpfs were
/// removed. The fixture also requires `$HOME` to be an exact mountpoint because
/// bwrap's new root is itself ephemeral; together those checks make removal of
/// the dedicated `$HOME` tmpfs observable.
fn synthetic_home() -> TempDir {
    let home = tempfile::Builder::new()
        .prefix(".foreman-sandbox-home-")
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))
        .unwrap();
    assert!(
        !home.path().starts_with("/tmp"),
        "sandbox e2e home must not be covered by the /tmp tmpfs: {}",
        home.path().display()
    );
    home
}

fn run_owned_helper(
    name: &str,
    home: &Path,
    sandbox: &str,
    configure: impl FnOnce(&mut std::process::Command),
) {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            name,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(HELPER_ENV, name)
        .env("HOME", home)
        .env("FOREMAN_SANDBOX", sandbox)
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("FOREMAN_SIBLING_REPOS");
    configure(&mut command);
    let out = command.output().expect("spawn owned sandbox helper");
    assert!(
        out.status.success(),
        "owned helper {name} failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn assert_owned_helper(name: &str) {
    assert_eq!(std::env::var(HELPER_ENV).as_deref(), Ok(name));
}

struct HelperHome(PathBuf);

impl HelperHome {
    fn from_environment() -> Self {
        Self(PathBuf::from(std::env::var_os("HOME").unwrap()))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

fn git(repo: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "sandbox test")
        .env("GIT_AUTHOR_EMAIL", "sandbox@example.com")
        .env("GIT_COMMITTER_NAME", "sandbox test")
        .env("GIT_COMMITTER_EMAIL", "sandbox@example.com")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("run git fixture command");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A fake `codex exec --json` that writes to two paths named by env vars,
/// then emits just enough JSONL for `CodexParser` to call the turn `Done`.
fn write_fixture(dir: &Path) -> PathBuf {
    let path = dir.join("fake-codex");
    support::write_executable(
        &path,
        "#!/bin/sh\n\
         echo hi > \"$PROBE_INSIDE\"\n\
         echo hi > \"$PROBE_OUTSIDE\"\n\
         if [ \"$EXPECT_HOME_MOUNT\" = 1 ]; then\n\
           grep -F \" $HOME \" /proc/self/mountinfo >/dev/null || exit 97\n\
         fi\n\
         echo '{\"type\":\"thread.started\",\"thread_id\":\"t1\"}'\n\
         echo '{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}'\n",
    );
    path
}

fn run_once(
    fixture: &Path,
    worktree: &Path,
    inside: &Path,
    outside: &Path,
    expect_home_mount: bool,
) -> StopReason {
    let driver = CodexDriver::new()
        .with_program(fixture.to_str().unwrap())
        .with_env("PROBE_INSIDE", inside.to_str().unwrap())
        .with_env("PROBE_OUTSIDE", outside.to_str().unwrap())
        .with_env(
            "EXPECT_HOME_MOUNT",
            if expect_home_mount { "1" } else { "0" },
        );
    let ws = Workspace {
        dir: worktree.to_path_buf(),
        verify_subdir: None,
    };
    let mut session = driver
        .start("do the thing", &ws, &Budget::default())
        .unwrap();
    while let Ok(Some(_)) = session.next_event(Duration::from_secs(10)) {}
    session.wait().unwrap().stop
}

#[test]
fn contained_write_outside_the_bind_set_does_not_reach_the_host() {
    if !bwrap_installed() {
        eprintln!("bwrap(1) not installed — skipping sandbox e2e test");
        return;
    }
    // A synthetic $HOME, wholly separate from the real one — this test
    // must never touch operator credentials, only its own tmpdir tree.
    let home = synthetic_home();
    let worktree = home.path().join("worktree");
    std::fs::create_dir_all(&worktree).unwrap();
    // The fixture "codex" binary lives under $HOME/.local/bin — the same
    // place the real codex binary resolves through (`~/.local/bin/codex`
    // symlinks into `~/.codex/`), and one of codex_spec's read-only
    // binds. A tempdir under the default /tmp would NOT be — `--tmpfs
    // /tmp` in `wrap()` is deliberately a fresh scratch area, so a
    // fixture binary living there would (correctly) vanish too.
    let local_bin = home.path().join(".local/bin");
    std::fs::create_dir_all(&local_bin).unwrap();
    let fixture = write_fixture(&local_bin);
    let inside = worktree.join("inside.txt");
    // Under the synthetic $HOME but NOT in codex_spec's bind list — stands
    // in for an unenumerated credential path.
    let outside = home.path().join("outside.txt");

    // CONTROL: sandbox off. Establishes the probe can actually produce a
    // host-visible write outside the worktree — without this, "the file
    // doesn't exist" in the sandboxed run below would prove nothing.
    let configure = |command: &mut std::process::Command| {
        command
            .env("SANDBOX_TEST_FIXTURE", &fixture)
            .env("SANDBOX_TEST_WORKTREE", &worktree)
            .env("SANDBOX_TEST_INSIDE", &inside)
            .env("SANDBOX_TEST_OUTSIDE", &outside);
    };
    run_owned_helper(
        "contained_write_run_once_owned_process",
        home.path(),
        "off",
        configure,
    );
    assert!(
        inside.exists(),
        "control: inside write should reach the host"
    );
    assert!(
        outside.exists(),
        "control: outside write should reach the host unsandboxed — \
         a probe that can't demonstrate a real write can't prove one was prevented"
    );

    std::fs::remove_file(&inside).unwrap();
    std::fs::remove_file(&outside).unwrap();

    // SANDBOXED: identical probe, FOREMAN_SANDBOX=bwrap.
    run_owned_helper(
        "contained_write_run_once_owned_process",
        home.path(),
        "bwrap",
        configure,
    );
    assert!(
        inside.exists(),
        "sandboxed: the worktree write must still persist — it's in the bound set"
    );
    assert!(
        !outside.exists(),
        "sandboxed: the outside write must NOT persist to the host — it happened, \
         but only inside the namespace's own tmpfs, which is discarded with the process"
    );
}

#[test]
#[ignore = "run only in the owned helper process"]
fn contained_write_run_once_owned_process() {
    assert_owned_helper("contained_write_run_once_owned_process");
    let path = |name| PathBuf::from(std::env::var_os(name).unwrap());
    let stop = run_once(
        &path("SANDBOX_TEST_FIXTURE"),
        &path("SANDBOX_TEST_WORKTREE"),
        &path("SANDBOX_TEST_INSIDE"),
        &path("SANDBOX_TEST_OUTSIDE"),
        std::env::var("FOREMAN_SANDBOX").as_deref() == Ok("bwrap"),
    );
    assert_eq!(stop, StopReason::Done);
}

/// A hook that merely exists is worthless: the regression behind task 25
/// was a `$HOME` tmpfs making the absolute hook binary/ledger/settings paths
/// vanish. This fixture uses the real native-install symlink shape, acts once
/// as Claude and once as GLM, proves it is inside the namespace, reads the
/// mounted settings and project manifest, and submits a Write call against
/// that gate settings file.
/// Only exit 2 counts as success; ENOENT (127) and accidental allow (0) both
/// fail distinctly.
#[test]
fn project_mode_claude_and_glm_hooks_deny_a_gate_path_write_by_effect() {
    if !bwrap_installed() {
        eprintln!("bwrap(1) not installed — skipping sandbox hook e2e test");
        return;
    }
    let home = synthetic_home();
    run_owned_helper(
        "project_mode_claude_and_glm_hooks_deny_a_gate_path_write_by_effect_owned_process",
        home.path(),
        "bwrap",
        |_| {},
    );
}

#[test]
#[ignore = "run only in the owned helper process"]
fn project_mode_claude_and_glm_hooks_deny_a_gate_path_write_by_effect_owned_process() {
    assert_owned_helper(
        "project_mode_claude_and_glm_hooks_deny_a_gate_path_write_by_effect_owned_process",
    );
    let home = HelperHome::from_environment();
    let worktree = home.path().join("worktree");
    let state = home.path().join(".cmctl/.foreman");
    let local_bin = home.path().join(".local/bin");
    let claude_versions = home.path().join(".local/share/claude/versions");
    let claude_state = home.path().join(".claude");
    std::fs::create_dir_all(&worktree).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::create_dir_all(&local_bin).unwrap();
    std::fs::create_dir_all(&claude_versions).unwrap();
    std::fs::create_dir_all(&claude_state).unwrap();
    std::fs::create_dir_all(home.path().join(".codex")).unwrap();
    std::fs::create_dir_all(home.path().join(".zcode")).unwrap();
    std::fs::write(claude_state.join("host-oauth-marker"), "claude-only").unwrap();
    std::fs::write(home.path().join(".claude.json"), "oauth\n").unwrap();
    std::fs::write(home.path().join(".codex/host-auth-marker"), "codex-only").unwrap();
    std::fs::write(home.path().join(".zcode/host-auth-marker"), "zai-file").unwrap();

    let project_repo = tempfile::tempdir().unwrap();
    git(project_repo.path(), &["init", "--initial-branch=main"]);
    std::fs::write(project_repo.path().join("README.md"), "fixture\n").unwrap();
    git(project_repo.path(), &["add", "README.md"]);
    git(project_repo.path(), &["commit", "-m", "fixture"]);

    let foreman = PathBuf::from(env!("CARGO_BIN_EXE_foreman"));
    let manifest_path = home.path().join("task25-project.mix");
    std::fs::write(
        &manifest_path,
        format!(
            "name: \"sandbox\"\nrepo: \"{}\"\ndb: \"ledger.db\"\ncache_dir: \"cache\"\ninstruction_pack: \"Sandbox fixture.\"\n",
            project_repo.path().display()
        ),
    )
    .unwrap();
    let init = std::process::Command::new(&foreman)
        .env_clear()
        .env("PATH", "/opt/cosmix/bin:/usr/bin:/bin")
        .env("HOME", "/nonexistent")
        .args(["--project"])
        .arg(&manifest_path)
        .arg("init")
        .output()
        .expect("initialise project ledger");
    assert!(
        init.status.success(),
        "project init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    let db = home
        .path()
        .join(".foreman-task25-project-sandbox/ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let task = ledger
        .add_task("sandbox hook", "prove denial", "impl", "low", &[], "none")
        .unwrap();
    let settings_path = state.join("policy-settings-task25-fixture.json");
    let settings = hook_settings(
        &PolicyContext {
            task_id: task,
            worktree: worktree.clone(),
            branch: Some(format!("task/{task}")),
            provider: "anthropic".into(),
            integration_base: "HEAD".into(),
            integration_branch: "main".into(),
            task_ref_template: "task/{id}".into(),
            package_manifest_template: Some(LEGACY_PACKAGE_MANIFEST_TEMPLATE.into()),
            restrict_manifest_edits: false,
            task_crates: Vec::new(),
        },
        &db,
        DbCreateMode::Never,
        Some(&manifest_path),
        &foreman,
    );
    std::fs::write(&settings_path, serde_json::to_string(&settings).unwrap()).unwrap();

    // Match the production native install exactly:
    // ~/.local/bin/claude -> ~/.local/share/claude/versions/<version>.
    // A regular fixture in ~/.local/bin would stay green if the target tree
    // disappeared from the bind set, which is the regression this test must
    // catch.
    let fixture_target = claude_versions.join("task25-fixture");
    support::write_executable(
        &fixture_target,
        r#"#!/bin/sh
set -eu
grep -F " $HOME " /proc/self/mountinfo >/dev/null || exit 97
test ! -e "$HOME/.codex/host-auth-marker"
test ! -e "$HOME/.zcode/host-auth-marker"
case "$EXPECTED_LANE" in
  claude)
    test -e "$HOME/.claude/host-oauth-marker"
    printf '%s\n' state > "$HOME/.claude/fixture-state"
    printf '%s\n' state >> "$HOME/.claude.json"
    ;;
  glm)
    test ! -e "$HOME/.claude/host-oauth-marker"
    test ! -e "$HOME/.claude.json"
    test "$ANTHROPIC_AUTH_TOKEN" = fixture-zai-token
    mkdir -p "$HOME/.claude"
    printf '%s\n' ephemeral > "$HOME/.claude/glm-state"
    ;;
  *) exit 98 ;;
esac
settings=
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--settings" ]; then settings=$2; shift 2; else shift; fi
done
test -n "$settings"
hook=$(sed -n 's/.*"command":"\([^"]*\)".*/\1/p' "$settings")
test -n "$hook"
payload=$(printf '{"tool_name":"Write","tool_input":{"file_path":"%s","content":"tamper"}}' "$settings")
set +e
printf '%s' "$payload" | /bin/sh -c "$hook" 2> "$GATE_STDERR"
status=$?
set -e
printf '%s\n' "$status" > "$GATE_STATUS"
[ "$status" -eq 2 ] || exit 91
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"gate denied","usage":{"input_tokens":1,"output_tokens":1}}'
"#,
    );
    let fixture = local_bin.join("claude");
    symlink(&fixture_target, &fixture).unwrap();

    for (kind, lane, driver) in [
        (AgentKind::Claude, "claude", ClaudeDriver::new()),
        (
            AgentKind::Glm,
            "glm",
            ClaudeDriver::glm("fixture-zai-token"),
        ),
    ] {
        let status_path = worktree.join(format!("gate-status-{lane}"));
        let stderr_path = worktree.join(format!("gate-stderr-{lane}"));
        let driver = driver
            .with_program(fixture.to_string_lossy())
            .with_extra_args(vec![
                "--settings".into(),
                settings_path.to_string_lossy().into_owned(),
            ])
            .with_hook_mounts(Some(HookMounts {
                foreman_bin: foreman.clone(),
                ledger: db.clone(),
                settings: settings_path.clone(),
                project: Some(ProjectHookMounts {
                    manifest: manifest_path.canonicalize().unwrap(),
                    repo: project_repo.path().canonicalize().unwrap(),
                    git_common_dir: project_repo.path().join(".git").canonicalize().unwrap(),
                }),
            }))
            .with_env("EXPECTED_LANE", lane)
            .with_env("GATE_STATUS", status_path.to_string_lossy())
            .with_env("GATE_STDERR", stderr_path.to_string_lossy());
        let ws = Workspace {
            dir: worktree.clone(),
            verify_subdir: None,
        };
        let mut session = driver
            .start("attempt the write", &ws, &Budget::default())
            .unwrap();
        while let Ok(Some(_)) = session.next_event(Duration::from_secs(10)) {}
        let outcome = session.wait().unwrap();

        assert_eq!(driver.kind(), kind);
        assert_eq!(outcome.stop, StopReason::Done, "{lane}: {outcome:?}");
        assert_eq!(
            std::fs::read_to_string(&status_path).unwrap().trim(),
            "2",
            "{lane}"
        );
        let stderr = std::fs::read_to_string(&stderr_path).unwrap();
        assert!(
            stderr.contains("policy denied"),
            "{lane} hook stderr: {stderr}"
        );
        assert!(
            stderr.contains("agents never modify their gates"),
            "{lane}: {stderr}"
        );
    }

    assert_eq!(
        std::fs::read_to_string(claude_state.join("fixture-state"))
            .unwrap()
            .trim(),
        "state",
        "Claude's mutable state must persist through its writable bind"
    );
    assert!(
        std::fs::read_to_string(home.path().join(".claude.json"))
            .unwrap()
            .ends_with("state\n"),
        "Claude's top-level mutable state file must also persist"
    );
    assert!(
        !claude_state.join("glm-state").exists(),
        "GLM may create ephemeral CLI state but must not reach Claude's host state"
    );
    assert!(
        std::fs::read_to_string(&settings_path)
            .unwrap()
            .contains("PreToolUse"),
        "the denied attempt must not alter its settings target"
    );
    let denials = ledger
        .open_findings(20)
        .unwrap()
        .into_iter()
        .filter(|(_, finding_task, _, title, _)| {
            *finding_task == Some(task) && title == "policy deny"
        })
        .count();
    assert_eq!(
        denials, 1,
        "both project-mode hooks must reach one deduplicated denial in the host ledger"
    );
}

#[test]
fn an_unmounted_project_manifest_fails_closed_with_exit_two() {
    if !bwrap_installed() {
        eprintln!("bwrap(1) not installed — skipping sandbox startup-failure test");
        return;
    }

    let root = synthetic_home();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let manifest = home.join("deliberately-unmounted.mix");
    std::fs::write(&manifest, "this host file is hidden by the HOME tmpfs\n").unwrap();
    let foreman = PathBuf::from(env!("CARGO_BIN_EXE_foreman"));

    let mut policy = std::process::Command::new(&foreman);
    policy
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .args(["--project"])
        .arg(&manifest)
        .arg("--db")
        .arg(home.join("hidden-ledger.db"))
        .args([
            "--db-create",
            "never",
            "policy-check",
            "--task",
            "1",
            "--worktree",
        ])
        .arg("/usr")
        .args(["--provider", "anthropic", "--integration-base", "HEAD"])
        .current_dir("/usr");
    let spec = SandboxSpec {
        home,
        required_readable: vec![foreman],
        ..Default::default()
    };
    let output = wrap(policy, &spec)
        .output()
        .expect("run contained policy check");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(2), "hook stderr: {stderr}");
    assert!(
        stderr.contains("foreman policy: startup failed")
            && stderr.contains("deliberately-unmounted.mix")
            && stderr.contains("denying"),
        "startup refusal must clearly identify the missing manifest and denial: {stderr}"
    );
}

#[test]
fn a_ledger_open_failure_fails_closed_with_exit_two() {
    let tmp = tempfile::tempdir().unwrap();
    let bad_ledger = tmp.path().join("ledger-is-a-directory");
    std::fs::create_dir(&bad_ledger).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_foreman"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", "/nonexistent")
        .arg("--db")
        .arg(&bad_ledger)
        .args([
            "--db-create",
            "never",
            "policy-check",
            "--task",
            "1",
            "--worktree",
            "/usr",
            "--provider",
            "anthropic",
            "--integration-base",
            "HEAD",
        ])
        .output()
        .expect("run policy check with unusable ledger");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(2), "hook stderr: {stderr}");
    assert!(
        stderr.contains("foreman policy: startup failed")
            && stderr.contains("ledger-is-a-directory")
            && stderr.contains("denying"),
        "ledger refusal must clearly identify the startup failure and denial: {stderr}"
    );
}

#[test]
fn escaping_the_namespace_with_umount_is_denied_and_the_hidden_path_stays_hidden() {
    if !bwrap_installed() {
        eprintln!("bwrap(1) not installed — skipping sandbox escape test");
        return;
    }

    let home = synthetic_home();
    let worktree = home.path().join("worktree");
    std::fs::create_dir_all(&worktree).unwrap();
    // A "secret" that exists on the host under $HOME but is never bound —
    // stands in for the Z.ai key / ledger.db / canonical checkouts.
    let secret = home.path().join("credential");
    std::fs::write(&secret, "top-secret").unwrap();

    let spec = SandboxSpec {
        home: home.path().to_path_buf(),
        writable: vec![worktree],
        readable: vec![],
        ..Default::default()
    };
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.arg("-c").arg(format!(
        "cat {secret} 2>&1; echo ---; umount {home} 2>&1; echo ---; cat {secret} 2>&1",
        secret = shell_quote(&secret),
        home = shell_quote(home.path()),
    ));
    let mut bw = wrap(cmd, &spec);
    let out = bw.output().expect("spawning bwrap");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let combined = format!("{stdout}{}", String::from_utf8_lossy(&out.stderr));

    let sections: Vec<&str> = combined.split("---").collect();
    assert_eq!(
        sections.len(),
        3,
        "expected three probe sections, got: {combined:?}"
    );
    assert!(
        sections[0].contains("No such file or directory"),
        "the secret must be invisible before any escape attempt: {combined:?}"
    );
    assert!(
        sections[1].to_lowercase().contains("superuser")
            || sections[1].contains("Operation not permitted"),
        "umount from inside the namespace must be refused, not merely absent: {combined:?}"
    );
    assert!(
        sections[2].contains("No such file or directory"),
        "the secret must STAY hidden after the denied escape attempt: {combined:?}"
    );
}

#[test]
fn host_user_runtime_dir_is_not_visible_inside_the_namespace() {
    if !bwrap_installed() {
        eprintln!("bwrap(1) not installed — skipping runtime-dir isolation test");
        return;
    }

    let home = synthetic_home();
    let spec = SandboxSpec {
        home: home.path().to_path_buf(),
        writable: vec![],
        readable: vec![],
        ..Default::default()
    };
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.arg("-c").arg("test ! -e /run/user/\"$(id -u)\"");
    let mut bw = wrap(cmd, &spec);
    let out = bw.output().expect("spawning bwrap");
    assert!(
        out.status.success(),
        "/run/user/<uid> must not exist inside the namespace; stdout={:?}, stderr={:?}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn shell_quote(p: &Path) -> String {
    format!("'{}'", p.display().to_string().replace('\'', r"'\''"))
}
