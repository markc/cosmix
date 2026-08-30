use super::*;

/// A completed task's own checkout changed after the runner released it.
/// This is local to that task, so it bounces without stopping unrelated
/// landings in the same refinery sweep.
#[derive(Debug)]
pub(super) struct DirtyTaskWorktree {
    pub(super) path: PathBuf,
    pub(super) detail: String,
}

impl std::fmt::Display for DirtyTaskWorktree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "task worktree {} became dirty after completion; refusing landing reuse:\n{}",
            self.path.display(),
            self.detail
        )
    }
}

impl std::error::Error for DirtyTaskWorktree {}

/// Validate the task-44 provenance boundary for an existing task worktree.
/// Path convention alone is not authority: it must be registered by this
/// clone and resolve to the same common Git directory. Branch validation is
/// separate because crash recovery must recognise a legitimate worktree
/// while Git has temporarily detached HEAD during an interrupted rebase.
pub(super) fn registered_task_worktree_identity(
    clone: &Path,
    task_id: i64,
    recorded_worktree: Option<&str>,
) -> Result<Option<PathBuf>> {
    let path = match recorded_worktree {
        Some(recorded) => {
            let path = PathBuf::from(recorded);
            anyhow::ensure!(
                path.is_absolute(),
                "recorded task worktree {recorded:?} is not absolute"
            );
            path
        }
        None => clone
            .parent()
            .context("integration clone has no parent directory")?
            .join(format!("task-{task_id}")),
    };
    git(clone, &["worktree", "prune"])?;
    if !path.exists() {
        anyhow::ensure!(
            recorded_worktree.is_none(),
            "recorded task worktree {} no longer exists",
            path.display()
        );
        return Ok(None);
    }
    let canon = path
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", path.display()))?;
    let registered = git(clone, &["worktree", "list", "--porcelain"])?
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .any(|listed| {
            Path::new(listed)
                .canonicalize()
                .is_ok_and(|listed| listed == canon)
        });
    let clone_common = std::fs::canonicalize(
        git(
            clone,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?
        .trim(),
    )
    .context("canonicalizing the clone's git common dir")?;
    let same_repo = matches!(
        git_status(
            &path,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        ),
        Ok((Some(0), ref out, _))
            if std::fs::canonicalize(out.trim()).is_ok_and(|common| common == clone_common)
    );
    anyhow::ensure!(
        registered && same_repo,
        "{} exists but is not this clone's worktree registered to this repository; remove it before continuing",
        path.display()
    );
    Ok(Some(path))
}

pub(super) fn registered_task_worktree(
    clone: &Path,
    task_id: i64,
    branch: &str,
    recorded_worktree: Option<&str>,
) -> Result<Option<PathBuf>> {
    let Some(path) = registered_task_worktree_identity(clone, task_id, recorded_worktree)? else {
        return Ok(None);
    };
    let on_expected_branch = matches!(
        git_status(&path, &["rev-parse", "--abbrev-ref", "HEAD"]),
        Ok((Some(0), ref out, _)) if out.trim() == branch
    );
    anyhow::ensure!(
        on_expected_branch,
        "{} is not on task branch {branch}; restore it before continuing",
        path.display()
    );
    Ok(Some(path))
}

/// Abort a rebase left open by a crash between `git rebase` and the normal
/// abort path. Git's own abort restores the task branch and its original tip;
/// only then may ledger recovery make the task claimable again.
pub(super) fn recover_interrupted_task_rebase(
    clone: &Path,
    task_id: i64,
    branch: &str,
    recorded_worktree: Option<&str>,
) -> Result<()> {
    let Some(path) = registered_task_worktree_identity(clone, task_id, recorded_worktree)? else {
        return Ok(());
    };
    let rebase_open = ["rebase-merge", "rebase-apply"].into_iter().try_fold(
        false,
        |open, state| -> Result<bool> {
            let state_path = git(
                &path,
                &["rev-parse", "--path-format=absolute", "--git-path", state],
            )?;
            Ok(open || Path::new(state_path.trim()).exists())
        },
    )?;
    if rebase_open {
        let (code, _, stderr) = git_status(&path, &["rebase", "--abort"])?;
        anyhow::ensure!(
            code == Some(0),
            "git rebase --abort failed in {}: {}",
            path.display(),
            stderr.trim()
        );
    }
    let current = git(&path, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    anyhow::ensure!(
        current.trim() == branch,
        "{} is on {:?}, not task branch {branch}, after interrupted-landing recovery",
        path.display(),
        current.trim()
    );
    Ok(())
}

/// Report every tracked, untracked, ignored, and submodule change except a
/// `target`-named path component or ignored `Cargo.lock` anywhere in the tree
/// (Cargo-generated output commonly ignored by library workspaces), and
/// untracked-but-not-ignored paths that fall under a `targets` entry Foreman
/// explicitly pinned for this landing. This is
/// narrower than "every ignored path is safe": a late write to some other
/// ignored file still counts as dirt. `-z` keeps unusual path names
/// unambiguous.
pub(super) fn worktree_dirt_except_targets(
    worktree: &Path,
    targets: &[PathBuf],
) -> Result<Vec<String>> {
    let root = worktree
        .canonicalize()
        .with_context(|| format!("canonicalizing task worktree {}", worktree.display()))?;
    let allowed = targets
        .iter()
        .map(|target| {
            let target = crate::target_dir::canonicalize_allow_missing(target)?;
            target
                .strip_prefix(&root)
                .map(PathBuf::from)
                .with_context(|| {
                    format!(
                        "pinned Cargo target {} is outside task worktree {}",
                        target.display(),
                        root.display()
                    )
                })
        })
        .collect::<Result<Vec<_>>>()?;
    let raw = git(
        worktree,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--ignored=matching",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
    )?;
    Ok(raw
        .split('\0')
        .filter(|record| !record.is_empty())
        .filter_map(|record| {
            if record.len() < 4 || record.as_bytes().get(2) != Some(&b' ') {
                // The second pathname of a -z rename/copy record has no
                // status prefix. The first record already marks the tree
                // dirty; keep this one as dirt too rather than turning a
                // task-local mutation into an infrastructure error.
                return Some(format!("git-status continuation {record:?}"));
            }
            let status = &record[..2];
            let path = Path::new(&record[3..]);
            let is_build_output = match status {
                "!!" => {
                    path.components().any(|c| c.as_os_str() == "target")
                        || path.file_name().is_some_and(|name| name == "Cargo.lock")
                        || allowed.iter().any(|target| path.starts_with(target))
                }
                "??" => allowed.iter().any(|target| path.starts_with(target)),
                _ => false,
            };
            (!is_build_output).then(|| record.to_string())
        })
        .collect())
}

/// Landing worktree. Normal dispatched tasks reuse their registered
/// `task-<id>` checkout and therefore its already-warm private target.
/// Legacy/manual tasks recreate a detached checkout at one deterministic
/// path beside the integration clone, so a reviewer sees the same cwd on
/// every sweep without retaining its private target between landings.
///
/// The worktree is a SIBLING of the repo (same filesystem depth), not under
/// /tmp: this workspace's manifests path-dep sibling checkouts via relative
/// `../../../../` paths, which only resolve from the repo's own depth.
pub(super) struct TempWorktree {
    pub(super) path: PathBuf,
    cleanup_repo: Option<PathBuf>,
}

impl TempWorktree {
    pub(super) fn add_or_reuse_task(
        repo: &Path,
        task_id: i64,
        branch: &str,
        recorded_worktree: Option<&str>,
    ) -> Result<Self> {
        if let Some(path) = registered_task_worktree(repo, task_id, branch, recorded_worktree)? {
            let dirty = git(&path, &["status", "--porcelain"])?;
            if !dirty.trim().is_empty() {
                return Err(DirtyTaskWorktree {
                    path,
                    detail: dirty.trim().to_string(),
                }
                .into());
            }
            return Ok(TempWorktree {
                path,
                cleanup_repo: None,
            });
        }
        let parent = repo
            .parent()
            .ok_or_else(|| infrastructure_message("repo has no parent directory"))?;
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
        let path = parent.join(format!(".foreman-review-{repo_tag}-task-{task_id}"));
        git(repo, &["worktree", "prune"])?;
        if path.exists() {
            let canon = path
                .canonicalize()
                .with_context(|| format!("canonicalizing {}", path.display()))?;
            let registered = git(repo, &["worktree", "list", "--porcelain"])?
                .lines()
                .filter_map(|line| line.strip_prefix("worktree "))
                .any(|listed| {
                    Path::new(listed)
                        .canonicalize()
                        .is_ok_and(|listed| listed == canon)
                });
            let clone_common = std::fs::canonicalize(
                git(
                    repo,
                    &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                )?
                .trim(),
            )
            .context("canonicalizing the clone's git common dir")?;
            let same_repo = matches!(
                git_status(
                    &path,
                    &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                ),
                Ok((Some(0), ref out, _))
                    if std::fs::canonicalize(out.trim())
                        .is_ok_and(|common| common == clone_common)
            );
            anyhow::ensure!(
                registered && same_repo,
                "{} exists but is not this clone's deterministic legacy review worktree",
                path.display()
            );
            git(
                repo,
                &[
                    "worktree",
                    "remove",
                    "--force",
                    path.to_str().context("worktree path not utf-8")?,
                ],
            )?;
            git(repo, &["worktree", "prune"])?;
        }

        std::fs::create_dir(&path).map_err(|error| {
            infrastructure_message(format!(
                "creating deterministic legacy review worktree {}: {error}",
                path.display()
            ))
        })?;
        if let Err(error) = git(
            repo,
            &[
                "worktree",
                "add",
                "--detach",
                path.to_str().context("worktree path not utf-8")?,
                &format!("refs/heads/{branch}"),
            ],
        ) {
            let _ = std::fs::remove_dir(&path);
            return Err(error);
        }
        Ok(TempWorktree {
            path,
            cleanup_repo: Some(repo.to_path_buf()),
        })
    }
}

impl Drop for TempWorktree {
    fn drop(&mut self) {
        let Some(repo) = self.cleanup_repo.as_deref() else {
            return;
        };
        let path = self.path.to_string_lossy().into_owned();
        match git_status(repo, &["worktree", "remove", "--force", &path]) {
            Ok((Some(0), _, _)) => {}
            Ok((code, _, stderr)) => eprintln!(
                "foreman: could not remove legacy review worktree {} (exit {code:?}): {}",
                self.path.display(),
                stderr.trim()
            ),
            Err(error) => eprintln!(
                "foreman: could not remove legacy review worktree {}: {error:#}",
                self.path.display()
            ),
        }
        if let Err(error) = git(repo, &["worktree", "prune"]) {
            eprintln!(
                "foreman: could not prune legacy review worktree metadata after removing {}: \
                 {error:#}",
                self.path.display()
            );
        }
    }
}

/// Provision (or reuse) a task's dedicated worktree as a SIBLING of the
/// integration clone — same path depth, so `../../../../`-style sibling
/// path deps resolve to the same neighbours the clone sees.
///
/// First dispatch creates the branch from the configured integration ref; a
/// retry after a bounce reuses the existing branch AND worktree, partial
/// state included — that context is the point of a retry. The worktree is
/// deliberately left in place after the run: the refinery lands from the
/// branch REF, and a parked task's tree is the human's forensic evidence.
/// A squatting directory or cross-task collision is refused, not adopted.
///
/// Reuse alone is not enough, though: a branch based on an OLD integration
/// commit keeps re-testing old in-tree code under tier-0, so harness fixes
/// never reach an existing task branch. When `integration` is given, the
/// reused branch is replayed onto that branch's head after its provenance is
/// validated — see [`rebase_onto`] — and the verdict comes back in
/// [`TaskWorktree::rebase`] for the caller to record (clean) or bounce on
/// (conflicted). Conflicts are never auto-resolved.
pub fn ensure_task_worktree(
    clone: &Path,
    task_id: i64,
    branch: &str,
    integration: Option<&str>,
) -> Result<TaskWorktree> {
    ensure_task_worktree_named(clone, task_id, branch, integration, "task-{id}")
}

pub fn ensure_task_worktree_named(
    clone: &Path,
    task_id: i64,
    branch: &str,
    integration: Option<&str>,
    worktree_template: &str,
) -> Result<TaskWorktree> {
    ensure_task_worktree_named_in(clone, task_id, branch, integration, worktree_template, None)
}

/// Project-aware worktree provisioning. Manifest mode supplies an explicit
/// project root; legacy callers retain the sibling-of-repo layout.
pub fn ensure_task_worktree_named_in(
    clone: &Path,
    task_id: i64,
    branch: &str,
    integration: Option<&str>,
    worktree_template: &str,
    project_root: Option<&Path>,
) -> Result<TaskWorktree> {
    anyhow::ensure!(
        crate::ledger::valid_branch_name(branch),
        "invalid branch name {branch:?}"
    );
    let parent = match project_root {
        Some(root) => root,
        None => clone
            .parent()
            .context("integration clone has no parent directory")?,
    };
    anyhow::ensure!(
        worktree_template.contains("{id}"),
        "worktree template must contain \"{{id}}\""
    );
    let worktree_name = worktree_template.replace("{id}", &task_id.to_string());
    anyhow::ensure!(
        Path::new(&worktree_name).components().count() == 1,
        "worktree template must resolve to one sibling path component"
    );
    let path = parent.join(worktree_name);
    // A stale registration (dir deleted out from under git) would make
    // `worktree add` refuse forever.
    git(clone, &["worktree", "prune"])?;
    if path.exists() {
        // Provenance, not just branch name: the dir must be a REGISTERED
        // worktree of THIS clone — an unrelated repo squatting the path on
        // a matching branch would swallow the agent's commits while the
        // refinery lands the clone's own (stale or absent) ref.
        let canon = path
            .canonicalize()
            .with_context(|| format!("canonicalizing {}", path.display()))?;
        let registered = git(clone, &["worktree", "list", "--porcelain"])?
            .lines()
            .filter_map(|l| l.strip_prefix("worktree "))
            .any(|p| {
                Path::new(p)
                    .canonicalize()
                    .is_ok_and(|listed| listed == canon)
            });
        // Registration alone is spoofable: prune keeps the entry when an
        // UNRELATED repo replaces the deleted worktree in-place, and its
        // branch name is attacker/accident-choosable. The unforgeable tie
        // is the git common dir — a real linked worktree's resolves to the
        // clone's own .git.
        let clone_common = std::fs::canonicalize(
            git(
                clone,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )?
            .trim(),
        )
        .context("canonicalizing the clone's git common dir")?;
        let same_repo = matches!(
            git_status(
                &path,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            ),
            Ok((Some(0), ref out, _))
                if std::fs::canonicalize(out.trim()).is_ok_and(|c| c == clone_common)
        );
        let on_expected_branch = matches!(
            git_status(&path, &["rev-parse", "--abbrev-ref", "HEAD"]),
            Ok((Some(0), ref out, _)) if out.trim() == branch
        );
        anyhow::ensure!(
            registered && same_repo && on_expected_branch,
            "{} exists but is not this clone's worktree on {branch}; \
             remove it before redispatching this task",
            path.display()
        );
        // Provenance first, THEN the rebase: replaying commits into a tree
        // whose ownership we have not established is exactly the swallow
        // the provenance check exists to refuse.
        let rebase = match integration {
            Some(int) => Some(rebase_onto(&path, int)?),
            None => None,
        };
        return Ok(TaskWorktree { path, rebase });
    }
    let on_branch = matches!(
        git_status(
            clone,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
        )?,
        (Some(0), _, _)
    );
    let path_str = path.to_string_lossy().into_owned();
    if on_branch {
        git(clone, &["worktree", "add", &path_str, branch])?;
    } else {
        let start = match integration {
            Some(integration) => {
                anyhow::ensure!(
                    crate::ledger::valid_branch_name(integration),
                    "invalid integration branch name {integration:?}"
                );
                format!("refs/heads/{integration}")
            }
            None => "HEAD".to_string(),
        };
        git(clone, &["worktree", "add", "-b", branch, &path_str, &start])?;
    }
    // Rebase only the reuse path above. A newly provisioned worktree is
    // created from the exact configured integration ref, while a
    // surviving branch recovered after a lost directory has not yet passed
    // the reuse-path provenance check required by this operation.
    Ok(TaskWorktree { path, rebase: None })
}
