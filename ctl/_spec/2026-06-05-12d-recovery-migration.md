---
title: Cosmix Property Substrate — Bootstrap, Recovery, Versioning, Migration
chapter: 12d
version: 0.2.2
status: draft
date: 2026-06-05
companion: _spec/2026-05-11-12-property-substrate.md
---

# Cosmix Property Substrate — Bootstrap, Recovery, Versioning, Migration

> **Split out of SPEC 12 §11–12 (2026-06-05).** What happens around the edges of a
> property store's life: the fail-closed bootstrap + recovery contract (§11) and the
> schema/storage versioning + migration story (§12). The fail-closed bootstrap is
> normative + code-backed; the portable JSON export and event-history replay
> dispatcher are **unbuilt**. Section numbers are preserved as **§11–§12** so
> cross-references resolve here.

## 11. Bootstrap and recovery

The substrate fails closed: if the owning service is not running,
its namespaces are unreachable. This is the right default for
in-flight operations, and wrong for two well-known cases:

- **Daemon-down recovery.** An operator needs to edit a daemon's
  config to repair the daemon itself (the daemon is crash-looping
  on a bad value). The substrate-mediated edit path is unavailable
  precisely because the daemon is unavailable. The escape hatch
  is the underlying storage: `MixData` and `Toml` backends produce
  human-readable files on disk that an operator can edit by hand.
  After edit, the daemon re-reads on next start (or accepts a
  `<svc>.props.reload` signal — out of scope for v1).

  *Reconciliation discipline.* A hand edit bypasses the
  transactional outbox (§6.6): record bytes change without an
  `nseq` allocation, a `version` bump, or an audit-digest entry.
  On startup, the substrate library runs a reconciliation pass
  per namespace by comparing each record file's content hash
  against the last hash recorded in the event-history sidecar
  for that key:

  1. **Match.** The substrate-mediated and on-disk views agree;
     no action.
  2. **Divergence detected.** The substrate emits a synthetic
     recovery event with `verb: <svc>.props.reconcile`,
     `actor: daemon:reconciliation`,
     `cause: external_edit_detected`,
     `old:` the last sidecar-known state, `new:` the current
     on-disk state, a new `nseq` allocated, and the namespace's
     `audit_epoch` incremented by one. Audit subscribers observe
     the epoch bump and MUST treat HMAC continuity across the
     bump as broken (a later event's digest does not chain to a
     prior one across an epoch boundary). The version field is
     bumped to one greater than the highest previously seen.

  Namespaces declare their reconciliation policy in
  `NamespaceSpec`:

  ```rust
  pub enum ExternalEditPolicy {
      ReconcileAndContinue, // default
      RefuseStartup,        // refuse to serve until operator
                            //   acknowledges divergence
                            //   out-of-band (see below)
  }
  ```

  `RefuseStartup` is for state where audit continuity is
  load-bearing (e.g. payment / billing namespaces). The daemon
  refuses to serve any ABP traffic for the affected namespace
  until the operator acknowledges the divergence
  **out-of-band**: by running the daemon's CLI with an explicit
  `--acknowledge-reconcile <namespace>` flag (or
  equivalent), which writes an acknowledgement sentinel into the
  namespace's sidecar directory and on next startup completes
  the reconciliation pass as if `ReconcileAndContinue` were the
  policy. There is no wire-callable `<svc>.props.reconcile`
  verb — the daemon is not serving ABP at this point, by
  construction. The string `<svc>.props.reconcile` appears
  throughout this spec only as an event-history / audit-stream
  `verb` value identifying synthetic reconcile rows. The
  acknowledgement is a daemon-local recovery action; the
  resulting reconcile event flows through the normal
  event-history / audit-stream paths once the daemon is back
  online.

  The substrate guarantees that file-level edits and
  substrate-mediated edits **converge on the same on-disk
  state**, but it does NOT guarantee that audit continuity
  survives a hand edit — the epoch bump is the spec's honest
  admission that hand-editing trades audit integrity for
  recoverability, and that this trade is sometimes the right
  call.
- **Initial provisioning.** The substrate cannot create the first
  account on a system whose ABP transport is not yet bound to a
  trusted peer. Bootstrap is the responsibility of the install
  procedure (SPEC 11 NetServa Package Install), which seeds the
  initial admin credential out-of-band.

The substrate explicitly DOES NOT provide a "local-fallback CLI
mode" that bypasses ABP when the daemon is down. Two paths to
the same record state are the source of bugs (different
validation, different audit, different concurrency); the
storage-format escape hatch is sufficient and intentionally
inconvenient enough that operators do not casually use it.

## 12. Versioning and migration

The substrate carries two version numbers:

- The **substrate version** (this chapter's `version:`
  frontmatter). Verbs, error taxonomy, and `PropertySchema` shape
  evolve under semver. v0.x guarantees additive-only changes.
- The **per-namespace schema version**, declared in
  `NamespaceSpec.since_version` and per-field `since` / `until`.

Field-level schema evolution rules:

- A new field MAY be added with `since: <new-version>`. It MUST
  have a default value; existing records are read as if the
  field's value were the default.

  **Normative clarifier — "read as if default" boundary.** The
  "read as if default" contract is satisfied at the **typed-consumer
  boundary**, not the substrate wire boundary. Specifically:

  - Substrate `<svc>.props.get` and `<svc>.props.list` MUST project
    stored bytes verbatim (after secret-redaction), without filling
    schema defaults for absent fields. This is load-bearing for §10
    audit-digest reproducibility — the commit-time digest is HMAC
    over `canonical_serialise(stored_value) || nseq.to_be_bytes()`,
    and verifiers re-derive it via `props.get`. A substrate-wide
    default-fill would break that re-derivation for every sparse
    pre-amendment row.
  - Namespace typed read helpers (e.g. `record_to_<ns>` in the
    citizen daemon) SHOULD fill amendment defaults so internal
    consumers see a dense, schema-current value regardless of
    on-disk shape.
  - Operators MAY materialise dense on-disk rows by issuing a
    Replace carrying the full field set. A Patch only densifies
    the fields the patch explicitly carries; `before_set`
    validators return `Result<(), Error>` and cannot transform the
    stored value, so a Patch over a pre-amendment row that touches
    only legacy fields leaves the row sparse on disk.
  - The asymmetry — engine sees dense, operator sees sparse via
    `props.get` until the next Replace — is intentional and
    audit-correct. Citizens MUST document the read-helper that
    supplies amendment defaults so reviewers can trace the
    contract.
- A field MAY be deprecated with `until: <version>`. Reads of
  records written before deprecation still return the field;
  writes after deprecation MUST not include it
  (`validation_error` if they do).
- A field's type MUST NOT change in place. A type change is
  modelled as deprecation + new field with a new name.
- A namespace MAY be renamed by registering the new name and
  leaving the old name as a deprecated alias for one
  substrate-version cycle.
- **Visibility-flag transitions are breaking schema changes** —
  changing a field's `secret`, `validator_secret`, or a
  namespace's `schema_public` flag alters what the `view: public`
  schema view exposes and therefore what an unprivileged GUI saw
  before vs after. Such transitions MUST bump the namespace's
  `since_version` and be treated as additive only:
  - `secret: false → true` is allowed and is breaking (existing
    cached `view: public` schemas now miss the field).
  - `secret: true → false` is allowed and is breaking (the field
    appears in `view: public` and existing audit digests over
    records containing the field are still valid but now
    correspond to data that is no longer redacted).
  - `schema_public: allow → deny` is allowed and is breaking;
    `deny → allow` is allowed and breaking in the same way as
    revealing a previously-redacted field. In both cases,
    clients re-fetch the schema rather than relying on cached
    views.

Migrations between storage backends (Toml → SqliteTable, for
example) are out-of-band one-shot operations and are not part of
the substrate's normal mode. The substrate provides a
`<svc>.props.export` verb (deferred to v0.2) that emits a
portable JSON dump suitable for migration; until then,
namespaces with storage-backend changes carry their own
migration logic.

