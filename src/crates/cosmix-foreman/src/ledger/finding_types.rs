
/// Machine-readable reason code for a finding. The shell (foreman, refinery,
/// policy-gate, etc.) owns this — agents cannot set it. Free prose in the body
/// is for context; this code is what any routing, scoring, or automation reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingReason {
    /// Tier-0 verifier failed (cargo test/clippy/fmt red, or verifier engine
    /// could not run).
    VerifierRed,
    /// A verifier observed the specific sccache EPERM and its one wrapper-free
    /// retry passed. Informational, but a stable code keeps incidents countable.
    SccacheBypassed,
    /// Branch contract broken: agent left uncommitted work or wrong branch.
    BranchContract,
    /// A reused task branch could not be rebased onto the integration head.
    RebaseConflict,
    /// Merge-authority review rejected the landing.
    ReviewRejected,
    /// One structurally validated merge-authority finding, including its
    /// source location and owning review run.
    ReviewFinding,
    /// Policy gate denied the action (write outside worktree, force-push,
    /// secret read, escalation-class action).
    PolicyDenied,
    /// Infrastructure refused (worktree provisioning, policy setup, ledger
    /// hiccup, sibling repo refresh).
    InfraRefusal,
    /// Claude Code returned from a single-turn headless session while a
    /// background Bash task was still live or killed during teardown.
    AgentAbandonedBackground,
    /// Escalation ladder exhausted — no rung could land this task.
    LadderExhausted,
    /// Governor had no headroom for the required reservation.
    GovernorNoHeadroom,
    /// The task's authored dollar budget has no headroom for another run.
    TaskBudgetExhausted,
    /// Operator-initiated finding (CLI commands, manual requeue/land).
    Operator,
    /// A task row carried a status outside the shell-owned vocabulary.
    UnknownStatus,
    /// Task retired by operator CLI command.
    Retired,
    /// Task reserved for explicit operator-driven execution.
    OperatorReserved,
    /// Task released from explicit operator-driven execution.
    OperatorReleased,
    /// An agent itself, through the MCP surface — a self-bounce with its
    /// own free-text reason, or a discovered-work filing. The reason CODE
    /// here is still shell-owned (the MCP handler picks it, not the
    /// agent's prose), but the initiator is the agent, not the operator or
    /// one of the gates.
    AgentReported,
    /// A routed lane could not enforce the task's budget. The planner skips
    /// that exact rung on the next pass without calling it a quality failure.
    RungRefusal,
    /// The refinery discarded an agent-authored package-version edit before
    /// applying the integration-base-owned landing bump.
    VersionBumpDiscarded,
    /// A claim's lease expired and its claiming process was confirmed gone
    /// (its dispatch supervisor died mid-run). Filed by
    /// [`Ledger::reap_dead_claims`]; never a ladder charge — the task did
    /// nothing wrong.
    DeadClaimReaped,
    /// Unknown reason — for legacy rows before reason codes were added.
    Unknown,
}

impl FindingReason {
    /// The exact string stored in the database. Round-trips through
    /// `from_db_str` / `as_db_str`.
    pub fn as_db_str(&self) -> &'static str {
        match self {
            FindingReason::VerifierRed => "verifier_red",
            FindingReason::SccacheBypassed => "sccache_bypassed",
            FindingReason::BranchContract => "branch_contract",
            FindingReason::RebaseConflict => "rebase_conflict",
            FindingReason::ReviewRejected => "review_rejected",
            FindingReason::ReviewFinding => "review_finding",
            FindingReason::PolicyDenied => "policy_denied",
            FindingReason::InfraRefusal => "infra_refusal",
            FindingReason::AgentAbandonedBackground => "agent_abandoned_background",
            FindingReason::LadderExhausted => "ladder_exhausted",
            FindingReason::GovernorNoHeadroom => "governor_no_headroom",
            FindingReason::TaskBudgetExhausted => "task_budget_exhausted",
            FindingReason::Operator => "operator",
            FindingReason::UnknownStatus => "unknown_status",
            FindingReason::Retired => "retired",
            FindingReason::OperatorReserved => "operator_reserved",
            FindingReason::OperatorReleased => "operator_released",
            FindingReason::AgentReported => "agent_reported",
            FindingReason::RungRefusal => "rung_refusal",
            FindingReason::VersionBumpDiscarded => "version_bump_discarded",
            FindingReason::DeadClaimReaped => "dead_claim_reaped",
            FindingReason::Unknown => "unknown",
        }
    }

    /// Parse a stored reason string. Unknown strings become `Unknown` rather
    /// than failing — this is a diagnostic field, not a core invariant.
    pub fn from_db_str(s: &str) -> Self {
        match s {
            "verifier_red" => FindingReason::VerifierRed,
            "sccache_bypassed" => FindingReason::SccacheBypassed,
            "branch_contract" => FindingReason::BranchContract,
            "rebase_conflict" => FindingReason::RebaseConflict,
            "review_rejected" => FindingReason::ReviewRejected,
            "review_finding" => FindingReason::ReviewFinding,
            "policy_denied" => FindingReason::PolicyDenied,
            "infra_refusal" => FindingReason::InfraRefusal,
            "agent_abandoned_background" => FindingReason::AgentAbandonedBackground,
            "ladder_exhausted" => FindingReason::LadderExhausted,
            "governor_no_headroom" => FindingReason::GovernorNoHeadroom,
            "task_budget_exhausted" => FindingReason::TaskBudgetExhausted,
            "operator" => FindingReason::Operator,
            "unknown_status" => FindingReason::UnknownStatus,
            "retired" => FindingReason::Retired,
            "operator_reserved" => FindingReason::OperatorReserved,
            "operator_released" => FindingReason::OperatorReleased,
            "agent_reported" => FindingReason::AgentReported,
            "rung_refusal" => FindingReason::RungRefusal,
            "version_bump_discarded" => FindingReason::VersionBumpDiscarded,
            "dead_claim_reaped" => FindingReason::DeadClaimReaped,
            _ => FindingReason::Unknown,
        }
    }
}

impl std::fmt::Display for FindingReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_db_str())
    }
}

#[derive(Debug, Clone)]
pub struct ReviewFindingInsert {
    pub run_id: i64,
    pub severity: String,
    pub file: String,
    pub line: u64,
    pub title: String,
    pub body: String,
    pub filed_by: String,
}

#[derive(Debug, Clone, Copy)]
pub struct ReviewRunRecord {
    pub run_id: i64,
    pub approve: bool,
    /// Whether the arm produced a quality verdict. Harness/vendor failures
    /// are fail-closed for landing but must not become implementation-quality
    /// evidence.
    pub delivered: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoredFinding {
    pub id: i64,
    pub severity: String,
    pub file: Option<String>,
    pub line: Option<u64>,
    pub title: String,
    pub body: String,
    pub run_id: Option<i64>,
}

/// Consecutive infrastructure refusals before dispatch files a finding.
const DEFAULT_INFRA_REFUSALS_FINDING: i64 = 3;
/// Consecutive infrastructure refusals before the task needs an operator.
const DEFAULT_INFRA_REFUSALS_PARK: i64 = 10;
const INFRA_RETRY_BACKOFF_SECS: i64 = 30;
const INFRA_RETRY_BACKOFF_MAX_SECS: i64 = 30 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InfraRefusalDisposition {
    pub count: i64,
    pub parked: bool,
}

fn normalise_utc_timestamp(now: chrono::DateTime<Utc>) -> String {
    now.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

fn parse_utc_timestamp(value: &str, label: &str) -> Result<chrono::DateTime<Utc>> {
    Ok(chrono::DateTime::parse_from_rfc3339(value)
        .with_context(|| format!("{label} is not RFC3339: {value:?}"))?
        .with_timezone(&Utc))
}

fn reset_infra_refusals_in_tx(tx: &rusqlite::Transaction<'_>, id: i64) -> Result<()> {
    tx.execute(
        "UPDATE tasks SET infra_refusals = 0, dispatch_after = NULL WHERE id = ?1",
        params![id],
    )?;
    Ok(())
}

/// Landing ends a reservation automatically: terminal rows must not inflate
/// the fleet's active operator-driven count forever. Record that automatic
/// release in the same transaction as the landing transition.
fn release_operator_driven_on_landing_in_tx(
    tx: &rusqlite::Transaction<'_>,
    task_id: i64,
    now: &str,
) -> Result<bool> {
    let released = tx.execute(
        "UPDATE tasks SET operator_driven = 0 WHERE id = ?1 AND operator_driven = 1",
        params![task_id],
    )? == 1;
    if released {
        tx.execute(
            "INSERT INTO findings
                 (task_id, severity, title, body, filed_by, reason_code, created_at)
             VALUES (?1, 'info', ?2, ?3, 'refinery', ?4, ?5)",
            params![
                task_id,
                format!("task {task_id} released from operator-driven execution"),
                "Automatically released because the task landed; terminal tasks no longer require an operator reservation.",
                FindingReason::OperatorReleased.as_db_str(),
                now
            ],
        )?;
    }
    Ok(released)
}

fn resolve_task_findings_in_tx(
    tx: &rusqlite::Transaction<'_>,
    task_id: i64,
    resolution: &str,
    resolved_at: &str,
) -> Result<usize> {
    Ok(tx.execute(
        "UPDATE findings
         SET status = 'resolved', resolution = ?2, resolved_at = ?3
         WHERE task_id = ?1 AND status = 'open'",
        params![task_id, resolution, resolved_at],
    )?)
}

/// A reservation that was atomically refused because admitting it would
/// exceed a configured governor ceiling. Callers use this marker to separate
/// ordinary capacity exhaustion from ledger and SQLite failures.
#[derive(Debug)]
pub(crate) struct ReservationRefused(String);

impl std::fmt::Display for ReservationRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ReservationRefused {}

/// How many consecutive infra refusals dispatch tolerates before filing an
/// operator-visible finding. Resolve this ONCE per dispatch sweep, not per
/// refusal: like the ladder knobs, a mistyped value is an ERROR rather than a
/// silent default, and the operator should hear about it at startup rather
/// than discover a livelocked task went unreported.
pub fn infra_refusal_finding_threshold() -> Result<i64> {
    match std::env::var("FOREMAN_INFRA_REFUSALS_FINDING") {
        Ok(v) => parse_infra_refusals_finding(&v),
        Err(std::env::VarError::NotPresent) => Ok(DEFAULT_INFRA_REFUSALS_FINDING),
        Err(e) => anyhow::bail!("FOREMAN_INFRA_REFUSALS_FINDING is not valid unicode: {e}"),
    }
}

/// The pure half of [`infra_refusal_finding_threshold`], so the override can be
/// tested without racing the process-global environment.
pub fn parse_infra_refusals_finding(spec: &str) -> Result<i64> {
    let n: i64 = spec
        .trim()
        .parse()
        .with_context(|| format!("parsing FOREMAN_INFRA_REFUSALS_FINDING {spec:?}"))?;
    anyhow::ensure!(n >= 1, "FOREMAN_INFRA_REFUSALS_FINDING must be >= 1");
    Ok(n)
}

/// How many consecutive infrastructure refusals park a task for an operator.
/// Resolve this once beside [`infra_refusal_finding_threshold`] for every
/// production operation which can record an infrastructure refusal.
pub fn infra_refusal_park_threshold() -> Result<i64> {
    match std::env::var("FOREMAN_INFRA_REFUSALS_PARK") {
        Ok(v) => parse_infra_refusals_park(&v),
        Err(std::env::VarError::NotPresent) => Ok(DEFAULT_INFRA_REFUSALS_PARK),
        Err(e) => anyhow::bail!("FOREMAN_INFRA_REFUSALS_PARK is not valid unicode: {e}"),
    }
}

/// Pure parser for [`infra_refusal_park_threshold`].
pub fn parse_infra_refusals_park(spec: &str) -> Result<i64> {
    let n: i64 = spec
        .trim()
        .parse()
        .with_context(|| format!("parsing FOREMAN_INFRA_REFUSALS_PARK {spec:?}"))?;
    anyhow::ensure!(n >= 1, "FOREMAN_INFRA_REFUSALS_PARK must be >= 1");
    Ok(n)
}

fn infra_retry_backoff_secs(count: i64) -> i64 {
    INFRA_RETRY_BACKOFF_SECS
        .saturating_mul(count.max(1))
        .min(INFRA_RETRY_BACKOFF_MAX_SECS)
}

fn note_infra_refusal_in_tx(
    tx: &rusqlite::Transaction<'_>,
    id: i64,
    error: &str,
    finding_threshold: i64,
    park_threshold: i64,
    now_dt: chrono::DateTime<Utc>,
) -> Result<Option<InfraRefusalDisposition>> {
    anyhow::ensure!(
        finding_threshold >= 1,
        "infrastructure-refusal finding threshold must be >= 1"
    );
    anyhow::ensure!(
        park_threshold >= 1,
        "infrastructure-refusal park threshold must be >= 1"
    );
    let now = normalise_utc_timestamp(now_dt);
    let updated = tx.execute(
        "UPDATE tasks SET infra_refusals = infra_refusals + 1, updated_at = ?1
         WHERE id = ?2 AND status IN ('queued', 'bounced', 'failed') AND claimed_by IS NULL",
        params![now, id],
    )?;
    if updated == 0 {
        return Ok(None);
    }

    let (count, task_title): (i64, String) = tx.query_row(
        "SELECT infra_refusals, title FROM tasks WHERE id = ?1",
        params![id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let dispatch_after = normalise_utc_timestamp(
        now_dt + chrono::Duration::seconds(infra_retry_backoff_secs(count)),
    );
    tx.execute(
        "UPDATE tasks SET dispatch_after = ?1 WHERE id = ?2",
        params![dispatch_after, id],
    )?;
    if count >= finding_threshold {
        let finding_title = format!("{INFRA_FINDING_TITLE_PREFIX} task {id}: {task_title}");
        let finding_body = format!(
            "Task {id}: {task_title} has failed to launch {count} times due to infrastructure \
             issues (worktree provisioning, policy setup, ledger hiccups). These failures are \
             NOT the task's fault — the harness cannot successfully dispatch it. Last error:\n\n{error}"
        );
        tx.execute(
            "INSERT INTO findings
                 (task_id, severity, title, body, filed_by, reason_code, created_at)
             SELECT ?1, 'major', ?2, ?3, 'dispatch', 'infra_refusal', ?4
             WHERE NOT EXISTS (
                 SELECT 1 FROM findings
                 WHERE task_id = ?1 AND filed_by = 'dispatch' AND status = 'open'
                   AND title LIKE ?5
             )",
            params![
                id,
                finding_title,
                finding_body,
                now,
                format!("{INFRA_FINDING_TITLE_PREFIX}%")
            ],
        )?;
    }
    let parked = if count >= park_threshold {
        let parked = tx.execute(
            "UPDATE tasks SET status = 'parked', dispatch_after = NULL, updated_at = ?1
             WHERE id = ?2 AND status IN ('queued', 'bounced', 'failed')
               AND claimed_by IS NULL",
            params![now, id],
        )? == 1;
        if parked {
            let finding_title = format!("{INFRA_FINDING_TITLE_PREFIX} task {id}: {task_title}");
            let finding_body = format!(
                "Task {id}: {task_title} was parked after {count} consecutive infrastructure \
                 refusals. The harness cannot dispatch this task; correct the infrastructure \
                 fault, then explicitly requeue it. The refusal which parked it is quoted \
                 verbatim below:\n\n{error}"
            );
            let promoted = tx.execute(
                "UPDATE findings SET severity = 'blocker', title = ?2, body = ?3
                 WHERE task_id = ?1 AND filed_by = 'dispatch' AND status = 'open'
                   AND title LIKE ?4",
                params![
                    id,
                    finding_title,
                    finding_body,
                    format!("{INFRA_FINDING_TITLE_PREFIX}%")
                ],
            )?;
            if promoted == 0 {
                tx.execute(
                    "INSERT INTO findings
                         (task_id, severity, title, body, filed_by, reason_code, created_at)
                     VALUES (?1, 'blocker', ?2, ?3, 'dispatch', 'infra_refusal', ?4)",
                    params![id, finding_title, finding_body, now],
                )?;
            }
        }
        parked
    } else {
        false
    };
    Ok(Some(InfraRefusalDisposition { count, parked }))
}

fn backoff_and_park_policy_denial_in_tx(
    tx: &rusqlite::Transaction<'_>,
    id: i64,
    error: &str,
    threshold: i64,
    now_dt: chrono::DateTime<Utc>,
) -> Result<bool> {
    anyhow::ensure!(threshold >= 1, "policy-denial retry limit must be >= 1");
    let now = normalise_utc_timestamp(now_dt);
    let dispatch_after =
        normalise_utc_timestamp(now_dt + chrono::Duration::seconds(INFRA_RETRY_BACKOFF_SECS));
    // Scoped to the refinery's OWN merge-review denials — dispatch-side and
    // MCP-side `policy_denied` findings (lane/credential checks before
    // claim) share the same reason_code but are not the recurrence this
    // bound governs. Counting them too dilutes or inflates the threshold
    // against unrelated open findings on the same task.
    let count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM findings
         WHERE task_id = ?1 AND status = 'open' AND reason_code = 'policy_denied'
           AND filed_by = 'refinery'",
        params![id],
        |row| row.get(0),
    )?;
    let updated = tx.execute(
        "UPDATE tasks SET dispatch_after = ?1, updated_at = ?2
         WHERE id = ?3 AND status IN ('bounced', 'failed') AND claimed_by IS NULL",
        params![dispatch_after, now, id],
    )?;
    if updated == 0 || count < threshold {
        return Ok(false);
    }
    let parked = tx.execute(
        "UPDATE tasks SET status = 'parked', claimed_by = NULL, lease_until = NULL,
                claim_pid = NULL, claimed_at = NULL, updated_at = ?1
         WHERE id = ?2 AND status IN ('bounced', 'failed')",
        params![now, id],
    )? == 1;
    if parked {
        tx.execute(
            "INSERT INTO findings
                 (task_id, severity, title, body, filed_by, reason_code, created_at)
             SELECT ?1, 'blocker', 'policy-denial retry limit reached',
                    ?2, 'ledger', 'policy_denied', ?3
             WHERE NOT EXISTS (
                 SELECT 1 FROM findings WHERE task_id = ?1 AND status = 'open'
                   AND title = 'policy-denial retry limit reached'
             )",
            params![
                id,
                format!(
                    "The task reached {count} policy-denied landing attempts (policy limit \
                     {threshold}). This is an operator configuration problem no agent can \
                     repair, so the task is parked until the lane or credential is fixed and \
                     the task is requeued. Last denial:\n\n{error}"
                ),
                now
            ],
        )?;
    }
    Ok(parked)
}

fn park_repeated_branch_contract_in_tx(
    tx: &rusqlite::Transaction<'_>,
    id: i64,
    threshold: i64,
    now: &str,
) -> Result<bool> {
    anyhow::ensure!(threshold >= 1, "branch-contract limit must be >= 1");
    let count: i64 = tx.query_row(
        "SELECT branch_contract_failures FROM tasks WHERE id = ?1",
        params![id],
        |row| row.get(0),
    )?;
    if count < threshold {
        return Ok(false);
    }
    let parked = tx.execute(
        "UPDATE tasks SET status = 'parked', claimed_by = NULL, lease_until = NULL,
                claim_pid = NULL, claimed_at = NULL, updated_at = ?1
         WHERE id = ?2 AND status IN ('queued', 'bounced', 'failed')
           AND branch_contract_failures = ?3",
        params![now, id, count],
    )? == 1;
    if parked {
        tx.execute(
            "INSERT INTO findings
                 (task_id, severity, title, body, filed_by, reason_code, created_at)
             SELECT ?1, 'blocker', 'branch-contract retry limit reached',
                    ?2, 'ledger', 'branch_contract', ?3
             WHERE NOT EXISTS (
                 SELECT 1 FROM findings WHERE task_id = ?1 AND status = 'open'
                   AND title = 'branch-contract retry limit reached'
             )",
            params![
                id,
                format!(
                    "The task produced {count} branch-contract or agent self-bounce dispositions \
                     without a successful landing or operator requeue (policy limit {threshold}). \
                     It is parked for an operator to inspect the handoff before requeueing."
                ),
                now
            ],
        )?;
    }
    Ok(parked)
}
