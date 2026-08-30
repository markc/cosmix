//! Escalation-ladder tests: configured entry, patience, exhaustion, the
//! dispatch planner's parking behavior, and the
//! requeue-restarts-the-ladder rule. No subprocesses, no tokens.

use cosmix_foreman::executor::AgentKind;
use cosmix_foreman::ladder::{Dispatch, Ladder, parse_ladder, plan, rung_for_task};
use cosmix_foreman::ledger::{ClaimToken, FindingReason, Ledger};

#[test]
fn rungs_follow_configured_entry_and_patience() {
    let ladder = Ladder::default(); // glm, claude:sonnet, claude:opus / patience 2

    let rung = |failures| ladder.rung_for("low", failures).map(|r| r.to_string());
    assert_eq!(rung(0).as_deref(), Some("glm"));
    assert_eq!(rung(1).as_deref(), Some("glm"));
    assert_eq!(rung(2).as_deref(), Some("claude:sonnet"));
    assert_eq!(rung(4).as_deref(), Some("claude:opus"));
    assert_eq!(rung(6), None, "past the top rung means a human");
}

fn initial_rung_for(ladder: &Ladder, risk: &str) -> String {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let id = ledger
        .add_task("entry test", "spec", "impl", risk, &[], "none")
        .unwrap();
    let task = ledger.task(id).unwrap().unwrap();
    rung_for_task(&ledger, ladder, &task)
        .unwrap()
        .unwrap()
        .to_string()
}

#[test]
fn medium_and_high_start_on_codex_with_a_two_rung_ladder() {
    let ladder = Ladder {
        rungs: parse_ladder("codex,claude:fable").unwrap(),
        ..Ladder::default()
    };
    for risk in ["medium", "high"] {
        assert_eq!(initial_rung_for(&ladder, risk), "codex", "risk={risk}");
    }
}

#[test]
fn every_risk_starts_at_zero_with_a_five_rung_ladder() {
    let ladder = Ladder {
        rungs: parse_ladder("codex,glm,claude:sonnet,claude:opus,claude:fable").unwrap(),
        ..Ladder::default()
    };
    for risk in ["low", "medium", "high"] {
        assert_eq!(initial_rung_for(&ladder, risk), "codex", "risk={risk}");
    }
}

#[test]
fn explicit_start_rung_applies_to_every_risk() {
    let ladder = Ladder {
        rungs: parse_ladder("codex,claude:fable").unwrap(),
        start_rung: 1,
        ..Ladder::default()
    };
    for risk in ["low", "medium", "high"] {
        assert_eq!(
            initial_rung_for(&ladder, risk),
            "claude:fable",
            "risk={risk}"
        );
    }
}

#[test]
fn ladder_spec_parses_and_rejects() {
    let rungs = parse_ladder("codex, claude:opus").unwrap();
    assert_eq!(rungs.len(), 2);
    assert_eq!(rungs[0].agent, AgentKind::Codex);
    assert_eq!(rungs[0].model, None);
    assert_eq!(rungs[1].agent, AgentKind::Claude);
    assert_eq!(rungs[1].model.as_deref(), Some("opus"));

    assert!(parse_ladder("").is_err(), "an empty ladder routes nothing");
    assert!(
        parse_ladder("gpt5:mini").is_err(),
        "unknown agents are errors, not silent defaults"
    );
    assert!(
        parse_ladder("claude:").is_err(),
        "an empty model must not become --model \"\" at session start"
    );
    assert!(
        parse_ladder("glm,,claude").is_err(),
        "empty rungs are typos, not a shorter ladder"
    );
    assert!(parse_ladder("glm,").is_err(), "trailing comma is a typo");
}

#[test]
fn hand_built_zero_patience_does_not_panic() {
    let ladder = Ladder {
        patience: 0,
        ..Ladder::default()
    };
    // Division guard: no panic; behaves as patience 1 (climbs per failure,
    // 5 failures exhaust a 3-rung ladder from the bottom).
    assert!(ladder.rung_for("low", 0).is_some());
    assert_eq!(
        ladder.rung_for("low", 1).map(|r| r.to_string()).as_deref(),
        Some("claude:sonnet")
    );
    assert!(ladder.rung_for("low", 5).is_none());
}

fn bounce_n(ledger: &Ledger, id: i64, n: i64) {
    for _ in 0..n {
        let (task, run) = ledger
            .start_attempt(id, "t", None, None, "claude", None)
            .unwrap();
        assert!(
            ledger
                .finish_task_classified(
                    id,
                    ClaimToken {
                        owner: "t",
                        generation: task.attempt,
                    },
                    run,
                    "bounced",
                    Some(FindingReason::ReviewRejected),
                )
                .unwrap()
        );
    }
}

fn no_exclusions() -> std::collections::HashSet<i64> {
    Default::default()
}

#[test]
fn planner_routes_parks_and_requeue_restarts() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let ladder = Ladder::default();
    let none = no_exclusions();

    // The exhausted task sits FIRST in scan order so the planner must park
    // it in passing before routing the fresh one behind it.
    let tired = ledger
        .add_task("tired", "spec", "impl", "high", &[], "none")
        .unwrap();
    let fresh = ledger
        .add_task("fresh", "spec", "impl", "low", &[], "none")
        .unwrap();
    // Every risk enters at rung 0, so six charges exhaust three rungs.
    bounce_n(&ledger, tired, 6);

    // A dry pass is READ-ONLY: it skips the exhausted task without parking.
    let dry = plan(
        &ledger,
        &ladder,
        None,
        None,
        false,
        &none,
        chrono::Utc::now(),
    )
    .unwrap();
    assert!(dry.parked.is_empty());
    assert_eq!(ledger.task(tired).unwrap().unwrap().status, "bounced");

    // Applying pass: the exhausted task is parked in passing (finding
    // filed, reported in `parked`), the fresh one routed at the bottom rung.
    let outcome = plan(
        &ledger,
        &ladder,
        None,
        None,
        true,
        &none,
        chrono::Utc::now(),
    )
    .unwrap();
    assert_eq!(
        outcome.parked,
        vec![cosmix_foreman::ladder::ParkedTask {
            task_id: tired,
            failures: 6,
            cause: cosmix_foreman::ladder::ParkCause::LadderExhausted,
        }],
        "parked-in-passing must be surfaced"
    );
    match outcome.decision {
        Dispatch::Run { task, rung } => {
            assert_eq!(task.id, fresh);
            assert_eq!(rung.to_string(), "glm");
        }
        other => panic!("expected a run, got {other:?}"),
    }
    assert_eq!(ledger.task(tired).unwrap().unwrap().status, "parked");
    let findings = ledger.open_findings(10).unwrap();
    assert!(
        findings
            .iter()
            .any(|(_, t, sev, title, _)| *t == Some(tired)
                && sev == "blocker"
                && title.contains("exhausted")),
        "{findings:?}"
    );

    // The exclusion set makes a just-dispatched task invisible.
    let outcome = plan(
        &ledger,
        &ladder,
        None,
        None,
        true,
        &std::collections::HashSet::from([fresh]),
        chrono::Utc::now(),
    )
    .unwrap();
    assert!(matches!(outcome.decision, Dispatch::Idle));

    // Parked tasks are unclaimable and invisible to the picker.
    assert!(ledger.claim_task(tired, "x").is_err());
    assert!(
        ledger
            .ready_tasks(None)
            .unwrap()
            .iter()
            .all(|t| t.id != tired)
    );

    // Pinned dispatch of a parked task refuses (it is not dispatchable).
    assert!(
        plan(
            &ledger,
            &ladder,
            Some(tired),
            None,
            true,
            &none,
            chrono::Utc::now()
        )
        .is_err()
    );

    // Operator requeue restarts the LADDER (failures reset) while the
    // attempt claim-generation stays monotonic for the stale-result guard.
    let attempts_before = ledger.task(tired).unwrap().unwrap().attempt;
    ledger.requeue_task(tired, false).unwrap();
    let t = ledger.task(tired).unwrap().unwrap();
    assert_eq!(t.status, "queued");
    assert_eq!(
        t.ladder_failures, 0,
        "requeue-from-parked restarts the ladder"
    );
    assert_eq!(t.attempt, attempts_before, "attempt is never reset");
    match plan(
        &ledger,
        &ladder,
        Some(tired),
        None,
        true,
        &none,
        chrono::Utc::now(),
    )
    .unwrap()
    .decision
    {
        Dispatch::Run { rung, .. } => assert_eq!(rung.to_string(), "glm"),
        other => panic!("expected a run, got {other:?}"),
    }

    // A SUCCESSFUL attempt does not climb the ladder: claim + done leaves
    // failures untouched.
    ledger.claim_task(tired, "s").unwrap();
    ledger.finish_task(tired, "s", "done").unwrap();
    assert_eq!(ledger.task(tired).unwrap().unwrap().ladder_failures, 0);
    ledger.requeue_task(tired, false).unwrap();

    // Pinned dispatch of an exhausted task reports Parked (not silence).
    bounce_n(&ledger, tired, 6);
    match plan(
        &ledger,
        &ladder,
        Some(tired),
        None,
        true,
        &none,
        chrono::Utc::now(),
    )
    .unwrap()
    .decision
    {
        Dispatch::Parked {
            task_id,
            failures,
            cause,
        } => {
            assert_eq!(task_id, tired);
            assert_eq!(failures, 6);
            assert_eq!(cause, cosmix_foreman::ladder::ParkCause::LadderExhausted);
        }
        other => panic!("expected parked, got {other:?}"),
    }

    // Nothing ready → Idle (fresh is still queued; claim it away first).
    ledger.claim_task(fresh, "w").unwrap();
    match plan(
        &ledger,
        &ladder,
        None,
        None,
        true,
        &none,
        chrono::Utc::now(),
    )
    .unwrap()
    .decision
    {
        Dispatch::Idle => {}
        other => panic!("expected idle, got {other:?}"),
    }
}

#[test]
fn refinery_transition_name_does_not_charge_the_ladder() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let id = ledger
        .add_task("t", "spec", "impl", "low", &[], "none")
        .unwrap();
    ledger.claim_task(id, "a").unwrap();
    ledger.finish_task(id, "a", "done").unwrap();
    assert!(ledger.transition_if(id, "done", "landing").unwrap());
    assert!(ledger.transition_if(id, "landing", "bounced").unwrap());
    assert_eq!(ledger.task(id).unwrap().unwrap().ladder_failures, 0);
}

#[test]
fn vendor_failure_stays_on_rung_and_never_charges() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let ladder = Ladder {
        rungs: parse_ladder("glm,claude:sonnet").unwrap(),
        start_rung: 0,
        patience: 1,
        per_rung_patience: Default::default(),
    };
    let id = ledger
        .add_task("t", "spec", "impl", "low", &[], "none")
        .unwrap();
    ledger.claim_task(id, "a").unwrap();
    ledger.finish_task(id, "a", "failed").unwrap();
    assert_eq!(ledger.task(id).unwrap().unwrap().status, "failed");
    let outcome = plan(
        &ledger,
        &ladder,
        None,
        None,
        true,
        &no_exclusions(),
        chrono::Utc::now(),
    )
    .unwrap();
    match outcome.decision {
        Dispatch::Run { task, rung } => {
            assert_eq!(task.id, id);
            assert_eq!(rung.agent, AgentKind::Glm);
        }
        other => panic!("failed task must be redispatched, got {other:?}"),
    }
    // A pre-claim refusal is not quality fuel; it skips only that rung.
    assert!(
        ledger
            .file_rung_refusal(id, "glm", "lane cannot meter dollars")
            .unwrap()
    );
    assert_eq!(ledger.task(id).unwrap().unwrap().ladder_failures, 0);
    let advanced = plan(
        &ledger,
        &ladder,
        None,
        None,
        true,
        &no_exclusions(),
        chrono::Utc::now(),
    )
    .unwrap();
    assert!(matches!(
        advanced.decision,
        Dispatch::Run { rung, .. } if rung.agent == AgentKind::Claude
    ));
    assert!(
        ledger
            .file_rung_refusal(id, "claude:sonnet", "no remaining meter-capable lane")
            .unwrap()
    );
    let exhausted = plan(
        &ledger,
        &ladder,
        None,
        None,
        true,
        &no_exclusions(),
        chrono::Utc::now(),
    )
    .unwrap();
    assert_eq!(exhausted.parked.len(), 1);
    assert_eq!(exhausted.parked[0].task_id, id);
    assert_eq!(
        exhausted.parked[0].cause,
        cosmix_foreman::ladder::ParkCause::RungsRefused
    );
    assert_eq!(ledger.task(id).unwrap().unwrap().status, "parked");
    let finding = ledger
        .task_findings(id)
        .unwrap()
        .into_iter()
        .find(|finding| finding.2.contains("remaining ladder rungs"))
        .expect("rung-refusal park finding");
    assert!(finding.3.contains("Every remaining rung"), "{}", finding.3);
    assert!(finding.3.contains("0 combined"), "{}", finding.3);
    assert!(!finding.3.contains("ladder exhausted"), "{}", finding.3);
}

#[test]
fn verifier_red_charges_advance_and_exhaust_the_ladder() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let ladder = Ladder {
        rungs: parse_ladder("glm,claude:sonnet").unwrap(),
        start_rung: 0,
        patience: 1,
        per_rung_patience: Default::default(),
    };
    let id = ledger
        .add_task("always red", "spec", "impl", "low", &[], "none")
        .unwrap();
    for expected in ["claude:sonnet", "parked"] {
        let (task, run) = ledger
            .start_attempt(id, "agent", None, None, "claude", None)
            .unwrap();
        assert!(
            ledger
                .finish_task_classified(
                    id,
                    ClaimToken {
                        owner: "agent",
                        generation: task.attempt,
                    },
                    run,
                    "bounced",
                    Some(FindingReason::VerifierRed),
                )
                .unwrap()
        );
        let outcome = plan(
            &ledger,
            &ladder,
            None,
            None,
            true,
            &no_exclusions(),
            chrono::Utc::now(),
        )
        .unwrap();
        if expected == "parked" {
            assert_eq!(outcome.parked.len(), 1);
            assert_eq!(outcome.parked[0].task_id, id);
            assert_eq!(
                outcome.parked[0].cause,
                cosmix_foreman::ladder::ParkCause::LadderExhausted
            );
        } else {
            assert!(matches!(
                outcome.decision,
                Dispatch::Run { rung, .. } if rung.to_string() == expected
            ));
        }
    }
    assert_eq!(ledger.task(id).unwrap().unwrap().ladder_failures, 2);
    assert_eq!(ledger.task(id).unwrap().unwrap().review_rejections, 0);
    assert_eq!(ledger.task(id).unwrap().unwrap().status, "parked");
}

#[test]
fn repeated_branch_contract_failures_park_at_default_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let id = ledger
        .add_task("broken handoff", "spec", "impl", "low", &[], "none")
        .unwrap();
    let mut final_run = 0;
    for count in 1..=3 {
        let (task, run) = ledger
            .start_attempt(id, "agent", None, None, "claude", None)
            .unwrap();
        final_run = run;
        assert!(
            !ledger
                .finish_task_classified(
                    id,
                    ClaimToken {
                        owner: "agent",
                        generation: task.attempt,
                    },
                    run,
                    "bounced",
                    Some(FindingReason::BranchContract),
                )
                .unwrap()
        );
        let task = ledger.task(id).unwrap().unwrap();
        assert_eq!(task.branch_contract_failures, count);
        assert_eq!(task.status, if count == 3 { "parked" } else { "bounced" });
    }
    assert_eq!(ledger.task(id).unwrap().unwrap().ladder_failures, 0);
    let payload: String = rusqlite::Connection::open(tmp.path().join("ledger.db"))
        .unwrap()
        .query_row(
            "SELECT payload FROM events WHERE run_id = ?1 AND kind = 'disposition'",
            [final_run],
            |row| row.get(0),
        )
        .unwrap();
    let event: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(event["status"], "parked");
}

#[test]
fn refinery_branch_contract_failures_survive_agent_completions_and_park() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let id = ledger
        .add_task("poisoned base handoff", "spec", "impl", "low", &[], "none")
        .unwrap();

    for count in 1..=3 {
        let (_task, run) = ledger
            .start_attempt(id, "agent", None, None, "claude", None)
            .unwrap();
        ledger.finish_task(id, "agent", "done").unwrap();
        assert_eq!(
            ledger.task(id).unwrap().unwrap().branch_contract_failures,
            count - 1,
            "normal agent completion is not evidence that refinery content is fixed"
        );
        assert!(ledger.transition_if(id, "done", "landing").unwrap());
        let (moved, charged) = ledger
            .finish_landing_classified(
                id,
                "bounced",
                Some(run),
                Some(FindingReason::BranchContract),
            )
            .unwrap();
        assert!(moved);
        assert!(!charged);
        let task = ledger.task(id).unwrap().unwrap();
        assert_eq!(task.attempt, count);
        assert_eq!(task.branch_contract_failures, count);
        assert_eq!(task.status, if count == 3 { "parked" } else { "bounced" });
    }

    ledger.requeue_task(id, false).unwrap();
    let task = ledger.task(id).unwrap().unwrap();
    assert_eq!(task.status, "queued");
    assert_eq!(task.branch_contract_failures, 0);
}

#[test]
fn successful_landing_resets_branch_contract_recurrence() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let id = ledger
        .add_task("fixed handoff", "spec", "impl", "low", &[], "none")
        .unwrap();
    assert!(
        ledger
            .set_operator_driven(id, true, "operator owns the landing", "operator")
            .unwrap()
    );
    let (task, run) = ledger
        .start_attempt(id, "agent", None, None, "claude", None)
        .unwrap();
    assert!(
        !ledger
            .finish_task_classified(
                id,
                ClaimToken {
                    owner: "agent",
                    generation: task.attempt,
                },
                run,
                "bounced",
                Some(FindingReason::BranchContract),
            )
            .unwrap()
    );
    let (_task, run) = ledger
        .start_attempt(id, "agent", None, None, "claude", None)
        .unwrap();
    ledger.finish_task(id, "agent", "done").unwrap();
    assert!(ledger.transition_if(id, "done", "landing").unwrap());
    assert!(
        ledger
            .finish_landing_classified(id, "landed", Some(run), None)
            .unwrap()
            .0
    );
    let task = ledger.task(id).unwrap().unwrap();
    assert_eq!(task.attempt, 2);
    assert_eq!(task.branch_contract_failures, 0);
    assert_eq!(task.status, "landed");
    assert!(
        !task.operator_driven,
        "landing must clear an otherwise permanently stale reservation"
    );
    let release: (String, String) = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT filed_by, reason_code FROM findings
             WHERE task_id = ?1 AND reason_code = 'operator_released'",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(release, ("refinery".into(), "operator_released".into()));
}

#[test]
fn per_rung_patience_override_applies_before_global_default() {
    let ladder = Ladder {
        per_rung_patience: [("glm".to_string(), 1)].into_iter().collect(),
        ..Ladder::default()
    };
    assert_eq!(ladder.rung_for("low", 0).unwrap().to_string(), "glm");
    assert_eq!(
        ladder.rung_for("low", 1).unwrap().to_string(),
        "claude:sonnet"
    );
    assert_eq!(
        ladder.rung_for("low", 2).unwrap().to_string(),
        "claude:sonnet"
    );
}

#[test]
fn planner_admission_uses_its_recorded_wall_time_at_backoff_boundary() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let id = ledger
        .add_task("backoff", "spec", "impl", "low", &[], "none")
        .unwrap();
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE tasks SET dispatch_after = '2026-08-26T00:00:30Z' WHERE id = ?1",
            [id],
        )
        .unwrap();
    let ladder = Ladder::default();
    let before = "2026-08-26T00:00:29.999Z".parse().unwrap();
    let boundary = "2026-08-26T00:00:30Z".parse().unwrap();
    assert!(matches!(
        plan(
            &ledger,
            &ladder,
            None,
            None,
            false,
            &no_exclusions(),
            before,
        )
        .unwrap()
        .decision,
        Dispatch::Idle
    ));
    assert!(matches!(
        plan(
            &ledger,
            &ladder,
            None,
            None,
            false,
            &no_exclusions(),
            boundary,
        )
        .unwrap()
        .decision,
        Dispatch::Run { .. }
    ));
}

#[test]
fn review_reject_charges_exactly_once_and_is_visible_per_attempt() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let id = ledger
        .add_task("review", "spec", "impl", "low", &[], "none")
        .unwrap();
    let (task, run) = ledger
        .start_attempt(id, "agent", None, Some("task/1"), "codex", None)
        .unwrap();
    ledger
        .finish_task_claimed(
            id,
            ClaimToken {
                owner: "agent",
                generation: task.attempt,
            },
            "done",
        )
        .unwrap();
    assert!(ledger.transition_if(id, "done", "landing").unwrap());
    let (moved, charged) = ledger
        .finish_landing_classified(
            id,
            "bounced",
            Some(run),
            Some(FindingReason::ReviewRejected),
        )
        .unwrap();
    assert!(moved && charged);
    let (moved_again, charged_again) = ledger
        .finish_landing_classified(
            id,
            "bounced",
            Some(run),
            Some(FindingReason::ReviewRejected),
        )
        .unwrap();
    assert!(!moved_again && !charged_again);
    let task = ledger.task(id).unwrap().unwrap();
    assert_eq!(task.ladder_failures, 1);
    assert_eq!(task.review_rejections, 1);
    let charges = ledger.task_attempt_charges(id).unwrap();
    assert_eq!(charges.len(), 1);
    assert!(charges[0].charged);
    assert_eq!(charges[0].reason.as_deref(), Some("review_rejected"));
    assert_eq!(ledger.run_event_count(run).unwrap(), 1);
}
