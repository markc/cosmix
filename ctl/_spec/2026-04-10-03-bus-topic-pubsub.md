---
title: ABP Topic Pub/Sub
chapter: 3
version: 0.3.0
status: draft
date: 2026-04-10
amends: 2026-04-07-05-amp-display-protocol.md
---

# ABP Topic Pub/Sub

> **Retirement note (2026-08-16):** Chapters 01b and 05 are **retired** —
> `ui.*` left ABP at the control-plane pivot
> (`_decisions/2026-07-18-amp-as-control-plane.md`). The topic primitive
> itself (broker-side `topic.*`) is unaffected; the `ui.panel`/`ui.window`
> subscriber examples and the 05-amendment framing below are dated history.
> This chapter's citations of chapter 05 § 15.1 are covered by the carve-out
> in chapter 05's retirement banner (the WireGuard /24 trust-domain
> description remains accurate).
>
> **Vocabulary note (post-2026-04-26, historical):** This chapter predates
> the `ui.panel` → `ui.window` rename in 01b v0.2.0. Read `ui.panel`
> here as `ui.window`; the `subscribe`-header semantics described applied to
> both names while backends accepted `ui.panel` as a deprecated alias.

This document introduces a topic pub/sub primitive in the broker (cosmix-noded)
and the amendments to `2026-04-07-05-amp-display-protocol.md` required to support it. The
primitive enables reactive dashboards where a producer publishes state once and
zero-or-more viewers receive current and future updates. The motivating use case
is `sysmon.mix` auto-refreshing without a per-viewer process.

The design follows the MQTT retained-message and NATS JetStream KV watcher patterns, both of which have run the same "named channel with cached latest value replayed on subscribe" semantics in production for years. This is not a novel primitive; it is a deliberate reimplementation of a well-understood one in ABP framing.

## 1. Summary of changes

| Existing section | Change |
|---|---|
| 0.3 Design Principles | Principle 6 amended to clarify that caching opaque topic payloads is a routing optimization, not application state. |
| 0.4 State Ownership | Broker row gains explicit allowance for an opaque topic snapshot cache. |
| 3.1 `ui.panel` | New optional behavior header `subscribe` with atomic-swap lifecycle semantics on updates. |
| 10.3 Orphan Handling | Topic snapshot TTL aligned with panel orphan timeout. |
| 14 Conformance Levels | New note: `topic.*` commands are a broker extension and do not change renderer conformance levels. |
| `noded.ping` (broker-internal) | Response body gains a versioned `extensions` map for capability discovery. |
| **New § 3.11** | `topic.*` command family: `publish`, `subscribe`, `unsubscribe`, `subscriber_count`, `list`, `clear`, plus broker-emitted `topic.active` / `topic.idle`. |

All new behavior is additive. A renderer or process that implements only the pre-delta spec remains conformant; it simply cannot participate in topic pub/sub.

## 2. Amendments to existing sections

### 2.1 Amendment to § 0.3 — Design Principles

Append to Principle 6 ("Process owns state, display owns pixels"), after the sentence *"The broker routes messages between them but owns no application state."*:

> The broker MAY cache **opaque topic payloads** (§ 3.11) as a routing optimization. Such a cache holds the verbatim bytes of the most recent publish per topic and is used solely to replay the latest value to new subscribers. The broker never parses, interprets, or diffs these payloads. This is the routing-buffer analogue of MQTT retained messages: bytes in, bytes out, addressed by topic name. It is not application state because the broker has no view into what the bytes mean — it cannot answer any question about them beyond "did one exist, and if so, what were its bytes."

Rationale: without this clarification, a strict reading of Principle 6 forbids any form of replay cache in the broker, forcing pub/sub to be implemented as a separate daemon or via a producer-pull model that breaks the "fresh subscriber sees current state immediately" property. The opacity constraint preserves the principle's intent (the broker can't reason about app data) while admitting a 25-year-old industry pattern.

### 2.2 Amendment to § 0.4 — State Ownership

In the responsibility table, modify the **Broker** column of the **Panel registry** row from:

> Tracks which panel IDs exist (for routing, wildcards, orphan detection)

to:

> Tracks which panel IDs exist (for routing, wildcards, orphan detection). MAY additionally maintain an **opaque topic snapshot cache** (§ 3.11) keyed by topic name. Cached payloads are treated as bytes, not application state.

No other rows change.

### 2.3 Amendment to § 3.1 — `ui.panel`

Insert a new row in the **Optional headers (behavior)** table, after the `collect_values` row:

| Header | Type | Default | Description |
|--------|------|---------|-------------|
| `subscribe` | string | (none) | Topic name to bind this panel to (§ 3.11). The display service MUST auto-issue `topic.subscribe` on panel creation and `topic.unsubscribe` on panel removal. |

Add a new sub-section **3.1.2 `subscribe` header lifecycle** after the grid layout sub-section:

> When a `ui.panel` message includes the `subscribe` header, the display service establishes a topic binding for the panel. The binding lives for the lifetime of the panel and is managed entirely by the display service — the process owning the panel does not need to issue explicit `topic.subscribe` / `topic.unsubscribe` calls.
>
> On **panel creation** (first `ui.panel` with a given `id`):
> - If `subscribe` is present and non-empty, the display service issues `topic.subscribe name=<value>` to the broker.
> - If `subscribe` is present and empty, it is treated as absent — no subscription.
> - If `subscribe` is absent, no subscription is created.
>
> On **panel update** (subsequent `ui.panel` with an existing `id`):
> - If `subscribe` is absent, the existing binding (if any) is preserved. This matches the general `ui.panel` update rule that absent headers preserve stored values.
> - If `subscribe` is present and equals the current binding, no action.
> - If `subscribe` is present and differs from the current binding (including transition from unbound to bound), the display service MUST atomically: issue `topic.unsubscribe` for the old binding (if any), issue `topic.subscribe` for the new binding, and update the stored binding. The swap MUST be atomic from the perspective of subsequent topic deliveries — no delivery on the old binding may arrive after the new subscription is established.
> - If `subscribe` is present and empty, the existing binding (if any) is removed: issue `topic.unsubscribe`, clear the stored binding. This is the `ui.panel` "empty string clears header" rule applied to subscriptions.
>
> On **panel removal** (`ui.remove`, orphan timeout, or TTL expiry), the display service MUST issue `topic.unsubscribe` for any active binding before removing the panel.
>
> The `subscribe` header is purely sugar over the `topic.subscribe` / `topic.unsubscribe` commands in § 3.11. A process MAY issue those commands directly instead of (or in addition to) using the header.

### 2.4 Amendment to § 10.3 — Orphan Handling

Append a new sub-section **10.3.1 Topic snapshot lifetime** after the orphan handling bullets:

> Topic snapshots (§ 3.11) are tied to producer presence with a grace period matching the panel orphan timeout:
>
> - When a peer that has published to topic `T` disconnects, the broker marks the snapshot for `T` as **stale**. Subsequent subscribers receive the stale snapshot with a `topic_stale: true` header on the first delivery (see § 3.11).
> - If the same peer reconnects (identified by registered service name) and publishes to `T` before the orphan timeout elapses, the stale flag is cleared. Brief producer restarts are invisible to subscribers.
> - If the orphan timeout elapses without a new publish, the broker MAY purge the snapshot. New subscribers after purge receive no replay; they see only future publishes (if any).
> - Anonymous publishers (no registered service name) cannot be matched across reconnects. Their snapshots are considered stale the instant the publishing connection closes and are purged at the next orphan-timeout sweep.
>
> The default orphan timeout from § 10.3 (60 seconds) applies unchanged. Implementations MAY expose a separate topic-snapshot-TTL configuration knob but SHOULD default it to the orphan timeout for consistency.

### 2.5 Amendment to § 14 — Conformance Levels

Append a new sub-section **14.4 Broker extensions** after § 14.3:

> The `topic.*` command family (§ 3.11) is a **broker extension** and does not affect renderer conformance levels. A renderer is unaware of the broker: it sees only messages that arrive through normal routing, regardless of whether those messages originated from a direct send or from a broker fan-out. The single renderer-facing feature is the `subscribe` header on `ui.panel` (§ 3.1.2), which a renderer MAY implement at any conformance level. A renderer that does not implement the `subscribe` header MUST ignore it (per the general "ignore unknown headers" rule) — panels continue to render, they simply do not auto-update from a topic.
>
> A broker implementation either supports `topic.*` or it does not. A broker that does not support `topic.*` MUST respond to `topic.*` commands with RC 10 and the error body `{"error": "topic_not_supported"}`. Processes detect broker availability by reading the `extensions` field of the `noded.ping` response (§ 2.6) — capability discovery belongs in the handshake, not in the operation. A process that cannot reach the broker at all receives no response; a process that reaches a broker without broker support sees `extensions.topic` absent from the ping response and SHOULD NOT issue `topic.*` commands.

No changes to § 14.1 levels table or § 14.2 renderer classification.

### 2.6 Amendment to `noded.ping` — Capability advertisement

`noded.ping` is a pre-existing broker-internal command used for liveness checks and handshake timing. It is not defined in the display protocol spec proper because it operates below the ui.* layer, but its response shape is observable to any process that issues it. This amendment adds a single field to the `noded.ping` response body without changing its headers or semantics.

**Change:** The `noded.ping` response body, previously empty or containing implementation-defined liveness metadata, MUST now include an `extensions` map describing the broker's supported command families and their versions.

**Response body addition:**

```json
{
  "extensions": {
    "core": "1.0",
    "topic": "1.0"
  }
}
```

**Semantics:**

- `core` is always present and carries the broker's implemented version of the ABP display protocol spec (§ 0.1). v1 brokers report `"1.0"`.
- Each additional key names a broker extension and carries its version string. `topic` is the first such extension; future extensions (`stream`, `presence`, etc.) will appear alongside it.
- Versions are independent per extension. A broker MAY support `core: 1.0` and `topic: 1.1` simultaneously. Clients SHOULD compare versions via semver semantics.
- An extension's absence means the broker does not implement it. Clients MUST NOT assume an extension exists; they MUST check before issuing commands from that extension.
- Additional keys beyond `extensions` MAY appear in the ping response for implementation-defined liveness data (latency hints, peer counts, etc.). Clients MUST ignore unknown keys per the general "ignore unknown headers/fields" rule.

**Backwards compatibility:** The `extensions` field is strictly additive. Pre-delta brokers that return an empty or implementation-defined body continue to work with clients that don't look at `extensions`. Clients that do look for `extensions` and find it absent MUST treat this as "core only, no extensions" — the safe-degradation default.

**Rationale:** Capability discovery via error-on-use (call `topic.list`, check for RC 10) is the kind of pattern that accretes startup-latency debugging cost as the extension set grows. Discovery belongs in the handshake. Versioning per-extension rather than a single monolithic spec version lets `topic.*` evolve independently of the core ui.* protocol — important because the broker is likely to grow features (federation, claim-producer, wildcards) on a faster cadence than the stable ui.* surface.

## 3. New § 3.11 — `topic.*` — Broker Topic Extension

The `topic.*` command family is a broker extension that provides named data channels with cached-latest semantics. It is separate from the `ui.subscribe` / `ui.unsubscribe` commands in §§ 3.9–3.10, which route filtered `ui.event` messages for behavior attachment. Topics carry **arbitrary payloads**, most commonly `ui.batch` messages for reactive panel updates; event filters carry **structured events** for script handlers.

### 3.11.1 Concepts

A **topic** is a flat string name identifying a data channel. Names are free-form UTF-8 with the exception that names beginning with `$` are reserved for broker-internal use (e.g. `$stats.publish_rate`, `$peers`); publishes to reserved names from non-broker peers MUST be rejected with RC 10.

A **snapshot** is the most recent published payload for a topic, cached by the broker. Caching is latest-wins: each publish replaces the previous snapshot. Snapshots are opaque to the broker — the broker stores bytes and forwards bytes.

A **subscription** is a (topic name, subscriber peer) pair registered in the broker. When a topic is published to, the broker fans out the payload to every current subscriber. When a peer subscribes, the broker immediately replays the cached snapshot (if one exists).

A **sequence number** is a monotonically increasing per-topic `u64` counter, incremented on each publish and attached to every delivery as the `topic_seq` header. Subscribers use it for gap detection and debugging multi-producer races.

**Reserved header names.** The broker injects the following headers into each delivery. Producers SHOULD NOT include these headers in publish payloads; if present, the broker silently overwrites them (RC 0, no error). Future versions MAY tighten this to outright rejection.

| Header | Purpose |
|---|---|
| `topic` | Topic name this delivery came from. |
| `topic_seq` | Monotonic per-topic sequence number. |
| `topic_stale` | Present with value `true` if the cached snapshot's publisher has been marked stale per § 10.3.1. |
| `topic_op` | Present with value `delete` on the final delivery after `topic.clear` with `notify: true`. |

Per § 1.2, header names are case-sensitive and lowercase with underscores. The reservation covers only the exact lowercase forms listed above; mixed-case variants (`Topic`, `TOPIC_SEQ`) are not valid ABP headers at all and are rejected by the wire format itself. The broker MUST overwrite any reserved header present in an incoming publish payload. Future versions MAY tighten this to outright rejection (RC 10) if pre-seeding turns out to be a vector for anything.

**Anonymous publisher identity.** Processes that connect to the broker without registering a service name (the default for Mix scripts using `connect_anonymous`) are assigned a synthesized, connection-scoped identity at WebSocket establishment time:

```
anon-<hex_nonce>-<unix_seconds>
```

Example: `anon-a4f8c2e1-1744243200`. The hex nonce is a random `u32` preventing collision among concurrent anonymous connections; the Unix timestamp makes log-grepping a dead producer trivial. This identity is stored in the peer record alongside the outbound channel and used as the `last_publisher` value for any `TopicEntry` the peer publishes to. It is visible in `topic.list` responses and in `noded.tap` stream frames.

Synthesized identities are **broker-local**: they are valid only on the broker that minted them, MUST NOT be federated across mesh bridges, and MUST NOT be persisted across broker restarts. A client that disconnects and reconnects receives a fresh identity; the broker treats the new connection as an unrelated publisher. Snapshot cleanup on disconnect proceeds through the existing reverse index — the synthesized identity is the key.

With this identity in place, `topic.active` / `topic.idle` notifications (§ 3.11.8) work for the full lifetime of a long-running anonymous producer. Notifications addressed to a dead connection's identity are dropped at the routing layer, not misdelivered to a later anonymous connection — the nonce makes cross-connection collisions impossible by construction.

### 3.11.2 `topic.publish` — Publish a snapshot

Replaces the topic's cached snapshot and fans the payload out to all current subscribers.

**Direction:** Process → broker
**Type:** request (expects RC response)

**Required headers:**

| Header | Type | Description |
|---|---|---|
| `command` | `"topic.publish"` | Command identifier |
| `name` | string | Topic name (must not begin with `$` unless sent by a broker-internal identity) |

**Optional headers:**

| Header | Type | Default | Description |
|---|---|---|---|
| `retain` | bool | `true` | If `false`, the payload is fanned out to current subscribers but not cached. Use for fire-and-forget notifications that shouldn't replay to later subscribers. |

**Body:** A **complete, serialized ABP message** (including its own `---` header
frame but **without** a trailing `---\nEOM\n` terminator). The broker treats the
body as opaque bytes. The most common payload is a `ui.batch` message (§ 3.8)
carrying `ui.update` / `ui.data` sub-messages, but the body MAY be any ABP
message.

**Nesting constraint:** The inner ABP message's body MUST NOT contain the
sequence `---\nEOM\n`, which would be interpreted as the outer message's
terminator by the stream parser (`2026-03-24-01-bus-wire-protocol.md` §5.2). In practice
this means `topic.publish` payloads cannot themselves nest further ABP messages
— nesting depth is limited to one level. This is sufficient for all v1 use
cases (`ui.batch` bodies are JSON, not nested ABP).

Maximum payload size: **1 MiB** (provisional). This bounds worst-case per-peer
buffer memory: 256 (channel capacity) × 1 MiB = 256 MiB per slow subscriber.
In practice, `ui.batch` payloads for reactive dashboards are well under 100 KiB.
The limit may be tightened in future versions based on production data. Publishes
exceeding this limit are rejected with RC 10 and an error body of
`{"error": "payload_too_large", "limit": 1048576}`.

**Response:** RC 0 with `{"seq": N, "delivered": M}` where `N` is the new sequence number for this topic and `M` is the number of subscribers the payload was fanned out to. On error, RC 10 with `{"error": "..."}`.

**Fan-out semantics:** The broker parses the outer `topic.publish` wrapper, extracts the body (which is itself an ABP message), injects two routing headers into the inner message — `topic: <name>` and `topic_seq: <seq>` — and additionally `topic_stale: true` if the producer was marked stale per § 10.3.1. The annotated inner message is then sent to each subscriber through normal per-peer routing. The broker does NOT introduce a new command type; subscribers see the inner message's original `command` header (typically `ui.batch`) and dispatch through their existing handlers. The annotated inner message becomes the new cached snapshot (if `retain` is true).

**`retain: false` edge cases.** When `retain` is `false` and no subscribers exist, the publish is a no-op: the payload is neither cached nor delivered anywhere. The response still returns RC 0 with `{"seq": N, "delivered": 0}` so producers can detect the condition if they care. The sequence number is incremented on every publish regardless of `retain` — it counts publishes, not snapshots — so subscribers that join mid-stream and see a mix of retained and non-retained publishes can still use `topic_seq` for gap detection without special-casing retention state.

**Join-after-non-retained publish:** A subscriber that joins after the most
recent publish was `retain: false` receives the most recent *retained* snapshot
(if one exists), with its original `topic_seq`. If no retained snapshot exists,
no replay occurs and the subscribe response returns `seq: 0`. The gap between
the replayed snapshot's seq and the next delivery's seq reflects the
non-retained publishes that occurred in between — this is expected and not an
error condition.

**Header injection is a security property, not an implementation convenience.** The broker MUST inject the routing headers (`topic`, `topic_seq`, `topic_stale`, `topic_op`) **after** accepting the publish and allocating the sequence number — never by passing through producer-supplied values. A producer MUST NOT pre-seed these headers to influence ordering, staleness, or delivery framing; any such headers present in the incoming inner message are unconditionally overwritten by the broker's values. This matters today for debugging (a producer cannot fake sequence gaps to confuse gap-detection tooling) and matters tomorrow for cross-mesh federation (a peer on a federated topic cannot spoof sequence numbers to cause subscribers on other meshes to skip messages). Implementations MUST treat producer-supplied routing headers as adversarial input even though the v1 trust domain (WireGuard /24, `2026-04-07-05-amp-display-protocol.md` § 15.1) does not assume an active attacker — the property costs nothing to enforce and is load-bearing for future versions.

**Example:**

```
---
command: topic.publish
name: sysmon.metrics
---
---
command: ui.batch
---
[
  {"command": "ui.update", "headers": {"target": "sysmon"}, "body": "# alpha\n\n**Load:** 0.88 0.93 0.90"},
  {"command": "ui.data", "headers": {"target": "proc-table"}, "body": "[{\"id\":\"1\",\"pid\":\"1234\",\"name\":\"cosmix-indexd\",\"cpu\":\"12.3\",\"mem\":\"4.1\",\"state\":\"S\"}]"},
  {"command": "ui.data", "headers": {"target": "disk-table"}, "body": "[]"}
---
EOM
```

Note: the inner message (`ui.batch`) does NOT have its own `---\nEOM\n`
terminator — the outer message's `---\nEOM\n` terminates the entire publish.
The inner `---` delimiters (header frame) are body content of the outer message.

A subscriber of `sysmon.metrics` will receive the inner message with routing
headers injected:

```
---
command: ui.batch
topic: sysmon.metrics
topic_seq: 42
---
[ ... same JSON body ... ]
---
EOM
```

The delivered message is a standalone ABP message (with its own `---\nEOM\n`
terminator) and dispatches through the existing `ui.batch` handler unchanged.

### 3.11.3 `topic.subscribe` — Subscribe to a topic

Registers the calling peer as a subscriber to the named topic. If a snapshot exists, the broker replays it to the caller immediately as a normal fan-out delivery (same format as § 3.11.2).

**Direction:** Process → broker
**Type:** request

**Required headers:**

| Header | Type | Description |
|---|---|---|
| `command` | `"topic.subscribe"` | Command identifier |
| `name` | string | Topic name |

**Body:** empty.

**Response:** RC 0 with `{"subscription_id": "...", "replayed": bool, "seq": N}`. `replayed` is `true` if the caller received a snapshot replay; `seq` is the sequence number of the replayed snapshot, or `0` if none existed. On error (reserved-name violation, etc.), RC 10.

The `subscription_id` is a diagnostic token for logging and tooling (`noded.tap`
stream frames include it). It has no operational use in v1 — subscribers do not
need to reference it in subsequent commands. Future versions may use it for
subscription-scoped features (e.g., delivery limits, per-subscription filters).

**Idempotency:** Subscriptions are keyed by `(peer, topic)`. A peer that subscribes to a topic it is already subscribed to receives the existing `subscription_id` and no duplicate delivery is fanned out on subsequent publishes. The operation is idempotent. This matches the implicit assumption in § 3.1.2 that a `ui.panel` update with an unchanged `subscribe` header is a no-op, and collapses the ambiguity in `subscriber_count` — the returned value unambiguously counts distinct peers, not subscription records.

### 3.11.4 `topic.unsubscribe` — Unsubscribe from a topic

Removes the caller's subscription to the named topic.

**Direction:** Process → broker
**Type:** request

**Required headers:**

| Header | Type | Description |
|---|---|---|
| `command` | `"topic.unsubscribe"` | Command identifier |
| `name` | string | Topic name |

**Body:** empty.

**Response:** RC 0. A peer that unsubscribes from a topic it was not subscribed to still receives RC 0 (idempotent).

### 3.11.5 `topic.subscriber_count` — Query subscriber count

Returns the number of current subscribers to a topic. Used by producers to back off when nobody is watching.

**Direction:** Process → broker
**Type:** request

**Required headers:** `command: topic.subscriber_count`, `name: <topic>`.

**Response:** RC 0 with `{"count": N}`. Absent topics return `{"count": 0}`.

### 3.11.6 `topic.list` — List topics

Returns metadata for all topics currently known to the broker. Intended for debugging and tooling.

**Direction:** Process → broker
**Type:** request

**Required headers:** `command: topic.list`.

**Optional headers:** `prefix: <string>` to filter by topic name prefix (simple string match, not a glob).

**Response:** RC 0 with a JSON array of topic records:

```json
[
  {
    "name": "sysmon.metrics",
    "subscribers": 2,
    "has_snapshot": true,
    "snapshot_seq": 42,
    "snapshot_size": 1284,
    "last_publisher": "sysmon",
    "stale": false
  }
]
```

### 3.11.7 `topic.clear` — Explicit snapshot removal

Removes the cached snapshot for a topic. Used by producers at shutdown to prevent stale replay to future subscribers.

**Direction:** Process → broker
**Type:** request

**Required headers:** `command: topic.clear`, `name: <topic>`.

**Optional headers:** `notify` (bool, default `true`) — if `true`, the broker fans out a final delivery to current subscribers with a `topic_op: delete` header and an empty body. If `false`, subscribers are not notified.

**Response:** RC 0. Clearing an absent topic is idempotent.

**Consumer-side behavior:** A subscriber receiving a delivery with `topic_op: delete` SHOULD treat it as "the producer has invalidated its state" but MUST NOT automatically remove any panels bound to the topic. The panel's current rendered state is preserved — the producer owns that decision, not the broker. Consumers that want to signal invalidation visually (e.g. dim the panel) may do so.

### 3.11.8 `topic.active` / `topic.idle` — Subscriber-count transitions (broker → producer)

These are **broker-emitted notifications**, not commands a process calls. The broker sends `topic.active` to the last-known publisher of a topic when the subscriber count transitions from `0` to `>0`, and `topic.idle` when it transitions from `>0` to `0`. The last-known publisher is determined by the `from` header of the most recent `topic.publish`; anonymous publishers do not receive these notifications.

**Direction:** Broker → process
**Type:** notification (no response expected)

**Headers:** `command: topic.active` (or `topic.idle`), `name: <topic>`, `subscribers: <N>`.

**Body:** empty.

**Producer behavior:** Producers MAY ignore these messages. A producer that honors them can implement back-off: stop publishing on `topic.idle`, resume on `topic.active`. With the synthesized anonymous identity from § 3.11.1, a long-running anonymous Mix producer does receive these notifications for the full lifetime of its connection — the "anonymous producers get no notifications" limitation from the first draft of this delta no longer applies.

**Racing producers are best-effort.** Under the documented racing-producers caveat (§ 5), `topic.active` / `topic.idle` fire only to the single most recent publisher as tracked by `TopicEntry.last_publisher`. Other live publishers racing on the same topic will not be informed of subscriber-count changes and will continue publishing at full rate regardless of viewer presence. This is a deliberate v1 scope decision: tracking a set of recent publishers per topic is plumbing the racing-producer caveat itself is already asking users to avoid. Producers requiring reliable back-off under contention should wait for `topic.claim_producer` in a future version, at which point at most one publisher per topic is live by construction and this ambiguity disappears.

### 3.11.9 Error codes

| Condition | RC (v1) | Error body |
|---|---|---|
| Reserved-name violation (`$`-prefix from non-broker identity) | 10 | `{"error": "reserved_name"}` |
| Payload exceeds 1 MiB | 10 | `{"error": "payload_too_large", "limit": 1048576}` |
| Malformed body (not a valid ABP message) | 10 | `{"error": "malformed_payload"}` |
| Producer-supplied reserved routing header (`topic`, `topic_seq`, `topic_stale`, `topic_op`) in publish body | 0 (silent overwrite) | — |
| Unknown topic on `topic.clear` | 0 | (idempotent success) |
| Topic extension not supported on this broker | 10 | `{"error": "topic_not_supported"}` |

**Reserved future error string.** When a future version tightens reserved-routing-header handling from silent overwrite to rejection (per § 3.11.2), the error body will be `{"error": "reserved_header"}`. Implementations MUST NOT reuse this error string for any other condition in v1, so that client-side tests asserting against the error surface remain forward-compatible.

## 4. Design rationale

### 4.1 Payload opacity and the three-reader model

The broker treats topic payloads as opaque bytes. This is the property that lets the "broker owns no application state" principle survive the amendment in § 2.1. But opacity has a second, subtler payoff specific to the ABP three-layer model (Principle 1 of § 0.3: plain text, machine-parseable, AI-comprehensible).

Because the payload is an ABP message and the broker doesn't touch it, the same bytes serve three different readers:

1. **Human tailing the topic** (`mix sub sysmon.metrics`) sees the inner ABP message in its plain-text form and can `grep` / read it directly.
2. **Display service** parses the inner message's `command` header and dispatches through its existing widget rendering path.
3. **AI process** (e.g. a Claude instance subscribed to the topic via `cosmix-mcp`) reads the markdown body as native LLM context and can reason about system state.

No format translation, no separate "human view" vs "machine view," no schema bridges. MQTT retained messages are opaque bytes; NATS KV values are opaque bytes; Phoenix PubSub payloads are opaque Erlang terms. None of them have this property because none of them made the protocol itself three-reader by construction. This is the ARexx message-port-as-universal-substrate design paying off, and it is worth preserving even if a future optimization (e.g. binary payloads for throughput) might seem attractive.

### 4.2 Why not unify `topic.subscribe` with `ui.subscribe`?

`ui.subscribe` (§ 3.9) filters the `ui.event` stream by `(source_panel, action)` — a structured predicate over a specific message family. `topic.subscribe` addresses via flat topic name — a lookup in a named channel space with cached-latest semantics. Collapsing them into one wire primitive either makes the predicate form accommodate flat names as a degenerate case (awkward), or makes the flat-name form accommodate predicates (awkward in the other direction). Phoenix reached the same split: `PubSub` (topic broadcast) and `Presence` (stateful distribution) are separate APIs because the data models differ.

The two families share an implementation registry in the broker (one subscription table, one reverse index, one disconnect-cleanup path). On the wire they are distinct because their addressing semantics are genuinely different. Spec users see two primitives with two purposes; implementers see one registry with two projections.

### 4.3 Why cache in the broker instead of pulling from the producer?

The alternative is: the broker stores only "who produces topic X," and on subscribe, the broker asks the producer to re-send the latest. This keeps the broker stateless but:

- Adds a round-trip on every subscribe.
- Fails when the producer is briefly offline during a viewer's reconnect.
- Cannot serve viewers that opened before any publisher was running.
- Makes the `stale=true` edge case impossible to detect (stale vs. never-published are indistinguishable).

MQTT and NATS both cache retained/latest values in the broker for the same reasons. The amendment in § 2.1 acknowledges this is a routing-layer buffer, not application state.

### 4.4 Why bounded per-peer channels?

Unbounded `mpsc::Sender` buffers let a slow subscriber consume memory without bound. Under normal routing this is unlikely (point-to-point messages are bursty but bounded by sender rates); under topic fan-out it is much more likely (a producer at 10 Hz multiplied across N slow subscribers). Bounded channels with drop-oldest convert an unbounded failure mode into a lost-message one. Since the topic snapshot is the source of truth, a subscriber that missed intermediate deliveries can catch up by re-reading the cache on reconnect; no data is permanently lost. MQTT QoS 0 drops on backpressure. NATS detects slow consumers and forcibly disconnects them. This plan does the latter: drop-oldest until a per-peer drop count threshold, then force-disconnect with an RC 20 "slow consumer" tap event.

## 5. Non-goals for v1

Each of these is additive to the wire format and can be introduced in a later version without breaking v1 consumers:

- **Cross-mesh topic federation.** A topic published on node A is not automatically visible on node B. Topic federation will reuse the existing mesh bridge in noded when added.
- **Per-widget topic binding.** The `subscribe` header on `ui.panel` binds an entire panel to one topic. Future versions may add a widget-level `subscribe` attribute for widgets that want to consume different topics within one panel.
- **Manifest-driven producer auto-spawn.** In v1, the user runs producer scripts manually. A later version may add a topic manifest in `settings.toml` mapping topic names to launcher commands so the broker can spawn a producer on first subscribe.
- **Topic ACLs.** The WireGuard /24 trust domain (`2026-04-07-05-amp-display-protocol.md` § 15.1) covers v1. ACLs on topics (who may publish, who may subscribe) will be added when the first multi-tenant use case arrives.
- **Versioned / append-only streams.** Snapshots are latest-wins. Topics with history (event sourcing, audit logs) will be a separate primitive (`stream.*`, TBD) rather than a generalization of topics.
- **Claim-single-producer lock.** Two producers publishing to the same topic race; last-write-wins on the snapshot. The sequence number makes the race debuggable but does not prevent it. A `topic.claim_producer` lock may be added later.
- **Wildcards in subscriptions.** Subscribers name one topic. No `sysmon.*` wildcards in v1.
- **Snapshot persistence across broker restart.** Snapshots live in memory only. A broker restart clears them. Consumers re-subscribe and wait for the next publish.

## 6. Implementation phasing

This section is informative — it is not part of the spec but is included so implementers can scope the work.

**Phase A0 — Bounded per-peer channels (hygiene)**
- Convert the broker's per-peer outbound channels from `UnboundedSender<String>` to a bounded channel (capacity 256) with drop-oldest + slow-consumer disconnect.
- Surface per-peer drop counters via `noded.tap`.
- Regression-test existing routing under the new bounds.
- No protocol changes.

**Phase A — Broker core**
- New `subscription.rs` module in cosmix-noded with a `SubscriptionBroker` struct holding a single registry usable by both `topic.*` and the future `ui.subscribe` implementation.
- `topic.*` command handlers per § 3.11.
- Snapshot cache with 1 MiB cap, sequence numbers, `$`-prefix reservation, `topic.clear` support, and the stale-producer grace period from § 10.3.1.
- Broker-emitted `topic.active` / `topic.idle` on subscriber-count transitions.
- Stub `ui.subscribe` / `ui.unsubscribe` that write into the shared registry but return RC 5 (warning) with a body noting that event routing is not yet wired. This validates the shared-registry claim and reserves the command names.
- Unit tests covering: publish/subscribe/replay, unsubscribe, disconnect cleanup, sequence monotonicity, snapshot cap, reserved-name rejection, clear + notify, stale flag transitions, multi-subscriber fan-out.
- Manual verification via `wsamp`: no consumer code required.

**Phase B — `lib-display` protocol types**
- `WindowProps.subscribe: Option<String>` field and parser
  (canonical from 01b v0.2.0; the previous `PanelProps` name is
  retained as a deprecated alias through the 0.2.x line).
- No new command types required on the consumer side (the whole point of § 3.11.2's fan-out semantics).

**Phase C — deskd subscription lifecycle**
- `topic_bindings: HashMap<PanelId, String>` in `App`.
- `handle_panel` auto-subscribes on `subscribe` header per § 3.1.2.
- `WindowEvent::CloseRequested`, `ui.remove`, TTL expiry, and orphan timeout all auto-unsubscribe.
- Incoming `ui.batch` (or any command) with a `topic` header is dispatched normally; the `topic` header may optionally be used to verify the binding is still active and drop late deliveries for unbound panels.

**Phase D — sysmon.mix rewrite**
- Extract gather logic into a function.
- Open panel once with `subscribe: sysmon.metrics`.
- Loop: build a `ui.batch` body, publish via `topic.publish name=sysmon.metrics`, sleep 2s.
- Running the same script twice produces two racing producers; documented as a known v1 caveat.

## 7. Resolutions to initial open questions

The first draft of this delta flagged three open questions. All three are resolved in the body above; this section records the decisions and their rationales so future readers can reconstruct why the wire looks the way it does.

**Header naming: kept unprefixed, reserved by name.** The reserved headers `topic`, `topic_seq`, `topic_stale`, `topic_op` live in the general ABP header namespace rather than behind a prefix like `x_topic_*`. Resolution in § 3.11.1. Rationale: RFC 6648 retired the `X-` convention in HTTP precisely because it created a permanent split between "real" and "experimental" headers that outlived the experiment — once `X-Forwarded-For` became universal, the `X-` told you nothing useful and couldn't be removed without breaking the internet. ABP is young enough to learn this for free. Case-sensitivity is not a dodge: per § 1.2 headers are lowercase-with-underscores at the wire level, so `Topic` and `TOPIC_SEQ` are not valid ABP headers at all and need no separate reservation. The reservation covers only the exact lowercase forms. Future versions MAY tighten overwrite-on-collision to reject-with-RC-10 if pre-seeding turns out to be a problem vector.

**Anonymous publisher identity: synthesized per connection.** Anonymous publishers receive a `anon-<hex_nonce>-<unix_seconds>` identity at connection time. Resolution in § 3.11.1. Rationale: the "anonymous means no notifications, full stop" alternative kills the `topic.active` / `topic.idle` back-off mechanism for the 90% case (Mix scripts running for hours or days are the common producer shape), defeating the purpose of wiring it up as a no-op in v1. The connection-scoped nonce makes cross-connection misdelivery impossible by construction — a notification addressed to a dead connection's identity is dropped at the routing layer, not misrouted to a later anonymous connection that happens to connect to the same topic. Identities are broker-local by explicit mandate: they MUST NOT be federated or persisted, which keeps the failure mode "brief producer restart loses back-off context" rather than "synthesized identity somehow leaks between meshes."

**Broker-extension discovery: via `noded.ping` extensions map.** Capability discovery is advertised in the `noded.ping` response as a versioned extensions map (`{"core": "1.0", "topic": "1.0"}`). Resolution in § 2.6. Rationale: error-on-use discovery accretes startup-latency debugging cost ("why does the first publish take 80ms? oh, we're probing for broker support"). Discovery belongs in the handshake. Versioning per-extension rather than a single monolithic spec version lets `topic.*` evolve on a faster cadence than the stable ui.* surface — important because the broker is the place features will grow first (federation, claim-producer, wildcards). The `extensions` field is strictly additive to `noded.ping`: pre-delta brokers keep working, pre-delta clients keep working, and new clients reading a missing field fall back to "core only, no extensions" — the safe-degradation default per Principle 5 of § 0.3.

No open questions remain. Phase A0 (bounded broker outbound channels) is cleared to start.
