    fn always_busy() -> anyhow::Error {
        anyhow::Error::new(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            Some("database is locked".to_string()),
        ))
    }

    /// The cleanup helper must actually spend a LARGER budget than the
    /// run-path one — that asymmetry is the whole reason the run-path
    /// exhaustion arm can still release its claim against a lock that
    /// outlasted the first budget. Counting attempts (rather than asserting
    /// the two constants) is what proves the wiring, not just the numbers.
    #[test]
    fn the_cleanup_budget_buys_more_attempts_than_the_run_path_budget() {
        let run_attempts = std::cell::Cell::new(0usize);
        let run = super::ledger_write_with_busy_retry("run-path probe", || {
            run_attempts.set(run_attempts.get() + 1);
            Err::<(), _>(always_busy())
        });
        let cleanup_attempts = std::cell::Cell::new(0usize);
        let cleanup = super::ledger_cleanup_write_with_busy_retry("cleanup probe", || {
            cleanup_attempts.set(cleanup_attempts.get() + 1);
            Err::<(), _>(always_busy())
        });

        assert!(super::sqlite_busy_retries_exhausted(&run.unwrap_err()));
        assert!(super::sqlite_busy_retries_exhausted(&cleanup.unwrap_err()));
        assert_eq!(run_attempts.get(), super::BUSY_RETRIES + 1);
        assert_eq!(cleanup_attempts.get(), super::CLEANUP_BUSY_RETRIES + 1);
        assert!(
            cleanup_attempts.get() > run_attempts.get(),
            "the last-chance cleanup must outlast the write that already gave up"
        );
    }

    /// A non-busy failure is a real error, not weather: it must surface on
    /// the FIRST attempt on either budget rather than being slept over.
    #[test]
    fn a_non_busy_error_is_never_retried_on_either_budget() {
        let run_attempts = std::cell::Cell::new(0usize);
        let run = super::ledger_write_with_busy_retry("run-path probe", || {
            run_attempts.set(run_attempts.get() + 1);
            Err::<(), _>(anyhow::anyhow!("constraint violated"))
        });
        let cleanup_attempts = std::cell::Cell::new(0usize);
        let cleanup = super::ledger_cleanup_write_with_busy_retry("cleanup probe", || {
            cleanup_attempts.set(cleanup_attempts.get() + 1);
            Err::<(), _>(anyhow::anyhow!("constraint violated"))
        });

        assert_eq!(run_attempts.get(), 1, "run-path retried a non-busy failure");
        assert_eq!(
            cleanup_attempts.get(),
            1,
            "cleanup retried a non-busy failure"
        );
        assert!(!super::sqlite_busy_retries_exhausted(&run.unwrap_err()));
        assert!(!super::sqlite_busy_retries_exhausted(&cleanup.unwrap_err()));
    }

    /// `file_finding` (the bare, 5-arg, no-reason method) exists ONLY so
    /// `policy.rs` — one of foreman's own gates, which no agent may edit —
    /// keeps compiling untouched; its reason code is hardcoded to
    /// `PolicyDenied` because that is structurally what its one remaining
    /// caller always means. Every OTHER call site must go through
    /// `file_finding_reasoned` and state its own reason explicitly. This
    /// scans the crate's own source so a future call site cannot silently
    /// fall back onto that policy-only default by mistake.
    #[test]
    fn file_finding_bare_call_is_policy_gate_only() {
        let refinery = refinery_source();
        let sources: &[(&str, &str)] = &[
            ("policy.rs", include_str!("../policy.rs")),
            ("runner.rs", include_str!("../runner.rs")),
            ("refinery", refinery.as_str()),
            ("mcp.rs", include_str!("../mcp.rs")),
            ("main.rs", include_str!("../main.rs")),
            ("ledger/busy.rs", include_str!("busy.rs")),
            ("ledger/finding_types.rs", include_str!("finding_types.rs")),
            ("ledger/task_state.rs", include_str!("task_state.rs")),
            ("ledger/types.rs", include_str!("types.rs")),
            ("ledger/connection.rs", include_str!("connection.rs")),
            ("ledger/schema.rs", include_str!("schema.rs")),
            ("ledger/tasks_create.rs", include_str!("tasks_create.rs")),
            ("ledger/tasks_query.rs", include_str!("tasks_query.rs")),
            ("ledger/claims.rs", include_str!("claims.rs")),
            ("ledger/requeue.rs", include_str!("requeue.rs")),
            ("ledger/finish_worker.rs", include_str!("finish_worker.rs")),
            ("ledger/finish_landing.rs", include_str!("finish_landing.rs")),
            ("ledger/park_retire.rs", include_str!("park_retire.rs")),
            ("ledger/runs.rs", include_str!("runs.rs")),
            ("ledger/findings.rs", include_str!("findings.rs")),
            ("ledger/reporting.rs", include_str!("reporting.rs")),
            ("ledger/reaping.rs", include_str!("reaping.rs")),
            ("ledger/governor.rs", include_str!("governor.rs")),
            ("ledger/verification.rs", include_str!("verification.rs")),
            ("ledger/landing.rs", include_str!("landing.rs")),
        ];
        for (name, src) in sources {
            // Scan production code only — a file's own `#[cfg(test)]` module
            // (this very test included, which necessarily talks ABOUT
            // `.file_finding(` in strings) is not a real call site.
            let src = production_code_only(src);
            for (lineno, line) in src.lines().enumerate() {
                if line.contains(".file_finding(") {
                    assert_eq!(
                        *name,
                        "policy.rs",
                        "unexpected bare `.file_finding(` call outside policy.rs \
                         at {name}:{}: {line}",
                        lineno + 1
                    );
                }
            }
        }
    }

    /// Cut a source file at its top-level `#[cfg(test)] mod tests` boundary
    /// (if any), keeping only what ships in the binary. Deliberately looks
    /// for the test *module* marker rather than any `#[cfg(test)]`
    /// occurrence — production code can carry its own cfg(test)-gated items
    /// (test-only hooks, helpers) ahead of the tests module, and cutting at
    /// the first such attribute would truncate real call sites out of scope.
    fn production_code_only(src: &str) -> &str {
        match src.find("\n#[cfg(test)]\nmod ") {
            Some(at) => &src[..at],
            None => src,
        }
    }

    fn refinery_source() -> String {
        [
            include_str!("../refinery/cargo.rs"),
            include_str!("../refinery/errors.rs"),
            include_str!("../refinery/land.rs"),
            include_str!("../refinery/manifest_base.rs"),
            include_str!("../refinery/manifest_live.rs"),
            include_str!("../refinery/preflight.rs"),
            include_str!("../refinery/rebase.rs"),
            include_str!("../refinery/recovery.rs"),
            include_str!("../refinery/reviews.rs"),
            include_str!("../refinery/version.rs"),
            include_str!("../refinery/version_fs.rs"),
            include_str!("../refinery/worktree.rs"),
            // Keep the facade last: `production_code_only` trims at its
            // test-module marker after scanning every production submodule.
            include_str!("../refinery/mod.rs"),
        ]
        .concat()
    }

    /// Every `FindingReason` variant a call site can actually choose is
    /// used somewhere — an added-but-unused variant is exactly the kind of
    /// drift this axis exists to prevent (a reason nobody ever files under
    /// is not a reason, it's dead documentation). `Unknown` is excluded: it
    /// exists for legacy pre-migration rows, not for any call site to pick.
    #[test]
    fn every_finding_reason_variant_is_used_at_a_call_site() {
        let refinery = refinery_source();
        let combined = [
            production_code_only(include_str!("../runner.rs")),
            refinery.as_str(),
            include_str!("../mcp.rs"),
            include_str!("../main.rs"),
            include_str!("busy.rs"),
            include_str!("finding_types.rs"),
            include_str!("task_state.rs"),
            include_str!("types.rs"),
            include_str!("connection.rs"),
            include_str!("schema.rs"),
            include_str!("tasks_create.rs"),
            include_str!("tasks_query.rs"),
            include_str!("claims.rs"),
            include_str!("requeue.rs"),
            include_str!("finish_worker.rs"),
            include_str!("finish_landing.rs"),
            include_str!("park_retire.rs"),
            include_str!("runs.rs"),
            include_str!("findings.rs"),
            include_str!("reporting.rs"),
            include_str!("reaping.rs"),
            include_str!("governor.rs"),
            include_str!("verification.rs"),
            include_str!("landing.rs"),
        ]
        .concat();
        for variant in [
            "VerifierRed",
            "SccacheBypassed",
            "BranchContract",
            "RebaseConflict",
            "ReviewRejected",
            "PolicyDenied",
            "InfraRefusal",
            "AgentAbandonedBackground",
            "LadderExhausted",
            "GovernorNoHeadroom",
            "TaskBudgetExhausted",
            "Operator",
            "UnknownStatus",
            "Retired",
            "OperatorReserved",
            "OperatorReleased",
            "AgentReported",
            "RungRefusal",
            "VersionBumpDiscarded",
        ] {
            assert!(
                combined.contains(&format!("FindingReason::{variant}")),
                "FindingReason::{variant} is never used at a call site"
            );
        }
    }

    #[test]
    fn task_budget_remainder_tracks_known_attempt_spend_without_parking() {
        let temp = tempfile::TempDir::new().unwrap();
        let db_path = temp.path().join("ledger.db");
        let ledger = Ledger::open(&db_path).unwrap();
        let id = ledger
            .add_task_scoped_with_budget(
                "budgeted task",
                "spec",
                "impl",
                "low",
                &[],
                TaskControls {
                    verifier_profile: "rust",
                    crates: &[],
                    operator_driven_reason: None,
                },
                Some(2.0),
            )
            .unwrap();
        let initial = ledger.task_budget_remainder(id).unwrap().unwrap();
        assert_eq!(initial.remaining_usd, 2.0);
        assert_eq!(ledger.task(id).unwrap().unwrap().budget_usd, Some(2.0));

        let run = ledger.store_run_start(id, "claude", None, None).unwrap();
        ledger
            .update_run_usage(
                run,
                &Usage {
                    input_tokens: 0,
                    fresh_input_tokens: None,
                    cache_read_input_tokens: None,
                    cache_creation_input_tokens: None,
                    output_tokens: 0,
                    cost_usd: Some(2.0),
                },
            )
            .unwrap();
        let exhausted = ledger.task_budget_remainder(id).unwrap().unwrap();
        assert_eq!(exhausted.charged_usd, 2.0);
        assert_eq!(exhausted.remaining_usd, 0.0);
        assert_eq!(ledger.task(id).unwrap().unwrap().status, "queued");
        assert!(ledger.task_findings(id).unwrap().is_empty());
    }

    #[test]
    fn task_budget_void_attempt_consumes_its_recorded_reservation() {
        let temp = tempfile::TempDir::new().unwrap();
        let ledger = Ledger::open(&temp.path().join("ledger.db")).unwrap();
        let id = ledger
            .add_task_scoped_with_budget(
                "budgeted task",
                "spec",
                "impl",
                "low",
                &[],
                TaskControls {
                    verifier_profile: "rust",
                    crates: &[],
                    operator_driven_reason: None,
                },
                Some(10.0),
            )
            .unwrap();
        let known = ledger.store_run_start(id, "claude", None, None).unwrap();
        ledger
            .update_run_usage(
                known,
                &Usage {
                    cost_usd: Some(2.0),
                    ..Default::default()
                },
            )
            .unwrap();
        ledger
            .conn
            .execute(
                "UPDATE runs SET delivery = 'delivered' WHERE id = ?1",
                [known],
            )
            .unwrap();
        let before_void = ledger.task_budget_remainder(id).unwrap().unwrap();
        assert_eq!(before_void.remaining_usd, 8.0);

        let unpriced = ledger.store_run_start(id, "claude", None, None).unwrap();
        ledger
            .conn
            .execute(
                "UPDATE runs SET reserved_usd = 1.5 WHERE id = ?1",
                [unpriced],
            )
            .unwrap();
        let void = ledger.delivery_void_fraction().unwrap();
        assert_eq!(void.contributing_runs, 2);
        assert_eq!(void.unknown_runs, 1);
        let after_void = ledger.task_budget_remainder(id).unwrap().unwrap();
        assert_eq!(after_void.charged_usd, 3.5);
        assert_eq!(after_void.remaining_usd, 6.5);
    }

    #[test]
    fn task_budget_must_be_finite_and_positive() {
        let temp = tempfile::TempDir::new().unwrap();
        let ledger = Ledger::open(&temp.path().join("ledger.db")).unwrap();
        for invalid in [0.0, -1.0, f64::INFINITY, f64::NAN] {
            ledger
                .add_task_scoped_with_budget(
                    "invalid budget",
                    "spec",
                    "impl",
                    "low",
                    &[],
                    TaskControls {
                        verifier_profile: "rust",
                        crates: &[],
                        operator_driven_reason: None,
                    },
                    Some(invalid),
                )
                .expect_err("invalid task budget must be refused");
        }

        let id = ledger
            .add_task("valid budget target", "spec", "impl", "low", &[], "rust")
            .unwrap();
        for invalid in [0.0, -1.0, f64::INFINITY, f64::NAN] {
            ledger
                .set_task_budget(id, Some(invalid))
                .expect_err("invalid replacement budget must be refused");
        }
        ledger.set_task_budget(id, Some(2.0)).unwrap();
        assert_eq!(ledger.task(id).unwrap().unwrap().budget_usd, Some(2.0));
        ledger.set_task_budget(id, None).unwrap();
        assert_eq!(ledger.task(id).unwrap().unwrap().budget_usd, None);
    }
