---
title: Cosmix Property Substrate
chapter: 12
version: 0.2.2
status: draft
date: 2026-05-26
amends:
  - _spec/2026-04-27-07-self-aware.md  (extends with mutation, audit, capability gating, managed collections; resolves §6.4 schema-language, §10 mutation-contract)
draws_from:
  - _spec/2026-03-24-01-bus-wire-protocol.md (verb framing, mesh addressing)
  - _spec/2026-04-10-03-bus-topic-pubsub.md (topic transport for the changed/audit events)
  - _spec/2026-05-09-10-cosmix-daemon-identity.md (storage location, /etc vs /var/lib boundary)
  - $COSMIX/src/crates/cosmix-lib-props-core/ (SPEC 07 read surface; PropTree, PropPath, PropValue) — split out of the former `cosmix-lib-props` crate on 2026-05-29.
  - $COSMIX/src/crates/cosmix-lib-props-store/ (SPEC 12 mutation surface; storage backends, audit HMAC, NamespaceSpec, runtime, mutation router) — renamed from `cosmix-lib-props` on 2026-05-29 in the same split.
---

# Cosmix Property Substrate

> **Crate naming (2026-05-29):** the substrate library originally
> shipped as a single crate `cosmix-lib-props`. On 2026-05-29 the
> crate was split into `cosmix-lib-props-core` (SPEC 07 read surface:
> `PropTree`, `PropPath`, `PropValue`, `PropDescribe`, `redact`,
> `diff`, plus an opt-in `amp` feature carrying `dispatch_props` and
> `publish::*`) and `cosmix-lib-props-store` (everything else: storage
> backends, audit HMAC, hooks, capability, runtime, SPEC 12 mutation
> router, dispatcher fan-out). References to `cosmix-lib-props` in
> this document — particularly in the v0.1.x historical retrospectives
> below and the worked examples — should be read as references to the
> appropriate half of the split pair, or to the pre-split single crate
> when discussing pre-2026-05-29 history. The split is a code-
> organization change, not a wire-contract change; every SPEC 12
> normative claim about the substrate library applies to the split
> pair acting together.
>
> **Status (2026-05-26):** v0.2.2 draft. v0.1.0 invented a parallel
> `property.*` verb family and a hypothetical `cosmix-lib-property`
> crate; v0.1.1 realigned with prior art: SPEC 07 normatively
> declares `<svc>.props.{get,list,describe,watch}` and the existing
> `cosmix-lib-props` crate implements the read/describe/redact half.
> v0.2.x positions this chapter as an **amendment to SPEC 07** that
> supplies the mutation contract SPEC 07 §10 deferred to SPEC 09,
> the collection model SPEC 07 §2.2 left out (its dotted-path
> grammar excludes keys containing `.` or `@`), and the audit /
> capability gating that SPEC 07 §7.2 only sketched. v0.2.1 added
> the `caught_up` watch marker; v0.2.2 added the §12 normative
> clarifier on the "read as if default" boundary (substrate-wire
> reads stay byte-faithful for §10 audit-digest reproducibility;
> typed namespace readers supply amendment defaults).
>
> Subordinate chapters (10 Daemon Identity, the planned per-daemon
> admin specs) will, once this stabilises, reference SPEC 07+SPEC 12
> as the unified property surface rather than re-specifying CRUD.

This chapter extends SPEC 07's read surface to cover the rest of
property lifecycle: **mutation**, **registered collections**,
**capability-gated access**, and **audit**. It does so by adding
verbs to the existing `<svc>.props.*` family (`set`, `delete`,
`validate`, `audit.watch`) and by extending `list` / `get` /
`describe` / `watch` with structured headers for collection records
whose keys cannot be expressed in SPEC 07's flat dotted-path grammar.

The intent is plain: a maild operator creating an email account, a
desktop user changing a widget color, and a mesh node operator
renaming the node are three instances of one operation — "write a
property" — and the substrate should treat them as such. The same
writes are visible to all three operator surfaces (CLI, GUI, web)
and to AI agents, because they all go through the same verbs on
the same namespaces with the same authorization and audit story.

The design draws explicitly from two precedents: ARexx's "every
application exposes a named, addressable port that can be scripted
universally" (Cosmix Mandate, §"The relationship to ARexx"), and
gsettings/dconf's "every preference is a typed, schema-described,
subscribable key in a hierarchical namespace" (without inheriting
dconf's binary-blob storage or single-daemon central database).

## 1. Summary

A **property** is a piece of named, typed, schema-described state
owned by a service. SPEC 07 §2 already defines its read surface —
`<svc>.props.get`, `<svc>.props.list`, `<svc>.props.describe`,
`<svc>.props.watch` — over a flat dotted-path tree (`config.bind`,
`lifecycle.uptime_s`). That model handles a daemon's bounded,
enumerable state (config, lifecycle, registered service summary,
counters) cleanly.

SPEC 12 adds two things SPEC 07 omitted by design:

1. **Mutation.** Per SPEC 07 §10, "mutation through `props.set` is
   deferred to SPEC 09". SPEC 12 supplies the wire-level mutation
   contract — `<svc>.props.set`, `<svc>.props.delete`,
   `<svc>.props.validate` — with optimistic concurrency, hooks,
   capability gating, and audit. SPEC 09 (Self-Improve Layer,
   v0.1.0 draft at `_spec/2026-04-27-09-self-improve.md`) is expected to
   compose *on top* of this contract: SPEC 09 actions will
   determine which capabilities a given agent presents, and SPEC
   12 enforces them at the verb. SPEC 09 will not respecify the
   wire. Until SPEC 09 stabilises, capability assignment is the
   deployment's responsibility (the
   namespace's `AuthPolicy::resolve` function — §7.2).

2. **Managed collections.** SPEC 07's path grammar
   (`[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*`) cannot address a record
   whose key contains `.` or `@` — e.g. an email address as a maild
   account's primary key. SPEC 12 introduces the **namespace**
   abstraction: a subtree of a service's property tree, declared by
   the owning service via `NamespaceSpec`, addressed on the wire as
   structured `(namespace, key, field_path)` headers. Singletons
   and bounded-tree paths continue to use SPEC 07's flat-path form
   unchanged.

A service registers one or more namespaces with the substrate. A
namespace declares its record shape, cardinality (singleton or
collection), storage backend, validation hooks, capability policy,
delete mode, replay window, and audit configuration. Once
registered, all CRUD operations on that namespace flow through the
`<svc>.props.*` verbs.

The verbs are ABP commands carried over the existing ABP wire
protocol (SPEC 01), addressable mesh-wide via the existing
`<service>@<node>` routing (SPEC 01 §4). Watch is layered on the
existing SPEC 07 `<svc>.props.changed` topic; audit uses a new
sibling topic `<svc>.props.audit` (an activity event topic per
SPEC 07 §3.5.2).

This chapter does not introduce a new daemon, a new wire format, or
a new transport. It extends an existing register, an existing verb
family, and an existing library — since 2026-05-29 the split pair
`cosmix-lib-props-core` (SPEC 07 read surface) +
`cosmix-lib-props-store` (SPEC 12 mutation surface).

## 2. Scope and non-goals

In scope:

- Mutation verbs `<svc>.props.{set,delete,validate}` and their
  message shapes (§5).
- Audit subscription verb `<svc>.props.audit.watch` and the
  `<svc>.props.audit` activity-event topic (§5, §10).
- Extensions to existing SPEC 07 verbs `<svc>.props.{list,get,describe,watch}`
  for namespace addressing, capability gating, and replay (§5).
- The namespace registration model (§6).
- Capability gating (§7), audit (§10), transport (§8), error
  taxonomy (§9).
- Conformance levels (§13).
- Relationship to existing primitives — SPEC 07 read surface, TOML
  bootstrap config, maild Accounts, display widget properties,
  topic pub/sub (§15).

Out of scope:

- The CLI / GUI / web frontends that consume the substrate. Those
  are separate components (`mixctl`, the desktop preferences
  surface, the web admin panel) and will be specified in their own
  chapters.
- Cross-namespace transactions. A `<svc>.props.set` on namespace
  `A` and another on namespace `B` are independent operations; if
  both must succeed atomically, the caller composes a higher-level
  operation inside one namespace. v1 deliberately omits a two-phase
  commit.
- Bulk operations (`<svc>.props.set_many`, `<svc>.props.import`).
  These can be added compatibly later; v1 keeps the verb count
  small.
- Migration of all existing config and CRUD surfaces onto the
  substrate. That is a multi-quarter effort tracked separately.
  This chapter defines the target; adoption is gradual and
  per-namespace.
- The persistence format of any specific namespace. The substrate
  provides storage *backends* (§6.4); each namespace picks one, but
  what bytes land on disk is a per-namespace concern.
- SPEC 09's trust-gradient policy. SPEC 12 enforces capabilities;
  it does not assign them to agent identities.

## 3. Vocabulary

| Term | Definition |
|------|------------|
| **property** | A named, typed piece of state. Per SPEC 07 §2.1: configuration, lifecycle, registered resources, derived state. The unit of read/write. |
| **flat-path property** | A property addressable by a SPEC 07 dotted path (`config.bind`, `lifecycle.uptime_s`). Used for singleton or bounded-tree state. |
| **namespace** | A managed subtree of a service's property tree owned by that service, declared by a `NamespaceSpec`. Examples: `accounts` (under maild), `themes` (under desktop), `peers` (under noded). Addresses collection records that cannot be expressed as SPEC 07 paths. |
| **key** | The identifier of a record within a namespace. For singleton namespaces, the key on the wire is the empty string; the substrate fills in the namespace's declared `canonical_key` (e.g. `"current"`) when echoing the key back in responses and audit / diagnostic output. For collection namespaces, the key is the record's primary identifier (e.g., email address). |
| **field_path** | An optional sub-selector inside a record. `["spam_enabled"]` selects one field; `["allowed_senders", 0]` selects a list element. |
| **record** | The struct value associated with a `<namespace>.<key>` pair. |
| **schema** | A machine-readable description of a namespace's record shape: field names, types, defaults, validation rules, secret-marking, help text. Returned by `<svc>.props.describe` (SPEC 07 §2.4 extended per §5.6 below). |
| **storage backend** | An implementation of the trait that persists records to disk. v1 ships MixData, Toml, SqliteTable, and Memory backends. |
| **owner** | The service whose process implements a namespace. The owner is the only writer; other peers reach the namespace through ABP, not by writing to its storage directly. |
| **version** | A per-record monotonic counter incremented on every successful `<svc>.props.set` or `<svc>.props.delete`. Used for optimistic concurrency. |
| **nseq** | A per-namespace monotonic event sequence number incremented by 1 on every successful write or delete. Used by `<svc>.props.watch` for gap-free replay. |
| **capability** | A named permission token a caller presents to invoke a verb. Capabilities are mapped from peer identity by the owning service's policy (§7). |

## 4. The property model

### 4.1 Address shape

Two address forms coexist on the wire:

- **Flat-path form (SPEC 07).** A single `path:` header carrying a
  dotted path under SPEC 07's grammar
  (`[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*`). Used for singleton and
  bounded-tree state: `path: config.bind`,
  `path: lifecycle.uptime_s`. This is what `<svc>.props.get` and
  `<svc>.props.describe` already accept per SPEC 07 §2.
- **Structured form (SPEC 12).** Three separate headers — `namespace:`,
  `key:`, `field_path:` — carried when addressing a record inside a
  registered namespace. The structured form is required for any
  namespace whose keys are not constrained to SPEC 07's path
  alphabet (the common case for collections — emails, UUIDs, file
  paths, IPs).

The components of the structured form:

- **`namespace`** — a registered namespace name. Regex
  `[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*`. The namespace name is the
  property-tree prefix the owning service has registered (e.g.
  `accounts` for maild's `<svc>.props.list namespace=accounts`).
  Carried in the ABP `namespace:` header.
- **`key`** — an arbitrary, opaque, UTF-8 string identifying the
  record. For singleton namespaces the key MUST be the empty
  string (the substrate fills it in on responses with the
  namespace's declared `canonical_key`). For collection
  namespaces the key value is the record's primary-key field. The
  substrate places no syntactic constraint on key bytes other than
  UTF-8 well-formedness and a 1 KiB length cap. Carried in the ABP
  `key:` header.
- **`field_path`** — an optional sub-selector inside a record,
  encoded as a JSON array of strings and integers (e.g.
  `["profile", "display_name"]` for a nested struct field,
  `["allowed_senders", 0]` for a list element). Carried in the ABP
  `field_path:` header when present; absent means "the whole
  record". The JSON-array form is unambiguous regardless of the
  bytes in any segment.

A verb receiving `path:` is in flat-path mode and consults the
service's bounded property tree. A verb receiving `namespace:`
(with optional `key:` / `field_path:`) is in structured mode and
consults the named namespace's registered records. The two modes
are mutually exclusive on a single request; a verb receiving both
returns `validation_error`.

Diagnostic and human-readable renderings (CLI output, log lines,
audit messages) MAY use a quoted dotted form like
`maild.accounts["user@alpha.amp"].spam_enabled`, but this form
is **presentation-only**; the wire never carries it. Implementations
MUST NOT parse such strings to recover `(namespace, key, field_path)`
— that information is always conveyed via the structured headers
above.

### 4.2 Cardinality

A namespace declares one of two cardinalities:

- **Singleton**: at most one record. The key is `""`. Examples:
  `theme` (current desktop theme), `feature_flags` (per-service
  flag bag). Bootstrap daemon identity is **not** a substrate
  namespace — see §15.6 for the SPEC 10 boundary.
- **Collection**: zero or more records keyed by primary identifier.
  Examples: `accounts` (email accounts under maild), `peers`
  (peered brokers under noded).

The cardinality is fixed at registration. A singleton cannot be
converted to a collection in place; that requires a new namespace
and a migration.

### 4.3 Schemas

Each namespace carries a schema. The schema is queryable at
runtime via `<svc>.props.describe` (§5.6) — SPEC 07's existing
describe verb, extended per §5.6 below with capability-gated
public / full views. The schema is the contract a GUI or web
frontend uses to render forms without hard-coding field lists.

A schema describes, for each field:

- Name and type. v0.1 types are: `bool`, `i64`, `u64`, `f64`,
  `string`, `bytes` (base64 on the wire), `path`, `email`, `url`,
  `enum<...>`, `list<T>`, `map<string, T>`, `struct{...}`, and
  `option<T>`. The type system is deliberately narrower than
  serde's; substrate types map onto a stable wire representation
  that GUIs and CLIs can render.

  `duration` and `timestamp` are explicitly **not** in the v0.1
  type set: their JSON wire encoding (RFC 3339 string vs integer
  seconds vs ISO 8601 with sub-second precision) needs to be pinned
  with a concrete in-tree user before it can be normative. Until
  v0.2 adds them, namespaces requiring time-shaped values use
  `i64` (epoch seconds) or `string` (RFC 3339) and declare the
  choice in their field's `help` text.
- Default value. Used when the namespace is first materialised or
  when a field is unset.
- `secret`: boolean. Carries the same meaning as SPEC 07 §2.4's
  `sensitive: true`; for the schema language the SPEC 12 name is
  `secret` (matches the existing `cosmix_props::redact` module
  in `cosmix-lib-props-core`)
  and SPEC 07's `sensitive` is the equivalent annotation in
  describe responses. Secret fields are redacted in
  `<svc>.props.list` and `<svc>.props.get` responses unless the
  caller holds `props.read:<svc>.<ns>:secrets`. Audit log entries
  for secret fields record only the HMAC digest of the value
  (§10), never the value itself.
- `validators`: zero or more named constraints (`min`, `max`,
  `regex`, `one_of`, `unique`). Validation runs server-side on
  every `<svc>.props.set`; clients MAY also pre-validate via
  `<svc>.props.validate` (§5.7).
- `help`: human-readable description. Surfaced in GUI forms and
  `--help` text.
- `since` / `until`: substrate-level version annotations enabling
  schema evolution; see §12.

Schemas are themselves serialised as a stable structure (the
`PropertySchema` wire shape; see §8.3). This shape **resolves SPEC
07 §6.4's open encoding decision** for the describe schema
language — the field set above is the v0.1 commitment, fronted by
`<svc>.props.describe`. GUIs MUST treat unknown schema fields as
forward-compatible extensions and MUST NOT fail to render a
namespace because a new schema field appeared.

### 4.4 Versioning and concurrency

Every record carries an integer `version` field, starting at 1 on
creation and incremented on every successful `<svc>.props.set` or
`<svc>.props.delete`. Writers MAY supply an `if_version=<N>`
header on `<svc>.props.set`; the owning service rejects the write
with `version_mismatch` if the current version is not `N`. This is
optimistic concurrency control — the substrate does not lock
records.

A writer that omits `if_version` performs a last-write-wins
update. This is the right default for human-driven edits (a single
operator on one terminal) and the wrong default for automation
that reads, modifies, and writes back. The substrate does not
enforce one mode over the other; the namespace's spec MAY declare
`require_version: true` to make `if_version` mandatory for that
namespace, in which case missing `if_version` is a
`validation_error`.

`version` is per-record. Cross-record consistency is not provided.

## 5–12. The normative surface (split into sub-chapters)

> **Re-chunked 2026-06-05.** The bulk of this chapter — the verbs, namespace
> registration, authorization, transport, errors, audit, recovery, and migration —
> was split into four sub-chapters so each can be amended independently. The model
> above (§1–4) and the framing below (§13–17) stay here. Section numbers are
> preserved in the sub-chapters, so an existing "SPEC 12 §N.x" cross-reference
> resolves to whichever sub-chapter now owns §N:

| Sub-chapter | Sections | Contents |
|---|---|---|
| [`2026-06-05-12a-property-verbs.md`](2026-06-05-12a-property-verbs.md) | §5 | The eight `props.*` verbs: `list` / `get` / `set` / `delete` / `watch` / `describe` / `validate` / `audit.watch` |
| [`2026-06-05-12b-namespace-registration.md`](2026-06-05-12b-namespace-registration.md) | §6 | `NamespaceSpec`, cardinality, schema derivation, storage backends, lifecycle hooks, the change/audit outbox |
| [`2026-06-05-12c-authz-transport-audit.md`](2026-06-05-12c-authz-transport-audit.md) | §7–§10 | Capability authorization, transport binding, error taxonomy, HMAC-chained audit trail |
| [`2026-06-05-12d-recovery-migration.md`](2026-06-05-12d-recovery-migration.md) | §11–§12 | Fail-closed bootstrap + recovery, schema/storage versioning + migration |

## 13. Conformance levels

SPEC 07 §9 defines L0..L3. SPEC 12 reproduces those rows
verbatim for cross-reference (the definitions remain owned by
SPEC 07) and adds two new levels, L4 and L5, on top:

| Level | Verbs implemented | Notes |
|-------|--------------------|-------|
| **L0** | `HELP`, `INFO`, `QUIT` (SPEC 02). | SPEC 07 §9. Baseline universals; no properties exposed. |
| **L1** | L0 + `<svc>.props.get`, `<svc>.props.list`, `<svc>.props.describe`. | SPEC 07 §9. Read-only property surface. |
| **L2** | L1 + `<svc>.props.watch`; emits `<svc>.props.changed` topic. | SPEC 07 §9. Watchable. |
| **L3** | L2 + retained `world.<svc>` snapshot topic. | SPEC 07 §9. Snapshottable. |
| **L4 — Mutable (SPEC 12)** | L3 + `<svc>.props.set`, `<svc>.props.delete`, `<svc>.props.validate`; emits `<svc>.props.records.changed` topic for any registered collection namespaces. | Full CRUD. Requires namespace registration per §6 for any structured-mode mutation. |
| **L5 — Auditable (SPEC 12)** | L4 + `<svc>.props.audit.watch`; emits `<svc>.props.audit` topic with HMAC digests per §10. | Required for any deployment with a security log aggregator or substrate doctor. |

L0..L3 definitions are owned by SPEC 07 §9; if those rows ever
drift between the two specs, SPEC 07 is authoritative. SPEC 12
owns only L4 and L5.

A namespace's highest level is declared in its `NamespaceSpec`;
the substrate library refuses to expose verbs the namespace did
not opt into. GUIs MUST handle L0..L3 namespaces gracefully
(read-only forms, no live updates beyond what SPEC 07 already
provides).

SPEC 07's `lifecycle.props_level` property reports the highest
level the service supports; namespaces may declare a lower level
than the service if their state is intentionally read-only.

## 14. Non-goals for v1 (deferred to later revisions)

The following are anticipated but deliberately omitted from v0.1:

- `<svc>.props.set_many` and `<svc>.props.delete_many` (batch
  writes).
- `<svc>.props.export` and `<svc>.props.import` (portable JSON
  dumps).
- Two-phase commit across namespaces.
- A typed Rust client SDK that wraps the verbs (the verbs are
  usable directly via `cosmix-lib-bus`; a typed wrapper is a
  polish item).
- Field-level encryption at rest (today, `secret` fields rely on
  filesystem permissions and audit-hashing; per-field key
  material is a future extension).
- A discovery verb listing all registered namespaces on a peer.
  Deferred until at least one consumer needs it; for now, the
  consumer either knows the namespace name or reads it from
  documentation.

Each is additive; v0.1 → v0.2 will not break L4/L5
implementations.

## 15. Relationship to existing primitives

This chapter unifies several existing patterns. The relationship
to each is normative — substrate adoption supersedes these
surfaces where they overlap:

### 15.1 SPEC 07 self-aware layer

SPEC 12 is positioned as an **amendment to SPEC 07** rather than
a parallel chapter. The boundary:

- SPEC 07 normatively owns the read surface: `<svc>.props.get`,
  `<svc>.props.list`, `<svc>.props.describe`, `<svc>.props.watch`,
  `<svc>.props.changed` topic, `world.<svc>` snapshot topic,
  `lifecycle.props_level` reporting, and the L0..L3 conformance
  ladder (extended to L4/L5 here).
- SPEC 12 adds the mutation, audit, capability, and managed-
  collection extensions on top: the new `set` / `delete` /
  `validate` / `audit.watch` verbs, the structured `(namespace,
  key, field_path)` headers for collection records, namespace
  registration via `NamespaceSpec`, capability gating, the
  audit-stream topic `<svc>.props.audit`, and the L4/L5
  conformance levels.
- The library boundary mirrors the spec boundary. Since 2026-05-29
  the substrate is a pair of crates: `cosmix-lib-props-core` carries
  the SPEC 07 read-surface modules (`amp` under feature, `describe`,
  `redact`, `tree`, `path`, `value`, `publish` under feature, `diff`);
  `cosmix-lib-props-store` carries the SPEC 12 surface, with modules
  (provisional names:
  `register`, `store`, `audit`, `hooks`, `outbox`, `dispatch`).
  There is no parallel `cosmix-lib-property` crate; v0.1.0's
  reference to one was an error that v0.1.1 corrects.
- SPEC 07 §6.4's open encoding decision for the describe schema
  language is resolved by SPEC 12 §4.3: the `PropertySchema`
  field set is the v0.1 commitment.
- SPEC 07 §10's "mutation through `props.set` is deferred to SPEC
  09" is partially resolved by SPEC 12: the **verb-level**
  mutation contract lives here (set / delete / validate with
  optimistic concurrency, hooks, capability checks, audit). The
  **trust-gradient policy** (which agents may invoke which
  mutations, prompt vs automatic) remains with SPEC 09, which
  composes on top of SPEC 12 by determining the capability set a
  given agent presents.

A namespace declared via SPEC 12 automatically satisfies SPEC 07
L1 (its records are listable, gettable, describable). Whether it
also satisfies L2 / L3 depends on its `conformance_level` and
whether the service emits `world.<svc>` snapshots covering
namespace state.

### 15.2 `cosmix-lib-config` per-service TOML

`cosmix-lib-config`'s current per-service TOML pattern persists
in two distinct roles after substrate adoption:

- **Bootstrap config** at `/etc/cosmix/<d>/config.toml`
  (root-owned, daemon read-only — SPEC 10 §3.3) remains the
  source of values the daemon needs before it can speak ABP.
  These are NOT substrate properties; they are exposed as
  read-only flat-path properties under SPEC 07 `config.*` but
  not mutable via `<svc>.props.set`.
- **Runtime settings** — values the daemon today reads from its
  `*Settings` struct after startup and never re-reads — become
  substrate properties stored under
  `/var/lib/cosmix/<d>/properties/` via the `Toml` or `MixData`
  backend (§6.4). The migration is per-namespace: a service may
  register `settings` (under itself) as a singleton namespace,
  derive the schema from its existing `*Settings` struct, and
  route `<svc>.props.get` / `<svc>.props.set` through the
  substrate library.

The split is sharp: a value lives in `/etc/cosmix/<d>/` if and
only if changing it requires `root` and a daemon restart.
Everything else lives in `/var/lib/cosmix/<d>/properties/` and is
mutated through the substrate.

After this split, the existing `load_service::<T>` /
`save_service::<T>` functions remain for the bootstrap-config
case (the daemon reads them at startup); their use for
runtime-mutable state is superseded by the substrate verbs.

### 15.3 maild `Account` and friends

maild's `account add` / `account list` / `account delete` CLI
verbs become thin wrappers over the substrate. The `accounts`
namespace (fully qualified `maild.accounts`) is registered as a
collection with `SqliteTable` storage, schema derived from the
`Account` struct, secret on `password_hash`.

The CLI subcommand stays — operators don't memorise ABP commands
— but its implementation is `mixctl props set maild accounts ...`
or equivalent. The CLI becomes a UX layer over the substrate,
not a parallel implementation. Other administrative surfaces (a
future desktop preferences panel, a web admin panel) reuse the
same namespace through the same verbs.

The `chmod a+rwX /var/lib/cosmix/maild` workaround applied
during Phase 7 smoke is reverted as soon as the substrate is
implemented for the `maild.accounts` namespace, since the CLI no
longer needs direct filesystem access to perform CRUD; it talks
ABP to the running maild. The managed-namespace contract from
§6.5 is what makes this safe: `account add` is the daemon
allocating an id, seeding mailboxes, materialising a spam
baseline, and then the substrate making the resulting record
visible — not the CLI hand-rolling that sequence against the
filesystem.

### 15.4 Display widget properties

The display backend's per-widget `value`, `selected`, `text`
properties are *not* property-substrate namespaces. They are
window-scoped, ephemeral, and bound to the display lifecycle —
the wrong granularity for the substrate, which is for
persistent service state.

However, display *settings* (theme, font size, default panel
geometry) are exactly the substrate's use case and SHOULD be
registered as namespaces (`theme`, `layout` under the display
service) once the display backend grows persistent preferences.

The line between "widget property" and "substrate property" is
sharp: widget properties live on a `ui.window`; substrate
properties live in a service's namespace. A property does not
straddle.

### 15.5 SPEC 03 topic pub/sub

The substrate's `<svc>.props.watch` and `<svc>.props.audit.watch`
are NOT thin wrappers over SPEC 03 `topic.subscribe` — see §5.5
and §5.8. The owning service is the subscription endpoint for
the capability-gated variants; topic pub/sub MAY be used as
transport underneath but is not the contract surface for those
verbs.

Two topic families are broker-reserved, because direct
subscribe to either would bypass an otherwise-required
capability check or a per-record visibility check:

1. `<svc>.props.audit` — audit-stream topic per §10.
2. `<svc>.props.records.changed` — structured collection event
   topic per §5.5.

For every topic name matching either pattern, the broker MUST:

- Refuse `topic.publish` from any peer other than the owning
  service of the relevant `<svc>` (the broker identifies the
  owning service via SPEC 02 service registration).
- Refuse `topic.subscribe` from any peer at all. Subscription
  for audit goes through `<svc>.props.audit.watch`; subscription
  for structured records goes through `<svc>.props.watch`. In
  both cases the owning service performs capability gating —
  per-namespace for `props.read:<svc>.<ns>` / `props.audit:
  <svc>.<ns>` — that the broker cannot express because the topic
  is per-service while authorisation is per-namespace.
- Refuse `topic.clear` from any peer other than the owning
  service.
- Exclude reserved-prefix topics from `topic.list` responses to
  every peer that is not the registered owning service for that
  prefix. This is a static, namespace-agnostic filter.
- Refuse `topic.subscriber_count` queries from non-owning peers;
  the subscriber count of either topic is itself operational
  information.

`<svc>.props.changed` is **not** broker-reserved: SPEC 07
already documents it as a directly-subscribable topic with
sensitive-field redaction in the event body. Capability-gated
watch with `since_nseq` replay goes through `<svc>.props.watch`;
unauthenticated topic subscribe remains available under SPEC
07's semantics. A namespace that wants its structured records
to be readable by unauthenticated peers declares so by granting
`props.read:<svc>.<ns>:public` in its `AuthPolicy`; that grant
makes the watch path succeed for unauthenticated callers without
opening the broker topic.

Other topic names are unaffected. The reservation is a small,
static rule additive to SPEC 03 and does not require SPEC 03's
forthcoming general topic-ACL story (still deferred per SPEC 03
§"Topic ACLs").

### 15.6 SPEC 10 daemon identity (out of scope for v0.1)

A daemon's bootstrap signing key and mesh credentials are not
substrate properties. SPEC 10 §3.3 keeps them under
`/etc/cosmix/<d>/` (root-owned, daemon-read-only, mounted
read-only into the daemon process); §6.4 of this chapter
restates that `/etc/cosmix/<d>/` is the bootstrap tree and the
substrate MUST NOT write it. Rotating those credentials is the
SPEC 10 install/upgrade procedure, not a `<svc>.props.set` call.

A future revision MAY route runtime-rotatable *derived* identity
material (short-lived session tokens, upstream API keys whose
storage location is `/var/lib/cosmix/<d>/`) through SPEC 12 once
the concrete need is identified, and only after SPEC 10 is
explicitly amended to delineate which material falls on each
side of the boundary. As of v0.1.1, no such routing exists, and
none is implied by the rest of this chapter.

## 16. Design rationale

A short list of the non-obvious choices, with the alternative
each was weighed against.

### 16.1 Extend `<svc>.props.*`, not invent `property.*`

v0.1.0 of this chapter introduced a flat `property.{list,get,set,
delete,subscribe,schema,validate}` verb family. That was wrong
on two counts: (a) SPEC 07 already declares `<svc>.props.*` as
the normative property-surface vocabulary, and the v0.1.0 family
would have been parallel and conflicting; (b) flat verbs hide
the owning service from the verb name, but every property
operation IS owned by a specific service — making `<svc>` part
of the verb is honest about that and lets the broker route by
verb name without inspecting headers.

The v0.1.1 alignment preserves all of v0.1.0's substance — the
managed-collection model, capability grammar, nseq replay,
transactional outbox, HMAC audit — while paying the SPEC 07
naming tax that should have been paid from the start.

### 16.2 Per-daemon storage, not a central config daemon

A central "configd" was considered. It would have given
mesh-wide single-source-of-truth for free. It was rejected
because:

- It is a new SPOF for a system that today survives single-
  daemon failure cleanly.
- Two-tier ownership (configd owns the bytes, the service owns
  the semantics) makes hook semantics murky — does configd or
  the service validate? Does configd or the service publish
  events?
- Cross-mesh deployment becomes much harder; each mesh would
  need its own configd and configd-to-configd replication.

The substrate as specified retains the property that each
namespace's owning service is the single source of truth. The
cost is that consumers route through the owning service. That
cost is small because ABP routing already does this for every
other message.

### 16.3 Optimistic concurrency (versions), not locks

Locks across ABP request boundaries are a deadlock factory: a
caller that disconnects while holding a lock strands the
namespace. Optimistic concurrency with `if_version` puts the
retry burden on the caller, which is the right place: only the
caller knows whether to retry, escalate, or surrender.
Last-write-wins remains available for the (common) human-edit
case where contention is not a concern.

### 16.4 Hooks split into before/after, not atomic both

Conflating pre-validation and post-side-effects in one hook
makes "did the side-effect run?" indistinguishable from "did the
record write?". The before/after split makes this question
trivially answerable: pre-hook failure ⇒ no write, no side
effect; post-hook failure ⇒ write happened, side effect
partially ran (logged). This matches the substrate's audit model
— the audit entry records the state of the record, not the
state of the side effects, because the latter is unbounded.

### 16.5 HMAC, not plain hash or value, in audit

Audit consumers (security log aggregators) need tamper-evident
change provenance, not data exfiltration. A plain hash leaks
low-entropy secret values to anyone who can enumerate likely
inputs; an HMAC keyed by a per-namespace audit key (§10)
prevents that brute-force attack while preserving the
tamper-detection property for callers who hold the key. The
audit body therefore contains the HMAC digest, never the raw
record bytes and never an unsalted hash.

### 16.6 Service-routed audit/records subscription, not direct topic subscribe

An earlier draft made `<svc>.props.audit.watch` a thin wrapper
over `topic.subscribe`, and v0.1.1 of this chapter made the
same mistake again for `<svc>.props.records.changed`. Both
designs required either (a) broker-resident per-namespace
allowlists synchronously published by every owning service, or
(b) the broker speaking the substrate's per-namespace
capability vocabulary on a per-service topic. Both contradict
SPEC 03's deliberate ACL-deferral posture and put the broker in
the policy-decision business.

The current design instead keeps both `<svc>.props.audit.watch`
and `<svc>.props.watch` as substrate verbs routed to the owning
service, which performs the per-namespace capability check and
`since_nseq` replay before transitioning the subscriber to live
delivery. The broker's contribution is the reserved-prefix rule
(§15.5) — a static, namespace-agnostic restriction on
`<svc>.props.audit` *and* `<svc>.props.records.changed`
topics. Implementations MAY still use topic pub/sub as transport
under the verb, but the contract surface is the substrate's.
`<svc>.props.changed` deliberately remains directly
subscribable per SPEC 07's established contract; capability-
gated watch with replay layers on top via `<svc>.props.watch`.

### 16.7 JSON wire body, not the Mix data format

The Mix data format (project memory:
`project_mix_as_substrate_data_format.md`) is the **storage** of
choice for substrate-internal bytes. The **wire** chooses JSON
because:

- Verb consumers may not be Mix programs (GUIs, web frontends,
  AI agents) and JSON is the universal interchange.
- ABP bodies are already textual; JSON keeps `cat | grep`
  ergonomic.
- Mix data on the wire would couple every consumer to the Mix
  parser, which is rightful storage scope but wrongful
  interchange scope.

Storage-format and wire-format are deliberately split: the same
record round-trips through JSON on the wire and Mix data on disk
without either format dictating the other.

## 17. Open questions

These are explicitly open in v0.1 and will be resolved in a v0.2
revision driven by adoption experience:

- Whether `<svc>.props.watch` should support filter predicates
  server-side (e.g., "only notify me of changes to records where
  `spam_enabled=true`"). The current spec emits all events;
  filtering is the subscriber's job. If GUIs find this
  expensive, v0.2 may add a `filter` header.
- Whether `<svc>.props.get` should accept a list of keys
  (multi-get). The current spec is one-key-per-call; tight loops
  issue N ABP round-trips. v0.2 may add a `keys` header.
- The exact serialisation of `duration` and `timestamp` in the
  JSON wire form. Candidates: ISO 8601 strings, integer seconds,
  RFC 3339. Pinned in v0.2 once a namespace actually uses them.
- How field-level encryption (out-of-scope for v0.1) interacts
  with hooks and audit. The cleanest design is "encrypt at the
  storage backend, never at the verb layer", but this needs a
  worked example before being normative.

---

*Originally drafted 2026-05-11 by Mark Constable and Claude as
v0.1.0 after the cosmix-maild Phase 7 deployment surfaced the
underlying tension: an operator running `cosmix-maild account
list` should not need `sudo -u`, world-writable
`/var/lib/cosmix/maild`, or any of the workarounds tried in the
preceding cooperation-loop iterations. The right answer is a
uniform property substrate by which every daemon exposes its
CRUD-shaped state to every operator surface (CLI, GUI, web,
agent) through one verb family with one authorisation story.*

*Revised to v0.1.1 on 2026-05-11 after a cross-codebase review
caught that v0.1.0 had drafted in isolation from SPEC 07 and the
existing `cosmix-lib-props` crate, inventing a parallel verb
family (`property.*`) and a parallel library
(`cosmix-lib-property`). v0.1.1 realigns the verb names to the
`<svc>.props.*` family SPEC 07 already established, positions
this chapter as an amendment to SPEC 07 that supplies the
mutation contract SPEC 07 §10 deferred, and identifies the
substrate library as the existing `cosmix-lib-props` extended.
All v0.1.0 substance — managed-collection model, capability
grammar, transactional outbox, nseq-based watch replay, HMAC
audit, soft-delete tombstones — survives the rename.*

---

## 18. Substrate data-format law

(Promoted 2026-07-23 from the substrate-mix-data-formats decision; full
rationale in git history. Shipped practice fleet-wide via the conf.mix
migration.)

**The seam is what reads the file, not what writes it:**

> **If the substrate reads it, it's Mix. If a third party reads it, it's
> whatever the third party expects.**

- Substrate-read structured artifacts (daemon config, specs-as-data,
  inventories, zone stores, job records) SHALL use Mix's literal-data
  subset (`*.mix` / `*.conf.mix`), parsed in **strict-data mode**.
- Strict-data mode SHALL accept only literal scalars, lists, and maps, and
  SHALL reject every executable construct — calls, `send`, `emit`,
  variables, interpolation. A data file must be inert: parsing it can never
  execute anything.
- Artifacts read by third parties (browsers, DNS resolvers, SMTP peers,
  external tooling, cargo/CI configs) stay in whatever format the third
  party expects; prose stays Markdown.
- This is a default for new artifacts; existing foreign-format files
  convert opportunistically when touched, never as a standalone migration.

**Extension taxonomy** (from the source decision; `.spec.mix` is live under
`_tasks/`, see its README):

| Artifact | Extension | Read by |
|---|---|---|
| Task specifications | `.spec.mix` | task/loop runners (`_tasks/`) |
| Component config | `.conf.mix` | the component, via `cosmix-lib-config` |
| Per-round journals | `.journal.mix` | loop runner, journal indexer |
| Reviewer verdicts | `.verdict.mix` | loop gate, judge agents |
| ABP call records | `.call.mix` | replay tooling, debugger |
| Component state snapshots | `.state.mix` | self-rebuild paths, audit |
| Mix scripts (executable) | `.mix` | the Mix interpreter |
