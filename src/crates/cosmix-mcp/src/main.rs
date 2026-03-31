//! cosmix-mcp — MCP server bridging Claude Code to the cosmix appmesh.
//!
//! Register with: `claude mcp add cosmix-mcp -- ~/.local/bin/cosmix-mcp`
//!
//! Hub connection is lazy — the MCP server starts immediately and only
//! connects to cosmix-hubd on the first tool call, avoiding startup timeouts.

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use serde::Deserialize;
use tokio::sync::OnceCell;

struct CosmixMcp {
    hub: OnceCell<Arc<cosmix_client::HubClient>>,
    tool_router: rmcp::handler::server::tool::ToolRouter<Self>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct AmpCallParams {
    /// Target service name (e.g. "edit", "view", "mon")
    to: String,
    /// AMP command (e.g. "edit.get-content", "view.open")
    command: String,
    /// Optional JSON arguments string (e.g. '{"path": "/tmp/test.md"}')
    #[serde(default)]
    args: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct LogTailParams {
    /// Log file: "amp" for AMP traffic, or app name like "cosmix-edit"
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

impl CosmixMcp {
    async fn hub(&self) -> Result<&Arc<cosmix_client::HubClient>, String> {
        self.hub.get_or_try_init(|| async {
            eprintln!("[cosmix-mcp] connecting to hub...");
            let client = cosmix_client::HubClient::connect_anonymous_default()
                .await
                .map_err(|e| format!("hub connect failed: {e}. Ensure cosmix-hubd is running."))?;
            eprintln!("[cosmix-mcp] connected");
            Ok(Arc::new(client))
        }).await.map_err(|e: String| e)
    }
}

#[tool_router]
impl CosmixMcp {
    /// Call an AMP command on a cosmix service and return the response.
    #[tool]
    async fn amp_call(&self, Parameters(p): Parameters<AmpCallParams>) -> String {
        let hub = match self.hub().await {
            Ok(h) => h,
            Err(e) => return format!("ERROR: {e}"),
        };
        let args_val = p.args
            .and_then(|a: String| serde_json::from_str(&a).ok())
            .unwrap_or(serde_json::Value::Null);
        match hub.call(&p.to, &p.command, args_val).await {
            Ok(val) => serde_json::to_string_pretty(&val).unwrap_or_else(|_| val.to_string()),
            Err(e) => format!("ERROR: {e}"),
        }
    }

    /// List all services currently registered on the cosmix hub.
    #[tool]
    async fn amp_list_services(&self) -> String {
        let hub = match self.hub().await {
            Ok(h) => h,
            Err(e) => return format!("ERROR: {e}"),
        };
        match hub.list_services().await {
            Ok(services) => serde_json::to_string_pretty(&services)
                .unwrap_or_else(|_| format!("{services:?}")),
            Err(e) => format!("ERROR: {e}"),
        }
    }

    /// List all mesh peer nodes.
    #[tool]
    async fn amp_list_peers(&self) -> String {
        let hub = match self.hub().await {
            Ok(h) => h,
            Err(e) => return format!("ERROR: {e}"),
        };
        match hub.call("hub", "hub.peers", serde_json::Value::Null).await {
            Ok(val) => serde_json::to_string_pretty(&val).unwrap_or_else(|_| val.to_string()),
            Err(e) => format!("ERROR: {e}"),
        }
    }

    /// Ping the cosmix hub to check connectivity.
    #[tool]
    async fn hub_ping(&self) -> String {
        let hub = match self.hub().await {
            Ok(h) => h,
            Err(e) => return format!("ERROR: {e}"),
        };
        match hub.call("hub", "hub.ping", serde_json::Value::Null).await {
            Ok(val) => val.to_string(),
            Err(e) => format!("ERROR: {e}"),
        }
    }

    /// Read last N lines from a cosmix log file. file="amp" for AMP traffic.
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
}

fn log_dir() -> PathBuf {
    std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".local/log/cosmix"))
        .unwrap_or_else(|_| PathBuf::from("/tmp/cosmix-log"))
}

fn resolve_log_path(name: &str) -> PathBuf {
    let dir = log_dir();
    if name == "amp" { return dir.join("amp.log"); }
    let exact = dir.join(name);
    if exact.exists() { return exact; }
    if let Ok(entries) = std::fs::read_dir(&dir) {
        let mut m: Vec<PathBuf> = entries.flatten().map(|e| e.path())
            .filter(|p| p.file_name().and_then(|f| f.to_str()).is_some_and(|f| f.starts_with(name)))
            .collect();
        m.sort();
        if let Some(latest) = m.last() { return latest.clone(); }
    }
    dir.join(name)
}

#[tool_handler]
impl ServerHandler for CosmixMcp {
    fn get_info(&self) -> rmcp::model::ServerInfo {
        rmcp::model::ServerInfo::new(
            rmcp::model::ServerCapabilities::builder().enable_tools().build(),
        )
        .with_instructions("Cosmix AppMesh bridge. Use amp_call to send AMP commands to running apps. Use amp_list_services to discover services. Use log_tail/log_search to read logs.")
    }
}

#[tokio::main]
async fn main() {
    eprintln!("[cosmix-mcp] starting (hub connection deferred until first tool call)");
    let server = CosmixMcp { hub: OnceCell::new(), tool_router: CosmixMcp::tool_router() };
    if let Err(e) = server.serve(rmcp::transport::io::stdio()).await {
        eprintln!("[cosmix-mcp] error: {e}");
        std::process::exit(1);
    }
}
