# cosmix-lib-mesh-trust

`cosmix-lib-mesh-trust` provides the pure verification primitives and optional Cosmix substrate integration used for cross-mesh trust, signed inventories, admission proofs, signed request envelopes, and capability grants. It is a library crate in `cos`, downstream of `bus` in the `bus <- mix <- cos` dependency chain; its core has no Bus or Mix dependency, while the default `cosmix` feature adds Bus and property-store integration.

## Synopsis

- Cargo package: `cosmix-lib-mesh-trust`
- Rust library: `cosmix_mesh_trust`
- Current crate version: `0.4.3`
- Binary targets: none
- Default posture: core verification plus the `cosmix` integration feature

The core is synchronous and carries no I/O. Callers supply trust state, timestamps, member records, grants, and key material. The integration layer adds an in-memory trust cache, a property subscriber, an authorisation-policy combinator, record conversion, and draft property namespace schemas.

## Features

| Feature | Default | Provides |
|---|---:|---|
| `default` | yes | Enables `cosmix`. |
| `cosmix` | yes | Adds `cache`, `combinator`, `subscriber`, `wgd_namespaces`, and `wgd_records`; enables the property store, Bus client and wire crates, Tokio, async traits, streams, and tracing. |

Use `--no-default-features` for the protocol-level core without Bus, property-store, async-runtime, or tracing dependencies.

## Core modules

| Module | Main API | Purpose |
|---|---|---|
| `admission` | `AdmissionTranscript`, `select_d2_pubkeys`, `select_wg_pubkeys`, `sign_admission_transcript`, `admit` | Builds and verifies domain-separated D2 session-admission proofs and selects epoch-valid D2 or WireGuard credentials. |
| `canonical` | `to_canonical_bytes`, `deserialize_strict_value` | Produces the shared v1 canonical JSON bytes and rejects duplicate object keys in signed JSON values. |
| `caps` | `Cap`, `CapabilitySet`, `TrustedMesh`, `Grant`, `TrustStore`, `resolve_cross_mesh_caps` | Models capability bags and resolves the effective cross-mesh capability set. |
| `ctk_caps` | `CTK_NOTIFY`, `CTK_ACTIONS`, `CTK_DIALOG` and grant helpers | Pins the canonical CTK capability-token spellings and supplies exact-set predicates and constructors. |
| `envelope` | `SignedEnvelope`, `Envelope`, `EnvelopeError` | Parses and structurally validates v1 signed cross-mesh request envelopes. |
| `freshness` | `parse_rfc3339`, `check_freshness`, `check_envelope_ts` | Applies a symmetric timestamp freshness window. |
| `inventory` | `SignedInventory`, `InventoryPayload`, `NodeTrustState`, `AcceptedInventory` | Parses and verifies signed mesh inventories against an existing trust-anchor set and rollback baseline. |
| `sig` | `sign_ed25519`, `verify_ed25519`, `select_pubkey` | Signs and strictly verifies Ed25519 messages and selects a fresh current or rotation-grace key. |

## Signed request envelopes

`SignedEnvelope::parse` parses JSON, rejects unknown fields, checks the `v1` envelope version, requires the identity, target, command, timestamp, and nonce fields to be non-empty, and requires `headers` to be an object.

Every `Envelope` contains twelve signed fields:

- envelope version;
- target mesh, service, and endpoint;
- caller mesh, selector, and node identifier;
- command;
- structured headers and body;
- RFC 3339 timestamp;
- base64 nonce.

The detached signature covers `Envelope::canonical_bytes()`, never the received wire bytes. Canonical JSON sorts object keys recursively, emits no whitespace, uses minimal JSON escaping, preserves Unicode without normalisation, and rejects duplicate keys at strict parse boundaries.

The crate exposes the pieces of the verification pipeline rather than an end-to-end HTTPS verifier. A caller parses the envelope, selects the trusted public key, verifies the signature over canonical bytes, checks freshness, performs replay handling, and resolves capabilities.

## Signed inventories

`SignedInventory::parse` performs structural JSON parsing. `SignedInventory::verify` then checks the closed canonical-encoding and schema-version values, validates the signed verify-key set, re-canonicalises the payload, and requires a valid signature from a declared key that the node already trusts.

Verification also enforces:

- monotonic normal epochs;
- genesis-key retention in the adopted key set;
- in-band adoption of the signed verify-key set;
- genesis-authorised recovery with a strictly increasing recovery generation;
- prevention of recovery-floor rollback or unauthorised floor increases;
- one count per signing key identifier.

Invalid or unknown entries in the unsigned signature bag do not invalidate an otherwise valid payload. Unknown fields and duplicate keys inside signed payload data fail closed.

On success, `AcceptedInventory` returns the epoch and recovery generation to persist, whether recovery authorised the result, the trusted keys whose signatures verified, and the verify-key set to adopt. Persistence remains the caller's responsibility.

Inventory `signed_at` and `valid_until` values are advisory. The verifier does not use wall-clock time as a security gate.

## Admission

`AdmissionTranscript::canonical_bytes` uses a fixed field order with a domain tag and length-prefixed byte fields. The transcript binds a proof to the mesh, claimed source node, verifying broker, inventory epoch, session identifier, server nonce, client ephemeral value, and channel-binding hash.

`select_d2_pubkeys` and `select_wg_pubkeys` return every matching credential in the half-open epoch interval `[from_epoch, until_epoch)`. Missing or malformed bounds and non-32-byte public keys are skipped.

`admit` requires an active member with `bus: true`, an exact claimed-node match, the broker's accepted inventory epoch, a current D2 credential, and a valid Ed25519 signature. Deny-list policy and nonce freshness or single-use state remain caller responsibilities.

## Capability resolution

`CapabilitySet` is backed by a `BTreeSet`, giving deterministic iteration and serialisation. It provides insertion, membership, iteration, union, and intersection.

`resolve_cross_mesh_caps` accepts only identities beginning with `mesh:`. It returns an empty set when trust is missing, disabled, or stale, when no enabled grant is currently valid, or when the granted capabilities do not intersect the target namespace's exposable set.

Capability tokens are opaque strings. Wildcards are not expanded: a wildcard token intersects a specific token only when the two strings are identical.

The `ctk_caps` helpers create and test exact capability bags for passive notifications and application actions. `CTK_DIALOG` reserves a separate modal capability spelling; the module does not implement policy enforcement.

## Cosmix integration

With `cosmix` enabled, `TrustGrantsCache` stores trusted meshes, grants grouped by mesh, and exposable capability sets grouped by namespace. `TrustGrantsCacheHandle` implements the core `TrustStore` trait and performs synchronous in-memory reads.

`TrustGrantsCache::start` launches one Tokio subscriber task for each of:

- `wgd.trusted_meshes`;
- `wgd.grants`;
- `wgd.cross_mesh_exposable`.

Each task lists its namespace, opens a watch from the returned sequence watermark, applies replay and live changes, and reseeds after list, watch, or stream failure with bounded exponential backoff. `Ready` becomes true only after all three watches emit their replay-caught-up marker. Dropping `SubscriberHandle` aborts the tasks.

`WgdClient` is the object-safe list/watch abstraction used by the subscriber. `parse_trusted_mesh`, `parse_grant`, and `parse_exposable_entry` convert property records into core values; malformed rows are skipped by the subscriber without discarding the rest of the namespace.

`with_cross_mesh_grants` wraps an existing property `AuthPolicy`. Non-mesh identities fall through to the base policy. A `mesh:` identity is resolved only through cross-mesh trust, grants, and the namespace exposable set; failure does not fall through.

## Draft property schemas

`wgd_namespaces` returns collection `NamespaceSpec` drafts for:

| Function | Namespace | Primary key |
|---|---|---|
| `trusted_meshes_spec` | `wgd.trusted_meshes` | `mesh_fqdn` |
| `grants_spec` | `wgd.grants` | `grant_id` |
| `replay_nonces_spec` | `wgd.replay_nonces` | `pk` |
| `cross_mesh_exposable_spec` | `wgd.cross_mesh_exposable` | `pk` |
| `cross_mesh_audit_spec` | `wgd.cross_mesh_audit` | `audit_id` |

`all_specs` returns all five drafts. Each uses collection cardinality, a SQLite-table storage declaration, and deny-all authorisation inherited from `NamespaceSpec::new`. A registering service must replace that placeholder authorisation with its real policy.

## Example

```rust
use cosmix_mesh_trust::caps::{Cap, CapabilitySet};
use cosmix_mesh_trust::ctk_caps::{grants_notify, notify_v1_grant};

let granted: CapabilitySet = [
    Cap::new("ctk.notify"),
    Cap::new("props.read:example.accounts"),
]
.into_iter()
.collect();

let effective = granted.intersect(&notify_v1_grant());
assert!(grants_notify(&effective));
```

## Limits

- The crate does not provide a complete HTTPS verifier.
- Replay protection storage and enforcement are not part of the freshness module.
- The core does not generate keys, perform I/O, or persist accepted inventory state.
- The inventory verifier does not yet enforce the retiring-key promotion rule described in its module contract.
- The property namespace definitions are drafts, not registration side effects.
- The crate has no CLI and no standalone configuration file.

## Verification

Run the core and default-feature test suites from the `cos` workspace:

```text
cargo test -p cosmix-lib-mesh-trust --no-default-features
cargo test -p cosmix-lib-mesh-trust --all-features
```
