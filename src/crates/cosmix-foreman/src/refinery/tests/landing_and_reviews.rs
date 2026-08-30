    #[test]
    fn bounce_finding_failure_rolls_back_disposition_and_suppresses_wake() {
        let tmp = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
        let id = ledger
            .add_task("bounce transaction", "spec", "impl", "low", &[], "none")
            .unwrap();
        let claimed = ledger.claim_task(id, "agent").unwrap();
        ledger.finish_task(id, "agent", "done").unwrap();
        assert!(ledger.transition_if(id, "done", "landing").unwrap());
        let report = LandingReport {
            task_id: id,
            branch: "task/bounce".into(),
            profile: "none".into(),
            landed: false,
            task_status: "bounced",
            detail: "branch content refused".into(),
            reason: FindingReason::BranchContract,
            finding_recorded: false,
            ladder_charged: false,
        };
        let woke = std::cell::Cell::new(false);
        crate::ledger::FAIL_LANDING_FINDING_BEFORE_INSERT.with(|fail| fail.set(true));
        let error = finish_landing_and_maybe_wake(
            &ledger,
            id,
            "bounced",
            None,
            &report,
            1,
            10,
            crate::ledger::DEFAULT_BRANCH_CONTRACT_LIMIT,
            chrono::Utc::now(),
            || woke.set(true),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("injected landing finding"));
        assert_eq!(ledger.task(id).unwrap().unwrap().status, "landing");
        assert!(ledger.task_findings(id).unwrap().is_empty());
        assert!(!woke.get());

        finish_landing_and_maybe_wake(
            &ledger,
            id,
            "bounced",
            None,
            &report,
            1,
            10,
            crate::ledger::DEFAULT_BRANCH_CONTRACT_LIMIT,
            chrono::Utc::now(),
            || {
                assert!(
                    ledger
                        .task_findings(id)
                        .unwrap()
                        .iter()
                        .any(|finding| finding.2 == "refinery bounce: task/bounce")
                );
                woke.set(true);
            },
        )
        .unwrap();
        assert!(woke.get());
        assert_eq!(ledger.task(id).unwrap().unwrap().status, "bounced");
        assert_eq!(claimed.attempt, 1);
    }

    #[test]
    fn repeated_merge_review_policy_denials_back_off_then_park() {
        let tmp = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
        let id = ledger
            .add_task("policy lane", "spec", "impl", "low", &[], "none")
            .unwrap();
        for count in 1..=3 {
            ledger.claim_task(id, "agent").unwrap();
            ledger.finish_task(id, "agent", "done").unwrap();
            let task = ledger.task(id).unwrap().unwrap();
            assert!(ledger.transition_if(id, "done", "landing").unwrap());
            let error = policy_denied(anyhow::anyhow!(
                "merge-review credential DEMO_REVIEW_TOKEN is missing"
            ));
            let report = landing_error_report(&task, &error);
            assert_eq!(report.reason, FindingReason::PolicyDenied);

            let disposition = finish_landing_and_maybe_wake(
                &ledger,
                id,
                "bounced",
                None,
                &report,
                3,
                10,
                crate::ledger::DEFAULT_BRANCH_CONTRACT_LIMIT,
                chrono::Utc::now(),
                || {},
            )
            .unwrap();
            assert_eq!(
                disposition.status.unwrap().as_db_str(),
                if count == 3 { "parked" } else { "bounced" }
            );
            let task = ledger.task(id).unwrap().unwrap();
            assert_eq!(task.branch_contract_failures, 0);
            assert_eq!(task.infra_refusals, 0);
            if count < 3 {
                assert!(task.dispatch_after.is_some());
            }
        }
        let task = ledger.task(id).unwrap().unwrap();
        assert_eq!(task.status, "parked");
        let findings = ledger.task_findings(id).unwrap();
        assert!(findings.iter().all(|finding| finding.1 == "blocker"));
        assert!(findings.iter().any(|finding| {
            finding.2 == "policy-denial retry limit reached"
                && finding.3.contains("DEMO_REVIEW_TOKEN")
        }));
        assert!(
            ledger
                .task_has_open_finding_reason(id, FindingReason::PolicyDenied)
                .unwrap()
        );
    }

    #[test]
    fn unrelated_dispatch_policy_denial_does_not_park_the_refinery_lane_early() {
        let tmp = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
        let id = ledger
            .add_task("policy lane", "spec", "impl", "low", &[], "none")
            .unwrap();
        // An unrelated dispatch-side policy denial (e.g. the routed agent's
        // lane lacks a credential) opens its own `policy_denied` finding
        // before this task ever reaches the refinery. It must not count
        // toward the refinery's OWN merge-review denial recurrence bound.
        ledger
            .file_finding_reasoned(
                Some(id),
                "blocker",
                "project policy denied the routed agent",
                "dispatch lane missing DEMO_DISPATCH_TOKEN",
                "dispatch",
                FindingReason::PolicyDenied,
            )
            .unwrap();

        for count in 1..=3 {
            ledger.claim_task(id, "agent").unwrap();
            ledger.finish_task(id, "agent", "done").unwrap();
            let task = ledger.task(id).unwrap().unwrap();
            assert!(ledger.transition_if(id, "done", "landing").unwrap());
            let error = policy_denied(anyhow::anyhow!(
                "merge-review credential DEMO_REVIEW_TOKEN is missing"
            ));
            let report = landing_error_report(&task, &error);
            assert_eq!(report.reason, FindingReason::PolicyDenied);

            let disposition = finish_landing_and_maybe_wake(
                &ledger,
                id,
                "bounced",
                None,
                &report,
                3,
                10,
                crate::ledger::DEFAULT_BRANCH_CONTRACT_LIMIT,
                chrono::Utc::now(),
                || {},
            )
            .unwrap();
            // With the unrelated dispatch finding wrongly diluting/inflating
            // the count, this would park on the SECOND refinery denial
            // instead of the third.
            assert_eq!(
                disposition.status.unwrap().as_db_str(),
                if count == 3 { "parked" } else { "bounced" },
                "unexpected status at refinery denial {count}"
            );
        }
        let task = ledger.task(id).unwrap().unwrap();
        assert_eq!(task.status, "parked");
    }

    #[test]
    fn successful_rebase_resolves_the_stale_handoff_finding() {
        let tmp = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
        let task = ledger
            .add_task("rebase", "spec", "impl", "low", &[], "none")
            .unwrap();
        ledger
            .file_finding_reasoned(
                Some(task),
                "major",
                "rebase conflict",
                "resolve this on the next attempt",
                "dispatch",
                FindingReason::RebaseConflict,
            )
            .unwrap();

        resolve_completed_rebase(
            &ledger,
            task,
            &RebaseOutcome::AlreadyOnBase {
                base: "base-sha".into(),
            },
        )
        .unwrap();

        assert!(ledger.open_findings(10).unwrap().is_empty());
    }

    #[test]
    fn project_lane_policy_constrains_merge_review_route() {
        let specs = [ReviewSpec {
            reviewer: crate::executor::AgentKind::Claude,
            model: "review-model".into(),
        }];
        let codex_only = crate::manifest::ProjectLanePolicy {
            name: "demo".into(),
            lanes: vec![crate::manifest::LaneSpec {
                agent: crate::executor::AgentKind::Codex,
                credentials: Vec::new(),
            }],
            push_remote: None,
        };
        let error = validate_review_lanes(&specs, Some(&codex_only), |_| true).unwrap_err();
        assert!(error.to_string().contains("merge-review lane claude"));
        let fault = error.downcast_ref::<LandingTaskFault>().unwrap();
        assert_eq!(fault.reason, FindingReason::PolicyDenied);

        let credentialled = crate::manifest::ProjectLanePolicy {
            name: "demo".into(),
            lanes: vec![crate::manifest::LaneSpec {
                agent: crate::executor::AgentKind::Claude,
                credentials: vec!["DEMO_REVIEW_TOKEN".into()],
            }],
            push_remote: None,
        };
        assert!(validate_review_lanes(&specs, Some(&credentialled), |_| false).is_err());
        assert!(
            validate_review_lanes(&specs, Some(&credentialled), |name| name
                == "DEMO_REVIEW_TOKEN")
            .is_ok()
        );
    }

    #[test]
    fn review_resume_lookup_failure_aborts_all_prepared_arms_and_releases_holds() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]).unwrap();
        git(&repo, &["config", "user.name", "test"]).unwrap();
        git(&repo, &["config", "user.email", "test@example.com"]).unwrap();
        std::fs::write(repo.join("a.rs"), "fn base() {}\n").unwrap();
        git(&repo, &["add", "."]).unwrap();
        git(&repo, &["commit", "-m", "base"]).unwrap();
        let base = git(&repo, &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();
        std::fs::write(repo.join("a.rs"), "fn changed() {}\n").unwrap();
        git(&repo, &["add", "."]).unwrap();
        git(&repo, &["commit", "-m", "tip"]).unwrap();
        let tip = git(&repo, &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();

        let db = tmp.path().join("ledger.db");
        let ledger = Ledger::open(&db).unwrap();
        let task_id = ledger
            .add_task("lookup failure", "spec", "impl", "high", &[], "none")
            .unwrap();
        let task = ledger.task(task_id).unwrap().unwrap();
        let policy =
            crate::config::FleetPolicy::load_with(tmp.path().join("absent-foreman.conf"), |_| None)
                .unwrap();
        let opts = RefineOptions {
            repo: repo.clone(),
            project_root: None,
            integration: "main".into(),
            subdir: ".".into(),
            tier: 0,
            review: true,
            db,
            echo: false,
            fleet_policy: Some(policy.clone()),
            profiles: Vec::new(),
            project_pack: String::new(),
            landing_gate: None,
            lane_policy: None,
        };
        let profile = crate::verify::lookup_profile("none").unwrap();
        let specs = [
            ReviewSpec {
                reviewer: crate::executor::AgentKind::Claude,
                model: "opus".into(),
            },
            ReviewSpec {
                reviewer: crate::executor::AgentKind::Codex,
                model: "gpt-test".into(),
            },
        ];
        crate::ledger::fail_next_last_run_ref_for_test();
        let error = run_landing_reviews(
            &LandingReviewContext {
                ledger: &ledger,
                task: &task,
                opts: &opts,
                fleet_policy: &policy,
                worktree: &repo,
                base: &base,
                tip: &tip,
                touches_foreman: false,
                profile: &profile,
            },
            &specs,
        )
        .unwrap_err();
        assert!(error.to_string().contains("injected last_run_ref failure"));
        assert_eq!(ledger.reserved_totals().unwrap(), (0.0, 0));

        let review_runs = ledger
            .recent_runs(10)
            .unwrap()
            .into_iter()
            .filter(|run| run.task_id == task_id && run.role == "review")
            .collect::<Vec<_>>();
        assert_eq!(review_runs.len(), 2);
        assert!(review_runs.iter().all(|run| {
            run.verdict.as_deref() == Some("error")
                && run.delivery == "harness_error"
                && run
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("review resume lookup failed"))
        }));
    }

    #[test]
    fn verifier_directory_host_io_is_infrastructure_not_verifier_red() {
        let error = anyhow::Error::new(std::io::Error::from_raw_os_error(libc::EIO))
            .context("canonicalizing verifier directory");
        assert!(verifier_directory_error_is_infrastructure(&error));
        let error = infrastructure(error);
        let task = version_task(&tempfile::tempdir().unwrap().path().join("verifier-dir.db"));
        let report = landing_error_report(&task, &error);
        assert_eq!(report.reason, FindingReason::InfraRefusal);

        let missing = anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::NotFound));
        assert!(!verifier_directory_error_is_infrastructure(&missing));
    }

    #[test]
    fn exact_approved_review_is_reused_only_for_the_same_attempt_and_tips() {
        let tmp = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
        let id = ledger
            .add_task("review retry", "spec", "impl", "low", &[], "none")
            .unwrap();
        let task = ledger.task(id).unwrap().unwrap();
        let approved = serde_json::json!({
            "kind": "review",
            "base": "base-a",
            "tip": "tip-a",
            "approve": true,
        });
        ledger
            .record_verification(id, 3, true, &approved.to_string())
            .unwrap();
        ledger
            .record_verification(id, 3, false, "legacy prose")
            .unwrap();
        for suffix in 0..11 {
            let unrelated = serde_json::json!({
                "kind": "review",
                "base": "base-a",
                "tip": format!("other-tip-{suffix}"),
                "approve": true,
            });
            ledger
                .record_verification(id, 3, true, &unrelated.to_string())
                .unwrap();
        }

        assert!(recorded_approved_review(&ledger, &task, "base-a", "tip-a").unwrap());
        assert!(!recorded_approved_review(&ledger, &task, "base-a", "tip-b").unwrap());

        let rejected = serde_json::json!({
            "kind": "two-arm-review",
            "base": "base-a",
            "tip": "tip-a",
            "approve": false,
        });
        ledger
            .record_verification(id, 3, false, &rejected.to_string())
            .unwrap();
        assert!(!recorded_approved_review(&ledger, &task, "base-a", "tip-a").unwrap());
    }

    #[test]
    fn recovered_review_evidence_uses_the_strongest_delivered_verdict() {
        let delivered_reject = serde_json::json!({
            "approve": false,
            "arms": [
                {"delivery": "delivered", "verdict": "REJECT"},
                {"delivery": "harness_error", "verdict": null},
            ],
        });
        assert!(recorded_review_has_delivered_reject(&delivered_reject));

        for evidence in [
            serde_json::json!({
                "approve": false,
                "arms": [
                    {"delivery": "vendor_error", "verdict": null},
                    {"delivery": "harness_error", "verdict": null},
                ],
            }),
            serde_json::json!({
                "approve": false,
                "arms": [
                    {"delivery": "delivered", "verdict": "APPROVE"},
                    {"delivery": "harness_error", "verdict": null},
                ],
            }),
        ] {
            assert!(!recorded_review_has_delivered_reject(&evidence));
        }
    }

    #[test]
    fn legacy_review_checkout_recreates_same_path_and_cleans_up_each_landing() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]).unwrap();
        git(&repo, &["config", "user.name", "test"]).unwrap();
        git(&repo, &["config", "user.email", "test@example.com"]).unwrap();
        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        git(&repo, &["add", "."]).unwrap();
        git(&repo, &["commit", "-m", "base"]).unwrap();
        git(&repo, &["checkout", "-b", "task/review-retry"]).unwrap();
        std::fs::write(repo.join("task.txt"), "task\n").unwrap();
        git(&repo, &["add", "."]).unwrap();
        let task_commit = Command::new("git")
            .args(["commit", "-m", "task change"])
            .env("GIT_AUTHOR_DATE", "2001-02-03T04:05:06+00:00")
            .env("GIT_COMMITTER_DATE", "2002-03-04T05:06:07+00:00")
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(
            task_commit.status.success(),
            "{}",
            String::from_utf8_lossy(&task_commit.stderr)
        );
        git(&repo, &["checkout", "main"]).unwrap();
        std::fs::write(repo.join("main.txt"), "main\n").unwrap();
        git(&repo, &["add", "."]).unwrap();
        git(&repo, &["commit", "-m", "integration change"]).unwrap();
        let base = git(&repo, &["rev-parse", "main"]).unwrap();

        let db = tmp.path().join("ledger.db");
        let ledger = Ledger::open(&db).unwrap();
        let task_id = ledger
            .add_task("legacy review", "spec", "impl", "low", &[], "none")
            .unwrap();
        assert_eq!(task_id, 1);
        let task = ledger.task(task_id).unwrap().unwrap();
        let argv_log = tmp.path().join("legacy-review.argv");
        let reviewer = tmp.path().join("legacy-reviewer");
        crate::fixture::write_executable(
            &reviewer,
            format!(
                "#!/bin/sh\nprintf '%s\\0' \"$@\" >> '{argv_log}'\nprintf '\\36' >> '{argv_log}'\nprintf '%s\\n' \
                 '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"legacy-thread\"}}' \
                 '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"Reviewed.\\n{{\\\"verdict\\\":\\\"APPROVE\\\",\\\"findings\\\":[],\\\"files_inspected\\\":[\\\"task.txt\\\"]}}\",\"session_id\":\"legacy-thread\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}},\"total_cost_usd\":0.0}}'\n",
                argv_log = argv_log.display(),
            ),
        );
        let mut policy =
            crate::config::FleetPolicy::load_with(tmp.path().join("absent-foreman.conf"), |_| None)
                .unwrap();
        policy.claude_bin.value = reviewer.to_string_lossy().into_owned();
        let opts = RefineOptions {
            repo: repo.clone(),
            project_root: None,
            integration: "main".into(),
            subdir: ".".into(),
            tier: 0,
            review: true,
            db,
            echo: false,
            fleet_policy: Some(policy.clone()),
            profiles: Vec::new(),
            project_pack: String::new(),
            landing_gate: None,
            lane_policy: None,
        };
        let profile = crate::verify::lookup_profile("none").unwrap();
        let specs = [ReviewSpec {
            reviewer: crate::executor::AgentKind::Claude,
            model: "opus".into(),
        }];
        let first = TempWorktree::add_or_reuse_task(&repo, 1, "task/review-retry", None).unwrap();
        let stable_path = first.path.clone();
        assert_eq!(
            rebase_for_landing(&first.path, base.trim()).unwrap().0,
            Some(0)
        );
        let first_tip = git(&first.path, &["rev-parse", "HEAD"]).unwrap();
        let first_dates = git(&first.path, &["show", "-s", "--format=%aI%n%cI", "HEAD"]).unwrap();
        let dates = first_dates.lines().collect::<Vec<_>>();
        assert_eq!(dates.len(), 2);
        assert_eq!(dates[0], dates[1]);
        let first_tip = first_tip.trim().to_string();
        let context = LandingReviewContext {
            ledger: &ledger,
            task: &task,
            opts: &opts,
            fleet_policy: &policy,
            worktree: &first.path,
            base: base.trim(),
            tip: &first_tip,
            touches_foreman: false,
            profile: &profile,
        };
        assert!(run_landing_reviews(&context, &specs).unwrap().approve);
        drop(first);
        assert!(
            !stable_path.exists(),
            "legacy checkout must be removed after the first landing"
        );
        assert!(
            !git(&repo, &["worktree", "list", "--porcelain"])
                .unwrap()
                .contains(stable_path.to_str().unwrap()),
            "legacy checkout registration survived the first landing"
        );

        let second = TempWorktree::add_or_reuse_task(&repo, 1, "task/review-retry", None).unwrap();
        assert_eq!(
            second.path, stable_path,
            "successive legacy landings must recreate the same reviewer cwd"
        );
        assert_eq!(
            rebase_for_landing(&second.path, base.trim()).unwrap().0,
            Some(0)
        );
        let second_tip = git(&second.path, &["rev-parse", "HEAD"]).unwrap();
        assert_eq!(first_tip, second_tip.trim());
        let second_tip = second_tip.trim().to_string();
        let context = LandingReviewContext {
            ledger: &ledger,
            task: &task,
            opts: &opts,
            fleet_policy: &policy,
            worktree: &second.path,
            base: base.trim(),
            tip: &second_tip,
            touches_foreman: false,
            profile: &profile,
        };
        assert!(run_landing_reviews(&context, &specs).unwrap().approve);
        drop(second);
        assert!(
            !stable_path.exists(),
            "legacy checkout must be removed after the second landing"
        );
        assert!(
            !git(&repo, &["worktree", "list", "--porcelain"])
                .unwrap()
                .contains(stable_path.to_str().unwrap()),
            "legacy checkout registration survived the second landing"
        );
        let argv = std::fs::read(&argv_log).unwrap();
        assert!(
            argv.windows(b"--resume\0legacy-thread\0".len())
                .any(|window| window == b"--resume\0legacy-thread\0"),
            "second legacy review did not continue its first thread: {:?}",
            String::from_utf8_lossy(&argv)
        );
    }
