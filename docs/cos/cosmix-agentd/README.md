# cosmix-agentd

`cosmix-agentd` is the Cos daemon that exposes multi-turn LLM agent loops and
pluggable tools through a Bus Unix-socket port. It belongs to the `cos` layer
of the `bus <- mix <- cos` dependency chain: it uses Bus directly for request
and response framing and uses agent, LLM, configuration, daemon, and logging
libraries from the substrate. Its manifest has no direct Mix dependency.

## Synopsis

```text
cosmix-agentd
```

The binary takes no command-line options or subcommands. Clients operate it by
sending Bus commands to the `agentd` port. The crate declares no Cargo features.

## Description

The daemon:

- creates and resumes agent sessions;
- sends user messages through an LLM-backed agent loop;
- registers the built-in tool set from `cosmix-lib-agent`;
- returns the registered tool schemas;
- persists session histories as JSONL;
- keeps active sessions in memory; and
- handles each accepted Unix-socket connection in its own asynchronous task.

The process initialises daemon logging without statistics, binds its port, and
runs until interrupted. On `Ctrl-C`, it removes the socket path and exits.

## Transport

The daemon listens on:

```text
/run/user/{uid}/cosmix/ports/agentd.sock
```

Each accepted connection carries one Bus request. The request must have a
`command` header. A non-empty request body must be valid JSON; an empty body is
treated as JSON `null`.

The response has:

- an `rc` Bus header containing a decimal return code; and
- a JSON response body.

Return code `0` reports success. Return code `10` reports command validation,
lookup, LLM initialisation, agent-loop, or unknown-command errors. Bus framing
and JSON parse failures terminate request handling without a normal command
response.

## Commands

| Command | Request body | Result |
| --- | --- | --- |
| `session.new` | Optional `system` and `backend` strings | Creates and saves a session; returns `session_id` |
| `message` | Required `session_id` and `text` strings | Runs one agent turn and returns its outcome |
| `session.list` | None | Returns persisted session IDs |
| `session.close` | Required `session_id` string | Evicts an in-memory session |
| `tools.list` | None | Returns registered tool schemas |
| `help` | None | Returns the command names |
| `info` | None | Returns daemon identity and live counts |

### `session.new`

`session.new` creates a `Session`. `system` supplies an optional system prompt.
`backend` selects an optional LLM backend for that session. When `backend` is
absent, the daemon-level backend is stored instead.

Request body:

```json
{
  "system": "Answer tersely.",
  "backend": "ollama"
}
```

Success body:

```json
{
  "session_id": "<session-id>"
}
```

The daemon attempts to save the new session before adding it to the active
session map. A persistence failure is logged but does not change the success
return code.

### `message`

`message` removes the selected session from the active map for the duration of
the turn. If the session is not active, the daemon attempts to load it from
persistent storage.

Request body:

```json
{
  "session_id": "<session-id>",
  "text": "Summarise the current state."
}
```

The LLM client uses the backend stored in the session, falling back to the
daemon-level backend. The registered tool set is available to the agent loop.

Success body fields are:

| Field | Meaning |
| --- | --- |
| `session_id` | Session that handled the turn |
| `text` | Final text produced by the agent loop |
| `iterations` | Number of loop iterations used |
| `hit_cap` | Whether the loop reached its iteration cap |
| `message_count` | Number of messages now held by the session |
| `usage.input_tokens` | Session input-token total |
| `usage.output_tokens` | Session output-token total |

The session is saved after a successful turn and reinserted into the active
map. If the agent loop fails, the daemon also attempts to save and reinsert the
session before returning an error.

### `session.list`

`session.list` calls the session store and returns persisted session IDs in a
`sessions` array. The result does not describe only the sessions currently held
in memory.

### `session.close`

`session.close` removes a session from the active in-memory map. It does not
delete the session JSONL file.

The success body contains `session_id` and an `evicted` boolean. `evicted` is
`false` when the session was not active. This is still a successful operation.

### `tools.list`

`tools.list` returns the schemas published by the daemon's `ToolRegistry` in a
`tools` array. The registry is populated by `BuiltinTools::register_all` during
daemon initialisation.

### `help`

`help` returns the seven command names in a `commands` array.

### `info`

`info` returns:

| Field | Value |
| --- | --- |
| `port` | `agentd` |
| `app` | `cosmix-agentd` |
| `version` | Crate package version |
| `tool_count` | Number of registered tools |
| `active_sessions` | Number of sessions held in memory |

## Configuration

The daemon selects its default LLM backend in this order:

1. Non-empty `COSMIX_AGENT_BACKEND`.
2. Non-empty `llm_backend` from the `skills` service settings.
3. `claude-api`.

The source identifies the service settings file as `skills.toml`. A session's
explicit `backend` value takes precedence when processing `message`.

The default backend must support tool calling. The source notes `ollama` as a
local tool-calling option when a suitable model is available.

## Persistence

Sessions persist as JSONL beneath:

```text
$COSMIX_VAR/agent/sessions/{id}.jsonl
```

`session.new` writes a new session. `message` loads an inactive session on
demand and saves it after a turn or agent-loop failure. `session.close` only
evicts memory state; it preserves the JSONL history.
