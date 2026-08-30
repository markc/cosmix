//! Shared types for cosmix-client across native and WASM backends.

use std::collections::BTreeMap;

/// An incoming command from another service via the broker.
///
/// The `headers` field carries ALL Bus headers from the original message,
/// preserving display protocol properties (layout, style, window geometry)
/// that don't map to named fields. Named fields are convenience shortcuts.
#[derive(Debug)]
pub struct IncomingCommand {
    pub from: String,
    pub command: String,
    pub id: Option<String>,
    pub args: serde_json::Value,
    pub body: String,
    /// All Bus headers from the original message.
    pub headers: BTreeMap<String, String>,
}

impl IncomingCommand {
    /// Get any Bus header by name.
    pub fn header(&self, key: &str) -> Option<&str> {
        self.headers.get(key).map(|s| s.as_str())
    }

    /// Get the `target` header (for ui.style, ui.remove, etc.).
    pub fn target(&self) -> Option<&str> {
        self.header("target")
    }

    /// Get the `parent` header.
    pub fn parent(&self) -> Option<&str> {
        self.header("parent")
    }

    /// Get the `source` header (for ui.event).
    pub fn source(&self) -> Option<&str> {
        self.header("source")
    }

    /// Check if this is a `ui.*` display protocol command.
    pub fn is_ui_command(&self) -> bool {
        self.command.starts_with("ui.")
    }
}
