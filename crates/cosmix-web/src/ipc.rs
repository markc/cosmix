//! IPC client for communicating with cosmix-daemon.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

/// Response from the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonResponse {
    pub ok: bool,
    pub data: Option<serde_json::Value>,
    pub error: Option<String>,
}

/// Get the daemon socket path for the current user.
pub fn socket_path() -> String {
    let uid = unsafe { libc::getuid() };
    format!("/run/user/{uid}/cosmix/cosmix.sock")
}

/// Send a command to the daemon and get the response.
pub async fn call(command: &str, body: Option<serde_json::Value>) -> Result<DaemonResponse> {
    let path = socket_path();
    let mut stream = tokio::net::UnixStream::connect(&path).await
        .map_err(|e| anyhow::anyhow!("Cannot connect to daemon at {path}: {e}"))?;

    // Build AMP request
    let mut msg = cosmix_port::amp::AmpMessage::new();
    msg.set("command", command);
    if let Some(b) = body {
        msg.body = serde_json::to_string(&b)?;
    }

    // Write and signal EOF
    stream.write_all(&msg.to_bytes()).await?;
    stream.shutdown().await?;

    // Read AMP response
    let resp_msg = cosmix_port::amp::read_from_stream(&mut stream).await?;

    let rc: u8 = resp_msg.get("rc").and_then(|s| s.parse().ok()).unwrap_or(0);
    let ok = rc == 0 || rc == 5;
    let error = resp_msg.get("error").map(|s| s.to_string());
    let data = if resp_msg.body.is_empty() {
        None
    } else {
        Some(serde_json::from_str(&resp_msg.body)?)
    };

    Ok(DaemonResponse { ok, data, error })
}

/// Send a command with named fields (serialized as JSON body).
pub async fn call_with_args(command: &str, args: serde_json::Value) -> Result<DaemonResponse> {
    call(command, Some(args)).await
}

/// Check if the daemon is running (used by API routes).
#[allow(dead_code)]
pub fn is_running() -> bool {
    std::path::Path::new(&socket_path()).exists()
}
