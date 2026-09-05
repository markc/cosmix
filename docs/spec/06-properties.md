---
title: Property model, validation and mutation
chapter: 6
version: 0.2.2
status: draft
date: 2026-09-05
---

# Property model, validation and mutation

This candidate describes baseline `96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be`.
**Verified-source** means inspected implementation, not newly executed tests.
**Intended** retains a requirement awaiting implementation evidence. **Conflict**
requires an explicit decision; neither implementation nor old prose silently wins.
The security/delivery exceptions in [the accepted compatibility amendment](compatibility-profile.md)
are now resolved in favour of the implemented profile; other conflicts remain open.
Chapter numbers are reading order, not changes to numeric `spec.get` identities.

## 1. Ownership and address model

**PROP-001 — Owner.** A property is typed, described state owned by one service.
Configuration, lifecycle, bounded resource summaries and durable derived state fit
this model. Per-frame rendering, ephemeral toolkit entity state and per-request
metrics do not. Persistent application preferences may use namespaces. An owner
mediates mutation; CLI, GUI, web and agent surfaces should invoke that same path.
Bootstrap configuration remains outside runtime mutation; see chapter 07.

**PROP-002 — Two address forms.** Flat paths name bounded property-tree leaves or
subtrees. Structured namespace/key addresses name collection records, including
keys containing dots or `@`. Do not recover structured addresses by splitting a
human-readable expression. `path` and `namespace` are mutually exclusive. Missing
flat `get` path means root; an empty string is not a valid `PropPath`.

**PROP-003 — Identifier invariants (verified-source).** `PropPath` has non-empty
dot-separated segments containing ASCII lowercase letters, digits or underscore;
digits and underscore may start a segment. `NamespaceName` instead requires an
ASCII lowercase first character in every segment. Both reject empty segments;
`PropPath` rejects wildcards (`*` is reserved; no wildcard-accepting path type
exists). Both constructors and Serde deserialisers enforce
their grammar, including through nested `PropDescribe` and `RecordKey` values.
This corrects old prose that assigned the namespace grammar to both types.

Evidence: [PropPath][path], [NamespaceName and registration types][namespace].

**PROP-004 — Keys.** Collection keys are opaque UTF-8, bounded to 1 KiB in the
record-key contract, and must agree with the declared primary-key field. Singleton
requests omit the key or use empty; responses use the registered canonical key.
Cardinality is fixed at registration. A singleton-to-collection conversion needs a
new namespace and explicit migration. Namespaced identity and field selectors are
distinct: the intended JSON-array `field_path` selector is currently refused by
the mutation router, even when empty. It is not a shipped partial-update API.

## 2. Flat property read surface

**PROP-005 — Read contract.** `<svc>.props.get` returns a tree or addressed value;
`list` enumerates leaves; `describe` accepts leaves and subtrees; `watch` belongs
to the event-capable level. Subtree descriptions enumerate direct children.
Descriptions carry `path`, `type`, `mutable`, `sensitive`, `description`, with
optional `format`, `enum`, `min`, `max`, `default`, `since`, `deprecated`,
`transient` and `children`. The minimal schema is not full JSON Schema.

**PROP-006 — Encoding.** Property wire bodies use JSON within Bus messages.
Storage encoding is independent. A schema reader tolerates unknown optional
fields. The earlier choices between JSON and child messages, dotted paths and
JSON Pointer, and delta versus snapshot are resolved for this profile as JSON,
dotted paths and separate delta/snapshot surfaces.

## 3. Registration and schemas

**PROP-007 — Registration (verified-source).** Core owns `PropTree`, `PropPath`,
`PropValue`, `PropDescribe`, redaction and diff; its Bus integration is opt-in.
Store owns namespace policy, storage, hooks, audit, runtime and mutation routing.
There is no separate `cosmix-lib-property` implementation. Registration uses the
actual store/runtime/router APIs, not the old illustrative global `register()`.

`NamespaceSpec` carries name, schema, cardinality, storage kind, authorisation,
hooks, validators, delete mode, tombstone TTL, replay window, subscribe payload,
schema visibility, lifecycle, external-edit policy, conformance level, introduction
version and `require_version`. A backend-kind enum does not prove that backend
exists. Default authorisation is deny-all; `schema_public: Allow` does not itself
grant the public-describe capability.

**PROP-008 — Schema contract.** Fields describe name/type, default, `secret`,
validator descriptions, help, introduction/deprecation version, and validator
secrecy. Types include bool, signed/unsigned integer, float, string, bytes, path,
email, URL, enum, list, map, struct and option. Duration/timestamp encodings remain
unsettled; owners specify an integer epoch or RFC3339 string explicitly. A derive
macro remains proposed. Schema metadata is not proof that every predicate runs:
the runtime executes registered pure validator functions on supplied input before
hooks. Generic automatic enforcement of every advertised schema constraint needs
its own evidence. Stored values remain sparse; chapter 07 owns amendment defaults.

## 4. Structured verb profile

The [mutation router][router] separates structured requests from the core flat
surface. All entries below are **verified-source** unless labelled otherwise.

| Verb suffix | Accepted operation | Material boundary |
|---|---|---|
| `get` | Namespace/key; returns namespace, key, version, snapshot nseq, fields | Field-selection body and subpaths refused |
| `list` | Namespace; returns records, snapshot nseq, `next_cursor: null` | Pagination, filters and projections refused |
| `set` | Namespace/key, object body, `if_version`, `merge` | Structured whole-record update only; merge defaults true |
| `delete` | Collection namespace/key, optional/required `if_version` | Singleton deletion refused |
| `describe` | Namespace, `view: public` or `full` | Separate capabilities; public secret fields omitted |
| `watch` | Namespace and optional `since_nseq` | Authorised live grant precedes replay snapshot |
| `audit.watch` | Namespace and optional `since_nseq` | Separate audit capability; no caught-up marker |
| `validate` | Intended dry-run set | Absent from router dispatch; unknown subcommand error |

**PROP-009 — Intended extensions and conflict.** The old verb contract promises
cursor-stable list snapshots, limit 1..1000/default 100, AND-combined exact/regex/
in/gt/lt filters, JSON `fields` projections and JSON-array `field_path` partial
selection. Preserve these as unmet requirements pending a scope decision; do not
send them to the current router. Their eventual implementation must validate
predicate types and authorise secret selection. A typed SDK, multi-get, batches,
namespace discovery and import/export remain separate proposed extensions.

**PROP-010 — Versions.** Record versions begin at 1; writes and deletes advance
them. `if_version` checks occur at commit; mismatch returns actual current version.
Omitting it is last-write-wins unless namespace `require_version` requires it.
Automation performing read-modify-write should use it. A retry after a committed
write may return mismatch; that is duplicate prevention, not replaying a successful
response. No cross-record or cross-namespace transaction is promised.

**PROP-011 — Delete.** Soft delete retains key/version tombstones; recreate
continues the sequence while the tombstone exists. Reads treat tombstones as
not-found. Hard delete forgets incarnation/version; recreate starts at 1 and
therefore permits ABA ambiguity for stale writers. `require_version` and hard
delete are incompatible. Default TTL intent is seven days; baseline SQLite does
not expire tombstones, so do not advertise that retention bound as enforced.

**PROP-012 — Dry-run requirement (intended).** `validate` must require write
capability, run pure schema/custom validators without hooks, writes or events,
and report field failures. Success proves only validation against that snapshot,
not that a later commit or side-effectful hook will succeed. The earlier phrase
“write would succeed” is too strong. Revalidate on actual mutation.

## 5. Capabilities and errors

**PROP-013 — Authority.** `AuthPolicy` maps transport-established `PeerIdentity`
to capabilities synchronously once per request; policies needing substrate state
use a separately maintained cache, not blocking Bus calls inside resolution.
Unknown identity yielding no capability remains intended policy, not a universal
implemented guarantee: the default policy denies all, but an owner-supplied
policy can grant unidentified peers. User-supplied identity headers do not prove
identity. Unix credentials, verified node claims and registered-service identity
must retain provenance; full transport coverage is not established by these types.

Conventional tokens are `props.<action>:<service>.<namespace>[:<scope>]`; actions
are read/write/describe/audit. The library compares exact strings; it does not
generically interpret `self`, `public` or wildcard scope semantics. In particular
`read:...:public` alone is not the base read token checked by the inspected router.
At reconciled props-store 0.3.0, Capability rejects empty strings and Actor checks
its supported forms at construction and nested Serde boundaries; see
[shared types](03-shared-types.md). Neither proves provenance or permission.
The [current mutation router](https://github.com/markc/cosmix/blob/4d2f1ebb77af51d8bbd08cb18f4e7070cebb58ac/src/crates/cosmix-lib-props-store/src/bus/mutation.rs)
checks capabilities, then derives set/delete attribution from service_name,
otherwise signed_ident as operator, otherwise Unix UID as operator, otherwise
the owning service. Invalid attribution returns `validation_error` before writing.
The owner fallback is not evidence that the caller was authenticated as that owner.

**PROP-014 — Secret views (accepted compatibility profile).** Get/list include secrets when the
caller holds both base read and secrets capabilities; otherwise redact them. Old
requirements for explicit secret field selection and privileged default redaction
are superseded for this profile. There is no additional per-request reveal opt-in:
a privileged whole-record read can return secret values. Consumers must protect
the resulting payload from logs, caches and display leakage. Public describe
omits secret fields and redacts secret validators; full describe needs its own
capability. Sensitive flat-tree reveal likewise needs an explicit owner policy;
the old WG-subnet-only policy is not proof of a universal deployed enforcement.

**PROP-015 — Error contract.** Error bodies carry `error_code`, message and optional
diagnostics; consumers ignore unknown diagnostic fields. Operational rc 10 covers
`auth_denied`, `not_found`, `validation_error`, `conflict`, `version_mismatch`,
`replay_window_exceeded`; rc 20 covers `storage_error`, `hook_error`, `unavailable`.
The baseline also returns rc 10 `grant_failed` when live subscription installation
fails; the old closed taxonomy omits this token, a further contract drift requiring
recorded amendment. No replay-only success is returned on that grant failure.
Since the 0.3.0 store, malformed peer attribution is refused with rc 10
`validation_error` before any write, while a persisted event row whose actor
string fails the validating grammar (a pre-0.3.0 or hand-edited row) fails
decoding on the read path — replay, watch and audit surfaces return rc 20
`storage_error` for it, and event dispatch for that namespace stalls at the
offending row until repaired (see STORE-010 in chapter 07).
Only an explicit contract amendment adds taxonomy tokens. Flat mutation and
unsupported selectors must fail visibly rather than silently changing meaning.

## 6. Changes, snapshots and replay

**PROP-016 — Distinct event families.** Flat `<svc>.props.changed` contains path,
old, new, timestamp and optional cause. Structured `<svc>.props.records.changed`
contains namespace/key, kind, verb, nseq, version, audit epoch, actor, timestamp,
changed non-secret field names and secret-fields-changed count; completion may
carry lifecycle/reason. Kinds are created/updated/deleted/completed/reconciled.
Never send structured events on the flat topic. Sensitive flat changes omit value
bytes; structured events must not expose secret field names or values.

**PROP-017 — Flat observation (intended per-owner obligation).** L2 owners emit
changes for listed non-transient paths. Coalesce to final per-request path values;
bulk operations may emit a summary. Ten Hz per path is a soft steady-state ceiling.
L3 publishes retained `world.<svc>` snapshots, normally coalesced to at most 1 Hz.
Snapshots are last-known state; stale producer metadata and orphan expiry belong
to the broker. Gaps require a fresh read; never synthesise missing deltas. Prefer
the originating cause ID when known, while treating optional causal links as hints.

**PROP-018 — Protected subscriptions.** Owners authorise structured watch/audit
before granting live delivery. Direct subscribe to either reserved topic is
forbidden, including wildcard bypass; publish/clear/list/count must honour owner
restrictions. Flat changed remains directly subscribable with safe payloads.
Current live grants use a broker bridge; absence of a production granter must not
be represented as successful live watching.

**PROP-019 — Replay boundary.** Read snapshots report namespace nseq. Watch with
`since_nseq=N` returns retained events newer than N, followed logically by exactly
one `caught_up` boundary at server high-water H, then live events. Current Bus
framing is one response with homogeneous `events`, a separate top-level
`caught_up: {event_type, namespace, nseq}` and `live`; subsequent records use the
grant. Drop duplicate live
events at or below H when constructing the logical stream. Same-marker redelivery
is harmless; a different second marker requires reseeding. Audit watch has no
marker. Flat and whole-surface streams have no namespace watermark guarantee.

**Unresolved cursorless discrepancy:** the retained intent is live-tail-only join
with no historical replay when `since_nseq` is absent. At both cited revisions,
`parse_since_nseq(None)` supplies zero and the router calls `events_since` with it.
Cursorless watch can therefore return retained history or `replay_window_exceeded`;
its marker is not proof of a history-free join. This discrepancy is not covered
by the accepted best-effort live-delivery amendment and needs its own disposition.

**PROP-020 — Best-effort live delivery.** This profile does not promise contiguous
live nseq, at-least-once delivery or per-subscriber durable watermarks. The
dispatcher starts at current nseq and advances even after publish failure:
**live delivery is best-effort**, with retained-history replay for recovery.
The stronger old live-delivery guarantees are superseded, not implementation
obligations for this profile. `caught_up` does not repair future loss. Consumers deduplicate by namespace/key/
nseq, detect gaps, and re-list on `replay_window_exceeded`. Default history intent
is min(1024 events, one hour); no unbounded offline replay is promised.
If the last notification is lost and no later event arrives, sequence-gap detection
alone cannot reveal that loss. Applications needing current state must explicitly
reconcile; applications needing durable workflow delivery need a separate mechanism.

## 7. Activity, conformance and evolution

**PROP-021 — Activity contract (intended; fleet coverage unverified).** Discrete
actions use activity events rather than fictitious property transitions. Required
body fields are actor, verb and timestamp; optional cause, outcome (`ok`, `error`,
`refused`), duration and details. Per-service or shared-family topics declare their
schemas and ownership; co-publishers must agree. Announce topics through
`lifecycle.activity_topics`. Batch high-rate activities; omit sensitive inputs
instead of inlining them. A plain hash does not make low-entropy secrets safe.

Actor variants remain daemon process (`service[:uuid]`), agent session
(`runtime:uuid[:call_seq]`), operator (`operator:principal`), and explicit internal
completion/reconciliation actors. Instance UUIDs are UUIDv7, not PIDs. Agent/operator
events name their mediating service in `details.via`; daemon process events omit
it. Tool invocation details include provider, params_hash, authority and dry_run;
proposal/repair families are owned by their respective contracts. Runtime activity
and commit trailers complement each other, not substitute for each other.

For agent sessions, the runtime token is the short, stable runtime name, distinct
from the mediating daemon's filesystem name; a new token requires a spec amendment.
Mint the UUIDv7 once at process start and retain it across calls. `call_seq` is
monotonically increasing per instance, included for call-scoped work (such as MCP)
and omitted for connection-scoped work (such as agentd). These retained emitter
obligations are not enforced by Actor's lexical parser.

**PROP-022 — Conformance is evidence.** L0 universals; L1 get/list/describe; L2
watch+changes; L3 retained world snapshot; L4 mutation+validate; L5 audit. Report
service and namespace levels separately. A declared enum cannot establish L4 when
validate is missing, nor L3 without a running snapshot producer. Existing commands
remain compatible during staged adoption; new structured daemons should design
observation alongside their state. Global adoption order is roadmap material.

**PROP-023 — Evolution.** Additive descriptions require tolerant readers. Path
renames retain a deprecated alias for one release cycle; breaking flat type or
semantic changes require an explicit service-version compatibility decision.
Namespace migration follows chapter 07. Cross-node world aggregation, generic
activity retention, a service-wide schema registry and dynamic watch predicates
remain proposed, not implied features.

## Evidence and validation

Baseline sources: [core path][path], [namespace/schema][namespace],
[mutation router][router], [runtime][runtime], [dispatcher][dispatcher].
Acceptance requires focused constructor/nested-Serde tests; both feature profiles
of core; store default, SQLite and Bus tests; owner integration tests for policy,
replay/grants and failure cases; then relevant workspace gates. No test was run
for this documentary audit. Fleet claims require per-owner runtime evidence.

[path]: https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-lib-props-core/src/path.rs
[namespace]: https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-lib-props-store/src/namespace.rs
[router]: https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-lib-props-store/src/bus/mutation.rs
[runtime]: https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-lib-props-store/src/runtime.rs
[dispatcher]: https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-lib-props-store/src/dispatcher.rs
