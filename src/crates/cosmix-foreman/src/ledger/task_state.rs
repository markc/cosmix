/// Typed task-status vocabulary. The DB column and `Task.status` (serde
/// field) stay plain TEXT/String for wire compatibility — this enum is the
/// internal vocabulary transition-guard code should use instead of ad-hoc
/// `&str` literals scattered through match arms. `as_db_str`/`FromStr` are
/// the read/write boundary; every currently-stored value round-trips.
///
/// (Ledger-hardening arc item 13: "legacy stored statuses migrate to a
/// generic-state + extension representation with a compatibility read
/// path" is implemented here at the TYPE level, not as a schema/column
/// change — `category()` below is the generic state (a DB migration to a
/// second column is deferred to the extraction arc; out of scope here per
/// the task's "no DB path changes" constraint). `FromStr` accepting every
/// string this table has ever stored IS the compatibility read path.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Queued,
    Claimed,
    Running,
    Done,
    Bounced,
    Failed,
    Parked,
    Landing,
    Landed,
    /// Terminal-until-operator: retired by operator command, excluded from
    /// dispatch and default task list output.
    Retired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ClassifiedDisposition {
    pub charged: bool,
    pub status: TaskStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LandingDisposition {
    pub moved: bool,
    pub charged: bool,
    pub status: Option<TaskStatus>,
}

fn ensure_worker_disposition(status: TaskStatus) -> Result<()> {
    anyhow::ensure!(
        matches!(
            status,
            TaskStatus::Done | TaskStatus::Bounced | TaskStatus::Failed
        ),
        "finish_task is for terminal states, not {status}"
    );
    Ok(())
}

/// An unattended claimant tried to take work reserved for an operator.
/// This stays typed so dispatch can treat a flag racing its planning snapshot
/// as a normal readiness change rather than a harness failure.
#[derive(Debug)]
pub struct OperatorDrivenTask(pub i64);

impl std::fmt::Display for OperatorDrivenTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "task {} not ready: operator-driven", self.0)
    }
}

impl std::error::Error for OperatorDrivenTask {}

/// One past-lease claim [`Ledger::reap_dead_claims_with`] snapshotted before
/// asking whether its process is still alive. A candidate, not a verdict:
/// the liveness check below decides, and most candidates on a busy fleet are
/// simply long-running claims that keep their hold.
struct ExpiredClaim {
    id: i64,
    claimant: String,
    attempt: i64,
    /// `None` for a claim taken through a path that cannot vouch for a pid
    /// — unreapable by construction, see `reap_dead_claims_with`.
    claim_pid: Option<i64>,
    lease_until: String,
    /// `None` for a claim taken before the column existed.
    claimed_at: Option<String>,
}

/// A claim [`Ledger::reap_dead_claims`] released because its lease expired
/// and its claiming process was confirmed gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReapedClaim {
    pub task_id: i64,
    pub claimant: String,
    /// The pid this claim named, observed absent by the liveness check the
    /// sweep was given. Recorded (here and in the filed finding) because it
    /// is the whole evidence for the reap: the state transition came from an
    /// observation of the host, so the observation is written down rather
    /// than left to be re-made — nothing can re-observe a process that has
    /// already gone.
    pub claim_pid: i64,
    /// Seconds since the claim's lease itself expired — NOT the claim's
    /// total age. `now - lease_until`, not `now - updated_at`: the two
    /// differ by up to [`CLAIM_LEASE_SECS`], and conflating them once
    /// overstated every reap's overdue time by the full lease window.
    pub overdue_secs: i64,
    /// How long the claim had been held when it was reaped: `now -
    /// claimed_at`. `None` for a claim taken before the `claimed_at` column
    /// existed — an unknown age is reported as unknown, never back-derived
    /// from the lease window.
    pub claim_age_secs: Option<i64>,
}

/// A claim [`Ledger::reap_dead_claims`] judged dead but could NOT release:
/// its requeue-and-finding write failed even after the sweep's own busy
/// retries (exhausted contention, a constraint, a storage fault). The claim
/// is left exactly as it was — still `running`, with no finding saying
/// otherwise — and the next sweep finds it just as expired and just as
/// dead. Reported rather than swallowed, because a sweep that cannot write
/// is a harness fault: the caller must fail its run on this, or the very
/// phantom claim the reaper exists to clear survives it silently.
#[derive(Debug)]
pub struct UnreapedClaim {
    pub task_id: i64,
    pub claimant: String,
    /// The pid observed absent — the claim WAS provably dead; only the
    /// write failed.
    pub claim_pid: i64,
    pub error: anyhow::Error,
}

/// Everything one [`Ledger::reap_dead_claims_with`] sweep did and failed to
/// do. The two lists are disjoint by construction: a candidate ends up in
/// `reaped` when its release committed, in `unreaped` when its release
/// failed, and in neither when it was skipped (live, unprovable, or lost a
/// race to a legitimate release).
#[derive(Debug, Default)]
pub struct ReapSweep {
    /// Claims released back to `queued`, each with its finding committed.
    pub reaped: Vec<ReapedClaim>,
    /// Claims proven dead whose release could not be written. Non-empty
    /// means the sweep is unhealthy and the caller must say so.
    pub unreaped: Vec<UnreapedClaim>,
}

impl ReapSweep {
    /// True when the sweep neither reaped nor failed to reap anything —
    /// the ledger is exactly as the sweep found it. Deliberately covers
    /// BOTH lists: a sweep that "reaped nothing" because every write failed
    /// is not a quiet sweep.
    pub fn is_empty(&self) -> bool {
        self.reaped.is_empty() && self.unreaped.is_empty()
    }
}

fn claim_task_in_tx(
    tx: &rusqlite::Transaction<'_>,
    id: i64,
    claimant: &str,
    // The claiming process's own pid, supplied ONLY by a call site that read
    // it from `std::process::id()` itself — never parsed back out of
    // `claimant`, which for an MCP-originated claim is agent-controlled free
    // text an agent could shape as `claude@<any pid>` to suppress or forge
    // reaping. `None` here is what keeps that text untrusted: it leaves the
    // stored `claim_pid` column NULL, and `Ledger::reap_dead_claims` treats a
    // NULL `claim_pid` as "cannot prove dead" rather than a license to trust
    // the claimant string.
    claim_pid: Option<i64>,
    allow_operator_driven: bool,
    now: &str,
) -> Result<Task> {
    let task = tx
        .query_row(
            "SELECT * FROM tasks WHERE id = ?1",
            params![id],
            row_to_task,
        )
        .optional()?
        .with_context(|| format!("no task {id}"))?;
    if task.operator_driven && !allow_operator_driven {
        Err(OperatorDrivenTask(id))?;
    }
    for dep in &task.deps {
        let dep_status: Option<String> = tx
            .query_row(
                "SELECT status FROM tasks WHERE id = ?1",
                params![dep],
                |r| r.get(0),
            )
            .optional()?;
        let Some(dep_status) = dep_status else {
            anyhow::bail!("task {id} dep {dep} does not exist");
        };
        let dep_status: TaskStatus = dep_status.parse()?;
        match dep_status {
            // A refined (landed) dependency is at least as done as done —
            // accepting only done deadlocks every chain the refinery touches.
            TaskStatus::Done | TaskStatus::Landed => {}
            other => anyhow::bail!("task {id} dep {dep} is {other}, not done"),
        }
    }
    // A generous, renewable lease so slice B can eventually recover a claim
    // its worker never released. The mechanism is independent of claim_pid:
    // generic/MCP and future remote claims receive the same expiring value.
    // Derived from the caller's `now` (not a fresh `Utc::now()`) so a
    // clock-injected replay reproduces the exact same lease every time —
    // golden-stream replay diffs the whole ledger byte-for-byte.
    let now_dt = parse_utc_timestamp(now, "claim timestamp")?;
    let lease_until = (now_dt + chrono::Duration::seconds(CLAIM_LEASE_SECS)).to_rfc3339();
    // worktree/branch reset per attempt: a stale branch surviving into a
    // new claim would let the refinery land the PREVIOUS attempt's work.
    let n = tx.execute(
        "UPDATE tasks SET claimed_by = ?1, status = ?2,
                attempt = attempt + 1, worktree = NULL, branch = NULL,
                dispatch_after = NULL, lease_until = ?3, claim_pid = ?4,
                claimed_at = ?5, updated_at = ?5
         WHERE id = ?6 AND claimed_by IS NULL
           AND status IN (?7, ?8, ?9)",
        params![
            claimant,
            TaskStatus::Claimed.as_db_str(),
            lease_until,
            claim_pid,
            // `claimed_at` and `updated_at` are the same instant HERE and
            // diverge immediately after: `updated_at` moves on every later
            // write of this run, which is exactly why a reap cannot read the
            // claim's age out of it.
            now,
            id,
            TaskStatus::Queued.as_db_str(),
            TaskStatus::Bounced.as_db_str(),
            TaskStatus::Failed.as_db_str()
        ],
    )?;
    if n == 0 {
        anyhow::bail!(
            "task {id} not claimable (status {}, claimed_by {:?})",
            task.status,
            task.claimed_by
        );
    }
    tx.query_row(
        "SELECT * FROM tasks WHERE id = ?1",
        params![id],
        row_to_task,
    )
    .optional()?
    .context("task vanished after claim")
}

/// True if adding `new_id -> new_deps` to `existing_deps` would make any
/// cycle reachable from the new task's dependency roots.
fn deps_form_cycle(
    existing_deps: &HashMap<i64, Vec<i64>>,
    new_id: i64,
    new_deps: &[i64],
) -> Option<i64> {
    fn visit(
        node: i64,
        existing_deps: &HashMap<i64, Vec<i64>>,
        new_id: i64,
        new_deps: &[i64],
        visiting: &mut HashSet<i64>,
        visited: &mut HashSet<i64>,
    ) -> Option<i64> {
        if visiting.contains(&node) {
            return Some(node);
        }
        if visited.contains(&node) {
            return None;
        }
        visiting.insert(node);
        let deps = if node == new_id {
            new_deps
        } else {
            existing_deps.get(&node).map_or(&[][..], Vec::as_slice)
        };
        for &dep in deps {
            if let Some(cycle) = visit(dep, existing_deps, new_id, new_deps, visiting, visited) {
                return Some(cycle);
            }
        }
        visiting.remove(&node);
        visited.insert(node);
        None
    }

    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    for &dep in new_deps {
        if let Some(cycle) = visit(
            dep,
            existing_deps,
            new_id,
            new_deps,
            &mut visiting,
            &mut visited,
        ) {
            return Some(cycle);
        }
    }
    None
}

/// The generic, foreman-agnostic half of a stored status — the vocabulary an
/// OS-level jobs library can own on its own. Every foreman-specific name
/// (`bounced`, `landing`, `landed`) survives as the extension half of
/// [`StoredStatus`], so extraction does not have to teach a generic job
/// store about the refinery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenericState {
    /// Dispatchable: no live claim, a worker may take it.
    Ready,
    /// Claimed but not yet started.
    Claimed,
    /// A worker (or the refinery) is acting on it.
    Running,
    /// Terminal success.
    Done,
    /// Terminal-until-human: needs a decision before it moves again.
    Blocked,
}

impl GenericState {
    /// The DB string an extension-less status of this state is stored as.
    /// This is what a generic jobs store would write if it had never heard
    /// of the foreman's vocabulary.
    pub fn as_db_str(&self) -> &'static str {
        match self {
            GenericState::Ready => "queued",
            GenericState::Claimed => "claimed",
            GenericState::Running => "running",
            GenericState::Done => "done",
            GenericState::Blocked => "parked",
        }
    }
}

/// A stored status decomposed into generic state + optional extension label
/// — item 13's representation. The pair round-trips BYTE-EXACTLY to the
/// legacy string in the `tasks.status` column ([`StoredStatus::as_db_str`]),
/// which is what lets the decomposition land before the extraction arc
/// without rewriting a single fleet row: the column keeps storing
/// `bounced`/`landing`/`landed`, and the compatibility read path
/// ([`StoredStatus::from_db_str`]) is what gives them meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoredStatus {
    pub state: GenericState,
    /// `None` for a status the generic vocabulary covers exactly; otherwise
    /// the foreman-specific refinement of `state`.
    pub extension: Option<&'static str>,
}

impl StoredStatus {
    /// The exact bytes this status is stored as — `extension` when present
    /// (that IS the legacy name), else the generic state's own name.
    pub fn as_db_str(&self) -> &'static str {
        self.extension.unwrap_or_else(|| self.state.as_db_str())
    }

    /// Compatibility read path: every string this column has ever stored,
    /// decoded into generic state + extension. Unknown strings FAIL — a
    /// status nobody wrote is corruption, not a new state.
    pub fn from_db_str(s: &str) -> std::result::Result<Self, TransitionError> {
        Ok(s.parse::<TaskStatus>()?.stored())
    }
}

impl TaskStatus {
    pub fn as_db_str(&self) -> &'static str {
        match self {
            TaskStatus::Queued => "queued",
            TaskStatus::Claimed => "claimed",
            TaskStatus::Running => "running",
            TaskStatus::Done => "done",
            TaskStatus::Bounced => "bounced",
            TaskStatus::Failed => "failed",
            TaskStatus::Parked => "parked",
            TaskStatus::Landing => "landing",
            TaskStatus::Landed => "landed",
            TaskStatus::Retired => "retired",
        }
    }

    /// This status as generic state + extension (item 13). The extension is
    /// exactly the legacy stored name whenever the generic vocabulary does
    /// not already carry it, so `stored().as_db_str() == as_db_str()` for
    /// every variant — proven by `legacy_statuses_round_trip_byte_exactly`.
    pub fn stored(&self) -> StoredStatus {
        let (state, extension) = match self {
            TaskStatus::Queued => (GenericState::Ready, None),
            TaskStatus::Bounced => (GenericState::Ready, Some("bounced")),
            TaskStatus::Failed => (GenericState::Ready, Some("failed")),
            TaskStatus::Claimed => (GenericState::Claimed, None),
            TaskStatus::Running => (GenericState::Running, None),
            TaskStatus::Landing => (GenericState::Running, Some("landing")),
            TaskStatus::Done => (GenericState::Done, None),
            TaskStatus::Landed => (GenericState::Done, Some("landed")),
            TaskStatus::Parked => (GenericState::Blocked, None),
            TaskStatus::Retired => (GenericState::Blocked, Some("retired")),
        };
        StoredStatus { state, extension }
    }

    /// Dispatchable — a worker may claim it. `bounced`/`failed` are ready
    /// too: both are retry fuel, and the ladder's parking bound (not this
    /// predicate) is what stops a task retrying forever.
    pub fn is_dispatchable(&self) -> bool {
        self.stored().state == GenericState::Ready
    }

    fn closes_findings(&self) -> bool {
        matches!(self, TaskStatus::Landed | TaskStatus::Retired)
    }
}

impl std::str::FromStr for TaskStatus {
    type Err = TransitionError;

    fn from_str(s: &str) -> std::result::Result<Self, TransitionError> {
        match s {
            "queued" => Ok(TaskStatus::Queued),
            "claimed" => Ok(TaskStatus::Claimed),
            "running" => Ok(TaskStatus::Running),
            "done" => Ok(TaskStatus::Done),
            "bounced" => Ok(TaskStatus::Bounced),
            "failed" => Ok(TaskStatus::Failed),
            "parked" => Ok(TaskStatus::Parked),
            "landing" => Ok(TaskStatus::Landing),
            "landed" => Ok(TaskStatus::Landed),
            "retired" => Ok(TaskStatus::Retired),
            other => Err(TransitionError::UnknownStatus(other.to_string())),
        }
    }
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_db_str())
    }
}

#[derive(Debug, Clone)]
pub enum TransitionError {
    UnknownStatus(String),
    IllegalTransition { from: TaskStatus, to: TaskStatus },
}

impl std::fmt::Display for TransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransitionError::UnknownStatus(status) => {
                write!(f, "unknown task status {status:?}")
            }
            TransitionError::IllegalTransition { from, to } => {
                write!(f, "illegal task-status transition {from} -> {to}")
            }
        }
    }
}

impl std::error::Error for TransitionError {}

#[derive(Debug, Clone)]
pub enum DepsError {
    Missing(i64),
    Future(i64),
    Duplicate(i64),
    Cyclic(i64),
}

impl std::fmt::Display for DepsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DepsError::Missing(id) => write!(f, "dependency task {id} does not exist"),
            DepsError::Future(id) => {
                write!(f, "dependency task {id} is this task or a future task")
            }
            DepsError::Duplicate(id) => write!(f, "dependency task {id} is listed twice"),
            DepsError::Cyclic(id) => write!(f, "dependency graph contains a cycle at task {id}"),
        }
    }
}

impl std::error::Error for DepsError {}
