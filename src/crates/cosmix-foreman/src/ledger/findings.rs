impl Ledger {
    /// Legacy 5-arg signature, kept byte-for-byte so `policy.rs` — one of
    /// foreman's own gates, which no agent (including the one implementing
    /// this) may edit — keeps compiling untouched. Every remaining caller
    /// of this exact shape is, structurally, a policy-gate verdict (the
    /// only caller left after every other site moved to
    /// `file_finding_reasoned`), so the reason code is fixed at
    /// `PolicyDenied` here rather than threaded through a call site that
    /// cannot be changed to state it explicitly.
    pub fn file_finding(
        &self,
        task_id: Option<i64>,
        severity: &str,
        title: &str,
        body: &str,
        filed_by: &str,
    ) -> Result<i64> {
        self.file_finding_reasoned(
            task_id,
            severity,
            title,
            body,
            filed_by,
            FindingReason::PolicyDenied,
        )
    }

    /// File a finding with an explicit, shell-owned reason code. Prose
    /// (`title`/`body`) is for humans and the next agent; `reason` is what
    /// any future routing, scoring, or automation reads.
    pub fn file_finding_reasoned(
        &self,
        task_id: Option<i64>,
        severity: &str,
        title: &str,
        body: &str,
        filed_by: &str,
        reason: FindingReason,
    ) -> Result<i64> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO findings (task_id, severity, title, body, filed_by, reason_code, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![task_id, severity, title, body, filed_by, reason.as_db_str(), now],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Atomically persist one complete merge-review batch: every typed finding,
    /// the tier-3 verdict evidence, and each arm's quality disposition. A crash
    /// or failed write therefore leaves none of the batch, rather than orphaned
    /// findings which a recovery retry would duplicate.
    pub fn record_review_verification(
        &self,
        task_id: i64,
        implementation_run: Option<i64>,
        pass: bool,
        report: &str,
        arms: &[ReviewRunRecord],
        findings: &[ReviewFindingInsert],
    ) -> Result<(i64, Vec<i64>)> {
        anyhow::ensure!(!arms.is_empty(), "review batch has no arms");
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let now = Utc::now().to_rfc3339();
        let mut arm_ids = HashSet::with_capacity(arms.len());
        for arm in arms {
            anyhow::ensure!(
                arm_ids.insert(arm.run_id),
                "review batch repeats run {}",
                arm.run_id
            );
            let run_owns_task: Option<i64> = tx
                .query_row(
                    "SELECT 1 FROM runs WHERE id = ?1 AND task_id = ?2 AND role = 'review'",
                    params![arm.run_id, task_id],
                    |row| row.get(0),
                )
                .optional()?;
            anyhow::ensure!(
                run_owns_task.is_some(),
                "review run {} does not belong to task {task_id}",
                arm.run_id
            );
        }

        if let Some(run_id) = implementation_run {
            let run_owns_task: Option<i64> = tx
                .query_row(
                    "SELECT 1 FROM runs WHERE id = ?1 AND task_id = ?2 AND role = 'implement'",
                    params![run_id, task_id],
                    |row| row.get(0),
                )
                .optional()?;
            anyhow::ensure!(
                run_owns_task.is_some(),
                "implementation run {run_id} does not belong to task {task_id}"
            );
        }

        let mut ids = Vec::with_capacity(findings.len());
        for finding in findings {
            anyhow::ensure!(
                matches!(
                    finding.severity.as_str(),
                    "blocker" | "major" | "minor" | "nit"
                ),
                "invalid review finding severity {:?}",
                finding.severity
            );
            anyhow::ensure!(
                !finding.file.is_empty(),
                "review finding file must not be empty"
            );
            anyhow::ensure!(finding.line > 0, "review finding line must be positive");
            let line = i64::try_from(finding.line).context("review finding line exceeds i64")?;
            anyhow::ensure!(
                arm_ids.contains(&finding.run_id),
                "review finding names run {} outside this batch",
                finding.run_id
            );
            tx.execute(
                "INSERT INTO findings
                 (task_id, severity, title, body, filed_by, reason_code, run_id, file, line, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    task_id,
                    finding.severity,
                    finding.title,
                    finding.body,
                    finding.filed_by,
                    FindingReason::ReviewFinding.as_db_str(),
                    finding.run_id,
                    finding.file,
                    line,
                    now,
                ],
            )?;
            ids.push(tx.last_insert_rowid());
        }

        let owning_run = (arms.len() == 1).then_some(arms[0].run_id);
        let inserted = tx.execute(
            "INSERT INTO verifications (task_id, run_id, attempt, tier, pass, report, at)
             SELECT id, ?2, attempt, 3, ?3, ?4, ?5 FROM tasks WHERE id = ?1",
            params![task_id, owning_run, pass, report, now],
        )?;
        anyhow::ensure!(inserted == 1, "no task {task_id}");
        let verification_id = tx.last_insert_rowid();
        for arm in arms.iter().filter(|arm| arm.delivered) {
            tx.execute(
                "UPDATE runs SET quality = ?1 WHERE id = ?2",
                params![
                    if arm.approve {
                        "review_approved"
                    } else {
                        "review_rejected"
                    },
                    arm.run_id
                ],
            )?;
        }
        let implementation_quality = if pass {
            arms.iter()
                .all(|arm| arm.delivered && arm.approve)
                .then_some("review_approved")
        } else {
            arms.iter()
                .any(|arm| arm.delivered && !arm.approve)
                .then_some("review_rejected")
        };
        if let (Some(run_id), Some(quality)) = (implementation_run, implementation_quality) {
            tx.execute(
                "UPDATE runs SET quality = ?1 WHERE id = ?2",
                params![quality, run_id],
            )?;
        }
        tx.commit()?;
        Ok((verification_id, ids))
    }

    /// Atomically file one informational finding per recovered sccache
    /// incident in a verifier report. A caller may retry this whole method on
    /// SQLITE_BUSY without duplicating an earlier item from the same report.
    pub fn file_sccache_bypass_findings(
        &self,
        task_id: i64,
        bodies: &[String],
        filed_by: &str,
    ) -> Result<Vec<i64>> {
        if bodies.is_empty() {
            return Ok(Vec::new());
        }
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let now = Utc::now().to_rfc3339();
        let mut ids = Vec::with_capacity(bodies.len());
        for body in bodies {
            tx.execute(
                "INSERT INTO findings (task_id, severity, title, body, filed_by, reason_code, created_at)
                 VALUES (?1, 'info', 'sccache bypassed during verifier step', ?2, ?3, ?4, ?5)",
                params![
                    task_id,
                    body,
                    filed_by,
                    FindingReason::SccacheBypassed.as_db_str(),
                    now
                ],
            )?;
            ids.push(tx.last_insert_rowid());
        }
        tx.commit()?;
        Ok(ids)
    }

    /// Claim-generation-fenced form for the runner. If an operator requeues
    /// and reclaims while tier 0 is executing, the stale verifier cannot
    /// attach its incident to the new attempt.
    pub fn file_sccache_bypass_findings_claimed(
        &self,
        task_id: i64,
        token: ClaimToken<'_>,
        bodies: &[String],
        filed_by: &str,
    ) -> Result<Vec<i64>> {
        if bodies.is_empty() {
            return Ok(Vec::new());
        }
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let held: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM tasks WHERE id = ?1 AND claimed_by = ?2 AND attempt = ?3
                 AND status IN (?4, ?5)",
                params![
                    task_id,
                    token.owner,
                    token.generation,
                    TaskStatus::Claimed.as_db_str(),
                    TaskStatus::Running.as_db_str()
                ],
                |row| row.get(0),
            )
            .optional()?;
        anyhow::ensure!(
            held.is_some(),
            "task {task_id} changed hands while the verifier ran — sccache finding discarded"
        );
        let now = Utc::now().to_rfc3339();
        let mut ids = Vec::with_capacity(bodies.len());
        for body in bodies {
            tx.execute(
                "INSERT INTO findings (task_id, severity, title, body, filed_by, reason_code, created_at)
                 VALUES (?1, 'info', 'sccache bypassed during verifier step', ?2, ?3, ?4, ?5)",
                params![
                    task_id,
                    body,
                    filed_by,
                    FindingReason::SccacheBypassed.as_db_str(),
                    now
                ],
            )?;
            ids.push(tx.last_insert_rowid());
        }
        tx.commit()?;
        Ok(ids)
    }

    /// Open findings, newest first: (id, task_id, severity, title, body).
    #[allow(clippy::type_complexity)]
    pub fn open_findings(
        &self,
        limit: i64,
    ) -> Result<Vec<(i64, Option<i64>, String, String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, severity, title, body FROM findings
             WHERE status = 'open' ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

}
