impl Ledger {
    #[allow(clippy::too_many_arguments)]
    fn park_task_with_finding(
        &self,
        id: i64,
        generation: ParkGeneration,
        title: &str,
        body: &str,
        filed_by: &str,
        reason: FindingReason,
    ) -> Result<bool> {
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let now = Utc::now().to_rfc3339();
        let parked = TaskStatus::Parked.as_db_str();
        let queued = TaskStatus::Queued.as_db_str();
        let bounced = TaskStatus::Bounced.as_db_str();
        let failed = TaskStatus::Failed.as_db_str();
        let n = match generation {
            ParkGeneration::LadderFailures(failures) => tx.execute(
                "UPDATE tasks SET status = ?1, updated_at = ?2
                 WHERE id = ?3 AND status IN (?4, ?5, ?6)
                   AND claimed_by IS NULL AND ladder_failures = ?7",
                params![parked, now, id, queued, bounced, failed, failures],
            )?,
            ParkGeneration::TaskBudget(budget_usd) => tx.execute(
                "UPDATE tasks SET status = ?1, updated_at = ?2
                 WHERE id = ?3 AND status IN (?4, ?5, ?6)
                   AND claimed_by IS NULL AND budget_usd = ?7",
                params![parked, now, id, queued, bounced, failed, budget_usd],
            )?,
        };
        if n == 0 {
            tx.commit()?;
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO findings (task_id, severity, title, body, filed_by, reason_code, created_at)
             VALUES (?1, 'blocker', ?2, ?3, ?4, ?5, ?6)",
            params![id, title, body, filed_by, reason.as_db_str(), now],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Park an exhausted task with its finding, atomically and GUARDED — a
    /// task claimed by someone else between the planner's read and this
    /// write must not be clobbered, and `failures` acts as a GENERATION
    /// check: a stale planner whose snapshot predates an operator's
    /// requeue-reset (failures back to 0) must not re-park the healthy
    /// task. Returns false when the task moved on.
    pub fn park_task(&self, id: i64, failures: i64, risk: &str) -> Result<bool> {
        self.park_task_with_finding(
            id,
            ParkGeneration::LadderFailures(failures),
            &format!("escalation ladder exhausted for task {id}"),
            &format!(
                "{failures} combined verifier-red/review-rejected ladder charges at risk {risk:?}; the top rung could \
                 not land it. Needs a human decision — respec, split, or \
                 `foreman task requeue {id}` to retry the ladder."
            ),
            "ladder",
            FindingReason::LadderExhausted,
        )
    }

    /// Park when quality charges still select a rung but every remaining
    /// rung has a durable pre-claim refusal (for example missing metering or
    /// capacity). This is not ladder exhaustion and must say so even when
    /// the task has zero quality charges.
    pub fn park_task_rungs_refused(&self, id: i64, failures: i64, risk: &str) -> Result<bool> {
        self.park_task_with_finding(
            id,
            ParkGeneration::LadderFailures(failures),
            &format!("all remaining ladder rungs refused for task {id}"),
            &format!(
                "Every remaining rung at risk {risk:?} was refused before claim by a metering, capacity, or lane constraint. The task has {failures} combined verifier-red/review-rejected ladder charges; those charges did not cause this park. Fix the rung refusals, then `foreman task requeue {id}`."
            ),
            "ladder",
            FindingReason::RungRefusal,
        )
    }

    /// Park a budget-exhausted task before claim, using the same guarded,
    /// atomic state-plus-finding transition as ladder exhaustion. The budget
    /// value is a generation check against a concurrent operator top-up.
    pub fn park_task_budget_exhausted(
        &self,
        id: i64,
        budget_usd: f64,
        charged_usd: f64,
        remaining_usd: f64,
        required_usd: f64,
    ) -> Result<bool> {
        for (name, value) in [
            ("budget", budget_usd),
            ("charged", charged_usd),
            ("remaining", remaining_usd),
            ("required", required_usd),
        ] {
            anyhow::ensure!(
                value.is_finite() && value >= 0.0,
                "task {id} has invalid {name} amount {value}"
            );
        }
        let title = format!(
            "task {id} budget exhausted: ${remaining_usd:.4} remaining, ${required_usd:.4} required"
        );
        let body = format!(
            "Task {id} is parked before claim: ${charged_usd:.4} has been charged against its \
             ${budget_usd:.4} budget, leaving ${remaining_usd:.4} for a run requiring \
             ${required_usd:.4}. Set a larger total with `foreman task set {id} --budget <usd>`, \
             then run `foreman task requeue {id}`. Use `--budget clear` to remove the task ceiling."
        );
        self.park_task_with_finding(
            id,
            ParkGeneration::TaskBudget(budget_usd),
            &title,
            &body,
            "budget",
            FindingReason::TaskBudgetExhausted,
        )
    }

    /// Retire a task by operator command. Refuses every live claim and the
    /// refinery's landing state. Files an info finding with the reason and
    /// transitions to retired status.
    pub fn retire_task(&self, id: i64, reason: &str) -> Result<()> {
        let retired = TaskStatus::Retired;
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let now = Utc::now().to_rfc3339();

        let task = tx
            .query_row(
                "SELECT * FROM tasks WHERE id = ?1",
                params![id],
                row_to_task,
            )
            .optional()?
            .ok_or_else(|| anyhow::anyhow!("no task {id}"))?;

        let status: TaskStatus = task.status.parse()?;
        match status {
            TaskStatus::Running => anyhow::bail!("cannot retire task {id} while it is running"),
            TaskStatus::Landing => anyhow::bail!("cannot retire task {id} while it is landing"),
            _ => {
                if let Some(claimant) = task.claimed_by.as_deref() {
                    anyhow::bail!(
                        "cannot retire task {id} while it is claimed by {claimant} ({status})"
                    );
                }
            }
        }

        let n = tx.execute(
            "UPDATE tasks SET status = ?1, updated_at = ?2
             WHERE id = ?3 AND claimed_by IS NULL AND status = ?4",
            params![retired.as_db_str(), now, id, status.as_db_str()],
        )?;
        anyhow::ensure!(
            n == 1,
            "task {id} became claimed or changed status during retire"
        );

        tx.execute(
            "INSERT INTO findings (task_id, severity, title, body, filed_by, reason_code, created_at)
             VALUES (?1, 'info', ?2, ?3, 'operator', ?4, ?5)",
            params![
                id,
                format!("task {id} retired"),
                reason,
                FindingReason::Retired.as_db_str(),
                now
            ],
        )?;
        resolve_task_findings_in_tx(&tx, id, &format!("task {id} retired: {reason}"), &now)?;
        tx.commit()?;
        Ok(())
    }

    /// Atomically reserve a terminal task for scratch cleanup. This is the
    /// scratch sweep's ONLY gate between reading a task as "landed/retired,
    /// unclaimed, not operator-reserved" and actually removing its build
    /// scratch: a plain re-read-then-write has a window where an operator's
    /// requeue (or a re-dispatch it enables) can hand the worktree back to a
    /// live run before `remove_dir_all` gets there. The guarded UPDATE
    /// closes that window by durably marking intent — the write commits
    /// before any filesystem work starts, so a crash mid-cleanup leaves
    /// [`SCRATCH_GC_CLAIMANT`] as the visible, durable evidence that cleanup
    /// was attempted rather than silently absent.
    ///
    /// The lease is stamped with THIS process's `(pid, starttime)` rather
    /// than a bare sentinel, so [`Ledger::requeue_task`] can refuse even a
    /// `--force` requeue for exactly as long as the reclaiming process is
    /// actually alive. `claim_pid` and `lease_until` are deliberately left
    /// untouched: a landed/retired row is outside
    /// [`Ledger::reap_dead_claims`]'s `('claimed', 'running')` candidate set
    /// anyway, and giving the reaper a lease to expire on a terminal task
    /// would let it resurrect a landed task into `queued`.
    ///
    /// Returns the freshly read row on success (`Ok(Some(_))`) — whose
    /// `claimed_by` IS the stamp the caller must hand back to
    /// [`Ledger::end_scratch_cleanup`] — or `Ok(None)` if the task is no
    /// longer landed/retired, is already claimed (by this or another
    /// claimant), or has become operator-reserved; the caller must skip it,
    /// never fall back to the stale snapshot.
    pub fn begin_scratch_cleanup(&self, id: i64) -> Result<Option<Task>> {
        let now = Utc::now().to_rfc3339();
        let n = self.conn.execute(
            "UPDATE tasks SET claimed_by = ?1, updated_at = ?2
             WHERE id = ?3 AND status IN ('landed', 'retired')
               AND claimed_by IS NULL AND operator_driven = 0",
            params![scratch_gc_claimant_stamp(), now, id],
        )?;
        if n != 1 {
            return Ok(None);
        }
        self.task(id)
    }

    /// Release a lease taken by [`Ledger::begin_scratch_cleanup`]. Guarded
    /// on the exact stamp that call returned, so a lease already reaped,
    /// force-requeued, or reassigned is never clobbered by a late release.
    pub fn end_scratch_cleanup(&self, id: i64, claimant: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE tasks SET claimed_by = NULL, updated_at = ?1
             WHERE id = ?2 AND claimed_by = ?3",
            params![now, id, claimant],
        )?;
        Ok(())
    }

    /// Is `id` still held by the exact scratch-cleanup lease `claimant`,
    /// and still unreserved, right now? The sweep revalidates with this
    /// between the candidate directories of one task.
    ///
    /// `operator_driven` is re-read as well as the claimant. The reservation
    /// edge is refused at its own write by
    /// [`Ledger::set_operator_driven_with`], so under a stamped lease from a
    /// live process this can never observe a reservation appear — but a lease
    /// left by a build predating the stamp answers "not alive" to that
    /// interlock and so is overridable. Re-reading the flag here means such a
    /// reservation still stops the sweep at the very next candidate instead
    /// of only at the next task.
    pub fn scratch_cleanup_still_held(&self, id: i64, claimant: &str) -> bool {
        matches!(
            self.task(id),
            Ok(Some(task)) if task.claimed_by.as_deref() == Some(claimant) && !task.operator_driven
        )
    }

}
