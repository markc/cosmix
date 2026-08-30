//! Bounded, non-interactive execution for Git operations that may contact a
//! remote.
//!
//! Remote delivery has three outcomes. [`RemoteOutcome::Unknown`] is
//! deliberately not a failure: once a child has started, a timeout, signal,
//! wait error, output I/O error, or unrecognised non-zero exit cannot prove
//! that the remote did not accept the ref.

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

/// Default maximum retained bytes for each of stdout and stderr.
pub const DEFAULT_OUTPUT_LIMIT: usize = 64 * 1024;

const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(10);
const KILL_REAP_GRACE: Duration = Duration::from_secs(1);
const PIPE_DRAIN_GRACE: Duration = Duration::from_secs(1);

/// The durable classification consumed by remote-delivery callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteOutcome {
    /// A zero Git exit proves the requested remote operation completed.
    Succeeded,
    /// Git never started, or machine-readable output proves the single
    /// requested ref was rejected, so delivery was impossible.
    Failed,
    /// The remote may have accepted the ref. This is not a failure verdict.
    Unknown,
}

impl RemoteOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }
}

/// How the direct Git child stopped (or failed to start).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteGitTermination {
    Exited(i32),
    Signalled(Option<i32>),
    TimedOut,
    SpawnFailed,
    WaitFailed,
}

/// Captured result of one bounded Git invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteGitRun {
    pub outcome: RemoteOutcome,
    pub termination: RemoteGitTermination,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    /// Spawn, wait, reaping, or pipe-reader diagnostic. Post-spawn I/O trouble
    /// makes delivery unknown; a spawn diagnostic accompanies the terminal
    /// `Failed` verdict because no Git process existed to contact the remote.
    pub io_error: Option<String>,
}

/// Fixed-policy runner for remote-capable Git commands.
#[derive(Debug, Clone, Copy)]
pub struct RemoteGitRunner {
    timeout: Duration,
    output_limit: usize,
}

impl RemoteGitRunner {
    /// Construct a runner with an explicit non-zero wall-clock timeout and a
    /// non-zero per-stream capture cap.
    pub fn new(timeout: Duration, output_limit: usize) -> anyhow::Result<Self> {
        anyhow::ensure!(!timeout.is_zero(), "remote Git timeout must be non-zero");
        anyhow::ensure!(output_limit > 0, "remote Git output limit must be non-zero");
        Ok(Self {
            timeout,
            output_limit,
        })
    }

    /// Execute `git <args>` in `repo` without a shell.
    ///
    /// The child has an empty inherited environment and receives only the
    /// fixed values below. Git config, credential helpers, askpass programs,
    /// SSH agents, host-key questions and pagers therefore cannot leak in
    /// from the refinery service. The process is placed in its own process
    /// group; expiry kills that group and boundedly reaps the direct child.
    pub fn run<I, S>(&self, repo: &Path, args: I) -> RemoteGitRun
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_with_credentials(repo, args, &[])
    }

    /// Execute with only the explicitly selected manifest credential
    /// variables added to the otherwise cleared child environment. Fixed Git
    /// safety values are applied afterwards, so a credential policy cannot
    /// replace PATH or interactive Git/SSH controls.
    pub fn run_with_credentials<I, S>(
        &self,
        repo: &Path,
        args: I,
        credentials: &[(String, OsString)],
    ) -> RemoteGitRun
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = Command::new("git");
        command
            .arg("--no-pager")
            .args(["-c", "credential.helper=", "-c", "core.askPass=/bin/false"])
            .args(args)
            .current_dir(repo)
            .env_clear()
            .envs(credentials.iter().map(|(name, value)| (name, value)))
            .env("PATH", "/usr/bin:/bin")
            .env("LC_ALL", "C")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "/bin/false")
            .env("SSH_ASKPASS", "/bin/false")
            .env("SSH_ASKPASS_REQUIRE", "force")
            .env("GCM_INTERACTIVE", "Never")
            .env("GIT_PAGER", "cat")
            .env("PAGER", "cat")
            .env(
                "GIT_SSH_COMMAND",
                "ssh -oBatchMode=yes -oStrictHostKeyChecking=yes -oNumberOfPasswordPrompts=0",
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        crate::executor::harden(&mut command);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return finish_run(
                    RemoteGitTermination::SpawnFailed,
                    Capture::default(),
                    Capture::default(),
                    Some(format!("spawning git failed: {error}")),
                );
            }
        };
        let stdout = capture_pipe(
            child.stdout.take().expect("remote Git stdout was piped"),
            self.output_limit,
        );
        let stderr = capture_pipe(
            child.stderr.take().expect("remote Git stderr was piped"),
            self.output_limit,
        );
        let deadline = Instant::now()
            .checked_add(self.timeout)
            .unwrap_or_else(Instant::now);

        let mut io_error = None;
        let termination = loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    // Git normally waits for its children. Still clear any
                    // background descendant that retained the group without
                    // retaining a pipe; no subprocess from a completed
                    // remote operation may outlive this bounded scope.
                    kill_process_group(&mut child);
                    #[cfg(unix)]
                    {
                        use std::os::unix::process::ExitStatusExt;
                        break match status.code() {
                            Some(code) => RemoteGitTermination::Exited(code),
                            None => RemoteGitTermination::Signalled(status.signal()),
                        };
                    }
                    #[cfg(not(unix))]
                    break match status.code() {
                        Some(code) => RemoteGitTermination::Exited(code),
                        None => RemoteGitTermination::Signalled(None),
                    };
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(CHILD_POLL_INTERVAL.min(self.timeout));
                }
                Ok(None) => {
                    kill_process_group(&mut child);
                    if let Err(error) = reap_bounded(&mut child, KILL_REAP_GRACE) {
                        io_error = Some(format!(
                            "remote Git timed out after {:?}; {error}",
                            self.timeout
                        ));
                    }
                    break RemoteGitTermination::TimedOut;
                }
                Err(error) => {
                    io_error = Some(format!("polling remote Git failed: {error}"));
                    kill_process_group(&mut child);
                    if let Err(reap_error) = reap_bounded(&mut child, KILL_REAP_GRACE) {
                        append_error(&mut io_error, reap_error);
                    }
                    break RemoteGitTermination::WaitFailed;
                }
            }
        };

        let drain_deadline = Instant::now()
            .checked_add(PIPE_DRAIN_GRACE)
            .unwrap_or_else(Instant::now);
        let stdout = receive_capture(stdout, drain_deadline, "stdout", &mut io_error);
        let stderr = receive_capture(stderr, drain_deadline, "stderr", &mut io_error);
        finish_run(termination, stdout, stderr, io_error)
    }
}

/// Classify a single-ref remote delivery from process termination and Git's
/// `push --porcelain` stdout.
///
/// Failure to spawn is terminal `Failed` evidence because Git never ran. Once
/// spawned, only a normal zero exit or one exact machine-readable rejection is
/// terminal evidence. Multiple porcelain ref records on a non-zero exit are
/// `Unknown`, because Git may have accepted one ref while rejecting another.
pub fn classify_remote_delivery(
    termination: &RemoteGitTermination,
    stdout: &[u8],
    output_io_ok: bool,
) -> RemoteOutcome {
    match termination {
        RemoteGitTermination::SpawnFailed => RemoteOutcome::Failed,
        _ if !output_io_ok => RemoteOutcome::Unknown,
        RemoteGitTermination::Exited(0) => RemoteOutcome::Succeeded,
        RemoteGitTermination::Exited(_) if one_provable_rejection(stdout) => RemoteOutcome::Failed,
        RemoteGitTermination::Exited(_)
        | RemoteGitTermination::Signalled(_)
        | RemoteGitTermination::TimedOut
        | RemoteGitTermination::WaitFailed => RemoteOutcome::Unknown,
    }
}

fn one_provable_rejection(stdout: &[u8]) -> bool {
    let stdout = String::from_utf8_lossy(stdout);
    let records: Vec<&str> = stdout
        .lines()
        .filter(|line| {
            line.as_bytes()
                .first()
                .is_some_and(|flag| b" *+-=!".contains(flag))
                && line.as_bytes().get(1) == Some(&b'\t')
        })
        .collect();
    if records.len() != 1 {
        return false;
    }
    let mut fields = records[0].splitn(3, '\t');
    fields.next() == Some("!")
        && fields.next().is_some()
        && fields.next().is_some_and(|summary| {
            summary.starts_with("[rejected]") || summary.starts_with("[remote rejected]")
        })
}

#[derive(Debug, Default)]
struct Capture {
    bytes: Vec<u8>,
    truncated: bool,
    error: Option<String>,
}

fn capture_pipe(mut pipe: impl Read + Send + 'static, limit: usize) -> Receiver<Capture> {
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut capture = Capture::default();
        let mut chunk = [0_u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    let keep = read.min(limit.saturating_sub(capture.bytes.len()));
                    capture.bytes.extend_from_slice(&chunk[..keep]);
                    capture.truncated |= keep < read;
                }
                Err(error) => {
                    capture.error = Some(error.to_string());
                    break;
                }
            }
        }
        let _ = sender.send(capture);
    });
    receiver
}

fn receive_capture(
    receiver: Receiver<Capture>,
    deadline: Instant,
    stream: &str,
    io_error: &mut Option<String>,
) -> Capture {
    let wait = deadline.saturating_duration_since(Instant::now());
    match receiver.recv_timeout(wait) {
        Ok(capture) => {
            if let Some(error) = &capture.error {
                append_error(
                    io_error,
                    format!("reading remote Git {stream} failed: {error}"),
                );
            }
            capture
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            append_error(
                io_error,
                format!("remote Git {stream} did not close after the child stopped"),
            );
            Capture::default()
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            append_error(io_error, format!("remote Git {stream} reader stopped"));
            Capture::default()
        }
    }
}

fn finish_run(
    termination: RemoteGitTermination,
    stdout: Capture,
    stderr: Capture,
    io_error: Option<String>,
) -> RemoteGitRun {
    let mut outcome = classify_remote_delivery(&termination, &stdout.bytes, io_error.is_none());
    // A truncated porcelain stream cannot prove that the one visible
    // rejection was the only ref record. It does not weaken a zero exit,
    // which independently proves that Git completed the remote operation.
    if stdout.truncated && outcome == RemoteOutcome::Failed {
        outcome = RemoteOutcome::Unknown;
    }
    RemoteGitRun {
        outcome,
        termination,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
        stdout_truncated: stdout.truncated,
        stderr_truncated: stderr.truncated,
        io_error,
    }
}

fn kill_process_group(child: &mut Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    let _ = child.kill();
}

fn reap_bounded(child: &mut Child, grace: Duration) -> Result<(), String> {
    let deadline = Instant::now()
        .checked_add(grace)
        .unwrap_or_else(Instant::now);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(CHILD_POLL_INTERVAL),
            Ok(None) => return Err("killed child could not be reaped before the deadline".into()),
            Err(error) => return Err(format!("waiting for killed child failed: {error}")),
        }
    }
}

fn append_error(target: &mut Option<String>, error: String) {
    match target {
        Some(existing) => {
            existing.push_str("; ");
            existing.push_str(&error);
        }
        None => *target = Some(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taxonomy_is_stable_and_ambiguous_exits_are_unknown() {
        assert_eq!(RemoteOutcome::Succeeded.as_str(), "succeeded");
        assert_eq!(RemoteOutcome::Failed.as_str(), "failed");
        assert_eq!(RemoteOutcome::Unknown.as_str(), "unknown");
        assert_eq!(
            classify_remote_delivery(&RemoteGitTermination::Exited(23), b"transport ended", true),
            RemoteOutcome::Unknown
        );
        assert_eq!(
            classify_remote_delivery(&RemoteGitTermination::WaitFailed, b"", true),
            RemoteOutcome::Unknown
        );
        assert_eq!(
            classify_remote_delivery(&RemoteGitTermination::Exited(0), b"", false),
            RemoteOutcome::Unknown
        );
        assert_eq!(
            classify_remote_delivery(&RemoteGitTermination::SpawnFailed, b"", false),
            RemoteOutcome::Failed
        );
    }

    #[test]
    fn only_one_machine_readable_rejection_is_failed() {
        let rejected = b"!\tHEAD:refs/heads/main\t[remote rejected] (hook declined)\n";
        assert_eq!(
            classify_remote_delivery(&RemoteGitTermination::Exited(1), rejected, true),
            RemoteOutcome::Failed
        );
        let partial = b"*\tHEAD:refs/heads/one\t[new branch]\n!\tHEAD:refs/heads/two\t[rejected] (fetch first)\n";
        assert_eq!(
            classify_remote_delivery(&RemoteGitTermination::Exited(1), partial, true),
            RemoteOutcome::Unknown
        );
        assert_eq!(
            classify_remote_delivery(&RemoteGitTermination::TimedOut, rejected, true),
            RemoteOutcome::Unknown
        );
    }
}
