impl Ledger {
    #[cfg(test)]
    pub(crate) fn set_busy_timeout_for_test(&self, timeout: std::time::Duration) -> Result<()> {
        self.conn.busy_timeout(timeout)?;
        Ok(())
    }

    /// Record where a task's work lives so the refinery can find it. The
    /// branch name later becomes git argv, so it is validated at this write
    /// boundary — an agent-supplied "-c" or "--exec=…" must die here, not
    /// inside a git invocation.
    ///
    /// CLAIM-SCOPED: the worktree/branch pair is the refinery's instruction
    /// for what to LAND, so an unguarded write here is as dangerous as an
    /// unguarded terminal transition — a delayed write from a dead attempt
    /// would point the refinery at the previous attempt's branch. Guarded on
    /// owner AND generation.
    pub fn set_task_workspace(
        &self,
        id: i64,
        token: ClaimToken<'_>,
        worktree: Option<&str>,
        branch: Option<&str>,
    ) -> Result<()> {
        if let Some(b) = branch {
            anyhow::ensure!(valid_branch_name(b), "invalid branch name {b:?}");
        }
        let now = Utc::now().to_rfc3339();
        let n = self.conn.execute(
            "UPDATE tasks SET worktree = COALESCE(?1, worktree),
                    branch = COALESCE(?2, branch), updated_at = ?3
             WHERE id = ?4 AND claimed_by = ?5 AND attempt = ?6",
            params![worktree, branch, now, id, token.owner, token.generation],
        )?;
        anyhow::ensure!(
            n == 1,
            "task {id} is no longer claim generation {} held by {}; \
             refusing to record its workspace",
            token.generation,
            token.owner
        );
        Ok(())
    }

    /// The refinery's queue: done tasks with a branch, oldest first.
    pub fn landable_tasks(&self) -> Result<Vec<Task>> {
        let mut stmt = self.conn.prepare(
            "SELECT * FROM tasks WHERE status = 'done' AND branch IS NOT NULL ORDER BY id",
        )?;
        let rows = stmt.query_map([], row_to_task)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn total_spend_usd(&self) -> Result<f64> {
        Ok(self
            .conn
            .query_row("SELECT COALESCE(SUM(cost_usd), 0.0) FROM runs", [], |r| {
                r.get(0)
            })?)
    }

    /// Retry a landing: re-enter the landable state (done, unclaimed) for a task
    /// that already has a branch. Refuses when the task has no branch, is claimed/
    /// running/landing, or the branch doesn't exist in the repo. Ladder failures
    /// are untouched (a landing retry is not an attempt). Records a finding for
    /// the operator-initiated re-land.
    ///
    /// This uses a single guarded UPDATE in an IMMEDIATE transaction to avoid
    /// racing a planner claim: the preconditions are checked atomically in the
    /// WHERE clause, and the UPDATE only succeeds if the task is still unclaimed
    /// and in a valid state when the statement executes.
    pub fn land_task(&self, id: i64, repo: &Path) -> Result<()> {
        use std::process::Command;

        // Fetch the task first for the branch name and to check if it was parked
        let task = self.task(id)?.with_context(|| format!("no task {id}"))?;

        // Guard: task must have a branch
        let branch = task.branch.as_ref().with_context(|| {
            format!(
                "task {id} has no branch — `foreman task land` is for retrying a landing, \
                 not for initiating one"
            )
        })?;

        // Guard: branch must exist in the repo
        let branch_ref = format!("refs/heads/{}", branch);
        let output = Command::new("git")
            .args(["rev-parse", "--verify", &branch_ref])
            .current_dir(repo)
            .stdin(std::process::Stdio::null())
            .output()
            .with_context(|| format!("spawning git rev-parse for branch {branch}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "branch {branch} does not exist in repo {} — \
                 git rev-parse failed: {}",
                repo.display(),
                stderr.trim()
            );
        }

        // Remember if this task was parked so we can resolve its ladder findings
        let was_parked = task.status == "parked";

        // Atomic guarded UPDATE: only succeeds if task is still unclaimed and
        // not in a live state (claimed, running, landing). The branch check is
        // NOT in the WHERE clause because it doesn't change between the read
        // above and this UPDATE (branches are only set by agents finishing work).
        let done = TaskStatus::Done;
        let claimed = TaskStatus::Claimed;
        let running = TaskStatus::Running;
        let landing = TaskStatus::Landing;
        let now = Utc::now().to_rfc3339();

        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;

        let n = tx.execute(
            "UPDATE tasks SET status = ?1, claimed_by = NULL, lease_until = NULL,
                    claim_pid = NULL, claimed_at = NULL, updated_at = ?2
             WHERE id = ?3 AND branch IS NOT NULL
               AND claimed_by IS NULL
               AND status NOT IN (?4, ?5, ?6)",
            params![
                done.as_db_str(),
                now,
                id,
                claimed.as_db_str(),
                running.as_db_str(),
                landing.as_db_str()
            ],
        )?;

        // Resolve ladder findings if this was a parked task
        if n == 1 && was_parked {
            tx.execute(
                "UPDATE findings SET status = 'resolved'
                 WHERE task_id = ?1 AND filed_by = 'ladder' AND status = 'open'",
                params![id],
            )?;
        }

        tx.commit()?;

        if n == 0 {
            // Re-read to diagnose which guard fired
            let current = self.task(id)?.with_context(|| format!("no task {id}"))?;
            if current.branch.is_none() {
                anyhow::bail!(
                    "task {id} has no branch — `foreman task land` is for retrying a landing"
                );
            }
            if current.claimed_by.is_some()
                || matches!(
                    current
                        .status
                        .parse::<TaskStatus>()
                        .unwrap_or(TaskStatus::Queued),
                    TaskStatus::Claimed | TaskStatus::Running | TaskStatus::Landing
                )
            {
                anyhow::bail!(
                    "task {id} is {} (claimed by {}); `foreman task land` refuses live work",
                    current.status,
                    current.claimed_by.unwrap_or_else(|| "(none)".into())
                );
            }
            anyhow::bail!("task {id} not found or moved unexpectedly");
        }

        // Record a finding for the operator-initiated re-land
        self.file_finding_reasoned(
            Some(id),
            "info",
            &format!("operator-initiated landing retry for task {id}"),
            &format!(
                "Task {id}: marked for landing retry on branch {branch}. Ladder failures \
                 were untouched — this is a landing retry, not a new attempt."
            ),
            "foreman",
            FindingReason::Operator,
        )?;

        Ok(())
    }
}
