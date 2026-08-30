//! Reclaim Cargo scratch left by terminal Foreman tasks.
//!
//! This is deliberately narrower than general worktree garbage collection:
//! it never removes a worktree or source. The only removable roots are a
//! registered task worktree's `src/target/`, plus the exact sibling
//! `<fleet>/task-N-target/` legacy shape.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use crate::gc;
use crate::ledger::{Ledger, Task};

pub const DEFAULT_TERMINAL_AGE_HOURS: u64 = 24;
pub const DEFAULT_PRESSURE_PERCENT: u8 = 80;
/// Per-cache bound. This deliberately retains the 55 GiB and 97 GiB hot
/// caches observed during the 2026-08-28 incident while stopping unbounded
/// growth.
pub const DEFAULT_SHARED_MAX_GB: u64 = 160;

#[derive(Debug, Clone)]
pub struct ScratchOptions {
    pub fleet_dir: PathBuf,
    pub repo: PathBuf,
    pub terminal_age_hours: u64,
    pub pressure_pool: Option<String>,
    pub pressure_percent: u8,
    pub shared_max_gb: u64,
    /// Wall-clock selection input. It is printed in every report and can be
    /// supplied by `--as-of` when replaying a sweep.
    pub selection_time: DateTime<Utc>,
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
pub struct ScratchReport {
    pub selection_time: DateTime<Utc>,
    pub before_bytes: u64,
    pub after_bytes: u64,
    pub would_reclaim_bytes: u64,
    pub removed_dirs: usize,
    pub candidate_dirs: usize,
    pub skipped_tasks: usize,
    pub skipped_paths: Vec<String>,
    pub pressure_pool: Option<String>,
    pub pressure_threshold: u8,
    pub pressure_before: Option<u8>,
    pub pressure_after: Option<u8>,
    pub pressure_escalated: bool,
    pub pressure_error: Option<String>,
    pub shared: Vec<SharedCacheReport>,
    pub review_worktrees: gc::ReviewWorktreeGcReport,
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
pub struct SharedCacheReport {
    pub path: PathBuf,
    pub before_bytes: u64,
    pub after_bytes: u64,
    pub would_reclaim_bytes: u64,
    pub candidate_entries: usize,
    pub detail: String,
    pub failed: bool,
}

impl ScratchReport {
    pub fn reclaimed_bytes(&self) -> u64 {
        self.before_bytes.saturating_sub(self.after_bytes)
    }

    pub fn failed(&self) -> bool {
        self.pressure_error.is_some()
            || !self.skipped_paths.is_empty()
            || self.shared.iter().any(|cache| cache.failed)
    }

    pub fn summary_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        let action = if self.dry_run {
            format!(
                "0 B reclaimed; {} would be reclaimed; dry-run",
                human_bytes(self.would_reclaim_bytes)
            )
        } else {
            format!("{} reclaimed", human_bytes(self.reclaimed_bytes()))
        };
        lines.push(format!(
            "scratch: {} candidate dir(s), {} -> {} ({action}); {} dir(s) removed, {} task(s) skipped",
            self.candidate_dirs,
            human_bytes(self.before_bytes),
            human_bytes(self.after_bytes),
            self.removed_dirs,
            self.skipped_tasks,
        ));
        lines.push(format!(
            "selection time: {} (replay with --as-of {})",
            self.selection_time.to_rfc3339(),
            self.selection_time.to_rfc3339(),
        ));

        match (
            &self.pressure_pool,
            self.pressure_before,
            self.pressure_after,
        ) {
            (Some(pool), Some(before), Some(after)) => lines.push(format!(
                "pool {pool}: {before}% -> {after}% (threshold {}%; pressure escalation {})",
                self.pressure_threshold,
                if self.pressure_escalated {
                    "active"
                } else {
                    "inactive"
                },
            )),
            (Some(pool), Some(before), None) => lines.push(format!(
                "pool {pool}: {before}% before sweep; after-sweep capacity unavailable"
            )),
            (Some(pool), None, _) => {
                lines.push(format!("pool {pool}: capacity unavailable; no escalation"))
            }
            (None, _, _) => lines.push("pool pressure: not configured; age policy only".into()),
        }
        if let Some(error) = &self.pressure_error {
            lines.push(format!("pool pressure ERROR: {error}"));
        }
        for path in &self.skipped_paths {
            lines.push(format!("scratch SKIPPED: {path}"));
        }
        for cache in &self.shared {
            lines.push(format!(
                "shared cache {}: {} -> {} ({})",
                cache.path.display(),
                human_bytes(cache.before_bytes),
                human_bytes(cache.after_bytes),
                cache.detail,
            ));
        }
        lines.push(format!(
            "legacy review worktrees: {} candidate(s), {} {}",
            self.review_worktrees.candidates,
            if self.dry_run {
                self.review_worktrees.candidates
            } else {
                self.review_worktrees.removed
            },
            if self.dry_run {
                "would be removed"
            } else {
                "removed"
            },
        ));
        let shared_before = self.shared.iter().fold(0_u64, |total, cache| {
            total.saturating_add(cache.before_bytes)
        });
        let shared_after = self.shared.iter().fold(0_u64, |total, cache| {
            total.saturating_add(cache.after_bytes)
        });
        let shared_would_reclaim = self.shared.iter().fold(0_u64, |total, cache| {
            total.saturating_add(cache.would_reclaim_bytes)
        });
        let shared_candidates = self
            .shared
            .iter()
            .map(|cache| cache.candidate_entries)
            .sum::<usize>();
        let total_before = self.before_bytes.saturating_add(shared_before);
        let total_after = self.after_bytes.saturating_add(shared_after);
        if self.dry_run {
            lines.push(format!(
                "sweep total: {} task dir(s) + {} shared entry(s), {} -> {} ({} would be reclaimed; dry-run)",
                self.candidate_dirs,
                shared_candidates,
                human_bytes(total_before),
                human_bytes(total_after),
                human_bytes(
                    self.would_reclaim_bytes
                        .saturating_add(shared_would_reclaim)
                ),
            ));
        } else {
            lines.push(format!(
                "sweep total: {} task dir(s) + {} shared entry(s), {} -> {} ({} reclaimed)",
                self.candidate_dirs,
                shared_candidates,
                human_bytes(total_before),
                human_bytes(total_after),
                human_bytes(total_before.saturating_sub(total_after)),
            ));
        }
        lines
    }
}

#[derive(Debug, Default)]
pub struct TaskScratchReport {
    pub before_bytes: u64,
    pub after_bytes: u64,
    pub would_reclaim_bytes: u64,
    pub candidate_dirs: usize,
    pub removed_dirs: usize,
    pub skipped_paths: Vec<String>,
}

impl TaskScratchReport {
    pub fn summary(&self, task_id: i64, dry_run: bool) -> String {
        if dry_run {
            format!(
                "task {task_id} scratch: {} candidate dir(s), {} -> {} ({} would be reclaimed; dry-run)",
                self.candidate_dirs,
                human_bytes(self.before_bytes),
                human_bytes(self.after_bytes),
                human_bytes(self.would_reclaim_bytes),
            )
        } else {
            format!(
                "task {task_id} scratch: {} candidate dir(s), {} -> {} ({} reclaimed; {} dir(s) removed)",
                self.candidate_dirs,
                human_bytes(self.before_bytes),
                human_bytes(self.after_bytes),
                human_bytes(self.before_bytes.saturating_sub(self.after_bytes)),
                self.removed_dirs,
            )
        }
    }
}

/// Sweep terminal task scratch and bound the two shared caches. ZFS capacity
/// is read before selection and again after the sweep; a failed probe is loud
/// but does not suppress the ordinary age pass. The selection time is an
/// explicit option so a recorded invocation is replayable.
pub fn sweep(ledger: &Ledger, options: &ScratchOptions) -> Result<ScratchReport> {
    let mut probe = |pool: &str| zpool_capacity(pool);
    sweep_with_probe(ledger, options, &mut probe)
}

fn sweep_with_probe(
    ledger: &Ledger,
    options: &ScratchOptions,
    probe: &mut impl FnMut(&str) -> Result<u8>,
) -> Result<ScratchReport> {
    anyhow::ensure!(
        (1..=100).contains(&options.pressure_percent),
        "pressure threshold must be in 1..=100, got {}",
        options.pressure_percent
    );
    let fleet_dir = canonical_existing_dir(&options.fleet_dir, "fleet directory")?;
    let repo = canonical_existing_dir(&options.repo, "shared repository")?;
    let review_worktrees = gc::reclaim_terminal_review_worktrees(ledger, &repo, options.dry_run)?;

    let mut pressure_error = None;
    let pressure_before = match options.pressure_pool.as_deref() {
        Some(pool) => match probe(pool) {
            Ok(capacity) => Some(capacity),
            Err(error) => {
                pressure_error = Some(format!("reading pool {pool:?} before sweep: {error:#}"));
                None
            }
        },
        None => None,
    };
    let pressure_escalated =
        pressure_before.is_some_and(|capacity| capacity >= options.pressure_percent);
    let max_age = Duration::from_secs(options.terminal_age_hours.saturating_mul(60 * 60));
    let mut tasks = ledger.tasks(None, true)?;
    tasks.sort_by(|left, right| {
        if pressure_escalated {
            right.updated_at.cmp(&left.updated_at)
        } else {
            left.updated_at.cmp(&right.updated_at)
        }
    });

    let mut report = ScratchReport {
        selection_time: options.selection_time,
        before_bytes: 0,
        after_bytes: 0,
        would_reclaim_bytes: 0,
        removed_dirs: 0,
        candidate_dirs: 0,
        skipped_tasks: 0,
        skipped_paths: Vec::new(),
        pressure_pool: options.pressure_pool.clone(),
        pressure_threshold: options.pressure_percent,
        pressure_before,
        pressure_after: None,
        pressure_escalated,
        pressure_error,
        shared: Vec::new(),
        review_worktrees,
        dry_run: options.dry_run,
    };

    for snapshot in tasks {
        if !terminal_status(&snapshot.status) || snapshot.operator_driven {
            report.skipped_tasks += 1;
            continue;
        }
        let updated_at = match DateTime::parse_from_rfc3339(&snapshot.updated_at) {
            Ok(value) => value.with_timezone(&Utc),
            Err(error) => {
                report.skipped_tasks += 1;
                report.skipped_paths.push(format!(
                    "task {} has invalid updated_at {:?}: {error}",
                    snapshot.id, snapshot.updated_at
                ));
                continue;
            }
        };
        let age = options.selection_time.signed_duration_since(updated_at);
        let old_enough = age.num_seconds() >= 0
            && u64::try_from(age.num_seconds()).is_ok_and(|seconds| seconds >= max_age.as_secs());
        if !pressure_escalated && !old_enough {
            report.skipped_tasks += 1;
            continue;
        }

        let task_report = if options.dry_run {
            // Dry-run never mutates the ledger or the tree, so a plain
            // re-read is enough: nothing is deleted, so there is no
            // requeue-vs-deletion race to close.
            let Some(current) = ledger.task(snapshot.id)? else {
                report.skipped_tasks += 1;
                continue;
            };
            if !terminal_status(&current.status)
                || current.operator_driven
                || (!pressure_escalated && current.updated_at != snapshot.updated_at)
            {
                report.skipped_tasks += 1;
                continue;
            }
            plan_task_scratch(&current, &repo, &fleet_dir)
        } else {
            // The atomic lease IS the freshness check for a real run: it
            // only succeeds if the task is still landed/retired, unclaimed,
            // and not operator-reserved at the instant of the write, closing
            // the window a plain re-read-then-delete leaves open between
            // "looked eligible" and `remove_dir_all` actually running.
            match ledger.begin_scratch_cleanup(snapshot.id) {
                Ok(Some(leased)) => {
                    let stamp = leased.claimed_by.clone().unwrap_or_default();
                    let task_report = reclaim_task_scratch_checked(
                        &leased,
                        &repo,
                        &fleet_dir,
                        false,
                        &mut || ledger.scratch_cleanup_still_held(snapshot.id, &stamp),
                    );
                    if let Err(error) = ledger.end_scratch_cleanup(snapshot.id, &stamp) {
                        report.skipped_paths.push(format!(
                            "task {} scratch cleanup finished but its lease could not be \
                             released: {error:#}",
                            snapshot.id
                        ));
                    }
                    task_report
                }
                Ok(None) => {
                    report.skipped_tasks += 1;
                    continue;
                }
                Err(error) => {
                    report.skipped_paths.push(format!(
                        "task {}: could not reserve for scratch cleanup: {error:#}",
                        snapshot.id
                    ));
                    report.skipped_tasks += 1;
                    continue;
                }
            }
        };
        report.before_bytes = report.before_bytes.saturating_add(task_report.before_bytes);
        report.after_bytes = report.after_bytes.saturating_add(task_report.after_bytes);
        report.would_reclaim_bytes = report
            .would_reclaim_bytes
            .saturating_add(task_report.would_reclaim_bytes);
        report.removed_dirs += task_report.removed_dirs;
        report.candidate_dirs += task_report.candidate_dirs;
        report.skipped_paths.extend(task_report.skipped_paths);
    }

    for name in ["target", "target-refine"] {
        report.shared.push(reclaim_shared_cache(
            &fleet_dir,
            name,
            options.shared_max_gb,
            options.dry_run,
        ));
    }

    if let Some(pool) = options.pressure_pool.as_deref()
        && report.pressure_before.is_some()
    {
        match probe(pool) {
            Ok(capacity) => report.pressure_after = Some(capacity),
            Err(error) => {
                let detail = format!("reading pool {pool:?} after sweep: {error:#}");
                report.pressure_error = Some(match report.pressure_error.take() {
                    Some(existing) => format!("{existing}; {detail}"),
                    None => detail,
                });
            }
        }
    }

    Ok(report)
}

/// Report which of one terminal task's exact Cargo scratch shapes WOULD be
/// removed, and how large they are. This is the only task-scratch entry
/// point exported from this module that takes a caller-supplied [`Task`]
/// snapshot, and it deletes nothing — deliberately.
///
/// The effect path is [`reclaim_task_scratch_leased`], which reserves the
/// task in the ledger first. An exported raw effect helper taking a snapshot
/// would be a way around that reservation, and the reservation is the whole
/// safety story: it is what stops a requeue handing the worktree back to a
/// live run while `remove_dir_all` is in flight. A planner cannot bypass an
/// arbiter that only guards deletion, so this one plans, and the private
/// [`reclaim_task_scratch_checked`] is the only code in the crate that
/// deletes.
///
/// Errors are folded into `skipped_paths` so a landed task is never bounced
/// after its commit is already integrated, while every refusal stays visible.
pub fn plan_task_scratch(task: &Task, repo: &Path, fleet_dir: &Path) -> TaskScratchReport {
    reclaim_task_scratch_checked(task, repo, fleet_dir, true, &mut || true)
}

/// Remove one terminal task's exact Cargo scratch shapes — the crate's ONLY
/// deleting path for task scratch, private so every real caller has to come
/// through [`reclaim_task_scratch_leased`] and its ledger reservation.
///
/// `still_leased` is called before each candidate directory is actually
/// removed (never in `dry_run`, which deletes nothing and holds no lease). A
/// task has at most two candidates (worktree `src/target/` and the sibling
/// `task-N-target/`), so this is the sweep's chance to notice mid-task that
/// its [`crate::ledger::SCRATCH_GC_CLAIMANT`] lease was taken away before
/// starting the next `remove_dir_all`.
///
/// This is defence in depth, NOT the interlock. It cannot abort a
/// `remove_dir_all` already in flight, and a check followed by a deletion is
/// two operations with a window between them. The interlock that actually
/// binds is in [`crate::ledger::Ledger::requeue_task_with`]: no requeue, not
/// even `--force`, can clear this lease while the reclaiming process is
/// alive, so the lease cannot be lost out from under a live sweep in the
/// first place. What remains here catches the case where it was lost
/// legitimately — this process died and something released the stale lease —
/// and stops a second candidate being deleted after that.
fn reclaim_task_scratch_checked(
    task: &Task,
    repo: &Path,
    fleet_dir: &Path,
    dry_run: bool,
    still_leased: &mut dyn FnMut() -> bool,
) -> TaskScratchReport {
    let mut report = TaskScratchReport::default();
    if !terminal_status(&task.status) {
        report
            .skipped_paths
            .push(format!("task {} is {}, not terminal", task.id, task.status));
        return report;
    }
    if task.operator_driven {
        report
            .skipped_paths
            .push(format!("task {} is operator-reserved", task.id));
        return report;
    }

    let fleet_dir = match canonical_existing_dir(fleet_dir, "fleet directory") {
        Ok(path) => path,
        Err(error) => {
            report
                .skipped_paths
                .push(format!("task {}: {error:#}", task.id));
            return report;
        }
    };
    let repo = match canonical_existing_dir(repo, "shared repository") {
        Ok(path) => path,
        Err(error) => {
            report
                .skipped_paths
                .push(format!("task {}: {error:#}", task.id));
            return report;
        }
    };

    if let Some(recorded) = task.worktree.as_deref() {
        let recorded = Path::new(recorded);
        let worktree_exists = match recorded.try_exists() {
            Ok(exists) => exists,
            Err(error) => {
                report.skipped_paths.push(format!(
                    "task {} recorded worktree {} could not be checked: {error}",
                    task.id,
                    recorded.display()
                ));
                false
            }
        };
        if worktree_exists {
            match validate_worktree(recorded, &repo, &fleet_dir) {
                Ok(worktree) => {
                    let relative = Path::new("src/target");
                    match validate_worktree_target(&worktree, relative) {
                        Ok(Some(candidate)) => {
                            if dry_run || still_leased() {
                                reclaim_candidate(&fleet_dir, candidate, dry_run, &mut report)
                            } else {
                                report.skipped_paths.push(format!(
                                    "task {}: scratch-cleanup lease lost mid-sweep, \
                                     refusing to remove {}",
                                    task.id,
                                    worktree.join(relative).display()
                                ));
                                return report;
                            }
                        }
                        Ok(None) => {}
                        Err(error) => report.skipped_paths.push(format!(
                            "task {} {}: {error:#}",
                            task.id,
                            worktree.join(relative).display()
                        )),
                    }
                }
                Err(error) => report.skipped_paths.push(format!(
                    "task {} recorded worktree {:?}: {error:#}",
                    task.id, recorded
                )),
            }
        }
    }

    let sibling = fleet_dir.join(format!("task-{}-target", task.id));
    match validate_sibling_target(&fleet_dir, &sibling) {
        Ok(Some(candidate)) => {
            if dry_run || still_leased() {
                reclaim_candidate(&fleet_dir, candidate, dry_run, &mut report)
            } else {
                report.skipped_paths.push(format!(
                    "task {}: scratch-cleanup lease lost mid-sweep, refusing to remove {}",
                    task.id,
                    sibling.display()
                ));
                return report;
            }
        }
        Ok(None) => {}
        Err(error) => report.skipped_paths.push(format!(
            "task {} sibling {}: {error:#}",
            task.id,
            sibling.display()
        )),
    }

    report
}

/// Reserve `task_id` for scratch cleanup and always release the reservation
/// afterward, whatever the filesystem work does. This is the only safe way
/// to reclaim a task's scratch outside a dry-run: a plain re-read-then-write
/// leaves a window between "this task looked landed/retired, unclaimed, not
/// operator-reserved" and `remove_dir_all` actually running, where an
/// operator's requeue — and the dispatch it re-enables — can hand the same
/// worktree back to a live run while cleanup is still deleting it.
/// [`Ledger::begin_scratch_cleanup`] closes that window atomically — and,
/// because the lease it takes is stamped with this process's pid,
/// [`Ledger::requeue_task_with`] refuses to clear it (with or without
/// `--force`) for as long as this process is alive.
///
/// The reservation edge into that same race is closed at the other end:
/// [`Ledger::set_operator_driven_with`] refuses to mark a task
/// operator-reserved while its scratch is being reclaimed by a live
/// process. Between them the two guards give "never touch an
/// operator-reserved worktree" a shape that holds across the whole
/// deletion, not just at the instant the lease was taken.
///
/// This wrapper is what every non-dry-run caller (the sweep, the
/// post-landing reclaim) goes through; there is no exported way to delete
/// task scratch that skips it.
pub fn reclaim_task_scratch_leased(
    ledger: &Ledger,
    task_id: i64,
    repo: &Path,
    fleet_dir: &Path,
    dry_run: bool,
) -> TaskScratchReport {
    if dry_run {
        return match ledger.task(task_id) {
            Ok(Some(task)) => plan_task_scratch(&task, repo, fleet_dir),
            Ok(None) => {
                let mut report = TaskScratchReport::default();
                report
                    .skipped_paths
                    .push(format!("task {task_id} vanished before scratch dry-run"));
                report
            }
            Err(error) => {
                let mut report = TaskScratchReport::default();
                report.skipped_paths.push(format!(
                    "task {task_id}: could not reload for scratch dry-run: {error:#}"
                ));
                report
            }
        };
    }
    let leased = match ledger.begin_scratch_cleanup(task_id) {
        Ok(Some(task)) => task,
        Ok(None) => {
            let mut report = TaskScratchReport::default();
            report.skipped_paths.push(format!(
                "task {task_id} is no longer eligible for scratch cleanup \
                 (status changed, claimed, or became operator-reserved since it was selected)"
            ));
            return report;
        }
        Err(error) => {
            let mut report = TaskScratchReport::default();
            report.skipped_paths.push(format!(
                "task {task_id}: could not reserve for scratch cleanup: {error:#}"
            ));
            return report;
        }
    };
    // `claimed_by` on the freshly read row IS the stamp
    // `begin_scratch_cleanup` just wrote; revalidation and release are both
    // guarded on that exact string, never on the bare sentinel, so a lease
    // reassigned in between is neither mistaken for ours nor clobbered.
    let stamp = leased.claimed_by.clone().unwrap_or_default();
    let report = reclaim_task_scratch_checked(&leased, repo, fleet_dir, false, &mut || {
        ledger.scratch_cleanup_still_held(task_id, &stamp)
    });
    if let Err(error) = ledger.end_scratch_cleanup(task_id, &stamp) {
        eprintln!(
            "foreman: task {task_id} scratch cleanup finished but its lease could not be \
             released: {error:#}"
        );
    }
    report
}

#[derive(Debug)]
struct Candidate {
    path: PathBuf,
}

/// Remove one validated candidate directory. A durable intent record is
/// written and fsynced BEFORE `remove_dir_all` runs, and a result record
/// after: a crash between the two must leave visible, durable evidence that
/// cleanup was attempted, never something indistinguishable from cleanup
/// never having been dispatched at all. If the intent record itself cannot
/// be written, the candidate is left untouched — no durable record, no
/// deletion.
fn reclaim_candidate(
    fleet_dir: &Path,
    candidate: Candidate,
    dry_run: bool,
    report: &mut TaskScratchReport,
) {
    let before = match gc::allocated_size(&candidate.path) {
        Ok(size) => size,
        Err(error) => {
            report
                .skipped_paths
                .push(format!("{}: {error:#}", candidate.path.display()));
            return;
        }
    };
    report.before_bytes = report.before_bytes.saturating_add(before);
    report.would_reclaim_bytes = report.would_reclaim_bytes.saturating_add(before);
    report.candidate_dirs += 1;
    if dry_run {
        report.after_bytes = report.after_bytes.saturating_add(before);
        return;
    }
    if let Err(error) = record_cleanup_intent(
        fleet_dir,
        &format!("removing {} ({before} bytes)", candidate.path.display()),
    ) {
        report.after_bytes = report.after_bytes.saturating_add(before);
        report.skipped_paths.push(format!(
            "{}: could not record cleanup intent, refusing to delete it: {error:#}",
            candidate.path.display()
        ));
        return;
    }
    match fs::remove_dir_all(&candidate.path) {
        Ok(()) => {
            report.removed_dirs += 1;
            match gc::allocated_size(&candidate.path) {
                Ok(after) => report.after_bytes = report.after_bytes.saturating_add(after),
                Err(error) => report.skipped_paths.push(format!(
                    "{} was removed but after-size failed: {error:#}",
                    candidate.path.display()
                )),
            }
            let _ = record_cleanup_result(
                fleet_dir,
                &format!("removed {} ({before} bytes)", candidate.path.display()),
            );
        }
        Err(error) => {
            report.after_bytes = report
                .after_bytes
                .saturating_add(gc::allocated_size(&candidate.path).unwrap_or(before));
            report.skipped_paths.push(format!(
                "{} could not be removed: {error}",
                candidate.path.display()
            ));
            let _ = record_cleanup_result(
                fleet_dir,
                &format!("FAILED removing {}: {error}", candidate.path.display()),
            );
        }
    }
}

/// Durable, crash-safe log of scratch-cleanup intent and outcome — append,
/// then `fsync`, before the caller is allowed to proceed. This is deliberately
/// independent of stdout/stderr capture: a process that dies mid-deletion
/// must leave evidence on disk, not just a log line nobody was tailing.
fn append_scratch_journal(fleet_dir: &Path, kind: &str, detail: &str) -> Result<()> {
    use std::io::Write;
    let path = fleet_dir.join(".foreman-scratch-gc.journal");
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening scratch-gc journal {}", path.display()))?;
    writeln!(file, "{} {kind} {detail}", Utc::now().to_rfc3339())
        .with_context(|| format!("writing scratch-gc journal {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing scratch-gc journal {}", path.display()))?;
    Ok(())
}

fn record_cleanup_intent(fleet_dir: &Path, detail: &str) -> Result<()> {
    append_scratch_journal(fleet_dir, "intent", detail)
}

fn record_cleanup_result(fleet_dir: &Path, detail: &str) -> Result<()> {
    append_scratch_journal(fleet_dir, "result", detail)
}

fn validate_worktree(recorded: &Path, repo: &Path, fleet_dir: &Path) -> Result<PathBuf> {
    anyhow::ensure!(recorded.is_absolute(), "path is not absolute");
    let worktree = canonical_existing_dir(recorded, "task worktree")?;
    anyhow::ensure!(
        worktree.starts_with(fleet_dir),
        "resolves outside fleet directory {}",
        fleet_dir.display()
    );
    anyhow::ensure!(worktree != repo, "refusing the shared repository itself");

    let top = git_path(
        &worktree,
        &["rev-parse", "--path-format=absolute", "--show-toplevel"],
    )?;
    anyhow::ensure!(top == worktree, "Git top level is {}", top.display());
    let worktree_common = git_path(
        &worktree,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let repo_common = git_path(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    anyhow::ensure!(
        worktree_common == repo_common,
        "Git common dir {} differs from shared repo {}",
        worktree_common.display(),
        repo_common.display()
    );
    Ok(worktree)
}

fn validate_worktree_target(worktree: &Path, relative: &Path) -> Result<Option<Candidate>> {
    debug_assert_eq!(relative, Path::new("src/target"));
    let candidate = worktree.join(relative);
    if !real_directory_exists(&candidate)? {
        return Ok(None);
    }
    let resolved = candidate
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", candidate.display()))?;
    anyhow::ensure!(
        resolved.starts_with(worktree),
        "resolves outside worktree {}",
        worktree.display()
    );
    let expected_parent = relative
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or_else(|| worktree.to_path_buf(), |parent| worktree.join(parent));
    let expected_parent = expected_parent.canonicalize().with_context(|| {
        format!(
            "canonicalizing expected parent {}",
            expected_parent.display()
        )
    })?;
    anyhow::ensure!(
        resolved.parent() == Some(expected_parent.as_path())
            && resolved.file_name() == Some(OsStr::new("target")),
        "resolved path is not the exact {:?} target shape",
        relative
    );
    require_untracked_and_ignored(worktree, relative)?;
    Ok(Some(Candidate { path: resolved }))
}

fn validate_sibling_target(fleet_dir: &Path, candidate: &Path) -> Result<Option<Candidate>> {
    if !real_directory_exists(candidate)? {
        return Ok(None);
    }
    let resolved = candidate
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", candidate.display()))?;
    anyhow::ensure!(
        resolved.parent() == Some(fleet_dir),
        "resolves outside fleet directory {}",
        fleet_dir.display()
    );
    require_not_tracked_if_in_repo(&resolved)?;
    Ok(Some(Candidate { path: resolved }))
}

fn real_directory_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata.file_type().is_dir(),
                "{} is not a real directory (symlinks refused)",
                path.display()
            );
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("reading metadata for {}", path.display()))
        }
    }
}

fn require_untracked_and_ignored(worktree: &Path, relative: &Path) -> Result<()> {
    let tracked = git_output(
        worktree,
        &[
            OsStr::new("ls-files"),
            OsStr::new("-z"),
            OsStr::new("--"),
            relative.as_os_str(),
        ],
    )?;
    anyhow::ensure!(
        tracked.stdout.is_empty(),
        "contains tracked files; refusing to remove it"
    );

    let mut ignored = relative.as_os_str().to_os_string();
    ignored.push("/");
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["check-ignore", "-q", "--no-index", "--"])
        .arg(&ignored)
        .output()
        .with_context(|| format!("checking whether {:?} is ignored", relative))?;
    anyhow::ensure!(
        output.status.success(),
        "is not gitignored; refusing to remove it"
    );
    Ok(())
}

fn require_not_tracked_if_in_repo(candidate: &Path) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(candidate)
        .args(["rev-parse", "--path-format=absolute", "--show-toplevel"])
        .output()
        .with_context(|| format!("checking Git ownership of {}", candidate.display()))?;
    if output.status.success() {
        let top = canonical_stdout_path(&output, "Git top level")?;
        let relative = candidate.strip_prefix(&top).with_context(|| {
            format!(
                "candidate {} is not beneath reported Git top level {}",
                candidate.display(),
                top.display()
            )
        })?;
        return require_untracked_and_ignored(&top, relative);
    }
    // A failed probe is proof of NOTHING by default: permission errors,
    // `safe.directory` dubious-ownership refusals, and a corrupt/dangling
    // gitdir pointer all exit non-zero too, and git's message for the last
    // one ("fatal: not a git repository: <path>") contains the same
    // substring as the genuine no-repository-anywhere case. Only the exact,
    // specific "no repository in this directory or any parent" message may
    // be read as "outside Git, therefore safe" — every other failure is
    // UNKNOWN, which is a refusal, never a deletion.
    let stderr = String::from_utf8_lossy(&output.stderr);
    anyhow::ensure!(
        definitively_outside_a_git_repository(&stderr),
        "Git ownership probe for {} was inconclusive, refusing to delete it: {}",
        candidate.display(),
        stderr.trim()
    );
    Ok(())
}

/// True only for git's message when no repository was found in `candidate`
/// or any parent directory — the one failure mode that actually proves the
/// path is outside Git. Every other fatal (dubious ownership, a corrupt or
/// dangling `.git` pointer, permission denial) must stay UNKNOWN.
fn definitively_outside_a_git_repository(stderr: &str) -> bool {
    stderr.contains("not a git repository (or any")
}

fn git_path(dir: &Path, args: &[&str]) -> Result<PathBuf> {
    let os_args = args.iter().map(OsStr::new).collect::<Vec<_>>();
    let output = git_output(dir, &os_args)?;
    canonical_stdout_path(&output, "Git path")
}

fn git_output(dir: &Path, args: &[&OsStr]) -> Result<Output> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .with_context(|| format!("running git in {}", dir.display()))?;
    anyhow::ensure!(
        output.status.success(),
        "git {:?} failed in {}: {}",
        args,
        dir.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output)
}

fn canonical_stdout_path(output: &Output, label: &str) -> Result<PathBuf> {
    let text = std::str::from_utf8(&output.stdout)
        .with_context(|| format!("{label} was not UTF-8"))?
        .trim();
    anyhow::ensure!(!text.is_empty(), "{label} was empty");
    PathBuf::from(text)
        .canonicalize()
        .with_context(|| format!("canonicalizing {label} {text:?}"))
}

fn canonical_existing_dir(path: &Path, label: &str) -> Result<PathBuf> {
    anyhow::ensure!(
        path.is_absolute(),
        "{label} {} is not absolute",
        path.display()
    );
    let path = path
        .canonicalize()
        .with_context(|| format!("canonicalizing {label} {}", path.display()))?;
    anyhow::ensure!(
        path.is_dir(),
        "{label} {} is not a directory",
        path.display()
    );
    Ok(path)
}

fn reclaim_shared_cache(
    fleet_dir: &Path,
    name: &str,
    max_gb: u64,
    dry_run: bool,
) -> SharedCacheReport {
    let path = fleet_dir.join(name);
    match real_directory_exists(&path) {
        Ok(false) => {
            return SharedCacheReport {
                path,
                before_bytes: 0,
                after_bytes: 0,
                would_reclaim_bytes: 0,
                candidate_entries: 0,
                detail: "absent; nothing to do".into(),
                failed: false,
            };
        }
        Ok(true) => {}
        Err(error) => {
            return SharedCacheReport {
                path,
                before_bytes: 0,
                after_bytes: 0,
                would_reclaim_bytes: 0,
                candidate_entries: 0,
                detail: format!("SKIPPED: {error:#}"),
                failed: true,
            };
        }
    }
    // The same proof a per-task candidate has to produce, for the same
    // reason and BEFORE anything is planned or deleted. `gc::run_gc` will
    // delete any entry under `{debug,release}/{deps,build,.fingerprint}`,
    // and the fleet root is routinely INSIDE a Git repository (the live one
    // sits under `~/.cmctl`, which is a checkout), so "it is called target"
    // is not evidence that nothing in it is tracked. This asks Git: no
    // tracked file anywhere beneath the root, and the root itself ignored,
    // or the cache is left alone. An inconclusive probe is a refusal, never
    // a deletion — see `require_not_tracked_if_in_repo`. It runs in dry-run
    // too, so a plan never advertises candidates the real sweep would
    // refuse.
    if let Err(error) = require_not_tracked_if_in_repo(&path) {
        return SharedCacheReport {
            path,
            before_bytes: 0,
            after_bytes: 0,
            would_reclaim_bytes: 0,
            candidate_entries: 0,
            detail: format!("SKIPPED: {error:#}"),
            failed: true,
        };
    }
    let before = match gc::allocated_size(&path) {
        Ok(size) => size,
        Err(error) => {
            return SharedCacheReport {
                path,
                before_bytes: 0,
                after_bytes: 0,
                would_reclaim_bytes: 0,
                candidate_entries: 0,
                detail: format!("ERROR: {error:#}"),
                failed: true,
            };
        }
    };
    if dry_run {
        return match gc::plan_gc(&path, Some(max_gb)) {
            Ok(gc_report) => SharedCacheReport {
                path,
                before_bytes: gc_report.before_bytes,
                after_bytes: gc_report.before_bytes,
                would_reclaim_bytes: gc_report.before_bytes.saturating_sub(gc_report.after_bytes),
                candidate_entries: gc_report.candidate_entries,
                detail: format!(
                    "dry-run; {} candidate entr{} would reclaim {} toward the {max_gb} GiB cap{}",
                    gc_report.candidate_entries,
                    if gc_report.candidate_entries == 1 {
                        "y"
                    } else {
                        "ies"
                    },
                    human_bytes(gc_report.before_bytes.saturating_sub(gc_report.after_bytes)),
                    if gc_report.outcome == gc::GcOutcome::StillOverCap {
                        "; projected result STILL OVER CAP"
                    } else {
                        ""
                    },
                ),
                failed: gc_report.outcome == gc::GcOutcome::StillOverCap,
            },
            Err(error) => SharedCacheReport {
                path,
                before_bytes: before,
                after_bytes: before,
                would_reclaim_bytes: 0,
                candidate_entries: 0,
                detail: format!("ERROR planning dry-run: {error:#}"),
                failed: true,
            },
        };
    }
    if let Err(error) = record_cleanup_intent(
        fleet_dir,
        &format!(
            "gc shared cache {} ({before} bytes, cap {max_gb} GiB)",
            path.display()
        ),
    ) {
        // No durable record, no deletion — same fail-closed rule as a
        // per-task candidate.
        return SharedCacheReport {
            path,
            before_bytes: before,
            after_bytes: before,
            would_reclaim_bytes: 0,
            candidate_entries: 0,
            detail: format!("ERROR: could not record cleanup intent, refusing to gc it: {error:#}"),
            failed: true,
        };
    }
    match gc::run_gc(&path, Some(max_gb)) {
        Ok(gc_report) => {
            let result = SharedCacheReport {
                path: path.clone(),
                before_bytes: gc_report.before_bytes,
                after_bytes: gc_report.after_bytes,
                would_reclaim_bytes: gc_report.before_bytes.saturating_sub(gc_report.after_bytes),
                candidate_entries: gc_report.candidate_entries,
                detail: gc_report.summary(),
                failed: gc_report.outcome == gc::GcOutcome::StillOverCap,
            };
            let _ = record_cleanup_result(
                fleet_dir,
                &format!("gc {}: {}", path.display(), result.detail),
            );
            result
        }
        Err(error) => {
            let after = gc::allocated_size(fleet_dir.join(name).as_path()).unwrap_or(before);
            let _ = record_cleanup_result(
                fleet_dir,
                &format!("FAILED gc {}: {error:#}", path.display()),
            );
            SharedCacheReport {
                path,
                before_bytes: before,
                after_bytes: after,
                would_reclaim_bytes: 0,
                candidate_entries: 0,
                detail: format!("ERROR: {error:#}"),
                failed: true,
            }
        }
    }
}

fn terminal_status(status: &str) -> bool {
    matches!(status, "landed" | "retired")
}

fn zpool_capacity(pool: &str) -> Result<u8> {
    validate_pool_name(pool)?;
    let output = Command::new("zpool")
        .args(["list", "-Hp", "-o", "capacity", pool])
        .output()
        .context("running zpool list")?;
    anyhow::ensure!(
        output.status.success(),
        "zpool list failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    parse_capacity(&output.stdout)
}

fn validate_pool_name(pool: &str) -> Result<()> {
    anyhow::ensure!(!pool.is_empty(), "pool name is empty");
    anyhow::ensure!(!pool.starts_with('-'), "pool name must not start with '-'");
    anyhow::ensure!(
        pool.bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':')),
        "pool name contains unsupported characters"
    );
    Ok(())
}

fn parse_capacity(stdout: &[u8]) -> Result<u8> {
    let text = std::str::from_utf8(stdout).context("zpool capacity output was not UTF-8")?;
    let mut lines = text.lines().filter(|line| !line.trim().is_empty());
    let value = lines
        .next()
        .context("zpool capacity output was empty")?
        .trim();
    anyhow::ensure!(
        lines.next().is_none(),
        "zpool returned more than one capacity row"
    );
    let capacity = value
        .strip_suffix('%')
        .unwrap_or(value)
        .parse::<u8>()
        .with_context(|| format!("invalid zpool capacity {value:?}"))?;
    anyhow::ensure!(capacity <= 100, "zpool capacity {capacity} exceeds 100");
    Ok(capacity)
}

fn human_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.2} GiB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.2} MiB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.2} KiB", bytes / KIB)
    } else {
        format!("{} B", bytes as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::{SCRATCH_GC_CLAIMANT, TaskControls};

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
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]);
        git(&repo, &["config", "user.name", "test"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        fs::write(repo.join(".gitignore"), "target/\n").unwrap();
        fs::write(repo.join("tracked"), "base\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "base"]);
        repo
    }

    fn add_task(
        ledger: &Ledger,
        repo: &Path,
        fleet: &Path,
        status: &str,
        operator_driven: bool,
    ) -> (i64, PathBuf, String) {
        let id = ledger
            .add_task_scoped(
                "scratch fixture",
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
        git(repo, &["branch", &branch]);
        let worktree = fleet.join(format!("task-{id}"));
        git(
            repo,
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
        match status {
            "landed" | "retired" => ledger.set_task_status(id, status).unwrap(),
            "landing" => assert!(ledger.transition_if(id, "done", "landing").unwrap()),
            "running" => {
                ledger.requeue_task(id, false).unwrap();
                ledger
                    .start_attempt(
                        id,
                        "fixture-running",
                        Some(worktree.to_str().unwrap()),
                        Some(&branch),
                        "codex",
                        None,
                    )
                    .unwrap();
            }
            other => panic!("unsupported fixture status {other}"),
        }
        if operator_driven {
            ledger
                .set_operator_driven(id, true, "protect terminal scratch", "operator")
                .unwrap();
        }
        (id, worktree, branch)
    }

    fn scratch_dirs(fleet: &Path, worktree: &Path, id: i64) -> Vec<PathBuf> {
        let paths = vec![
            worktree.join("src/target"),
            fleet.join(format!("task-{id}-target")),
        ];
        for (index, path) in paths.iter().enumerate() {
            fs::create_dir_all(path).unwrap();
            fs::write(path.join(format!("artifact-{index}")), vec![7_u8; 8192]).unwrap();
        }
        paths
    }

    #[test]
    fn terminal_cleanup_removes_all_shapes_but_preserves_worktree_and_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = tmp.path();
        let repo = repository(fleet);
        let ledger = Ledger::open(&fleet.join("ledger.db")).unwrap();
        let (id, worktree, branch) = add_task(&ledger, &repo, fleet, "landed", false);
        let paths = scratch_dirs(fleet, &worktree, id);
        let forbidden_root_target = worktree.join("target");
        fs::create_dir(&forbidden_root_target).unwrap();
        fs::write(forbidden_root_target.join("keep"), "outside allow-list\n").unwrap();
        let task = ledger.task(id).unwrap().unwrap();

        let report = reclaim_task_scratch_checked(&task, &repo, fleet, false, &mut || true);

        assert!(
            report.skipped_paths.is_empty(),
            "{:?}",
            report.skipped_paths
        );
        assert_eq!(report.candidate_dirs, 2);
        assert_eq!(report.removed_dirs, 2);
        assert!(report.before_bytes > 0);
        assert_eq!(report.after_bytes, 0);
        assert!(paths.iter().all(|path| !path.exists()));
        assert!(
            forbidden_root_target.join("keep").is_file(),
            "worktree-root target is outside the deletion allow-list"
        );
        assert!(worktree.is_dir(), "worktree itself survives");
        let branches = git_output(
            &repo,
            &[
                OsStr::new("branch"),
                OsStr::new("--list"),
                OsStr::new(&branch),
            ],
        )
        .unwrap();
        assert!(
            !branches.stdout.is_empty(),
            "branch survives scratch cleanup"
        );
        assert!(
            worktree.join("tracked").is_file(),
            "tracked source survives"
        );
    }

    #[test]
    fn tracked_or_symlinked_target_roots_are_refused() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let fleet = tmp.path();
        let repo = repository(fleet);
        let ledger = Ledger::open(&fleet.join("ledger.db")).unwrap();
        let (id, worktree, _) = add_task(&ledger, &repo, fleet, "landed", false);
        let tracked_target = worktree.join("src/target");
        fs::create_dir_all(&tracked_target).unwrap();
        fs::write(tracked_target.join("keep"), "tracked\n").unwrap();
        git(&worktree, &["add", "-f", "src/target/keep"]);
        git(&worktree, &["commit", "-m", "tracked target fixture"]);

        let outside = fleet.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), "outside\n").unwrap();
        symlink(&outside, fleet.join(format!("task-{id}-target"))).unwrap();
        let task = ledger.task(id).unwrap().unwrap();

        let report = reclaim_task_scratch_checked(&task, &repo, fleet, false, &mut || true);

        assert_eq!(report.removed_dirs, 0);
        assert_eq!(report.skipped_paths.len(), 2, "{:?}", report.skipped_paths);
        assert!(tracked_target.join("keep").is_file());
        assert!(outside.join("keep").is_file());
        assert!(fleet.join(format!("task-{id}-target")).is_symlink());
    }

    #[test]
    fn sweep_skips_running_landing_and_operator_reserved_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = tmp.path();
        let repo = repository(fleet);
        let ledger = Ledger::open(&fleet.join("ledger.db")).unwrap();
        let mut all_paths = Vec::new();
        for (status, operator) in [("running", false), ("landing", false), ("landed", true)] {
            let (id, worktree, _) = add_task(&ledger, &repo, fleet, status, operator);
            all_paths.extend(scratch_dirs(fleet, &worktree, id));
        }
        let options = ScratchOptions {
            fleet_dir: fleet.to_path_buf(),
            repo,
            terminal_age_hours: 0,
            pressure_pool: None,
            pressure_percent: 80,
            shared_max_gb: DEFAULT_SHARED_MAX_GB,
            selection_time: Utc::now(),
            dry_run: false,
        };

        let report = sweep(&ledger, &options).unwrap();

        assert_eq!(report.candidate_dirs, 0);
        assert!(all_paths.iter().all(|path| path.is_dir()));
    }

    #[test]
    fn sweep_reclaims_crash_stranded_terminal_legacy_review_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = tmp.path();
        let repo = repository(fleet);
        let ledger = Ledger::open(&fleet.join("ledger.db")).unwrap();
        let (id, _, _) = add_task(&ledger, &repo, fleet, "landed", false);
        let legacy = fleet.join(format!(".foreman-review-workdir-task-{id}"));
        git(
            &repo,
            &[
                "worktree",
                "add",
                "--detach",
                legacy.to_str().unwrap(),
                "main",
            ],
        );
        fs::create_dir_all(legacy.join("src/target")).unwrap();
        fs::write(legacy.join("src/target/private-artifact"), "large\n").unwrap();
        let options = ScratchOptions {
            fleet_dir: fleet.to_path_buf(),
            repo: repo.clone(),
            terminal_age_hours: 0,
            pressure_pool: None,
            pressure_percent: 80,
            shared_max_gb: DEFAULT_SHARED_MAX_GB,
            selection_time: Utc::now(),
            dry_run: false,
        };

        let report = sweep(&ledger, &options).unwrap();

        assert_eq!(report.review_worktrees.candidates, 1);
        assert_eq!(report.review_worktrees.removed, 1);
        assert!(!legacy.exists(), "terminal legacy review checkout leaked");
        let listed = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .unwrap();
        assert!(listed.status.success());
        assert!(
            !String::from_utf8_lossy(&listed.stdout).contains(legacy.to_str().unwrap()),
            "terminal legacy review registration leaked"
        );
    }

    #[test]
    fn dry_run_records_allocated_before_and_after_without_deleting() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = tmp.path();
        let repo = repository(fleet);
        let ledger = Ledger::open(&fleet.join("ledger.db")).unwrap();
        let (id, worktree, _) = add_task(&ledger, &repo, fleet, "landed", false);
        let paths = scratch_dirs(fleet, &worktree, id);
        let options = ScratchOptions {
            fleet_dir: fleet.to_path_buf(),
            repo,
            terminal_age_hours: 0,
            pressure_pool: None,
            pressure_percent: 80,
            shared_max_gb: DEFAULT_SHARED_MAX_GB,
            selection_time: Utc::now(),
            dry_run: true,
        };

        let report = sweep(&ledger, &options).unwrap();

        assert!(report.before_bytes > 0);
        assert_eq!(report.after_bytes, report.before_bytes);
        assert_eq!(report.would_reclaim_bytes, report.before_bytes);
        assert_eq!(report.removed_dirs, 0);
        assert!(paths.iter().all(|path| path.is_dir()));
        let line = report.summary_lines().join("\n");
        assert!(line.contains(" -> "), "{line}");
        assert!(line.contains("dry-run"), "{line}");
    }

    #[test]
    fn dry_run_includes_shared_cache_candidates_in_the_total() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = tmp.path();
        let repo = repository(fleet);
        let ledger = Ledger::open(&fleet.join("ledger.db")).unwrap();
        let candidate = fleet.join("target/debug/deps/stale.rlib");
        fs::create_dir_all(candidate.parent().unwrap()).unwrap();
        fs::write(&candidate, vec![3_u8; 8192]).unwrap();
        let options = ScratchOptions {
            fleet_dir: fleet.to_path_buf(),
            repo,
            terminal_age_hours: 0,
            pressure_pool: None,
            pressure_percent: 80,
            shared_max_gb: 0,
            selection_time: Utc::now(),
            dry_run: true,
        };

        let report = sweep(&ledger, &options).unwrap();

        assert!(candidate.is_file(), "dry-run must retain shared entries");
        assert_eq!(report.shared[0].candidate_entries, 1);
        assert!(report.shared[0].would_reclaim_bytes > 0);
        let summary = report.summary_lines().join("\n");
        assert!(summary.contains("1 shared entry(s)"), "{summary}");
        assert!(summary.contains("would be reclaimed; dry-run"), "{summary}");
    }

    /// The fleet root is routinely INSIDE a Git checkout — the live one sits
    /// under `~/.cmctl`, which is a repository — so `gc`-ing the shared
    /// `target`/`target-refine` caches is not automatically safe just
    /// because of their names. `gc::run_gc` will delete any entry under
    /// `{debug,release}/{deps,build,.fingerprint}`, so a tracked file in one
    /// of those places is eligible unless something asks Git first. This is
    /// that check: a shared cache with a tracked file in it is refused
    /// whole, loudly, in dry-run and for real, and the file survives.
    #[test]
    fn shared_cache_with_a_tracked_file_is_refused_not_gcd() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = tmp.path().join("fleet");
        fs::create_dir(&fleet).unwrap();
        // The fleet root IS the repository here, which is the shape that
        // makes the blocker reachable at all.
        git(&fleet, &["init", "-b", "main"]);
        git(&fleet, &["config", "user.name", "test"]);
        git(&fleet, &["config", "user.email", "test@example.com"]);
        let tracked = fleet.join("target/debug/deps/tracked.rlib");
        fs::create_dir_all(tracked.parent().unwrap()).unwrap();
        fs::write(&tracked, vec![5_u8; 8192]).unwrap();
        git(&fleet, &["add", "-f", "target/debug/deps/tracked.rlib"]);
        git(&fleet, &["commit", "-m", "a tracked file inside the cache"]);

        for dry_run in [true, false] {
            let cache = reclaim_shared_cache(&fleet, "target", 0, dry_run);
            assert!(
                cache.failed,
                "a cache holding a tracked file must be reported as a refusal (dry_run={dry_run})"
            );
            assert!(
                cache.detail.contains("tracked"),
                "the refusal must say why (dry_run={dry_run}): {}",
                cache.detail
            );
            assert_eq!(cache.candidate_entries, 0, "dry_run={dry_run}");
            assert_eq!(cache.would_reclaim_bytes, 0, "dry_run={dry_run}");
            assert!(
                tracked.is_file(),
                "the tracked file must survive (dry_run={dry_run})"
            );
        }
    }

    /// The other half of the gate: an ordinary gitignored, untracked shared
    /// cache inside a repository is still reclaimed. A safety check that
    /// refuses everything would be indistinguishable from the feature not
    /// working.
    #[test]
    fn ignored_untracked_shared_cache_inside_a_repository_is_still_gcd() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = tmp.path().join("fleet");
        fs::create_dir(&fleet).unwrap();
        git(&fleet, &["init", "-b", "main"]);
        git(&fleet, &["config", "user.name", "test"]);
        git(&fleet, &["config", "user.email", "test@example.com"]);
        fs::write(fleet.join(".gitignore"), "target/\n").unwrap();
        git(&fleet, &["add", ".gitignore"]);
        git(&fleet, &["commit", "-m", "ignore the shared cache"]);
        let stale = fleet.join("target/debug/deps/stale.rlib");
        fs::create_dir_all(stale.parent().unwrap()).unwrap();
        fs::write(&stale, vec![3_u8; 8192]).unwrap();

        // A 0 GiB cap can never be MET (the tree itself has a size), so this
        // asserts the gate let the cache through — not that the gc met its
        // bound.
        let planned = reclaim_shared_cache(&fleet, "target", 0, true);
        assert!(!planned.detail.contains("SKIPPED"), "{}", planned.detail);
        assert_eq!(planned.candidate_entries, 1, "{}", planned.detail);
        assert!(stale.is_file(), "dry-run deletes nothing");

        let done = reclaim_shared_cache(&fleet, "target", 0, false);
        assert!(!done.detail.contains("SKIPPED"), "{}", done.detail);
        assert!(!stale.exists(), "the untracked, ignored entry must be gone");
    }

    #[test]
    fn pressure_reclaims_a_younger_terminal_task_and_reports_real_capacity() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = tmp.path();
        let repo = repository(fleet);
        let ledger = Ledger::open(&fleet.join("ledger.db")).unwrap();
        let (id, worktree, branch) = add_task(&ledger, &repo, fleet, "landed", false);
        let paths = scratch_dirs(fleet, &worktree, id);
        git(
            &repo,
            &["update-ref", "-d", &format!("refs/heads/{branch}")],
        );
        let options = ScratchOptions {
            fleet_dir: fleet.to_path_buf(),
            repo,
            terminal_age_hours: 24 * 365,
            pressure_pool: Some("fixture".into()),
            pressure_percent: 80,
            shared_max_gb: DEFAULT_SHARED_MAX_GB,
            selection_time: Utc::now(),
            dry_run: false,
        };
        let mut capacities = [94_u8, 73_u8].into_iter();
        let mut probe = |_pool: &str| Ok(capacities.next().unwrap());

        let report = sweep_with_probe(&ledger, &options, &mut probe).unwrap();

        assert!(report.pressure_escalated);
        assert_eq!(report.pressure_before, Some(94));
        assert_eq!(report.pressure_after, Some(73));
        assert!(paths.iter().all(|path| !path.exists()));
        assert!(worktree.is_dir(), "branchless landed worktree survives");
    }

    #[test]
    fn capacity_parser_is_strict() {
        assert_eq!(parse_capacity(b"94\n").unwrap(), 94);
        assert_eq!(parse_capacity(b"55%\n").unwrap(), 55);
        assert!(parse_capacity(b"101\n").is_err());
        assert!(parse_capacity(b"94\n55\n").is_err());
        assert!(validate_pool_name("-oops").is_err());
        assert!(validate_pool_name("pool name").is_err());
    }

    /// A corrupt/dangling `.git` gitlink makes `git rev-parse --show-toplevel`
    /// fail with "fatal: not a git repository: <path>" — the SAME substring
    /// the genuine "no repository anywhere" case prints, just without the
    /// "(or any parent...)" suffix. A probe that reads any non-zero exit as
    /// "outside Git, therefore safe" would delete this candidate; the fix
    /// must refuse it as inconclusive instead.
    #[test]
    fn indeterminate_git_probe_refuses_sibling_deletion() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = tmp.path();
        let repo = repository(fleet);
        let ledger = Ledger::open(&fleet.join("ledger.db")).unwrap();
        let (id, _worktree, _) = add_task(&ledger, &repo, fleet, "landed", false);

        let sibling = fleet.join(format!("task-{id}-target"));
        fs::create_dir_all(&sibling).unwrap();
        fs::write(sibling.join("artifact"), vec![7_u8; 4096]).unwrap();
        fs::write(
            sibling.join(".git"),
            "gitdir: /nonexistent/task-97-fixture-gitdir\n",
        )
        .unwrap();

        let task = ledger.task(id).unwrap().unwrap();
        let report = reclaim_task_scratch_checked(&task, &repo, fleet, false, &mut || true);

        assert_eq!(report.removed_dirs, 0, "{:?}", report.skipped_paths);
        assert!(
            !report.skipped_paths.is_empty(),
            "an inconclusive git probe must be reported, not silently allowed"
        );
        assert!(
            sibling.join("artifact").is_file(),
            "indeterminate git ownership must refuse deletion, not permit it"
        );
    }

    /// `Ledger::begin_scratch_cleanup` is the exclusive lock the sweep and
    /// the post-landing reclaim now both go through. A second attempt to
    /// lease the same task must fail while the first lease is held, and an
    /// ordinary requeue must be refused too — the exact race the manual
    /// sweep's "task-state check races deletion" gap allowed: a task coming
    /// back to `queued` (and from there, dispatch) while its scratch is
    /// mid-deletion.
    #[test]
    fn scratch_gc_lease_is_exclusive_and_blocks_ordinary_requeue() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = tmp.path();
        let repo = repository(fleet);
        let ledger = Ledger::open(&fleet.join("ledger.db")).unwrap();
        let (id, _worktree, _) = add_task(&ledger, &repo, fleet, "landed", false);

        let leased = ledger
            .begin_scratch_cleanup(id)
            .unwrap()
            .expect("a landed, unclaimed, non-operator task must be leasable");
        let stamp = leased.claimed_by.clone().unwrap();
        assert!(
            crate::ledger::is_scratch_gc_claimant(&stamp),
            "the lease must be recognisable as a scratch-cleanup lease: {stamp}"
        );
        assert!(
            stamp.contains(&format!("pid={}", std::process::id())),
            "the lease must be stamped with the reclaiming process: {stamp}"
        );
        assert!(
            crate::ledger::scratch_gc_owner_alive(&stamp),
            "this process holds the lease and is obviously alive: {stamp}"
        );

        assert!(
            ledger.begin_scratch_cleanup(id).unwrap().is_none(),
            "a second lease attempt must fail while the first is held"
        );

        let err = ledger
            .requeue_task(id, false)
            .expect_err("an ordinary requeue must be refused while the scratch lease is held");
        assert!(
            format!("{err:#}").contains(SCRATCH_GC_CLAIMANT),
            "the refusal should name the holder: {err:#}"
        );

        // The interlock: `--force` is NOT an override while the reclaiming
        // process is alive. It used to be, which left a window where an
        // operator could clear the lease and let dispatch into a worktree a
        // `remove_dir_all` was still walking.
        let err = ledger
            .requeue_task_with(id, true, |_, _| true)
            .expect_err("--force must be refused while the reclaiming process is alive");
        let err = format!("{err:#}");
        assert!(
            err.contains("refused with and without --force"),
            "the refusal must say --force does not override it: {err}"
        );
        assert!(
            err.contains(&format!("pid {}", std::process::id())),
            "the refusal must name the pid to kill: {err}"
        );
        assert_eq!(
            ledger.task(id).unwrap().unwrap().status,
            "landed",
            "a refused force requeue must not have moved the task"
        );
        assert_eq!(ledger.task(id).unwrap().unwrap().claimed_by, Some(stamp));

        // ...and once that process is observed GONE — a sweep that crashed
        // mid-lease — `--force` is the recovery path it always was, because
        // now nothing is deleting.
        ledger.requeue_task_with(id, true, |_, _| false).unwrap();
        assert_eq!(ledger.task(id).unwrap().unwrap().status, "queued");
        assert!(ledger.task(id).unwrap().unwrap().claimed_by.is_none());

        // Once released normally, requeue works again without --force.
        let (id2, _worktree2, _) = add_task(&ledger, &repo, fleet, "landed", false);
        let stamp2 = ledger
            .begin_scratch_cleanup(id2)
            .unwrap()
            .unwrap()
            .claimed_by
            .unwrap();
        ledger.end_scratch_cleanup(id2, &stamp2).unwrap();
        ledger.requeue_task(id2, false).unwrap();
        assert_eq!(ledger.task(id2).unwrap().unwrap().status, "queued");
    }

    /// The other edge into the same race: an operator RESERVING a task
    /// while its scratch is being reclaimed. `begin_scratch_cleanup` refuses
    /// to lease an already-reserved task, but nothing stopped the flag being
    /// set after the lease was taken — and the sweep's revalidation checked
    /// only the claimant, so `remove_dir_all` carried on emptying a worktree
    /// the ledger was by then calling operator-reserved. Reserving must be
    /// refused for exactly as long as the reclaiming process is alive.
    #[test]
    fn reserving_a_task_is_refused_while_its_scratch_is_being_reclaimed() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = tmp.path();
        let repo = repository(fleet);
        let ledger = Ledger::open(&fleet.join("ledger.db")).unwrap();
        let (id, _worktree, _) = add_task(&ledger, &repo, fleet, "landed", false);

        let stamp = ledger
            .begin_scratch_cleanup(id)
            .unwrap()
            .unwrap()
            .claimed_by
            .unwrap();

        let err = ledger
            .set_operator_driven_with(id, true, "protect scratch", "operator", |_, _| true)
            .expect_err("reserving must be refused while the reclaiming process is alive");
        let err = format!("{err:#}");
        assert!(
            err.contains(SCRATCH_GC_CLAIMANT),
            "the refusal should name the holder: {err}"
        );
        assert!(
            err.contains(&format!("pid {}", std::process::id())),
            "the refusal must name the pid to kill: {err}"
        );
        assert!(
            !ledger.task(id).unwrap().unwrap().operator_driven,
            "a refused reservation must not have set the flag"
        );
        assert!(
            ledger.scratch_cleanup_still_held(id, &stamp),
            "the refusal must leave the sweep's lease exactly as it was"
        );

        // Un-reserving is never refused: a reserved task is one the lease
        // guard would not have leased, so clearing the flag races nothing.
        ledger
            .set_operator_driven_with(id, false, "already released", "operator", |_, _| true)
            .unwrap();

        // A sweep whose process is gone cannot be deleting anything, so the
        // operator is not locked out by a stranded lease.
        ledger
            .set_operator_driven_with(id, true, "protect scratch", "operator", |_, _| false)
            .unwrap();
        assert!(ledger.task(id).unwrap().unwrap().operator_driven);

        // ...and defence in depth: with the flag now set, the sweep's own
        // revalidation stops it even though the claimant is unchanged.
        assert!(
            !ledger.scratch_cleanup_still_held(id, &stamp),
            "an operator-reserved task must fail revalidation regardless of the claimant"
        );
    }

    /// Defence in depth behind the [`Ledger::requeue_task_with`] interlock:
    /// if the lease is nonetheless lost mid-task — this process died and
    /// something released the stale lease — a task with two candidates
    /// (worktree `src/target/` and the sibling `task-N-target/`) must stop
    /// before the second `remove_dir_all` rather than deleting a directory
    /// that has already been handed back.
    #[test]
    fn lease_lost_between_candidates_stops_further_deletion() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = tmp.path();
        let repo = repository(fleet);
        let ledger = Ledger::open(&fleet.join("ledger.db")).unwrap();
        let (id, worktree, _) = add_task(&ledger, &repo, fleet, "landed", false);
        let paths = scratch_dirs(fleet, &worktree, id);
        assert_eq!(paths.len(), 2, "worktree target + sibling target");

        let leased = ledger.begin_scratch_cleanup(id).unwrap().unwrap();
        let stamp = leased.claimed_by.clone().unwrap();
        let mut calls = 0u32;
        let report = reclaim_task_scratch_checked(&leased, &repo, fleet, false, &mut || {
            calls += 1;
            if calls == 1 {
                true
            } else {
                // Simulate the lease being released and the task requeued
                // between the two candidate deletions — which the ledger
                // only permits once this process is observed gone.
                ledger.end_scratch_cleanup(id, &stamp).unwrap();
                ledger.requeue_task_with(id, true, |_, _| false).unwrap();
                false
            }
        });

        assert_eq!(
            report.removed_dirs, 1,
            "only the first candidate may be removed once the lease is lost: {:?}",
            report.skipped_paths
        );
        assert!(
            paths.iter().filter(|p| p.exists()).count() == 1,
            "exactly one candidate must survive the lost lease"
        );
        assert!(
            report
                .skipped_paths
                .iter()
                .any(|line| line.contains("lease lost mid-sweep")),
            "the refusal must be visible in the report: {:?}",
            report.skipped_paths
        );
        assert_eq!(
            ledger.task(id).unwrap().unwrap().status,
            "queued",
            "the forced requeue itself must have gone through"
        );
    }

    /// [`reclaim_task_scratch_leased`] must skip a task outright — never
    /// fall back to a stale snapshot — when the lease can't be acquired,
    /// e.g. because something else (another sweep instance, an operator
    /// action) already holds it.
    #[test]
    fn leased_reclaim_skips_a_task_that_lost_the_lease_race() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = tmp.path();
        let repo = repository(fleet);
        let ledger = Ledger::open(&fleet.join("ledger.db")).unwrap();
        let (id, worktree, _) = add_task(&ledger, &repo, fleet, "landed", false);
        let paths = scratch_dirs(fleet, &worktree, id);

        // Simulate a concurrent actor already holding the cleanup lease.
        ledger.begin_scratch_cleanup(id).unwrap().unwrap();

        let report = reclaim_task_scratch_leased(&ledger, id, &repo, fleet, false);

        assert_eq!(report.removed_dirs, 0);
        assert!(!report.skipped_paths.is_empty());
        assert!(
            paths.iter().all(|path| path.is_dir()),
            "a lost lease race must never touch the filesystem"
        );
    }

    /// Real (non-dry-run) cleanup must write a durable intent record before
    /// deletion and a result record after, for both a task's own scratch and
    /// the shared caches — the durable audit trail the manual sweep left
    /// entirely to (possibly never-captured) stdout.
    #[test]
    fn real_sweep_writes_a_durable_intent_and_result_journal() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = tmp.path();
        let repo = repository(fleet);
        let ledger = Ledger::open(&fleet.join("ledger.db")).unwrap();
        let (id, worktree, _) = add_task(&ledger, &repo, fleet, "landed", false);
        scratch_dirs(fleet, &worktree, id);
        let shared = fleet.join("target/debug/deps/stale.rlib");
        fs::create_dir_all(shared.parent().unwrap()).unwrap();
        fs::write(&shared, vec![3_u8; 8192]).unwrap();

        let options = ScratchOptions {
            fleet_dir: fleet.to_path_buf(),
            repo,
            terminal_age_hours: 0,
            pressure_pool: None,
            pressure_percent: 80,
            shared_max_gb: 0,
            selection_time: Utc::now(),
            dry_run: false,
        };

        let report = sweep(&ledger, &options).unwrap();
        assert!(report.removed_dirs > 0);

        let journal =
            fs::read_to_string(fleet.join(".foreman-scratch-gc.journal")).expect("journal exists");
        let intents = journal.lines().filter(|l| l.contains(" intent ")).count();
        let results = journal.lines().filter(|l| l.contains(" result ")).count();
        assert!(intents > 0, "no intent recorded: {journal}");
        assert_eq!(
            intents, results,
            "every recorded intent must have a matching result: {journal}"
        );
        assert!(
            journal.contains("bytes"),
            "the journal should carry a size, not just a bare notice: {journal}"
        );
    }
}
