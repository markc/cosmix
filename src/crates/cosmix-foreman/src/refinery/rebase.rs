use super::*;

/// What the worktree-provisioning rebase did to a task branch.
///
/// A retry reuses the task branch by design (partial state is the point),
/// but a branch based on an OLD integration commit keeps re-testing old
/// in-tree code under tier-0 — harness fixes never reach it. Measured
/// 2026-08-20: an attempt dispatched AFTER a harness fix landed still
/// failed against its pre-fix base. So every reused worktree replays the
/// branch onto the integration head, and this is the verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebaseOutcome {
    /// The branch already contained the integration head; nothing moved.
    AlreadyOnBase { base: String },
    /// Replayed cleanly. `from` is the pre-rebase tip, `to` the new one.
    Rebased {
        base: String,
        from: String,
        to: String,
    },
    /// Replay conflicted. The rebase has been ABORTED — the branch is back
    /// at `from` and the autostashed partial state is restored — and these
    /// paths are the ones git could not merge. NEVER auto-resolved: the
    /// agent's next attempt resolves them from the finding this becomes.
    Conflicted {
        base: String,
        from: String,
        files: Vec<String>,
        /// git's own output, kept verbatim for the case where the conflict
        /// left no unmerged paths for us to name.
        git_output: String,
    },
}

impl RebaseOutcome {
    /// The integration commit this attempt's branch is based on — the "base
    /// of record" that goes into the ledger trail either way.
    pub fn base(&self) -> &str {
        match self {
            RebaseOutcome::AlreadyOnBase { base }
            | RebaseOutcome::Rebased { base, .. }
            | RebaseOutcome::Conflicted { base, .. } => base,
        }
    }

    pub fn conflicted(&self) -> bool {
        matches!(self, RebaseOutcome::Conflicted { .. })
    }
}

/// Resolve the handoff finding only after provisioning has proved that the
/// task branch now sits cleanly on the requested integration base.
pub fn resolve_completed_rebase(
    ledger: &Ledger,
    task_id: i64,
    outcome: &RebaseOutcome,
) -> Result<()> {
    anyhow::ensure!(
        !outcome.conflicted(),
        "cannot resolve a rebase-conflict finding from a conflicted rebase"
    );
    ledger_write_with_busy_retry("resolving completed rebase handoff", || {
        ledger.resolve_task_findings_reason(task_id, FindingReason::RebaseConflict)
    })?;
    Ok(())
}

/// A provisioned task worktree plus what provisioning did to its branch.
#[derive(Debug)]
pub struct TaskWorktree {
    pub path: PathBuf,
    /// Present only when an existing, provenance-validated worktree was
    /// reused with an integration branch configured.
    pub rebase: Option<RebaseOutcome>,
}

/// Replay `worktree`'s checked-out branch onto `integration`'s head.
///
/// Three things this gets right that the obvious version does not:
///
///  1. **`--autostash`.** Worktree reuse EXISTS to carry partial state, and
///     `git rebase` refuses a dirty tracked tree. Autostash saves it,
///     replays, restores — and on the conflict path `--abort` restores it
///     too (verified: tracked edits, staged adds and untracked files all
///     came back).
///  2. **The rebase-in-progress probe asks GIT for the path.** A linked
///     worktree's `.git` is a FILE, so `<worktree>/.git/rebase-merge` never
///     exists — the state dir lives under the per-worktree `$GIT_DIR`
///     (`.../.git/worktrees/<name>/rebase-merge`). Probing the naive path
///     answers "not a conflict" for every real conflict, and then does not
///     abort, leaving a poisoned worktree behind forever.
///  3. **Conflicting paths come from the index, not from parsing.** git
///     writes `CONFLICT (content): Merge conflict in <path>` to *stdout*,
///     not stderr, and the wording varies by conflict class; the unmerged
///     index entries are the authority.
pub(super) fn rebase_onto(worktree: &Path, integration: &str) -> Result<RebaseOutcome> {
    anyhow::ensure!(
        crate::ledger::valid_branch_name(integration),
        "invalid integration branch name {integration:?}"
    );
    // Pin the base as a SHA: the ledger records WHICH commit this attempt
    // is based on, and a moving branch name cannot be that record.
    let base = git(
        worktree,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{integration}^{{commit}}"),
        ],
    )
    .with_context(|| format!("resolving integration branch {integration}"))?
    .trim()
    .to_string();
    let from = git(worktree, &["rev-parse", "HEAD"])?.trim().to_string();
    if matches!(
        git_status(worktree, &["merge-base", "--is-ancestor", &base, &from])?,
        (Some(0), _, _)
    ) {
        return Ok(RebaseOutcome::AlreadyOnBase { base });
    }
    let (code, stdout, stderr) = git_status(worktree, &["rebase", "--autostash", &base])?;
    if code == Some(0) {
        let to = git(worktree, &["rev-parse", "HEAD"])?.trim().to_string();
        return Ok(RebaseOutcome::Rebased { base, from, to });
    }
    // Read the unmerged paths BEFORE aborting — the abort clears them.
    let files = unmerged_files(worktree);
    let in_progress = rebase_in_progress(worktree);
    // Abort regardless of the verdict: a half-rebased worktree (detached
    // HEAD, conflict markers in tracked files) poisons every later attempt
    // at this task, so the one thing that must not happen is returning with
    // the rebase still open. Its own failure is reported, not swallowed.
    let (abort_code, _, abort_err) = git_status(worktree, &["rebase", "--abort"])?;
    let git_output = format!("{stdout}{stderr}");
    if !in_progress {
        // Not a conflict: git broke (bad ref, unreadable object, refusal).
        // Infrastructure, not the task's fault — bail rather than bounce.
        anyhow::bail!(
            "rebasing onto {integration} ({base}) failed without leaving a rebase \
             in progress — this is git failing, not a conflict: {}",
            git_output.trim()
        );
    }
    anyhow::ensure!(
        abort_code == Some(0),
        "rebase onto {integration} ({base}) conflicted AND `git rebase --abort` \
         failed ({abort_code:?}): {} — the worktree is left mid-rebase and needs \
         an operator",
        abort_err.trim()
    );
    Ok(RebaseOutcome::Conflicted {
        base,
        from,
        files,
        git_output,
    })
}

/// Unmerged index paths, NUL-separated so a path containing a newline
/// cannot forge an extra entry. Empty on any git trouble — the caller
/// falls back to quoting git's own output.
fn unmerged_files(worktree: &Path) -> Vec<String> {
    let Ok((Some(0), out, _)) =
        git_status(worktree, &["diff", "--name-only", "-z", "--diff-filter=U"])
    else {
        return Vec::new();
    };
    let mut files: Vec<String> = out
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    files.sort();
    files.dedup();
    files
}

/// Is a rebase open in this worktree? Asks git where its own state dir is
/// — see `rebase_onto`'s point 2 for why the literal `.git/rebase-merge`
/// path is always wrong here.
fn rebase_in_progress(worktree: &Path) -> bool {
    ["rebase-merge", "rebase-apply"].iter().any(|dir| {
        matches!(
            git_status(worktree, &["rev-parse", "--git-path", dir]),
            Ok((Some(0), ref out, _)) if worktree.join(out.trim()).exists()
        )
    })
}

/// Record a provisioning conflict for the implementation attempt which will
/// still launch on the cleanly-aborted branch. The prompt carries this
/// finding and an authoritative rebase-first instruction; provisioning does
/// not score an attempt which has not started.
///
/// The finding is the whole mechanism — the next attempt is handed it in
/// its prompt (see `runner::findings_section`) and resolves the conflict on
/// the branch itself. The refinery never resolves one for an agent.
pub fn bounce_rebase_conflict(
    ledger: &Ledger,
    task_id: i64,
    branch: &str,
    integration: &str,
    outcome: &RebaseOutcome,
) -> Result<()> {
    let RebaseOutcome::Conflicted {
        base,
        from,
        files,
        git_output,
    } = outcome
    else {
        anyhow::bail!("bounce_rebase_conflict called on a non-conflicting rebase");
    };
    let named = if files.is_empty() {
        format!(
            "git named no unmerged paths; its own output was:\n\n{}",
            git_output.trim()
        )
    } else {
        format!(
            "Conflicting files:\n\n{}",
            files
                .iter()
                .map(|f| format!("  - {f}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };
    let body = format!(
        "Branch `{branch}` could not be replayed onto the integration branch \
         `{integration}` at base {base}.\n\n{named}\n\nThe rebase was ABORTED: \
         the branch is back at {from} with its partial state intact, and \
         nothing was auto-resolved. Resolve these conflicts on `{branch}` \
         yourself — `git rebase {integration}`, fix each file, \
         `git rebase --continue` — then redo the task against the new base. \
         Until the branch sits on {base} your tier-0 run is testing the OLD \
         in-tree code, so a green gate here would not mean the work lands."
    );
    ledger_write_with_busy_retry("recording task rebase conflict", || {
        ledger.file_finding_reasoned(
            Some(task_id),
            "major",
            &format!("rebase conflict: {branch} onto {integration}"),
            &body,
            "dispatch",
            FindingReason::RebaseConflict,
        )
    })?;
    Ok(())
}
pub(super) fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let (code, stdout, stderr) = git_status(repo, args)?;
    if code != Some(0) {
        return Err(infrastructure_message(format!(
            "git {} failed ({code:?}): {}",
            args.join(" "),
            stderr.trim()
        )));
    }
    Ok(stdout)
}

/// git with the exit code exposed, for callers that must distinguish "the
/// answer is no" (e.g. rev-parse exit 1, merge-base exit 1) from "git broke".
pub(super) fn git_status(repo: &Path, args: &[&str]) -> Result<(Option<i32>, String, String)> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| {
            infrastructure_message(format!(
                "spawning git {args:?} in {}: {error}",
                repo.display()
            ))
        })?;
    Ok((
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}
