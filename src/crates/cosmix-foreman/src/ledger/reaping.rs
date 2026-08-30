impl Ledger {
    /// Sweep stale reservations: rows past the expiry cutoff whose owning
    /// process is gone (a LIVE long run keeps its hold — expiry alone must
    /// not strip a five-hour run mid-flight), plus rows of any age carrying
    /// a pid whose process is confirmed gone. A pid is useful immediately;
    /// the expiry remains the fallback for rows whose owner cannot be
    /// evaluated. Its own transaction means a later refused reserve cannot
    /// roll the sweep back, and read paths (status/admit) can clear a crashed
    /// hold without waiting for another reserve.
    pub fn sweep_reservations(&self, expire_before_rfc3339: &str) -> Result<usize> {
        // Snapshot candidates without a write transaction: process-liveness
        // checks read /proc and must never extend SQLite's single-writer
        // critical section. The guarded delete below re-checks every immutable
        // field, so a concurrent release plus rowid reuse cannot delete the
        // replacement reservation.
        let candidates = {
            let mut stmt = self.conn.prepare(
                "SELECT id, claimant, task_id, usd, tokens, pid, pid_start, created_at
                 FROM reservations WHERE created_at < ?1 OR pid IS NOT NULL",
            )?;
            let rows = stmt.query_map(params![expire_before_rfc3339], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                    r.get::<_, f64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                    r.get::<_, String>(7)?,
                ))
            })?;
            let mut candidates = Vec::new();
            for row in rows {
                candidates.push(row?);
            }
            candidates
        };
        let dead: Vec<_> = candidates
            .into_iter()
            .filter(|(_, _, _, _, _, pid, pid_start, _)| {
                !crate::procutil::owner_alive(*pid, *pid_start)
            })
            .collect();
        if dead.is_empty() {
            return Ok(0);
        }

        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let mut deleted = 0;
        for (id, claimant, task_id, usd, tokens, pid, pid_start, created_at) in dead {
            deleted += tx.execute(
                "DELETE FROM reservations
                 WHERE id = ?1 AND claimant = ?2 AND task_id IS ?3
                   AND usd = ?4 AND tokens = ?5 AND pid IS ?6 AND pid_start IS ?7
                   AND created_at = ?8 AND (created_at < ?9 OR pid IS NOT NULL)",
                params![
                    id,
                    claimant,
                    task_id,
                    usd,
                    tokens,
                    pid,
                    pid_start,
                    created_at,
                    expire_before_rfc3339
                ],
            )?;
        }
        tx.commit()?;
        Ok(deleted)
    }

    /// Reap tasks whose claim lease has expired AND whose claiming process
    /// is confirmed gone — a dead dispatch supervisor's phantom `running`
    /// claim, the fleet gap task 94 closed. Delegates to
    /// [`Ledger::reap_dead_claims_with`] wired to the real `/proc` liveness
    /// check; see that method for the reaping predicate and everything it
    /// deliberately never touches.
    ///
    /// Prefer [`Ledger::reap_dead_claims_with`] at a call site that owns a
    /// sweep's inputs (`foreman dispatch` does): passing the observer
    /// explicitly keeps the host observation visible as an INPUT to the
    /// sweep, next to its recorded `now`, rather than hidden inside this
    /// wrapper.
    pub fn reap_dead_claims(&self, now_rfc3339: &str) -> Result<ReapSweep> {
        self.reap_dead_claims_with(now_rfc3339, crate::procutil::owner_alive)
    }

    /// Same sweep as [`Ledger::reap_dead_claims`], with the liveness check
    /// supplied by the caller rather than hard-wired to `/proc`.
    ///
    /// Process liveness is an OBSERVATION OF THE HOST, and it is the only
    /// input to this sweep that a later reader cannot re-derive: a process
    /// that was gone at sweep time is gone permanently, and one that was
    /// alive may be gone by the time anyone looks again. So it is handled
    /// like every other such input in this crate — supplied at the seam
    /// (production hands in `procutil::owner_alive`; a replay or a test
    /// hands in the answer it recorded), and WRITTEN DOWN when it decides
    /// anything: every reap files a finding naming the pid observed absent
    /// and the instant it was observed, so the ledger explains the
    /// `running -> queued` transition on its own terms instead of resting
    /// on an unrecorded look at `/proc`. Given the same ledger, the same
    /// `now` and the same observer, this function is a pure function of its
    /// inputs.
    ///
    /// The lease is only the "old enough to be worth checking" gate;
    /// liveness is the actual predicate, so a claim whose process is still
    /// alive is NEVER touched no matter how old its lease looks — a
    /// false-dead verdict would steal a live long run out from under its
    /// agent.
    ///
    /// Only a claim carrying a `claim_pid` — set exclusively by the trusted
    /// production claim path, never derived from the claimant string — can
    /// be proven dead this way. An MCP-originated claim's self-reported
    /// `claimed_by` text is not evidence of anything: `claim_pid` is NULL
    /// for it regardless of what the text looks like, and an expired lease
    /// alone must never reap it.
    ///
    /// Every reap resets the task straight to `queued` (never `failed` or
    /// `bounced`) and files a `major` finding — reportable, never a
    /// `blocker`, so it parks nothing — naming the dead claimant, the pid
    /// observed absent, how long the claim had been HELD, and how far past
    /// its lease it was. The task did nothing wrong, so this never touches
    /// `ladder_failures` or any other quality counter. The abandoned run row is left as-is: nothing here
    /// can honestly reconstruct its duration or usage, since the one
    /// process that knew them is the process that died.
    ///
    /// The sweep is per candidate all the way down — SQLite contention is
    /// retried inside it, and a candidate whose write ultimately fails is
    /// left claimed rather than aborting the sweep, so one bad write does
    /// not cost the sweep the candidates after it. But it is NOT swallowed:
    /// every such candidate comes back in [`ReapSweep::unreaped`] with its
    /// error, and the caller must treat a non-empty `unreaped` as a harness
    /// fault (dispatch exits non-zero on it). An earlier cut printed the
    /// failure to stderr and returned `Ok`, which let a persistent write
    /// fault leave the dead claim `running` behind a green dispatch — the
    /// exact silent strand this reaper exists to end. Only the candidate
    /// SNAPSHOT failing is an `Err`: with no candidates there is nothing to
    /// report partially.
    ///
    /// Do NOT wrap the call in a retry: a retry out there re-runs the sweep
    /// from scratch, and a claim reaped by the abandoned pass is no longer
    /// a candidate, so it vanishes from the returned report (which is the
    /// operator's only account of what the sweep did) while staying
    /// committed in the ledger. [`ReapSweep::reaped`] is exactly the claims
    /// this call reaped.
    pub fn reap_dead_claims_with(
        &self,
        now_rfc3339: &str,
        owner_alive: impl Fn(Option<i64>, Option<i64>) -> bool,
    ) -> Result<ReapSweep> {
        let now = parse_utc_timestamp(now_rfc3339, "reap timestamp")?;
        // Every retry in this sweep is INTERNAL, per step, on purpose. The
        // whole sweep used to sit inside one `ledger_write_with_busy_retry`
        // at the call site, and a retry there re-ran it from scratch: claims
        // already reaped in the abandoned pass are no longer candidates
        // (they are `queued`), so the returned Vec — the operator's only
        // report of what the sweep did — silently lost them. The durable
        // record was never at risk (each reap commits its own requeue and
        // finding together), but a report that understates the sweep is the
        // same class of dishonesty this whole arc is about.
        let candidates: Vec<ExpiredClaim> =
            ledger_write_with_busy_retry("snapshotting expired claims", || {
                self.expired_claims(now_rfc3339)
            })?;
        let mut sweep = ReapSweep::default();
        for ExpiredClaim {
            id,
            claimant,
            attempt,
            claim_pid,
            lease_until,
            claimed_at,
        } in candidates
        {
            // No authenticated pid on this claim — cannot be proven dead,
            // skip it rather than guess. See the doc comment above.
            let Some(pid) = claim_pid else {
                continue;
            };
            // No recorded pid_start for tasks (unlike `reservations`): a
            // reused pid degrades to "looks alive" rather than a false
            // reap, which is the fail-safe direction.
            if owner_alive(Some(pid), None) {
                continue;
            }
            // `lease_until` is only ever written by this crate, always as an
            // RFC3339 timestamp (claim time, or NULLed out right here) — a
            // parse failure means the row is corrupt in a way this reaper
            // cannot honestly characterise. Skip it rather than fabricate an
            // overdue time; the row stays claimed for an operator to find.
            let Ok(lease_expired_at) = parse_utc_timestamp(&lease_until, "task lease_until") else {
                continue;
            };
            let overdue_secs = (now - lease_expired_at).num_seconds();
            // The claim's real age, which is the number an operator reading
            // this finding wants: "held for 7h" says a supervisor died, where
            // "1h past its lease" alone hides the whole lease window. A claim
            // predating the `claimed_at` column (or carrying an unparseable
            // one) reports its age as unknown rather than a guess.
            let claim_age_secs = claimed_at
                .as_deref()
                .and_then(|at| parse_utc_timestamp(at, "task claimed_at").ok())
                .map(|claimed| (now - claimed).num_seconds());
            let age_note = match claim_age_secs {
                Some(age) => format!("held for {age}s"),
                None => "held since before claim times were recorded".to_string(),
            };
            // One candidate's write failing must not cost the sweep the
            // candidates it already reaped, so this is retried and judged
            // per candidate rather than allowed to abort the loop. A claim
            // left behind here is not lost: it is still expired and still
            // dead at the next sweep, which is minutes away — but the
            // failure itself IS reported, in `unreaped`, so the caller can
            // fail its run rather than call a sweep that could not write a
            // success.
            let outcome = ledger_write_with_busy_retry("reaping a dead claim", || {
                self.reap_one_dead_claim(
                    now_rfc3339,
                    id,
                    &claimant,
                    attempt,
                    pid,
                    &age_note,
                    overdue_secs,
                )
            });
            match outcome {
                Ok(true) => sweep.reaped.push(ReapedClaim {
                    task_id: id,
                    claimant,
                    claim_pid: pid,
                    overdue_secs,
                    claim_age_secs,
                }),
                // Lost a race: the claim changed hands or was released
                // between the snapshot and the write, so there was nothing
                // of this generation left to reap.
                Ok(false) => {}
                Err(error) => sweep.unreaped.push(UnreapedClaim {
                    task_id: id,
                    claimant,
                    claim_pid: pid,
                    error,
                }),
            }
        }
        Ok(sweep)
    }

    /// The candidate snapshot behind [`Ledger::reap_dead_claims_with`]:
    /// every claim old enough to be worth a liveness check. Retried as a
    /// unit by the caller, hence a separate method rather than an inline
    /// block borrowing a prepared statement across the retry closure.
    fn expired_claims(&self, now_rfc3339: &str) -> Result<Vec<ExpiredClaim>> {
        let candidates = {
            let mut stmt = self.conn.prepare(
                "SELECT id, claimed_by, attempt, claim_pid, lease_until, claimed_at FROM tasks
                 WHERE status IN ('claimed', 'running')
                   AND claimed_by IS NOT NULL
                   AND lease_until IS NOT NULL AND lease_until < ?1",
            )?;
            let rows = stmt.query_map(params![now_rfc3339], |r| {
                Ok(ExpiredClaim {
                    id: r.get(0)?,
                    claimant: r.get(1)?,
                    attempt: r.get(2)?,
                    claim_pid: r.get(3)?,
                    lease_until: r.get(4)?,
                    claimed_at: r.get(5)?,
                })
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            out
        };
        Ok(candidates)
    }

    /// Reap ONE snapshotted dead claim: the requeue and the finding that
    /// explains it commit together or not at all, so no reader can ever see
    /// a `running -> queued` transition with no recorded cause. Returns
    /// `false` when the claim changed hands (or was released) between the
    /// snapshot and this write — nothing of that generation was left to
    /// reap, which is a skip and not a failure.
    #[allow(clippy::too_many_arguments)]
    fn reap_one_dead_claim(
        &self,
        now_rfc3339: &str,
        id: i64,
        claimant: &str,
        attempt: i64,
        pid: i64,
        age_note: &str,
        overdue_secs: i64,
    ) -> Result<bool> {
        #[cfg(test)]
        fail_armed_claim_reap_for_test(id)?;
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let n = tx.execute(
            "UPDATE tasks SET status = 'queued', claimed_by = NULL, claim_pid = NULL,
                    lease_until = NULL, claimed_at = NULL, dispatch_after = NULL,
                    updated_at = ?1
             WHERE id = ?2 AND claimed_by = ?3 AND attempt = ?4
               AND status IN ('claimed', 'running')",
            params![now_rfc3339, id, claimant, attempt],
        )?;
        if n != 1 {
            // Lost the race — the transaction drops uncommitted on scope
            // exit and this candidate is simply skipped.
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO findings
                 (task_id, severity, title, body, filed_by, reason_code, created_at)
             VALUES (?1, 'major', 'reaped a dead claim', ?2, 'dispatch', ?3, ?4)",
            params![
                id,
                // The evidence, written down at the moment it was observed:
                // which claimant, which pid, how old the claim was, how far
                // past its lease, and that the pid was observed absent at
                // `now`. This finding IS the record of the observation —
                // see `reap_dead_claims_with`.
                format!(
                    "claim held by `{claimant}` (pid {pid}, {age_note}, \
                     {overdue_secs}s past its lease) — pid {pid} observed absent at \
                     {now_rfc3339}, so the claim was released back to queued; not \
                     charged against the task's ladder position"
                ),
                FindingReason::DeadClaimReaped.as_db_str(),
                now_rfc3339,
            ],
        )?;
        tx.commit()?;
        Ok(true)
    }

}
