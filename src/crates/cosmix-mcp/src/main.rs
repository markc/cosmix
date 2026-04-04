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
    /// Workspace path to index (e.g. "~/.cosmix", "~/.ns"). Empty = current working directory.
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
struct SkillsGraduateParams {
    /// ID of the skill to manually graduate to CLAUDE.md
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
            p.domain.as_deref().filter(|d| !d.is_empty()).map(|d| d.to_string()).or_else(|| {
                let d = cosmix_skills::detect_domain_cwd();
                if d == "general" { None } else { Some(d) }
            })
        };

        // Load knowledge settings for trust weighting + journal decay
        let k = cosmix_config::store::load()
            .map(|c| c.knowledge)
            .unwrap_or_default();

        let mut result = serde_json::json!({
            "domain": domain.as_deref().unwrap_or("all"),
            "query": &p.query,
            "skills": [],
            "docs": [],
            "journals": [],
            "memory": [],
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

        if let Ok(resp) = indexd.raw_request(&skill_req).await {
            if let Some(hits) = resp.get("results").and_then(|r| r.as_array()) {
                let mut skills: Vec<serde_json::Value> = hits.iter().filter_map(|h| {
                    let meta: serde_json::Value = h.get("metadata")
                        .and_then(|m| m.as_str())
                        .and_then(|s| serde_json::from_str(s).ok())?;
                    // Skip superseded skills (they have a non-zero superseded_by)
                    if meta.get("superseded_by").and_then(|v| v.as_i64()).unwrap_or(0) > 0 {
                        return None;
                    }
                    let raw = h.get("distance").and_then(|d| d.as_f64()).unwrap_or(1.0);
                    let graduated = meta.get("graduated").and_then(|v| v.as_bool()).unwrap_or(false);
                    let trust = if graduated { k.trust_weight_graduated } else { k.trust_weight_skill };
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
                }).collect();
                skills.sort_by(|a, b| {
                    let da = a.get("distance").and_then(|v| v.as_f64()).unwrap_or(1.0);
                    let db = b.get("distance").and_then(|v| v.as_f64()).unwrap_or(1.0);
                    da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
                });
                result["skills"] = serde_json::Value::Array(skills);
            }
        }

        // Search docs (domain-filtered, then cross-domain if few results)
        let mut doc_results = search_source(&mut indexd, &p.query, "doc", domain.as_deref(), limit).await;
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

        // Search journals (domain-filtered, then cross-domain), with temporal decay
        let mut journal_results = search_source(&mut indexd, &p.query, "journal", domain.as_deref(), limit).await;
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
        let mut memory_results = search_source(&mut indexd, &p.query, "memory", domain.as_deref(), limit).await;
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

        serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".into())
    }

    /// Index (or re-index) _doc/ and _journal/ markdown files for a workspace.
    /// Splits files on ## headings, stores each section with domain and source metadata.
    /// Idempotent — deletes old entries for re-indexed files before storing new ones.
    #[tool]
    async fn index_workspace(&self, Parameters(p): Parameters<IndexWorkspaceParams>) -> String {
        let workspace = p.path
            .map(|p| p.replace("~", &std::env::var("HOME").unwrap_or_default()))
            .unwrap_or_else(|| std::env::current_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_default());

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
            let pattern = dir.join("*.md");
            let md_files: Vec<std::path::PathBuf> = glob::glob(&pattern.to_string_lossy())
                .map(|paths| paths.filter_map(|p| p.ok()).collect())
                .unwrap_or_default();

            for filepath in &md_files {
                let filename = filepath.file_name().and_then(|f| f.to_str()).unwrap_or("?");

                // Apply filter if specified
                if let Some(ref filter) = p.filter {
                    if !filename.contains(filter.as_str()) {
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

                // Store one section at a time to avoid long mutex holds
                for (title, text) in &sections {
                    let meta = serde_json::json!({
                        "path": path_str,
                        "filename": filename,
                        "section": title,
                        "domain": &domain,
                        "type": source,
                        "date": date,
                    });

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
                errors.iter().map(|e| serde_json::Value::String(e.clone())).collect()
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
            graduated: false,
            superseded_by: None,
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
                }).to_string()
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
            return serde_json::json!({"graduated": false, "reason": "already graduated"}).to_string();
        }

        match cosmix_skills::check_graduation(&mut indexd, p.id, &existing).await {
            Ok(true) => serde_json::json!({
                "graduated": true,
                "id": p.id,
                "name": &existing.name,
            }).to_string(),
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
                    }).to_string(),
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
            let existing_ids: std::collections::HashSet<i64> = results.iter()
                .filter_map(|r| r.get("id").and_then(|i| i.as_i64()))
                .collect();
            for hit in cross_hits {
                if let Some(id) = hit.get("id").and_then(|i| i.as_i64()) {
                    if !existing_ids.contains(&id) {
                        results.push(hit);
                    }
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
            hits.iter().filter_map(|h| {
                let meta: serde_json::Value = h.get("metadata")
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
            }).collect()
        })
        .unwrap_or_default()
}

/// Find _doc/, _journal/, and _memory/ directories under a workspace root.
fn find_content_dirs(root: &std::path::Path) -> Vec<(std::path::PathBuf, &'static str)> {
    let mut dirs = Vec::new();

    // Walk up to 3 levels deep looking for content dirs
    for entry in walkdir(root, 3) {
        let name = entry.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name == "_doc" && entry.is_dir() {
            dirs.push((entry, "doc"));
        } else if name == "_journal" && entry.is_dir() {
            dirs.push((entry, "journal"));
        } else if name == "_memory" && entry.is_dir() {
            dirs.push((entry, "memory"));
        }
    }

    dirs
}

/// Simple directory walker limited to max_depth levels.
fn walkdir(root: &std::path::Path, max_depth: usize) -> Vec<std::path::PathBuf> {
    let mut results = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];

    while let Some((dir, depth)) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                results.push(path.clone());
                if path.is_dir() && depth < max_depth {
                    stack.push((path, depth + 1));
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
        if line.starts_with("## ") {
            if !current_lines.is_empty() {
                let text = current_lines.join("\n");
                let text = text.trim();
                if text.len() > 50 {
                    sections.push((current_title.clone(), text.to_string()));
                }
            }
            current_title = line[3..].trim().to_string();
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

    let resp = indexd.raw_request(&list_req).await.map_err(|e| e.to_string())?;
    let mut ids_to_delete = Vec::new();

    if let Some(items) = resp.get("items").and_then(|i| i.as_array()) {
        for item in items {
            if let Some(meta_str) = item.get("metadata").and_then(|m| m.as_str()) {
                if let Ok(meta) = serde_json::from_str::<serde_json::Value>(meta_str) {
                    if meta.get("path").and_then(|p| p.as_str()) == Some(path) {
                        if let Some(id) = item.get("id").and_then(|i| i.as_i64()) {
                            ids_to_delete.push(id);
                        }
                    }
                }
            }
        }
    }

    if !ids_to_delete.is_empty() {
        let del_req = serde_json::json!({"action": "delete", "ids": ids_to_delete});
        indexd.raw_request(&del_req).await.map_err(|e| e.to_string())?;
    }

    Ok(())
}

#[tool_handler]
impl ServerHandler for CosmixMcp {
    fn get_info(&self) -> rmcp::model::ServerInfo {
        rmcp::model::ServerInfo::new(
            rmcp::model::ServerCapabilities::builder().enable_tools().build(),
        )
        .with_instructions(
            "Cosmix AppMesh bridge with knowledge base and skill learning loop.\n\n\
             AMP tools: amp_call, amp_list_services, amp_list_peers, hub_ping.\n\
             Log tools: log_tail, log_search.\n\
             Knowledge tools: context_search, index_workspace, docs_feedback.\n\
             Skill tools: skills_retrieve, skills_store, skills_refine, skills_graduate, skills_list, skills_delete.\n\n\
             KNOWLEDGE PROTOCOL:\n\
             1. At the START of non-trivial tasks, call context_search with a task description.\n\
             2. Review returned skills, docs, and journals for relevant context.\n\
             3. After SUCCESSFULLY completing a non-trivial task, call skills_store to capture what you learned.\n\
             4. If you used a retrieved skill, call skills_refine to report whether it helped.\n\
             5. Use index_workspace to index/re-index a workspace's _doc/ and _journal/ files.\n\
             6. If you used retrieved docs/journals, call docs_feedback to report whether each chunk was useful."
        )
    }
}

#[tokio::main]
async fn main() {
    // Init tracing subscriber so RUST_LOG=trace shows rmcp internals on stderr
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    eprintln!("[cosmix-mcp] starting (hub + indexd connections deferred until first tool call)");
    let server = CosmixMcp { hub: OnceCell::new(), tool_router: CosmixMcp::tool_router() };
    let service = match server.serve(rmcp::transport::io::stdio()).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[cosmix-mcp] error: {e}");
            std::process::exit(1);
        }
    };
    // Wait for the service to finish (client disconnect / EOF)
    service.waiting().await;
}
