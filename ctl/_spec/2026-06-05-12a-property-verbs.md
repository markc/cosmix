---
title: Cosmix Property Substrate — Verbs
chapter: 12a
version: 0.2.2
status: draft
date: 2026-06-05
amends: _spec/2026-06-05-07b-property-surface.md (extends the read verbs with structured addressing; adds the mutation/validate/audit-watch verbs)
companion: _spec/2026-05-11-12-property-substrate.md
---

# Cosmix Property Substrate — Verbs

> **Split out of SPEC 12 §5 (2026-06-05).** The ABP verb surface for the property
> substrate: `<svc>.props.{list,get,set,delete,watch,describe,validate,audit.watch}`.
> The read verbs (`list`/`get`/`watch`/`describe`) extend SPEC 07b §2 with structured
> `(namespace, key, field_path)` addressing + `nseq` replay; `set`/`delete`/`validate`/
> `audit.watch` are the SPEC-12 additions. `props.validate` is **unbuilt**; the rest are
> code-backed (`cosmix-lib-props-store`). The conceptual model + conformance live in the
> core chapter 12; §-numbers are preserved as **§5.x** so cross-references resolve here.

## 5. The verbs

All verbs are ABP commands. Headers are listed under each verb;
unmentioned headers follow SPEC 01 conventions (`from`, `to`,
`id`, `reply-to`, etc.). Responses are ABP replies (SPEC 01 §6)
with body JSON unless otherwise noted.

The verb family is the existing `<svc>.props.*` defined in SPEC 07,
extended by this chapter. `<svc>` is the owning service's
registered ABP name (e.g. `maild`, `noded`, `display`).

### 5.1 `<svc>.props.list` (extends SPEC 07 §2)

List records or flat paths.

Request headers:

| Header | Required | Description |
|--------|----------|-------------|
| `namespace` | no | Namespace name. Required for collection enumeration; omitted for SPEC 07 flat-path listing. |
| `cursor` | no | Pagination cursor from a previous response. |
| `limit` | no | Max records per page (1..=1000). Default 100. |

When `namespace` is omitted, the verb returns SPEC 07's flat-path
listing: all defined leaf paths under the service's property tree.
When `namespace` is present, the verb enumerates records in that
namespace.

Optional **request body** (JSON object, structured mode only):

```json
{
  "filter": {
    "spam_enabled": true,
    "username": { "regex": "^markc" }
  },
  "fields": ["email", "spam_enabled"]
}
```

- `filter` is a JSON object mapping field names to either a scalar
  (exact match) or a predicate object (`{"regex": "..."}`,
  `{"in": [...]}`, `{"gt": N}`, `{"lt": N}`). Predicates compose
  with AND. Field types and predicate types must match the
  namespace's schema; type mismatches produce `validation_error`.
- `fields` is a JSON array of field names to return; absent means
  "all non-secret fields". An explicit field listing is the only
  way to include `secret` fields, and is rejected without
  `props.read:<svc>.<ns>:secrets`.

Comma-separated header forms are deliberately avoided here:
namespace field names can contain UTF-8, and predicate values may
themselves be arrays or contain commas. JSON in the body is the
only form that round-trips unambiguously.

Response body (JSON, structured mode):

```json
{
  "namespace": "accounts",
  "nseq": 1042,
  "records": [
    { "key": "user@alpha.amp", "version": 3, "fields": { ... } },
    ...
  ],
  "next_cursor": "opaque-string-or-null"
}
```

`nseq` is the per-namespace event sequence observed by this
snapshot (§5.5). Pagination is cursor-based; the cursor is opaque
to the caller and stable for the duration of the namespace's
storage backend's snapshot (typically the connection lifetime).

Response body (JSON, flat-path mode, per SPEC 07 §2): an array of
property path strings.

### 5.2 `<svc>.props.get` (extends SPEC 07 §2)

Read one record (structured mode) or one flat-path value (SPEC 07
mode).

Request headers:

| Header | Required | Description |
|--------|----------|-------------|
| `path` | yes (flat-path mode) | Dotted path under SPEC 07 §2.2. |
| `namespace` | yes (structured mode) | Namespace name. |
| `key` | yes (collection) / no (singleton) | Record key. |
| `field_path` | no | JSON-array sub-selector inside the record. |

Exactly one of `path` or `namespace` MUST be supplied.

Optional **request body** (JSON object):

```json
{ "fields": ["email", "spam_enabled", "password_hash"] }
```

`fields` is a JSON array; absent means "all non-secret fields".
Including a `secret` field requires `props.read:<svc>.<ns>:secrets`
and is rejected with `auth_denied` otherwise.

Response body: a single record envelope (§8.3) for structured
mode, or a SPEC 07 §2.3 property-tree fragment for flat-path mode.
`not_found` if the addressed record or path does not exist.

### 5.3 `<svc>.props.set` (NEW — fulfils SPEC 07 §10 deferred contract)

Create or update a record in a registered namespace. SPEC 07 §10
deferred the shape of this verb; SPEC 12 v0.1 supplies the
**structured-mode** form. Flat-path mutation of SPEC 07 leaves
(the `path: config.bind` form) is deferred to v0.2 — the wire
shape for scalar-leaf mutation needs a worked in-tree use case
before being pinned. v0.1 daemons that need to mutate SPEC 07
leaves expose service-specific commands as before.

Request headers (structured mode only):

| Header | Required | Description |
|--------|----------|-------------|
| `namespace` | yes | Namespace name. |
| `key` | yes (collection) / no (singleton) | Record key. For singletons, the substrate fills in the canonical key. |
| `field_path` | no | JSON-array sub-selector for a partial update. |
| `if_version` | no, unless spec requires | Expected current version; reject with `version_mismatch` if different. Omit to upsert. |
| `merge` | no | `true` to merge supplied fields with existing record (PATCH semantics); `false` to replace (PUT semantics). Default `true`. |

A request carrying a `path:` header is rejected with
`validation_error: flat_path_mutation_deferred` until v0.2.

Request body (JSON): an object with field-value pairs.

Response body for `Simple` namespaces (`NamespaceSpec.lifecycle
= Simple`):

```json
{ "namespace": "themes", "key": "", "version": 4, "nseq": 1043, "created": false }
```

`created` is `true` when this set materialised a new record,
`false` when an existing record was updated. The verb is
idempotent under `if_version`: a retry that lands a duplicate
write returns `version_mismatch`, not silent success. (Example
uses the `themes` singleton because `accounts` is a `Saga`
namespace per §6.5 and uses the Saga response shape below.)

Response body for `Saga` namespaces — the verb does **not**
return until the saga has reached a terminal lifecycle state
(`Active` or `Failed`), so the response carries both the
initial-set sequence pair and the terminal-complete sequence
pair:

```json
{
  "namespace": "accounts",
  "key": "...",
  "set_version": 4,
  "set_nseq": 1043,
  "complete_version": 5,
  "complete_nseq": 1044,
  "lifecycle": "active",
  "created": true
}
```

When the saga lands in `Failed`, `lifecycle` is `failed` and an
additional `reason` string is present; the response is still a
verb success (HTTP-equivalent 2xx) because the substrate-level
operation completed — the failure is a domain outcome, not a
substrate error. Callers that treat `failed` lifecycles as
errors do so at the application layer.

### 5.4 `<svc>.props.delete` (NEW)

Remove a record.

Request headers:

| Header | Required | Description |
|--------|----------|-------------|
| `namespace` | yes | Namespace name. |
| `key` | yes (collection) | Record key. Singleton namespaces cannot be deleted, only `set` with default values. |
| `if_version` | no, unless spec requires | Expected current version. |

Flat-path deletion is not supported — config leaves cannot be
"deleted" through the substrate (they revert to defaults via
`<svc>.props.set` with the schema's default value).

Response body:

```json
{ "namespace": "accounts", "key": "...", "deleted": true, "tombstone_version": 5, "nseq": 1044 }
```

The tombstone version is the version the record would have had if
updated. Subscribers (§5.5) receive a delete event carrying this
version.

A namespace declares one of two delete modes in its `NamespaceSpec`:

- **`SoftDelete`** (default for any namespace that supports
  `if_version`): the record's storage row is retained as a
  tombstone carrying `version` and `key` only; subsequent
  `<svc>.props.set` to the same key resumes the version sequence
  (`tombstone_version + 1`), so `if_version` checks remain
  well-defined across deletion and recreation. Tombstones expire
  per the namespace's `tombstone_ttl` (default 7 days), after
  which the key is forgotten.
- **`HardDelete`**: the storage row is removed immediately and no
  version state is retained. A namespace using `HardDelete` MUST
  NOT declare `require_version: true`, and `<svc>.props.set` to a
  previously-hard-deleted key starts a fresh version sequence at
  1 — meaning stale `if_version=<old>` checks against a deleted
  key cannot be distinguished from `if_version` against a
  same-key-new-incarnation record. Hard-delete namespaces
  therefore SHOULD NOT be used with optimistic-concurrency writers.

`<svc>.props.get` returns `not_found` after a successful delete in
either mode; the tombstone in `SoftDelete` mode is visible only
through the version-mismatch error path.

### 5.5 `<svc>.props.watch` (extends SPEC 07 §2 and §3)

Watch a service's property surface for changes. SPEC 07 already
defines `props.watch` as L2 conformance; SPEC 12 extends it with
capability gating, namespace scoping, and gap-free replay.

Request headers:

| Header | Required | Description |
|--------|----------|-------------|
| `path` | no (flat-path mode) | Filter to changes under this SPEC 07 path prefix. |
| `namespace` | no (structured mode) | Filter to changes within this namespace. |
| `since_nseq` | no | Replay events with `nseq > since_nseq` (structured mode only). |

Omitting both `path` and `namespace` watches the entire service
property surface — SPEC 07's default behaviour.

Every namespace exposes a monotonically increasing **namespace
sequence** `nseq` that increments by 1 on each successful
`<svc>.props.set` or `<svc>.props.delete` in the namespace. The
substrate guarantees:

- `<svc>.props.list` and `<svc>.props.get` responses (structured
  mode) include `nseq:` giving the sequence number observed by
  the read (the point-in-time of the snapshot).
- `<svc>.props.watch` with `since_nseq:` replays events with
  `nseq > since_nseq` from the retained event history (§6.6)
  before joining the live subscription stream. This closes the
  classic list-then-watch race: a caller lists at `nseq=N`,
  immediately watches with `since_nseq=N`, and is guaranteed to
  see every subsequent change in `nseq` order with **no gaps** —
  `nseq` values are contiguous on the wire. Network delivery
  itself is at-least-once (§6.6); subscribers MUST therefore
  de-duplicate by `(namespace, key, nseq)` and accept that a
  duplicate event carrying the same `nseq` as a prior one is a
  redelivery, not a new change.
- The replay window is bounded by the namespace's `replay_window`
  (default: the most recent 1024 events, or 1 hour, whichever is
  smaller). If `since_nseq` falls outside the window the service
  responds with `error_code: replay_window_exceeded` (§9); the
  caller MUST re-list to recover.
- On a **structured-mode watch** (`namespace:` present) the
  service emits exactly one **`caught_up`** control message on
  the watch reply stream, after draining the replay window and
  immediately before forwarding the first live event. The
  replay window is the set of retained-history events with
  `nseq > since_nseq` (§6.6) when `since_nseq` is present, and
  is empty when `since_nseq` is absent — in the absent case
  the watch joins the live tail directly with no replay, and
  `caught_up` fires once at the live-tail join point. Body
  shape: `{ "event_type": "caught_up", "namespace": "<ns>",
  "nseq": <N> }`, where `N` is the namespace's high-water mark
  observed by the watch handler at the instant the live
  subscription is installed (the largest `nseq` that has been
  committed to event history and that the handler is now ready
  to forward from). `N` is NOT the caller's `since_nseq` even
  when replay drained empty (a caller passing
  `since_nseq >= current_nseq` still receives the server-
  observed high-water mark, never a value above it). The
  marker's guarantee depends on whether replay was non-empty:
  - When `since_nseq` is **present**, `caught_up` proves that
    every committed event in the interval `(since_nseq, N]`
    has been delivered on this watch (either as a replay event
    or as a live event the handler ordered ahead of the marker
    — the watch handler is responsible for ensuring any such
    live event with `nseq <= N` reaches the subscriber before
    the marker does). A consumer building a local mirror
    through list-then-watch holds its readiness gate closed
    until the marker arrives, then opens it knowing every
    event in `(since_nseq, N]` has been delivered and every
    later event will arrive in `nseq` order.
  - When `since_nseq` is **absent**, replay is empty and
    `caught_up` makes no claim about historical events — it
    is purely a **live-tail join boundary**, reporting the
    current high-water mark as a watermark for the live
    stream that follows. Consumers that need historical
    coverage MUST use `since_nseq:` (typically driven by a
    prior `<svc>.props.list` response's `nseq`).

  The marker fires regardless of whether
  replay drained zero or many events — a structured-mode watch
  with `since_nseq` absent MUST still emit one `caught_up`
  once the watch is established.
- The watch handler MUST emit `caught_up` exactly once per
  successful watch. Network-level redelivery of any single
  reply-stream message remains possible (ABP transport offers
  no exactly-once guarantee), so subscribers MUST treat the
  **first** accepted `caught_up` as the watch's readiness
  boundary. A later `caught_up` carrying the **same** `nseq`
  on the same watch is a redelivery and MUST be ignored. A
  later `caught_up` carrying a **different** `nseq` on the
  same watch is a protocol violation; subscribers SHOULD close
  the watch and reseed (re-list, re-watch). This is a separate
  ordering contract from the §6.6 transactional outbox — the
  outbox governs durable record-event dispatch to topics, not
  per-watch control messages.
- `caught_up` is delivered **only** on the
  `<svc>.props.watch` reply stream; the underlying
  `<svc>.props.records.changed` topic carries record events
  only (direct `topic.subscribe` on that topic is refused per
  the existing §5.5 rules in any case). Flat-path-mode watches
  (`path:` present) do not carry `nseq` and therefore MUST NOT
  emit `caught_up`; whole-surface watches (no `path` and no
  `namespace`) MUST NOT emit it either, because the flat-path
  half of the merge has no watermark to which the marker would
  refer and a per-namespace marker on a whole-surface stream
  would have undefined `namespace:` scoping.
- **Wire framing under current ABP transport.** ABP correlates
  one request to exactly one reply message (SPEC 01 §3
  `oneshot`-style correlation). The "watch reply stream" of
  this section is therefore realised, on today's transport, as
  the single watch response envelope: replay events appear in
  the `events` field, then a top-level `caught_up` field
  carries the marker, then live record events arrive on the
  granted `<svc>.props.records.changed` subscription. A
  consumer building the logical event stream (e.g. mesh-trust's
  production `WgdClient`) walks `events` → emits the marker
  from `caught_up.nseq` → switches to live record events,
  dropping any record with `nseq <= caught_up.nseq` to honour
  the "live event ≤ N reaches subscriber before the marker"
  guarantee (the granted subscription is live from before the
  cursor snapshot, so a live record may have already been
  forwarded). The `events` array stays homogeneous — it carries
  record-event bodies only, never a `caught_up`-shaped element
  — because §5.5's records-changed body and the marker are
  distinct schemas and the replay/live byte-identical invariant
  applies only to record events. A future ABP transport
  extension that introduces genuine multi-frame reply streams
  MAY relocate the marker into its own frame without changing
  this section's per-watch ordering or dedup guarantees;
  consumers MUST tolerate either framing.

SPEC 12 keeps the two event surfaces separate so SPEC 07's
existing topic contract is preserved unchanged:

- **`<svc>.props.changed`** (SPEC 07 §3.1 topic, unchanged):
  carries flat-path leaf changes only. Body fields stay exactly
  as SPEC 07 defines them — `path`, `old`, `new`, `ts`, optional
  `cause`. Secret-field redaction follows SPEC 07 §7.2. SPEC 12
  does not extend this topic's body shape.
- **`<svc>.props.records.changed`** (NEW SPEC 12 topic): carries
  structured-mode (namespace, key) record changes. Body fields:

| Body field | Description |
|------------|-------------|
| `namespace` | Namespace name. |
| `key` | Record key. |
| `kind` | One of `created`, `updated`, `deleted`, `completed`, `reconciled`. The first three arise from caller-driven `set` / `delete`; `completed` is the Saga lifecycle transition (`Provisioning → Active|Failed`) committed by the library on `after_set` return (§6.5); `reconciled` is the synthetic hand-edit event emitted on startup (§11). |
| `verb` | The event-history verb that produced this row: `<svc>.props.set` (`kind=created|updated`), `<svc>.props.delete` (`kind=deleted`), `<svc>.props.complete` (`kind=completed`), or `<svc>.props.reconcile` (`kind=reconciled`). |
| `nseq` | Namespace sequence of this event. |
| `version` | The record's new version (or tombstone version on delete). |
| `audit_epoch` | Per-namespace generation counter (§11). Incremented only on `reconciled` events; attached to every event so subscribers can detect epoch discontinuities. |
| `actor` | Peer identity that performed the change, in SPEC 07 §3.5.1 actor-variant form. `completed` events carry `daemon:<svc>` (the library transition is attributed to the owning daemon, not the original caller); `reconciled` events carry `daemon:reconciliation`. |
| `fields_changed` | Array of changed non-secret field names (best-effort; absent on `deleted`; on `completed`, lists the lifecycle-related fields the substrate flipped, typically `["_lifecycle"]`; on `reconciled`, lists the union of field names that differ between pre-edit and post-edit record state). Secret field names — declared via `secret: true` in the namespace schema — MUST be omitted from this list and replaced by a single `secret_fields_changed_count: <n>` body field giving how many secret fields were touched. This matches the SPEC 07 §7.2 redaction discipline applied to the structured surface. |
| `lifecycle` | Present only on `completed` events from `Saga` namespaces. One of `active`, `failed`. When `failed`, an additional `reason` field carries the `after_set` error string (secret-redacted by the sanitiser). |
| `cause` | Optional ABP id of the originating request (per SPEC 07 §7.4). For `completed` events, the `cause` of the originating `set` is preserved so subscribers can correlate set → complete pairs; for `reconciled` events, `cause: external_edit_detected`. |
| `ts` | Wall-clock timestamp at commit (RFC 3339). |

`<svc>.props.records.changed` event bodies are deltas, not
snapshots; subscribers wanting the new record state call
`<svc>.props.get` with the structured headers. This avoids
emitting secret-field values in fan-out. A namespace MAY opt in
to inline-payload events with the `subscribe_payload: full` spec
field if no field is `secret`.

`<svc>.props.watch` is the substrate-facing verb that merges
both surfaces transparently: a request with `path:` joins
`<svc>.props.changed`; a request with `namespace:` joins
`<svc>.props.records.changed`; an unfiltered watch joins both
and delivers each event with its native shape. The owning service
performs the capability check (`props.read:<svc>.<ns>` for
structured-mode watches, the SPEC 07 sensitivity check for
flat-path watches), the `since_nseq` replay (structured mode
only — flat-path events do not carry `nseq`), and the live
transition.

Direct `topic.subscribe` on `<svc>.props.changed` remains
available under SPEC 07's semantics (no capability check; secret
fields already redacted in event bodies), but does not
participate in `since_nseq` replay and yields only events emitted
after the subscribe lands.

Direct `topic.subscribe` on `<svc>.props.records.changed` is
**not** permitted. Unlike flat-path events whose bodies are
constrained to `path`/`old`/`new` (and whose `path` SPEC 07
already mandates be redacted for sensitive fields), structured
records.changed bodies carry identifying keys (e.g. account
email addresses) and per-actor mutation cadence — the body
shape itself is the leak surface. Because the topic is
per-service, not per-namespace, a relaxation flag on any single
namespace would let subscribers see records from every other
namespace on the same service; a per-namespace topic-suffix
scheme (`<svc>.props.records.changed.<namespace>`) was
considered and rejected as more wire surface than it earns. The
broker therefore reserves `<svc>.props.records.changed` flatly
(§15.5): only the owning service may publish, and
`topic.subscribe` from any other peer is refused. All
subscribers reach structured records via `<svc>.props.watch`,
which performs `props.read:<svc>.<ns>` enforcement at the owning
service.

Namespaces that want a genuinely-public read surface (e.g. mesh
peer announcements) declare so by granting
`props.read:<svc>.<ns>:public` in their `AuthPolicy`; the watch
verb then succeeds for unauthenticated peers. The capability
grant — not a broker-level topic flag — is the single source of
truth for public visibility.

### 5.6 `<svc>.props.describe` (extends SPEC 07 §2)

Fetch the schema for a path or namespace. SPEC 07 §2.4 defines the
verb for flat paths; SPEC 12 extends it for namespaces and adds
capability gating.

Request headers:

| Header | Required | Description |
|--------|----------|-------------|
| `path` | yes (flat-path mode) | SPEC 07 path; returns the SPEC 07 §2.4 describe envelope for that leaf or subtree. |
| `namespace` | yes (structured mode) | Namespace name; returns the full namespace `PropertySchema`. |
| `view` | no | `public` (default) or `full`. See below. |

For structured mode, the response body is a `PropertySchema` JSON
document (§8.3). The shape is stable within a major substrate
version; SPEC 12 v0.x guarantees additive changes only.

Schema is **gated by capability**, strengthening SPEC 07 §7.2's
sensitivity model: field names, validators, defaults, and
secret-flag annotations are themselves operational information (a
caller who can enumerate a namespace's private-key field names
learns part of the daemon's security surface). Two views exist:

- `view: public` requires `props.describe:<svc>.<ns>:public` and
  returns a redacted schema view: fields marked `secret` are
  entirely omitted, and validators containing potentially
  sensitive regex patterns (declared by the namespace owner as
  `validator_secret: true`) are replaced with the token
  `"<redacted>"`. A low-privilege GUI reaches for this view to
  render a non-admin form *only when the namespace's `AuthPolicy`
  grants `props.describe:<svc>.<ns>:public` to the caller* —
  typically by extending the public-capability set to
  unauthenticated peers, or to a broad role like `staff`.
  Namespaces that do not grant the public capability return
  `auth_denied` to such a caller (the namespace's
  `schema_public: Deny` setting on the `NamespaceSpec` is the
  declarative form of this refusal — see §6).
- `view: full` requires `props.describe:<svc>.<ns>:full` and
  returns the full schema including secret-field names and all
  validators. Required to render an admin form or a substrate-
  aware migration tool.

For flat-path mode, the SPEC 07 §2.4 envelope continues
unchanged. The view-gating model applies to structured mode only;
SPEC 07's `sensitive: true` annotation continues to govern
flat-path describe.

Namespaces MAY declare in their `NamespaceSpec` that the public
view is empty (`schema_public: deny`), in which case the public
view returns `auth_denied`. This is appropriate for namespaces
whose mere existence is sensitive (e.g. `peers` on a node that
doesn't wish to advertise its peer list shape).

### 5.7 `<svc>.props.validate` (NEW)

Dry-run a set without applying it.

Request headers and body match `<svc>.props.set`, except no change
is written and no event is emitted. Response body lists any
`validation_error`s; an empty list means the write would succeed
under the current state.

This verb is used by GUIs for live form validation as the user
types. It runs **only** the namespace's pure schema validators —
type, range, regex, `one_of`, uniqueness check against the
current snapshot. It MUST NOT invoke `before_set` / `after_set`
hooks, because those hooks are not required to be pure: a hook
may issue network requests, mutate external state, or charge an
external system. Conflating validation with hook execution would
let a keystroke-rate GUI accidentally drive side-effectful
operations.

A namespace that needs custom validation logic beyond the schema
vocabulary declares a dedicated `validator` (distinct from
`before_set`) — a function the namespace owner guarantees to be
pure and idempotent. Validators run in `<svc>.props.validate` and
again at the start of `<svc>.props.set`; hooks run only inside
`<svc>.props.set`.

`<svc>.props.validate` requires the **same
`props.write:<svc>.<ns>` capability** as `<svc>.props.set` it
shadows. Without this check, an unauthorised caller could use
validate as an oracle: uniqueness predicates would reveal which
keys exist, regex validators with back-references could
fingerprint record contents, and feeding crafted inputs would
reveal validator structure. Requiring the write capability
collapses the verb's auth surface to match the verb it dry-runs.

`<svc>.props.validate` takes no lock — between a successful
validate and a subsequent `<svc>.props.set`, another writer may
invalidate the input. Callers needing read-modify-write semantics
use `if_version` on the `<svc>.props.set`.

### 5.8 `<svc>.props.audit.watch` (NEW)

Subscribe to a namespace's audit stream. The audit stream is the
activity-event topic `<svc>.props.audit` (per SPEC 07 §3.5.2's
per-daemon naming convention). Subscription is routed through the
owning service rather than direct `topic.subscribe`, so the
capability check (`props.audit:<svc>.<ns>` or `props.audit:*`)
runs against per-namespace policy that the broker does not
evaluate.

Request headers:

| Header | Required | Description |
|--------|----------|-------------|
| `namespace` | yes | Namespace to receive audit events for. `*` requires `props.audit:*`. |
| `since_nseq` | no | Replay audit entries with `nseq > since_nseq` within the namespace's retention window (§6.6). |

The owning service checks `props.audit:<svc>.<namespace>` (or
`props.audit:*`) before admitting the subscription, replays from
`since_nseq`, then transitions the subscriber to live audit
delivery. Audit events arrive as `<svc>.props.audit` topic
messages carrying the headers in §10.

A subscriber holding `props.audit:*` MAY pass `namespace=*` to
receive events from every namespace on the service through one
subscription; otherwise the request MUST name a specific
namespace.

No anonymous audit subscription exists; audit is operational
intelligence, not public information. See §15.5 for the
reserved-prefix rule that prevents bypass via direct
`topic.subscribe` on `<svc>.props.audit`.

