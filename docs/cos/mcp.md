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
| Term | `term_list`, `term_snapshot`, `term_type`, `term_tab`, `term_pane` |
| Logs | `log_tail`, `log_search` |
| Knowledge | `context_search`, `index_workspace`, `knowledge_digest`, `knowledge_brief` |
| Skills | `skills_retrieve`, `skills_store`, `skills_refine`, `skills_list`, `skills_delete`, `skills_graduate` |
| Feedback | `docs_feedback`, `journal_feedback`, `memory_feedback`, `journal_supersede` |
| Scripts | `mix_execute` |
| Status | `mcp_status` (uptime, broker state, per-tool call counts, recent calls) |

The knowledge protocol in practice: `context_search` before a non-trivial task
→ do the work → `skills_store` what you learned → `skills_refine` if you used a
retrieved skill → `*_feedback` to score the chunks you used.

## Term diagnostic tools

Since cosmix-mcp 0.5.0, five dedicated tools drive CosMix Term over ABP.
The self-asserted terminal service is diagnostic pending authenticated
per-instance identity (P0-I). These tools reuse the Bus client and central
per-call metrics. Replies are bounded to 1 MiB.

**Which frontend they address is RESOLVED, not fixed (cosmix-mcp 0.5.2).**
TODO-term D1 split the global Bus name in two: the iced+wgpu frontend is
`term` / `term.*`, the Bevy one is `bterm` / `bterm.*`, and a frontend
*refuses* a verb from the other namespace. Before every tool call the MCP asks
the broker which of `bterm`, then `term`, is registered, and **probes each in
turn with a bounded `INFO`** — taking the first that actually answers and
building the verb from that same name.

**`bterm` is preferred when both answer (TODO-term D10, cosmix-mcp 0.5.2).**
It is the frontend with the tabs, the panes and the whole verb surface; the
iced `term` is a skeleton until it reaches parity at T6. There is no
environment override for this lane — `COSMIX_TERM_BIN` governs only which
*binary* `mix --gui` launches, not which *service* these tools drive, so while
both are running these tools address `bterm`.

Resolution is per call, never cached
for the process: the MCP outlives any one terminal, and both frontends may be
up at once for an A/B.

*Live means answering, not merely registered.* A frontend whose event loop is
stuck keeps its Bus name, so registration alone would hand every tool call to
it and each would block for the client's 60s transport timeout while a healthy
sibling sat unused. The probe costs about a millisecond against a responsive
frontend and is capped at three seconds against a wedged one. It is a probe
and never a retry-on-timeout: a timed-out *verb* may still have executed, so
falling back after one would replay a mutation (`term_type`, `term_tab`) into
a **different** terminal — keystrokes in the wrong window. `INFO` is read-only
and idempotent, so two bounded round trips are preferred to one ambiguous one.
This is the same definition of "live" that `term-desktop.mix` uses, so the two
control surfaces agree.

The failure modes are reported distinctly, because they call for different
actions — start a terminal, find the stuck one, or go and look at the broker:

- nothing registered → `ERROR: no CosMix terminal is registered on the Bus
  (looked for `bterm`, then `term`) — start one with `mix --gui``
- registered but silent → `ERROR: CosMix terminal registered but unresponsive:
  `term` did not answer INFO within 3s (wedged, or shutting down). No other
  frontend is registered. Nothing was sent.`
- the Bus itself went away mid-resolution → `ERROR: the broker stopped
  answering while probing for a CosMix terminal (…) — this is a Bus problem,
  not a wedged terminal. Check cosmix-noded. Nothing was sent.`

The third exists because the client call deliberately conflates an
application error with a transport one, so a silent probe is not by itself
evidence about the *terminal*. If the connection drops after the registration
listing, every probe fails for a reason that has nothing to do with a frontend
— so before blaming the terminals the resolver spends one bounded ping on the
broker and says which it was. That ping happens only on the failure path.

All three say **nothing was sent**, so an agent whose mutation errored does not
have to wonder whether it half-landed.

| Tool | Arguments | Behaviour |
|---|---|---|
| `term_list` | `{}` | Read-only JSON arrays `tabs` and `panes`: ids, active flags, dimensions, child pids, tab titles and pane geometry. Sequential reads are not atomic. |
| `term_snapshot` | `{}` | Read-only active screen text, cols/rows, cursor, pid, byte counters and diagnostic timings. |
| `term_type` | `{"pane":3,"text":"echo hello\n"}` or `{"tab":1,"text":"…"}` | DIAGNOSTIC keyboard-encoder input; newline is Enter. `pane` or `tab` is required (ids from `term_list`; a tab means its active pane); neither returns an error without an ABP call. Not the authenticated input API. |
| `term_tab` | `{"op":"new"}`, `{"op":"select","id":1}`, `{"op":"close","id":1}` | Create/select/close a tab; closing the last tab quits. |
| `term_pane` | `{"op":"split","dir":"h"}`, `{"op":"select","id":1}`, `{"op":"close"}` | Split/select/close active-tab panes; closing the last pane closes the tab. |

The MCP schemas use String for text/op, optional u64 for id/pane/tab, and
optional String for dir. Invalid operations, missing required ids/directions, and
invalid split directions return errors without an ABP call.

Term 0.3.0 changes every request to a JSON object. The verb column is written
in the canonical `term.` namespace; on the wire the prefix is the **resolved
frontend's name**, so against the Bevy frontend these are `bterm.snapshot`,
`bterm.tabs` and so on:

| Bus verb | JSON body |
|---|---|
| `term.snapshot`, `term.tabs`, `term.tab.new`, `term.panes`, `term.pane.close` | `{}` |
| `term.type` | `{"pane":3,"text":"..."}` or `{"tab":1,"text":"..."}` (one required) |
| `term.tab.select`, `term.tab.close`, `term.pane.select` | `{"id":1}` (non-negative u64 integer) |
| `term.pane.split` | `{"dir":"h"}` (h, horizontal, v, vertical) |

An empty body means `{}` only for no-argument verbs. The 8192-byte request
limit includes JSON syntax and escaping (8181 plain ASCII text bytes fit).
Non-JSON, non-object bodies, unknown fields, missing/wrongly typed values,
invalid directions and oversized requests fail before mutation (ABP rc=10).
Existing reply shapes, verb names, INFO/HELP diagnostic identity gating and
keyboard encoding remain unchanged. Raw-body callers must migrate.

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
