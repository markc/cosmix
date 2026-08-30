impl Ledger {
    /// Atomically reserve budget headroom: finished spend + live
    /// reservations + this request are checked against the ceilings and the
    /// reservation inserted — in one IMMEDIATE transaction, so concurrent
    /// claims cannot jointly exceed a ceiling by racing the check. Ceiling 0
    /// = disabled. Returns the reservation id to release when the run's
    /// actuals land. Call [`Ledger::sweep_reservations`] first.
    #[allow(clippy::too_many_arguments)]
    pub fn reserve(
        &self,
        claimant: &str,
        task_id: Option<i64>,
        usd: f64,
        tokens: u64,
        ceiling_usd: f64,
        ceiling_tokens: u64,
        since_rfc3339: &str,
    ) -> Result<i64> {
        anyhow::ensure!(
            usd.is_finite() && usd >= 0.0,
            "reservation amount must be a finite non-negative dollar value, got {usd}"
        );
        anyhow::ensure!(
            tokens <= (i64::MAX as u64) / 4,
            "reservation token amount {tokens} is implausibly large"
        );
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let (spent_usd, spent_tokens): (f64, i64) = tx.query_row(
            "SELECT COALESCE(SUM(COALESCE(
                        cost_usd,
                        CASE WHEN verdict IS NOT NULL
                                   AND (tokens_in > 0 OR tokens_out > 0)
                             THEN reserved_usd ELSE 0.0 END
                    )), 0.0),
                    COALESCE(SUM(tokens_out), 0)
             FROM runs WHERE started_at >= ?1",
            params![since_rfc3339],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let (held_usd, held_tokens): (f64, i64) = tx.query_row(
            "SELECT COALESCE(SUM(usd), 0.0), COALESCE(SUM(tokens), 0) FROM reservations",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if ceiling_usd > 0.0 && spent_usd + held_usd + usd > ceiling_usd {
            return Err(ReservationRefused(format!(
                "reservation refused: ${spent_usd:.2} spent + ${held_usd:.2} reserved \
                 + ${usd:.2} requested exceeds the ${ceiling_usd:.2} daily ceiling"
            ))
            .into());
        }
        if ceiling_tokens > 0
            && (spent_tokens.max(0) as u64) + (held_tokens.max(0) as u64) + tokens > ceiling_tokens
        {
            return Err(ReservationRefused(format!(
                "reservation refused: {spent_tokens} output tokens spent + {held_tokens} \
                 reserved + {tokens} requested exceeds the {ceiling_tokens} daily ceiling"
            ))
            .into());
        }
        let now = Utc::now().to_rfc3339();
        let pid = std::process::id();
        tx.execute(
            "INSERT INTO reservations (claimant, task_id, usd, tokens, pid, pid_start,
                                       created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                claimant,
                task_id,
                usd,
                tokens as i64,
                pid as i64,
                crate::procutil::starttime(pid as i64),
                now
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(id)
    }

    /// Release a reservation (the run's actuals are in `runs` now, or the
    /// run never started). Idempotent.
    pub fn release_reservation(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM reservations WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// (usd, tokens) currently held by live reservations.
    pub fn reserved_totals(&self) -> Result<(f64, u64)> {
        let row = self.conn.query_row(
            "SELECT COALESCE(SUM(usd), 0.0), COALESCE(SUM(tokens), 0) FROM reservations",
            [],
            |r| Ok((r.get::<_, f64>(0)?, r.get::<_, i64>(1)?)),
        )?;
        Ok((row.0, row.1.max(0) as u64))
    }

    /// (spend_usd, output_tokens) across runs started at/after `since_rfc3339`.
    /// A terminal run with recorded tokens but no reported price is charged
    /// its recorded dollar reservation. That is the only defensible amount
    /// after an orphan is terminalised: treating NULL as free undercounts
    /// real work, while an open run is still covered by its live reservation
    /// and must not be charged twice. RFC 3339 UTC strings compare
    /// lexicographically, which is what makes the timestamp comparison sound.
    pub fn usage_since(&self, since_rfc3339: &str) -> Result<(f64, u64)> {
        let row = self.conn.query_row(
            "SELECT COALESCE(SUM(COALESCE(
                        cost_usd,
                        CASE WHEN verdict IS NOT NULL
                                   AND (tokens_in > 0 OR tokens_out > 0)
                             THEN reserved_usd ELSE 0.0 END
                    )), 0.0),
                    COALESCE(SUM(tokens_out), 0)
             FROM runs WHERE started_at >= ?1",
            params![since_rfc3339],
            |r| Ok((r.get::<_, f64>(0)?, r.get::<_, i64>(1)?)),
        )?;
        Ok((row.0, row.1.max(0) as u64))
    }

}
