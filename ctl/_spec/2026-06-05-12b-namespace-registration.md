---
title: Cosmix Property Substrate — Namespace Registration and Schema
chapter: 12b
version: 0.2.2
status: draft
date: 2026-06-05
amends: _spec/2026-06-05-07b-property-surface.md (supplies the registration + schema-derivation surface behind the read model)
companion: _spec/2026-05-11-12-property-substrate.md
---

# Cosmix Property Substrate — Namespace Registration and Schema

> **Split out of SPEC 12 §6 (2026-06-05).** How a service declares a namespace:
> the `NamespaceSpec`, cardinality (singleton vs collection), schema derivation,
> storage backends, lifecycle hooks, and the change/audit outbox. The
> `NamespaceSpec` registration path is normative + code-backed
> (`cosmix-lib-props-store`); the Toml / MixData storage backends and the
> schema-derive macro are **unbuilt**. Section numbers are preserved as **§6.x**
> so cross-references resolve here.

## 6. Namespace registration

A service registers a namespace by calling
`cosmix_props::register(NamespaceSpec)` during startup. The
substrate library — since 2026-05-29 the split pair
`cosmix-lib-props-core` (which implements SPEC 07's
`<svc>.props.{get,list,describe}` and the `<svc>.props.changed`
publish path under its `amp` feature) plus `cosmix-lib-props-store`
(which owns the SPEC 12 surface: `register`, `store`, `audit`,
`hooks`, `outbox`, `dispatch`) — provides the registration entry
point. The spec is the source of truth for everything the substrate
needs to know about a namespace.

### 6.1 `NamespaceSpec`

```rust
pub struct NamespaceSpec {
    /// Namespace path under the owning service, e.g. "accounts".
    /// The fully qualified name across services is `<svc>.<name>`,
    /// which is what capability strings carry.
    pub name: &'static str,

    /// Record schema. Defines field names, types, defaults,
    /// validation, secrecy. Hand-written in v0.1; a
    /// `#[derive(Property)]` companion is proposed for v0.2
    /// (see §6.3).
    pub schema: PropertySchema,

    /// Singleton or Collection.
    pub cardinality: Cardinality,

    /// Persistence backend. See §6.4.
    pub storage: StorageBackend,

    /// Authorization policy. See §7.
    pub auth: AuthPolicy,

    /// Hooks fired before / after writes. See §6.5.
    pub hooks: Hooks,

    /// Pure validators (distinct from hooks). Run in
    /// `<svc>.props.validate` and at the start of
    /// `<svc>.props.set`. See §5.7.
    pub validators: Vec<ValidatorFn>,

    /// `SoftDelete` or `HardDelete`. See §5.4.
    pub delete_mode: DeleteMode,

    /// Tombstone retention for `SoftDelete` namespaces.
    /// Default: 7 days. Ignored for `HardDelete`.
    pub tombstone_ttl: Duration,

    /// Event-history retention bound for `since_nseq` replay
    /// (§5.5, §6.6). Default: min(1024 entries, 1 hour).
    pub replay_window: ReplayWindow,

    /// Whether `<svc>.props.records.changed` bodies carry the
    /// new record inline (`Full`) or are empty pointers
    /// requiring a follow-up `<svc>.props.get` (`Pointer`, the
    /// default). MUST be `Pointer` if any field is `secret`.
    /// Has no effect on the SPEC 07 flat-path
    /// `<svc>.props.changed` topic, whose body shape is fixed
    /// by SPEC 07 §3.1.
    pub subscribe_payload: SubscribePayload,

    /// `Allow` (the default) lets unauthenticated callers fetch
    /// the redacted schema via `view: public`. `Deny` returns
    /// `auth_denied` on the public view. See §5.6.
    pub schema_public: SchemaPublic,

    /// `Simple` (default) for plain CRUD records; `Saga` for
    /// managed-namespace records whose creation spans
    /// multiple services and needs the substrate-managed
    /// `_lifecycle` field + library-internal complete
    /// transition. See §6.5. Maild `accounts` is `Saga`; maild
    /// `themes` is `Simple`.
    pub lifecycle: NamespaceLifecycle,

    /// Reconciliation policy when a daemon-down hand edit to
    /// a backing storage file is detected on startup (§11).
    /// Default `ReconcileAndContinue` emits a synthetic
    /// `<svc>.props.reconcile` event and bumps the
    /// namespace's `audit_epoch`. `RefuseStartup` is for
    /// namespaces whose audit continuity is load-bearing
    /// (billing, payments) — the daemon refuses to serve
    /// until an operator runs the explicit reconciliation
    /// path.
    pub external_edit_policy: ExternalEditPolicy,

    /// L0..L5, the highest conformance level this namespace
    /// implements (SPEC 07 §9 owns L0..L3; SPEC 12 §13 owns
    /// L4/L5). The substrate library refuses to dispatch verbs
    /// above the declared level — e.g. a namespace declared L3
    /// cannot serve `<svc>.props.set` even if the host service
    /// reports `lifecycle.props_level = L4`. Conversely a
    /// namespace MAY declare a level lower than its host
    /// service to publish itself as intentionally read-only.
    pub conformance_level: ConformanceLevel,

    /// Substrate version this namespace was introduced in.
    /// Used for schema migration (§12).
    pub since_version: SchemaVersion,
}
```

A namespace registered with only `name`, `schema`, `cardinality`,
and `storage` adopts the substrate library defaults for every
other field; these defaults are the right starting point for a new
namespace and exactly match the behaviour described in this
chapter.

### 6.2 Cardinality declaration

```rust
pub enum Cardinality {
    Singleton { canonical_key: &'static str },
    Collection { primary_key_field: &'static str },
}
```

For collections, the `primary_key_field` names the schema field
that serves as the record's key. The substrate enforces uniqueness
on that field and exposes its value as the `key` in all verb
responses.

### 6.3 Schema derivation (proposed, v0.2)

The v0.1 commitment is hand-written `NamespaceSpec` / field
declarations using the types in `cosmix_props::describe`. No
derive macro ships with v0.1 — adopters write the spec by hand
(see §6.5 for the maild `accounts` worked example).

A **proposed v0.2 deliverable** is a companion proc-macro crate
`cosmix-lib-props-derive` (not yet in the workspace) that would
let a record struct derive its schema directly:

```rust
// Proposed for v0.2 — not implemented in v0.1.
#[derive(Property)]
#[property(namespace = "accounts")]
pub struct Account {
    #[property(primary_key)]
    pub email: String,

    #[property(secret)]
    pub password_hash: String,

    #[property(default = "true")]
    pub spam_enabled: bool,

    #[property(validate = "regex:^[a-z0-9_-]+$")]
    pub username: String,
}
```

The intent is for the derive to emit the matching `NamespaceSpec`
declaration and a serde-compatible `PropertySchema` at compile
time, with no runtime code beyond the schema constant — storage,
validation, and authorization remain runtime concerns delegated
to the substrate library. Promotion from proposed to committed
follows the spec amendment process once the v0.1 hand-written
form has been exercised by at least one adopter (likely
maild.accounts) and the ergonomic gap is concrete.

### 6.4 Storage backends

v1 ships four storage backends. A namespace picks exactly one;
mixing backends within a namespace is not supported.

All writable substrate state lives under the owning daemon's
state directory `/var/lib/cosmix/<d>/properties/`, which is
daemon-owned (`cosmix-<d>:cosmix-<d> 0750`) per SPEC 10 §3.2. The
substrate MUST NOT write to `/etc/cosmix/<d>/`: that tree is
package-managed bootstrap configuration owned by `root`, mounted
read-only to the daemon process by SPEC 10 §3.3, and editing it
through the substrate would require either a privilege escalation
or a parallel privileged config-writer — both of which contradict
the SPEC 10 contract that daemons cannot mutate their own
`/etc/cosmix/<d>/`.

| Backend | Use case | File layout |
|---------|----------|-------------|
| `MixData` | Substrate-internal records that benefit from the Mix data-format affordances (per project memory on Mix-as-substrate-data-format). | `/var/lib/cosmix/<d>/properties/<namespace>.mix` |
| `Toml` | Records the operator might inspect or hand-edit with a text editor for diagnostics. | `/var/lib/cosmix/<d>/properties/<namespace>.toml` |
| `SqliteTable` | Collection namespaces with substantial record counts or query needs (e.g. maild's `Account`, `Mailbox`). | A table inside the daemon's SQLite database under `/var/lib/cosmix/<d>/`; the backend manages migrations. |
| `Memory` | Ephemeral state that should not survive a restart (e.g. session tokens, transient peer metadata). | Process memory only. |

`/etc/cosmix/<d>/config.toml` remains the canonical *bootstrap*
configuration as defined in SPEC 10 §3.3 — values the daemon
needs to read before it can serve ABP traffic at all (listen
addresses, TLS material, storage roots, identity-key paths).
These are not substrate properties; changing them requires `root`
and a daemon restart. The substrate is for state the running
daemon owns and can mutate while serving — accounts, runtime
tuning, feature flags, desktop themes, peer membership.

Storage backends implement a small trait (`PropertyStore`) with
`list`, `get`, `set`, `delete`, plus the transactional outbox
hooks described in §6.6. The `Toml` and `MixData` backends
rewrite the whole file on every write; this is correct for the
scale of human-readable storage and avoids ad-hoc partial-write
parsing.

### 6.5 Managed-namespace contract: hooks and side effects

A namespace mutation is not a row insert. The owning daemon owns
the *operation*, not just the record bytes. SPEC 12's contract is
that the substrate library threads versioning, audit, capability,
and outbox around the daemon's operation — it does not replace
the daemon's logic.

Concretely: creating a maild account is not "write a row into the
accounts table." It allocates an account id, seeds default JMAP
mailboxes via the MDS adapter, materialises a spam baseline, and
optionally provisions DKIM material — work that spans multiple
services and cannot honestly participate in one storage
transaction. The substrate models this as a *saga*, not a write:
the record is created in a substrate-managed `provisioning`
state, side effects run in `after_set`, and the record then
transitions to `active` (or `failed`) — at which point the
external view of "the account exists" becomes true.

```rust
pub struct Hooks {
    pub before_set: Option<BeforeSetFn>,
    pub after_set:  Option<AfterSetFn>,
    pub before_delete: Option<BeforeDeleteFn>,
    pub after_delete:  Option<AfterDeleteFn>,
}
```

Managed namespaces (declared via `NamespaceSpec.lifecycle =
Saga` rather than the default `Simple`) gain a substrate-reserved
field `_lifecycle: Lifecycle` on every record:

```rust
pub enum Lifecycle {
    Provisioning,           // after_set in flight
    Active,                 // after_set succeeded
    Failed { reason: String }, // after_set returned an error
}
```

The field is owned by the substrate library: daemons MUST NOT
write it directly through `<svc>.props.set`, and `validate`
rejects callers that try. The `provisioning → active` and
`provisioning → failed` transitions are **not** wire verbs;
they are internal library operations performed atomically as
part of returning from `after_set`. The substrate library
inspects `after_set`'s return value (`Ok(())` vs
`Err(reason)`), commits the lifecycle flip as a new event row
in the namespace's event history (verb
`<svc>.props.complete`, see §6.6) under the same `key`,
allocates a fresh `nseq` and bumps `version`, and dispatches
the resulting record event onto `<svc>.props.records.changed`
just like a set. Wire callers therefore never invoke a
`props.complete` verb directly — the audit stream and the
records.changed topic carry the complete event, and subscribers
observe the lifecycle transition there. `validate` schema-side
hides `_lifecycle` from `view: public` describe by default.

`before_*` hooks may reject a write by returning a
`validation_error` or `hook_error`. `after_*` hooks fire after
the storage backend has committed and the event has been recorded
in the namespace's event history (§6.6); their return value
governs the lifecycle transition. The event-history row is
durable at this point even though network fan-out is still in
flight.

The ordering and atomicity guarantee — explicit because §6.6
makes network delivery asynchronous:

1. `before_set` runs first. On failure the verb returns
   `validation_error` or `hook_error` and nothing else has run.
2. On `before_set` success, the storage backend commits the
   record write and the matching event-history row in one
   transaction (§6.6). For `Saga` namespaces, the committed
   record carries `_lifecycle: Provisioning`. If commit fails,
   no further hooks run, no event row exists, and the verb
   returns `storage_error`.
3. On commit success, the verb response is built and the verb is
   considered "done" from the caller's perspective. The new
   record is durable, `nseq` is allocated, and (for `Saga`
   namespaces) the record is visible as `Provisioning`.
4. `after_set` runs synchronously, on the same task as the verb,
   *after* commit but *before* the response is sent to the
   caller. For `Simple` namespaces, `after_set` failures are
   logged and appended to the response body as a `warnings`
   array; they do not change the response's success status. For
   `Saga` namespaces, the substrate library performs an
   internal lifecycle-transition step on `after_set` return —
   `Ok(())` transitions the record to `Active`, `Err(reason)`
   transitions it to `Failed { reason }` — by appending a fresh
   event-history row with verb `<svc>.props.complete` (the
   verb name is an audit/event-stream identifier, **not a
   wire-callable verb**) and updating the on-disk
   `_lifecycle` field in a single backend transaction. The
   verb response then carries the resulting `lifecycle` value
   (plus a `reason` string on `Failed`). The original set
   commit and event row are not rolled back; the audit log
   captures the full saga (set + complete) as two consecutive
   event rows under the same key.
5. Independent of `after_set`, the event-history dispatcher
   (§6.6) picks up the new rows and fans them out to current
   subscribers asynchronously. Subscribers observe the record's
   lifecycle as a sequence: a `Set` event with
   `_lifecycle: Provisioning`, then a `Complete` event with
   `_lifecycle: Active` (or `Failed`). Watch consumers that want
   "only externally-usable records" filter on
   `_lifecycle == Active`.

`Simple` namespaces have no `_lifecycle` field and behave as a
plain CRUD record. `Saga` is opt-in per namespace, paid for by
the daemon when its operation genuinely spans services. Maild
`accounts` is `Saga`; maild `themes` is `Simple`.

Recovery: if the daemon crashes between step 3 (commit) and the
library-internal complete transition, the record stays as
`Provisioning` on disk. On daemon restart, the substrate
library performs startup work in a fixed order:

1. **Hand-edit reconciliation runs first** (§11). For each
   namespace, the library compares each record file's content
   hash against the sidecar's last-known hash and, on
   divergence, applies the namespace's `ExternalEditPolicy`:
   `ReconcileAndContinue` emits a synthetic
   `<svc>.props.reconcile` event and proceeds with the
   reconciled on-disk record as the new ground truth;
   `RefuseStartup` blocks all further startup work — including
   saga replay below — until an operator runs the explicit
   reconcile path.
2. **Saga replay runs second**, against the *post-reconcile*
   state. The library walks the namespace for records still
   carrying `_lifecycle: Provisioning` whose `nseq` is older
   than the most recent `Complete` event for the same key (or
   which have no `Complete` event at all) and re-invokes
   `after_set` with the post-reconcile record as input. Daemons
   MUST therefore make their `after_set` idempotent. If replay
   also fails, the record is transitioned to `Failed` with
   reason `provisioning_crash_replay_exhausted` and the
   operator is expected to delete and recreate.

This ordering matters: replaying a saga against a stale
pre-edit record would re-drive side effects using bytes the
operator has already replaced, defeating the point of the
hand-edit escape hatch.

The wider point: a "PUT this record" framing is too naive for
real daemons. The managed-namespace contract preserves the daemon
owning the operation — substrate-managed means
substrate-coordinated, not substrate-mechanical. The substrate's
contribution to that coordination is the lifecycle field and the
saga replay path, not a fake atomic transaction across services.

### 6.6 Event delivery: transactional outbox

The substrate guarantees that every committed state transition
produces exactly one event in the namespace's sequence (`nseq`),
durably ordered with respect to the storage commit. For
`Simple` namespaces and for `<svc>.props.delete`, one verb call
produces one transition (and therefore one event). For `Saga`
namespaces, one `<svc>.props.set` call produces **two**
transitions and therefore two events: the initial set (committed
record carries `_lifecycle: Provisioning`, event verb
`<svc>.props.set`, `kind: created` or `updated`), then the
library-internal lifecycle flip on `after_set` return (event
verb `<svc>.props.complete`, `kind: completed`). Both rows are
in the same nseq sequence, are immutable, and chain under the
same `audit_epoch`. The synthetic startup reconciliation pass
(§11) similarly produces one transition per divergent record,
event verb `<svc>.props.reconcile`. Event *delivery* to
subscribers is at-least-once after commit. The contract is
implemented via a transactional outbox in each storage backend:

- The record write and the matching event-history row (carrying
  `nseq`, `key`, `kind`, `verb` (one of
  `<svc>.props.set` | `<svc>.props.delete` |
  `<svc>.props.complete` (saga lifecycle transition, §6.5) |
  `<svc>.props.reconcile` (hand-edit reconciliation, §11)),
  `version`, `actor`, `audit_epoch` (per-namespace generation
  counter, §11; bumped only by reconcile events, attached to
  every event for chain verification), `at` (wall-clock
  timestamp, RFC 3339, captured at commit time),
  `fields_changed`, and `audit_digest` (§10 HMAC computed over
  the post-commit record state, or over a delete-tombstone
  marker for `<svc>.props.delete`, or over the
  `provisioning → active|failed` transition for
  `<svc>.props.complete`, or over the synthetic
  pre/post-edit pair for `<svc>.props.reconcile`)) are committed
  in the same backend transaction. The event row is **immutable** once written;
  subsequent changes to the same key produce new rows with higher
  `nseq`, never edits in place. This is what lets
  `<svc>.props.audit.watch` replay faithfully reproduce
  historical audit events even after the underlying record has
  changed or been deleted. For the `Toml` and `MixData` backends,
  the event history is a small sidecar file rewritten atomically
  alongside the record file; for `SqliteTable`, both writes happen
  inside one SQL transaction; for `Memory`, the history is an
  in-process ring.
- Rows are retained for the namespace's `replay_window`
  (default 1024 entries or 1 hour, whichever is smaller; §5.5);
  trimmed rows beyond that are eligible for deletion. Retention
  is therefore the unified mechanism behind both dispatcher
  recovery and `since_nseq` replay — there is one log, not two.
- A background dispatcher in the substrate library reads the
  event history in `nseq` order and emits records to two
  separate topics per §5.5:
  - Structured collection events go to
    `<svc>.props.records.changed` (the SPEC 12 topic), with the
    full body schema defined in §5.5 (including `kind`, `verb`,
    `nseq`, `version`, `audit_epoch`, `actor`, `fields_changed`,
    `secret_fields_changed_count`, optional `lifecycle` and
    `reason` on `completed` events, `cause`, `ts`).
  - Flat-path scalar leaf changes (only relevant where a
    SPEC 12 namespace also exposes itself through SPEC 07's
    flat-path read surface) go to `<svc>.props.changed` with
    the fixed SPEC 07 §3.1 body shape (`path`/`old`/`new`/
    `ts`/`cause`). The dispatcher MUST NOT emit structured
    collection events onto this topic; doing so would break
    every existing SPEC 07 subscriber.

  Audit events go to `<svc>.props.audit` for current audit
  subscribers. The dispatcher maintains a per-subscriber
  `delivered_nseq` watermark; recovery after a dispatcher
  restart re-emits from the lowest watermark across active
  subscribers. The dispatcher MAY batch deliveries.
- Network delivery is therefore at-least-once: a subscriber MAY
  receive duplicate events for the same `nseq` after a crash or
  reconnect, and MUST de-duplicate by `(namespace, key, nseq)`.
  Within one connection the dispatcher guarantees monotonic,
  contiguous `nseq` (no gaps), which is what §5.5 promises.
- Audit history (§10) is the same log: every record-write row
  also constitutes the audit entry for that `nseq`. There is one
  ordered history per namespace; `<svc>.props.watch` and
  `<svc>.props.audit.watch` are two views over it differing only
  in body shape and capability check.

The substrate does not claim "exactly-once delivery to network
subscribers" — that is unattainable across an unreliable
transport. It claims "exactly-once production and at-least-once
delivery, with `nseq` enabling subscriber-side deduplication."

