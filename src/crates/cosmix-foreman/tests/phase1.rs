//! Phase-1 tests: verifier engine, governor ceilings + kill switch, MCP tool
//! surface (called directly, no transport), and the refinery against a real
//! throwaway git repo. No cargo runs, no vendor CLIs, no tokens.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use cosmix_foreman::config::{CONF_FILE, FleetPolicy};
use cosmix_foreman::executor::{AgentKind, RunOutcome, StopReason, Usage};
use cosmix_foreman::governor::Governor;
use cosmix_foreman::ledger::{ClaimToken, FindingReason, Ledger, TaskControls};
use cosmix_foreman::manifest::{LaneSpec, ProjectLanePolicy, ProjectManifest};
use cosmix_foreman::mcp::{
    ForemanMcp, TaskBounceParams, TaskClaimParams, TaskCompleteParams, TaskHeartbeatParams,
    TaskNextParams,
};
use cosmix_foreman::refinery::{self, RefineOptions};
use cosmix_foreman::verify;
use rmcp::{ServerHandler, handler::server::wrapper::Parameters};

mod support;

fn script(dir: &Path, name: &str, body: &str) -> Vec<String> {
    let path = dir.join(name);
    support::write_executable(&path, format!("#!/bin/sh\n{body}\n"));
    vec![path.to_string_lossy().into_owned()]
}

fn finding_audit(db: &Path, task_id: i64, reason: FindingReason) -> (String, Option<String>) {
    rusqlite::Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT status, resolution FROM findings
             WHERE task_id = ?1 AND reason_code = ?2 ORDER BY id DESC LIMIT 1",
            rusqlite::params![task_id, reason.as_db_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}

// ---- verifier ----

/// The cargo shape: the REASON a test failed goes to stdout (test names,
/// assertions, panic messages) while stderr carries only progress chatter
/// and a closing "error: test failed". A digest that keeps one budget over
/// the concatenation keeps the chatter and discards the reason — which is
/// exactly what every tier-0 finding in the live fleet did until
/// 2026-08-19, leaving agents to bounce against a wall the finding never
/// named. Both streams must survive.
#[test]
fn failure_digest_keeps_the_reason_not_just_the_chatter() {
    let tmp = tempfile::tempdir().unwrap();
    // stdout carries the diagnosis; stderr then floods past the tail budget
    // with chatter, as cargo does across many test binaries.
    let report = verify::run_commands(
        "test",
        &[script(
            tmp.path(),
            "failure-tail-fixture",
            "echo 'assertion failed: left == right (the-real-reason)'; \
             i=0; while [ $i -lt 900 ]; do echo \"     Running tests/noise-$i.rs\" >&2; \
             i=$((i+1)); done; echo 'error: test failed' >&2; exit 101",
        )],
        tmp.path(),
    )
    .unwrap();
    assert!(!report.pass);
    let digest = report.failure_digest();
    assert!(
        digest.contains("the-real-reason"),
        "the stdout diagnosis must survive a stderr flood; got:\n{digest}"
    );
    assert!(
        digest.contains("error: test failed"),
        "the stderr summary must survive too; got:\n{digest}"
    );
}

#[test]
fn verifier_engine_reports_failures_with_tails() {
    let tmp = tempfile::tempdir().unwrap();
    let report = verify::run_commands(
        "test",
        &[
            script(tmp.path(), "step-one", "echo step-one-ok"),
            script(tmp.path(), "step-two", "echo the-diagnosis >&2; exit 3"),
            script(tmp.path(), "step-three", "echo never-runs"),
        ],
        tmp.path(),
    )
    .unwrap();
    assert!(!report.pass);
    assert_eq!(report.steps.len(), 2, "first failure stops the run");
    assert!(report.steps[0].pass);
    assert!(!report.steps[1].pass);
    assert_eq!(report.steps[1].exit_code, Some(3));
    assert!(report.steps[1].tail.contains("the-diagnosis"));
    assert!(report.failure_digest().contains("the-diagnosis"));
}

#[test]
fn verifier_profiles_none_passes_and_unknown_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let report = verify::run_profile("none", tmp.path(), None).unwrap();
    assert!(report.pass);
    assert!(report.steps.is_empty());
    assert!(verify::run_profile("yolo", tmp.path(), None).is_err());
}

// ---- governor ----

fn spend_run(ledger: &Ledger, task: i64, cost: f64, tokens_out: u64) {
    let run = ledger
        .start_run(task, AgentKind::Claude, None, None)
        .unwrap();
    let outcome = RunOutcome {
        stop: StopReason::Done,
        result: None,
        error: None,
        usage: Usage {
            input_tokens: 0,
            fresh_input_tokens: None,
            output_tokens: tokens_out,
            cost_usd: Some(cost),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        },
        session_ref: None,
        terminal_session_ref: None,
        usage_observed: true,
        output_observed: true,
        resume_failure: None,
    };
    ledger.finish_run(run, &outcome, 1).unwrap();
}

#[test]
fn governor_ceilings_and_kill_switch() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let task = ledger
        .add_task("t", "spec", "impl", "low", &[], "none")
        .unwrap();

    let governor = Governor::with_limits(&db, 1.0, 100);
    governor.admit(&ledger).expect("fresh ledger admits");

    // Spend over the dollar ceiling → refused, and the refusal names it.
    spend_run(&ledger, task, 1.5, 10);
    let err = format!("{:#}", governor.admit(&ledger).unwrap_err());
    assert!(err.contains("claude-attributed spend ceiling"), "{err}");

    // Token ceiling independently enforced.
    let governor = Governor::with_limits(&db, 0.0, 100);
    spend_run(&ledger, task, 0.0, 200);
    let err = format!("{:#}", governor.admit(&ledger).unwrap_err());
    assert!(err.contains("output-token ceiling"), "{err}");

    // Kill switch beats everything; resume clears it.
    let governor = Governor::with_limits(&db, 0.0, 0);
    governor.admit(&ledger).expect("ceilings disabled");
    governor.stop("test").unwrap();
    let err = format!("{:#}", governor.admit(&ledger).unwrap_err());
    assert!(err.contains("kill switch"), "{err}");
    governor.resume().unwrap();
    governor.admit(&ledger).expect("resumed");
    governor.resume().expect("resume is idempotent");
}

#[test]
fn codex_only_headroom_ignores_the_claude_dollar_ceiling() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let task = ledger
        .add_task("spend", "spec", "impl", "low", &[], "none")
        .unwrap();
    spend_run(&ledger, task, 2.0, 0);
    let governor = Governor::with_limits(&db, 1.0, 100);

    assert!(!governor.check_headroom(&ledger, 0.0, 1).unwrap());
    assert!(
        governor
            .check_headroom_dimensions(&ledger, false, 0.0, 1)
            .unwrap(),
        "a Codex-only route is token-governed and cannot spend Claude dollars"
    );
}

// ---- mcp tool surface ----

#[tokio::test]
async fn mcp_pull_claim_complete_flow() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    {
        let ledger = Ledger::open(&db).unwrap();
        let dep = ledger
            .add_task("dep", "spec", "impl", "low", &[], "none")
            .unwrap();
        ledger.claim_task(dep, "setup").unwrap();
        ledger.finish_task(dep, "setup", "done").unwrap();
        ledger
            .add_task("work", "do the thing", "impl", "low", &[dep], "none")
            .unwrap();
    }
    let mcp = ForemanMcp::new(db.clone()).unwrap();

    let next = mcp
        .task_next(Parameters(TaskNextParams { kind: None }))
        .await;
    assert!(next.contains("\"title\": \"work\""), "{next}");
    assert!(
        next.contains("\"route\""),
        "task_next must carry the ladder's advisory route: {next}"
    );

    let claimed = mcp
        .task_claim(Parameters(TaskClaimParams {
            id: 2,
            claimant: "claude:w1".into(),
        }))
        .await;
    assert!(claimed.contains("\"claimed\""), "{claimed}");
    assert!(
        claimed.contains("\"lease_until\""),
        "task_claim must return the lease deadline: {claimed}"
    );
    let claimed_task = Ledger::open(&db).unwrap().task(2).unwrap().unwrap();
    let claim_pid: Option<i64> = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row("SELECT claim_pid FROM tasks WHERE id = 2", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(claim_pid, None, "MCP claims have no controller-local pid");
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE tasks SET lease_until = '2000-01-01T00:00:00Z' WHERE id = 2",
            [],
        )
        .unwrap();
    let heartbeat = mcp
        .task_heartbeat(Parameters(TaskHeartbeatParams {
            id: 2,
            claimant: "claude:w1".into(),
            attempt: claimed_task.attempt,
        }))
        .await;
    assert!(heartbeat.contains("lease renewed until"), "{heartbeat}");
    let renewed = Ledger::open(&db)
        .unwrap()
        .task(2)
        .unwrap()
        .unwrap()
        .lease_until
        .unwrap();
    assert!(renewed.as_str() > "2000-01-01T00:00:00Z");
    Ledger::open(&db)
        .unwrap()
        .set_task_workspace(
            2,
            ClaimToken {
                owner: "claude:w1",
                generation: claimed_task.attempt,
            },
            Some(tmp.path().to_str().unwrap()),
            Some("task/2"),
        )
        .unwrap();

    // Completing under someone else's name is refused.
    let stolen = mcp
        .task_complete(Parameters(TaskCompleteParams {
            id: 2,
            claimant: "codex:w2".into(),
            workdir: tmp.path().to_str().unwrap().into(),
            branch: None,
        }))
        .await;
    assert!(stolen.starts_with("ERROR:"), "{stolen}");

    let foreign = mcp
        .task_complete(Parameters(TaskCompleteParams {
            id: 2,
            claimant: "claude:w1".into(),
            workdir: tmp.path().to_str().unwrap().into(),
            branch: Some("release".into()),
        }))
        .await;
    assert!(foreign.starts_with("ERROR:"), "{foreign}");
    assert!(
        foreign.contains("does not match recorded branch"),
        "{foreign}"
    );
    assert_eq!(
        Ledger::open(&db)
            .unwrap()
            .task(2)
            .unwrap()
            .unwrap()
            .branch
            .as_deref(),
        Some("task/2"),
        "a foreign completion branch must never replace ledger authority"
    );

    let done = mcp
        .task_complete(Parameters(TaskCompleteParams {
            id: 2,
            claimant: "claude:w1".into(),
            workdir: tmp.path().to_str().unwrap().into(),
            branch: Some("task/2".into()),
        }))
        .await;
    assert!(done.contains("done"), "{done}");

    let ledger = Ledger::open(&db).unwrap();
    let task = ledger.task(2).unwrap().unwrap();
    assert_eq!(task.status, "done");
    assert_eq!(task.branch.as_deref(), Some("task/2"));
    assert_eq!(task.lease_until, None);

    // Nothing claimable remains.
    let next = mcp
        .task_next(Parameters(TaskNextParams { kind: None }))
        .await;
    assert_eq!(next, "no claimable task");

    let status = mcp.build_status().await;
    assert!(status.contains("\"governor\""), "{status}");
    assert!(status.contains("\"done\": 2"), "{status}");
    assert!(status.contains("\"delivery_void_fraction\""), "{status}");
    assert!(status.contains("\"quality_void_fraction\""), "{status}");
}

#[tokio::test]
async fn project_mcp_completion_is_bound_to_recorded_worktree_and_becomes_landable() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    let manifest_path = tmp.path().join("project.mix");
    std::fs::write(
        &manifest_path,
        format!(
            "name: \"mcp-demo\"\n\
             repo: \"{}\"\n\
             db: \"ledger.db\"\n\
             cache_dir: \"cache\"\n\
             integration: \"main\"\n\
             branch_template: \"task/{{id}}\"\n\
             worktree_template: \"task-{{id}}\"\n\
             verifier: \"none\"\n\
             instruction_pack: \"Test-only MCP project policy.\"\n",
            repo.display()
        ),
    )
    .unwrap();
    let project = ProjectManifest::load(&manifest_path).unwrap();
    let mcp = ForemanMcp::new_with_project_manifest(project.db.clone(), &project).unwrap();
    let instructions = mcp
        .get_info()
        .instructions
        .expect("MCP server instructions");
    assert!(
        instructions.contains("# Project context (trusted operator configuration)")
            && instructions.contains("Test-only MCP project policy."),
        "project-mode MCP workers must receive the manifest instruction pack: {instructions}"
    );
    let ledger = Ledger::open(&project.db).unwrap();
    let id = ledger
        .add_task("MCP project work", "spec", "impl", "low", &[], "none")
        .unwrap();
    drop(ledger);

    let claimed = mcp
        .task_claim(Parameters(TaskClaimParams {
            id,
            claimant: "codex:project-worker".into(),
        }))
        .await;
    assert!(!claimed.starts_with("ERROR:"), "{claimed}");
    let claimed_row = Ledger::open(&project.db)
        .unwrap()
        .task(id)
        .unwrap()
        .unwrap();
    let recorded_worktree = claimed_row.worktree.clone().expect("recorded worktree");
    assert_eq!(claimed_row.branch.as_deref(), Some("task/1"));
    assert!(
        claimed.contains(&recorded_worktree),
        "claim response must tell the MCP worker its recorded worktree: {claimed}"
    );

    let foreign = tmp.path().join("foreign");
    std::fs::create_dir(&foreign).unwrap();
    let refused = mcp
        .task_complete(Parameters(TaskCompleteParams {
            id,
            claimant: "codex:project-worker".into(),
            workdir: foreign.to_string_lossy().into_owned(),
            branch: None,
        }))
        .await;
    assert!(refused.starts_with("ERROR:"), "{refused}");
    assert!(
        refused.contains("does not match recorded task worktree"),
        "{refused}"
    );
    let ledger = Ledger::open(&project.db).unwrap();
    assert_eq!(ledger.task(id).unwrap().unwrap().status, "claimed");
    assert!(
        ledger.recent_verifications(1).unwrap().is_empty(),
        "foreign workdir must be refused before verification"
    );
    drop(ledger);

    let done = mcp
        .task_complete(Parameters(TaskCompleteParams {
            id,
            claimant: "codex:project-worker".into(),
            workdir: recorded_worktree.clone(),
            branch: Some("task/1".into()),
        }))
        .await;
    assert_eq!(done, "task 1 done (tier-0 green)");

    let ledger = Ledger::open(&project.db).unwrap();
    let task = ledger.task(id).unwrap().unwrap();
    assert_eq!(task.status, "done");
    assert_eq!(task.worktree.as_deref(), Some(recorded_worktree.as_str()));
    assert_eq!(task.branch.as_deref(), Some("task/1"));
    let selected = ledger.landable_tasks().unwrap();
    assert_eq!(
        selected.len(),
        1,
        "refinery must select the MCP-completed task"
    );
    assert_eq!(selected[0].id, id);
    assert_eq!(selected[0].branch.as_deref(), Some("task/1"));
}

#[tokio::test]
async fn mcp_claim_respects_kill_switch() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    {
        let ledger = Ledger::open(&db).unwrap();
        ledger
            .add_task("t", "spec", "impl", "low", &[], "none")
            .unwrap();
    }
    Governor::new(&db).unwrap().stop("halt").unwrap();
    let mcp = ForemanMcp::new(db).unwrap();
    let refused = mcp
        .task_claim(Parameters(TaskClaimParams {
            id: 1,
            claimant: "claude:w1".into(),
        }))
        .await;
    assert!(refused.contains("kill switch"), "{refused}");
}

#[tokio::test]
async fn mcp_claim_enforces_manifest_lane_policy() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    std::fs::write(
        tmp.path().join("foreman.conf.mix"),
        "ladder: [\"claude\", \"codex\"]\nladder_patience: 1\n",
    )
    .unwrap();
    let ledger = Ledger::open(&db).unwrap();
    ledger
        .add_task("t", "spec", "impl", "low", &[], "none")
        .unwrap();
    drop(ledger);
    let mcp = ForemanMcp::new_with_project_policy(
        db.clone(),
        Vec::new(),
        Some(ProjectLanePolicy {
            name: "restricted".into(),
            lanes: vec![LaneSpec {
                agent: AgentKind::Codex,
                credentials: Vec::new(),
            }],
            push_remote: None,
        }),
    )
    .unwrap();

    let next = mcp
        .task_next(Parameters(TaskNextParams { kind: None }))
        .await;
    assert_eq!(next, "no claimable task");
    let denied_ledger = Ledger::open(&db).unwrap();
    assert_eq!(denied_ledger.task(1).unwrap().unwrap().ladder_failures, 0);
    assert!(
        denied_ledger
            .task_findings(1)
            .unwrap()
            .iter()
            .any(|(_, severity, title, _)| severity == "blocker"
                && title.contains("policy denied"))
    );
    let next = mcp
        .task_next(Parameters(TaskNextParams { kind: None }))
        .await;
    assert_eq!(next, "no claimable task");

    let task = Ledger::open(&db).unwrap().task(1).unwrap().unwrap();
    assert_eq!(task.status, "queued");
    assert!(task.claimed_by.is_none());
    Ledger::open(&db)
        .unwrap()
        .add_task("direct claim", "spec", "impl", "low", &[], "none")
        .unwrap();

    let refused = mcp
        .task_claim(Parameters(TaskClaimParams {
            id: 2,
            claimant: "claude:w2".into(),
        }))
        .await;
    assert!(refused.contains("refused by project policy"), "{refused}");
    assert!(refused.contains("ladder unchanged"), "{refused}");
    let task = Ledger::open(&db).unwrap().task(2).unwrap().unwrap();
    assert_eq!(task.ladder_failures, 0);
    assert_eq!(task.status, "queued");
}

#[tokio::test]
async fn mcp_claim_refuses_operator_driven_before_ladder_parking() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    {
        let ledger = Ledger::open(&db).unwrap();
        ledger
            .add_task_scoped(
                "gate edit",
                "spec",
                "impl",
                "high",
                &[],
                TaskControls {
                    verifier_profile: "none",
                    crates: &[],
                    operator_driven_reason: Some("MCP claim refusal test"),
                },
            )
            .unwrap();
    }
    let mcp = ForemanMcp::new(db.clone()).unwrap();
    let refused = mcp
        .task_claim(Parameters(TaskClaimParams {
            id: 1,
            claimant: "claude:w1".into(),
        }))
        .await;
    assert!(refused.contains("not ready: operator-driven"), "{refused}");

    let ledger = Ledger::open(&db).unwrap();
    let task = ledger.task(1).unwrap().unwrap();
    assert_eq!(task.status, "queued");
    assert!(task.claimed_by.is_none());
    let findings = ledger.task_findings(1).unwrap();
    assert_eq!(findings.len(), 1, "MCP refusal must add no finding");
    assert_eq!(
        findings[0].2,
        "task 1 reserved for operator-driven execution"
    );
    assert_eq!(findings[0].3, "MCP claim refusal test");
}

#[tokio::test]
async fn mcp_self_bounce_uses_bounded_branch_contract_counter() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    {
        let ledger = Ledger::open(&db).unwrap();
        let id = ledger
            .add_task("self bounce", "spec", "impl", "low", &[], "none")
            .unwrap();
        let error = anyhow::anyhow!("temporary infrastructure refusal");
        assert_eq!(
            ledger
                .note_infra_refusal(id, &error, 3, 10)
                .unwrap()
                .unwrap()
                .count,
            1
        );
        assert_eq!(
            ledger
                .note_infra_refusal(id, &error, 3, 10)
                .unwrap()
                .unwrap()
                .count,
            2
        );
    }
    let mcp = ForemanMcp::new(db.clone()).unwrap();
    for count in 1..=3 {
        let claimed = mcp
            .task_claim(Parameters(TaskClaimParams {
                id: 1,
                claimant: "agent".into(),
            }))
            .await;
        let task: serde_json::Value = serde_json::from_str(&claimed).unwrap();
        let attempt = task["attempt"].as_i64().unwrap();
        let result = mcp
            .task_bounce(Parameters(TaskBounceParams {
                id: 1,
                claimant: "agent".into(),
                reason: "need another pass".into(),
                attempt,
            }))
            .await;
        assert!(
            result.contains(if count == 3 { "parked" } else { "bounced" }),
            "{result}"
        );
        if count == 1 {
            let ledger = Ledger::open(&db).unwrap();
            assert_eq!(ledger.task(1).unwrap().unwrap().infra_refusals, 0);
            let error = anyhow::anyhow!("a new refusal sequence");
            assert_eq!(
                ledger
                    .note_infra_refusal(1, &error, 3, 10)
                    .unwrap()
                    .unwrap()
                    .count,
                1
            );
            assert!(
                !ledger
                    .task_has_open_finding_reason(1, FindingReason::InfraRefusal)
                    .unwrap(),
                "two refusals + MCP bounce + one refusal is not three consecutive"
            );
        }
    }
    let task = Ledger::open(&db).unwrap().task(1).unwrap().unwrap();
    assert_eq!(task.branch_contract_failures, 3);
    assert_eq!(task.ladder_failures, 0);
    assert_eq!(task.status, "parked");
}

#[tokio::test]
async fn stale_mcp_bounce_cannot_disposition_a_newer_same_name_claim() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    ledger
        .add_task(
            "generation-fenced bounce",
            "spec",
            "impl",
            "low",
            &[],
            "none",
        )
        .unwrap();
    let mcp = ForemanMcp::new(db.clone()).unwrap();

    let first: serde_json::Value = serde_json::from_str(
        &mcp.task_claim(Parameters(TaskClaimParams {
            id: 1,
            claimant: "same-agent".into(),
        }))
        .await,
    )
    .unwrap();
    let stale_attempt = first["attempt"].as_i64().unwrap();
    ledger.requeue_task(1, true).unwrap();
    let second: serde_json::Value = serde_json::from_str(
        &mcp.task_claim(Parameters(TaskClaimParams {
            id: 1,
            claimant: "same-agent".into(),
        }))
        .await,
    )
    .unwrap();
    assert_eq!(second["attempt"].as_i64(), Some(stale_attempt + 1));

    let refused = mcp
        .task_bounce(Parameters(TaskBounceParams {
            id: 1,
            claimant: "same-agent".into(),
            reason: "delayed duplicate".into(),
            attempt: stale_attempt,
        }))
        .await;
    assert!(refused.starts_with("ERROR:"), "{refused}");
    assert!(refused.contains("claim generation"), "{refused}");
    let task = ledger.task(1).unwrap().unwrap();
    assert_eq!(task.status, "claimed");
    assert_eq!(task.attempt, stale_attempt + 1);
    assert_eq!(task.branch_contract_failures, 0);
}

// ---- refinery ----

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn write(repo: &Path, name: &str, content: &str) {
    std::fs::write(repo.join(name), content).unwrap();
}

/// Thin alias for the shared writer (see `src/fixture.rs`) so this file's
/// five call sites read as before; it waits out inherited write descriptors
/// exactly like every other write-then-exec fixture in the crate.
fn write_executable(path: &Path, content: &str) {
    support::write_executable(path, content);
}

/// main with one commit; returns the repo path.
fn git_repo(tmp: &Path) -> PathBuf {
    let repo = tmp.join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["config", "user.name", "t"]);
    git(&repo, &["config", "user.email", "t@t"]);
    write(&repo, "base.txt", "base\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "base"]);
    repo
}

fn add_done_task(ledger: &Ledger, title: &str, branch: &str) -> i64 {
    let id = ledger
        .add_task(title, "spec", "impl", "low", &[], "none")
        .unwrap();
    let claimed = ledger.claim_task(id, "test-agent").unwrap();
    ledger
        .set_task_workspace(
            id,
            ClaimToken {
                owner: "test-agent",
                generation: claimed.attempt,
            },
            None,
            Some(branch),
        )
        .unwrap();
    ledger.finish_task(id, "test-agent", "done").unwrap();
    id
}

fn run_refinery(
    ledger: &Ledger,
    opts: &RefineOptions,
    landing_gate: Option<&OsStr>,
) -> anyhow::Result<Vec<refinery::LandingReport>> {
    run_refinery_with_policy_env(
        ledger,
        opts,
        landing_gate,
        &[
            ("FOREMAN_REVIEW_OVERRIDE", None),
            ("FOREMAN_TWO_ARM_REVIEW", None),
        ],
    )
}

fn fleet_policy(
    db: &Path,
    landing_gate: Option<&OsStr>,
    extra_env: &[(&'static str, Option<&OsStr>)],
) -> anyhow::Result<FleetPolicy> {
    let mut env = vec![("FOREMAN_LANDING_GATE", landing_gate)];
    env.extend_from_slice(extra_env);
    let default_path = db
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(CONF_FILE);
    FleetPolicy::load_with(default_path, |key| {
        env.iter()
            .rev()
            .find(|(name, _)| *name == key)
            .and_then(|(_, value)| value.map(ToOwned::to_owned))
    })
}

fn run_refinery_with_policy_env(
    ledger: &Ledger,
    opts: &RefineOptions,
    landing_gate: Option<&OsStr>,
    extra_env: &[(&'static str, Option<&OsStr>)],
) -> anyhow::Result<Vec<refinery::LandingReport>> {
    let mut opts = opts.clone();
    opts.fleet_policy = Some(fleet_policy(&opts.db, landing_gate, extra_env)?);
    refinery::refine(ledger, &opts)
}

/// The refinery's `done -> landing` claim must be committed before any
/// verifier process starts. The verifier here is a child invocation of this
/// test binary: it opens the same ledger independently, sets a one-second
/// busy timeout, and writes a finding. If refine carries BEGIN IMMEDIATE
/// across `run_tier_with_policy`, the child fails and the landing bounces.
#[test]
fn refinery_verifier_runs_without_ledger_transaction() {
    const WRITER_DB: &str = "FOREMAN_PHASE1_VERIFIER_WRITER_DB";
    if let Some(db) = std::env::var_os(WRITER_DB) {
        let conn = rusqlite::Connection::open(db).unwrap();
        conn.busy_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        conn.execute(
            "INSERT INTO findings
                 (task_id, severity, title, body, filed_by, reason_code, created_at)
             VALUES (NULL, 'info', 'verifier transaction boundary', '',
                     'test-verifier', 'unknown', '2026-08-23T00:00:00Z')",
            [],
        )
        .expect("verifier must be able to write through its own short-timeout connection");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    git(&repo, &["checkout", "-b", "task/verifier-write"]);
    write(&repo, "verifier-boundary.txt", "work\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "verifier boundary work"]);
    git(&repo, &["checkout", "main"]);

    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let id = ledger
        .add_task("verifier write", "spec", "impl", "low", &[], "rust")
        .unwrap();
    let claimed = ledger.claim_task(id, "test-agent").unwrap();
    ledger
        .set_task_workspace(
            id,
            ClaimToken {
                owner: "test-agent",
                generation: claimed.attempt,
            },
            None,
            Some("task/verifier-write"),
        )
        .unwrap();
    ledger.finish_task(id, "test-agent", "done").unwrap();

    let current_test = std::env::current_exe().unwrap();
    let mut policy = fleet_policy(&db, None, &[]).unwrap();
    policy.tier2_commands.value = vec![format!(
        "env {WRITER_DB}={} {} --exact refinery_verifier_runs_without_ledger_transaction --nocapture",
        db.display(),
        current_test.display()
    )];
    let reports = refinery::refine(
        &ledger,
        &RefineOptions {
            repo: repo.clone(),
            project_root: None,
            integration: "main".into(),
            subdir: ".".into(),
            tier: 2,
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

    assert_eq!(reports.len(), 1);
    assert!(reports[0].landed, "{}", reports[0].detail);
    assert_eq!(ledger.task(id).unwrap().unwrap().status, "landed");
    assert!(
        ledger
            .open_findings(10)
            .unwrap()
            .iter()
            .any(|(_, _, _, title, _)| title == "verifier transaction boundary"),
        "the fake verifier's independent ledger write must commit"
    );
}

#[test]
fn refinery_lands_clean_branch_and_bounces_conflict() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());

    // Clean branch: adds a new file.
    git(&repo, &["checkout", "-b", "task/clean"]);
    write(&repo, "clean.txt", "clean\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "clean work"]);
    git(&repo, &["checkout", "main"]);

    // Conflicting branch: edits base.txt...
    git(&repo, &["checkout", "-b", "task/conflict"]);
    write(&repo, "base.txt", "branch version\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "conflicting work"]);
    git(&repo, &["checkout", "main"]);
    // ...while main moves the same line.
    write(&repo, "base.txt", "main version\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "main moved"]);

    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    for (title, branch) in [("clean", "task/clean"), ("conflict", "task/conflict")] {
        let id = ledger
            .add_task(title, "spec", "impl", "low", &[], "none")
            .unwrap();
        let claimed = ledger.claim_task(id, "a").unwrap();
        ledger
            .set_task_workspace(
                id,
                ClaimToken {
                    owner: "a",
                    generation: claimed.attempt,
                },
                None,
                Some(branch),
            )
            .unwrap();
        ledger.finish_task(id, "a", "done").unwrap();
    }

    let reports = run_refinery(
        &ledger,
        &RefineOptions {
            repo: repo.clone(),
            project_root: None,
            integration: "main".into(),
            subdir: ".".into(),
            tier: 0,
            review: false,
            db: db.clone(),
            echo: false,
            fleet_policy: None,
            profiles: Vec::new(),
            project_pack: String::new(),
            landing_gate: None,
            lane_policy: None,
        },
        None,
    )
    .unwrap();
    assert_eq!(reports.len(), 2);
    assert!(reports[0].landed, "{}", reports[0].detail);
    assert!(!reports[1].landed);
    assert!(
        reports[1].detail.contains("conflicted"),
        "{}",
        reports[1].detail
    );

    assert_eq!(ledger.task(1).unwrap().unwrap().status, "landed");
    assert_eq!(ledger.task(2).unwrap().unwrap().status, "bounced");
    // The landed file is on main; the repo is left on main, not mid-rebase.
    assert!(repo.join("clean.txt").exists());
    assert!(!repo.join(".git/rebase-merge").exists());
    let head = Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), "main");
    // The bounce filed a finding carrying the concrete conflict detail.
    let findings = ledger.open_findings(10).unwrap();
    assert!(
        findings
            .iter()
            .any(|(_, task, _, title, body)| *task == Some(2)
                && title.contains("task/conflict")
                && body.contains("conflicted")),
        "{findings:?}"
    );

    // A landed dependency must still satisfy the claim gate — accepting only
    // "done" deadlocks every chain the refinery touches.
    let dependent = ledger
        .add_task("dependent", "spec", "impl", "low", &[1], "none")
        .unwrap();
    ledger
        .claim_task(dependent, "b")
        .expect("landed dep satisfies the claim gate");
}

#[test]
fn refinery_versions_in_landing_commits_and_same_crate_branches_land_back_to_back() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    std::fs::create_dir_all(repo.join("crate/src")).unwrap();
    write(
        &repo,
        "crate/Cargo.toml",
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(&repo, "crate/src/lib.rs", "pub fn base() {}\n");
    let generated = Command::new("cargo")
        .args(["generate-lockfile", "--offline", "--manifest-path"])
        .arg(repo.join("crate/Cargo.toml"))
        .status()
        .unwrap();
    assert!(generated.success());
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "add fixture crate"]);
    let common_base = git_out(&repo, &["rev-parse", "HEAD"]);

    for (branch, file) in [("task/one", "one.rs"), ("task/two", "two.rs")] {
        git(&repo, &["checkout", "-b", branch, &common_base]);
        write(&repo, &format!("crate/src/{file}"), "pub fn changed() {}\n");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", branch]);
    }
    git(&repo, &["checkout", "main"]);

    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    for (title, branch) in [("one", "task/one"), ("two", "task/two")] {
        let id = ledger
            .add_task(title, "spec", "impl", "low", &[], "none")
            .unwrap();
        let claimed = ledger.claim_task(id, "agent").unwrap();
        ledger
            .set_task_workspace(
                id,
                ClaimToken {
                    owner: "agent",
                    generation: claimed.attempt,
                },
                None,
                Some(branch),
            )
            .unwrap();
        ledger.finish_task(id, "agent", "done").unwrap();
    }

    let reports = refinery::refine(
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
    assert_eq!(reports.len(), 2);
    assert!(reports.iter().all(|report| report.landed), "{reports:?}");
    let manifest = std::fs::read_to_string(repo.join("crate/Cargo.toml")).unwrap();
    assert!(manifest.contains("version = \"0.1.2\""), "{manifest}");
    let lock = std::fs::read_to_string(repo.join("crate/Cargo.lock")).unwrap();
    assert!(lock.contains("version = \"0.1.2\""), "{lock}");
    let log = git_out(&repo, &["log", "--format=%s", "-6"]);
    assert_eq!(
        log.matches("refinery: version packages for task").count(),
        2
    );
}

#[test]
fn refinery_honours_explicit_minor_for_low_risk_impl_task() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    std::fs::create_dir_all(repo.join("crate/src")).unwrap();
    write(
        &repo,
        "crate/Cargo.toml",
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(&repo, "crate/src/lib.rs", "pub fn base() {}\n");
    assert!(
        Command::new("cargo")
            .args(["generate-lockfile", "--offline", "--manifest-path"])
            .arg(repo.join("crate/Cargo.toml"))
            .status()
            .unwrap()
            .success()
    );
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "add fixture crate"]);
    git(&repo, &["checkout", "-b", "task/explicit-minor"]);
    write(
        &repo,
        "crate/src/lib.rs",
        "pub fn base() {}\npub fn additive_flag() {}\n",
    );
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "implement low-risk task"]);
    git(&repo, &["checkout", "main"]);

    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let id = ledger
        .add_task_scoped_with_budget_and_bump(
            "explicit minor",
            "spec",
            "impl",
            "low",
            &[],
            TaskControls {
                verifier_profile: "none",
                crates: &[],
                operator_driven_reason: None,
            },
            None,
            Some("minor"),
        )
        .unwrap();
    let claimed = ledger.claim_task(id, "agent").unwrap();
    ledger
        .set_task_workspace(
            id,
            ClaimToken {
                owner: "agent",
                generation: claimed.attempt,
            },
            None,
            Some("task/explicit-minor"),
        )
        .unwrap();
    ledger.finish_task(id, "agent", "done").unwrap();

    let reports = refinery::refine(
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
    assert_eq!(reports.len(), 1);
    assert!(reports[0].landed, "{reports:?}");
    let manifest = std::fs::read_to_string(repo.join("crate/Cargo.toml")).unwrap();
    assert!(manifest.contains("version = \"0.2.0\""), "{manifest}");
    let lock = std::fs::read_to_string(repo.join("crate/Cargo.lock")).unwrap();
    assert!(lock.contains("version = \"0.2.0\""), "{lock}");
}

#[test]
fn refinery_discards_agent_patch_and_minor_bumps_then_lands_its_patch() {
    for agent_version in ["0.1.1", "0.2.0"] {
        let tmp = tempfile::tempdir().unwrap();
        let repo = git_repo(tmp.path());
        std::fs::create_dir_all(repo.join("crate/src")).unwrap();
        write(
            &repo,
            "crate/Cargo.toml",
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        );
        write(&repo, "crate/src/lib.rs", "pub fn base() {}\n");
        assert!(
            Command::new("cargo")
                .args(["generate-lockfile", "--offline", "--manifest-path"])
                .arg(repo.join("crate/Cargo.toml"))
                .status()
                .unwrap()
                .success()
        );
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "base crate"]);
        git(&repo, &["checkout", "-b", "task/agent-bump"]);
        write(
            &repo,
            "crate/Cargo.toml",
            &format!(
                "[package]\nname = \"fixture\"\nversion = \"{agent_version}\"\nedition = \"2024\"\n"
            ),
        );
        write(&repo, "crate/src/lib.rs", "pub fn changed() {}\n");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "agent bump"]);
        git(&repo, &["checkout", "main"]);

        let db = tmp.path().join("ledger.db");
        let ledger = Ledger::open(&db).unwrap();
        let id = ledger
            .add_task("agent bump", "spec", "impl", "low", &[], "none")
            .unwrap();
        let claimed = ledger.claim_task(id, "agent").unwrap();
        ledger
            .set_task_workspace(
                id,
                ClaimToken {
                    owner: "agent",
                    generation: claimed.attempt,
                },
                None,
                Some("task/agent-bump"),
            )
            .unwrap();
        ledger.finish_task(id, "agent", "done").unwrap();

        let reports = refinery::refine(
            &ledger,
            &RefineOptions {
                repo: repo.clone(),
                project_root: None,
                integration: "main".into(),
                subdir: ".".into(),
                tier: 0,
                review: false,
                db: db.clone(),
                echo: false,
                fleet_policy: None,
                profiles: Vec::new(),
                project_pack: String::new(),
                landing_gate: None,
                lane_policy: None,
            },
        )
        .unwrap();
        assert!(
            reports[0].landed,
            "{}: {}",
            agent_version, reports[0].detail
        );
        let manifest = std::fs::read_to_string(repo.join("crate/Cargo.toml")).unwrap();
        assert!(manifest.contains("version = \"0.1.1\""), "{manifest}");
        assert_eq!(
            finding_audit(&db, id, FindingReason::VersionBumpDiscarded),
            ("resolved".into(), Some(format!("task {id} landed"))),
            "discarded {agent_version} bump must remain as resolved audit evidence"
        );
    }
}

#[test]
fn merge_review_tolerates_discarded_branch_bump_but_rejects_off_spec_dependency() {
    for (case, dependency_edit, should_land) in
        [("version", false, true), ("dependency", true, false)]
    {
        let tmp = tempfile::tempdir().unwrap();
        let repo = git_repo(tmp.path());
        std::fs::create_dir_all(repo.join("crate/src")).unwrap();
        std::fs::create_dir_all(repo.join("dep/src")).unwrap();
        write(
            &repo,
            "crate/Cargo.toml",
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        );
        write(&repo, "crate/src/lib.rs", "pub fn base() {}\n");
        write(
            &repo,
            "dep/Cargo.toml",
            "[package]\nname = \"fixture-dep\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        );
        write(&repo, "dep/src/lib.rs", "pub fn dependency() {}\n");
        assert!(
            Command::new("cargo")
                .args(["generate-lockfile", "--offline", "--manifest-path"])
                .arg(repo.join("crate/Cargo.toml"))
                .status()
                .unwrap()
                .success()
        );
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "review fixture base"]);
        git(&repo, &["checkout", "-b", &format!("task/{case}")]);

        if dependency_edit {
            write(
                &repo,
                "crate/Cargo.toml",
                "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nfixture-dep = { path = \"../dep\" }\n",
            );
            assert!(
                Command::new("cargo")
                    .args(["generate-lockfile", "--offline", "--manifest-path"])
                    .arg(repo.join("crate/Cargo.toml"))
                    .status()
                    .unwrap()
                    .success()
            );
        } else {
            write(
                &repo,
                "crate/Cargo.toml",
                "[package]\nname = \"fixture\"\nversion = \"0.9.0\"\nedition = \"2024\"\n",
            );
        }
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", &format!("agent {case} edit")]);
        git(&repo, &["checkout", "main"]);

        // The fixture enforces the trusted prompt statement, then models the
        // requested review distinction against the actual refinery-produced
        // tip: a historical version-only edit approves, while the task's
        // off-spec dependency addition remains a blocking finding.
        let reviewer = tmp.path().join("fake-codex-version-contract");
        write_executable(
            &reviewer,
            r#"#!/bin/sh
prompt=
for arg do prompt=$arg; done
case "$prompt" in
  *"VersionBumpDiscarded"*"not a violation and must not cause rejection"*) review= ;;
  *) review='{\"verdict\":\"REJECT\",\"findings\":[{\"severity\":\"BLOCKER\",\"file\":\"crate/Cargo.toml\",\"line\":1,\"title\":\"Missing versioning contract\",\"body\":\"The review prompt omitted refinery-owned versioning.\"}],\"files_inspected\":[\"crate/Cargo.lock\",\"crate/Cargo.toml\"]}' ;;
esac
if [ -z "$review" ]; then
  if git diff main..HEAD -- crate/Cargo.toml | grep -q '^+fixture-dep = '; then
    review='{\"verdict\":\"REJECT\",\"findings\":[{\"severity\":\"BLOCKER\",\"file\":\"crate/Cargo.toml\",\"line\":7,\"title\":\"Off-spec dependency edit\",\"body\":\"The branch changes a dependency line; refinery version ownership does not excuse it.\"}],\"files_inspected\":[\"crate/Cargo.lock\",\"crate/Cargo.toml\"]}'
  else
    review='{\"verdict\":\"APPROVE\",\"findings\":[],\"files_inspected\":[\"crate/Cargo.lock\",\"crate/Cargo.toml\"]}'
  fi
fi
printf '%s\n' '{"type":"thread.started","thread_id":"version-contract"}'
printf '%s\n' "{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"$review\"}}"
printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":20,"cached_input_tokens":0,"output_tokens":10}}'
"#,
        );

        let db = tmp.path().join("ledger.db");
        let ledger = Ledger::open(&db).unwrap();
        let id = add_done_task(&ledger, case, &format!("task/{case}"));
        let reports = run_refinery_with_policy_env(
            &ledger,
            &RefineOptions {
                repo: repo.clone(),
                project_root: None,
                integration: "main".into(),
                subdir: ".".into(),
                tier: 0,
                review: true,
                db: db.clone(),
                echo: false,
                fleet_policy: None,
                profiles: Vec::new(),
                project_pack: String::new(),
                landing_gate: None,
                lane_policy: None,
            },
            None,
            &[
                ("FOREMAN_REVIEW_OVERRIDE", Some(OsStr::new("codex"))),
                ("FOREMAN_CODEX_BIN", Some(reviewer.as_os_str())),
            ],
        )
        .unwrap();

        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].landed, should_land, "{case}: {reports:?}");
        if should_land {
            let manifest = std::fs::read_to_string(repo.join("crate/Cargo.toml")).unwrap();
            assert!(manifest.contains("version = \"0.1.1\""), "{manifest}");
            assert_eq!(
                finding_audit(&db, id, FindingReason::VersionBumpDiscarded),
                ("resolved".into(), Some(format!("task {id} landed")))
            );
        } else {
            assert_eq!(reports[0].reason, FindingReason::ReviewRejected);
            assert!(reports[0].detail.contains("Off-spec dependency edit"));
        }
    }
}

#[test]
fn refinery_relocks_workspace_member_in_root_lockfile() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    std::fs::create_dir_all(repo.join("member/src")).unwrap();
    write(
        &repo,
        "Cargo.toml",
        "[workspace]\nmembers = [\"member\"]\nresolver = \"3\"\n\n[workspace.package]\nversion = \"0.1.0\"\n",
    );
    write(
        &repo,
        "member/Cargo.toml",
        "[package]\nname = \"workspace-member\"\nversion.workspace = true\nedition = \"2024\"\n",
    );
    write(&repo, "member/src/lib.rs", "pub fn base() {}\n");
    assert!(
        Command::new("cargo")
            .args(["generate-lockfile", "--offline", "--manifest-path"])
            .arg(repo.join("Cargo.toml"))
            .status()
            .unwrap()
            .success()
    );
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "workspace base"]);
    git(&repo, &["checkout", "-b", "task/workspace"]);
    write(
        &repo,
        "Cargo.toml",
        "[workspace]\nmembers = [\"member\"]\nresolver = \"3\"\n\n[workspace.package]\nversion = \"0.9.0\"\n",
    );
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "member change"]);
    git(&repo, &["checkout", "main"]);

    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let id = ledger
        .add_task_scoped(
            "workspace bump-only",
            "spec",
            "impl",
            "low",
            &[],
            TaskControls {
                verifier_profile: "none",
                crates: &["workspace-member".into()],
                operator_driven_reason: None,
            },
        )
        .unwrap();
    let claimed = ledger.claim_task(id, "agent").unwrap();
    ledger
        .set_task_workspace(
            id,
            ClaimToken {
                owner: "agent",
                generation: claimed.attempt,
            },
            None,
            Some("task/workspace"),
        )
        .unwrap();
    ledger.finish_task(id, "agent", "done").unwrap();
    let reports = refinery::refine(
        &ledger,
        &RefineOptions {
            repo: repo.clone(),
            project_root: None,
            integration: "main".into(),
            subdir: ".".into(),
            tier: 0,
            review: false,
            db: db.clone(),
            echo: false,
            fleet_policy: None,
            profiles: Vec::new(),
            project_pack: String::new(),
            landing_gate: None,
            lane_policy: None,
        },
    )
    .unwrap();
    assert!(reports[0].landed, "{}", reports[0].detail);
    let lock = std::fs::read_to_string(repo.join("Cargo.lock")).unwrap();
    assert!(
        lock.contains("name = \"workspace-member\"\nversion = \"0.1.1\""),
        "{lock}"
    );
    assert_eq!(
        finding_audit(&db, id, FindingReason::VersionBumpDiscarded),
        ("resolved".into(), Some(format!("task {id} landed")))
    );
}

#[test]
fn agent_manifest_fault_bounces_and_refinery_continues_the_queue() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    std::fs::create_dir_all(repo.join("crate/src")).unwrap();
    write(
        &repo,
        "crate/Cargo.toml",
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(&repo, "crate/src/lib.rs", "pub fn base() {}\n");
    assert!(
        Command::new("cargo")
            .args(["generate-lockfile", "--offline", "--manifest-path"])
            .arg(repo.join("crate/Cargo.toml"))
            .status()
            .unwrap()
            .success()
    );
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "base crate"]);
    let base = git_out(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "-b", "task/bad-name", &base]);
    write(
        &repo,
        "crate/Cargo.toml",
        "[package]\nname = \"redirected\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "bad manifest"]);
    git(&repo, &["checkout", "-b", "task/good", &base]);
    write(&repo, "crate/src/good.rs", "pub fn good() {}\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "good work"]);
    git(&repo, &["checkout", "main"]);

    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    for (title, branch) in [("bad", "task/bad-name"), ("good", "task/good")] {
        let id = ledger
            .add_task(title, "spec", "impl", "low", &[], "none")
            .unwrap();
        let claimed = ledger.claim_task(id, "agent").unwrap();
        ledger
            .set_task_workspace(
                id,
                ClaimToken {
                    owner: "agent",
                    generation: claimed.attempt,
                },
                None,
                Some(branch),
            )
            .unwrap();
        ledger.finish_task(id, "agent", "done").unwrap();
    }
    let reports = refinery::refine(
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
    assert_eq!(reports.len(), 2);
    assert!(!reports[0].landed);
    assert_eq!(reports[0].reason, FindingReason::BranchContract);
    assert!(reports[0].detail.contains("changed package name"));
    assert!(reports[1].landed, "{}", reports[1].detail);
    assert!(repo.join("crate/src/good.rs").exists());
}

#[test]
fn colon_path_bounces_with_a_finding_and_refinery_lands_the_next_task() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    let base = git_out(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "-b", "task/colon", &base]);
    write(&repo, "a:b", "agent content\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "add unsupported colon path"]);
    git(&repo, &["checkout", "-b", "task/after-colon", &base]);
    write(&repo, "after-colon.txt", "land me\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "valid task after colon"]);
    git(&repo, &["checkout", "main"]);

    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let bad = add_done_task(&ledger, "colon path", "task/colon");
    let good = add_done_task(&ledger, "next task", "task/after-colon");
    let reports = run_refinery(
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
        None,
    )
    .unwrap();

    assert_eq!(reports.len(), 2);
    assert!(!reports[0].landed);
    assert_eq!(reports[0].reason, FindingReason::BranchContract);
    assert!(reports[0].detail.contains("unsupported ':' byte"));
    assert_eq!(ledger.task(bad).unwrap().unwrap().status, "bounced");
    assert!(
        ledger
            .task_findings(bad)
            .unwrap()
            .iter()
            .any(|finding| finding.3.contains("unsupported ':' byte"))
    );
    assert!(reports[1].landed, "{}", reports[1].detail);
    assert_eq!(ledger.task(good).unwrap().unwrap().status, "landed");
    assert!(repo.join("after-colon.txt").exists());
}

#[test]
fn poisoned_orphan_base_manifest_bounces_names_path_and_can_be_removed() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    std::fs::create_dir(repo.join("poison")).unwrap();
    std::fs::create_dir_all(repo.join("healthy/src")).unwrap();
    write(&repo, "poison/Cargo.toml", "[package\ninvalid = true\n");
    write(&repo, "poison/owned.txt", "base\n");
    write(
        &repo,
        "healthy/Cargo.toml",
        "[package]\nname='healthy'\nversion='0.1.0'\nedition='2024'\n",
    );
    write(&repo, "healthy/src/lib.rs", "pub fn healthy() {}\n");
    git(&repo, &["add", "."]);
    git(
        &repo,
        &["commit", "-m", "previously landed orphan manifest"],
    );
    let poisoned_base = git_out(&repo, &["rev-parse", "HEAD"]);

    git(
        &repo,
        &["checkout", "-b", "task/touches-poison", &poisoned_base],
    );
    write(&repo, "poison/owned.txt", "agent change\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "touch poisoned subtree"]);

    git(
        &repo,
        &["checkout", "-b", "task/after-poison", &poisoned_base],
    );
    write(&repo, "after-poison.txt", "land me\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "valid task after poison"]);

    git(
        &repo,
        &["checkout", "-b", "task/remove-poison", &poisoned_base],
    );
    git(&repo, &["rm", "poison/Cargo.toml"]);
    git(&repo, &["commit", "-m", "remove poisoned orphan manifest"]);
    git(&repo, &["checkout", "main"]);

    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let poisoned = add_done_task(&ledger, "touch poison", "task/touches-poison");
    let after = add_done_task(&ledger, "after poison", "task/after-poison");
    let removal = ledger
        .add_task_scoped(
            "remove poison",
            "spec",
            "impl",
            "low",
            &[],
            TaskControls {
                verifier_profile: "none",
                crates: &["healthy".into()],
                operator_driven_reason: None,
            },
        )
        .unwrap();
    let claimed = ledger.claim_task(removal, "test-agent").unwrap();
    ledger
        .set_task_workspace(
            removal,
            ClaimToken {
                owner: "test-agent",
                generation: claimed.attempt,
            },
            None,
            Some("task/remove-poison"),
        )
        .unwrap();
    ledger.finish_task(removal, "test-agent", "done").unwrap();
    let reports = run_refinery(
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
        None,
    )
    .unwrap();

    assert_eq!(reports.len(), 3);
    assert!(!reports[0].landed);
    assert_eq!(reports[0].reason, FindingReason::BranchContract);
    assert!(reports[0].detail.contains("poison/Cargo.toml"));
    assert!(reports[1].landed, "{}", reports[1].detail);
    assert!(reports[2].landed, "{}", reports[2].detail);
    assert_eq!(ledger.task(poisoned).unwrap().unwrap().status, "bounced");
    assert_eq!(ledger.task(after).unwrap().unwrap().status, "landed");
    assert_eq!(ledger.task(removal).unwrap().unwrap().status, "landed");
    assert!(
        ledger
            .task_findings(poisoned)
            .unwrap()
            .iter()
            .any(|finding| finding.3.contains("poison/Cargo.toml"))
    );
    assert!(repo.join("after-poison.txt").exists());
    assert!(!repo.join("poison/Cargo.toml").exists());
}

#[test]
fn valid_toml_orphan_with_unusable_semver_can_be_removed_and_landed() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    std::fs::create_dir_all(repo.join("orphan/src")).unwrap();
    write(
        &repo,
        "orphan/Cargo.toml",
        "[package]\nname='unusable-orphan'\nversion='0.1'\nedition='2024'\n",
    );
    write(&repo, "orphan/src/lib.rs", "pub fn orphan() {}\n");
    git(&repo, &["add", "."]);
    git(
        &repo,
        &["commit", "-m", "base with unusable orphan manifest"],
    );
    let base = git_out(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "-b", "task/remove-unusable", &base]);
    git(&repo, &["rm", "orphan/Cargo.toml"]);
    git(&repo, &["commit", "-m", "remove unusable orphan manifest"]);
    git(&repo, &["checkout", "main"]);

    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let id = add_done_task(&ledger, "remove unusable manifest", "task/remove-unusable");
    let reports = run_refinery(
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
        None,
    )
    .unwrap();

    assert_eq!(reports.len(), 1);
    assert!(reports[0].landed, "{}", reports[0].detail);
    assert_eq!(reports[0].task_status, "landed");
    assert_eq!(ledger.task(id).unwrap().unwrap().status, "landed");
    assert!(!repo.join("orphan/Cargo.toml").exists());
}

#[test]
fn refinery_report_names_parked_after_repeated_branch_contract_bounces() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    git(&repo, &["branch", "task/no-content"]);
    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let id = ledger
        .add_task("no-content branch", "spec", "impl", "low", &[], "none")
        .unwrap();
    let opts = RefineOptions {
        repo,
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
    };

    for count in 1..=3 {
        let claimed = ledger.claim_task(id, "test-agent").unwrap();
        ledger
            .set_task_workspace(
                id,
                ClaimToken {
                    owner: "test-agent",
                    generation: claimed.attempt,
                },
                None,
                Some("task/no-content"),
            )
            .unwrap();
        ledger.finish_task(id, "test-agent", "done").unwrap();
        let reports = run_refinery(&ledger, &opts, None).unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(
            reports[0].task_status,
            if count == 3 { "parked" } else { "bounced" }
        );
    }
    assert_eq!(ledger.task(id).unwrap().unwrap().status, "parked");
}

#[test]
fn refinery_prunes_locally_and_never_mutates_origin() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    let remote = tmp.path().join("canonical.git");
    std::fs::create_dir(&remote).unwrap();
    git(&remote, &["init", "--bare", "-b", "main"]);
    git(
        &repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&repo, &["push", "origin", "main"]);
    git(&repo, &["checkout", "-b", "task/62"]);
    write(&repo, "local-only.txt", "landed\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "local-only task"]);
    git(&repo, &["push", "origin", "task/62"]);
    git(&repo, &["checkout", "main"]);
    git(&repo, &["branch", "release"]);

    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let id = add_done_task(&ledger, "local-only prune", "task/62");
    let reports = run_refinery(
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
        None,
    )
    .unwrap();
    assert!(reports[0].landed, "{}", reports[0].detail);
    assert_eq!(ledger.task(id).unwrap().unwrap().status, "landed");
    let local = Command::new("git")
        .args(["show-ref", "--verify", "--quiet", "refs/heads/task/62"])
        .current_dir(&repo)
        .status()
        .unwrap();
    assert!(!local.success(), "local task branch must be pruned");
    let foreign = Command::new("git")
        .args(["show-ref", "--verify", "--quiet", "refs/heads/release"])
        .current_dir(&repo)
        .status()
        .unwrap();
    assert!(
        foreign.success(),
        "foreign local branch must never be pruned"
    );
    let published = Command::new("git")
        .args(["show-ref", "--verify", "--quiet", "refs/heads/task/62"])
        .current_dir(&remote)
        .status()
        .unwrap();
    assert!(
        published.success(),
        "origin task branch must survive local-only landing"
    );
}

#[test]
fn refinery_landing_gate_lands_green_bounces_red_and_fails_closed_on_spawn() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());

    git(&repo, &["checkout", "-b", "task/gate-green"]);
    write(&repo, "gate-green.txt", "green\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "green gate fixture"]);
    git(&repo, &["checkout", "main"]);

    git(&repo, &["checkout", "-b", "task/gate-red"]);
    write(&repo, "gate-red.txt", "red\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "red gate fixture"]);
    git(&repo, &["checkout", "main"]);

    git(&repo, &["checkout", "-b", "task/gate-sccache-bypass"]);
    write(&repo, "gate-sccache-bypass.txt", "bypass\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "sccache bypass gate fixture"]);
    git(&repo, &["checkout", "main"]);

    let gate = tmp.path().join("fixture-landing-gate");
    let bypass_marker = tmp.path().join("landing-gate-bypass-attempted");
    support::write_executable(
        &gate,
        format!(
            "#!/bin/sh\n\
         test \"$1\" = expected-arg || exit 90\n\
         test -z \"$FOREMAN_VERIFY_LANE_HELD\" || exit 91\n\
         if test -f gate-sccache-bypass.txt && test ! -f '{}'; then\n\
           touch '{}'\n\
           echo 'sccache: error: Operation not permitted (os error 1)' >&2\n\
           exit 2\n\
         fi\n\
         if test -f gate-sccache-bypass.txt; then\n\
           test -z \"$RUSTC_WRAPPER\" || exit 92\n\
         fi\n\
         if test -f gate-red.txt; then\n\
           echo 'hygiene sentinel from gate output' >&2\n\
           exit 23\n\
         fi\n\
         echo 'gate green'\n",
            bypass_marker.display(),
            bypass_marker.display()
        ),
    );

    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    for (title, branch) in [
        ("gate-green", "task/gate-green"),
        ("gate-red", "task/gate-red"),
        ("gate-sccache-bypass", "task/gate-sccache-bypass"),
    ] {
        let id = ledger
            .add_task(title, "spec", "impl", "low", &[], "none")
            .unwrap();
        let claimed = ledger.claim_task(id, "a").unwrap();
        ledger
            .set_task_workspace(
                id,
                ClaimToken {
                    owner: "a",
                    generation: claimed.attempt,
                },
                None,
                Some(branch),
            )
            .unwrap();
        ledger.finish_task(id, "a", "done").unwrap();
    }

    let opts = RefineOptions {
        repo: repo.clone(),
        project_root: None,
        integration: "main".into(),
        subdir: ".".into(),
        tier: 0,
        review: false,
        db: db.clone(),
        echo: false,
        fleet_policy: None,
        profiles: Vec::new(),
        project_pack: String::new(),
        landing_gate: None,
        lane_policy: None,
    };
    let gate_command = format!("{} expected-arg", gate.display());
    let reports = run_refinery(&ledger, &opts, Some(OsStr::new(&gate_command))).unwrap();
    assert_eq!(reports.len(), 3, "{reports:?}");
    assert!(reports[0].landed, "green gate: {}", reports[0].detail);
    assert!(!reports[1].landed, "red gate must bounce");
    assert!(reports[2].landed, "bypassed gate: {}", reports[2].detail);
    assert!(
        reports[1]
            .detail
            .contains("hygiene sentinel from gate output"),
        "red gate output tail must reach the bounce: {}",
        reports[1].detail
    );
    assert!(repo.join("gate-green.txt").exists());
    assert_eq!(ledger.task(1).unwrap().unwrap().status, "landed");
    assert_eq!(ledger.task(2).unwrap().unwrap().status, "bounced");
    assert_eq!(ledger.task(3).unwrap().unwrap().status, "landed");
    let findings = ledger.open_findings(10).unwrap();
    assert!(
        findings.iter().any(|(_, task, _, _, body)| {
            *task == Some(2) && body.contains("hygiene sentinel from gate output")
        }),
        "gate output must be filed as the finding: {findings:?}"
    );
    assert_eq!(
        finding_audit(&db, 3, FindingReason::SccacheBypassed),
        ("resolved".into(), Some("task 3 landed".into())),
        "landing-gate bypass must remain as resolved audit evidence"
    );

    // A configured executable that does not exist is still policy failure,
    // not an infrastructure error that stops the refinery queue.
    git(&repo, &["checkout", "-b", "task/gate-spawn-failure"]);
    write(&repo, "spawn-failure.txt", "must not land\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "spawn failure fixture"]);
    git(&repo, &["checkout", "main"]);
    let id = ledger
        .add_task("gate-spawn-failure", "spec", "impl", "low", &[], "none")
        .unwrap();
    let claimed = ledger.claim_task(id, "a").unwrap();
    ledger
        .set_task_workspace(
            id,
            ClaimToken {
                owner: "a",
                generation: claimed.attempt,
            },
            None,
            Some("task/gate-spawn-failure"),
        )
        .unwrap();
    ledger.finish_task(id, "a", "done").unwrap();

    let missing = tmp.path().join("fixture-gate-missing");
    let reports = run_refinery(&ledger, &opts, Some(missing.as_os_str())).unwrap();
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert!(!reports[0].landed, "spawn failure must bounce");
    assert!(
        reports[0].detail.contains("fixture-gate-missing"),
        "spawn failure diagnosis must name the command: {}",
        reports[0].detail
    );
    assert_eq!(ledger.task(id).unwrap().unwrap().status, "bounced");
    assert!(!repo.join("spawn-failure.txt").exists());
}

#[test]
fn refinery_refuses_laundering_and_noop_landings() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());

    // A stale branch pointing at the integration tip (no unique commits).
    git(&repo, &["branch", "task/stale"]);

    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let cases = [
        ("integration-name", "main"),
        ("remote-ref", "origin/main"),
        ("stale", "task/stale"),
        ("nonexistent", "task/ghost"),
    ];
    // All four names are syntactically valid; the refinery must reject them
    // SEMANTICALLY (integration itself, non-local ref, no-op tip, missing).
    for (title, branch) in cases {
        let id = ledger
            .add_task(title, "spec", "impl", "low", &[], "none")
            .unwrap();
        let claimed = ledger.claim_task(id, "a").unwrap();
        ledger
            .set_task_workspace(
                id,
                ClaimToken {
                    owner: "a",
                    generation: claimed.attempt,
                },
                None,
                Some(branch),
            )
            .unwrap();
        ledger.finish_task(id, "a", "done").unwrap();
    }

    let reports = run_refinery(
        &ledger,
        &RefineOptions {
            repo,
            project_root: None,
            integration: "main".into(),
            subdir: ".".into(),
            tier: 0,
            review: false,
            db: db.clone(),
            echo: false,
            fleet_policy: None,
            profiles: Vec::new(),
            project_pack: String::new(),
            landing_gate: None,
            lane_policy: None,
        },
        None,
    )
    .unwrap();
    assert!(
        reports.iter().all(|r| !r.landed),
        "none of these may land: {reports:?}"
    );
    for id in 1..=4 {
        assert_eq!(ledger.task(id).unwrap().unwrap().status, "bounced");
    }
}

#[test]
fn refinery_refuses_dirty_repo() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    write(&repo, "uncommitted.txt", "wip\n");

    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let err = run_refinery(
        &ledger,
        &RefineOptions {
            repo,
            project_root: None,
            integration: "main".into(),
            subdir: ".".into(),
            tier: 0,
            review: false,
            db: db.clone(),
            echo: false,
            fleet_policy: None,
            profiles: Vec::new(),
            project_pack: String::new(),
            landing_gate: None,
            lane_policy: None,
        },
        None,
    )
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("uncommitted changes"),
        "a dirty shared repo is an operator problem, not a task bounce: {err:#}"
    );
}

#[test]
fn refinery_forces_review_and_keeps_undelivered_single_arm_out_of_quality() {
    // Self-modification is ALWAYS reviewed: the policy gate deliberately
    // lets agents edit the foreman crate, and the stated backstop is the
    // merge-authority review — which must therefore be code-bound, not
    // --review-optional. A rejecting fake reviewer proves (a) the review
    // fires for a foreman-touching diff even with review: false, and (b)
    // it does NOT fire for an ordinary diff (which lands despite the
    // reviewer whose non-exact verdict is a harness delivery failure).
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    let reviewer = tmp.path().join("fake-claude");
    support::write_executable(
        &reviewer,
        r#"#!/bin/sh
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"finding\nVERDICT: approve","usage":{"input_tokens":10,"output_tokens":2},"total_cost_usd":0.01,"session_id":"review-1"}'
"#,
    );
    git(&repo, &["checkout", "-b", "task/ordinary"]);
    write(&repo, "notes.txt", "ordinary\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "ordinary work"]);
    git(&repo, &["checkout", "main"]);
    git(&repo, &["checkout", "-b", "task/selfmod"]);
    std::fs::create_dir_all(repo.join("crates/cosmix-foreman/src")).unwrap();
    write(&repo, "crates/cosmix-foreman/src/notes.rs", "// self\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "foreman-touching work"]);
    git(&repo, &["checkout", "main"]);

    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    for (title, branch) in [("ordinary", "task/ordinary"), ("selfmod", "task/selfmod")] {
        let id = ledger
            .add_task(title, "spec", "impl", "low", &[], "none")
            .unwrap();
        let claimed = ledger.claim_task(id, "a").unwrap();
        ledger
            .set_task_workspace(
                id,
                ClaimToken {
                    owner: "a",
                    generation: claimed.attempt,
                },
                None,
                Some(branch),
            )
            .unwrap();
        ledger.finish_task(id, "a", "done").unwrap();
    }
    let reports = run_refinery_with_policy_env(
        &ledger,
        &RefineOptions {
            repo: repo.clone(),
            project_root: None,
            integration: "main".into(),
            subdir: ".".into(),
            tier: 0,
            review: false,
            db: db.clone(),
            echo: false,
            fleet_policy: None,
            profiles: Vec::new(),
            project_pack: String::new(),
            landing_gate: None,
            lane_policy: None,
        },
        None,
        &[
            ("FOREMAN_REVIEW_OVERRIDE", Some(OsStr::new("claude"))),
            ("FOREMAN_TWO_ARM_REVIEW", None),
            ("FOREMAN_CLAUDE_BIN", Some(reviewer.as_os_str())),
            (
                "FOREMAN_REVIEW_MODEL",
                Some(OsStr::new("review-test-model")),
            ),
        ],
    )
    .unwrap();
    assert_eq!(reports.len(), 2);
    assert!(
        reports[0].landed,
        "ordinary diff must land WITHOUT consulting the (rejecting) reviewer: {}",
        reports[0].detail
    );
    assert!(
        !reports[1].landed,
        "foreman-touching diff must face the mandatory review"
    );
    assert!(
        reports[1]
            .detail
            .contains("invalid structured output (fail-closed reject)"),
        "bounce must carry the delivery failure: {}",
        reports[1].detail
    );
    let run = ledger.recent_runs(1).unwrap().remove(0);
    assert_eq!(run.role, "review");
    assert_eq!(run.model.as_deref(), Some("review-test-model"));
    assert_eq!(run.delivery, "harness_error");
    assert_eq!(run.quality, "unknown");
    assert_eq!(run.result.as_deref(), Some("rejected"));
    assert!(run.duration_ms.unwrap_or(0) > 0, "review duration is real");
    assert!(
        ledger.run_event_count(run.id).unwrap() > 0,
        "review events must not be discarded"
    );
}

#[test]
fn high_risk_two_arm_review_records_both_runs_and_reports_and_one_reject_bounces() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());

    let claude = tmp.path().join("fake-claude-two-arm");
    let claude_review = serde_json::json!({
        "verdict": "APPROVE",
        "findings": [],
        "files_inspected": ["high-risk.txt"],
    });
    let claude_report =
        format!("claude checked the implementation\nclaude found no defects\n{claude_review}");
    support::write_executable(
        &claude,
        format!(
            "#!/bin/sh\nprintf '%s\\n' '{}'\n",
            serde_json::json!({
                "type": "result",
                "subtype": "success",
                "is_error": false,
                "result": claude_report,
                "usage": {"input_tokens":11,"output_tokens":7},
                "total_cost_usd": 0.01,
            })
        ),
    );
    let codex = tmp.path().join("fake-codex-two-arm");
    let codex_review = serde_json::json!({
        "verdict": "REJECT",
        "findings": [{
            "severity": "MAJOR",
            "file": "high-risk.txt",
            "line": 1,
            "title": "Unsafe high-risk change",
            "body": "The fixture arm rejects this changed line.",
        }],
        "files_inspected": ["high-risk.txt"],
    });
    support::write_executable(
        &codex,
        format!(
            "#!/bin/sh\nprintf '%s\\n' '{}' '{}' '{}'\n",
            serde_json::json!({"type":"thread.started","thread_id":"test-thread"}),
            serde_json::json!({"type":"item.completed","item":{"type":"agent_message","text":format!("codex arm report\n{codex_review}")}}),
            serde_json::json!({"type":"turn.completed","usage":{"input_tokens":13,"cached_input_tokens":0,"output_tokens":9}}),
        ),
    );
    git(&repo, &["checkout", "-b", "task/two-arm"]);
    write(&repo, "high-risk.txt", "blast radius\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "high risk work"]);
    git(&repo, &["checkout", "main"]);

    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let id = ledger
        .add_task("two arm", "spec", "impl", "high", &[], "none")
        .unwrap();
    let claimed = ledger.claim_task(id, "a").unwrap();
    ledger
        .set_task_workspace(
            id,
            ClaimToken {
                owner: "a",
                generation: claimed.attempt,
            },
            None,
            Some("task/two-arm"),
        )
        .unwrap();
    ledger.finish_task(id, "a", "done").unwrap();

    let reports = run_refinery_with_policy_env(
        &ledger,
        &RefineOptions {
            repo,
            project_root: None,
            integration: "main".into(),
            subdir: ".".into(),
            tier: 0,
            review: true,
            db,
            echo: false,
            fleet_policy: None,
            profiles: Vec::new(),
            project_pack: String::new(),
            landing_gate: None,
            lane_policy: None,
        },
        None,
        &[
            ("FOREMAN_REVIEW_OVERRIDE", None),
            ("FOREMAN_TWO_ARM_REVIEW", Some(OsStr::new("true"))),
            ("FOREMAN_CLAUDE_BIN", Some(claude.as_os_str())),
            ("FOREMAN_CODEX_BIN", Some(codex.as_os_str())),
        ],
    )
    .unwrap();
    assert_eq!(reports.len(), 1);
    assert!(!reports[0].landed, "either arm's reject must bounce");

    let verification: serde_json::Value =
        serde_json::from_str(&ledger.latest_verification(id).unwrap().expect("tier-3 row"))
            .unwrap();
    assert_eq!(verification["kind"], "two-arm-review");
    assert_eq!(verification["approve"], false);
    let arms = verification["arms"].as_array().unwrap();
    assert_eq!(arms.len(), 2);
    assert!(
        arms.iter().any(|arm| arm["approve"] == true),
        "the approving fixture arm must remain approved"
    );
    assert!(arms.iter().any(|arm| {
        arm["report"]
            .as_str()
            .is_some_and(|report| report.contains("codex arm report"))
    }));

    let findings = ledger.task_findings_detailed(id).unwrap();
    assert_eq!(findings.len(), 1, "typed fixture finding lands directly");
    assert_eq!(findings[0].severity, "major");
    assert_eq!(findings[0].file.as_deref(), Some("high-risk.txt"));
    assert_eq!(findings[0].line, Some(1));
    assert_eq!(findings[0].title, "Unsafe high-risk change");
    assert!(findings[0].run_id.is_some());

    let review_runs = ledger
        .recent_runs(10)
        .unwrap()
        .into_iter()
        .filter(|run| run.role == "review")
        .collect::<Vec<_>>();
    assert_eq!(review_runs.len(), 2, "each arm is an accounted run");
    assert!(
        review_runs
            .iter()
            .any(|run| run.agent == "claude" && run.quality == "review_approved")
    );
    assert!(
        review_runs
            .iter()
            .any(|run| run.agent == "codex" && run.quality == "review_rejected")
    );
}

/// The policy snapshot must own the reviewer program for the whole refine
/// invocation. The parent starts this test in a child process whose real
/// `FOREMAN_CODEX_BIN` points at a rejecting fake B, while the injected
/// policy points at approving fake A. No process environment is mutated:
/// `Command::env` constructs the child's environment before it starts.
/// Reintroducing review.rs's late live-env read deterministically spawns B
/// and fails this test; the snapshot implementation spawns only A and lands.
#[test]
fn review_uses_injected_binary_snapshot_not_live_process_environment() {
    const CHILD: &str = "FOREMAN_PHASE1_SNAPSHOT_CHILD";
    const SNAPSHOT_BIN: &str = "FOREMAN_PHASE1_SNAPSHOT_BIN";
    const SNAPSHOT_MARKER: &str = "FOREMAN_PHASE1_SNAPSHOT_MARKER";
    const LIVE_MARKER: &str = "FOREMAN_PHASE1_LIVE_MARKER";

    if std::env::var_os(CHILD).is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let snapshot_bin = tmp.path().join("fake-codex-policy-snapshot");
        let live_bin = tmp.path().join("fake-codex-live-environment");
        let snapshot_marker = tmp.path().join("snapshot-spawned");
        let live_marker = tmp.path().join("live-spawned");
        let snapshot_review = serde_json::json!({
            "verdict": "APPROVE",
            "findings": [],
            "files_inspected": ["snapshot.txt"],
        });
        write_executable(
            &snapshot_bin,
            &format!(
                "#!/bin/sh\nprintf snapshot > '{}'\nprintf '%s\\n' '{}' '{}' '{}'\n",
                snapshot_marker.display(),
                serde_json::json!({"type":"thread.started","thread_id":"snapshot-thread"}),
                serde_json::json!({"type":"item.completed","item":{"type":"agent_message","text":format!("SNAPSHOT FAKE A\n{snapshot_review}")}}),
                serde_json::json!({"type":"turn.completed","usage":{"input_tokens":3,"cached_input_tokens":0,"output_tokens":2}}),
            ),
        );
        write_executable(
            &live_bin,
            &format!(
                "#!/bin/sh\nprintf live > '{}'\nprintf '%s\\n' \
                 '{{\"type\":\"thread.started\",\"thread_id\":\"live-thread\"}}' \
                 '{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":\"LIVE ENV FAKE B\\nVERDICT: REJECT\"}}}}' \
                 '{{\"type\":\"turn.completed\",\"usage\":{{\"input_tokens\":3,\"cached_input_tokens\":0,\"output_tokens\":2}}}}'\n",
                live_marker.display()
            ),
        );

        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "review_uses_injected_binary_snapshot_not_live_process_environment",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env(SNAPSHOT_BIN, &snapshot_bin)
            .env(SNAPSHOT_MARKER, &snapshot_marker)
            .env(LIVE_MARKER, &live_marker)
            .env("FOREMAN_CODEX_BIN", &live_bin)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "snapshot child failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(snapshot_marker.exists(), "injected fake A was not spawned");
        assert!(
            !live_marker.exists(),
            "late live-env read spawned fake B instead of the policy snapshot"
        );
        return;
    }

    let snapshot_bin = PathBuf::from(std::env::var_os(SNAPSHOT_BIN).unwrap());
    let snapshot_marker = PathBuf::from(std::env::var_os(SNAPSHOT_MARKER).unwrap());
    let live_marker = PathBuf::from(std::env::var_os(LIVE_MARKER).unwrap());
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    git(&repo, &["checkout", "-b", "task/snapshot-review"]);
    write(&repo, "snapshot.txt", "reviewed\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "snapshot review fixture"]);
    git(&repo, &["checkout", "main"]);

    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let id = add_done_task(&ledger, "snapshot review", "task/snapshot-review");
    let reports = run_refinery_with_policy_env(
        &ledger,
        &RefineOptions {
            repo,
            project_root: None,
            integration: "main".into(),
            subdir: ".".into(),
            tier: 0,
            review: true,
            db,
            echo: false,
            fleet_policy: None,
            profiles: Vec::new(),
            project_pack: String::new(),
            landing_gate: None,
            lane_policy: None,
        },
        None,
        &[
            ("FOREMAN_REVIEW_OVERRIDE", Some(OsStr::new("codex"))),
            ("FOREMAN_CODEX_BIN", Some(snapshot_bin.as_os_str())),
        ],
    )
    .unwrap();
    assert_eq!(reports.len(), 1);
    assert!(
        reports[0].landed,
        "the injected approving fake A must control the review: {}",
        reports[0].detail
    );
    assert!(snapshot_marker.exists(), "injected fake A was not spawned");
    assert!(
        !live_marker.exists(),
        "late live-env read spawned fake B instead of the policy snapshot"
    );
    let verification: serde_json::Value =
        serde_json::from_str(&ledger.latest_verification(id).unwrap().unwrap()).unwrap();
    assert_eq!(verification["approve"], true);
    assert!(
        verification["report"]
            .as_str()
            .unwrap()
            .contains("SNAPSHOT FAKE A")
    );
}

/// The refinery.rs invariant next to `run_landing_reviews` reads: "all
/// reservations are acquired before either session starts, so two-arm
/// review cannot degrade into one-arm review ... Any failure or REJECT is a
/// recorded, fail-closed rejection." This pins the "failure" half: a
/// harness-level arm failure (a child that exits non-zero without ever
/// streaming a `turn.completed`/verdict) must bounce the landing exactly
/// like an explicit REJECT does, even though the OTHER arm cleanly
/// approves. No sleeps, no scheduling dependence — the fake codex binary
/// exits immediately with no output, so `CodexParser::finish` deterministically
/// falls into `StopReason::Error` ("stream ended without turn.completed").
#[test]
fn high_risk_two_arm_review_bounces_when_one_arm_fails_harness_side() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());

    let claude = tmp.path().join("fake-claude-two-arm-ok");
    let claude_review = serde_json::json!({
        "verdict": "APPROVE",
        "findings": [],
        "files_inspected": ["high-risk.txt"],
    });
    support::write_executable(
        &claude,
        format!(
            "#!/bin/sh\nprintf '%s\\n' '{}'\n",
            serde_json::json!({
                "type":"result",
                "subtype":"success",
                "is_error":false,
                "result":format!("claude arm report\n{claude_review}"),
                "usage":{"input_tokens":11,"output_tokens":7},
                "total_cost_usd":0.01,
            })
        ),
    );
    // No stdout, no turn.completed, no verdict — an arm that dies before
    // rendering anything the parser can trust. `exit 1` proves the exit
    // status alone never leniently substitutes for stream truth.
    let codex = tmp.path().join("fake-codex-two-arm-crash");
    support::write_executable(&codex, "#!/bin/sh\nexit 1\n");
    git(&repo, &["checkout", "-b", "task/two-arm-crash"]);
    write(&repo, "high-risk.txt", "blast radius\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "high risk work"]);
    git(&repo, &["checkout", "main"]);

    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let id = ledger
        .add_task("two arm crash", "spec", "impl", "high", &[], "none")
        .unwrap();
    let (claimed, implementation_run) = ledger
        .start_attempt(id, "a", None, None, "claude", Some("implementer"))
        .unwrap();
    ledger
        .set_task_workspace(
            id,
            ClaimToken {
                owner: "a",
                generation: claimed.attempt,
            },
            None,
            Some("task/two-arm-crash"),
        )
        .unwrap();
    ledger.finish_task(id, "a", "done").unwrap();

    let reports = run_refinery_with_policy_env(
        &ledger,
        &RefineOptions {
            repo,
            project_root: None,
            integration: "main".into(),
            subdir: ".".into(),
            tier: 0,
            review: true,
            db,
            echo: false,
            fleet_policy: None,
            profiles: Vec::new(),
            project_pack: String::new(),
            landing_gate: None,
            lane_policy: None,
        },
        None,
        &[
            ("FOREMAN_REVIEW_OVERRIDE", None),
            ("FOREMAN_TWO_ARM_REVIEW", Some(OsStr::new("true"))),
            ("FOREMAN_CLAUDE_BIN", Some(claude.as_os_str())),
            ("FOREMAN_CODEX_BIN", Some(codex.as_os_str())),
        ],
    )
    .unwrap();
    assert_eq!(reports.len(), 1);
    assert!(
        !reports[0].landed,
        "an arm that fails harness-side must bounce the landing even though \
         the other arm approved: {}",
        reports[0].detail
    );
    assert_eq!(reports[0].reason, FindingReason::InfraRefusal);
    assert!(
        !reports[0].ladder_charged,
        "an approval plus a delivery failure is not a quality charge"
    );

    let verification: serde_json::Value =
        serde_json::from_str(&ledger.latest_verification(id).unwrap().expect("tier-3 row"))
            .unwrap();
    assert_eq!(verification["kind"], "two-arm-review");
    assert_eq!(verification["approve"], false);
    let arms = verification["arms"].as_array().unwrap();
    assert_eq!(arms.len(), 2);
    assert_eq!(
        arms.iter().find(|arm| arm["reviewer"] == "claude").unwrap()["approve"],
        true,
        "the healthy claude arm approved"
    );
    assert_eq!(
        arms.iter().find(|arm| arm["reviewer"] == "codex").unwrap()["approve"],
        false,
        "the crashed codex arm must be recorded as a rejection, not skipped"
    );

    let review_runs = ledger
        .recent_runs(10)
        .unwrap()
        .into_iter()
        .filter(|run| run.role == "review")
        .collect::<Vec<_>>();
    assert_eq!(
        review_runs.len(),
        2,
        "the failed arm is still an accounted run, not a silent drop"
    );
    let codex_run = review_runs
        .iter()
        .find(|run| run.agent == "codex")
        .expect("codex arm run recorded");
    assert_eq!(
        codex_run.quality, "unknown",
        "a harness-side arm failure is not implementation-quality evidence"
    );
    let implementation = ledger
        .recent_runs(10)
        .unwrap()
        .into_iter()
        .find(|run| run.id == implementation_run)
        .unwrap();
    assert_ne!(implementation.quality, "review_rejected");
    let task = ledger.task(id).unwrap().unwrap();
    assert_eq!(task.infra_refusals, 1);
    assert_eq!(task.review_rejections, 0);
}

#[test]
fn refinery_skips_landing_when_governor_has_no_headroom_before_tier1() {
    // Governor preflight: BEFORE tier-1 verification, refine checks whether
    // the merge-review reservation could currently be admitted. A hold that
    // cannot fit under the daily ceiling must skip the landing THIS RUN —
    // without running tier-1 cargo and without bouncing the task — leaving
    // it 'done' so the refinery can retry once headroom frees up.
    //
    // The review path must read the same conf-only ceiling as dispatch. The
    // injected provider is a complete environment, so ambient one-shot
    // overrides cannot leak into this test.
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());

    git(&repo, &["checkout", "-b", "task/needs-review"]);
    write(&repo, "notes.txt", "work\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "work"]);
    git(&repo, &["checkout", "main"]);

    let db = tmp.path().join("ledger.db");
    std::fs::write(
        tmp.path().join("foreman.conf.mix"),
        "daily_budget_usd: 50\n",
    )
    .unwrap();
    let ledger = Ledger::open(&db).unwrap();
    let id = ledger
        .add_task("needs-review", "spec", "impl", "low", &[], "none")
        .unwrap();
    let claimed = ledger.claim_task(id, "a").unwrap();
    ledger
        .set_task_workspace(
            id,
            ClaimToken {
                owner: "a",
                generation: claimed.attempt,
            },
            None,
            Some("task/needs-review"),
        )
        .unwrap();
    ledger.finish_task(id, "a", "done").unwrap();

    // Pre-spend enough that the review reservation plainly cannot fit —
    // without touching any process-global env var (this ledger is private
    // to this test, but the environment is not: every foreman-dispatched
    // agent runs with FOREMAN_DAILY_BUDGET_USD set, so a hardcoded "47 of
    // the default 50" left the reservation fitting and drove this test into
    // a real, billed merge review). Read the ceilings the refine governor
    // will actually use and pre-spend against those.
    // Leave half a reservation of headroom, so it is the reservation itself
    // that fails to fit — the state this preflight exists to catch. A
    // dimension whose ceiling or reserve is zero cannot express that, so it
    // is skipped rather than faked.
    // This is specifically a dollar-metered Claude headroom test. Reviewer
    // routing is configurable now, so select that lane explicitly instead of
    // relying on the pre-0.16 implementer-derived default.
    let policy = fleet_policy(
        &db,
        None,
        &[("FOREMAN_REVIEW_OVERRIDE", Some(OsStr::new("claude")))],
    )
    .unwrap();
    let ceilings = Governor::from_policy(&db, &policy);
    let reserve_usd = policy.reserve_usd.value;
    let reserve_tokens = policy.reserve_tokens.value;
    let (pre_usd, pre_tokens) = if ceilings.daily_budget_usd > 0.0 && reserve_usd > 0.0 {
        ((ceilings.daily_budget_usd - reserve_usd / 2.0).max(0.0), 0)
    } else if ceilings.daily_output_tokens > 0 && reserve_tokens > 0 {
        (
            0.0,
            ceilings
                .daily_output_tokens
                .saturating_sub(reserve_tokens / 2),
        )
    } else {
        panic!(
            "no live ceiling/reserve pair in this environment \
             (${:.2}/{} ceiling, ${:.2}/{} reserve) — the preflight has \
             nothing to refuse against",
            ceilings.daily_budget_usd, ceilings.daily_output_tokens, reserve_usd, reserve_tokens
        );
    };
    // Charge the synthetic spend to a separate task. A successful run on
    // the landing task keeps the capacity fixture separate from review
    // evidence and quality attribution.
    let spend_task = ledger
        .add_task("budget fixture", "spec", "impl", "low", &[], "none")
        .unwrap();
    spend_run(&ledger, spend_task, pre_usd, pre_tokens);

    let reports = refinery::refine(
        &ledger,
        &RefineOptions {
            repo: repo.clone(),
            project_root: None,
            integration: "main".into(),
            subdir: ".".into(),
            tier: 0,
            review: true,
            db: db.clone(),
            echo: false,
            fleet_policy: Some(policy),
            profiles: Vec::new(),
            project_pack: String::new(),
            landing_gate: None,
            lane_policy: None,
        },
    )
    .unwrap();

    // Skipped, not bounced, not landed: no report at all this run.
    assert!(reports.is_empty(), "{reports:?}");
    assert_eq!(
        ledger.task(id).unwrap().unwrap().status,
        "done",
        "a refused reservation must leave the task retryable, not bounced"
    );
    // Tier-1 cargo never ran: no verification row was recorded.
    assert!(
        ledger.verification_reports(id, 10).unwrap().is_empty(),
        "the preflight must turn the landing away BEFORE tier-1 verification runs"
    );
}

#[test]
fn refinery_continues_queue_after_reserve_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());

    // Only the first task needs review. The second proves that losing the
    // non-binding-preflight race does not stop unrelated queue work.
    git(&repo, &["checkout", "-b", "task/review-race"]);
    std::fs::create_dir_all(repo.join("crates/cosmix-foreman")).unwrap();
    write(&repo, "crates/cosmix-foreman/race.txt", "review me\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "reviewed work"]);
    git(&repo, &["checkout", "main"]);
    git(&repo, &["checkout", "-b", "task/following"]);
    write(&repo, "following.txt", "land me\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "following work"]);
    git(&repo, &["checkout", "main"]);

    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let reviewed = add_done_task(&ledger, "review race", "task/review-race");
    let following = add_done_task(&ledger, "following", "task/following");

    // The preflight sees $10 free and admits the $6 review hold. The landing
    // gate then models a concurrent dispatcher taking $5 before the binding
    // reserve, which must be a typed governor refusal rather than an IO error.
    let gate = tmp.path().join("consume-headroom.sh");
    write_executable(
        &gate,
        &format!(
            "#!/bin/sh\nexec /usr/sbin/sqlite3 '{}' \"INSERT INTO reservations \
             (claimant, task_id, usd, tokens, pid, pid_start, created_at) VALUES \
             ('concurrent-dispatch', NULL, 5.0, 0, NULL, NULL, \
             '2999-01-01T00:00:00+00:00')\"\n",
            db.display()
        ),
    );

    let reports = run_refinery_with_policy_env(
        &ledger,
        &RefineOptions {
            repo: repo.clone(),
            project_root: None,
            integration: "main".into(),
            subdir: ".".into(),
            tier: 0,
            review: false,
            db: db.clone(),
            echo: false,
            fleet_policy: None,
            profiles: Vec::new(),
            project_pack: String::new(),
            landing_gate: None,
            lane_policy: None,
        },
        Some(gate.as_os_str()),
        &[
            ("FOREMAN_REVIEW_OVERRIDE", Some(OsStr::new("claude"))),
            ("FOREMAN_DAILY_BUDGET_USD", Some(OsStr::new("10"))),
            ("FOREMAN_DAILY_OUTPUT_TOKENS", Some(OsStr::new("0"))),
            ("FOREMAN_RESERVE_USD", Some(OsStr::new("6"))),
            ("FOREMAN_RESERVE_TOKENS", Some(OsStr::new("0"))),
        ],
    )
    .unwrap();

    assert_eq!(reports.len(), 1, "only the following task should report");
    assert_eq!(reports[0].task_id, following);
    assert!(reports[0].landed, "following task should still land");
    assert_eq!(ledger.task(reviewed).unwrap().unwrap().status, "done");
    assert_eq!(ledger.task(following).unwrap().unwrap().status, "landed");
    assert!(
        ledger.task_findings(reviewed).unwrap().is_empty(),
        "capacity races are not task findings"
    );
}

#[test]
fn refinery_backs_off_non_governor_reserve_error_and_continues_queue() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());

    git(&repo, &["checkout", "-b", "task/review-io"]);
    std::fs::create_dir_all(repo.join("crates/cosmix-foreman")).unwrap();
    write(&repo, "crates/cosmix-foreman/io.txt", "review me\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "reviewed work"]);
    git(&repo, &["checkout", "main"]);
    git(&repo, &["checkout", "-b", "task/never-attempted"]);
    write(&repo, "later.txt", "must wait\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "later work"]);
    git(&repo, &["checkout", "main"]);

    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let reviewed = add_done_task(&ledger, "review IO", "task/review-io");
    let later = add_done_task(&ledger, "later", "task/never-attempted");

    // Inject a non-capacity failure at the exact INSERT performed by
    // Ledger::reserve(), after the preflight has passed.
    let gate = tmp.path().join("break-reserve.sh");
    write_executable(
        &gate,
        &format!(
            "#!/bin/sh\nexec /usr/sbin/sqlite3 '{}' \"CREATE TRIGGER IF NOT EXISTS fail_reservation \
             BEFORE INSERT ON reservations BEGIN SELECT RAISE(FAIL, \
             'injected reservation failure'); END\"\n",
            db.display()
        ),
    );

    let reports = run_refinery_with_policy_env(
        &ledger,
        &RefineOptions {
            repo: repo.clone(),
            project_root: None,
            integration: "main".into(),
            subdir: ".".into(),
            tier: 0,
            review: false,
            db: db.clone(),
            echo: false,
            fleet_policy: None,
            profiles: Vec::new(),
            project_pack: String::new(),
            landing_gate: None,
            lane_policy: None,
        },
        Some(gate.as_os_str()),
        &[
            ("FOREMAN_DAILY_BUDGET_USD", Some(OsStr::new("10"))),
            ("FOREMAN_DAILY_OUTPUT_TOKENS", Some(OsStr::new("0"))),
            ("FOREMAN_RESERVE_USD", Some(OsStr::new("6"))),
            ("FOREMAN_RESERVE_TOKENS", Some(OsStr::new("0"))),
        ],
    )
    .unwrap();

    assert_eq!(reports.len(), 2);
    assert_eq!(reports[0].reason, FindingReason::InfraRefusal);
    assert!(reports[0].detail.contains("injected reservation failure"));
    assert!(reports[1].landed, "{}", reports[1].detail);
    let reviewed = ledger.task(reviewed).unwrap().unwrap();
    assert_eq!(reviewed.status, "bounced");
    assert_eq!(reviewed.infra_refusals, 1);
    assert_eq!(reviewed.branch_contract_failures, 0);
    assert!(reviewed.dispatch_after.is_some());
    assert_eq!(ledger.task(later).unwrap().unwrap().status, "landed");
}

// ---- task worktrees (dispatch-side) ----

#[test]
fn task_worktree_created_as_sibling_and_reused() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    let wt = cosmix_foreman::refinery::ensure_task_worktree(&repo, 7, "task/7", None)
        .unwrap()
        .path;
    // Sibling of the clone: same path depth, so ../..-style sibling deps
    // resolve to the same neighbours the clone sees.
    assert_eq!(wt, tmp.path().join("task-7"));
    let head = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(&wt)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), "task/7");
    // A retry reuses the worktree, partial state included.
    write(&wt, "wip.txt", "partial\n");
    let again = cosmix_foreman::refinery::ensure_task_worktree(&repo, 7, "task/7", None)
        .unwrap()
        .path;
    assert_eq!(again, wt);
    assert!(wt.join("wip.txt").exists(), "retry must keep partial state");
}

#[test]
fn task_worktree_survives_branch_left_by_deleted_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    let wt = cosmix_foreman::refinery::ensure_task_worktree(&repo, 3, "task/3", None)
        .unwrap()
        .path;
    // Simulate a lost worktree dir (tmpfs cleanup, operator rm) with the
    // branch surviving: the stale registration must be pruned and the
    // existing branch re-checked-out, not `-b`-recreated (which would fail).
    std::fs::remove_dir_all(&wt).unwrap();
    let wt2 = cosmix_foreman::refinery::ensure_task_worktree(&repo, 3, "task/3", None)
        .unwrap()
        .path;
    assert_eq!(wt2, wt);
    assert!(wt2.join("base.txt").exists());
}

#[test]
fn task_worktree_refuses_squatting_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    std::fs::create_dir(tmp.path().join("task-9")).unwrap();
    let err = cosmix_foreman::refinery::ensure_task_worktree(&repo, 9, "task/9", None).unwrap_err();
    assert!(
        format!("{err:#}").contains("not this clone's worktree"),
        "a squatting dir must be refused, not adopted: {err:#}"
    );
}

#[test]
fn task_worktree_refuses_unrelated_repo_on_matching_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    // An unrelated repo squatting the worktree path, on the RIGHT branch
    // name: branch match alone must not be adopted — provenance is the
    // clone's worktree registry, not the checked-out ref.
    let imposter = tmp.path().join("task-5");
    std::fs::create_dir(&imposter).unwrap();
    git(&imposter, &["init", "-b", "task/5"]);
    git(&imposter, &["config", "user.name", "t"]);
    git(&imposter, &["config", "user.email", "t@t"]);
    write(&imposter, "x.txt", "x\n");
    git(&imposter, &["add", "."]);
    git(&imposter, &["commit", "-m", "x"]);
    let err = cosmix_foreman::refinery::ensure_task_worktree(&repo, 5, "task/5", None).unwrap_err();
    assert!(
        format!("{err:#}").contains("not this clone's worktree"),
        "an unrelated repo must be refused, not adopted: {err:#}"
    );
}

#[test]
fn task_worktree_refuses_inplace_imposter_with_stale_registration() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    let wt = cosmix_foreman::refinery::ensure_task_worktree(&repo, 6, "task/6", None)
        .unwrap()
        .path;
    // The registered worktree dir is replaced IN PLACE by an unrelated repo
    // on the matching branch: `worktree prune` keeps the registration (the
    // path exists again), so registry + branch checks both pass — only the
    // git-common-dir tie can refuse it.
    std::fs::remove_dir_all(&wt).unwrap();
    std::fs::create_dir(&wt).unwrap();
    git(&wt, &["init", "-b", "task/6"]);
    git(&wt, &["config", "user.name", "t"]);
    git(&wt, &["config", "user.email", "t@t"]);
    write(&wt, "x.txt", "x\n");
    git(&wt, &["add", "."]);
    git(&wt, &["commit", "-m", "x"]);
    let err = cosmix_foreman::refinery::ensure_task_worktree(&repo, 6, "task/6", None).unwrap_err();
    assert!(
        format!("{err:#}").contains("not this clone's worktree"),
        "an in-place imposter must be refused, not adopted: {err:#}"
    );
}

// ---- worktree-provisioning rebase onto the integration branch ----
//
// Retries reuse the task worktree AND branch by design — the partial state
// is the point. But a branch left sitting on an OLD integration commit
// keeps re-testing old in-tree code under tier-0, so a landed harness fix
// never reaches an existing task branch (measured 2026-08-20: an attempt
// dispatched after the fix still failed against its pre-fix base). Every
// reuse therefore replays the branch onto the integration head.

/// Read a git value out of `dir`, trimmed. Panics on failure — these are
/// facts the assertions below depend on, not conditions to skip over.
fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn task_worktree_reuse_rebases_onto_integration_head() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    let first =
        cosmix_foreman::refinery::ensure_task_worktree(&repo, 11, "task/11", Some("main")).unwrap();
    let wt = first.path.clone();
    // Provisioning a new worktree does not enter the reuse-path rebase.
    assert_eq!(first.rebase, None);

    // The agent's first attempt: one commit plus uncommitted partial state.
    write(&wt, "work.txt", "attempt one\n");
    git(&wt, &["add", "."]);
    git(&wt, &["commit", "-m", "task work"]);
    let task_commit = git_out(&wt, &["rev-parse", "HEAD"]);
    write(&wt, "wip.txt", "not committed\n");
    git(&wt, &["add", "wip.txt"]);

    // Meanwhile a harness fix lands on the integration branch.
    write(&repo, "harness-fix.txt", "the fix\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "harness fix"]);
    let new_base = git_out(&repo, &["rev-parse", "main"]);

    let again =
        cosmix_foreman::refinery::ensure_task_worktree(&repo, 11, "task/11", Some("main")).unwrap();
    assert_eq!(again.path, wt);
    let Some(cosmix_foreman::refinery::RebaseOutcome::Rebased { base, from, to }) = &again.rebase
    else {
        panic!(
            "a non-conflicting replay must report Rebased: {:?}",
            again.rebase
        );
    };
    assert_eq!(base, &new_base);
    assert_eq!(from, &task_commit);
    assert_ne!(
        to, &task_commit,
        "the branch must have moved onto the new base"
    );

    // The whole point: the reused worktree now carries the harness fix.
    assert!(
        wt.join("harness-fix.txt").exists(),
        "a reused worktree must pick up the integration commit it was rebased onto"
    );
    // ...without losing the attempt's own work, committed or not.
    assert!(
        wt.join("work.txt").exists(),
        "the task's commit must survive the replay"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("wip.txt")).unwrap(),
        "not committed\n",
        "--autostash must give the uncommitted partial state back: it is why reuse exists"
    );
    assert_eq!(
        git_out(&wt, &["status", "--porcelain", "--untracked-files=no"]),
        "A  wip.txt",
        "--autostash must restore the index as well as the file"
    );
    assert_eq!(
        git_out(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "task/11"
    );
    assert_eq!(
        git_out(&wt, &["rev-parse", "HEAD"]),
        *to,
        "the reported new tip must be the branch's actual tip"
    );
    assert_eq!(
        git_out(&wt, &["rev-parse", "HEAD~1"]),
        new_base,
        "the replayed commit must sit directly on the integration head"
    );
    assert!(!again.rebase.as_ref().unwrap().conflicted());
}

#[test]
fn task_worktree_reuse_hands_rebase_conflict_to_agent_without_charging() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("l.db");
    let ledger = Ledger::open(&db).unwrap();
    let task = ledger
        .add_task("conflicting", "spec", "impl", "low", &[], "none")
        .unwrap();
    let repo = git_repo(tmp.path());
    let wt = cosmix_foreman::refinery::ensure_task_worktree(&repo, task, "task/1", Some("main"))
        .unwrap()
        .path;

    // The attempt and the integration branch edit the SAME line of the
    // SAME file — the one case git cannot replay on its own.
    write(&wt, "base.txt", "the task's answer\n");
    git(&wt, &["add", "."]);
    git(&wt, &["commit", "-m", "task edits base"]);
    let before = git_out(&wt, &["rev-parse", "HEAD"]);
    write(&wt, "wip.txt", "not committed\n");
    git(&wt, &["add", "wip.txt"]);

    write(&repo, "base.txt", "the harness's answer\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "harness edits base"]);
    let new_base = git_out(&repo, &["rev-parse", "main"]);

    let again = cosmix_foreman::refinery::ensure_task_worktree(&repo, task, "task/1", Some("main"))
        .unwrap();
    let Some(outcome) = &again.rebase else {
        panic!("provisioning with an integration branch always reports a verdict");
    };
    let cosmix_foreman::refinery::RebaseOutcome::Conflicted {
        base, from, files, ..
    } = outcome
    else {
        panic!("a same-line conflict must report Conflicted, not {outcome:?}");
    };
    assert_eq!(base, &new_base);
    assert_eq!(from, &before);
    assert_eq!(
        files,
        &vec!["base.txt".to_string()],
        "the finding's whole value is NAMING the files the agent has to resolve"
    );

    // Un-rebased: the branch is exactly where it was, no rebase is open,
    // nothing was auto-resolved, and the partial state came back. A
    // worktree left mid-rebase would poison every later attempt at this task.
    assert_eq!(
        git_out(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "task/1",
        "an aborted rebase must leave the branch checked out, not a detached HEAD"
    );
    assert_eq!(git_out(&wt, &["rev-parse", "HEAD"]), before);
    let git_dir = PathBuf::from(git_out(&wt, &["rev-parse", "--absolute-git-dir"]));
    assert!(
        !git_dir.join("rebase-merge").exists() && !git_dir.join("rebase-apply").exists(),
        "the rebase must have been ABORTED, not left open in {}",
        git_dir.display()
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("base.txt")).unwrap(),
        "the task's answer\n",
        "no conflict markers, no auto-resolution: the tree is the agent's own"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("wip.txt")).unwrap(),
        "not committed\n",
        "--abort must restore the autostashed partial state too"
    );
    assert_eq!(
        git_out(&wt, &["status", "--porcelain", "--untracked-files=no"]),
        "A  wip.txt",
        "an aborted rebase must restore the staged partial state exactly"
    );

    // Replay task 25's four provisioning sweeps: findings are handed to the
    // agent, but no non-existent attempt is charged or parked.
    for _ in 0..4 {
        cosmix_foreman::refinery::bounce_rebase_conflict(&ledger, task, "task/1", "main", outcome)
            .unwrap();
    }
    let findings = ledger.task_findings(task).unwrap();
    assert_eq!(findings.len(), 4, "{findings:?}");
    let (_, severity, title, body) = &findings[0];
    assert_eq!(severity, "major");
    assert!(
        title.contains("task/1") && title.contains("main"),
        "{title}"
    );
    assert!(
        body.contains("base.txt"),
        "the finding must NAME the conflicting file — it is what the agent resolves from: {body}"
    );
    assert!(
        body.contains(&new_base),
        "the base of record must be in the trail: {body}"
    );
    assert!(
        body.contains("ABORTED"),
        "the finding must tell the agent the branch was left alone: {body}"
    );
    let reason: String = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT reason_code FROM findings WHERE task_id = ?1 LIMIT 1",
            [task],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(reason, "rebase_conflict");
    assert_eq!(
        ledger.task(task).unwrap().unwrap().ladder_failures,
        0,
        "provisioning has no agent attempt to charge"
    );
    assert_eq!(ledger.task(task).unwrap().unwrap().status, "queued");

    // The next sweep launches exactly one real attempt on the aborted branch;
    // the open typed finding is what lowering turns into the rebase-first
    // instruction in the implementation prompt.
    let (launched, run_id) = ledger
        .start_attempt(
            task,
            "codex:fixture",
            wt.to_str(),
            Some("task/1"),
            "codex",
            None,
        )
        .unwrap();
    assert_eq!(launched.attempt, 1);
    assert_eq!(launched.status, "running");
    let task_runs: Vec<_> = ledger
        .recent_runs(10)
        .unwrap()
        .into_iter()
        .filter(|run| run.task_id == task)
        .collect();
    assert_eq!(task_runs.len(), 1);
    assert_eq!(task_runs[0].id, run_id);
    assert!(
        ledger
            .task_has_open_finding_reason(task, FindingReason::RebaseConflict)
            .unwrap()
    );
}

#[test]
fn rung_refusal_never_charges_quality_ladder() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("l.db")).unwrap();
    let id = ledger
        .add_task("t", "spec", "feature", "low", &[], "none")
        .unwrap();
    assert!(
        ledger
            .file_rung_refusal(id, "glm", "lane cannot meter dollars")
            .unwrap()
    );
    assert_eq!(ledger.task(id).unwrap().unwrap().ladder_failures, 0);
    // Once claimed, a launch failure is NOT a countable refusal here.
    ledger.claim_task(id, "claude:w1").unwrap();
    assert!(
        !ledger
            .file_rung_refusal(id, "claude:sonnet", "claimed concurrently")
            .unwrap()
    );
    assert_eq!(ledger.task(id).unwrap().unwrap().ladder_failures, 0);
}

/// `refine --tier 2`: the in-process composition that self-deadlocked before
/// the lane-held handshake. `refine` holds the clone lane for its whole run,
/// then calls `verify::run_tier` on a throwaway worktree that is a SIBLING of
/// the repo — so the worktree's `../clone.lock` is the very file refine is
/// holding. Re-acquiring there waits out the full timeout and fails; joining
/// is the only correct move.
///
/// Latent in production only because the unit passes `--tier 1`, and the CLI
/// accepts `--tier 2`. The parent runs the fixture in a child with a 1s wait
/// override, so a regression fails quickly without mutating this test
/// process's environment.
#[test]
fn refine_at_tier_two_joins_its_own_clone_lane() {
    const CHILD: &str = "FOREMAN_PHASE1_TIER2_CLONE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "refine_at_tier_two_joins_its_own_clone_lane",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("FOREMAN_CLONE_LOCK_WAIT_SECS", "1")
            .env_remove("FOREMAN_CLONE_LANE_HELD")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "tier-2 clone-lane child failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());

    git(&repo, &["checkout", "-b", "task/tier2"]);
    write(&repo, "tier2.txt", "work\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "tier2 work"]);
    git(&repo, &["checkout", "main"]);

    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    // Profile "none" so tier 2 runs no verifier commands — this test is
    // about the lane, not about cargo.
    let id = ledger
        .add_task("tier2", "spec", "impl", "low", &[], "none")
        .unwrap();
    let claimed = ledger.claim_task(id, "a").unwrap();
    ledger
        .set_task_workspace(
            id,
            ClaimToken {
                owner: "a",
                generation: claimed.attempt,
            },
            None,
            Some("task/tier2"),
        )
        .unwrap();
    ledger.finish_task(id, "a", "done").unwrap();

    let reports = run_refinery(
        &ledger,
        &RefineOptions {
            repo: repo.clone(),
            project_root: None,
            integration: "main".into(),
            subdir: ".".into(),
            tier: 2,
            review: false,
            db: db.clone(),
            echo: false,
            fleet_policy: None,
            profiles: Vec::new(),
            project_pack: String::new(),
            landing_gate: None,
            lane_policy: None,
        },
        None,
    )
    .unwrap();
    assert_eq!(reports.len(), 1);
    assert!(
        reports[0].landed,
        "tier-2 refine must land, not deadlock on its own lane: {}",
        reports[0].detail
    );
    assert!(repo.join("tier2.txt").exists());
}

/// `--subdir` is the FALLBACK the refinery's tier-1 pre-land gate must keep
/// using for a profile that owns no `cwd` of its own — the fleet units keep
/// passing it and must keep working unchanged. A nontrivial (non-`.`)
/// subdir at tier 1 (not just tier 0) exercises the exact code path fixed
/// here: `land_one` used to compute the verify directory straight from
/// `opts.subdir` without ever asking the task's profile.
#[test]
fn refinery_tier1_respects_subdir_fallback_and_bounces_missing_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    std::fs::create_dir_all(repo.join("sub")).unwrap();
    write(&repo, "sub/base.txt", "base\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "add sub"]);

    // Clean branch: work lands inside the nontrivial subdir.
    git(&repo, &["checkout", "-b", "task/subdir-ok"]);
    write(&repo, "sub/clean.txt", "clean\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "clean work in sub"]);
    git(&repo, &["checkout", "main"]);

    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    // "none" profile owns no cwd, so tier 1 must fall back to --subdir —
    // proving that fallback still reaches the resolved directory, not just
    // that an empty-command profile trivially "passes" regardless of dir.
    let id = ledger
        .add_task("subdir-ok", "spec", "impl", "low", &[], "none")
        .unwrap();
    let claimed = ledger.claim_task(id, "a").unwrap();
    ledger
        .set_task_workspace(
            id,
            ClaimToken {
                owner: "a",
                generation: claimed.attempt,
            },
            None,
            Some("task/subdir-ok"),
        )
        .unwrap();
    ledger.finish_task(id, "a", "done").unwrap();

    let reports = run_refinery(
        &ledger,
        &RefineOptions {
            repo: repo.clone(),
            project_root: None,
            integration: "main".into(),
            subdir: "sub".into(),
            tier: 1,
            review: false,
            db: db.clone(),
            echo: false,
            fleet_policy: None,
            profiles: Vec::new(),
            project_pack: String::new(),
            landing_gate: None,
            lane_policy: None,
        },
        None,
    )
    .unwrap();
    assert_eq!(reports.len(), 1);
    assert!(reports[0].landed, "{}", reports[0].detail);
    assert!(repo.join("sub/clean.txt").exists());
}

/// A `--subdir` that does not exist on the branch being landed must bounce
/// the task loudly, with the subdir named, at tier 1 — never silently fall
/// back to the worktree root and verify (or land) something unrelated.
#[test]
fn refinery_tier1_bounces_on_missing_subdir() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());

    git(&repo, &["checkout", "-b", "task/no-such-subdir"]);
    write(&repo, "clean.txt", "clean\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "work with no matching subdir"]);
    git(&repo, &["checkout", "main"]);

    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let id = ledger
        .add_task("no-such-subdir", "spec", "impl", "low", &[], "none")
        .unwrap();
    let claimed = ledger.claim_task(id, "a").unwrap();
    ledger
        .set_task_workspace(
            id,
            ClaimToken {
                owner: "a",
                generation: claimed.attempt,
            },
            None,
            Some("task/no-such-subdir"),
        )
        .unwrap();
    ledger.finish_task(id, "a", "done").unwrap();

    let reports = run_refinery(
        &ledger,
        &RefineOptions {
            repo: repo.clone(),
            project_root: None,
            integration: "main".into(),
            subdir: "does-not-exist".into(),
            tier: 1,
            review: false,
            db: db.clone(),
            echo: false,
            fleet_policy: None,
            profiles: Vec::new(),
            project_pack: String::new(),
            landing_gate: None,
            lane_policy: None,
        },
        None,
    )
    .unwrap();
    assert_eq!(reports.len(), 1);
    assert!(
        !reports[0].landed,
        "a missing subdir must bounce, not silently land: {}",
        reports[0].detail
    );
    assert!(
        reports[0].detail.contains("does-not-exist"),
        "bounce detail should name the missing subdir: {}",
        reports[0].detail
    );
    // Refused, not landed: main never gained the branch's file.
    assert!(!repo.join("clean.txt").exists());
    assert_eq!(ledger.task(id).unwrap().unwrap().status, "bounced");
}

/// An unknown verifier-profile name is an infra-level problem. It receives
/// infrastructure backoff rather than a branch-contract charge, and it does
/// not stop unrelated entries in the landing queue.
#[test]
fn refinery_tier1_backs_off_unknown_profile_as_infrastructure() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());

    git(&repo, &["checkout", "-b", "task/bogus-profile"]);
    write(&repo, "clean.txt", "clean\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "work"]);
    git(&repo, &["checkout", "main"]);

    let db = tmp.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let id = ledger
        .add_task("bogus-profile", "spec", "impl", "low", &[], "yolo")
        .unwrap();
    let claimed = ledger.claim_task(id, "a").unwrap();
    ledger
        .set_task_workspace(
            id,
            ClaimToken {
                owner: "a",
                generation: claimed.attempt,
            },
            None,
            Some("task/bogus-profile"),
        )
        .unwrap();
    ledger.finish_task(id, "a", "done").unwrap();

    let reports = run_refinery(
        &ledger,
        &RefineOptions {
            repo: repo.clone(),
            project_root: None,
            integration: "main".into(),
            subdir: ".".into(),
            tier: 1,
            review: false,
            db: db.clone(),
            echo: false,
            fleet_policy: None,
            profiles: Vec::new(),
            project_pack: String::new(),
            landing_gate: None,
            lane_policy: None,
        },
        None,
    )
    .unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].reason, FindingReason::InfraRefusal);
    assert!(reports[0].detail.contains("unknown verifier profile"));
    assert!(!repo.join("clean.txt").exists());
    let task = ledger.task(id).unwrap().unwrap();
    assert_eq!(task.status, "bounced");
    assert_eq!(task.infra_refusals, 1);
    assert_eq!(task.branch_contract_failures, 0);
    assert!(task.dispatch_after.is_some());
}
