# cosmix-agentd — agent supervision daemon

`cosmix-agentd` runs multi-turn LLM agent loops as an Bus port. A caller opens a
session, sends messages, and the daemon drives the model — dispatching any tool
calls it requests — until the turn completes. Sessions persist to disk so they
survive a restart.

## What it is

A thin Bus daemon over the agent runtime library. The loop, session model, and
tool registry live in [`cosmix-lib-agent`](libraries.md); the LLM backend
abstraction lives in [`cosmix-lib-llm`](libraries.md). `agentd` binds a Unix
socket, decodes Bus requests, and calls into those libraries.

Because agent loops need tool-calling models, the default `claude-cli` backend
will not work. The backend is selected in order: `$COSMIX_AGENT_BACKEND` →
`llm_backend` in `skills.conf.mix` → a hardcoded `claude-api` fallback. Set it
to `ollama` for zero-cost local tool calling once a tool-capable model is pulled.

## What it does

- **session.new** — create a new agent session (optional system prompt and per-session backend override).
- **message** — send a user message and drive the loop to completion; returns the final text, iteration count, whether the iteration cap was hit, message count, and token usage.
- **session.list** — list persisted session IDs.
- **session.close** — evict a session from memory (the on-disk JSONL is kept).
- **tools.list** — return the registered tool schemas.
- **help / info** — standard cosmix daemon verbs; `info` reports port name, version, tool count, and active session count.

Sessions persist as JSONL under `$COSMIX_VAR/agent/sessions/{id}.jsonl`; a
`message` for a session not in memory transparently reloads it from disk.

## Running it

Installs to `/opt/cosmix/bin/cosmix-agentd`. It listens on a per-user socket:

```
/run/user/{uid}/cosmix/ports/agentd.sock
```

Start it directly (or under a systemd unit); it logs to journald under the
`cosmix-agentd` tag and shuts down cleanly on Ctrl-C, removing its socket:

```sh
/opt/cosmix/bin/cosmix-agentd
```

Drive it from Mix over the broker:

```mix
send agentd session.new
$sid = $result.session_id
send agentd message session_id=$sid text="List the files in /tmp"
print($result.text)
```

## Interfaces

- **Transport:** Bus over a Unix domain socket at `/run/user/{uid}/cosmix/ports/agentd.sock`.
- **Commands:** `session.new`, `message`, `session.list`, `session.close`, `tools.list`, `help`, `info`.
- **State:** JSONL session logs under `$COSMIX_VAR/agent/sessions/`.
- **Backend:** any backend `cosmix-lib-llm` supports (Anthropic, OpenAI-compatible, Ollama, or Bus-routed) — must be tool-capable.

## Related: cosmix-claud

`cosmix-claud` is a sibling Bus port focused on a single Claude-backed
request/response surface with knowledge-augmented generation. It listens on
`/run/user/{uid}/cosmix/ports/claud.sock` and exposes `ask` (prompt
auto-enriched with context retrieved from the cosmix knowledge base — skills,
docs, journals), `ask_raw` (no injection), `analyze`, `generate`, and — because
it links `cosmix-lib-mix` — `mix.execute` / `mix.expr` to run Mix scripts or
evaluate expressions. After a successful `ask` it evaluates the interaction for
reusable-skill extraction (async, non-blocking). Where `agentd` is a general
tool-driven agent loop, `claud` is the knowledge-loop LLM port the mesh routes
`llm-backend = bus` calls to.

## Where it fits

`agentd` makes an agent loop a first-class mesh service: any node can delegate a
tool-using task to it by sending one Bus message, rather than embedding an agent
runtime of its own. It is a building block for the substrate's
self-observation / self-modification loops, not an end-user chat product.

## See also

- [overview](overview.md) — the substrate at a glance
- [noded](noded.md) — the Bus broker
- [libraries](libraries.md) — `cosmix-lib-agent`, `cosmix-lib-llm`, `cosmix-lib-skills`
- [disp-skia](disp-skia.md) — the display surface
