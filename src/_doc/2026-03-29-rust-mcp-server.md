# Writing a Rust MCP Server for Claude Code

A deep dive into building Model Context Protocol servers in Rust using the official `rmcp` SDK, targeting integration with Claude Code's stdio transport.

## The Landscape: Which Crate?

The Rust MCP ecosystem has several options, but the one that matters is **`rmcp`** — the official SDK maintained under `modelcontextprotocol/rust-sdk` on GitHub. As of March 2026 it's at version 1.2.0 (crate name `rmcp`, currently publishing 0.16.x on crates.io with the 1.x migration underway). It's built on tokio, uses `serde` for serialization, and `schemars` for JSON Schema generation.

Other crates exist (`mcpkit`, `mcp-protocol-sdk`, `mcp_rust_sdk`, `prism-mcp-rs`) but `rmcp` is the canonical implementation with 3.2k GitHub stars, 149 contributors, and conformance tests against the spec. If you're building something serious, use `rmcp`.

## How Claude Code Consumes MCP Servers

Claude Code acts as an MCP **client**. It launches your server as a subprocess and communicates over **stdio** (stdin/stdout) using JSON-RPC 2.0. This is the default and most common transport for local MCP servers.

Key points about Claude Code's MCP integration:

- **Three scopes**: `local` (current project only, default), `project` (shared via `.mcp.json`), `user` (all projects, stored in `~/.claude.json`)
- **Registration**: `claude mcp add <name> -- /path/to/your/binary` for stdio servers
- **JSON config**: `claude mcp add-json <name> '{"command":"/path/to/binary","args":["--flag"],"env":{"KEY":"val"}}'`
- **Timeout**: Configurable via `MCP_TIMEOUT` env var (default is a few seconds)
- **Output limit**: Warning at 10,000 tokens per tool output; adjustable via `MAX_MCP_OUTPUT_TOKENS`
- **Logging**: Your server must **never** write to stdout except MCP JSON-RPC messages. Use stderr for logging.

## Project Setup

### Cargo.toml

```toml
[package]
name = "cosmix-mcp"          # your server name
version = "0.1.0"
edition = "2024"

[dependencies]
rmcp = { version = "0.16", features = ["server"] }
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
schemars = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

The `"server"` feature on `rmcp` pulls in everything needed for the server-side handler trait, tool macros, and stdio transport.

### Binary Entry Point

```rust
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::EnvFilter;

mod server;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Log to stderr only — stdout is reserved for MCP JSON-RPC
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let service = server::MyServer::new();
    let server = service.serve(stdio()).await?;

    // Block until the client disconnects or cancels
    let _quit_reason = server.waiting().await?;
    Ok(())
}
```

The `stdio()` function creates a transport from `(tokio::io::stdin(), tokio::io::stdout())`. The `.serve()` call handles the MCP initialization handshake automatically — your server sends its capabilities, the client sends its capabilities, and then you're live.

## Implementing ServerHandler

The core trait is `ServerHandler`. You implement methods corresponding to the MCP capabilities you want to expose. The trait has default implementations that return "not supported" for everything, so you only override what you need.

### Minimal Tool Server

```rust
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, 
    model::*,
    service::RequestContext,
};
use serde_json::json;

#[derive(Clone)]
pub struct MyServer {
    // your state here
}

impl MyServer {
    pub fn new() -> Self {
        Self {}
    }
}

impl ServerHandler for MyServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: Default::default(),
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .build(),
            server_info: Implementation {
                name: "cosmix-mcp".into(),
                version: "0.1.0".into(),
            },
            instructions: Some(
                "Cosmix infrastructure management tools".into()
            ),
        }
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult {
            tools: vec![
                Tool::new(
                    "node_status",
                    "Get status of a Cosmix mesh node",
                    json!({
                        "type": "object",
                        "properties": {
                            "node": {
                                "type": "string",
                                "description": "Node hostname or AMP address"
                            }
                        },
                        "required": ["node"]
                    }),
                ),
                Tool::new(
                    "list_services",
                    "List running services across the mesh",
                    json!({
                        "type": "object",
                        "properties": {
                            "filter": {
                                "type": "string",
                                "description": "Optional service name filter"
                            }
                        }
                    }),
                ),
            ],
            next_cursor: None,
            meta: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        match request.name.as_str() {
            "node_status" => {
                let node = request.arguments
                    .as_ref()
                    .and_then(|a| a.get("node"))
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| McpError::invalid_params(
                        "missing required parameter: node", None
                    ))?;

                // Your actual logic here
                let status = format!(
                    "Node: {}\nStatus: online\nUptime: 47d 3h\nServices: 12",
                    node
                );

                Ok(CallToolResult {
                    content: vec![Content::text(status)],
                    is_error: Some(false),
                    meta: None,
                })
            }
            "list_services" => {
                let filter = request.arguments
                    .as_ref()
                    .and_then(|a| a.get("filter"))
                    .and_then(|v| v.as_str());

                let services = vec!["dovecot", "postfix", "nginx", "cosmix-jmap"];
                let filtered: Vec<_> = services.into_iter()
                    .filter(|s| filter.map_or(true, |f| s.contains(f)))
                    .collect();

                Ok(CallToolResult {
                    content: vec![Content::text(
                        serde_json::to_string_pretty(&filtered).unwrap()
                    )],
                    is_error: Some(false),
                    meta: None,
                })
            }
            _ => Err(McpError::method_not_found(
                &format!("unknown tool: {}", request.name), None
            )),
        }
    }
}
```

## Using the `#[tool]` Macro (Declarative Approach)

The manual `call_tool` dispatch works but gets tedious. `rmcp` provides proc macros for a cleaner pattern. This is the **recommended** approach for non-trivial servers.

```rust
use rmcp::{
    ServerHandler, ErrorData as McpError, RoleServer,
    handler::server::tool::ToolCallContext,
    model::*,
    schemars::JsonSchema,
    tool,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct NodeStatusArgs {
    /// Node hostname or AMP address
    pub node: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ListServicesArgs {
    /// Optional service name filter
    pub filter: Option<String>,
}

#[derive(Clone)]
pub struct CosmixServer;

#[tool(tool_box)]
impl CosmixServer {
    pub fn new() -> Self { Self }

    /// Get the status of a Cosmix mesh node
    #[tool(name = "node_status", description = "Get status of a Cosmix mesh node")]
    async fn node_status(
        &self,
        #[tool(aggr)] args: NodeStatusArgs,
    ) -> Result<CallToolResult, McpError> {
        let output = format!(
            "Node: {}\nStatus: online\nUptime: 47d 3h",
            args.node
        );
        Ok(CallToolResult::text(output))
    }

    /// List running services across the mesh  
    #[tool(name = "list_services", description = "List services across the mesh")]
    async fn list_services(
        &self,
        #[tool(aggr)] args: ListServicesArgs,
    ) -> Result<CallToolResult, McpError> {
        let all = vec!["dovecot", "postfix", "nginx", "cosmix-jmap"];
        let filtered: Vec<_> = all.into_iter()
            .filter(|s| args.filter.as_deref().map_or(true, |f| s.contains(f)))
            .collect();
        Ok(CallToolResult::text(
            serde_json::to_string_pretty(&filtered).unwrap()
        ))
    }
}

// The #[tool(tool_box)] macro generates the ServerHandler impl
// with list_tools and call_tool routing automatically.
// You still need to provide get_info:
impl ServerHandler for CosmixServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .build(),
            server_info: Implementation {
                name: "cosmix-mcp".into(),
                version: "0.1.0".into(),
            },
            ..Default::default()
        }
    }
}
```

The `#[tool(aggr)]` attribute tells the macro to deserialize the entire arguments map into your struct (as opposed to individual `#[tool(param)]` fields). The `JsonSchema` derive on the args struct auto-generates the input schema that gets reported in `list_tools`.

## Adding Resources

Resources expose read-only data that Claude Code can pull into context. Useful for configuration files, documentation, status dashboards, etc.

```rust
impl ServerHandler for CosmixServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
            // ...
        }
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(ListResourcesResult {
            resources: vec![
                RawResource::new(
                    "config://mesh/topology",
                    "Mesh Topology"
                ).no_annotation(),
                RawResource::new(
                    "config://amp/addressing",
                    "AMP Addressing Scheme"
                ).no_annotation(),
            ],
            next_cursor: None,
            meta: None,
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        match request.uri.as_str() {
            "config://mesh/topology" => {
                let topology = serde_json::json!({
                    "nodes": ["alpha", "beta", "gamma"],
                    "mesh": "wireguard",
                    "addressing": "port.app.node.amp"
                });
                Ok(ReadResourceResult {
                    contents: vec![ResourceContents::text(
                        serde_json::to_string_pretty(&topology).unwrap(),
                        &request.uri
                    )],
                })
            }
            _ => Err(McpError::resource_not_found(
                "resource_not_found",
                Some(serde_json::json!({ "uri": request.uri })),
            )),
        }
    }
}
```

## Adding Prompts

Prompts are reusable message templates. Useful for giving Claude Code canned workflows.

```rust
use rmcp::{
    prompt_router, prompt_handler, prompt,
    handler::server::{router::prompt::PromptRouter, wrapper::Parameters},
};

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct DeployArgs {
    /// Target node for deployment
    pub node: String,
    /// Service to deploy
    pub service: String,
}

#[derive(Clone)]
pub struct CosmixServer {
    prompt_router: PromptRouter<Self>,
}

#[prompt_router]
impl CosmixServer {
    pub fn new() -> Self {
        Self { prompt_router: Self::prompt_router() }
    }

    #[prompt(name = "deploy_checklist", description = "Pre-deployment checklist for a service")]
    async fn deploy_checklist(
        &self,
        Parameters(args): Parameters<DeployArgs>,
    ) -> Vec<PromptMessage> {
        vec![
            PromptMessage::new_text(
                PromptMessageRole::User,
                format!(
                    "Run the deployment checklist for {} on node {}:\n\
                    1. Check service health\n\
                    2. Verify WireGuard mesh connectivity\n\
                    3. Confirm DNS resolution via AMP addressing\n\
                    4. Run smoke tests\n\
                    5. Confirm rollback path exists",
                    args.service, args.node
                ),
            ),
        ]
    }
}

#[prompt_handler]
impl ServerHandler for CosmixServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .build(),
            // ...
        }
    }
}
```

## State Management

Real servers need state. Since `ServerHandler` requires `Clone`, use `Arc<Mutex<_>>` or `Arc<RwLock<_>>` for shared mutable state, or `Arc<AtomicU64>` for simple counters.

```rust
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct CosmixServer {
    state: Arc<RwLock<ServerState>>,
}

struct ServerState {
    connections: HashMap<String, NodeConnection>,
    last_health_check: std::time::Instant,
}

impl CosmixServer {
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(ServerState {
                connections: HashMap::new(),
                last_health_check: std::time::Instant::now(),
            })),
        }
    }
}
```

For Cosmix specifically — where you might be talking to WireGuard peers, querying Proxmox, or hitting Dovecot — the `call_tool` implementation would make async calls to those backends while holding the state lock only briefly to read connection info.

## Accessing the Client (Peer)

The `RequestContext` gives you access to the client peer, letting your server ask the client for things like roots or sampling:

```rust
async fn call_tool(
    &self,
    request: CallToolRequestParams,
    context: RequestContext<RoleServer>,
) -> Result<CallToolResult, McpError> {
    // Ask Claude Code what workspace roots it has
    let roots = context.peer.list_roots().await?;
    
    // Send a log message back to Claude Code
    context.peer.notify_logging_message(LoggingMessageNotificationParam {
        level: LoggingLevel::Info,
        logger: Some("cosmix-mcp".into()),
        data: serde_json::json!({
            "message": "Tool invoked",
            "tool": request.name
        }),
    }).await?;

    // ... actual tool logic
}
```

## Error Handling Best Practices

MCP distinguishes between **protocol errors** (bad JSON-RPC) and **tool errors** (your tool failed but the protocol is fine). For tool errors, return a successful `CallToolResult` with `is_error: Some(true)`:

```rust
// Protocol error — tool doesn't exist
Err(McpError::method_not_found("unknown tool", None))

// Tool error — tool exists but something went wrong
Ok(CallToolResult {
    content: vec![Content::text(
        "Error: Node 'delta' not found in mesh. Available nodes: alpha, beta, gamma.\n\
         Try using the list_services tool first to discover available nodes."
    )],
    is_error: Some(true),
    meta: None,
})
```

Actionable error messages are critical. Claude Code will read these and decide what to do next — vague errors lead to confused retries, specific errors lead to corrective action.

## Tool Annotations

Annotations help the client understand what your tools do without calling them:

```rust
Tool::new("node_status", "Get node status", schema)
    .with_annotations(ToolAnnotations {
        read_only_hint: Some(true),
        destructive_hint: Some(false),
        idempotent_hint: Some(true),
        open_world_hint: Some(true),
    })
```

Claude Code uses these hints to make decisions about tool approval — read-only tools are less likely to need explicit confirmation.

## Naming Conventions

Follow the MCP convention of prefixing tools with your service name to avoid collisions when multiple MCP servers are active:

- `cosmix_node_status` not `node_status`
- `cosmix_list_services` not `list_services`  
- `cosmix_deploy_service` not `deploy`

Use `snake_case`, be action-oriented (verb first: `get`, `list`, `create`, `update`, `delete`).

## Building and Registering with Claude Code

### Build a static binary

```bash
cargo build --release
# Binary at target/release/cosmix-mcp
```

For your CachyOS setup, a standard dynamically-linked build is fine. If you want to distribute to other nodes in the Proxmox cluster, consider static musl builds for the headless server daemons (which you've already established as your pattern):

```bash
# For headless deployment to other nodes
cargo build --release --target x86_64-unknown-linux-musl
```

### Register with Claude Code

```bash
# Local scope (current project only, default)
claude mcp add cosmix -- /path/to/target/release/cosmix-mcp

# User scope (available in all projects)
claude mcp add cosmix --scope user -- /path/to/target/release/cosmix-mcp

# With environment variables
claude mcp add cosmix --scope user \
  --env COSMIX_MESH_CONFIG=/etc/cosmix/mesh.toml \
  -- /path/to/target/release/cosmix-mcp

# Or via JSON for more complex configs
claude mcp add-json cosmix '{
  "command": "/home/mark/cosmix/target/release/cosmix-mcp",
  "args": ["--config", "/etc/cosmix/mesh.toml"],
  "env": {
    "RUST_LOG": "info"
  }
}'
```

### Project-shared config (.mcp.json)

For sharing with collaborators (e.g., Tom), create `.mcp.json` at the project root:

```json
{
  "mcpServers": {
    "cosmix": {
      "command": "./target/release/cosmix-mcp",
      "args": ["--config", "./config/mesh.toml"],
      "env": {
        "RUST_LOG": "info"
      }
    }
  }
}
```

### Verify

```bash
claude mcp list          # See registered servers
claude mcp get cosmix    # Check specific server status
```

Inside Claude Code, use `/mcp` to see connected servers and their tools.

## Testing

### MCP Inspector

The official inspector lets you test your server interactively:

```bash
npx @modelcontextprotocol/inspector -- /path/to/target/release/cosmix-mcp
```

This launches a web UI where you can call tools, read resources, and inspect the JSON-RPC messages.

### Integration test in Rust

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::{ServiceExt, transport::TokioChildProcess};
    use tokio::process::Command;

    #[tokio::test]
    async fn test_server_tools() {
        let client = ().serve(
            TokioChildProcess::new(
                Command::new("cargo")
                    .args(["run", "--release"])
            ).unwrap()
        ).await.unwrap();

        let tools = client.list_tools(None).await.unwrap();
        assert!(!tools.tools.is_empty());

        let result = client.call_tool(CallToolRequestParams {
            name: "cosmix_node_status".into(),
            arguments: Some(serde_json::json!({
                "node": "alpha"
            }).as_object().unwrap().clone()),
            meta: None,
        }).await.unwrap();
        
        assert_eq!(result.is_error, Some(false));
    }
}
```

## Architecture Patterns for Cosmix

Given your ARexx-inspired IPC model and the AMP addressing scheme (`port.app.node.amp`), here's how I'd think about structuring an MCP server for the Cosmix mesh:

**Tool categories:**

1. **Discovery tools** — `cosmix_list_nodes`, `cosmix_list_services`, `cosmix_resolve_amp` (translate AMP addresses). Read-only, idempotent.
2. **Inspection tools** — `cosmix_node_status`, `cosmix_service_logs`, `cosmix_wireguard_peers`. Read-only, open-world (they hit external systems).
3. **Action tools** — `cosmix_restart_service`, `cosmix_deploy`, `cosmix_run_lua_script`. Destructive, need careful error reporting.
4. **AMP messaging tools** — `cosmix_send_amp_message`, `cosmix_query_amp` (send AMP messages to apps, get responses). This maps directly to your ARexx-style `ADDRESS app command` pattern.

**Resources:**

- `config://cosmix/mesh` — current mesh topology
- `config://cosmix/amp-schema` — AMP addressing documentation
- `log://cosmix/{node}/{service}` — resource templates for log access

**Prompts:**

- `cosmix_deploy_checklist` — pre-deployment workflow
- `cosmix_debug_service` — structured debugging prompt with node/service args
- `cosmix_mesh_audit` — full mesh health review prompt

The AMP messaging tools are where this gets interesting — Claude Code could effectively `ADDRESS` any app in your mesh via the MCP server, which is the exact ARexx pattern you're after, just mediated through Claude's tool-calling interface instead of a REXX `ADDRESS` instruction.

## Key Gotchas

1. **stdout is sacred.** Any stray `println!` or library that writes to stdout will corrupt the JSON-RPC stream and crash the connection. Use `tracing` with a stderr writer, or `eprintln!`.

2. **Startup speed matters.** Claude Code has a connection timeout (configurable via `MCP_TIMEOUT`). Don't do heavy initialization in `new()` — defer it to the first tool call or use a lazy init pattern.

3. **Token budget.** Tool outputs over 10,000 tokens get a warning. Keep responses concise. Return structured JSON rather than verbose prose. Claude Code can always ask follow-up questions.

4. **No async in get_info.** The `get_info` method is synchronous. Don't try to read config files or query external services there.

5. **Clone requirement.** `ServerHandler` requires `Clone`. This is why you need `Arc` wrappers for any shared state. The server may clone your handler for internal use.

6. **Schemars version.** `rmcp` currently depends on `schemars` 1.x (the rewrite). Make sure you're using the right version — the 0.8.x API is different.
