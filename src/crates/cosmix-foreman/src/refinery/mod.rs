//! The refinery: a single-lane merge queue. Done tasks with a branch are
//! landed one at a time — the branch is rebased in its now-unclaimed task
//! worktree, re-verified there with the task's own
//! spec-owned profile, and the **exact verified commit** is fast-forwarded
//! into the integration branch. Nothing lands unverified, nothing lands by
//! name (a branch an agent advances after verification cannot smuggle an
//! unverified tip), and a landing that would not move the integration head
//! is refused as laundering, not counted as a landing.
//!
//! Failure split: a problem with the TASK (conflict, red verifier, bogus
//! branch) bounces that task with the concrete detail as a finding. A
//! problem explicitly classified as INFRASTRUCTURE (host filesystem, Git or
//! ledger I/O, verifier engine unable to run) bounces with infrastructure
//! backoff and no quality or branch-contract charge. A third, narrower case:
//! the governor has no headroom
//! for the merge-review reservation this landing would need — neither the
//! task's fault nor damage to the infrastructure, so it is neither bounced
//! nor allowed to stop the queue; the task is restored to 'done' and this
//! run moves on to the next task (see [`GovernorNoHeadroom`]).
//!
//! Trust boundary: the refinery guarantees WHAT lands is verified and moves
//! the tree. The branch comes from the task row; MCP completion may assert
//! that recorded name but cannot replace it.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};
use std::{collections::BTreeMap, fs};

use anyhow::{Context, Result};

use crate::ledger::{
    FindingReason, LandingDisposition, Ledger, PUSH_REPLAY_CLAIM_DETAIL, PushIntent,
    PushIntentOutcome, PushRecoveryReport, Task, ledger_write_with_busy_retry,
};
use crate::verify;
use crate::wake;

/// Colon-separated fleet-home clones that relative workspace path deps
/// resolve through (e.g. the sibling `bus`/`mix` checkouts). Shared with
/// [`crate::sandbox`], which reads the same list to build a contained
/// session's read-only bind set — one name for "what a build needs to see
/// outside the worktree", not two that can drift apart.
pub(crate) const SIBLING_REPOS_ENV: &str = "FOREMAN_SIBLING_REPOS";
const SIBLING_GIT_TIMEOUT_SECS: u64 = 120;
const SIBLING_GIT_KILL_AFTER_SECS: u64 = 5;
/// Cargo may wait on its shared package-cache lock. Bound that host-level
/// contention so one refinery child cannot stall the sole merge lane forever.
const CARGO_CHILD_DEADLINE: Duration = Duration::from_secs(120);
/// Cargo diagnostics and metadata are useful but not trusted to fit in memory.
const CARGO_CHILD_OUTPUT_LIMIT: usize = 4 * 1024 * 1024;
const CARGO_CHILD_POLL_INTERVAL: Duration = Duration::from_millis(25);
const REMOTE_PUSH_DEADLINE: Duration = Duration::from_secs(120);

#[derive(Debug, Clone)]
struct PushDelivery {
    remote: String,
    credentials: Vec<(String, OsString)>,
}

#[derive(Clone)]
pub struct RefineOptions {
    /// The shared repository the task branches live in. The refinery never
    /// checks task branches out here — only the integration branch moves.
    pub repo: PathBuf,
    /// Manifest-derived project root for task worktrees and `clone.lock`.
    /// `None` preserves the legacy repo-parent layout.
    pub project_root: Option<PathBuf>,
    /// The integration branch landings fast-forward.
    pub integration: String,
    /// Where the buildable workspace lives inside the repo (the verifier
    /// runs there — for cos, "src"). "." for manifest-at-root repos.
    pub subdir: String,
    /// Verifier tier for the pre-land re-check (plan §5: tier 1 is the
    /// refinery gate — workspace tests + cargo-deny on top of the fast
    /// gate).
    pub tier: u8,
    /// Merge-authority review: a cross-family Claude or Codex session judges
    /// every landing's diff and can reject it (fail-closed once on). Opt-in EXCEPT for
    /// self-modification: a landing diff touching crates/cosmix-foreman/ is
    /// reviewed regardless of this flag (and incurs governed review spend)
    /// — the policy gate deliberately lets agents edit the foreman crate,
    /// and this review is the code-bound backstop for that freedom.
    pub review: bool,
    /// Ledger path — reviews are governed (kill switch + reservation) and
    /// their spend is accounted like any other session.
    pub db: PathBuf,
    pub echo: bool,
    /// Optional pre-resolved invocation policy. The CLI injects the snapshot
    /// it loaded at invocation start; [`refine`] loads once at entry when an
    /// embedding caller leaves this `None`. Tests can inject a complete
    /// snapshot without mutating the process environment.
    pub fleet_policy: Option<crate::config::FleetPolicy>,
    /// Project-manifest verifier profiles available to task rows.
    pub profiles: Vec<crate::verify::Profile>,
    /// Trusted project instructions included in every review arm.
    pub project_pack: String,
    /// Optional project-manifest landing gate argv.
    pub landing_gate: Option<crate::verify::ProfileStep>,
    /// Project lane eligibility also constrains merge-authority arms. `None`
    /// preserves the unrestricted legacy path.
    pub lane_policy: Option<crate::manifest::ProjectLanePolicy>,
}

#[derive(Debug)]
pub struct LandingReport {
    pub task_id: i64,
    pub branch: String,
    pub profile: String,
    pub landed: bool,
    /// Final ledger status after counters and parking policy are applied.
    pub task_status: &'static str,
    pub detail: String,
    /// Why this bounce happened, shell-decided at the exact `bounce()` call
    /// site that produced it. Meaningless (and unused) when `landed` is true;
    /// approved reviews may still file non-blocking typed findings.
    pub reason: FindingReason,
    /// Structured review findings were already written directly; do not add
    /// a second flattened bounce row derived from the aggregate report.
    pub finding_recorded: bool,
    /// Whether this refinery disposition consumed the implementation
    /// attempt's single quality charge.
    pub ladder_charged: bool,
}

mod cargo;
mod errors;
mod land;
mod manifest_base;
mod manifest_live;
mod preflight;
mod rebase;
mod recovery;
mod reviews;
mod version;
mod version_fs;
mod worktree;

use cargo::*;
use errors::*;
use land::*;
use manifest_base::*;
use manifest_live::*;
use preflight::*;
pub use rebase::{RebaseOutcome, TaskWorktree, bounce_rebase_conflict, resolve_completed_rebase};
use rebase::{git, git_status, rebase_onto};
pub(crate) use recovery::landing_ledger_write;
#[cfg(test)]
use recovery::recorded_review_has_delivered_reject;
pub use recovery::recover_push_intents;
use recovery::{
    finish_landing_and_maybe_wake, latest_implementation_run_for_landing, prune_landed_branch,
    reclaim_landed_scratch, recover_landings,
};
use reviews::*;
use version::*;
use version_fs::*;
use worktree::{
    DirtyTaskWorktree, TempWorktree, recover_interrupted_task_rebase, worktree_dirt_except_targets,
};
pub use worktree::{
    ensure_task_worktree, ensure_task_worktree_named, ensure_task_worktree_named_in,
};

/// Process the whole queue; returns one report per attempted landing.
/// Single-lane is enforced across processes with an exclusive lock. Legacy
/// invocations retain <repo>/../clone.lock; manifest invocations use the
/// manifest-derived project root. The binary itself takes the lock (not just
/// systemd wrappers), so a manual 'foreman refine' cannot race the nightly.
///
/// `acquire_lane` (not a bare acquire) because the lane may already be held
/// on this run's behalf — by the `flock(1)` wrapper in the systemd unit,
/// whose locked descriptor survives the exec into this very process. Taking
/// it again there is a self-deadlock, not a wait; see `clone_lock`'s docs.
pub fn refine(ledger: &Ledger, opts: &RefineOptions) -> Result<Vec<LandingReport>> {
    let mut fleet_policy = match &opts.fleet_policy {
        Some(policy) => policy.clone(),
        None => crate::config::FleetPolicy::load_for_db(&opts.db)?,
    };
    if let Some(root) = &opts.project_root {
        fleet_policy.scope_verify_lane_to_project(root);
    }
    let push_delivery = resolve_push_delivery(opts)?;
    if opts.project_root.is_some() && push_delivery.is_none() && opts.echo {
        println!("push_remote is not configured; remote update is a no-op");
    }
    let _clone_lane = match &opts.project_root {
        Some(root) => crate::clone_lock::acquire_lane_in_project(root)?,
        None => crate::clone_lock::acquire_lane(&opts.repo)?,
    };
    if opts.project_root.is_some() {
        // A process can die immediately after update-ref while the checkout
        // still reflects the old tree. Recover and sync that exact state
        // before ordinary clean-tree preflight. The no-manifest path keeps
        // its established preflight order.
        recover_landings(ledger, opts)?;
        preflight(opts, &fleet_policy)?;
    } else {
        preflight(opts, &fleet_policy)?;
        recover_landings(ledger, opts)?;
    }
    let queue = ledger.landable_tasks()?;
    let mut reports = Vec::new();
    for task in queue {
        // Guarded snapshot re-check: an operator may have requeued (and an
        // agent reclaimed) this task since the queue was read. Marking it
        // in-flight only succeeds while it is still an unclaimed done task.
        if !landing_ledger_write("entering landing", || {
            ledger.transition_if(task.id, "done", "landing")
        })? {
            if opts.echo {
                println!(
                    "task {} skipped (no longer an unclaimed done task)",
                    task.id
                );
            }
            continue;
        }
        let report = land_one(ledger, &task, opts, &fleet_policy, push_delivery.as_ref());
        let mut report = match report {
            Ok(r) => r,
            Err(error) if error.downcast_ref::<GovernorNoHeadroom>().is_some() => {
                // Capacity skips are neutral: restore done, do not file a
                // finding, and try the next task in this queue pass.
                let restored = landing_ledger_write("restoring interrupted landing", || {
                    ledger.transition_if(task.id, "landing", "done")
                })?;
                anyhow::ensure!(
                    restored,
                    "task {} left 'landing' state while restoring after a governor skip",
                    task.id
                );
                if opts.echo {
                    println!("task {}: {error}", task.id);
                }
                continue;
            }
            Err(error) => {
                // Safe default at the landing trust boundary: only errors
                // explicitly marked LandingInfrastructure get infrastructure
                // backoff. Every new/unannotated fallible call is presumed
                // branch-influenced and becomes a task bounce, so agent
                // content cannot silently restore `done` and wedge the whole
                // merge queue on every tick.
                landing_error_report(&task, &error)
            }
        };
        let to = if report.landed { "landed" } else { "bounced" };
        let implementation_run = latest_implementation_run_for_landing(
            ledger,
            task.id,
            "reading landing implementation run",
        )?;
        let refusal_threshold = if matches!(
            report.reason,
            FindingReason::InfraRefusal | FindingReason::PolicyDenied
        ) {
            crate::ledger::infra_refusal_finding_threshold()?
        } else {
            1
        };
        let infra_park_threshold = if report.reason == FindingReason::InfraRefusal {
            crate::ledger::infra_refusal_park_threshold()?
        } else {
            1
        };
        let disposition = finish_landing_and_maybe_wake(
            ledger,
            task.id,
            to,
            implementation_run,
            &report,
            refusal_threshold,
            infra_park_threshold,
            i64::from(fleet_policy.branch_contract_limit.value),
            chrono::Utc::now(),
            || {
                wake::fire(wake::WAKE_VERB);
            },
        )?;
        if !disposition.moved {
            anyhow::bail!(
                "task {} left 'landing' state mid-refine — another writer is live",
                task.id
            );
        }
        report.ladder_charged = disposition.charged;
        let final_status = disposition
            .status
            .context("a moved landing disposition omitted its final status")?;
        report.task_status = final_status.as_db_str();
        if report.landed {
            reclaim_landed_scratch(ledger, opts, task.id);
            prune_landed_branch(opts, &report.branch);
        }
        if opts.echo {
            println!(
                "task {} [{}]: {} (profile: {})",
                report.task_id, report.branch, report.task_status, report.profile
            );
            if !report.landed {
                println!(
                    "[disposition attempt={} ladder_charge={} reason={}]",
                    task.attempt,
                    i64::from(report.ladder_charged),
                    report.reason.as_db_str()
                );
                println!("{}", report.detail.trim());
            }
        }
        reports.push(report);
    }
    Ok(reports)
}

fn resolve_push_delivery(opts: &RefineOptions) -> Result<Option<PushDelivery>> {
    let Some(policy) = opts
        .lane_policy
        .as_ref()
        .filter(|policy| policy.push_remote.is_some())
    else {
        return Ok(None);
    };
    let names = policy
        .push_credential_names(crate::manifest::credential_in_environment)
        .map_err(anyhow::Error::msg)?;
    let credentials = names
        .into_iter()
        .map(|name| {
            let value = std::env::var_os(&name).with_context(|| {
                format!("push credential {name} disappeared after manifest policy validation")
            })?;
            anyhow::ensure!(
                !value.is_empty(),
                "push credential {name} became empty after manifest policy validation"
            );
            Ok((name, value))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(PushDelivery {
        remote: policy
            .push_remote
            .clone()
            .expect("filtered for a configured push remote"),
        credentials,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    include!("tests/errors_and_cargo.rs");
    include!("tests/version.rs");
    include!("tests/landing_and_reviews.rs");
    include!("tests/worktree_and_recovery.rs");
}
