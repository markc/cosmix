pub mod amp;

use anyhow::Result;
use serde::{Deserialize, Serialize};

// ── RC codes (ARexx convention) ──

pub const RC_SUCCESS: u8 = 0;
pub const RC_WARNING: u8 = 5;
pub const RC_ERROR: u8 = 10;
pub const RC_FAILURE: u8 = 20;

// ── Wire format ──

#[derive(Debug, Deserialize)]
pub struct PortRequest {
    pub command: String,
    #[serde(default = "default_args")]
    pub args: serde_json::Value,
}

fn default_args() -> serde_json::Value {
    serde_json::Value::Null
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PortResponse {
    pub rc: u8,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl PortResponse {
    pub fn success(data: serde_json::Value) -> Self {
        Self { rc: RC_SUCCESS, ok: true, data: Some(data), error: None }
    }

    pub fn ok() -> Self {
        Self { rc: RC_SUCCESS, ok: true, data: None, error: None }
    }

    pub fn warning(msg: &str) -> Self {
        Self { rc: RC_WARNING, ok: true, data: None, error: Some(msg.to_string()) }
    }

    pub fn error(msg: &str) -> Self {
        Self { rc: RC_ERROR, ok: false, data: None, error: Some(msg.to_string()) }
    }

    pub fn failure(msg: &str) -> Self {
        Self { rc: RC_FAILURE, ok: false, data: None, error: Some(msg.to_string()) }
    }
}

// ── Script info (for macro menus) ──

/// Metadata for a Lua script that appears in an app's Scripts menu.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptInfo {
    /// Display name (derived from filename, e.g. "Add Watermark")
    pub display_name: String,
    /// Full path to the .lua file
    pub path: String,
}

// ── Port events (notification channel for UI updates) ──

#[derive(Debug, Clone)]
pub enum PortEvent {
    /// A command was dispatched on the port
    Command { name: String, ok: bool },
    /// App should bring its window to front
    Activate,
    /// Scripts menu was updated by daemon
    ScriptsUpdated(Vec<ScriptInfo>),
}

// ── Command metadata and handler (native only) ──

#[cfg(feature = "native")]
type CommandFn = Box<dyn Fn(serde_json::Value) -> Result<serde_json::Value> + Send + Sync>;

#[cfg(feature = "native")]
struct CommandEntry {
    handler: CommandFn,
    description: String,
}

// ── Unix socket port (native only) ──

#[cfg(feature = "native")]
mod native_port;
#[cfg(feature = "native")]
pub use native_port::*;
