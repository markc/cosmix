---
title: Persistence, lifecycle, recovery and migration
chapter: 7
version: 0.2.1
status: draft
date: 2026-09-05
---

# Persistence, lifecycle, recovery and migration

Baseline `96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be`; verified-source observations
are not fresh test results. Intended requirements remain binding candidates where
implementation is incomplete; conflicts are not resolved by this editorial rewrite.
Exception: [the accepted compatibility amendment](compatibility-profile.md) resolves
the audit-integrity and live-delivery guarantees below to the implemented profile.

## 1. Storage ownership and formats

**STORE-001 — One writer.** The namespace owner controls records and operation
semantics. Mutable state belongs under `/var/lib/cosmix/<service>/properties/`
or the owner's database; bootstrap identity/listener/key material belongs in
root-managed `/etc/cosmix/<service>/`, read-only to the daemon. Properties must
not become an alternate privileged writer of bootstrap files. Existing service
config migrations require per-service evidence; old TOML-only path examples do
not override the newer Mix config format convention.

**STORE-002 — Data-format boundary.** Newly authored substrate-read structured
files use inert Mix literal data; third-party formats remain their native format,
and prose remains Markdown. Strict data parsing rejects calls, variables,
interpolation and executable statements. Scripts and data may both end in `.mix`
but require different parsers. Recognised roles include `.conf.mix`, `.spec.mix`,
`.journal.mix`, `.verdict.mix`, `.call.mix`, `.state.mix`. Existing foreign formats
convert when touched. This rule does not imply replacing SQLite databases or
changing JSON protocol/canonical cryptographic bytes.

**STORE-003 — Backend availability.** Memory and SQLite backends are present.
Memory is ephemeral; SQLite stores business data and substrate metadata together
through `SqliteTableMapping`. `MixData`/`Toml` enum variants and whole-file+sidecar
designs are intended, not implemented backends. A future file backend must prove
crash-consistent multi-file commit; two independent atomic renames are not a
transaction. One namespace chooses one backend.

Evidence: [store trait][store], [SQLite][sqlite], [memory][memory].

## 2. Atomic writes and hooks

**STORE-004 — Commit invariant.** A successful transition commits record state,
record version, namespace nseq and immutable event history together. SQLite uses
one SQL transaction; Memory supplies process-local consistency without durability.
History carries event kind/verb, actor, key, version, nseq, audit epoch, timestamp,
changed-field metadata and digest. Set/delete/complete/reconcile are distinct
transitions; complete and reconcile are internal event verbs, not callable CRUD
commands. No atomicity across services or namespaces is claimed.

**STORE-005 — Hook ordering (verified-source).** Validate key/cardinality and
required version presence; read prior context; check primary key; run registered
pure validators; call `before_set`; commit; notify dispatcher; call `after_set`.
At reconciled props-store 0.3.0, Saga completion attribution is additionally
validated before hooks and the initial Provisioning write; an invalid owning
service therefore fails before those effects. See the
[current runtime](https://github.com/markc/cosmix/blob/4d2f1ebb77af51d8bbd08cb18f4e7070cebb58ac/src/crates/cosmix-lib-props-store/src/runtime.rs).
Pre-hook failure prevents commit, but a hook can itself have external side effects,
so do not infer that nothing outside storage happened. The backend checks expected
version at commit, after pre-hooks. Validation success cannot promise commit success.

For Simple lifecycle, post-hook failure leaves the commit successful and returns
warnings. For Saga, commit the initial record as Provisioning, run post-hook, then
commit Active or Failed as a second version/nseq transition. The caller receives
set and complete sequence pairs plus terminal lifecycle; failed domain provisioning
can therefore be a successful substrate response. Completion is pinned to the
initial committed version: concurrent changes may cause completion mismatch after
the first write already succeeded. Do not promise rollback of that initial write.

**STORE-006 — Managed lifecycle.** Callers must not write reserved lifecycle state.
Consumers needing externally usable resources filter Active records. Post-hooks
must be idempotent for recovery. Hooks implement business operations; an account
creation involving external mailbox provisioning is a saga, not a fake database
transaction. Schema-aware secret sanitisation of failure text remains an evidence
gap: the inspected runtime sanitizer strips controls and bounds length, which is
not comprehensive secret redaction. Owners must not put secrets in hook errors.

Evidence: [runtime][runtime], [lifecycle helpers][lifecycle].

## 3. Audit and retained history

**STORE-007 — Digest (verified-source).** Each row independently computes
`HMAC-SHA256(namespace_key, canonical_record || nseq_be_u64)` and exposes lowercase
hex. The namespace key is 32 random bytes, retained privately by its backend.
Canonical form is deterministic UTF-8 JSON with sorted object keys. Deletes use
the literal `null`; non-finite floats also canonicalise to null, matching the
wire projection. Signed/unsigned equivalent integers share decimal bytes.

**STORE-008 — Integrity scope (accepted compatibility profile).** There is **no previous-digest
chain**. Earlier “HMAC-chained” wording is superseded; it overclaimed the implementation and its own
formula. The digest covers value and nseq, not all event metadata; it does not
alone authenticate actor/key/verb/epoch nor prove absence of deleted history.
Privileged verification requires the key and the exact historical unredacted
value and event nseq. A later current `get` or a redacted response cannot verify
an older row. Do not substitute snapshot-wide observed nseq for that row's nseq.
Stronger provenance authentication needs an explicit versioned design decision.

**STORE-009 — Public audit projection.** Audit events contain namespace, key,
verb, version, nseq, epoch, actor, timestamp, changed-field metadata and digest;
never raw secret values. Audit/watch share retained history and projection helpers.
Access is capability-gated through the owner and protected broker grants. Keys
must not leave private storage through any property response. Rotation with key IDs
and retained verification keys is deferred; namespace recreation is not a seamless
key-rotation procedure.

**STORE-010 — Best-effort delivery.** Atomic event production is separate from delivery.
The current dispatcher starts at the observed tail and advances after publish
errors; it has no durable per-subscriber acknowledgement cursor. The old
at-least-once/contiguous-live promise is superseded for this profile. Recovery uses
retained replay and re-list on expired history. Neither a successful commit nor
a caught-up marker proves all future notifications will arrive.

At 0.3.0, [SQLite history decoding](https://github.com/markc/cosmix/blob/4d2f1ebb77af51d8bbd08cb18f4e7070cebb58ac/src/crates/cosmix-lib-props-store/src/sqlite.rs)
validates persisted Actor values. A batch containing a malformed actor fails with
`StoreError::Storage`; replay/read fails rather than skipping or quarantining that
row. The dispatcher retries failed history reads, distinct from advancing after
publish errors. Writes are not automatically disabled by this decoding failure.
Compatibility with an existing database requires inspecting stored values; this
audit did not do so. Actor metadata is outside the row HMAC and editing it alone
does not establish a digest mismatch.

Evidence: [audit implementation][audit], [dispatcher][dispatcher].

## 4. Recovery

**STORE-011 — Owner unavailable.** A down owner means unavailable properties.
There is no normal alternate CLI path that bypasses owner validation/audit.
Out-of-band bootstrap provisions initial authority. Future human-readable file
backends permit deliberate daemon-down edits; this is an exceptional recovery path.
Do not claim those files exist for SQLite namespaces.

**STORE-012 — Reconciliation intent.** Before saga replay, detect divergence
between record bytes and last recorded state. ReconcileAndContinue records a
synthetic reconcile event, advances nseq/version and increments audit epoch;
RefuseStartup blocks that namespace pending explicit local acknowledgement.
Acknowledgement must bind the observed discrepancy and cannot be a wire reconcile
verb on a daemon that is not serving. The event actor is `daemon:reconciliation`,
cause `external_edit_detected`. Secret bytes must stay out of public old/new
diagnostics. Epoch records discontinuity; it does not repair lost audit history.

**STORE-013 — Recovery implementation boundary.** `commit_reconcile` exists in
the stores, and commit/completion primitives can support provisioning replay; that is not evidence
of a universal file-hash scanner, acknowledgement CLI or full daemon startup wiring.
Owners must establish reconcile-before-replay sequencing, replay against the
post-reconcile record, and leave failed replay visible as Failed with an actionable
reason. The old “provisioning nseq older than most recent Complete” selection is
not a sufficient restart predicate by itself; surviving Provisioning state and
version-pinned completion must govern replay, including subsequent re-provisioning.

## 5. Schema and backend migration

**STORE-014 — Separate versions.** Spec version, crate version, namespace schema
version, record version, namespace nseq and audit epoch are different quantities.
Never use one as a substitute for another. Baseline `SchemaVersion` validates its
string deserialisation. Versions describe contract changes, not fleet verification.

**STORE-015 — Defaults.** Add fields with defaults; typed namespace readers supply
defaults for older sparse records. Wire get/list project stored fields (subject
to redaction), without silently materialising defaults. A full Replace may densify
storage; a Patch updates only supplied fields. This preserves reproducible audit
input. Each owner documents the typed read helper supplying amendment defaults.

**STORE-016 — Compatibility intent.** Deprecation via `until` preserves old reads
but rejects new writes of retired fields; type changes use a new field name.
Namespace renames require an explicit alias/migration interval. Changes to secret,
validator-secret or public-schema visibility are behavioural compatibility changes:
version them, invalidate cached descriptions, and document exposure effects.
The old simultaneous “v0.x additive-only” and “visibility changes are breaking but
additive” statements conflict; this draft records the conflict and does not infer
a free pre-GA permission to break consumers silently.

**STORE-017 — Deferred extensions.** Portable export/import, backend migration,
field encryption, transaction batches and automatic schema derivation require
separate acceptance evidence. Until a portable export exists, each backend migration
must preserve records, versions, history, audit keys and recovery semantics or
explicitly declare the losses; a data-only dump is not an audit-preserving backup.

## Validation requirements

Run backend contract tests for set/delete/recreate, CAS mismatch, event atomicity,
secret projection, replay expiry and digest vectors; runtime tests for pre/post-hook
failure, Saga completion races and replay; broker integration for protected grants.
SQLite restart and failure-injection evidence is required before claiming durable
recovery. File backends need crash points across every persistence step before
promotion. This audit ran none of those tests.

[store]: https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-lib-props-store/src/store.rs
[sqlite]: https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-lib-props-store/src/sqlite.rs
[memory]: https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-lib-props-store/src/memory.rs
[runtime]: https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-lib-props-store/src/runtime.rs
[lifecycle]: https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-lib-props-store/src/lifecycle.rs
[audit]: https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-lib-props-store/src/audit.rs
[dispatcher]: https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-lib-props-store/src/dispatcher.rs
