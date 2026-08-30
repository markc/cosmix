/// A claim's identity: who holds it, and which attempt (claim generation).
/// `attempt` is the monotonic counter `claim_task` bumps — never reset — so
/// a stale claimant with a recycled claimant NAME but an OLD generation is
/// refused deterministically (a force-requeue + same-name reclaim cannot
/// let a delayed old-attempt write land on the new attempt).
#[derive(Debug, Clone, Copy)]
pub struct ClaimToken<'a> {
    pub owner: &'a str,
    pub generation: i64,
}

/// Neutral, storage-boundary shape of a finished run — the ledger must not
/// depend on executor::RunOutcome; callers (executor.rs) convert.
#[derive(Debug, Clone)]
pub struct StoredRunOutcome {
    pub stop: String,
    pub result: Option<String>,
    pub error: Option<String>,
    pub input_tokens: u64,
    pub fresh_input_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub output_tokens: u64,
    pub cost_usd: Option<f64>,
    pub session_ref: Option<String>,
}

/// What a prior run of a task told us — enough for a caller to decide
/// whether a fresh attempt at the SAME rung can resume it instead of
/// starting cold. See [`Ledger::last_run_ref`].
#[derive(Debug, Clone)]
pub struct RunRef {
    pub id: i64,
    pub agent: String,
    pub model: Option<String>,
    pub session_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: i64,
    pub title: String,
    pub spec: String,
    pub kind: String,
    pub risk: String,
    /// Operator-owned package-version bump intent. `None` preserves the
    /// historical risk/kind derivation for tasks authored before this field
    /// existed or without `task add --bump`.
    pub bump: Option<String>,
    pub status: String,
    pub deps: Vec<i64>,
    /// Operator-designated crate names whose package-version line this task
    /// may bump even before another file under that crate changes.
    pub crates: Vec<String>,
    pub claimed_by: Option<String>,
    /// Expiry of the current dispatch claim. Returned to claimants so even a
    /// worker with no controller-local pid can observe and renew its lease.
    pub lease_until: Option<String>,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    /// Monotonic claim generation — NEVER reset; the stale-result guard in
    /// [`Ledger::complete_verified`] depends on it not repeating.
    pub attempt: i64,
    /// Per-attempt delivery/quality charges. Runnable verifier failures and
    /// review rejections both advance the ladder, at most once per attempt.
    pub ladder_failures: i64,
    /// Review rejections retained as a diagnostic subset of ladder charges.
    pub review_rejections: i64,
    /// Branch-contract or MCP self-bounce dispositions since the last
    /// successful landing or explicit operator requeue. They are not quality
    /// charges, but park at the configured recurrence limit.
    pub branch_contract_failures: i64,
    /// Consecutive infrastructure refusals (worktree, policy setup, ledger
    /// hiccups) — tracked separately from ladder failures because these are
    /// the harness's fault, not the task's. Reset by every later non-infra
    /// disposition; claiming alone deliberately preserves the sequence.
    pub infra_refusals: i64,
    /// Vendor/harness refusals are not retried in the same wake loop.
    pub dispatch_after: Option<String>,
    /// Consecutive claimed runs that ended with dirty work and live Claude
    /// Code background Bash. One retry is free; a repeat parks without
    /// consuming the model ladder.
    pub background_abandonments: i64,
    /// Reserved for the explicit operator-run path. Unattended dispatch and
    /// MCP claiming skip these tasks without consuming an attempt.
    pub operator_driven: bool,
    /// Spec-owned (set at task creation, never by the completing agent —
    /// the anti-gaming rule): which tier-0 profile gates completion.
    pub verifier_profile: String,
    /// Operator-owned total dollar budget across this task's attempts.
    /// `None` leaves each run on the fleet reserve or explicit invocation cap.
    pub budget_usd: Option<f64>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionBump {
    Patch,
    Minor,
}

impl VersionBump {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Patch => "patch",
            Self::Minor => "minor",
        }
    }
}

impl std::fmt::Display for VersionBump {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for VersionBump {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "patch" => Ok(Self::Patch),
            "minor" => Ok(Self::Minor),
            _ => anyhow::bail!("bump must be 'patch' or 'minor', got {value:?}"),
        }
    }
}

/// Historical landing-owned version derivation. Keep this isolated so a task
/// without explicit intent continues to behave exactly as it did before
/// schema 15.
pub fn derived_version_bump(risk: &str, kind: &str) -> VersionBump {
    if risk == "high" || matches!(kind, "feature" | "breaking" | "schema") {
        VersionBump::Minor
    } else {
        VersionBump::Patch
    }
}

impl Task {
    pub fn effective_version_bump(&self) -> Result<VersionBump> {
        self.bump
            .as_deref()
            .map(str::parse)
            .unwrap_or_else(|| Ok(derived_version_bump(&self.risk, &self.kind)))
    }

    pub fn version_bump_source(&self) -> &'static str {
        if self.bump.is_some() {
            "explicit"
        } else {
            "derived"
        }
    }
}

/// Operator-owned task controls that route completion and manifest policy.
/// Keeping these structured and separate from prose prevents an agent-facing
/// task description from becoming an authority channel.
#[derive(Debug, Clone, Copy)]
pub struct TaskControls<'a> {
    pub verifier_profile: &'a str,
    pub crates: &'a [String],
    /// A reason reserves the task at creation; absence leaves it available to
    /// unattended dispatch. This shape prevents a caller from creating a new
    /// unexplained reservation through the ledger API.
    pub operator_driven_reason: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct OperatorDrivenStatus {
    pub task_id: i64,
    pub reservation_explained: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TaskBudgetRemainder {
    pub limit_usd: f64,
    pub charged_usd: f64,
    pub remaining_usd: f64,
}

/// The operation a durable remote-delivery intent represents. Deletion is a
/// separate kind because replaying it as an update would target a different
/// Git operation despite carrying the same verified landing tip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushIntentKind {
    Update,
    Delete,
}

impl PushIntentKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Update => "update",
            Self::Delete => "delete",
        }
    }
}

/// Durable result taxonomy for one remote-delivery intent. This deliberately
/// mirrors `remote_git::RemoteOutcome` from remote-push Slice A; the push
/// slice can adapt between them without weakening `Unknown` into `Failed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushIntentOutcome {
    Succeeded,
    Failed,
    Unknown,
}

impl PushIntentOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }
}

/// One immutable operation plus its mutable delivery outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushIntent {
    pub id: i64,
    pub task_id: i64,
    pub attempt: i64,
    pub kind: PushIntentKind,
    /// Immutable, fully qualified single-ref refspec. Update sources are the
    /// verified object id, never a mutable local branch name.
    pub refspec: String,
    pub verified_tip: String,
    pub outcome: PushIntentOutcome,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PushRecoveryReport {
    pub replayed_failed: usize,
    pub reported_unknown: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: i64,
    pub task_id: i64,
    pub agent: String,
    pub model: Option<String>,
    pub session_ref: Option<String>,
    pub tokens_in: i64,
    /// Fresh, non-cached input tokens. `None` means the lane did not report them.
    pub fresh_input_tokens: Option<i64>,
    /// Cache-read input tokens. `None` means the lane did not report them.
    pub cache_read_input_tokens: Option<i64>,
    /// Cache-creation input tokens. `None` means the lane did not report them.
    pub cache_creation_input_tokens: Option<i64>,
    pub tokens_out: i64,
    pub cost_usd: Option<f64>,
    /// Dollar hold selected for this run. Used to charge a budgeted task
    /// conservatively when the lane dies before reporting any usage.
    pub reserved_usd: Option<f64>,
    pub verdict: Option<String>,
    pub result: Option<String>,
    pub error: Option<String>,
    pub duration_ms: Option<i64>,
    pub started_at: String,
    pub role: String,
    pub delivery: String,
    pub quality: String,
    /// Claim generation which produced this run. Review rows inherit the
    /// task's current generation; legacy rows remain unknown.
    pub attempt: Option<i64>,
    /// At most one quality charge can be attached to an implementation run.
    pub ladder_charge: bool,
    pub ladder_charge_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AttemptCharge {
    pub attempt: i64,
    pub run_id: i64,
    pub charged: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct VoidFraction {
    pub contributing_runs: i64,
    pub unknown_runs: i64,
    pub fraction: f64,
}

pub struct Ledger {
    conn: Connection,
    open_options: LedgerOpenOptions,
}

enum ParkGeneration {
    LadderFailures(i64),
    TaskBudget(f64),
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum LedgerCreate {
    ParentsAndFile,
    FileOnly,
    Never,
}

/// Cloneable authority for opening another connection to the ledger object
/// selected by a successful primary open.
///
/// Concurrent lanes cannot share a rusqlite connection. This authority keeps
/// the primary file's filesystem identity so every lane reopen can refuse a
/// SQLite connection bound through a pathname which has since been rebound.
#[derive(Debug, Clone)]
pub struct LedgerOpenOptions {
    authority: std::sync::Arc<LedgerOpenAuthority>,
}

#[derive(Debug)]
struct LedgerOpenAuthority {
    path: PathBuf,
    project_identity: Option<(String, String)>,
    identity: LedgerFileIdentity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LedgerFileIdentity {
    device: u64,
    inode: u64,
}
