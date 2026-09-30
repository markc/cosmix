//! Isolate the !Send Mix evaluator, process cwd and inherited stdio in an
//! owned Rust worker. Only the parent speaks MCP. Cancellation terminates the
//! worker process group; it cannot undo effects already committed through Bus.

use std::io::{Read, Write};
use std::process::Stdio;

use rmcp::model::CallToolResult;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

const SCRIPT_LIMIT: usize = 256 * 1024;
const INPUT_LIMIT: usize = SCRIPT_LIMIT * 6 + 4096;
const OUTPUT_LIMIT: usize = 1024 * 1024;
pub(crate) const WORKER_ARG: &str = "--internal-mix-worker";

#[derive(Clone, Default)]
pub(crate) struct CaptureBuffer(std::sync::Arc<std::sync::Mutex<(Vec<u8>, bool)>>);

impl CaptureBuffer {
    pub(crate) fn to_string_lossy(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap_or_else(|e| e.into_inner()).0).into_owned()
    }

    pub(crate) fn exceeded(&self) -> bool {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).1
    }
}

impl Write for CaptureBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if bytes.len() > OUTPUT_LIMIT.saturating_sub(state.0.len()) {
            state.1 = true;
            return Err(std::io::Error::other("Mix captured output exceeded 1 MiB"));
        }
        state.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    script: String,
    cwd: Option<String>,
}

/// Own descendants as well as the worker. A dropped MCP future must not leave
/// evaluator threads changing the server's cwd or holding its transport fds.
struct ProcessGroup(std::sync::atomic::AtomicU32);

impl ProcessGroup {
    fn terminate(&self) {
        let pid = self.0.swap(0, std::sync::atomic::Ordering::Relaxed);
        if pid == 0 {
            return;
        }
        #[cfg(unix)]
        // SAFETY: only the child group created by process_group(0) is addressed.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.terminate();
    }
}

async fn bounded_read(mut stream: impl AsyncRead + Unpin) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    (&mut stream)
        .take((OUTPUT_LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|e| format!("worker output read failed: {e}"))?;
    if bytes.len() > OUTPUT_LIMIT {
        return Err(format!(
            "Mix output exceeded {OUTPUT_LIMIT} bytes; execution stopped, effects may already have occurred"
        ));
    }
    Ok(bytes)
}

pub(crate) async fn execute(script: String, cwd: Option<String>) -> CallToolResult {
    match execute_inner(script, cwd).await {
        Ok((stdout, stderr, exit_code)) => {
            let text = if stdout.is_empty() && stderr.is_empty() {
                "(no output)".into()
            } else if stderr.is_empty() {
                stdout.clone()
            } else {
                format!("{stdout}\n--- stderr ---\n{stderr}")
            };
            let mut result = CallToolResult::structured(serde_json::json!(
                super::tool_output::MixExecutionObservation {
                    result: text,
                    stdout,
                    stderr,
                    exit_code,
                }
            ));
            result.is_error = Some(exit_code != 0);
            result
        }
        Err(message) => CallToolResult::error(vec![rmcp::model::ContentBlock::text(message)]),
    }
}

async fn execute_inner(
    script: String,
    cwd: Option<String>,
) -> Result<(String, String, i32), String> {
    if script.len() > SCRIPT_LIMIT {
        return Err(format!(
            "script exceeds {SCRIPT_LIMIT} bytes; nothing was started"
        ));
    }
    let input = serde_json::to_vec(&Request { script, cwd })
        .map_err(|e| format!("worker request serialization failed: {e}"))?;
    let executable = std::env::current_exe().map_err(|e| format!("resolve MCP binary: {e}"))?;
    let mut command = tokio::process::Command::new(executable);
    command
        .arg(WORKER_ARG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("start Mix worker: {e}"))?;
    let group = ProcessGroup(std::sync::atomic::AtomicU32::new(
        child.id().ok_or("Mix worker has no pid")?,
    ));
    let mut stdin = child.stdin.take().ok_or("Mix worker has no stdin")?;
    let stdout = child.stdout.take().ok_or("Mix worker has no stdout")?;
    let stderr = child.stderr.take().ok_or("Mix worker has no stderr")?;
    stdin
        .write_all(&input)
        .await
        .map_err(|e| format!("write Mix request: {e}"))?;
    stdin
        .shutdown()
        .await
        .map_err(|e| format!("close Mix request: {e}"))?;
    drop(stdin);
    // Read both pipes while waiting, otherwise a child filling either pipe
    // deadlocks. A read bound failure drops the child and its group guard.
    let (stdout, stderr, status) =
        tokio::try_join!(bounded_read(stdout), bounded_read(stderr), async {
            let status = child
                .wait()
                .await
                .map_err(|e| format!("wait Mix worker: {e}"))?;
            // A completed script does not leave descendants holding the output
            // pipes open indefinitely. Persistent applications belong to their
            // owning native service lifecycle, not this transient worker.
            group.terminate();
            Ok(status)
        })?;
    Ok((
        String::from_utf8_lossy(&stdout).into_owned(),
        String::from_utf8_lossy(&stderr).into_owned(),
        status.code().unwrap_or(1),
    ))
}

/// Internal execution entrypoint, after the side-effect-free --version gate
/// and before logging/MCP startup. Worker stdout is tool output, never JSON-RPC.
pub(crate) fn main() -> i32 {
    let mut input = Vec::new();
    if let Err(e) = std::io::stdin()
        .take((INPUT_LIMIT + 1) as u64)
        .read_to_end(&mut input)
    {
        eprintln!("read Mix request: {e}");
        return 1;
    }
    if input.len() > INPUT_LIMIT {
        eprintln!("Mix request exceeds input limit");
        return 1;
    }
    let request: Request = match serde_json::from_slice(&input) {
        Ok(request) => request,
        Err(e) => {
            eprintln!("invalid Mix worker request: {e}");
            return 1;
        }
    };
    if request.script.len() > SCRIPT_LIMIT {
        eprintln!("Mix script exceeds input limit");
        return 1;
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("start Mix runtime: {e}");
            return 1;
        }
    };
    let result = runtime.block_on(super::run_mix_script(
        &request.script,
        request.cwd.as_deref(),
    ));
    match result {
        Ok((stdout, stderr, code)) => {
            if let Err(e) = std::io::stdout().write_all(stdout.as_bytes()) {
                eprintln!("write Mix output: {e}");
                return 1;
            }
            if let Err(e) = std::io::stderr().write_all(stderr.as_bytes()) {
                eprintln!("write Mix stderr: {e}");
                return 1;
            }
            code
        }
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}
