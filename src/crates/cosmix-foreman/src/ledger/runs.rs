impl Ledger {
    pub fn store_run_start(
        &self,
        task_id: i64,
        agent: &str,
        model: Option<&str>,
        role: Option<&str>,
    ) -> Result<i64> {
        let now = Utc::now().to_rfc3339();
        let inserted = self.conn.execute(
            "INSERT INTO runs (task_id, agent, model, role, started_at, attempt)
             SELECT id, ?2, ?3, COALESCE(?4, 'implement'), ?5, attempt
             FROM tasks WHERE id = ?1",
            params![task_id, agent, model, role, now],
        )?;
        anyhow::ensure!(inserted == 1, "cannot start run for missing task {task_id}");
        Ok(self.conn.last_insert_rowid())
    }

    /// Writes the authoritative outcome over the row — except that it must
    /// never *regress* the streamed checkpoint [`Ledger::update_run_usage`]
    /// already wrote. An outcome with no usage of its own is what an
    /// interrupted/errored path produces when the parser's accumulation was
    /// lost (abandoned reader, a ledger write that failed mid-stream); zeroing
    /// the columns there would erase real, already-paid-for spend and
    /// undercount the governor's day. Same for cost: a `None` is "this driver
    /// never priced it", not "it cost nothing", so it coalesces onto whatever
    /// the stream last reported.
    pub fn store_run_finish(
        &self,
        run_id: i64,
        outcome: &StoredRunOutcome,
        duration_ms: i64,
    ) -> Result<()> {
        let delivery = match outcome.stop.as_str() {
            "done" => "delivered",
            "budget_ceiling" => "resource_exhausted",
            "interrupted" => "operator_stopped",
            "error" => "vendor_error",
            _ => "unknown",
        };
        self.store_run_finish_as(run_id, outcome, duration_ms, delivery)
    }

    pub fn store_run_finish_as(
        &self,
        run_id: i64,
        outcome: &StoredRunOutcome,
        duration_ms: i64,
        delivery: &str,
    ) -> Result<()> {
        let input_tokens = i64::try_from(outcome.input_tokens)
            .context("input_tokens overflows i64 — cannot store")?;
        let output_tokens = i64::try_from(outcome.output_tokens)
            .context("output_tokens overflows i64 — cannot store")?;
        let component = |name: &str, v: Option<u64>| -> Result<Option<i64>> {
            v.map(|v| {
                i64::try_from(v).with_context(|| format!("{name} overflows i64 — cannot store"))
            })
            .transpose()
        };
        let fresh_input = component("fresh_input_tokens", outcome.fresh_input_tokens)?;
        let cache_read = component("cache_read_input_tokens", outcome.cache_read_input_tokens)?;
        let cache_creation = component(
            "cache_creation_input_tokens",
            outcome.cache_creation_input_tokens,
        )?;
        // Unchanged guard: an outcome carrying no usage must not clobber the
        // mid-stream checkpoint. The breakdown rides the same flag so the
        // components can never disagree with the total they belong to.
        let no_usage = outcome.input_tokens == 0 && outcome.output_tokens == 0;
        let n = self.conn.execute(
            "UPDATE runs SET
                    tokens_in  = CASE WHEN ?1 THEN tokens_in  ELSE ?2 END,
                    tokens_out = CASE WHEN ?1 THEN tokens_out ELSE ?3 END,
                    cost_usd   = COALESCE(?4, cost_usd),
                    fresh_input_tokens = CASE WHEN ?1 THEN fresh_input_tokens ELSE ?5 END,
                    cache_read_input_tokens = CASE WHEN ?1 THEN cache_read_input_tokens ELSE ?6 END,
                    cache_creation_input_tokens = CASE WHEN ?1 THEN cache_creation_input_tokens ELSE ?7 END,
                    verdict = ?8, result = ?9, error = ?10, duration_ms = ?11,
                    session_ref = ?12, delivery = ?13
             WHERE id = ?14",
            params![
                no_usage,
                input_tokens,
                output_tokens,
                outcome.cost_usd,
                fresh_input,
                cache_read,
                cache_creation,
                &outcome.stop,
                outcome.result,
                outcome.error,
                duration_ms,
                outcome.session_ref,
                delivery,
                run_id
            ],
        )?;
        anyhow::ensure!(n == 1, "no run {run_id}");
        Ok(())
    }

    /// Record the latest gate disposition for a run. The full causal chain
    /// remains in `verifications` (linked by run_id); this column is the
    /// current quality dimension carried by the run itself.
    pub fn set_run_quality(&self, run_id: i64, quality: &str) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE runs SET quality = ?1 WHERE id = ?2",
            params![quality, run_id],
        )?;
        anyhow::ensure!(n == 1, "no run {run_id}");
        Ok(())
    }

    /// Current attempt's implementation run, with one migration-only
    /// fallback. A schema-13 NULL-attempt run is eligible only while no
    /// attempt-stamped implementation run exists and a current-attempt
    /// verification explicitly names that run. Absence is not ownership:
    /// MCP completion/self-bounce/operator-land paths can create a later
    /// attempt without a run row or runless verification, so an unproven
    /// fallback charges nothing rather than charging historical work.
    pub fn latest_implementation_run(&self, task_id: i64) -> Result<Option<i64>> {
        self.conn
            .query_row(
                "SELECT candidate.id
                 FROM runs candidate
                 JOIN tasks task ON task.id = candidate.task_id
                 WHERE candidate.id = (
                     SELECT id FROM runs
                     WHERE task_id = ?1 AND role = 'implement'
                     ORDER BY id DESC LIMIT 1
                 )
                   AND (candidate.attempt = task.attempt OR (
                       candidate.attempt IS NULL
                       AND NOT EXISTS (
                           SELECT 1 FROM runs current_runs
                           WHERE current_runs.task_id = ?1
                             AND current_runs.role = 'implement'
                             AND current_runs.attempt IS NOT NULL
                       )
                       AND NOT EXISTS (
                           SELECT 1 FROM verifications current_verification
                           WHERE current_verification.task_id = ?1
                             AND current_verification.attempt = task.attempt
                             AND current_verification.run_id IS NULL
                       )
                       AND EXISTS (
                           SELECT 1 FROM verifications ownership_proof
                           WHERE ownership_proof.task_id = ?1
                             AND ownership_proof.attempt = task.attempt
                             AND ownership_proof.run_id = candidate.id
                       )
                   ))",
                params![task_id],
                |r| r.get(0),
            )
            .optional()
            .context("loading implementation run")
    }

    /// The session id a resource-exhausted prior attempt left behind, when
    /// the next attempt is at the SAME rung.
    ///
    /// `model` is part of the rung, not a detail: a ladder climb keeps the
    /// agent and changes the model, and resuming the previous model's
    /// conversation under a new model is not the "same context, more
    /// headroom" this lookup exists to provide. Filtering on agent alone let
    /// a climb resume through the preconfigured driver path even though
    /// `runner.rs`'s same-rung guard would have refused it — the two must
    /// agree on what a rung is. `None` for `model` matches only rows that
    /// also recorded no model.
    pub fn latest_resumable_session(
        &self,
        task_id: i64,
        agent: &str,
        model: Option<&str>,
    ) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT session_ref FROM runs
                 WHERE id = (
                     SELECT MAX(id) FROM runs
                     WHERE task_id = ?1 AND role = 'implement'
                 )
                   AND agent = ?2
                   AND model IS ?3
                   AND delivery = 'resource_exhausted'
                   AND session_ref IS NOT NULL AND session_ref != ''
                 LIMIT 1",
                params![task_id, agent, model],
                |row| row.get(0),
            )
            .optional()
            .context("loading resumable implementation session")
    }

    /// Retire a vendor session after an exact not-found/mismatch result.
    /// Clearing the reference is compatible with existing schema-v15 fleet
    /// databases and makes both resume lookups fail closed without inventing
    /// a run quality the installed v15 trigger would reject.
    pub fn mark_run_session_dead(&self, run_id: i64, session_ref: &str) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE runs SET session_ref = NULL
             WHERE id = ?1 AND session_ref = ?2",
            params![run_id, session_ref],
        )?;
        anyhow::ensure!(
            changed == 1,
            "run {run_id} no longer owns session {session_ref:?}"
        );
        Ok(())
    }

    /// Persist the conversation a prepared run is about to resume, or the
    /// fresh conversation it established before its outer lifecycle writes
    /// the terminal outcome. A Foreman crash between those boundaries must
    /// not leave this newer row NULL or pointing at a retired conversation.
    pub fn record_run_resume_intent(&self, run_id: i64, session_ref: &str) -> Result<()> {
        anyhow::ensure!(
            !session_ref.is_empty(),
            "resume session id must not be empty"
        );
        let changed = self.conn.execute(
            "UPDATE runs SET session_ref = ?1
             WHERE id = ?2 AND verdict IS NULL
               AND (session_ref IS NULL OR session_ref = ?1)",
            params![session_ref, run_id],
        )?;
        anyhow::ensure!(
            changed == 1,
            "run {run_id} cannot record resume intent {session_ref:?}"
        );
        Ok(())
    }

    /// Incremental usage checkpoint, called on every streamed `Usage`
    /// event (not just at `finish_run`): a run killed mid-stream — SIGTERM
    /// to foreman itself, which has no signal handler and so skips every
    /// Drop guard — must not leave the row's usage NULL when real spend
    /// already happened. `finish_run` still writes the authoritative final
    /// figure whenever the process lives long enough to reach it; this is
    /// the best-evidence fallback for when it doesn't.
    ///
    /// While the run is live its checkpoint is summed alongside the
    /// reservation still held for it, so the governor briefly counts the
    /// same spend twice — conservative, never permissive, and it settles the
    /// moment the reservation is released.
    ///
    /// Token counts are last-seen-wins (a driver's later figure supersedes
    /// its earlier one — the claude parser's terminal `result` line is
    /// authoritative even when it reports *less* than the running
    /// accumulation). Cost coalesces instead, by the same rule
    /// [`Ledger::finish_run`] follows: a `None` is "this event carried no
    /// price", not "the run is free", so it must never null a figure the
    /// stream already reported.
    pub fn update_run_usage(&self, run_id: i64, usage: &Usage) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET tokens_in = ?1, tokens_out = ?2,
                    cost_usd = COALESCE(?3, cost_usd),
                    fresh_input_tokens = ?4,
                    cache_read_input_tokens = ?5,
                    cache_creation_input_tokens = ?6
             WHERE id = ?7",
            params![
                usage.input_tokens as i64,
                usage.output_tokens as i64,
                usage.cost_usd,
                usage.fresh_input_tokens.map(|v| v as i64),
                usage.cache_read_input_tokens.map(|v| v as i64),
                usage.cache_creation_input_tokens.map(|v| v as i64),
                run_id
            ],
        )?;
        Ok(())
    }

    pub fn record_event(&self, run_id: i64, seq: i64, kind: &str, payload: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.record_event_at(run_id, seq, kind, payload, &now)
    }

    pub(crate) fn record_event_at(
        &self,
        run_id: i64,
        seq: i64,
        kind: &str,
        payload: &str,
        now: &str,
    ) -> Result<()> {
        #[cfg(test)]
        FAIL_NEXT_RUN_EVENT_WRITE.with(|fail| -> Result<()> {
            anyhow::ensure!(!fail.replace(false), "injected run event write failure");
            Ok(())
        })?;
        self.conn.execute(
            "INSERT INTO events (run_id, seq, kind, payload, at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![run_id, seq, kind, payload, now],
        )?;
        Ok(())
    }

    /// Atomically journal a fresh fallback and clear the dead resume intent
    /// from the current, still-open run. A process death after this commit but
    /// before the fresh driver emits its first event must leave the newest run
    /// cold, not pointing back at the session the journal just retired.
    pub(crate) fn record_resume_fallback_and_retire_current_at(
        &self,
        run_id: i64,
        seq: i64,
        payload: &str,
        session_ref: &str,
        now: &str,
    ) -> Result<()> {
        #[cfg(test)]
        FAIL_NEXT_RUN_EVENT_WRITE.with(|fail| -> Result<()> {
            anyhow::ensure!(!fail.replace(false), "injected run event write failure");
            Ok(())
        })?;
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        tx.execute(
            "INSERT INTO events (run_id, seq, kind, payload, at) \
             VALUES (?1, ?2, 'resume_fallback', ?3, ?4)",
            params![run_id, seq, payload, now],
        )?;
        let changed = tx.execute(
            "UPDATE runs SET session_ref = NULL
             WHERE id = ?1 AND verdict IS NULL AND session_ref = ?2",
            params![run_id, session_ref],
        )?;
        anyhow::ensure!(
            changed == 1,
            "current run {run_id} no longer owns dead session {session_ref:?}"
        );
        tx.commit()?;
        Ok(())
    }

    pub fn run_event_count(&self, run_id: i64) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM events WHERE run_id = ?1",
            params![run_id],
            |r| r.get(0),
        )?)
    }

    /// Every event sequence number recorded against `run_id`, in insertion
    /// order. `events` carries no UNIQUE(run_id, seq), so a run whose driver
    /// numbered two passes from zero would silently interleave rather than
    /// error — this is how a test can see that it did not.
    pub fn run_event_seqs(&self, run_id: i64) -> Result<Vec<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT seq FROM events WHERE run_id = ?1 ORDER BY id")?;
        let rows = stmt.query_map(params![run_id], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<Vec<i64>, _>>()?)
    }

    /// Event kinds in journal order, used by end-to-end fixtures to prove a
    /// controlled resume boundary was persisted rather than merely logged.
    pub fn run_event_kinds(&self, run_id: i64) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT kind FROM events WHERE run_id = ?1 ORDER BY seq")?;
        let rows = stmt.query_map(params![run_id], |row| row.get(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("loading run event kinds")
    }

}
