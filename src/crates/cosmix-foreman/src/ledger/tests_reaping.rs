    /// Task 94: a claim whose lease expired AND whose process is confirmed
    /// gone is reaped — requeued, findable, and never charged. `true`'s pid
    /// is guaranteed reused-clean here: it is `wait()`-ed to completion
    /// before the claimant string is even built, so `owner_alive` has no
    /// live process to find under that pid by the time the reaper runs.
    /// Claims through `start_attempt_at` with an explicit `claim_pid`, the
    /// same trusted call runner.rs makes — this is what lets the real
    /// (non-injected) `Ledger::reap_dead_claims` prove this claim dead.
    #[test]
    fn reap_dead_claims_recovers_a_claim_whose_process_is_confirmed_gone() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("ledger.db");
        let ledger = Ledger::open(&db).unwrap();
        let id = ledger
            .add_task("phantom claim", "spec", "impl", "low", &[], "none")
            .unwrap();

        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id() as i64;
        child.wait().unwrap(); // fully reaped: this pid is now provably dead
        let claimant = format!("claude@{pid}");

        let now = chrono::Utc::now().to_rfc3339();
        ledger
            .start_attempt_at(
                id,
                &claimant,
                Some(pid),
                None,
                None,
                "claude",
                None,
                None,
                &now,
                true,
            )
            .unwrap();
        assert_eq!(ledger.task(id).unwrap().unwrap().status, "running");
        let stale = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        backdate_lease(&db, id, &stale);
        // A claim taken 7h ago whose 6h lease ran out 1h ago: the two
        // numbers a reap reports are deliberately different, and the age is
        // the one that says "a supervisor died", so both are recorded.
        let claimed_7h_ago = (chrono::Utc::now() - chrono::Duration::hours(7)).to_rfc3339();
        backdate_claimed_at(&db, id, Some(&claimed_7h_ago));

        let reap_at = chrono::Utc::now();
        let sweep = ledger.reap_dead_claims(&reap_at.to_rfc3339()).unwrap();
        assert!(sweep.unreaped.is_empty(), "{:?}", sweep.unreaped);
        let reaped = sweep.reaped;
        assert_eq!(reaped.len(), 1);
        assert_eq!(reaped[0].task_id, id);
        assert_eq!(reaped[0].claimant, claimant);
        // The lease was backdated to exactly 1h before `stale` was taken, a
        // moment before `reap_at` — so the lease has been overdue for very
        // close to 3600s, never the ~0s a total-claim-age reading would give
        // (this claim's `updated_at` was set moments ago, at claim time).
        assert!(
            (3600..3620).contains(&reaped[0].overdue_secs),
            "overdue_secs should reflect time past the LEASE, not total claim \
             age: {}",
            reaped[0].overdue_secs
        );
        assert_eq!(
            reaped[0].claim_pid, pid,
            "the reap must name the pid it observed absent"
        );
        let age = reaped[0]
            .claim_age_secs
            .expect("a claim taken through the trusted path records its claim time");
        assert!(
            (25_200..25_220).contains(&age),
            "claim age must be the claim's real age (~7h here), not its \
             lease overdue time (~1h): {age}"
        );

        let task = ledger.task(id).unwrap().unwrap();
        assert_eq!(task.status, "queued");
        assert!(task.claimed_by.is_none());
        assert_eq!(
            task.ladder_failures, 0,
            "a reaped claim must never charge the task's ladder position"
        );

        assert!(
            ledger
                .task_has_open_finding_reason(id, FindingReason::DeadClaimReaped)
                .unwrap(),
            "reaping must file a finding naming the dead claim"
        );
        let findings = ledger.task_findings(id).unwrap();
        assert!(
            findings
                .iter()
                .any(|(_, _, _, body)| body.contains(&claimant)),
            "the finding must name the dead claimant: {findings:?}"
        );
        // The finding is the durable record of the observation this reap
        // acted on: which pid, when it was observed absent, and how old the
        // claim was. Nothing can re-observe a process that has already
        // gone, so the observation is written down at the moment it decides
        // something rather than left to be re-made by a later reader.
        let evidence = findings
            .iter()
            .find(|(_, _, _, body)| body.contains(&claimant))
            .map(|(_, _, _, body)| body.clone())
            .unwrap();
        for fragment in [
            format!("pid {pid}"),
            format!("{age}s"),
            "observed absent at".to_string(),
            reap_at.to_rfc3339(),
        ] {
            assert!(
                evidence.contains(&fragment),
                "the reap's evidence must record {fragment:?}: {evidence}"
            );
        }

        // Idempotent: a second sweep at the same instant finds nothing left
        // to reap (the row is `queued`, not `claimed`/`running` any more).
        assert!(
            ledger
                .reap_dead_claims(&reap_at.to_rfc3339())
                .unwrap()
                .is_empty()
        );
    }

    /// A claim taken before `claimed_at` existed (a live fleet ledger
    /// upgraded mid-flight) is still reapable — the age is simply unknown,
    /// and is reported as unknown. Back-deriving it from `lease_until -
    /// CLAIM_LEASE_SECS` would be a plausible number that silently lies the
    /// moment the lease constant changes, which is the whole reason the
    /// claim time is recorded rather than computed.
    #[test]
    fn reap_dead_claims_reports_an_unknown_age_for_a_claim_older_than_the_column() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("ledger.db");
        let ledger = Ledger::open(&db).unwrap();
        let id = ledger
            .add_task("pre-upgrade claim", "spec", "impl", "low", &[], "none")
            .unwrap();

        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id() as i64;
        child.wait().unwrap();
        let claimant = format!("claude@{pid}");
        let now = chrono::Utc::now().to_rfc3339();
        ledger
            .start_attempt_at(
                id,
                &claimant,
                Some(pid),
                None,
                None,
                "claude",
                None,
                None,
                &now,
                true,
            )
            .unwrap();
        let stale = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        backdate_lease(&db, id, &stale);
        backdate_claimed_at(&db, id, None);

        let reaped = ledger
            .reap_dead_claims(&chrono::Utc::now().to_rfc3339())
            .unwrap()
            .reaped;
        assert_eq!(reaped.len(), 1);
        assert_eq!(
            reaped[0].claim_age_secs, None,
            "an unrecorded claim time must report as unknown, not as a guess"
        );
        let findings = ledger.task_findings(id).unwrap();
        assert!(
            findings.iter().any(|(_, _, _, body)| body
                .contains("held since before claim times were recorded")),
            "the finding must say the age is unknown rather than omit it: {findings:?}"
        );
        assert_eq!(ledger.task(id).unwrap().unwrap().status, "queued");
    }

    /// The liveness check is the actual predicate, not the lease: a claim
    /// held by a process that is very much alive is never reaped, no matter
    /// how far in the past its lease is backdated. Goes through the real,
    /// non-injected `owner_alive` against this test's own (unambiguously
    /// alive) pid.
    #[test]
    fn reap_dead_claims_never_touches_a_live_claim_even_with_an_ancient_lease() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("ledger.db");
        let ledger = Ledger::open(&db).unwrap();
        let id = ledger
            .add_task("live claim", "spec", "impl", "low", &[], "none")
            .unwrap();

        // This test process's own pid: unambiguously alive for the test's
        // whole lifetime.
        let pid = std::process::id() as i64;
        let claimant = format!("claude@{pid}");
        let now = chrono::Utc::now().to_rfc3339();
        ledger
            .start_attempt_at(
                id,
                &claimant,
                Some(pid),
                None,
                None,
                "claude",
                None,
                None,
                &now,
                true,
            )
            .unwrap();
        let ancient = (chrono::Utc::now() - chrono::Duration::days(30)).to_rfc3339();
        backdate_lease(&db, id, &ancient);

        let now = chrono::Utc::now().to_rfc3339();
        assert!(ledger.reap_dead_claims(&now).unwrap().is_empty());

        let task = ledger.task(id).unwrap().unwrap();
        assert_eq!(task.status, "running");
        assert_eq!(task.claimed_by.as_deref(), Some(claimant.as_str()));
    }

    /// Same guarantee as above, proven through the injected liveness check
    /// instead of a real process — the reap decision is a pure function of
    /// ledger state + supplied `now` + supplied liveness answer, not of
    /// whatever else happens to be alive on the test host. This is what
    /// closes the replay gap: an identical ledger and timestamp always
    /// produce the identical reap outcome for a given liveness answer.
    #[test]
    fn reap_dead_claims_liveness_check_is_injectable_for_deterministic_replay() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("ledger.db");
        let ledger = Ledger::open(&db).unwrap();
        let id = ledger
            .add_task("injectable liveness", "spec", "impl", "low", &[], "none")
            .unwrap();

        // An implausible pid a real /proc check would call dead — but the
        // injected closure below says otherwise, and the injected answer is
        // what must govern the outcome.
        let pid = 999_999_999_i64;
        let claimant = format!("claude@{pid}");
        let now = chrono::Utc::now().to_rfc3339();
        ledger
            .start_attempt_at(
                id,
                &claimant,
                Some(pid),
                None,
                None,
                "claude",
                None,
                None,
                &now,
                true,
            )
            .unwrap();
        let ancient = (chrono::Utc::now() - chrono::Duration::days(1)).to_rfc3339();
        backdate_lease(&db, id, &ancient);

        let now = chrono::Utc::now().to_rfc3339();
        assert!(
            ledger
                .reap_dead_claims_with(&now, |_, _| true)
                .unwrap()
                .is_empty(),
            "an injected 'alive' answer must be trusted exactly like a real one"
        );
        assert_eq!(ledger.task(id).unwrap().unwrap().status, "running");

        let reaped = ledger
            .reap_dead_claims_with(&now, |_, _| false)
            .unwrap()
            .reaped;
        assert_eq!(
            reaped.len(),
            1,
            "an injected 'dead' answer must reap exactly like a real one"
        );
        assert_eq!(ledger.task(id).unwrap().unwrap().status, "queued");
    }

    /// A sweep reaps candidate by candidate, and one candidate's write
    /// failing must cost only that candidate — neither the claims already
    /// reaped (which are committed, and whose only report to an operator is
    /// this returned Vec) nor the candidates after it. The whole sweep used
    /// to sit under one busy retry at the call site, where a retry re-ran it
    /// from scratch and silently dropped everything the abandoned pass had
    /// already reaped from the report.
    #[test]
    fn a_failed_candidate_costs_the_sweep_only_that_candidate() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("ledger.db");
        let ledger = Ledger::open(&db).unwrap();

        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id() as i64;
        child.wait().unwrap(); // fully reaped: this pid is now provably dead
        let claimant = format!("claude@{pid}");
        let stale = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();

        let mut ids = Vec::new();
        for title in ["first phantom", "doomed phantom", "third phantom"] {
            let id = ledger
                .add_task(title, "spec", "impl", "low", &[], "none")
                .unwrap();
            let now = chrono::Utc::now().to_rfc3339();
            ledger
                .start_attempt_at(
                    id,
                    &claimant,
                    Some(pid),
                    None,
                    None,
                    "claude",
                    None,
                    None,
                    &now,
                    true,
                )
                .unwrap();
            backdate_lease(&db, id, &stale);
            ids.push(id);
        }
        // The middle candidate cannot be written — the ordering matters:
        // one reap is committed BEFORE the failure and one AFTER it.
        super::fail_claim_reap_for_task_in_test(ids[1]);

        let sweep = ledger
            .reap_dead_claims(&chrono::Utc::now().to_rfc3339())
            .unwrap();

        assert_eq!(
            sweep.reaped.iter().map(|r| r.task_id).collect::<Vec<_>>(),
            vec![ids[0], ids[2]],
            "the sweep must report every claim it actually reaped, before \
             and after the one it could not write"
        );
        // The failure is RETURNED, not just printed: a caller that only
        // looked at `reaped` would see two successes and no hint that a
        // provably dead claim is still stranded `running` behind them.
        assert_eq!(sweep.unreaped.len(), 1, "{:?}", sweep.unreaped);
        let unreaped = &sweep.unreaped[0];
        assert_eq!(unreaped.task_id, ids[1]);
        assert_eq!(unreaped.claimant, claimant);
        assert_eq!(unreaped.claim_pid, pid);
        assert!(
            format!("{:#}", unreaped.error).contains("injected dead-claim reap failure"),
            "the returned error must be the write's own, not a summary: {:#}",
            unreaped.error
        );
        assert!(
            !sweep.is_empty(),
            "a sweep that failed a write is not a quiet sweep"
        );
        assert_eq!(ledger.task(ids[0]).unwrap().unwrap().status, "queued");
        assert_eq!(ledger.task(ids[2]).unwrap().unwrap().status, "queued");
        // The unwritten one is untouched, not half-reaped: still claimed,
        // with no finding claiming it was released.
        let doomed = ledger.task(ids[1]).unwrap().unwrap();
        assert_eq!(doomed.status, "running");
        assert_eq!(doomed.claimed_by.as_deref(), Some(claimant.as_str()));
        assert!(
            !ledger
                .task_has_open_finding_reason(ids[1], FindingReason::DeadClaimReaped)
                .unwrap(),
            "a reap that did not happen must not leave a finding saying it did"
        );

        // It is still expired and still dead, so the next sweep gets it.
        super::FAIL_CLAIM_REAP_FOR_TASK.with(|fail| fail.set(None));
        let sweep = ledger
            .reap_dead_claims(&chrono::Utc::now().to_rfc3339())
            .unwrap();
        assert_eq!(
            sweep.reaped.iter().map(|r| r.task_id).collect::<Vec<_>>(),
            vec![ids[1]]
        );
        assert!(sweep.unreaped.is_empty(), "{:?}", sweep.unreaped);
        assert_eq!(ledger.task(ids[1]).unwrap().unwrap().status, "queued");
    }

    /// The exploit finding 4 closed: an MCP-originated claim is self-reported
    /// free text, so an agent could shape it exactly like a trusted
    /// `kind@pid` claimant naming a genuinely dead pid, trying to either
    /// trigger a false reap or (by naming a live pid) suppress one. Neither
    /// works, because `claim_task` never writes `claim_pid` — the reaper has
    /// nothing it trusts to check, regardless of what the claimant text says.
    #[test]
    fn reap_dead_claims_never_reaps_an_untrusted_claim_no_matter_how_the_claimant_text_is_shaped() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("ledger.db");
        let ledger = Ledger::open(&db).unwrap();
        let id = ledger
            .add_task("spoofed claimant", "spec", "impl", "low", &[], "none")
            .unwrap();

        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap(); // fully reaped: this pid is now provably dead
        // Shaped exactly like a trusted `kind@pid` claimant, but claimed
        // through the untrusted MCP-facing path.
        let claimant = format!("claude@{pid}");
        ledger.claim_task(id, &claimant).unwrap();
        let stale = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        backdate_lease(&db, id, &stale);

        let now = chrono::Utc::now().to_rfc3339();
        assert!(ledger.reap_dead_claims(&now).unwrap().is_empty());
        assert_eq!(ledger.task(id).unwrap().unwrap().status, "claimed");
    }

    /// `last_run_ref` is the lookup the runner and refinery use to decide
    /// whether a retry can resume: it must skip the row the caller just
    /// inserted for the CURRENT attempt, narrow to one agent when asked
    /// (two-arm review keeps one thread per reviewer kind), and surface
    /// whatever session_ref the prior run recorded even when it didn't end
    /// cleanly (a bounced run's session still exists to resume).
    #[test]
    fn last_run_ref_finds_the_prior_row_for_the_same_role_and_agent() {
        let temp = tempfile::TempDir::new().unwrap();
        let ledger = Ledger::open(&temp.path().join("ledger.db")).unwrap();
        let task_id = ledger
            .add_task("t", "spec", "impl", "low", &[], "none")
            .unwrap();

        let claude_run = ledger
            .store_run_start(task_id, "claude", Some("opus"), Some("implement"))
            .unwrap();
        ledger
            .store_run_finish(
                claude_run,
                &super::StoredRunOutcome {
                    stop: "bounced".into(),
                    result: None,
                    error: None,
                    input_tokens: 0,
                    fresh_input_tokens: None,
                    cache_read_input_tokens: None,
                    cache_creation_input_tokens: None,
                    output_tokens: 0,
                    cost_usd: None,
                    session_ref: Some("sess-1".into()),
                },
                10,
            )
            .unwrap();
        let codex_review_run = ledger
            .store_run_start(task_id, "codex", Some("gpt-5.6-sol"), Some("review"))
            .unwrap();
        ledger
            .store_run_finish(
                codex_review_run,
                &super::StoredRunOutcome {
                    stop: "done".into(),
                    result: None,
                    error: None,
                    input_tokens: 0,
                    fresh_input_tokens: None,
                    cache_read_input_tokens: None,
                    cache_creation_input_tokens: None,
                    output_tokens: 0,
                    cost_usd: None,
                    session_ref: Some("thread-1".into()),
                },
                10,
            )
            .unwrap();
        // The row the caller just inserted for the current implement attempt
        // — this must be excluded, or a task would "resume" its own new run.
        let current_run = ledger
            .store_run_start(task_id, "claude", Some("opus"), Some("implement"))
            .unwrap();

        let found = ledger
            .last_run_ref(task_id, "implement", None, current_run)
            .unwrap()
            .expect("the prior implement run must be found");
        assert_eq!(found.agent, "claude");
        assert_eq!(found.model.as_deref(), Some("opus"));
        assert_eq!(found.session_ref.as_deref(), Some("sess-1"));

        let review = ledger
            .last_run_ref(task_id, "review", Some("codex"), current_run)
            .unwrap()
            .expect("the codex review run must be found");
        assert_eq!(review.session_ref.as_deref(), Some("thread-1"));

        assert!(
            ledger
                .last_run_ref(task_id, "review", Some("claude"), current_run)
                .unwrap()
                .is_none(),
            "no claude review run exists — narrowing by agent must not fall back to codex's"
        );
        // Excluding `claude_run` must not return `claude_run` itself — it
        // falls through to the OTHER implement row (`current_run`, still
        // unfinished, so its session_ref is unset).
        let excluded = ledger
            .last_run_ref(task_id, "implement", None, claude_run)
            .unwrap()
            .expect("must fall through to the other implement row, not the excluded one");
        assert_eq!(excluded.session_ref, None);
    }
