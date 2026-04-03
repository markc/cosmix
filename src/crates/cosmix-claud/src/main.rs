//! cosmix-claud — Claude AI AMP port daemon with knowledge-augmented generation.
//!
//! Listens on `/run/user/{uid}/cosmix/ports/claud.sock` and exposes
//! LLM operations as AMP commands. Automatically enriches prompts with
//! relevant context from the cosmix knowledge base (skills, docs, journals)
//! and extracts reusable skills from successful interactions.
//!
//! Commands:
//!   ask      — Send a prompt, auto-enriched with knowledge context
//!   ask_raw  — Send a prompt without knowledge injection (for internal use)
//!   analyze  — Analyze code or errors
//!   generate — Generate code for a task
//!   help     — List available commands
//!   info     — Return port metadata
//!
//! Usage from Mix:
//!   send "claud" "ask" prompt="What is 2+2?"
//!   address "claud"
//!       ask prompt="Explain recursion" context="teaching a beginner"
//!   end
//!
//! Knowledge loop:
//!   1. On `ask`: search indexd for relevant skills/docs/journals
//!   2. Prepend context to system prompt
//!   3. Forward to LLM backend (Haiku by default, configurable)
//!   4. After response: evaluate for skill extraction (async, non-blocking)

use std::path::PathBuf;
use std::sync::Arc;

use cosmix_amp::amp;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixListener;
use tracing::{error, info, warn};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Shared state for the daemon.
struct State {
    /// LLM backend name from config (None = use default).
    llm_backend: Option<String>,
    /// Whether knowledge injection is enabled.
    knowledge_enabled: bool,
    /// Domain for this workspace (auto-detected).
    domain: String,
}

#[tokio::main]
async fn main() {
    let _log = cosmix_daemon::init_tracing("cosmix_claud");
    info!("Starting cosmix-claud with knowledge-augmented generation");

    let socket_path = port_socket_path("claud");
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let _ = std::fs::remove_file(&socket_path);

    let listener = match UnixListener::bind(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            error!("Failed to bind {}: {e}", socket_path.display());
            std::process::exit(1);
        }
    };

    info!("Listening on {}", socket_path.display());

    let domain = cosmix_skills::detect_domain_cwd();
    let cfg = cosmix_config::store::load().unwrap_or_default();
    let llm_backend = if cfg.skills.llm_backend.is_empty() {
        None
    } else {
        Some(cfg.skills.llm_backend.clone())
    };

    let state = Arc::new(State {
        llm_backend,
        knowledge_enabled: true,
        domain,
    });

    loop {
        tokio::select! {
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _)) => {
                        let st = state.clone();
                        tokio::spawn(handle_connection(stream, st));
                    }
                    Err(e) => error!("Accept error: {e}"),
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Shutting down");
                let _ = std::fs::remove_file(&socket_path);
                break;
            }
        }
    }
}

async fn handle_connection(mut stream: tokio::net::UnixStream, state: Arc<State>) {
    if let Err(e) = handle_request(&mut stream, &state).await {
        error!("Connection error: {e}");
    }
}

async fn handle_request(
    stream: &mut tokio::net::UnixStream,
    state: &State,
) -> anyhow::Result<()> {
    let msg = amp::read_from_stream(stream).await?;
    let command = msg.get("command")
        .ok_or_else(|| anyhow::anyhow!("Missing 'command' header"))?
        .to_string();

    let args: serde_json::Value = if msg.body.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(&msg.body)?
    };

    let (rc, body) = match command.as_str() {
        "ask" => handle_ask(&args, state, true).await,
        "ask_raw" => handle_ask(&args, state, false).await,
        "analyze" => handle_analyze(&args, state).await,
        "generate" => handle_generate(&args, state).await,
        "help" => (0, serde_json::json!({
            "commands": ["ask", "ask_raw", "analyze", "generate", "help", "info"],
        })),
        "info" => (0, serde_json::json!({
            "port": "claud",
            "app": "cosmix-claud",
            "version": env!("CARGO_PKG_VERSION"),
            "knowledge_enabled": state.knowledge_enabled,
            "domain": &state.domain,
            "commands": 6,
            "command_list": ["ask", "ask_raw", "analyze", "generate", "help", "info"],
        })),
        _ => (10, serde_json::json!({"error": format!("Unknown command: {command}")})),
    };

    let mut resp = amp::AmpMessage::new();
    resp.set("rc", &rc.to_string());
    resp.body = serde_json::to_string(&body)?;
    stream.write_all(&resp.to_bytes()).await?;

    Ok(())
}

/// Ask with optional knowledge injection.
async fn handle_ask(
    args: &serde_json::Value,
    state: &State,
    inject_knowledge: bool,
) -> (u8, serde_json::Value) {
    let prompt = match args.get("prompt").and_then(|v| v.as_str())
        .or_else(|| args.as_str())
    {
        Some(p) => p.to_string(),
        None => return (10, serde_json::json!({"error": "Missing 'prompt' argument"})),
    };

    let user_context = args.get("context").and_then(|v| v.as_str()).unwrap_or("");

    // Build system prompt with knowledge context
    let mut system_parts: Vec<String> = Vec::new();

    if !user_context.is_empty() {
        system_parts.push(user_context.to_string());
    }

    // Knowledge injection: search indexd for relevant context
    if inject_knowledge && state.knowledge_enabled {
        match search_knowledge(&prompt, &state.domain).await {
            Ok(context) if !context.is_empty() => {
                info!("Injected {} chars of knowledge context", context.len());
                system_parts.push(context);
            }
            Ok(_) => {} // no results
            Err(e) => warn!("Knowledge search failed (continuing without): {e}"),
        }
    }

    let system = system_parts.join("\n\n");

    // Call LLM
    let llm = match create_llm(state) {
        Ok(l) => l,
        Err(e) => return (10, serde_json::json!({"error": format!("LLM init failed: {e}")})),
    };

    match llm.complete(&system, &prompt).await {
        Ok(response) => {
            // Async skill extraction — fire and forget, don't block the response
            if inject_knowledge && state.knowledge_enabled {
                let prompt_clone = prompt.clone();
                let response_clone = response.clone();
                let domain = state.domain.clone();
                let backend = state.llm_backend.clone();
                tokio::spawn(async move {
                    maybe_extract_skill(&prompt_clone, &response_clone, &domain, backend.as_deref()).await;
                });
            }

            (0, serde_json::json!({"response": response}))
        }
        Err(e) => (10, serde_json::json!({"error": format!("LLM call failed: {e}")})),
    }
}

async fn handle_analyze(args: &serde_json::Value, state: &State) -> (u8, serde_json::Value) {
    let code = args.get("code").and_then(|v| v.as_str()).unwrap_or("");
    let error_msg = args.get("error").and_then(|v| v.as_str()).unwrap_or("");
    let language = args.get("language").and_then(|v| v.as_str()).unwrap_or("Mix");

    if code.is_empty() && error_msg.is_empty() {
        return (10, serde_json::json!({"error": "Provide 'code' and/or 'error' arguments"}));
    }

    let mut prompt = format!("Analyze this {language} code issue. Be concise.\n\n");
    if !code.is_empty() {
        prompt.push_str(&format!("Code:\n```\n{code}\n```\n\n"));
    }
    if !error_msg.is_empty() {
        prompt.push_str(&format!("Error: {error_msg}\n"));
    }

    // Analyze uses knowledge injection too
    let (rc, mut body) = handle_ask(
        &serde_json::json!({"prompt": prompt}),
        state,
        true,
    ).await;

    // Rename "response" to "analysis" in output
    if let Some(resp) = body.get("response").and_then(|v| v.as_str()).map(|s| s.to_string()) {
        body = serde_json::json!({"analysis": resp});
    }

    (rc, body)
}

async fn handle_generate(args: &serde_json::Value, state: &State) -> (u8, serde_json::Value) {
    let task = match args.get("task").and_then(|v| v.as_str())
        .or_else(|| args.as_str())
    {
        Some(t) => t,
        None => return (10, serde_json::json!({"error": "Missing 'task' argument"})),
    };

    let language = args.get("language").and_then(|v| v.as_str()).unwrap_or("Mix");

    let prompt = format!(
        "Generate {language} code for this task. Return only the code, no explanation.\n\nTask: {task}"
    );

    let (rc, mut body) = handle_ask(
        &serde_json::json!({"prompt": prompt}),
        state,
        true,
    ).await;

    // Rename "response" to "code" in output
    if let Some(resp) = body.get("response").and_then(|v| v.as_str()).map(|s| s.to_string()) {
        body = serde_json::json!({"code": resp});
    }

    (rc, body)
}

// ---- Knowledge injection ----

/// Search indexd for relevant skills, docs, and journals.
/// Returns a formatted context string to prepend to the system prompt.
async fn search_knowledge(query: &str, domain: &str) -> anyhow::Result<String> {
    let mut indexd = cosmix_skills::IndexdClient::from_config().await?;

    let mut context_parts: Vec<String> = Vec::new();

    // Search skills (domain-filtered)
    let skill_req = serde_json::json!({
        "action": "search", "query": query, "limit": 3, "source": "skill",
        "metadata_filter": [{"field": "domain", "op": "eq", "value": domain}]
    });
    if let Ok(resp) = indexd.raw_request(&skill_req).await {
        let skills = format_skill_hits(&resp);
        if !skills.is_empty() {
            context_parts.push(format!("## Relevant Skills\n\n{skills}"));
        }
    }

    // Search docs (domain-filtered, then cross-domain backfill)
    let doc_req = serde_json::json!({
        "action": "search", "query": query, "limit": 3, "source": "doc",
        "metadata_filter": [{"field": "domain", "op": "eq", "value": domain}]
    });
    if let Ok(resp) = indexd.raw_request(&doc_req).await {
        let docs = format_doc_hits(&resp);
        if !docs.is_empty() {
            context_parts.push(format!("## Relevant Documentation\n\n{docs}"));
        }
    }

    // Search journals (domain-filtered)
    let journal_req = serde_json::json!({
        "action": "search", "query": query, "limit": 2, "source": "journal",
        "metadata_filter": [{"field": "domain", "op": "eq", "value": domain}]
    });
    if let Ok(resp) = indexd.raw_request(&journal_req).await {
        let journals = format_doc_hits(&resp);
        if !journals.is_empty() {
            context_parts.push(format!("## Relevant Journal Entries\n\n{journals}"));
        }
    }

    Ok(context_parts.join("\n\n"))
}

fn format_skill_hits(resp: &serde_json::Value) -> String {
    let Some(results) = resp.get("results").and_then(|r| r.as_array()) else {
        return String::new();
    };

    results.iter().filter_map(|h| {
        let meta: serde_json::Value = h.get("metadata")
            .and_then(|m| m.as_str())
            .and_then(|s| serde_json::from_str(s).ok())?;
        let name = meta.get("name").and_then(|n| n.as_str()).unwrap_or("?");
        let trigger = meta.get("trigger").and_then(|t| t.as_str()).unwrap_or("");
        let approach = meta.get("approach").and_then(|a| a.as_str()).unwrap_or("");
        let confidence = meta.get("confidence").and_then(|c| c.as_f64()).unwrap_or(0.0);
        let failures = meta.get("failure_modes").and_then(|f| f.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", "))
            .unwrap_or_default();

        let mut s = format!("### {name} (confidence: {:.0}%)\n", confidence * 100.0);
        s.push_str(&format!("**When:** {trigger}\n"));
        s.push_str(&format!("**Approach:** {approach}\n"));
        if !failures.is_empty() {
            s.push_str(&format!("**Watch out:** {failures}\n"));
        }
        Some(s)
    }).collect::<Vec<_>>().join("\n")
}

fn format_doc_hits(resp: &serde_json::Value) -> String {
    let Some(results) = resp.get("results").and_then(|r| r.as_array()) else {
        return String::new();
    };

    results.iter().filter_map(|h| {
        let meta: serde_json::Value = h.get("metadata")
            .and_then(|m| m.as_str())
            .and_then(|s| serde_json::from_str(s).ok())?;
        let filename = meta.get("filename").and_then(|f| f.as_str()).unwrap_or("?");
        let section = meta.get("section").and_then(|s| s.as_str()).unwrap_or("?");
        let content = h.get("content").and_then(|c| c.as_str()).unwrap_or("");
        // Truncate to 400 chars
        let snippet = if content.len() > 400 {
            format!("{}...", &content[..400])
        } else {
            content.to_string()
        };

        Some(format!("**{filename} > {section}**\n{snippet}\n"))
    }).collect::<Vec<_>>().join("\n")
}

// ---- Skill extraction (async, post-response) ----

/// Evaluate whether a response is worth extracting as a skill.
/// Runs asynchronously after the response has been returned to the caller.
async fn maybe_extract_skill(
    prompt: &str,
    response: &str,
    domain: &str,
    llm_backend: Option<&str>,
) {
    // Skip very short interactions (not worth learning from)
    if prompt.len() < 100 || response.len() < 100 {
        return;
    }

    let llm = match cosmix_skills::LlmClient::from_config(llm_backend) {
        Ok(l) => l,
        Err(e) => {
            warn!("Skill extraction skipped (LLM init failed): {e}");
            return;
        }
    };

    // Build a minimal transcript for evaluation
    let transcript = cosmix_skills::TaskTranscript {
        task_description: truncate(prompt, 500),
        system_prompt: String::new(),
        messages: vec![
            cosmix_skills::Message { role: "user".into(), content: truncate(prompt, 1000) },
            cosmix_skills::Message { role: "assistant".into(), content: truncate(response, 1000) },
        ],
        tool_calls: vec![],
        final_output: truncate(response, 2000),
        duration_ms: 0,
        token_count: 0,
        success: true,
    };

    // Evaluate: is this worth learning from?
    let eval = match cosmix_skills::evaluate_task(&llm, &transcript).await {
        Ok(Some(score)) => score,
        Ok(None) => return, // not worth extracting
        Err(e) => {
            warn!("Skill evaluation failed: {e}");
            return;
        }
    };

    info!(
        success = eval.success,
        novelty = eval.novelty,
        "Task evaluated for skill extraction"
    );

    // Extract skill
    let skill = match cosmix_skills::extract_skill(&llm, &transcript, &eval).await {
        Ok(mut s) => {
            s.domain = domain.to_string();
            s
        }
        Err(e) => {
            warn!("Skill extraction failed: {e}");
            return;
        }
    };

    // Store in indexd
    let mut indexd = match cosmix_skills::IndexdClient::from_config().await {
        Ok(c) => c,
        Err(e) => {
            warn!("Skill storage skipped (indexd unavailable): {e}");
            return;
        }
    };

    match indexd.store_skill(&skill).await {
        Ok(id) => info!(id, name = %skill.name, "New skill extracted and stored"),
        Err(e) => warn!("Skill storage failed: {e}"),
    }
}

// ---- Helpers ----

fn create_llm(state: &State) -> anyhow::Result<cosmix_llm::LlmClient> {
    cosmix_llm::LlmClient::from_config(state.llm_backend.as_deref())
}

fn port_socket_path(name: &str) -> PathBuf {
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/run/user/{uid}/cosmix/ports/{name}.sock"))
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}
