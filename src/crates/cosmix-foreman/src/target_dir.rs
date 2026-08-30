//! Cargo target-dir isolation against a shared `CARGO_TARGET_DIR`.
//!
//! # The bug this closes
//!
//! The fleet once built every task worktree, the refinery's throwaway clones,
//! and the installer's warm/probe checkouts into ONE `CARGO_TARGET_DIR`
//! (set so tier-0 fits its time cap on a large workspace — recompiling every
//! third-party dependency from cold in every sibling worktree would blow
//! that cap many times over). Cargo's unit-metadata hash for a *local path*
//! crate does not fold in the crate's absolute path, so two worktrees of the
//! same repo produce byte-identical hashes for the same crate — the shared
//! target dir is not an accident, cargo genuinely cannot tell them apart by
//! identity. What decides whether a `cargo` invocation reuses the existing
//! artifact or rebuilds is cargo's freshness check, which for local path
//! packages compares each source file's mtime against the mtime recorded
//! when the cached artifact was built — a check with no notion of WHICH
//! worktree wrote either mtime. Proven live 2026-08-22/23 (fleet task 44
//! evidence, three separate incidents): a `concurrency` test binary panicked
//! at a line number that only exists in another task's pre-fix source, an
//! installer probe executed a foreman build carrying a ledger schema version
//! that exists only on an unmerged branch, and a tier-0 run panicked on
//! paths belonging to an unrelated, already-deleted probe worktree.
//!
//! # The first fix attempt, and why it was rejected
//!
//! Task 44's first landing attempt (fleet task 44, 2026-08-22) touched every
//! local source file's mtime to `SystemTime::now()` before each verifier's
//! `cargo` steps, so cargo's freshness check could no longer read a stale
//! cache entry as "unchanged". Both review passes (Claude merge authority
//! and a cold codex review) rejected it as a BLOCKER: the touch closes the
//! window at the START of a run, but a fully concurrent writer — a raw
//! `cargo build` run by an agent's own shell, or another worktree's verifier
//! racing on the same host lane's blind spot — can still write a NEWER
//! artifact into the same shared slot WHILE this run's `cargo` steps
//! execute, and cargo's freshness check has no way to prefer "the one THIS
//! run just compiled" over "the one that happens to be newest". Two of the
//! three live incidents this module's docs cite were exactly that: a
//! concurrent codex session and a concurrently-building probe worktree, both
//! outside foreman's own verifier engine and therefore outside what a
//! source-touch at the start of a run could ever see coming.
//!
//! Touching mtimes can only ever narrow a race. It cannot close one.
//!
//! # The actual fix: don't share the directory
//!
//! Every child this crate spawns — every verifier step
//! ([`crate::verify::run_commands`] and its callers: tier 0, the refinery's
//! tier-1 pre-land gate, the nightly tier-2 unit) AND every agent session
//! this crate launches (the Claude, Codex and GLM drivers) — has the ambient
//! `CARGO_TARGET_DIR` replaced with a canonical private target immediately
//! before spawn, after every other environment overlay. Cargo's environment
//! value outranks `.cargo/config.toml`, so an earlier step cannot redirect a
//! later one by changing configuration after the initial target selection.
//! This is unconditional at the child boundary rather than
//! keyed to `argv[0] == "cargo"`, so `env cargo`, `memguard cargo`, a Mix
//! script, or `sh -c` cannot accidentally reintroduce the inherited value.
//! The selected path is inside the worktree under verification and only this
//! worktree's own cargo invocations write to it. No other worktree's cargo
//! process has a path to it, so there is no shared slot left to
//! collide on, race for, or launder through, at any point during a run —
//! not just at the moment a run starts.
//!
//! [`pinned_target_dir`] derives one path from the verifier's Cargo workspace
//! root. Immediately before EACH Cargo step, Foreman runs metadata with that
//! path pinned and requires Cargo to report the exact pin.
//! A mismatch refuses the step. For direct cargo commands and transparent
//! argv wrappers such as `env cargo`, the probe resolves Cargo from the
//! verifier process's own PATH, outside the worktree. It parses `env K=V`
//! assignments but never executes `env`, memguard, timeout, flock, a shell,
//! or any other wrapper while probing. Cargo's toolchain,
//! `--manifest-path`, `--config`, and `--target-dir` arguments are preserved.
//! The verifier pin is applied after parsed assignments, exactly as it is to
//! the real step. An opaque
//! shell string such as `sh -c 'cargo test'` cannot be transformed safely
//! and is refused unless that exact step is explicitly declared opaque by
//! its built-in profile. It REFUSES — a hard error, propagated by `?`, failing the run
//! closed — if the resolved target is not canonically contained in the
//! resolved workspace. That catches command-line overrides which outrank the
//! environment; committed Cargo config cannot win against the pin. The
//! first landing attempt's analogous check (`fenced_files == 0`) only
//! printed a warning and let the run continue; that was named directly as a
//! review finding ("an outstanding unknown is being recorded as a normal
//! zero and read as success") and is not repeated here. A `cargo metadata`
//! failure for an ordinary reason (no `Cargo.toml`, a malformed manifest —
//! [`TargetDirCheck::NotACargoProject`]) is deliberately NOT this hard
//! refusal: that is cargo genuinely having nothing to build, the same
//! failure any real `cargo` step in that directory would hit anyway, and it
//! is reported as an ordinary failed verify step rather than aborting the
//! run — the hard refusal is reserved for the one case that is actually
//! about isolation.
//!
//! # Cold dependency cost remains
//!
//! A private target dir per worktree pays the dependency graph's real cold
//! compile cost. Although this host has a global `sccache` rustc wrapper,
//! the 2026-08-23 measurement recorded zero new Rust hits: path-dependent
//! metadata prevents cross-worktree reuse today. Third-party Rust crates are
//! therefore NOT warm across worktrees via sccache. The specification's
//! shared read-only dependency cache is not achieved by this change and is
//! future work; the measured cold tier still fits the fleet cap.
//!
//! Cold round-5 measurement, 2026-08-24, used the rebuilt Foreman 0.13.0
//! implementation against an absent target in a fresh detached task-44
//! worktree: fmt 2.38s, clippy 116.39s, and test including pre/post provenance
//! 492.38s; 611.32s total. The test step retained 107.62s of its 600s deadline
//! and the tier retained 1788.68s of the fleet's 2400s cap. Sccache deltas:
//! 2315 requests, 1535 executed, 421 hits, 1110 misses (Rust 0/1061), and 774
//! non-cacheable calls. The 421 hits were assembler/C/C++. The temporary 12GB
//! target and worktree were removed. A cold dependency cache would add each
//! dependency's real compile once; that worst case was not measured.
//! From the task-worktree reuse release, the refinery normally verifies in the
//! completed task's own registered worktree, so this cold detached-target cost
//! is not repaid at landing. The detached path remains only for legacy/manual
//! branches without a task worktree.
//!
//! # Residual scope
//!
//! Every child process this crate is responsible for launching pins
//! `CARGO_TARGET_DIR` in its own environment: verifier steps
//! ([`crate::verify::run_step`]) and agent sessions (the Claude, Codex and
//! GLM drivers). Wrapped cargo therefore inherits the same private target as a
//! literal cargo command. Two things remain genuinely out of
//! reach, and are stated here rather than argued away — the first landing
//! attempt was rejected precisely for claiming a residue did not exist:
//!
//! 1. **`cargo` that foreman did not spawn.** A systemd unit or script that
//!    runs `cargo` directly is untouched by this crate. The installer's WARM
//!    action deliberately remains such a command with its explicit shared
//!    cache target: it warms the shared build cache and makes no
//!    verification claim. Its tier-0 PROBE is different: it runs `foreman
//!    verify`, which replaces the inherited variable itself. The operator hand-
//!    off therefore removes the unit setting only from dispatch, refine and
//!    tier 2; the runbook names those exact units.
//! 2. **A `cargo` invocation that sets its own `CARGO_TARGET_DIR`.** Typed
//!    into an agent's shell, pointed back at the shared path or somewhere
//!    else unsafe (2026-08-22's `/tmp` incident: four agents did exactly
//!    this, independently, and filled a 51 GB `/tmp` to EDQUOT). No
//!    environment-scoping mechanism can stop a value a process chooses for
//!    itself. What this change does do is remove the *reason* to: the
//!    agent-facing prompt ([`crate::lowering`]) says explicitly not to, and
//!    with the variable pinned there is no longer an ambient value pointing
//!    at a shared location for an agent to inherit, notice
//!    interfering with its build, and "fix" by inventing a private one under
//!    `/tmp`. That was the actual causal chain in the `/tmp` incident.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

/// The variable whose presence turns this from a no-op into an active
/// override: when unset, cargo's own default (`<workspace>/target`) is
/// already private per checkout and there is nothing to isolate.
pub const AMBIENT_TARGET_DIR_ENV: &str = "CARGO_TARGET_DIR";

/// Remove [`AMBIENT_TARGET_DIR_ENV`] from a child's environment.
/// Idempotent and safe to call even when the variable was never set — every
/// verifier step and agent session goes through this regardless of its
/// executable name, so wrappers which later reach cargo inherit the removal.
pub fn strip_ambient_target_dir(cmd: &mut Command) {
    cmd.env_remove(AMBIENT_TARGET_DIR_ENV);
}

/// Set a verifier-owned target directory on a child. Call this after every
/// caller/model overlay, immediately before spawn: Cargo's environment value
/// outranks `.cargo/config.toml`, so configuration changed by an earlier step
/// cannot redirect this child after its preflight.
pub fn pin_target_dir(cmd: &mut Command, target_dir: &Path) {
    cmd.env_remove(AMBIENT_TARGET_DIR_ENV);
    cmd.env(AMBIENT_TARGET_DIR_ENV, target_dir);
}

/// The one canonical Cargo target for a worktree and verifier subdirectory.
///
/// Agent drivers call this with the task worktree plus `--subdir`; the
/// verifier calls it with its already-resolved Cargo workspace and `None`.
/// Both therefore select the same `<worktree>/<subdir>/target` path. Existing
/// symlinks are resolved, and a workspace or target escape is refused.
pub fn pinned_target_dir(worktree: &Path, subdir: Option<&str>) -> Result<PathBuf> {
    let root = canonicalize_allow_missing(worktree)?;
    let workspace = match subdir {
        Some(subdir) => canonicalize_allow_missing(&root.join(subdir))?,
        None => root.clone(),
    };
    anyhow::ensure!(
        workspace.strip_prefix(&root).is_ok(),
        "refusing Cargo workspace {} because it escapes task worktree {}",
        workspace.display(),
        root.display()
    );
    let target = canonicalize_allow_missing(&workspace.join("target"))?;
    anyhow::ensure!(
        target.strip_prefix(&workspace).is_ok(),
        "refusing private Cargo target {} because it escapes Cargo workspace {}",
        target.display(),
        workspace.display()
    );
    Ok(target)
}

/// The outcome of [`ensure_isolated`], distinguishing two failure shapes
/// that must NOT be handled the same way. "cargo has nothing to build here"
/// (no `Cargo.toml`, a malformed manifest, …) is an ordinary verifier
/// failure — cargo genuinely cannot proceed, for a reason a task's own
/// content controls, and the caller should report it as a failed step like
/// any other. "cargo resolved somewhere that is not private to this
/// worktree" is the actual hazard this module exists to catch, and must
/// hard-refuse the whole run rather than let any step proceed unverified.
/// Collapsing the two (an earlier draft of this function did) made a
/// missing `Cargo.toml` in a test fixture abort the verifier engine itself
/// instead of producing the ordinary failed report every OTHER cargo error
/// produces — caught by `dispatch_exit::refine_verify_failure_exits_zero`,
/// which exercises exactly that fixture shape.
#[derive(Debug)]
pub enum TargetDirCheck {
    /// Isolation verified; this is the directory cargo will actually build
    /// into.
    Isolated(PathBuf),
    /// `cargo metadata` itself failed — not a cargo project, or an invalid
    /// one. The caller should surface this as a normal failed verify step,
    /// not abort the run: the SAME failure a `cargo build` in this
    /// directory would have hit anyway, just detected one command earlier.
    NotACargoProject(String),
    /// This argv has no separately-addressable cargo token. It is either a
    /// non-cargo command, or an opaque command the profile explicitly
    /// declared and accepted responsibility for.
    NoCargoCommand,
}

/// Ask trusted Cargo (via `cargo metadata`, which does not compile anything)
/// which target directory `argv` would use from `dir`. Cargo is resolved from
/// the verifier process's PATH, excluding relative entries and candidates
/// inside `dir`; neither the command's wrapper nor its cargo token is
/// executed. Recognised transparent wrappers are parsed only. Their
/// `env K=V` assignments are applied to the probe, then the verifier pin is
/// applied last. Cargo's target-affecting arguments are preserved.
///
/// Returns [`TargetDirCheck::Isolated`] with the resolved directory
/// (recorded in the verify report for legibility) on success,
/// [`TargetDirCheck::NotACargoProject`] when cargo itself cannot answer
/// (surfaced as an ordinary failed step, not a run-aborting error), or a
/// hard `Err` — the one case that DOES abort the run — when cargo answers
/// but the resolved directory is not inside the workspace cargo itself
/// resolved for `dir`.
///
/// # Why containment, not "is it the ambient path?"
///
/// The first draft of this check asked only whether the resolved directory
/// EQUALLED this process's ambient `CARGO_TARGET_DIR`. That is wrong in
/// both directions, and neither error is theoretical:
///
/// - **Fail-open.** It catches exactly one string. A committed
///   `.cargo/config.toml` naming any OTHER out-of-tree directory — a
///   second shared cache, `/tmp/whatever` (the shape of 2026-08-22's
///   EDQUOT incident) — resolves to something that is not the ambient
///   value, compares unequal, and is waved through as "isolated" while
///   being nothing of the sort. So would the same shared directory reached
///   by a differently-spelled path: a trailing slash, a symlink, `..` in
///   the middle. A check keyed to one literal string is defeated by
///   spelling.
/// - **Fail-closed on a legitimate setup.** If a host's ambient
///   `CARGO_TARGET_DIR` happens to already BE this worktree's own
///   `<workspace>/target`, equality fires and refuses a directory that is
///   in fact perfectly private.
///
/// Containment in cargo's own `workspace_root` is the property actually
/// wanted, stated directly: a target directory inside the tree under
/// verification cannot be written by another worktree's cargo, whatever it
/// is called and however it was configured, because no other worktree's
/// cargo resolves a path into this one. Both paths come from the same
/// `cargo metadata` answer, so they are spelled consistently with each
/// other. Both are canonicalised before a component-wise `strip_prefix`
/// check, including paths whose final target components do not exist yet;
/// symlinks cannot turn a lexically-inside target into an outside one.
pub fn ensure_isolated(argv: &[String], dir: &Path) -> Result<TargetDirCheck> {
    ensure_isolated_for_profile(argv, dir, false)
}

/// Profile-aware form of [`ensure_isolated`]. `opaque_declared` is an
/// explicit, built-in profile exception, never inferred from command text.
/// It permits an opaque command to run with unknown cargo provenance; it
/// does not fabricate a target-directory result.
pub fn ensure_isolated_for_profile(
    argv: &[String],
    dir: &Path,
    opaque_declared: bool,
) -> Result<TargetDirCheck> {
    let Some(invocation) = cargo_invocation_for_probe(argv, opaque_declared)? else {
        return Ok(TargetDirCheck::NoCargoCommand);
    };
    let cargo = trusted_cargo_from_path(dir)?;
    probe_cargo_invocation(argv, dir, &cargo, None, invocation)
}

/// Re-probe a step immediately before execution with the verifier's selected
/// target pinned. The metadata answer must report that exact canonical path;
/// an argv-level override which outranks the environment is therefore
/// refused instead of silently running somewhere else.
pub fn ensure_isolated_with_pin_for_profile(
    argv: &[String],
    dir: &Path,
    pinned_target: &Path,
    opaque_declared: bool,
) -> Result<TargetDirCheck> {
    let Some(invocation) = cargo_invocation_for_probe(argv, opaque_declared)? else {
        return Ok(TargetDirCheck::NoCargoCommand);
    };
    let cargo = trusted_cargo_from_path(dir)?;
    probe_cargo_invocation(argv, dir, &cargo, Some(pinned_target), invocation)
}

/// The probe itself, parameterised on the trusted cargo binary so tests can
/// inject one resolved from a fixture PATH ([`trusted_cargo_on_path`]) —
/// production callers pass the binary [`trusted_cargo_from_path`] found on
/// the real PATH.
#[cfg(test)]
fn ensure_isolated_inner(
    argv: &[String],
    dir: &Path,
    cargo: &Path,
    pinned_target: Option<&Path>,
    opaque_declared: bool,
) -> Result<TargetDirCheck> {
    let Some(invocation) = cargo_invocation_for_probe(argv, opaque_declared)? else {
        return Ok(TargetDirCheck::NoCargoCommand);
    };
    probe_cargo_invocation(argv, dir, cargo, pinned_target, invocation)
}

fn cargo_invocation_for_probe(
    argv: &[String],
    opaque_declared: bool,
) -> Result<Option<CargoInvocation>> {
    anyhow::ensure!(!argv.is_empty(), "empty verifier command");
    let Some(invocation) = parse_cargo_invocation(argv)? else {
        if opaque_shell_command(argv) && !opaque_declared {
            anyhow::bail!(
                "refusing opaque verifier command {argv:?}: its shell string may invoke cargo, \
                 but the effective cargo argv and target directory cannot be probed. Use a \
                 transparent argv wrapper (for example `env … cargo test`) or declare this \
                 exact step opaque in the built-in verifier profile."
            );
        }
        return Ok(None);
    };
    Ok(Some(invocation))
}

fn probe_cargo_invocation(
    argv: &[String],
    dir: &Path,
    cargo: &Path,
    pinned_target: Option<&Path>,
    invocation: CargoInvocation,
) -> Result<TargetDirCheck> {
    let cargo_index = invocation.cargo_index;
    let supplied_cargo = Path::new(&argv[cargo_index]);
    if argv[cargo_index] != "cargo" {
        anyhow::ensure!(
            supplied_cargo.is_absolute(),
            "refusing relative Cargo argv[0] {:?} in verifier command {argv:?}",
            argv[cargo_index]
        );
        let supplied_cargo = supplied_cargo.canonicalize().with_context(|| {
            format!(
                "resolving supplied Cargo executable {}",
                supplied_cargo.display()
            )
        })?;
        let trusted_cargo = cargo
            .canonicalize()
            .with_context(|| format!("resolving trusted Cargo executable {}", cargo.display()))?;
        anyhow::ensure!(
            supplied_cargo == trusted_cargo,
            "refusing Cargo argv[0] {} because preflight uses the verifier process Cargo at {}",
            supplied_cargo.display(),
            cargo.display()
        );
    }
    let mut probe = Command::new(cargo);
    let mut toolchain = None;
    let mut forwarded = Vec::new();
    let mut explicit_target_dir = None;
    collect_probe_arguments(
        &argv[cargo_index + 1..],
        &mut toolchain,
        &mut forwarded,
        &mut explicit_target_dir,
    )?;
    if let Some(toolchain) = toolchain {
        probe.arg(toolchain);
    }
    probe
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .args(&forwarded)
        .current_dir(dir);
    strip_ambient_target_dir(&mut probe);
    for (key, value) in &invocation.env_assignments {
        probe.env(key, value);
    }
    // `cargo metadata` has no `--target-dir` option. The environment form
    // has the same path resolution and makes metadata report the directory
    // the real build command's explicit option selects.
    if let Some(target_dir) = explicit_target_dir {
        probe.env(AMBIENT_TARGET_DIR_ENV, target_dir);
    } else if let Some(pinned_target) = pinned_target {
        pin_target_dir(&mut probe, pinned_target);
    }
    let out = probe
        .output()
        .with_context(|| format!("probing verifier command {argv:?} in {}", dir.display()))?;
    if !out.status.success() {
        return Ok(TargetDirCheck::NotACargoProject(format!(
            "cargo metadata probe for {argv:?} failed in {} (exit {:?}): {}",
            dir.display(),
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let parsed: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("parsing cargo metadata output as JSON")?;
    let field = |key: &str| -> Result<PathBuf> {
        parsed
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from)
            .with_context(|| format!("cargo metadata in {} has no {key}", dir.display()))
    };
    let resolved = canonicalize_allow_missing(&field("target_directory")?)?;
    let workspace_root = canonicalize_allow_missing(&field("workspace_root")?)?;
    let target_environment = match pinned_target {
        Some(pinned_target) => format!(
            "{AMBIENT_TARGET_DIR_ENV} pinned to {}",
            pinned_target.display()
        ),
        None => format!("{AMBIENT_TARGET_DIR_ENV} removed"),
    };

    anyhow::ensure!(
        resolved.strip_prefix(&workspace_root).is_ok(),
        "refusing to run verifier command {argv:?} in {}: with the verifier target environment \
         ({target_environment}), cargo STILL resolved its target directory to \
         {}, which is OUTSIDE \
         the workspace at {} — so it is a directory other worktrees' cargo can also \
         resolve and write to, which is exactly the shared slot that lets one tree's \
         verification execute another tree's binary. Some configuration other than the \
         environment variable (a committed .cargo/config.toml `build.target-dir` is the \
         likely cause{}) is forcing it. Isolation cannot be established, so this verifier \
         refuses to run cargo unverified rather than trust it.",
        dir.display(),
        resolved.display(),
        workspace_root.display(),
        match std::env::var_os(AMBIENT_TARGET_DIR_ENV) {
            Some(ambient) if Path::new(&ambient) == resolved =>
                "; note it resolved to the very path this process's ambient \
                 CARGO_TARGET_DIR names, so that configuration is pointing straight back \
                 at the fleet's shared cache",
            _ => "",
        },
    );
    if let Some(pinned_target) = pinned_target {
        let pinned_target = canonicalize_allow_missing(pinned_target)?;
        anyhow::ensure!(
            resolved == pinned_target,
            "refusing to run verifier command {argv:?} in {}: its immediate preflight was pinned to {}, but cargo reported {}. An argv-level target override outranked the verifier pin; the step was not started.",
            dir.display(),
            pinned_target.display(),
            resolved.display()
        );
    }
    Ok(TargetDirCheck::Isolated(resolved))
}

/// Locate Cargo only through the grammar of supported transparent wrappers.
/// An arbitrary script with a dummy `cargo` argument is not a Cargo command.
pub fn cargo_argument_index(argv: &[String]) -> Option<usize> {
    parse_cargo_invocation(argv)
        .ok()
        .flatten()
        .map(|invocation| invocation.cargo_index)
}

fn opaque_shell_command(argv: &[String]) -> bool {
    argv.windows(2).any(|pair| {
        Path::new(&pair[0]).file_name().is_some_and(|name| {
            matches!(name.to_str(), Some("sh" | "bash" | "dash" | "zsh")) && pair[1] == "-c"
        })
    })
}

#[derive(Debug)]
struct CargoInvocation {
    cargo_index: usize,
    env_assignments: Vec<(String, String)>,
    env_assignment_indices: Vec<usize>,
}

fn parse_cargo_invocation(argv: &[String]) -> Result<Option<CargoInvocation>> {
    if argv.is_empty() {
        return Ok(None);
    }
    let mut index = 0;
    let mut env_assignments = Vec::new();
    let mut env_assignment_indices = Vec::new();
    loop {
        let Some(arg) = argv.get(index) else {
            return Ok(None);
        };
        let name = Path::new(arg).file_name().and_then(|name| name.to_str());
        match name {
            Some("cargo") => {
                anyhow::ensure!(
                    arg == "cargo" || Path::new(arg).is_absolute(),
                    "refusing relative Cargo argv[0] {arg:?} in verifier command {argv:?}"
                );
                return Ok(Some(CargoInvocation {
                    cargo_index: index,
                    env_assignments,
                    env_assignment_indices,
                }));
            }
            Some("env") => {
                index += 1;
                if argv.get(index).is_some_and(|arg| arg == "--") {
                    index += 1;
                }
                while let Some(arg) = argv.get(index) {
                    let Some((key, value)) = parse_env_assignment(arg) else {
                        break;
                    };
                    env_assignments.push((key.to_string(), value.to_string()));
                    env_assignment_indices.push(index);
                    index += 1;
                }
            }
            Some("memguard") => index += 1,
            Some("timeout") => index = timeout_command_index(argv, index + 1)?,
            Some("flock") => index = flock_command_index(argv, index + 1)?,
            _ => {
                anyhow::ensure!(
                    !argv[index..].iter().any(|arg| {
                        Path::new(arg)
                            .file_name()
                            .is_some_and(|name| name == "cargo")
                    }),
                    "refusing untrusted verifier wrapper {arg:?} with a cargo-shaped argument; only env assignments, memguard, timeout, and flock are transparent"
                );
                return Ok(None);
            }
        }
    }
}

/// Return the real step argv with the same trusted Cargo used by preflight.
/// Wrapper `env CARGO_TARGET_DIR=…` assignments are rewritten to the pin so
/// they cannot override the last environment mutation on the outer child.
pub fn hardened_cargo_argv(argv: &[String], dir: &Path, pin: &Path) -> Result<Vec<String>> {
    let Some(invocation) = parse_cargo_invocation(argv)? else {
        return Ok(argv.to_vec());
    };
    let cargo = trusted_cargo_from_path(dir)?;
    let mut hardened = argv.to_vec();
    if hardened[invocation.cargo_index] == "cargo" {
        hardened[invocation.cargo_index] = cargo.to_string_lossy().into_owned();
    }
    for index in invocation.env_assignment_indices {
        if hardened[index]
            .split_once('=')
            .is_some_and(|(key, _)| key == AMBIENT_TARGET_DIR_ENV)
        {
            hardened[index] = format!("{AMBIENT_TARGET_DIR_ENV}={}", pin.display());
        }
    }
    Ok(hardened)
}

fn parse_env_assignment(arg: &str) -> Option<(&str, &str)> {
    let (key, value) = arg.split_once('=')?;
    let mut chars = key.chars();
    let first = chars.next()?;
    (first == '_' || first.is_ascii_alphabetic())
        .then_some(())
        .filter(|_| chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric()))
        .map(|_| (key, value))
}

fn timeout_command_index(argv: &[String], mut index: usize) -> Result<usize> {
    let mut saw_duration = false;
    while let Some(arg) = argv.get(index) {
        if saw_duration {
            return Ok(index);
        }
        match arg.as_str() {
            "-k" | "--kill-after" | "-s" | "--signal" => index += 2,
            "--foreground" | "--preserve-status" | "--verbose" => index += 1,
            "--" => index += 1,
            _ if arg.starts_with("--kill-after=") || arg.starts_with("--signal=") => index += 1,
            _ if arg.starts_with('-') => {
                anyhow::bail!("unsupported timeout option {arg:?} in verifier command {argv:?}")
            }
            _ => {
                saw_duration = true;
                index += 1;
            }
        }
    }
    anyhow::bail!("timeout wrapper has no command in verifier argv {argv:?}")
}

fn flock_command_index(argv: &[String], mut index: usize) -> Result<usize> {
    let mut saw_lock = false;
    while let Some(arg) = argv.get(index) {
        if saw_lock {
            return Ok(index);
        }
        match arg.as_str() {
            "-w" | "--wait" | "-E" | "--conflict-exit-code" => index += 2,
            "-s" | "--shared" | "-x" | "--exclusive" | "-u" | "--unlock" | "-n" | "--nonblock"
            | "--no-fork" | "--verbose" | "--close" => index += 1,
            "--" => index += 1,
            _ if arg.starts_with("--wait=") || arg.starts_with("--conflict-exit-code=") => {
                index += 1
            }
            _ if arg.starts_with('-') => {
                anyhow::bail!("unsupported flock option {arg:?} in verifier command {argv:?}")
            }
            _ => {
                saw_lock = true;
                index += 1;
            }
        }
    }
    anyhow::bail!("flock wrapper has no command in verifier argv {argv:?}")
}

/// Resolve the trusted cargo from the verifier process's real `PATH`.
///
/// This is the only place the environment lookup happens. Every function
/// below takes the resolved binary as an explicit parameter, so tests drive
/// [`trusted_cargo_on_path`] with a fixture PATH instead of mutating process
/// environment.
pub(crate) fn trusted_cargo_from_path(workspace: &Path) -> Result<PathBuf> {
    let path =
        std::env::var_os("PATH").context("verifier process has no PATH for trusted cargo")?;
    trusted_cargo_on_path(workspace, &path)
}

/// Pure `PATH` scan: the first `cargo` outside the verifier checkout, given
/// the PATH to search as an explicit value rather than process environment.
fn trusted_cargo_on_path(workspace: &Path, path: &std::ffi::OsStr) -> Result<PathBuf> {
    let workspace = canonicalize_allow_missing(workspace)?;
    let checkout = workspace
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .unwrap_or(&workspace);
    for directory in std::env::split_paths(path) {
        if !directory.is_absolute() {
            continue;
        }
        let candidate = directory.join("cargo");
        let Ok(resolved) = candidate.canonicalize() else {
            continue;
        };
        if candidate.starts_with(checkout) || resolved.starts_with(checkout) || !resolved.is_file()
        {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if resolved.metadata()?.permissions().mode() & 0o111 == 0 {
                continue;
            }
        }
        return Ok(candidate);
    }
    anyhow::bail!(
        "no trusted cargo executable outside verifier checkout {} was found on the verifier process PATH",
        checkout.display()
    )
}

fn collect_probe_arguments(
    cargo_args: &[String],
    toolchain: &mut Option<String>,
    forwarded: &mut Vec<String>,
    explicit_target_dir: &mut Option<String>,
) -> Result<()> {
    let mut i = 0;
    if cargo_args.first().is_some_and(|arg| arg.starts_with('+')) {
        *toolchain = Some(cargo_args[0].clone());
        i += 1;
    }
    while i < cargo_args.len() {
        let arg = &cargo_args[i];
        if arg == "--" {
            break;
        }
        match arg.as_str() {
            "--manifest-path" | "-m" | "--config" => {
                let value = cargo_args
                    .get(i + 1)
                    .with_context(|| format!("{arg} in cargo command has no value"))?;
                forwarded.push(arg.clone());
                forwarded.push(value.clone());
                i += 2;
            }
            "--target-dir" => {
                let value = cargo_args
                    .get(i + 1)
                    .with_context(|| "--target-dir in cargo command has no value")?;
                *explicit_target_dir = Some(value.clone());
                i += 2;
            }
            _ if arg.starts_with("--manifest-path=") || arg.starts_with("--config=") => {
                forwarded.push(arg.clone());
                i += 1;
            }
            _ if arg.starts_with("--target-dir=") => {
                *explicit_target_dir = arg.split_once('=').map(|(_, value)| value.to_string());
                i += 1;
            }
            _ => i += 1,
        }
    }
    Ok(())
}

/// Canonicalise the longest existing prefix, then append missing normal
/// components. Cargo commonly reports `<workspace>/target` before that
/// directory exists, so plain `canonicalize` would turn a safe fresh
/// worktree into an error. Existing symlinks are still resolved.
pub(crate) fn canonicalize_allow_missing(path: &Path) -> Result<PathBuf> {
    let mut cursor = path.to_path_buf();
    let mut missing = Vec::new();
    loop {
        match cursor.canonicalize() {
            Ok(mut canonical) => {
                for component in missing.iter().rev() {
                    match Path::new(component).components().next() {
                        Some(std::path::Component::Normal(name)) => canonical.push(name),
                        Some(std::path::Component::CurDir) => {}
                        Some(std::path::Component::ParentDir) => {
                            anyhow::ensure!(
                                canonical.pop(),
                                "canonical path {} escapes its filesystem root",
                                path.display()
                            );
                        }
                        _ => anyhow::bail!(
                            "non-normal missing component in canonical path {}",
                            path.display()
                        ),
                    }
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = cursor.file_name().with_context(|| {
                    format!(
                        "cannot canonicalize any existing prefix of {}",
                        path.display()
                    )
                })?;
                missing.push(name.to_os_string());
                anyhow::ensure!(cursor.pop(), "cannot ascend from {}", path.display());
            }
            Err(error) => {
                return Err(error).with_context(|| format!("canonicalizing {}", path.display()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_crate(dir: &Path) {
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"target-dir-probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
    }

    fn probe_cargo(workspace: &Path) -> PathBuf {
        trusted_cargo_on_path(workspace, std::ffi::OsStr::new(env!("PATH"))).unwrap()
    }

    fn ensure_isolated_for_test(argv: &[String], dir: &Path) -> Result<TargetDirCheck> {
        ensure_isolated_inner(argv, dir, &probe_cargo(dir), None, false)
    }

    fn ensure_isolated_with_pin_for_test(
        argv: &[String],
        dir: &Path,
        pinned_target: &Path,
    ) -> Result<TargetDirCheck> {
        ensure_isolated_inner(argv, dir, &probe_cargo(dir), Some(pinned_target), false)
    }

    #[test]
    fn resolves_to_the_workspace_target_dir_when_ambient_is_unset() {
        let dir = tempfile::tempdir().unwrap();
        write_crate(dir.path());
        let check =
            ensure_isolated_for_test(&["cargo".into(), "build".into()], dir.path()).unwrap();
        let TargetDirCheck::Isolated(resolved) = check else {
            panic!("expected Isolated, got a NotACargoProject result");
        };
        assert_eq!(
            resolved,
            dir.path().canonicalize().unwrap().join("target"),
            "cargo's own default must be the workspace-local target/ dir"
        );
    }

    #[test]
    fn driver_and_verifier_forms_select_one_subdir_target() {
        let worktree = tempfile::tempdir().unwrap();
        std::fs::create_dir(worktree.path().join("src")).unwrap();
        let driver = pinned_target_dir(worktree.path(), Some("src")).unwrap();
        let verifier = pinned_target_dir(&worktree.path().join("src"), None).unwrap();
        assert_eq!(driver, verifier);
        assert_eq!(
            driver,
            worktree.path().canonicalize().unwrap().join("src/target")
        );
    }

    #[test]
    fn a_directory_with_no_cargo_project_is_reported_not_refused() {
        // A missing Cargo.toml must produce the SAME class of outcome an
        // ordinary `cargo build` failure would — a reportable step failure
        // — not the hard-refusal error path meant for a proven isolation
        // breach. Conflating the two broke
        // `dispatch_exit::refine_verify_failure_exits_zero`, whose fixture
        // repo intentionally has no Cargo.toml at all.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "no cargo project here\n").unwrap();
        let check =
            ensure_isolated_for_test(&["cargo".into(), "build".into()], dir.path()).unwrap();
        assert!(
            matches!(check, TargetDirCheck::NotACargoProject(_)),
            "a missing Cargo.toml must be a reportable failure, not an aborting error"
        );
    }

    #[test]
    fn refuses_when_a_committed_config_still_forces_the_shared_dir() {
        // A checked-in .cargo/config.toml that ignores the env var entirely
        // — the one way isolation can still fail even with the variable
        // stripped from cargo's own child environment.
        let shared = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        write_crate(dir.path());
        std::fs::create_dir_all(dir.path().join(".cargo")).unwrap();
        std::fs::write(
            dir.path().join(".cargo/config.toml"),
            format!(
                "[build]\ntarget-dir = {:?}\n",
                shared.path().to_str().unwrap()
            ),
        )
        .unwrap();
        let err = ensure_isolated_for_test(&["cargo".into(), "build".into()], dir.path())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("Isolation cannot be established"),
            "must refuse rather than silently trust the shared dir: {err}"
        );
    }

    #[test]
    fn strip_ambient_target_dir_is_a_harmless_no_op_when_unset() {
        // env_remove on an absent var must not error or panic — the whole
        // point is that every cargo spawn can call this unconditionally.
        let mut cmd = Command::new("true");
        strip_ambient_target_dir(&mut cmd);
        assert!(
            cmd.get_envs()
                .any(|(k, v)| k == AMBIENT_TARGET_DIR_ENV && v.is_none()),
            "must record an explicit removal, not skip it"
        );
    }

    #[test]
    fn component_containment_rejects_target_x_as_a_sibling_of_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let target_x = dir.path().join("targetX");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(&target_x).unwrap();

        let target = canonicalize_allow_missing(&target).unwrap();
        let target_x = canonicalize_allow_missing(&target_x).unwrap();
        assert!(
            target_x.strip_prefix(&target).is_err(),
            "targetX is a sibling, not a child of target"
        );
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_workspace_lexical_target_symlinked_outside() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        write_crate(dir.path());
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), dir.path().join("target")).unwrap();

        let result = ensure_isolated_for_test(&["cargo".into(), "build".into()], dir.path());

        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Isolation cannot be established"),
            "a symlink must not make an outside target look contained: {err}"
        );
    }

    #[test]
    fn actual_target_dir_argument_is_part_of_the_probe() {
        let dir = tempfile::tempdir().unwrap();
        write_crate(dir.path());
        let shared = tempfile::tempdir().unwrap();
        let argv = vec![
            "cargo".into(),
            "test".into(),
            "--target-dir".into(),
            shared.path().to_string_lossy().into_owned(),
        ];

        let result = ensure_isolated_for_test(&argv, dir.path());

        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Isolation cannot be established"),
            "the preflight must reject the real command's outside --target-dir: {err}"
        );
    }

    #[test]
    fn actual_manifest_and_config_arguments_are_part_of_the_probe() {
        let invocation_dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        write_crate(workspace.path());
        let shared = tempfile::tempdir().unwrap();
        let argv = vec![
            "env".into(),
            "cargo".into(),
            "test".into(),
            "--manifest-path".into(),
            workspace
                .path()
                .join("Cargo.toml")
                .to_string_lossy()
                .into_owned(),
            "--config".into(),
            format!("build.target-dir={:?}", shared.path().to_str().unwrap()),
        ];

        let cargo = probe_cargo(workspace.path());
        let result = ensure_isolated_inner(&argv, invocation_dir.path(), &cargo, None, false);

        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Isolation cannot be established"),
            "the preflight must preserve wrapper cargo's manifest and config: {err}"
        );
    }

    #[test]
    fn wrapper_target_dir_assignment_is_effective_and_refused() {
        let dir = tempfile::tempdir().unwrap();
        write_crate(dir.path());
        let shared = tempfile::tempdir().unwrap();
        let argv = vec![
            "env".into(),
            format!(
                "{AMBIENT_TARGET_DIR_ENV}={}",
                shared.path().to_string_lossy()
            ),
            "cargo".into(),
            "test".into(),
        ];
        let error = ensure_isolated_for_test(&argv, dir.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("Isolation cannot be established"), "{error}");
        assert!(
            error.contains(&shared.path().to_string_lossy().to_string()),
            "{error}"
        );
    }

    #[test]
    fn harmless_env_wrapper_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        write_crate(dir.path());
        let check = ensure_isolated_for_test(
            &[
                "env".into(),
                "FOO=bar".into(),
                "cargo".into(),
                "test".into(),
            ],
            dir.path(),
        )
        .unwrap();
        assert!(matches!(check, TargetDirCheck::Isolated(_)));
    }

    #[test]
    fn pinned_probe_overrides_wrapper_target_assignment() {
        let dir = tempfile::tempdir().unwrap();
        write_crate(dir.path());
        let pin = pinned_target_dir(dir.path(), None).unwrap();
        let check = ensure_isolated_with_pin_for_test(
            &[
                "env".into(),
                format!("{AMBIENT_TARGET_DIR_ENV}=/elsewhere"),
                "cargo".into(),
                "test".into(),
            ],
            dir.path(),
            &pin,
        )
        .unwrap();
        let TargetDirCheck::Isolated(reported) = check else {
            panic!("pinned cargo metadata must resolve");
        };
        assert_eq!(reported, pin);
    }

    #[test]
    fn memguard_style_transparent_wrapper_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        write_crate(dir.path());
        let wrappers = tempfile::tempdir().unwrap();
        let memguard = wrappers.path().join("memguard");
        let marker = dir.path().join("wrapper-ran");
        crate::fixture::write_executable(
            &memguard,
            format!("#!/bin/sh\ntouch {:?}\nexec \"$@\"\n", marker),
        );
        let check = ensure_isolated_for_test(
            &[
                memguard.to_string_lossy().into_owned(),
                "cargo".into(),
                "test".into(),
            ],
            dir.path(),
        )
        .unwrap();
        assert!(matches!(check, TargetDirCheck::Isolated(_)));
        assert!(
            !marker.exists(),
            "metadata preflight must not execute memguard"
        );
    }

    #[test]
    fn worktree_cargo_on_path_is_not_executed_by_probe() {
        let worktree = tempfile::tempdir().unwrap();
        std::fs::write(worktree.path().join(".git"), "gitdir: elsewhere\n").unwrap();
        let workspace = worktree.path().join("src");
        std::fs::create_dir(&workspace).unwrap();
        write_crate(&workspace);
        let bin = worktree.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let marker = worktree.path().join("cargo-ran");
        let fake_cargo = bin.join("cargo");
        // The hostile PATH — worktree cargo first, the real PATH after — is
        // an ARGUMENT to the pure resolver, never installed into process
        // environment.
        crate::fixture::write_executable(&fake_cargo, format!("#!/bin/sh\ntouch {:?}\n", marker));
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(std::ffi::OsStr::new(env!("PATH"))));
        let path = std::env::join_paths(paths).unwrap();
        let cargo = trusted_cargo_on_path(&workspace, &path).unwrap();
        assert_ne!(
            cargo.canonicalize().unwrap(),
            fake_cargo.canonicalize().unwrap(),
            "the worktree cargo must be skipped in favour of a trusted system cargo"
        );

        // The probe runs that resolved binary explicitly; the fake's marker
        // proves it never executed.
        let pin = pinned_target_dir(&workspace, None).unwrap();
        let check = ensure_isolated_inner(
            &["cargo".into(), "test".into()],
            &workspace,
            &cargo,
            Some(&pin),
            false,
        );

        assert!(matches!(check.unwrap(), TargetDirCheck::Isolated(_)));
        assert!(
            !marker.exists(),
            "probe must use system cargo, not worktree PATH"
        );
    }

    #[test]
    fn relative_and_worktree_cargo_executables_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        write_crate(dir.path());
        let cargo = dir.path().join("cargo");
        crate::fixture::write_executable(&cargo, "#!/bin/sh\nexit 0\n");
        let trusted = probe_cargo(dir.path());
        for executable in ["./cargo".to_string(), cargo.to_string_lossy().into_owned()] {
            let error = ensure_isolated_inner(
                &[executable, "test".into()],
                dir.path(),
                &trusted,
                None,
                false,
            )
            .unwrap_err()
            .to_string();
            assert!(
                error.contains("relative Cargo") || error.contains("verifier process Cargo"),
                "{error}"
            );
        }
    }

    #[test]
    fn opaque_shell_cargo_is_refused_with_reason() {
        let dir = tempfile::tempdir().unwrap();
        write_crate(dir.path());
        let error = ensure_isolated(&["sh".into(), "-c".into(), "cargo test".into()], dir.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("opaque verifier command"), "{error}");
        assert!(error.contains("cannot be probed"), "{error}");
    }

    #[test]
    fn declared_opaque_shell_is_unknown_not_fabricated() {
        let dir = tempfile::tempdir().unwrap();
        write_crate(dir.path());
        let check = ensure_isolated_for_profile(
            &["sh".into(), "-c".into(), "cargo test".into()],
            dir.path(),
            true,
        )
        .unwrap();
        assert!(matches!(check, TargetDirCheck::NoCargoCommand));
    }
}
