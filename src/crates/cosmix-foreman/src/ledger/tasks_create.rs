impl Ledger {
    pub fn add_task(
        &self,
        title: &str,
        spec: &str,
        kind: &str,
        risk: &str,
        deps: &[i64],
        verifier_profile: &str,
    ) -> Result<i64> {
        self.add_task_scoped(
            title,
            spec,
            kind,
            risk,
            deps,
            TaskControls {
                verifier_profile,
                crates: &[],
                operator_driven_reason: None,
            },
        )
    }

    /// Add a task with an operator-owned crate designation for the policy
    /// gate. Crate names are structured authority; free-form title/spec text
    /// is deliberately not consulted by policy routing.
    pub fn add_task_scoped(
        &self,
        title: &str,
        spec: &str,
        kind: &str,
        risk: &str,
        deps: &[i64],
        controls: TaskControls<'_>,
    ) -> Result<i64> {
        self.add_task_scoped_with_budget(title, spec, kind, risk, deps, controls, None)
    }

    /// Add a task with the normal structured controls and an optional dollar
    /// reservation/run cap for its attempts.
    #[allow(clippy::too_many_arguments)]
    pub fn add_task_scoped_with_budget(
        &self,
        title: &str,
        spec: &str,
        kind: &str,
        risk: &str,
        deps: &[i64],
        controls: TaskControls<'_>,
        budget_usd: Option<f64>,
    ) -> Result<i64> {
        self.add_task_scoped_with_budget_and_bump(
            title, spec, kind, risk, deps, controls, budget_usd, None,
        )
    }

    /// Add a task with all structured operator controls, including an
    /// optional package-version bump intent.
    #[allow(clippy::too_many_arguments)]
    pub fn add_task_scoped_with_budget_and_bump(
        &self,
        title: &str,
        spec: &str,
        kind: &str,
        risk: &str,
        deps: &[i64],
        controls: TaskControls<'_>,
        budget_usd: Option<f64>,
        bump: Option<&str>,
    ) -> Result<i64> {
        let TaskControls {
            verifier_profile,
            crates,
            operator_driven_reason,
        } = controls;
        if let Some(reason) = operator_driven_reason {
            anyhow::ensure!(
                !reason.trim().is_empty(),
                "operator-driven reservation reason must not be blank"
            );
        }
        let operator_driven = operator_driven_reason.is_some();
        for crate_name in crates {
            anyhow::ensure!(
                valid_crate_name(crate_name),
                "invalid task crate name {crate_name:?}"
            );
        }
        let mut unique_crates = HashSet::new();
        anyhow::ensure!(
            crates.iter().all(|name| unique_crates.insert(name)),
            "duplicate task crate designation"
        );
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let next_id: i64 = tx.query_row("SELECT COALESCE(MAX(id), 0) + 1 FROM tasks", [], |r| {
            r.get(0)
        })?;
        for &dep in deps {
            if dep >= next_id {
                Err(DepsError::Future(dep))?;
            }
            let present: Option<i64> = tx
                .query_row("SELECT 1 FROM tasks WHERE id = ?1", params![dep], |r| {
                    r.get(0)
                })
                .optional()?;
            if present.is_none() {
                Err(DepsError::Missing(dep))?;
            }
        }
        let mut unique = HashSet::new();
        for &dep in deps {
            if !unique.insert(dep) {
                Err(DepsError::Duplicate(dep))?;
            }
        }
        let mut existing_deps = HashMap::new();
        {
            let mut stmt = tx.prepare("SELECT id, deps FROM tasks")?;
            let rows = stmt.query_map([], |r| {
                let id = r.get::<_, i64>(0)?;
                let deps_json = r.get::<_, String>(1)?;
                let decoded = decode_deps(&deps_json, 1)?;
                Ok((id, decoded))
            })?;
            for row in rows {
                let (id, recorded_deps) = row?;
                existing_deps.insert(id, recorded_deps);
            }
        }
        if let Some(node) = deps_form_cycle(&existing_deps, next_id, deps) {
            Err(DepsError::Cyclic(node))?;
        }
        let now = Utc::now().to_rfc3339();
        if let Some(usd) = budget_usd {
            anyhow::ensure!(
                usd.is_finite() && usd > 0.0,
                "task budget must be a finite positive value, got {usd}"
            );
        }
        if let Some(bump) = bump {
            bump.parse::<VersionBump>()?;
        }
        tx.execute(
            "INSERT INTO tasks (title, spec, kind, risk, deps, verifier_profile, crates,
                                operator_driven, budget_usd, bump, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)",
            params![
                title,
                spec,
                kind,
                risk,
                serde_json::to_string(deps)?,
                verifier_profile,
                serde_json::to_string(crates)?,
                operator_driven,
                budget_usd,
                bump,
                now
            ],
        )?;
        let id = tx.last_insert_rowid();
        if let Some(reason) = operator_driven_reason {
            tx.execute(
                "INSERT INTO findings
                     (task_id, severity, title, body, filed_by, reason_code, created_at)
                 VALUES (?1, 'info', ?2, ?3, 'operator', ?4, ?5)",
                params![
                    id,
                    format!("task {id} reserved for operator-driven execution"),
                    reason,
                    FindingReason::OperatorReserved.as_db_str(),
                    now
                ],
            )?;
        }
        tx.commit()?;
        Ok(id)
    }

    /// Replace a task's explicit package-version bump intent. A claimed,
    /// running or landing task is already being acted on and cannot have its
    /// landing contract changed underneath that attempt.
    pub fn set_task_bump(&self, id: i64, bump: &str) -> Result<()> {
        let bump: VersionBump = bump.parse()?;
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let task = tx
            .query_row(
                "SELECT * FROM tasks WHERE id = ?1",
                params![id],
                row_to_task,
            )
            .optional()?
            .with_context(|| format!("no task {id}"))?;
        let status: TaskStatus = task.status.parse()?;
        anyhow::ensure!(
            task.claimed_by.is_none()
                && !matches!(
                    status,
                    TaskStatus::Claimed | TaskStatus::Running | TaskStatus::Landing
                ),
            "cannot change task {id} bump while it is {}{}",
            task.status,
            task.claimed_by
                .as_deref()
                .map(|claimant| format!(" (claimed by {claimant})"))
                .unwrap_or_default()
        );

        let previous = task.effective_version_bump()?;
        let previous_source = task.version_bump_source();
        let now = Utc::now().to_rfc3339();
        tx.execute(
            "UPDATE tasks SET bump = ?1, updated_at = ?2 WHERE id = ?3",
            params![bump.as_str(), now, id],
        )?;
        tx.execute(
            "INSERT INTO findings
                 (task_id, severity, title, body, filed_by, reason_code, created_at)
             VALUES (?1, 'info', ?2, ?3, 'operator', ?4, ?5)",
            params![
                id,
                format!("task {id} version bump set to {bump}"),
                format!(
                    "Task {id} ('{}') version bump changed from {previous} ({previous_source}) to {bump} (explicit).",
                    task.title
                ),
                FindingReason::Operator.as_db_str(),
                now
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Change whether unattended dispatch may claim a task. This is an
    /// operator-owned scheduling attribute and survives every status change.
    ///
    /// RESERVING a task (`operator_driven = true`) interlocks with a live
    /// scratch-cleanup lease exactly as [`Ledger::requeue_task`] does; see
    /// [`Ledger::set_operator_driven_with`] for why the reservation edge
    /// has to be enforced here rather than re-checked by the sweep.
    /// Un-reserving is never refused: a reserved task is one
    /// [`Ledger::begin_scratch_cleanup`] would not have leased in the first
    /// place, so clearing the flag cannot surprise a deletion in flight.
    /// Returns `true` only when the state changed. Repeating the same state is
    /// a no-op and does not manufacture another decision finding.
    pub fn set_operator_driven(
        &self,
        id: i64,
        operator_driven: bool,
        reason: &str,
        filed_by: &str,
    ) -> Result<bool> {
        self.set_operator_driven_with(
            id,
            operator_driven,
            reason,
            filed_by,
            crate::procutil::owner_alive,
        )
    }

    /// [`Ledger::set_operator_driven`] with the process-liveness observation
    /// supplied by the caller, so the interlock can be exercised against a
    /// recorded answer instead of whatever `/proc` says during a test run.
    ///
    /// The interlock: "never touch an operator-reserved worktree" is a hard
    /// safety requirement of the scratch sweep, and
    /// [`Ledger::begin_scratch_cleanup`] enforces it at lease time with
    /// `operator_driven = 0`. But an unguarded write here could set the flag
    /// AFTER that lease was taken, and the sweep's revalidation
    /// ([`Ledger::scratch_cleanup_still_held`]) checks the claimant — so the
    /// reservation would land silently while `remove_dir_all` was already
    /// walking the task's `src/target/`, and the operator would be told
    /// their worktree was reserved while it was being emptied.
    ///
    /// Revalidating harder in the sweep cannot close this: the check and the
    /// deletion are separate operations. So the refusal is enforced HERE,
    /// where the reservation commits, and — like the requeue interlock — it
    /// is decided by whether the reclaiming pid is actually running, not by
    /// a flag. Both this read and `begin_scratch_cleanup`'s write take
    /// SQLite's write lock, so they serialise: a reservation that commits
    /// leaves `operator_driven = 1`, which the lease guard then refuses.
    ///
    /// A sweep whose process died mid-lease is not alive, so a stranded
    /// stamp never blocks an operator permanently.
    pub fn set_operator_driven_with(
        &self,
        id: i64,
        operator_driven: bool,
        reason: &str,
        filed_by: &str,
        owner_alive: impl Fn(Option<i64>, Option<i64>) -> bool,
    ) -> Result<bool> {
        anyhow::ensure!(
            !reason.trim().is_empty(),
            "operator-driven reason must not be blank"
        );
        anyhow::ensure!(
            !filed_by.trim().is_empty(),
            "operator-driven filed_by must not be blank"
        );
        let now = Utc::now().to_rfc3339();
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let current: Option<bool> = tx
            .query_row(
                "SELECT operator_driven FROM tasks WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        let current = current.ok_or_else(|| anyhow::anyhow!("no task {id}"))?;
        if current == operator_driven {
            tx.commit()?;
            return Ok(false);
        }
        if operator_driven {
            let live_scratch_lease: Option<(String, String)> = tx
                .query_row(
                    "SELECT status, claimed_by FROM tasks WHERE id = ?1 AND claimed_by IS NOT NULL",
                    params![id],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                )
                .optional()?
                .filter(|(status, claimant)| {
                    is_scratch_gc_claimant(claimant)
                        && matches!(status.as_str(), "landed" | "retired")
                        && scratch_gc_owner_alive_with(claimant, &owner_alive)
                });
            if let Some((status, claimant)) = live_scratch_lease {
                drop(tx);
                let pid = scratch_gc_claim_owner(&claimant)
                    .map(|(pid, _)| pid.to_string())
                    .unwrap_or_else(|| "?".to_string());
                anyhow::bail!(
                    "task {id} is {status} and its build scratch is being reclaimed right now by \
                     `{claimant}` (pid {pid}, observed running) — reserving it now would report a \
                     protected worktree while `remove_dir_all` is still walking its build \
                     scratch, so this is refused. The lease clears by itself the moment that \
                     sweep finishes; if it is wedged, kill pid {pid} and reserve then, which \
                     will succeed because the pid is gone"
                );
            }
        }
        let changed = tx.execute(
            "UPDATE tasks SET operator_driven = ?1, updated_at = ?2
             WHERE id = ?3 AND operator_driven = ?4",
            params![operator_driven, now, id, current],
        )?;
        anyhow::ensure!(
            changed == 1,
            "task {id} operator-driven state changed concurrently"
        );
        #[cfg(test)]
        FAIL_OPERATOR_DRIVEN_FINDING_BEFORE_INSERT.with(|fail| -> Result<()> {
            anyhow::ensure!(
                !fail.replace(false),
                "injected operator-driven finding failure"
            );
            Ok(())
        })?;
        let (title, finding_reason) = if operator_driven {
            (
                format!("task {id} reserved for operator-driven execution"),
                FindingReason::OperatorReserved,
            )
        } else {
            (
                format!("task {id} released from operator-driven execution"),
                FindingReason::OperatorReleased,
            )
        };
        tx.execute(
            "INSERT INTO findings
                 (task_id, severity, title, body, filed_by, reason_code, created_at)
             VALUES (?1, 'info', ?2, ?3, ?4, ?5, ?6)",
            params![id, title, reason, filed_by, finding_reason.as_db_str(), now],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Replace or clear a task's operator-owned dollar budget. A live claim
    /// keeps the budget immutable for the whole attempt, so the recorded run
    /// cap and later accounting cannot disagree.
    pub fn set_task_budget(&self, id: i64, budget_usd: Option<f64>) -> Result<()> {
        if let Some(usd) = budget_usd {
            anyhow::ensure!(
                usd.is_finite() && usd > 0.0,
                "task budget must be a finite positive value, got {usd}"
            );
        }
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let task = tx
            .query_row(
                "SELECT * FROM tasks WHERE id = ?1",
                params![id],
                row_to_task,
            )
            .optional()?
            .with_context(|| format!("no task {id}"))?;
        let status: TaskStatus = task.status.parse()?;
        anyhow::ensure!(
            task.claimed_by.is_none()
                && !matches!(
                    status,
                    TaskStatus::Claimed | TaskStatus::Running | TaskStatus::Landing
                ),
            "cannot change task {id} budget while it is {}{}",
            task.status,
            task.claimed_by
                .as_deref()
                .map(|claimant| format!(" (claimed by {claimant})"))
                .unwrap_or_default()
        );

        let now = Utc::now().to_rfc3339();
        tx.execute(
            "UPDATE tasks SET budget_usd = ?1, updated_at = ?2 WHERE id = ?3",
            params![budget_usd, now, id],
        )?;
        let render = |value: Option<f64>| {
            value
                .map(|usd| format!("${usd:.4}"))
                .unwrap_or_else(|| "cleared".to_string())
        };
        tx.execute(
            "INSERT INTO findings
                 (task_id, severity, title, body, filed_by, reason_code, created_at)
             VALUES (?1, 'info', ?2, ?3, 'operator', ?4, ?5)",
            params![
                id,
                format!("task {id} budget changed to {}", render(budget_usd)),
                format!(
                    "Task {id} ('{}') budget changed from {} to {}. Requeue it separately if it is parked.",
                    task.title,
                    render(task.budget_usd),
                    render(budget_usd)
                ),
                FindingReason::Operator.as_db_str(),
                now
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Set a task's verifier profile. Refuses changes on running or landing tasks.
    /// Records the change as an operator finding.
    pub fn set_verifier_profile(&self, id: i64, profile: &str) -> Result<String> {
        use crate::verify::lookup_profile;

        let task = self.task(id)?.with_context(|| format!("no task {id}"))?;
        let previous = lookup_profile(&task.verifier_profile)?.name;
        let canonical = lookup_profile(profile)?.name;
        self.set_verifier_profile_resolved(id, &previous, &canonical)
    }

    /// Store profile identities already resolved by the active project
    /// manifest. The ledger cannot load operator config itself, so the CLI
    /// supplies both canonical names; the same transaction/status fences as
    /// the built-in-only wrapper above still apply.
    pub fn set_verifier_profile_resolved(
        &self,
        id: i64,
        previous: &str,
        canonical: &str,
    ) -> Result<String> {
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let task = tx
            .query_row(
                "SELECT * FROM tasks WHERE id = ?1",
                params![id],
                row_to_task,
            )
            .optional()?
            .ok_or_else(|| anyhow::anyhow!("no task {id}"))?;

        // Refuse changes on running or landing tasks
        let status: TaskStatus = task.status.parse()?;
        match status {
            TaskStatus::Running => {
                anyhow::bail!("cannot change verifier profile while task {id} is running")
            }
            TaskStatus::Landing => {
                anyhow::bail!("cannot change verifier profile while task {id} is landing")
            }
            _ => {}
        }

        let now = Utc::now().to_rfc3339();
        let changed = tx.execute(
            "UPDATE tasks SET verifier_profile = ?1, updated_at = ?2 WHERE id = ?3",
            params![canonical, now, id],
        )?;
        anyhow::ensure!(changed == 1, "no task {id}");

        let title = format!("verifier profile changed for task {id}");
        let body = format!(
            "Task {} ('{}') verifier profile changed from '{}' to '{}'",
            id, task.title, previous, canonical
        );
        tx.execute(
            "INSERT INTO findings
                 (task_id, severity, title, body, filed_by, reason_code, created_at)
             VALUES (?1, 'info', ?2, ?3, 'operator', ?4, ?5)",
            params![id, title, body, FindingReason::Operator.as_db_str(), now],
        )?;
        tx.commit()?;

        Ok(canonical.to_string())
    }

}
