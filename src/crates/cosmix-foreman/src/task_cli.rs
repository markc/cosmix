use std::{path::PathBuf, sync::LazyLock};

use clap::Subcommand;

static TASK_ADD_VERIFIER_HELP: LazyLock<String> = LazyLock::new(|| {
    format!(
        "Tier-0 verifier gating completion. Built-in profiles: {}. Spec-owned — set here at authoring time, never by the completing agent.",
        cosmix_foreman::verify::builtin_profile_names().join(" | ")
    )
});

fn task_add_verifier_help() -> &'static str {
    TASK_ADD_VERIFIER_HELP.as_str()
}

#[derive(Clone)]
pub(super) enum TaskBudgetUpdate {
    Set(f64),
    Clear,
}

impl std::str::FromStr for TaskBudgetUpdate {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        if value.eq_ignore_ascii_case("clear") {
            return Ok(Self::Clear);
        }
        let usd = value
            .parse::<f64>()
            .map_err(|_| "budget must be a finite positive USD amount or 'clear'".to_string())?;
        if !usd.is_finite() || usd <= 0.0 {
            return Err(format!(
                "budget must be a finite positive USD amount or 'clear', got {value}"
            ));
        }
        Ok(Self::Set(usd))
    }
}

#[derive(Subcommand)]
pub(super) enum TaskCmd {
    /// Add a task (spec inline or from a file)
    Add {
        title: String,
        #[arg(long, conflicts_with = "spec_file")]
        spec: Option<String>,
        #[arg(long)]
        spec_file: Option<PathBuf>,
        #[arg(long, default_value = "impl")]
        kind: String,
        /// low | medium | high — routing signal, not enforcement
        #[arg(long, default_value = "low")]
        risk: String,
        /// Operator-owned package-version intent. When omitted, landing keeps
        /// the historical risk/kind derivation.
        #[arg(long, value_parser = ["patch", "minor"])]
        bump: Option<String>,
        /// Task ids this task depends on (repeatable)
        #[arg(long = "dep")]
        deps: Vec<i64>,
        /// Protected crate scope retained for legacy policy and bump-only
        /// maintenance. Ordinary task branches do not bump versions; the
        /// refinery versions packages owning changed files at landing.
        /// This is policy authority; spec prose is not.
        #[arg(long = "crate")]
        crates: Vec<String>,
        /// Tier-0 verifier gating completion (default: "rust", or the
        /// active `--project` manifest's `verifier`).
        #[arg(long, long_help = task_add_verifier_help())]
        verifier: Option<String>,
        /// Reserve this task for explicit `foreman run --task`; unattended
        /// dispatch and MCP claiming skip it. Requires --reason.
        #[arg(long)]
        operator_driven: bool,
        /// Human-readable reason for an operator-driven reservation (filed as
        /// an info finding).
        #[arg(long, requires = "operator_driven")]
        reason: Option<String>,
        /// Total task budget in USD. Each attempt holds its remaining budget;
        /// a narrower explicit run cap wins. Requires a dollar-metering lane.
        #[arg(long)]
        budget: Option<f64>,
    },
    /// List tasks, optionally by status
    List {
        #[arg(long)]
        status: Option<String>,
        /// Include retired tasks in output (default: exclude)
        #[arg(long)]
        all: bool,
        /// Machine-readable output (the Mix/script surface)
        #[arg(long)]
        json: bool,
    },
    /// Show one task in full
    Show { id: i64 },
    /// Change operator-owned scheduling, verifier, budget and bump controls.
    Set {
        id: i64,
        /// Reserve/unreserve the task for explicit operator runs. With no
        /// value this means true; use --operator-driven=false to clear it.
        #[arg(
            long,
            num_args = 0..=1,
            default_missing_value = "true",
            require_equals = true
        )]
        operator_driven: Option<bool>,
        /// Human-readable reason for reserving or releasing the task. Required
        /// whenever --operator-driven is supplied and filed as an info finding.
        #[arg(long)]
        reason: Option<String>,
        /// Change the task's verifier profile.
        /// Refuses changes while the task is running or landing.
        #[arg(long)]
        verifier: Option<String>,
        /// Replace the task's total budget with a finite positive USD amount,
        /// or pass `clear` to remove it. Requeue a parked task separately.
        #[arg(long, value_name = "USD|clear")]
        budget: Option<TaskBudgetUpdate>,
        /// Replace the package-version intent used at landing.
        #[arg(long, value_parser = ["patch", "minor"])]
        bump: Option<String>,
    },
    /// Requeue a parked/failed/bounced/done task (parked resets its ladder
    /// failures and resolves its blocker findings). A running task keeps its claim
    /// unless --force — requeuing a live run invites a second agent into the
    /// same worktree.
    Requeue {
        id: i64,
        #[arg(long)]
        force: bool,
    },
    /// Retry a landing: re-enter the landable state for a task that already
    /// has a branch. Refuses when the task has no branch, is claimed/running/
    /// landing, or the branch doesn't exist in the repo. Ladder failures are
    /// untouched (a landing retry is not an attempt).
    Land {
        id: i64,
        /// The shared repository the task branch lives in (default: ".",
        /// or the active `--project` manifest's `repo`)
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Retire an unclaimed task (operator-only terminal state). Refused for
    /// live claims and landing tasks. Files an info finding with the reason.
    /// Excludes from dispatch and default task list output.
    Retire {
        id: i64,
        /// Human-readable reason for retirement (filed as an info finding)
        #[arg(long)]
        reason: String,
    },
}
