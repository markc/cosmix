---
title: ABP Standard Command Vocabulary
chapter: 2
version: 0.3.3
status: draft
date: 2026-04-18
---

# ABP Standard Command Vocabulary

This chapter defines how ABP commands are named, what universal commands every
service must implement, and how scripts discover a service's capabilities. It
is the naming-convention reference for the entire stack — every command in
every chapter follows these rules.

---

## 1. Command Naming Convention

```
<service>.<verb>[-<noun>]
```

- **service** — the broker registration name (`maild`, `syncd`, `display`, `noded`)
- **verb** — what to do (`get`, `set`, `open`, `list`, `close`)
- **noun** — optional target within the service (`path`, `content`, `status`)

**Rules:**

- Hyphen-separated for multi-word nouns: `get-content`, not `getContent`
- Lowercase only. No underscores in command names.
- Verbs are drawn from the standard vocabulary (§2) where possible
- Service-specific verbs are permitted when standard verbs don't fit
- The service prefix is required in all commands on the wire.
  `maild.account.list` is correct; bare `account.list` and
  `list-accounts` are not.
- **Mix shorthand:** in Mix source, `send "<service>" verb.noun`
  resolves to the fully-qualified `<service>.verb.noun` on the
  wire — the target string supplies the prefix. Examples like
  `send "maild" mailbox.list` and `send "maild" email.get` are
  Mix shorthand, not wire-level commands; the ABP frame still
  carries `command: maild.mailbox.list` etc.

**Examples:**

| Command | Service | Verb | Noun |
|---------|---------|------|------|
| `maild.account.list` | maild | list | account |
| `syncd.share.add` | syncd | add | share |
| `noded.list` | noded | list | — |
| `ui.window` | (display) | — | window |

> **Bare-prefix exceptions.** Two command families are routed by
> *namespace* rather than by service prefix:
>
> - **Display namespace** (`ui.*`, `menu.*`) — **retired 2026-08-16** along
>   with chapters 01b and 05 (`ui.*` left ABP at the control-plane pivot,
>   `_decisions/2026-07-18-amp-as-control-plane.md`; the display stack is
>   chapter 16). The routing described here — to the active display service
>   per `2026-04-27-01b-amp-ui-vocabulary.md`, full surface in
>   `2026-04-07-05-amp-display-protocol.md` §3.11 — is dated history, not
>   live protocol.
> - **Topic broker** (`topic.*`) — the broker topic extension per
>   `2026-04-10-03-bus-topic-pubsub.md` §3.11. These commands are addressed to
>   the local `noded` broker but use the bare `topic.*` prefix on the
>   wire rather than `noded.topic.*`. Capability-dependent: a broker
>   may not support the extension (RC 10 with
>   `{"error": "topic_not_supported"}`); capability discovery is via
>   `noded.ping` `extensions.topic`.

**Note on display commands:** Commands prefixed `ui.*` and `menu.*` are
display-surface introspection commands handled by the display service. They
follow the same naming convention but are specified in
`2026-04-07-05-amp-display-protocol.md` §3.11 rather than here (retired
2026-08-16 — historical reference only).

---

## 2. Standard Verb Vocabulary

These verbs have consistent semantics across all cosmix services. Use them
before inventing service-specific verbs.

### Content verbs

| Verb | Semantics | Example |
|------|-----------|---------|
| `open` | Load, display, or begin working with a resource | `open {"path": "/etc/hosts"}` |
| `close` | Release the current resource | `close` |
| `get` | Read a specific item or property | `get {"id": "inbox"}` |
| `set` | Write a specific item or property | `set {"id": "inbox", "name": "Primary"}` |
| `list` | Enumerate items of a type | `list` or `list {"filter": "active"}` |

### Lifecycle verbs

| Verb | Semantics | Example |
|------|-----------|---------|
| `status` | Return current state or health | `status` → `{state, uptime, ...}` |
| `refresh` | Force data reload from source | `refresh` |
| `save` | Persist current state | `save` |
| `add` | Create a new item | `add {"name": "work", "path": "/sync/work"}` |
| `remove` | Delete an item | `remove {"id": "..."}` |

### Mutation verbs

| Verb | Semantics | Example |
|------|-----------|---------|
| `start` | Begin a long-running operation | `start {"task": "full-sync"}` |
| `stop` | Cancel a long-running operation | `stop {"task": "full-sync"}` |
| `pause` | Suspend without canceling | `pause {"id": "..."}` |
| `resume` | Continue after pause | `resume {"id": "..."}` |

**When standard verbs don't fit:** use a domain-specific verb and document it
in the service's command reference. Example: `syncd.conflict.resolve` uses
`resolve` because none of the standard verbs capture the semantics of conflict
resolution.

---

## 3. Universal Service Commands

Every ABP service SHOULD support these commands. They are the minimum surface
that enables ARexx-style discovery and scripting. Chapter 01 §5.7 introduces
these; this section provides full signatures.

> **Implementation status (audited 2026-06-05).** Bare `HELP`/`INFO`/`QUIT` are
> implemented today by **Mix serve-mode citizens** (SPEC 18 §3.6 L0 conformance,
> `cosmix-mix` `serve_runtime`). The Rust daemons have **not** adopted the bare
> universals — they expose namespaced equivalents instead (`noded.info` /
> `<svc>.info`, build/version via the `noded.list` → `ServiceInfo` record §4.1,
> and the SPEC 07/12 `props.*` surface). "Every service SHOULD support" remains
> the target; treat the bare universals as the Mix-citizen contract until the
> daemons adopt them.

| Command | Description | Args | Returns |
|---------|-------------|------|---------|
| `HELP` | List all commands this service accepts | none | `[{name, description, args}]` as JSON body |
| `INFO` | Service identity and capabilities | none | `{name, version, description}` as JSON body |
| `QUIT` | Graceful shutdown | none | `rc: 0` then disconnect |

**Notes:**

- `HELP` is the discovery entry point. Scripts call `HELP` first to learn what
  a service can do. The response is a JSON array of command descriptors.
- `INFO` provides metadata for fleet inventory and version tracking.
- `QUIT` requests graceful shutdown. The service should clean up, flush state,
  and disconnect from the local broker. systemd will restart it if configured to.
- `ACTIVATE`, `OPEN`, `SAVE`, `SAVEAS`, `CLOSE` from Chapter 01 §5.7 are
  optional — they apply to services with a user-visible surface (display
  panels) but not to headless daemons.

---

## 4. Noded Commands

The local ABP broker (`cosmix-noded`) exposes its own command
surface for service management and mesh discovery.

| Command | Description | Args | Returns |
|---------|-------------|------|---------|
| `noded.register` | Register a service on the local broker | body = `RegisterProvenance` (all optional; `name` from the `from` header) | `rc: 0`, `{registered}` |
| `noded.list` | List all registered services with build provenance | none | `[ServiceInfo]` |
| `noded.info` | Local node identity + the broker's own build + live uptime/service-count | none | `NodeInfo` |
| `noded.ping` | Heartbeat / health check | none | `rc: 0` with optional `{extensions}` |
| `noded.peers` | List peer nodes in the mesh | none | `{node, wg_ip, port, peers: [{name, wg_ip, port}]}` (self-info + per-peer name/IP/port) |
| `noded.tap` | **DEPRECATED — superseded by `noded.observe.*` (§4.2).** Legacy firehose; unredacted, local-route-only coverage. Removed once the `log` consumer migrates. | `{filter?: string}` | streamed ABP frames |
| `noded.observe.start` | Open a bounded, redacted broker observation subscription (§4.2) | filter + body + capacity (§4.2) | `{subscription_id, …}` |
| `noded.observe.stop` | Close an owned observation subscription (§4.2) | `{subscription_id}` | `{stopped}` |

### 4.1 Service & node discovery records (version-discovery contract)

This section is the canonical discovery contract; the shared Rust type lives
in `cosmix-lib-amp::service_info`.

**`ServiceInfo`** — one per registered service, returned by `noded.list`,
nested in `NodeInfo`. **Open struct**: every field except `name` is optional
with a serde default and the struct is NOT `deny_unknown_fields`, so adding a
field later is non-breaking on typed clients. Holds only **immutable**
identity + build provenance (dynamic state belongs in `*.health` / props /
metrics, never the stored registry record):

| field | meaning |
|---|---|
| `name` (required) | registered ABP service name (registry key) |
| `binary` | producing package/crate (`CARGO_PKG_NAME`, e.g. `cosmix-maild`) |
| `version` | `CARGO_PKG_VERSION` |
| `git_sha` · `git_dirty` | build fingerprint (catches a forgotten version bump) |
| `build_time` | RFC3339 UTC build stamp (catches a stale binary) |
| `pid` · `started_at` | citizen process id + RFC3339 start (→ uptime) |
| `registered_at` | RFC3339 registry-binding time (broker-stamped; a same-name refresh re-stamps it — binding, not process, metadata) |
| `schema_version` | record-format version (additive fields do NOT bump it) |
| `meta` | open forward field — experimental JSON values (`Map<String, Value>`) |

The citizen supplies the provenance subset as a `RegisterProvenance` JSON body
on `noded.register`; the broker merges it with `name` + `registered_at`.

**`NodeInfo`** — one per node, returned by `noded.info`. Unlike the stored
`ServiceInfo` it is **computed on read**, so its dynamic fields are live:
`node`, `wg_ip`, `mesh?`, `noded` (the broker's own `ServiceInfo`),
`uptime_s`, `service_count`, `schema_version`, `meta`.

**Dual-parse / rollout.** `ServiceInfo`'s deserializer accepts **either** an
object **or** a bare name string (the pre-2026-06-01 `noded.list` element
shape), so a new client tolerates an old broker during the client-first
rollout. The string arm is dropped a release after every broker emits objects.
A new typed field is additive-safe ONLY for key-accessing consumers — Mix/JSON
consumers MUST read by key (never positional/length/whole-object compare).

**Topic broker commands** (`topic.*`) are specified in
`2026-04-10-03-bus-topic-pubsub.md` — they are broker extensions, not universal service
commands.

### 4.2 Broker observation — `noded.observe.*` (observe extension 1.0)

*Added 2026-07-24 (Tower P0; design record: cmctl
`_journal/2026-07-24-cosmix-desktop-arc.md` + the tower plan). Replaces
`noded.tap`, whose coverage was local-route-only and unredacted.*

**Scope.** Observation is a broker-native tap over **accepted, canonicalised
ABP envelopes after route outcome is known** — invalid frames are excluded,
and observer control/event frames are never themselves observed (no
recursion). This is NOT a topic: topics carry producer publication with
retention/ownership semantics; observation has neither, hence dedicated
verbs. Brokers advertise support via `noded.ping` →
`extensions.observe = "1.0"`.

**Authorisation (v1 — operational gate, not cryptographic).** A subscriber
must satisfy ALL of: (a) registered service on this broker, (b) same-node
origin (the existing loopback/own-bind-IP test), (c) anchored service-name
match against broker config `[observe] allowed_services = [...]` — default
empty, **fail-closed**. Off-node callers are rejected regardless of name.
Upgrade path: authenticated node provenance + bound service identity + an
explicit observe capability grant (SPEC 13 track). A subscription is owned
by its connection; another connection cannot stop it.

**`noded.observe.start`** request body:

```json
{ "filter": { "verbs": ["maild.*"], "services": ["maild"],
              "directions": ["local", "mesh_in", "mesh_out"] },
  "body": "none" | "redacted",   // default "none" (metadata only)
  "capacity": 1024 }
```

`rc: 0` response: `{subscription_id, filter (normalised), body, capacity,
byte_limit, redaction_policy: "observe-v1"}`. The acknowledgement is queued
before the first event.

**`noded.observe.event`** (type `event`, `from: noded`, carries
`subscription_id`) body fields: `seq` (monotonic per subscription), `ts`
(RFC3339), `direction` (`local|mesh_in|mesh_out`), `outcome`
(`delivered|broker_handled|rejected|dropped`), `message_type`
(`request|response|event|stream`), `from`/`to`/`verb` (canonical identities
or null), `size` (canonical wire bytes), `correlation_id` (survives the
broker's internal id rewrite), `rc`, `dropped_count` (ring evictions since
the prior event), and — only when `body: "redacted"` — `payload {headers,
body}` plus `payload_omitted` (`disabled|oversize|opaque|policy|null`).

**`noded.observe.stop`** `{subscription_id}` → `{stopped: true|false}`.
Stop is a **fence**: queued events are purged and none follow the `rc: 0`
response. Stopping an absent or non-owned id returns `stopped: false` with
`rc: 0` (no existence oracle). Disconnect removes all owned subscriptions;
subscriptions are never retained or resumed across reconnect.

**Bounds & backpressure (normative).** Per-subscription drop-oldest ring;
routing never awaits an observer. Capacity default 1024 events (min 64, max
4096), hard byte ceiling 8 MiB per subscription; captured payloads over
64 KiB degrade to metadata (`payload_omitted: "oversize"`). Limits: 4
subscriptions per connection, 16 per broker, 32 verb globs (≤128 bytes
each), 64 service filters. The v1 drainer wakes every 2 ms and batch-drains
ready events up to 1 MiB of queued wire bytes **per subscription per wake**;
this work budget bounds one subscriber's turn without imposing a one-event
throughput ceiling.

**Redaction (normative).** Applied broker-side **before** enqueueing, outside
routing locks: a per-service/verb policy hook first, then a recursive
case-insensitive denylist (credential/authorisation/cookie/password/token/
API-key/private-key/signature fields). The v1 built-in policy table omits
whole payloads for `noded.register`, `noded.admit.*`, and verbs in the
registration, auth, login, and token families; these return
`payload_omitted: "policy"` before field redaction is considered. The table
is an extension point for further service/verb rules. Structured payloads
may otherwise be returned redacted; opaque payloads are omitted
(`payload_omitted: "opaque"`) because field-level redaction cannot be proven.

**Errors.** `rc: 10` — `observe_unauthorised`, `observe_invalid_args`,
`observe_filter_invalid`, `observe_limit_exceeded`; `rc: 20` —
`observe_unavailable`.

### 4.3 Broker-stamped delivery origin

*Added 2026-07-24 for same-node app mutation gates.*

Every accepted **request, event, or correlated response** envelope delivered
by noded to a registered local service carries exactly one broker-owned
header:

```text
broker_origin: local | mesh
```

The **recipient noded** MUST remove every case-insensitive spelling of
`broker_origin` supplied by a client before stamping the header. It stamps
`local` only when the delivering connection satisfies noded's same-node
loopback/own-bind-IP classifier; mesh ingress and every other connection are
stamped `mesh`. A sender cannot select or preserve this value across routing.

For `topic.publish`, the broker strips and stamps the **inner** envelope from
the publisher connection before live fan-out or retention. A retained replay
keeps the origin computed at publish time; it is not recomputed from the later
subscriber connection. A locally correlated response is stamped from the
responder socket, while a response returning over mesh is stamped `mesh` by
the recipient broker before forwarding to its caller.

A recipient using this header as a same-node authorisation gate MUST require
exactly one value equal to `local`; a missing, duplicated, unknown, or `mesh`
value fails closed. This deliberately makes such mutation unavailable through
older brokers which do not stamp the header: brokers and gated recipients must
be deployed together.

Broker-generated direct notifications are not authorisation inputs and remain
outside this stamping contract.

`broker_origin` proves only the delivery class at the recipient broker. It
does not authenticate a service identity, grant cross-node capability, or
replace signed mesh provenance. Those remain SPEC 13 work.

---

## 5. Return Code Conventions

ABP return codes follow the ARexx convention. See Chapter 01 §5.6 for the
authoritative definition:

| Code | Meaning | When to use |
|------|---------|-------------|
| 0 | Success | Command executed normally |
| 5 | Warning | Partial result, non-fatal issue (e.g., some items skipped) |
| 10 | Error | Command failed but service is healthy (e.g., bad args, not found) |
| 20 | Failure | Severe error, service may be degraded |

Absence of `rc:` in a response implies `rc: 0`.

---

## 6. Extension Guidance

When adding commands to a new or existing service:

1. **Name it correctly.** `service.verb-noun` form, standard verbs first.
2. **Document it in HELP.** Every command must appear in the service's `HELP`
   response with a description and expected args.
3. **Use standard return codes.** Don't invent new codes; the 0/5/10/20 range
   is sufficient.
4. **Args are JSON objects.** Use `args:` header for command parameters, not
   positional encoding.
5. **Bodies are for content.** Structured data goes in `args:` or `json:`.
   Bodies are for markdown, prose, or large payloads.
6. **Don't duplicate display commands.** *(Historical — chapter 05 retired
   2026-08-16; `ui.*` is no longer an ABP surface.)* Widget introspection was
   specified in `2026-04-07-05-amp-display-protocol.md` §3.11.

---

*Document created: 2026-03-29, rewritten 2026-04-18*
*Supersedes: AMP Command Vocabulary v0.1 (partial command registry) — released under the protocol's former AMP name*
