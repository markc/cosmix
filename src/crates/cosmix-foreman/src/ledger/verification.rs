impl Ledger {
    /// The whole completion decision in one IMMEDIATE transaction: re-check
    /// the claim (claimant AND attempt — a forced requeue + reclaim makes
    /// this verifier result a dead attempt's), record the verification, and
    /// on green record the workspace and finish the task. Cross-process
    /// safe, unlike a check in one foreman process's memory. The task's
    /// ledger-recorded branch is deliberately preserved; completion has no
    /// branch parameter and therefore cannot redirect later cleanup.
    /// Returns true when the task was completed (green + claim intact);
    /// false when the verification was red (recorded, task stays claimed).
    #[allow(clippy::too_many_arguments)]
    pub fn complete_verified(
        &self,
        id: i64,
        token: ClaimToken<'_>,
        workdir: &str,
        report: &str,
        pass: bool,
        sccache_bypass_bodies: &[String],
    ) -> Result<bool> {
        let claimed = TaskStatus::Claimed;
        let running = TaskStatus::Running;
        let done = TaskStatus::Done;
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let now = Utc::now().to_rfc3339();
        let held: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM tasks WHERE id = ?1 AND claimed_by = ?2 AND attempt = ?3
                 AND status IN (?4, ?5)",
                params![
                    id,
                    token.owner,
                    token.generation,
                    claimed.as_db_str(),
                    running.as_db_str()
                ],
                |r| r.get(0),
            )
            .optional()?;
        anyhow::ensure!(
            held.is_some(),
            "task {id} changed hands while the verifier ran — result discarded"
        );
        tx.execute(
            "INSERT INTO verifications (task_id, attempt, tier, pass, report, at)
             VALUES (?1, ?2, 0, ?3, ?4, ?5)",
            params![id, token.generation, pass, report, now],
        )?;
        for body in sccache_bypass_bodies {
            tx.execute(
                "INSERT INTO findings (task_id, severity, title, body, filed_by, reason_code, created_at)
                 VALUES (?1, 'info', 'sccache bypassed during verifier step', ?2, 'mcp', ?3, ?4)",
                params![id, body, FindingReason::SccacheBypassed.as_db_str(), now],
            )?;
        }
        if pass {
            tx.execute(
                "UPDATE tasks SET status = ?1, claimed_by = NULL, lease_until = NULL,
                        claim_pid = NULL, claimed_at = NULL, worktree = ?2,
                        updated_at = ?3 WHERE id = ?4",
                params![done.as_db_str(), workdir, now, id],
            )?;
            reset_infra_refusals_in_tx(&tx, id)?;
        }
        tx.commit()?;
        Ok(pass)
    }

    /// Recent verifications, newest first: (task_id, tier, pass, at).
    /// Tier 3 = merge-authority review.
    pub fn recent_verifications(&self, limit: i64) -> Result<Vec<(i64, i64, bool, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT task_id, tier, pass, at FROM verifications ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Latest verification report for a task, if any.
    pub fn latest_verification(&self, task_id: i64) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT report FROM verifications WHERE task_id = ?1 ORDER BY id DESC LIMIT 1",
                params![task_id],
                |r| r.get(0),
            )
            .optional()
            .context("loading verification")
    }

    /// One task's OPEN findings, newest first: (id, severity, title, body).
    pub fn task_findings(&self, task_id: i64) -> Result<Vec<(i64, String, String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, severity, title, body FROM findings
             WHERE task_id = ?1 AND status = 'open' ORDER BY id DESC",
        )?;
        let rows = stmt.query_map(params![task_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// One task's open findings with structured source evidence.
    pub fn task_findings_detailed(&self, task_id: i64) -> Result<Vec<StoredFinding>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, severity, file, line, title, body, run_id FROM findings
             WHERE task_id = ?1 AND status = 'open' ORDER BY id DESC",
        )?;
        let rows = stmt.query_map(params![task_id], |row| {
            let line: Option<i64> = row.get(3)?;
            Ok(StoredFinding {
                id: row.get(0)?,
                severity: row.get(1)?,
                file: row.get(2)?,
                line: line.and_then(|value| u64::try_from(value).ok()),
                title: row.get(4)?,
                body: row.get(5)?,
                run_id: row.get(6)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn task_has_open_finding_reason(
        &self,
        task_id: i64,
        reason: FindingReason,
    ) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM findings
                 WHERE task_id = ?1 AND status = 'open' AND reason_code = ?2
                 ORDER BY id DESC LIMIT 1",
                params![task_id, reason.as_db_str()],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .is_some())
    }

    pub fn resolve_task_findings_reason(
        &self,
        task_id: i64,
        reason: FindingReason,
    ) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE findings SET status = 'resolved'
             WHERE task_id = ?1 AND status = 'open' AND reason_code = ?2",
            params![task_id, reason.as_db_str()],
        )?)
    }

    /// A task's recent verification reports, newest first.
    pub fn verification_reports(&self, task_id: i64, limit: i64) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT report FROM verifications WHERE task_id = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![task_id, limit], |r| r.get(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// A task attempt's recent verification reports, newest first.
    ///
    /// Landing recovery uses this structural generation fence. Rows from
    /// before schema version 4 have a NULL attempt and are intentionally not
    /// returned: their producing attempt cannot be reconstructed safely.
    pub fn verification_reports_for_attempt(
        &self,
        task_id: i64,
        attempt: i64,
        limit: i64,
    ) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT report FROM verifications
             WHERE task_id = ?1 AND attempt = ?2
             ORDER BY id DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![task_id, attempt, limit], |r| r.get(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// One attempt's merge-review evidence only, newest first. Recovery and
    /// refinery retry use this narrower stream instead of scanning unrelated
    /// verifier rows.
    pub fn review_verification_reports_for_attempt(
        &self,
        task_id: i64,
        attempt: i64,
    ) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT report FROM verifications
             WHERE task_id = ?1 AND attempt = ?2 AND tier = 3
             ORDER BY id DESC",
        )?;
        let rows = stmt.query_map(params![task_id, attempt], |row| row.get(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Tasks stuck in the refinery's in-flight state (crash recovery).
    pub fn landing_tasks(&self) -> Result<Vec<Task>> {
        let mut stmt = self
            .conn
            .prepare("SELECT * FROM tasks WHERE status = 'landing' ORDER BY id")?;
        let rows = stmt.query_map([], row_to_task)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn record_verification(
        &self,
        task_id: i64,
        tier: i64,
        pass: bool,
        report: &str,
    ) -> Result<i64> {
        let now = Utc::now().to_rfc3339();
        let n = self.conn.execute(
            "INSERT INTO verifications (task_id, attempt, tier, pass, report, at)
             SELECT id, attempt, ?2, ?3, ?4, ?5 FROM tasks WHERE id = ?1",
            params![task_id, tier, pass, report, now],
        )?;
        anyhow::ensure!(n == 1, "no task {task_id}");
        Ok(self.conn.last_insert_rowid())
    }

    /// Record gate evidence and advance the quality dimension on the exact
    /// run that produced the work. Keeping run_id and attempt on the
    /// verification makes the earlier tier results available after the run's
    /// summary quality advances to a later gate while fencing landing recovery
    /// to the task's current attempt.
    pub fn record_run_verification(
        &self,
        task_id: i64,
        run_id: i64,
        tier: i64,
        pass: bool,
        report: &str,
    ) -> Result<i64> {
        let quality = match (tier, pass) {
            (0, true) => "tier_0_passed",
            (0, false) => "tier_0_failed",
            (1, true) => "tier_1_passed",
            (1, false) => "tier_1_failed",
            (2, true) => "tier_2_passed",
            (2, false) => "tier_2_failed",
            (3, true) => "review_approved",
            (3, false) => "review_rejected",
            _ => anyhow::bail!("unsupported verification tier {tier}"),
        };
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let owned: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM runs WHERE id = ?1 AND task_id = ?2",
                params![run_id, task_id],
                |r| r.get(0),
            )
            .optional()?;
        anyhow::ensure!(
            owned.is_some(),
            "run {run_id} does not belong to task {task_id}"
        );
        let now = Utc::now().to_rfc3339();
        let inserted = tx.execute(
            "INSERT INTO verifications (task_id, run_id, attempt, tier, pass, report, at)
             SELECT id, ?2, attempt, ?3, ?4, ?5, ?6 FROM tasks WHERE id = ?1",
            params![task_id, run_id, tier, pass, report, now],
        )?;
        anyhow::ensure!(inserted == 1, "no task {task_id}");
        let verification_id = tx.last_insert_rowid();
        tx.execute(
            "UPDATE runs SET quality = ?1 WHERE id = ?2",
            params![quality, run_id],
        )?;
        tx.commit()?;
        Ok(verification_id)
    }

}
