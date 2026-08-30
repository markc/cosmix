//! Cargo target-dir garbage collection: bounded size, stalest-first cleanup.
//!
//! Historical shared-cache note, retained for API context and superseded by
//! the 0.13.0 correction below: every task worktree was treated as leaving a
//! full artifact set in `CARGO_TARGET_DIR`. This module implements a GC that:
//! - Checks a target directory's size
//! - Removes stalest entries under {debug,release}/{deps,build,.fingerprint}
//! - Never removes the whole directory (a full re-cold is the failure mode)
//! - Validates containment before any removal
//!
//! Nightly wiring (operator-side, outside this repo): the tier-2 systemd
//! unit runs `foreman gc-cache` as the FIRST command in
//! `tier2_commands` in `foreman.conf.mix` — the cap is
//! enforced before the rest of the nightly tier adds fresh artifacts on top
//! of an already-oversized cache.
//!
//! **`$VAR` does not expand in command entries.** Each entry is split on
//! whitespace into an argv that `run_step` hands
//! straight to `Command` — there is no shell, and systemd does not expand
//! `$VAR` inside a data string either. A step written as
//! `--dir $CARGO_TARGET_DIR` therefore passes the eleven literal characters
//! `$CARGO_TARGET_DIR` as the path, which does not exist. Two forms that
//! actually work:
//!
//! ```text
//! # 1. No --dir: the dir comes from CARGO_TARGET_DIR in foreman's OWN
//! #    environment, which the unit already sets for the cargo steps and
//! #    which run_step inherits. Expansion happens in-process, so there is
//! #    no shell to miss.
//! Environment=CARGO_TARGET_DIR=/home/user/.cmctl/.foreman/target
//! tier2_commands: ["foreman gc-cache", "cargo test --workspace"]
//!
//! # 2. A literal absolute path in the step itself.
//! tier2_commands: ["foreman gc-cache --dir /home/user/.cmctl/.foreman/target", ...]
//! ```
//!
//! Either way the step is loud when the cap is not met: a missing target
//! dir is an error, and finishing still over the cap exits non-zero rather
//! than printing "nothing to do" — see [`GcOutcome`].
//!
//! **Correction for per-worktree targets (0.13.0, now completed):** the
//! historical wiring above is obsolete. Cargo's unit metadata does not
//! distinguish sibling worktrees reliably, so verifiers use one private
//! `src/target/` per live worktree. Terminal scratch is now reclaimed by the
//! refinery and the `gc-scratch` timer backstop; live worktrees are never
//! selected. This module remains the bounded, stalest-first mechanism used
//! for the shared `target/` and `target-refine/` caches.

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use anyhow::{Context, Result};

use crate::ledger::Ledger;

/// Default maximum cache size in GB.
pub const DEFAULT_MAX_GB: u64 = 40;
/// Environment variable for the max GB cap.
pub const MAX_GB_ENV: &str = "FOREMAN_CACHE_MAX_GB";

#[derive(Debug, Clone, Default)]
pub struct ReviewWorktreeGcReport {
    pub candidates: usize,
    pub removed: usize,
    pub paths: Vec<PathBuf>,
}

/// Reclaim deterministic legacy review checkouts left by a process crash.
/// Normal landing cleanup removes these through `TempWorktree::drop`; this is
/// the `gc-scratch` backstop and deliberately selects only terminal tasks.
pub fn reclaim_terminal_review_worktrees(
    ledger: &Ledger,
    repo: &Path,
    dry_run: bool,
) -> Result<ReviewWorktreeGcReport> {
    let repo = repo
        .canonicalize()
        .with_context(|| format!("canonicalizing repository {}", repo.display()))?;
    let parent = repo
        .parent()
        .context("repository has no parent for legacy review worktrees")?;
    let repo_tag = repo
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("repo")
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    let prefix = format!(".foreman-review-{repo_tag}-task-");
    let output = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .context("listing legacy review worktrees")?;
    anyhow::ensure!(
        output.status.success(),
        "git worktree list failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );

    let mut report = ReviewWorktreeGcReport::default();
    for listed in String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
    {
        let path = PathBuf::from(listed);
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(task_id) = name
            .strip_prefix(&prefix)
            .and_then(|suffix| suffix.parse::<i64>().ok())
        else {
            continue;
        };
        anyhow::ensure!(
            path == parent.join(name),
            "refusing legacy review worktree outside repository parent: {}",
            path.display()
        );
        let Some(task) = ledger.task(task_id)? else {
            continue;
        };
        if !matches!(task.status.as_str(), "landed" | "retired") {
            continue;
        }
        report.candidates += 1;
        report.paths.push(path.clone());
        if dry_run {
            continue;
        }
        let removal = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "remove", "--force"])
            .arg(&path)
            .output()
            .with_context(|| format!("removing legacy review worktree {}", path.display()))?;
        anyhow::ensure!(
            removal.status.success(),
            "git worktree remove failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&removal.stderr).trim()
        );
        report.removed += 1;
    }
    if !dry_run && report.removed > 0 {
        let prune = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "prune"])
            .output()
            .context("pruning legacy review worktree metadata")?;
        anyhow::ensure!(
            prune.status.success(),
            "git worktree prune failed: {}",
            String::from_utf8_lossy(&prune.stderr).trim()
        );
    }
    Ok(report)
}

/// Resolve the cache cap: explicit arg → `MAX_GB_ENV` (non-unicode and
/// unparsable values fall through) → [`DEFAULT_MAX_GB`]. Pure seam over the
/// env read so tests exercise every precedence arm without mutating the
/// process environment.
fn cap_from(max_gb: Option<u64>, env: Option<std::ffi::OsString>) -> u64 {
    max_gb
        .or_else(|| {
            env.and_then(|v| v.into_string().ok())
                .and_then(|s| s.parse().ok())
        })
        .unwrap_or(DEFAULT_MAX_GB)
}
/// Backwards-compatible environment variable naming one cache when `--dir`
/// is omitted. Fleet maintenance now uses explicit per-worktree paths; see
/// the module docs.
pub const TARGET_DIR_ENV: &str = "CARGO_TARGET_DIR";

/// Subdirectories under each profile (debug/release) that we're allowed to GC.
const GC_SUBDIRS: &[&str] = &["deps", "build", ".fingerprint"];

/// Profiles we GC under the target directory.
const PROFILES: &[&str] = &["debug", "release"];

/// How a GC run ended. Three states, not two: "under the cap" and "still
/// over the cap having reclaimed nothing" both leave the tree untouched,
/// but only the first is success. Collapsing them into `!trimmed` is what
/// let a mis-wired nightly step print "nothing to do" and exit 0 forever
/// while the cache grew unbounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcOutcome {
    /// The cache was already at or under the cap; nothing was removed.
    UnderCap,
    /// Entries were removed and the cache is now at or under the cap.
    Trimmed,
    /// The pass finished with the cache STILL over the cap — whether it
    /// reclaimed some, or nothing at all. Never green.
    StillOverCap,
}

/// Result of a GC run.
#[derive(Debug)]
pub struct GcReport {
    /// Total size before GC (in bytes).
    pub before_bytes: u64,
    /// Total size after GC (in bytes).
    pub after_bytes: u64,
    /// The cap this pass was enforcing (bytes) — what `after_bytes` is
    /// judged against.
    pub max_bytes: u64,
    /// How the pass ended.
    pub outcome: GcOutcome,
    /// Entries the containment check refused (they canonicalize outside the
    /// target dir, or not at all). Reported rather than silently dropped —
    /// a cache that will not come down to the cap because every stale entry
    /// is an escaping symlink must say so, not read as "nothing to reclaim".
    pub skipped_uncontained: usize,
    /// Entries selected from the six allowed GC subdirectories. This is
    /// useful to dry-run callers which need to report what the same
    /// stalest-first pass would remove.
    pub candidate_entries: usize,
}

impl GcReport {
    /// Did this pass actually remove anything?
    pub fn trimmed(&self) -> bool {
        self.before_bytes > self.after_bytes
    }

    /// Human-readable summary.
    pub fn summary(&self) -> String {
        let gb = |bytes: u64| bytes as f64 / (1024.0 * 1024.0 * 1024.0);
        let mut out = format!(
            "cache: {:.2} GB → {:.2} GB ({:.2} GB removed, cap {:.2} GB)",
            gb(self.before_bytes),
            gb(self.after_bytes),
            gb(self.before_bytes.saturating_sub(self.after_bytes)),
            gb(self.max_bytes),
        );
        out.push_str(match self.outcome {
            GcOutcome::UnderCap => " — already under cap",
            GcOutcome::Trimmed => " — under cap",
            GcOutcome::StillOverCap => " — STILL OVER CAP",
        });
        if self.skipped_uncontained > 0 {
            out.push_str(&format!(
                " — {} entr{} skipped: not contained in the target dir",
                self.skipped_uncontained,
                if self.skipped_uncontained == 1 {
                    "y"
                } else {
                    "ies"
                },
            ));
        }
        out
    }
}

/// Resolve which directory to GC: the explicit `--dir`, else
/// `$CARGO_TARGET_DIR` from foreman's own environment (the nightly path —
/// see the module docs on why no shell expands it for us), else a refusal.
///
/// A refusal, not a default. `target` relative to whatever cwd the unit
/// happened to land in is the same silent no-op as an unexpanded `$VAR`.
pub fn resolve_target_dir(dir: Option<PathBuf>) -> Result<PathBuf> {
    resolve_target_dir_with(dir, std::env::var_os(TARGET_DIR_ENV))
}

/// The precedence itself, with the environment passed in. Tested through
/// this seam rather than by mutating the real `CARGO_TARGET_DIR`: this test
/// binary shares its environment with every other test in the process, some
/// of which spawn cargo, and a stray `CARGO_TARGET_DIR` would redirect their
/// build output.
fn resolve_target_dir_with(
    dir: Option<PathBuf>,
    env: Option<std::ffi::OsString>,
) -> Result<PathBuf> {
    if let Some(dir) = dir {
        return Ok(dir);
    }
    match env {
        Some(v) if !v.is_empty() => Ok(PathBuf::from(v)),
        _ => anyhow::bail!(
            "no cache directory: pass --dir <target> or set {TARGET_DIR_ENV} in \
             foreman's environment (note: {TARGET_DIR_ENV} is read here, in-process — \
             a literal \"${TARGET_DIR_ENV}\" in tier2_commands is never expanded)"
        ),
    }
}

/// Run GC on a target directory.
///
/// # Arguments
/// * `target_dir` - The cargo target directory to GC. Must exist: a path
///   that isn't there measures 0 bytes, which would read as "under cap" —
///   the exact way a mis-wired step goes green forever.
/// * `max_gb` - Maximum size in GB (uses [`DEFAULT_MAX_GB`] if None, or env var).
///
/// # Returns
/// A report describing the before/after state.
pub fn run_gc(target_dir: &Path, max_gb: Option<u64>) -> Result<GcReport> {
    run_gc_internal(target_dir, max_gb, dir_size, false)
}

/// Plan the same bounded, stalest-first pass without removing anything.
/// `after_bytes` is the projected size after the selected entries, while
/// `before_bytes` and every path on disk remain unchanged.
pub fn plan_gc(target_dir: &Path, max_gb: Option<u64>) -> Result<GcReport> {
    run_gc_internal(target_dir, max_gb, dir_size, true)
}

/// Internal GC implementation that accepts a size measurement function.
fn run_gc_internal<F>(
    target_dir: &Path,
    max_gb: Option<u64>,
    size_fn: F,
    dry_run: bool,
) -> Result<GcReport>
where
    F: Fn(&Path) -> Result<u64>,
{
    // Resolve max_gb from arg → env → default
    let max_gb = cap_from(max_gb, std::env::var_os(MAX_GB_ENV));

    // Saturating: an absurd FOREMAN_CACHE_MAX_GB (say 2^60) would wrap the
    // multiply in release and produce a TINY cap — a runaway deletion from
    // a fat-fingered env var. Saturate to "no cap in practice" instead.
    let max_bytes = max_gb.saturating_mul(1024 * 1024 * 1024);

    // The dir must exist. Measuring a path that isn't there yields 0 bytes,
    // which is trivially "under cap" — how an unexpanded `$CARGO_TARGET_DIR`
    // or a stale path stays green while the real cache grows unbounded.
    anyhow::ensure!(
        target_dir.is_dir(),
        "cache directory {} does not exist (or is not a directory) — refusing to \
         report a nonexistent cache as under cap",
        target_dir.display()
    );
    // Canonicalize once: this is the root every removal candidate is
    // contained against, so it must be symlink-free before any comparison.
    let target_dir = target_dir
        .canonicalize()
        .with_context(|| format!("target directory {}", target_dir.display()))?;

    // Measure current size
    let before_bytes = size_fn(&target_dir)?;

    if before_bytes <= max_bytes {
        return Ok(GcReport {
            before_bytes,
            after_bytes: before_bytes,
            max_bytes,
            outcome: GcOutcome::UnderCap,
            skipped_uncontained: 0,
            candidate_entries: 0,
        });
    }

    // Collect all removable entries with their mtimes
    let mut entries: Vec<(PathBuf, SystemTime)> = Vec::new();
    for profile in PROFILES {
        for subdir in GC_SUBDIRS {
            let path = target_dir.join(profile).join(subdir);
            if let Ok(iter) = fs::read_dir(&path) {
                for entry in iter.flatten() {
                    let path = entry.path();
                    // We only remove regular files and directories within the GC subdir
                    // Never remove the GC subdir itself (would break future builds)
                    let metadata = entry.metadata()?;
                    let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                    entries.push((path, mtime));
                }
            }
        }
    }

    // Sort by mtime (oldest first) for stalest-first removal
    entries.sort_by_key(|(_, mtime)| *mtime);

    // Remove entries until we're under the cap
    let mut removed_bytes = 0u64;
    let mut skipped_uncontained = 0usize;
    let mut candidate_entries = 0usize;
    for (path, _) in entries {
        // Check if we're already under the cap AFTER previous removals.
        // Saturating: `size_fn` measures each entry at removal time, which
        // can outrun the one-shot `before_bytes` if a build is writing
        // concurrently. Overshooting the total means "done", not a panic.
        let current_bytes = before_bytes.saturating_sub(removed_bytes);
        if current_bytes <= max_bytes {
            break;
        }

        // Containment: a symlink under deps/build/.fingerprint could point
        // anywhere. Refuse to touch anything that doesn't canonicalize to
        // somewhere inside target_dir — same fail-closed pattern as
        // runner::resolve_verify_dir. Skip (never remove) rather than error,
        // so one hostile/broken entry can't abort the whole GC pass; the
        // count rides out in the report so the skip isn't silent.
        if canonical_contained(&target_dir, &path).is_err() {
            skipped_uncontained += 1;
            continue;
        }

        // Measure before removal
        let size = size_fn(&path)?;
        if size == 0 {
            continue;
        }
        candidate_entries += 1;

        // A dry-run advances the same projected-size counter without
        // mutating the tree, so selection and ordering are identical to the
        // real pass.
        if !dry_run {
            if path.is_dir() {
                fs::remove_dir_all(&path)
                    .with_context(|| format!("removing {}", path.display()))?;
            } else {
                fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            }
        }
        removed_bytes = removed_bytes.saturating_add(size);
    }

    // A real pass remeasures the root rather than projecting before_bytes
    // minus what we removed: a concurrent Cargo write landing mid-sweep, or
    // an entry hard-linked elsewhere, means the subtraction can be wrong in
    // either direction. `dry_run` never mutates the tree, so its "after" is
    // necessarily the projection — there is nothing new on disk to measure.
    let after_bytes = if dry_run {
        before_bytes.saturating_sub(removed_bytes)
    } else {
        size_fn(&target_dir)?
    };
    // Judged on the cap, not on whether anything moved. Over-cap-and-
    // reclaimed-nothing (dir full of out-of-bounds bloat, every stale entry
    // refused by containment) is a failure that must not read as success.
    let outcome = if after_bytes <= max_bytes {
        GcOutcome::Trimmed
    } else {
        GcOutcome::StillOverCap
    };
    Ok(GcReport {
        before_bytes,
        after_bytes,
        max_bytes,
        outcome,
        skipped_uncontained,
        candidate_entries,
    })
}

/// Canonicalize `candidate` and verify it resolves to somewhere inside the
/// already-canonical `root` — the removal path's containment gate, mirroring
/// `runner::resolve_verify_dir`.
///
/// Errors — never touch it — for anything that escapes `root` (a symlink
/// pointing outside the target dir) or that fails to canonicalize at all
/// (a dangling symlink, a racing concurrent build). Both are "cannot prove
/// this is ours", which is a refusal, not a removal.
fn canonical_contained(root: &Path, candidate: &Path) -> Result<PathBuf> {
    let resolved = candidate
        .canonicalize()
        .with_context(|| format!("canonicalizing GC candidate {}", candidate.display()))?;
    anyhow::ensure!(
        resolved.starts_with(root),
        "GC candidate {} resolves to {} — outside the target dir {}",
        candidate.display(),
        resolved.display(),
        root.display()
    );
    Ok(resolved)
}

/// Compute the allocated size of a directory tree (bytes), equivalent to
/// `du -sB1` rather than apparent file length.
///
/// Returns 0 for a non-existent path. Permission and traversal failures are
/// errors: reporting an unreadable cache as empty would make a stopped sweep
/// look healthy.
///
/// Uses `symlink_metadata`, so a symlink counts as the link itself and is
/// never followed. Following would charge the cache for bytes it does not
/// own (an escaping link inflating `before_bytes` into a phantom overage)
/// and would recurse forever on a link cycle.
///
/// A multiply-linked file is counted ONCE per traversal, like `du`. Cargo
/// hard-links rather than copies — `target/debug/<bin>` is typically a
/// second name for `target/debug/deps/<bin>-<hash>` — so charging every
/// name would inflate a cache's measured size, and with it both the
/// "reclaimed" figure a sweep reports and the overage a bound is trimmed
/// against. That is not a cosmetic error in either direction: an inflated
/// `before_bytes` is what would trim a hot shared cache that was never
/// actually over its bound, trading the I/O bottleneck this whole module
/// exists to remove for a rebuild bottleneck.
pub fn allocated_size(path: &Path) -> Result<u64> {
    let mut seen_links = HashSet::new();
    allocated_size_dedup(path, &mut seen_links)
}

/// [`allocated_size`]'s recursion, threading the set of `(dev, ino)` pairs
/// already charged so a hard-linked file is counted once per traversal.
///
/// Only multiply-linked NON-directories are recorded. Directories are
/// skipped because `st_nlink` on a directory counts `.` and its
/// subdirectories rather than alternative names for the same bytes — every
/// directory would look "already seen" and the walk would charge nothing.
/// Single-linked files are skipped too: they cannot be reached by another
/// name, so tracking them would grow the set by one entry per file in a
/// cache with hundreds of thousands of them, to answer a question that can
/// only be "no".
fn allocated_size_dedup(path: &Path, seen_links: &mut HashSet<(u64, u64)>) -> Result<u64> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(error).with_context(|| format!("measuring {}", path.display()));
        }
    };

    // POSIX st_blocks is expressed in 512-byte units and is what `du`
    // accounts. Using file length here materially overstates reclaim on
    // sparse or compressed Cargo artefacts (the production ZFS dataset is
    // compressed), so every public report is based on allocated bytes.
    let charge_blocks = if metadata.is_dir() || metadata.nlink() <= 1 {
        true
    } else {
        // First name for these bytes charges them; every later name is a
        // second reference to storage already accounted for.
        seen_links.insert((metadata.dev(), metadata.ino()))
    };
    let mut total = if charge_blocks {
        metadata.blocks().saturating_mul(512)
    } else {
        0
    };
    if metadata.is_dir() {
        let entries = fs::read_dir(path)
            .with_context(|| format!("reading directory while measuring {}", path.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| {
                format!("reading directory entry while measuring {}", path.display())
            })?;
            total = total.saturating_add(allocated_size_dedup(&entry.path(), seen_links)?);
        }
    }
    Ok(total)
}

fn dir_size(path: &Path) -> Result<u64> {
    allocated_size(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::Write;
    use tempfile::TempDir;

    /// Age a fixture entry: set its mtime `age_secs` into the past.
    ///
    /// Uses `utimensat(AT_SYMLINK_NOFOLLOW)` so it stamps the entry ITSELF,
    /// never the thing a symlink points at — the containment fixture needs a
    /// stale symlink whose target's own mtime is untouched. `libc` is already
    /// a crate dependency, so ageing fixtures costs no new third-party crate.
    fn touch_aged(path: &Path, age_secs: u64) -> Result<()> {
        use std::os::unix::ffi::OsStrExt;

        let now = SystemTime::now();
        let aged = now
            .checked_sub(std::time::Duration::from_secs(age_secs))
            .unwrap_or(now);
        let secs = aged.duration_since(SystemTime::UNIX_EPOCH)?.as_secs() as libc::time_t;
        let stamp = libc::timespec {
            tv_sec: secs,
            tv_nsec: 0,
        };
        let times = [stamp, stamp];
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())?;

        // SAFETY: `c_path` is a NUL-terminated path living across the call,
        // and `times` is the 2-element [atime, mtime] array utimensat reads.
        let rc = unsafe {
            libc::utimensat(
                libc::AT_FDCWD,
                c_path.as_ptr(),
                times.as_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        anyhow::ensure!(
            rc == 0,
            "utimensat({}): {}",
            path.display(),
            std::io::Error::last_os_error()
        );
        Ok(())
    }

    /// Test-only helper that runs GC with a custom size measurement function.
    /// This allows tests to control file sizes without creating large files.
    fn run_gc_with_size_fn<F>(
        target_dir: &Path,
        max_gb: Option<u64>,
        size_fn: F,
    ) -> Result<GcReport>
    where
        F: Fn(&Path) -> Result<u64>,
    {
        run_gc_internal(target_dir, max_gb, size_fn, false)
    }

    #[test]
    fn allocated_size_does_not_report_sparse_apparent_length() {
        let tmp = TempDir::new().unwrap();
        let sparse = tmp.path().join("sparse-artifact");
        let file = File::create(&sparse).unwrap();
        file.set_len(1024 * 1024 * 1024).unwrap();

        let allocated = allocated_size(&sparse).unwrap();

        assert!(
            allocated < 1024 * 1024 * 1024,
            "allocated bytes must not be the 1 GiB apparent length: {allocated}"
        );
    }

    /// Cargo hard-links its finished binaries into `debug/` from
    /// `debug/deps/`, so a real target dir holds the same bytes under two
    /// names. `du` charges them once; so must this, or a sweep reports
    /// physical space it never reclaimed and trims a cache that was under
    /// its bound all along.
    #[test]
    fn allocated_size_charges_hard_linked_bytes_once() {
        let tmp = TempDir::new().unwrap();
        let deps = tmp.path().join("debug/deps");
        fs::create_dir_all(&deps).unwrap();

        let artifact = deps.join("app-abc123");
        let mut file = File::create(&artifact).unwrap();
        file.write_all(&vec![7u8; 256 * 1024]).unwrap();
        file.sync_all().unwrap();
        drop(file);

        let alone = allocated_size(tmp.path()).unwrap();

        // The second name Cargo would create for exactly these bytes.
        fs::hard_link(&artifact, tmp.path().join("debug/app")).unwrap();
        let with_link = allocated_size(tmp.path()).unwrap();

        assert_eq!(
            with_link, alone,
            "a second hard link to already-counted bytes must not add to the total: \
             {alone} became {with_link}"
        );

        // And an independent copy of the same bytes still counts, so the
        // dedup cannot be passing by measuring nothing at all.
        let mut copy = File::create(deps.join("other-def456")).unwrap();
        copy.write_all(&vec![7u8; 256 * 1024]).unwrap();
        copy.sync_all().unwrap();
        drop(copy);

        let with_copy = allocated_size(tmp.path()).unwrap();
        assert!(
            with_copy > with_link,
            "a distinct file holding its own bytes must be charged: \
             {with_link} should have grown, got {with_copy}"
        );
    }

    #[test]
    fn gc_trims_to_cap() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();

        // Create a debug/deps structure with small files (we'll mock their sizes)
        let deps = target.join("debug/deps");
        fs::create_dir_all(&deps).unwrap();

        // Create small files (actual size doesn't matter due to mock)
        let old_file = deps.join("old.rlib");
        File::create(&old_file).unwrap();
        touch_aged(&old_file, 1000).unwrap();

        let new_file = deps.join("new.rlib");
        File::create(&new_file).unwrap();
        touch_aged(&new_file, 10).unwrap();

        let mid_file = deps.join("mid.rlib");
        File::create(&mid_file).unwrap();
        touch_aged(&mid_file, 100).unwrap();

        // Mock size function: report sizes as if files were large. The root
        // (and `deps`) are remeasured after real removal, so their mock size
        // must reflect which of the three files still exist on disk rather
        // than a fixed constant — a real `dir_size` would too.
        let mut entry_sizes = std::collections::HashMap::new();
        entry_sizes.insert(old_file.clone(), 400 * 1024 * 1024_u64);
        entry_sizes.insert(mid_file.clone(), 300 * 1024 * 1024_u64);
        entry_sizes.insert(new_file.clone(), 400 * 1024 * 1024_u64);
        let entries = entry_sizes.clone();

        let size_fn = move |path: &Path| {
            if let Some(&size) = entry_sizes.get(path) {
                return Ok(size);
            }
            if path == deps || path == target {
                let total: u64 = entries
                    .iter()
                    .filter(|(entry_path, _)| entry_path.exists())
                    .map(|(_, size)| *size)
                    .sum();
                return Ok(total);
            }
            Ok(0)
        };

        // Cap at 1GB → should remove oldest files until under.
        let report = run_gc_with_size_fn(target, Some(1), size_fn).unwrap();

        assert!(report.trimmed(), "should have trimmed");
        assert!(
            report.after_bytes < report.before_bytes,
            "should be smaller after GC"
        );
        // New file should still exist (it's the newest)
        assert!(new_file.exists(), "newest file should remain");
        // Old file should be gone (it's the oldest)
        assert!(!old_file.exists(), "oldest file should be removed");
    }

    #[test]
    fn gc_skips_when_under_cap() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();

        let deps = target.join("debug/deps");
        fs::create_dir_all(&deps).unwrap();

        let tiny = deps.join("tiny.rlib");
        let mut f = File::create(&tiny).unwrap();
        f.write_all(&vec![0u8; 1024]).unwrap();
        f.flush().unwrap();
        drop(f);

        // Cap at 1024GB, we have 1KB → no trimming
        let report = run_gc(target, Some(1024)).unwrap();

        assert!(!report.trimmed(), "should not trim when under cap");
        assert_eq!(report.after_bytes, report.before_bytes, "size unchanged");
        assert_eq!(report.outcome, GcOutcome::UnderCap);
    }

    #[test]
    fn dry_run_plans_the_same_entries_without_removing_them() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();
        let deps = target.join("debug/deps");
        fs::create_dir_all(&deps).unwrap();
        let candidate = deps.join("stale.rlib");
        fs::write(&candidate, vec![0_u8; 4096]).unwrap();

        let report = plan_gc(target, Some(0)).unwrap();

        assert_eq!(report.candidate_entries, 1);
        assert!(report.before_bytes > report.after_bytes);
        assert!(
            candidate.is_file(),
            "planning must not delete the candidate"
        );
    }

    /// A path that isn't there measures zero bytes, which is trivially
    /// "under cap". That is precisely how a mis-wired nightly step — an
    /// unexpanded `$CARGO_TARGET_DIR`, a stale path — reports success while
    /// the real cache grows without bound. Refuse instead.
    #[test]
    fn gc_refuses_a_nonexistent_dir() {
        let tmp = TempDir::new().unwrap();

        for bad in [
            tmp.path().join("nonexistent"),
            // The literal string an unexpanded `--dir $CARGO_TARGET_DIR`
            // hands us.
            tmp.path().join("$CARGO_TARGET_DIR"),
        ] {
            let err = run_gc(&bad, Some(10)).unwrap_err();
            assert!(
                format!("{err:#}").contains("does not exist"),
                "got: {err:#}"
            );
        }

        // A regular file where a target dir was expected is the same class
        // of wiring error, and must not read as an empty cache either.
        let not_a_dir = tmp.path().join("target");
        fs::write(&not_a_dir, b"x").unwrap();
        assert!(run_gc(&not_a_dir, Some(10)).is_err());
    }

    /// Over the cap with nothing reclaimable is NOT "nothing to do".
    #[test]
    fn over_cap_with_nothing_reclaimable_is_not_success() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();

        // All the bloat lives outside the six GC subdirs, so the pass is
        // allowed to remove exactly nothing.
        let incremental = target.join("debug/incremental");
        fs::create_dir_all(&incremental).unwrap();
        let hog = incremental.join("session.bin");
        fs::write(&hog, vec![0u8; 4096]).unwrap();

        let report = run_gc(target, Some(0)).unwrap();

        assert!(!report.trimmed(), "there was nothing it could take");
        assert_eq!(
            report.outcome,
            GcOutcome::StillOverCap,
            "over cap and empty-handed must be distinguishable from under cap"
        );
        assert!(report.after_bytes > report.max_bytes);
        assert!(
            report.summary().contains("STILL OVER CAP"),
            "operator-facing summary must say so: {}",
            report.summary()
        );
        assert!(
            hog.exists(),
            "and it must not have taken the hog to get there"
        );
    }

    /// A real (non-dry-run) pass must remeasure the root rather than report
    /// `before_bytes - removed_bytes`: a concurrent Cargo write landing
    /// mid-sweep (or a hard-linked entry) can leave the cache above what the
    /// subtraction would claim. Modelled here with a `size_fn` whose second
    /// call (the post-removal remeasurement) reports growth the projection
    /// could never see.
    #[test]
    fn real_pass_remeasures_after_bytes_instead_of_projecting() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();
        let deps = target.join("debug/deps");
        fs::create_dir_all(&deps).unwrap();
        let stale = deps.join("stale.rlib");
        fs::write(&stale, vec![0u8; 4096]).unwrap();
        touch_aged(&stale, 1000).unwrap();

        let calls = std::cell::Cell::new(0u32);
        let size_fn = |path: &Path| {
            if path == target {
                let call = calls.get();
                calls.set(call + 1);
                if call == 0 {
                    Ok(4096)
                } else {
                    // stale.rlib is gone, but 9000 bytes of concurrent
                    // writes landed while the pass was removing it.
                    Ok(9000)
                }
            } else if path == stale {
                Ok(4096)
            } else {
                Ok(0)
            }
        };

        let report = run_gc_with_size_fn(target, Some(0), size_fn).unwrap();

        assert!(!stale.exists(), "the stale entry was still removed");
        assert_eq!(
            report.after_bytes, 9000,
            "after_bytes must be the remeasured root, not before_bytes - removed_bytes"
        );
    }

    /// Partial reclamation that still leaves the cache over the cap is also
    /// not success — `trimmed()` is true, the outcome is still red.
    #[test]
    fn partial_reclamation_still_over_cap_is_not_success() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();
        let deps = target.join("debug/deps");
        let incremental = target.join("debug/incremental");
        fs::create_dir_all(&deps).unwrap();
        fs::create_dir_all(&incremental).unwrap();
        fs::write(deps.join("stale.rlib"), vec![0u8; 4096]).unwrap();
        fs::write(incremental.join("session.bin"), vec![0u8; 4096]).unwrap();

        let report = run_gc(target, Some(0)).unwrap();

        assert!(report.trimmed(), "the deps entry was reclaimable");
        assert_eq!(report.outcome, GcOutcome::StillOverCap);
    }

    #[test]
    fn resolve_target_dir_prefers_the_flag_then_the_env_then_refuses() {
        use std::ffi::OsString;

        let explicit = PathBuf::from("/tmp/explicit-target");
        let env = || Some(OsString::from("/tmp/from-env"));

        // The flag wins, with or without the env set.
        assert_eq!(
            resolve_target_dir_with(Some(explicit.clone()), None).unwrap(),
            explicit
        );
        assert_eq!(
            resolve_target_dir_with(Some(explicit.clone()), env()).unwrap(),
            explicit
        );

        // No flag → the env, which is the nightly unit's route.
        assert_eq!(
            resolve_target_dir_with(None, env()).unwrap(),
            PathBuf::from("/tmp/from-env")
        );

        // Neither → a refusal, never a cwd-relative "target" guess.
        let err = resolve_target_dir_with(None, None).unwrap_err();
        assert!(
            format!("{err:#}").contains("no cache directory"),
            "got: {err:#}"
        );
        // An empty value is a missing value.
        assert!(resolve_target_dir_with(None, Some(OsString::new())).is_err());

        // And the public wrapper reads the real variable through that seam.
        let live = std::env::var_os(TARGET_DIR_ENV);
        match live {
            Some(v) if !v.is_empty() => {
                assert_eq!(resolve_target_dir(None).unwrap(), PathBuf::from(v))
            }
            _ => assert!(resolve_target_dir(None).is_err()),
        }
    }

    #[test]
    fn gc_removes_from_both_profiles() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();

        // debug/deps with an old file
        let debug_deps = target.join("debug/deps");
        fs::create_dir_all(&debug_deps).unwrap();
        let debug_file_path = debug_deps.join("debug_lib.rlib");
        File::create(&debug_file_path).unwrap();
        touch_aged(&debug_file_path, 1000).unwrap();

        // release/deps with an old file
        let release_deps = target.join("release/deps");
        fs::create_dir_all(&release_deps).unwrap();
        let release_file_path = release_deps.join("release_lib.rlib");
        File::create(&release_file_path).unwrap();
        touch_aged(&release_file_path, 1000).unwrap();

        // Mock size function. The root is remeasured after real removal, so
        // its mock size sums whichever of the two files still exist rather
        // than reporting a fixed constant.
        let entries = [
            (debug_file_path.clone(), 600 * 1024 * 1024_u64),
            (release_file_path.clone(), 600 * 1024 * 1024_u64),
        ];
        let size_fn = move |path: &Path| {
            for (entry_path, size) in &entries {
                if path == entry_path {
                    return Ok(*size);
                }
            }
            if path == target || path == debug_deps.parent().and_then(|p| p.parent()).unwrap() {
                let total: u64 = entries
                    .iter()
                    .filter(|(entry_path, _)| entry_path.exists())
                    .map(|(_, size)| *size)
                    .sum();
                return Ok(total);
            }
            Ok(0)
        };

        // Total ~1.2GB (mock), cap at 1GB → should remove at least one (stopping when under cap)
        let report = run_gc_with_size_fn(target, Some(1), size_fn).unwrap();

        assert!(report.trimmed(), "should have trimmed");
        // At least one should be removed (we stop once under the cap)
        assert!(
            !debug_file_path.exists() || !release_file_path.exists(),
            "at least one file should be removed"
        );
    }

    #[test]
    fn gc_only_touches_allowed_subdirs() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();

        // Create something outside the GC subdirs: debug/incremental
        let incremental = target.join("debug/incremental");
        fs::create_dir_all(&incremental).unwrap();
        let incremental_file = incremental.join("hash.rlib");
        File::create(&incremental_file).unwrap();

        // Create something inside GC path: debug/deps
        let deps = target.join("debug/deps");
        fs::create_dir_all(&deps).unwrap();
        let deps_file = deps.join("lib.rlib");
        File::create(&deps_file).unwrap();
        touch_aged(&deps_file, 1000).unwrap();

        // Mock size function: report large sizes for both files. `deps_file`
        // is inside a GC subdir and removable; `incremental_file` is not, so
        // it always contributes to the remeasured root total.
        let entries = [
            (incremental_file.clone(), 600 * 1024 * 1024_u64),
            (deps_file.clone(), 600 * 1024 * 1024_u64),
        ];
        let size_fn = move |path: &Path| {
            for (entry_path, size) in &entries {
                if path == entry_path {
                    return Ok(*size);
                }
            }
            if path == target {
                let total: u64 = entries
                    .iter()
                    .filter(|(entry_path, _)| entry_path.exists())
                    .map(|(_, size)| *size)
                    .sum();
                return Ok(total);
            }
            Ok(0)
        };

        // Total ~1.2GB (mock, incremental + deps), cap at 1GB → only deps should be touched
        let report = run_gc_with_size_fn(target, Some(1), size_fn).unwrap();

        assert!(report.trimmed(), "should have trimmed");
        assert!(
            incremental_file.exists(),
            "incremental (outside GC subdirs) should remain"
        );
        assert!(
            !deps_file.exists(),
            "deps (inside GC subdirs) should be removed"
        );
    }

    #[test]
    fn gc_never_removes_the_gc_subdir_itself() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();

        // Create the GC subdir with content
        let deps = target.join("debug/deps");
        fs::create_dir_all(&deps).unwrap();
        let file = deps.join("lib.rlib");
        File::create(&file).unwrap();

        // Create a large file (1.5GB mock) to trigger GC
        let large_file = deps.join("large.rlib");
        File::create(&large_file).unwrap();

        // Mock size function. Root/profile totals are remeasured after real
        // removal, so they sum whichever entries still exist rather than a
        // fixed constant.
        let entries = [
            (file.clone(), 400 * 1024 * 1024_u64),
            (large_file.clone(), 1500 * 1024 * 1024_u64),
        ];
        let debug_dir = deps.parent().unwrap().to_path_buf();
        let size_fn = move |path: &Path| {
            for (entry_path, size) in &entries {
                if path == entry_path {
                    return Ok(*size);
                }
            }
            if path == target || path == debug_dir {
                let total: u64 = entries
                    .iter()
                    .filter(|(entry_path, _)| entry_path.exists())
                    .map(|(_, size)| *size)
                    .sum();
                return Ok(total);
            }
            Ok(0)
        };

        // Cap at 1GB → should remove the file, NOT the deps dir
        let report = run_gc_with_size_fn(target, Some(1), size_fn).unwrap();

        assert!(report.trimmed(), "should have trimmed");
        assert!(deps.exists(), "deps directory should still exist");
        assert!(!large_file.exists(), "file inside deps should be removed");
    }

    #[test]
    fn containment_refuses_an_outside_path() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("target");
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&target).unwrap();
        fs::create_dir_all(&outside).unwrap();

        // A real (large, stale) file OUTSIDE the target dir — this must
        // survive no matter what.
        let outside_file = outside.join("secret.txt");
        {
            let mut f = File::create(&outside_file).unwrap();
            f.write_all(b"do not touch").unwrap();
        }
        touch_aged(&outside_file, 100_000).unwrap();

        // A symlink INSIDE the GC'd subdir pointing at the outside dir —
        // the classic escape a naive path-based deleter would follow.
        let deps = target.join("debug/deps");
        fs::create_dir_all(&deps).unwrap();
        let escape_link = deps.join("escape");
        std::os::unix::fs::symlink(&outside, &escape_link).unwrap();
        // Stamp the LINK, not its target, so the escape entry sorts stalest
        // (first candidate considered) while `outside`'s own mtime stands.
        touch_aged(&escape_link, 100_000).unwrap();

        // A legitimate stale file inside target, so there's something the
        // cap can actually reclaim.
        let inside_file = deps.join("old.rlib");
        File::create(&inside_file).unwrap();
        touch_aged(&inside_file, 50_000).unwrap();

        // The root is remeasured after real removal, so its mock total sums
        // only entries that actually live inside `target` and still exist —
        // `outside_file` is never part of that sum, matching a real
        // `dir_size(target)` walk that never crosses the escape symlink.
        let root_entries = [
            // Reported huge too — if containment were broken, "removing"
            // the escaping symlink would look like it freed the cap and
            // this test would pass for the wrong reason.
            (escape_link.clone(), 600 * 1024 * 1024_u64),
            (inside_file.clone(), 600 * 1024 * 1024_u64),
        ];
        let size_fn = |path: &Path| {
            if path == outside_file {
                return Ok(600 * 1024 * 1024);
            }
            for (entry_path, size) in &root_entries {
                if path == entry_path {
                    return Ok(*size);
                }
            }
            if path == target || path == deps {
                let total: u64 = root_entries
                    .iter()
                    .filter(|(entry_path, _)| entry_path.exists())
                    .map(|(_, size)| *size)
                    .sum();
                return Ok(total);
            }
            Ok(0)
        };

        let report = run_gc_with_size_fn(&target, Some(1), size_fn).unwrap();

        assert!(report.trimmed(), "should have trimmed the in-bounds file");
        assert!(
            outside_file.exists(),
            "path outside the target dir must never be touched"
        );
        assert_eq!(
            fs::read(&outside_file).unwrap(),
            b"do not touch",
            "outside file contents must be untouched"
        );
        assert!(
            !inside_file.exists(),
            "the legitimate in-bounds stale file should be removed"
        );
        assert!(
            escape_link.exists(),
            "the escaping symlink itself must be left alone, not unlinked"
        );
        assert_eq!(
            report.skipped_uncontained, 1,
            "the refusal must be reported, not silently swallowed"
        );
        assert!(
            report.summary().contains("not contained"),
            "operator-facing summary must name the skip: {}",
            report.summary()
        );
    }

    /// End-to-end through the PUBLIC `run_gc` — real `dir_size`, real
    /// removals, no mocked sizes. A cap of 0 GB is the only way to drive the
    /// real measurement path without fabricating gigabytes: nothing can get
    /// under it, so the GC must consider every candidate and we get to see
    /// exactly which ones it refuses to take.
    #[test]
    fn real_size_path_trims_contained_entries_only() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("target");
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();

        // Deliberately FAT (1 MiB), so if the size walk followed the escape
        // symlink below, `before_bytes` would visibly carry bytes the cache
        // does not own.
        let outside_file = outside.join("keep.txt");
        fs::write(&outside_file, vec![b'x'; 1024 * 1024]).unwrap();

        let deps = target.join("debug/deps");
        let fingerprint = target.join("debug/.fingerprint");
        let build = target.join("release/build");
        let incremental = target.join("debug/incremental");
        for dir in [&deps, &fingerprint, &build, &incremental] {
            fs::create_dir_all(dir).unwrap();
        }

        // Reclaimable: a file in deps, a whole subtree in release/build, and
        // a fingerprint dir — the three shapes cargo actually leaves behind.
        let stale_rlib = deps.join("libstale.rlib");
        fs::write(&stale_rlib, vec![0u8; 4096]).unwrap();
        let build_script = build.join("somecrate-abc123");
        fs::create_dir_all(&build_script).unwrap();
        fs::write(build_script.join("output"), vec![0u8; 4096]).unwrap();
        let fp = fingerprint.join("somecrate-abc123");
        fs::create_dir_all(&fp).unwrap();
        fs::write(fp.join("lib-somecrate.json"), vec![0u8; 512]).unwrap();

        // Off-limits: not one of the GC subdirs.
        let incremental_file = incremental.join("session.bin");
        fs::write(&incremental_file, vec![0u8; 4096]).unwrap();

        // Off-limits: escapes the target dir.
        let escape = deps.join("escape");
        std::os::unix::fs::symlink(&outside, &escape).unwrap();

        let report = run_gc(&target, Some(0)).unwrap();

        assert!(report.trimmed());
        assert!(
            report.after_bytes < report.before_bytes,
            "{}",
            report.summary()
        );
        assert_eq!(report.skipped_uncontained, 1, "{}", report.summary());
        // The size walk must not follow the escape symlink: the whole tree
        // here is a few tens of KB, so crossing into the 1 MiB outside file
        // would show up as a phantom overage measured against a real cap.
        assert!(
            report.before_bytes < 1024 * 1024,
            "size walk followed a symlink out of the cache: {}",
            report.summary()
        );

        // Everything contained and in a GC subdir is gone …
        assert!(!stale_rlib.exists());
        assert!(!build_script.exists());
        assert!(!fp.exists());

        // … but the GC subdirs THEMSELVES survive. Removing these is the
        // full re-cold this cache exists to prevent.
        for dir in [&deps, &fingerprint, &build] {
            assert!(dir.is_dir(), "{} must survive", dir.display());
        }

        // … and so does everything out of bounds.
        assert!(incremental_file.exists(), "incremental is not a GC subdir");
        assert!(
            escape.exists(),
            "the escaping symlink is not ours to unlink"
        );
        assert_eq!(
            fs::read(&outside_file).unwrap(),
            vec![b'x'; 1024 * 1024],
            "the file outside the cache must be byte-for-byte untouched"
        );
    }

    #[test]
    fn canonical_contained_refuses_paths_outside_the_root() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("target");
        let outside = tmp.path().join("outside");
        fs::create_dir_all(root.join("debug/deps")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let root = root.canonicalize().unwrap();

        // In-bounds: accepted, and resolved to a real canonical path.
        let inside = root.join("debug/deps/lib.rlib");
        File::create(&inside).unwrap();
        assert_eq!(
            canonical_contained(&root, &inside).unwrap(),
            inside.canonicalize().unwrap()
        );

        // A plain path outside the root: refused.
        let outside_file = outside.join("secret.txt");
        File::create(&outside_file).unwrap();
        let err = canonical_contained(&root, &outside_file).unwrap_err();
        assert!(
            format!("{err:#}").contains("outside the target dir"),
            "got: {err:#}"
        );

        // A symlink inside the root that resolves outside it: refused on the
        // RESOLVED path, which is the whole point of canonicalizing first.
        let link = root.join("debug/deps/escape");
        std::os::unix::fs::symlink(&outside_file, &link).unwrap();
        assert!(canonical_contained(&root, &link).is_err());

        // `..` traversal that climbs out of the root: refused.
        assert!(canonical_contained(&root, &root.join("debug/deps/../../..")).is_err());

        // A dangling symlink cannot be proven ours, so it is refused too.
        let dangling = root.join("debug/deps/dangling");
        std::os::unix::fs::symlink(root.join("debug/deps/gone.rlib"), &dangling).unwrap();
        assert!(canonical_contained(&root, &dangling).is_err());

        // Sibling-prefix confusion: "<root>-evil" shares a string prefix with
        // the root but is not inside it.
        let sibling = tmp.path().join("target-evil");
        fs::create_dir_all(&sibling).unwrap();
        assert!(canonical_contained(&root, &sibling).is_err());
    }

    /// The cap precedence (arg → env → default) is pinned by the pure seam;
    /// the behavioural arms prove a 0 cap really bites through the full GC
    /// pipeline. The previous version of this test set the variable to 1024
    /// GB over a 1 KB fixture — under the default 40 GB too, so it passed
    /// identically with the env lookup deleted. Set a cap the fixture
    /// VIOLATES, so only a cap that was really enforced can produce the
    /// removal.
    #[test]
    fn env_var_overrides_the_default_cap() {
        // Pure precedence: env → cap, arg → env, default fallback,
        // unparsable and empty values → default.
        assert_eq!(cap_from(None, Some("0".into())), 0);
        assert_eq!(cap_from(Some(1), Some("0".into())), 1);
        assert_eq!(cap_from(None, None), DEFAULT_MAX_GB);
        assert_eq!(cap_from(None, Some("not-a-number".into())), DEFAULT_MAX_GB);
        assert_eq!(
            cap_from(None, Some(std::ffi::OsString::new())),
            DEFAULT_MAX_GB,
            "an empty value is not a cap"
        );

        let tmp = TempDir::new().unwrap();
        let target = tmp.path();
        let deps = target.join("debug/deps");
        fs::create_dir_all(&deps).unwrap();
        let file = deps.join("lib.rlib");
        fs::write(&file, vec![0u8; 4096]).unwrap();

        // The cap must actually bite through the whole pipeline: a 0 GB cap
        // over a 4 KB fixture can only end in removal.
        let report = run_gc(target, Some(0)).unwrap();
        assert_eq!(report.max_bytes, 0);
        assert!(report.trimmed(), "a 0 GB cap must reclaim the stale entry");
        assert!(!file.exists());

        // And the public entry really wires the live variable through the
        // seam — read-only, no mutation, both arms computed from the live
        // value. No test in this binary mutates process environment.
        let live = std::env::var_os(MAX_GB_ENV);
        let report = run_gc(target, None).unwrap();
        assert_eq!(
            report.max_bytes,
            cap_from(None, live).saturating_mul(1024 * 1024 * 1024),
            "run_gc must resolve its cap through cap_from(live env)"
        );
    }

    /// An absurd cap must saturate, not wrap into a tiny one — a wrapped
    /// `u64::MAX * 1024^3` in release would be a runaway deletion driven by
    /// a fat-fingered env var.
    #[test]
    fn an_absurd_cap_saturates_instead_of_wrapping() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();
        let deps = target.join("debug/deps");
        fs::create_dir_all(&deps).unwrap();
        let file = deps.join("lib.rlib");
        fs::write(&file, vec![0u8; 4096]).unwrap();

        let report = run_gc(target, Some(u64::MAX)).unwrap();
        assert_eq!(report.max_bytes, u64::MAX);
        assert_eq!(report.outcome, GcOutcome::UnderCap);
        assert!(file.exists(), "a saturated cap removes nothing");
    }
}
