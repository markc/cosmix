//! cosmix-mcp — MCP server bridging Claude Code to the cosmix appmesh.
//!
//! Register with: `claude mcp add cosmix-mcp -- ~/.local/bin/cosmix-mcp`
//!
//! Hub connection is lazy — the MCP server starts immediately and only
//! connects to cosmix-hubd on the first tool call, avoiding startup timeouts.
//!
//! Skills tools connect to cosmix-indexd for semantic skill storage and
//! retrieval. The learning loop: retrieve before tasks, capture after.

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

// --- AMP tool params ---

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

    async fn indexd(&self) -> Result<cosmix_skills::IndexdClient, String> {
        cosmix_skills::IndexdClient::from_config()
            .await
            .map_err(|e| format!("indexd connect failed: {e}. Ensure cosmix-indexd is running."))
    }
}

#[tool_router]
impl CosmixMcp {
    // ---- AMP tools ----

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
            cosmix_config::store::load()
                .map(|c| c.skills.max_skills as usize)
                .unwrap_or(3)
        });

        let domain = p.domain.as_deref().filter(|d| !d.is_empty());
        let domain = domain.or_else(|| {
            let d = cosmix_skills::detect_domain_cwd();
            if d == "general" { None } else { Some(Box::leak(d.into_boxed_str()) as &str) }
        });

        let results = match cosmix_skills::retrieve_skills_domain(
            &mut indexd,
            &p.task,
            max,
            domain,
        ).await {
            Ok(r) => r,
            Err(e) => return format!("ERROR: {e}"),
        };

        if results.is_empty() {
            return "No matching skills found.".into();
        }

        // Return structured JSON with IDs (needed for refine) + the formatted prompt section
        let skills_json: Vec<serde_json::Value> = results.iter().map(|(id, doc)| {
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
        }).collect();

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
        let domain = p.domain
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
        };

        match indexd.store_skill(&skill).await {
            Ok(id) => serde_json::json!({
                "stored": true,
                "id": id,
                "name": &skill.name,
                "domain": &skill.domain,
            }).to_string(),
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
        let skills_cfg = cosmix_config::store::load().unwrap_or_default().skills;
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
            Ok(updated) => serde_json::json!({
                "refined": true,
                "id": p.id,
                "name": &updated.name,
                "version": updated.version,
                "confidence": updated.confidence,
                "use_count": updated.use_count,
                "success_count": updated.success_count,
            }).to_string(),
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
                let items: Vec<serde_json::Value> = skills.iter().map(|(id, doc)| {
                    serde_json::json!({
                        "id": id,
                        "name": &doc.name,
                        "domain": &doc.domain,
                        "trigger": &doc.trigger,
                        "confidence": doc.confidence,
                        "use_count": doc.use_count,
                        "version": doc.version,
                    })
                }).collect();
                serde_json::json!({
                    "skills": items,
                    "total": total,
                    "offset": offset,
                    "limit": limit,
                }).to_string()
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
        .with_instructions(
            "Cosmix AppMesh bridge with skill learning loop.\n\n\
             AMP tools: amp_call, amp_list_services, amp_list_peers, hub_ping.\n\
             Log tools: log_tail, log_search.\n\
             Skill tools: skills_retrieve, skills_store, skills_refine, skills_list, skills_delete.\n\n\
             SKILL LEARNING PROTOCOL:\n\
             1. At the START of non-trivial tasks, call skills_retrieve with a task description.\n\
             2. Apply any matching skills to guide your approach.\n\
             3. After SUCCESSFULLY completing a non-trivial task, call skills_store to capture what you learned.\n\
             4. If you used a retrieved skill, call skills_refine to report whether it helped."
        )
    }
}

#[tokio::main]
async fn main() {
    eprintln!("[cosmix-mcp] starting (hub + indexd connections deferred until first tool call)");
    let server = CosmixMcp { hub: OnceCell::new(), tool_router: CosmixMcp::tool_router() };
    if let Err(e) = server.serve(rmcp::transport::io::stdio()).await {
        eprintln!("[cosmix-mcp] error: {e}");
        std::process::exit(1);
    }
}
