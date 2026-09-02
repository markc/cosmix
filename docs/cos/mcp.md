# cosmix-mcp — Claude Code bridge to the mesh

**`cosmix-mcp` is an MCP server that exposes the cosmix mesh to Claude Code as a
set of tools: call any Bus service, tail logs, run Mix, and drive the
knowledge/skill learning loop backed by [indexd](indexd.md).** It is the seam
that lets an agent operate the substrate through the Model Context Protocol.

## What it is

A per-user helper (not a resident daemon) that speaks MCP over stdio. Claude
Code launches it; it connects lazily to the local [cosmix-noded](noded.md)
broker on the first tool call, so startup never blocks. Each tool call is
logged (name + redacted arg summary + duration + ok/err) to stderr, a per-user
log file, and journald (`journalctl -t cosmix-mcp`).

Its reason to exist is the **knowledge loop**: retrieve relevant context before
a task, capture what worked after. Skills, docs, and journals live in
[indexd](indexd.md); the MCP tools are how an agent reads and refines them.

## What it does

- **Bridges Bus** — call any mesh service, list services/peers, read node info, ping the broker, all as MCP tools.
- **Exposes logs** — tail and search per-daemon logs from the agent.
- **Runs Mix** — evaluate a Mix script/one-liner on the node.
- **Drives the knowledge loop** — semantic search, workspace (re)indexing, skill store/retrieve/refine, and retrieval-scoring feedback.
- **Redacts by default** — string values are never logged verbatim; full args need an explicit `RUST_LOG=cosmix_mcp=trace` opt-in.

## Running it

Registered with Claude Code, which spawns it on demand:

```sh
claude mcp add cosmix-mcp -- /opt/cosmix/bin/cosmix-mcp
```

The binary connects to the local broker for Bus tools and to
[cosmix-indexd](indexd.md) for the knowledge tools. No systemd unit — its
lifecycle is Claude Code's.

## Interfaces

MCP tools exposed:

| Group | Tools |
|---|---|
| Bus | `bus_call`, `bus_list_services`, `bus_node_info`, `bus_list_peers`, `noded_ping` |
| Logs | `log_tail`, `log_search` |
| Knowledge | `context_search`, `index_workspace`, `knowledge_digest`, `knowledge_brief` |
| Skills | `skills_retrieve`, `skills_store`, `skills_refine`, `skills_list`, `skills_delete`, `skills_graduate` |
| Feedback | `docs_feedback`, `journal_feedback`, `memory_feedback`, `journal_supersede` |
| Scripts | `mix_execute` |
| Status | `mcp_status` (uptime, broker state, per-tool call counts, recent calls) |

The knowledge protocol in practice: `context_search` before a non-trivial task
→ do the work → `skills_store` what you learned → `skills_refine` if you used a
retrieved skill → `*_feedback` to score the chunks you used.

## Where it fits

Depends on the Bus client from [bus](https://github.com/markc/cosmix)
(`cosmix-lib-client`), `cosmix-lib-skills` for the skill loop, and the embedded
[mix](https://github.com/markc/cosmix) engine for `mix_execute`. At runtime it
talks to [cosmix-noded](noded.md) (Bus surface) and [cosmix-indexd](indexd.md)
(semantic recall + skills). It is a client of the mesh, not a member of it.

## See also

- [indexd](indexd.md) — the vector store behind the knowledge + skill tools
- [noded](noded.md) — the Bus broker mcp calls through
- [overview](overview.md) — the daemon family at a glance
- [libraries](libraries.md) — `cosmix-lib-skills`, `cosmix-lib-agent`
