impl Ledger {
    pub fn recent_runs(&self, limit: i64) -> Result<Vec<Run>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, agent, model, session_ref, tokens_in, tokens_out,
                    cost_usd, verdict, result, error, duration_ms, started_at, role, delivery, quality,
                    fresh_input_tokens, cache_read_input_tokens, cache_creation_input_tokens,
                    reserved_usd, attempt, ladder_charge, ladder_charge_reason
             FROM runs ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| {
            Ok(Run {
                id: r.get(0)?,
                task_id: r.get(1)?,
                agent: r.get(2)?,
                model: r.get(3)?,
                session_ref: r.get(4)?,
                tokens_in: r.get(5)?,
                fresh_input_tokens: r.get(16)?,
                cache_read_input_tokens: r.get(17)?,
                cache_creation_input_tokens: r.get(18)?,
                tokens_out: r.get(6)?,
                cost_usd: r.get(7)?,
                reserved_usd: r.get(19)?,
                verdict: r.get(8)?,
                result: r.get(9)?,
                error: r.get(10)?,
                duration_ms: r.get(11)?,
                started_at: r.get(12)?,
                role: r.get(13)?,
                delivery: r.get(14)?,
                quality: r.get(15)?,
                attempt: r.get(20)?,
                ladder_charge: r.get(21)?,
                ladder_charge_reason: r.get(22)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// One row per implementation attempt, newest first, including explicit
    /// zeroes so `task show` makes a missing or duplicate charge obvious.
    pub fn task_attempt_charges(&self, task_id: i64) -> Result<Vec<AttemptCharge>> {
        let mut stmt = self.conn.prepare(
            "SELECT attempt, id, ladder_charge, ladder_charge_reason
             FROM runs
             WHERE task_id = ?1 AND role = 'implement' AND attempt IS NOT NULL
             ORDER BY attempt DESC, id DESC",
        )?;
        let rows = stmt.query_map(params![task_id], |row| {
            Ok(AttemptCharge {
                attempt: row.get(0)?,
                run_id: row.get(1)?,
                charged: row.get(2)?,
                reason: row.get(3)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Find the agent that implemented a task: the latest cleanly completed
    /// implementation run. Governed review rows are deliberately excluded so
    /// a rejected landing retry cannot route from its own previous reviewer.
    pub fn implementing_agent_for_task(&self, task_id: i64) -> Result<Option<String>> {
        let agent: Option<String> = self
            .conn
            .query_row(
                "SELECT agent FROM runs
                 WHERE task_id = ?1
                 AND role = 'implement'
                 AND verdict = 'done'
                 AND (error IS NULL OR error = '')
                 ORDER BY id DESC
                 LIMIT 1",
                params![task_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(agent)
    }

    /// The most recent OTHER run for `task_id` with `role`, optionally
    /// narrowed to a specific `agent` (two-arm review keeps one thread per
    /// reviewer kind), excluding `exclude_run_id` (the row the caller just
    /// inserted for the CURRENT attempt). `None` when there is no such run.
    /// Every terminal verdict counts, not just `done` — a bounced or
    /// budget-ceilinged run's session still exists to resume; only the
    /// caller decides whether the rung still matches.
    pub fn last_run_ref(
        &self,
        task_id: i64,
        role: &str,
        agent: Option<&str>,
        exclude_run_id: i64,
    ) -> Result<Option<RunRef>> {
        #[cfg(test)]
        FAIL_NEXT_LAST_RUN_REF_ERROR.with(|fail| -> Result<()> {
            anyhow::ensure!(!fail.replace(false), "injected last_run_ref failure");
            Ok(())
        })?;
        #[cfg(test)]
        FAIL_NEXT_LAST_RUN_REF_BUSY.with(|fail| -> Result<()> {
            if fail.replace(false) {
                return Err(anyhow::Error::new(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                    Some("database is locked".to_string()),
                )));
            }
            Ok(())
        })?;
        self.conn
            .query_row(
                "SELECT id, agent, model, session_ref FROM runs
                 WHERE task_id = ?1 AND role = ?2 AND id != ?3
                   AND (?4 IS NULL OR agent = ?4)
                 ORDER BY id DESC LIMIT 1",
                params![task_id, role, exclude_run_id, agent],
                |r| {
                    Ok(RunRef {
                        id: r.get(0)?,
                        agent: r.get(1)?,
                        model: r.get(2)?,
                        session_ref: r.get(3)?,
                    })
                },
            )
            .optional()
            .context("loading last run reference")
    }

    pub fn status_counts(&self) -> Result<Vec<(String, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT status, COUNT(*) FROM tasks GROUP BY status ORDER BY status")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Delivery void for every run contributing to an all-time aggregate.
    pub fn delivery_void_fraction(&self) -> Result<VoidFraction> {
        self.void_fraction("delivery", None)
    }

    /// Quality void for every run contributing to an all-time aggregate.
    pub fn quality_void_fraction(&self) -> Result<VoidFraction> {
        self.void_fraction("quality", None)
    }

    /// Delivery void over the exact time window used by the daily governor.
    pub fn delivery_void_fraction_since(&self, since_rfc3339: &str) -> Result<VoidFraction> {
        self.void_fraction("delivery", Some(since_rfc3339))
    }

    fn void_fraction(&self, dimension: &str, since: Option<&str>) -> Result<VoidFraction> {
        anyhow::ensure!(matches!(dimension, "delivery" | "quality"));
        let where_since = if since.is_some() {
            " WHERE started_at >= ?1"
        } else {
            ""
        };
        let total_sql = format!("SELECT COUNT(*) FROM runs{where_since}");
        let unknown_sql = format!(
            "SELECT COUNT(*) FROM runs{where_since}{} {dimension} = 'unknown'",
            if since.is_some() { " AND" } else { " WHERE" }
        );
        let (total, unknown) = match since {
            Some(since) => (
                self.conn
                    .query_row(&total_sql, params![since], |r| r.get(0))?,
                self.conn
                    .query_row(&unknown_sql, params![since], |r| r.get(0))?,
            ),
            None => (
                self.conn.query_row(&total_sql, [], |r| r.get(0))?,
                self.conn.query_row(&unknown_sql, [], |r| r.get(0))?,
            ),
        };
        Ok(VoidFraction {
            contributing_runs: total,
            unknown_runs: unknown,
            fraction: if total > 0 {
                unknown as f64 / total as f64
            } else {
                0.0
            },
        })
    }

}
