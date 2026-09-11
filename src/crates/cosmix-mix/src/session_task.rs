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
//! The declared residual is the double-forked grandchild. `PR_SET_PDEATHSIG`
//! binds the task LEADER to the supervising thread, so a shell that dies —
//! including by SIGKILL, which no teardown hook can catch — takes the leader
//! with it. A grandchild that left the group first survives, and the manual
//! says so rather than implying a containment this does not have.

use serde::Serialize;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Concurrent tasks per shell. Beyond this, RESOURCE_LIMIT — a real limit
/// (processes and supervisor threads), not a bookkeeping one.
pub(crate) const TASKS: usize = 4;
/// Per-stream capture cap. With the 64 KiB result cap this keeps a full settled
/// record inside one Term reply under the 256 KiB envelope, with headroom —
/// which is what lets v1 ship without chunking machinery.
pub(crate) const MAX_STREAM: usize = 64 * 1024;
/// The declared grace between SIGTERM and SIGKILL, and also the deadline for
/// draining the result pipe: a grandchild holding the write end open must not
/// be able to wedge the supervisor.
const GRACE: Duration = Duration::from_secs(2);
const MAX_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_ENV_VARS: usize = 64;
const MAX_ENV_BYTES: usize = 16 * 1024;
const MAX_ARGV: usize = 256;
const MAX_ARG_BYTES: usize = 64 * 1024;

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

/// Called once from REPL startup, before any user code can mutate environ.
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
        // between); this one exists so the common mistake is a clean refusal
        // rather than a spawn failure the caller has to decode.
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
    Signalled {
        signal: i32,
        /// True when this signal was the supervisor's own escalation, so a
        /// caller can tell "the task was killed by policy" from "the task was
        /// killed by something else".
        escalated: bool,
    },
    /// The supervisor's deadline fired. `escalated_to` names how far the
    /// ladder had to go.
    Timeout {
        escalated_to: &'static str,
    },
    Cancelled {
        escalated_to: &'static str,
    },
    /// Waited and got no status we can interpret.
    Unknown {
        detail: String,
    },
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Stream {
    pub bytes: cosmix_lib_bus::native_session::DecimalU64,
    pub truncated: bool,
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
    Cancel,
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
) -> Result<Handle, String> {
    let started = Instant::now();
    let (result_read, result_write) = pipe()?;
    let mut command = match &spec.mode {
        Mode::Source(source) => {
            let mut command = Command::new(crate::cosmix_paths::cosmix_path(
                crate::cosmix_paths::CosmixDir::Bin,
            )
            .join("mix"));
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
                libc::_exit(127);
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

    let mut child = command.spawn().map_err(|error| error.to_string())?;
    // The parent's copy of the write end must close, or the read below never
    // sees EOF and the drain waits for a descriptor nobody will write to.
    drop(result_write);
    let pid = child.id() as libc::pid_t;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let (tx, rx) = mpsc::channel();
    let cancel = tx.clone();
    let cancelling = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Drains run CONCURRENTLY with the wait, never before or after it. A 64 KiB
    // pipe buffer against a larger output is a deadlock, and a deadlock here
    // would be reported as a timeout — a wrong answer that looks like a
    // policy decision.
    let out_drain = stdout.map(|s| drain(s, MAX_STREAM));
    let err_drain = stderr.map(|s| drain(s, MAX_STREAM));
    let result_drain = wants_result.then(|| drain_raw(result_read, MAX_STREAM * 2));

    let waiter = tx;
    std::thread::Builder::new()
        .name("mix-task-wait".into())
        .spawn(move || {
            let status = child.wait();
            let _ = waiter.send(match status {
                Ok(status) => Event::Exited(status),
                Err(_) => Event::Exited(std::process::ExitStatus::default()),
            });
        })
        .map_err(|error| error.to_string())?;

    let supervised = cancelling.clone();
    std::thread::Builder::new()
        .name("mix-task".into())
        .spawn(move || {
            let (outcome, _) = supervise(&rx, pid, spec.timeout, &supervised);
            let stdout = out_drain.map(join_stream).unwrap_or_else(empty_stream);
            let stderr = err_drain.map(join_stream).unwrap_or_else(empty_stream);
            let result = match result_drain {
                None => TaskResult::NotApplicable,
                Some(handle) => decode_frame(&handle.join().unwrap_or_default()),
            };
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
        })
        .map_err(|error| error.to_string())?;

    Ok(Handle {
        cancel,
        started,
        cancelling,
    })
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
) -> (Outcome, bool) {
    let reason = match rx.recv_timeout(timeout) {
        Ok(Event::Exited(status)) => return (natural(status, false), false),
        Ok(Event::Cancel) => Reason::Cancelled,
        Err(mpsc::RecvTimeoutError::Timeout) => Reason::Timeout,
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return (
                Outcome::Unknown {
                    detail: "the waiter thread ended without a status".into(),
                },
                false,
            );
        }
    };
    cancelling.store(true, std::sync::atomic::Ordering::Release);
    // Declared policy, in order: TERM to the GROUP, a stated grace, then KILL.
    signal_group(pid, libc::SIGTERM);
    if let Some(status) = settle_within(rx, GRACE) {
        return (reason.into_outcome("sigterm", status), true);
    }
    signal_group(pid, libc::SIGKILL);
    // SIGKILL cannot be caught, so this wait is bounded in practice; the
    // deadline is belt-and-braces against an unkillable D-state.
    match settle_within(rx, GRACE) {
        Some(status) => (reason.into_outcome("sigkill", status), true),
        None => (
            Outcome::Unknown {
                detail: "the group did not reap after SIGKILL".into(),
            },
            true,
        ),
    }
}

enum Reason {
    Timeout,
    Cancelled,
}
impl Reason {
    /// The outcome names the POLICY that ended the task, not the signal the
    /// policy happened to use — the signal is reported beside it as
    /// `escalated_to`. Reporting a timeout as "signalled: SIGTERM" would make
    /// a deliberate deadline indistinguishable from an external kill.
    fn into_outcome(self, escalated_to: &'static str, _status: std::process::ExitStatus) -> Outcome {
        match self {
            Self::Timeout => Outcome::Timeout { escalated_to },
            Self::Cancelled => Outcome::Cancelled { escalated_to },
        }
    }
}

/// Drain any duplicate cancels while waiting for the real settlement.
fn settle_within(
    rx: &mpsc::Receiver<Event>,
    budget: Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + budget;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(Event::Exited(status)) => return Some(status),
            Ok(Event::Cancel) => continue,
            Err(_) => return None,
        }
    }
}

fn natural(status: std::process::ExitStatus, escalated: bool) -> Outcome {
    use std::os::unix::process::ExitStatusExt;
    if let Some(signal) = status.signal() {
        Outcome::Signalled { signal, escalated }
    } else if let Some(code) = status.code() {
        Outcome::Exited { code }
    } else {
        Outcome::Unknown {
            detail: "wait returned neither a code nor a signal".into(),
        }
    }
}

fn signal_group(pid: libc::pid_t, signal: libc::c_int) {
    // The GROUP, because the task is a session leader and its own children are
    // the reason a leader-only signal would leave work running.
    // SAFETY: a plain signal to a group this supervisor created.
    unsafe {
        libc::killpg(pid, signal);
    }
}

// ---------------------------------------------------------------- capture

type Drain = std::thread::JoinHandle<(Vec<u8>, usize)>;

fn drain(mut source: impl Read + Send + 'static, cap: usize) -> Drain {
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut total = 0usize;
        let mut chunk = [0u8; 8192];
        loop {
            match source.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    total += n;
                    // Read past the cap rather than stopping: leaving bytes in
                    // the pipe would block the writer, and a blocked writer
                    // never exits, which the supervisor would report as a
                    // timeout. Truncation is about what is KEPT.
                    if kept.len() < cap {
                        let room = cap - kept.len();
                        kept.extend_from_slice(&chunk[..n.min(room)]);
                    }
                }
            }
        }
        (kept, total)
    })
}

fn drain_raw(file: std::fs::File, cap: usize) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut source = file;
        let mut kept = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match source.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if kept.len() < cap {
                        let room = cap - kept.len();
                        kept.extend_from_slice(&chunk[..n.min(room)]);
                    }
                }
            }
        }
        kept
    })
}

fn join_stream(handle: Drain) -> Stream {
    let (kept, total) = handle.join().unwrap_or_default();
    let truncated = total > kept.len();
    Stream {
        bytes: cosmix_lib_bus::native_session::DecimalU64(total as u64),
        truncated,
        text: String::from_utf8_lossy(&kept).into_owned(),
    }
}

fn empty_stream() -> Stream {
    Stream {
        bytes: cosmix_lib_bus::native_session::DecimalU64(0),
        truncated: false,
        text: String::new(),
    }
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
}
