impl Ledger {
    pub fn task(&self, id: i64) -> Result<Option<Task>> {
        self.conn
            .query_row(
                "SELECT * FROM tasks WHERE id = ?1",
                params![id],
                row_to_task,
            )
            .optional()
            .context("loading task")
    }

    /// Return the reservation available to the next attempt of a budgeted
    /// task. Known costs consume their actual amount. A run without a dollar
    /// figure is delivery-void evidence, not free headroom, so it consumes
    /// the amount that attempt reserved. Legacy unpriced rows from before the
    /// reservation was recorded consume the full remainder conservatively.
    pub fn task_budget_remainder(&self, id: i64) -> Result<Option<TaskBudgetRemainder>> {
        let budget_usd = self
            .conn
            .query_row(
                "SELECT budget_usd FROM tasks WHERE id = ?1",
                params![id],
                |r| r.get::<_, Option<f64>>(0),
            )
            .optional()
            .context("loading task budget")?
            .flatten();
        let Some(limit_usd) = budget_usd else {
            return Ok(None);
        };
        anyhow::ensure!(
            limit_usd.is_finite() && limit_usd > 0.0,
            "task {id} has invalid budget_usd {limit_usd}"
        );
        let mut stmt = self.conn.prepare(
            "SELECT cost_usd, reserved_usd FROM runs
             WHERE task_id = ?1 AND role = 'implement' ORDER BY id",
        )?;
        let costs = stmt.query_map(params![id], |r| {
            Ok((r.get::<_, Option<f64>>(0)?, r.get::<_, Option<f64>>(1)?))
        })?;
        let mut charged_usd = 0.0;
        for cost in costs {
            let remaining_usd = (limit_usd - charged_usd).max(0.0);
            if remaining_usd == 0.0 {
                break;
            }
            let (actual_usd, reserved_usd) = cost?;
            if let Some(reserved_usd) = reserved_usd {
                anyhow::ensure!(
                    reserved_usd.is_finite() && reserved_usd >= 0.0,
                    "task {id} has invalid run reserved_usd {reserved_usd}"
                );
            }
            let charge = actual_usd
                .unwrap_or_else(|| reserved_usd.unwrap_or(remaining_usd).min(remaining_usd));
            anyhow::ensure!(
                charge.is_finite() && charge >= 0.0,
                "task {id} has invalid run cost_usd {charge}"
            );
            charged_usd += charge;
        }
        Ok(Some(TaskBudgetRemainder {
            limit_usd,
            charged_usd,
            remaining_usd: (limit_usd - charged_usd).max(0.0),
        }))
    }

    pub fn tasks(&self, status: Option<&str>, all: bool) -> Result<Vec<Task>> {
        let mut out = Vec::new();
        match status {
            Some(s) => {
                let mut stmt = self
                    .conn
                    .prepare("SELECT * FROM tasks WHERE status = ?1 ORDER BY id")?;
                let rows = stmt.query_map(params![s], row_to_task)?;
                for row in rows {
                    out.push(row?);
                }
            }
            None => {
                let mut stmt = self.conn.prepare("SELECT * FROM tasks ORDER BY id")?;
                let rows = stmt.query_map([], row_to_task)?;
                for row in rows {
                    let t = row?;
                    if !all {
                        match t.status.parse::<TaskStatus>() {
                            Ok(TaskStatus::Retired) | Err(TransitionError::UnknownStatus(_)) => {
                                continue;
                            }
                            Ok(_) => {}
                            Err(error) => return Err(error.into()),
                        }
                    }
                    out.push(t);
                }
            }
        }
        Ok(out)
    }

    /// Claimable unattended work, oldest first: queued/bounced/failed,
    /// unclaimed, every dep done or landed, and not operator-driven. The
    /// shared picker behind MCP `task_next` and the dispatcher.
    pub fn ready_tasks(&self, kind: Option<&str>) -> Result<Vec<Task>> {
        self.ready_tasks_at(kind, Utc::now())
    }

    /// Clock-injected admission used by replay and deterministic scheduling
    /// tests. The supplied wall time, not the host clock, decides whether an
    /// infrastructure backoff has expired.
    pub fn ready_tasks_at(
        &self,
        kind: Option<&str>,
        now: chrono::DateTime<Utc>,
    ) -> Result<Vec<Task>> {
        self.tasks_by_dispatch_flag_at(kind, false, now)
    }

    /// Otherwise-ready work reserved for the explicit operator-run path.
    /// Dispatch uses this only to explain its queue accurately.
    pub fn operator_driven_tasks(&self, kind: Option<&str>) -> Result<Vec<Task>> {
        self.tasks_by_dispatch_flag_at(kind, true, Utc::now())
    }

    /// Current reservation audit state for status/board consumers. New
    /// reservations use the structured reason code; the narrow prose match
    /// recognises the two pre-existing hand-filed reservation conventions so
    /// they are not falsely labelled unexplained.
    pub fn operator_driven_statuses(&self) -> Result<Vec<OperatorDrivenStatus>> {
        let mut stmt = self.conn.prepare(
            "SELECT task.id,
                    EXISTS (
                        SELECT 1 FROM findings finding
                         WHERE finding.task_id = task.id AND (
                            finding.reason_code = 'operator_reserved'
                            OR (finding.reason_code IN ('operator', 'unknown') AND (
                                lower(finding.title) LIKE '%reserv%'
                                OR lower(finding.body) LIKE '%reserved operator-driven%'
                            ))
                         )
                    )
             FROM tasks task
             WHERE task.operator_driven = 1
               AND task.status NOT IN ('landed', 'retired')
             ORDER BY task.id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(OperatorDrivenStatus {
                task_id: row.get(0)?,
                reservation_explained: row.get(1)?,
            })
        })?;
        let mut statuses = Vec::new();
        for row in rows {
            statuses.push(row?);
        }
        Ok(statuses)
    }

    /// Active reserved task ids for which no reservation reason can be found.
    pub fn unexplained_operator_driven_task_ids(&self) -> Result<Vec<i64>> {
        Ok(self
            .operator_driven_statuses()?
            .into_iter()
            .filter(|status| !status.reservation_explained)
            .map(|status| status.task_id)
            .collect())
    }

    fn tasks_by_dispatch_flag_at(
        &self,
        kind: Option<&str>,
        operator_driven: bool,
        now: chrono::DateTime<Utc>,
    ) -> Result<Vec<Task>> {
        let all = self.tasks(None, true)?;
        // Statuses are decoded through the typed vocabulary, not compared as
        // strings: a row carrying a status nobody writes is corruption, and
        // treating it as "not done" would fail OPEN (dispatch a task whose
        // dependency state is unknown).
        let mut decoded = Vec::with_capacity(all.len());
        for t in all {
            match t.status.parse::<TaskStatus>() {
                Ok(status) => {
                    decoded.push((t, status));
                }
                Err(TransitionError::UnknownStatus(s)) => {
                    self.file_unknown_status_finding(t.id, &s)?;
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
        }
        let done: std::collections::HashSet<i64> = decoded
            .iter()
            .filter(|(_, s)| s.stored().state == GenericState::Done)
            .map(|(t, _)| t.id)
            .collect();
        let mut ready = Vec::new();
        for (task, status) in decoded {
            // `failed` is dispatchable too: failure status is workflow, not a
            // quality verdict. Typed infrastructure failures are held out by
            // `dispatch_after`; only review rejections move the ladder.
            if !status.is_dispatchable()
                || task.claimed_by.is_some()
                || task.operator_driven != operator_driven
                || kind.is_some_and(|kind| task.kind != kind)
                || !task.deps.iter().all(|dependency| done.contains(dependency))
            {
                continue;
            }
            let backoff_expired = task
                .dispatch_after
                .as_deref()
                .map(|after| parse_utc_timestamp(after, "task dispatch_after"))
                .transpose()?
                .is_none_or(|after| after <= now);
            // Backoff defers another attempt, but must not hide an exhausted
            // operator budget from the dispatcher's terminal parking pass.
            // Such a task reaches `launch`, which parks it before any claim,
            // reservation or process is created.
            let budget_exhausted = if backoff_expired {
                false
            } else {
                self.task_budget_remainder(task.id)?
                    .is_some_and(|budget| budget.remaining_usd <= 0.0)
            };
            if backoff_expired || budget_exhausted {
                ready.push(task);
            }
        }
        Ok(ready)
    }

    fn file_unknown_status_finding(&self, task_id: i64, status: &str) -> Result<()> {
        ledger_write_with_busy_retry("filing unknown task-status finding", || {
            let existing = self
                .conn
                .query_row(
                    "SELECT id FROM findings
                     WHERE task_id = ?1 AND reason_code = ?2 AND status = 'open'",
                    params![task_id, FindingReason::UnknownStatus.as_db_str()],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?;
            if existing.is_none() {
                self.file_finding_reasoned(
                    Some(task_id),
                    "warn",
                    &format!("task {task_id} has unknown status"),
                    &format!("unknown status {status} — skipped; upgrade or fix the row"),
                    "foreman",
                    FindingReason::UnknownStatus,
                )?;
            }
            Ok(())
        })
    }

}
