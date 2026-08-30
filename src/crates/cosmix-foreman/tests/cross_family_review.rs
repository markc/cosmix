//! Cross-family merge-authority routing and two-arm merge tests.

use cosmix_foreman::config::FleetPolicy;
use cosmix_foreman::executor::AgentKind;
use cosmix_foreman::ledger::{ClaimToken, FindingReason, Ledger, StoredRunOutcome, Task};
use cosmix_foreman::review::{
    ChangedFile, ReviewArmOutcome, ReviewOutcome, ReviewVerdict, merge_review_outcomes,
    parse_review_response, reviewer_for_task, reviewers_for_task, verification_record,
};
use tempfile::TempDir;

fn temp_ledger() -> (Ledger, TempDir) {
    let dir = TempDir::new().expect("tempdir");
    let ledger = Ledger::open(&dir.path().join("test.db")).expect("ledger opens");
    (ledger, dir)
}

fn add_task(ledger: &Ledger, risk: &str) -> Task {
    let id = ledger
        .add_task("test task", "spec", "impl", risk, &[], "rust")
        .expect("task added");
    ledger.task(id).expect("task loaded").expect("task exists")
}

fn finish_run(ledger: &Ledger, task: i64, role: &str, agent: &str, stop: &str) -> i64 {
    let run = ledger
        .store_run_start(task, agent, Some("model"), Some(role))
        .expect("run started");
    ledger
        .store_run_finish(
            run,
            &StoredRunOutcome {
                stop: stop.into(),
                result: (stop == "done").then(|| "result".into()),
                error: (stop != "done").then(|| "failed".into()),
                input_tokens: 100,
                fresh_input_tokens: None,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
                output_tokens: 50,
                cost_usd: None,
                session_ref: None,
            },
            1,
        )
        .expect("run finished");
    run
}

fn outcome(approve: bool, report: &str) -> ReviewOutcome {
    ReviewOutcome {
        approve,
        verdict: Some(if approve {
            ReviewVerdict::Approve
        } else {
            ReviewVerdict::Reject
        }),
        report: report.into(),
        usage: Default::default(),
        delivery: "delivered",
        findings: Vec::new(),
        files_inspected: Vec::new(),
        session_ref: None,
        usage_observed: true,
        output_observed: true,
        resume_failure: None,
    }
}

fn arm(
    run_id: i64,
    reviewer: AgentKind,
    model: &str,
    approve: bool,
    report: &str,
) -> ReviewArmOutcome {
    ReviewArmOutcome {
        reviewer,
        model: model.into(),
        run_id,
        outcome: outcome(approve, report),
    }
}

fn undelivered_arm(
    run_id: i64,
    reviewer: AgentKind,
    model: &str,
    delivery: &'static str,
) -> ReviewArmOutcome {
    ReviewArmOutcome {
        reviewer,
        model: model.into(),
        run_id,
        outcome: ReviewOutcome {
            approve: false,
            verdict: None,
            report: format!(
                "{} arm failed before producing a verdict",
                reviewer.as_str()
            ),
            usage: Default::default(),
            delivery,
            findings: Vec::new(),
            files_inspected: Vec::new(),
            session_ref: None,
            usage_observed: true,
            output_observed: true,
            resume_failure: None,
        },
    }
}

fn dispose_batch(batch: &cosmix_foreman::review::ReviewBatch) -> (bool, bool, i64, i64) {
    let (ledger, _dir) = temp_ledger();
    let id = ledger
        .add_task("review disposition", "spec", "impl", "high", &[], "rust")
        .unwrap();
    let (claimed, implementation_run) = ledger
        .start_attempt(
            id,
            "worker",
            None,
            Some("task/review-disposition"),
            "codex",
            Some("implementer"),
        )
        .unwrap();
    ledger
        .finish_task_claimed(
            id,
            ClaimToken {
                owner: "worker",
                generation: claimed.attempt,
            },
            "done",
        )
        .unwrap();
    assert!(ledger.transition_if(id, "done", "landing").unwrap());

    let reason = batch.rejection_reason().expect("rejected batch reason");
    let (moved, charged) = ledger
        .finish_landing_classified(id, "bounced", Some(implementation_run), Some(reason))
        .unwrap();
    assert!(moved);
    let (moved_again, charged_again) = ledger
        .finish_landing_classified(id, "bounced", Some(implementation_run), Some(reason))
        .unwrap();
    assert!(!moved_again);
    let task = ledger.task(id).unwrap().unwrap();
    (
        charged,
        charged_again,
        task.infra_refusals,
        task.review_rejections,
    )
}

#[test]
fn verdict_is_validated_from_final_json_and_prose_only_rejects() {
    let changed = [ChangedFile {
        path: "src/lib.rs".into(),
        additions: Some(1),
        deletions: Some(0),
        hunks: 1,
    }];
    let reply = r#"analysis
{"verdict":"APPROVE","findings":[],"files_inspected":["src/lib.rs"]}"#;
    assert!(parse_review_response(reply, &changed).unwrap().approve);
    assert!(parse_review_response("analysis only\nVERDICT: APPROVE", &changed).is_err());
}

#[test]
fn configured_primary_is_independent_of_implementer_and_glm_never_reviews() {
    let (ledger, _dir) = temp_ledger();
    let policy = FleetPolicy::defaults();

    for implementer in ["claude", "codex", "glm"] {
        let task = add_task(&ledger, "low");
        finish_run(&ledger, task.id, "implement", implementer, "done");
        assert_eq!(
            reviewer_for_task(&ledger, &task, &policy).unwrap(),
            AgentKind::Codex
        );
    }
}

#[test]
fn unknown_or_unrecorded_implementer_uses_configured_codex_default() {
    let (ledger, _dir) = temp_ledger();
    let policy = FleetPolicy::defaults();

    let unrecorded = add_task(&ledger, "low");
    assert_eq!(
        reviewer_for_task(&ledger, &unrecorded, &policy).unwrap(),
        AgentKind::Codex
    );

    let unknown = add_task(&ledger, "low");
    finish_run(&ledger, unknown.id, "implement", "future-engine", "done");
    assert_eq!(
        reviewer_for_task(&ledger, &unknown, &policy).unwrap(),
        AgentKind::Codex
    );
}

#[test]
fn failed_incomplete_and_review_runs_never_supply_implementer_family() {
    let (ledger, _dir) = temp_ledger();
    let task = add_task(&ledger, "low");
    finish_run(&ledger, task.id, "implement", "claude", "error");
    ledger
        .store_run_start(task.id, "codex", Some("model"), None)
        .unwrap();
    finish_run(&ledger, task.id, "review", "claude", "done");

    assert_eq!(
        reviewer_for_task(&ledger, &task, &FleetPolicy::defaults()).unwrap(),
        AgentKind::Codex
    );
}

#[test]
fn fixed_override_wins_over_routing_and_two_arm_policy() {
    let (ledger, _dir) = temp_ledger();
    let task = add_task(&ledger, "high");
    finish_run(&ledger, task.id, "implement", "codex", "done");
    let mut policy = FleetPolicy::defaults();
    policy.review_override.value = Some(AgentKind::Codex);
    policy.two_arm_review.value = true;

    assert_eq!(
        reviewers_for_task(&ledger, &task, &policy).unwrap(),
        vec![AgentKind::Codex]
    );

    policy.review_override.value = Some(AgentKind::Glm);
    assert!(reviewers_for_task(&ledger, &task, &policy).is_err());
}

#[test]
fn two_arm_is_default_for_high_risk_and_operator_can_disable_it() {
    let (ledger, _dir) = temp_ledger();
    let high = add_task(&ledger, "high");
    let low = add_task(&ledger, "low");
    finish_run(&ledger, high.id, "implement", "claude", "done");
    finish_run(&ledger, low.id, "implement", "claude", "done");

    let mut defaults = FleetPolicy::defaults();
    assert_eq!(
        reviewers_for_task(&ledger, &high, &defaults).unwrap(),
        vec![AgentKind::Codex, AgentKind::Claude]
    );
    assert_eq!(
        reviewers_for_task(&ledger, &low, &defaults).unwrap(),
        vec![AgentKind::Codex]
    );

    defaults.two_arm_review.value = false;
    assert_eq!(
        reviewers_for_task(&ledger, &high, &defaults).unwrap(),
        vec![AgentKind::Codex]
    );
}

#[test]
fn either_arm_rejects_and_both_reports_are_verification_evidence() {
    let (ledger, _dir) = temp_ledger();
    let task = add_task(&ledger, "high");
    let batch = merge_review_outcomes(vec![
        arm(
            11,
            AgentKind::Claude,
            "opus",
            true,
            "claude report\nVERDICT: APPROVE",
        ),
        arm(
            12,
            AgentKind::Codex,
            "gpt-5.6-sol",
            false,
            "codex report\nVERDICT: REJECT",
        ),
    ]);
    assert!(!batch.approve, "one reject must reject the landing");

    let record = verification_record("base", "tip", &batch);
    ledger
        .record_verification(task.id, 3, batch.approve, &record.to_string())
        .expect("verification recorded");
    let stored: serde_json::Value = serde_json::from_str(
        &ledger
            .latest_verification(task.id)
            .unwrap()
            .expect("verification exists"),
    )
    .unwrap();
    let arms = stored["arms"].as_array().expect("structured arm reports");
    assert_eq!(arms.len(), 2);
    assert_eq!(arms[0]["reviewer"], "claude");
    assert_eq!(arms[0]["run_id"], 11);
    assert!(
        arms[0]["report"]
            .as_str()
            .unwrap()
            .contains("claude report")
    );
    assert_eq!(arms[1]["reviewer"], "codex");
    assert_eq!(arms[1]["run_id"], 12);
    assert!(arms[1]["report"].as_str().unwrap().contains("codex report"));
    assert_eq!(stored["approve"], false);
    assert_eq!(arms[0]["findings"], serde_json::json!([]));
    assert_eq!(arms[0]["verdict"], "APPROVE");
    assert_eq!(arms[1]["verdict"], "REJECT");
}

#[test]
fn delivered_reject_outweighs_a_sibling_harness_error_and_charges_once() {
    let batch = merge_review_outcomes(vec![
        arm(
            21,
            AgentKind::Claude,
            "opus",
            false,
            "definite substantive rejection",
        ),
        undelivered_arm(22, AgentKind::Codex, "gpt-5.6-sol", "harness_error"),
    ]);

    assert!(!batch.approve);
    assert_eq!(
        batch.rejection_reason(),
        Some(FindingReason::ReviewRejected)
    );
    assert!(batch.report().contains("REJECT; delivery=delivered"));
    assert!(
        batch
            .report()
            .contains("NO VERDICT; delivery=harness_error")
    );
    let (charged, charged_again, infra_refusals, review_rejections) = dispose_batch(&batch);
    assert!(charged);
    assert!(!charged_again, "the same attempt must not charge twice");
    assert_eq!(infra_refusals, 0);
    assert_eq!(review_rejections, 1);
}

#[test]
fn two_undelivered_arms_are_infrastructure_and_do_not_charge() {
    let batch = merge_review_outcomes(vec![
        undelivered_arm(31, AgentKind::Claude, "opus", "vendor_error"),
        undelivered_arm(32, AgentKind::Codex, "gpt-5.6-sol", "harness_error"),
    ]);

    assert!(!batch.approve);
    assert_eq!(batch.rejection_reason(), Some(FindingReason::InfraRefusal));
    let (charged, charged_again, infra_refusals, review_rejections) = dispose_batch(&batch);
    assert!(!charged);
    assert!(!charged_again);
    assert_eq!(infra_refusals, 1);
    assert_eq!(review_rejections, 0);
}

#[test]
fn delivered_approve_with_sibling_harness_error_still_blocks_without_charging() {
    let batch = merge_review_outcomes(vec![
        arm(41, AgentKind::Claude, "opus", true, "definite approval"),
        undelivered_arm(42, AgentKind::Codex, "gpt-5.6-sol", "harness_error"),
    ]);

    assert!(
        !batch.approve,
        "the failed sibling must still block landing"
    );
    assert_eq!(batch.rejection_reason(), Some(FindingReason::InfraRefusal));
    let (charged, charged_again, _infra_refusals, review_rejections) = dispose_batch(&batch);
    assert!(!charged);
    assert!(!charged_again);
    assert_eq!(review_rejections, 0);
}
