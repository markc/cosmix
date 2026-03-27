//! Shared types for cosmix-client across native and WASM backends.

/// An incoming command from another service via the hub.
#[derive(Debug)]
pub struct IncomingCommand {
    pub from: String,
    pub command: String,
    pub id: Option<String>,
    pub args: serde_json::Value,
    pub body: String,
}
