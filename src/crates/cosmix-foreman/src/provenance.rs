//! Bounded, diagnostic provenance for verifier-executed Cargo binaries.
//!
//! Before a `cargo test` or `cargo bench` step starts, Foreman runs a
//! verifier-chosen Cargo invocation with the same transparent
//! wrapper, Cargo selectors and verified private target, but with execution
//! disabled by `--no-run --message-format=json`. Cargo's own
//! `compiler-artifact` records are control data from that separate
//! invocation: they are not the step's captured stdout, and build scripts or
//! test processes do not choose the control argv. Foreman hashes exactly
//! the non-null `executable` paths Cargo reports. It repeats the listing and
//! hashing after the real step. A complete record means exactly: "these bytes
//! existed at these paths when the step began and were unchanged when it
//! ended; cargo ran them". The residual is deliberate: a test could exec a
//! different file it carries itself; that is outside provenance's claim.
//! Every path is still treated
//! as untrusted data and must be a canonical, regular executable inside the
//! already-verified private target.
//!
//! Evidence cannot affect a step's verdict, authorisation, cache, routing or
//! replay. A failed/timed-out listing, malformed control record, escaping
//! path, race, cap breach or exhausted deadline is recorded as
//! [`BinaryProvenance::Unavailable`], never as a complete empty list.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs::{File, Metadata, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// One debug executable in this workspace is currently about 512 MiB.
const MAX_FILE_BYTES: u64 = 1024 * 1024 * 1024;
/// A cold full-workspace test currently produces about 25 GiB of executables.
const MAX_AGGREGATE_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const MAX_ARTIFACT_MESSAGES: usize = 16 * 1024;
const MAX_CONTROL_BYTES: usize = 64 * 1024 * 1024;
const HASH_CHUNK_BYTES: usize = 1024 * 1024;
const STDERR_TAIL_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BinaryDigest {
    /// Component-wise relative to the canonical target directory.
    pub path: String,
    /// Always `sha256:<lowercase hex>`.
    pub digest: String,
}

/// Explicit evidence state. Missing evidence and inapplicable commands are
/// both distinct from a complete, honestly empty Cargo artifact listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BinaryProvenance {
    Complete { binaries: Vec<BinaryDigest> },
    NotApplicable,
    Unavailable { reason: String },
}

/// Exact claim made by [`BinaryProvenance::Complete`].
pub const COMPLETE_GUARANTEE: &str = "these bytes existed at these paths when the step began and were unchanged when it ended; cargo ran them";

impl BinaryProvenance {
    /// Backward-compatible value for a persisted `VerifyStep` written before
    /// provenance existed. This is UNKNOWN, deliberately not complete-empty.
    pub fn unavailable_from_legacy_report() -> Self {
        Self::Unavailable {
            reason: "report predates executed-binary provenance".to_string(),
        }
    }

    pub fn rendered_lines(&self) -> Vec<String> {
        match self {
            Self::Complete { binaries } => binaries
                .iter()
                .map(|binary| format!("{} {}", binary.path, binary.digest))
                .collect(),
            Self::NotApplicable => vec!["not applicable: not a cargo test/bench step".to_string()],
            Self::Unavailable { reason } => vec![format!("unavailable: {reason}")],
        }
    }
}

#[derive(Clone, Copy)]
struct Limits {
    per_file: u64,
    aggregate: u64,
    entries: usize,
}

const PRODUCTION_LIMITS: Limits = Limits {
    per_file: MAX_FILE_BYTES,
    aggregate: MAX_AGGREGATE_BYTES,
    entries: MAX_ARTIFACT_MESSAGES,
};

/// Take one executable snapshot for a verifier step.
///
/// The snapshot uses the same command prefix and Cargo-side selection flags.
/// Arguments after Cargo's `--` separator are runtime arguments and are
/// removed because `--no-run` starts no test/benchmark process. The ambient
/// target override is replaced with the canonical target the immediate
/// preflight proved private; listed paths are independently contained again.
pub fn collect(
    argv: &[String],
    dir: &Path,
    target_dir: &Path,
    deadline: Instant,
    use_memguard: bool,
) -> BinaryProvenance {
    collect_with_rustc_wrapper(argv, dir, target_dir, deadline, use_memguard, false)
}

/// [`collect`] with the verifier's one-shot sccache bypass applied to the
/// control invocation as well as the real step. This keeps the provenance
/// claim honest after a recovered retry: both observations use the same
/// explicit empty `RUSTC_WRAPPER` as the command whose binaries they cover.
pub(crate) fn collect_with_rustc_wrapper(
    argv: &[String],
    dir: &Path,
    target_dir: &Path,
    deadline: Instant,
    use_memguard: bool,
    bypass_rustc_wrapper: bool,
) -> BinaryProvenance {
    let listing = match artifact_listing_argv(argv) {
        Ok(Some(listing)) => listing,
        Ok(None) => return BinaryProvenance::NotApplicable,
        Err(error) => {
            return BinaryProvenance::Unavailable {
                reason: format!("cannot construct Cargo artifact listing: {error:#}"),
            };
        }
    };
    match collect_inner(
        &listing,
        dir,
        target_dir,
        deadline,
        use_memguard,
        bypass_rustc_wrapper,
        PRODUCTION_LIMITS,
    ) {
        Ok(binaries) => BinaryProvenance::Complete { binaries },
        Err(error) => BinaryProvenance::Unavailable {
            reason: format!("{error:#}"),
        },
    }
}

fn collect_inner(
    listing: &[String],
    dir: &Path,
    target_dir: &Path,
    deadline: Instant,
    use_memguard: bool,
    bypass_rustc_wrapper: bool,
    limits: Limits,
) -> Result<Vec<BinaryDigest>> {
    check_deadline(deadline, &mut Instant::now)?;
    let stdout = run_artifact_listing(
        listing,
        dir,
        target_dir,
        deadline,
        use_memguard,
        bypass_rustc_wrapper,
        limits.entries,
    )?;
    let target_root = target_dir
        .canonicalize()
        .with_context(|| format!("canonicalising target directory {}", target_dir.display()))?;
    let paths = parse_artifact_paths(&stdout, limits.entries)?;
    hash_listed_paths(&target_root, paths, deadline, limits, Instant::now)
}

/// Compare the pre-run snapshot with a fresh post-run snapshot. Only an
/// identical path/digest set is complete, and the pre-run digests are the
/// ones returned. Both sets are kept in a mismatch reason so the diagnostic
/// says what disappeared, appeared, or changed.
pub fn finish(
    before: BinaryProvenance,
    argv: &[String],
    dir: &Path,
    target_dir: &Path,
    deadline: Instant,
    use_memguard: bool,
) -> BinaryProvenance {
    finish_with_rustc_wrapper(before, argv, dir, target_dir, deadline, use_memguard, false)
}

pub(crate) fn finish_with_rustc_wrapper(
    before: BinaryProvenance,
    argv: &[String],
    dir: &Path,
    target_dir: &Path,
    deadline: Instant,
    use_memguard: bool,
    bypass_rustc_wrapper: bool,
) -> BinaryProvenance {
    let BinaryProvenance::Complete {
        binaries: before_binaries,
    } = before
    else {
        return before;
    };
    match collect_with_rustc_wrapper(
        argv,
        dir,
        target_dir,
        deadline,
        use_memguard,
        bypass_rustc_wrapper,
    ) {
        BinaryProvenance::Complete {
            binaries: after_binaries,
        } if after_binaries == before_binaries => BinaryProvenance::Complete {
            binaries: before_binaries,
        },
        BinaryProvenance::Complete {
            binaries: after_binaries,
        } => BinaryProvenance::Unavailable {
            reason: format!(
                "artifacts changed during the step; before={before_binaries:?}; after={after_binaries:?}"
            ),
        },
        BinaryProvenance::Unavailable { reason } => BinaryProvenance::Unavailable {
            reason: format!(
                "post-step artifact snapshot unavailable: {reason}; before={before_binaries:?}; after=unavailable"
            ),
        },
        BinaryProvenance::NotApplicable => BinaryProvenance::Unavailable {
            reason: format!(
                "post-step artifact snapshot became inapplicable; before={before_binaries:?}; after=not_applicable"
            ),
        },
    }
}

/// Transform a transparent Cargo argv into the verifier-owned control query.
/// Cargo-side selectors stay intact; any existing message format is replaced,
/// runtime arguments after `--` are dropped, and no test binary is started.
fn artifact_listing_argv(argv: &[String]) -> Result<Option<Vec<String>>> {
    let Some(cargo_index) = crate::target_dir::cargo_argument_index(argv) else {
        return Ok(None);
    };
    let cargo_args = &argv[cargo_index + 1..];
    let Some(subcommand_offset) = cargo_subcommand_offset(cargo_args) else {
        return Ok(None);
    };
    let subcommand = cargo_args[subcommand_offset].as_str();
    if !matches!(subcommand, "test" | "bench") {
        return Ok(None);
    }

    let subcommand_index = cargo_index + 1 + subcommand_offset;
    let mut listing = argv[..=subcommand_index].to_vec();
    let mut i = subcommand_index + 1;
    while i < argv.len() {
        match argv[i].as_str() {
            "--" => break,
            // A step that is ITSELF --no-run executes no binary: reporting
            // Complete would assert "cargo ran them" about binaries nothing
            // ran (codex merge-arm blocker, task 44 round 7). Build-only
            // steps have no executed-binary provenance to claim.
            "--no-run" => return Ok(None),
            "--message-format" => {
                anyhow::ensure!(
                    argv.get(i + 1).is_some(),
                    "--message-format in cargo command has no value"
                );
                i += 2;
            }
            value if value.starts_with("--message-format=") => i += 1,
            _ => {
                listing.push(argv[i].clone());
                i += 1;
            }
        }
    }
    listing.push("--no-run".to_string());
    listing.push("--message-format=json".to_string());
    Ok(Some(listing))
}

/// Find Cargo's subcommand without mistaking values of its global options
/// for one. The first non-option token (after an optional `+toolchain`) is
/// Cargo's subcommand.
fn cargo_subcommand_offset(args: &[String]) -> Option<usize> {
    let mut i = usize::from(args.first().is_some_and(|arg| arg.starts_with('+')));
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" {
            return None;
        }
        if matches!(arg, "--color" | "--config" | "-Z" | "-C") {
            i = i.saturating_add(2);
        } else if arg.starts_with('-') {
            i += 1;
        } else {
            return Some(i);
        }
    }
    None
}

fn run_artifact_listing(
    listing: &[String],
    dir: &Path,
    target_root: &Path,
    deadline: Instant,
    use_memguard: bool,
    bypass_rustc_wrapper: bool,
    entry_cap: usize,
) -> Result<Vec<u8>> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .context("deadline exhausted before Cargo artifact listing")?;
    anyhow::ensure!(
        !remaining.is_zero(),
        "deadline exhausted before Cargo artifact listing"
    );
    let seconds = remaining.as_secs_f64().max(0.001);
    let mut cmd = Command::new("timeout");
    cmd.arg("-k").arg("30").arg(format!("{seconds:.3}s"));
    if use_memguard && crate::target_dir::cargo_argument_index(listing) == Some(0) {
        cmd.arg("memguard");
    }
    let listing = crate::target_dir::hardened_cargo_argv(listing, dir, target_root)?;
    cmd.args(&listing)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if bypass_rustc_wrapper {
        cmd.env("RUSTC_WRAPPER", "");
    }
    // LAST environment mutation before spawn. This is the canonical
    // directory the immediate preflight proved and reported.
    crate::target_dir::pin_target_dir(&mut cmd, target_root);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning Cargo artifact listing {listing:?}"))?;
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let out_thread = std::thread::spawn(move || read_capped(stdout, MAX_CONTROL_BYTES, false));
    let err_thread = std::thread::spawn(move || read_capped(stderr, STDERR_TAIL_BYTES, true));
    let status = child.wait().context("waiting for Cargo artifact listing")?;
    let (stdout, stdout_overflow) = out_thread
        .join()
        .map_err(|_| anyhow::anyhow!("Cargo artifact stdout reader panicked"))??;
    let (stderr, _) = err_thread
        .join()
        .map_err(|_| anyhow::anyhow!("Cargo artifact stderr reader panicked"))??;
    check_deadline(deadline, &mut Instant::now)?;
    anyhow::ensure!(
        !stdout_overflow,
        "Cargo artifact control output exceeded {MAX_CONTROL_BYTES} bytes"
    );
    anyhow::ensure!(
        status.success(),
        "Cargo artifact listing failed or timed out (exit {:?}): {}",
        status.code(),
        String::from_utf8_lossy(&stderr).trim()
    );
    // A cheap pre-parse bound prevents a stream of tiny irrelevant messages
    // from bypassing the existing entry ceiling.
    anyhow::ensure!(
        stdout.iter().filter(|&&byte| byte == b'\n').count() <= entry_cap,
        "artifact-message ceiling of {entry_cap} exceeded"
    );
    Ok(stdout)
}

fn read_capped(mut pipe: impl Read, cap: usize, rolling: bool) -> std::io::Result<(Vec<u8>, bool)> {
    let mut kept = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut overflow = false;
    loop {
        let n = pipe.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        if rolling {
            kept.extend_from_slice(&chunk[..n]);
            if kept.len() > cap {
                let cut = kept.len() - cap;
                kept.drain(..cut);
            }
        } else if kept.len().saturating_add(n) <= cap {
            kept.extend_from_slice(&chunk[..n]);
        } else {
            overflow = true;
        }
    }
    Ok((kept, overflow))
}

fn parse_artifact_paths(stdout: &[u8], entry_cap: usize) -> Result<Vec<PathBuf>> {
    let text = std::str::from_utf8(stdout).context("Cargo artifact listing is not UTF-8")?;
    let mut paths = BTreeSet::new();
    let mut messages = 0usize;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        messages = messages.saturating_add(1);
        anyhow::ensure!(
            messages <= entry_cap,
            "artifact-message ceiling of {entry_cap} exceeded"
        );
        let message: serde_json::Value =
            serde_json::from_str(line).with_context(|| "parsing Cargo artifact control record")?;
        if message.get("reason").and_then(|value| value.as_str()) == Some("compiler-artifact")
            && let Some(executable) = message.get("executable")
            && !executable.is_null()
        {
            let executable = executable
                .as_str()
                .context("compiler-artifact executable is not a string")?;
            paths.insert(PathBuf::from(executable));
            anyhow::ensure!(
                paths.len() <= entry_cap,
                "executable-artifact ceiling of {entry_cap} exceeded"
            );
        }
    }
    Ok(paths.into_iter().collect())
}

fn hash_listed_paths(
    target_root: &Path,
    paths: Vec<PathBuf>,
    deadline: Instant,
    limits: Limits,
    mut now: impl FnMut() -> Instant,
) -> Result<Vec<BinaryDigest>> {
    check_deadline(deadline, &mut now)?;
    let mut identities = BTreeSet::new();
    let mut aggregate = 0u64;
    let mut binaries = Vec::new();
    for path in paths {
        check_deadline(deadline, &mut now)?;
        anyhow::ensure!(
            path.is_absolute(),
            "Cargo listed a non-absolute executable path {}",
            path.display()
        );
        let metadata = std::fs::symlink_metadata(&path)
            .with_context(|| format!("reading listed artifact metadata for {}", path.display()))?;
        validate_regular_executable(&path, &metadata)?;
        let canonical = path
            .canonicalize()
            .with_context(|| format!("canonicalising listed artifact {}", path.display()))?;
        check_deadline(deadline, &mut now)?;
        anyhow::ensure!(
            canonical.strip_prefix(target_root).is_ok(),
            "Cargo listed executable {} outside canonical private target {}",
            canonical.display(),
            target_root.display()
        );
        if !identities.insert((metadata.dev(), metadata.ino())) {
            continue;
        }
        anyhow::ensure!(
            metadata.len() <= limits.per_file,
            "per-file byte ceiling of {} exceeded by {} ({} bytes)",
            limits.per_file,
            path.display(),
            metadata.len()
        );
        aggregate = aggregate
            .checked_add(metadata.len())
            .context("aggregate executable byte count overflowed")?;
        anyhow::ensure!(
            aggregate <= limits.aggregate,
            "aggregate byte ceiling of {} exceeded at {} ({} bytes)",
            limits.aggregate,
            path.display(),
            aggregate
        );
        let digest = hash_regular_file(&path, &metadata, limits.per_file, deadline, &mut now)?;
        let relative = canonical
            .strip_prefix(target_root)
            .context("canonical artifact lost target containment")?
            .to_str()
            .with_context(|| format!("artifact path {} is not UTF-8", canonical.display()))?
            .to_string();
        binaries.push(BinaryDigest {
            path: relative,
            digest: format!("sha256:{digest}"),
        });
    }
    check_deadline(deadline, &mut now)?;
    Ok(binaries)
}

fn validate_regular_executable(path: &Path, metadata: &Metadata) -> Result<()> {
    anyhow::ensure!(
        !metadata.file_type().is_symlink(),
        "rejecting symlink artifact {}",
        path.display()
    );
    anyhow::ensure!(
        metadata.is_file(),
        "rejecting non-regular artifact {}",
        path.display()
    );
    anyhow::ensure!(
        metadata.permissions().mode() & 0o111 != 0,
        "Cargo listed non-executable artifact {}",
        path.display()
    );
    Ok(())
}

fn hash_regular_file(
    path: &Path,
    expected: &Metadata,
    per_file_limit: u64,
    deadline: Instant,
    now: &mut impl FnMut() -> Instant,
) -> Result<String> {
    check_deadline(deadline, now)?;
    // O_PATH pins the classified inode without invoking FIFO/device open
    // semantics. Reopening it via /proc closes the path-replacement race.
    let handle = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("pinning listed artifact {}", path.display()))?;
    let pinned = handle
        .metadata()
        .with_context(|| format!("reading pinned metadata for {}", path.display()))?;
    validate_regular_executable(path, &pinned)?;
    anyhow::ensure!(
        (pinned.dev(), pinned.ino()) == (expected.dev(), expected.ino()),
        "listed artifact {} raced before hashing",
        path.display()
    );
    let proc_path = PathBuf::from(format!("/proc/self/fd/{}", handle.as_raw_fd()));
    let mut file = File::open(&proc_path)
        .with_context(|| format!("opening pinned listed artifact {}", path.display()))?;
    let opened = file
        .metadata()
        .with_context(|| format!("reading opened metadata for {}", path.display()))?;
    anyhow::ensure!(
        opened.is_file()
            && (opened.dev(), opened.ino()) == (expected.dev(), expected.ino())
            && opened.len() == expected.len()
            && metadata_times(&opened) == metadata_times(expected),
        "listed artifact {} changed identity before hashing",
        path.display()
    );

    let mut hasher = Sha256::new();
    let mut chunk = vec![0u8; HASH_CHUNK_BYTES];
    let mut bytes_read = 0u64;
    loop {
        check_deadline(deadline, now)?;
        let n = match file.read(&mut chunk) {
            Ok(n) => n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error).with_context(|| format!("hashing {}", path.display())),
        };
        if n == 0 {
            break;
        }
        bytes_read = bytes_read
            .checked_add(n as u64)
            .context("hashed byte count overflowed")?;
        anyhow::ensure!(
            bytes_read <= per_file_limit,
            "per-file byte ceiling of {per_file_limit} exceeded while hashing {}",
            path.display()
        );
        hasher.update(&chunk[..n]);
    }
    anyhow::ensure!(
        bytes_read == expected.len(),
        "listed artifact {} was truncated or grew while hashing (expected {}, read {})",
        path.display(),
        expected.len(),
        bytes_read
    );
    let final_metadata = file
        .metadata()
        .with_context(|| format!("rechecking {} after hashing", path.display()))?;
    anyhow::ensure!(
        final_metadata.len() == expected.len()
            && (final_metadata.dev(), final_metadata.ino()) == (expected.dev(), expected.ino())
            && metadata_times(&final_metadata) == metadata_times(expected),
        "listed artifact {} raced while hashing",
        path.display()
    );
    let mut hex = String::with_capacity(64);
    for byte in hasher.finalize() {
        write!(&mut hex, "{byte:02x}").expect("writing to String cannot fail");
    }
    check_deadline(deadline, now)?;
    Ok(hex)
}

fn metadata_times(metadata: &Metadata) -> (i64, i64, i64, i64) {
    (
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    )
}

fn check_deadline(deadline: Instant, now: &mut impl FnMut() -> Instant) -> Result<()> {
    anyhow::ensure!(
        now() < deadline,
        "deadline exhausted while collecting binary provenance"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::write_executable;
    use std::cell::Cell;
    use std::os::unix::fs::symlink;

    fn executable(path: &Path, bytes: &[u8]) {
        write_executable(path, bytes);
    }

    fn future_deadline() -> Instant {
        Instant::now() + std::time::Duration::from_secs(30)
    }

    fn fake_cargo(root: &Path, executable_path: &Path) -> PathBuf {
        let cargo = root.join("cargo");
        let message = serde_json::json!({
            "reason": "compiler-artifact",
            "executable": executable_path,
        });
        executable(
            &cargo,
            format!("#!/bin/sh\nprintf '%s\\n' '{}'\n", message).as_bytes(),
        );
        cargo
    }

    #[test]
    fn listing_preserves_selectors_and_replaces_runtime_tail() {
        let argv = vec![
            "env".into(),
            "RUSTFLAGS=-g".into(),
            "cargo".into(),
            "+nightly".into(),
            "test".into(),
            "-p".into(),
            "widget".into(),
            "--message-format=short".into(),
            "--".into(),
            "name-filter".into(),
        ];
        assert_eq!(
            artifact_listing_argv(&argv).unwrap().unwrap(),
            vec![
                "env",
                "RUSTFLAGS=-g",
                "cargo",
                "+nightly",
                "test",
                "-p",
                "widget",
                "--no-run",
                "--message-format=json",
            ]
        );
    }

    #[test]
    fn a_step_that_is_itself_no_run_has_no_executed_binary_provenance() {
        // The step builds but executes nothing; Complete would assert
        // "cargo ran them" about binaries nothing ran.
        let argv: Vec<String> = ["cargo", "test", "-p", "widget", "--no-run"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(artifact_listing_argv(&argv).unwrap(), None);
    }

    #[test]
    fn non_test_steps_are_not_applicable() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        std::fs::create_dir(&target).unwrap();
        for argv in [vec!["true".into()], vec!["cargo".into(), "clippy".into()]] {
            assert_eq!(
                collect(&argv, tmp.path(), &target, future_deadline(), false),
                BinaryProvenance::NotApplicable
            );
        }
    }

    #[test]
    fn cargo_artifact_listing_is_the_source_and_warm_reuse_stays_complete() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        let artifact = target.join("debug/deps/fixture-test-123");
        std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        executable(&artifact, b"abc");
        let cargo = fake_cargo(tmp.path(), &artifact);
        let argv = vec![cargo.to_string_lossy().into_owned(), "test".into()];
        let expected = BinaryProvenance::Complete {
            binaries: vec![BinaryDigest {
                path: "debug/deps/fixture-test-123".to_string(),
                digest: "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
                    .to_string(),
            }],
        };
        assert_eq!(
            collect(&argv, tmp.path(), &target, future_deadline(), false),
            expected
        );
        // No rewrite or mtime touch between calls: the second warm listing
        // still names and hashes the reused executable.
        assert_eq!(
            collect(&argv, tmp.path(), &target, future_deadline(), false),
            expected
        );
    }

    #[test]
    fn cargo_listing_outside_private_target_is_unavailable() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let outside = tmp.path().join("outside-test");
        executable(&outside, b"outside");
        let cargo = fake_cargo(tmp.path(), &outside);
        let evidence = collect(
            &[cargo.to_string_lossy().into_owned(), "test".into()],
            tmp.path(),
            &target,
            future_deadline(),
            false,
        );
        let BinaryProvenance::Unavailable { reason } = evidence else {
            panic!("escaping Cargo artifact must make provenance unavailable");
        };
        assert!(
            reason.contains("outside canonical private target"),
            "{reason}"
        );
    }

    #[test]
    fn symlink_and_fifo_artifacts_are_rejected_before_read_open() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let real = target.join("real");
        executable(&real, b"real");
        let linked = target.join("linked");
        symlink(&real, &linked).unwrap();
        let error = hash_listed_paths(
            &target.canonicalize().unwrap(),
            vec![linked],
            future_deadline(),
            PRODUCTION_LIMITS,
            Instant::now,
        )
        .unwrap_err();
        assert!(error.to_string().contains("symlink"));

        let fifo = target.join("fifo");
        let path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o755) }, 0);
        let error = hash_listed_paths(
            &target.canonicalize().unwrap(),
            vec![fifo],
            future_deadline(),
            PRODUCTION_LIMITS,
            Instant::now,
        )
        .unwrap_err();
        assert!(error.to_string().contains("non-regular"));
    }

    #[test]
    fn byte_caps_and_inode_deduplication_remain_enforced() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let a = target.join("a");
        let a_link = target.join("a-link");
        let b = target.join("b");
        executable(&a, b"1234");
        std::fs::hard_link(&a, &a_link).unwrap();
        executable(&b, b"5678");
        let error = hash_listed_paths(
            &target.canonicalize().unwrap(),
            vec![a, a_link, b],
            future_deadline(),
            Limits {
                per_file: 10,
                aggregate: 7,
                entries: 10,
            },
            Instant::now,
        )
        .unwrap_err();
        assert!(error.to_string().contains("aggregate byte ceiling"));
        assert!(error.to_string().contains("8 bytes"));
    }

    #[test]
    fn deadline_exhaustion_mid_hash_is_unavailable_without_changing_verdict() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let artifact = target.join("large");
        executable(&artifact, &vec![0x5a; HASH_CHUNK_BYTES * 3]);
        let before = Instant::now();
        let deadline = before + std::time::Duration::from_secs(1);
        let checks = Cell::new(0usize);
        let error = hash_listed_paths(
            &target.canonicalize().unwrap(),
            vec![artifact],
            deadline,
            Limits {
                per_file: 8 * 1024 * 1024,
                aggregate: 8 * 1024 * 1024,
                entries: 10,
            },
            || {
                let count = checks.get() + 1;
                checks.set(count);
                if count < 6 { before } else { deadline }
            },
        )
        .unwrap_err();
        let reason = format!("{error:#}");
        assert!(reason.contains("deadline exhausted"), "{reason}");
        let step = crate::verify::VerifyStep {
            command: "cargo test".to_string(),
            pass: true,
            exit_code: Some(0),
            tail: String::new(),
            annotations: Vec::new(),
            sccache_incident: None,
            executed_binaries: Some(BinaryProvenance::Unavailable { reason }),
        };
        assert!(step.pass);
    }

    #[test]
    fn evidence_states_are_distinct_in_json() {
        let states = [
            BinaryProvenance::Complete { binaries: vec![] },
            BinaryProvenance::NotApplicable,
            BinaryProvenance::Unavailable {
                reason: "missing".to_string(),
            },
        ];
        let json: Vec<_> = states
            .iter()
            .map(|value| serde_json::to_value(value).unwrap())
            .collect();
        assert_ne!(json[0], json[1]);
        assert_ne!(json[1], json[2]);
        for (value, encoded) in states.iter().zip(json) {
            assert_eq!(
                serde_json::from_value::<BinaryProvenance>(encoded).unwrap(),
                *value
            );
        }
    }
}
