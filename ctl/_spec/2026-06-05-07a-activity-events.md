---
title: Cosmix Self-Aware Layer — Activity Events
chapter: 7a
version: 0.1.0
status: draft
date: 2026-06-05
substrate_layer: aware
amends: _spec/2026-04-27-07-self-aware.md (the activity-event sister taxonomy; was SPEC 07 §3.5)
companion:
  - _spec/2026-04-27-08-self-repair.md (repair.* events follow this shape)
  - _spec/2026-04-27-09-self-improve.md (proposal-lifecycle / learn-back events)
---

# Cosmix Self-Aware Layer — Activity Events

> **Split out of SPEC 07 §3.5 (2026-06-05).** SPEC 07's `props.changed` (its §3.1–3.4)
> covers *state transitions* — a property moving from `old` to `new`. **Activity
> events** are the orthogonal sister taxonomy for *discrete actions* — a tool
> invoked, a message dispatched, a scheduled task fired, a proposal applied. They
> are not property state. This chapter is **aspirational/unbuilt** (no daemon emits
> activity events yet — see the status notes below); it was extracted so SPEC 07's
> core observability contract reads as the implemented surface it is. Section
> numbers are preserved as **§3.5.x** so existing cross-references resolve.

### 3.5 Activity events (sister taxonomy)

`props.changed` covers *state transitions* — a property's value moved
from `old` to `new`. It does **not** cover *discrete actions* — a tool
invocation, a message dispatched, a scheduled task fired, a proposal
applied. Those are activity, not state. Forcing them into the property
surface produces fictitious "before/after" pairs and pollutes
`world.<svc>` snapshots with event history that does not belong there.

A daemon whose operation includes discrete actions worth observing MUST
emit them as **activity events** on the topic broker, distinct from
`props.changed`.

#### 3.5.1 Event shape

```
---
amp: 1
type: event
from: <svc>
command: <topic>
topic: <topic>
---
{
  "actor": "<svc>:<instance_id>",
  "verb": "<verb>",
  "ts": "2026-04-25T18:42:11.244Z",
  "cause": "01HXP7K3V8YYZ...",
  "outcome": "ok|error|refused",
  "duration_ms": 12,
  "details": { ... }
}
```

Required body fields: `actor`, `verb`, `ts`. Optional: `cause` (ABP id
of the originating request, per §7.4), `outcome` (small enum — keep
high-cardinality detail out of this field), `duration_ms`, `details`
(daemon-defined structured payload — see cardinality guidance below).

The `actor` field uses the same identity scheme as the constitution's
audit trailers (Article IV.4) and has **three variants**, distinguished
by the prefix kind. Implementers MUST emit one of these exact shapes
— ad-hoc forms are a conformance failure.

| Variant | Shape | Use when | `details.via` |
|---|---|---|---|
| **Daemon-process** | `<svc>` or `<svc>:<instance_uuid>` | A daemon performed the action on its own initiative. The bare `<svc>` form is acceptable when the daemon is single-instance per host (the common case); add `:<instance_uuid>` only when multiple instances of the same `<svc>` may emit concurrently and need disambiguation. | omitted |
| **Agent-session** | `<runtime>:<instance_uuid>[:<call_seq>]` | An agent runtime triggered the action via a tool call. Defined fully in §3.5.7. | required (the mediating tool host) |
| **Operator-principal** | `operator:<principal>` | A human operator triggered the action directly (CLI, manual ABP send). `<principal>` is a stable identifier — shell user, key id, or whatever the surrounding deployment uses for authentication. | required (the daemon that received the operator's command) |

`<instance_uuid>` is always a UUIDv7 minted at process start, NOT a PID
(PIDs reuse and are not unique across reboots — the audit chain breaks
on every restart). For the agent-session variant, see §3.5.7 for the
runtime-name registry and call-sequence semantics.

#### 3.5.2 Topic naming: per-daemon vs cross-runtime family

Two patterns are valid; daemons MUST pick one and document it via
`describe` on `lifecycle.activity_topics`:

- **Per-daemon**: `<svc>.<verb>` (e.g., `maild.message.delivered`,
  `indexd.embed.completed`). Use when the activity is unique to one
  daemon.
- **Cross-runtime family**: `<family>.<verb>` (e.g., `agent.tool.invoked`,
  `mesh.peer.handshake`). Use when multiple distinct services emit the
  same kind of activity and downstream subscribers reason about the
  category, not the producer. The `from:` header still identifies the
  emitter; the topic groups by category.

A daemon publishing to a cross-runtime family MUST agree on the schema
with all other publishers to that family. Schema drift across
co-publishers of the same topic is a conformance failure.

**Verb-family ownership.** Each `<family>` is owned by the spec that
defines its verb catalog (e.g., SPEC 08 §5.4.1 owns `repair.<verb>`;
this SPEC owns `agent.<verb>` per §3.5.7). Two daemons claiming the
same `<family>.<verb>` outside the family-owning spec is a
SPEC-amendment matter, not a runtime negotiation. Until a central
verb registry exists (deferred), conflict resolution is by spec
review during the dual-reviewer loop.

#### 3.5.3 Conformance posture

Activity events are **orthogonal to the L0-L3 property ladder**. A
pure-state daemon (one whose entire surface is properties) can be L3
without emitting any activity events. A near-stateless dispatcher can
emit rich activity while remaining L1. Activity emission is required
only when the daemon performs discrete actions whose observability the
substrate would otherwise have to reconstruct from logs.

Specifically required (MUST emit activity events):

- Agent runtimes (any daemon hosting an LLM-driven loop, mounting a
  tool provider, or applying a proposal). Topic family:
  `agent.tool.invoked`. Required `details`: `provider`, `params_hash`,
  `authority`, `dry_run`.
- Components participating in the SPEC 09 self-improve pipeline
  (proposal lifecycle: drafted, validated, applied, reverted). Topic
  family: `proposal.<verb>`. Schema specified in SPEC 09.
- Any daemon whose action would otherwise require log scraping to
  reconstruct (mailers, scheduled-task firers, mesh peer handshakes).

#### 3.5.4 Activity vs `props.changed`: choosing correctly

Quick test:

| Question | Answer | Use |
|---|---|---|
| Does a path's value transition from X to Y? | yes | `props.changed` |
| Does the system *do something* with no persistent state to reflect it? | yes | activity event |
| Both — a tool call mutated a counter? | both | `props.changed` for the counter, `agent.tool.invoked` for the action |

When in doubt, ask: *would a meta-subscriber querying `props.get` an
hour later be able to answer "did this happen?"* If yes (because the
state still reflects it), `props.changed` is enough. If no, the system
needs an activity event to make the action observable at all.

#### 3.5.5 Cardinality and privacy

§7.1 cardinality discipline applies to activity events: 10 Hz per topic
under steady load is the soft ceiling, with `transient: true` not
applicable (activity is inherently event-shaped — coalesce instead via
batching with a summary `details.batch_size` field).

§7.2 sensitive-value redaction applies to the `details` payload:
parameters that would be redacted under `props.describe sensitive: true`
MUST be hashed (e.g., `params_hash`) or omitted from `details`, never
inlined.

#### 3.5.6 Relationship to the constitution audit trailers

Article IV.4 mandates audit trailers on autonomous commits. Activity
events are the wire-protocol counterpart for autonomous *runtime
actions*: every tool invocation, proposal application, or autonomous
decision becomes an activity event whose `actor` and `cause` fields
let SPEC 08 reconstruct causation chains and SPEC 09 enforce the trust
gradient. The two layers compose — git audit trailers cover commits,
activity events cover everything in between.

#### 3.5.7 Agent session identity (elaboration of the agent-session actor variant)

§3.5.1 introduces three actor variants. The agent-session variant —
`<runtime>:<instance_uuid>[:<call_seq>]` — applies to any process
hosting an LLM-driven loop and exposing tools to it (`cosmix-mcp`,
`cosmix-agentd`, future autoresearch loops, future hosted-agent
daemons). This subsection pins the field semantics so all such
runtimes produce compatible actor keys.

- `<runtime>` is the agent-runtime daemon name (`mcp`, `agentd`,
  etc.) — short, stable, and distinct from the mediating daemon's
  filesystem name. New runtimes register their `<runtime>` token in
  this SPEC by amendment.
- `<instance_uuid>` is the UUIDv7 process-instance identifier per
  §3.5.1. Survives across multiple tool calls within the same
  agent session.
- `<call_seq>` is an optional monotonically-increasing per-instance
  counter, included when the runtime's session model is call-scoped
  (one logical request per tool call — MCP) and omitted when it is
  connection-scoped (a long-lived ABP session — agentd).

`details.via` requirements per §3.5.1's actor-variant table apply:
agent-session events MUST name the mediating tool host (the daemon
that translated the agent's intent into the substrate call).
operator-principal events MUST name the daemon that received the
operator's command. Daemon-process events omit `details.via`.

**Worked examples** (verbs use the SPEC 08 §5.4.1 and SPEC 07 §8.4
catalogs to show cross-runtime composition):

| Scenario | actor | details.via | verb |
|---|---|---|---|
| Claude calls `mds.export` via MCP, call #42 | `mcp:7f3a-...:42` | `cosmix-mcp` | `mds.export` |
| Claude script via agentd session | `agentd:9b1c-...` | `cosmix-agentd` | `mds.export` |
| Operator runs `cosmix-mds export` on the CLI | `operator:markc` | `cosmix-mds-cli` | `mds.export` |
| `gc()` runs autonomously inside maild | `cosmix-maild` | (omitted) | `mds.gc.completed` |
| Meta-supervisor restarts indexd per §5.1 | `cosmix-noded` | (omitted) | `repair.escalation` |

**Why this matters as a substrate invariant.** The unification plan
for `cosmix-lib-tools` (`_doc/2026-05-03-cosmix-agent-runtime-
unification-plan.md`) introduces this convention as an implementation
choice for two specific runtimes. Pinning it here makes it binding on
*any* future agent runtime — autoresearch loops, hosted-agent daemons,
alternate MCP implementations — so the audit chain (constitution V.5)
and the cross-runtime activity-event surface stay coherent without
each new runtime reinventing the format.

The agent-session variant is also the join key by which a `repair.*`
subscriber filters "show me only what the agent in session X
triggered" versus "show me what the daemon did on its own initiative"
— a distinction `harness.events`-style envelopes obscured.

---

