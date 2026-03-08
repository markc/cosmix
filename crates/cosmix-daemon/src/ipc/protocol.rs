use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

fn default_delay() -> u64 { 5000 }

/// All CLI commands that can be sent to the cosmix daemon over IPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum IpcRequest {
    ListWindows,
    ListWorkspaces,
    GetClipboard,
    SetClipboard { text: String },
    Activate { query: String },
    Close { query: String },
    Minimize { query: String },
    Maximize { query: String },
    Fullscreen { query: String },
    Sticky { query: String },
    Notify { summary: String, body: String },
    RunScript { name: String, args: Vec<String> },
    ListApps,
    TypeText { text: String, #[serde(default = "default_delay")] delay_us: u64 },
    SendKey { combo: String, #[serde(default = "default_delay")] delay_us: u64 },
    ConfigList,
    ConfigKeys { component: String },
    ConfigRead { component: String, key: String },
    ConfigWrite { component: String, key: String, value: String },
    DbusCall { service: String, path: String, interface: String, method: String, args: Option<Vec<serde_json::Value>>, system: bool },
    /// Call a command on an app port (Layer 3)
    CallPort { port: String, port_command: String, args: Option<serde_json::Value> },
    /// List all registered ports (built-in + app)
    ListPorts,
    /// Take a native Wayland screenshot
    Screenshot { path: Option<String> },
    Status,
    Ping,
}

/// Unified response from the daemon to a client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl IpcResponse {
    /// A successful response carrying JSON data.
    pub fn success(data: serde_json::Value) -> Self {
        Self {
            ok: true,
            data: Some(data),
            error: None,
        }
    }

    /// An error response with a human-readable message.
    pub fn error(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(msg.into()),
        }
    }

    /// A successful response with no payload (acknowledgement).
    pub fn ok() -> Self {
        Self {
            ok: true,
            data: None,
            error: None,
        }
    }
}

/// Encode an [`IpcResponse`] into a length-prefixed frame.
///
/// Wire format: 4-byte big-endian length prefix followed by JSON bytes.
pub fn encode(response: &IpcResponse) -> Vec<u8> {
    let json = serde_json::to_vec(response).expect("IpcResponse should always serialize");
    let len = (json.len() as u32).to_be_bytes();
    let mut buf = Vec::with_capacity(4 + json.len());
    buf.extend_from_slice(&len);
    buf.extend_from_slice(&json);
    buf
}

/// Encode an [`IpcRequest`] into a length-prefixed frame (client side).
///
/// Wire format: 4-byte big-endian length prefix followed by JSON bytes.
pub fn encode_request(request: &IpcRequest) -> Vec<u8> {
    let json = serde_json::to_vec(request).expect("IpcRequest should always serialize");
    let len = (json.len() as u32).to_be_bytes();
    let mut buf = Vec::with_capacity(4 + json.len());
    buf.extend_from_slice(&len);
    buf.extend_from_slice(&json);
    buf
}

/// Decode a length-prefixed frame into an [`IpcRequest`].
///
/// Expects the full frame: 4-byte big-endian length prefix followed by that
/// many bytes of JSON. Returns the parsed request.
#[allow(dead_code)]
pub fn decode_request(bytes: &[u8]) -> Result<IpcRequest> {
    anyhow::ensure!(
        bytes.len() >= 4,
        "frame too short: need at least 4 bytes for length prefix, got {}",
        bytes.len()
    );

    let len = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;

    anyhow::ensure!(
        bytes.len() >= 4 + len,
        "incomplete frame: header says {} bytes but only {} available",
        len,
        bytes.len() - 4
    );

    serde_json::from_slice(&bytes[4..4 + len]).context("failed to deserialize IpcRequest")
}
