//! Clone lock: serialization for clone-heavy operations (refine, tier-2 verify)
//!
//! Refine and tier-2 verify both clone worktrees from a shared repo; concurrent
//! runs would race and corrupt each other. This lock serializes them at the
//! binary level (not just in systemd wrappers) by taking an exclusive flock on
//! `<repo>/../clone.lock` for legacy invocations, or the manifest-derived
//! project's own `clone.lock` in project mode.
//!
//! # Re-entrancy: flock never grants a second exclusive, not even to you
//!
//! `flock(2)` says a lock the calling process already holds through another
//! open file description "may be" denied on a fresh `open()` + `LOCK_EX`.
//! On Linux it is. Two compositions in this codebase hit exactly that:
//!
//! 1. **`flock(1)` wrappers.** `foreman-refine.service` and
//!    `foreman-tier2.service` run `flock -w N <clone.lock> foreman …`.
//!    `flock(1)` opens the file, locks it, and **execs** foreman with that
//!    descriptor still open — that inheritance is the whole mechanism by
//!    which the lock outlives the wrapper. So the exec'd foreman is not a
//!    separate process contending for the lock; it *is* the holder, and a
//!    fresh acquire would wait out its own timeout and fail, every tick,
//!    permanently.
//! 2. **`refine --tier 2`.** `refine` holds the lane for its whole run and
//!    then calls [`crate::verify::run_tier`], whose tier-2 arm joins the
//!    lane for the throwaway worktree — a sibling of the repo, so the very
//!    same `clone.lock`.
//!
//! Both are solved the way [`crate::verify::LANE_HELD_ENV`] already solves
//! the verifier lane's delegation: an ancestor announces that it holds the
//! lane, and the descendant joins it instead of re-acquiring. Across
//! processes that announcement is the [`LANE_HELD_ENV`] environment marker
//! (which the operator sets in the two wrapper units); within one process
//! it is tracked directly. [`lane_held`] answers both. This is why the
//! binary is correct whether the wrappers are present or removed — the two
//! deployment orderings are otherwise each unsafe on their own.
//!
//! The holder stamps `(pid, pid_start, acquired_at)` into the lock file the
//! moment it wins the flock — the same identity the ledger's reservation
//! sweep already records (procutil::owner_alive). A waiter that times out
//! reads that stamp back and names who it's blocked on, live or dead,
//! instead of failing silently: the 2026-08-19 incident where an orphaned
//! holder wedged the merge queue for two hours was invisible precisely
//! because the timeout error carried no identity to act on.

use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;

/// Set by an ancestor that already holds the clone lock, telling this
/// process to JOIN that lane rather than re-acquire it — the clone-lane twin
/// of [`crate::verify::LANE_HELD_ENV`], and the same reason: flock never
/// grants a second exclusive on a file the process already holds, so
/// re-acquiring is a guaranteed self-deadlock, not a wait.
///
/// **Operators set this in the two `flock(1)`-wrapped units**
/// (`foreman-refine.service`, `foreman-tier2.service`) —
/// `Environment=FOREMAN_CLONE_LANE_HELD=1` — for as long as those wrappers
/// exist. Remove the wrapper and the marker together, in that order or the
/// same edit; the binary is correct either way, but never with the wrapper
/// and no marker. Presence alone is the signal (any value, including
/// empty), matching [`crate::verify::LANE_HELD_ENV`].
pub const LANE_HELD_ENV: &str = "FOREMAN_CLONE_LANE_HELD";

/// How many [`CloneLock`]s this process currently holds. The in-process half
/// of the handshake: an ancestor's marker arrives through the environment,
/// but `refine`'s own hold has to be visible to the tier-2 verify it calls
/// directly, in the same process, with no environment crossing between them.
static IN_PROCESS_HOLDS: AtomicUsize = AtomicUsize::new(0);

/// Default wait time for clone lock acquisition (15 minutes).
const DEFAULT_WAIT_SECS: u64 = 900;

/// Environment variable for wait timeout (0 = fail fast, empty = default).
const WAIT_ENV: &str = "FOREMAN_CLONE_LOCK_WAIT_SECS";

/// Poll cadence while waiting for the lock. flock has no timeout of its
/// own, so the wait is a poll loop. One cadence, deliberately: a stamp
/// naming a dead pid is NOT evidence the lock is free — the kernel released
/// that holder's flock the instant it exited, so if the acquire is still
/// blocked, something else (a `flock(1)` wrapper, which stamps nothing) is
/// holding it. Polling faster on a dead stamp would spin at that cadence for
/// the whole wait and free nothing.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Exclusive lock on `<repo>/../clone.lock`. Blocks up to the configured wait
/// timeout with a clear error on expiry. Released on drop/exit.
pub struct CloneLock {
    _file: std::fs::File,
}

impl std::fmt::Debug for CloneLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloneLock").finish()
    }
}

/// Who last recorded themselves as the clone lock holder, and whether
/// they're still around. Read from the lock file's contents without taking
/// the flock — safe to call while someone else holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockHolder {
    pub pid: Option<i64>,
    pub pid_start: Option<i64>,
    pub acquired_at: Option<String>,
}

impl LockHolder {
    /// Is the recorded holder still the process that recorded it — pid
    /// alive and (when recorded) the same `/proc` starttime.
    pub fn is_alive(&self) -> bool {
        crate::procutil::owner_alive(self.pid, self.pid_start)
    }

    /// Name who a blocked acquire is blocked on, as well as it can be
    /// known. The stamp in the lock file is the good case; when there
    /// isn't one — `flock(1)` never writes one — fall back to
    /// `/proc/locks`, which at least names the pid, and specifically call
    /// out the case where that pid is US, because "blocked on a lock this
    /// process already holds" is not a wait anyone can outlast.
    fn describe(&self, lock_path: &Path) -> String {
        if let Some(pid) = self.pid {
            let who = if self.is_alive() {
                format!("pid {pid} (alive)")
            } else {
                format!(
                    "pid {pid} (no longer running — the lock should clear on its own; if it \
                     doesn't, the lock file may be stale)"
                )
            };
            return match &self.acquired_at {
                Some(at) => format!("{who}, acquired at {at}"),
                None => who,
            };
        }

        // No stamp. The overwhelmingly likely author of an unstamped lock is
        // `flock(1)`, which writes nothing — so say so every time, and name
        // the marker that resolves it. /proc/locks adds the pid when it can.
        // Note it names the process that TOOK the lock: for a wrapper that
        // is the `flock` parent, not the foreman it exec'd, which is exactly
        // why "am I blocked on myself" cannot be inferred from the pid alone.
        let holders = crate::procutil::flock_holders(lock_path);
        let me = std::process::id() as i64;
        let who = if holders.contains(&me) {
            format!("a descriptor THIS process (pid {me}) already holds")
        } else if holders.is_empty() {
            "an unknown process (no owner info recorded, and /proc/locks names no holder)"
                .to_string()
        } else {
            let list = holders
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            format!("pid {list} per /proc/locks (no owner stamp)")
        };
        format!(
            "{who}. An unstamped holder is almost always a `flock(1)` wrapper — and a \
             wrapper execs foreman with its locked descriptor still open, so this process \
             may BE the holder it is waiting for, which no wait can resolve. If this run \
             was launched by such a wrapper, set {LANE_HELD_ENV}=1 in that unit (or drop \
             the wrapper) so foreman joins the lane instead of re-taking it"
        )
    }
}

/// Does an ancestor process, or this process itself, already hold the clone
/// lane? Callers that would otherwise acquire must JOIN instead — see the
/// module docs; re-acquiring is a self-deadlock, not a wait.
pub fn lane_held() -> bool {
    IN_PROCESS_HOLDS.load(Ordering::SeqCst) > 0 || std::env::var_os(LANE_HELD_ENV).is_some()
}

/// Take the clone lane for `repo`, or `None` when it is already held on this
/// run's behalf (an ancestor's `flock(1)` wrapper, or an outer [`CloneLock`]
/// in this same process). This is the entry point `refine` uses: it OWNS the
/// lane, so it creates the lock file if it has to.
pub fn acquire_lane(repo: &Path) -> Result<Option<CloneLock>> {
    if lane_held() {
        return Ok(None);
    }
    Ok(Some(CloneLock::acquire(repo)?))
}

/// Take the clone lane rooted by an active project manifest. Unlike the
/// legacy repo-parent convention, this remains project-scoped when unrelated
/// repositories are siblings.
pub fn acquire_lane_in_project(root: &Path) -> Result<Option<CloneLock>> {
    if lane_held() {
        return Ok(None);
    }
    Ok(Some(CloneLock::acquire_in_project(root)?))
}

/// Hand the lane down to a child process that would otherwise contend with
/// its own parent — the same delegation the marker expresses across an
/// exec, applied to a spawn. A no-op when this run holds no lane, so it can
/// be called unconditionally at a spawn site.
pub fn export_lane_marker(cmd: &mut std::process::Command) {
    if lane_held() {
        cmd.env(LANE_HELD_ENV, "1");
    }
}

/// Outcome of a single non-blocking acquire attempt.
enum TryAcquire {
    Acquired(CloneLock),
    WouldBlock,
    Err(anyhow::Error),
}

impl CloneLock {
    /// Acquire the clone lock for `repo`, blocking up to the configured wait.
    /// Returns immediately with an error if `FOREMAN_CLONE_LOCK_WAIT_SECS=0`.
    pub fn acquire(repo: &Path) -> Result<Self> {
        let lock_path = lock_path_for_repo(repo)?;
        Self::acquire_path(&lock_path)
    }

    /// Acquire the manifest project's own `clone.lock`.
    pub fn acquire_in_project(root: &Path) -> Result<Self> {
        Self::acquire_path(&root.join("clone.lock"))
    }

    fn acquire_path(lock_path: &Path) -> Result<Self> {
        let wait_secs = wait_timeout();

        // Fail-fast path: just try once with LOCK_NB.
        if wait_secs == 0 {
            return match Self::try_acquire(lock_path) {
                TryAcquire::Acquired(lock) => Ok(lock),
                TryAcquire::WouldBlock => Err(fail_fast_error(lock_path)),
                TryAcquire::Err(e) => Err(e),
            };
        }

        // Blocking wait with timeout: poll until we either get the lock or
        // hit the deadline (flock has no built-in timeout, so the wait is a
        // poll loop at POLL_INTERVAL). A dead holder needs no reclaiming —
        // the kernel released its flock when it exited, so the next attempt
        // simply wins.
        let deadline = std::time::Instant::now() + Duration::from_secs(wait_secs);

        loop {
            match Self::try_acquire(lock_path) {
                TryAcquire::Acquired(lock) => return Ok(lock),
                TryAcquire::Err(e) => return Err(e),
                TryAcquire::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        let holder = read_holder(lock_path);
                        anyhow::bail!(
                            "clone lock acquisition timed out after {wait_secs}s waiting on \
                             {} — blocked on {}",
                            lock_path.display(),
                            holder.describe(lock_path),
                        );
                    }
                    std::thread::sleep(POLL_INTERVAL);
                    continue;
                }
            }
        }
    }

    /// Try once to acquire the lock (non-blocking) and, on success, stamp
    /// our identity into it.
    fn try_acquire(lock_path: &Path) -> TryAcquire {
        let mut file = match std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path)
            .with_context(|| format!("opening {}", lock_path.display()))
        {
            Ok(f) => f,
            Err(e) => return TryAcquire::Err(e),
        };

        // LOCK_NB = non-blocking: fails immediately if another process holds it.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return TryAcquire::WouldBlock;
            }
            return TryAcquire::Err(
                anyhow::Error::new(err).context(format!("locking {}", lock_path.display())),
            );
        }

        // Best-effort: an operator reading the lock file mid-hold should see
        // who owns it. Failure to stamp identity doesn't invalidate the lock
        // itself — the flock is what actually serializes.
        let _ = stamp_holder(&mut file);

        IN_PROCESS_HOLDS.fetch_add(1, Ordering::SeqCst);
        TryAcquire::Acquired(CloneLock { _file: file })
    }
}

impl Drop for CloneLock {
    fn drop(&mut self) {
        // Closing `_file` is what releases the flock; this only retires the
        // in-process half of the handshake.
        IN_PROCESS_HOLDS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Overwrite the lock file's contents with our identity.
fn stamp_holder(file: &mut std::fs::File) -> std::io::Result<()> {
    let pid = std::process::id() as i64;
    let pid_start = crate::procutil::starttime(pid);
    let body = format!(
        "pid={pid}\npid_start={}\nacquired_at={}\n",
        pid_start.map(|s| s.to_string()).unwrap_or_default(),
        Utc::now().to_rfc3339(),
    );
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(body.as_bytes())?;
    file.flush()
}

/// Read the current holder's identity from the lock file's contents,
/// without taking the flock — safe to call while someone else holds it.
/// Returns an empty [`LockHolder`] (no known owner) on any read/parse
/// failure, since this is diagnostics-only and must never itself block or
/// fail lock acquisition.
fn read_holder(lock_path: &Path) -> LockHolder {
    let mut holder = LockHolder {
        pid: None,
        pid_start: None,
        acquired_at: None,
    };
    let Ok(mut file) = std::fs::File::open(lock_path) else {
        return holder;
    };
    let mut contents = String::new();
    if file.read_to_string(&mut contents).is_err() {
        return holder;
    }
    for line in contents.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "pid" => holder.pid = value.parse().ok(),
            "pid_start" => holder.pid_start = value.parse().ok(),
            "acquired_at" => holder.acquired_at = Some(value.to_string()),
            _ => {}
        }
    }
    holder
}

fn fail_fast_error(lock_path: &Path) -> anyhow::Error {
    let holder = read_holder(lock_path);
    anyhow::anyhow!(
        "another process holds the clone lock {} ({WAIT_ENV}=0, not waiting) — blocked on {}",
        lock_path.display(),
        holder.describe(lock_path),
    )
}

/// Compute the lock file path for a repo: `<repo>/../clone.lock`.
///
/// For a typical layout like `~/.cos/src/` (the repo), this yields
/// `~/.cos/clone.lock` — sibling of the repo, same file the systemd units use.
fn lock_path_for_repo(repo: &Path) -> Result<PathBuf> {
    let parent = repo.parent().context("repo has no parent directory")?;
    Ok(parent.join("clone.lock"))
}

/// Get the wait timeout from environment (0 = fail fast, default = 900s).
fn wait_timeout() -> u64 {
    wait_timeout_from(std::env::var_os(WAIT_ENV))
}

/// Pure seam over the env read so tests exercise every value arm without
/// mutating the process environment: `0` = fail fast; unset, empty,
/// non-unicode, or unparsable = default (the last with a warning).
fn wait_timeout_from(env: Option<std::ffi::OsString>) -> u64 {
    match env {
        Some(v) => {
            let Ok(s) = v.into_string() else {
                return DEFAULT_WAIT_SECS;
            };
            match s.trim().parse::<u64>() {
                Ok(secs) => secs,
                Err(_) => {
                    eprintln!(
                        "foreman: {}={s:?} is not a valid integer — using default {}s",
                        WAIT_ENV, DEFAULT_WAIT_SECS
                    );
                    DEFAULT_WAIT_SECS
                }
            }
        }
        None => DEFAULT_WAIT_SECS,
    }
}

/// Read who currently holds (or last held) `repo`'s clone lock, without
/// taking it — safe to call while another process holds the lock. For
/// operator diagnostics: e.g. reporting who a stuck `foreman refine` is
/// blocked on. `pid: None` means no lock file yet, or no holder recorded.
pub fn inspect(repo: &Path) -> Result<LockHolder> {
    let lock_path = lock_path_for_repo(repo)?;
    Ok(read_holder(&lock_path))
}

/// Acquire the clone lock if `dir` is inside a git working tree whose repo has a
/// sibling `clone.lock`. Returns `None` when there is nothing to serialize
/// against: `dir` is not in a repo, or that repo has no sibling lock file.
///
/// The pre-existing-file probe is deliberate, and it is the difference between
/// this and [`CloneLock::acquire`]. Refine OWNS the lane, so it creates the lock
/// file if it has to. Verify only JOINS a lane someone else defined — the
/// clone.lock the systemd wrappers and refine already use. Creating one on
/// demand would mean any `foreman verify --dir` run inside an unrelated
/// checkout drops a `clone.lock` beside it and starts serializing against a
/// lane that has no other members, so the file would spread to repos that
/// never wanted it. Absent file, absent lane, no lock.
pub fn acquire_if_in_repo(dir: &Path) -> Result<Option<CloneLock>> {
    // Already in the lane (an ancestor's wrapper, or the `refine` that
    // called us): joining means doing nothing, not taking the lock again.
    if lane_held() {
        return Ok(None);
    }
    let Some(repo) = repo_containing(dir)? else {
        return Ok(None);
    };
    if !lock_path_for_repo(&repo)?.exists() {
        return Ok(None);
    }
    Ok(Some(CloneLock::acquire(&repo)?))
}

/// Find the git repo containing `dir` (the repo's own `.git` directory), if any.
/// Returns the repo path itself, not the .git directory — the lock lives next to
/// the repo, not inside it.
fn repo_containing(dir: &Path) -> Result<Option<PathBuf>> {
    let mut current = Some(dir);

    while let Some(path) = current {
        let git_dir = path.join(".git");
        if git_dir.exists() {
            return Ok(Some(path.to_path_buf()));
        }

        current = path.parent();
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_path_for_regular_repo() {
        // For a repo at /home/user/project, lock is at /home/user/clone.lock
        let repo = PathBuf::from("/home/user/project");
        let lock = lock_path_for_repo(&repo).unwrap();
        assert_eq!(lock, PathBuf::from("/home/user/clone.lock"));
    }

    #[test]
    fn lock_path_for_nested_repo() {
        // For a repo at /home/user/deeply/nested/project, lock is at
        // /home/user/deeply/nested/clone.lock
        let repo = PathBuf::from("/home/user/deeply/nested/project");
        let lock = lock_path_for_repo(&repo).unwrap();
        assert_eq!(lock, PathBuf::from("/home/user/deeply/nested/clone.lock"));
    }

    #[test]
    fn wait_timeout_default() {
        // No env var set → default. Pure seam: no process-env mutation.
        assert_eq!(wait_timeout_from(None), DEFAULT_WAIT_SECS);
    }

    #[test]
    fn wait_timeout_custom() {
        // Set env var → custom value, trim-tolerant like the production read.
        assert_eq!(wait_timeout_from(Some("60".into())), 60);
        assert_eq!(wait_timeout_from(Some(" 60 ".into())), 60);
    }

    #[test]
    fn wait_timeout_fail_fast() {
        // 0 = fail fast.
        assert_eq!(wait_timeout_from(Some("0".into())), 0);
    }

    #[test]
    fn wait_timeout_invalid() {
        // Invalid and empty values → default with warning.
        assert_eq!(
            wait_timeout_from(Some("not-a-number".into())),
            DEFAULT_WAIT_SECS
        );
        assert_eq!(wait_timeout_from(Some("".into())), DEFAULT_WAIT_SECS);
    }

    #[test]
    fn wait_timeout_non_unicode_is_default() {
        use std::os::unix::ffi::OsStringExt;
        assert_eq!(
            wait_timeout_from(Some(std::ffi::OsString::from_vec(vec![0xff]))),
            DEFAULT_WAIT_SECS
        );
    }

    /// The public reader really consults the live variable — read-only, no
    /// mutation, both arms computed from the live value.
    #[test]
    fn wait_timeout_wires_the_live_env() {
        let live = std::env::var_os(WAIT_ENV);
        assert_eq!(wait_timeout(), wait_timeout_from(live));
    }

    #[test]
    fn read_holder_on_missing_file_is_empty() {
        let holder = read_holder(Path::new("/nonexistent/clone.lock"));
        assert_eq!(
            holder,
            LockHolder {
                pid: None,
                pid_start: None,
                acquired_at: None,
            }
        );
    }

    #[test]
    fn stamp_and_read_holder_round_trips() {
        let temp = tempfile::TempDir::new().unwrap();
        let lock_path = temp.path().join("clone.lock");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        stamp_holder(&mut file).unwrap();

        let holder = read_holder(&lock_path);
        assert_eq!(holder.pid, Some(std::process::id() as i64));
        assert!(holder.acquired_at.is_some());
        assert!(holder.is_alive(), "the current process is always alive");
    }
}
