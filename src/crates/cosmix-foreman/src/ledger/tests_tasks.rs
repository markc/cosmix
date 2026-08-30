    #[test]
    fn absent_bump_reproduces_the_historical_risk_kind_matrix() {
        for risk in ["low", "medium", "high"] {
            for kind in ["impl", "bug", "feature", "breaking", "schema"] {
                let historical_minor =
                    risk == "high" || matches!(kind, "feature" | "breaking" | "schema");
                assert_eq!(
                    derived_version_bump(risk, kind),
                    if historical_minor {
                        VersionBump::Minor
                    } else {
                        VersionBump::Patch
                    },
                    "risk={risk}, kind={kind}"
                );
            }
        }
    }

    #[test]
    fn operator_driven_transition_and_finding_are_one_atomic_write() {
        let tmp = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
        let id = ledger
            .add_task("reserved work", "spec", "impl", "low", &[], "none")
            .unwrap();

        super::FAIL_OPERATOR_DRIVEN_FINDING_BEFORE_INSERT.with(|fail| fail.set(true));
        let error = ledger
            .set_operator_driven(id, true, "needs Mark's decision", "operator")
            .expect_err("injected finding failure must abort the transition");
        assert!(
            format!("{error:#}").contains("injected operator-driven finding failure"),
            "{error:#}"
        );
        assert!(
            !ledger.task(id).unwrap().unwrap().operator_driven,
            "the task-row write must roll back with the failed finding write"
        );
        let reserved_findings: i64 = ledger
            .conn
            .query_row(
                "SELECT COUNT(*) FROM findings
                 WHERE task_id = ?1 AND reason_code = 'operator_reserved'",
                rusqlite::params![id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            reserved_findings, 0,
            "neither half of the transaction landed"
        );

        assert!(
            ledger
                .set_operator_driven(id, true, "needs Mark's decision", "operator")
                .unwrap()
        );
        let finding: (i64, String, String, String, String) = ledger
            .conn
            .query_row(
                "SELECT task_id, body, filed_by, reason_code, severity
                 FROM findings WHERE task_id = ?1 AND reason_code = 'operator_reserved'",
                rusqlite::params![id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            finding,
            (
                id,
                "needs Mark's decision".into(),
                "operator".into(),
                "operator_reserved".into(),
                "info".into(),
            )
        );

        assert!(
            !ledger
                .set_operator_driven(id, true, "same state", "operator")
                .unwrap(),
            "setting the same state is a no-op"
        );
        let reserved_findings: i64 = ledger
            .conn
            .query_row(
                "SELECT COUNT(*) FROM findings
                 WHERE task_id = ?1 AND reason_code = 'operator_reserved'",
                rusqlite::params![id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            reserved_findings, 1,
            "a no-op must not duplicate the finding"
        );

        assert!(
            ledger
                .set_operator_driven(id, false, "Mark approved unattended work", "operator")
                .unwrap()
        );
        let released: (String, String, String) = ledger
            .conn
            .query_row(
                "SELECT body, filed_by, reason_code FROM findings
                 WHERE task_id = ?1 AND reason_code = 'operator_released'",
                rusqlite::params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            released,
            (
                "Mark approved unattended work".into(),
                "operator".into(),
                "operator_released".into(),
            )
        );
    }

    #[test]
    fn unexplained_operator_reservations_are_queryable() {
        let tmp = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
        let unexplained = ledger
            .add_task("legacy reservation", "spec", "impl", "low", &[], "none")
            .unwrap();
        ledger
            .conn
            .execute(
                "UPDATE tasks SET operator_driven = 1 WHERE id = ?1",
                rusqlite::params![unexplained],
            )
            .unwrap();
        assert_eq!(
            ledger.unexplained_operator_driven_task_ids().unwrap(),
            vec![unexplained]
        );

        ledger
            .file_finding_reasoned(
                Some(unexplained),
                "info",
                "reserved after repeated attempts",
                "needs redesign",
                "operator",
                FindingReason::Operator,
            )
            .unwrap();
        assert!(
            ledger
                .unexplained_operator_driven_task_ids()
                .unwrap()
                .is_empty(),
            "the narrow legacy hand-filed convention counts as an explanation"
        );
    }

    #[test]
    fn schema_14_columns_and_version_stamp_roll_back_together() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("schema-14-atomic.db");
        drop(Ledger::open(&db).unwrap());
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "ALTER TABLE tasks DROP COLUMN review_rejections;
             ALTER TABLE tasks DROP COLUMN branch_contract_failures;
             ALTER TABLE tasks DROP COLUMN dispatch_after;
             ALTER TABLE runs DROP COLUMN attempt;
             ALTER TABLE runs DROP COLUMN ladder_charge;
             ALTER TABLE runs DROP COLUMN ladder_charge_reason;
             PRAGMA user_version = 13;",
        )
        .unwrap();
        drop(conn);

        FAIL_SCHEMA_14_BEFORE_COMMIT.with(|fail| fail.set(true));
        let error = Ledger::open(&db).err().expect("migration must fail");
        assert!(error.to_string().contains("injected schema-14"));

        let conn = rusqlite::Connection::open(&db).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 13);
        for (table, column) in [
            ("tasks", "review_rejections"),
            ("tasks", "branch_contract_failures"),
            ("tasks", "dispatch_after"),
            ("runs", "attempt"),
            ("runs", "ladder_charge"),
            ("runs", "ladder_charge_reason"),
        ] {
            let present: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?1"),
                    [column],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(present, 0, "{table}.{column} escaped the rollback");
        }
    }

    #[test]
    fn dependency_cycle_helper_detects_hostile_existing_graph() {
        let graph = HashMap::from([(1, vec![2]), (2, vec![1])]);
        assert!(deps_form_cycle(&graph, 3, &[1]).is_some());
    }

    #[test]
    fn infrastructure_backoff_grows_and_is_bounded() {
        assert_eq!(infra_retry_backoff_secs(1), 30);
        assert_eq!(infra_retry_backoff_secs(2), 60);
        assert_eq!(infra_retry_backoff_secs(10), 300);
        assert_eq!(infra_retry_backoff_secs(60), 1_800);
        assert_eq!(infra_retry_backoff_secs(i64::MAX), 1_800);
    }

    fn task_with_two_infra_refusals(ledger: &Ledger) -> i64 {
        let id = ledger
            .add_task("counter reset", "spec", "impl", "low", &[], "none")
            .unwrap();
        let error = anyhow::anyhow!("temporary harness refusal");
        assert_eq!(
            ledger
                .note_infra_refusal(id, &error, 99, 100)
                .unwrap()
                .unwrap()
                .count,
            1
        );
        assert_eq!(
            ledger
                .note_infra_refusal(id, &error, 99, 100)
                .unwrap()
                .unwrap()
                .count,
            2
        );
        id
    }

    #[test]
    fn legacy_finish_claimed_resets_infra_refusals() {
        let tmp = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
        let id = task_with_two_infra_refusals(&ledger);
        let task = ledger.claim_task(id, "worker").unwrap();
        ledger
            .finish_claimed(
                id,
                ClaimToken {
                    owner: "worker",
                    generation: task.attempt,
                },
                TaskStatus::Bounced,
            )
            .unwrap();
        assert_eq!(ledger.task(id).unwrap().unwrap().infra_refusals, 0);
    }
    #[test]
    fn mcp_verified_completion_resets_infra_refusals() {
        let tmp = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
        let id = task_with_two_infra_refusals(&ledger);
        let task = ledger.claim_task(id, "mcp-agent").unwrap();
        assert!(
            ledger
                .complete_verified(
                    id,
                    ClaimToken {
                        owner: "mcp-agent",
                        generation: task.attempt,
                    },
                    ".",
                    "green",
                    true,
                    &[],
                )
                .unwrap()
        );
        assert_eq!(ledger.task(id).unwrap().unwrap().infra_refusals, 0);
    }

    #[test]
    fn abandoned_background_disposition_resets_infra_refusals() {
        let tmp = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
        let id = task_with_two_infra_refusals(&ledger);
        let task = ledger.claim_task(id, "worker").unwrap();
        ledger
            .finish_abandoned_background_at(
                id,
                ClaimToken {
                    owner: "worker",
                    generation: task.attempt,
                },
                "background process remained live",
                "2026-08-26T00:00:00Z",
            )
            .unwrap();
        assert_eq!(ledger.task(id).unwrap().unwrap().infra_refusals, 0);
    }
