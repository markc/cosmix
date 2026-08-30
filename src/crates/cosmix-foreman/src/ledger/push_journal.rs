pub const PUSH_REPLAY_CLAIM_DETAIL: &str =
    "claimed for replay; remote outcome is unknown until replay completes";

impl Ledger {
    /// Commit the update and deletion intents as one durable ledger write.
    /// The caller must let this transaction finish before advancing the local
    /// integration ref; [`crate::refinery`] enforces that external ordering.
    ///
    /// Both refspecs are derived from trusted landing inputs. In particular,
    /// the update source is the exact verified object id, never the mutable
    /// task or integration branch name. The deletion remains its own kind and
    /// its own row even though both operations carry the same verified tip.
    pub fn record_push_intents_before_landing(
        &self,
        task_id: i64,
        attempt: i64,
        integration: &str,
        verified_tip: &str,
    ) -> Result<(PushIntent, PushIntent)> {
        anyhow::ensure!(valid_branch_name(integration), "invalid integration branch");
        anyhow::ensure!(valid_commit_sha(verified_tip), "invalid verified tip sha");

        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let (branch, stored_attempt, status): (String, i64, String) = tx
            .query_row(
                "SELECT branch, attempt, status FROM tasks WHERE id = ?1",
                params![task_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .with_context(|| format!("reading task {task_id} for push intent"))?;
        anyhow::ensure!(stored_attempt == attempt, "task attempt changed before push intent");
        anyhow::ensure!(status == "landing", "task is not in landing state");
        anyhow::ensure!(valid_branch_name(&branch), "task has invalid recorded branch");
        anyhow::ensure!(
            branch != integration && branch != "main",
            "refusing deletion intent for protected branch"
        );

        let update_refspec = format!("{verified_tip}:refs/heads/{integration}");
        let delete_refspec = format!(":refs/heads/{branch}");
        let now = Utc::now().to_rfc3339();
        for (kind, refspec) in [
            (PushIntentKind::Update, update_refspec.as_str()),
            (PushIntentKind::Delete, delete_refspec.as_str()),
        ] {
            tx.execute(
                "INSERT OR IGNORE INTO push_intents
                     (task_id, attempt, kind, refspec, verified_tip, outcome,
                      detail, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'unknown', '', ?6, ?6)",
                params![task_id, attempt, kind.as_str(), refspec, verified_tip, now],
            )?;
        }
        let update = push_intent_in_tx(
            &tx,
            task_id,
            attempt,
            PushIntentKind::Update,
            &update_refspec,
            verified_tip,
        )?;
        let delete = push_intent_in_tx(
            &tx,
            task_id,
            attempt,
            PushIntentKind::Delete,
            &delete_refspec,
            verified_tip,
        )?;
        tx.commit()?;
        Ok((update, delete))
    }

    pub fn push_intents_for_attempt(
        &self,
        task_id: i64,
        attempt: i64,
    ) -> Result<Vec<PushIntent>> {
        let mut statement = self.conn.prepare(
            "SELECT id, task_id, attempt, kind, refspec, verified_tip, outcome, detail
             FROM push_intents
             WHERE task_id = ?1 AND attempt = ?2
             ORDER BY id",
        )?;
        let rows = statement.query_map(params![task_id, attempt], row_to_push_intent)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    /// Rows still needing an operator or recovery decision, in journal order.
    pub fn outstanding_push_intents(&self) -> Result<Vec<PushIntent>> {
        let mut statement = self.conn.prepare(
            "SELECT id, task_id, attempt, kind, refspec, verified_tip, outcome, detail
             FROM push_intents
             WHERE outcome IN ('failed', 'unknown')
             ORDER BY id",
        )?;
        let rows = statement.query_map([], row_to_push_intent)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    /// Atomically take the right to replay one definitive failure. The
    /// durable state becomes `unknown` before any remote operation starts:
    /// if the process dies after delivery but before recording its result,
    /// the next recovery reports the ambiguity instead of replaying it.
    ///
    /// The guarded update is also the concurrency claim. Exactly one
    /// recovery can change a given row from `failed`; every competing caller
    /// observes `false` and must not dispatch it.
    pub fn claim_failed_push_for_replay(&self, id: i64) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE push_intents
             SET outcome = 'unknown',
                 detail = ?2,
                 updated_at = ?3
             WHERE id = ?1 AND outcome = 'failed'",
            params![id, PUSH_REPLAY_CLAIM_DETAIL, Utc::now().to_rfc3339()],
        )?;
        Ok(changed == 1)
    }

    /// Store an attempted delivery result. A succeeded row is terminal;
    /// failed rows become replayable only through the durable claim above,
    /// and unknown rows remain report-only.
    pub fn record_push_outcome(
        &self,
        id: i64,
        outcome: PushIntentOutcome,
        detail: &str,
    ) -> Result<bool> {
        anyhow::ensure!(detail.len() <= 16 * 1024, "push intent detail is too large");
        let changed = self.conn.execute(
            "UPDATE push_intents
             SET outcome = ?2, detail = ?3, updated_at = ?4
             WHERE id = ?1 AND outcome != 'succeeded'",
            params![id, outcome.as_str(), detail, Utc::now().to_rfc3339()],
        )?;
        Ok(changed == 1)
    }
}

fn valid_commit_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn push_intent_in_tx(
    tx: &rusqlite::Transaction<'_>,
    task_id: i64,
    attempt: i64,
    kind: PushIntentKind,
    refspec: &str,
    verified_tip: &str,
) -> Result<PushIntent> {
    tx.query_row(
        "SELECT id, task_id, attempt, kind, refspec, verified_tip, outcome, detail
         FROM push_intents
         WHERE task_id = ?1 AND attempt = ?2 AND kind = ?3 AND refspec = ?4
           AND verified_tip = ?5",
        params![task_id, attempt, kind.as_str(), refspec, verified_tip],
        row_to_push_intent,
    )
    .map_err(Into::into)
}

fn row_to_push_intent(row: &rusqlite::Row<'_>) -> rusqlite::Result<PushIntent> {
    let kind: String = row.get("kind")?;
    let outcome: String = row.get("outcome")?;
    Ok(PushIntent {
        id: row.get("id")?,
        task_id: row.get("task_id")?,
        attempt: row.get("attempt")?,
        kind: match kind.as_str() {
            "update" => PushIntentKind::Update,
            "delete" => PushIntentKind::Delete,
            _ => return Err(rusqlite::Error::InvalidQuery),
        },
        refspec: row.get("refspec")?,
        verified_tip: row.get("verified_tip")?,
        outcome: match outcome.as_str() {
            "succeeded" => PushIntentOutcome::Succeeded,
            "failed" => PushIntentOutcome::Failed,
            "unknown" => PushIntentOutcome::Unknown,
            _ => return Err(rusqlite::Error::InvalidQuery),
        },
        detail: row.get("detail")?,
    })
}
