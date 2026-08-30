//! Generic multi-backend LLM client for the Cosmix ecosystem.
//!
//! Supports Anthropic, OpenAI-compatible (OpenAI/vLLM/LMStudio), Ollama,
//! and Bus (route through cosmix-claud on the mesh).
//!
//! ```ignore
//! let client = cosmix_llm::LlmClient::from_config(None)?;
//! let response = client.complete("You are helpful.", "What is 2+2?").await?;
//! ```

use anyhow::{Context, Result};
use cosmix_config::{LlmBackendConfig, LlmSettings};
use serde::{Deserialize, Serialize};

// --- Multi-turn chat types (for agent loops) ---

/// Message role in a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// A single piece of a message — text, a tool call the model wants to make,
/// or a tool result fed back to the model.
///
/// Wire format matches Anthropic's content-block shape; other providers are
/// mapped into this when tool-use support lands for them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

/// A message in the conversation history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    pub fn user_tool_result(
        tool_use_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: tool_use_id.into(),
                content: content.into(),
                is_error,
            }],
        }
    }
}

/// A tool the model can call. `input_schema` is a JSON Schema object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// Why the model stopped generating this turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    Other,
}

/// Token usage reported by the backend.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// A multi-turn chat request with optional tools.
#[derive(Debug, Clone)]
pub struct ChatRequest<'a> {
    pub system: Option<&'a str>,
    pub messages: &'a [Message],
    pub tools: &'a [ToolSchema],
    pub max_tokens: u32,
}

impl<'a> ChatRequest<'a> {
    pub fn new(messages: &'a [Message]) -> Self {
        Self {
            system: None,
            messages,
            tools: &[],
            max_tokens: 4096,
        }
    }

    pub fn with_system(mut self, system: &'a str) -> Self {
        self.system = Some(system);
        self
    }

    pub fn with_tools(mut self, tools: &'a [ToolSchema]) -> Self {
        self.tools = tools;
        self
    }

    pub fn with_max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = n;
        self
    }
}

/// A multi-turn chat response.
#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

impl ChatResponse {
    /// Convert into an assistant `Message` ready to append to history.
    pub fn into_message(self) -> Message {
        Message {
            role: Role::Assistant,
            content: self.content,
        }
    }

    /// Iterate over `tool_use` blocks in this response.
    pub fn tool_uses(&self) -> impl Iterator<Item = (&str, &str, &serde_json::Value)> {
        self.content.iter().filter_map(|b| match b {
            ContentBlock::ToolUse { id, name, input } => Some((id.as_str(), name.as_str(), input)),
            _ => None,
        })
    }
}

/// Unified LLM client — wraps a configured backend.
pub struct LlmClient {
    backend: Backend,
    model: String,
}

enum Backend {
    /// Anthropic Messages API (api.anthropic.com/v1/messages).
    Anthropic {
        base_url: String,
        api_key: String,
        http: reqwest::Client,
    },
    /// OpenAI-compatible chat completions (OpenAI, vLLM, LMStudio).
    OpenAi {
        base_url: String,
        api_key: String,
        http: reqwest::Client,
    },
    /// Ollama native /api/chat endpoint.
    Ollama {
        base_url: String,
        http: reqwest::Client,
    },
    /// Route through cosmix-claud Bus port on the mesh.
    Bus { port: String, command: String },
    /// Shell out to the `claude` CLI (Claude Code) in -p mode. Uses the
    /// user's OAuth session, so requests are billed against the Claude MAX
    /// subscription rather than an API key. Text-only — no tool-use support.
    ClaudeCli { binary: String },
}

impl LlmClient {
    /// Create a client from `~/.config/cosmix/llm.toml`.
    ///
    /// `backend_name`: specific backend key, or `None` to use the default.
    pub fn from_config(backend_name: Option<&str>) -> Result<Self> {
        let llm = cosmix_config::store::load_service::<LlmSettings>("llm").unwrap_or_default();
        let name = backend_name.unwrap_or(&llm.default);
        let cfg = llm
            .backends
            .get(name)
            .with_context(|| format!("LLM backend '{name}' not found in llm.toml [backends]"))?;
        Self::from_backend_config(cfg)
    }

    /// Create from an explicit backend config.
    pub fn from_backend_config(cfg: &LlmBackendConfig) -> Result<Self> {
        let model = cfg.model.clone();
        let backend = match cfg.provider.as_str() {
            "anthropic" => Backend::Anthropic {
                base_url: normalise_url(&cfg.base_url, "https://api.anthropic.com"),
                api_key: resolve_api_key(cfg)?,
                http: reqwest::Client::new(),
            },
            "openai" => Backend::OpenAi {
                base_url: normalise_url(&cfg.base_url, "https://api.openai.com"),
                api_key: resolve_api_key(cfg)?,
                http: reqwest::Client::new(),
            },
            "ollama" => Backend::Ollama {
                base_url: normalise_url(&cfg.base_url, "http://localhost:11434"),
                http: reqwest::Client::new(),
            },
            "bus" => Backend::Bus {
                port: if cfg.port.is_empty() {
                    "claud".into()
                } else {
                    cfg.port.clone()
                },
                command: if cfg.command.is_empty() {
                    "ask".into()
                } else {
                    cfg.command.clone()
                },
            },
            "claude-cli" => Backend::ClaudeCli {
                binary: if cfg.base_url.is_empty() {
                    "claude".into()
                } else {
                    cfg.base_url.clone()
                },
            },
            other => anyhow::bail!("Unknown LLM provider: '{other}'"),
        };
        Ok(Self { backend, model })
    }

    /// Send a system + user prompt, get a text response.
    pub async fn complete(&self, system: &str, user: &str) -> Result<String> {
        match &self.backend {
            Backend::Anthropic {
                base_url,
                api_key,
                http,
            } => anthropic_complete(http, base_url, api_key, &self.model, system, user).await,
            Backend::OpenAi {
                base_url,
                api_key,
                http,
            } => openai_complete(http, base_url, api_key, &self.model, system, user).await,
            Backend::Ollama { base_url, http } => {
                ollama_complete(http, base_url, &self.model, system, user).await
            }
            Backend::Bus { port, command } => bus_complete(port, command, system, user).await,
            Backend::ClaudeCli { binary } => {
                claude_cli_complete(binary, &self.model, system, user).await
            }
        }
    }

    /// Multi-turn chat with optional tool use. Foundation for agent loops.
    ///
    /// Anthropic and Ollama backends support tool use natively. Other
    /// backends (OpenAI-compatible, Bus, ClaudeCli) fall back to a single
    /// `complete()` call for plain-text requests and will error if `tools`
    /// is non-empty.
    pub async fn chat(&self, req: ChatRequest<'_>) -> Result<ChatResponse> {
        if let Backend::Anthropic {
            base_url,
            api_key,
            http,
        } = &self.backend
        {
            return anthropic_chat(http, base_url, api_key, &self.model, req).await;
        }

        if let Backend::Ollama { base_url, http } = &self.backend {
            return ollama_chat(http, base_url, &self.model, req).await;
        }

        if !req.tools.is_empty() {
            anyhow::bail!(
                "{} backend does not yet support tool use — use anthropic or ollama for agent loops",
                self.provider()
            );
        }

        let system = req.system.unwrap_or("");
        let user = last_user_text(req.messages)
            .context("chat() requires at least one user text message")?
            .to_string();
        let text = self.complete(system, &user).await?;
        Ok(ChatResponse {
            content: vec![ContentBlock::Text { text }],
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        })
    }

    /// The model identifier for the active backend.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Which provider is active.
    pub fn provider(&self) -> &str {
        match &self.backend {
            Backend::Anthropic { .. } => "anthropic",
            Backend::OpenAi { .. } => "openai",
            Backend::Ollama { .. } => "ollama",
            Backend::Bus { .. } => "bus",
            Backend::ClaudeCli { .. } => "claude-cli",
        }
    }

    /// Create from the LlmSettings with an explicit backend name override.
    pub fn from_settings(llm: &LlmSettings, backend_name: Option<&str>) -> Result<Self> {
        let name = backend_name.unwrap_or(&llm.default);
        let cfg = llm
            .backends
            .get(name)
            .with_context(|| format!("LLM backend '{name}' not found"))?;
        Self::from_backend_config(cfg)
    }
}

// --- Anthropic Messages API ---

async fn anthropic_complete(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    system: &str,
    user: &str,
) -> Result<String> {
    let body = serde_json::json!({
        "model": model,
        "max_tokens": 4096,
        "system": system,
        "messages": [{"role": "user", "content": user}],
    });

    let resp = http
        .post(format!("{base_url}/v1/messages"))
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .context("sending request to Anthropic API")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("Anthropic API returned {status}: {text}");
    }

    let data: AnthropicResponse = resp.json().await.context("parsing Anthropic response")?;
    data.content
        .into_iter()
        .find(|b| b.r#type == "text")
        .map(|b| b.text)
        .context("No text block in Anthropic response")
}

#[derive(Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicBlock>,
}

#[derive(Deserialize)]
struct AnthropicBlock {
    r#type: String,
    #[serde(default)]
    text: String,
}

async fn anthropic_chat(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    req: ChatRequest<'_>,
) -> Result<ChatResponse> {
    let mut body = serde_json::json!({
        "model": model,
        "max_tokens": req.max_tokens,
        "messages": req.messages,
    });
    if let Some(s) = req.system {
        body["system"] = serde_json::Value::String(s.to_string());
    }
    if !req.tools.is_empty() {
        body["tools"] = serde_json::to_value(req.tools)?;
    }

    let resp = http
        .post(format!("{base_url}/v1/messages"))
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .context("sending chat request to Anthropic API")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("Anthropic API returned {status}: {text}");
    }

    let data: AnthropicChatResponse = resp
        .json()
        .await
        .context("parsing Anthropic chat response")?;

    let stop_reason = match data.stop_reason.as_deref() {
        Some("end_turn") => StopReason::EndTurn,
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        Some("stop_sequence") => StopReason::StopSequence,
        _ => StopReason::Other,
    };

    Ok(ChatResponse {
        content: data.content,
        stop_reason,
        usage: Usage {
            input_tokens: data.usage.input_tokens,
            output_tokens: data.usage.output_tokens,
        },
    })
}

#[derive(Deserialize)]
struct AnthropicChatResponse {
    content: Vec<ContentBlock>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: AnthropicUsage,
}

#[derive(Deserialize, Default)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
}

// --- OpenAI-compatible chat completions ---

async fn openai_complete(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    system: &str,
    user: &str,
) -> Result<String> {
    let body = serde_json::json!({
        "model": model,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
    });

    let mut req = http
        .post(format!("{base_url}/v1/chat/completions"))
        .header("content-type", "application/json")
        .json(&body);

    if !api_key.is_empty() {
        req = req.header("authorization", format!("Bearer {api_key}"));
    }

    let resp = req
        .send()
        .await
        .context("sending request to OpenAI-compatible API")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("OpenAI API returned {status}: {text}");
    }

    let data: OpenAiResponse = resp.json().await.context("parsing OpenAI response")?;
    data.choices
        .into_iter()
        .next()
        .map(|c| c.message.content)
        .context("No choices in OpenAI response")
}

#[derive(Deserialize)]
struct OpenAiResponse {
    choices: Vec<OpenAiChoice>,
}

#[derive(Deserialize)]
struct OpenAiChoice {
    message: OpenAiMessage,
}

#[derive(Deserialize)]
struct OpenAiMessage {
    content: String,
}

// --- Ollama /api/chat ---

async fn ollama_complete(
    http: &reqwest::Client,
    base_url: &str,
    model: &str,
    system: &str,
    user: &str,
) -> Result<String> {
    let body = OllamaChatRequest {
        model,
        messages: vec![
            OllamaChatMessage {
                role: "system",
                content: system,
            },
            OllamaChatMessage {
                role: "user",
                content: user,
            },
        ],
        stream: false,
    };

    let resp = http
        .post(format!("{base_url}/api/chat"))
        .json(&body)
        .send()
        .await
        .context("sending request to Ollama")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("Ollama returned {status}: {text}");
    }

    let data: OllamaChatResponse = resp.json().await.context("parsing Ollama response")?;
    Ok(data.message.content)
}

#[derive(Serialize)]
struct OllamaChatRequest<'a> {
    model: &'a str,
    messages: Vec<OllamaChatMessage<'a>>,
    stream: bool,
}

#[derive(Serialize)]
struct OllamaChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct OllamaChatResponse {
    message: OllamaResponseMessage,
}

#[derive(Deserialize)]
struct OllamaResponseMessage {
    content: String,
}

async fn ollama_chat(
    http: &reqwest::Client,
    base_url: &str,
    model: &str,
    req: ChatRequest<'_>,
) -> Result<ChatResponse> {
    let mut messages: Vec<serde_json::Value> = Vec::new();
    if let Some(sys) = req.system
        && !sys.is_empty()
    {
        messages.push(serde_json::json!({"role": "system", "content": sys}));
    }

    for m in req.messages {
        match m.role {
            Role::User => {
                for block in &m.content {
                    match block {
                        ContentBlock::Text { text } => {
                            messages.push(serde_json::json!({"role": "user", "content": text}));
                        }
                        ContentBlock::ToolResult { content, .. } => {
                            messages.push(serde_json::json!({"role": "tool", "content": content}));
                        }
                        ContentBlock::ToolUse { .. } => {}
                    }
                }
            }
            Role::Assistant => {
                let mut text_parts: Vec<&str> = Vec::new();
                let mut tool_calls: Vec<serde_json::Value> = Vec::new();
                for block in &m.content {
                    match block {
                        ContentBlock::Text { text } => text_parts.push(text),
                        ContentBlock::ToolUse { name, input, .. } => {
                            tool_calls.push(serde_json::json!({
                                "function": {"name": name, "arguments": input}
                            }));
                        }
                        ContentBlock::ToolResult { .. } => {}
                    }
                }
                let mut msg = serde_json::json!({
                    "role": "assistant",
                    "content": text_parts.join("\n"),
                });
                if !tool_calls.is_empty() {
                    msg["tool_calls"] = serde_json::Value::Array(tool_calls);
                }
                messages.push(msg);
            }
        }
    }

    let tools: Vec<serde_json::Value> = req
        .tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                }
            })
        })
        .collect();

    let mut body = serde_json::json!({
        "model": model,
        "messages": messages,
        "stream": false,
    });
    if !tools.is_empty() {
        body["tools"] = serde_json::Value::Array(tools);
    }

    let resp = http
        .post(format!("{base_url}/api/chat"))
        .json(&body)
        .send()
        .await
        .context("sending chat request to Ollama")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("Ollama returned {status}: {text}");
    }

    let data: OllamaChatEnvelope = resp.json().await.context("parsing Ollama chat response")?;

    let mut content: Vec<ContentBlock> = Vec::new();
    if !data.message.content.is_empty() {
        content.push(ContentBlock::Text {
            text: data.message.content,
        });
    }
    let num_tool_calls = data
        .message
        .tool_calls
        .as_ref()
        .map(|v| v.len())
        .unwrap_or(0);
    if let Some(tool_calls) = data.message.tool_calls {
        for (i, call) in tool_calls.into_iter().enumerate() {
            content.push(ContentBlock::ToolUse {
                id: format!("ollama_{i}"),
                name: call.function.name,
                input: call.function.arguments,
            });
        }
    }

    let stop_reason = if num_tool_calls > 0 {
        StopReason::ToolUse
    } else {
        match data.done_reason.as_deref() {
            Some("stop") => StopReason::EndTurn,
            Some("length") => StopReason::MaxTokens,
            _ => StopReason::EndTurn,
        }
    };

    Ok(ChatResponse {
        content,
        stop_reason,
        usage: Usage {
            input_tokens: data.prompt_eval_count.unwrap_or(0),
            output_tokens: data.eval_count.unwrap_or(0),
        },
    })
}

#[derive(Deserialize)]
struct OllamaChatEnvelope {
    message: OllamaChatReply,
    #[serde(default)]
    done_reason: Option<String>,
    #[serde(default)]
    prompt_eval_count: Option<u32>,
    #[serde(default)]
    eval_count: Option<u32>,
}

#[derive(Deserialize)]
struct OllamaChatReply {
    #[serde(default)]
    content: String,
    #[serde(default)]
    tool_calls: Option<Vec<OllamaToolCall>>,
}

#[derive(Deserialize)]
struct OllamaToolCall {
    function: OllamaToolCallFunction,
}

#[derive(Deserialize)]
struct OllamaToolCallFunction {
    name: String,
    #[serde(default)]
    arguments: serde_json::Value,
}

// --- Claude Code CLI (MAX plan via `claude -p`) ---

async fn claude_cli_complete(
    binary: &str,
    model: &str,
    system: &str,
    user: &str,
) -> Result<String> {
    let mut cmd = tokio::process::Command::new(binary);
    cmd.arg("-p").arg("--output-format").arg("text");
    if !system.is_empty() {
        cmd.arg("--system-prompt").arg(system);
    }
    if !model.is_empty() {
        cmd.arg("--model").arg(model);
    }
    cmd.arg(user);

    let output = cmd
        .output()
        .await
        .with_context(|| format!("spawning `{binary} -p`"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("`{binary} -p` exited with {}: {stderr}", output.status);
    }

    let stdout =
        String::from_utf8(output.stdout).context("`claude -p` stdout was not valid UTF-8")?;
    Ok(stdout.trim_end().to_string())
}

// --- Bus port (cosmix-claud) ---

async fn bus_complete(port: &str, command: &str, system: &str, user: &str) -> Result<String> {
    let uid = cosmix_config::current_uid();
    let socket_path = format!("/run/user/{uid}/cosmix/ports/{port}.sock");

    let prompt = if system.is_empty() {
        user.to_string()
    } else {
        format!("Context: {system}\n\n{user}")
    };

    let args = serde_json::json!({ "prompt": prompt });
    let resp = cosmix_bus::call_port(&socket_path, command, args)
        .await
        .context("calling cosmix-claud Bus port")?;

    resp.get("response")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .context("No 'response' field in Bus reply")
}

// --- Helpers ---

fn normalise_url(url: &str, default: &str) -> String {
    if url.is_empty() {
        default.to_string()
    } else {
        url.trim_end_matches('/').to_string()
    }
}

fn resolve_api_key(cfg: &LlmBackendConfig) -> Result<String> {
    // Try env var first.
    if !cfg.api_key_env.is_empty()
        && let Ok(key) = std::env::var(&cfg.api_key_env)
        && !key.is_empty()
    {
        return Ok(key);
    }

    // Try shell command.
    if !cfg.api_key_cmd.is_empty() {
        let output = std::process::Command::new("sh")
            .args(["-c", &cfg.api_key_cmd])
            .output()
            .with_context(|| format!("running api_key_cmd: {}", cfg.api_key_cmd))?;
        if output.status.success() {
            let key = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !key.is_empty() {
                return Ok(key);
            }
        }
    }

    // Ollama, Bus, and ClaudeCli don't need API keys.
    // (ClaudeCli authenticates via the user's OAuth session / MAX plan.)
    if cfg.provider == "ollama" || cfg.provider == "bus" || cfg.provider == "claude-cli" {
        return Ok(String::new());
    }

    anyhow::bail!(
        "No API key for provider '{}': set {} env var or api_key_cmd",
        cfg.provider,
        if cfg.api_key_env.is_empty() {
            "an api_key_env"
        } else {
            &cfg.api_key_env
        }
    )
}

fn last_user_text(messages: &[Message]) -> Option<&str> {
    messages.iter().rev().find_map(|m| {
        if m.role != Role::User {
            return None;
        }
        m.content.iter().find_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_use_block_serializes_to_anthropic_format() {
        let block = ContentBlock::ToolUse {
            id: "toolu_01".into(),
            name: "read_file".into(),
            input: serde_json::json!({"path": "foo.rs"}),
        };
        let v = serde_json::to_value(&block).unwrap();
        assert_eq!(v["type"], "tool_use");
        assert_eq!(v["id"], "toolu_01");
        assert_eq!(v["name"], "read_file");
        assert_eq!(v["input"]["path"], "foo.rs");
    }

    #[test]
    fn tool_result_message_serializes_correctly() {
        let msg = Message::user_tool_result("toolu_01", "file contents", false);
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["role"], "user");
        assert_eq!(v["content"][0]["type"], "tool_result");
        assert_eq!(v["content"][0]["tool_use_id"], "toolu_01");
        assert_eq!(v["content"][0]["content"], "file contents");
        assert_eq!(v["content"][0]["is_error"], false);
    }

    #[test]
    fn role_serializes_lowercase() {
        let m = Message::assistant_text("hello");
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["role"], "assistant");
        assert_eq!(v["content"][0]["type"], "text");
        assert_eq!(v["content"][0]["text"], "hello");
    }

    #[test]
    fn anthropic_chat_envelope_parses() {
        let raw = r#"{
            "content": [
                {"type": "text", "text": "I'll read that file"},
                {"type": "tool_use", "id": "toolu_01", "name": "read_file", "input": {"path": "Cargo.toml"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 42, "output_tokens": 17}
        }"#;
        let parsed: AnthropicChatResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.content.len(), 2);
        assert_eq!(parsed.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(parsed.usage.input_tokens, 42);
        assert_eq!(parsed.usage.output_tokens, 17);
        match &parsed.content[1] {
            ContentBlock::ToolUse { name, input, .. } => {
                assert_eq!(name, "read_file");
                assert_eq!(input["path"], "Cargo.toml");
            }
            _ => panic!("expected tool_use block"),
        }
    }

    #[test]
    fn chat_request_builder_defaults() {
        let msgs = vec![Message::user_text("hi")];
        let req = ChatRequest::new(&msgs);
        assert!(req.system.is_none());
        assert_eq!(req.max_tokens, 4096);
        assert!(req.tools.is_empty());
    }

    #[test]
    fn ollama_chat_envelope_parses_tool_calls() {
        let raw = r#"{
            "message": {
                "role": "assistant",
                "content": "I'll check the weather",
                "tool_calls": [
                    {"function": {"name": "get_weather", "arguments": {"city": "Paris"}}}
                ]
            },
            "done_reason": "stop",
            "prompt_eval_count": 120,
            "eval_count": 35
        }"#;
        let parsed: OllamaChatEnvelope = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.message.content, "I'll check the weather");
        let calls = parsed.message.tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "get_weather");
        assert_eq!(calls[0].function.arguments["city"], "Paris");
        assert_eq!(parsed.prompt_eval_count, Some(120));
        assert_eq!(parsed.eval_count, Some(35));
    }

    #[test]
    fn ollama_chat_envelope_parses_without_tool_calls() {
        let raw = r#"{
            "message": {"role": "assistant", "content": "hello there"},
            "done_reason": "stop"
        }"#;
        let parsed: OllamaChatEnvelope = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.message.content, "hello there");
        assert!(parsed.message.tool_calls.is_none());
    }

    #[test]
    fn tool_uses_iterator_filters_text_blocks() {
        let resp = ChatResponse {
            content: vec![
                ContentBlock::Text {
                    text: "reading".into(),
                },
                ContentBlock::ToolUse {
                    id: "a".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({"path": "x"}),
                },
                ContentBlock::ToolUse {
                    id: "b".into(),
                    name: "list_files".into(),
                    input: serde_json::json!({}),
                },
            ],
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        };
        let tools: Vec<_> = resp.tool_uses().map(|(_, n, _)| n).collect();
        assert_eq!(tools, vec!["read_file", "list_files"]);
    }
}
