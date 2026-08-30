//! foreman-mcp: the ledger exposed as an MCP server (rmcp, stdio) so agents
//! PULL work instead of being pushed scripts. Register per agent with e.g.
//! `claude mcp add foreman -- foreman --db <path> mcp`.
//!
//! The anti-gaming rules live here, not in agent prompts: `task_complete`
//! runs the task's spec-owned tier-0 verifier itself and refuses on red —
//! the verdict is computed from raw command output, never claimed by the
//! agent. The governor gates `task_claim`.
//!
//! Trust model (single-box, agentic-first): `claimant` is a self-reported
//! identity string — claims serialize work between cooperating agents, they
//! do not authenticate adversaries. Anything with ledger or CLI access is
//! inside the trust boundary; the refinery's verified-tip landing is the
//! integrity gate that holds regardless.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use serde::Deserialize;

use crate::governor::Governor;
use crate::ledger::{ClaimToken, Ledger, LedgerCreate, Task, TaskStatus};
use crate::verify;
use crate::wake;

pub struct ForemanMcp {
    ledger: Mutex<Ledger>,
    /// Ledger path — policy is rebuilt per tool call so conf edits take
    /// effect without restarting every MCP server.
    db: PathBuf,
    profiles: Vec<crate::verify::Profile>,
    verify_subdir: Option<String>,
    /// Manifest eligibility and credential policy. MCP claims use the task's
    /// shell-owned ladder rung; claimant prose never selects an agent lane.
    lane_policy: Option<crate::manifest::ProjectLanePolicy>,
    /// Manifest-owned repository/worktree identity. In project mode an MCP
    /// claim provisions and records this task's named worktree; completion
    /// may verify only that recorded worktree.
    project: Option<McpProjectWorkspace>,
    /// One verifier at a time per server: each is a full cargo build/test
    /// under memguard; unbounded concurrency is an OOM amplifier.
    verify_lane: tokio::sync::Semaphore,
}

/// The manifest fields which own MCP worktree placement and branch identity.
/// Keeping this as operator configuration, rather than accepting either value
/// from a tool call, prevents MCP input from redirecting verifier execution or
/// refinery branch authority.
#[derive(Debug, Clone)]
pub struct McpProjectWorkspace {
    repo: PathBuf,
    root: PathBuf,
    integration: String,
    branch_template: String,
    worktree_template: String,
    instruction_pack: String,
}

impl From<&crate::manifest::ProjectManifest> for McpProjectWorkspace {
    fn from(project: &crate::manifest::ProjectManifest) -> Self {
        Self {
            repo: project.repo.clone(),
            root: project.root.clone(),
            integration: project.integration.clone(),
            branch_template: project.branch_template.clone(),
            worktree_template: project.worktree_template.clone(),
            instruction_pack: project.instruction_pack.clone(),
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TaskNextParams {
    /// Only consider tasks of this kind (e.g. "impl", "review").
    pub kind: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TaskClaimParams {
    pub id: i64,
    /// Stable identity of the claiming agent (e.g. "claude:worker-1").
    pub claimant: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TaskHeartbeatParams {
    pub id: i64,
    pub claimant: String,
    /// Required claim generation (`attempt` from `task_claim`). A delayed
    /// heartbeat from an older same-name worker cannot extend a new claim.
    pub attempt: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TaskCompleteParams {
    pub id: i64,
    pub claimant: String,
    /// Directory the work was done in. In project mode this is an assertion:
    /// it must resolve to the task worktree recorded by `task_claim`.
    pub workdir: String,
    /// Optional assertion of the branch already recorded in the task ledger.
    /// This field cannot set or replace branch authority.
    pub branch: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TaskShowParams {
    pub id: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TaskBounceParams {
    pub id: i64,
    pub claimant: String,
    pub reason: String,
    /// Required claim generation (the `attempt` field of the task row this
    /// agent claimed). A delayed request from an older same-name claimant is
    /// refused instead of dispositioning a newer attempt.
    pub attempt: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindingFileParams {
    pub task_id: Option<i64>,
    /// info | minor | major | blocker
    pub severity: Option<String>,
    pub title: String,
    pub body: Option<String>,
    /// Who found it (defaults to "mcp").
    pub filed_by: Option<String>,
}

fn err(e: impl std::fmt::Display) -> String {
    format!("ERROR: {e}")
}

#[tool_router]
impl ForemanMcp {
    pub fn new(db: PathBuf) -> Result<Self> {
        Self::new_with_profiles(db, Vec::new())
    }

    pub fn new_with_profiles(db: PathBuf, profiles: Vec<crate::verify::Profile>) -> Result<Self> {
        Self::new_with_project_policy(db, profiles, None)
    }

    pub fn new_with_project_policy(
        db: PathBuf,
        profiles: Vec<crate::verify::Profile>,
        lane_policy: Option<crate::manifest::ProjectLanePolicy>,
    ) -> Result<Self> {
        let ledger = Ledger::open(&db)?;
        Ok(ForemanMcp {
            ledger: Mutex::new(ledger),
            db,
            profiles,
            verify_subdir: None,
            lane_policy,
            project: None,
            verify_lane: tokio::sync::Semaphore::new(1),
        })
    }

    /// Construct a direct-call MCP surface with all project-manifest policy.
    /// The CLI server uses [`serve_with_ledger`], which receives an already
    /// identity-bound ledger and the same resolved fields.
    pub fn new_with_project_manifest(
        db: PathBuf,
        project: &crate::manifest::ProjectManifest,
    ) -> Result<Self> {
        anyhow::ensure!(
            db == project.db,
            "MCP ledger {} does not match project ledger {}",
            db.display(),
            project.db.display()
        );
        let ledger = Ledger::open_with_create_for_project(
            &db,
            LedgerCreate::ParentsAndFile,
            Some((&project.name, &project.repo_identity)),
        )?;
        Ok(ForemanMcp {
            ledger: Mutex::new(ledger),
            db,
            profiles: project.profiles.clone(),
            verify_subdir: project.subdir.clone(),
            lane_policy: project.lane_policy(),
            project: Some(project.into()),
            verify_lane: tokio::sync::Semaphore::new(1),
        })
    }

    fn check_lane(&self, agent: crate::executor::AgentKind) -> std::result::Result<(), String> {
        let Some(policy) = &self.lane_policy else {
            return Ok(());
        };
        policy.check_lane(agent, crate::manifest::credential_in_environment)
    }

    /// Poison-recovering lock: one panicked tool call must not brick every
    /// subsequent call on this server (SQLite transactions keep the data
    /// itself consistent).
    fn ledger(&self) -> std::sync::MutexGuard<'_, Ledger> {
        self.ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Next ROUTABLE task (queued/bounced/failed, all deps done), oldest first,
    /// with the escalation ladder's advisory route (`route`: which
    /// agent/model tier this task has earned; honor it when you can).
    /// Unroutable tasks encountered along the way (quality ladder exhausted
    /// or every remaining rung refused) are parked for a human with a
    /// cause-specific blocker finding — this call can mutate task state.
    /// Call task_claim to take the returned task.
    #[tool]
    pub async fn task_next(&self, Parameters(p): Parameters<TaskNextParams>) -> String {
        let policy = match crate::config::FleetPolicy::load_for_db(&self.db) {
            Ok(mut policy) => {
                if let Some(project) = &self.project {
                    policy.scope_verify_lane_to_project(&project.root);
                }
                policy
            }
            Err(e) => return err(format!("{e:#}")),
        };
        let ledger = self.ledger();
        let ready = match ledger.ready_tasks(p.kind.as_deref()) {
            Ok(t) => t,
            Err(e) => return err(format!("{e:#}")),
        };
        let ladder = policy.ladder();
        // Exhausted tasks are PARKED in passing, exactly as the dispatcher
        // does — otherwise a pure-MCP fleet head-of-line-blocks forever on
        // the oldest exhausted task.
        for t in ready {
            match crate::ladder::rung_for_task(&ledger, &ladder, &t) {
                Err(error) => return err(format!("routing task {}: {error:#}", t.id)),
                Ok(Some(rung)) => {
                    if let Err(reason) = self.check_lane(rung.agent) {
                        match ledger.task_has_open_finding_reason(
                            t.id,
                            crate::ledger::FindingReason::PolicyDenied,
                        ) {
                            Ok(true) => {}
                            Ok(false) => {
                                if let Err(error) = ledger.file_finding_reasoned(
                                    Some(t.id),
                                    "blocker",
                                    "project policy denied the routed agent",
                                    &reason,
                                    "mcp",
                                    crate::ledger::FindingReason::PolicyDenied,
                                ) {
                                    return err(format!(
                                        "filing project-policy blocker for task {}: {error:#}",
                                        t.id
                                    ));
                                }
                            }
                            Err(error) => {
                                return err(format!(
                                    "checking project-policy blocker for task {}: {error:#}",
                                    t.id
                                ));
                            }
                        }
                        continue;
                    }
                    let mut out = serde_json::to_value(&t).unwrap_or_default();
                    out["route"] = serde_json::json!({
                        "agent": rung.agent.as_str(),
                        "model": rung.model,
                        "failures": t.ladder_failures,
                    });
                    return serde_json::to_string_pretty(&out).unwrap_or_else(err);
                }
                Ok(None) => {
                    // A park that ERRORS must fail the call loudly (a lost
                    // race — Ok(false) — just means someone else moved it).
                    let cause = crate::ladder::park_cause(&ladder, &t);
                    let parked = match cause {
                        crate::ladder::ParkCause::LadderExhausted => {
                            ledger.park_task(t.id, t.ladder_failures, &t.risk)
                        }
                        crate::ladder::ParkCause::RungsRefused => {
                            ledger.park_task_rungs_refused(t.id, t.ladder_failures, &t.risk)
                        }
                    };
                    if let Err(e) = parked {
                        return err(format!("parking unroutable task {}: {e:#}", t.id));
                    }
                }
            }
        }
        "no claimable task".into()
    }

    /// Claim a task for exclusive work. Refused when the governor's kill
    /// switch or a daily ceiling is in effect, when deps are unmet, or when
    /// someone else holds the claim.
    #[tool]
    pub async fn task_claim(&self, Parameters(p): Parameters<TaskClaimParams>) -> String {
        let policy = match crate::config::FleetPolicy::load_for_db(&self.db) {
            Ok(policy) => policy,
            Err(e) => return err(format!("{e:#}")),
        };
        let ledger = self.ledger();
        if let Err(e) = Governor::from_policy(&self.db, &policy).admit(&ledger) {
            return err(format!("{e:#}"));
        }
        // The ladder gate holds on this surface too: an exhausted task is
        // parked at claim time, not retried unboundedly at an agent-chosen
        // tier. A mistyped ladder policy fails the claim rather than
        // skipping the gate.
        let ladder = policy.ladder();
        if let Ok(Some(t)) = ledger.task(p.id)
            && t.status.parse::<TaskStatus>().is_ok_and(|s| s.is_dispatchable())
            // Operator reservation wins over ladder disposition. A direct
            // MCP claim must reach the ledger's explicit refusal without
            // parking or otherwise mutating the reserved task first.
            && !t.operator_driven
        {
            let rung = match crate::ladder::rung_for_task(&ledger, &ladder, &t) {
                Ok(rung) => rung,
                Err(error) => return err(format!("routing task {}: {error:#}", t.id)),
            };
            let Some(rung) = rung else {
                let cause = crate::ladder::park_cause(&ladder, &t);
                let parked = match cause {
                    crate::ladder::ParkCause::LadderExhausted => {
                        ledger.park_task(t.id, t.ladder_failures, &t.risk)
                    }
                    crate::ladder::ParkCause::RungsRefused => {
                        ledger.park_task_rungs_refused(t.id, t.ladder_failures, &t.risk)
                    }
                };
                let cause = match cause {
                    crate::ladder::ParkCause::LadderExhausted => "quality ladder exhausted",
                    crate::ladder::ParkCause::RungsRefused => "all remaining rungs refused",
                };
                return match parked {
                    Ok(true) => err(format!(
                        "task {} is unroutable ({cause}; {} combined verifier-red/review-rejected charges) and has been parked for a human — pick another task",
                        t.id, t.ladder_failures,
                    )),
                    Ok(false) => err(format!(
                        "task {} looked unroutable but changed state \
                         concurrently — call task_next again",
                        t.id
                    )),
                    Err(e) => err(format!("parking unroutable task {}: {e:#}", t.id)),
                };
            };
            if let Err(reason) = self.check_lane(rung.agent) {
                match ledger
                    .task_has_open_finding_reason(t.id, crate::ledger::FindingReason::PolicyDenied)
                {
                    Ok(true) => {}
                    Ok(false) => {
                        if let Err(error) = ledger.file_finding_reasoned(
                            Some(t.id),
                            "blocker",
                            "project policy denied the routed agent",
                            &reason,
                            "mcp",
                            crate::ledger::FindingReason::PolicyDenied,
                        ) {
                            return err(format!(
                                "filing project-policy blocker for task {}: {error:#}",
                                t.id
                            ));
                        }
                    }
                    Err(error) => {
                        return err(format!(
                            "checking project-policy blocker for task {}: {error:#}",
                            t.id
                        ));
                    }
                }
                return err(format!(
                    "task {} route {} is refused by project policy: {reason}; ladder unchanged",
                    t.id,
                    rung.agent.as_str()
                ));
            }
        }
        match ledger.claim_task(p.id, &p.claimant) {
            Ok(task) => {
                let task = match &self.project {
                    Some(project) => {
                        match record_project_workspace(&ledger, task, &p.claimant, project) {
                            Ok(task) => task,
                            Err(error) => {
                                return err(format!(
                                    "preparing project worktree for task {}: {error:#}",
                                    p.id
                                ));
                            }
                        }
                    }
                    None => task,
                };
                serde_json::to_string_pretty(&task).unwrap_or_else(err)
            }
            Err(e) => err(format!("{e:#}")),
        }
    }

    /// Renew a claimed task's lease. Call this periodically while doing work
    /// and before a long quiet operation. It works for remote claimants with
    /// no controller-local pid; the returned RFC3339 value is the new expiry.
    #[tool]
    pub async fn task_heartbeat(&self, Parameters(p): Parameters<TaskHeartbeatParams>) -> String {
        match self.ledger().renew_claim(
            p.id,
            ClaimToken {
                owner: &p.claimant,
                generation: p.attempt,
            },
        ) {
            Ok(lease_until) => format!("task {} lease renewed until {lease_until}", p.id),
            Err(error) => err(format!("{error:#}")),
        }
    }

    /// Mark a claimed task complete. Runs the task's spec-owned tier-0
    /// verifier in the ledger-recorded task worktree first; a red verifier refuses the completion
    /// (response starts with "ERROR: tier-0") and the task stays claimed —
    /// fix the failures and call again. Note the completion-time verifier
    /// only proves that recorded worktree is green; the refinery's re-verify of the
    /// rebased branch tip is the gate that decides what actually lands.
    #[tool]
    pub async fn task_complete(&self, Parameters(p): Parameters<TaskCompleteParams>) -> String {
        let policy = match crate::config::FleetPolicy::load_for_db(&self.db) {
            Ok(mut policy) => {
                if let Some(project) = &self.project {
                    policy.scope_verify_lane_to_project(&project.root);
                }
                policy
            }
            Err(e) => return err(format!("{e:#}")),
        };
        let supplied_workdir = PathBuf::from(&p.workdir);
        if !supplied_workdir.is_absolute() || !supplied_workdir.is_dir() {
            return err(format!(
                "workdir {:?} must be an absolute path to an existing directory",
                p.workdir
            ));
        }
        // Snapshot under the lock; the verifier then runs for minutes with
        // the lock RELEASED, so the snapshot (claimant AND attempt) is
        // re-checked before any write below.
        let (profile, crates, attempt, recorded_worktree, recorded_branch) = {
            let ledger = self.ledger();
            match ledger.task(p.id) {
                Ok(Some(t)) => {
                    if t.claimed_by.as_deref() != Some(p.claimant.as_str()) {
                        return err(format!(
                            "task {} is not claimed by {} — claim it first",
                            p.id, p.claimant
                        ));
                    }
                    if let Some(asserted) = p.branch.as_deref()
                        && t.branch.as_deref() != Some(asserted)
                    {
                        return err(format!(
                            "task {} branch assertion {:?} does not match recorded branch {:?}; completion cannot rename a task branch",
                            p.id, asserted, t.branch
                        ));
                    }
                    if let Err(error) = ledger.renew_claim(
                        p.id,
                        ClaimToken {
                            owner: &p.claimant,
                            generation: t.attempt,
                        },
                    ) {
                        return err(format!(
                            "renewing task {} before verification: {error:#}",
                            p.id
                        ));
                    }
                    (
                        t.verifier_profile,
                        t.crates,
                        t.attempt,
                        t.worktree,
                        t.branch,
                    )
                }
                Ok(None) => return err(format!("no task {}", p.id)),
                Err(e) => return err(format!("{e:#}")),
            }
        };
        let workdir = match &self.project {
            Some(project) => {
                let workdir = match supplied_workdir.canonicalize() {
                    Ok(workdir) => workdir,
                    Err(error) => {
                        return err(format!("canonicalizing workdir {:?}: {error}", p.workdir));
                    }
                };
                let Some(recorded_worktree) = recorded_worktree.as_deref() else {
                    return err(format!(
                        "task {} has no recorded project worktree — claim it through this project MCP server first",
                        p.id
                    ));
                };
                let Some(recorded_branch) = recorded_branch.as_deref() else {
                    return err(format!(
                        "task {} has no recorded project branch — claim it through this project MCP server first",
                        p.id
                    ));
                };
                if let Err(error) = validate_project_completion_workspace(
                    project,
                    p.id,
                    recorded_worktree,
                    recorded_branch,
                    &workdir,
                ) {
                    return err(format!("refusing task {} completion: {error:#}", p.id));
                }
                workdir
            }
            None => supplied_workdir,
        };
        let _lane = self.verify_lane.acquire().await.expect("semaphore open");
        let profile = match self
            .profiles
            .iter()
            .find(|candidate| candidate.name == profile)
            .cloned()
            .map(Ok)
            .unwrap_or_else(|| verify::lookup_profile(&profile))
        {
            Ok(profile) => profile,
            Err(error) => return err(format!("verifier profile could not resolve: {error:#}")),
        };
        let report = match tokio::task::spawn_blocking({
            let workdir = workdir.clone();
            let verify_subdir = self.verify_subdir.clone();
            move || {
                let request = verify::GateRequest::local(
                    p.id,
                    attempt,
                    verify::GateIdentity::McpCompletion,
                    0,
                    &workdir,
                    &profile,
                    verify_subdir.as_deref(),
                    &crates,
                    &policy,
                )?;
                verify::GateRunner::run_gate(&verify::LOCAL_GATE_RUNNER, &request)
            }
        })
        .await
        {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return err(format!("verifier could not run: {e:#}")),
            Err(join) => return err(format!("verifier task panicked: {join}")),
        };
        // One IMMEDIATE transaction re-checks claimant + attempt and applies
        // everything — a forced requeue + reclaim in ANY process during the
        // verifier run discards this result rather than polluting the new
        // attempt.
        let sccache_bypasses = report.sccache_bypass_digests();
        let completed = self.ledger().complete_verified(
            p.id,
            ClaimToken {
                owner: &p.claimant,
                generation: attempt,
            },
            &workdir.to_string_lossy(),
            &serde_json::to_string(&report).unwrap_or_default(),
            report.pass,
            &sccache_bypasses,
        );
        match completed {
            Ok(true) => {
                // Best-effort ABP wake — see wake.rs. `done` can unblock
                // dependents before the supervisor's backstop timer fires.
                wake::fire(wake::WAKE_VERB);
                let refinery_note = if recorded_branch.is_none() {
                    " (no branch recorded: this task will not pass through the refinery)"
                } else {
                    ""
                };
                format!("task {} done (tier-0 green){refinery_note}", p.id)
            }
            Ok(false) => format!(
                "ERROR: tier-0 verifier ({}) is red — task stays claimed; fix and retry:\n{}",
                report.profile,
                report.failure_digest()
            ),
            Err(e) => err(format!("{e:#}")),
        }
    }

    /// Give a claimed task back (bounced, claimable again) with the reason
    /// recorded as a finding.
    #[tool]
    pub async fn task_bounce(&self, Parameters(p): Parameters<TaskBounceParams>) -> String {
        let policy = match crate::config::FleetPolicy::load_for_db(&self.db) {
            Ok(policy) => policy,
            Err(error) => return err(format!("{error:#}")),
        };
        let ledger = self.ledger();
        match ledger.finish_agent_bounce(
            p.id,
            &p.claimant,
            p.attempt,
            &p.reason,
            i64::from(policy.branch_contract_limit.value),
        ) {
            Ok(parked) => {
                // Best-effort ABP wake — see wake.rs. A bounced task is
                // dispatchable again immediately, not just at backstop time.
                wake::fire(wake::WAKE_VERB);
                if parked {
                    format!("task {} parked after repeated self-bounces", p.id)
                } else {
                    format!("task {} bounced", p.id)
                }
            }
            Err(e) => err(format!("{e:#}")),
        }
    }

    /// One task in full: its row, recent verification reports (tier 3 =
    /// merge-authority review, with the review text), and open findings —
    /// the "why did task 41 bounce?" tool.
    #[tool]
    pub async fn task_show(&self, Parameters(p): Parameters<TaskShowParams>) -> String {
        const REPORT_CAP: usize = 4 * 1024;
        let ledger = self.ledger();
        let task = match ledger.task(p.id) {
            Ok(Some(t)) => t,
            Ok(None) => return err(format!("no task {}", p.id)),
            Err(e) => return err(format!("{e:#}")),
        };
        let mut out = serde_json::to_value(&task).unwrap_or_default();
        match ledger.verification_reports(p.id, 5) {
            Ok(reports) => {
                out["verifications"] = reports
                    .into_iter()
                    .map(|mut r| {
                        if r.len() > REPORT_CAP {
                            let cut = r
                                .char_indices()
                                .map(|(i, _)| i)
                                .take_while(|&i| i <= REPORT_CAP)
                                .last()
                                .unwrap_or(0);
                            r.truncate(cut);
                            r.push('…');
                        }
                        serde_json::Value::String(r)
                    })
                    .collect();
            }
            Err(e) => out["verifications_error"] = serde_json::json!(format!("{e:#}")),
        }
        match ledger.task_findings_detailed(p.id) {
            Ok(findings) => {
                out["open_findings"] = findings
                    .into_iter()
                    .map(|finding| {
                        serde_json::json!({
                            "id": finding.id,
                            "severity": finding.severity,
                            "file": finding.file,
                            "line": finding.line,
                            "title": finding.title,
                            "body": finding.body,
                            "run_id": finding.run_id,
                        })
                    })
                    .collect();
            }
            Err(e) => out["findings_error"] = serde_json::json!(format!("{e:#}")),
        }
        serde_json::to_string_pretty(&out).unwrap_or_else(err)
    }

    /// File discovered work (a bug, a TODO, a design question) instead of
    /// drive-by fixing it — the context-preservation rule.
    #[tool]
    pub async fn finding_file(&self, Parameters(p): Parameters<FindingFileParams>) -> String {
        let ledger = self.ledger();
        match ledger.file_finding_reasoned(
            p.task_id,
            p.severity.as_deref().unwrap_or("info"),
            &p.title,
            p.body.as_deref().unwrap_or(""),
            p.filed_by.as_deref().unwrap_or("mcp"),
            crate::ledger::FindingReason::AgentReported,
        ) {
            Ok(id) => format!("finding {id} filed"),
            Err(e) => err(format!("{e:#}")),
        }
    }

    /// Fleet status: task counts, recent runs, spend, governor state.
    #[tool]
    pub async fn build_status(&self) -> String {
        let policy = match crate::config::FleetPolicy::load_for_db(&self.db) {
            Ok(policy) => policy,
            Err(e) => return err(format!("{e:#}")),
        };
        let ledger = self.ledger();
        let counts = match ledger.status_counts() {
            Ok(c) => c,
            Err(e) => return err(e),
        };
        // Partial failures are reported, not hidden — an empty run list and
        // a broken run query must not look alike.
        let mut out = serde_json::json!({
            "tasks": counts.into_iter().collect::<std::collections::BTreeMap<_, _>>(),
        });
        match ledger.recent_runs(10) {
            Ok(runs) => out["recent_runs"] = serde_json::json!(runs),
            Err(e) => out["recent_runs_error"] = serde_json::json!(format!("{e:#}")),
        }
        match (
            ledger.total_spend_usd(),
            ledger.delivery_void_fraction(),
            ledger.quality_void_fraction(),
        ) {
            (Ok(spend), Ok(delivery), Ok(quality)) => {
                out["total_spend_usd"] = serde_json::json!(spend);
                out["total_spend_usd_delivery_void"] = serde_json::json!(delivery);
                out["runs"] = serde_json::json!({
                    "total": delivery.contributing_runs,
                    "unknown_delivery": delivery.unknown_runs,
                    "delivery_void_fraction": delivery.fraction,
                    "unknown_quality": quality.unknown_runs,
                    "quality_void_fraction": quality.fraction,
                });
            }
            (spend, delivery, quality) => {
                out["run_aggregates_error"] = serde_json::json!(format!(
                    "spend={:?}; delivery_void={:?}; quality_void={:?}",
                    spend.err(),
                    delivery.err(),
                    quality.err()
                ));
            }
        }
        match ledger.recent_verifications(10) {
            Ok(rows) => {
                out["recent_verifications"] = rows
                    .into_iter()
                    .map(|(task, tier, pass, at)| {
                        serde_json::json!({
                            "task": task,
                            // Tier 3 is the merge-authority review verdict.
                            "tier": tier,
                            "pass": pass,
                            "at": at,
                        })
                    })
                    .collect();
            }
            Err(e) => out["recent_verifications_error"] = serde_json::json!(format!("{e:#}")),
        }
        match Governor::from_policy(&self.db, &policy).status(&ledger) {
            Ok(g) => {
                out["governor"] = serde_json::json!({
                    "stopped": g.stopped,
                    "spend_today_usd": g.spend_today_usd,
                    "output_tokens_today": g.output_tokens_today,
                    "delivery_void": g.delivery_void_fraction,
                    "reserved_usd": g.reserved_usd,
                    "reserved_tokens": g.reserved_tokens,
                    "daily_budget_usd": g.daily_budget_usd,
                    "daily_output_tokens": g.daily_output_tokens,
                });
            }
            Err(e) => out["governor_error"] = serde_json::json!(format!("{e:#}")),
        }
        // Same contract as `foreman status --json`: advisory, and gated on
        // `has_report` so an active sweep is visible to the mayor even when
        // it is perfectly healthy.
        let health =
            crate::unit_health::check_fleet_health(crate::unit_health::current_binary_mtime());
        if health.has_report() {
            out["unit_health"] = serde_json::json!(health);
        }
        serde_json::to_string_pretty(&out).unwrap_or_else(err)
    }
}

fn record_project_workspace(
    ledger: &Ledger,
    task: Task,
    claimant: &str,
    project: &McpProjectWorkspace,
) -> Result<Task> {
    let branch = project
        .branch_template
        .replace("{id}", &task.id.to_string());
    let _clone_lane = crate::clone_lock::acquire_lane_in_project(&project.root)?;
    let worktree = crate::refinery::ensure_task_worktree_named_in(
        &project.repo,
        task.id,
        &branch,
        Some(&project.integration),
        &project.worktree_template,
        Some(&project.root),
    )?;
    let worktree = worktree
        .path
        .canonicalize()
        .with_context(|| format!("canonicalizing task worktree {}", worktree.path.display()))?;
    let worktree = worktree
        .to_str()
        .context("project task worktree path is not UTF-8")?;
    ledger.set_task_workspace(
        task.id,
        ClaimToken {
            owner: claimant,
            generation: task.attempt,
        },
        Some(worktree),
        Some(&branch),
    )?;
    ledger.task(task.id)?.with_context(|| {
        format!(
            "task {} vanished after recording its project workspace",
            task.id
        )
    })
}

fn validate_project_completion_workspace(
    project: &McpProjectWorkspace,
    task_id: i64,
    recorded_worktree: &str,
    recorded_branch: &str,
    supplied_worktree: &Path,
) -> Result<()> {
    let recorded_path = Path::new(recorded_worktree);
    let recorded_canonical = recorded_path
        .canonicalize()
        .with_context(|| format!("canonicalizing recorded worktree {recorded_worktree}"))?;
    anyhow::ensure!(
        recorded_canonical == recorded_path,
        "recorded worktree {} no longer resolves to its originally recorded canonical path",
        recorded_path.display()
    );
    anyhow::ensure!(
        supplied_worktree == recorded_canonical,
        "caller workdir {} does not match recorded task worktree {}",
        supplied_worktree.display(),
        recorded_canonical.display()
    );

    let expected_path = project.root.join(
        project
            .worktree_template
            .replace("{id}", &task_id.to_string()),
    );
    anyhow::ensure!(
        recorded_canonical == expected_path,
        "recorded worktree {} is not the manifest-owned path {}",
        recorded_canonical.display(),
        expected_path.display()
    );
    let expected_branch = project
        .branch_template
        .replace("{id}", &task_id.to_string());
    anyhow::ensure!(
        recorded_branch == expected_branch,
        "recorded branch {recorded_branch:?} is not the manifest-owned branch {expected_branch:?}"
    );
    let actual_branch = git_output(&recorded_canonical, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    anyhow::ensure!(
        actual_branch == expected_branch,
        "recorded worktree is on branch {actual_branch:?}, expected {expected_branch:?}"
    );
    let project_common = git_common_dir(&project.repo)?;
    let worktree_common = git_common_dir(&recorded_canonical)?;
    anyhow::ensure!(
        worktree_common == project_common,
        "recorded worktree belongs to git common dir {}, not project git common dir {}",
        worktree_common.display(),
        project_common.display()
    );
    Ok(())
}

fn git_common_dir(dir: &Path) -> Result<PathBuf> {
    let common = git_output(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    PathBuf::from(&common)
        .canonicalize()
        .with_context(|| format!("canonicalizing git common dir {common:?}"))
}

fn git_output(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("running git {args:?} in {}", dir.display()))?;
    anyhow::ensure!(
        output.status.success(),
        "git {args:?} failed in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8(output.stdout)
        .context("git output is not UTF-8")?
        .trim()
        .to_string())
}

#[tool_handler]
impl ServerHandler for ForemanMcp {
    fn get_info(&self) -> rmcp::model::ServerInfo {
        let mut instructions =
            "foreman task ledger. Pull work with task_next -> task_claim; do the \
             work in the task's worktree and call task_heartbeat periodically \
             during long work; task_complete runs the task's tier-0 \
             verifier and refuses on red (fix and retry). File discovered work \
             with finding_file instead of drive-by fixing. task_bounce returns a \
             task you cannot finish. build_status shows fleet + governor state."
                .to_string();
        if let Some(project) = &self.project {
            instructions.push_str("\n\n# Project context (trusted operator configuration)\n\n");
            instructions.push_str(&project.instruction_pack);
        }
        rmcp::model::ServerInfo::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
        .with_instructions(instructions)
    }
}

/// Serve the ledger over stdio until the client disconnects.
pub fn serve(db: PathBuf) -> Result<()> {
    let ledger = Ledger::open(&db)?;
    serve_with_ledger(db, ledger, Vec::new(), None, None, None)
}

/// Serve a ledger whose path/create policy was already resolved by the CLI.
pub fn serve_with_ledger(
    db: PathBuf,
    ledger: Ledger,
    profiles: Vec<crate::verify::Profile>,
    verify_subdir: Option<String>,
    lane_policy: Option<crate::manifest::ProjectLanePolicy>,
    project: Option<McpProjectWorkspace>,
) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let server = ForemanMcp {
            ledger: Mutex::new(ledger),
            db,
            profiles,
            verify_subdir,
            lane_policy,
            project,
            verify_lane: tokio::sync::Semaphore::new(1),
        };
        let service = server
            .serve(rmcp::transport::io::stdio())
            .await
            .map_err(|e| anyhow::anyhow!("mcp serve: {e}"))?;
        service
            .waiting()
            .await
            .map_err(|e| anyhow::anyhow!("mcp wait: {e}"))?;
        Ok(())
    })
}
