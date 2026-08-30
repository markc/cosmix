//! Mount-namespace filesystem containment for a dispatched agent child.
//!
//! Increment 14a introduced this for Codex; task 25 composes the Claude
//! PreToolUse hook into the same view and extends it to the Claude and GLM
//! lanes. Opt-in via `FOREMAN_SANDBOX`, default OFF per the agentic-first
//! law (2026-08-16): the guard rail is a flag an operator turns on after
//! soak, not a default this crate flips for itself.
//!
//! # Why bwrap, not `unshare -r`
//!
//! `unshare -U -m -r` maps the invoking user to root INSIDE the new
//! namespace, which hands the payload `CAP_SYS_ADMIN` there — proven live
//! (2026-08-19 recon) to let the sandboxed process `umount` its own
//! containment and read what it hid. bwrap builds the mount view
//! privileged and drops ALL capabilities before exec: `CapBnd` reads
//! all-zero inside, so the same `umount`/`mount` calls fail with "must be
//! superuser" instead of succeeding. Containment a payload can switch off
//! is not containment.
//!
//! # Allowlist, not denylist
//!
//! [`SandboxSpec`] is an allowlist: a tmpfs replaces `$HOME` and every
//! other path an agent might read comes from the same tmpfs unless
//! explicitly bound. An unenumerated credential is invisible by
//! construction — a denylist instead would need to keep discovering every
//! future secret path, and a miss fails open.
//!
//! # The write set is small because sibling deps already live in-fleet
//!
//! cos's `../../../../`-style sibling path deps (`cosmix-lib-bus`,
//! `cosmix-lib-mix`, …) resolve into the fleet home's own clones
//! ([`crate::refinery::SIBLING_REPOS_ENV`]), not the operator's canonical
//! `~/.cos`/`~/.bus`/`~/.mix` checkouts — verified live 2026-08-19. So the
//! canonical checkouts need not be bound at all, which is a correctness
//! win as much as a secrecy one: a live breach that same night put an
//! earlier agent's uncommitted edits into the canonical `~/.cos`, stalling
//! the merge queue's publish step for hours before an operator noticed.
//! Hiding the canonical trees at the kernel closes that channel outright —
//! no tool-call gate has to enumerate it.
//!
//! # Fixed system view and limits
//!
//! The payload also sees read-only `/usr` and `/etc`, `/dev`, `/proc`, the
//! systemd-resolved directory needed by `/etc/resolv.conf`, and a fresh
//! `/tmp`. It does NOT see `/run/user/<uid>`: binding all of `/run` would
//! expose the host user bus and let a payload ask `systemd --user` to launch
//! a process outside this mount namespace. This increment confines the
//! filesystem view only; it does not create a PID namespace or prevent
//! same-UID signalling of host processes.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::executor::AgentKind;

/// How `FOREMAN_SANDBOX` is set. Off is the default until soaked; `bwrap`
/// is the only mechanism this increment implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    Off,
    Bwrap,
}

/// Read `FOREMAN_SANDBOX` (`bwrap` | `off`, case-insensitive; unset or
/// empty is `off`). An unrecognized value refuses rather than silently
/// running unsandboxed — a typo in an opt-in flag must not read as "the
/// operator chose off".
pub fn mode() -> Result<SandboxMode> {
    mode_from(std::env::var_os("FOREMAN_SANDBOX"))
}

fn mode_from(value: Option<std::ffi::OsString>) -> Result<SandboxMode> {
    match value.and_then(|value| value.into_string().ok()) {
        None => Ok(SandboxMode::Off),
        Some(v) if v.is_empty() || v.eq_ignore_ascii_case("off") => Ok(SandboxMode::Off),
        Some(v) if v.eq_ignore_ascii_case("bwrap") => Ok(SandboxMode::Bwrap),
        Some(other) => anyhow::bail!("unknown FOREMAN_SANDBOX={other:?} (want bwrap|off)"),
    }
}

/// A filesystem allowlist: `home` is replaced by an empty tmpfs, then
/// the optional and required write/read sets are bound back explicitly.
/// Optional paths are skipped when absent; required paths always reach
/// bubblewrap's argv so a source disappearing after validation fails hard.
/// Everything else under `home` — and everything outside the fixed system
/// paths [`wrap`] always binds — simply does not exist inside the namespace.
#[derive(Debug, Clone, Default)]
pub struct SandboxSpec {
    pub home: PathBuf,
    pub writable: Vec<PathBuf>,
    pub readable: Vec<PathBuf>,
    pub required_writable: Vec<PathBuf>,
    pub required_readable: Vec<PathBuf>,
}

/// Host paths baked into one Claude/GLM invocation's policy hook. They must
/// be mounted at the SAME absolute paths because Claude executes the command
/// string from [`crate::policy::hook_settings`] verbatim.
#[derive(Debug, Clone)]
pub struct HookMounts {
    pub foreman_bin: PathBuf,
    pub ledger: PathBuf,
    pub settings: PathBuf,
    pub project: Option<ProjectHookMounts>,
}

/// Extra startup inputs baked into a project-mode policy child. Loading the
/// manifest canonicalises and identifies its repository with Git before the
/// policy check can open the project-bound ledger.
#[derive(Debug, Clone)]
pub struct ProjectHookMounts {
    pub manifest: PathBuf,
    pub repo: PathBuf,
    pub git_common_dir: PathBuf,
}

impl HookMounts {
    /// Refuse a contained launch unless every path baked into the hook command
    /// can actually be mounted. These sources are the gate, not optional
    /// convenience grants: silently dropping one can leave Claude with a hook
    /// configuration that exists but cannot produce a verdict.
    fn validate(&self) -> Result<()> {
        for (label, path) in [
            ("foreman hook executable", &self.foreman_bin),
            ("Claude policy settings", &self.settings),
        ] {
            validate_file(label, path, || File::open(path).map(drop))?;
        }
        validate_file("foreman ledger", &self.ledger, || {
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(&self.ledger)
                .map(drop)
        })?;
        if let Some(project) = &self.project {
            validate_file("project manifest", &project.manifest, || {
                File::open(&project.manifest).map(drop)
            })?;
            for (label, path) in [
                ("project repository", &project.repo),
                ("project Git common directory", &project.git_common_dir),
            ] {
                validate_directory(label, path)?;
            }
        }
        Ok(())
    }
}

fn validate_path(label: &str, path: &Path) -> Result<std::fs::Metadata> {
    anyhow::ensure!(
        path.is_absolute(),
        "FOREMAN_SANDBOX=bwrap requires an absolute {label} path, got {}",
        path.display()
    );
    std::fs::metadata(path).with_context(|| {
        format!(
            "FOREMAN_SANDBOX=bwrap requires {label} at {}",
            path.display()
        )
    })
}

fn validate_file(
    label: &str,
    path: &Path,
    open: impl FnOnce() -> std::io::Result<()>,
) -> Result<()> {
    let metadata = validate_path(label, path)?;
    anyhow::ensure!(
        metadata.is_file(),
        "FOREMAN_SANDBOX=bwrap requires {label} to be a file: {}",
        path.display()
    );
    open().with_context(|| {
        format!(
            "FOREMAN_SANDBOX=bwrap cannot access {label} at {}",
            path.display()
        )
    })
}

fn validate_directory(label: &str, path: &Path) -> Result<()> {
    let metadata = validate_path(label, path)?;
    anyhow::ensure!(
        metadata.is_dir(),
        "FOREMAN_SANDBOX=bwrap requires {label} to be a directory: {}",
        path.display()
    );
    let inaccessible = || {
        format!(
            "FOREMAN_SANDBOX=bwrap cannot read {label} at {}",
            path.display()
        )
    };
    if let Some(entry) = std::fs::read_dir(path).with_context(inaccessible)?.next() {
        entry.with_context(|| {
            format!(
                "FOREMAN_SANDBOX=bwrap cannot read {label} at {}",
                path.display()
            )
        })?;
    }
    Ok(())
}

/// A lane's spec: the task worktree + the git common dir (a
/// worktree's `.git` is a file pointing at the MAIN checkout's, where
/// commits actually write objects and refs) + `$CARGO_TARGET_DIR` +
/// `~/.cargo` (registry fetch writes there) are writable; `~/.rustup`
/// (toolchain), only that lane's own credential/state, `~/.local/bin` +
/// `/opt/cosmix/bin` (the vendor and Mix entrypoints), and the fleet's sibling
/// dep clones ([`crate::refinery::SIBLING_REPOS_ENV`]) are bound back. Claude
/// and GLM also see the read-only native Claude installation that their
/// `~/.local/bin/claude` symlink targets. A Claude/GLM policy hook additionally
/// receives its explicit hook mounts. Project mode additionally mounts the
/// manifest and the repository paths its startup identity check reads.
pub fn lane_spec(
    kind: AgentKind,
    ws_dir: &Path,
    sibling_repos: Option<&str>,
    hook: Option<&HookMounts>,
) -> SandboxSpec {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/root"));
    let target = crate::gc::resolve_target_dir(None).ok();
    lane_spec_with_paths(kind, ws_dir, sibling_repos, hook, home, target)
}

fn lane_spec_with_paths(
    kind: AgentKind,
    ws_dir: &Path,
    sibling_repos: Option<&str>,
    hook: Option<&HookMounts>,
    home: PathBuf,
    target: Option<PathBuf>,
) -> SandboxSpec {
    let mut writable = vec![ws_dir.to_path_buf()];
    if let Some(git_dir) = crate::driver::codex::git_common_dir(ws_dir) {
        writable.push(PathBuf::from(git_dir));
    }
    if let Some(target) = target {
        writable.push(target);
    }
    writable.push(home.join(".cargo"));

    let mut readable = vec![
        home.join(".rustup"),
        home.join(".local/bin"),
        PathBuf::from("/opt/cosmix/bin"),
    ];
    let mut required_writable = Vec::new();
    let mut required_readable = Vec::new();
    match kind {
        AgentKind::Codex => readable.push(home.join(".codex")),
        AgentKind::Claude => {
            // Claude stores OAuth plus live session state in both paths and
            // updates them during ordinary runs. They belong only to this
            // lane, but must be writable for the real CLI to function.
            writable.push(home.join(".claude"));
            writable.push(home.join(".claude.json"));
            readable.push(home.join(".local/share/claude"));
        }
        // GLM authenticates with its Z.ai token in the child's environment.
        // It must not inherit Claude's stored OAuth credential merely because
        // both lanes happen to use the same CLI executable.
        AgentKind::Glm => readable.push(home.join(".local/share/claude")),
    }
    if let Some(spec) = sibling_repos {
        readable.extend(std::env::split_paths(spec));
    }

    if let Some(hook) = hook {
        required_readable.push(hook.foreman_bin.clone());
        required_readable.push(hook.settings.clone());
        if let Some(project) = &hook.project {
            required_readable.push(project.manifest.clone());
            for path in [&project.repo, &project.git_common_dir] {
                if !writable.iter().any(|bind| path.starts_with(bind))
                    && !required_writable.iter().any(|bind| path.starts_with(bind))
                {
                    required_readable.push(path.clone());
                }
            }
        }

        // This is deliberately writable: policy-check records denials and
        // escalations in the shared ledger. It also means a shell inside the
        // namespace can tamper with ledger.db. That is already possible in an
        // unsandboxed run, but it matters to the claim: mount containment does
        // NOT subsume the per-tool-call gate-path rail. The policy hook must
        // continue denying agent writes to the ledger even though its own
        // subprocess needs write access here.
        required_writable.push(hook.ledger.clone());
        // The parent keeps SQLite in WAL mode. Bind any live sidecars too so
        // the hook observes the same database rather than private tmpfs files.
        writable.extend(sqlite_sidecars(&hook.ledger));
    }

    SandboxSpec {
        home,
        writable,
        readable,
        required_writable,
        required_readable,
    }
}

fn sqlite_sidecars(db: &Path) -> [PathBuf; 2] {
    let db = db.as_os_str().to_string_lossy();
    [
        PathBuf::from(format!("{db}-wal")),
        PathBuf::from(format!("{db}-shm")),
    ]
}

/// Backwards-compatible spelling for callers/tests concerned specifically
/// with the original Codex lane.
pub fn codex_spec(ws_dir: &Path, sibling_repos: Option<&str>) -> SandboxSpec {
    lane_spec(AgentKind::Codex, ws_dir, sibling_repos, None)
}

/// Wrap `cmd` so it runs under `bwrap` inside the view `spec` describes.
///
/// Pure — returns a new `Command` (`bwrap`, with `cmd`'s program, args and
/// env changes carried over as its payload) rather than spawning anything,
/// so the argv is testable the same way `CodexDriver::args_for` split the
/// grant from the spawn.
///
/// Ordering is load-bearing: call this AFTER every `env`/`env_remove` on
/// `cmd` — bwrap inherits whatever environment its own process has, and
/// this function replays `cmd`'s recorded env changes onto the `bwrap`
/// process so they still apply to the sandboxed child.
pub fn wrap(cmd: Command, spec: &SandboxSpec) -> Command {
    let mut bw = Command::new("bwrap");
    bw.arg("--die-with-parent")
        .arg("--dev")
        .arg("/dev")
        .arg("--proc")
        .arg("/proc")
        .arg("--tmpfs")
        .arg(&spec.home)
        .arg("--ro-bind")
        .arg("/usr")
        .arg("/usr")
        .arg("--symlink")
        .arg("usr/bin")
        .arg("/bin")
        .arg("--symlink")
        .arg("usr/lib")
        .arg("/lib")
        .arg("--symlink")
        .arg("usr/lib")
        .arg("/lib64")
        .arg("--symlink")
        .arg("usr/bin")
        .arg("/sbin")
        .arg("--ro-bind")
        .arg("/etc")
        .arg("/etc")
        .arg("--tmpfs")
        .arg("/tmp");

    // /etc/resolv.conf is a symlink into this directory on the fleet host
    // family. Bind only the resolver files, not all of /run: /run/user/<uid>
    // carries the host user bus and agent sockets, and exposing the bus lets
    // a payload ask systemd --user to execute outside this namespace.
    let resolver = Path::new("/run/systemd/resolve");
    if resolver.exists() {
        bw.arg("--ro-bind").arg(resolver).arg(resolver);
    }

    bind_existing(&mut bw, "--bind", &spec.writable);
    bind_required(&mut bw, "--bind", &spec.required_writable);
    bind_existing(&mut bw, "--ro-bind", &spec.readable);
    bind_required(&mut bw, "--ro-bind", &spec.required_readable);

    if let Some(dir) = cmd.get_current_dir() {
        bw.arg("--chdir").arg(dir);
    }
    bw.arg("--");
    bw.arg(cmd.get_program());
    bw.args(cmd.get_args());
    for (key, val) in cmd.get_envs() {
        match val {
            Some(val) => {
                bw.env(key, val);
            }
            None => {
                bw.env_remove(key);
            }
        }
    }
    bw
}

/// Bind each path that exists on the host, deduplicated and in a stable
/// order (so the argv — and any test asserting on it — is deterministic).
/// A path that doesn't exist is silently dropped rather than handed to
/// bwrap, which refuses a missing bind source outright; an optional grant
/// (an override env var pointing nowhere, a toolchain dir absent on this
/// host) must not turn into a hard launch failure.
fn bind_existing(bw: &mut Command, flag: &str, paths: &[PathBuf]) {
    let mut existing: Vec<&PathBuf> = paths.iter().filter(|p| p.exists()).collect();
    existing.sort();
    existing.dedup();
    for path in existing {
        bw.arg(flag).arg(path).arg(path);
    }
}

/// Bind mandatory paths without another existence filter. [`HookMounts`]
/// validates them before command construction; if one is removed afterwards,
/// bubblewrap must refuse rather than launch a view with part of its gate
/// silently absent.
fn bind_required(bw: &mut Command, flag: &str, paths: &[PathBuf]) {
    let mut required: Vec<&PathBuf> = paths.iter().collect();
    required.sort();
    required.dedup();
    for path in required {
        bw.arg(flag).arg(path).arg(path);
    }
}

/// Whether `bwrap` is on `PATH` — checked once so a missing binary refuses
/// with a clear message instead of a raw ENOENT out of `Command::spawn`.
pub fn bwrap_available() -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join("bwrap").is_file()))
        .unwrap_or(false)
}

/// Build the sandboxed `Command` for `cmd` per [`mode`], or hand `cmd` back
/// unchanged when the mode is off. Called at the same point every driver's
/// `start()` finishes its own env scrubbing — see [`wrap`]'s ordering note.
pub fn apply(
    cmd: Command,
    kind: AgentKind,
    ws_dir: &Path,
    sibling_repos: Option<&str>,
    hook: Option<&HookMounts>,
) -> Result<Command> {
    let mode = mode()?;
    let bwrap_available = mode != SandboxMode::Bwrap || bwrap_available();
    apply_with(
        mode,
        bwrap_available,
        cmd,
        kind,
        ws_dir,
        sibling_repos,
        hook,
    )
}

fn apply_with(
    mode: SandboxMode,
    bwrap_available: bool,
    cmd: Command,
    kind: AgentKind,
    ws_dir: &Path,
    sibling_repos: Option<&str>,
    hook: Option<&HookMounts>,
) -> Result<Command> {
    match mode {
        SandboxMode::Off => Ok(cmd),
        SandboxMode::Bwrap => {
            if let Some(hook) = hook {
                anyhow::ensure!(
                    kind != AgentKind::Codex,
                    "a Claude policy hook cannot be mounted into the codex lane"
                );
                hook.validate()?;
            }
            anyhow::ensure!(
                bwrap_available,
                "FOREMAN_SANDBOX=bwrap but bwrap(1) is not on PATH"
            );
            let spec = lane_spec(kind, ws_dir, sibling_repos, hook);
            Ok(wrap(cmd, &spec))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(cmd: &Command) -> Vec<String> {
        std::iter::once(cmd.get_program())
            .chain(cmd.get_args())
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    }

    fn all_binds(spec: &SandboxSpec) -> impl Iterator<Item = &PathBuf> {
        spec.writable
            .iter()
            .chain(spec.required_writable.iter())
            .chain(spec.readable.iter())
            .chain(spec.required_readable.iter())
    }

    #[test]
    fn mode_defaults_to_off() {
        assert_eq!(mode_from(None).unwrap(), SandboxMode::Off);
        assert_eq!(mode_from(Some("".into())).unwrap(), SandboxMode::Off);
    }

    #[test]
    fn mode_parses_bwrap_case_insensitively() {
        assert_eq!(mode_from(Some("BWrap".into())).unwrap(), SandboxMode::Bwrap);
    }

    #[test]
    fn mode_refuses_an_unknown_value() {
        assert!(mode_from(Some("chroot".into())).is_err());
    }

    #[test]
    fn wrap_carries_the_payload_program_and_args_after_the_separator() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cmd = Command::new("codex");
        cmd.arg("exec").arg("--json").current_dir(tmp.path());
        let spec = SandboxSpec {
            home: tmp.path().to_path_buf(),
            writable: vec![tmp.path().to_path_buf()],
            readable: vec![],
            ..Default::default()
        };
        let bw = wrap(cmd, &spec);
        assert_eq!(bw.get_program().to_str().unwrap(), "bwrap");
        let a = argv(&bw);
        let sep = a
            .iter()
            .position(|s| s == "--")
            .expect("has a -- separator");
        assert_eq!(&a[sep + 1..], &["codex", "exec", "--json"]);
    }

    #[test]
    fn wrap_binds_the_home_tmpfs_before_the_writable_and_readable_sets() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join("worktree");
        let secret_dir = tmp.path().join("secret-parent");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(&secret_dir).unwrap();
        let spec = SandboxSpec {
            home: tmp.path().to_path_buf(),
            writable: vec![wt.clone()],
            readable: vec![secret_dir.clone()],
            ..Default::default()
        };
        let bw = wrap(Command::new("true"), &spec);
        let a = argv(&bw);
        assert!(
            a.windows(2)
                .any(|w| w[0] == "--tmpfs" && w[1] == tmp.path().to_str().unwrap())
        );
        assert!(
            a.windows(3)
                .any(|w| w[0] == "--bind" && w[1] == wt.to_str().unwrap())
        );
        assert!(
            a.windows(3)
                .any(|w| w[0] == "--ro-bind" && w[1] == secret_dir.to_str().unwrap())
        );
    }

    #[test]
    fn wrap_binds_only_the_resolver_part_of_run() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = SandboxSpec {
            home: tmp.path().to_path_buf(),
            writable: vec![],
            readable: vec![],
            ..Default::default()
        };
        let bw = wrap(Command::new("true"), &spec);
        let a = argv(&bw);
        assert!(
            !a.windows(3)
                .any(|w| w[0] == "--ro-bind" && w[1] == "/run" && w[2] == "/run"),
            "the whole /run tree must never be exposed: {a:?}"
        );
        if Path::new("/run/systemd/resolve").exists() {
            assert!(
                a.windows(3).any(|w| {
                    w[0] == "--ro-bind"
                        && w[1] == "/run/systemd/resolve"
                        && w[2] == "/run/systemd/resolve"
                }),
                "the resolver directory must remain readable: {a:?}"
            );
        }
    }

    #[test]
    fn wrap_drops_a_writable_or_readable_entry_that_does_not_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let spec = SandboxSpec {
            home: tmp.path().to_path_buf(),
            writable: vec![missing.clone()],
            readable: vec![missing.clone()],
            ..Default::default()
        };
        let bw = wrap(Command::new("true"), &spec);
        let a = argv(&bw);
        assert!(
            !a.contains(&missing.to_string_lossy().into_owned()),
            "a nonexistent bind source must not reach bwrap's argv: {a:?}"
        );
    }

    #[test]
    fn wrap_never_drops_a_missing_required_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing-policy-settings.json");
        let spec = SandboxSpec {
            home: tmp.path().to_path_buf(),
            required_readable: vec![missing.clone()],
            ..Default::default()
        };
        let bw = wrap(Command::new("true"), &spec);
        let a = argv(&bw);
        assert!(
            a.windows(3).any(|w| {
                w[0] == "--ro-bind"
                    && w[1] == missing.to_string_lossy()
                    && w[2] == missing.to_string_lossy()
            }),
            "a required source must reach bwrap and fail hard if absent: {a:?}"
        );
    }

    #[test]
    fn wrap_replays_env_remove_from_the_wrapped_command() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cmd = Command::new("true");
        cmd.env_remove("SANDBOX_TEST_SECRET");
        let spec = SandboxSpec {
            home: tmp.path().to_path_buf(),
            writable: vec![],
            readable: vec![],
            ..Default::default()
        };
        let bw = wrap(cmd, &spec);
        assert!(
            bw.get_envs()
                .any(|(k, v)| k == "SANDBOX_TEST_SECRET" && v.is_none()),
            "env_remove on the wrapped command must carry over to bwrap's own env"
        );
    }

    #[test]
    fn apply_is_a_passthrough_when_the_mode_is_off() {
        let tmp = tempfile::tempdir().unwrap();
        let cmd = Command::new("true");
        let out = apply_with(
            SandboxMode::Off,
            false,
            cmd,
            AgentKind::Codex,
            tmp.path(),
            None,
            None,
        )
        .unwrap();
        assert_eq!(out.get_program().to_str().unwrap(), "true");
    }

    #[test]
    fn apply_refuses_each_missing_mandatory_hook_mount() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = [
            tmp.path().join("foreman"),
            tmp.path().join("ledger.db"),
            tmp.path().join("policy-settings-task25.json"),
            tmp.path().join("project.mix"),
        ];
        for path in &paths {
            std::fs::write(path, "fixture").unwrap();
        }
        let repo = tmp.path().join("project-repo");
        let git_common_dir = repo.join(".git");
        std::fs::create_dir_all(&git_common_dir).unwrap();

        for missing in 0..paths.len() {
            std::fs::remove_file(&paths[missing]).unwrap();
            let err = apply_with(
                SandboxMode::Bwrap,
                true,
                Command::new("true"),
                AgentKind::Claude,
                tmp.path(),
                None,
                Some(&HookMounts {
                    foreman_bin: paths[0].clone(),
                    ledger: paths[1].clone(),
                    settings: paths[2].clone(),
                    project: Some(ProjectHookMounts {
                        manifest: paths[3].clone(),
                        repo: repo.clone(),
                        git_common_dir: git_common_dir.clone(),
                    }),
                }),
            )
            .expect_err("a missing hook source must refuse before bwrap spawn");
            let message = format!("{err:#}");
            assert!(
                message.contains(&paths[missing].display().to_string()),
                "missing path must be named in the refusal: {message}"
            );
            std::fs::write(&paths[missing], "fixture").unwrap();
        }
        for (missing, expected) in [(&git_common_dir, &git_common_dir), (&repo, &repo)] {
            std::fs::remove_dir_all(missing).unwrap();
            let err = apply_with(
                SandboxMode::Bwrap,
                true,
                Command::new("true"),
                AgentKind::Claude,
                tmp.path(),
                None,
                Some(&HookMounts {
                    foreman_bin: paths[0].clone(),
                    ledger: paths[1].clone(),
                    settings: paths[2].clone(),
                    project: Some(ProjectHookMounts {
                        manifest: paths[3].clone(),
                        repo: repo.clone(),
                        git_common_dir: git_common_dir.clone(),
                    }),
                }),
            )
            .expect_err("a missing project startup directory must refuse before bwrap spawn");
            assert!(
                format!("{err:#}").contains(&expected.display().to_string()),
                "missing project path must be named in the refusal: {err:#}"
            );
            std::fs::create_dir_all(&git_common_dir).unwrap();
        }
    }

    #[test]
    fn lane_specs_partition_credentials_and_mount_only_the_hook_inputs() {
        let home = tempfile::tempdir().unwrap();
        let ws = home.path().join("worktree");
        let state = home.path().join(".cmctl/.foreman");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(home.path().join(".codex")).unwrap();
        std::fs::create_dir_all(home.path().join(".claude")).unwrap();
        std::fs::create_dir_all(home.path().join(".local/share/claude/versions")).unwrap();
        std::fs::create_dir_all(home.path().join(".zcode")).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(home.path().join(".claude.json"), "oauth").unwrap();
        std::fs::write(state.join("env"), "ZAI_API_KEY=secret").unwrap();
        let ledger = state.join("ledger.db");
        let settings = state.join("policy-settings-task25-1.json");
        let manifest = state.join("project.mix");
        let project_repo = home.path().join("project-repo");
        let project_git_common = project_repo.join(".git");
        let foreman = home.path().join(".local/bin/foreman");
        std::fs::create_dir_all(foreman.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&project_git_common).unwrap();
        for path in [&ledger, &settings, &manifest, &foreman] {
            std::fs::write(path, "fixture").unwrap();
        }

        let hook = HookMounts {
            foreman_bin: foreman.clone(),
            ledger: ledger.clone(),
            settings: settings.clone(),
            project: Some(ProjectHookMounts {
                manifest: manifest.clone(),
                repo: project_repo.clone(),
                git_common_dir: project_git_common.clone(),
            }),
        };
        let codex = lane_spec_with_paths(
            AgentKind::Codex,
            &ws,
            None,
            None,
            home.path().to_path_buf(),
            None,
        );
        let claude = lane_spec_with_paths(
            AgentKind::Claude,
            &ws,
            None,
            Some(&hook),
            home.path().to_path_buf(),
            None,
        );
        let glm = lane_spec_with_paths(
            AgentKind::Glm,
            &ws,
            None,
            Some(&hook),
            home.path().to_path_buf(),
            None,
        );

        assert!(codex.readable.contains(&home.path().join(".codex")));
        assert!(!codex.readable.contains(&home.path().join(".claude")));
        assert!(!codex.readable.contains(&home.path().join(".claude.json")));
        assert!(claude.writable.contains(&home.path().join(".claude")));
        assert!(claude.writable.contains(&home.path().join(".claude.json")));
        assert!(
            claude
                .readable
                .contains(&home.path().join(".local/share/claude"))
        );
        assert!(!claude.readable.contains(&home.path().join(".codex")));
        assert!(!glm.writable.contains(&home.path().join(".claude")));
        assert!(!glm.writable.contains(&home.path().join(".claude.json")));
        assert!(
            glm.readable
                .contains(&home.path().join(".local/share/claude"))
        );
        assert!(!glm.readable.contains(&home.path().join(".codex")));

        for spec in [&codex, &claude, &glm] {
            for forbidden in [home.path().join(".zcode"), state.join("env")] {
                assert!(
                    !all_binds(spec).any(|bind| forbidden.starts_with(bind)),
                    "{:?} lane bind covers forbidden path {}: {spec:?}",
                    spec.home,
                    forbidden.display()
                );
            }
        }

        for spec in [&claude, &glm] {
            assert!(spec.required_readable.contains(&foreman));
            assert!(spec.required_readable.contains(&settings));
            assert!(spec.required_readable.contains(&manifest));
            assert!(spec.required_readable.contains(&project_repo));
            assert!(spec.required_readable.contains(&project_git_common));
            assert!(spec.required_writable.contains(&ledger));
        }
        assert!(!codex.required_readable.contains(&foreman));
        assert!(!codex.required_readable.contains(&settings));
        assert!(!codex.required_writable.contains(&ledger));
    }

    /// The breach this increment exists to close, asserted directly on the
    /// bind set rather than left implicit in "well, we didn't add it".
    ///
    /// Live incident, 2026-08-19: task 12's agent wrote three files into the
    /// CANONICAL `~/.cos` checkout — outside its worktree entirely — which
    /// stalled the merge queue's publish step until an operator noticed. The
    /// policy hook was NOT broken; it denied that exact write on the path it
    /// could see (finding 142). The write simply arrived through a channel a
    /// per-tool-call gate never sees. A kernel-level view has no such
    /// channels: the canonical checkouts are not in the bind set, so they do
    /// not exist inside the namespace.
    ///
    /// That property is only free while nobody widens a bind. The failure
    /// mode this guards is a plausible future edit — binding `$HOME`, or
    /// `~/.cmctl` wholesale to "just let the build see the fleet" — which
    /// would silently restore reachability of every path below it and leave
    /// every other test here still green. So the check is an ANCESTOR test,
    /// not an equality test: a bind is a breach if it *covers* a forbidden
    /// path, however far above it.
    #[test]
    fn codex_spec_never_binds_the_canonical_checkouts_or_the_fleet_secrets() {
        let home = tempfile::tempdir().unwrap();
        let ws = home.path().join(".cmctl/.foreman/task-99");
        std::fs::create_dir_all(&ws).unwrap();

        // A realistic shared target dir, so the spec under test is the
        // production shape and not a degenerate one.
        let target = home.path().join(".cmctl/.foreman/target");
        // The sibling clones the build genuinely needs: inside the fleet
        // home, NOT the canonical checkouts. This is what makes hiding
        // `~/.cos` free rather than a trade-off. Production receives this
        // string from FleetPolicy rather than reading the live environment.
        let sibling_repos = std::env::join_paths([
            home.path().join(".cmctl/.foreman/.bus"),
            home.path().join(".cmctl/.foreman/.mix"),
        ])
        .unwrap();
        let spec = lane_spec_with_paths(
            AgentKind::Codex,
            &ws,
            sibling_repos.to_str(),
            None,
            home.path().to_path_buf(),
            Some(target),
        );

        let forbidden = [
            // The canonical checkouts — the actual 2026-08-19 breach target.
            ".cos",
            ".bus",
            ".mix",
            // Other lanes' credentials: the cross-lane isolation this buys.
            ".claude",
            ".zcode",
            ".ssh",
            // Fleet state: the Z.ai key and the ledger the agent is graded in.
            ".cmctl/.foreman/env",
            ".cmctl/.foreman/ledger.db",
        ];
        for rel in forbidden {
            let path = home.path().join(rel);
            for bind in all_binds(&spec) {
                assert!(
                    !path.starts_with(bind),
                    "codex_spec binds {} — which covers {}, a path no contained \
                     session may reach. A write into the canonical checkout is \
                     what stalled the merge queue on 2026-08-19.",
                    bind.display(),
                    path.display(),
                );
            }
        }

        // $HOME itself is the tmpfs root, never a bind: binding it would make
        // every assertion above vacuous in one line.
        assert!(
            !all_binds(&spec).any(|b| b == home.path()),
            "codex_spec must not bind $HOME itself — the tmpfs over it is the \
             whole allowlist"
        );
        // And the guard is only meaningful if the spec actually bound the
        // things the build needs; an empty spec would pass everything above.
        assert!(
            spec.writable.contains(&ws),
            "the worktree must still be writable: {:?}",
            spec.writable
        );
        assert!(
            spec.readable
                .contains(&home.path().join(".cmctl/.foreman/.bus")),
            "the sibling dep clones must still be readable: {:?}",
            spec.readable
        );
    }
}
