const SCHEMA_VERSION: i64 = 18;
const INFRA_FINDING_TITLE_PREFIX: &str = "dispatch infrastructure refusals:";
const ABANDONED_BACKGROUND_LIMIT: i64 = 2;
pub(crate) const DEFAULT_BRANCH_CONTRACT_LIMIT: i64 = 3;
/// How long a claim remains valid without a heartbeat. This is deliberately
/// much longer than the 7,200-second tier-1 verifier allowance: reclaiming a
/// live remote worker during a mesh outage is worse than waiting longer to
/// recover a dead one. Slice B owns expiry/reclaim policy; this constant and
/// [`CLAIM_HEARTBEAT_SECS`] own only the lease mechanism.
pub(crate) const CLAIM_LEASE_SECS: i64 = 24 * 60 * 60;
/// How often the local runner renews its claim while an agent process is live.
/// Remote/PID-less workers use the same generation-fenced renewal through the
/// MCP `task_heartbeat` tool.
pub(crate) const CLAIM_HEARTBEAT_SECS: u64 = 5 * 60;
/// Prefix of the `claimed_by` value the scratch-cleanup sweep holds for
/// exactly as long as it is removing a landed/retired task's build scratch.
/// Its sole purpose is closing the window between "this task looked terminal
/// and unclaimed" and `remove_dir_all` actually running: NO requeue — not
/// even `--force` — may clear this lease while the process that took it is
/// still running, so a task cannot come back to `queued`, and thence to
/// dispatch, under filesystem work in flight.
///
/// The lease is not a bare sentinel: [`Ledger::begin_scratch_cleanup`]
/// stamps it with the reclaiming process's `(pid, /proc starttime)`, which
/// is what makes the interlock ENFORCEABLE rather than advisory. `--force`
/// used to be an unconditional override, which left the operator holding a
/// gun: clearing a live sweep's lease re-enables dispatch into a worktree
/// `remove_dir_all` is still walking. Now the override is decided by the
/// host, not by the flag — see [`scratch_gc_owner_alive`]. The recovery path
/// for a genuinely wedged sweep is to kill its pid (after which `--force`
/// works immediately), not to talk the ledger out of the check.
pub const SCRATCH_GC_CLAIMANT: &str = "foreman-scratch-gc";

/// The stamped `claimed_by` this process writes when it takes a
/// scratch-cleanup lease: `foreman-scratch-gc:pid=<pid>:start=<starttime>`.
/// `start` is omitted only if `/proc` could not be read for our own pid, in
/// which case pid reuse degrades the liveness answer to "looks alive" — a
/// refused `--force`, never a permitted deletion under a live run.
fn scratch_gc_claimant_stamp() -> String {
    let pid = std::process::id() as i64;
    match crate::procutil::starttime(pid) {
        Some(start) => format!("{SCRATCH_GC_CLAIMANT}:pid={pid}:start={start}"),
        None => format!("{SCRATCH_GC_CLAIMANT}:pid={pid}"),
    }
}

/// Is `claimant` a scratch-cleanup lease — the bare sentinel, or a stamped
/// one written by [`Ledger::begin_scratch_cleanup`]?
pub fn is_scratch_gc_claimant(claimant: &str) -> bool {
    claimant == SCRATCH_GC_CLAIMANT
        || claimant
            .strip_prefix(SCRATCH_GC_CLAIMANT)
            .is_some_and(|rest| rest.starts_with(':'))
}

/// The `(pid, pid_start)` stamped into a scratch-cleanup claimant, or `None`
/// if it carries no parsable pid.
fn scratch_gc_claim_owner(claimant: &str) -> Option<(i64, Option<i64>)> {
    let rest = claimant
        .strip_prefix(SCRATCH_GC_CLAIMANT)?
        .strip_prefix(':')?;
    let mut pid = None;
    let mut start = None;
    for field in rest.split(':') {
        if let Some(value) = field.strip_prefix("pid=") {
            pid = value.parse::<i64>().ok();
        } else if let Some(value) = field.strip_prefix("start=") {
            start = value.parse::<i64>().ok();
        }
    }
    pid.map(|pid| (pid, start))
}

/// Is the process that took this scratch-cleanup lease still running?
///
/// This is the predicate that makes the lease enforceable, and it is
/// deliberately asymmetric. A stamped lease whose pid is alive answers
/// `true` and the requeue is refused, because a `remove_dir_all` may be in
/// flight inside that task's worktree right now. A stamped lease whose pid
/// is gone answers `false`: the deletion died with the process, so there is
/// nothing left to race and `--force` is the correct recovery. An UNSTAMPED
/// bare sentinel also answers `false` — it can only have been written by a
/// build predating the stamp, so there is no pid to check and refusing it
/// forever would strand the row with no recovery at all.
pub fn scratch_gc_owner_alive(claimant: &str) -> bool {
    scratch_gc_owner_alive_with(claimant, crate::procutil::owner_alive)
}

/// [`scratch_gc_owner_alive`] with the host observation supplied by the
/// caller, so the interlock can be tested against a recorded answer instead
/// of whatever `/proc` happens to say during the test run.
pub fn scratch_gc_owner_alive_with(
    claimant: &str,
    owner_alive: impl Fn(Option<i64>, Option<i64>) -> bool,
) -> bool {
    match scratch_gc_claim_owner(claimant) {
        Some((pid, start)) => owner_alive(Some(pid), start),
        None => false,
    }
}
const BUSY_RETRIES: usize = 5;
/// The last-chance cleanup after a run-path write already exhausted
/// [`BUSY_RETRIES`] gets a deliberately larger budget. The asymmetry is the
/// point: a slow release only costs the sweep time, while a cleanup write
/// that gives up leaves the task claimed and `running` — and dispatch's
/// infrastructure-refusal path only touches UNCLAIMED tasks, so nothing in
/// the fleet recovers it without an operator.
const CLEANUP_BUSY_RETRIES: usize = 10;
/// A live session's event stream is the one ledger write that may outwait an
/// ordinary transition instead of disposing of the run. This is a wall-clock
/// budget rather than an attempt count: not every SQLite busy class invokes
/// the connection's busy handler for the full `busy_timeout`.
const RUN_EVENT_BUSY_BUDGET: Duration = Duration::from_secs(60);
const BUSY_INITIAL_BACKOFF: Duration = Duration::from_millis(25);
/// Cap the exponential backoff so the larger cleanup budget buys more
/// *attempts* against a clearing lock rather than one enormous final sleep.
const BUSY_MAX_BACKOFF: Duration = Duration::from_millis(400);

#[cfg(test)]
std::thread_local! {
    static BUSY_RETRIES_EXHAUSTED_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
std::thread_local! {
    static FAIL_SCHEMA_14_BEFORE_COMMIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
std::thread_local! {
    pub(crate) static FAIL_LANDING_FINDING_BEFORE_INSERT: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

// Fail after changing `tasks.operator_driven` but before inserting its
// finding. The enclosing transaction must roll both sides back.
#[cfg(test)]
std::thread_local! {
    static FAIL_OPERATOR_DRIVEN_FINDING_BEFORE_INSERT: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

// Deterministically fail the next `record_event_at` write once, then reset.
// A real SQLite lock cannot cleanly isolate "the claim write succeeds, the
// very next ledger write fails" on a fast test clock — the run-event retry
// budget is a 60-second wall clock, not an attempt count — so this is the
// injection point that stands in for the run 425 incident task 94 fixed
// (a ledger-event append that failed before `drive()` ever started).
#[cfg(test)]
std::thread_local! {
    pub(crate) static FAIL_NEXT_RUN_EVENT_WRITE: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

#[cfg(test)]
std::thread_local! {
    static FAIL_NEXT_LAST_RUN_REF_BUSY: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
    static FAIL_NEXT_LAST_RUN_REF_ERROR: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

// The same idea one step later in the run: fail the write that REPORTS the
// outcome (and, in the same transaction, releases the claim), non-busily —
// the class of failure that used to escape the runner's closure through its
// pass-through arm and strand the task claimed. Non-busy on purpose: the
// busy-exhausted route already had an arm of its own.
#[cfg(test)]
std::thread_local! {
    pub(crate) static FAIL_NEXT_TASK_DISPOSITION_WRITE: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

// Fail the reap of ONE named task, non-busily, so a sweep with several dead
// claims can be observed losing exactly one of them. The report the sweep
// returns is what an operator sees, and a per-candidate failure must cost
// only that candidate — the reason the retry lives inside the sweep rather
// than around it.
#[cfg(test)]
std::thread_local! {
    pub(crate) static FAIL_CLAIM_REAP_FOR_TASK: std::cell::Cell<Option<i64>> =
        const { std::cell::Cell::new(None) };
}

/// Marker carried by a ledger write that remained locked after the shared
/// bounded retry. Dispatch uses this to distinguish SQLite weather from an
/// agent outcome after a run has already been claimed.
#[derive(Debug)]
pub struct SqliteBusyRetriesExhausted {
    operation: String,
}

impl std::fmt::Display for SqliteBusyRetriesExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} remained blocked after bounded SQLite busy retries",
            self.operation
        )
    }
}

impl std::error::Error for SqliteBusyRetriesExhausted {}

/// Retry a ledger operation only when SQLite reports transient lock
/// contention. Every statement or transaction remains atomic: a busy write
/// did not commit and is safe to retry. Other failures retain their original
/// error and fail immediately.
pub fn ledger_write_with_busy_retry<T>(
    operation: &str,
    write: impl FnMut() -> Result<T>,
) -> Result<T> {
    retry_while_busy(operation, BUSY_RETRIES, write)
}

/// The same retry, on the larger [`CLEANUP_BUSY_RETRIES`] budget, for the
/// writes that dispose of a run whose ordinary ledger write already gave up.
/// There is nothing after this: whatever it fails to write stays unwritten.
pub fn ledger_cleanup_write_with_busy_retry<T>(
    operation: &str,
    write: impl FnMut() -> Result<T>,
) -> Result<T> {
    retry_while_busy(operation, CLEANUP_BUSY_RETRIES, write)
}

/// Append a live run event with a bounded wall-clock budget large enough to
/// outwait legitimate foreman transition bursts. A genuine wedged writer
/// still surfaces once [`RUN_EVENT_BUSY_BUDGET`] has elapsed.
pub fn ledger_run_event_write_with_busy_retry<T>(
    operation: &str,
    write: impl FnMut() -> Result<T>,
) -> Result<T> {
    retry_while_busy_for(operation, RUN_EVENT_BUSY_BUDGET, write)
}

fn retry_while_busy<T>(
    operation: &str,
    budget: usize,
    mut write: impl FnMut() -> Result<T>,
) -> Result<T> {
    let started = Instant::now();
    let mut backoff = BUSY_INITIAL_BACKOFF;
    for retry in 0..=budget {
        match write() {
            Ok(value) => return Ok(value),
            Err(error) if sqlite_busy(&error) && retry < budget => {
                eprintln!(
                    "foreman: SQLite busy while {operation}; retrying in {} ms",
                    backoff.as_millis()
                );
                std::thread::sleep(backoff);
                backoff = backoff.saturating_mul(2).min(BUSY_MAX_BACKOFF);
            }
            Err(error) if sqlite_busy(&error) => {
                // Deliberately NOT fired for the cleanup budget: a test that
                // could unblock the ledger from inside the cleanup's own
                // exhaustion would be testing the hook, not the contention.
                #[cfg(test)]
                if budget == BUSY_RETRIES {
                    fire_busy_retries_exhausted_hook_for_test();
                }
                return busy_budget_exhausted(operation, started.elapsed(), error);
            }
            Err(error) => return Err(error).with_context(|| operation.to_string()),
        }
    }
    unreachable!("bounded retry loop always returns")
}

fn retry_while_busy_for<T>(
    operation: &str,
    budget: Duration,
    mut write: impl FnMut() -> Result<T>,
) -> Result<T> {
    let started = Instant::now();
    let mut backoff = BUSY_INITIAL_BACKOFF;
    loop {
        match write() {
            Ok(value) => return Ok(value),
            Err(error) if sqlite_busy(&error) => {
                let elapsed = started.elapsed();
                if elapsed >= budget {
                    #[cfg(test)]
                    fire_busy_retries_exhausted_hook_for_test();
                    return busy_budget_exhausted(operation, elapsed, error);
                }
                let delay = backoff.min(budget.saturating_sub(elapsed));
                eprintln!(
                    "foreman: SQLite busy while {operation}; retrying in {} ms",
                    delay.as_millis()
                );
                std::thread::sleep(delay);
                backoff = backoff.saturating_mul(2).min(BUSY_MAX_BACKOFF);
            }
            Err(error) => return Err(error).with_context(|| operation.to_string()),
        }
    }
}

fn busy_budget_exhausted<T>(operation: &str, elapsed: Duration, error: anyhow::Error) -> Result<T> {
    eprintln!(
        "foreman: SQLite busy budget exhausted after {} ms while {operation}; \
         SQLite cannot identify the lock holder",
        elapsed.as_millis()
    );
    Err(error).context(SqliteBusyRetriesExhausted {
        operation: format!(
            "{operation} ({} ms elapsed; holder unavailable)",
            elapsed.as_millis()
        ),
    })
}

pub fn sqlite_busy_retries_exhausted(error: &anyhow::Error) -> bool {
    error.is::<SqliteBusyRetriesExhausted>()
}

#[cfg(test)]
pub(crate) fn set_busy_retries_exhausted_hook_for_test(hook: impl FnOnce() + 'static) {
    BUSY_RETRIES_EXHAUSTED_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

#[cfg(test)]
pub(crate) fn fail_next_run_event_write_for_test() {
    FAIL_NEXT_RUN_EVENT_WRITE.with(|fail| fail.set(true));
}

#[cfg(test)]
pub(crate) fn fail_next_last_run_ref_busy_for_test() {
    FAIL_NEXT_LAST_RUN_REF_BUSY.with(|fail| fail.set(true));
}

#[cfg(test)]
pub(crate) fn fail_next_last_run_ref_for_test() {
    FAIL_NEXT_LAST_RUN_REF_ERROR.with(|fail| fail.set(true));
}

#[cfg(test)]
pub(crate) fn fail_next_task_disposition_write_for_test() {
    FAIL_NEXT_TASK_DISPOSITION_WRITE.with(|fail| fail.set(true));
}

#[cfg(test)]
pub(crate) fn fail_claim_reap_for_task_in_test(task_id: i64) {
    FAIL_CLAIM_REAP_FOR_TASK.with(|fail| fail.set(Some(task_id)));
}

/// Armed once per task and left armed: the sweep must be seen giving up on
/// this candidate rather than reaping it on an internal retry.
#[cfg(test)]
fn fail_armed_claim_reap_for_test(task_id: i64) -> Result<()> {
    FAIL_CLAIM_REAP_FOR_TASK.with(|fail| {
        if fail.get() == Some(task_id) {
            anyhow::bail!("injected dead-claim reap failure for task {task_id}");
        }
        Ok(())
    })
}

#[cfg(test)]
fn fire_busy_retries_exhausted_hook_for_test() {
    BUSY_RETRIES_EXHAUSTED_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(test)]
pub(crate) fn clear_busy_retries_exhausted_hook_for_test() {
    BUSY_RETRIES_EXHAUSTED_HOOK.with(|slot| drop(slot.borrow_mut().take()));
}

fn sqlite_busy(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<rusqlite::Error>()
            .is_some_and(|sqlite| {
                matches!(
                    sqlite,
                    rusqlite::Error::SqliteFailure(code, _)
                        if matches!(code.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
                )
            })
    })
}
