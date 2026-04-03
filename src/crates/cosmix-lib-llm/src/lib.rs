//! Generic multi-backend LLM client for the Cosmix ecosystem.
//!
//! Supports Anthropic, OpenAI-compatible (OpenAI/vLLM/LMStudio), Ollama,
//! and AMP (route through cosmix-claud on the mesh).
//!
//! ```no_run
//! let client = cosmix_llm::LlmClient::from_config(None)?;
//! let response = client.complete("You are helpful.", "What is 2+2?").await?;
//! ```

use anyhow::{Context, Result};
use cosmix_config::{LlmBackendConfig, LlmSettings};
use serde::{Deserialize, Serialize};

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
    /// Route through cosmix-claud AMP port on the mesh.
    Amp {
        port: String,
        command: String,
    },
}

impl LlmClient {
    /// Create a client from settings.toml.
    ///
    /// `backend_name`: specific backend key, or `None` to use the default.
    pub fn from_config(backend_name: Option<&str>) -> Result<Self> {
        let settings = cosmix_config::store::load().unwrap_or_default();
        let llm = &settings.llm;
        let name = backend_name.unwrap_or(&llm.default);
        let cfg = llm
            .backends
            .get(name)
            .with_context(|| format!("LLM backend '{name}' not found in settings.toml [llm.backends]"))?;
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
            "amp" => Backend::Amp {
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
            Backend::Amp { port, command } => amp_complete(port, command, system, user).await,
        }
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
            Backend::Amp { .. } => "amp",
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

    let resp = req.send().await.context("sending request to OpenAI-compatible API")?;

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

// --- AMP port (cosmix-claud) ---

async fn amp_complete(port: &str, command: &str, system: &str, user: &str) -> Result<String> {
    let uid = unsafe { libc::getuid() };
    let socket_path = format!("/run/user/{uid}/cosmix/ports/{port}.sock");

    let prompt = if system.is_empty() {
        user.to_string()
    } else {
        format!("Context: {system}\n\n{user}")
    };

    let args = serde_json::json!({ "prompt": prompt });
    let resp = cosmix_amp::call_port(&socket_path, command, args)
        .await
        .context("calling cosmix-claud AMP port")?;

    resp.get("response")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .context("No 'response' field in AMP reply")
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
    if !cfg.api_key_env.is_empty() {
        if let Ok(key) = std::env::var(&cfg.api_key_env) {
            if !key.is_empty() {
                return Ok(key);
            }
        }
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

    // Ollama and AMP don't need keys.
    if cfg.provider == "ollama" || cfg.provider == "amp" {
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
