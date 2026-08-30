impl Ledger {
    /// Terminal transition guarded by claimant: only the agent holding the
    /// claim may disposition the task. Fails if the claim changed hands
    /// (e.g. an operator requeued mid-run and another agent claimed it).
    pub fn finish_task(&self, id: i64, claimant: &str, status: &str) -> Result<()> {
        let status: TaskStatus = status.parse()?;
        ensure_worker_disposition(status)?;
        let now = Utc::now().to_rfc3339();
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let n = tx.execute(
            "UPDATE tasks SET status = ?1, claimed_by = NULL, lease_until = NULL,
                    claim_pid = NULL, claimed_at = NULL, updated_at = ?2,
                    background_abandonments = 0
             WHERE id = ?3 AND claimed_by = ?4 AND status IN (?5, ?6)",
            params![
                status.as_db_str(),
                now,
                id,
                claimant,
                TaskStatus::Claimed.as_db_str(),
                TaskStatus::Running.as_db_str()
            ],
        )?;
        anyhow::ensure!(
            n == 1,
            "task {id} is no longer claimed by {claimant}; refusing to disposition it"
        );
        reset_infra_refusals_in_tx(&tx, id)?;
        tx.commit()?;
        Ok(())
    }

    /// Like `finish_task`, but additionally requires the claim's attempt
    /// generation to match. A delayed old attempt cannot disposition a new
    /// same-name claim.
    pub fn finish_task_claimed(&self, id: i64, token: ClaimToken<'_>, status: &str) -> Result<()> {
        self.finish_claimed(id, token, status.parse()?)
    }

    /// Typed core of [`Ledger::finish_task_claimed`].
    pub fn finish_claimed(&self, id: i64, token: ClaimToken<'_>, status: TaskStatus) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.finish_claimed_at(id, token, status, &now)
    }

    fn finish_claimed_at(
        &self,
        id: i64,
        token: ClaimToken<'_>,
        status: TaskStatus,
        now: &str,
    ) -> Result<()> {
        ensure_worker_disposition(status)?;
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let n = tx.execute(
            "UPDATE tasks SET status = ?1, claimed_by = NULL, lease_until = NULL,
                    claim_pid = NULL, claimed_at = NULL, updated_at = ?2,
                    background_abandonments = 0
             WHERE id = ?3 AND claimed_by = ?4 AND attempt = ?5
               AND status IN (?6, ?7)",
            params![
                status.as_db_str(),
                now,
                id,
                token.owner,
                token.generation,
                TaskStatus::Claimed.as_db_str(),
                TaskStatus::Running.as_db_str()
            ],
        )?;
        anyhow::ensure!(
            n == 1,
            "task {id} is no longer claim generation {} held by {}; \
             refusing to disposition it",
            token.generation,
            token.owner
        );
        reset_infra_refusals_in_tx(&tx, id)?;
        tx.commit()?;
        Ok(())
    }

    /// Classified MCP self-bounce. MCP claims have no run row, so this path
    /// cannot carry a quality charge; it shares the bounded consecutive
    /// branch-contract counter because repeated self-bounces are the same
    /// handoff/branch-completion loop from the scheduler's point of view.
    pub fn finish_agent_bounce(
        &self,
        id: i64,
        claimant: &str,
        generation: i64,
        detail: &str,
        branch_contract_limit: i64,
    ) -> Result<bool> {
        let now = normalise_utc_timestamp(Utc::now());
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let updated = tx.execute(
            "UPDATE tasks SET status = 'bounced', claimed_by = NULL, lease_until = NULL,
                    claim_pid = NULL, claimed_at = NULL, updated_at = ?1,
                    branch_contract_failures = branch_contract_failures + 1,
                    background_abandonments = 0
             WHERE id = ?2 AND claimed_by = ?3
               AND attempt = ?4
               AND status IN ('claimed', 'running')",
            params![now, id, claimant, generation],
        )?;
        anyhow::ensure!(
            updated == 1,
            "task {id} is no longer claim generation {generation} held by {claimant}; \
             refusing to bounce it"
        );
        reset_infra_refusals_in_tx(&tx, id)?;
        tx.execute(
            "INSERT INTO findings
                 (task_id, severity, title, body, filed_by, reason_code, created_at)
             VALUES (?1, 'info', ?2, ?3, ?4, 'agent_reported', ?5)",
            params![id, format!("bounced by {claimant}"), detail, claimant, now],
        )?;
        let parked = park_repeated_branch_contract_in_tx(&tx, id, branch_contract_limit, &now)?;
        tx.commit()?;
        Ok(parked)
    }

    /// Disposition one claimed implementation attempt and attach its
    /// classification atomically. Only runnable verifier failures and real
    /// review rejections charge; transition names have no scoring meaning.
    /// The run-row guard makes the operation at-most-once even if a future
    /// caller attempts to classify the same generation twice.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn finish_task_classified_at(
        &self,
        id: i64,
        token: ClaimToken<'_>,
        run_id: i64,
        status: &str,
        reason: Option<FindingReason>,
        infra_error: Option<&str>,
        infra_threshold: i64,
        infra_park_threshold: i64,
        branch_contract_limit: i64,
        now: &str,
    ) -> Result<ClassifiedDisposition> {
        // Stands in for a run-ending REPORTING write that fails for a reason
        // SQLite's busy retry cannot help with (a disk error, a constraint,
        // a corrupt row). Nothing commits, so the caller is left exactly
        // where run 425 was: the run is over and the claim is still held.
        #[cfg(test)]
        FAIL_NEXT_TASK_DISPOSITION_WRITE.with(|fail| -> Result<()> {
            anyhow::ensure!(
                !fail.replace(false),
                "injected task disposition write failure"
            );
            Ok(())
        })?;
        let status: TaskStatus = status.parse()?;
        ensure_worker_disposition(status)?;
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let owned: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM runs
                 WHERE id = ?1 AND task_id = ?2 AND attempt = ?3 AND role = 'implement'",
                params![run_id, id, token.generation],
                |row| row.get(0),
            )
            .optional()?;
        anyhow::ensure!(
            owned.is_some(),
            "run {run_id} is not implementation attempt {} for task {id}",
            token.generation
        );

        let charge_reason = reason.filter(|reason| {
            matches!(
                reason,
                FindingReason::VerifierRed | FindingReason::ReviewRejected
            )
        });
        let charged = if let Some(reason) = charge_reason {
            tx.execute(
                "UPDATE runs SET ladder_charge = 1, ladder_charge_reason = ?1
                 WHERE id = ?2 AND ladder_charge = 0",
                params![reason.as_db_str(), run_id],
            )? == 1
        } else {
            false
        };
        let branch_contract = i64::from(matches!(
            reason,
            Some(FindingReason::BranchContract | FindingReason::AgentReported)
        ));
        let review_rejected = i64::from(charged && reason == Some(FindingReason::ReviewRejected));
        let updated = tx.execute(
            "UPDATE tasks SET status = ?1, claimed_by = NULL, lease_until = NULL,
                    claim_pid = NULL, claimed_at = NULL, updated_at = ?2,
                    ladder_failures = ladder_failures + ?3,
                    review_rejections = review_rejections + ?4,
                    branch_contract_failures = branch_contract_failures + ?5,
                    dispatch_after = NULL,
                    background_abandonments = 0
             WHERE id = ?6 AND claimed_by = ?7 AND attempt = ?8
               AND status IN (?9, ?10)",
            params![
                status.as_db_str(),
                now,
                i64::from(charged),
                review_rejected,
                branch_contract,
                id,
                token.owner,
                token.generation,
                TaskStatus::Claimed.as_db_str(),
                TaskStatus::Running.as_db_str(),
            ],
        )?;
        anyhow::ensure!(
            updated == 1,
            "task {id} is no longer claim generation {} held by {}; refusing to disposition it",
            token.generation,
            token.owner
        );
        let infra_parked = if reason == Some(FindingReason::InfraRefusal) {
            let now_dt = chrono::DateTime::parse_from_rfc3339(now)
                .context("classified disposition timestamp is not RFC3339")?
                .with_timezone(&Utc);
            let error = infra_error.unwrap_or("vendor or harness failure");
            note_infra_refusal_in_tx(
                &tx,
                id,
                error,
                infra_threshold,
                infra_park_threshold,
                now_dt,
            )?
            .with_context(|| {
                format!("task {id} moved before its infrastructure backoff could be recorded")
            })?
            .parked
        } else {
            reset_infra_refusals_in_tx(&tx, id)?;
            false
        };
        let branch_contract_parked = branch_contract == 1
            && park_repeated_branch_contract_in_tx(&tx, id, branch_contract_limit, now)?;
        let final_status = if infra_parked || branch_contract_parked {
            TaskStatus::Parked
        } else {
            status
        };
        let payload = serde_json::json!({
            "attempt": token.generation,
            "status": final_status.as_db_str(),
            "reason": reason.map(|reason| reason.as_db_str()),
            "ladder_charge": i64::from(charged),
        })
        .to_string();
        tx.execute(
            "INSERT INTO events (run_id, seq, kind, payload, at)
             SELECT ?1, COALESCE(MAX(seq), -1) + 1, 'disposition', ?2, ?3
             FROM events WHERE run_id = ?1",
            params![run_id, payload, now],
        )?;
        tx.commit()?;
        Ok(ClassifiedDisposition {
            charged,
            status: final_status,
        })
    }

    pub fn finish_task_classified(
        &self,
        id: i64,
        token: ClaimToken<'_>,
        run_id: i64,
        status: &str,
        reason: Option<FindingReason>,
    ) -> Result<bool> {
        self.finish_task_classified_at(
            id,
            token,
            run_id,
            status,
            reason,
            None,
            DEFAULT_INFRA_REFUSALS_FINDING,
            DEFAULT_INFRA_REFUSALS_PARK,
            DEFAULT_BRANCH_CONTRACT_LIMIT,
            &Utc::now().to_rfc3339(),
        )
        .map(|disposition| disposition.charged)
    }

}
