impl Ledger {
    /// Atomic claim: succeeds for exactly one caller. Unmet deps (any dep not
    /// yet `done`) refuse the claim. Dep gate and claim run inside one
    /// IMMEDIATE transaction so a concurrent requeue of a dependency cannot
    /// interleave between check and claim.
    pub fn claim_task(&self, id: i64, claimant: &str) -> Result<Task> {
        let now = Utc::now().to_rfc3339();
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        // No trusted pid: this is the generic/MCP claim path, where
        // `claimant` is agent-controlled free text — see `claim_task_in_tx`.
        let claimed = claim_task_in_tx(&tx, id, claimant, None, false, &now)?;
        tx.commit()?;
        Ok(claimed)
    }

    /// Renew a live claim's lease, fenced by both owner and generation.
    ///
    /// This deliberately does not depend on `claim_pid`: MCP and future
    /// remote workers have no controller-local process to inspect, but their
    /// heartbeat is still authoritative for the lease they hold. It also
    /// leaves `updated_at` and `claimed_at` alone; a heartbeat is not a task
    /// state change and must not erase the claim's real age.
    pub fn renew_claim(&self, id: i64, token: ClaimToken<'_>) -> Result<String> {
        self.renew_claim_at(id, token, &Utc::now().to_rfc3339())
    }

    /// Clock-supplied renewal for the replayable runner path and focused
    /// ledger tests. Returns the value read back from SQLite, not merely the
    /// timestamp the caller attempted to write.
    pub(crate) fn renew_claim_at(
        &self,
        id: i64,
        token: ClaimToken<'_>,
        now: &str,
    ) -> Result<String> {
        let now = parse_utc_timestamp(now, "claim heartbeat timestamp")?;
        let lease_until = (now + chrono::Duration::seconds(CLAIM_LEASE_SECS)).to_rfc3339();
        let renewed = self
            .conn
            .query_row(
                "UPDATE tasks SET lease_until = ?1
                 WHERE id = ?2 AND claimed_by = ?3 AND attempt = ?4
                   AND status IN (?5, ?6)
                 RETURNING lease_until",
                params![
                    lease_until,
                    id,
                    token.owner,
                    token.generation,
                    TaskStatus::Claimed.as_db_str(),
                    TaskStatus::Running.as_db_str()
                ],
                |row| row.get(0),
            )
            .optional()?;
        renewed.with_context(|| {
            format!(
                "task {id} is no longer claim generation {} held by {}; refusing heartbeat",
                token.generation, token.owner
            )
        })
    }

    /// Claim + record workspace + insert the run row + mark running, as ONE
    /// IMMEDIATE transaction. Replaces the four separate guarded writes the
    /// runner used to make, closing crash gaps and stale-generation races.
    #[allow(clippy::too_many_arguments)]
    pub fn start_attempt(
        &self,
        id: i64,
        claimant: &str,
        workdir: Option<&str>,
        branch: Option<&str>,
        agent: &str,
        model: Option<&str>,
    ) -> Result<(Task, i64)> {
        let now = Utc::now().to_rfc3339();
        self.start_attempt_at(
            id, claimant, None, workdir, branch, agent, model, None, &now, true,
        )
    }

    /// Clock-supplied runner path. Kept crate-private so ledger callers do
    /// not casually forge timestamps; replay is the one authority that must
    /// reproduce a previously supplied timeline.
    ///
    /// `claim_pid` must come from the caller's own `std::process::id()` —
    /// this is the one production entry point (`runner::run_task_with_clock_and_policy`)
    /// where that is true, which is exactly why `Ledger::reap_dead_claims`
    /// can trust the column it writes.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start_attempt_at(
        &self,
        id: i64,
        claimant: &str,
        claim_pid: Option<i64>,
        workdir: Option<&str>,
        branch: Option<&str>,
        agent: &str,
        model: Option<&str>,
        reserved_usd: Option<f64>,
        now: &str,
        allow_operator_driven: bool,
    ) -> Result<(Task, i64)> {
        self.start_attempt_with_role_at(
            id,
            claimant,
            claim_pid,
            workdir,
            branch,
            agent,
            model,
            Some("implement"),
            reserved_usd,
            now,
            allow_operator_driven,
        )
    }

    /// Start an attempt with an explicit role (implement | review | verify).
    /// For normal implementation runs, use `start_attempt` which defaults to "implement".
    #[allow(clippy::too_many_arguments)]
    pub fn start_attempt_with_role(
        &self,
        id: i64,
        claimant: &str,
        workdir: Option<&str>,
        branch: Option<&str>,
        agent: &str,
        model: Option<&str>,
        role: Option<&str>,
    ) -> Result<(Task, i64)> {
        let now = Utc::now().to_rfc3339();
        self.start_attempt_with_role_at(
            id, claimant, None, workdir, branch, agent, model, role, None, &now, true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn start_attempt_with_role_at(
        &self,
        id: i64,
        claimant: &str,
        claim_pid: Option<i64>,
        workdir: Option<&str>,
        branch: Option<&str>,
        agent: &str,
        model: Option<&str>,
        role: Option<&str>,
        reserved_usd: Option<f64>,
        now: &str,
        allow_operator_driven: bool,
    ) -> Result<(Task, i64)> {
        if let Some(b) = branch {
            anyhow::ensure!(valid_branch_name(b), "invalid branch name {b:?}");
        }
        if let Some(usd) = reserved_usd {
            anyhow::ensure!(
                usd.is_finite() && usd >= 0.0,
                "run reservation must be a finite non-negative value, got {usd}"
            );
        }
        let running = TaskStatus::Running;
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let claimed = claim_task_in_tx(&tx, id, claimant, claim_pid, allow_operator_driven, now)?;
        let workspace_updated = tx.execute(
            "UPDATE tasks SET worktree = COALESCE(?1, worktree),
                    branch = COALESCE(?2, branch), updated_at = ?3
             WHERE id = ?4 AND claimed_by = ?5 AND attempt = ?6",
            params![workdir, branch, now, id, claimant, claimed.attempt],
        )?;
        anyhow::ensure!(
            workspace_updated == 1,
            "task {id} claim changed while recording its workspace"
        );
        tx.execute(
            "INSERT INTO runs
                 (task_id, agent, model, role, reserved_usd, started_at, attempt)
             VALUES (?1, ?2, ?3, COALESCE(?4, 'implement'), ?5, ?6, ?7)",
            params![id, agent, model, role, reserved_usd, now, claimed.attempt],
        )?;
        let run_id = tx.last_insert_rowid();
        let marked = tx.execute(
            "UPDATE tasks SET status = ?1, updated_at = ?2
             WHERE id = ?3 AND claimed_by = ?4 AND attempt = ?5 AND status = ?6",
            params![
                running.as_db_str(),
                now,
                id,
                claimant,
                claimed.attempt,
                TaskStatus::Claimed.as_db_str()
            ],
        )?;
        anyhow::ensure!(marked == 1, "task {id} is no longer claimed by {claimant}");
        let task = tx
            .query_row(
                "SELECT * FROM tasks WHERE id = ?1",
                params![id],
                row_to_task,
            )
            .optional()?
            .context("task vanished while starting attempt")?;
        tx.commit()?;
        Ok((task, run_id))
    }

    /// Unconditional status write, for operator commands. Releasing states
    /// clear the claim; the runner uses [`Ledger::finish_task`] instead so a
    /// requeued-and-reclaimed task cannot have its new claim clobbered.
    pub fn set_task_status(&self, id: i64, status: &str) -> Result<()> {
        self.set_status(id, status.parse()?)
    }

    /// Typed core of [`Ledger::set_task_status`] — no string can reach the
    /// column that did not come through [`TaskStatus`].
    pub fn set_status(&self, id: i64, status: TaskStatus) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let release = matches!(
            status,
            TaskStatus::Queued
                | TaskStatus::Bounced
                | TaskStatus::Failed
                | TaskStatus::Done
                | TaskStatus::Landed
        );
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let n = if release {
            tx.execute(
                "UPDATE tasks SET status = ?1, claimed_by = NULL, lease_until = NULL,
                        claim_pid = NULL, claimed_at = NULL, updated_at = ?2 WHERE id = ?3",
                params![status.as_db_str(), now, id],
            )?
        } else {
            tx.execute(
                "UPDATE tasks SET status = ?1, updated_at = ?2 WHERE id = ?3",
                params![status.as_db_str(), now, id],
            )?
        };
        anyhow::ensure!(n == 1, "no task {id}");
        if status == TaskStatus::Landed {
            release_operator_driven_on_landing_in_tx(&tx, id, &now)?;
        }
        if status.closes_findings() {
            resolve_task_findings_in_tx(
                &tx,
                id,
                &format!("task {id} {}", status.as_db_str()),
                &now,
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Guarded unclaimed transition: succeeds only if the task is still in
    /// `from` with no live claim — the refinery's protection against landing
    /// or bouncing a task an operator requeued and another agent reclaimed.
    /// Returns false (not an error) when the task moved on.
    pub fn transition_if(&self, id: i64, from: &str, to: &str) -> Result<bool> {
        self.transition(id, from.parse()?, to.parse()?)
    }

    /// Typed core of [`Ledger::transition_if`]: the legal unclaimed
    /// transitions are enumerated here, and anything else is a typed
    /// [`TransitionError`], never a silently-stored string.
    pub fn transition(&self, id: i64, from: TaskStatus, to: TaskStatus) -> Result<bool> {
        if !matches!(
            (from, to),
            (TaskStatus::Done, TaskStatus::Landing)
                | (TaskStatus::Landing, TaskStatus::Done)
                | (TaskStatus::Landing, TaskStatus::Bounced)
                | (TaskStatus::Landing, TaskStatus::Landed)
        ) {
            Err(TransitionError::IllegalTransition { from, to })?;
        }
        let now = Utc::now().to_rfc3339();
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let n = tx.execute(
            "UPDATE tasks SET status = ?1, updated_at = ?2
             WHERE id = ?3 AND status = ?4 AND claimed_by IS NULL",
            params![to.as_db_str(), now, id, from.as_db_str()],
        )?;
        if n == 1 && to == TaskStatus::Landed {
            release_operator_driven_on_landing_in_tx(&tx, id, &now)?;
        }
        if n == 1 && to.closes_findings() {
            resolve_task_findings_in_tx(&tx, id, &format!("task {id} {}", to.as_db_str()), &now)?;
        }
        tx.commit()?;
        Ok(n == 1)
    }

    /// Persist a lane-specific refusal so planning can advance past that rung
    /// without pretending the task failed a quality gate.
    pub fn file_rung_refusal(&self, id: i64, rung: &str, detail: &str) -> Result<bool> {
        let now = normalise_utc_timestamp(Utc::now());
        let title = format!("rung refused: {rung}");
        let inserted = self.conn.execute(
            "INSERT INTO findings
                 (task_id, severity, title, body, filed_by, reason_code, created_at)
             SELECT ?1, 'major', ?2, ?3, 'dispatch', 'rung_refusal', ?4
             WHERE EXISTS (
                 SELECT 1 FROM tasks WHERE id = ?1
                   AND status IN ('queued', 'bounced', 'failed') AND claimed_by IS NULL
             ) AND NOT EXISTS (
                 SELECT 1 FROM findings WHERE task_id = ?1 AND status = 'open'
                   AND reason_code = 'rung_refusal' AND title = ?2
             )",
            params![id, title, detail, now],
        )?;
        Ok(inserted == 1)
    }

    pub fn task_rung_refused(&self, id: i64, rung: &str) -> Result<bool> {
        let title = format!("rung refused: {rung}");
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM findings WHERE task_id = ?1 AND status = 'open'
                   AND reason_code = 'rung_refusal' AND title = ?2",
                params![id, title],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Record an infrastructure refusal, file an operator-visible finding at
    /// `finding_threshold`, and park the task at `park_threshold`.
    ///
    /// Infra refusals are harness failures (worktree provisioning, policy
    /// setup, a ledger hiccup) that are NOT the task's fault, so they must not
    /// move the escalation ladder. But a task that fails pre-claim on every
    /// tick never climbs and never parks either — it livelocks invisibly while
    /// fleet status still reads healthy. The finding is how the operator hears
    /// about it.
    ///
    /// Guarded on dispatchable states; returns `None` when the task moved on
    /// mid-failure. The increment and
    /// the finding INSERT share one Immediate transaction, so the threshold test
    /// and the already-open test cannot race a concurrent foreman. Exactly one
    /// finding is filed while it stays open; parking promotes it to blocker and
    /// replaces its body with the refusal which caused the park.
    /// A successful non-infrastructure disposition resets the consecutive
    /// count; claiming alone does not, because a post-run refusal is recorded
    /// only after that claim is released.
    /// Production passes [`infra_refusal_finding_threshold`] and
    /// [`infra_refusal_park_threshold`].
    pub fn note_infra_refusal(
        &self,
        id: i64,
        error: &anyhow::Error,
        finding_threshold: i64,
        park_threshold: i64,
    ) -> Result<Option<InfraRefusalDisposition>> {
        let now_dt = Utc::now();
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let disposition = note_infra_refusal_in_tx(
            &tx,
            id,
            &format!("{error:#}"),
            finding_threshold,
            park_threshold,
            now_dt,
        )?;
        tx.commit()?;
        Ok(disposition)
    }

    /// Claim-guarded transition to `running` — refuses if the claim was
    /// requeued away between claim and start, AND if the claim generation
    /// moved (a force-requeue reclaimed by the same claimant NAME is a
    /// different attempt; a delayed write from the old one must not land).
    /// [`Ledger::start_attempt`] is the production path; this stays for a
    /// caller that claims and starts separately.
    pub fn mark_running(&self, id: i64, token: ClaimToken<'_>) -> Result<()> {
        let claimed = TaskStatus::Claimed;
        let running = TaskStatus::Running;
        let now = Utc::now().to_rfc3339();
        let n = self.conn.execute(
            "UPDATE tasks SET status = ?1, updated_at = ?2
             WHERE id = ?3 AND claimed_by = ?4 AND attempt = ?5 AND status = ?6",
            params![
                running.as_db_str(),
                now,
                id,
                token.owner,
                token.generation,
                claimed.as_db_str()
            ],
        )?;
        anyhow::ensure!(
            n == 1,
            "task {id} is no longer claim generation {} held by {}",
            token.generation,
            token.owner
        );
        Ok(())
    }

}
