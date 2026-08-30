//! The executor surface: one trait, three drivers.
//!
//! Drivers spawn a vendor CLI as a child process and normalize its output
//! stream into [`AgentEvent`]s. The trait keeps the session shape
//! (start / event stream / interrupt / capabilities) consistent across
//! subprocess drivers.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::clock::{RunClock, SystemClock};
use crate::ledger::{Ledger, StoredRunOutcome};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Claude,
    Codex,
    Glm,
}

impl AgentKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
            AgentKind::Glm => "glm",
        }
    }

    /// Whether this lane's spend is attributed to the dollar ceiling.
    ///
    /// The GLM lane is driven through the claude CLI against a remapped
    /// Z.ai endpoint, so the CLI prices it at Anthropic rates — a number
    /// that is fiction for traffic Z.ai bills against a flat subscription.
    /// The runner therefore discards `cost_usd` for this lane, and the
    /// governor must reserve nothing in dollars for it: a lane whose spend
    /// can never land in the meter must not be gated by the meter, or the
    /// free lane stops the moment the metered one fills up.
    ///
    /// One predicate, used on both sides of that contract. They disagreed
    /// once — the runner nulled the cost while `reserve` still held $5 for
    /// it — and on 2026-08-19 the whole fleet halted at $96.43 of a $100
    /// ceiling with the free lane refused by a ceiling it cannot spend
    /// against.
    /// INVARIANT: this must agree with the lane's driver
    /// [`ExecutorCaps::enforces_cost_cap`], which is the ground truth for
    /// "does a real dollar figure exist here". `lane_metering_matches_drivers`
    /// in tests/phase2.rs pins the two together — a new lane that answers
    /// them differently fails that test rather than shipping a phantom hold.
    pub fn meters_dollars(&self) -> bool {
        match self {
            AgentKind::Claude => true,
            // Codex reports no cost at all: its parser fills only token
            // counts (`driver/codex.rs`), `enforces_cost_cap` is false, and
            // it authenticates against a ChatGPT subscription. Held $5 per
            // run it can never spend, it would have reproduced the
            // 2026-08-19 halt on the codex rung the first time the claude
            // rung filled the ceiling — latent only because codex was not
            // yet on the ladder. Both review arms caught this independently.
            AgentKind::Codex => false,
            AgentKind::Glm => false,
        }
    }
}

impl std::str::FromStr for AgentKind {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, String> {
        match s {
            "acp" => Err("the acp lane is retired (deprecated upstream adapter, \
                 no usage reporting, no resume); use claude instead"
                .to_string()),
            "claude" => Ok(AgentKind::Claude),
            "codex" => Ok(AgentKind::Codex),
            "glm" => Ok(AgentKind::Glm),
            other => Err(format!(
                "unknown agent kind: {other} (want claude|codex|glm)"
            )),
        }
    }
}

/// Spend limits applied to a single session. Drivers map what they can onto
/// native CLI flags (Claude: --max-turns/--max-budget-usd); the rest is
/// enforced by the runner killing the session when a [`Usage`] event crosses
/// the line.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Budget {
    pub max_turns: Option<u32>,
    pub max_budget_usd: Option<f64>,
    pub max_output_tokens: Option<u64>,
    pub max_wall_secs: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Folded input total, the sum of whichever components the lane reported.
    /// Its meaning is fixed: cap enforcement and the governor read it, so the
    /// breakdown below is strictly additional and never redefines it.
    pub input_tokens: u64,
    /// Fresh, non-cached input tokens. `None` means the lane did not report
    /// them — a different claim from a reported zero.
    #[serde(default)]
    pub fresh_input_tokens: Option<u64>,
    /// Cache-read input tokens. `None` means the lane did not report them.
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
    /// Cache-creation input tokens. `None` means the lane did not report them.
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
    pub output_tokens: u64,
    pub cost_usd: Option<f64>,
}

impl Usage {
    /// Add two distinct process totals. Optional components stay known only
    /// when both processes reported them.
    pub fn add_process(&self, other: &Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            fresh_input_tokens: add_known(self.fresh_input_tokens, other.fresh_input_tokens),
            cache_read_input_tokens: add_known(
                self.cache_read_input_tokens,
                other.cache_read_input_tokens,
            ),
            cache_creation_input_tokens: add_known(
                self.cache_creation_input_tokens,
                other.cache_creation_input_tokens,
            ),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            cost_usd: match (self.cost_usd, other.cost_usd) {
                (Some(left), Some(right)) => Some(left + right),
                _ => None,
            },
        }
    }
}

fn add_known(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    left.zip(right)
        .map(|(left, right)| left.saturating_add(right))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Done,
    BudgetCeiling,
    Interrupted,
    Error,
}

/// A resume request that did not attach to the requested vendor session.
/// This is deliberately separate from the error string: only these typed,
/// terminal classifications may authorise a controlled fresh fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResumeFailure {
    SessionNotFound,
    SessionIdMismatch,
    SessionIdMissing,
}

impl ResumeFailure {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionNotFound => "session_not_found",
            Self::SessionIdMismatch => "session_id_mismatch",
            Self::SessionIdMissing => "session_id_missing",
        }
    }

    pub fn permits_fresh_fallback(self) -> bool {
        matches!(self, Self::SessionNotFound)
    }
}

impl StopReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            StopReason::Done => "done",
            StopReason::BudgetCeiling => "budget_ceiling",
            StopReason::Interrupted => "interrupted",
            StopReason::Error => "error",
        }
    }
}

/// Normalized event stream, common to every driver.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentEvent {
    Started {
        session_ref: Option<String>,
    },
    Text {
        text: String,
    },
    ToolUse {
        name: String,
        detail: String,
    },
    Usage {
        usage: Usage,
    },
    /// A tool's (possibly failed) output coming back to the agent — kept in
    /// the ledger so the event record is complete, bounded by the driver.
    ToolResult {
        detail: String,
    },
    /// A stream line that carries no reportable content but proves the agent
    /// is alive — resets the runner's stall clock, never hits the ledger.
    Heartbeat,
    Raw {
        line: String,
    },
}

/// What a finished session amounts to, after the driver has reconciled its
/// event stream with the child's exit status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunOutcome {
    pub stop: StopReason,
    pub result: Option<String>,
    pub error: Option<String>,
    pub usage: Usage,
    pub session_ref: Option<String>,
    /// Session id carried by the terminal vendor result, when that protocol
    /// has one. Claude reports identity at both init and result; retaining
    /// both prevents either line from masking a disagreement in the other.
    #[serde(default)]
    pub terminal_session_ref: Option<String>,
    /// True only when the stream contained a vendor Usage event. Numeric
    /// zeros alone cannot distinguish a free turn from unknown accounting.
    #[serde(default)]
    pub usage_observed: bool,
    /// True when the child emitted any non-empty stdout beyond the one exact
    /// structured Codex not-found error. Init/result/text/tool output is work
    /// even when Usage telemetry is absent and forbids a fresh fallback.
    #[serde(default)]
    pub output_observed: bool,
    /// Set only for a resume invocation and only by terminal classification
    /// against the exact requested session id.
    #[serde(default)]
    pub resume_failure: Option<ResumeFailure>,
}

impl Ledger {
    pub fn start_run(
        &self,
        task_id: i64,
        agent: AgentKind,
        model: Option<&str>,
        role: Option<&str>,
    ) -> Result<i64> {
        self.store_run_start(task_id, agent.as_str(), model, role)
    }

    pub fn start_review_run(
        &self,
        task_id: i64,
        agent: AgentKind,
        model: Option<&str>,
    ) -> Result<i64> {
        self.store_run_start(task_id, agent.as_str(), model, Some("review"))
    }

    pub fn finish_run(&self, run_id: i64, outcome: &RunOutcome, duration_ms: i64) -> Result<()> {
        self.finish_run_as(run_id, outcome, duration_ms, None)
    }

    /// Finish a run with a harness-observed delivery classification when
    /// the generic subprocess stop is not specific enough (for example, a
    /// ledger/driver orchestration failure is a harness error, not a vendor
    /// error). `None` uses the conservative stop-reason mapping.
    pub fn finish_run_as(
        &self,
        run_id: i64,
        outcome: &RunOutcome,
        duration_ms: i64,
        delivery: Option<&str>,
    ) -> Result<()> {
        let stored = StoredRunOutcome {
            stop: outcome.stop.as_str().to_string(),
            result: outcome.result.clone(),
            error: outcome.error.clone(),
            input_tokens: outcome.usage.input_tokens,
            fresh_input_tokens: outcome.usage.fresh_input_tokens,
            cache_read_input_tokens: outcome.usage.cache_read_input_tokens,
            cache_creation_input_tokens: outcome.usage.cache_creation_input_tokens,
            output_tokens: outcome.usage.output_tokens,
            cost_usd: outcome.usage.cost_usd,
            session_ref: outcome.session_ref.clone(),
        };
        match delivery {
            Some(delivery) => self.store_run_finish_as(run_id, &stored, duration_ms, delivery),
            None => self.store_run_finish(run_id, &stored, duration_ms),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ExecutorCaps {
    pub resume: bool,
    pub follow_up: bool,
    pub mcp_drivable: bool,
    /// Whether this driver can enforce `--max-budget-usd` (a native CLI
    /// flag reporting real cost) -- false for lanes that report no dollar
    /// figure at all, where a caller must not assume a cap silently bites.
    pub enforces_cost_cap: bool,
    /// Whether a `--max-output-tokens` hold against this driver can ever
    /// bite -- either a native flag, or (the codex/claude/glm lanes) real
    /// per-turn [`AgentEvent::Usage`] events the runner's kill-on-overage
    /// backstop can act on. False means the driver reports no usable usage
    /// at all, so a token hold against it would never fire: the governor
    /// must refuse to reserve for it rather than sell a ceiling that is
    /// fiction.
    pub enforces_token_cap: bool,
}

#[derive(Debug, Clone)]
pub struct Workspace {
    pub dir: PathBuf,
    /// Buildable Cargo workspace relative to `dir`. Agent sessions inherit
    /// the same target directory that tier 0 later verifies, so their dry
    /// runs warm the judged tree rather than a second sibling target.
    pub verify_subdir: Option<String>,
}

pub trait Executor {
    fn kind(&self) -> AgentKind;
    fn capabilities(&self) -> ExecutorCaps;
    /// Refuse caps this driver cannot enforce. The runner calls this BEFORE
    /// claiming a task — an unenforceable budget must not burn an attempt or
    /// flip the task to failed.
    fn check_budget(&self, _budget: &Budget) -> Result<()> {
        Ok(())
    }
    fn start(&self, prompt: &str, ws: &Workspace, budget: &Budget) -> Result<Session>;
    /// Resume a prior session in the SAME worktree it was started in, with
    /// `turn` as the next input, instead of starting a cold conversation
    /// that re-ingests the whole crate. Callers must check
    /// [`ExecutorCaps::resume`] first; the default here is a hard refusal so
    /// a driver that never opts in cannot be silently handed a session ref
    /// it has no way to honour.
    fn resume(
        &self,
        session_ref: &str,
        turn: &str,
        ws: &Workspace,
        budget: &Budget,
    ) -> Result<Session> {
        let _ = (session_ref, turn, ws, budget);
        anyhow::bail!(
            "{} driver does not support session resume",
            self.kind().as_str()
        )
    }
}

/// Per-driver stream parser: turns raw stdout lines into events and, once the
/// child exits, folds its accumulated state + the exit status into an outcome.
pub trait StreamParser: Send + 'static {
    fn parse_line(&mut self, line: &str) -> Vec<AgentEvent>;
    fn finish(self: Box<Self>, exit: Option<ExitStatus>, interrupted: bool) -> RunOutcome;
}

/// The half of a driver that outlives its stream: folding whatever the
/// session accumulated, plus the child's exit status, into an outcome.
/// Split out of [`StreamParser`] so [`Session`] can own a type-erased parser
/// result alongside the child process.
pub trait SessionFinish: Send + 'static {
    fn finish_run(self: Box<Self>, exit: Option<ExitStatus>, interrupted: bool) -> RunOutcome;
}

/// Adapts a line-oriented parser to [`SessionFinish`].
struct ParsedSession(Box<dyn StreamParser>);

impl SessionFinish for ParsedSession {
    fn finish_run(self: Box<Self>, exit: Option<ExitStatus>, interrupted: bool) -> RunOutcome {
        self.0.finish(exit, interrupted)
    }
}

/// One stream line may legitimately carry a multi-megabyte tool result;
/// beyond this it is truncated (the parser then reports it as Raw).
const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;
/// Rolling stderr tail kept for the failure report.
const STDERR_TAIL_BYTES: usize = 8 * 1024;
/// How long `wait()` gives a child that closed stdout but keeps running
/// before killing its process group.
const EXIT_GRACE: Duration = Duration::from_secs(60);

/// Read one `\n`-terminated line, capping the bytes kept at `max` while
/// still consuming to the newline — `read_until` alone would buffer an
/// arbitrarily long line in memory before returning.
fn read_line_bounded<R: BufRead>(
    r: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
    raw_bytes: &AtomicUsize,
) -> std::io::Result<usize> {
    let mut total = 0usize;
    loop {
        let chunk = r.fill_buf()?;
        if chunk.is_empty() {
            return Ok(total);
        }
        let (take, done) = match chunk.iter().position(|&b| b == b'\n') {
            Some(pos) => (pos + 1, true),
            None => (chunk.len(), false),
        };
        let keep = take.min(max.saturating_sub(buf.len()));
        buf.extend_from_slice(&chunk[..keep]);
        r.consume(take);
        let _ = raw_bytes.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |total| {
            Some(total.saturating_add(take))
        });
        total = total.saturating_add(take);
        if done {
            return Ok(total);
        }
    }
}

struct StderrCapture {
    tail: String,
    total_bytes: usize,
}

impl Default for StderrCapture {
    fn default() -> Self {
        Self {
            tail: "(stderr capture unavailable)".into(),
            total_bytes: usize::MAX,
        }
    }
}

/// Rolling stderr tail kept for the failure report plus an untruncated raw
/// byte count used to prove that the tail is the child's entire stderr.
fn stderr_tail(pipe: std::process::ChildStderr) -> JoinHandle<StderrCapture> {
    std::thread::spawn(move || {
        let mut pipe = pipe;
        let mut tail: Vec<u8> = Vec::new();
        let mut total_bytes = 0usize;
        let mut chunk = [0u8; 4096];
        loop {
            let n = match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => {
                    total_bytes = usize::MAX;
                    break;
                }
            };
            if total_bytes != usize::MAX {
                total_bytes = total_bytes.saturating_add(n);
            }
            tail.extend_from_slice(&chunk[..n]);
            if tail.len() > STDERR_TAIL_BYTES {
                let cut = tail.len() - STDERR_TAIL_BYTES;
                tail.drain(..cut);
            }
        }
        StderrCapture {
            tail: String::from_utf8_lossy(&tail).into_owned(),
            total_bytes,
        }
    })
}

/// Own process group so `interrupt()` can kill the agent's whole tree —
/// killing only the direct child leaves grandchildren holding the stdout
/// pipe open and the reader thread blocked short of EOF. The pdeathsig
/// makes the vendor CLI die with foreman: a crashed foreman must not leave
/// a paid agent running unsupervised. Shared by every driver.
pub(crate) fn harden(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
        let parent = std::process::id();
        unsafe {
            cmd.pre_exec(move || {
                // SIGTERM, not SIGKILL: pdeathsig reaches only the direct
                // child, and the vendor CLIs shut their own tool trees
                // down on SIGTERM — a KILL would orphan the grandchildren
                // this exists to prevent. Known residual: a vendor that
                // ignores TERM survives a foreman crash; only a
                // supervising daemon can close that (Phase 1 owns it).
                // (Note pdeathsig fires when the spawning THREAD dies —
                // also a Phase-1 concern for worker threads.)
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // Close the fork-vs-parent-death race: if foreman died
                // before prctl took effect, we were already reparented.
                if libc::getppid() != parent as libc::pid_t {
                    return Err(std::io::Error::other("foreman died before agent spawn"));
                }
                Ok(())
            });
        }
    }
    #[cfg(not(unix))]
    let _ = cmd;
}

/// A live agent session: the child process plus the reader thread pumping its
/// stdout through the driver's parser into an event channel.
pub struct Session {
    pub kind: AgentKind,
    child: Child,
    /// One channel item per non-empty raw stdout line. Keeping the boundary
    /// here lets replay advance the stall clock from captured LINE timing
    /// even when one vendor line normalizes into several ledger events.
    events: mpsc::Receiver<Vec<AgentEvent>>,
    pending: VecDeque<AgentEvent>,
    reader: Option<JoinHandle<Box<dyn SessionFinish>>>,
    stderr: Option<JoinHandle<StderrCapture>>,
    interrupted: bool,
    requested_resume: Option<String>,
    stdout_bytes: Arc<AtomicUsize>,
}

impl Session {
    /// Spawn `cmd` and pump its stdout through `parser`. The child's stdin is
    /// closed (codex wedges waiting on it otherwise) and stderr is captured
    /// for the failure report.
    pub fn spawn(kind: AgentKind, cmd: Command, parser: Box<dyn StreamParser>) -> Result<Self> {
        Self::spawn_with_resume(kind, cmd, parser, None)
    }

    /// Spawn a resume request whose terminal result must prove it attached to
    /// `requested_resume`. The ordinary [`Self::spawn`] path has no such
    /// identity contract.
    pub fn spawn_with_resume(
        kind: AgentKind,
        mut cmd: Command,
        parser: Box<dyn StreamParser>,
        requested_resume: Option<&str>,
    ) -> Result<Self> {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        harden(&mut cmd);
        let mut child = cmd.spawn().with_context(|| {
            format!("spawning {} driver: {:?}", kind.as_str(), cmd.get_program())
        })?;

        let stdout = child.stdout.take().expect("stdout was piped");
        let (tx, rx) = mpsc::channel();
        let stdout_bytes = Arc::new(AtomicUsize::new(0));
        let reader_stdout_bytes = Arc::clone(&stdout_bytes);
        let reader = std::thread::spawn(move || {
            let mut parser = parser;
            let mut reader = BufReader::new(stdout);
            let mut buf: Vec<u8> = Vec::new();
            loop {
                buf.clear();
                match read_line_bounded(&mut reader, &mut buf, MAX_LINE_BYTES, &reader_stdout_bytes)
                {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
                let line = String::from_utf8_lossy(&buf);
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if tx.send(parser.parse_line(line)).is_err() {
                    break;
                }
            }
            Box::new(ParsedSession(parser)) as Box<dyn SessionFinish>
        });

        let stderr = stderr_tail(child.stderr.take().expect("stderr was piped"));

        Ok(Session {
            kind,
            child,
            events: rx,
            pending: VecDeque::new(),
            reader: Some(reader),
            stderr: Some(stderr),
            interrupted: false,
            requested_resume: requested_resume.map(str::to_owned),
            stdout_bytes,
        })
    }

    /// Receive the next event, waiting up to `timeout`. `None` means the
    /// stream has ended (child exited and the pipe drained).
    pub fn next_event(&mut self, timeout: Duration) -> Result<Option<AgentEvent>> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }
            match self.events.recv_timeout(timeout) {
                Ok(batch) => self.pending.extend(batch),
                Err(mpsc::RecvTimeoutError::Timeout) => anyhow::bail!(
                    "no event from {} session for {:.1?}",
                    self.kind.as_str(),
                    timeout
                ),
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(None),
            }
        }
    }

    /// Receive exactly one raw-line/structured-notification batch. The
    /// runner uses this rather than [`Session::next_event`] so its clock is
    /// reset once per input line, not once per normalized event.
    pub(crate) fn next_batch(&mut self, timeout: Duration) -> Result<Option<Vec<AgentEvent>>> {
        debug_assert!(self.pending.is_empty(), "cannot mix event and batch reads");
        match self.events.recv_timeout(timeout) {
            Ok(batch) => Ok(Some(batch)),
            Err(mpsc::RecvTimeoutError::Timeout) => anyhow::bail!(
                "no event from {} session for {:.1?}",
                self.kind.as_str(),
                timeout
            ),
            Err(mpsc::RecvTimeoutError::Disconnected) => Ok(None),
        }
    }

    /// Kill the child. The stream drains, and `wait()` reports Interrupted
    /// (or BudgetCeiling if the runner killed it for spend — the runner
    /// records that distinction itself).
    pub fn interrupt(&mut self) {
        self.interrupted = true;
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.kill();
    }

    /// Join the child and fold parser state + exit status into the outcome.
    pub fn wait(self) -> Result<RunOutcome> {
        let clock = SystemClock::new();
        self.wait_with_clock(&clock)
    }

    /// Clock-injected form used by full-run replay. Production callers enter
    /// through [`Session::wait`] or the runner's [`SystemClock`].
    pub(crate) fn wait_with_clock(mut self, clock: &dyn RunClock) -> Result<RunOutcome> {
        // Normal path: the stream has ended, the child exits promptly. A
        // child that closed stdout but keeps running gets EXIT_GRACE, then
        // its group is killed and the run reports Interrupted — never an
        // unbounded block inside an unattended harness.
        let exit = if self.interrupted {
            Some(self.child.wait().context("waiting for agent child")?)
        } else {
            let deadline = clock.monotonic() + EXIT_GRACE;
            loop {
                match self.child.try_wait().context("polling agent child")? {
                    Some(status) => break Some(status),
                    None if clock.monotonic() >= deadline => {
                        self.interrupt();
                        break Some(self.child.wait().context("waiting for agent child")?);
                    }
                    None => clock.sleep(Duration::from_millis(50)),
                }
            }
        };
        let reader = self.reader.take().expect("wait called once");
        if self.interrupted {
            // The pgroup was SIGKILLed, but a descendant that re-parented via
            // setsid can hold the stdout pipe open indefinitely; give the
            // reader a grace period, then abandon it rather than hang.
            let deadline = clock.monotonic() + Duration::from_secs(10);
            while !reader.is_finished() && clock.monotonic() < deadline {
                clock.sleep(Duration::from_millis(50));
            }
            if !reader.is_finished() {
                return Ok(RunOutcome {
                    stop: StopReason::Interrupted,
                    result: None,
                    error: Some(
                        "session killed but its output pipe never drained \
                         (escaped descendant still holds it); reader abandoned"
                            .into(),
                    ),
                    usage: Usage::default(),
                    session_ref: None,
                    terminal_session_ref: None,
                    usage_observed: false,
                    output_observed: self.stdout_bytes.load(Ordering::Relaxed) > 0,
                    resume_failure: None,
                });
            }
        }
        let parser = reader
            .join()
            .map_err(|_| anyhow::anyhow!("stdout reader thread panicked"))?;
        // Same escaped-descendant hazard as stdout: a daemonized grandchild
        // that kept only stderr open would wedge an unconditional join.
        let stderr = self
            .stderr
            .take()
            .map(|h| join_with_grace(h, Duration::from_secs(5), clock))
            .unwrap_or_default();
        let mut outcome = parser.finish_run(exit, self.interrupted);
        let stdout_bytes = self.stdout_bytes.load(Ordering::Relaxed);
        let exact_not_found = self.requested_resume.as_deref().is_some_and(|requested| {
            exact_session_not_found(
                self.kind,
                requested,
                stdout_bytes,
                stderr.total_bytes,
                &stderr.tail,
            )
        });
        outcome.output_observed = stdout_bytes > 0 || (stderr.total_bytes > 0 && !exact_not_found);
        if outcome.stop == StopReason::Error
            && let Some(err) = outcome.error.as_mut()
            && !stderr.tail.trim().is_empty()
        {
            err.push_str("\nstderr: ");
            err.push_str(stderr.tail.trim());
        }
        if let Some(requested) = self.requested_resume.as_deref() {
            let identity_observed =
                outcome.session_ref.is_some() || outcome.terminal_session_ref.is_some();
            let identity_mismatch = outcome
                .session_ref
                .as_deref()
                .into_iter()
                .chain(outcome.terminal_session_ref.as_deref())
                .any(|returned| returned != requested);
            if identity_mismatch {
                outcome.resume_failure = Some(ResumeFailure::SessionIdMismatch);
                outcome.stop = StopReason::Error;
                outcome.error = Some(format!(
                    "resume reported init session id {:?} and terminal session id {:?}, requested {requested:?}",
                    outcome.session_ref.as_deref(),
                    outcome.terminal_session_ref.as_deref()
                ));
                // The requested conversation was not established. Preserve
                // the evidence in the error above, but do not persist either
                // returned id as resumable state: the current run must fail
                // and the next sweep must start cold rather than retrying the
                // same mismatched session forever.
                outcome.session_ref = None;
                outcome.terminal_session_ref = None;
            } else if outcome.stop == StopReason::Error && exact_not_found {
                outcome.resume_failure = Some(ResumeFailure::SessionNotFound);
            } else if !identity_observed {
                outcome.resume_failure = Some(ResumeFailure::SessionIdMissing);
                outcome.stop = StopReason::Error;
                outcome.error = Some(format!(
                    "resume reported neither an init nor terminal session id; requested {requested:?}"
                ));
            }
        }
        Ok(outcome)
    }
}

pub(crate) fn exact_session_not_found(
    kind: AgentKind,
    requested: &str,
    stdout_bytes: usize,
    total_stderr_bytes: usize,
    stderr_tail: &str,
) -> bool {
    if stdout_bytes != 0 {
        return false;
    }
    let expected = match kind {
        AgentKind::Claude | AgentKind::Glm => {
            format!("No conversation found with session ID: {requested}")
        }
        AgentKind::Codex => format!(
            "Error: thread/resume: thread/resume failed: no rollout found for thread id \
             {requested} (code -32600)"
        ),
    };
    let tail_is_exact = stderr_tail == expected || stderr_tail == format!("{expected}\n");
    tail_is_exact && total_stderr_bytes == stderr_tail.len()
}

/// Exact branch tip plus complete porcelain status. A resume fallback is
/// permitted only when this snapshot exists and is byte-identical before and
/// after the failed process; inability to prove stability fails closed.
pub(crate) fn workspace_fingerprint(dir: &Path) -> Option<String> {
    let tip = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .ok()?;
    if !tip.status.success() {
        return None;
    }
    let status = Command::new("git")
        .args(["status", "--porcelain=v1", "--untracked-files=all"])
        .current_dir(dir)
        .output()
        .ok()?;
    if !status.status.success() {
        return None;
    }
    Some(format!(
        "{}\0{}",
        String::from_utf8(tip.stdout).ok()?,
        String::from_utf8(status.stdout).ok()?
    ))
}

/// Join a thread if it finishes within `grace`, else abandon it — its pipe
/// may be held open forever by an escaped descendant. Abandonment leaves a
/// marker instead of silently losing the diagnosis.
fn join_with_grace(
    handle: JoinHandle<StderrCapture>,
    grace: Duration,
    clock: &dyn RunClock,
) -> StderrCapture {
    let deadline = clock.monotonic() + grace;
    while !handle.is_finished() && clock.monotonic() < deadline {
        clock.sleep(Duration::from_millis(50));
    }
    if handle.is_finished() {
        handle.join().unwrap_or_default()
    } else {
        StderrCapture {
            tail: "(stderr abandoned: an escaped descendant still holds the pipe)".into(),
            total_bytes: usize::MAX,
        }
    }
}

/// A dropped (not waited) session must not orphan a live, paid agent: a
/// ledger-write error mid-run, or any early return in the runner, kills the
/// whole process group and reaps the child. The reader/stderr threads are
/// left to finish on EOF (they hold no resources beyond the pipes).
impl Drop for Session {
    fn drop(&mut self) {
        if self.reader.is_some() {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(test)]
mod raw_accounting_tests {
    use super::*;

    #[test]
    fn whitespace_stdout_is_counted_before_line_normalization() {
        let mut reader = BufReader::new(std::io::Cursor::new(b" \n"));
        let mut buf = Vec::new();
        let raw_bytes = AtomicUsize::new(0);
        assert_eq!(
            read_line_bounded(&mut reader, &mut buf, MAX_LINE_BYTES, &raw_bytes).unwrap(),
            2
        );
        assert_eq!(raw_bytes.load(Ordering::Relaxed), 2);
        assert!(String::from_utf8_lossy(&buf).trim().is_empty());
    }
}
