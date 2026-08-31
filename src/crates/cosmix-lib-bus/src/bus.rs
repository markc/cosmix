//! CosMix Agent Bus wire format — markdown frontmatter framing.
//!
//! Every Bus message is `---\n` delimited headers with an optional body:
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

// ── Bus Message ──

/// The minimum valid Bus message — heartbeat, ACK, or keepalive.
pub const EMPTY_MESSAGE: &str = "---\n---\n";

/// Maximum size of a single Bus message read from a local transport
/// (the native Unix-socket port). A message larger than this is rejected
/// rather than buffered in full, bounding a local memory-exhaustion DoS
/// (`read_to_end` is otherwise unbounded). 16 MiB is generous headroom
/// over the ~1 MiB norm for the largest legitimate Bus bodies
/// (subscription snapshots, stats — see `MAX_SNAPSHOT_BYTES`). The
/// mesh-facing WebSocket transport is bounded separately by the
/// broker's configured frame/message caps.
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

/// Maximum size of one WebSocket frame on the broker path. A Bus message is
/// written as a single WebSocket frame on that path, so its effective
/// per-message ceiling is `min(MAX_MESSAGE_BYTES, WS_MAX_FRAME_BYTES)`.
pub const WS_MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Maximum number of header lines parsed from a single Bus message.
/// Bounds the `BTreeMap` a hostile frame can force the parser to build —
/// a 16 MiB frame of 1-byte header lines would otherwise allocate
/// millions of map entries. A legitimate Bus message carries a handful
/// of headers; this leaves four orders of magnitude of slack. Excess
/// lines are recorded in the [`ParseReport`] (so `parse_strict` rejects
/// them) and parsing stops.
pub const MAX_HEADERS: usize = 4096;

/// A parsed Bus message: ordered headers + optional body.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BusMessage {
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

impl BusMessage {
    /// Create a new empty message.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create the empty Bus message (heartbeat/keepalive).
    pub fn empty() -> Self {
        Self::new()
    }

    /// Create a command message from header pairs (no body).
    pub fn command(headers: impl IntoIterator<Item = (&'static str, String)>) -> Self {
        Self {
            headers: headers
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
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

    /// Best-effort human-readable error detail for an error reply
    /// (`rc >= 10`). Looks in priority order: the `error` *header*, then
    /// an `{"error": "..."}` field in the JSON *body* (where cosmix
    /// daemons actually place their errors), then the raw body text, and
    /// only falls back to the literal `"unknown error"` when the reply
    /// carries no detail at all.
    ///
    /// Without the body fallback, every daemon that reports its error via
    /// the body — the common case (`json_error()` in cosmix-indexd,
    /// cosmix-maild, …) — surfaces to Bus callers as the opaque string
    /// `"unknown error"`, discarding the real cause. That opacity is what
    /// turned an `invalid type: floating point 2.0, expected usize`
    /// deserialize error into an undiagnosable `rc=10 unknown error`.
    pub fn error_message(&self) -> String {
        if let Some(e) = self.get("error") {
            return e.to_string();
        }
        let body = self.body.trim();
        if !body.is_empty() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(body)
                && let Some(e) = v.get("error").and_then(|e| e.as_str())
            {
                return e.to_string();
            }
            return body.to_string();
        }
        "unknown error".to_string()
    }

    // ── Display protocol accessors ──

    /// Get the panel/widget `id`.
    pub fn ui_id(&self) -> Option<&str> {
        self.get("id")
    }

    /// Get the `target` (for ui.style, ui.data, ui.remove).
    pub fn target(&self) -> Option<&str> {
        self.get("target")
    }

    /// Get the `parent` panel ID.
    pub fn parent(&self) -> Option<&str> {
        self.get("parent")
    }

    /// Get the `source` (for ui.event).
    pub fn source(&self) -> Option<&str> {
        self.get("source")
    }

    /// Check if this is a `ui.*` display protocol message.
    pub fn is_ui_command(&self) -> bool {
        self.command_name().is_some_and(|c| c.starts_with("ui."))
    }

    /// Serialize to Bus wire format bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.to_wire().into_bytes()
    }

    /// Serialize to Bus wire format string.
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

impl fmt::Display for BusMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_wire())
    }
}

/// Structured report from Bus parsing — lines and values the parser
/// could not strictly interpret. Per spec chapter 01 §6.4, canonical
/// Bus consumers MUST require this to be empty; legacy or external
/// content consumers MAY accept non-empty reports but SHOULD log them.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ParseReport {
    /// Header block lines that did not strictly match `^key: value\n` —
    /// typically YAML list items, indented continuations, nested-object
    /// children, anchors, or bare scalars. Entries are `(line_number,
    /// content)` with 1-indexed line numbers within the header block.
    /// The headers map still contains anything that split on `": "` for
    /// backward compatibility; this list surfaces what a strict parser
    /// would reject.
    pub skipped_lines: Vec<(usize, String)>,
    /// Header values starting with `[` or `{` that failed `serde_json`
    /// parsing per spec §5.5.1. Stored as `(key, raw_value)`. Typical
    /// cause: YAML flow syntax (`[A, B, C]` without quoted strings)
    /// rather than JSON syntax (`["A", "B", "C"]`). The raw value
    /// remains accessible in the headers map as a string.
    pub json_parse_errors: Vec<(String, String)>,
}

impl ParseReport {
    pub fn is_empty(&self) -> bool {
        self.skipped_lines.is_empty() && self.json_parse_errors.is_empty()
    }
}

/// Parse a Bus message and return what the parser could not strictly
/// interpret. The headers map is populated identically to `parse`;
/// the additional `ParseReport` surfaces non-compliant content so
/// callers can police strictness at their layer (spec §6.4).
pub fn parse_lenient(raw: &str) -> Result<(BusMessage, ParseReport)> {
    let content = raw.strip_prefix("---\n").ok_or_else(|| {
        anyhow::anyhow!(
            "Bus message must start with '---\\n', got: {:?}",
            &raw[..raw.len().min(40)]
        )
    })?;

    let (header_block, body) = match content.split_once("\n---\n") {
        Some((h, b)) => (h, b),
        None => {
            let h = content
                .strip_suffix("\n---\n")
                .or_else(|| content.strip_suffix("\n---"))
                .or_else(|| content.strip_suffix("---\n"))
                .or_else(|| content.strip_suffix("---"))
                .unwrap_or(content);
            (h, "")
        }
    };

    let mut headers = BTreeMap::new();
    let mut skipped_lines = Vec::new();
    let mut json_parse_errors = Vec::new();

    // Count every non-empty line we actually process — NOT distinct map
    // entries. Each processed line can push to `headers`, `skipped_lines`,
    // or `json_parse_errors`; capping the map's `len()` would let
    // duplicate keys (which overwrite) or repeated malformed/JSON-error
    // lines (which grow the report vecs) bypass the bound. Capping
    // processed lines bounds all three together.
    let mut processed = 0usize;
    for (idx, line) in header_block.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        // Bound the work a hostile frame can force. Beyond the cap, record
        // the overflow (so strict callers reject) and stop — the body is
        // already split out above.
        if processed >= MAX_HEADERS {
            skipped_lines.push((
                idx + 1,
                format!("header line count exceeds {MAX_HEADERS}; remaining lines skipped"),
            ));
            break;
        }
        processed += 1;
        match line.split_once(": ") {
            Some((k, v)) => {
                let key = k.trim().to_string();
                let val = v.trim().to_string();
                // Flag non-Bus-compliant lines (leading whitespace, empty
                // or whitespaced key) without blocking insertion — keeps
                // the existing `parse()` behaviour while surfacing the
                // issue to strict callers.
                if line.starts_with(|c: char| c.is_whitespace())
                    || key.is_empty()
                    || key.contains(char::is_whitespace)
                {
                    skipped_lines.push((idx + 1, line.to_string()));
                }
                // Per §5.5.1: values starting with `[` or `{` MAY be JSON.
                // Validate opportunistically; record failures without
                // discarding the raw value.
                if let Some(first) = val.chars().find(|c| !c.is_whitespace())
                    && (first == '[' || first == '{')
                    && serde_json::from_str::<serde_json::Value>(&val).is_err()
                {
                    json_parse_errors.push((key.clone(), val.clone()));
                }
                headers.insert(key, val);
            }
            None => {
                // No `": "` on the line — comment (`# …`), list item
                // (`- …`), bare scalar, or YAML anchor. Record and skip.
                skipped_lines.push((idx + 1, line.to_string()));
            }
        }
    }

    Ok((
        BusMessage {
            headers,
            body: body.trim_end().to_string(),
        },
        ParseReport {
            skipped_lines,
            json_parse_errors,
        },
    ))
}

/// Parse a Bus message, returning an error if any header line does
/// not strictly conform or any `[`/`{` value fails JSON parsing.
/// Use for canonical Bus content — wire messages, cosmix-native doc
/// headers. See spec §6.4.
pub fn parse_strict(raw: &str) -> Result<BusMessage> {
    let (msg, report) = parse_lenient(raw)?;
    if !report.is_empty() {
        let first = report
            .skipped_lines
            .iter()
            .map(|(n, l)| format!("line {n}: {l:?}"))
            .chain(
                report
                    .json_parse_errors
                    .iter()
                    .map(|(k, v)| format!("json-parse {k}: {v:?}")),
            )
            .next()
            .unwrap_or_default();
        anyhow::bail!(
            "Bus message non-compliant: {} skipped line(s), {} json parse error(s); first: {first}",
            report.skipped_lines.len(),
            report.json_parse_errors.len(),
        );
    }
    Ok(msg)
}

/// Parse a Bus message from raw text. Non-compliant lines are silently
/// skipped — preserved for backward compatibility. For canonical content
/// use `parse_strict`; for legacy or external content use `parse_lenient`
/// to inspect what was skipped. See spec §6.4.
pub fn parse(raw: &str) -> Result<BusMessage> {
    parse_lenient(raw).map(|(msg, _report)| msg)
}

// ── Transport helpers (native only) ──

#[cfg(feature = "native")]
mod transport {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Read a Bus message from a Unix stream (reads until EOF).
    ///
    /// The sender must shut down their write side to signal EOF.
    pub async fn read_from_stream(stream: &mut tokio::net::UnixStream) -> Result<BusMessage> {
        let mut buf = Vec::with_capacity(4096);

        // Read with a timeout (hung clients) AND a byte cap (memory DoS):
        // `take` one byte past the limit so an over-cap frame still reads
        // enough to be detected, then reject. Without the cap, a local
        // peer could force an unbounded `read_to_end` allocation.
        let mut limited = stream.take(MAX_MESSAGE_BYTES as u64 + 1);
        match tokio::time::timeout(
            std::time::Duration::from_secs(10),
            limited.read_to_end(&mut buf),
        )
        .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => anyhow::bail!("Read error: {e}"),
            Err(_) => anyhow::bail!("Bus read timed out (10s)"),
        }

        if buf.is_empty() {
            anyhow::bail!("Empty Bus message (no data received)");
        }
        if buf.len() > MAX_MESSAGE_BYTES {
            anyhow::bail!("Bus message exceeds {MAX_MESSAGE_BYTES} byte limit");
        }

        let raw = String::from_utf8(buf)?;
        parse(&raw)
    }

    /// Write a Bus message to a Unix stream.
    pub async fn write_to_stream(
        stream: &mut tokio::net::UnixStream,
        msg: &BusMessage,
    ) -> Result<()> {
        stream.write_all(&msg.to_bytes()).await?;
        Ok(())
    }
}

#[cfg(feature = "native")]
pub use transport::{read_from_stream, write_to_stream};

// ── Bus Address ──

/// Maximum length of a single DNS-style label.
const MAX_LABEL_LEN: usize = 63;

/// Maximum total length of a Bus address (including `@<mesh-fqdn>` suffix).
const MAX_ADDRESS_LEN: usize = 253;

/// A local Bus address, per SPEC 01 §4.1.
///
/// Canonical forms (`.bus` suffix optional on 2-/3-label forms):
/// - `<service>.<node>[.bus]` — service on a node
/// - `<sub>.<service>.<node>[.bus]` — sub-protocol/instance on a service on a node
/// - `<node>.bus` — the node itself (its broker; service implicit `noded`)
///
/// The `<sub>` slot is opaque to the broker: the broker routes by
/// `<service>.<node>`, and the destination service interprets `<sub>` to
/// demultiplex internal endpoints (e.g. `maild` treats `imap` as the IMAP
/// sub-protocol; `disp-skia` treats `editor` as a window/instance ID).
///
/// Bare `<service>` (no dot, no `.bus` suffix) is NOT a parseable address;
/// it is a local-only shorthand the caller hands to the broker registry
/// directly. See `BusTarget::parse` for the full target shape including
/// cross-mesh.
///
/// Examples:
/// ```
/// # use cosmix_bus::bus::{BusAddress, BusTarget};
/// let t = BusTarget::parse("imap.maild.alpha.bus").unwrap();
/// let addr = t.local();
/// assert_eq!(addr.sub.as_deref(), Some("imap"));
/// assert_eq!(addr.service.as_deref(), Some("maild"));
/// assert_eq!(addr.node, "alpha");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BusAddress {
    /// Optional sub-protocol/instance label. Opaque to the broker;
    /// interpreted by the destination service.
    pub sub: Option<String>,
    /// Service name. `None` only for the `<node>.bus` form, which
    /// implicitly addresses the node's broker (noded).
    pub service: Option<String>,
    /// Node name. Always present.
    pub node: String,
}

/// A resolved Bus routing target, per SPEC 01 §4.
///
/// `Local` is the in-mesh form (no `@`). `CrossMesh` is the cross-mesh form
/// (`<local-bus>@<mesh-fqdn>`); routers MUST refuse this with `cross-mesh
/// routing not implemented` until federation transport exists.
///
/// The enum makes the routing distinction type-level: every router branch
/// must explicitly handle (or refuse) `CrossMesh` — a passive `mesh:
/// Option<String>` field on `BusAddress` would allow code to accidentally
/// deliver a cross-mesh address to a local service whose node name happened
/// to match.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BusTarget {
    /// In-mesh address.
    Local(BusAddress),
    /// Cross-mesh address. Reserved at the parser; refused at the router
    /// until federation transport is designed.
    CrossMesh {
        /// The mesh-local part (left of `@`).
        local: BusAddress,
        /// The destination mesh FQDN (right of `@`). Strict
        /// IDNA-canonical: lowercase ASCII, contains at least one `.`,
        /// labels 1..=63 chars from `[a-z0-9-]` with no leading/trailing
        /// hyphen, no `xn--` punycode pending homograph review.
        mesh_fqdn: String,
    },
}

impl BusTarget {
    /// Parse a Bus target string per SPEC 01 §4.
    ///
    /// Returns `None` for inputs that are not Bus addresses (bare service
    /// shorthand without `.bus` and without dots, malformed labels, more
    /// than three left-side labels, invalid FQDN on the right of `@`,
    /// etc.). Callers fall back to direct service-registry lookup when
    /// this returns `None`.
    pub fn parse(s: &str) -> Option<Self> {
        if s.is_empty() || s.len() > MAX_ADDRESS_LEN {
            return None;
        }

        // Split on `@` for cross-mesh. Exactly one `@` permitted.
        let (local_str, mesh_fqdn) = match s.split_once('@') {
            Some((l, r)) => {
                if r.contains('@') || r.is_empty() || l.is_empty() {
                    return None;
                }
                (l, Some(r))
            }
            None => (s, None),
        };

        let local = BusAddress::parse_local(local_str)?;

        match mesh_fqdn {
            None => Some(BusTarget::Local(local)),
            Some(fqdn) => {
                let normalised = validate_mesh_fqdn(fqdn)?;
                Some(BusTarget::CrossMesh {
                    local,
                    mesh_fqdn: normalised,
                })
            }
        }
    }

    /// Borrow the local component regardless of variant. Useful when a
    /// caller has already verified (or refused) the `CrossMesh` case.
    pub fn local(&self) -> &BusAddress {
        match self {
            BusTarget::Local(a) => a,
            BusTarget::CrossMesh { local, .. } => local,
        }
    }

    /// True if this is a cross-mesh target. Routers MUST check this before
    /// dispatching and refuse with `cross-mesh routing not implemented`.
    pub fn is_cross_mesh(&self) -> bool {
        matches!(self, BusTarget::CrossMesh { .. })
    }
}

impl BusAddress {
    /// Parse a *local* (no `@`) Bus address per SPEC 01 §4.1. Prefer
    /// `BusTarget::parse` which also handles the cross-mesh form.
    ///
    /// Accepts (with optional `.bus` suffix on 2-/3-label forms):
    /// - `<node>.bus` — node only (service implicit, sub absent)
    /// - `<service>.<node>` or `<service>.<node>.bus`
    /// - `<sub>.<service>.<node>` or `<sub>.<service>.<node>.bus`
    pub fn parse_local(s: &str) -> Option<Self> {
        if s.is_empty() || s.contains('@') {
            return None;
        }

        // Strip optional `.bus` suffix. Remember whether the suffix was
        // explicit, since single-label inputs require it (`alpha.bus`
        // is the node form; bare `alpha` is service shorthand and is
        // NOT a parseable address).
        let (stem, had_bus_suffix) = match s.strip_suffix(".bus") {
            Some(stripped) => (stripped, true),
            None => (s, false),
        };

        if stem.is_empty() {
            return None;
        }

        let parts: Vec<&str> = stem.split('.').collect();

        // Reject empty labels (`.foo`, `foo..bar`, `foo.`) and >3 labels.
        if parts.iter().any(|p| p.is_empty()) || parts.len() > 3 {
            return None;
        }

        // Validate each label as a DNS-style ASCII label.
        for part in &parts {
            if !is_valid_label(part) {
                return None;
            }
        }

        match parts.len() {
            1 => {
                // Single label is the node-only form `<node>.bus` and
                // requires the explicit `.bus` suffix. Bare `<service>`
                // is shorthand and must not parse as an address.
                if !had_bus_suffix {
                    return None;
                }
                Some(Self {
                    sub: None,
                    service: None,
                    node: parts[0].to_string(),
                })
            }
            2 => Some(Self {
                sub: None,
                service: Some(parts[0].to_string()),
                node: parts[1].to_string(),
            }),
            3 => Some(Self {
                sub: Some(parts[0].to_string()),
                service: Some(parts[1].to_string()),
                node: parts[2].to_string(),
            }),
            _ => None,
        }
    }

    /// Check if this address targets a specific node.
    pub fn is_for_node(&self, node_name: &str) -> bool {
        self.node == node_name
    }

    /// Resolve the service name for routing. `None` indicates the node's
    /// broker (the `<node>.bus` form); callers typically map this to
    /// `"noded"`.
    pub fn service_name(&self) -> Option<&str> {
        self.service.as_deref()
    }
}

impl fmt::Display for BusAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(sub) = &self.sub {
            write!(f, "{sub}.")?;
        }
        if let Some(service) = &self.service {
            write!(f, "{service}.")?;
        }
        write!(f, "{}.bus", self.node)
    }
}

impl fmt::Display for BusTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BusTarget::Local(a) => write!(f, "{a}"),
            BusTarget::CrossMesh { local, mesh_fqdn } => {
                write!(f, "{local}@{mesh_fqdn}")
            }
        }
    }
}

/// Validate a DNS-style label per SPEC 01 §4.1: 1..=63 ASCII characters
/// from `[a-z0-9-]`, not starting or ending with `-`.
///
/// This is the fleet-wide label grammar authority; inventory and routing
/// validators must use it rather than maintaining a parallel grammar.
pub fn is_valid_label(label: &str) -> bool {
    let bytes = label.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_LABEL_LEN {
        return false;
    }
    if bytes[0] == b'-' || *bytes.last().unwrap() == b'-' {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// Validate a mesh FQDN (right-hand side of `@`) per SPEC 01 §4.2.
/// Returns the normalised form (currently identical to input on success)
/// or `None` if invalid.
///
/// Rules: total ≤ 253 chars, contains at least one `.`, each label
/// 1..=63 chars from `[a-z0-9-]` with no leading/trailing hyphen, no
/// trailing dot, no `xn--` punycode (pending homograph review).
fn validate_mesh_fqdn(fqdn: &str) -> Option<String> {
    if fqdn.is_empty() || fqdn.len() > MAX_ADDRESS_LEN {
        return None;
    }
    if !fqdn.contains('.') || fqdn.ends_with('.') {
        return None;
    }
    for label in fqdn.split('.') {
        if !is_valid_label(label) {
            return None;
        }
        if label.starts_with("xn--") {
            return None;
        }
    }
    Some(fqdn.to_string())
}

// ── Validation ──

/// Known Bus header fields.
pub const KNOWN_HEADERS: &[&str] = &[
    // Core protocol
    "bus",
    "type",
    "id",
    "from",
    "to",
    "command",
    "args",
    "json",
    "reply-to",
    "ttl",
    "error",
    "timestamp",
    "rc",
    // Display protocol — window
    "parent",
    "title",
    "width",
    "height",
    "position",
    "decorations",
    "layer",
    "sticky",
    // Display protocol — layout
    "layout",
    "gap",
    "padding",
    "align",
    "scrollable",
    "overflow",
    // Display protocol — style
    "background",
    "text_color",
    "border_color",
    "border_width",
    "border_radius",
    "font_size",
    "opacity",
    // Display protocol — targeting
    "target",
    "source",
    "name",
    // Display protocol — permissions (federated)
    "source_peer",
    "permissions",
];

/// Valid message types.
pub const VALID_TYPES: &[&str] = &["request", "response", "event", "stream"];

/// Validate a Bus message for protocol conformance.
///
/// Returns a list of warnings (not errors — Bus is permissive).
/// An empty Vec means the message is fully conformant.
pub fn validate(msg: &BusMessage) -> Vec<String> {
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
    if let Some(msg_type) = msg.get("type")
        && !VALID_TYPES.contains(&msg_type)
    {
        warnings.push(format!("invalid type: {msg_type}"));
    }

    // Validate args is valid JSON
    if let Some(args) = msg.get("args")
        && serde_json::from_str::<serde_json::Value>(args).is_err()
    {
        warnings.push("args is not valid JSON".to_string());
    }

    // Validate json payload is valid JSON
    if let Some(json) = msg.get("json")
        && serde_json::from_str::<serde_json::Value>(json).is_err()
    {
        warnings.push("json payload is not valid JSON".to_string());
    }

    // Validate rc is numeric
    if let Some(rc) = msg.get("rc")
        && rc.parse::<u8>().is_err()
    {
        warnings.push(format!("rc is not a valid integer: {rc}"));
    }

    // Validate ttl is numeric
    if let Some(ttl) = msg.get("ttl")
        && ttl.parse::<u32>().is_err()
    {
        warnings.push(format!("ttl is not a valid integer: {ttl}"));
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
        let msg = BusMessage::new()
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
        let msg = BusMessage::new()
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
        let msg = BusMessage::new()
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
        let msg = BusMessage::new()
            .with_header("command", "status")
            .with_body(r#"{"unread": 3, "total": 1247}"#);

        let raw = String::from_utf8(msg.to_bytes()).unwrap();
        assert!(raw.starts_with("---\n"));
        assert!(raw.contains("command: status\n"));
        assert!(raw.contains("---\n{\"unread\": 3"));
    }

    #[test]
    fn display_trait() {
        let msg = BusMessage::new().with_header("command", "ping");
        let display = format!("{msg}");
        assert!(display.starts_with("---\n"));
        assert!(display.contains("command: ping"));
    }

    #[test]
    fn empty_message_constant() {
        let parsed = parse(EMPTY_MESSAGE).unwrap();
        assert!(parsed.is_empty_message());
        assert_eq!(BusMessage::empty().to_wire(), EMPTY_MESSAGE);
    }

    #[test]
    fn command_constructor() {
        let msg = BusMessage::command([
            ("command", "search".to_string()),
            ("from", "mix.alpha.bus".to_string()),
        ]);
        assert_eq!(msg.command_name(), Some("search"));
        assert_eq!(msg.from_addr(), Some("mix.alpha.bus"));
        assert!(msg.body.is_empty());
    }

    #[test]
    fn convenience_accessors() {
        let raw = "---\ntype: request\nfrom: mix.alpha.bus\nto: maild.delta.bus\ncommand: status\nargs: {\"limit\": 10}\n---\n";
        let msg = parse(raw).unwrap();

        assert_eq!(msg.message_type(), Some("request"));
        assert_eq!(msg.from_addr(), Some("mix.alpha.bus"));
        assert_eq!(msg.to_addr(), Some("maild.delta.bus"));
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

    // -- Parser robustness (spec §6.4) --

    #[test]
    fn parse_lenient_clean_message() {
        let raw = "---\ncommand: status\nrc: 0\n---\n";
        let (msg, report) = parse_lenient(raw).unwrap();
        assert_eq!(msg.get("command"), Some("status"));
        assert!(
            report.is_empty(),
            "clean Bus should report empty: {report:?}"
        );
    }

    #[test]
    fn parse_lenient_yaml_list() {
        // YAML indented list — three non-Bus lines: the bare-colon
        // `draws_from:` (Bus grammar requires `key: value` with the
        // space-delimited separator) and both indented list items.
        // All three surface in skipped_lines so the caller can see
        // that structured data was lost.
        let raw = "---\ntitle: foo\ndraws_from:\n  - A\n  - B\n---\n";
        let (msg, report) = parse_lenient(raw).unwrap();
        assert_eq!(msg.get("title"), Some("foo"));
        assert_eq!(
            msg.get("draws_from"),
            None,
            "bare-colon key does not match Bus `key: value` grammar"
        );
        assert_eq!(
            report.skipped_lines.len(),
            3,
            "expected 3 skipped lines: draws_from: plus two list items"
        );
        assert!(report.skipped_lines.iter().any(|(_, l)| l == "draws_from:"));
        assert!(report.skipped_lines.iter().any(|(_, l)| l.contains("- A")));
        assert!(report.skipped_lines.iter().any(|(_, l)| l.contains("- B")));
    }

    #[test]
    fn parse_lenient_yaml_comment() {
        let raw = "---\n# this is a comment\ncommand: ping\n---\n";
        let (msg, report) = parse_lenient(raw).unwrap();
        assert_eq!(msg.get("command"), Some("ping"));
        assert_eq!(report.skipped_lines.len(), 1);
        assert!(report.skipped_lines[0].1.starts_with('#'));
    }

    #[test]
    fn parse_lenient_json_value_valid() {
        let raw = "---\nargs: {\"limit\": 10}\n---\n";
        let (msg, report) = parse_lenient(raw).unwrap();
        assert_eq!(msg.get("args"), Some(r#"{"limit": 10}"#));
        assert!(report.json_parse_errors.is_empty());
    }

    #[test]
    fn parse_lenient_yaml_flow_fails_json() {
        // YAML flow syntax without quoted strings is NOT valid JSON.
        let raw = "---\ndraws_from: [A, B, C]\n---\n";
        let (msg, report) = parse_lenient(raw).unwrap();
        // Raw value preserved as string — no silent misinterpretation.
        assert_eq!(msg.get("draws_from"), Some("[A, B, C]"));
        assert_eq!(report.json_parse_errors.len(), 1);
        assert_eq!(report.json_parse_errors[0].0, "draws_from");
        assert_eq!(report.json_parse_errors[0].1, "[A, B, C]");
    }

    #[test]
    fn parse_strict_accepts_clean() {
        let raw = "---\ncommand: ping\nrc: 0\n---\n";
        let msg = parse_strict(raw).unwrap();
        assert_eq!(msg.get("command"), Some("ping"));
    }

    #[test]
    fn parse_strict_rejects_yaml_list() {
        let raw = "---\ntitle: foo\ndraws_from:\n  - A\n---\n";
        let err = parse_strict(raw).unwrap_err();
        assert!(
            err.to_string().contains("non-compliant"),
            "expected 'non-compliant' in error, got: {err}"
        );
    }

    #[test]
    fn parse_strict_rejects_yaml_flow() {
        let raw = "---\ndraws_from: [A, B, C]\n---\n";
        let err = parse_strict(raw).unwrap_err();
        assert!(
            err.to_string().contains("json parse error")
                || err.to_string().contains("non-compliant"),
            "expected JSON error in: {err}"
        );
    }

    #[test]
    fn parse_backward_compatible() {
        // `parse` must return identical headers to `parse_lenient`
        // regardless of non-compliant content.
        let raw = "---\nkey: val\n  indented: foo\n---\n";
        let a = parse(raw).unwrap();
        let (b, _report) = parse_lenient(raw).unwrap();
        assert_eq!(a.headers, b.headers);
    }

    // -- Address parsing (SPEC 01 §4) --

    #[test]
    fn address_sub_service_node() {
        let addr = BusAddress::parse_local("imap.maild.alpha.bus").unwrap();
        assert_eq!(addr.sub.as_deref(), Some("imap"));
        assert_eq!(addr.service.as_deref(), Some("maild"));
        assert_eq!(addr.node, "alpha");
        assert!(addr.is_for_node("alpha"));
        assert!(!addr.is_for_node("delta"));
        assert_eq!(addr.to_string(), "imap.maild.alpha.bus");
    }

    #[test]
    fn address_service_node_with_bus() {
        let addr = BusAddress::parse_local("maild.alpha.bus").unwrap();
        assert_eq!(addr.sub, None);
        assert_eq!(addr.service.as_deref(), Some("maild"));
        assert_eq!(addr.node, "alpha");
        assert_eq!(addr.to_string(), "maild.alpha.bus");
    }

    #[test]
    fn address_service_node_without_bus() {
        // <service>.<node> without `.bus` is equivalent to <service>.<node>.bus.
        let addr = BusAddress::parse_local("maild.alpha").unwrap();
        assert_eq!(addr.sub, None);
        assert_eq!(addr.service.as_deref(), Some("maild"));
        assert_eq!(addr.node, "alpha");
        assert_eq!(addr.to_string(), "maild.alpha.bus");
    }

    #[test]
    fn address_node_only() {
        let addr = BusAddress::parse_local("alpha.bus").unwrap();
        assert_eq!(addr.sub, None);
        assert_eq!(addr.service, None);
        assert_eq!(addr.node, "alpha");
        assert_eq!(addr.to_string(), "alpha.bus");
        // service_name() returns None — callers (e.g. noded routing)
        // map that to the implicit broker service "noded".
        assert_eq!(addr.service_name(), None);
    }

    #[test]
    fn address_bare_service_rejected() {
        // Bare `<service>` (no dot, no `.bus`) is service-registry
        // shorthand handed to the broker directly, not a Bus address.
        assert!(BusAddress::parse_local("noded").is_none());
        assert!(BusAddress::parse_local("maild").is_none());
    }

    #[test]
    fn address_invalid() {
        // No address bits at all.
        assert!(BusAddress::parse_local("").is_none());
        // Empty labels.
        assert!(BusAddress::parse_local(".bus").is_none());
        assert!(BusAddress::parse_local("a..b.bus").is_none());
        assert!(BusAddress::parse_local(".a.b").is_none());
        assert!(BusAddress::parse_local("a.b.").is_none());
        // >3 left-side labels — splitn(3) silent-pad bug regression.
        assert!(BusAddress::parse_local("a.b.c.d.bus").is_none());
        // Invalid label chars.
        assert!(BusAddress::parse_local("UPPER.alpha.bus").is_none());
        assert!(BusAddress::parse_local("with_underscore.alpha.bus").is_none());
        assert!(BusAddress::parse_local("-leading.alpha.bus").is_none());
        assert!(BusAddress::parse_local("trailing-.alpha.bus").is_none());
    }

    // -- Target parsing (local + cross-mesh) --

    #[test]
    fn target_local() {
        let t = BusTarget::parse("maild.alpha.bus").unwrap();
        assert!(!t.is_cross_mesh());
        assert_eq!(t.local().node, "alpha");
        assert_eq!(t.local().service.as_deref(), Some("maild"));
    }

    #[test]
    fn target_cross_mesh() {
        let t = BusTarget::parse("maild.delta.bus@example.org").unwrap();
        assert!(t.is_cross_mesh());
        match t {
            BusTarget::CrossMesh { local, mesh_fqdn } => {
                assert_eq!(local.service.as_deref(), Some("maild"));
                assert_eq!(local.node, "delta");
                assert_eq!(mesh_fqdn, "example.org");
            }
            _ => panic!("expected CrossMesh"),
        }
        assert_eq!(
            BusTarget::parse("maild.delta.bus@example.org")
                .unwrap()
                .to_string(),
            "maild.delta.bus@example.org"
        );
    }

    #[test]
    fn target_cross_mesh_invalid() {
        // Empty LHS.
        assert!(BusTarget::parse("@example.org").is_none());
        // Empty RHS.
        assert!(BusTarget::parse("maild.delta.bus@").is_none());
        // RHS without `.` (single-label).
        assert!(BusTarget::parse("maild.delta.bus@localmesh").is_none());
        // Multiple `@`.
        assert!(BusTarget::parse("a@b@c.org").is_none());
        // Punycode rejected pending homograph review.
        assert!(BusTarget::parse("maild.delta.bus@xn--example.org").is_none());
        // Invalid FQDN label.
        assert!(BusTarget::parse("maild.delta.bus@Bad.Org").is_none());
        // Trailing dot.
        assert!(BusTarget::parse("maild.delta.bus@example.org.").is_none());
    }

    #[test]
    fn target_oversized_address_rejected() {
        let huge = format!("a.b.c.bus@{}", "x".repeat(300));
        assert!(BusTarget::parse(&huge).is_none());
    }

    #[test]
    fn target_label_max_len() {
        let label63 = "a".repeat(63);
        let label64 = "a".repeat(64);
        assert!(BusTarget::parse(&format!("{label63}.alpha.bus")).is_some());
        assert!(BusTarget::parse(&format!("{label64}.alpha.bus")).is_none());
    }

    // -- Validation --

    #[test]
    fn validate_conformant_message() {
        let msg = parse("---\nbus: 1\ntype: request\ncommand: ping\n---\n").unwrap();
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
        assert!(validate(&BusMessage::empty()).is_empty());
    }

    #[test]
    fn validate_bad_rc() {
        let msg = parse("---\nrc: abc\n---\n").unwrap();
        let warnings = validate(&msg);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("rc is not a valid integer"))
        );
    }

    #[test]
    fn validate_bad_args_json() {
        let msg = parse("---\nargs: not-json\n---\n").unwrap();
        let warnings = validate(&msg);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("args is not valid JSON"))
        );
    }

    #[test]
    fn error_message_prefers_header() {
        let mut msg = BusMessage::new().with_header("error", "header wins");
        msg.body = r#"{"error":"body loses"}"#.to_string();
        assert_eq!(msg.error_message(), "header wins");
    }

    #[test]
    fn error_message_falls_back_to_json_body() {
        // The common cosmix-daemon case: error only in the JSON body.
        // This is the regression guard for the rc=10 "unknown error"
        // opacity — the body's "error" field must surface.
        let msg = BusMessage::new()
            .with_body(r#"{"error":"invalid type: floating point `2.0`, expected usize"}"#);
        assert_eq!(
            msg.error_message(),
            "invalid type: floating point `2.0`, expected usize"
        );
    }

    #[test]
    fn error_message_falls_back_to_raw_body() {
        let msg = BusMessage::new().with_body("plain text failure");
        assert_eq!(msg.error_message(), "plain text failure");
    }

    #[test]
    fn error_message_unknown_when_empty() {
        assert_eq!(BusMessage::new().error_message(), "unknown error");
    }

    #[test]
    fn header_count_is_capped() {
        // A frame with far more header lines than MAX_HEADERS must not
        // build an unbounded map: parsing stops at the cap, records the
        // overflow, and strict parse rejects it.
        let mut raw = String::from("---\n");
        for i in 0..(MAX_HEADERS + 50) {
            raw.push_str(&format!("k{i}: v\n"));
        }
        raw.push_str("---\n");
        let (msg, report) = parse_lenient(&raw).unwrap();
        assert_eq!(
            msg.headers.len(),
            MAX_HEADERS,
            "header map is bounded by MAX_HEADERS"
        );
        assert!(
            !report.is_empty(),
            "overflow is recorded so strict callers reject"
        );
        assert!(
            parse_strict(&raw).is_err(),
            "strict parse rejects an over-cap frame"
        );
    }

    #[test]
    fn duplicate_keys_do_not_bypass_cap() {
        // Duplicate keys overwrite in the map (len stays 1) and malformed
        // lines grow the report vecs — the cap must count PROCESSED LINES,
        // not map entries, or these bypass the bound entirely.
        let mut raw = String::from("---\n");
        for _ in 0..(MAX_HEADERS + 100) {
            raw.push_str("dup: v\n"); // same key every time → map len 1
        }
        raw.push_str("---\n");
        let (msg, report) = parse_lenient(&raw).unwrap();
        assert_eq!(msg.headers.len(), 1, "duplicate keys collapse to one entry");
        assert!(
            report
                .skipped_lines
                .iter()
                .any(|(_, l)| l.contains("exceeds")),
            "the line-count cap tripped despite the map staying small"
        );
    }

    #[test]
    fn headers_under_cap_parse_fully() {
        // A normal handful-of-headers message is unaffected by the cap.
        let raw = "---\ncommand: get\nrc: 0\nfrom: node1\n---\nbody";
        let (msg, report) = parse_lenient(raw).unwrap();
        assert_eq!(msg.headers.len(), 3);
        assert!(report.is_empty());
        assert_eq!(msg.body, "body");
    }
}
