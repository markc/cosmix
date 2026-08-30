impl Ledger {
    /// Requeue in one guarded statement — check-then-write from the CLI
    /// would race a claim landing in between. Non-force refuses live
    /// (running/claimed) tasks, AND any terminal task currently held by
    /// [`Ledger::begin_scratch_cleanup`] — a landed/retired task's build
    /// scratch is mid-deletion and must not come back to `queued` while that
    /// filesystem work is in flight. Even --force refuses 'landing': a
    /// landing task may already have its merge in flight, and yanking it
    /// desyncs git from the ledger — `foreman refine` recovers those itself.
    /// And NEITHER mode — `--force` included — may clear a scratch-cleanup
    /// lease whose reclaiming process is still alive; see
    /// [`Ledger::requeue_task_with`] for why that one override had to become
    /// the host's decision rather than the flag's.
    pub fn requeue_task(&self, id: i64, force: bool) -> Result<()> {
        self.requeue_task_with(id, force, crate::procutil::owner_alive)
    }

    /// [`Ledger::requeue_task`] with the process-liveness observation
    /// supplied by the caller, so the scratch-cleanup interlock can be
    /// exercised against a recorded answer instead of whatever `/proc`
    /// happens to say during a test run.
    ///
    /// The interlock: a requeue is what re-enables dispatch, and dispatch
    /// hands the task's worktree to a live agent. While the scratch sweep
    /// holds its lease it may be inside `remove_dir_all` on that worktree's
    /// `src/target/`. Revalidating the lease between candidate deletions
    /// (which the sweep does) is not sufficient on its own — the check and
    /// the deletion are separate operations, so a `--force` that cleared the
    /// lease immediately after a successful check could still dispatch into
    /// a deletion already in flight. So the refusal is enforced HERE, where
    /// the requeue commits, and it is decided by whether the reclaiming pid
    /// is actually running rather than by whether the operator passed a
    /// flag. Both this read and [`Ledger::begin_scratch_cleanup`]'s write
    /// take SQLite's write lock, so they serialise: the sweep cannot acquire
    /// a lease inside this transaction's window, and a requeue that commits
    /// leaves the task `queued`, which `begin_scratch_cleanup`'s
    /// `status IN ('landed', 'retired')` guard then refuses.
    ///
    /// A sweep whose process died mid-lease is NOT alive, so the crashed-
    /// sweep recovery path `--force` always documented still works, and
    /// works without the operator having to judge whether the process is
    /// dead. A sweep that is alive but wedged is recovered by killing its
    /// pid — the message says so, and names the pid.
    pub fn requeue_task_with(
        &self,
        id: i64,
        force: bool,
        owner_alive: impl Fn(Option<i64>, Option<i64>) -> bool,
    ) -> Result<()> {
        let queued = TaskStatus::Queued;
        let claimed = TaskStatus::Claimed;
        let running = TaskStatus::Running;
        let parked = TaskStatus::Parked;
        let landing = TaskStatus::Landing;
        // An explicit requeue is the operator saying "retry the handoff".
        // Branch-contract recurrence therefore resets for every requeue;
        // ladder/review/background counters retain their established
        // parked-only reset. The `attempt` claim generation is
        // monotonic and never resets: complete_verified's stale-result
        // guard depends on it not repeating. A parked task's blocker
        // findings resolve, and an infrastructure refusal sequence starts
        // fresh after the operator has corrected and requeued a parked task.
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        // Decided inside the write transaction, ahead of either UPDATE: a
        // live scratch-cleanup lease is refused in BOTH modes. This is the
        // only claim `--force` cannot override, because the thing it would
        // release is not a stalled agent but an in-flight `remove_dir_all`
        // on the worktree the requeue is about to make dispatchable.
        let live_scratch_lease: Option<(String, String)> = tx
            .query_row(
                "SELECT status, claimed_by FROM tasks WHERE id = ?1 AND claimed_by IS NOT NULL",
                params![id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?
            .filter(|(status, claimant)| {
                is_scratch_gc_claimant(claimant)
                    && matches!(status.as_str(), "landed" | "retired")
                    && scratch_gc_owner_alive_with(claimant, &owner_alive)
            });
        if let Some((status, claimant)) = live_scratch_lease {
            drop(tx);
            let pid = scratch_gc_claim_owner(&claimant)
                .map(|(pid, _)| pid.to_string())
                .unwrap_or_else(|| "?".to_string());
            anyhow::bail!(
                "task {id} is {status} and its build scratch is being reclaimed right now by \
                 `{claimant}` (pid {pid}, observed running) — a requeue would let dispatch hand \
                 the worktree to a live agent while `remove_dir_all` is still walking it, so \
                 this is refused with and without --force. The lease clears by itself the \
                 moment that sweep finishes; if it is wedged, kill pid {pid} and requeue \
                 --force then, which will succeed because the pid is gone"
            );
        }
        let was_parked: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM tasks WHERE id = ?1 AND status = ?2",
                params![id, parked.as_db_str()],
                |r| r.get(0),
            )
            .optional()?;
        let now = Utc::now().to_rfc3339();
        let n = if force {
            tx.execute(
                "UPDATE tasks SET status = ?1, claimed_by = NULL, lease_until = NULL,
                        claim_pid = NULL, claimed_at = NULL,
                        ladder_failures = CASE WHEN status = ?2 THEN 0
                                          ELSE ladder_failures END,
                        review_rejections = CASE WHEN status = ?2 THEN 0
                                            ELSE review_rejections END,
                        branch_contract_failures = 0,
                        background_abandonments = CASE WHEN status = ?2 THEN 0
                                                  ELSE background_abandonments END,
                        infra_refusals = CASE WHEN status = ?2 THEN 0
                                         ELSE infra_refusals END,
                        dispatch_after = NULL,
                        updated_at = ?3
                 WHERE id = ?4 AND status != ?5",
                params![
                    queued.as_db_str(),
                    parked.as_db_str(),
                    now,
                    id,
                    landing.as_db_str()
                ],
            )?
        } else {
            tx.execute(
                "UPDATE tasks SET status = ?1, claimed_by = NULL, lease_until = NULL,
                        claim_pid = NULL, claimed_at = NULL,
                        ladder_failures = CASE WHEN status = ?2 THEN 0
                                          ELSE ladder_failures END,
                        review_rejections = CASE WHEN status = ?2 THEN 0
                                            ELSE review_rejections END,
                        branch_contract_failures = 0,
                        background_abandonments = CASE WHEN status = ?2 THEN 0
                                                  ELSE background_abandonments END,
                        infra_refusals = CASE WHEN status = ?2 THEN 0
                                         ELSE infra_refusals END,
                        dispatch_after = NULL,
                        updated_at = ?3
                 WHERE id = ?4 AND status NOT IN (?5, ?6, ?7) AND claimed_by IS NULL",
                params![
                    queued.as_db_str(),
                    parked.as_db_str(),
                    now,
                    id,
                    running.as_db_str(),
                    claimed.as_db_str(),
                    landing.as_db_str()
                ],
            )?
        };
        if n == 1 && was_parked.is_some() {
            tx.execute(
                "UPDATE findings SET status = 'resolved'
                 WHERE task_id = ?1 AND status = 'open'
                   AND (filed_by = 'ladder' OR reason_code IN (?2, ?3, ?4, ?5, ?6, ?7))",
                params![
                    id,
                    FindingReason::AgentAbandonedBackground.as_db_str(),
                    FindingReason::TaskBudgetExhausted.as_db_str(),
                    FindingReason::RungRefusal.as_db_str(),
                    FindingReason::BranchContract.as_db_str(),
                    FindingReason::PolicyDenied.as_db_str(),
                    FindingReason::InfraRefusal.as_db_str()
                ],
            )?;
        }
        tx.commit()?;
        if n == 0 {
            let t = self.task(id)?.with_context(|| format!("no task {id}"))?;
            if t.status == landing.as_db_str() {
                anyhow::bail!(
                    "task {id} is mid-landing; its merge may already be in git — \
                     run `foreman refine` to recover it, never requeue"
                );
            }
            let claimant = t.claimed_by.as_deref().unwrap_or("?");
            let status = &t.status;
            if is_scratch_gc_claimant(claimant) {
                // Reached only when the claimant is a scratch lease whose
                // process is NOT alive (a live one bailed above, before the
                // UPDATE): a crashed sweep. Nothing is deleting, so --force
                // is the correct and now-safe recovery.
                anyhow::bail!(
                    "task {id} is {status} and holds a scratch-cleanup lease (`{claimant}`) \
                     whose process is no longer running — it crashed mid-sweep. Nothing is \
                     deleting from that worktree, so `foreman task requeue {id} --force` will \
                     release it"
                );
            }
            anyhow::bail!(
                "task {id} is {status} (claimed by {claimant}); a requeue would let a second \
                 agent into the same worktree — pass --force if the agent is dead"
            );
        }
        Ok(())
    }

}
