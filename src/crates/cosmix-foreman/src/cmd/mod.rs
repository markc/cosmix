mod admin;
mod attachment_harm;
mod dispatch;
mod launch;
mod launch_support;
mod maintenance;
mod mayor;
mod policy;
mod run_task;
mod status;
mod task;
mod verify;

use std::{path::PathBuf, time::Duration};

use anyhow::{Context as _, Result};
use cosmix_foreman::config::FleetPolicy;
use cosmix_foreman::driver;
use cosmix_foreman::executor::{AgentKind, Budget};
use cosmix_foreman::gc;
use cosmix_foreman::governor::Governor;
use cosmix_foreman::ledger::{FindingReason, Ledger, TaskControls, ledger_write_with_busy_retry};
use cosmix_foreman::manifest::ProjectManifest;
use cosmix_foreman::refinery::{self, RefineOptions};
use cosmix_foreman::runner::{RunOptions, run_task_with_policy};
use cosmix_foreman::scratch;
use cosmix_foreman::unit_health;
use cosmix_foreman::wake;

use crate::cli::{Cli, Cmd, ConfigCmd, GovernorCmd};
use crate::task_cli::{TaskBudgetUpdate, TaskCmd};

#[derive(Debug)]
struct ProjectPolicyDenied(String);

impl std::fmt::Display for ProjectPolicyDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "project policy denied launch: {}", self.0)
    }
}

impl std::error::Error for ProjectPolicyDenied {}

struct Context {
    manifest: Option<ProjectManifest>,
    resolved_db: cosmix_foreman::state::ResolvedDbPath,
    db: PathBuf,
    db_create: cosmix_foreman::state::DbCreateMode,
    fleet_policy: FleetPolicy,
}

impl Context {
    fn load(cli: &Cli) -> Result<Self> {
        // Loaded once, up front, exactly like `fleet_policy` below: every
        // command sees one immutable snapshot. Absent `--project`, `manifest`
        // stays `None` and nothing downstream changes from before this flag
        // existed. Project identity fields are resolved as manifest value plus
        // optional matching assertion; other settings keep their documented
        // flag/manifest/default precedence.
        let manifest = match &cli.project {
            Some(path) => Some(
                ProjectManifest::load(path)
                    .with_context(|| format!("loading project manifest {}", path.display()))?,
            ),
            None => None,
        };
        if let (Some(explicit), Some(project)) = (&cli.db, &manifest) {
            anyhow::ensure!(
                explicit == &project.db,
                "--project fixes this invocation to ledger {}; refusing different --db {}",
                project.db.display(),
                explicit.display()
            );
        }
        let db_hint = manifest
            .as_ref()
            .map(|project| project.db.clone())
            .or_else(|| cli.db.clone());
        let resolved_db = cosmix_foreman::state::db_path(db_hint.as_deref(), cli.db_create)?;
        let db = resolved_db.path().to_path_buf();
        let db_create = resolved_db.create_mode();
        // One immutable snapshot per CLI invocation. Long-lived MCP servers
        // resolve again at each tool call.
        let mut fleet_policy = FleetPolicy::load_for_db(&db)?;
        if let Some(project) = &manifest {
            fleet_policy.scope_verify_lane_to_project(&project.root);
        }
        if !fleet_policy.file_found {
            eprintln!(
                "foreman: configuration missing at {}; using environment overrides and compiled defaults",
                fleet_policy.path.display()
            );
        }
        if command_uses_state(&cli.command) {
            resolved_db.require_existing_implicit_parent()?;
        }
        Ok(Self {
            manifest,
            resolved_db,
            db,
            db_create,
            fleet_policy,
        })
    }
}

pub(super) fn run(cli: Cli) -> Result<()> {
    let context = Context::load(&cli)?;
    match cli.command {
        command @ (Cmd::Config(_)
        | Cmd::Init
        | Cmd::Finding { .. }
        | Cmd::Mcp
        | Cmd::Refine { .. }
        | Cmd::Governor(_)) => admin::run(context, command),
        Cmd::Task(command) => task::run(context, command),
        command @ Cmd::Status { .. } => status::run(context, command),
        command @ Cmd::AttachmentHarm { .. } => attachment_harm::run(context, command),
        command @ (Cmd::FleetCheck { .. }
        | Cmd::Wake
        | Cmd::GcScratch { .. }
        | Cmd::GcCache { .. }) => maintenance::run(context, command),
        command @ (Cmd::Verify { .. } | Cmd::PhysicalAcceptance { .. }) => {
            verify::run(context, command)
        }
        command @ Cmd::Mayor { .. } => mayor::run(context, command),
        command @ Cmd::Run { .. } => run_task::run(context, command),
        command @ Cmd::Dispatch { .. } => dispatch::run(context, command),
        command @ Cmd::PolicyCheck { .. } => policy::run(context, command),
    }
}

fn command_uses_state(command: &Cmd) -> bool {
    !matches!(
        command,
        Cmd::Config(_)
            | Cmd::FleetCheck { .. }
            | Cmd::Wake
            | Cmd::GcCache { .. }
            | Cmd::Verify { task: None, .. }
            | Cmd::PhysicalAcceptance { .. }
    )
}

fn print_verify_report(report: &cosmix_foreman::verify::VerifyReport) {
    println!(
        "target directory: {}",
        report.target_dir.as_deref().unwrap_or("unavailable")
    );
    if let Some(tier) = report.provenance_tier {
        println!("executable provenance: tier {tier} (one principal test step)");
    } else {
        println!("executable provenance: not collected in this tier");
    }
    for step in &report.steps {
        println!(
            "{} {} (exit {:?})",
            if step.pass { "PASS" } else { "FAIL" },
            step.command,
            step.exit_code
        );
        for annotation in &step.annotations {
            println!("  INFO {annotation}");
        }
        if let Some(incident) = &step.sccache_incident {
            for line in incident.render().lines() {
                println!("  {line}");
            }
        }
        if let Some(provenance) = &step.executed_binaries {
            for binary in provenance.rendered_lines() {
                println!("  executed binary: {binary}");
            }
        }
    }
    for gap in &report.uncovered {
        println!(
            "{} UNCOVERED {}: {}",
            report.execution.label(),
            gap.area,
            gap.status
        );
    }
    if report.pass {
        println!(
            "{} {}: green ({} steps, {} uncovered)",
            report.execution.label(),
            report.profile,
            report.steps.len(),
            report.uncovered.len()
        );
    } else {
        println!(
            "{} {}: RED\n{}",
            report.execution.label(),
            report.profile,
            report.failure_digest()
        );
    }
}

fn resolve_profile(
    manifest: Option<&ProjectManifest>,
    name: &str,
) -> Result<cosmix_foreman::verify::Profile> {
    match manifest {
        Some(project) => project.profile(name),
        None => cosmix_foreman::verify::lookup_profile(name),
    }
}

fn open_ledger(
    resolved: &cosmix_foreman::state::ResolvedDbPath,
    manifest: Option<&ProjectManifest>,
) -> Result<Ledger> {
    resolved.open_for_project(
        manifest.map(|project| (project.name.as_str(), project.repo_identity.as_str())),
    )
}

/// In project mode a repository-shaped flag is an assertion, not an
/// override. Canonical comparison accepts harmless spelling differences but
/// refuses redirection to another checkout.
fn resolve_project_repo_arg(
    explicit: Option<PathBuf>,
    manifest: Option<&ProjectManifest>,
    flag: &str,
) -> Result<Option<PathBuf>> {
    let Some(project) = manifest else {
        return Ok(explicit);
    };
    if let Some(explicit) = explicit {
        let canonical = explicit
            .canonicalize()
            .with_context(|| format!("{flag} {}", explicit.display()))?;
        anyhow::ensure!(
            canonical == project.repo,
            "--project {:?} fixes {flag} to {}; refusing {}",
            project.name,
            project.repo.display(),
            canonical.display()
        );
    }
    Ok(Some(project.repo.clone()))
}

fn resolve_project_integration_arg(
    explicit: Option<String>,
    manifest: Option<&ProjectManifest>,
) -> Result<String> {
    let Some(project) = manifest else {
        return Ok(explicit.unwrap_or_else(|| "main".to_string()));
    };
    if let Some(explicit) = explicit {
        anyhow::ensure!(
            explicit == project.integration,
            "--project {:?} fixes --integration to {:?}; refusing {:?}",
            project.name,
            project.integration,
            explicit
        );
    }
    Ok(project.integration.clone())
}

/// Everything one governed, optionally policy-gated run needs. `run` and
/// `dispatch` both go through here — the reservation/cap/policy wiring must
/// not diverge between the manual and the automated path.
struct LaunchSpec {
    task: i64,
    agent: AgentKind,
    model: Option<String>,
    workdir: PathBuf,
    budget: Budget,
    stall_secs: u64,
    permission_mode: Option<String>,
    extra_args: Vec<String>,
    branch: Option<String>,
    integration: String,
    verify_subdir: Option<String>,
    no_governor: bool,
    no_verify: bool,
    policy: bool,
    allow_operator_driven: bool,
    worktree_template: String,
    profiles: Vec<cosmix_foreman::verify::Profile>,
    project_pack: String,
}

/// A rung refusal that could not be recorded must not end the sweep for
/// every later task: log it and treat the task as excluded for this pass,
/// the same way the harness-fault arm tolerates the identical error.
fn tolerate_rung_refusal_write(task_id: i64, result: Result<bool>) -> bool {
    match result {
        Ok(_) => true,
        Err(error) => {
            eprintln!(
                "dispatch: could not record task {task_id} rung refusal; \
                 leaving it excluded for this sweep: {error:#}"
            );
            false
        }
    }
}

fn dispatch_park_cause(cause: cosmix_foreman::ladder::ParkCause, failures: i64) -> String {
    match cause {
        cosmix_foreman::ladder::ParkCause::LadderExhausted => {
            format!("after {failures} combined verifier-red/review-rejected ladder charges")
        }
        cosmix_foreman::ladder::ParkCause::RungsRefused => format!(
            "because every remaining rung was refused before claim ({failures} combined verifier-red/review-rejected ladder charges)"
        ),
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn persistent_rung_refusal_write_error_is_tolerated() {
        assert!(!super::tolerate_rung_refusal_write(
            7,
            Err(anyhow::anyhow!("database is locked")),
        ));
        assert!(super::tolerate_rung_refusal_write(7, Ok(false)));
    }

    #[test]
    fn parked_dispatch_text_names_combined_charges_and_rung_refusals() {
        let exhausted =
            super::dispatch_park_cause(cosmix_foreman::ladder::ParkCause::LadderExhausted, 3);
        assert!(exhausted.contains("3 combined verifier-red/review-rejected"));
        assert!(!exhausted.contains("3 review rejections"));

        let refused =
            super::dispatch_park_cause(cosmix_foreman::ladder::ParkCause::RungsRefused, 0);
        assert!(refused.contains("every remaining rung was refused"));
        assert!(refused.contains("0 combined verifier-red/review-rejected"));
    }
}
