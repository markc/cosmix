---
title: Cosmix specification suite
version: 0.2.1
status: draft
date: 2026-09-05
---

# Cosmix specifications

This is the replacement specification candidate, audited against source commit
`96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be`. It is not a claim that every
requirement is implemented, tested, deployed or accepted. Publication does not
resolve the discrepancies recorded in individual chapters.

The [security and delivery compatibility amendment](compatibility-profile.md)
is accepted: its listed guarantees now match the implementation. The suite as a
whole remains draft; other conflicts and publication gates remain open.

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

The intended public home is `docs/spec/` in `markc/cosmix`, served under `/spec/`.
Private audit notes, original snapshots, deployment details and review transcripts
stay private. While this candidate remains draft, do not treat its unresolved
conflicts as permission to weaken an existing safety requirement.

Before v1.0 GA, expect revisions and occasional reorganisation. Stable requirement
IDs, source revisions, explicit dispositions and migration notes—not filenames—
provide continuity. See [authority](00-authority.md) and [conformance](13-conformance.md).
