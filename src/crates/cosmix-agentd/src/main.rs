//! cosmix-agentd — agent loop Bus port.
//!
//! Listens on `/run/user/{uid}/cosmix/ports/agentd.sock` and exposes multi-turn
//! LLM agent loops with pluggable tools.
//!
//! Commands:
//!   session.new   — create a new agent session
//!   message       — send a user message and drive the loop to completion
//!   session.list  — list persisted session IDs
//!   session.close — evict a session from memory (JSONL on disk is preserved)
//!   tools.list    — return registered tool schemas
//!   help / info   — standard cosmix daemon verbs
//!
//! Sessions persist as JSONL under `$COSMIX_VAR/agent/sessions/{id}.jsonl`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use cosmix_agent::{BuiltinTools, Session, ToolRegistry, run_turn};
use cosmix_bus::bus;
use cosmix_llm::LlmClient;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

struct State {
    llm_backend: Option<String>,
    tools: ToolRegistry,
    sessions: Mutex<HashMap<String, Session>>,
}

impl State {
    fn new(llm_backend: Option<String>) -> Self {
        let mut tools = ToolRegistry::new();
        BuiltinTools::register_all(&mut tools);
        Self {
            llm_backend,
            tools,
            sessions: Mutex::new(HashMap::new()),
        }
    }
}

#[tokio::main]
async fn main() {
    let _log = cosmix_log::init(
        &cosmix_log::LogOpts::default(),
        &cosmix_log::StatsOpts::default(),
        cosmix_log::LogDefaults::daemon("cosmix-agentd").with_stats(false),
    )
    .expect("logging init failed");
    info!("Starting cosmix-agentd");

    let socket_path = port_socket_path("agentd");
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

    let skills_cfg = cosmix_config::store::load_service::<cosmix_config::SkillsSettings>("skills")
        .unwrap_or_default();
    // Agent loops need tool-calling LLMs, so the default (`claude-cli`) won't
    // work. Selection order: $COSMIX_AGENT_BACKEND env → llm_backend in
    // skills.toml → hardcoded "claude-api" fallback. Set to "ollama" for
    // zero-cost local tool calling once a tool-capable model is pulled.
    let llm_backend = std::env::var("COSMIX_AGENT_BACKEND")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            if skills_cfg.llm_backend.is_empty() {
                None
            } else {
                Some(skills_cfg.llm_backend.clone())
            }
        })
        .unwrap_or_else(|| "claude-api".to_string());
    info!("Using LLM backend: {llm_backend}");

    let state = Arc::new(State::new(Some(llm_backend)));

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

async fn handle_request(stream: &mut tokio::net::UnixStream, state: &State) -> anyhow::Result<()> {
    let msg = bus::read_from_stream(stream).await?;
    let command = msg
        .get("command")
        .ok_or_else(|| anyhow::anyhow!("Missing 'command' header"))?
        .to_string();

    let args: serde_json::Value = if msg.body.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(&msg.body)?
    };

    let (rc, body) = match command.as_str() {
        "session.new" => handle_session_new(&args, state).await,
        "message" => handle_message(&args, state).await,
        "session.list" => handle_session_list().await,
        "session.close" => handle_session_close(&args, state).await,
        "tools.list" => handle_tools_list(state).await,
        "help" => (
            0,
            serde_json::json!({
                "commands": ["session.new", "message", "session.list", "session.close", "tools.list", "help", "info"],
            }),
        ),
        "info" => (
            0,
            serde_json::json!({
                "port": "agentd",
                "app": "cosmix-agentd",
                "version": env!("CARGO_PKG_VERSION"),
                "tool_count": state.tools.len(),
                "active_sessions": state.sessions.lock().await.len(),
            }),
        ),
        _ => (
            10,
            serde_json::json!({"error": format!("Unknown command: {command}")}),
        ),
    };

    let mut resp = bus::BusMessage::new();
    resp.set("rc", &rc.to_string());
    resp.body = serde_json::to_string(&body)?;
    stream.write_all(&resp.to_bytes()).await?;
    Ok(())
}

async fn handle_session_new(args: &serde_json::Value, state: &State) -> (u8, serde_json::Value) {
    let system = args
        .get("system")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let backend = args
        .get("backend")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| state.llm_backend.clone());
    let session = Session::new(system, backend);
    let id = session.id.clone();
    if let Err(e) = session.save() {
        warn!("save new session: {e}");
    }
    state.sessions.lock().await.insert(id.clone(), session);
    (0, serde_json::json!({"session_id": id}))
}

async fn handle_message(args: &serde_json::Value, state: &State) -> (u8, serde_json::Value) {
    let session_id = match args.get("session_id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return (10, serde_json::json!({"error": "Missing 'session_id'"})),
    };
    let text = match args.get("text").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return (10, serde_json::json!({"error": "Missing 'text'"})),
    };

    let mut session = {
        let mut map = state.sessions.lock().await;
        match map.remove(&session_id) {
            Some(s) => s,
            None => match Session::load(&session_id) {
                Ok(s) => s,
                Err(e) => {
                    return (
                        10,
                        serde_json::json!({"error": format!("session not found: {e}")}),
                    );
                }
            },
        }
    };

    let backend = session
        .backend
        .clone()
        .or_else(|| state.llm_backend.clone());
    let client = match LlmClient::from_config(backend.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            state.sessions.lock().await.insert(session_id, session);
            return (
                10,
                serde_json::json!({"error": format!("LLM init failed: {e}")}),
            );
        }
    };

    let outcome = match run_turn(&mut session, &text, &client, &state.tools).await {
        Ok(o) => o,
        Err(e) => {
            if let Err(se) = session.save() {
                warn!("save after failure: {se}");
            }
            state.sessions.lock().await.insert(session_id, session);
            return (
                10,
                serde_json::json!({"error": format!("agent loop failed: {e}")}),
            );
        }
    };

    if let Err(e) = session.save() {
        warn!("save session: {e}");
    }

    let usage = session.total_usage;
    let message_count = session.messages.len();
    state
        .sessions
        .lock()
        .await
        .insert(session_id.clone(), session);

    (
        0,
        serde_json::json!({
            "session_id": session_id,
            "text": outcome.text,
            "iterations": outcome.iterations,
            "hit_cap": outcome.hit_cap,
            "message_count": message_count,
            "usage": {
                "input_tokens": usage.input_tokens,
                "output_tokens": usage.output_tokens,
            },
        }),
    )
}

async fn handle_session_list() -> (u8, serde_json::Value) {
    let ids = Session::list_ids();
    (0, serde_json::json!({"sessions": ids}))
}

async fn handle_session_close(args: &serde_json::Value, state: &State) -> (u8, serde_json::Value) {
    let session_id = match args.get("session_id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return (10, serde_json::json!({"error": "Missing 'session_id'"})),
    };
    let removed = state.sessions.lock().await.remove(&session_id).is_some();
    (
        0,
        serde_json::json!({"evicted": removed, "session_id": session_id}),
    )
}

async fn handle_tools_list(state: &State) -> (u8, serde_json::Value) {
    let schemas = state.tools.schemas();
    (0, serde_json::json!({"tools": schemas}))
}

fn port_socket_path(name: &str) -> PathBuf {
    let uid = cosmix_config::current_uid();
    PathBuf::from(format!("/run/user/{uid}/cosmix/ports/{name}.sock"))
}
