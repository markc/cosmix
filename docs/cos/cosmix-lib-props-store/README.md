# cosmix-lib-props-store

`cosmix-lib-props-store` is the SPEC 12 storage and mutation substrate for
CosMix daemon property namespaces. It supplies namespace declarations, atomic
storage contracts, backends, hooks, lifecycle coordination, audit digests,
Bus mutation routing, and live event fan-out.
The crate belongs to the `cos` layer of the `bus <- mix <- cos` dependency
chain. It depends directly on the `bus` repository's
`cosmix-lib-props-core` read surface and does not depend on `mix`.

The Cargo package is `cosmix-lib-props-store`; its Rust library is `cosmix_props`.

## Crate pair

The property substrate is split across two crates:

- `cosmix-lib-props-core` owns the SPEC 07 read types, read dispatch, and
  publish builders.
- `cosmix-lib-props-store` owns the SPEC 12 mutation, storage, audit,
  namespace, and lifecycle surfaces.

This crate re-exports the core read types and preserves the earlier module
paths: `PropPath`, `PropTree`, `PropValue`, `PropDescribe`, `diff`, and
`redact` remain available through `cosmix_props`. New read-only consumers should depend on
`cosmix-lib-props-core` directly.

## Features

| Feature | Default | Provides |
|---|---:|---|
| `cosmix` | Yes | Bus message integration, `PropsRouter`, publish builders, live dispatch, subscription grants, tracing, and Tokio runtime support |
| `sqlite` | Yes | `SqliteStore`, `SqliteTableMapping`, `JsonValuesMapping`, and the `rusqlite` dependency |
| `replay-only-harness` | No | Exposes the test-only replay acknowledgement on `PropsRouter` for downstream harnesses |

Production daemon builds must not enable `replay-only-harness`. Without an
installed `SubscribeGranter`, watch requests fail unless a test acknowledges replay-only operation.

## Namespace declarations

`NamespaceSpec` is the registration-time description of one property
namespace. `NamespaceSpec::new` requires a `NamespaceName`, `PropertySchema`,
`Cardinality`, and `StorageBackendKind`, then applies the crate defaults.

The declaration surface includes:

- `NamespaceName` for validated, unqualified namespace names.
- `PropertySchema`, `FieldSchema`, and `FieldType` for record schemas.
- `Cardinality` for singleton or keyed collection namespaces.
- `StorageBackendKind` for memory, MixData, TOML, or SQLite-table storage.
- `DeleteMode` for soft or hard delete behaviour.
- `NamespaceLifecycle` for simple CRUD or saga transitions.
- `ReplayWindow` and `SubscribePayload` for event replay and subscription
  payload policy.
- `SchemaPublic`, `ExternalEditPolicy`, `ConformanceLevel`, and
  `SchemaVersion` for description, reconciliation, and conformance policy.
- `AuthPolicy`, `PeerIdentity`, `Capability`, and `CapabilitySet` for
  request-time authorisation.
- `Validator` for synchronous, side-effect-free value checks.

Namespace names are unqualified on the structured wire surface. The crate
derives the fully qualified `<service>.<namespace>` form for capability and
event strings.

## Records and events

`RecordKey` addresses a singleton or collection record and can carry a
future-facing field path. Current mutation storage operates on whole
records.

`Record` carries the value, per-record `Version`, last-change `Nseq`, and
optional saga `Lifecycle`. `RecordEvent` is the durable history row;
`AuditRow` is its audit-stream projection.

`Version` advances on record state transitions. `Nseq` is monotonic within
a namespace and provides the replay cursor. `AuditEpoch` advances when
reconciliation promotes externally changed state.

`EventKind` distinguishes created, updated, deleted, completed, and
reconciled transitions. `Actor` records the initiating caller, service,
operator, daemon completion, or reconciliation identity as a token.

## Storage contract

`PropertyStore` is the object-safe asynchronous storage trait. Its boxed
future methods provide:

- `get` and `list`, each returning a `Snapshot<T>` with the namespace's
  observed `nseq`.
- `commit_set`, `commit_delete`, `commit_complete`, and
  `commit_reconcile`.
- `events_since` for rows with `nseq` strictly greater than a cursor.
- `audit_epoch` for the namespace epoch.
- `version_anchor` for optimistic concurrency across live rows and
  soft-delete tombstones.

Every commit method atomically changes record state and appends its matching
event row. A backend exposes both writes or neither.

`MergeMode::Patch` merges top-level object fields and is the default.
`MergeMode::Replace` replaces the record value. `Version::zero()` is the
create-new optimistic-concurrency anchor.

`StoreError` represents not-found, version mismatch, storage, validation,
conflict, and expired replay-window failures.

## Storage backends

`MemoryStore` is a process-local backend. It serialises operations through a
mutex, keeps records and events in memory, and allocates audit keys lazily.
It uses hard-delete semantics and does not apply `ReplayWindow`.

`SqliteStore` wraps one `rusqlite::Connection`. It stores substrate
metadata, event history, audit keys, versions, cursors, lifecycle state,
and tombstones in substrate tables. Mutations use one `BEGIN IMMEDIATE`
transaction for the business value, substrate record metadata, and event
row.

`SqliteTableMapping` bridges a daemon's business table to `PropValue`.
Implementations bootstrap, read, list, upsert, and delete value rows inside
the substrate transaction. `JsonValuesMapping` is the supplied fallback
mapping and stores JSON values in `__props_values`.

SQLite supports hard and soft delete. Soft delete preserves the version
anchor while hiding the row from `get` and `list`. The current backend
records `tombstone_ttl` but does not expire tombstones.

## Runtime and hooks

`Runtime` binds one `NamespaceSpec` to one `PropertyStore`. It validates
keys, cardinality, primary-key shape, required versions, validators, hooks,
and lifecycle rules before and around storage commits.

`HookHandler` provides `before_set`, `after_set`, `before_delete`, and
`after_delete`. `Hooks` stores a shared handler, and `NoopHooks` supplies
the default implementation. `HookCtx` carries old and new values, record
version, actor, merge mode, key, and `WriteOrigin`.

Caller-facing writes use `WriteOrigin::caller`. Daemon-internal writes can
use `Runtime::set_with_origin` or `Runtime::delete_with_origin` with
`WriteOrigin::backend`.

For a simple namespace, a successful set is durable before `after_set`
runs; an `after_set` failure becomes a warning. For a saga namespace, the
initial set stores `Lifecycle::Provisioning`, then the hook result produces
a second completion event with `Lifecycle::Active` or
`Lifecycle::Failed`.

## Audit

`AuditKey` is a per-namespace 32-byte key. `canonical_serialise` produces
deterministic JSON bytes, `compute_digest` computes HMAC-SHA256 over those
bytes followed by the big-endian `nseq`, and `tombstone_value` supplies the
canonical delete marker.

Each event has an independent digest. There is no previous-digest chain.
The raw audit key remains in the storage backend and is not part of Bus
responses.

## Bus mutation router

With the `cosmix` feature, `PropsRouter` registers one `Runtime` per
namespace and dispatches the suffix after `<service>.props.`.

| Suffix | Required capability | Operation |
|---|---|---|
| `set` | `props.write:<service>.<namespace>` | Patch or replace a structured record |
| `delete` | `props.write:<service>.<namespace>` | Delete a collection record |
| `get` | `props.read:<service>.<namespace>` | Read one structured record |
| `list` | `props.read:<service>.<namespace>` | List structured records with an observed cursor |
| `describe` | `props.describe:<service>.<namespace>:public` or `:full` | Return the public or full schema projection |
| `watch` | `props.read:<service>.<namespace>` | Grant live record events and replay newer rows |
| `audit.watch` | `props.audit:<service>.<namespace>` | Grant live audit events and replay newer audit rows |

Callers holding both the base read capability and
`props.read:<service>.<namespace>:secrets` also receive secret field values.
Public description remains subject to both `AuthPolicy` and `SchemaPublic`.

The mutation router accepts structured `namespace` and `key` addressing.
Flat-path reads belong to the SPEC 07 core surface. Structured mutation
does not currently implement sub-record `field_path` writes.

Bus errors use the crate's closed response vocabulary:
`auth_denied`, `not_found`, `validation_error`, `conflict`,
`version_mismatch`, `storage_error`, `hook_error`, `unavailable`,
`replay_window_exceeded`, and `grant_failed`.

## Live fan-out

`spawn_dispatcher` starts one task per namespace. It tails committed
`RecordEvent` rows and publishes non-retained deltas to
`<service>.props.records.changed` and `<service>.props.audit`.

`DispatchPublisher` abstracts publication. `SubscribeGranter` abstracts the
watch-mediated broker subscription granted after capability checks.
`DispatcherHandle::shutdown` stops a dispatcher cleanly.

Watch handling grants the live subscription before taking the observed
cursor, then returns replay rows up to that cursor. Events after the cursor
belong to live delivery. Replay is authoritative; live publication is
best-effort.
