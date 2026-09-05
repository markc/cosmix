---
title: Mesh membership, transport and trust
chapter: 8
version: 0.2.1
status: draft
date: 2026-09-05
---

# Mesh membership, transport and trust

Baseline `96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be`. Verified-source statements
describe inspected code, not a deployment attestation. Intended requirements and
conflicts remain explicit. Examples are conceptual; no deployment roster or private
key location is published here.
The [accepted compatibility amendment](compatibility-profile.md) resolves the
genesis-only boot trust and never-verified roster fallback discrepancies below.

## 1. Planes and invariants

**MESH-001 — Separate authority from delivery.** D0 provides transport (WireGuard),
D1 carries Bus frames, D2 brokers route, D3 citizens implement applications.
Authority A declares membership/identity; tooling C authors and distributes it.
C is never a runtime dependency. Membership is neither WireGuard presence nor
liveness. Citizens register on member nodes; registration does not create a node.

**MESH-002 — Single declared source in the verified profile.** Membership is signed, versioned, replicated
and locally cached. Generated config, host projections and UI rosters are derived;
observed health and contacts cannot overwrite authority or create members. Keep
serving accepted membership when control tooling is unavailable, subject to the
explicit trust profile. Topology independence does not promise fault tolerance:
hub-spoke transport loses remote spoke connectivity when its only hub fails.
The never-verified compatibility fallback in MESH-018 is an explicit exception
to signed-authority routing, not proof of verified membership.

**MESH-003 — Responsibility.** Noded implements broker routing and authority
enforcement; WireGuard control services manage transport; citizens own application
state. Property projections of peers/trust must not become an independent membership
registry. Snapshot the member view when running an operation and tolerate later
changes. Node reachability, session health and registered-service liveness are three
different advisory observations; do not pre-empt a valid route using stale health.

## 2. Identities and credentials

**MESH-004 — Node identity.** Immutable per-mesh names follow ASCII label grammar
`^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$`. Names retire by tombstone and are not
reused. Rename means a new identity; any explicit migration alias needs an expiry
epoch. Node records carry name, mesh IP, optional noded port, bus flag, status,
credentials and last-touched provenance. Transport-only `bus:false` is distinct
from active routable membership; tombstoned records never route.

**MESH-005 — Three key roles.** WireGuard credentials secure transport; separate
Ed25519 D2 credentials prove broker-session origin; inventory signing keys authorise
the member set. A credential is current at accepted epoch E iff
`from_epoch <= E < until_epoch`, with absent end unbounded. Rotation overlaps
credentials without changing the node name. No wall-clock expiry is inferred.
Private D2 seeds remain node-local and distinct from WG keys; bootstrap storage is
root-owned but readable by the unprivileged daemon (0640 with its service group).

Evidence: [credential selection and admission][admission], [strict routing types][routing].

## 3. Signed inventory and freshness

**MESH-006 — Signed content.** Envelope contains payload and external signatures.
Payload includes schema version, canonical encoding, mesh FQDN, subnet, epoch,
recovery generation, advisory timestamps, hubs, verify keys and member records.
Normal payloads omit `recovery`; recovery payloads include `recovery:true`.
Signatures cover canonical payload bytes, not a struct containing its own signature.
The pinned algorithm is Ed25519 and canonical encoding is `json/rfc8785`.
Unknown signed encoding is refused; it cannot select a weaker verifier.

**MESH-007 — Malleable signature bag.** Verify using already-trusted key bytes,
with the key declared in the payload. Skip malformed, unsupported, duplicate or
untrusted signature entries; count each valid key ID once and require at least one
authoritative signature. Unsigned carrier junk cannot invalidate an otherwise
valid authoritative signature. Payload structural validation remains mandatory.

**MESH-008 — Trust-set transition.** Genesis is provisioned out-of-band and cannot
be removed in-band. Retiring keys may verify but cannot solely authorise addition
or promotion of verify keys. Intended rotation introduces a new key under the old,
overlaps signatures, then removes the retiring key after adoption. This remains
a proposed stronger rotation profile. In the accepted current profile, noded
re-derives trust from genesis on boot; adopted multi-key anchors are
reported but not persisted as next-boot trust. Library support is not a complete
daemon rotation procedure. Restart-persistent multi-key adoption is not guaranteed;
operators cannot assume an in-band adoption survives a restart as a trust anchor.

**MESH-009 — Epoch and recovery.** Authored changes advance the inventory epoch.
Normal acceptance cannot lower epoch or recovery generation. Recovery may lower
epoch only with genesis verification and a strictly greater generation. Equal
generation recovery is accepted only when canonical hash proves an exact retry.
Normal higher generation requires genesis verification; legacy absent generation
means zero only while the cached generation is zero. Explicit `recovery:false`
is invalid. Bind accepted freshness pairs to canonical hashes; changing signed_at
at the same bound pair is not an equivalent re-sign.

**MESH-010 — Persisted floor.** Store epoch, recovery generation and canonical
hash durably. A wiped node needs a current provisioning floor; a file under a state
directory alone cannot survive deletion of that directory. Anti-rollback protects
subsequent acceptance, not initial poisoning by an old but valid provisioning
artifact. Recovery must remove compromised credentials and attacker-added verify
keys. Higher recovery epoch is useful hygiene, not the security condition replacing
generation. Single-key compromise remains a documented profile risk; multi-party
quorum/offline custody are future profile choices, not new default ceremonies.

Evidence: [inventory verifier][inventory], [daemon authority loader][authority].

## 4. Authorship, distribution and rollout

The reconciled 0.3.0 [capability adapter](https://github.com/markc/cosmix/blob/4d2f1ebb77af51d8bbd08cb18f4e7070cebb58ac/src/crates/cosmix-lib-mesh-trust/src/combinator.rs)
filters empty cross-mesh grants rejected by the core Capability constructor.
An invalid cross-mesh grant does not fall through to the base policy. This is a
fail-closed conversion boundary, not stronger transport or membership verification.

**MESH-011 — Exact proposal lifecycle (intended orchestration contract).** Keep
authorised proposal bytes distinct from fully rolled-out canonical bytes. Under
one exclusive control lock, snapshot the freshness floor; reuse a pending verified
proposal rather than silently minting another; use one immutable byte snapshot for
verification, derivation and distribution. Normal distribution requires explicit
generation and genesis verification for cold-node catch-up. Advance the control
floor after first target acceptance; publish the canonical rolled-out artifact only
after every target accepts. Retain partial proposals for byte-identical resume;
discard requires identifying the authenticated canonical hash.

Recovery distribution is separate: it must record byte-pinned progress and handle
equal-generation exact retries at already reached targets. Do not copy the normal
first-accept lifecycle without that recovery resume design. Joins are authored and
signed, not auto-discovered; leave tombstones and revoke transport credentials.

**MESH-012 — Derivation.** Generate broker rosters, DNS/hosts projection, UI roster
and WG configuration reproducibly. Allocate addresses from the union of authored
and rolled-out coverage, retaining tombstoned reservations; require matching
subnets. A reconciler reports declared/on-wire/IP/key mismatch and separately
states whether listener-bind and intra-mesh masquerade checks ran. A membership
check cannot claim a clean admission posture it never examined.

**MESH-013 — Listener cutover.** Resolve port absence as 4200; explicit ports are
integers 1..65535. Roll compatible binaries before publishing changed endpoints.
Prepare and verify the actual new listener before publishing signed endpoint
changes. Outage during prepared cutover is explicit; rollback of listener intent
is safe only before first new-inventory acceptance, then resume forward. Compare
resolved old/new endpoints, including custom-port-to-absent/default transitions.
A daemon reports self-port divergence but does not restart/rebind itself from an
inventory update; listener identity is start-bound.

## 5. Session admission profiles

**MESH-014 — Origin proof.** In enforced profile, every inter-node session proves a
current D2 credential for an active bus member. Same-node local citizens are the
local-trust exception, not proof by remote source IP. A remote peer cannot bypass
admission by registering a non-bridge name. `bridge-<node>` declares the identity
to prove; the signature supplies proof. D0 source-IP checks may strengthen hub
configuration but cannot establish spoke origin through a common hub.

**MESH-015 — Transcript and exchange.** Broker sends `noded.admit.challenge`
first; peer answers `noded.admit.response` under the same ID. Challenge carries
mesh FQDN, verifying broker, epoch, session ID (16 bytes), nonce (32 bytes), version
and profile. Response carries source node, signed epoch, signature, ephemeral share
and channel-binding hash. Binary fields are base64 on wire but decoded before
canonical transcript construction; epoch is native u64. Transcript order is domain
tag `cosmix-d2-admit-v1`, mesh, source, verifier, epoch, session ID, nonce,
ephemeral share, channel binding, using the shared implementation's canonical
encoding. Unused fixed-size fields are zero, not omitted.

Nonce is fresh, unpredictable, socket-bound and consumed once before verification.
Bound challenge state and waits; a missing broker-first challenge permits legacy
client fallback, never an enforced broker accepting failed proof. Wire-to-transcript
round-trip fixtures are mandatory. The canonical digest in observation distinguishes
wire-construction errors from absent keys.

**MESH-016 — Posture.** `off` is the staged default; `observe` computes verdicts
without refusal; `enforce` refuses failures. In enforce, no verified trust root or
non-mesh-bound listener refuses inter-node sessions, with explicit reason, while
local citizens continue. Missing self D2 credential means prover-incapable, not
incapable of verifying another peer. Admission features must remain agent-operable;
human confirmation is not an inherent protocol requirement.

**MESH-017 — Honest trust boundary.** Origin-only profile does not defeat a hub
actively relaying/altering traffic. Hardened profile requires an end-to-end shared
secret, role/identity/share-bound exporter and key confirmation/AEAD before use;
hashing public shares or the common hub's WG key does not provide that property.
This hardened exchange is intended, not established by origin-only admission.
A WG PSK protects a hop and is not a substitute for node proof.

Node proof also does not prove which services it may claim. Mesh-wide service
claim constraints remain unresolved for mixed-trust use; signed `may_register`
is a candidate, not an existing field. Untrusted local code needs a concrete
isolation boundary preventing broker access; ordinary trusted same-UID agent code
is not reclassified as hostile. Correctness checks (identity binding, CAS, durable
floors, lock ownership) stay unconditional even in an unattended profile.

Evidence: [admission core][admission], [broker admission][broker-admission].

## 6. Routing and reload

**MESH-018 — Routing profiles.** The Bus chapter owns address grammar. Verified
routing uses accepted ActiveBus entries and signed IP/port. Cross-mesh syntax
remains reserved and fails explicitly. Never-verified noded boot may use its
derived compatibility roster: universal denial of unverified remote routing is
superseded for this profile. Such routes must not be described as authenticated
signed membership. Verified authority never silently falls back after a bad reload.
This does not disable MESH-016's enforced-admission refusal when verified trust
is absent; routing selection and session admission remain separate gates.

Target failures distinguish not-a-member, tombstoned, not-bus-member and
cross-mesh-unimplemented; source admission failure is admission-refused; attempted
delivery may fail delivery-timeout. Keep rc 10 compatibility and one structured
reason. Health hints cannot become a second membership authority. Do not promise
every refinement exists without handler evidence.

**MESH-019 — Reload authority.** Use the existing verification path; retain last
good state on rejected/partial/older updates. Off/observe inbound sessions are
grandfathered; enforce rechecks membership/current credential eligibility and
revokes ineligible sessions. Do not reverify an old session signature at a new
epoch: that would wrongly invalidate every session after an epoch change.

**MESH-020 — Outbound fence.** Before publishing accepted routing authority,
reconcile desired active name→endpoint projection. Retire only removed names or
changed endpoints, not unchanged connections after unrelated epoch/credential
changes. Atomically fence enqueue, cancel in-progress connects, stop reader/writer,
discard stale-generation dequeued frames and fail pending RPCs once, without
transparent retry. Old resolved routes must not redial a retired endpoint. A frame
already written may have executed: outcome is unknown. Rejected reload performs
no transport operation. Emit accepted/reload-applied only after fence and authority
publication; remote kernel-visible socket closure need not precede the event.

**MESH-021 — Connect/delivery.** TCP plus WebSocket upgrade is bounded to five
seconds; callers for the same peer share a single attempt and its failure. Admission
first-frame wait is separate. Fail-fast RPC, best-effort events and no implicit
retry are the conservative floor; transparent retries need a real idempotency and
deduplication contract. Earlier B3 “cannot ship teardown” prose conflicts with
already implemented enforcement teardown. Preserve the missing operational drill
evidence rather than pretending implementation itself ratifies delivery behaviour.

Evidence: [authority loader][authority], [noded runtime][noded], [mesh transport][mesh].

## 7. Partitions, discovery and observability

**MESH-022 — Freshness limitations.** Intended cold/unconfirmed posture serves
local state, accepts verified revocations and refuses expansion. A known newer
signed epoch can signal stale authority. Wall-clock signed_at/valid_until are
advisory for inventory, not trusted expiry. Current Verified/Unverified state is
not evidence of the complete old degraded/fresh state machine. Emergency signed
revocation-only deny-list overlay remains intended; it must only remove authority,
combine most-restrictively and never advance inventory epoch. Exact replay and
reinstatement rules need implementation review before adoption.

A sole carrier that suppresses updates can hide staleness indefinitely. Redundant
carriers, trusted-time design and bounded beacon suspicion remain optional future
work. Liveness cannot fabricate members. Hub outage preserves local service and
cached knowledge, not remote delivery. Missed tombstones/rotations apply when a
new verified authority is accepted, subject to the actual profile.

**MESH-023 — Topology extensions.** Redundant hubs and opportunistic direct WG
peering are separate future work. Direct peers must use inventory credentials,
fallback to relaying when NAT prevents connection, and never use TOFU. No node
count, particular hub, endpoint roster or prior measurement is a public contract.

**MESH-024 — Observable evidence.** Inventory accepted/rejected identity includes
hash, epoch, generation and verify-key IDs. Report admission configured/effective
posture, admitted/refused/observed verdict, and claimed-versus-proven source.
Admission details map to credential signature/window, tombstone, bus flag, name,
epoch, malformed member, stale challenge, version, missing proof/seed and local
trust/bind failure. Report delivery fate by session-churn, admission-refused or
reload-teardown and inbound/outbound direction. Outbound retirement reports peer,
endpoint and generation; reload-applied reports changed/retained counts. Connect
timeout and listener divergence/convergence must remain observable. Intended
freshness/deny-list events must not be advertised as emitted until implemented.

## 8. Compatibility and acceptance

**MESH-025 — Upgrade evidence.** Clients precede brokers where separately staged;
combined prover/challenger binaries stage off→observe→enforce with machine-readable
verdict evidence. Delivery drills must include actual teardown, not observe-only
telemetry. Current signed-endpoint and restart-floor tests are source evidence;
fresh fleet acceptance is separate. Future negotiation should independently name
inventory schema, D2 admission, routing reasons, reload delivery and E2E profile.
No compatibility vector is implied to be on wire today.

Acceptance: verifier adversarial fixtures (malformed bags, recovery generations,
equal-pair hash conflict, retired/genesis keys); typed endpoint/member tests;
wire transcript round-trip and enforce failure tests; reload race/fence tests;
restart durability; and isolated multi-node disruption drills. No live network,
keys or admission settings were changed or tested by this audit.

[inventory]: https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-lib-mesh-trust/src/inventory.rs
[admission]: https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-lib-mesh-trust/src/admission.rs
[routing]: https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-lib-mesh-trust/src/routing.rs
[authority]: https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-noded/src/authority.rs
[broker-admission]: https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-noded/src/admission.rs
[noded]: https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-noded/src/noded.rs
[mesh]: https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-lib-mesh/src/lib.rs
