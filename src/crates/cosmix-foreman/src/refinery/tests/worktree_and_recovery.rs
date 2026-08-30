    #[test]
    fn registered_task_worktree_is_reused_for_reviewer_resume() {
        // A task with a registered `task-<id>` checkout is exactly the case
        // reviewer session resume relies on: the SAME path is handed to
        // `--resume`/`exec resume` on every landing sweep. The legacy test
        // above now proves the same path identity for an unregistered task.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]).unwrap();
        git(&repo, &["config", "user.name", "test"]).unwrap();
        git(&repo, &["config", "user.email", "test@example.com"]).unwrap();
        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        git(&repo, &["add", "."]).unwrap();
        git(&repo, &["commit", "-m", "base"]).unwrap();
        git(&repo, &["checkout", "-b", "task/stable-72"]).unwrap();
        git(&repo, &["checkout", "main"]).unwrap();

        let registered = tmp.path().join("task-72");
        git(
            &repo,
            &[
                "worktree",
                "add",
                registered.to_str().unwrap(),
                "task/stable-72",
            ],
        )
        .unwrap();

        let wt = TempWorktree::add_or_reuse_task(&repo, 72, "task/stable-72", None).unwrap();
        assert_eq!(wt.path, registered);
    }

    #[test]
    fn landing_transition_retries_sqlite_busy_and_proceeds() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]).unwrap();
        git(&repo, &["config", "user.name", "test"]).unwrap();
        git(&repo, &["config", "user.email", "test@example.com"]).unwrap();
        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        git(&repo, &["add", "."]).unwrap();
        git(&repo, &["commit", "-m", "base"]).unwrap();
        git(&repo, &["checkout", "-b", "task/busy"]).unwrap();
        std::fs::write(repo.join("landed.txt"), "landed\n").unwrap();
        git(&repo, &["add", "."]).unwrap();
        git(&repo, &["commit", "-m", "landing"]).unwrap();
        git(&repo, &["checkout", "main"]).unwrap();

        let db = tmp.path().join("ledger.db");
        let ledger = Ledger::open(&db).unwrap();
        let id = ledger
            .add_task("busy landing", "spec", "impl", "low", &[], "none")
            .unwrap();
        let claimed = ledger.claim_task(id, "test").unwrap();
        ledger
            .set_task_workspace(
                id,
                crate::ledger::ClaimToken {
                    owner: "test",
                    generation: claimed.attempt,
                },
                None,
                Some("task/busy"),
            )
            .unwrap();
        ledger.finish_task(id, "test", "done").unwrap();
        // Keep each SQLite attempt short so this test exercises the
        // refinery's bounded retry rather than the connection's normal
        // five-second busy timeout.
        ledger
            .set_busy_timeout_for_test(Duration::from_millis(1))
            .unwrap();

        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let holder_db = db.clone();
        let holder = std::thread::spawn(move || {
            let conn = rusqlite::Connection::open(holder_db).unwrap();
            conn.execute_batch("BEGIN IMMEDIATE").unwrap();
            ready_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(120));
            conn.execute_batch("COMMIT").unwrap();
        });
        ready_rx.recv().unwrap();

        let reports = refine(
            &ledger,
            &RefineOptions {
                repo: repo.clone(),
                project_root: None,
                integration: "main".into(),
                subdir: ".".into(),
                tier: 0,
                review: false,
                db,
                echo: false,
                fleet_policy: None,
                profiles: Vec::new(),
                project_pack: String::new(),
                landing_gate: None,
                lane_policy: None,
            },
        )
        .unwrap();
        holder.join().unwrap();

        assert_eq!(reports.len(), 1);
        assert!(reports[0].landed, "{}", reports[0].detail);
        assert_eq!(ledger.task(id).unwrap().unwrap().status, "landed");
        assert!(repo.join("landed.txt").exists());
    }

    #[test]
    fn unannotated_landing_error_bounces_and_queue_continues() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]).unwrap();
        git(&repo, &["config", "user.name", "test"]).unwrap();
        git(&repo, &["config", "user.email", "test@example.com"]).unwrap();
        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        git(&repo, &["add", "."]).unwrap();
        git(&repo, &["commit", "-m", "base"]).unwrap();
        let base = git(&repo, &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();
        for (branch, file) in [
            ("task/unannotated", "first.txt"),
            ("task/continues", "second.txt"),
        ] {
            git(&repo, &["checkout", "-b", branch, &base]).unwrap();
            std::fs::write(repo.join(file), "change\n").unwrap();
            git(&repo, &["add", "."]).unwrap();
            git(&repo, &["commit", "-m", branch]).unwrap();
        }
        git(&repo, &["checkout", "main"]).unwrap();

        let db = tmp.path().join("ledger.db");
        let ledger = Ledger::open(&db).unwrap();
        for (title, branch) in [
            ("plain error", "task/unannotated"),
            ("later task", "task/continues"),
        ] {
            let id = ledger
                .add_task(title, "spec", "impl", "low", &[], "none")
                .unwrap();
            let claimed = ledger.claim_task(id, "agent").unwrap();
            ledger
                .set_task_workspace(
                    id,
                    crate::ledger::ClaimToken {
                        owner: "agent",
                        generation: claimed.attempt,
                    },
                    None,
                    Some(branch),
                )
                .unwrap();
            ledger.finish_task(id, "agent", "done").unwrap();
        }
        let policy =
            crate::config::FleetPolicy::load_with(tmp.path().join("absent-foreman.conf"), |_| None)
                .unwrap();
        FAIL_NEXT_LANDING_UNANNOTATED.with(|fail| fail.set(true));
        let reports = refine(
            &ledger,
            &RefineOptions {
                repo: repo.clone(),
                project_root: None,
                integration: "main".into(),
                subdir: ".".into(),
                tier: 0,
                review: false,
                db,
                echo: false,
                fleet_policy: Some(policy),
                profiles: Vec::new(),
                project_pack: String::new(),
                landing_gate: None,
                lane_policy: None,
            },
        )
        .unwrap();

        assert_eq!(reports.len(), 2);
        assert!(!reports[0].landed);
        assert_eq!(reports[0].reason, FindingReason::BranchContract);
        assert!(
            reports[0]
                .detail
                .contains("injected unannotated landing-path failure")
        );
        assert!(reports[1].landed, "{}", reports[1].detail);
        assert_eq!(ledger.task(1).unwrap().unwrap().status, "bounced");
        assert_eq!(ledger.task(2).unwrap().unwrap().status, "landed");
        assert!(repo.join("second.txt").exists());
    }

    #[test]
    fn dirt_check_ignores_gitignored_build_output_anywhere_in_the_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]).unwrap();
        git(&repo, &["config", "user.name", "test"]).unwrap();
        git(&repo, &["config", "user.email", "test@example.com"]).unwrap();
        std::fs::write(repo.join(".gitignore"), "target/\nCargo.lock\n/late.txt\n").unwrap();
        std::fs::create_dir_all(repo.join("fixture")).unwrap();
        std::fs::write(repo.join("fixture/keep.txt"), "keep\n").unwrap();
        git(&repo, &["add", "."]).unwrap();
        git(&repo, &["commit", "-m", "base"]).unwrap();

        std::fs::create_dir_all(repo.join("fixture/target")).unwrap();
        std::fs::write(repo.join("fixture/target/warm"), "warm\n").unwrap();
        std::fs::create_dir_all(repo.join("target")).unwrap();
        std::fs::write(repo.join("target/warm"), "warm\n").unwrap();
        std::fs::write(repo.join("fixture/Cargo.lock"), "generated\n").unwrap();

        let allowed = vec![repo.join("target")];
        let dirty = worktree_dirt_except_targets(&repo, &allowed).unwrap();
        assert!(
            dirty.is_empty(),
            "ignored build output outside the pinned target must not read as dirt: {dirty:?}"
        );

        std::fs::write(repo.join("stray.txt"), "oops\n").unwrap();
        let dirty = worktree_dirt_except_targets(&repo, &allowed).unwrap();
        assert_eq!(
            dirty.len(),
            1,
            "untracked file must read as dirt: {dirty:?}"
        );
        assert!(dirty[0].contains("stray.txt"), "{dirty:?}");
        std::fs::remove_file(repo.join("stray.txt")).unwrap();

        std::fs::write(repo.join("late.txt"), "late\n").unwrap();
        let dirty = worktree_dirt_except_targets(&repo, &allowed).unwrap();
        assert_eq!(
            dirty.len(),
            1,
            "non-target ignored path must read as dirt: {dirty:?}"
        );
        assert!(dirty[0].contains("late.txt"), "{dirty:?}");
    }

    fn push_journal_landing_task(ledger: &Ledger, branch: &str) -> (i64, i64) {
        let id = ledger
            .add_task("push recovery", "spec", "impl", "low", &[], "none")
            .unwrap();
        let claimed = ledger.claim_task(id, "test").unwrap();
        ledger
            .set_task_workspace(
                id,
                crate::ledger::ClaimToken {
                    owner: "test",
                    generation: claimed.attempt,
                },
                None,
                Some(branch),
            )
            .unwrap();
        ledger.finish_task(id, "test", "done").unwrap();
        assert!(ledger.transition_if(id, "done", "landing").unwrap());
        (id, claimed.attempt)
    }

    struct UpdatePushFixture {
        _tmp: tempfile::TempDir,
        repo: PathBuf,
        remote: PathBuf,
        ledger: Ledger,
        task_id: i64,
        attempt: i64,
        base: String,
        verified_tip: String,
        update: PushIntent,
        delete: PushIntent,
    }

    fn fixture_git(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(repo)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LC_ALL", "C")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "/bin/false")
            .env("GIT_AUTHOR_NAME", "Foreman Test")
            .env("GIT_AUTHOR_EMAIL", "foreman@example.test")
            .env("GIT_COMMITTER_NAME", "Foreman Test")
            .env("GIT_COMMITTER_EMAIL", "foreman@example.test")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed in {}: {}",
            repo.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn update_push_fixture() -> UpdatePushFixture {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let remote = tmp.path().join("publish.git");
        std::fs::create_dir(&repo).unwrap();
        fixture_git(&repo, &["init", "-b", "main"]);
        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        fixture_git(&repo, &["add", "."]);
        fixture_git(&repo, &["commit", "-m", "base"]);
        let base = fixture_git(&repo, &["rev-parse", "HEAD"]);
        fixture_git(tmp.path(), &["init", "--bare", remote.to_str().unwrap()]);
        fixture_git(
            &repo,
            &["push", remote.to_str().unwrap(), "HEAD:refs/heads/main"],
        );

        std::fs::write(repo.join("verified.txt"), "verified\n").unwrap();
        fixture_git(&repo, &["add", "."]);
        fixture_git(&repo, &["commit", "-m", "verified"]);
        let verified_tip = fixture_git(&repo, &["rev-parse", "HEAD"]);
        fixture_git(
            &repo,
            &[
                "push",
                remote.to_str().unwrap(),
                "HEAD:refs/heads/task/117",
            ],
        );

        let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
        let (task_id, attempt) = push_journal_landing_task(&ledger, "task/117");
        let (update, delete) = ledger
            .record_push_intents_before_landing(task_id, attempt, "main", &verified_tip)
            .unwrap();
        UpdatePushFixture {
            _tmp: tmp,
            repo,
            remote,
            ledger,
            task_id,
            attempt,
            base,
            verified_tip,
            update,
            delete,
        }
    }

    fn fixture_delivery(remote: &Path) -> PushDelivery {
        PushDelivery {
            remote: remote.to_string_lossy().into_owned(),
            credentials: vec![(
                "PUBLISH_TOKEN".into(),
                std::ffi::OsString::from("fixture-token"),
            )],
        }
    }

    #[test]
    fn verified_sha_update_push_succeeds_and_resolves_its_journal_row() {
        let fixture = update_push_fixture();
        let hook = fixture.remote.join("hooks/pre-receive");
        std::fs::write(
            &hook,
            "#!/bin/sh\nset -eu\ntest \"${HOME+x}\" != x\ntest \"$PUBLISH_TOKEN\" = fixture-token\n",
        )
        .unwrap();
        let mut mode = std::fs::metadata(&hook).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        mode.set_mode(0o755);
        std::fs::set_permissions(&hook, mode).unwrap();

        let before = fixture
            .ledger
            .push_intents_for_attempt(fixture.task_id, fixture.attempt)
            .unwrap();
        assert_eq!(before.len(), 2);
        assert_eq!(before[0].outcome, PushIntentOutcome::Unknown);
        assert_eq!(
            before[0].refspec,
            format!("{}:refs/heads/main", fixture.verified_tip)
        );

        let outcome = deliver_update_push(
            &fixture.ledger,
            &fixture.repo,
            &fixture_delivery(&fixture.remote),
            &fixture.update,
        );
        assert_eq!(
            outcome,
            Some(crate::remote_git::RemoteOutcome::Succeeded)
        );

        assert_eq!(
            fixture_git(&fixture.remote, &["rev-parse", "refs/heads/main"]),
            fixture.verified_tip
        );
        let after = fixture
            .ledger
            .push_intents_for_attempt(fixture.task_id, fixture.attempt)
            .unwrap();
        assert_eq!(after[0].outcome, PushIntentOutcome::Succeeded);
        assert_eq!(after[1].outcome, PushIntentOutcome::Unknown);
    }

    #[test]
    fn moving_the_local_branch_cannot_replace_the_sha_and_is_recorded_rejected() {
        let fixture = update_push_fixture();
        fixture_git(
            &fixture.repo,
            &["checkout", "-b", "racing", &fixture.base],
        );
        std::fs::write(fixture.repo.join("racing.txt"), "racing\n").unwrap();
        fixture_git(&fixture.repo, &["add", "."]);
        fixture_git(&fixture.repo, &["commit", "-m", "racing"]);
        let racing_tip = fixture_git(&fixture.repo, &["rev-parse", "HEAD"]);
        fixture_git(
            &fixture.repo,
            &[
                "push",
                fixture.remote.to_str().unwrap(),
                "HEAD:refs/heads/main",
            ],
        );
        fixture_git(
            &fixture.repo,
            &[
                "update-ref",
                "refs/heads/main",
                &racing_tip,
                &fixture.verified_tip,
            ],
        );
        assert_eq!(
            fixture_git(&fixture.repo, &["rev-parse", "refs/heads/main"]),
            racing_tip,
            "a name-based push would now silently select the racing tip"
        );
        assert_eq!(
            fixture
                .ledger
                .push_intents_for_attempt(fixture.task_id, fixture.attempt)
                .unwrap()[0]
                .outcome,
            PushIntentOutcome::Unknown
        );

        let outcome = deliver_update_push(
            &fixture.ledger,
            &fixture.repo,
            &fixture_delivery(&fixture.remote),
            &fixture.update,
        );
        assert_eq!(outcome, Some(crate::remote_git::RemoteOutcome::Failed));

        let after = fixture
            .ledger
            .push_intents_for_attempt(fixture.task_id, fixture.attempt)
            .unwrap();
        assert_eq!(after[0].outcome, PushIntentOutcome::Failed);
        assert!(after[0].detail.contains("[rejected]"), "{}", after[0].detail);
        assert_eq!(
            fixture_git(&fixture.remote, &["rev-parse", "refs/heads/main"]),
            racing_tip,
            "the immutable verified SHA must be rejected, never replaced with the moved branch"
        );
    }

    #[test]
    fn ambiguous_exit_stays_unknown_in_the_existing_journal_row() {
        let fixture = update_push_fixture();
        let before = fixture
            .ledger
            .push_intents_for_attempt(fixture.task_id, fixture.attempt)
            .unwrap();
        assert_eq!(before.len(), 2);
        assert_eq!(before[0].id, fixture.update.id);
        assert_eq!(before[0].outcome, PushIntentOutcome::Unknown);

        let run = crate::remote_git::RemoteGitRun {
            outcome: crate::remote_git::RemoteOutcome::Unknown,
            termination: crate::remote_git::RemoteGitTermination::Exited(75),
            stdout: Vec::new(),
            stderr: b"connection lost after send".to_vec(),
            stdout_truncated: false,
            stderr_truncated: false,
            io_error: None,
        };
        assert!(record_update_push_run(&fixture.ledger, &fixture.update, &run).unwrap());

        let after = fixture
            .ledger
            .push_intents_for_attempt(fixture.task_id, fixture.attempt)
            .unwrap();
        assert_eq!(after.len(), 2, "recording must update, never replace, the row");
        assert_eq!(after[0].id, before[0].id);
        assert_eq!(after[0].outcome, PushIntentOutcome::Unknown);
        assert!(after[0].detail.contains("connection lost after send"));
    }

    #[test]
    fn successful_remote_prune_uses_and_resolves_the_delete_journal_row() {
        let fixture = update_push_fixture();
        let before = fixture
            .ledger
            .push_intents_for_attempt(fixture.task_id, fixture.attempt)
            .unwrap();
        assert_eq!(before.len(), 2);
        assert_eq!(before[0].kind, crate::ledger::PushIntentKind::Update);
        assert_eq!(before[1].kind, crate::ledger::PushIntentKind::Delete);
        assert_eq!(before[1].id, fixture.delete.id);
        assert_eq!(before[1].refspec, ":refs/heads/task/117");

        deliver_remote_pushes(
            &fixture.ledger,
            &fixture.repo,
            &fixture_delivery(&fixture.remote),
            &fixture.update,
            &fixture.delete,
        );

        assert_eq!(
            fixture_git(&fixture.remote, &["rev-parse", "refs/heads/main"]),
            fixture.verified_tip
        );
        let task_branch = Command::new("git")
            .args(["show-ref", "--verify", "refs/heads/task/117"])
            .current_dir(&fixture.remote)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LC_ALL", "C")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(!task_branch.status.success(), "the task branch must be pruned");

        let after = fixture
            .ledger
            .push_intents_for_attempt(fixture.task_id, fixture.attempt)
            .unwrap();
        assert_eq!(after.len(), 2, "delivery must update the existing rows");
        assert_eq!(after[0].kind, crate::ledger::PushIntentKind::Update);
        assert_eq!(after[0].outcome, PushIntentOutcome::Succeeded);
        assert_eq!(after[1].id, fixture.delete.id);
        assert_eq!(after[1].kind, crate::ledger::PushIntentKind::Delete);
        assert_eq!(after[1].outcome, PushIntentOutcome::Succeeded);
    }

    #[test]
    fn caller_supplied_remote_branch_name_is_refused_before_git_runs() {
        let fixture = update_push_fixture();
        fixture_git(
            &fixture.repo,
            &[
                "push",
                fixture.remote.to_str().unwrap(),
                "HEAD:refs/heads/someone-elses-work",
            ],
        );
        let forged = PushIntent {
            refspec: ":refs/heads/someone-elses-work".into(),
            ..fixture.delete.clone()
        };

        let error = deliver_delete_push(
            &fixture.ledger,
            &fixture.repo,
            &fixture_delivery(&fixture.remote),
            &forged,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("refusing caller-supplied"),
            "{error:#}"
        );
        assert_eq!(
            fixture_git(
                &fixture.remote,
                &["rev-parse", "refs/heads/someone-elses-work"]
            ),
            fixture.verified_tip,
            "the caller-selected branch must survive"
        );
        assert_eq!(
            fixture_git(&fixture.remote, &["rev-parse", "refs/heads/task/117"]),
            fixture.verified_tip,
            "refusal must not fall through to the recorded branch either"
        );
        let rows = fixture
            .ledger
            .push_intents_for_attempt(fixture.task_id, fixture.attempt)
            .unwrap();
        assert_eq!(rows[1].kind, crate::ledger::PushIntentKind::Delete);
        assert_eq!(rows[1].outcome, PushIntentOutcome::Unknown);
    }

    #[test]
    fn ambiguous_remote_prune_exit_stays_unknown_in_the_delete_row() {
        let fixture = update_push_fixture();
        let run = crate::remote_git::RemoteGitRun {
            outcome: crate::remote_git::RemoteOutcome::Unknown,
            termination: crate::remote_git::RemoteGitTermination::Exited(75),
            stdout: Vec::new(),
            stderr: b"connection lost after delete send".to_vec(),
            stdout_truncated: false,
            stderr_truncated: false,
            io_error: None,
        };

        assert!(record_delete_push_run(&fixture.ledger, &fixture.delete, &run).unwrap());
        let after = fixture
            .ledger
            .push_intents_for_attempt(fixture.task_id, fixture.attempt)
            .unwrap();
        assert_eq!(after.len(), 2);
        assert_eq!(after[0].kind, crate::ledger::PushIntentKind::Update);
        assert_eq!(after[0].outcome, PushIntentOutcome::Unknown);
        assert_eq!(after[1].id, fixture.delete.id);
        assert_eq!(after[1].kind, crate::ledger::PushIntentKind::Delete);
        assert_eq!(after[1].outcome, PushIntentOutcome::Unknown);
        assert!(after[1].detail.contains("connection lost after delete send"));
    }

    #[test]
    fn push_intent_commit_is_observable_before_the_local_ref_advance() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]).unwrap();
        git(&repo, &["config", "user.name", "test"]).unwrap();
        git(&repo, &["config", "user.email", "test@example.com"]).unwrap();
        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        git(&repo, &["add", "."]).unwrap();
        git(&repo, &["commit", "-m", "base"]).unwrap();
        let base = git(&repo, &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();
        git(&repo, &["checkout", "-b", "task/116"]).unwrap();
        std::fs::write(repo.join("change.txt"), "change\n").unwrap();
        git(&repo, &["add", "."]).unwrap();
        git(&repo, &["commit", "-m", "change"]).unwrap();
        let tip = git(&repo, &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();
        git(&repo, &["checkout", "main"]).unwrap();

        let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
        let (task_id, attempt) = push_journal_landing_task(&ledger, "task/116");
        journal_then_advance_integration(&ledger, task_id, attempt, "main", &tip, || {
            let observed = ledger
                .push_intents_for_attempt(task_id, attempt)
                .unwrap();
            assert_eq!(observed.len(), 2, "both operations must already be durable");
            assert_eq!(
                git(&repo, &["rev-parse", "refs/heads/main"])
                    .unwrap()
                    .trim(),
                base,
                "the observation hook runs at the ref-advance boundary"
            );
            git(
                &repo,
                &[
                    "update-ref",
                    "refs/heads/main",
                    tip.as_str(),
                    base.as_str(),
                ],
            )
        })
        .unwrap();
        assert_eq!(
            git(&repo, &["rev-parse", "refs/heads/main"])
                .unwrap()
                .trim(),
            tip
        );
    }

    #[test]
    fn push_recovery_replays_failed_intent_and_records_the_result() {
        let tmp = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
        let (task_id, attempt) = push_journal_landing_task(&ledger, "task/116");
        let tip = "0123456789abcdef0123456789abcdef01234567";
        let (update, delete) = ledger
            .record_push_intents_before_landing(task_id, attempt, "main", tip)
            .unwrap();
        ledger
            .record_push_outcome(update.id, PushIntentOutcome::Failed, "rejected")
            .unwrap();

        let mut replayed = Vec::new();
        let mut reported = Vec::new();
        let report = recover_push_intents(
            &ledger,
            |intent| {
                assert_eq!(
                    intent.outcome,
                    PushIntentOutcome::Unknown,
                    "the callback receives the durably claimed state"
                );
                assert_eq!(
                    ledger
                        .push_intents_for_attempt(task_id, attempt)
                        .unwrap()[0]
                        .outcome,
                    PushIntentOutcome::Unknown,
                    "failed must be committed as unknown before replay dispatch"
                );
                replayed.push(intent.id);
                Ok((PushIntentOutcome::Succeeded, "delivered on retry".into()))
            },
            |intent| reported.push(intent.id),
        )
        .unwrap();

        assert_eq!(replayed, vec![update.id]);
        assert_eq!(reported, vec![delete.id]);
        assert_eq!(report.replayed_failed, 1);
        assert_eq!(report.reported_unknown, 1);
        assert_eq!(
            ledger
                .push_intents_for_attempt(task_id, attempt)
                .unwrap()[0]
                .outcome,
            PushIntentOutcome::Succeeded
        );
    }

    #[test]
    fn crash_after_replay_dispatch_leaves_unknown_and_next_recovery_only_reports() {
        let tmp = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
        let (task_id, attempt) = push_journal_landing_task(&ledger, "task/116");
        let tip = "0123456789abcdef0123456789abcdef01234567";
        let (update, _) = ledger
            .record_push_intents_before_landing(task_id, attempt, "main", tip)
            .unwrap();
        ledger
            .record_push_outcome(update.id, PushIntentOutcome::Failed, "rejected")
            .unwrap();

        let mut first_dispatches = 0;
        let error = recover_push_intents(
            &ledger,
            |intent| -> Result<(PushIntentOutcome, String)> {
                first_dispatches += 1;
                assert_eq!(intent.outcome, PushIntentOutcome::Unknown);
                assert_eq!(
                    ledger
                        .push_intents_for_attempt(task_id, attempt)
                        .unwrap()[0]
                        .outcome,
                    PushIntentOutcome::Unknown,
                    "the crash window must already be represented durably"
                );
                anyhow::bail!("injected crash after remote dispatch")
            },
            |_| {},
        )
        .unwrap_err();
        assert!(error.to_string().contains("injected crash"), "{error:#}");
        assert_eq!(first_dispatches, 1);

        let mut repeated_dispatches = 0;
        let mut reported = Vec::new();
        let report = recover_push_intents(
            &ledger,
            |_| {
                repeated_dispatches += 1;
                Ok((PushIntentOutcome::Succeeded, "wrong replay".into()))
            },
            |intent| reported.push(intent.id),
        )
        .unwrap();

        assert_eq!(repeated_dispatches, 0);
        assert_eq!(report.replayed_failed, 0);
        assert_eq!(report.reported_unknown, 2);
        assert!(reported.contains(&update.id));
        assert_eq!(
            ledger
                .push_intents_for_attempt(task_id, attempt)
                .unwrap()[0]
                .outcome,
            PushIntentOutcome::Unknown
        );
    }

    #[test]
    fn push_recovery_reports_unknown_without_blind_replay() {
        let tmp = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
        let (task_id, attempt) = push_journal_landing_task(&ledger, "task/116");
        let tip = "0123456789abcdef0123456789abcdef01234567";
        ledger
            .record_push_intents_before_landing(task_id, attempt, "main", tip)
            .unwrap();

        let mut replay_count = 0;
        let mut reported = Vec::new();
        let report = recover_push_intents(
            &ledger,
            |_| {
                replay_count += 1;
                Ok((PushIntentOutcome::Succeeded, String::new()))
            },
            |intent| reported.push((intent.kind, intent.refspec.clone())),
        )
        .unwrap();

        assert_eq!(replay_count, 0);
        assert_eq!(report.replayed_failed, 0);
        assert_eq!(report.reported_unknown, 2);
        assert_eq!(reported.len(), 2);
        assert!(
            ledger
                .outstanding_push_intents()
                .unwrap()
                .iter()
                .all(|intent| intent.outcome == PushIntentOutcome::Unknown)
        );
    }
