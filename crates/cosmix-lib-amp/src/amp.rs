//! AMP (AppMesh Protocol) wire format — markdown frontmatter framing.
//!
//! Every AMP message is `---\n` delimited headers with an optional body:
//!
//! ```text
//! ---
//! command: get
//! rc: 0
//! ---
//! {"key": "value"}
//! ```
//!
//! Used for ALL cosmix IPC: local Unix sockets, mesh WebSockets, and log files.

use std::collections::BTreeMap;
use std::fmt;

use anyhow::Result;

// ── AMP Message ──

/// The minimum valid AMP message — heartbeat, ACK, or keepalive.
pub const EMPTY_MESSAGE: &str = "---\n---\n";

/// A parsed AMP message: ordered headers + optional body.
#[derive(Debug, Clone, PartialEq)]
pub struct AmpMessage {
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

impl AmpMessage {
    /// Create a new empty message.
    pub fn new() -> Self {
        Self {
            headers: BTreeMap::new(),
            body: String::new(),
        }
    }

    /// Create the empty AMP message (heartbeat/keepalive).
    pub fn empty() -> Self {
        Self::new()
    }

    /// Create a command message from header pairs (no body).
    pub fn command(headers: impl IntoIterator<Item = (&'static str, String)>) -> Self {
        Self {
            headers: headers.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            body: String::new(),
        }
    }

    /// Add a header (builder pattern).
    pub fn with_header(mut self, key: &str, value: &str) -> Self {
        self.headers.insert(key.to_string(), value.to_string());
        self
    }

    /// Set the body (builder pattern).
    pub fn with_body(mut self, body: &str) -> Self {
        self.body = body.to_string();
        self
    }

    /// Add a header (mutable).
    pub fn set(&mut self, key: &str, value: &str) {
        self.headers.insert(key.to_string(), value.to_string());
    }

    /// Get a header value.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.headers.get(key).map(|s| s.as_str())
    }

    /// Check if this is the empty message (heartbeat/keepalive).
    pub fn is_empty_message(&self) -> bool {
        self.headers.is_empty() && self.body.is_empty()
    }

    // ── Convenience accessors ──

    /// Get the `from` address.
    pub fn from_addr(&self) -> Option<&str> {
        self.get("from")
    }

    /// Get the `to` address.
    pub fn to_addr(&self) -> Option<&str> {
        self.get("to")
    }

    /// Get the `command` name.
    pub fn command_name(&self) -> Option<&str> {
        self.get("command")
    }

    /// Get the `type` (request/response/event/stream).
    pub fn message_type(&self) -> Option<&str> {
        self.get("type")
    }

    /// Get `args` as parsed JSON.
    pub fn args(&self) -> Option<serde_json::Value> {
        self.get("args").and_then(|s| serde_json::from_str(s).ok())
    }

    /// Get `json` payload as parsed JSON.
    pub fn json_payload(&self) -> Option<serde_json::Value> {
        self.get("json").and_then(|s| serde_json::from_str(s).ok())
    }

    /// Serialize to AMP wire format bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.to_wire().into_bytes()
    }

    /// Serialize to AMP wire format string.
    pub fn to_wire(&self) -> String {
        let mut out = String::from("---\n");
        for (k, v) in &self.headers {
            out.push_str(k);
            out.push_str(": ");
            out.push_str(v);
            out.push('\n');
        }
        out.push_str("---\n");
        if !self.body.is_empty() {
            out.push_str(&self.body);
            if !self.body.ends_with('\n') {
                out.push('\n');
            }
        }
        out
    }
}

impl fmt::Display for AmpMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_wire())
    }
}

/// Parse an AMP message from raw text.
pub fn parse(raw: &str) -> Result<AmpMessage> {
    let content = raw.strip_prefix("---\n")
        .ok_or_else(|| anyhow::anyhow!("AMP message must start with '---\\n', got: {:?}",
            &raw[..raw.len().min(40)]))?;

    let (header_block, body) = match content.split_once("\n---\n") {
        Some((h, b)) => (h, b),
        None => {
            // Try trailing ---\n (no body)
            let h = content.strip_suffix("\n---\n")
                .or_else(|| content.strip_suffix("\n---"))
                .or_else(|| content.strip_suffix("---\n"))
                .or_else(|| content.strip_suffix("---"))
                .unwrap_or(content);
            (h, "")
        }
    };

    let mut headers = BTreeMap::new();
    for line in header_block.lines() {
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(": ") {
            headers.insert(k.trim().to_string(), v.trim().to_string());
        }
    }

    Ok(AmpMessage {
        headers,
        body: body.trim_end().to_string(),
    })
}

// ── Transport helpers (native only) ──

#[cfg(feature = "native")]
mod transport {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Read an AMP message from a Unix stream (reads until EOF).
    ///
    /// The sender must shut down their write side to signal EOF.
    pub async fn read_from_stream(stream: &mut tokio::net::UnixStream) -> Result<AmpMessage> {
        let mut buf = Vec::with_capacity(4096);

        // Read with timeout to prevent hanging on misbehaving clients
        match tokio::time::timeout(
            std::time::Duration::from_secs(10),
            stream.read_to_end(&mut buf),
        ).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => anyhow::bail!("Read error: {e}"),
            Err(_) => anyhow::bail!("AMP read timed out (10s)"),
        }

        if buf.is_empty() {
            anyhow::bail!("Empty AMP message (no data received)");
        }

        let raw = String::from_utf8(buf)?;
        parse(&raw)
    }

    /// Write an AMP message to a Unix stream.
    pub async fn write_to_stream(stream: &mut tokio::net::UnixStream, msg: &AmpMessage) -> Result<()> {
        stream.write_all(&msg.to_bytes()).await?;
        Ok(())
    }
}

#[cfg(feature = "native")]
pub use transport::{read_from_stream, write_to_stream};

// ── AMP Address ──

/// An AMP address: `port.app.node.amp`
///
/// Hierarchical addressing for the mesh:
/// - `node.amp` — the node itself
/// - `app.node.amp` — an application on a node
/// - `port.app.node.amp` — a specific port/endpoint within an app
///
/// Examples:
/// ```
/// # use cosmix_amp::amp::AmpAddress;
/// let addr = AmpAddress::parse("cosmix-toot.cosmix.cachyos.amp").unwrap();
/// assert_eq!(addr.port.as_deref(), Some("cosmix-toot"));
/// assert_eq!(addr.app.as_deref(), Some("cosmix"));
/// assert_eq!(addr.node, "cachyos");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AmpAddress {
    pub port: Option<String>,
    pub app: Option<String>,
    pub node: String,
}

impl AmpAddress {
    /// Parse an AMP address string.
    ///
    /// Accepts:
    /// - `node.amp` → node only
    /// - `app.node.amp` → app + node
    /// - `port.app.node.amp` → port + app + node
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.strip_suffix(".amp")?;
        let parts: Vec<&str> = s.splitn(3, '.').collect();

        match parts.len() {
            1 => Some(Self {
                port: None,
                app: None,
                node: parts[0].to_string(),
            }),
            2 => Some(Self {
                port: None,
                app: Some(parts[0].to_string()),
                node: parts[1].to_string(),
            }),
            3 => Some(Self {
                port: Some(parts[0].to_string()),
                app: Some(parts[1].to_string()),
                node: parts[2].to_string(),
            }),
            _ => None,
        }
    }

    /// Check if this address targets a specific node.
    pub fn is_for_node(&self, node_name: &str) -> bool {
        self.node == node_name
    }
}

impl fmt::Display for AmpAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(port) = &self.port {
            write!(f, "{port}.")?;
        }
        if let Some(app) = &self.app {
            write!(f, "{app}.")?;
        }
        write!(f, "{}.amp", self.node)
    }
}

// ── Validation ──

/// Known AMP header fields.
pub const KNOWN_HEADERS: &[&str] = &[
    "amp", "type", "id", "from", "to", "command", "args", "json",
    "reply-to", "ttl", "error", "timestamp", "rc",
];

/// Valid message types.
pub const VALID_TYPES: &[&str] = &["request", "response", "event", "stream"];

/// Validate an AMP message for protocol conformance.
///
/// Returns a list of warnings (not errors — AMP is permissive).
/// An empty Vec means the message is fully conformant.
pub fn validate(msg: &AmpMessage) -> Vec<String> {
    let mut warnings = Vec::new();

    // Empty messages are always valid
    if msg.is_empty_message() {
        return warnings;
    }

    // Check for unknown headers
    for key in msg.headers.keys() {
        if !KNOWN_HEADERS.contains(&key.as_str()) {
            warnings.push(format!("unknown header: {key}"));
        }
    }

    // Validate type field
    if let Some(msg_type) = msg.get("type") {
        if !VALID_TYPES.contains(&msg_type) {
            warnings.push(format!("invalid type: {msg_type}"));
        }
    }

    // Validate args is valid JSON
    if let Some(args) = msg.get("args") {
        if serde_json::from_str::<serde_json::Value>(args).is_err() {
            warnings.push("args is not valid JSON".to_string());
        }
    }

    // Validate json payload is valid JSON
    if let Some(json) = msg.get("json") {
        if serde_json::from_str::<serde_json::Value>(json).is_err() {
            warnings.push("json payload is not valid JSON".to_string());
        }
    }

    // Validate rc is numeric
    if let Some(rc) = msg.get("rc") {
        if rc.parse::<u8>().is_err() {
            warnings.push(format!("rc is not a valid integer: {rc}"));
        }
    }

    // Validate ttl is numeric
    if let Some(ttl) = msg.get("ttl") {
        if ttl.parse::<u32>().is_err() {
            warnings.push(format!("ttl is not a valid integer: {ttl}"));
        }
    }

    warnings
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;

    // -- Message parsing --

    #[test]
    fn round_trip_with_body() {
        let msg = AmpMessage::new()
            .with_header("command", "get")
            .with_header("rc", "0")
            .with_body(r#"{"key": "value"}"#);

        let bytes = msg.to_bytes();
        let raw = String::from_utf8(bytes).unwrap();
        let parsed = parse(&raw).unwrap();

        assert_eq!(parsed.get("command"), Some("get"));
        assert_eq!(parsed.get("rc"), Some("0"));
        assert_eq!(parsed.body, r#"{"key": "value"}"#);
    }

    #[test]
    fn round_trip_no_body() {
        let msg = AmpMessage::new()
            .with_header("command", "ping")
            .with_header("rc", "0");

        let bytes = msg.to_bytes();
        let raw = String::from_utf8(bytes).unwrap();
        let parsed = parse(&raw).unwrap();

        assert_eq!(parsed.get("command"), Some("ping"));
        assert_eq!(parsed.get("rc"), Some("0"));
        assert!(parsed.body.is_empty());
    }

    #[test]
    fn round_trip_error_response() {
        let msg = AmpMessage::new()
            .with_header("rc", "10")
            .with_header("error", "Port not found");

        let bytes = msg.to_bytes();
        let raw = String::from_utf8(bytes).unwrap();
        let parsed = parse(&raw).unwrap();

        assert_eq!(parsed.get("rc"), Some("10"));
        assert_eq!(parsed.get("error"), Some("Port not found"));
        assert!(parsed.body.is_empty());
    }

    #[test]
    fn parse_minimal() {
        let raw = "---\n---\n";
        let parsed = parse(raw).unwrap();
        assert!(parsed.headers.is_empty());
        assert!(parsed.body.is_empty());
        assert!(parsed.is_empty_message());
    }

    #[test]
    fn human_readable_output() {
        let msg = AmpMessage::new()
            .with_header("command", "status")
            .with_body(r#"{"unread": 3, "total": 1247}"#);

        let raw = String::from_utf8(msg.to_bytes()).unwrap();
        assert!(raw.starts_with("---\n"));
        assert!(raw.contains("command: status\n"));
        assert!(raw.contains("---\n{\"unread\": 3"));
    }

    #[test]
    fn display_trait() {
        let msg = AmpMessage::new()
            .with_header("command", "ping");
        let display = format!("{msg}");
        assert!(display.starts_with("---\n"));
        assert!(display.contains("command: ping"));
    }

    #[test]
    fn empty_message_constant() {
        let parsed = parse(EMPTY_MESSAGE).unwrap();
        assert!(parsed.is_empty_message());
        assert_eq!(AmpMessage::empty().to_wire(), EMPTY_MESSAGE);
    }

    #[test]
    fn command_constructor() {
        let msg = AmpMessage::command([
            ("command", "search".to_string()),
            ("from", "script.cosmix.cachyos.amp".to_string()),
        ]);
        assert_eq!(msg.command_name(), Some("search"));
        assert_eq!(msg.from_addr(), Some("script.cosmix.cachyos.amp"));
        assert!(msg.body.is_empty());
    }

    #[test]
    fn convenience_accessors() {
        let raw = "---\ntype: request\nfrom: cosmix-toot.cosmix.cachyos.amp\nto: cosmix-mail.cosmix.mko.amp\ncommand: status\nargs: {\"limit\": 10}\n---\n";
        let msg = parse(raw).unwrap();

        assert_eq!(msg.message_type(), Some("request"));
        assert_eq!(msg.from_addr(), Some("cosmix-toot.cosmix.cachyos.amp"));
        assert_eq!(msg.to_addr(), Some("cosmix-mail.cosmix.mko.amp"));
        assert_eq!(msg.command_name(), Some("status"));

        let args = msg.args().unwrap();
        assert_eq!(args["limit"], 10);
    }

    #[test]
    fn json_payload() {
        let raw = "---\njson: {\"count\": 12, \"unread\": 3}\n---\n";
        let msg = parse(raw).unwrap();
        let json = msg.json_payload().unwrap();
        assert_eq!(json["count"], 12);
        assert_eq!(json["unread"], 3);
    }

    // -- Address parsing --

    #[test]
    fn address_full() {
        let addr = AmpAddress::parse("cosmix-toot.cosmix.cachyos.amp").unwrap();
        assert_eq!(addr.port.as_deref(), Some("cosmix-toot"));
        assert_eq!(addr.app.as_deref(), Some("cosmix"));
        assert_eq!(addr.node, "cachyos");
        assert!(addr.is_for_node("cachyos"));
        assert!(!addr.is_for_node("mko"));
        assert_eq!(addr.to_string(), "cosmix-toot.cosmix.cachyos.amp");
    }

    #[test]
    fn address_app_only() {
        let addr = AmpAddress::parse("cosmix.mko.amp").unwrap();
        assert_eq!(addr.port, None);
        assert_eq!(addr.app.as_deref(), Some("cosmix"));
        assert_eq!(addr.node, "mko");
        assert_eq!(addr.to_string(), "cosmix.mko.amp");
    }

    #[test]
    fn address_node_only() {
        let addr = AmpAddress::parse("cachyos.amp").unwrap();
        assert_eq!(addr.port, None);
        assert_eq!(addr.app, None);
        assert_eq!(addr.node, "cachyos");
        assert_eq!(addr.to_string(), "cachyos.amp");
    }

    #[test]
    fn address_invalid() {
        assert!(AmpAddress::parse("cachyos.local").is_none());
        assert!(AmpAddress::parse("just-a-name").is_none());
        assert!(AmpAddress::parse("").is_none());
    }

    // -- Validation --

    #[test]
    fn validate_conformant_message() {
        let msg = parse("---\namp: 1\ntype: request\ncommand: ping\n---\n").unwrap();
        assert!(validate(&msg).is_empty());
    }

    #[test]
    fn validate_unknown_header() {
        let msg = parse("---\nfoo: bar\n---\n").unwrap();
        let warnings = validate(&msg);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("unknown header: foo"));
    }

    #[test]
    fn validate_invalid_type() {
        let msg = parse("---\ntype: banana\n---\n").unwrap();
        let warnings = validate(&msg);
        assert!(warnings.iter().any(|w| w.contains("invalid type")));
    }

    #[test]
    fn validate_empty_always_valid() {
        assert!(validate(&AmpMessage::empty()).is_empty());
    }

    #[test]
    fn validate_bad_rc() {
        let msg = parse("---\nrc: abc\n---\n").unwrap();
        let warnings = validate(&msg);
        assert!(warnings.iter().any(|w| w.contains("rc is not a valid integer")));
    }

    #[test]
    fn validate_bad_args_json() {
        let msg = parse("---\nargs: not-json\n---\n").unwrap();
        let warnings = validate(&msg);
        assert!(warnings.iter().any(|w| w.contains("args is not valid JSON")));
    }
}
