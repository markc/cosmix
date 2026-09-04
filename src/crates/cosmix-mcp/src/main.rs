//! cosmix-mcp — MCP server bridging Claude Code to the cosmix appmesh.
//!
//! Register with: `claude mcp add cosmix-mcp -- ~/.local/bin/cosmix-mcp`
//!
//! Broker connection is lazy — the MCP server starts immediately and only
//! connects to cosmix-noded on the first tool call, avoiding startup timeouts.
//!
//! Skills tools connect to cosmix-indexd for semantic skill storage and
//! retrieval. The learning loop: retrieve before tasks, capture after.

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{ServerHandler, ServiceExt, tool, tool_router};
use serde::Deserialize;
use tokio::sync::OnceCell;

struct CosmixMcp {
    noded: OnceCell<Arc<cosmix_client::NodedClient>>,
    tool_router: rmcp::handler::server::tool::ToolRouter<Self>,
    metrics: Arc<Metrics>,
}

// ---- Observability: per-tool-call metrics + arg redaction ----

/// Newest-N tool calls kept in memory for the `mcp_status` tool.
const RECENT_CAP: usize = 50;

/// Total-summary cap for the always-on redacted arg string (chars, not bytes).
const ARGS_SUMMARY_CAP: usize = 400;

/// Total-summary cap for the always-on redacted result string (chars).
const RESULT_SUMMARY_CAP: usize = 400;

// Note: there is deliberately no per-key allowlist. `summarize_args` runs on
// the *raw, unvalidated* MCP request before any tool-specific deserialization,
// and the MCP wire is an untrusted boundary (a caller can place a secret under
// any key — even nominally "safe" ones like `command`/`name`/`id`). The only
// always-on-safe rule is therefore value-type based: no string value is ever
// logged verbatim. Full string values are available solely under the explicit
// `RUST_LOG=cosmix_mcp=trace` opt-in.

struct CallRecord {
    tool: String,
    at: chrono::DateTime<chrono::Utc>,
    ms: u64,
    ok: bool,
}

#[derive(Default)]
struct MetricsInner {
    total: u64,
    errors: u64,
    counts: std::collections::BTreeMap<String, u64>,
    recent: std::collections::VecDeque<CallRecord>,
}

struct Metrics {
    started: chrono::DateTime<chrono::Utc>,
    started_at: std::time::Instant,
    inner: std::sync::Mutex<MetricsInner>,
}

impl Metrics {
    fn new() -> Self {
        Self {
            started: chrono::Utc::now(),
            started_at: std::time::Instant::now(),
            inner: std::sync::Mutex::new(MetricsInner::default()),
        }
    }

    fn record(&self, tool: &str, ms: u64, ok: bool) {
        // Metrics are advisory — a poisoned lock must not take down a tool call.
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.total += 1;
        if !ok {
            g.errors += 1;
        }
        *g.counts.entry(tool.to_string()).or_insert(0) += 1;
        g.recent.push_back(CallRecord {
            tool: tool.to_string(),
            at: chrono::Utc::now(),
            ms,
            ok,
        });
        while g.recent.len() > RECENT_CAP {
            g.recent.pop_front();
        }
    }

    fn snapshot_json(&self, broker_connected: bool) -> serde_json::Value {
        let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let recent: Vec<serde_json::Value> = g
            .recent
            .iter()
            .rev()
            .map(|r| {
                serde_json::json!({
                    "tool": r.tool,
                    "at": r.at.to_rfc3339(),
                    "ms": r.ms,
                    "ok": r.ok,
                })
            })
            .collect();
        serde_json::json!({
            "started": self.started.to_rfc3339(),
            "uptime_secs": self.started_at.elapsed().as_secs(),
            "broker_connected": broker_connected,
            "total_calls": g.total,
            "error_calls": g.errors,
            "per_tool": g.counts,
            "recent": recent,
        })
    }
}

/// Truncate to at most `max` chars (panic-safe on UTF-8 boundaries), appending
/// an ellipsis when truncation occurred.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Scrub an untrusted argument *key* before it reaches the log line. Keys are
/// schema parameter names in well-formed requests, but the MCP wire is
/// untrusted: control bytes are replaced (defeats ANSI / Trojan-Source log
/// injection) and the key is length-bounded (caps a pathological key). Keys
/// are *not* redacted to opacity — they are the only thing that keeps the
/// always-on line legible, and a secret-in-a-key requires a deliberately
/// malformed request whose payload is bounded here anyway.
fn safe_key(k: &str) -> String {
    let scrubbed: String = k
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect();
    truncate_chars(&scrubbed, 40)
}

/// Render request arguments as a compact, secret-safe one-liner for the
/// always-on log. **No string value is ever logged verbatim** — every string
/// collapses to `<str:N chars>` regardless of key, because this runs on the
/// raw unvalidated request and the MCP wire is untrusted. Keys are scrubbed
/// and length-bounded via [`safe_key`]. Arrays/objects collapse to shape only;
/// numbers/bools/null cannot carry a secret string and are shown to keep the
/// line useful. Full string values are available only via the
/// `RUST_LOG=cosmix_mcp=trace` gate in `call_tool`.
fn summarize_args(args: Option<&serde_json::Map<String, serde_json::Value>>) -> String {
    let Some(map) = args else {
        return "{}".to_string();
    };
    let mut parts: Vec<String> = Vec::with_capacity(map.len());
    for (k, v) in map {
        let rendered = match v {
            serde_json::Value::Null => "null".to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::String(s) => format!("<str:{} chars>", s.chars().count()),
            serde_json::Value::Array(a) => format!("<arr:{}>", a.len()),
            serde_json::Value::Object(o) => format!("<obj:{} keys>", o.len()),
        };
        parts.push(format!("{}={rendered}", safe_key(k)));
    }
    truncate_chars(&format!("{{{}}}", parts.join(", ")), ARGS_SUMMARY_CAP)
}

/// Redacted, shape-only summary of a tool's return value, for the always-on
/// `tool done` log line. Same redact-by-default discipline as
/// [`summarize_args`]: no text body, base64 blob, resource URI, or
/// structured value from tool output is ever rendered verbatim — only the
/// content kind and a size. The match over [`rmcp::model::ContentBlock`]
/// handles every current variant; the wildcard keeps future non-exhaustive
/// variants redacted. Bounded by [`RESULT_SUMMARY_CAP`].
fn summarize_result(r: &rmcp::model::CallToolResult) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(r.content.len() + 1);
    for c in &r.content {
        let token = match c {
            rmcp::model::ContentBlock::Text(t) => {
                format!("text:{} chars", t.text.chars().count())
            }
            rmcp::model::ContentBlock::Image(i) => format!("image:{} b64", i.data.len()),
            rmcp::model::ContentBlock::Audio(a) => format!("audio:{} b64", a.data.len()),
            rmcp::model::ContentBlock::Resource(_) => "resource".to_string(),
            rmcp::model::ContentBlock::ResourceLink(_) => "resource_link".to_string(),
            _ => "unknown".to_string(),
        };
        parts.push(token);
    }
    if let Some(sc) = &r.structured_content {
        let shape = match sc {
            serde_json::Value::Object(o) => format!("struct:{} keys", o.len()),
            serde_json::Value::Array(a) => format!("struct:<arr:{}>", a.len()),
            _ => "struct".to_string(),
        };
        parts.push(shape);
    }
    let body = if parts.is_empty() {
        "empty".to_string()
    } else {
        parts.join(", ")
    };
    // Bound the body *before* wrapping so a huge multi-content result can
    // never truncate the brackets or the `is_error` marker off the line —
    // the app-level error flag is the load-bearing signal here.
    let body = truncate_chars(&body, RESULT_SUMMARY_CAP);
    if r.is_error == Some(true) {
        format!("[{body}] is_error")
    } else {
        format!("[{body}]")
    }
}

/// Bus handler that routes Mix `send`/`emit`/`port_exists` to the broker.
struct McpBusHandler {
    noded: Arc<cosmix_client::NodedClient>,
}

impl cosmix_mix::evaluator::BusHandler for McpBusHandler {
    fn send<'a>(
        &'a self,
        target: &'a str,
        command: &'a str,
        args: &'a cosmix_mix::value::Value,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = cosmix_mix::error::MixResult<(i32, cosmix_mix::value::Value)>,
                > + 'a,
        >,
    > {
        Box::pin(async move {
            // Fallible since cosmix-lib-mix 0.21.1 (non-finite numbers have no
            // JSON representation) — surface as an rc=10 error like any other
            // call failure, never a silently-mangled payload.
            let json_args = match cosmix_mix::json::mix_to_json(args) {
                Ok(j) => j,
                Err(e) => {
                    return Ok((
                        10,
                        cosmix_mix::value::Value::String(format!(
                            "send args not JSON-encodable: {e}"
                        )),
                    ));
                }
            };
            // call_typed keeps TRANSPORT failures as `Err` (→ `$rc = -1`,
            // RC_TRANSPORT) while a peer `rc >= 10` reply preserves its exact
            // status (→ `$rc = rc`) and a success carries its rc (0 or a
            // warning 5) — the Mix rc-band contract (was every Err → rc=10,
            // conflating transport with application error).
            match self.noded.call_typed(target, command, json_args).await {
                Ok(cosmix_client::PortReply::Ok { rc, value }) => {
                    Ok((i32::from(rc), cosmix_mix::json::json_to_mix(value)))
                }
                Ok(cosmix_client::PortReply::AppError { rc, message }) => {
                    Ok((i32::from(rc), cosmix_mix::value::Value::String(message)))
                }
                Err(e) => Err(cosmix_mix::error::MixError::RuntimeError {
                    span: None,
                    msg: format!("mcp send transport failure: {e}"),
                }),
            }
        })
    }

    fn emit<'a>(
        &'a self,
        target: &'a str,
        command: &'a str,
        args: &'a cosmix_mix::value::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = cosmix_mix::error::MixResult<()>> + 'a>>
    {
        Box::pin(async move {
            // emit is fire-and-forget but has an error channel — a
            // non-JSON-encodable payload (non-finite number) raises rather
            // than emitting a mangled event.
            let json_args = cosmix_mix::json::mix_to_json(args).map_err(|e| {
                cosmix_mix::error::MixError::RuntimeError {
                    span: None,
                    msg: format!("emit args not JSON-encodable: {e}"),
                }
            })?;
            let _ = self.noded.send(target, command, json_args).await;
            Ok(())
        })
    }

    fn port_exists<'a>(
        &'a self,
        target: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = cosmix_mix::error::MixResult<bool>> + 'a>>
    {
        Box::pin(async move {
            match self.noded.list_services().await {
                Ok(services) => Ok(services.iter().any(|s| s == target)),
                Err(_) => Ok(false),
            }
        })
    }

    fn next_incoming<'a>(
        &'a self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<cosmix_mix::evaluator::IncomingEvent>> + 'a>,
    > {
        // MCP runs scripts synchronously for tool calls; it does not host
        // long-lived `on` handlers, so incoming delivery is not supported.
        Box::pin(async move { None })
    }
}

// --- Bus tool params ---

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct BusCallParams {
    /// Target service name (e.g. "edit", "view", "mon")
    to: String,
    /// Bus command (e.g. "edit.get-content", "view.open")
    command: String,
    /// Optional JSON arguments string (e.g. '{"path": "/tmp/test.md"}')
    #[serde(default)]
    args: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct LogTailParams {
    /// Log file: "bus" for Bus traffic, or app name like "cosmix-edit"
    file: String,
    /// Number of lines (default 50)
    #[serde(default)]
    lines: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct LogSearchParams {
    /// Log file name
    file: String,
    /// Search pattern (case-insensitive)
    pattern: String,
    /// Max results (default 20)
    #[serde(default)]
    limit: Option<usize>,
}

// --- Knowledge base tool params ---

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ContextSearchParams {
    /// Natural language description of what you need context for
    query: String,
    /// Project domain filter (e.g. "cosmix", "ns"). Empty = auto-detect from PWD.
    /// Set to "all" to search across all domains.
    #[serde(default)]
    domain: Option<String>,
    /// Max results per source type (default 3 each for skills/docs/journals)
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct IndexWorkspaceParams {
    /// Workspace path to index (e.g. "$COSMIX_SRC", "~/.ns"). Empty = current working directory.
    #[serde(default)]
    path: Option<String>,
    /// Only re-index files matching this glob (e.g. "2026-04-03*"). Empty = all .md files.
    #[serde(default)]
    filter: Option<String>,
}

// --- Skills tool params ---

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SkillsRetrieveParams {
    /// Description of the task you're about to work on
    task: String,
    /// Project domain filter (e.g. "cosmix", "ns"). Empty or omitted = auto-detect from PWD.
    #[serde(default)]
    domain: Option<String>,
    /// Max skills to return (default from config, typically 3)
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SkillsStoreParams {
    /// Skill name (short, descriptive)
    name: String,
    /// Project domain (e.g. "cosmix", "ns"). Empty = auto-detect.
    #[serde(default)]
    domain: Option<String>,
    /// When this skill should be applied (natural language trigger)
    trigger: String,
    /// Step-by-step approach for executing this skill
    approach: String,
    /// Tools required (e.g. ["Edit", "Bash", "Grep"])
    #[serde(default)]
    tools_required: Vec<String>,
    /// Known failure modes and edge cases
    #[serde(default)]
    failure_modes: Vec<String>,
    /// Initial confidence 0.0-1.0 (default 0.5)
    #[serde(default)]
    confidence: Option<f32>,
    /// Git commit hash the skill was derived from (optional provenance)
    #[serde(default)]
    source_commit: Option<String>,
    /// Primary file path the skill was derived from (optional provenance)
    #[serde(default)]
    source_file: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SkillsRefineParams {
    /// ID of the skill to refine (from skills_retrieve results)
    id: i64,
    /// Did the skill work successfully?
    success: bool,
    /// Notes on what happened — what worked, what didn't, improvements
    notes: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SkillsListParams {
    /// Max skills to return (default 20)
    #[serde(default)]
    limit: Option<usize>,
    /// Offset for pagination (default 0)
    #[serde(default)]
    offset: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SkillsDeleteParams {
    /// ID of the skill to delete
    id: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct DocsFeedbackParams {
    /// ID of the document chunk (from context_search results)
    id: i64,
    /// Was this document chunk useful for your task?
    useful: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct JournalFeedbackParams {
    /// ID of the journal chunk (from context_search results)
    id: i64,
    /// Was this journal chunk useful for your task?
    useful: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct MemoryFeedbackParams {
    /// ID of the memory chunk (from context_search results)
    id: i64,
    /// Was this memory chunk useful for your task?
    useful: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct JournalSupersedeParams {
    /// ID of the older journal chunk being superseded (will be hidden from context_search).
    old_id: i64,
    /// ID of the newer journal chunk that replaces it.
    new_id: i64,
    /// Short human-readable reason the old entry is being superseded (for the log record).
    #[serde(default)]
    reason: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SkillsGraduateParams {
    /// ID of the skill to manually graduate to CLAUDE.md
    id: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct KnowledgeDigestParams {
    /// Project domain (e.g. "cosmix", "ns"). Empty = auto-detect from PWD.
    #[serde(default)]
    domain: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct KnowledgeBriefParams {
    /// Description of the task the subagent will work on
    task: String,
    /// Project domain filter (e.g. "cosmix", "ns"). Empty = auto-detect from PWD.
    #[serde(default)]
    domain: Option<String>,
    /// Max skills to include (default 3)
    #[serde(default)]
    limit: Option<usize>,
}

// --- Mix execution params ---

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct MixExecuteParams {
    /// Mix script source code to execute
    script: String,
    /// Working directory for the script (optional, defaults to $HOME)
    #[serde(default)]
    cwd: Option<String>,
}

impl CosmixMcp {
    async fn noded(&self) -> Result<&Arc<cosmix_client::NodedClient>, String> {
        self.noded
            .get_or_try_init(|| async {
                tracing::info!(target: "cosmix_mcp", "connecting to broker...");
                let client = cosmix_config::client_helpers::connect_anonymous_default()
                    .await
                    .map_err(|e| {
                        format!("broker connect failed: {e}. Ensure cosmix-noded is running.")
                    })?;
                tracing::info!(target: "cosmix_mcp", "broker connected");
                Ok(Arc::new(client))
            })
            .await
            .map_err(|e: String| e)
    }

    async fn indexd(&self) -> Result<cosmix_skills::IndexdClient, String> {
        cosmix_skills::IndexdClient::from_config()
            .await
            .map_err(|e| format!("indexd connect failed: {e}. Ensure cosmix-indexd is running."))
    }
}

#[tool_router]
impl CosmixMcp {
    // ---- Bus tools ----

    /// Call an Bus command on a cosmix service and return the response.
    #[tool]
    async fn bus_call(&self, Parameters(p): Parameters<BusCallParams>) -> String {
        let noded = match self.noded().await {
            Ok(h) => h,
            Err(e) => return format!("ERROR: {e}"),
        };
        let args_val = p
            .args
            .and_then(|a: String| serde_json::from_str(&a).ok())
            .unwrap_or(serde_json::Value::Null);
        match noded.call(&p.to, &p.command, args_val).await {
            Ok(val) => serde_json::to_string_pretty(&val).unwrap_or_else(|_| val.to_string()),
            Err(e) => format!("ERROR: {e}"),
        }
    }

    /// List all services registered on the cosmix broker, with build
    /// provenance (version / git_sha / build_time / pid / …) — the
    /// version-discovery surface, so an agent can spot a stale binary.
    #[tool]
    async fn bus_list_services(&self) -> String {
        let noded = match self.noded().await {
            Ok(h) => h,
            Err(e) => return format!("ERROR: {e}"),
        };
        match noded.service_inventory().await {
            Ok(services) => {
                serde_json::to_string_pretty(&services).unwrap_or_else(|_| format!("{services:?}"))
            }
            Err(e) => format!("ERROR: {e}"),
        }
    }

    /// Node identity + the local broker's own build, plus live uptime
    /// and registered-service count (`noded.info`). Pairs with
    /// `bus_list_services` for a mesh-wide version audit.
    #[tool]
    async fn bus_node_info(&self) -> String {
        let noded = match self.noded().await {
            Ok(h) => h,
            Err(e) => return format!("ERROR: {e}"),
        };
        match noded
            .call("noded", "noded.info", serde_json::Value::Null)
            .await
        {
            Ok(info) => serde_json::to_string_pretty(&info).unwrap_or_else(|_| format!("{info:?}")),
            Err(e) => format!("ERROR: {e}"),
        }
    }

    /// List all mesh peer nodes.
    #[tool]
    async fn bus_list_peers(&self) -> String {
        let noded = match self.noded().await {
            Ok(h) => h,
            Err(e) => return format!("ERROR: {e}"),
        };
        match noded
            .call("noded", "noded.peers", serde_json::Value::Null)
            .await
        {
            Ok(val) => serde_json::to_string_pretty(&val).unwrap_or_else(|_| val.to_string()),
            Err(e) => format!("ERROR: {e}"),
        }
    }

    /// Ping the cosmix broker to check connectivity.
    #[tool]
    async fn noded_ping(&self) -> String {
        let noded = match self.noded().await {
            Ok(h) => h,
            Err(e) => return format!("ERROR: {e}"),
        };
        match noded
            .call("noded", "noded.ping", serde_json::Value::Null)
            .await
        {
            Ok(val) => val.to_string(),
            Err(e) => format!("ERROR: {e}"),
        }
    }

    /// Read last N lines from a cosmix log file. file="bus" for Bus traffic.
    #[tool]
    async fn log_tail(&self, Parameters(p): Parameters<LogTailParams>) -> String {
        let n = p.lines.unwrap_or(50);
        let path = resolve_log_path(&p.file);
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                let all: Vec<&str> = content.lines().collect();
                let start = all.len().saturating_sub(n);
                all[start..].join("\n")
            }
            Err(e) => format!("ERROR reading {}: {e}", path.display()),
        }
    }

    /// Search a cosmix log file for a pattern (case-insensitive).
    #[tool]
    async fn log_search(&self, Parameters(p): Parameters<LogSearchParams>) -> String {
        let max = p.limit.unwrap_or(20);
        let path = resolve_log_path(&p.file);
        let pat = p.pattern.to_lowercase();
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                let matches: Vec<&str> = content
                    .lines()
                    .filter(|line| line.to_lowercase().contains(&pat))
                    .collect();
                if matches.is_empty() {
                    "No matches found".to_string()
                } else {
                    let start = matches.len().saturating_sub(max);
                    matches[start..].join("\n")
                }
            }
            Err(e) => format!("ERROR reading {}: {e}", path.display()),
        }
    }

    // ---- Knowledge base tools ----

    /// Unified semantic search across skills, docs, and journals.
    /// Searches the current project domain first, with cross-domain fallback.
    /// Call this at the START of non-trivial tasks to get relevant context.
    /// Returns results grouped by source type (skills, docs, journals).
    #[tool]
    async fn context_search(&self, Parameters(p): Parameters<ContextSearchParams>) -> String {
        let mut indexd = match self.indexd().await {
            Ok(c) => c,
            Err(e) => return format!("ERROR: {e}"),
        };

        let limit = p.limit.unwrap_or(3);
        let search_all = p.domain.as_deref() == Some("all");
        let domain = if search_all {
            None
        } else {
            p.domain
                .as_deref()
                .filter(|d| !d.is_empty())
                .map(|d| d.to_string())
                .or_else(|| {
                    let d = cosmix_skills::detect_domain_cwd();
                    if d == "general" { None } else { Some(d) }
                })
        };

        // Load knowledge settings for trust weighting + journal decay
        let k = cosmix_config::store::load_service::<cosmix_config::KnowledgeSettings>("knowledge")
            .unwrap_or_default();

        let mut result = serde_json::json!({
            "domain": domain.as_deref().unwrap_or("all"),
            "query": &p.query,
            "skills": [],
            "docs": [],
            "journals": [],
            "memory": [],
            "source": [],
            "observations": [],
        });

        // Search skills (domain-filtered)
        let skill_req = if let Some(ref d) = domain {
            serde_json::json!({
                "action": "search", "query": &p.query, "limit": limit,
                "source": "skill",
                "metadata_filter": [{"field": "domain", "op": "eq", "value": d}]
            })
        } else {
            serde_json::json!({"action": "search", "query": &p.query, "limit": limit, "source": "skill"})
        };

        if let Ok(resp) = indexd.raw_request(&skill_req).await
            && let Some(hits) = resp.get("results").and_then(|r| r.as_array())
        {
            let mut skills: Vec<serde_json::Value> = hits
                .iter()
                .filter_map(|h| {
                    let meta: serde_json::Value = h
                        .get("metadata")
                        .and_then(|m| m.as_str())
                        .and_then(|s| serde_json::from_str(s).ok())?;
                    // Skip superseded skills (they have a non-zero superseded_by)
                    if meta
                        .get("superseded_by")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0)
                        > 0
                    {
                        return None;
                    }
                    let raw = h.get("distance").and_then(|d| d.as_f64()).unwrap_or(1.0);
                    let graduated = meta
                        .get("graduated")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let trust = if graduated {
                        k.trust_weight_graduated
                    } else {
                        k.trust_weight_skill
                    };
                    let adj = raw - trust;
                    Some(serde_json::json!({
                        "id": h.get("id"),
                        "name": meta.get("name"),
                        "trigger": meta.get("trigger"),
                        "approach": meta.get("approach"),
                        "failure_modes": meta.get("failure_modes"),
                        "confidence": meta.get("confidence"),
                        "graduated": graduated,
                        "distance": adj,
                    }))
                })
                .collect();
            skills.sort_by(|a, b| {
                let da = a.get("distance").and_then(|v| v.as_f64()).unwrap_or(1.0);
                let db = b.get("distance").and_then(|v| v.as_f64()).unwrap_or(1.0);
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
            });
            result["skills"] = serde_json::Value::Array(skills);
        }

        // Search docs (domain-filtered, then cross-domain if few results)
        let mut doc_results =
            search_source(&mut indexd, &p.query, "doc", domain.as_deref(), limit).await;
        for hit in doc_results.iter_mut() {
            if let Some(d) = hit.get_mut("distance").and_then(|v| v.as_f64()) {
                hit["distance"] = serde_json::json!(d - k.trust_weight_doc);
            }
        }
        doc_results.sort_by(|a, b| {
            let da = a.get("distance").and_then(|v| v.as_f64()).unwrap_or(1.0);
            let db = b.get("distance").and_then(|v| v.as_f64()).unwrap_or(1.0);
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        });
        result["docs"] = serde_json::Value::Array(doc_results);

        // Search specs (normative contracts, highest trust weight)
        let mut spec_results =
            search_source(&mut indexd, &p.query, "spec", domain.as_deref(), limit).await;
        for hit in spec_results.iter_mut() {
            if let Some(d) = hit.get_mut("distance").and_then(|v| v.as_f64()) {
                hit["distance"] = serde_json::json!(d - k.trust_weight_spec);
            }
        }
        spec_results.sort_by(|a, b| {
            let da = a.get("distance").and_then(|v| v.as_f64()).unwrap_or(1.0);
            let db = b.get("distance").and_then(|v| v.as_f64()).unwrap_or(1.0);
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        });
        result["specs"] = serde_json::Value::Array(spec_results);

        // Search plans (provisional implementation plans)
        let mut plan_results =
            search_source(&mut indexd, &p.query, "plan", domain.as_deref(), limit).await;
        for hit in plan_results.iter_mut() {
            if let Some(d) = hit.get_mut("distance").and_then(|v| v.as_f64()) {
                hit["distance"] = serde_json::json!(d - k.trust_weight_plan);
            }
        }
        plan_results.sort_by(|a, b| {
            let da = a.get("distance").and_then(|v| v.as_f64()).unwrap_or(1.0);
            let db = b.get("distance").and_then(|v| v.as_f64()).unwrap_or(1.0);
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        });
        result["plans"] = serde_json::Value::Array(plan_results);

        // Search notes (_notes.md persistent operational facts)
        let mut notes_results =
            search_source(&mut indexd, &p.query, "notes", domain.as_deref(), limit).await;
        for hit in notes_results.iter_mut() {
            if let Some(d) = hit.get_mut("distance").and_then(|v| v.as_f64()) {
                hit["distance"] = serde_json::json!(d - k.trust_weight_notes);
            }
        }
        notes_results.sort_by(|a, b| {
            let da = a.get("distance").and_then(|v| v.as_f64()).unwrap_or(1.0);
            let db = b.get("distance").and_then(|v| v.as_f64()).unwrap_or(1.0);
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        });
        result["notes"] = serde_json::Value::Array(notes_results);

        // Search journals (domain-filtered, then cross-domain), with temporal decay
        let mut journal_results =
            search_source(&mut indexd, &p.query, "journal", domain.as_deref(), limit).await;
        let today = chrono::Local::now().date_naive();
        journal_results.retain_mut(|hit| {
            let date_str = hit.get("date").and_then(|v| v.as_str()).unwrap_or("");
            let days_old = chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d")
                .map(|d| (today - d).num_days().max(0))
                .unwrap_or(0);
            if k.journal_max_age_days > 0 && days_old as u32 > k.journal_max_age_days {
                return false;
            }
            if let Some(raw) = hit.get("distance").and_then(|v| v.as_f64()) {
                let decay = (days_old as f64 / 30.0) * k.journal_decay_per_month;
                let adj = raw + decay - k.trust_weight_journal;
                hit["distance"] = serde_json::json!(adj);
            }
            true
        });
        journal_results.sort_by(|a, b| {
            let da = a.get("distance").and_then(|v| v.as_f64()).unwrap_or(1.0);
            let db = b.get("distance").and_then(|v| v.as_f64()).unwrap_or(1.0);
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        });
        result["journals"] = serde_json::Value::Array(journal_results);

        // Search _memory/ (CMM-generated observations), with same temporal decay as journals.
        let mut memory_results =
            search_source(&mut indexd, &p.query, "memory", domain.as_deref(), limit).await;
        memory_results.retain_mut(|hit| {
            let date_str = hit.get("date").and_then(|v| v.as_str()).unwrap_or("");
            let days_old = chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d")
                .map(|d| (today - d).num_days().max(0))
                .unwrap_or(0);
            if k.journal_max_age_days > 0 && days_old as u32 > k.journal_max_age_days {
                return false;
            }
            if let Some(raw) = hit.get("distance").and_then(|v| v.as_f64()) {
                let decay = (days_old as f64 / 30.0) * k.journal_decay_per_month;
                let adj = raw + decay - k.trust_weight_memory;
                hit["distance"] = serde_json::json!(adj);
            }
            true
        });
        memory_results.sort_by(|a, b| {
            let da = a.get("distance").and_then(|v| v.as_f64()).unwrap_or(1.0);
            let db = b.get("distance").and_then(|v| v.as_f64()).unwrap_or(1.0);
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        });
        result["memory"] = serde_json::Value::Array(memory_results);

        // Search rust-doc (/// and //! doc comments from .rs source files)
        let mut rust_doc_results =
            search_source(&mut indexd, &p.query, "rust-doc", domain.as_deref(), limit).await;
        for hit in rust_doc_results.iter_mut() {
            if let Some(d) = hit.get_mut("distance").and_then(|v| v.as_f64()) {
                hit["distance"] = serde_json::json!(d - k.trust_weight_doc);
            }
        }
        rust_doc_results.sort_by(|a, b| {
            let da = a.get("distance").and_then(|v| v.as_f64()).unwrap_or(1.0);
            let db = b.get("distance").and_then(|v| v.as_f64()).unwrap_or(1.0);
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        });
        result["source"] = serde_json::Value::Array(rust_doc_results);

        // Search observations (tool output from CMM code-side jobs: clippy warnings,
        // dead code reports, bench regressions, etc.). Phase 0.1 adds domain
        // scoping so observations from different workspaces don't collide.
        // No temporal decay (git_sha in metadata tracks the observed code version;
        // superseded-by handles staleness when code changes invalidate a warning).
        let observation_req = if let Some(ref d) = domain {
            serde_json::json!({
                "action": "search",
                "query": &p.query,
                "limit": limit,
                "source": "observation",
                "metadata_filter": [{"field": "domain", "op": "eq", "value": d}]
            })
        } else {
            serde_json::json!({
                "action": "search",
                "query": &p.query,
                "limit": limit,
                "source": "observation"
            })
        };
        if let Ok(resp) = indexd.raw_request(&observation_req).await
            && let Some(hits) = resp.get("results").and_then(|r| r.as_array())
        {
            let mut obs: Vec<serde_json::Value> = hits
                .iter()
                .map(|h| {
                    let meta: serde_json::Value = h
                        .get("metadata")
                        .and_then(|m| m.as_str())
                        .and_then(|s| serde_json::from_str(s).ok())
                        .unwrap_or_else(|| serde_json::json!({}));
                    let raw = h.get("distance").and_then(|d| d.as_f64()).unwrap_or(1.0);
                    serde_json::json!({
                        "id": h.get("id"),
                        "observer": meta.get("observer"),
                        "tool": meta.get("tool"),
                        "lint": meta.get("lint"),
                        "domain": meta.get("domain"),
                        "file": meta.get("file"),
                        "line": meta.get("line"),
                        "git_sha": meta.get("git_sha"),
                        "observed_at": meta.get("observed_at"),
                        "content": h.get("content"),
                        "distance": raw,
                    })
                })
                .collect();
            obs.sort_by(|a, b| {
                let da = a.get("distance").and_then(|v| v.as_f64()).unwrap_or(1.0);
                let db = b.get("distance").and_then(|v| v.as_f64()).unwrap_or(1.0);
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
            });
            result["observations"] = serde_json::Value::Array(obs);
        }

        // Search mix-script (.mix scripts)
        let mut mix_results = search_source(
            &mut indexd,
            &p.query,
            "mix-script",
            domain.as_deref(),
            limit,
        )
        .await;
        if !mix_results.is_empty() {
            for hit in mix_results.iter_mut() {
                if let Some(d) = hit.get_mut("distance").and_then(|v| v.as_f64()) {
                    hit["distance"] = serde_json::json!(d - k.trust_weight_doc);
                }
            }
            mix_results.sort_by(|a, b| {
                let da = a.get("distance").and_then(|v| v.as_f64()).unwrap_or(1.0);
                let db = b.get("distance").and_then(|v| v.as_f64()).unwrap_or(1.0);
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
            });
            result["scripts"] = serde_json::Value::Array(mix_results);
        }

        serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".into())
    }

    /// Index (or re-index) _doc/ and _journal/ markdown files for a workspace.
    /// Splits files on ## headings, stores each section with domain and source metadata.
    /// Idempotent — deletes old entries for re-indexed files before storing new ones.
    #[tool]
    async fn index_workspace(&self, Parameters(p): Parameters<IndexWorkspaceParams>) -> String {
        let workspace = p
            .path
            .map(|p| p.replace("~", &std::env::var("HOME").unwrap_or_default()))
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default()
            });

        let workspace_path = std::path::Path::new(&workspace);
        if !workspace_path.exists() {
            return format!("ERROR: workspace path does not exist: {workspace}");
        }

        let domain = cosmix_skills::detect_domain(workspace_path);

        let mut indexd = match self.indexd().await {
            Ok(c) => c,
            Err(e) => return format!("ERROR: {e}"),
        };

        let mut total_files = 0u32;
        let mut total_sections = 0u32;
        let mut errors = Vec::new();

        // Scan for _doc/ and _journal/ directories anywhere under workspace
        let scan_dirs: Vec<(std::path::PathBuf, &str)> = find_content_dirs(workspace_path);

        for (dir, source) in &scan_dirs {
            let md_files: Vec<std::path::PathBuf> = if dir.is_file() {
                vec![dir.clone()]
            } else {
                // Recursive: picks up _doc/infra/**, _decisions/**, etc.,
                // not just files directly in the scan root.
                collect_md_files(dir)
            };

            for filepath in &md_files {
                let filename = filepath.file_name().and_then(|f| f.to_str()).unwrap_or("?");

                // Skip constitutional audit log — activity-log.md is committed
                // for tamper-evident audit (_spec 2026-04-20-00-constitution.md IV.5/V.5),
                // not vector retrieval. Indexing it produces thousands of
                // memory-date validation errors against indexd (filename has
                // no YYYY-MM-DD prefix, so the [..10] slice is "activity-l").
                if *source == "memory" && filename == "activity-log.md" {
                    continue;
                }

                // Apply filter if specified — supports both substring match
                // and glob patterns (e.g. "2026-04-06" or "2026-04-06*")
                if let Some(ref filter) = p.filter {
                    let matches = if filter.contains('*') || filter.contains('?') {
                        glob::Pattern::new(filter)
                            .map(|pat| pat.matches(filename))
                            .unwrap_or(false)
                    } else {
                        filename.contains(filter.as_str())
                    };
                    if !matches {
                        continue;
                    }
                }

                let content = match std::fs::read_to_string(filepath) {
                    Ok(c) => c,
                    Err(e) => {
                        errors.push(format!("{filename}: {e}"));
                        continue;
                    }
                };

                let sections = split_markdown_sections(&content, filename);
                if sections.is_empty() {
                    continue;
                }

                // Delete old entries for this file (idempotent re-index)
                let path_str = filepath.to_string_lossy().to_string();
                if let Err(e) = delete_by_path(&mut indexd, &path_str).await {
                    errors.push(format!("{filename}: delete failed: {e}"));
                }

                // Extract date from filename (YYYY-MM-DD-title.md convention)
                let date = filename.get(..10).unwrap_or("");

                // For memory files, derive generator from filename convention:
                //   YYYY-MM-DD-task-name.md → "cmm/task-name"
                //   activity-log.md         → "cmm/activity-log"
                // Legacy "- Generator:" lines in file content are now ignored
                // by the indexer and can be removed from _memory/ files.
                let generator: Option<String> = if *source == "memory" {
                    let stem = filename.strip_suffix(".md").unwrap_or(filename);
                    Some(
                        if stem.len() > 11 && stem.as_bytes().get(10) == Some(&b'-') {
                            format!("cmm/{}", &stem[11..])
                        } else {
                            format!("cmm/{stem}")
                        },
                    )
                } else {
                    None
                };

                // Store one section at a time to avoid long mutex holds
                for (title, text) in &sections {
                    let mut meta = serde_json::json!({
                        "path": path_str,
                        "filename": filename,
                        "section": title,
                        "domain": &domain,
                        "type": source,
                        "date": date,
                    });
                    if let Some(ref g) = generator {
                        meta["generator"] = serde_json::Value::String(g.clone());
                    }

                    let req = serde_json::json!({
                        "action": "store",
                        "texts": [text],
                        "source": source,
                        "metadata": [meta.to_string()],
                    });

                    match indexd.raw_request(&req).await {
                        Ok(_) => total_sections += 1,
                        Err(e) => errors.push(format!("{filename}/{title}: {e}")),
                    }
                }

                total_files += 1;
            }
        }

        let mut result = serde_json::json!({
            "indexed": true,
            "workspace": workspace,
            "domain": domain,
            "files": total_files,
            "sections": total_sections,
            "dirs_scanned": scan_dirs.iter().map(|(d, s)| format!("{} ({})", d.display(), s)).collect::<Vec<_>>(),
        });

        if !errors.is_empty() {
            result["errors"] = serde_json::Value::Array(
                errors
                    .iter()
                    .map(|e| serde_json::Value::String(e.clone()))
                    .collect(),
            );
        }

        serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".into())
    }

    // ---- Skills tools ----

    /// Retrieve relevant skills for a task from the semantic skill store.
    /// Call this at the start of non-trivial tasks to leverage prior experience.
    /// Returns matching skills with their IDs, triggers, approaches, and confidence scores.
    #[tool]
    async fn skills_retrieve(&self, Parameters(p): Parameters<SkillsRetrieveParams>) -> String {
        let mut indexd = match self.indexd().await {
            Ok(c) => c,
            Err(e) => return format!("ERROR: {e}"),
        };

        let max = p.limit.unwrap_or_else(|| {
            cosmix_config::store::load_service::<cosmix_config::SkillsSettings>("skills")
                .map(|s| s.max_skills as usize)
                .unwrap_or(3)
        });

        let domain = p.domain.as_deref().filter(|d| !d.is_empty());
        let domain = domain.or_else(|| {
            let d = cosmix_skills::detect_domain_cwd();
            if d == "general" {
                None
            } else {
                Some(Box::leak(d.into_boxed_str()) as &str)
            }
        });

        let results =
            match cosmix_skills::retrieve_skills_domain(&mut indexd, &p.task, max, domain).await {
                Ok(r) => r,
                Err(e) => return format!("ERROR: {e}"),
            };

        if results.is_empty() {
            return "No matching skills found.".into();
        }

        // Return structured JSON with IDs (needed for refine) + the formatted prompt section
        let skills_json: Vec<serde_json::Value> = results
            .iter()
            .map(|(id, doc)| {
                serde_json::json!({
                    "id": id,
                    "name": &doc.name,
                    "domain": &doc.domain,
                    "trigger": &doc.trigger,
                    "approach": &doc.approach,
                    "tools_required": &doc.tools_required,
                    "failure_modes": &doc.failure_modes,
                    "confidence": doc.confidence,
                    "use_count": doc.use_count,
                    "success_count": doc.success_count,
                })
            })
            .collect();

        serde_json::to_string_pretty(&skills_json).unwrap_or_else(|_| "[]".into())
    }

    /// Store a new skill learned from completing a task.
    /// Call this after successfully completing a non-trivial task worth remembering.
    /// Claude should generate the skill fields directly from its task context.
    #[tool]
    async fn skills_store(&self, Parameters(p): Parameters<SkillsStoreParams>) -> String {
        let mut indexd = match self.indexd().await {
            Ok(c) => c,
            Err(e) => return format!("ERROR: {e}"),
        };

        let now = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let domain = p
            .domain
            .filter(|d| !d.is_empty())
            .unwrap_or_else(cosmix_skills::detect_domain_cwd);

        let skill = cosmix_skills::SkillDocument {
            name: p.name,
            version: 1,
            domain,
            trigger: p.trigger,
            approach: p.approach,
            tools_required: p.tools_required,
            failure_modes: p.failure_modes,
            confidence: p.confidence.unwrap_or(0.5),
            use_count: 1,
            success_count: 1,
            last_used: Some(now.clone()),
            created: now.clone(),
            updated: now,
            graduated: false,
            source_commit: p.source_commit,
            source_file: p.source_file,
            superseded_by: None,
        };

        match indexd.store_skill(&skill).await {
            Ok(id) => serde_json::json!({
                "stored": true,
                "id": id,
                "name": &skill.name,
                "domain": &skill.domain,
            })
            .to_string(),
            Err(e) => format!("ERROR: {e}"),
        }
    }

    /// Report outcome after using a skill and refine it based on experience.
    /// Uses the configured LLM backend (default: claude-haiku) to analyze the outcome
    /// and adjust the skill's confidence, approach, and failure modes.
    #[tool]
    async fn skills_refine(&self, Parameters(p): Parameters<SkillsRefineParams>) -> String {
        let mut indexd = match self.indexd().await {
            Ok(c) => c,
            Err(e) => return format!("ERROR: {e}"),
        };

        // Fetch the existing skill
        let (skills, _total) = match indexd.list_skills(100, 0).await {
            Ok(r) => r,
            Err(e) => return format!("ERROR listing skills: {e}"),
        };

        let existing = match skills.iter().find(|(id, _)| *id == p.id) {
            Some((_, doc)) => doc.clone(),
            None => return format!("ERROR: skill ID {} not found", p.id),
        };

        // Get LLM client for refinement
        let skills_cfg =
            cosmix_config::store::load_service::<cosmix_config::SkillsSettings>("skills")
                .unwrap_or_default();
        let llm_backend = if skills_cfg.llm_backend.is_empty() {
            None
        } else {
            Some(skills_cfg.llm_backend.as_str())
        };

        let llm = match cosmix_skills::LlmClient::from_config(llm_backend) {
            Ok(l) => l,
            Err(e) => return format!("ERROR creating LLM client: {e}"),
        };

        let outcome = cosmix_skills::TaskOutcome {
            skill_id: p.id,
            success: p.success,
            notes: p.notes,
            duration_ms: 0,
        };

        match cosmix_skills::refine_skill(&llm, &mut indexd, p.id, &existing, &outcome).await {
            Ok((new_id, updated)) => {
                // Check if the new skill version qualifies for graduation
                let graduated = cosmix_skills::check_graduation(&mut indexd, new_id, &updated)
                    .await
                    .unwrap_or(false);

                serde_json::json!({
                    "refined": true,
                    "old_id": p.id,
                    "id": new_id,
                    "name": &updated.name,
                    "version": updated.version,
                    "confidence": updated.confidence,
                    "use_count": updated.use_count,
                    "success_count": updated.success_count,
                    "graduated": graduated,
                })
                .to_string()
            }
            Err(e) => format!("ERROR refining: {e}"),
        }
    }

    /// List all stored skills, optionally paginated.
    #[tool]
    async fn skills_list(&self, Parameters(p): Parameters<SkillsListParams>) -> String {
        let mut indexd = match self.indexd().await {
            Ok(c) => c,
            Err(e) => return format!("ERROR: {e}"),
        };

        let limit = p.limit.unwrap_or(20);
        let offset = p.offset.unwrap_or(0);

        match indexd.list_skills(limit, offset).await {
            Ok((skills, total)) => {
                let items: Vec<serde_json::Value> = skills
                    .iter()
                    .map(|(id, doc)| {
                        serde_json::json!({
                            "id": id,
                            "name": &doc.name,
                            "domain": &doc.domain,
                            "trigger": &doc.trigger,
                            "confidence": doc.confidence,
                            "use_count": doc.use_count,
                            "version": doc.version,
                        })
                    })
                    .collect();
                serde_json::json!({
                    "skills": items,
                    "total": total,
                    "offset": offset,
                    "limit": limit,
                })
                .to_string()
            }
            Err(e) => format!("ERROR: {e}"),
        }
    }

    /// Delete a skill by ID.
    #[tool]
    async fn skills_delete(&self, Parameters(p): Parameters<SkillsDeleteParams>) -> String {
        let mut indexd = match self.indexd().await {
            Ok(c) => c,
            Err(e) => return format!("ERROR: {e}"),
        };

        match indexd.delete_skill(p.id).await {
            Ok(()) => serde_json::json!({"deleted": true, "id": p.id}).to_string(),
            Err(e) => format!("ERROR: {e}"),
        }
    }

    /// Manually graduate a skill to CLAUDE.md, bypassing threshold checks.
    /// Use this to promote a skill you trust as a permanent project rule.
    #[tool]
    async fn skills_graduate(&self, Parameters(p): Parameters<SkillsGraduateParams>) -> String {
        let mut indexd = match self.indexd().await {
            Ok(c) => c,
            Err(e) => return format!("ERROR: {e}"),
        };

        // Fetch the skill
        let (skills, _) = match indexd.list_skills(100, 0).await {
            Ok(r) => r,
            Err(e) => return format!("ERROR listing skills: {e}"),
        };

        let existing = match skills.into_iter().find(|(id, _)| *id == p.id) {
            Some((_, doc)) => doc,
            None => return format!("ERROR: skill ID {} not found", p.id),
        };

        if existing.graduated {
            return serde_json::json!({"graduated": false, "reason": "already graduated"})
                .to_string();
        }

        match cosmix_skills::check_graduation(&mut indexd, p.id, &existing).await {
            Ok(true) => serde_json::json!({
                "graduated": true,
                "id": p.id,
                "name": &existing.name,
            })
            .to_string(),
            Ok(false) => {
                // Force graduation even if below thresholds
                // Temporarily boost to pass thresholds since this is manual
                let mut boosted = existing.clone();
                boosted.confidence = 1.0;
                boosted.use_count = boosted.use_count.max(5);
                boosted.success_count = boosted.success_count.max(4);
                match cosmix_skills::check_graduation(&mut indexd, p.id, &boosted).await {
                    Ok(_) => serde_json::json!({
                        "graduated": true,
                        "id": p.id,
                        "name": &existing.name,
                        "forced": true,
                    })
                    .to_string(),
                    Err(e) => format!("ERROR graduating: {e}"),
                }
            }
            Err(e) => format!("ERROR graduating: {e}"),
        }
    }

    /// Report whether a retrieved document chunk was useful for your task.
    /// Call this after using context from context_search to improve future search ranking.
    /// Positive feedback boosts the chunk in future searches; negative feedback demotes it.
    #[tool]
    async fn docs_feedback(&self, Parameters(p): Parameters<DocsFeedbackParams>) -> String {
        let mut indexd = match self.indexd().await {
            Ok(c) => c,
            Err(e) => return format!("ERROR: {e}"),
        };

        let req = serde_json::json!({
            "action": "feedback",
            "id": p.id,
            "useful": p.useful,
        });

        match indexd.raw_request(&req).await {
            Ok(resp) => serde_json::to_string_pretty(&resp).unwrap_or_else(|_| "{}".into()),
            Err(e) => format!("ERROR: {e}"),
        }
    }

    /// Report whether a retrieved journal chunk was useful for your task.
    /// Same retrieval-scoring semantics as docs_feedback. Call this after using a
    /// journal entry surfaced by context_search or the UserPromptSubmit hook.
    #[tool]
    async fn journal_feedback(&self, Parameters(p): Parameters<JournalFeedbackParams>) -> String {
        let mut indexd = match self.indexd().await {
            Ok(c) => c,
            Err(e) => return format!("ERROR: {e}"),
        };

        let req = serde_json::json!({
            "action": "feedback",
            "id": p.id,
            "useful": p.useful,
        });

        match indexd.raw_request(&req).await {
            Ok(resp) => serde_json::to_string_pretty(&resp).unwrap_or_else(|_| "{}".into()),
            Err(e) => format!("ERROR: {e}"),
        }
    }

    /// Report whether a retrieved memory chunk was useful for your task.
    /// Same retrieval-scoring semantics as docs_feedback. Call this after using a
    /// _memory/ chunk (CMM sweep output) surfaced by context_search or the hook.
    #[tool]
    async fn memory_feedback(&self, Parameters(p): Parameters<MemoryFeedbackParams>) -> String {
        let mut indexd = match self.indexd().await {
            Ok(c) => c,
            Err(e) => return format!("ERROR: {e}"),
        };

        let req = serde_json::json!({
            "action": "feedback",
            "id": p.id,
            "useful": p.useful,
        });

        match indexd.raw_request(&req).await {
            Ok(resp) => serde_json::to_string_pretty(&resp).unwrap_or_else(|_| "{}".into()),
            Err(e) => format!("ERROR: {e}"),
        }
    }

    /// Mark an earlier journal chunk as superseded by a newer one.
    /// The superseded chunk stays in the database for audit/rollback but is
    /// filtered out of context_search results, so future searches surface the
    /// newer chunk instead. Use this when a later journal entry directly
    /// contradicts or invalidates an earlier one (e.g. threshold tuning that
    /// reverses an earlier change).
    #[tool]
    async fn journal_supersede(&self, Parameters(p): Parameters<JournalSupersedeParams>) -> String {
        let mut indexd = match self.indexd().await {
            Ok(c) => c,
            Err(e) => return format!("ERROR: {e}"),
        };

        let req = serde_json::json!({
            "action": "supersede",
            "old_id": p.old_id,
            "new_id": p.new_id,
            "reason": p.reason,
        });

        match indexd.raw_request(&req).await {
            Ok(resp) => serde_json::to_string_pretty(&resp).unwrap_or_else(|_| "{}".into()),
            Err(e) => format!("ERROR: {e}"),
        }
    }

    /// Quick knowledge base health overview. Call at session start for situational awareness.
    /// Returns skill counts by confidence tier, stale chunk counts, and total indexed content.
    #[tool]
    async fn knowledge_digest(&self, Parameters(p): Parameters<KnowledgeDigestParams>) -> String {
        let mut indexd = match self.indexd().await {
            Ok(c) => c,
            Err(e) => return format!("ERROR: {e}"),
        };

        let domain = p
            .domain
            .as_deref()
            .filter(|d| !d.is_empty())
            .map(|d| d.to_string())
            .or_else(|| {
                let d = cosmix_skills::detect_domain_cwd();
                if d == "general" { None } else { Some(d) }
            });

        // Get stats
        let stats = match indexd.stats().await {
            Ok(s) => s,
            Err(e) => return format!("ERROR getting stats: {e}"),
        };

        // Get all skills and bucket by confidence tier
        let (skills, _total) = match indexd.list_skills(1000, 0).await {
            Ok(r) => r,
            Err(e) => return format!("ERROR listing skills: {e}"),
        };

        let mut new = 0u32; // 0.0–0.3
        let mut developing = 0u32; // 0.3–0.7
        let mut proven = 0u32; // 0.7–0.9
        let mut graduate_candidate = 0u32; // 0.9+
        let mut graduated = 0u32;
        let mut superseded = 0u32;
        let mut top_skills: Vec<(i64, String, f32, u32)> = Vec::new();

        for (id, doc) in &skills {
            if doc.superseded_by.is_some() {
                superseded += 1;
                continue;
            }
            if let Some(ref d) = domain
                && !doc.domain.is_empty()
                && &doc.domain != d
            {
                continue;
            }
            if doc.graduated {
                graduated += 1;
            }
            match doc.confidence {
                c if c >= 0.9 => graduate_candidate += 1,
                c if c >= 0.7 => proven += 1,
                c if c >= 0.3 => developing += 1,
                _ => new += 1,
            }
            top_skills.push((*id, doc.name.clone(), doc.confidence, doc.use_count));
        }

        // Sort by use_count desc, take top 5
        top_skills.sort_by_key(|s| std::cmp::Reverse(s.3));
        top_skills.truncate(5);

        // Get stale counts
        let stale = indexd.stale(None, 90, 3, 180, 5).await.ok();
        let stale_json = stale
            .as_ref()
            .map(|s| {
                serde_json::json!({
                    "never_retrieved_old": s.never_retrieved_old.len(),
                    "low_value": s.low_value.len(),
                    "long_dormant": s.long_dormant.len(),
                })
            })
            .unwrap_or(serde_json::json!(null));

        let top_json: Vec<serde_json::Value> = top_skills.iter().map(|(id, name, conf, uses)| {
            serde_json::json!({"id": id, "name": name, "confidence": conf, "uses": uses})
        }).collect();

        // Collect domain breakdown from skill metadata
        let mut domain_chunks: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for (_, doc) in &skills {
            if doc.superseded_by.is_some() {
                continue;
            }
            let d = if doc.domain.is_empty() {
                "general"
            } else {
                &doc.domain
            };
            *domain_chunks.entry(d.to_string()).or_default() += 1;
        }
        // Also scan doc/journal chunk domains via a lightweight list
        let list_req = serde_json::json!({"action": "list", "limit": 5000, "offset": 0});
        if let Ok(resp) = indexd.raw_request(&list_req).await
            && let Some(items) = resp.get("items").and_then(|i| i.as_array())
        {
            for item in items {
                if let Some(meta_str) = item.get("metadata").and_then(|m| m.as_str())
                    && let Ok(meta) = serde_json::from_str::<serde_json::Value>(meta_str)
                {
                    let d = meta
                        .get("domain")
                        .and_then(|v| v.as_str())
                        .unwrap_or("general");
                    *domain_chunks.entry(d.to_string()).or_default() += 1;
                }
            }
        }

        let domains_json: Vec<serde_json::Value> = domain_chunks
            .iter()
            .map(|(d, c)| serde_json::json!({"domain": d, "chunks": c}))
            .collect();

        serde_json::json!({
            "domain": domain.as_deref().unwrap_or("all"),
            "total_indexed": stats.total_vectors,
            "by_source": stats.by_source.iter().map(|s| serde_json::json!({"source": &s.source, "count": s.count})).collect::<Vec<_>>(),
            "indexed_domains": domains_json,
            "skills": {
                "new": new,
                "developing": developing,
                "proven": proven,
                "graduate_candidate": graduate_candidate,
                "graduated": graduated,
                "superseded": superseded,
            },
            "top_skills": top_json,
            "stale": stale_json,
        }).to_string()
    }

    /// Generate a compact knowledge briefing for a subagent task.
    /// Returns a self-contained text block with relevant skills, doc pointers,
    /// and recent journal entries — designed to be included in agent prompts.
    /// Call this before spawning non-trivial Agent subagents.
    #[tool]
    async fn knowledge_brief(&self, Parameters(p): Parameters<KnowledgeBriefParams>) -> String {
        let mut indexd = match self.indexd().await {
            Ok(c) => c,
            Err(e) => return format!("ERROR: {e}"),
        };

        let limit = p.limit.unwrap_or(3);
        let domain = p
            .domain
            .as_deref()
            .filter(|d| !d.is_empty() && *d != "all")
            .map(|d| d.to_string())
            .or_else(|| {
                let d = cosmix_skills::detect_domain_cwd();
                if d == "general" { None } else { Some(d) }
            });

        let mut brief = String::from("=== KNOWLEDGE BRIEF ===\n\n");

        // ── Skills ──
        let skills = indexd
            .search_skills_domain(&p.task, limit, domain.as_deref())
            .await
            .unwrap_or_default();
        let active_skills: Vec<_> = skills
            .iter()
            .filter(|(_, doc, _)| doc.superseded_by.is_none())
            .collect();

        if !active_skills.is_empty() {
            brief.push_str("## Relevant Skills\n\n");
            for (i, (_, doc, _)) in active_skills.iter().enumerate() {
                brief.push_str(&format!("{}. **{}**: {}\n", i + 1, doc.name, doc.trigger));
                let approach = if doc.approach.len() > 200 {
                    format!("{}...", &doc.approach[..200])
                } else {
                    doc.approach.clone()
                };
                brief.push_str(&format!("   Approach: {}\n\n", approach));
            }
        }

        // ── Docs ──
        let docs = search_source(&mut indexd, &p.task, "doc", domain.as_deref(), limit).await;
        if !docs.is_empty() {
            brief.push_str("## Doc Pointers\n\n");
            for doc in &docs {
                let filename = doc.get("filename").and_then(|v| v.as_str()).unwrap_or("?");
                let section = doc.get("section").and_then(|v| v.as_str()).unwrap_or("");
                let snippet = doc.get("snippet").and_then(|v| v.as_str()).unwrap_or("");
                // Take first 2 lines of snippet
                let short: String = snippet.lines().take(2).collect::<Vec<_>>().join(" ");
                let short = if short.len() > 150 {
                    format!("{}...", &short[..150])
                } else {
                    short
                };
                brief.push_str(&format!("- **{}** > {}: {}\n", filename, section, short));
            }
            brief.push('\n');
        }

        // ── Journals ──
        let journals = search_source(&mut indexd, &p.task, "journal", domain.as_deref(), 2).await;
        if !journals.is_empty() {
            brief.push_str("## Recent Context\n\n");
            for j in &journals {
                let date = j.get("date").and_then(|v| v.as_str()).unwrap_or("?");
                let filename = j.get("filename").and_then(|v| v.as_str()).unwrap_or("?");
                let snippet = j.get("snippet").and_then(|v| v.as_str()).unwrap_or("");
                let first_line = snippet.lines().next().unwrap_or("");
                let first_line = if first_line.len() > 120 {
                    format!("{}...", &first_line[..120])
                } else {
                    first_line.to_string()
                };
                brief.push_str(&format!("- [{}] {}: {}\n", date, filename, first_line));
            }
            brief.push('\n');
        }

        if active_skills.is_empty() && docs.is_empty() && journals.is_empty() {
            brief.push_str("No relevant knowledge found for this task.\n\n");
        }

        brief.push_str("=== END BRIEF ===");
        brief
    }

    // ---- Mix execution tool ----

    /// Execute a Mix script inline and return its output.
    /// Mix has ~300 builtins including JSON, YAML, regex (subject-first re_*), TOML, datetime, crypto, URL parsing.
    /// The script runs with Bus connectivity — `send`/`emit`/`port_exists` route to the broker.
    #[tool]
    async fn mix_execute(&self, Parameters(p): Parameters<MixExecuteParams>) -> String {
        // Get broker connection before spawning (ref is !Send across spawn_blocking)
        let noded = self.noded().await.ok().cloned();
        let script = p.script;
        let cwd = p.cwd;

        // Run evaluator on a dedicated thread (Evaluator is !Send due to Rc<RefCell>)
        match tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            rt.block_on(async move { run_mix_script(&script, cwd.as_deref(), noded).await })
        })
        .await
        {
            Ok(Ok(output)) => output,
            Ok(Err(err)) => err,
            Err(e) => format!("ERROR: task panicked: {e}"),
        }
    }

    /// Report cosmix-mcp's own runtime state: start time, uptime, broker
    /// connection, per-tool call counts, and the most recent calls (newest
    /// first). Use this to observe the otherwise-invisible MCP bridge directly;
    /// for a live human stream see `journalctl -t cosmix-mcp -f` or
    /// `~/.local/log/cosmix/cosmix-mcp.log`.
    #[tool]
    async fn mcp_status(&self) -> String {
        let broker = self.noded.get().is_some();
        serde_json::to_string_pretty(&self.metrics.snapshot_json(broker))
            .unwrap_or_else(|e| format!("ERROR: {e}"))
    }
}

/// Run a Mix script with optional Bus connectivity. Called from spawn_blocking.
async fn run_mix_script(
    script: &str,
    cwd: Option<&str>,
    noded: Option<Arc<cosmix_client::NodedClient>>,
) -> Result<String, String> {
    // Set working directory if requested
    if let Some(cwd) = cwd {
        let expanded = if cwd.starts_with("~/") {
            if let Ok(home) = std::env::var("HOME") {
                format!("{}{}", home, &cwd[1..])
            } else {
                cwd.to_string()
            }
        } else {
            cwd.to_string()
        };
        std::env::set_current_dir(&expanded)
            .map_err(|e| format!("ERROR: failed to set cwd to {expanded}: {e}"))?;
    }

    // Parse
    let mut lexer = cosmix_mix::lexer::Lexer::new(script);
    let tokens = lexer
        .tokenize()
        .map_err(|e| format!("ERROR: parse error: {e}"))?;
    let mut parser = cosmix_mix::parser::Parser::new(tokens, script);
    let stmts = parser
        .parse_program()
        .map_err(|e| format!("ERROR: parse error: {e}"))?;

    // Create evaluator
    let stdout = cosmix_mix::evaluator::SharedBuf::new();
    let stderr = cosmix_mix::evaluator::SharedBuf::new();
    let mut eval = cosmix_mix::evaluator::Evaluator::with_output(
        Box::new(stdout.clone()),
        Box::new(stderr.clone()),
    );

    // Wire Bus IPC if broker is available
    if let Some(noded) = noded {
        eval.set_bus_handler(std::rc::Rc::new(McpBusHandler { noded }));
    }

    // Execute
    match eval.execute(&stmts).await {
        Ok(_) => {
            let out = stdout.to_string_lossy();
            let err = stderr.to_string_lossy();
            Ok(if err.is_empty() {
                if out.is_empty() {
                    "(no output)".to_string()
                } else {
                    out
                }
            } else {
                format!("{out}\n--- stderr ---\n{err}")
            })
        }
        Err(e) => {
            let out = stdout.to_string_lossy();
            let err = stderr.to_string_lossy();
            let mut result = format!("ERROR: {e}");
            if !out.is_empty() {
                result = format!("{out}\n{result}");
            }
            if !err.is_empty() {
                result = format!("{result}\n--- stderr ---\n{err}");
            }
            Err(result)
        }
    }
}

fn log_dir() -> PathBuf {
    std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".local/log/cosmix"))
        .unwrap_or_else(|_| PathBuf::from("/tmp/cosmix-log"))
}

fn resolve_log_path(name: &str) -> PathBuf {
    let dir = log_dir();
    if name == "bus" {
        return dir.join("bus.log");
    }
    let exact = dir.join(name);
    if exact.exists() {
        return exact;
    }
    if let Ok(entries) = std::fs::read_dir(&dir) {
        let mut m: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|f| f.to_str())
                    .is_some_and(|f| f.starts_with(name))
            })
            .collect();
        m.sort();
        if let Some(latest) = m.last() {
            return latest.clone();
        }
    }
    dir.join(name)
}

// ---- Knowledge base helpers ----

/// Search a specific source type with optional domain filter.
/// Returns formatted result objects with filename, section, snippet, distance.
async fn search_source(
    indexd: &mut cosmix_skills::IndexdClient,
    query: &str,
    source: &str,
    domain: Option<&str>,
    limit: usize,
) -> Vec<serde_json::Value> {
    // First try domain-filtered search
    let mut req = serde_json::json!({
        "action": "search", "query": query, "limit": limit, "source": source,
    });
    if let Some(d) = domain {
        req["metadata_filter"] = serde_json::json!([
            {"field": "domain", "op": "eq", "value": d}
        ]);
    }

    let mut results = Vec::new();
    if let Ok(resp) = indexd.raw_request(&req).await {
        results = format_search_hits(&resp);
    }

    // If domain-filtered returned fewer than limit, backfill with cross-domain
    if results.len() < limit && domain.is_some() {
        let remaining = limit - results.len();
        let cross_req = serde_json::json!({
            "action": "search", "query": query, "limit": remaining, "source": source,
        });
        if let Ok(resp) = indexd.raw_request(&cross_req).await {
            let cross_hits = format_search_hits(&resp);
            // Deduplicate by id
            let existing_ids: std::collections::HashSet<i64> = results
                .iter()
                .filter_map(|r| r.get("id").and_then(|i| i.as_i64()))
                .collect();
            for hit in cross_hits {
                if let Some(id) = hit.get("id").and_then(|i| i.as_i64())
                    && !existing_ids.contains(&id)
                {
                    results.push(hit);
                }
            }
        }
    }

    results
}

fn format_search_hits(resp: &serde_json::Value) -> Vec<serde_json::Value> {
    resp.get("results")
        .and_then(|r| r.as_array())
        .map(|hits| {
            hits.iter()
                .filter_map(|h| {
                    let meta: serde_json::Value = h
                        .get("metadata")
                        .and_then(|m| m.as_str())
                        .and_then(|s| serde_json::from_str(s).ok())?;
                    let dist = h.get("distance").and_then(|d| d.as_f64()).unwrap_or(1.0);
                    let content = h.get("content").and_then(|c| c.as_str()).unwrap_or("");
                    let snippet = if content.len() > 500 {
                        format!("{}...", &content[..500])
                    } else {
                        content.to_string()
                    };
                    Some(serde_json::json!({
                        "id": h.get("id"),
                        "filename": meta.get("filename"),
                        "section": meta.get("section"),
                        "domain": meta.get("domain"),
                        "date": meta.get("date"),
                        "distance": dist,
                        "snippet": snippet,
                    }))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Find _doc/, _journal/, and _memory/ directories under a workspace root.
fn find_content_dirs(root: &std::path::Path) -> Vec<(std::path::PathBuf, &'static str)> {
    let mut dirs = Vec::new();

    for entry in walkdir(root) {
        let name = entry.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if entry.is_dir() {
            match name {
                "_doc" => dirs.push((entry, "doc")),
                // Decision records were elevated out of _decisions/ to a
                // top-level _decisions/ (cmctl 52e799c0); keep indexing them
                // under the same "doc" source they always had.
                "_decisions" => dirs.push((entry, "doc")),
                "_journal" => dirs.push((entry, "journal")),
                "_memory" => dirs.push((entry, "memory")),
                "_plan" => dirs.push((entry, "plan")),
                "_spec" => dirs.push((entry, "spec")),
                _ => {}
            }
        } else if name == "_notes.md" && entry.is_file() {
            dirs.push((entry, "notes"));
        }
    }

    dirs
}

/// Directory names that are themselves indexed roots (see `find_content_dirs`).
/// `collect_md_files` refuses to descend into a nested one so each special
/// tree is indexed exactly once, under its own source — not doubly via a
/// parent's recursive scan.
const SPECIAL_DIRS: &[&str] = &[
    "_doc",
    "_decisions",
    "_journal",
    "_memory",
    "_plan",
    "_spec",
];

const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    ".venv",
    "vendor",
    ".direnv",
    "dist",
    "build",
];

fn walkdir(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut results = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                results.push(path.clone());
                if path.is_dir() {
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if !SKIP_DIRS.contains(&name) {
                        stack.push(path);
                    }
                }
            }
        }
    }

    results
}

/// Recursively collect every `.md` file under `root`.
///
/// Skips build/vendor dirs (`SKIP_DIRS`) and refuses to descend into a nested
/// registered special dir (`SPECIAL_DIRS`) — that subtree is owned by its own
/// source via `find_content_dirs`, so descending here would double-index it.
/// `root` itself is never name-checked, so scanning e.g. `_doc` works even
/// though `_doc` is in `SPECIAL_DIRS`.
fn collect_md_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut results = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if SKIP_DIRS.contains(&name) || SPECIAL_DIRS.contains(&name) {
                        continue;
                    }
                    stack.push(path);
                } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                    results.push(path);
                }
            }
        }
    }

    results
}

/// Split markdown content on ## headings into (title, text) sections.
fn split_markdown_sections(content: &str, filename: &str) -> Vec<(String, String)> {
    let mut sections = Vec::new();
    let mut current_title = filename.strip_suffix(".md").unwrap_or(filename).to_string();
    let mut current_lines: Vec<&str> = Vec::new();

    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            if !current_lines.is_empty() {
                let text = current_lines.join("\n");
                let text = text.trim();
                if text.len() > 50 {
                    sections.push((current_title.clone(), text.to_string()));
                }
            }
            current_title = rest.trim().to_string();
            current_lines = vec![line];
        } else {
            current_lines.push(line);
        }
    }

    if !current_lines.is_empty() {
        let text = current_lines.join("\n");
        let text = text.trim();
        if text.len() > 50 {
            sections.push((current_title, text.to_string()));
        }
    }

    sections
}

/// Delete all indexed entries for a specific file path.
async fn delete_by_path(
    indexd: &mut cosmix_skills::IndexdClient,
    path: &str,
) -> Result<(), String> {
    // List entries and find those matching this path
    let list_req = serde_json::json!({
        "action": "list", "limit": 1000, "offset": 0,
    });

    let resp = indexd
        .raw_request(&list_req)
        .await
        .map_err(|e| e.to_string())?;
    let mut ids_to_delete = Vec::new();

    if let Some(items) = resp.get("items").and_then(|i| i.as_array()) {
        for item in items {
            if let Some(meta_str) = item.get("metadata").and_then(|m| m.as_str())
                && let Ok(meta) = serde_json::from_str::<serde_json::Value>(meta_str)
                && meta.get("path").and_then(|p| p.as_str()) == Some(path)
                && let Some(id) = item.get("id").and_then(|i| i.as_i64())
            {
                ids_to_delete.push(id);
            }
        }
    }

    if !ids_to_delete.is_empty() {
        let del_req = serde_json::json!({"action": "delete", "ids": ids_to_delete});
        indexd
            .raw_request(&del_req)
            .await
            .map_err(|e| e.to_string())?;
    }

    Ok(())
}

/// Per-call deadline for a tool, in seconds (0 = no deadline). The server
/// must answer every accepted tool call — a wedged backend leg (broker
/// connect, indexd query, embedding HTTP round-trip) previously hung the
/// call forever and the MCP client sat on it until its own idle abort
/// (observed: a context_search received at 01:59:53 on 2026-07-29 that never
/// logged `tool done`; the client gave up after 1800s).
///
/// A deadline is only applied where dropping the in-flight future is safe:
/// NodedClient correlates replies via a pending-map with an RAII removal
/// guard, read-only indexd/HTTP futures are plain drops, and mix_execute's
/// spawn_blocking task is ABANDONED on cancel — it runs to completion in the
/// background (consistent final state, unobserved result).
///
/// Three tools are EXEMPT because they are multi-step mutating pipelines that
/// are NOT cancellation-safe: dropping them mid-await can leave partial state
/// (index_workspace deletes old chunks before re-adding; skills_refine stores
/// the refined skill before superseding the old; skills_graduate appends to
/// CLAUDE.md before its indexd update). index_workspace additionally walks
/// the tree with synchronous std::fs inside the async handler, so on a
/// stalled filesystem the future never yields and a timeout could not fire
/// anyway. Owed: make these pipelines cancellation-safe (transactional step
/// order or a detached spawn), then give them deadlines too.
///
/// `COSMIX_MCP_TOOL_DEADLINE_SECS` overrides the 120s default; `0` disables
/// every deadline (debug escape hatch, exempt tools included).
fn tool_deadline_secs(tool: &str) -> u64 {
    const DEFAULT_SECS: u64 = 120;
    const LONG_SECS: u64 = 600;
    let base = std::env::var("COSMIX_MCP_TOOL_DEADLINE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_SECS);
    if base == 0 {
        return 0;
    }
    match tool {
        // NOT cancellation-safe (see doc above) — no deadline until they are.
        "index_workspace" | "skills_refine" | "skills_graduate" => 0,
        // Non-idempotent `feedback_score += delta` mutations in indexd: a
        // deadline firing after indexd committed but before the reply lands,
        // followed by the error message's advised retry, would apply the
        // delta twice. Owed: request-level idempotency keys in indexd, then
        // these get a deadline too.
        "docs_feedback" | "journal_feedback" | "memory_feedback" => 0,
        // spawn_blocking-backed: cancel = abandon-to-completion, so a deadline
        // is safe; long, because scripts are legitimately slow.
        "mix_execute" => base.max(LONG_SECS),
        _ => base,
    }
}

// `call_tool` is hand-written (not via `#[tool_handler]`) so every tool
// invocation passes through one instrumentation point: redacted arg summary +
// duration + ok/err to all log sinks, full args under RUST_LOG=cosmix_mcp=trace,
// in-memory metrics for the `mcp_status` tool, and the per-call deadline
// (`tool_deadline_secs`). `list_tools`/`get_tool` reproduce the macro's
// bodies verbatim (rmcp 3.1.2 tool_handler expansion).
impl ServerHandler for CosmixMcp {
    fn get_info(&self) -> rmcp::model::ServerInfo {
        rmcp::model::ServerInfo::new(
            rmcp::model::ServerCapabilities::builder().enable_tools().build(),
        )
        .with_instructions(
            "Cosmix Bus (CosMix Agent Bus) bridge with knowledge base and skill learning loop.\n\n\
             Bus tools: bus_call, bus_list_services, bus_node_info, bus_list_peers, noded_ping.\n\
             Log tools: log_tail, log_search.\n\
             Knowledge tools: context_search, index_workspace, knowledge_digest, knowledge_brief.\n\
             Feedback tools: docs_feedback, journal_feedback, memory_feedback, journal_supersede.\n\
             Skill tools: skills_retrieve, skills_store, skills_refine, skills_graduate, skills_list, skills_delete.\n\
             Script tools: mix_execute.\n\
             Status tools: mcp_status (uptime, broker state, per-tool call counts, recent calls).\n\n\
             OBSERVABILITY: every tool call is logged (name + redacted arg summary + duration + ok/err) \
             to stderr, ~/.local/log/cosmix/cosmix-mcp.log, and journald \
             (`journalctl -t cosmix-mcp -f`). Set RUST_LOG=cosmix_mcp=trace for full request args.\n\n\
             KNOWLEDGE PROTOCOL:\n\
             1. At the START of non-trivial tasks, call context_search with a task description.\n\
             2. Review returned skills, docs, and journals for relevant context.\n\
             3. After SUCCESSFULLY completing a non-trivial task, call skills_store to capture what you learned.\n\
             4. If you used a retrieved skill, call skills_refine to report whether it helped.\n\
             5. Use index_workspace to index/re-index a workspace's _doc/ and _journal/ files.\n\
             6. If you used a retrieved chunk, call the matching feedback tool by source type:\n\
                - docs_feedback for _doc/ chunks\n\
                - journal_feedback for _journal/ chunks\n\
                - memory_feedback for _memory/ chunks (CMM output)\n\
                All three update retrieval scoring the same way; the distinction is for your clarity.\n\
             7. If a later journal chunk directly contradicts or invalidates an earlier one,\n\
                call journal_supersede(old_id, new_id, reason). The old chunk stays in the\n\
                database for audit but is filtered from future context_search results.\n\
             8. Before spawning non-trivial Agent subagents, call knowledge_brief to get context for the agent prompt."
        )
    }

    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
        let tool = request.name.to_string();
        // The message string embeds the tool name + redacted summary so the
        // default `journalctl -t cosmix-mcp` short format (which prints only
        // MESSAGE, never structured fields) is self-sufficient for a human
        // following the stream. The structured fields are kept for machine
        // consumers (`journalctl -o json`, the fmt file/stderr layer).
        let args_summary = summarize_args(request.arguments.as_ref());
        tracing::debug!(
            target: "cosmix_mcp",
            tool = %tool,
            args = %args_summary,
            "tool call: {tool} {args_summary}"
        );
        if tracing::enabled!(target: "cosmix_mcp", tracing::Level::TRACE) {
            let full = request
                .arguments
                .as_ref()
                .map(|a| serde_json::Value::Object(a.clone()).to_string())
                .unwrap_or_else(|| "null".to_string());
            tracing::trace!(
                target: "cosmix_mcp",
                tool = %tool,
                args_full = %full,
                "tool call args: {tool} {full}"
            );
        }
        let started = std::time::Instant::now();
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        let deadline = tool_deadline_secs(&tool);
        let result = if deadline == 0 {
            self.tool_router.call(tcc).await
        } else {
            match tokio::time::timeout(
                std::time::Duration::from_secs(deadline),
                self.tool_router.call(tcc),
            )
            .await
            {
                Ok(r) => r,
                Err(_elapsed) => {
                    // Always-on: the silent-hang class this deadline exists to
                    // kill must never be silent again. (The generic Err branch
                    // below logs only the numeric code always-on.)
                    tracing::warn!(
                        target: "cosmix_mcp",
                        tool = %tool,
                        deadline_secs = deadline,
                        "tool deadline exceeded: {tool} after {deadline}s"
                    );
                    // The advice must match the cancellation semantics:
                    // mix_execute's spawn_blocking task is ABANDONED, not
                    // cancelled — the script keeps running to completion, so
                    // a blind retry would execute it twice.
                    let msg = if tool == "mix_execute" {
                        format!(
                            "tool '{tool}' exceeded its {deadline}s deadline. The script was \
                             NOT cancelled — it continues running to completion in the \
                             background; only its result is lost. Do NOT retry blindly: \
                             verify the script's effects first."
                        )
                    } else {
                        format!(
                            "tool '{tool}' exceeded its {deadline}s deadline and was cancelled \
                             — a backend leg (broker connect, indexd, or an embedding/LLM \
                             round-trip) is unresponsive. The server remains usable; retry \
                             once the backend recovers (mcp_status shows broker state)."
                        )
                    };
                    Err(rmcp::ErrorData::internal_error(msg, None))
                }
            }
        };
        let ms = started.elapsed().as_millis() as u64;
        match &result {
            Ok(r) => {
                // rmcp 3 routes parameter-deserialization failures here as a
                // completed result with `is_error: true` (rmcp 1 surfaced them
                // as JSON-RPC INVALID_PARAMS through the Err arm), so `ok` for
                // logs and `mcp_status` metrics must come from the result, not
                // the transport arm.
                let (result_summary, ok) = match r {
                    rmcp::model::CallToolResponse::Complete(r) => {
                        (summarize_result(r), !r.is_error.unwrap_or(false))
                    }
                    rmcp::model::CallToolResponse::InputRequired(_) => {
                        ("[input_required]".to_string(), true)
                    }
                    rmcp::model::CallToolResponse::Task(_) => ("[task]".to_string(), true),
                    _ => ("[unknown]".to_string(), true),
                };
                if ok {
                    tracing::info!(target: "cosmix_mcp", tool = %tool, ms, ok = true, result = %result_summary, "tool done: {tool} ok ({ms}ms) -> {result_summary}");
                } else {
                    tracing::warn!(target: "cosmix_mcp", tool = %tool, ms, ok = false, result = %result_summary, "tool done: {tool} is_error ({ms}ms) -> {result_summary}");
                }
                self.metrics.record(&tool, ms, ok);
            }
            Err(e) => {
                // `e.message` (Cow<str>) can echo untrusted argument values
                // verbatim on serde/rmcp parameter-deserialization failures
                // (e.g. `invalid type: string "<secret>", expected u64`).
                // Same redact-by-default tier as args: the numeric JSON-RPC
                // code is logged always-on; the free-form message is
                // trace-gated only. (Replaces the prior always-on `err = %e`
                // field, which leaked into the file/stderr sinks too.)
                let code = e.code.0;
                tracing::warn!(target: "cosmix_mcp", tool = %tool, ms, ok = false, err_code = code, "tool failed: {tool} ({ms}ms) code={code}");
                if tracing::enabled!(target: "cosmix_mcp", tracing::Level::TRACE) {
                    tracing::trace!(target: "cosmix_mcp", tool = %tool, err = %e, "tool failed detail");
                }
                self.metrics.record(&tool, ms, false);
            }
        }
        result
    }

    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListToolsResult, rmcp::ErrorData> {
        // Cache hints are REQUIRED by protocol 2026-07-28 (SEP-2549); mirror
        // the rmcp tool_handler macro exactly — `ttlMs: 0, cacheScope: public`
        // for peers that negotiated it, absent for older peers.
        let supports_cache_hints = context
            .protocol_version()
            .is_some_and(|version| version >= rmcp::model::ProtocolVersion::V_2026_07_28);
        Ok(rmcp::model::ListToolsResult {
            result_type: Some(rmcp::model::ResultType::COMPLETE),
            tools: self.tool_router.list_all(),
            meta: None,
            next_cursor: None,
            ttl_ms: supports_cache_hints.then_some(0),
            cache_scope: supports_cache_hints.then_some(rmcp::model::CacheScope::Public),
        })
    }

    fn get_tool(&self, name: &str) -> Option<rmcp::model::Tool> {
        self.tool_router.get(name).cloned()
    }
}

#[tokio::main]
async fn main() {
    // --version / -V: print build provenance and exit BEFORE starting the
    // stdio server. mcp connects anonymously (no noded.register), so this
    // CLI line is its only version surface — the 2026-06-01 stale-binary
    // case, where a sha + build_time would have made the staleness obvious.
    if std::env::args()
        .skip(1)
        .any(|a| a == "--version" || a == "-V")
    {
        println!("{}", cosmix_buildinfo::build_info!().line());
        return;
    }

    // Logging via the shared `cosmix_log` core. Preserves cosmix-mcp's
    // hand-tuned posture: the `info,cosmix_mcp=debug` baseline filter
    // (RUST_LOG still overrides — set RUST_LOG=cosmix_mcp=trace for full
    // request args), NEVER rotation, journald (`journalctl -t cosmix-mcp
    // -f`), AND the persistent `~/.local/log/cosmix/cosmix-mcp.log` file
    // the server's own log_tail/log_search tools read. Stats off — this
    // arc is logging-only.
    let _log = cosmix_log::init(
        &cosmix_log::LogOpts::default(),
        &cosmix_log::StatsOpts::default(),
        cosmix_log::LogDefaults::daemon("cosmix-mcp")
            .with_stats(false)
            .with_filter("info,cosmix_mcp=debug")
            .with_rotation(cosmix_log::RotationMode::Never)
            .with_log_file(log_dir()),
    )
    .expect("logging init failed");

    tracing::info!(
        target: "cosmix_mcp",
        "cosmix-mcp starting (broker + indexd connections deferred until first tool call)"
    );
    let server = CosmixMcp {
        noded: OnceCell::new(),
        tool_router: CosmixMcp::tool_router(),
        metrics: Arc::new(Metrics::new()),
    };
    let service = match server.serve(rmcp::transport::io::stdio()).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(target: "cosmix_mcp", error = %e, "serve failed");
            std::process::exit(1);
        }
    };
    // Wait for the service to finish (client disconnect / EOF)
    let _ = service.waiting().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn no_string_value_is_ever_logged_verbatim() {
        // The always-on security invariant: regardless of key, a string value
        // never appears verbatim — only `<str:N chars>`.
        let s = summarize_args(Some(&map(json!({
            "query": "AKIA_SECRET_TOKEN",
            "command": "secret-under-nominally-safe-key",
            "content": "x"
        }))));
        assert!(!s.contains("AKIA_SECRET_TOKEN"), "value leaked: {s}");
        assert!(!s.contains("secret-under"), "value leaked: {s}");
        assert!(s.contains("query=<str:17 chars>"), "{s}");
        assert!(s.contains("content=<str:1 chars>"), "{s}");
    }

    #[test]
    fn non_strings_shown_shape_only_for_containers() {
        let s = summarize_args(Some(&map(json!({
            "limit": 3,
            "flag": true,
            "nil": null,
            "tags": ["a", "b"],
            "obj": {"k": 1}
        }))));
        assert!(s.contains("limit=3"), "{s}");
        assert!(s.contains("flag=true"), "{s}");
        assert!(s.contains("nil=null"), "{s}");
        assert!(s.contains("tags=<arr:2>"), "{s}");
        assert!(s.contains("obj=<obj:1 keys>"), "{s}");
    }

    #[test]
    fn keys_are_scrubbed_and_bounded() {
        // Control bytes in an untrusted key must not reach the log line
        // (ANSI / Trojan-Source / newline log-injection defense).
        let k = "evil\u{1b}[31m\nkey";
        let s = summarize_args(Some(&map(json!({ k: "v" }))));
        assert!(!s.contains('\u{1b}'), "ESC reached log: {s:?}");
        assert!(!s.contains('\n'), "newline reached log: {s:?}");
        assert!(s.contains("evil?[31m?key="), "unexpected scrub: {s}");

        let long = "a".repeat(100);
        let s2 = summarize_args(Some(&map(json!({ long: 1 }))));
        assert!(
            s2.chars().filter(|c| *c == 'a').count() <= 40,
            "key not bounded: {s2}"
        );
        assert!(s2.contains('…'), "no truncation marker: {s2}");
    }

    #[test]
    fn empty_and_absent_args() {
        assert_eq!(summarize_args(None), "{}");
        assert_eq!(summarize_args(Some(&map(json!({})))), "{}");
    }

    #[test]
    fn result_summary_never_logs_body_verbatim() {
        use rmcp::model::{CallToolResult, ContentBlock};

        // Text body must collapse to a char count, never appear verbatim.
        let secret = "SECRET_TOOL_OUTPUT_AKIA123";
        let r = CallToolResult::success(vec![ContentBlock::text(secret)]);
        let s = summarize_result(&r);
        assert!(!s.contains(secret), "result body leaked: {s}");
        assert_eq!(s, format!("[text:{} chars]", secret.chars().count()));

        // Base64 image data is a length only, not the blob.
        let blob = "QUJD".repeat(50);
        let ri = CallToolResult::success(vec![ContentBlock::image(blob.clone(), "image/png")]);
        let si = summarize_result(&ri);
        assert!(!si.contains(&blob), "image blob leaked: {si}");
        assert_eq!(si, format!("[image:{} b64]", blob.len()));

        // is_error result is flagged; empty content reads `empty`.
        let re = CallToolResult::error(vec![]);
        assert_eq!(summarize_result(&re), "[empty] is_error");

        // structured_content is shape-only, never its values.
        let mut rs = CallToolResult::success(vec![]);
        rs.structured_content = Some(json!({"token": "shhh", "n": 1}));
        let ss = summarize_result(&rs);
        assert!(!ss.contains("shhh"), "structured value leaked: {ss}");
        assert_eq!(ss, "[struct:2 keys]");

        // A body large enough to exceed the cap must still keep the
        // bracket wrapper and the `is_error` marker (the load-bearing
        // signal) — body is bounded before wrapping.
        let many = vec![ContentBlock::text("x".repeat(50)); 100];
        let rbig = CallToolResult::error(many);
        let sb = summarize_result(&rbig);
        assert!(sb.starts_with('['), "lost opening bracket: {sb}");
        assert!(sb.ends_with("] is_error"), "lost is_error marker: {sb}");
        assert!(sb.contains('…'), "body not truncated: {sb}");
    }
}
