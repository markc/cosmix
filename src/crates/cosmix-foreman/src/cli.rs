use std::path::PathBuf;

use clap::{Parser, Subcommand};
use cosmix_foreman::executor::AgentKind;

use super::task_cli::TaskCmd;

#[derive(Parser)]
#[command(
    name = "foreman",
    version,
    about = "Cosmix build-orchestration harness"
)]
pub(super) struct Cli {
    /// Ledger database path (default: $FOREMAN_DB, $STATE_DIRECTORY/ledger.db,
    /// an existing legacy ./.foreman/ledger.db, or XDG/FHS Cosmix state)
    #[arg(long, global = true)]
    pub(super) db: Option<PathBuf>,
    /// Internal child-process hand-off for the resolved ledger creation mode.
    #[arg(long, global = true, hide = true, requires = "db")]
    pub(super) db_create: Option<cosmix_foreman::state::DbCreateMode>,
    /// A project manifest naming the repo, integration branch, verifier
    /// profile, and instruction pack this invocation targets. Repository and
    /// integration flags may only repeat the manifest values; neither they
    /// nor `--db` may redirect a project invocation. Omitted
    /// entirely, nothing here changes: an operator unit that never passes
    /// `--project` runs the exact code path it ran before this flag
    /// existed.
    #[arg(long, global = true)]
    pub(super) project: Option<PathBuf>,
    #[command(subcommand)]
    pub(super) command: Cmd,
}

#[derive(Subcommand)]
pub(super) enum Cmd {
    /// Inspect the effective fleet policy and where each value came from
    #[command(subcommand)]
    Config(ConfigCmd),
    /// Create the ledger database
    Init,
    /// Author, inspect, budget, requeue, land and retire tasks
    #[command(subcommand)]
    Task(TaskCmd),
    /// Run one task through one agent
    Run {
        #[arg(long)]
        task: i64,
        /// claude | codex | glm
        #[arg(long)]
        agent: AgentKind,
        #[arg(long)]
        model: Option<String>,
        /// Working directory for the agent (default: ".", or the active
        /// project's repo)
        #[arg(long)]
        workdir: Option<PathBuf>,
        #[arg(long)]
        max_turns: Option<u32>,
        /// Explicit per-run dollar cap. On a budgeted task the smaller of
        /// this cap and the task's remaining budget is reserved.
        #[arg(long)]
        max_budget_usd: Option<f64>,
        #[arg(long)]
        max_output_tokens: Option<u64>,
        #[arg(long)]
        max_wall_secs: Option<u64>,
        /// Seconds of event silence before the session is declared stalled
        #[arg(long, default_value_t = 600)]
        stall_secs: u64,
        /// Claude/GLM only: --permission-mode passed through to the CLI.
        /// Defaults to bypassPermissions — the unattended path is the
        /// default; pass a stricter mode to opt a run into guard rails.
        #[arg(long)]
        permission_mode: Option<String>,
        /// Extra args appended to the vendor CLI invocation
        #[arg(long = "extra-arg")]
        extra_args: Vec<String>,
        /// Branch the work lands on (recorded for the refinery queue)
        #[arg(long)]
        branch: Option<String>,
        /// Integration branch used both to rebase a reused task branch and
        /// to scope crates already changed by this task. Resolved to an
        /// immutable commit before either operation.
        #[arg(long)]
        integration: Option<String>,
        /// Buildable workspace subdirectory the tier-0 verifier runs in,
        /// relative to the workdir (for cos: src — the repo root has no
        /// Cargo.toml, so verifying there is red regardless of the work)
        #[arg(long)]
        subdir: Option<String>,
        /// Skip the governor's daily-ceiling/kill-switch gate for this run
        #[arg(long)]
        no_governor: bool,
        /// Skip the tier-0 verifier gate on done (operator override)
        #[arg(long)]
        no_verify: bool,
        /// Contain this run. Claude/GLM: a PreToolUse hook checks every tool
        /// call (worktree containment, gate-path protection, push
        /// discipline, zai secret rules). Codex: its own
        /// --sandbox workspace-write confines WRITES to the worktree, with
        /// no per-call verdicts and no restriction on reads.
        #[arg(long)]
        policy: bool,
    },
    /// Route ready tasks through the escalation ladder and run them: every
    /// task enters at the policy's `start_rung`, charged failures climb it, an
    /// exhausted ladder parks the task for a human. One rung per task per
    /// invocation; rerun (or raise --max-tasks) to keep draining.
    Dispatch {
        /// Working directory for the dispatched agents (default: ".", or
        /// the active `--project` manifest's `repo`)
        #[arg(long)]
        workdir: Option<PathBuf>,
        /// Dispatch this specific task instead of the next ready one
        #[arg(long)]
        task: Option<i64>,
        /// Only consider tasks of this kind
        #[arg(long)]
        kind: Option<String>,
        /// How many DISTINCT tasks to dispatch sequentially before exiting
        /// (each gets one rung per invocation; ignored with --task)
        #[arg(long, default_value_t = 1)]
        max_tasks: u32,
        /// Wall-clock cap per dispatched run, seconds
        #[arg(long)]
        max_wall_secs: Option<u64>,
        /// Record a branch per task, "{id}" substituted (e.g. "task/{id}";
        /// default: none, or the active `--project` manifest's
        /// `branch_template`)
        #[arg(long)]
        branch_template: Option<String>,
        /// Integration branch used to scope changed crates and to rebase a
        /// reused task branch before the attempt runs (default: "main", or
        /// the active `--project` manifest's `integration`).
        #[arg(long)]
        integration: Option<String>,
        /// Buildable workspace subdirectory the tier-0 verifier runs in,
        /// relative to each task's worktree (for cos: src)
        #[arg(long)]
        subdir: Option<String>,
        /// Enable the policy gate on dispatched claude/glm runs
        #[arg(long)]
        policy: bool,
        /// Skip the tier-0 verifier gate on done (operator override)
        #[arg(long)]
        no_verify: bool,
        /// Print the routing decision without running anything
        #[arg(long)]
        dry_run: bool,
        /// Seconds of event silence before a session is declared stalled
        #[arg(long, default_value_t = 600)]
        stall_secs: u64,
    },
    /// PreToolUse hook entrypoint (invoked by agent sessions, not by hand):
    /// reads the tool-call JSON on stdin, exits 0 to allow / 2 to deny
    #[command(hide = true)]
    PolicyCheck {
        #[arg(long)]
        task: i64,
        #[arg(long)]
        worktree: PathBuf,
        #[arg(long, default_value = "anthropic")]
        provider: String,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        integration_base: String,
    },
    /// Serve the ledger as an MCP server on stdio (agents pull work)
    Mcp,
    /// Land done tasks' branches: rebase in a throwaway worktree, re-run the
    /// task's own tier-0 profile, fast-forward the verified tip
    Refine {
        /// The shared repository the task branches live in (default: the
        /// active `--project` manifest's `repo`; required otherwise)
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Integration branch landings fast-forward (default: "main", or
        /// the active `--project` manifest's `integration`)
        #[arg(long)]
        integration: Option<String>,
        /// Buildable workspace subdirectory the verifier runs in (for cos:
        /// "src"; "." when the manifest is at the repo root; default: ".",
        /// or the active `--project` manifest's `subdir`)
        #[arg(long)]
        subdir: Option<String>,
        /// Verifier tier for the pre-land re-check (1 = workspace tests +
        /// cargo-deny on top of the fast gate)
        #[arg(long)]
        tier: Option<u8>,
        /// Merge-authority review: a cross-family Claude or Codex session
        /// judges every landing's diff and can reject it (fail-closed once enabled)
        #[arg(long, num_args = 0..=1, default_missing_value = "true")]
        review: Option<bool>,
    },
    /// Run a HEADLESS verifier tier (tier 2 is the unattended fleet policy);
    /// never selects physical acceptance; records the result with --task
    Verify {
        #[arg(long)]
        dir: Option<PathBuf>,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long, default_value_t = 0)]
        tier: u8,
        /// Record the verification against this task
        #[arg(long)]
        task: Option<i64>,
    },
    /// Run operator-requested PHYSICAL compositor acceptance on the active VT
    PhysicalAcceptance {
        /// Repository root containing the compositor-owned desktop/ workspace
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Absolute primary DRM device path, for example /dev/dri/card0
        #[arg(long)]
        device: PathBuf,
        /// Exact connected DRM connector name, for example DP-1
        #[arg(long)]
        connector: String,
        /// Hard process-tree deadline; acceptance never becomes a background session
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=3_600))]
        max_secs: u64,
        /// Required acknowledgement that this command will take the active VT and display
        #[arg(long, required = true)]
        take_vt_and_display: bool,
    },
    /// Talk to the fleet: a claude session with the foreman MCP server
    /// attached (interactive, or one-shot with a question)
    Mayor {
        /// One-shot question; omit for an interactive session
        question: Vec<String>,
        #[arg(long)]
        model: Option<String>,
        /// Pre-allow the MUTATING fleet tools too (claim/complete/bounce) —
        /// default is the read/report set (status, task detail, findings)
        #[arg(long)]
        full_tools: bool,
    },
    /// Fleet spend control: daily ceilings + kill switch
    #[command(subcommand)]
    Governor(GovernorCmd),
    /// File a finding (discovered work) against a task
    Finding {
        #[arg(long)]
        task: Option<i64>,
        #[arg(long, default_value = "info")]
        severity: String,
        title: String,
        #[arg(long, default_value = "")]
        body: String,
    },
    /// Fleet status: task counts and budgets, recent runs, total spend
    Status {
        /// Machine-readable output (the Mix/script surface)
        #[arg(long)]
        json: bool,
    },
    /// Rank files by observed whole-file reattachment after compaction
    AttachmentHarm {
        /// Claude's project transcript root (default: ~/.claude/projects)
        #[arg(long)]
        claude_projects: Option<PathBuf>,
        /// Maximum ranked files shown per population
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Machine-readable output; contains paths and sizes, never contents
        #[arg(long)]
        json: bool,
    },
    /// What the fleet's own systemd units are doing: any unit in
    /// `failed` (with its exit code, when it failed, and what that means
    /// for the fleet), and any dispatch/refine/tier-2 sweep still running
    /// code older than the installed binary. Prints nothing when the fleet
    /// is healthy and current, and always exits 0 — this is advisory, and
    /// an installer that calls it must not fail because systemd is absent.
    ///
    /// Meant as the last line of a deploy: the installer's version probe
    /// asserts the ARTIFACT is new, and this asserts that RUNNING WORK is
    /// using it. Nothing is killed — an agent mid-task is paid work in
    /// flight, and interrupting it is the operator's call.
    FleetCheck {
        /// Compare unit start times against this binary's mtime instead of
        /// the running foreman's. An installer passes the artifact it just
        /// wrote (e.g. /opt/cosmix/bin/foreman).
        #[arg(long)]
        binary: Option<PathBuf>,
        /// Also report healthy active sweeps, not just stale ones.
        #[arg(long)]
        all: bool,
    },
    /// Ring the foreman.wake ABP verb (answered by the
    /// `~/.cmctl/_etc/foreman/foreman-wake.mix` citizen), best effort:
    /// nudges the supervisor to run now instead of waiting for its backstop
    /// timer. `task add`/`requeue`, a run/dispatch that dispositions a
    /// task, and a refinery bounce all fire this automatically; exposed
    /// standalone for manual/scripted use. Never fails — a missed wake only
    /// costs latency.
    Wake,
    /// Reclaim Cargo scratch for terminal tasks, plus crash-stranded
    /// `.foreman-review-*` worktrees. Live task worktrees and source are
    /// never eligible.
    GcScratch {
        /// Fleet/project state root containing task worktrees and legacy
        /// task-N-target directories (default: active project's root).
        #[arg(long)]
        fleet_dir: Option<PathBuf>,
        /// Shared Git repository whose registered worktrees are eligible
        /// (default: active project's repo; required otherwise).
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Minimum terminal age for the ordinary backstop pass.
        #[arg(long)]
        terminal_age_hours: Option<u64>,
        /// ZFS pool whose real capacity controls pressure escalation. When
        /// omitted, only the age policy runs.
        #[arg(long)]
        pool: Option<String>,
        /// At or above this real zpool capacity, include younger terminal
        /// tasks too.
        #[arg(
            long,
            value_parser = clap::value_parser!(u8).range(1..=100)
        )]
        pressure_percent: Option<u8>,
        /// Bound each shared target and target-refine cache. The compiled
        /// default is deliberately generous enough to retain known hot
        /// caches; fleet policy or this flag may override it.
        #[arg(long)]
        shared_max_gb: Option<u64>,
        /// Recorded wall-clock input used for terminal-age selection. Omit
        /// for a live sweep; pass the reported RFC 3339 value to replay the
        /// same selection later.
        #[arg(long)]
        as_of: Option<chrono::DateTime<chrono::Utc>>,
        /// Report candidates and allocated size without deleting anything.
        #[arg(long)]
        dry_run: bool,
        /// Required to actually delete anything. This is a real-deletion
        /// gate, not a human-confirmation prompt: the installed timer's
        /// `ExecStart` always passes it, so the unattended backstop never
        /// blocks on a person. Its purpose is to make the bare, undecorated
        /// command name — what an agent poking at this binary would type
        /// first — a safe no-op preview against the live fleet rather than
        /// an immediate delete; a deliberate `--dry-run` still previews
        /// without it.
        #[arg(long)]
        confirm: bool,
    },
    /// Garbage-collect a cargo target directory: bounded size, stalest-first
    /// cleanup under {debug,release}/{deps,build,.fingerprint}.
    GcCache {
        /// The cargo target directory to GC (default: $CARGO_TARGET_DIR).
        /// No fallback beyond that — a relative "target" resolved against
        /// whatever cwd a systemd unit landed in is a silent no-op.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Maximum size in GB (default: 40, or $FOREMAN_CACHE_MAX_GB)
        #[arg(long)]
        max_gb: Option<u64>,
    },
}

#[derive(Subcommand)]
pub(super) enum GovernorCmd {
    /// Today's spend vs ceilings + kill-switch state
    Status,
    /// Throw the kill switch (creates the STOP file; claims refuse)
    Stop {
        #[arg(default_value = "stopped by operator")]
        reason: String,
    },
    /// Clear the kill switch
    Resume,
}

#[derive(Subcommand)]
pub(super) enum ConfigCmd {
    /// Show every effective key and its env/conf/default source
    Show {
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
}
