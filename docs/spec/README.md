---
title: Cosmix specification suite
version: 0.3.4
amends: suite-0.3.3
status: accepted
date: 2026-09-11
---

# Cosmix specifications

This is the canonical pre-GA development specification suite, audited against source commit
`96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be`. It is not a claim that every
requirement is implemented, tested or deployed through this acceptance. Publication does not
resolve the discrepancies recorded in individual chapters.

The [security and delivery compatibility amendment](compatibility-profile.md)
is accepted: its listed guarantees now match the implementation. The suite's
authority transfer and its limits are recorded in the
[accepted handover](authority-handover.md). Accepted successor content also
lives in chapters 02 (composition obligations under the article II disposition)
and 10a (the HANDOVER-003 identity-registry disposition) — the handover's
article VI successor list names the policy documents, not every chapter
carrying accepted content. Technical discrepancies remain open;
acceptance is not implementation or deployment certification.

The complete 47-file delta through commit
`4d2f1ebb77af51d8bbd08cb18f4e7070cebb58ac` has also been reconciled.
Chapters 03, 06, 07 and 08 describe the changed boundaries; unchanged modules
retain original source pins. This does not imply fresh Rust tests or deployment.

## Reading order

| Chapter | Contract |
| --- | --- |
| [00 — Authority and change](00-authority.md) | Status, evidence, naming and amendment rules |
| [01 — Foundations](01-foundations.md) | Ownership, safety, autonomy and architectural principles |
| [02 — Composition](02-composition.md) | Workspace boundaries, dependencies and installation |
| [03 — Shared types](03-shared-types.md) | Validated values, records and environment gates |
| [04 — Bus wire](04-bus-wire.md) | Framing, envelopes and command contracts |
| [05 — Broker and topics](05-broker-topics.md) | Routing, pub/sub and distribution |
| [06 — Properties](06-properties.md) | Read surfaces, namespaces, mutations and authorisation |
| [07 — Persistence and recovery](07-persistence-recovery.md) | Transactions, history, audit and reconciliation |
| [08 — Mesh and trust](08-mesh-trust.md) | Peer identity, transport and authority |
| [09 — Mix integration](09-mix-integration.md) | Language discovery and runtime contracts |
| [10 — Daemons and agents](10-daemon-agent-operation.md) | Lifecycle, observation, repair and improvement |
| [11 — Desktop](11-desktop.md) | Compositor, Wayland and control-plane boundaries |
| [12 — Toolkit and apps](12-toolkit-apps.md) | Design system, UI state and application integration |
| [13 — Conformance](13-conformance.md) | Evidence gates, priorities and release readiness |

Retained-detail profiles follow their parent chapters: daemon identity (10a),
repair/improvement (10b), package installation (10c), and design format (12a).
They preserve detailed intended rules, not implementation certification.

## Scope and stability

Bus names the protocol/library family. **ABP means Agent Bus Protocol**.
AMP is historical terminology, not a new spelling to introduce into contracts.
Existing historical identifiers and encoded data must not be mechanically renamed.

The chapter prefixes are reading order only. They do **not** replace legacy
`SPEC 07`, `SPEC 12`, `spec.get` numeric IDs, or `world.specs.NN` keys. Runtime
distribution needs an explicit compatibility plan before switching directories.

The canonical public home is `docs/spec/` in `markc/cosmix`, served under `/spec/`.
Private audit notes, original snapshots, deployment details and review transcripts
stay private. Chapter drafts evolve within this accepted baseline; do not treat unresolved
conflicts as permission to weaken an existing safety requirement.

Before v1.0 GA, expect revisions and occasional reorganisation. Stable requirement
IDs, source revisions, explicit dispositions and migration notes—not filenames—
provide continuity. See [authority](00-authority.md) and [conformance](13-conformance.md).

## Suite change record

This section is the suite-level change record required by GOV-002/003;
accepted meta-documents (the handover, the compatibility profile) carry their
own records. Chapter entries name every chapter whose version moved in the
suite revision.

- **0.3.4, amending suite-0.3.3 (2026-09-11)** — chapter 04 → 0.1.4.
  Correct BUS-013's orphaned subject and chapter date (0.1.3 was amended on
  September 11). **GOV-002 disposition:** all requirements are **retained**;
  this is an editorial correction with no wire or compatibility change.
  Chapter and suite-index HTML regenerated.

- **0.3.3, amending suite-0.3.2 (2026-09-11)** — chapter 04 → 0.1.3.
  **GOV-002 disposition:** BUS-014 is **clarified**: destination transport gates
  principal disclosure; TCP/mesh egress strips even genuine Unix sender stamps.
  BUS-013 endpoint publication is **changed** to advertise the actual bound path
  in ping, with explicit/config/discovered/system client precedence. Discovery
  remains untrusted until existing endpoint verification succeeds. Both profile
  requirements remain intended pending the full acceptance fixtures.
  Compatibility: ordinary TCP clients retain their wire contract and receive
  no identity metadata; the additive ping locator repairs dev-root discovery.
  BROKER-002/003 and BROKER-012 are **retained**; no legacy registration,
  response-ownership or loss-without-disconnect rule changes. HTML regenerated.

- **0.3.2, amending suite-0.3.1 (2026-09-10)** — native-session S0 contract:
  04 → 0.1.2 (Unix/WebSocket binding, trusted metadata, validation and exact
  proof bytes); 05 → 0.2.2 (allocation, key-based bootstrap/recovery, lifecycle,
  discovery, quotas, recipient policy and fixtures); 06 → 0.2.3 (trusted peer
  context and protected watch delivery); 10a → 0.2.2 (dynamic identity boundary).
  All additions remain **intended**, awaiting implementation and acceptance
  fixtures; no runtime or deployment evidence is added. Generated HTML follows
  these sources, including this suite index.
  **GOV-002 disposition:** BUS-013–017, BROKER-016–025, PROP-024/025 and
  DAEMON-PROFILE-002 are **added, intended, no change to checked requirements**.
  BROKER-005 is **clarified** only by an intended extension-advertisement note;
  its checked entries and wire shape are retained. BROKER-002/003 are **retained**
  as legacy name-to-connection contracts. BROKER-012's notification-loss-without-
  disconnect semantics are **retained for the native-session profile**: lifecycle
  notices are best effort, with a sticky gap indicator and lease-expiry backstop,
  no ACK/retry timer or slow-consumer disconnect.
  Recovery clarifications in this revision retain parent-key continuity within
  and across broker epochs, bounded/degraded wake registration, bounded recipient
  dependencies, and synchronous cached authorisation with checks outside resolution.
  Accepted design defaults are Unix ingress, default-open same-UID owner trust with opt-in restricted target
  policy, and Term-owned lifetime. Login-session roots and remote delegation
  are **deferred**. Compatibility: the reserved session-name shape and stronger
  parsing, proof, quota, lifecycle and observation rules apply to the new profile;
  existing citizens, TCP/WS registration outside that shape and D2 admission
  retain their checked contracts. Reserved-shape conflicts prevent activation,
  never evict a legacy citizen. No existing requirement IDs are renumbered or
  removed; no crate or existing wire version is bumped by this documentary stage.
- **0.3.1, amending suite-0.3.0** — traceability amendment from the GOV-008
  independent review (four-batch cold review + one consolidated fix round,
  2026-09-05; agent-prepared under the handover's article VII disposition).
  Change-record home defined here and in 00; late chapter bumps recorded for
  the suite-0.3.0 acceptance edits (00 → 0.2.1, 01 → 0.2.1); compatibility
  profile 0.2.0 → 0.2.1 (amends field, change record, post-acceptance framing);
  02 → 0.1.1 (evidence pin for the props-crate roles); 03 → 0.1.2 and
  06 → 0.2.2 (persisted-actor read-path error surface documented; file-count
  prose replaced by the commit pin; PropPath wildcard wording); 10a → 0.2.1
  (§9.3 pseudocode stamped as a frozen 1.4.4 snapshot); 10c → 0.1.2
  (staged-path conflict recorded; handover pointer); 13 → 0.1.2 (pointer to
  the accepted handover in the P1 row). BUS-004/BUS-008 normative phrasing
  retained as a recorded style disposition. No normative guarantee changed.
- **0.3.0, amending suite-0.2.1** — operator-approved authority transfer;
  constitutional dispositions recorded; identity registry reconciled
  (see [authority-handover](authority-handover.md)). Chapters edited in this
  revision: 00, 01 (acceptance references; versions bumped late, at 0.3.1),
  10a (registry 1.4.6 reconciliation), 10b (precedence note).
- **0.2.1 and earlier** — pre-acceptance candidate lineage; records live in
  the private audit ledger.
