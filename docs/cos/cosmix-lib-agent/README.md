# cosmix-lib-agent

`cosmix-lib-agent` provides the agent turn loop, persistent conversation sessions,
and a pluggable tool registry for LLM-driven workflows. It belongs to the `cos`
layer of the `bus <- mix <- cos` dependency chain and uses other `cos` libraries
for LLM access and Cosmix path resolution; it does not define Bus protocol types
or Mix language behaviour.

The Cargo package is named `cosmix-lib-agent`. Rust code imports it as
`cosmix_agent`.

## Synopsis

```rust
use cosmix_agent::{BuiltinTools, Session, ToolRegistry};

let mut tools = ToolRegistry::new();
BuiltinTools::register_all(&mut tools);

let session = Session::new(
    Some("Answer tersely.".to_string()),
    Some("default".to_string()),
);

assert_eq!(tools.len(), 3);
assert!(session.messages.is_empty());
```

`run_turn` additionally requires an initialised `cosmix_llm::LlmClient`.

## Modules

| Module | Purpose |
| --- | --- |
| `runner` | Drives one user-to-assistant turn, including repeated model and tool calls. |
| `session` | Owns conversation state, usage totals, identifiers, and JSONL persistence. |
| `tools` | Defines the tool contract, registry, and bundled file tools. |

The crate root re-exports the main entry points:

- `run_turn` and `TurnOutcome`
- `Session`, `SessionId`, and `sessions_dir`
- `Tool`, `ToolRegistry`, and `BuiltinTools`

## Agent turn loop

```rust
pub async fn run_turn(
    session: &mut Session,
    user_text: &str,
    client: &LlmClient,
    tools: &ToolRegistry,
) -> anyhow::Result<TurnOutcome>
```

`run_turn` appends the user message, sends the complete session history to the
LLM client, and advertises every schema in the supplied registry. It applies the
session system prompt when one is present.

If the response contains tool requests, the function:

1. Appends the assistant response to the session.
2. Resolves each requested tool by name.
3. Runs the calls in response order.
4. Appends their results together as a user-role message.
5. Calls the model again with the expanded history.

The loop ends when the model returns no tool requests or reaches
`MAX_ITERATIONS`. Tool execution failures and unknown tool names become error
tool results, allowing the model to react to them. An LLM client error stops the
turn and is returned to the caller.

The request limit is `DEFAULT_MAX_TOKENS`, currently `4096`. The hard tool-use
limit is `MAX_ITERATIONS`, currently `50`.

`TurnOutcome` reports:

| Field | Meaning |
| --- | --- |
| `text` | Text blocks from the last model response, joined with newlines. |
| `iterations` | Number of model responses that requested tools. |
| `hit_cap` | Whether the hard iteration limit ended the loop. |

`run_turn` updates the in-memory session and token totals. It does not save the
session; call `Session::save` when persistence is required.

## Sessions

`Session::new` creates a UUID-based identifier, records the current UTC time,
stores optional system and backend strings, and starts with empty messages and
zero usage.

The public session state is:

| Field | Type | Purpose |
| --- | --- | --- |
| `id` | `SessionId` | String identifier used as the JSONL filename. |
| `created_at` | `DateTime<Utc>` | Session creation time. |
| `system` | `Option<String>` | Optional system prompt used by `run_turn`. |
| `backend` | `Option<String>` | Optional backend metadata stored with the session. |
| `messages` | `Vec<Message>` | Ordered user, assistant, and tool-result history. |
| `total_usage` | `Usage` | Saturating input-token and output-token totals. |

`SessionId` is a type alias for `String`.

Session methods provide the following operations:

| Method | Behaviour |
| --- | --- |
| `append` | Adds one message to the history. |
| `add_usage` | Adds token usage with saturating arithmetic. |
| `jsonl_path` | Returns the session file path. |
| `save` | Rewrites the complete JSONL file through a temporary file and rename. |
| `load` | Reconstructs a session from its header and message records. |
| `list_ids` | Returns sorted identifiers for files ending in `.jsonl`. |
| `delete` | Removes one session file; a missing file is accepted. |

`sessions_dir` resolves the session directory beneath the Cosmix variable-data
directory as `agent/sessions`. The crate-level contract describes the resulting
location as `$COSMIX_VAR/agent/sessions/`.

Each session file starts with one JSON header record. The header contains the
identifier, creation time, optional system and backend values, and accumulated
usage. Each following non-empty line contains one message record.

Saving creates the session directory when needed. It writes the complete content
to a `.jsonl.tmp` path and renames that path over the session file.

## Tool registry

Implement `Tool` to add a callable operation:

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn schema(&self) -> ToolSchema;
    async fn run(&self, input: serde_json::Value) -> anyhow::Result<String>;
}
```

The schema supplies the tool name, description, and JSON input schema presented
to the model. `run` receives the model-provided JSON value and returns text for
the tool-result block.

`ToolRegistry` owns boxed tools keyed by schema name. Registering another tool
with the same name replaces the previous entry.

| Method | Behaviour |
| --- | --- |
| `new` | Creates an empty registry. |
| `register` | Inserts a boxed tool under its schema name. |
| `get` | Returns a borrowed tool by name. |
| `schemas` | Returns schemas for all registered tools. |
| `names` | Returns all registered names. |
| `len` | Returns the tool count. |
| `is_empty` | Reports whether the registry has no tools. |

Registry schema and name order is not defined because storage uses a hash map.

## Built-in tools

`BuiltinTools::register_all` installs three file tools:

| Tool name | Behaviour |
| --- | --- |
| `read_file` | Reads an absolute path or a path relative to the agent process working directory as UTF-8 text. |
| `write_file` | Writes UTF-8 text, overwrites an existing file, and creates missing parent directories. |
| `list_files` | Lists one directory level, sorts entries, and appends `/` to directory names. |

`list_files` uses `.` when its optional `path` input is omitted.

The built-in tools do not impose a path sandbox. Callers decide whether these
tools are suitable for their execution environment and which additional access
controls surround the agent.

## Cargo features

The crate declares no Cargo features.

## Errors and limits

Public operations use `anyhow::Result` where failure is possible. File I/O,
invalid JSON, invalid message records, and LLM client failures are returned to
the caller.

Tool failures inside `run_turn` are different: they are converted to error
results and sent back to the model. This keeps one failed or unknown tool call
inside the conversation loop.

Session usage counters saturate instead of overflowing. Session loading defaults
missing or invalid stored usage to zero and substitutes the current UTC time
when the stored creation time cannot be decoded.

This crate exposes a library API only. It has no command-line interface, daemon
configuration format, Bus verb surface, or standalone binary.
