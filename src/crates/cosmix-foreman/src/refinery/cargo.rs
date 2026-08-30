use super::*;

#[derive(Debug)]
pub(super) struct BoundedCapture {
    bytes: Vec<u8>,
    truncated: bool,
}

pub(super) fn capture_pipe_bounded(
    mut pipe: impl Read + Send + 'static,
) -> std::thread::JoinHandle<std::io::Result<BoundedCapture>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut truncated = false;
        let mut chunk = [0_u8; 8192];
        loop {
            let read = pipe.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            let keep = read.min(CARGO_CHILD_OUTPUT_LIMIT.saturating_sub(bytes.len()));
            bytes.extend_from_slice(&chunk[..keep]);
            truncated |= keep < read;
        }
        Ok(BoundedCapture { bytes, truncated })
    })
}

pub(super) fn kill_cargo_child_group(child: &mut Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

pub(super) fn join_cargo_capture(
    reader: std::thread::JoinHandle<std::io::Result<BoundedCapture>>,
    operation: &str,
    stream: &str,
) -> Result<BoundedCapture> {
    reader
        .join()
        .map_err(|_| infrastructure_message(format!("{operation} {stream} reader panicked")))?
        .map_err(|error| {
            infrastructure_message(format!("reading {stream} from {operation} failed: {error}"))
        })
}

/// Join a capture reader, but never past `deadline`. The tracked child
/// exiting is not proof its pipes closed: a grandchild that inherited one
/// (e.g. a backgrounded helper cargo spawns) can keep it open indefinitely,
/// which would otherwise wedge an unconditional join exactly like the
/// long-fixed executor.rs reader-hang defect — and this is the sole
/// refinery lane, so a wedge here stalls every task. On deadline, kill the
/// process group again to reap any such straggler and abandon the reader.
pub(super) fn join_cargo_capture_bounded(
    reader: std::thread::JoinHandle<std::io::Result<BoundedCapture>>,
    child: &mut Child,
    deadline: Instant,
    timeout: Duration,
    operation: &str,
    stream: &str,
) -> Result<BoundedCapture> {
    loop {
        if reader.is_finished() {
            return join_cargo_capture(reader, operation, stream);
        }
        if Instant::now() >= deadline {
            kill_cargo_child_group(child);
            drop(reader);
            return Err(infrastructure_message(format!(
                "{operation} {stream} reader did not drain within {timeout:?} of the child \
                 exiting; a descendant likely inherited and kept open the pipe"
            )));
        }
        std::thread::sleep(CARGO_CHILD_POLL_INTERVAL);
    }
}

pub(super) fn run_bounded_cargo_child(
    mut command: Command,
    operation: &str,
    timeout: Duration,
) -> Result<Output> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::executor::harden(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| infrastructure_message(format!("spawning {operation} failed: {error}")))?;
    let stdout = capture_pipe_bounded(child.stdout.take().expect("cargo stdout was piped"));
    let stderr = capture_pipe_bounded(child.stderr.take().expect("cargo stderr was piped"));
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| infrastructure_message(format!("{operation} deadline overflowed")))?;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(CARGO_CHILD_POLL_INTERVAL.min(timeout));
            }
            Ok(None) => {
                kill_cargo_child_group(&mut child);
                // Do not wait on the pipe readers after the deadline. An
                // escaped descendant could retain a pipe even after the
                // cargo process group is gone; dropping these handles keeps
                // the refinery deadline authoritative while capture memory
                // remains bounded.
                drop(stdout);
                drop(stderr);
                return Err(infrastructure_message(format!(
                    "{operation} timed out after {timeout:?}; killed and reaped its process group"
                )));
            }
            Err(error) => {
                kill_cargo_child_group(&mut child);
                drop(stdout);
                drop(stderr);
                return Err(infrastructure_message(format!(
                    "polling {operation} failed: {error}"
                )));
            }
        }
    };
    let stdout =
        join_cargo_capture_bounded(stdout, &mut child, deadline, timeout, operation, "stdout")?;
    let stderr =
        join_cargo_capture_bounded(stderr, &mut child, deadline, timeout, operation, "stderr")?;
    if stdout.truncated || stderr.truncated {
        // A real cos workspace `cargo metadata` run is ~0.18 MiB, 22x under
        // this cap — exceeding it is a diagnosed refusal of THIS invocation
        // (a runaway build script, pathological diagnostics), not evidence
        // of host trouble. Classifying it as infrastructure would retry it
        // forever instead of bouncing the task that produced it.
        return Err(task_fault(anyhow::anyhow!(
            "{operation} exceeded the bounded {CARGO_CHILD_OUTPUT_LIMIT}-byte output capture"
        )));
    }
    Ok(Output {
        status,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
    })
}

pub(super) fn cargo_child_failure(operation: &str, stderr: &str, worktree: &Path) -> anyhow::Error {
    let detail = anyhow::anyhow!("{operation} failed: {}", stderr.trim());
    if !cargo_diagnostic_is_infrastructure(stderr)
        && cargo_diagnostic_is_branch_fault(stderr, worktree)
    {
        task_fault(detail)
    } else {
        // Cargo's exit status has no typed cause channel. Failures not
        // recognisably produced by manifest/dependency/lockfile resolution
        // are infrastructure: this may retry a genuinely bad branch, but it
        // cannot park an innocent task for host I/O or a corrupt Cargo cache.
        infrastructure(detail)
    }
}

pub(super) fn cargo_diagnostic_is_infrastructure(stderr: &str) -> bool {
    const INFRASTRUCTURE_DIAGNOSTICS: &[&str] = &[
        "No space left on device",
        "Input/output error",
        "Read-only file system",
        "Permission denied",
        "Too many open files",
        "Cannot allocate memory",
        "Stale file handle",
        "Device or resource busy",
        "checksum failed",
        "failed to verify the checksum",
        "corrupt package cache",
        "corrupt registry cache",
    ];
    INFRASTRUCTURE_DIAGNOSTICS
        .iter()
        .any(|diagnostic| stderr.contains(diagnostic))
}

pub(super) fn cargo_diagnostic_is_branch_fault(stderr: &str, worktree: &Path) -> bool {
    const RESOLUTION_DIAGNOSTICS: &[&str] = &[
        "failed to select a version for",
        "package ID specification",
        "did not match any packages",
        "current package believes it's in a workspace",
        "workspace member",
        "is not a member of the workspace",
        "no targets specified in the manifest",
        "the lock file needs to be updated but --offline was passed",
    ];
    let local_parse = [
        "failed to parse manifest at",
        "failed to load manifest for",
        "failed to parse lock file",
    ]
    .iter()
    .any(|diagnostic| stderr.contains(diagnostic))
        && stderr.contains(worktree.to_string_lossy().as_ref());
    local_parse
        || RESOLUTION_DIAGNOSTICS
            .iter()
            .any(|diagnostic| stderr.contains(diagnostic))
}
