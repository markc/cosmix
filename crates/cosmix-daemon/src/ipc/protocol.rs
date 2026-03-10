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
    /// Clip List — set a named value
    SetClip { key: String, value: serde_json::Value, set_by: Option<String>, ttl_secs: Option<u64> },
    /// Clip List — get a named value
    GetClip { key: String },
    /// Clip List — list all entries
    ListClips,
    /// Clip List — delete a named value
    DelClip { key: String },
    /// Named queue — push a value
    PushQueue { queue: String, item: serde_json::Value },
    /// Named queue — pop a value
    PopQueue { queue: String },
    /// Named queue — get size
    QueueSize { queue: String },
    /// Named queue — list all queues
    ListQueues,
    /// Run a script with a calling port pre-addressed (for macro menus)
    RunScriptForApp { script_path: String, caller_port: String },
    /// Rescan and push scripts to a specific port
    RescanScripts { port: Option<String> },
    /// Send a raw AMP message to a mesh peer
    MeshSend { node: String, mesh_command: String, args: Option<serde_json::Value> },
    /// Call a port command on a remote node (waits for response)
    MeshCall { node: String, port: String, port_command: String, args: Option<serde_json::Value> },
    /// Get mesh status
    MeshStatus,
    /// List connected mesh peers
    MeshPeers,
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

/// Encode an [`IpcResponse`] into AMP wire format.
///
/// Format: `---\nrc: N\n[error: ...]\n---\n[json_body]\n`
pub fn encode(response: &IpcResponse) -> Vec<u8> {
    let mut msg = cosmix_port::amp::AmpMessage::new();
    msg.set("rc", if response.ok { "0" } else { "10" });
    if let Some(ref error) = response.error {
        msg.set("error", error);
    }
    if let Some(ref data) = response.data {
        msg.body = serde_json::to_string(data).unwrap_or_default();
    }
    msg.to_bytes()
}

/// Encode an [`IpcRequest`] into AMP wire format (client side).
///
/// Serializes the enum to JSON, extracts the `command` tag into an AMP header,
/// and puts the remaining fields in the body.
pub fn encode_request(request: &IpcRequest) -> Vec<u8> {
    let json = serde_json::to_value(request).expect("IpcRequest should always serialize");
    let mut map = json.as_object().expect("IpcRequest serializes to object").clone();

    let command = map.remove("command").expect("tagged enum has command field");
    let command_str = command.as_str().expect("command is a string");

    let mut msg = cosmix_port::amp::AmpMessage::new();
    msg.set("command", command_str);

    // Only include body if there are extra fields beyond the command tag
    if !map.is_empty() {
        msg.body = serde_json::to_string(&map).unwrap_or_default();
    }

    msg.to_bytes()
}

/// Decode an AMP message into an [`IpcRequest`].
///
/// Reads the `command` header and merges it with the body JSON to reconstruct
/// the tagged enum that serde expects.
pub fn decode_amp_request(msg: &cosmix_port::amp::AmpMessage) -> Result<IpcRequest> {
    let command = msg.get("command")
        .ok_or_else(|| anyhow::anyhow!("Missing 'command' header in AMP request"))?;

    if msg.body.is_empty() {
        // No extra fields — just the command
        let json = serde_json::json!({"command": command});
        serde_json::from_value(json).context("failed to deserialize IpcRequest")
    } else {
        // Merge command into body JSON
        let mut map: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&msg.body).context("AMP body is not valid JSON object")?;
        map.insert("command".to_string(), serde_json::Value::String(command.to_string()));
        serde_json::from_value(serde_json::Value::Object(map))
            .context("failed to deserialize IpcRequest from AMP")
    }
}

/// Decode an AMP message into an [`IpcResponse`].
pub fn decode_amp_response(msg: &cosmix_port::amp::AmpMessage) -> Result<IpcResponse> {
    let rc: u8 = msg.get("rc").and_then(|s| s.parse().ok()).unwrap_or(0);
    let ok = rc == 0;
    let error = msg.get("error").map(|s| s.to_string());
    let data = if msg.body.is_empty() {
        None
    } else {
        Some(serde_json::from_str(&msg.body).context("AMP response body is not valid JSON")?)
    };
    Ok(IpcResponse { ok, data, error })
}
