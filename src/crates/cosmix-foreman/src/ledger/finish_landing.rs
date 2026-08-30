impl Ledger {
    /// Finish a refinery landing transition with the same run-attributed,
    /// at-most-once classification used by the runner.
    pub fn finish_landing_classified(
        &self,
        id: i64,
        to: &str,
        run_id: Option<i64>,
        reason: Option<FindingReason>,
    ) -> Result<(bool, bool)> {
        self.finish_landing_classified_with_infra(
            id,
            to,
            run_id,
            reason,
            None,
            DEFAULT_INFRA_REFUSALS_FINDING,
            DEFAULT_INFRA_REFUSALS_PARK,
            DEFAULT_BRANCH_CONTRACT_LIMIT,
            None,
            Utc::now(),
        )
        .map(|disposition| (disposition.moved, disposition.charged))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn finish_landing_classified_with_infra(
        &self,
        id: i64,
        to: &str,
        run_id: Option<i64>,
        reason: Option<FindingReason>,
        infra_error: Option<&str>,
        infra_threshold: i64,
        infra_park_threshold: i64,
        branch_contract_limit: i64,
        bounce_finding: Option<(&str, &str)>,
        now_dt: chrono::DateTime<Utc>,
    ) -> Result<LandingDisposition> {
        let to: TaskStatus = to.parse()?;
        anyhow::ensure!(matches!(to, TaskStatus::Bounced | TaskStatus::Landed));
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let attempt: Option<i64> = tx
            .query_row(
                "SELECT attempt FROM tasks
                 WHERE id = ?1 AND status = 'landing' AND claimed_by IS NULL",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(attempt) = attempt else {
            tx.commit()?;
            return Ok(LandingDisposition {
                moved: false,
                charged: false,
                status: None,
            });
        };
        let charge_reason = reason.filter(|reason| {
            matches!(
                reason,
                FindingReason::VerifierRed | FindingReason::ReviewRejected
            )
        });
        let charged = match (run_id, charge_reason) {
            (Some(run_id), Some(reason)) => {
                tx.execute(
                    "UPDATE runs SET ladder_charge = 1, ladder_charge_reason = ?1
                     WHERE id = ?2 AND task_id = ?3 AND role = 'implement'
                       AND (attempt = ?4 OR (
                           attempt IS NULL AND id = (
                               SELECT id FROM runs
                               WHERE task_id = ?3 AND role = 'implement'
                               ORDER BY id DESC LIMIT 1
                           ) AND NOT EXISTS (
                               SELECT 1 FROM runs current_runs
                               WHERE current_runs.task_id = ?3
                                 AND current_runs.role = 'implement'
                                 AND current_runs.attempt IS NOT NULL
                           ) AND NOT EXISTS (
                               SELECT 1 FROM verifications current_verification
                               WHERE current_verification.task_id = ?3
                                 AND current_verification.attempt = ?4
                                 AND current_verification.run_id IS NULL
                           ) AND EXISTS (
                               SELECT 1 FROM verifications ownership_proof
                               WHERE ownership_proof.task_id = ?3
                                 AND ownership_proof.attempt = ?4
                                 AND ownership_proof.run_id = ?2
                           )
                       )) AND ladder_charge = 0",
                    params![reason.as_db_str(), run_id, id, attempt],
                )? == 1
            }
            _ => false,
        };
        let updated = tx.execute(
            "UPDATE tasks SET status = ?1, updated_at = ?2,
                    ladder_failures = ladder_failures + ?3,
                    review_rejections = review_rejections + ?4,
                    branch_contract_failures = CASE
                        WHEN ?5 = 1 THEN branch_contract_failures + 1
                        WHEN ?1 = 'landed' THEN 0
                        ELSE branch_contract_failures END,
                    dispatch_after = NULL
             WHERE id = ?6 AND status = 'landing' AND claimed_by IS NULL",
            params![
                to.as_db_str(),
                now_dt.to_rfc3339(),
                i64::from(charged),
                i64::from(charged && reason == Some(FindingReason::ReviewRejected)),
                i64::from(matches!(
                    reason,
                    Some(FindingReason::BranchContract | FindingReason::AgentReported)
                )),
                id,
            ],
        )?;
        if updated == 1 && to == TaskStatus::Landed {
            release_operator_driven_on_landing_in_tx(&tx, id, &normalise_utc_timestamp(now_dt))?;
        }
        let infra_parked = if updated == 1 && reason == Some(FindingReason::InfraRefusal) {
            let error = infra_error.unwrap_or("refinery vendor or harness failure");
            note_infra_refusal_in_tx(
                &tx,
                id,
                error,
                infra_threshold,
                infra_park_threshold,
                now_dt,
            )?
            .with_context(|| {
                format!(
                    "task {id} moved before its refinery infrastructure backoff could be recorded"
                )
            })?
            .parked
        } else if updated == 1 {
            reset_infra_refusals_in_tx(&tx, id)?;
            false
        } else {
            false
        };
        if updated == 1
            && let Some((title, body)) = bounce_finding
        {
            let finding_reason = reason.context("a recovery finding requires a reason")?;
            #[cfg(test)]
            FAIL_LANDING_FINDING_BEFORE_INSERT.with(|fail| -> Result<()> {
                anyhow::ensure!(
                    !fail.replace(false),
                    "injected landing finding write failure"
                );
                Ok(())
            })?;
            let severity = if finding_reason == FindingReason::PolicyDenied {
                "blocker"
            } else {
                "major"
            };
            tx.execute(
                "INSERT INTO findings
                     (task_id, severity, title, body, filed_by, reason_code, created_at)
                 VALUES (?1, ?2, ?3, ?4, 'refinery', ?5, ?6)",
                params![
                    id,
                    severity,
                    title,
                    body,
                    finding_reason.as_db_str(),
                    now_dt.to_rfc3339()
                ],
            )?;
        }
        let policy_parked = if updated == 1 && reason == Some(FindingReason::PolicyDenied) {
            backoff_and_park_policy_denial_in_tx(
                &tx,
                id,
                infra_error.unwrap_or("merge-review lane or credential denied by policy"),
                infra_threshold,
                now_dt,
            )?
        } else {
            false
        };
        let branch_contract_parked = if updated == 1
            && matches!(
                reason,
                Some(FindingReason::BranchContract | FindingReason::AgentReported)
            ) {
            park_repeated_branch_contract_in_tx(
                &tx,
                id,
                branch_contract_limit,
                &normalise_utc_timestamp(now_dt),
            )?
        } else {
            false
        };
        let final_status = if infra_parked || policy_parked || branch_contract_parked {
            TaskStatus::Parked
        } else {
            to
        };
        if updated == 1 && final_status.closes_findings() {
            resolve_task_findings_in_tx(
                &tx,
                id,
                &format!("task {id} {}", final_status.as_db_str()),
                &normalise_utc_timestamp(now_dt),
            )?;
        }
        if updated == 1
            && let Some(run_id) = run_id
        {
            let payload = serde_json::json!({
                "attempt": attempt,
                "status": final_status.as_db_str(),
                "reason": reason.map(|reason| reason.as_db_str()),
                "ladder_charge": i64::from(charged),
            })
            .to_string();
            tx.execute(
                "INSERT INTO events (run_id, seq, kind, payload, at)
                 SELECT ?1, COALESCE(MAX(seq), -1) + 1, 'disposition', ?2, ?3
                 FROM events WHERE run_id = ?1",
                params![run_id, payload, now_dt.to_rfc3339()],
            )?;
        }
        tx.commit()?;
        Ok(LandingDisposition {
            moved: updated == 1,
            charged,
            status: (updated == 1).then_some(final_status),
        })
    }

    /// Release a run whose ledger retry budget was exhausted back to the
    /// dispatchable queue without charging the escalation ladder. This is a
    /// harness failure, not an agent verdict. The claim generation guard is
    /// the same fence used by normal completion.
    pub(crate) fn finish_infrastructure_failure_at(
        &self,
        id: i64,
        token: ClaimToken<'_>,
        now: &str,
    ) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE tasks SET status = ?1, claimed_by = NULL, lease_until = NULL,
                    claim_pid = NULL, claimed_at = NULL, updated_at = ?2
             WHERE id = ?3 AND claimed_by = ?4 AND attempt = ?5",
            params![
                TaskStatus::Queued.as_db_str(),
                now,
                id,
                token.owner,
                token.generation
            ],
        )?;
        anyhow::ensure!(
            n == 1,
            "task {id} is no longer claim generation {} held by {}; \
             refusing to disposition its infrastructure failure",
            token.generation,
            token.owner
        );
        Ok(())
    }

    /// Release a dirty Claude/GLM run which ended while background Bash was
    /// still live. This mechanism is not a task-quality verdict, so it has a
    /// separate bounded counter and never increments `ladder_failures`: the
    /// first occurrence requeues with one diagnostic for the next prompt; a
    /// consecutive repeat parks with that same finding promoted to blocker.
    /// The claim release, counter and finding are one transaction so a crash
    /// cannot strand the task between them.
    pub(crate) fn finish_abandoned_background_at(
        &self,
        id: i64,
        token: ClaimToken<'_>,
        evidence: &str,
        now: &str,
    ) -> Result<(i64, bool)> {
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let updated = tx.execute(
            "UPDATE tasks SET
                    background_abandonments = background_abandonments + 1,
                    status = CASE
                        WHEN background_abandonments + 1 >= ?1 THEN ?2
                        ELSE ?3
                    END,
                    claimed_by = NULL,
                    lease_until = NULL,
                    claim_pid = NULL,
                    claimed_at = NULL,
                    updated_at = ?4
             WHERE id = ?5 AND claimed_by = ?6 AND attempt = ?7",
            params![
                ABANDONED_BACKGROUND_LIMIT,
                TaskStatus::Parked.as_db_str(),
                TaskStatus::Queued.as_db_str(),
                now,
                id,
                token.owner,
                token.generation
            ],
        )?;
        anyhow::ensure!(
            updated == 1,
            "task {id} is no longer claim generation {} held by {}; refusing to disposition \
             its abandoned background task",
            token.generation,
            token.owner
        );
        reset_infra_refusals_in_tx(&tx, id)?;
        let count: i64 = tx.query_row(
            "SELECT background_abandonments FROM tasks WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )?;
        let parked = count >= ABANDONED_BACKGROUND_LIMIT;
        let body = format!(
            "Claude Code used background Bash in a single-turn `claude -p` run. Ending the \
             final response ended the session, so there was no next turn to receive the \
             completion notification and uncommitted work was abandoned. Never use \
             `run_in_background` here: run gates in the foreground with an explicit timeout \
             and commit all work before the final response.\n\nDriver evidence: {evidence}"
        );
        tx.execute(
            "INSERT INTO findings
                 (task_id, severity, title, body, filed_by, reason_code, created_at)
             SELECT ?1, 'major', ?2, ?3, 'runner', ?4, ?5
             WHERE NOT EXISTS (
                 SELECT 1 FROM findings
                 WHERE task_id = ?1 AND status = 'open'
                   AND reason_code = ?4
             )",
            params![
                id,
                "agent abandoned background Bash in a headless session",
                body,
                FindingReason::AgentAbandonedBackground.as_db_str(),
                now
            ],
        )?;
        if parked {
            tx.execute(
                "UPDATE findings SET severity = 'blocker',
                     body = body || ?1
                 WHERE task_id = ?2 AND status = 'open'
                   AND reason_code = ?3",
                params![
                    format!(
                        "\n\nThe mechanism repeated {count} consecutive times. The task is parked without \
                         charging the escalation ladder; an operator must requeue it."
                    ),
                    id,
                    FindingReason::AgentAbandonedBackground.as_db_str()
                ],
            )?;
        }
        tx.commit()?;
        Ok((count, parked))
    }

}
