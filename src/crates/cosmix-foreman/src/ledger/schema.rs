impl Ledger {
    fn migrate(&self, path: &Path) -> Result<()> {
        let version = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?;
        if version > SCHEMA_VERSION {
            anyhow::bail!(
                "ledger schema at {} is user_version {version}, newer than this foreman build \
                 supports ({SCHEMA_VERSION}) — refusing to open (never migrate down)",
                path.display()
            );
        }
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS project_identity (
                singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                name TEXT NOT NULL UNIQUE,
                repository_identity TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS tasks (
                id INTEGER PRIMARY KEY,
                title TEXT NOT NULL,
                spec TEXT NOT NULL,
                kind TEXT NOT NULL DEFAULT 'impl',
                risk TEXT NOT NULL DEFAULT 'low',
                bump TEXT CHECK (bump IN ('patch', 'minor')),
                status TEXT NOT NULL DEFAULT 'queued',
                deps TEXT NOT NULL DEFAULT '[]',
                crates TEXT NOT NULL DEFAULT '[]',
                claimed_by TEXT,
                worktree TEXT,
                branch TEXT,
                lease_until TEXT,
                claim_pid INTEGER,
                claimed_at TEXT,
                attempt INTEGER NOT NULL DEFAULT 0,
                ladder_failures INTEGER NOT NULL DEFAULT 0,
                review_rejections INTEGER NOT NULL DEFAULT 0,
                branch_contract_failures INTEGER NOT NULL DEFAULT 0,
                infra_refusals INTEGER NOT NULL DEFAULT 0,
                dispatch_after TEXT,
                background_abandonments INTEGER NOT NULL DEFAULT 0,
                operator_driven INTEGER NOT NULL DEFAULT 0
                    CHECK (operator_driven IN (0, 1)),
                verifier_profile TEXT NOT NULL DEFAULT 'rust',
                budget_usd REAL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS runs (
                id INTEGER PRIMARY KEY,
                task_id INTEGER NOT NULL REFERENCES tasks(id),
                agent TEXT NOT NULL,
                model TEXT,
                session_ref TEXT,
                tokens_in INTEGER NOT NULL DEFAULT 0,
                tokens_out INTEGER NOT NULL DEFAULT 0,
                cost_usd REAL,
                reserved_usd REAL,
                verdict TEXT,
                result TEXT,
                error TEXT,
                duration_ms INTEGER,
                started_at TEXT NOT NULL,
                role TEXT NOT NULL DEFAULT 'implement',
                delivery TEXT NOT NULL DEFAULT 'unknown',
                quality TEXT NOT NULL DEFAULT 'unknown',
                attempt INTEGER,
                ladder_charge INTEGER NOT NULL DEFAULT 0
                    CHECK (ladder_charge IN (0, 1)),
                ladder_charge_reason TEXT
            );
            CREATE TABLE IF NOT EXISTS findings (
                id INTEGER PRIMARY KEY,
                task_id INTEGER REFERENCES tasks(id),
                severity TEXT NOT NULL DEFAULT 'info',
                title TEXT NOT NULL,
                body TEXT NOT NULL DEFAULT '',
                filed_by TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'open',
                reason_code TEXT NOT NULL DEFAULT 'unknown',
                run_id INTEGER REFERENCES runs(id),
                file TEXT,
                line INTEGER,
                resolution TEXT,
                resolved_at TEXT,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS events (
                id INTEGER PRIMARY KEY,
                run_id INTEGER NOT NULL REFERENCES runs(id),
                seq INTEGER NOT NULL,
                kind TEXT NOT NULL,
                payload TEXT NOT NULL,
                at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS reservations (
                id INTEGER PRIMARY KEY,
                claimant TEXT NOT NULL,
                task_id INTEGER,
                usd REAL NOT NULL,
                tokens INTEGER NOT NULL,
                pid INTEGER,
                pid_start INTEGER,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS verifications (
                id INTEGER PRIMARY KEY,
                task_id INTEGER NOT NULL REFERENCES tasks(id),
                run_id INTEGER REFERENCES runs(id),
                attempt INTEGER,
                tier INTEGER NOT NULL DEFAULT 0,
                pass INTEGER NOT NULL,
                report TEXT NOT NULL,
                at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS push_intents (
                id INTEGER PRIMARY KEY,
                task_id INTEGER NOT NULL REFERENCES tasks(id),
                attempt INTEGER NOT NULL,
                kind TEXT NOT NULL CHECK (kind IN ('update', 'delete')),
                refspec TEXT NOT NULL,
                verified_tip TEXT NOT NULL
                    CHECK (length(verified_tip) = 40
                        AND verified_tip NOT GLOB '*[^0-9a-fA-F]*'),
                outcome TEXT NOT NULL DEFAULT 'unknown'
                    CHECK (outcome IN ('succeeded', 'failed', 'unknown')),
                detail TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                UNIQUE (task_id, attempt, kind, refspec, verified_tip)
            );
            CREATE INDEX IF NOT EXISTS idx_tasks_status ON tasks(status);
            CREATE INDEX IF NOT EXISTS idx_runs_task ON runs(task_id);
            CREATE INDEX IF NOT EXISTS idx_events_run ON events(run_id, seq);
            CREATE INDEX IF NOT EXISTS idx_verifications_task ON verifications(task_id);
            CREATE INDEX IF NOT EXISTS idx_push_intents_outcome_id
                ON push_intents(outcome, id);
            CREATE TRIGGER IF NOT EXISTS trg_push_intents_immutable
            BEFORE UPDATE OF task_id, attempt, kind, refspec, verified_tip
                ON push_intents
            BEGIN
                SELECT RAISE(ABORT, 'push_intents: immutable intent');
            END;
            CREATE TRIGGER IF NOT EXISTS trg_push_intents_succeeded_terminal
            BEFORE UPDATE OF outcome ON push_intents
            WHEN OLD.outcome = 'succeeded' AND NEW.outcome != OLD.outcome
            BEGIN
                SELECT RAISE(ABORT, 'push_intents: succeeded outcome is terminal');
            END;
            CREATE TRIGGER IF NOT EXISTS trg_runs_nonneg_ins
            BEFORE INSERT ON runs
            WHEN NEW.tokens_in < 0 OR NEW.tokens_out < 0
                 OR (NEW.cost_usd IS NOT NULL AND NEW.cost_usd < 0)
            BEGIN
                SELECT RAISE(ABORT, 'runs: tokens_in/tokens_out/cost_usd must be non-negative');
            END;
            CREATE TRIGGER IF NOT EXISTS trg_runs_nonneg_upd
            BEFORE UPDATE ON runs
            WHEN NEW.tokens_in < 0 OR NEW.tokens_out < 0
                 OR (NEW.cost_usd IS NOT NULL AND NEW.cost_usd < 0)
            BEGIN
                SELECT RAISE(ABORT, 'runs: tokens_in/tokens_out/cost_usd must be non-negative');
            END;
            CREATE TRIGGER IF NOT EXISTS trg_reservations_nonneg_ins
            BEFORE INSERT ON reservations
            WHEN NEW.usd < 0 OR NEW.tokens < 0
            BEGIN
                SELECT RAISE(ABORT, 'reservations: usd/tokens must be non-negative');
            END;
            CREATE TRIGGER IF NOT EXISTS trg_reservations_nonneg_upd
            BEFORE UPDATE ON reservations
            WHEN NEW.usd < 0 OR NEW.tokens < 0
            BEGIN
                SELECT RAISE(ABORT, 'reservations: usd/tokens must be non-negative');
            END;",
        )?;
        // CREATE IF NOT EXISTS never upgrades an existing ledger — columns
        // added after first release must be back-filled explicitly.
        for (table, col, decl) in [
            ("runs", "result", "TEXT"),
            ("runs", "error", "TEXT"),
            ("runs", "role", "TEXT NOT NULL DEFAULT 'implement'"),
            ("runs", "delivery", "TEXT NOT NULL DEFAULT 'unknown'"),
            ("runs", "quality", "TEXT NOT NULL DEFAULT 'unknown'"),
            ("tasks", "verifier_profile", "TEXT NOT NULL DEFAULT 'rust'"),
            ("tasks", "crates", "TEXT NOT NULL DEFAULT '[]'"),
            ("tasks", "ladder_failures", "INTEGER NOT NULL DEFAULT 0"),
            // When this claim's renewable lease runs out. Written at claim
            // time, returned through `Task`, and advanced by generation-
            // fenced heartbeats for local and PID-less workers alike.
            ("tasks", "lease_until", "TEXT"),
            // When the current claim was taken, so a reap can report the
            // claim's real AGE (not just how far past its lease it is — the
            // two differ by the whole lease window). Recorded rather than
            // derived from `lease_until - CLAIM_LEASE_SECS`: that derivation
            // silently lies about any claim taken before a release that
            // changed the constant. NULL on a claim taken before this column
            // existed, which the reaper reports as an unknown age rather
            // than inventing one.
            ("tasks", "claimed_at", "TEXT"),
            // The claiming process's own pid, set ONLY by the trusted
            // production claim path (never parsed back out of `claimed_by`,
            // which for an MCP-originated claim is untrusted free text) —
            // see `claim_task_in_tx`. NULL for every other claim path, which
            // `Ledger::reap_dead_claims` treats as unreapable.
            ("tasks", "claim_pid", "INTEGER"),
            ("tasks", "infra_refusals", "INTEGER NOT NULL DEFAULT 0"),
            (
                "tasks",
                "background_abandonments",
                "INTEGER NOT NULL DEFAULT 0",
            ),
            (
                "tasks",
                "operator_driven",
                "INTEGER NOT NULL DEFAULT 0 CHECK (operator_driven IN (0, 1))",
            ),
            ("tasks", "budget_usd", "REAL"),
            ("reservations", "pid", "INTEGER"),
            ("reservations", "pid_start", "INTEGER"),
            ("findings", "reason_code", "TEXT NOT NULL DEFAULT 'unknown'"),
            ("findings", "run_id", "INTEGER REFERENCES runs(id)"),
            ("findings", "file", "TEXT"),
            ("findings", "line", "INTEGER"),
            ("verifications", "run_id", "INTEGER REFERENCES runs(id)"),
            // Version 4: old rows deliberately remain NULL. There is no
            // sound way to infer which attempt produced them, and treating
            // them as current could revive a previous landing's verdict.
            ("verifications", "attempt", "INTEGER"),
            // Input-token breakdown: None means unknown (lane doesn't report it).
            ("runs", "fresh_input_tokens", "INTEGER"),
            ("runs", "cache_read_input_tokens", "INTEGER"),
            ("runs", "cache_creation_input_tokens", "INTEGER"),
            ("runs", "reserved_usd", "REAL"),
            ("project_identity", "repository_identity", "TEXT"),
        ] {
            let present: Option<i64> = self
                .conn
                .query_row(
                    &format!("SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1"),
                    params![col],
                    |r| r.get(0),
                )
                .optional()?;
            if present.is_none() {
                self.conn
                    .execute(&format!("ALTER TABLE {table} ADD COLUMN {col} {decl}"), [])?;
            }
        }
        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_verifications_task_attempt
             ON verifications(task_id, attempt, id)",
            [],
        )?;
        self.conn.execute_batch(
            "CREATE TRIGGER IF NOT EXISTS trg_runs_reservation_nonneg_ins
             BEFORE INSERT ON runs
             WHEN NEW.reserved_usd IS NOT NULL AND NEW.reserved_usd < 0
             BEGIN
                 SELECT RAISE(ABORT, 'runs: reserved_usd must be non-negative');
             END;
             CREATE TRIGGER IF NOT EXISTS trg_runs_reservation_nonneg_upd
             BEFORE UPDATE OF reserved_usd ON runs
             WHEN NEW.reserved_usd IS NOT NULL AND NEW.reserved_usd < 0
             BEGIN
                 SELECT RAISE(ABORT, 'runs: reserved_usd must be non-negative');
             END;",
        )?;
        // Version 3 outcome backfill. The old ledger recorded only the
        // subprocess stop in `verdict`, so only clean completion and a
        // named budget ceiling are safe to derive. Generic `error` mixed
        // vendor failures with harness failures and unfinished rows have no
        // terminal evidence: both remain explicitly unknown. Token columns
        // have always been NOT NULL, so they cannot distinguish an
        // unreported hard kill from a genuine zero-usage terminal failure.
        //
        // `model='merge-review'` was a role masquerading as a model. The
        // review role is derivable, but the real historical model is not;
        // clear the lie rather than guessing the then-current default.
        // These updates are deliberately idempotent so opening an already
        // migrated copy cannot promote any unknown row.
        if version < 3 {
            self.conn.execute_batch(
                "UPDATE runs SET role = 'review' WHERE model = 'merge-review';
                 UPDATE runs SET model = NULL WHERE model = 'merge-review';
                 UPDATE runs SET role = 'implement' WHERE role IS NULL OR role = '';
                 UPDATE runs SET delivery = CASE verdict
                     WHEN 'done' THEN 'delivered'
                     WHEN 'budget_ceiling' THEN 'resource_exhausted'
                     WHEN 'interrupted' THEN 'operator_stopped'
                     ELSE 'unknown'
                 END
                 WHERE delivery IS NULL OR delivery = '' OR delivery = 'unknown';
                 UPDATE runs SET quality = 'unknown' WHERE quality IS NULL OR quality = '';",
            )?;
        }
        // Version 7 expands the quality enum enforced by these triggers.
        // CREATE IF NOT EXISTS cannot replace the version-6 definitions, so
        // remove them before installing the new fail-closed allow-list.
        if version < 7 {
            self.conn.execute_batch(
                "DROP TRIGGER IF EXISTS trg_runs_outcomes_ins;
                 DROP TRIGGER IF EXISTS trg_runs_outcomes_upd;",
            )?;
        }
        // Version 12 -> 13: fix the Codex tokens_in double-count (Task 72).
        // The old fold stored `input + cached`, where cached was already a
        // subset of input. Rows with the captured cache component can be
        // reconstructed exactly:
        //   corrected total = old total - cached
        //   fresh = corrected total - cached = old total - 2 * cached
        // Leave rows without enough consistent evidence untouched rather
        // than inventing a split or writing a negative token count.
        if version < 13 {
            self.conn.execute_batch(
                "UPDATE runs
                   SET tokens_in = tokens_in - cache_read_input_tokens,
                       fresh_input_tokens = tokens_in - cache_read_input_tokens
                                            - cache_read_input_tokens
                 WHERE agent = 'codex'
                   AND fresh_input_tokens IS NULL
                   AND cache_read_input_tokens IS NOT NULL
                   AND tokens_in - cache_read_input_tokens
                       >= cache_read_input_tokens;",
            )?;
        }
        self.conn.execute_batch(
            "CREATE TRIGGER IF NOT EXISTS trg_runs_outcomes_ins
             BEFORE INSERT ON runs
             WHEN NEW.role NOT IN ('implement', 'review', 'verify')
               OR NEW.delivery NOT IN ('delivered', 'resource_exhausted', 'vendor_error',
                    'harness_error', 'spec_blocked', 'operator_stopped', 'unknown')
               OR NEW.quality NOT IN ('unknown', 'branch_contract_failed',
                    'agent_abandoned_background',
                    'tier_0_passed', 'tier_0_failed', 'tier_1_passed', 'tier_1_failed',
                    'tier_2_passed', 'tier_2_failed', 'review_approved',
                    'review_rejected', 'landed', 'post_land_regression')
             BEGIN
                 SELECT RAISE(ABORT, 'runs: invalid role/delivery/quality');
             END;
             CREATE TRIGGER IF NOT EXISTS trg_runs_outcomes_upd
             BEFORE UPDATE OF role, delivery, quality ON runs
             WHEN NEW.role NOT IN ('implement', 'review', 'verify')
               OR NEW.delivery NOT IN ('delivered', 'resource_exhausted', 'vendor_error',
                    'harness_error', 'spec_blocked', 'operator_stopped', 'unknown')
               OR NEW.quality NOT IN ('unknown', 'branch_contract_failed',
                    'agent_abandoned_background',
                    'tier_0_passed', 'tier_0_failed', 'tier_1_passed', 'tier_1_failed',
                    'tier_2_passed', 'tier_2_failed', 'review_approved',
                    'review_rejected', 'landed', 'post_land_regression')
             BEGIN
                 SELECT RAISE(ABORT, 'runs: invalid role/delivery/quality');
             END;",
        )?;
        match version {
            SCHEMA_VERSION => {}
            _ => {
                // Version 1 -> 2 (`findings.reason_code`), version 3 -> 4
                // (`verifications.attempt`), version 4 -> 5 (`tasks.crates`),
                // version 5 -> 6 (`tasks.operator_driven`) and version 7 -> 8
                // (`tasks.background_abandonments`) are additive columns;
                // version 8 -> 9 adds the project identity singleton, and
                // version 9 -> 10 binds identity to repository history, and
                // version 10 -> 11 adds `tasks.budget_usd`, and version 11 ->
                // 12 records each run's dollar reservation, version 12 -> 13
                // corrects reconstructable historical Codex input folds, and
                // version 13 -> 14 atomically attributes quality charges to an
                // attempt while splitting reason-specific routing counters,
                // version 14 -> 15 adds nullable operator-owned version bump
                // intent (NULL deliberately retains the old derivation), and
                // version 15 -> 16 adds finding resolution audit fields, and
                // version 16 -> 17 reconciles stale landed reservations, and
                // version 17 -> 18 adds the durable push-intent journal.
                // The older additive changes are handled by the loop above;
                // version 6 -> 7 replaces the run-outcome allow-list triggers
                // above. A second ALTER
                // here would duplicate them on a real fleet DB. The
                // idempotent v2 -> v3 outcome column/backfill work also ran
                // above; stamp only after every step succeeded. Legacy
                // verification rows intentionally retain NULL attempt:
                // inventing a generation would make stale landing evidence
                // look current.
                let tx = rusqlite::Transaction::new_unchecked(
                    &self.conn,
                    rusqlite::TransactionBehavior::Immediate,
                )?;
                for (table, col, decl) in [
                    ("tasks", "review_rejections", "INTEGER NOT NULL DEFAULT 0"),
                    (
                        "tasks",
                        "branch_contract_failures",
                        "INTEGER NOT NULL DEFAULT 0",
                    ),
                    ("tasks", "dispatch_after", "TEXT"),
                    ("runs", "attempt", "INTEGER"),
                    (
                        "runs",
                        "ladder_charge",
                        "INTEGER NOT NULL DEFAULT 0 CHECK (ladder_charge IN (0, 1))",
                    ),
                    ("runs", "ladder_charge_reason", "TEXT"),
                    ("tasks", "bump", "TEXT CHECK (bump IN ('patch', 'minor'))"),
                    ("findings", "resolution", "TEXT"),
                    ("findings", "resolved_at", "TEXT"),
                ] {
                    add_column_if_missing(&tx, table, col, decl)?;
                }
                if version < 16 {
                    // The policy hook already enforced these refusals. They
                    // are audit events from a working gate, not evidence that
                    // the task itself is blocked. Keep the match narrow so an
                    // operator-authored finding with similar prose is never
                    // silently reclassified.
                    tx.execute(
                        "UPDATE findings SET severity = 'info'
                         WHERE status = 'open' AND severity = 'blocker'
                           AND title = 'policy escalation'
                           AND filed_by = 'policy-gate'",
                        [],
                    )?;
                    // Reconcile pre-schema-16 rows only after the policy
                    // severity is corrected. Original title/body evidence is
                    // untouched, and non-terminal states deliberately remain
                    // open. tasks.updated_at is the closest durable timestamp
                    // to the historical terminal transition.
                    tx.execute_batch(
                        "UPDATE findings
                         SET status = 'resolved',
                             resolution = CASE (
                                 SELECT status FROM tasks WHERE id = findings.task_id
                             )
                                 WHEN 'landed' THEN 'task ' || task_id ||
                                     ' landed (schema 16 reconciliation)'
                                 WHEN 'retired' THEN 'task ' || task_id ||
                                     ' retired (schema 16 reconciliation)'
                             END,
                             resolved_at = (
                                 SELECT updated_at FROM tasks WHERE id = findings.task_id
                             )
                         WHERE status = 'open' AND EXISTS (
                             SELECT 1 FROM tasks
                             WHERE tasks.id = findings.task_id
                               AND tasks.status IN ('landed', 'retired')
                         );",
                    )?;
                }
                if version < 17 {
                    // Before schema 17, landing left the scheduling flag set.
                    // Preserve an auditable automatic-release event and clear
                    // only landed rows; active reservations are untouched.
                    tx.execute_batch(
                        "INSERT INTO findings
                             (task_id, severity, title, body, filed_by, status,
                              reason_code, resolution, resolved_at, created_at)
                         SELECT id, 'info',
                                'task ' || id ||
                                    ' released from operator-driven execution',
                                'Automatically released by schema 17 reconciliation because the task had already landed; historical landing did not clear this flag.',
                                'schema-17-migration', 'resolved',
                                'operator_released',
                                'task ' || id || ' landed (schema 17 reconciliation)',
                                updated_at, updated_at
                         FROM tasks
                         WHERE status = 'landed' AND operator_driven = 1;
                         UPDATE tasks SET operator_driven = 0
                         WHERE status = 'landed' AND operator_driven = 1;",
                    )?;
                }
                #[cfg(test)]
                FAIL_SCHEMA_14_BEFORE_COMMIT.with(|fail| -> Result<()> {
                    anyhow::ensure!(!fail.replace(false), "injected schema-14 migration failure");
                    Ok(())
                })?;
                tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
                tx.commit()?;
            }
        }
        Ok(())
    }

}

/// Conservative subset of git's ref-name rules, biased to reject: no leading
/// dash (argv injection), no whitespace/control, none of git's forbidden
/// metacharacters, no "..", bounded length.
pub fn valid_branch_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && !name.starts_with('-')
        && !name.starts_with('.')
        && !name.ends_with('/')
        && !name.ends_with(".lock")
        && !name.contains("..")
        && name
            .chars()
            .all(|c| !c.is_whitespace() && !c.is_control() && !"~^:?*[\\".contains(c))
}

fn row_to_task(r: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
    let deps_json: String = r.get("deps")?;
    let crates_json: String = r.get("crates")?;
    Ok(Task {
        id: r.get("id")?,
        title: r.get("title")?,
        spec: r.get("spec")?,
        kind: r.get("kind")?,
        risk: r.get("risk")?,
        bump: r.get("bump")?,
        status: r.get("status")?,
        deps: decode_deps(&deps_json, 0)?,
        crates: decode_crates(&crates_json, 0)?,
        claimed_by: r.get("claimed_by")?,
        lease_until: r.get("lease_until")?,
        worktree: r.get("worktree")?,
        branch: r.get("branch")?,
        attempt: r.get("attempt")?,
        ladder_failures: r.get("ladder_failures")?,
        review_rejections: r.get("review_rejections")?,
        branch_contract_failures: r.get("branch_contract_failures")?,
        infra_refusals: r.get("infra_refusals")?,
        dispatch_after: r.get("dispatch_after")?,
        background_abandonments: r.get("background_abandonments")?,
        operator_driven: r.get("operator_driven")?,
        verifier_profile: r.get("verifier_profile")?,
        budget_usd: r.get("budget_usd")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
    })
}

fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    declaration: &str,
) -> Result<()> {
    let present: Option<i64> = conn
        .query_row(
            &format!("SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1"),
            params![column],
            |row| row.get(0),
        )
        .optional()?;
    if present.is_none() {
        conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {declaration}"),
            [],
        )?;
    }
    Ok(())
}

fn decode_crates(json: &str, column: usize) -> rusqlite::Result<Vec<String>> {
    serde_json::from_str(json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(column, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn valid_crate_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn decode_deps(json: &str, column: usize) -> rusqlite::Result<Vec<i64>> {
    serde_json::from_str(json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(column, rusqlite::types::Type::Text, Box::new(e))
    })
}
