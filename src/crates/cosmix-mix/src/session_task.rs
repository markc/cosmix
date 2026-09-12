//! Isolated supervised tasks (P4) — the second §5.2 execution mode.
//!
//! An interactive evaluation runs INSIDE the shell: it sees the shell's
//! variables, its directory and its terminal, and everything about its
//! cancellation is therefore cooperative. A task is the opposite by
//! construction: a separate process, in its own group, with an enumerated
//! environment, separate stdout and stderr pipes, and a hard termination
//! policy. That is why idle-prompt admission does NOT apply here — BUSY is
//! never a task refusal, and a task submitted while the human is typing runs
//! anyway. The independence is the whole point of the mode.
//!
//! This module owns spawn and supervision only. Identity, the capability gate,
//! the dedupe store and the operation-id space stay in `session_execute`, which
//! already has all four; a second owner would duplicate every one of them.
//!
//! ## What is guaranteed, and what is not
//!
//! Termination IS hard here, and it is the arc's only hard guarantee: cancel or
//! timeout sends SIGTERM to the group, waits a declared grace, then SIGKILL,
//! and the outcome is read from `wait()` rather than from the fact that a
//! signal was sent. A cancel request is not proof a process stopped; the wait
//! status is.
//!
//! Teardown has three mechanisms because none covers the others' cases.
//! `PR_SET_PDEATHSIG` binds the task LEADER to the supervisor thread that
//! forked it, which is the only thing that survives a shell SIGKILL — no
//! teardown hook runs then. At SETTLEMENT the supervisor SIGKILLs the whole
//! group one last time, which is what reaches a child the task backgrounded and
//! pdeathsig cannot see. And `sweep()` SIGKILLs every still-running task group
//! when the shell exits by any ordinary route.
//!
//! The settlement kill is only possible because the leader is not reaped until
//! after it: a process group exists while any member does, including a zombie,
//! so holding the leader un-reaped (`waitid` with `WNOWAIT`) is what keeps the
//! group addressable long enough to clear it.
//!
//! The declared residual is therefore exactly one case: a process that leaves
//! the task's group by calling `setsid`/`setpgid` for ITSELF. That is a
//! deliberate act of daemonisation, and the manual says so rather than implying
//! a containment this does not have.

use serde::Serialize;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Concurrent tasks per shell. Beyond this, RESOURCE_LIMIT — a real limit
/// (processes and supervisor threads), not a bookkeeping one.
pub(crate) const TASKS: usize = 4;
/// Per-stream capture cap. With the 64 KiB result cap this keeps a full settled
/// record inside one Term reply under the 256 KiB envelope, with headroom —
/// which is what lets v1 ship without chunking machinery.
pub(crate) const MAX_STREAM: usize = 64 * 1024;
/// How much of the result pipe is kept. The writer caps a frame at `MAX_RESULT`
/// encoded bytes, so this is that frame plus its length prefix plus enough
/// slack to SEE an over-long frame rather than mistake a cut-off one for a
/// complete read.
const MAX_RESULT_CAPTURE: usize = MAX_STREAM + 1024;
/// Per-field budgets measured in ENCODED bytes — what the reply actually costs
/// — rather than in raw captured bytes, which is what the caps used to count.
/// Three of these plus the envelope is what must fit one 256 KiB Term reply;
/// `a_worst_case_report_fits_one_reply` does that arithmetic for real.
const MAX_STREAM_ENCODED: usize = 64 * 1024;
const MAX_RESULT_ENCODED: usize = 64 * 1024;
/// The declared grace between SIGTERM and SIGKILL, and also the deadline for
/// draining the result pipe: a grandchild holding the write end open must not
/// be able to wedge the supervisor.
const GRACE: Duration = Duration::from_secs(2);
const MAX_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_ENV_VARS: usize = 64;
const MAX_ARGV: usize = 256;
/// Byte budgets for argv and the environment overlay.
///
/// Sized to the DISPATCH request cap, not to what exec could carry. A whole
/// `shell.task.submit` body must fit 8192 bytes, so a 64 KiB argv limit was
/// unreachable decoration: the advertised RESOURCE_LIMIT could never fire and
/// the manual described a refusal no caller could provoke. These are under the
/// dispatch cap on purpose, so the limit a caller reads about is the limit that
/// actually answers.
const MAX_ENV_BYTES: usize = 4096;
const MAX_ARG_BYTES: usize = 4096;

/// The child lost the fork/prctl race and has no supervisor.
const ORPHANED_BEFORE_START: libc::c_int = 125;

/// Why a spawn failed, in the surface's OWN vocabulary.
///
/// The error set callers switch on is closed, so a spawn failure has to land on
/// a code that already exists rather than earning a new one. Which code is not
/// cosmetic: it tells a caller whether to fix the request or to wait and retry.
pub(crate) struct SpawnFailed {
    pub code: &'static str,
    pub detail: String,
}

impl SpawnFailed {
    /// Split on errno, because the two halves need different answers.
    ///
    /// A program that is not there, or a directory that stopped being one
    /// between validation and the fork, is something the CALLER named and can
    /// correct — NOT_FOUND, the same answer validation gives for a missing cwd,
    /// so the same mistake does not change its name depending on how quickly
    /// the filesystem moved. Everything else here is the box running out of
    /// something (descriptors, memory, processes): RESOURCE_LIMIT, which is
    /// already the transient class Term declines to retain, so a retry reaches
    /// the child instead of replaying the refusal forever.
    fn of(error: &std::io::Error) -> Self {
        let code = match error.raw_os_error() {
            Some(libc::ENOENT | libc::EACCES | libc::ENOTDIR | libc::ELOOP | libc::ENAMETOOLONG) => {
                "NOT_FOUND"
            }
            _ => "RESOURCE_LIMIT",
        };
        Self {
            code,
            detail: error.to_string(),
        }
    }

    /// Failures of this supervisor's own machinery — a descriptor it could not
    /// make, a thread it could not start. Never the caller's request.
    fn resources(detail: String) -> Self {
        Self {
            code: "RESOURCE_LIMIT",
            detail,
        }
    }
}

/// The environment a task starts from, snapshotted ONCE at shell startup.
///
/// Enumerated, never a glob and never the shell's live variables: a task must
/// inherit nothing it was not given, and "nothing it was not given" is only
/// checkable if the list is written down. Snapshotting at startup rather than
/// at spawn means a task sees the shell's STARTUP PATH — stated in the manual,
/// because a shell that has since changed its own PATH would otherwise hand
/// tasks something no document describes.
static BASE_ENV: std::sync::OnceLock<Vec<(String, String)>> = std::sync::OnceLock::new();

/// Names carried through from the shell's startup environment, plus the fixed
/// `TERM=dumb`. The COSMIX entries are the ones a child Mix needs to resolve
/// its own root, source and binaries; without them a source task could not find
/// its prelude.
const BASE_NAMES: &[&str] = &[
    "HOME",
    "USER",
    "PATH",
    "LANG",
    "COSMIX",
    "COSMIX_SRC",
    "COSMIX_BIN",
    "COSMIX_ETC",
    "COSMIX_NODE_CONFIG",
    "COSMIX_BROKER_ACCOUNT",
];

/// Called once from `main`, before Bus dispatch starts and therefore before any
/// user code can mutate environ. Idempotent, so the REPL's own call is a no-op
/// rather than a second, later snapshot.
pub(crate) fn capture_base_env() {
    let _ = BASE_ENV.get_or_init(|| {
        let mut base: Vec<(String, String)> = BASE_NAMES
            .iter()
            .filter_map(|name| std::env::var(name).ok().map(|v| ((*name).to_owned(), v)))
            .collect();
        // Fixed, not inherited: a task has no terminal, and a child that
        // believes it does will emit escapes into a captured pipe.
        base.push(("TERM".into(), "dumb".into()));
        base
    });
}

fn base_env() -> &'static [(String, String)] {
    BASE_ENV.get().map(Vec::as_slice).unwrap_or(&[])
}

// ------------------------------------------------------------------ requests

/// Exactly one of `source` and `argv`. Both or neither is INVALID_ARGUMENT:
/// a union that silently preferred one would be a shell-string construction
/// path by another name.
pub(crate) enum Mode {
    Source(String),
    Argv(Vec<String>),
}

pub(crate) struct Spec {
    pub mode: Mode,
    pub cwd: String,
    pub env: Vec<(String, String)>,
    pub timeout: Duration,
}

impl Spec {
    /// Every check that can be made without spawning. Returns the refusal code
    /// a caller sees, so validation and reporting cannot drift apart.
    pub(crate) fn validate(
        source: Option<String>,
        argv: Option<Vec<String>>,
        cwd: String,
        env: Vec<(String, String)>,
        timeout_ms: u64,
    ) -> Result<Self, &'static str> {
        let mode = match (source, argv) {
            (Some(source), None) if !source.trim().is_empty() => Mode::Source(source),
            (None, Some(argv)) if !argv.is_empty() => {
                if argv.len() > MAX_ARGV
                    || argv.iter().map(String::len).sum::<usize>() > MAX_ARG_BYTES
                {
                    return Err("INVALID_ARGUMENT");
                }
                // A NUL cannot survive the exec boundary; refusing beats
                // silently truncating an argument at the first zero byte.
                if argv.iter().any(|a| a.contains('\0')) {
                    return Err("INVALID_ARGUMENT");
                }
                Mode::Argv(argv)
            }
            _ => return Err("INVALID_ARGUMENT"),
        };
        if timeout_ms == 0 || Duration::from_millis(timeout_ms) > MAX_TIMEOUT {
            return Err("INVALID_ARGUMENT");
        }
        if env.len() > MAX_ENV_VARS
            || env
                .iter()
                .map(|(k, v)| k.len() + v.len())
                .sum::<usize>()
                > MAX_ENV_BYTES
        {
            return Err("RESOURCE_LIMIT");
        }
        // A duplicate name in the overlay has no defensible meaning — last
        // wins and first wins are both arbitrary — so it is refused rather
        // than resolved.
        let mut seen = std::collections::HashSet::new();
        for (name, value) in &env {
            if name.is_empty()
                || name.contains('=')
                || name.contains('\0')
                || value.contains('\0')
                || !seen.insert(name.as_str())
            {
                return Err("INVALID_ARGUMENT");
            }
        }
        // Checked here AND again by the spawn (the directory can vanish in
        // between). Both answer NOT_FOUND: the spawn failure is classified by
        // errno, and ENOENT/ENOTDIR land on the same code this returns, so a
        // caller sees one name for one mistake however fast the filesystem
        // moved underneath it. This check exists to make the common case cheap
        // and precise, not to give it a different answer.
        if !std::path::Path::new(&cwd).is_dir() {
            return Err("NOT_FOUND");
        }
        Ok(Self {
            mode,
            cwd,
            env,
            timeout: Duration::from_millis(timeout_ms),
        })
    }
    /// Base first, overlay second: the overlay WINS on a name collision,
    /// because the operator naming a variable explicitly is stating intent
    /// about that variable.
    fn environment(&self) -> Vec<(String, String)> {
        let mut env: Vec<(String, String)> = base_env().to_vec();
        for (name, value) in &self.env {
            if let Some(existing) = env.iter_mut().find(|(n, _)| n == name) {
                existing.1 = value.clone();
            } else {
                env.push((name.clone(), value.clone()));
            }
        }
        env
    }
}

// ------------------------------------------------------------------- reports

/// How the task ended, as distinct kinds rather than one overloaded code.
///
/// A timeout is POLICY, not a guess: the supervisor decided, and the report
/// says so instead of presenting the resulting SIGTERM as if the task had been
/// signalled by something else.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Outcome {
    Exited {
        code: i32,
    },
    /// Something outside this supervisor ended the task. The supervisor's own
    /// escalations never arrive here — they are reported as the POLICY that
    /// chose them, with the signal named in `escalated_to`.
    Signalled {
        signal: i32,
    },
    /// The supervisor's deadline fired. `escalated_to` names how far the
    /// ladder had to go; `wait` carries what the status actually said, so
    /// naming the policy never costs the caller the underlying fact.
    Timeout {
        escalated_to: &'static str,
        wait: WaitFacts,
    },
    Cancelled {
        escalated_to: &'static str,
        wait: WaitFacts,
    },
    /// Waited and got no status we can interpret.
    Unknown {
        detail: String,
    },
}

/// What `wait()` said, reported beside a policy outcome rather than instead of
/// it. A task that was already exiting when the deadline fired has a real exit
/// code, and discarding it would make the policy name the only evidence.
#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct WaitFacts {
    pub signal: Option<i32>,
    pub code: Option<i32>,
    /// No status at all — the group was gone before the ladder could read one.
    pub reaped: bool,
}

impl WaitFacts {
    fn of(status: Option<std::process::ExitStatus>) -> Self {
        use std::os::unix::process::ExitStatusExt;
        match status {
            None => Self {
                signal: None,
                code: None,
                reaped: false,
            },
            Some(status) => Self {
                signal: status.signal(),
                code: status.code(),
                reaped: true,
            },
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Stream {
    pub bytes: cosmix_lib_bus::native_session::DecimalU64,
    pub truncated: bool,
    /// The drain deadline fired with the pipe still open: something the task
    /// left behind outlived it and kept writing. Distinct from `truncated`,
    /// which is about the cap, and load-bearing — it is the difference between
    /// "this is all of it" and "this is all we waited for".
    pub writer_survived: bool,
    pub text: String,
}

/// The framed structured value, or why there is not one.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum TaskResult {
    /// argv mode has no interpreter value by construction, and says so rather
    /// than presenting an absent one as a failure.
    NotApplicable,
    Value { data: String },
    /// Nothing was written to the result descriptor at all.
    ResultMissing,
    /// A length prefix the payload did not satisfy — the writer was killed
    /// mid-frame. Distinct from truncation, which is a complete frame saying
    /// the value was too big.
    ResultTorn,
    /// The drain deadline fired while a survivor still held the write end, and
    /// what had arrived was not yet a whole frame. Not the same event as a torn
    /// frame: nobody was killed mid-write, we simply stopped waiting.
    ResultAbandoned,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct TaskReport {
    pub version: u8,
    pub outcome: Outcome,
    pub stdout: Stream,
    pub stderr: Stream,
    pub result: TaskResult,
    pub duration_ms: cosmix_lib_bus::native_session::DecimalU64,
}

// ---------------------------------------------------------------- supervision

enum Event {
    Exited(std::process::ExitStatus),
    /// `wait()` itself failed. A distinct event, because fabricating a status
    /// here would report a task we know nothing about as a clean exit 0.
    WaitFailed(String),
    Cancel,
}

/// Every live task group, so shell teardown can end them.
static LIVE_GROUPS: Mutex<Vec<libc::pid_t>> = Mutex::new(Vec::new());

fn groups() -> std::sync::MutexGuard<'static, Vec<libc::pid_t>> {
    LIVE_GROUPS.lock().unwrap_or_else(|held| held.into_inner())
}

/// SIGKILL every live task group. Called on the way out of the shell.
///
/// PDEATHSIG covers the LEADER, and only when the supervisor thread dies, so a
/// healthy shell exiting normally would otherwise leave a task's own children
/// running with nothing supervising them. Deliberately SIGKILL and deliberately
/// not waited on: teardown is not the place to grant a grace a departing shell
/// cannot supervise.
pub(crate) fn sweep() {
    for pid in std::mem::take(&mut *groups()) {
        signal_group(pid, libc::SIGKILL);
    }
}

/// Handle held by the surface so a cancel can reach a running supervisor.
pub(crate) struct Handle {
    cancel: mpsc::Sender<Event>,
    pub started: Instant,
    pub cancelling: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
impl Handle {
    /// Idempotent. The supervisor drains duplicates; a second cancel on an
    /// already-settling task changes nothing.
    pub(crate) fn cancel(&self) {
        self.cancelling
            .store(true, std::sync::atomic::Ordering::Release);
        let _ = self.cancel.send(Event::Cancel);
    }
}

/// Spawn and supervise. Returns the handle immediately; `settled` is called on
/// the supervisor thread once `wait()` has spoken.
pub(crate) fn spawn(
    spec: Spec,
    settled: impl FnOnce(TaskReport) + Send + 'static,
) -> Result<Handle, SpawnFailed> {
    let started = Instant::now();
    let (tx, rx) = mpsc::channel();
    let cancel = tx.clone();
    let cancelling = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let supervised = cancelling.clone();
    // The fork MUST happen on the supervisor thread. PR_SET_PDEATHSIG binds the
    // leader to the THREAD that created it, so forking here — on the Bus
    // dispatch thread — would tie every task's life to the pump instead: a
    // broker outage or an identity rejection that ends the pump would have the
    // kernel SIGKILL live tasks mid-timeout, reported as an external signal
    // nobody sent. The caller still gets a synchronous accepted/refused answer;
    // it waits for the spawn, not for the task.
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), SpawnFailed>>(1);
    std::thread::Builder::new()
        .name("mix-task".into())
        .spawn(move || supervise_task(spec, started, tx, rx, supervised, ready_tx, settled))
        .map_err(|error| SpawnFailed::resources(error.to_string()))?;
    ready_rx
        .recv()
        .map_err(|_| {
            SpawnFailed::resources("the supervisor thread ended before it spawned the task".into())
        })??;
    Ok(Handle {
        cancel,
        started,
        cancelling,
    })
}

/// Fork, wait, drain and report — all on the one thread, which is what makes
/// the pdeathsig binding above mean what its comment says.
#[allow(clippy::too_many_arguments)]
fn supervise_task(
    spec: Spec,
    started: Instant,
    waiter_tx: mpsc::Sender<Event>,
    rx: mpsc::Receiver<Event>,
    cancelling: Arc<std::sync::atomic::AtomicBool>,
    ready: mpsc::SyncSender<Result<(), SpawnFailed>>,
    settled: impl FnOnce(TaskReport) + Send + 'static,
) {
    let (result_read, result_write) = match pipe() {
        Ok(pair) => pair,
        Err(error) => {
            let _ = ready.send(Err(SpawnFailed::resources(error)));
            return;
        }
    };
    let mut command = match &spec.mode {
        Mode::Source(source) => {
            let mut command = Command::new(interpreter());
            // Flags BEFORE -c: `-c` consumes the remainder as script argv, so
            // a trailing --result-fd would be an argument, not a flag.
            command.arg("--result-fd").arg(TASK_RESULT_FD.to_string());
            command.arg("-c").arg(source);
            command
        }
        Mode::Argv(argv) => {
            let mut command = Command::new(&argv[0]);
            command.args(&argv[1..]);
            command
        }
    };
    command
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(spec.environment())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let parent = std::process::id() as libc::pid_t;
    let write_fd = result_write.as_raw_fd();
    let wants_result = matches!(spec.mode, Mode::Source(_));
    // SAFETY: everything below is async-signal-safe — setsid, prctl, getppid,
    // dup2, sigprocmask, _exit. No allocation, no locks, no Rust runtime.
    unsafe {
        command.pre_exec(move || {
            // Own process group FIRST: every later signal is aimed at the
            // group, and a task still in the shell's group would take the
            // shell's Ctrl-C with it.
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Bind the leader's life to the supervising THREAD. That is the
            // desirable binding: it covers supervisor-thread death and whole-
            // shell death including SIGKILL, which no teardown hook can catch.
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // The classic fork/prctl race: if the parent died between fork and
            // the prctl above, the signal has already been missed and this
            // child would outlive its supervisor forever.
            if libc::getppid() != parent {
                // Deliberately NOT 127: that is the shell's "command not
                // found", and this child found its command perfectly well.
                // A distinctive code makes a lost race legible in the report
                // instead of looking like a typo in the argv.
                libc::_exit(ORPHANED_BEFORE_START);
            }
            // std resets signal HANDLERS across exec but not the MASK. An
            // inherited full mask would make the SIGTERM grace a no-op and turn
            // every cancellation into a SIGKILL.
            let mut empty: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut empty);
            if libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut()) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if wants_result {
                if libc::dup2(write_fd, TASK_RESULT_FD) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // Clear CLOEXEC on the duplicate so it survives exec — the
                // duplicate, never the original, so nothing else leaks.
                if libc::fcntl(TASK_RESULT_FD, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = ready.send(Err(SpawnFailed::of(&error)));
            return;
        }
    };
    // The parent's copy of the write end must close, or the read below never
    // sees EOF and the drain waits for a descriptor nobody will write to.
    drop(result_write);
    let pid = child.id() as libc::pid_t;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // A live task exists from here on, so every remaining failure path must
    // kill its group before refusing — a refusal that left a process running
    // would be a lie the caller cannot even see.
    let stop = match DrainStop::new() {
        Ok(stop) => Arc::new(stop),
        Err(error) => {
            signal_group(pid, libc::SIGKILL);
            // Reap, or the refusal leaves a zombie nothing will ever collect.
            let _ = child.wait();
            let _ = ready.send(Err(SpawnFailed::resources(error)));
            return;
        }
    };
    // Drains run CONCURRENTLY with the wait, never before or after it. A 64 KiB
    // pipe buffer against a larger output is a deadlock, and a deadlock here
    // would be reported as a timeout — a wrong answer that looks like a
    // policy decision.
    let drains = (|| -> std::io::Result<(Option<Drain>, Option<Drain>, Option<Drain>)> {
        Ok((
            stdout.map(|s| drain_fd(s, MAX_STREAM, &stop)).transpose()?,
            stderr.map(|s| drain_fd(s, MAX_STREAM, &stop)).transpose()?,
            wants_result
                .then(|| drain_fd(result_read, MAX_RESULT_CAPTURE, &stop))
                .transpose()?,
        ))
    })();
    let (out_drain, err_drain, result_drain) = match drains {
        Ok(drains) => drains,
        Err(error) => {
            signal_group(pid, libc::SIGKILL);
            let _ = child.wait();
            let _ = ready.send(Err(SpawnFailed::resources(error.to_string())));
            return;
        }
    };

    // The waiter OBSERVES the status without reaping it (WNOWAIT), so the
    // leader stays a zombie until this thread says otherwise. That zombie is
    // what keeps the process GROUP alive: a pgid exists while any member does,
    // including a dead one nobody has collected. Reaping here instead — which
    // is what `child.wait()` did — retired the pgid the instant the leader
    // died, so a plain backgrounded child (`sh -c "sleep 600 &"`, still in the
    // group) escaped pdeathsig (its parent was gone) AND the exit sweep (the
    // pid was deregistered), which is exactly the containment the manual
    // claimed and did not have.
    if let Err(error) = std::thread::Builder::new()
        .name("mix-task-wait".into())
        .spawn(move || {
            let _ = waiter_tx.send(observe(pid));
        })
    {
        signal_group(pid, libc::SIGKILL);
        let _ = child.wait();
        let _ = ready.send(Err(SpawnFailed::resources(error.to_string())));
        return;
    }

    groups().push(pid);
    let _ = ready.send(Ok(()));

    let outcome = supervise(&rx, pid, spec.timeout, &cancelling);
    // The outcome is settled, so nothing may wait on the task's leftovers any
    // longer than the declared grace. This is what stops a backgrounded
    // survivor holding the report open for the rest of the shell's life.
    stop.release();
    let stdout = out_drain.map(join_stream).unwrap_or_else(empty_stream);
    let stderr = err_drain.map(join_stream).unwrap_or_else(empty_stream);
    let result = match result_drain {
        None => TaskResult::NotApplicable,
        Some(handle) => {
            let capture = handle.join().unwrap_or_default();
            match (decode_frame(&capture.kept), capture.writer_survived) {
                // A whole frame is a whole frame however the drain ended. Its
                // own encoded budget still applies: the frame is already
                // escaped once, and carrying it in the reply escapes it again.
                (TaskResult::Value { data }, _) => TaskResult::Value {
                    data: within_budget(data),
                },
                (_, true) => TaskResult::ResultAbandoned,
                (other, false) => other,
            }
        }
    };
    // Containment, at the last moment it can still be done. The leader is a
    // zombie and the pgid is therefore still valid, so this reaches every
    // member the task left behind — the backgrounded child that pdeathsig
    // cannot see. Only AFTER it does the leader get reaped and the group
    // deregistered, which is also what makes every killpg in this file
    // recycle-safe: the pid it names cannot be reissued while we hold it.
    signal_group(pid, libc::SIGKILL);
    // SAFETY: reaping a child of this process, once, after the final signal.
    unsafe {
        let mut ignored: libc::c_int = 0;
        libc::waitpid(pid, &mut ignored, 0);
    }
    groups().retain(|live| *live != pid);
    settled(TaskReport {
        version: 1,
        outcome,
        stdout,
        stderr,
        result,
        duration_ms: cosmix_lib_bus::native_session::DecimalU64(
            started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        ),
    });
}

/// Which `mix` evaluates a source task.
///
/// THIS one, normally: the interpreter a caller is talking to should be the one
/// that runs its task. Deriving a path from the install layout instead asks
/// where a mix OUGHT to be, which fails for any shell not running from
/// `$COSMIX/bin` — a dev build, a test harness, a relocated tree — with
/// NOT_FOUND for a binary that is demonstrably running. Where the derived path
/// happens to exist it is worse than a refusal: the task is evaluated by a
/// DIFFERENT build than the shell, silently.
///
/// But `current_exe` reads `/proc/self/exe`, which names the ORIGINAL inode and
/// is suffixed " (deleted)" once that inode is unlinked. A mesh deploy replaces
/// `/opt/cosmix/bin/mix` under long-lived shells, and BOTH shapes of replace do
/// unlink it — measured, not assumed: `rm`-then-write and an atomic
/// rename-over each produce `…/mix (deleted)`, with the on-disk inode differing
/// from the running one. Re-exec'ing that path would fail, or worse resolve to
/// something else entirely. So a marked or unresolvable answer falls back to
/// the installed path, and the stated limit is narrow and true: a shell whose
/// binary was replaced under it runs source tasks with the INSTALLED build, not
/// with its own image.
fn interpreter() -> std::path::PathBuf {
    interpreter_from(std::env::current_exe())
}

/// Split out from `interpreter` so the replaced-binary case can be tested.
/// It cannot be reached otherwise: it needs a deploy to unlink a running
/// image, and a fixture cannot ask `current_exe` for a different answer.
fn interpreter_from(exe: std::io::Result<std::path::PathBuf>) -> std::path::PathBuf {
    use std::os::unix::ffi::OsStrExt;
    let installed =
        || crate::cosmix_paths::cosmix_path(crate::cosmix_paths::CosmixDir::Bin).join("mix");
    let Ok(path) = exe else {
        eprintln!("mix: cannot read this process's own path; a source task will use the install");
        return installed();
    };
    if let Some(stripped) = path.as_os_str().as_bytes().strip_suffix(b" (deleted)") {
        // The replacement landed at EXACTLY this path — that is what the
        // deploy did — so stripping the marker names the new binary, whatever
        // shape the deploy took. Deriving an install path instead would be
        // wrong twice over: with no checkout above it, the resolver answers
        // `~/.local/bin` (or `/usr/local/bin` as root), not `/opt/cosmix/bin`,
        // so on a canonical fleet host it names a file that does not exist —
        // and on a developer's host it names their dev build, resurrecting the
        // silently-wrong-interpreter hazard this whole function exists to kill.
        //
        // If the binary was deleted rather than replaced, this path does not
        // exist and the spawn refuses NOT_FOUND. That is the honest answer:
        // there is no interpreter to run the task.
        let replaced = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(stripped));
        eprintln!(
            "mix: this shell's binary was replaced underneath it; a source task \
             will run {}",
            replaced.display()
        );
        return replaced;
    }
    if path.is_file() {
        return path;
    }
    // Last ditch: the path is neither marked nor resolvable, which is not a
    // shape any deploy produces. Nothing better to offer than the install.
    eprintln!(
        "mix: {} is not a file; a source task will use the install",
        path.display()
    );
    installed()
}

/// The descriptor the child sees its result channel on. Above stderr, fixed so
/// the flag value and the dup2 target cannot disagree.
const TASK_RESULT_FD: RawFd = 3;

/// The escalation ladder, driven entirely by events: one blocking wait on the
/// channel with a deadline. No polling, no `try_wait` loop.
fn supervise(
    rx: &mpsc::Receiver<Event>,
    pid: libc::pid_t,
    timeout: Duration,
    cancelling: &std::sync::atomic::AtomicBool,
) -> Outcome {
    let reason = match rx.recv_timeout(timeout) {
        Ok(Event::Exited(status)) => return natural(status),
        Ok(Event::WaitFailed(detail)) => return Outcome::Unknown { detail },
        Ok(Event::Cancel) => Reason::Cancelled,
        Err(mpsc::RecvTimeoutError::Timeout) => Reason::Timeout,
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return Outcome::Unknown {
                detail: "the waiter thread ended without a status".into(),
            };
        }
    };
    // Only a cancel is a cancellation. The timeout path used to set this too,
    // which made an uncancelled task report "cancelling" to anyone polling.
    if matches!(reason, Reason::Cancelled) {
        cancelling.store(true, std::sync::atomic::Ordering::Release);
    }
    // The task may have exited between the cancel being queued and this dequeue
    // — or in the instant the deadline expired. A status that already exists is
    // the truth: signalling first and reporting policy would throw away the
    // exit code the contract promises comes from wait(), and would aim a killpg
    // at a pid the kernel is free to have recycled.
    if let Some(settled) = already_settled(rx) {
        return settled;
    }
    // Declared policy, in order: TERM to the GROUP, a stated grace, then KILL.
    if !signal_group(pid, libc::SIGTERM) {
        // ESRCH: the group is already gone, so its status is on its way rather
        // than something this ladder produced.
        return match settle_within(rx, GRACE) {
            Some(Ok(status)) => natural(status),
            Some(Err(detail)) => Outcome::Unknown { detail },
            None => Outcome::Unknown {
                detail: "the task group was already gone and no status followed".into(),
            },
        };
    }
    match settle_within(rx, GRACE) {
        Some(Ok(status)) => return reason.into_outcome("sigterm", Some(status)),
        Some(Err(detail)) => return Outcome::Unknown { detail },
        None => {}
    }
    signal_group(pid, libc::SIGKILL);
    // SIGKILL cannot be caught, so this wait is bounded in practice; the
    // deadline is belt-and-braces against an unkillable D-state.
    match settle_within(rx, GRACE) {
        Some(Ok(status)) => reason.into_outcome("sigkill", Some(status)),
        Some(Err(detail)) => Outcome::Unknown { detail },
        None => Outcome::Unknown {
            detail: "the group did not reap after SIGKILL".into(),
        },
    }
}

/// A status already queued behind the event we just dequeued.
fn already_settled(rx: &mpsc::Receiver<Event>) -> Option<Outcome> {
    loop {
        match rx.try_recv() {
            Ok(Event::Exited(status)) => return Some(natural(status)),
            Ok(Event::WaitFailed(detail)) => return Some(Outcome::Unknown { detail }),
            Ok(Event::Cancel) => continue,
            Err(_) => return None,
        }
    }
}

enum Reason {
    Timeout,
    Cancelled,
}
impl Reason {
    /// The outcome names the POLICY that ended the task, not the signal the
    /// policy happened to use — the signal is reported beside it as
    /// `escalated_to`, and the wait status beside that. Reporting a timeout as
    /// "signalled: SIGTERM" would make a deliberate deadline indistinguishable
    /// from an external kill; dropping the status would make the policy name
    /// the only surviving evidence.
    fn into_outcome(
        self,
        escalated_to: &'static str,
        status: Option<std::process::ExitStatus>,
    ) -> Outcome {
        let wait = WaitFacts::of(status);
        match self {
            Self::Timeout => Outcome::Timeout { escalated_to, wait },
            Self::Cancelled => Outcome::Cancelled { escalated_to, wait },
        }
    }
}

/// Drain any duplicate cancels while waiting for the real settlement.
type Settlement = Result<std::process::ExitStatus, String>;

fn settle_within(rx: &mpsc::Receiver<Event>, budget: Duration) -> Option<Settlement> {
    let deadline = Instant::now() + budget;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(Event::Exited(status)) => return Some(Ok(status)),
            Ok(Event::WaitFailed(detail)) => return Some(Err(detail)),
            Ok(Event::Cancel) => continue,
            Err(_) => return None,
        }
    }
}

/// Wait for the leader to exit and read its status WITHOUT collecting it.
///
/// `WNOWAIT` is the whole point: the status is readable and the leader stays a
/// zombie, so its process group keeps existing until the supervisor has
/// finished with it. Everything that makes the group signallable at settlement
/// — and every killpg in this file safe against pid recycling — rests on that.
fn observe(pid: libc::pid_t) -> Event {
    // SAFETY: waitid on a child of this process, into a zeroed siginfo_t.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        loop {
            let waited = libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            );
            if waited == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Event::WaitFailed(error.to_string());
        }
        // Rebuild the wait-status word the rest of this file speaks in. The
        // shifts are the inverse of WEXITSTATUS/WTERMSIG, which is the only
        // reason this is arithmetic rather than a library call.
        let status = info.si_status();
        let raw = match info.si_code {
            libc::CLD_EXITED => (status & 0xff) << 8,
            libc::CLD_DUMPED => (status & 0x7f) | 0x80,
            // CLD_KILLED, and anything else that can end a process.
            _ => status & 0x7f,
        };
        Event::Exited(
            <std::process::ExitStatus as std::os::unix::process::ExitStatusExt>::from_raw(raw),
        )
    }
}

fn natural(status: std::process::ExitStatus) -> Outcome {
    use std::os::unix::process::ExitStatusExt;
    if let Some(signal) = status.signal() {
        Outcome::Signalled { signal }
    } else if let Some(code) = status.code() {
        Outcome::Exited { code }
    } else {
        Outcome::Unknown {
            detail: "wait returned neither a code nor a signal".into(),
        }
    }
}

/// False means ESRCH — there is no such group, so there is nothing this signal
/// could have done. The caller needs that answer: "I signalled it" and "it was
/// already gone" lead to different reports.
///
/// There is no pid-recycle window here, and that is bought rather than assumed:
/// the waiter observes the leader's status with `WNOWAIT` and nothing reaps it
/// until `supervise_task` has sent its last signal, so the pid this names is
/// held by a zombie of ours at every call site and cannot have been reissued to
/// anyone else.
fn signal_group(pid: libc::pid_t, signal: libc::c_int) -> bool {
    // The GROUP, because the task is a session leader and its own children are
    // the reason a leader-only signal would leave work running.
    // SAFETY: a plain signal to a group this supervisor created.
    unsafe { libc::killpg(pid, signal) == 0 }
}

// ---------------------------------------------------------------- capture

/// What a drain came back with, and how it ended.
#[derive(Default)]
struct Capture {
    kept: Vec<u8>,
    total: usize,
    /// The deadline fired while the pipe was still open.
    writer_survived: bool,
}

type Drain = std::thread::JoinHandle<Capture>;

/// The one wake-up shared by all three drains.
///
/// An eventfd rather than a flag, because a drain must be able to block
/// indefinitely on its pipe — a task can be silent for its whole timeout — and
/// still be woken the moment the supervisor has an outcome. A flag would need a
/// timer to notice it, which is the sleep-loop polling the no-poll law bans.
struct DrainStop(OwnedFd);

impl DrainStop {
    fn new() -> Result<Self, String> {
        // SAFETY: a fresh eventfd; the descriptor is owned from here.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        // SAFETY: fd is fresh, valid and unowned elsewhere.
        Ok(Self(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    /// Start every drain's deadline. Level-triggered and never read, so a drain
    /// that arrives late still sees it.
    fn release(&self) {
        let one: u64 = 1;
        // SAFETY: an 8-byte write of a u64 to an owned eventfd, as its
        // interface requires.
        unsafe {
            libc::write(self.0.as_raw_fd(), std::ptr::addr_of!(one).cast(), 8);
        }
    }
}

/// Read until EOF, or until GRACE after the supervisor releases — whichever
/// comes first.
///
/// The deadline is the whole point. Without it a task that backgrounds a
/// long-lived child (`sh -c "sleep 600 &"`) hands that child the inherited
/// stdout, stderr and result descriptors; the task itself exits, the supervisor
/// has its outcome, and the report still cannot be assembled because these
/// reads would sit on descriptors the survivor holds open. The record would pin
/// at "running" forever and its TASKS slot would never come back.
fn drain_fd<S: AsRawFd + Send + 'static>(
    source: S,
    cap: usize,
    stop: &Arc<DrainStop>,
) -> std::io::Result<Drain> {
    let stop = stop.clone();
    // Builder, not the bare spawn: thread exhaustion is a condition this
    // supervisor can report, and panicking the caller over it would take down a
    // healthy shell because one task could not get a thread.
    std::thread::Builder::new().name("mix-task-drain".into()).spawn(move || {
        let fd = source.as_raw_fd();
        // Non-blocking, so a readiness that evaporates cannot park this thread
        // inside read() past its own deadline.
        // SAFETY: fd is owned by `source` for the life of this thread.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags >= 0 {
                libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
        }
        let mut capture = Capture::default();
        let mut chunk = [0u8; 8192];
        let mut deadline: Option<Instant> = None;
        capture.writer_survived = loop {
            let wait_ms = match deadline {
                None => -1,
                Some(at) => {
                    let left = at.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        break true;
                    }
                    left.as_millis().min(i32::MAX as u128) as i32
                }
            };
            let mut fds = [
                libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    // Once the deadline is running the stop is permanently
                    // readable, so watching it further would spin. poll(2)
                    // ignores a negative descriptor.
                    fd: if deadline.is_none() {
                        stop.0.as_raw_fd()
                    } else {
                        -1
                    },
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: a well-formed two-entry array of owned descriptors.
            let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, wait_ms) };
            if ready < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                break false;
            }
            if ready == 0 {
                break true;
            }
            if fds[1].revents != 0 && deadline.is_none() {
                deadline = Some(Instant::now() + GRACE);
            }
            if fds[0].revents == 0 {
                continue;
            }
            // Empty the pipe on one readiness rather than paying a poll per
            // 8 KiB of a chatty task — but not past the deadline. A survivor
            // writing continuously always leaves more to read, so without this
            // check the inner loop never returns to the outer one and GRACE
            // becomes unbounded for exactly the case it exists to bound.
            let ended = loop {
                if deadline.is_some_and(|at| at <= Instant::now()) {
                    break false;
                }
                // SAFETY: reading into a local buffer from an owned descriptor.
                let n = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
                if n == 0 {
                    break true;
                }
                if n < 0 {
                    match std::io::Error::last_os_error().kind() {
                        std::io::ErrorKind::Interrupted => continue,
                        std::io::ErrorKind::WouldBlock => break false,
                        _ => break true,
                    }
                }
                let n = n as usize;
                capture.total += n;
                // Read past the cap rather than stopping: leaving bytes in the
                // pipe would block the writer, and a blocked writer never
                // exits, which the supervisor would report as a timeout.
                // Truncation is about what is KEPT.
                if capture.kept.len() < cap {
                    let room = cap - capture.kept.len();
                    capture.kept.extend_from_slice(&chunk[..n.min(room)]);
                }
            };
            if ended {
                break false;
            }
        };
        // Closing the read end here SIGPIPEs a survivor still writing, rather
        // than leaving it blocked forever on a pipe nobody is reading.
        drop(source);
        capture
    })
}

fn join_stream(handle: Drain) -> Stream {
    let capture = handle.join().unwrap_or_default();
    let lossy = String::from_utf8_lossy(&capture.kept);
    let (text, trimmed) = fit_encoded(&lossy, MAX_STREAM_ENCODED);
    Stream {
        bytes: cosmix_lib_bus::native_session::DecimalU64(capture.total as u64),
        truncated: trimmed || capture.total > capture.kept.len(),
        writer_survived: capture.writer_survived,
        text,
    }
}

fn empty_stream() -> Stream {
    Stream {
        bytes: cosmix_lib_bus::native_session::DecimalU64(0),
        truncated: false,
        writer_survived: false,
        text: String::new(),
    }
}

/// What one character costs once JSON has escaped it.
///
/// This is the arithmetic the raw caps got wrong. `serde_json` turns a NUL into
/// the six bytes ` `, and `from_utf8_lossy` has already turned each invalid
/// byte into a three-byte replacement character — so a 64 KiB cap counted in RAW
/// bytes admits a 384 KiB field, and three of those overflow the 256 KiB reply
/// the surface can actually deliver.
fn encoded_cost(ch: char) -> usize {
    match ch {
        '"' | '\\' | '\n' | '\r' | '\t' => 2,
        // The rest of C0 has no short form: \u00XX.
        c if (c as u32) < 0x20 => 6,
        c => c.len_utf8(),
    }
}

/// A complete frame's value, kept whole or replaced WHOLESALE by a reference.
///
/// Never cut. The frame's payload is strict data, and a strict-data document
/// sliced at an arbitrary character is not a smaller document — it is an
/// unparseable fragment that still presents itself as the value. The writer
/// already faced this and answered it the same way, so the shape a caller sees
/// for "too big" is the same whichever side decided it.
///
/// This is a real case, not a theoretical one: a list of a few thousand short
/// strings encodes to ~36 KiB inside a complete 50 KiB frame whose JSON cost is
/// ~79 KiB, and trimming that to the budget cuts ~13 KiB off the end, mid-token.
fn within_budget(data: String) -> String {
    let (fitted, trimmed) = fit_encoded(&data, MAX_RESULT_ENCODED);
    if !trimmed {
        return fitted;
    }
    serde_json::json!({
        "ok": true,
        "truncated": true,
        "bytes": data.len().to_string(),
    })
    .to_string()
}

/// Truncate on a character boundary so the ENCODED form fits `budget`.
fn fit_encoded(text: &str, budget: usize) -> (String, bool) {
    // The quotes serde will add around it.
    let mut used = 2usize;
    for (index, ch) in text.char_indices() {
        let cost = encoded_cost(ch);
        if used + cost > budget {
            return (text[..index].to_owned(), true);
        }
        used += cost;
    }
    (text.to_owned(), false)
}

/// Three outcomes the length prefix makes distinguishable, and which an
/// unframed stream would collapse into one.
pub(crate) fn decode_frame(bytes: &[u8]) -> TaskResult {
    if bytes.is_empty() {
        return TaskResult::ResultMissing;
    }
    if bytes.len() < 4 {
        return TaskResult::ResultTorn;
    }
    let declared = u32::from_be_bytes(bytes[..4].try_into().expect("four bytes")) as usize;
    let payload = &bytes[4..];
    if payload.len() < declared {
        return TaskResult::ResultTorn;
    }
    match std::str::from_utf8(&payload[..declared]) {
        Ok(data) => TaskResult::Value { data: data.into() },
        Err(_) => TaskResult::ResultTorn,
    }
}

fn pipe() -> Result<(std::fs::File, std::fs::File), String> {
    let mut fds = [0 as libc::c_int; 2];
    // CLOEXEC by default so the pair does not leak into unrelated spawns; the
    // child's copy is created deliberately by the dup2 above.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    // SAFETY: both descriptors are fresh and owned from here.
    unsafe {
        Ok((
            std::fs::File::from_raw_fd(fds[0]),
            std::fs::File::from_raw_fd(fds[1]),
        ))
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mode_union_admits_exactly_one_side() {
        let dir = std::env::temp_dir().display().to_string();
        let ok = Spec::validate(Some("1+1".into()), None, dir.clone(), vec![], 1000);
        assert!(ok.is_ok());
        let ok = Spec::validate(None, Some(vec!["true".into()]), dir.clone(), vec![], 1000);
        assert!(ok.is_ok());
        // Both, neither, and empty-on-the-present-side are all the same
        // refusal: a union that picked a winner would be building a shell
        // string by another name.
        for (source, argv) in [
            (Some("1".into()), Some(vec!["true".into()])),
            (None, None),
            (Some("   ".into()), None),
            (None, Some(vec![])),
        ] {
            assert_eq!(
                Spec::validate(source, argv, dir.clone(), vec![], 1000).err(),
                Some("INVALID_ARGUMENT")
            );
        }
    }

    #[test]
    fn a_timeout_is_required_and_capped_and_zero_is_refused() {
        let dir = std::env::temp_dir().display().to_string();
        for bad in [0, MAX_TIMEOUT.as_millis() as u64 + 1] {
            assert_eq!(
                Spec::validate(Some("1".into()), None, dir.clone(), vec![], bad).err(),
                Some("INVALID_ARGUMENT")
            );
        }
    }

    #[test]
    fn a_missing_cwd_is_refused_before_any_spawn() {
        assert_eq!(
            Spec::validate(
                Some("1".into()),
                None,
                "/nonexistent/for/p4".into(),
                vec![],
                1000
            )
            .err(),
            Some("NOT_FOUND")
        );
    }

    #[test]
    fn the_overlay_is_bounded_and_refuses_duplicate_names() {
        let dir = std::env::temp_dir().display().to_string();
        let duplicate = vec![("A".into(), "1".into()), ("A".into(), "2".into())];
        assert_eq!(
            Spec::validate(Some("1".into()), None, dir.clone(), duplicate, 1000).err(),
            Some("INVALID_ARGUMENT")
        );
        let too_many: Vec<(String, String)> = (0..MAX_ENV_VARS + 1)
            .map(|i| (format!("V{i}"), "x".into()))
            .collect();
        assert_eq!(
            Spec::validate(Some("1".into()), None, dir.clone(), too_many, 1000).err(),
            Some("RESOURCE_LIMIT")
        );
        // A name carrying '=' or NUL cannot survive the exec boundary.
        for bad in ["", "A=B", "A\0B"] {
            assert_eq!(
                Spec::validate(
                    Some("1".into()),
                    None,
                    dir.clone(),
                    vec![(bad.into(), "x".into())],
                    1000
                )
                .err(),
                Some("INVALID_ARGUMENT")
            );
        }
    }

    #[test]
    fn the_overlay_wins_over_the_base_and_the_base_is_enumerated() {
        capture_base_env();
        let spec = Spec::validate(
            Some("1".into()),
            None,
            std::env::temp_dir().display().to_string(),
            vec![("PATH".into(), "/overridden".into()), ("NEW".into(), "1".into())],
            1000,
        )
        .unwrap();
        let env = spec.environment();
        assert_eq!(
            env.iter().filter(|(n, _)| n == "PATH").count(),
            1,
            "the overlay must replace the base entry, not shadow it with a second"
        );
        assert_eq!(
            env.iter().find(|(n, _)| n == "PATH").unwrap().1,
            "/overridden"
        );
        assert!(env.iter().any(|(n, v)| n == "TERM" && v == "dumb"));
        assert!(env.iter().any(|(n, _)| n == "NEW"));
        // Nothing outside the enumerated base plus the overlay.
        for (name, _) in &env {
            assert!(
                BASE_NAMES.contains(&name.as_str()) || name == "TERM" || name == "NEW",
                "{name} leaked into a task environment"
            );
        }
    }

    /// The three cases the length prefix exists to separate.
    #[test]
    fn framing_distinguishes_missing_torn_and_complete() {
        assert!(matches!(decode_frame(&[]), TaskResult::ResultMissing));
        assert!(matches!(decode_frame(&[0, 0]), TaskResult::ResultTorn));
        // A declared length the payload does not satisfy: killed mid-frame.
        let mut torn = 99u32.to_be_bytes().to_vec();
        torn.extend_from_slice(b"short");
        assert!(matches!(decode_frame(&torn), TaskResult::ResultTorn));
        let mut whole = 2u32.to_be_bytes().to_vec();
        whole.extend_from_slice(b"hi");
        assert!(matches!(
            decode_frame(&whole),
            TaskResult::Value { ref data } if data == "hi"
        ));
    }

    /// The caps must bound what the reply COSTS, not what was read.
    #[test]
    fn the_budget_counts_escaped_bytes() {
        // A NUL is one raw byte and six encoded ones. Budgeting raw was how a
        // 64 KiB cap admitted a 384 KiB field.
        let (text, trimmed) = fit_encoded(&"\0".repeat(MAX_STREAM), MAX_STREAM_ENCODED);
        assert!(trimmed, "a NUL-filled capture must be trimmed");
        assert!(
            serde_json::to_string(&text).expect("a string encodes").len() <= MAX_STREAM_ENCODED,
            "encoded {} exceeds the budget",
            serde_json::to_string(&text).expect("a string encodes").len()
        );
        // Truncation is on a character boundary, not a byte one.
        let (text, trimmed) = fit_encoded(&"é".repeat(MAX_STREAM), 1024);
        assert!(trimmed);
        assert!(text.chars().all(|c| c == 'é'));
        // Text that already fits is returned whole and unflagged.
        let (text, trimmed) = fit_encoded("plain", MAX_STREAM_ENCODED);
        assert_eq!((text.as_str(), trimmed), ("plain", false));
    }

    /// A replaced binary resolves to the REPLACEMENT, not to a derived install.
    #[test]
    fn a_replaced_binary_runs_the_file_that_replaced_it() {
        let deleted = std::path::PathBuf::from("/opt/cosmix/bin/mix (deleted)");
        assert_eq!(
            interpreter_from(Ok(deleted)),
            std::path::PathBuf::from("/opt/cosmix/bin/mix"),
            "the marker must be stripped, naming what the deploy wrote"
        );
        // The specific wrong answer this guards: a derived install path. With
        // no checkout above the binary the resolver answers ~/.local/bin or
        // /usr/local/bin, so on a canonical fleet host it names a file that is
        // not there, and on a developer's host it names their dev build.
        let derived =
            crate::cosmix_paths::cosmix_path(crate::cosmix_paths::CosmixDir::Bin).join("mix");
        assert_ne!(
            interpreter_from(Ok(std::path::PathBuf::from("/opt/cosmix/bin/mix (deleted)"))),
            derived,
            "a replaced binary must not fall back to a derived install path"
        );
        // An ordinary running binary is used as-is: this file exists.
        let real = std::path::PathBuf::from(file!());
        if real.is_file() {
            assert_eq!(interpreter_from(Ok(real.clone())), real);
        }
    }

    /// A value over the reply budget is REPLACED, never cut.
    #[test]
    fn an_oversized_value_becomes_a_reference_not_a_fragment() {
        // A quote-heavy strict-data document: complete, well under the frame
        // cap in raw bytes, and far over the budget once escaped for the reply.
        let quoted: Vec<String> = (0..9000).map(|i| format!("\"s{i}\"")).collect();
        let frame = format!("[{}]", quoted.join(","));
        assert!(
            serde_json::to_string(&frame).expect("encodes").len() > MAX_RESULT_ENCODED,
            "the fixture's own premise: this must exceed the budget"
        );
        let out = within_budget(frame.clone());
        // The failure this guards is a cut string: parseable input, unparseable
        // output, and nothing in the report saying so.
        let parsed: serde_json::Value =
            serde_json::from_str(&out).expect("a reference is always parseable");
        assert_eq!(parsed["truncated"], true, "{out}");
        assert_eq!(parsed["bytes"], frame.len().to_string(), "{out}");
        assert!(
            !out.starts_with('['),
            "the value was cut down rather than replaced: {out}"
        );
        // And a value that fits is passed through untouched.
        assert_eq!(within_budget("[1,2,3]".into()), "[1,2,3]");
    }

    /// The arithmetic the 256 KiB reply envelope depends on, done for real
    /// rather than asserted in a comment.
    #[test]
    fn a_worst_case_report_fits_one_reply() {
        let worst = |budget| fit_encoded(&"\0".repeat(MAX_STREAM * 8), budget).0;
        let stream = || Stream {
            bytes: cosmix_lib_bus::native_session::DecimalU64(u64::MAX),
            truncated: true,
            writer_survived: true,
            text: worst(MAX_STREAM_ENCODED),
        };
        let report = TaskReport {
            version: 1,
            outcome: Outcome::Unknown {
                detail: "x".repeat(512),
            },
            stdout: stream(),
            stderr: stream(),
            result: TaskResult::Value {
                data: worst(MAX_RESULT_ENCODED),
            },
            duration_ms: cosmix_lib_bus::native_session::DecimalU64(u64::MAX),
        };
        let encoded = serde_json::to_string(&report).expect("the report encodes");
        // Term's per-reply cap, with the operation envelope still to come.
        const TERM_REPLY: usize = 256 * 1024;
        assert!(
            encoded.len() < TERM_REPLY - 16 * 1024,
            "worst-case report is {} bytes, too close to the {TERM_REPLY} reply cap",
            encoded.len()
        );
    }
}
