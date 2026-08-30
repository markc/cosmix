# cosmix-lib-llm

`cosmix-lib-llm` is the generic multi-backend LLM client for Cosmix. It gives
Cos crates one interface for Anthropic, OpenAI-compatible servers, Ollama,
Bus-routed requests, and the Claude CLI. In the `bus <- mix <- cos` dependency
chain, it belongs to the cos substrate layer and calls the bus library directly
for mesh-routed completions.

The Cargo library name is `cosmix_llm`.

## Synopsis

```rust
use cosmix_llm::LlmClient;

let client = LlmClient::from_config(None)?;
let response = client
    .complete("Answer concisely.", "What is 2 + 2?")
    .await?;
```

For multi-turn work, construct messages and a chat request:

```rust
use cosmix_llm::{ChatRequest, ContentBlock, LlmClient, Message};

let client = LlmClient::from_config(Some("local"))?;
let messages = vec![Message::user_text("Summarise the request.")];
let response = client
    .chat(
        ChatRequest::new(&messages)
            .with_system("Answer concisely.")
            .with_max_tokens(1024),
    )
    .await?;

for block in response.content {
    if let ContentBlock::Text { text } = block {
        println!("{text}");
    }
}
```

## Providers

| Provider value | Transport | Default endpoint or command | Native tool use |
|---|---|---|---|
| `anthropic` | Anthropic Messages API | `https://api.anthropic.com` | Yes |
| `openai` | OpenAI-compatible chat completions | `https://api.openai.com` | No |
| `ollama` | Ollama `/api/chat` | `http://localhost:11434` | Yes |
| `bus` | Bus port call | port `claud`, command `ask` | No |
| `claude-cli` | `claude -p` subprocess | binary `claude` | No |

OpenAI-compatible mode covers services which implement
`/v1/chat/completions`, including OpenAI, vLLM, and LM Studio.

The Bus backend joins the system and user prompts, sends them as `prompt`, and
expects a string `response` field in the reply. The Claude CLI backend is
text-only and uses the CLI's existing user authentication.

## Client construction

`LlmClient::from_config(backend_name)` loads the `llm` service settings from
`~/.config/cosmix/llm.toml`. `None` selects the configured default backend. A
name selects that entry from the backend map.

`LlmClient::from_settings(settings, backend_name)` performs the same selection
against an existing `LlmSettings` value.

`LlmClient::from_backend_config(config)` constructs a client directly from one
`LlmBackendConfig`.

The client consumes these configuration fields:

| Field | Purpose |
|---|---|
| `provider` | Selects one of the five provider values |
| `model` | Supplies the backend model identifier |
| `base_url` | Overrides an HTTP endpoint, or the binary in `claude-cli` mode |
| `api_key_env` | Names an environment variable containing an API key |
| `api_key_cmd` | Supplies a shell command which prints an API key |
| `port` | Overrides the Bus port name |
| `command` | Overrides the Bus command name |

Anthropic and OpenAI-compatible backends require an API key. Resolution checks
`api_key_env` first, then executes `api_key_cmd`. Ollama, Bus, and Claude CLI
do not require a key from this library. HTTP base URLs have trailing slashes
removed before request paths are appended.

`LlmClient::provider()` returns the active provider value.
`LlmClient::model()` returns the configured model identifier.

## Text completion

`LlmClient::complete(system, user)` sends one system prompt and one user prompt
and returns the first text result.

Anthropic requests use `/v1/messages` with a fixed output limit of 4096 tokens.
OpenAI-compatible requests use `/v1/chat/completions`; an empty key omits the
Authorization header. Ollama requests use non-streaming `/api/chat`. The
Claude CLI runs in print mode with text output.

All operations are asynchronous. Transport failures, non-success HTTP statuses,
invalid responses, missing response fields, unknown providers, missing keys,
and failed subprocesses return errors.

## Chat types

`Role` identifies a `user` or `assistant` message.

`ContentBlock` represents one content item:

- `Text` contains model or user text.
- `ToolUse` contains a call ID, tool name, and JSON input.
- `ToolResult` contains the originating call ID, result text, and an error flag.

The serialised content-block shape follows Anthropic's tagged content format.
Roles serialise as lower-case strings and block types as snake-case strings.

`Message` contains a role and a list of content blocks. Its constructors are:

- `Message::user_text(text)`
- `Message::assistant_text(text)`
- `Message::user_tool_result(tool_use_id, content, is_error)`

`ToolSchema` describes a callable tool with a name, description, and JSON Schema
input object.

`ChatRequest` borrows the message history and optional tool definitions.
`ChatRequest::new(messages)` sets no system prompt, no tools, and a 4096-token
limit. Builder methods set the system prompt, tools, and token limit.

`ChatResponse` returns content blocks, a `StopReason`, and `Usage` counts.
`ChatResponse::into_message()` converts the response into an assistant message
for appending to history. `ChatResponse::tool_uses()` iterates over tool-call
IDs, names, and JSON inputs.

`StopReason` distinguishes `EndTurn`, `ToolUse`, `MaxTokens`, `StopSequence`,
and `Other`. `Usage` reports input and output token counts when supplied by the
backend.

## Backend behaviour

Anthropic and Ollama implement the full `chat()` path with optional tools.
Anthropic passes the configured token limit and reports its returned stop reason
and usage. Ollama maps tool schemas to function tools, creates synthetic
`ollama_N` IDs for returned calls, and reports prompt and generation counts.
The Ollama request does not send the `max_tokens` value.

OpenAI-compatible, Bus, and Claude CLI backends accept `chat()` only when the
tool list is empty. They reduce the request to `complete()` using the system
prompt and the most recent user text block. Earlier history is not sent through
this fallback. Their chat response reports `EndTurn` and zeroed usage.

Ollama receives the conversation history. User tool results become `tool`
messages, while assistant tool calls become function calls. The request remains
non-streaming.

## Dependencies

The Cargo manifest declares direct dependencies on:

- `cosmix-lib-config` for LLM settings and the current user identity.
- `cosmix-lib-bus` for Bus port calls.
- `reqwest` and `tokio` for asynchronous HTTP and subprocess execution.
- `serde` and `serde_json` for request, response, and tool data.
- `anyhow` for contextual errors.
- `tracing`.

The crate declares no Cargo features.
