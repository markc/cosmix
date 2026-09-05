---
title: Accepted pre-GA authority handover
version: 0.3.0
amends: suite-0.2.1
status: accepted
date: 2026-09-05
---

# Accepted pre-GA authority handover

Mark Constable authorised proceeding with the outstanding authority decisions on
2026-09-05. This records that operator decision; the document and commit are
agent-prepared, not represented as human-authored constitutional amendments.

**HANDOVER-001 — Canonical home.** `docs/spec/` is now the canonical public
architecture and development specification suite. The private dated originals
and frozen refactor snapshot are historical evidence, not competing specifications.
They remain intact. Public requirement IDs and the explicit dispositions below
govern new work; reading-order numbers do not reassign legacy runtime IDs.

**HANDOVER-002 — Acceptance is not certification.** Suite 0.3.0 is accepted as
the pre-GA development baseline, with its existing source pins and evidence labels.
Chapter `draft` labels continue to identify evolving detail. Proposed, deferred,
partial and unverified features do not become implemented, tested or deployed
through acceptance. Unresolved technical discrepancies remain conformance work,
not implicit permission to weaken a requirement. The
[compatibility profile](compatibility-profile.md) remains the explicit accepted
exception for its six security/delivery guarantees.

## Constitutional disposition

This table replaces the historical constitution's role as the project's current
specification-level authority. It does not edit or claim procedural compliance
with that document's human-authored-commit requirement. Current task instructions
and repository working agreements govern agent actions; no specification grants
permission to publish secrets or perform unrelated destructive work.

| Historical article | Accepted disposition |
| --- | --- |
| I — ownership/data | Retain operator-controlled canonical state and explicit authority for external replication and credential operations under FOUND-001/002 and GOV-007. Configuration syntax is deployment-specific, not necessarily TOML. This handover grants no credential or federation action. |
| II — code/dependencies | Retain Rust/Mix stack language intent, open-source provenance and reviewed dependency obligations under FOUND-005/007 and composition. Historical repository paths are logical scope references. Dependency pinning discrepancies remain visible compliance work; no new dependency or runtime language is approved here. |
| III — mesh/trust | Use the mesh chapter and accepted compatibility profile for actual admission, routing and trust guarantees. Subnet shape is deployment configuration, not proof of authentication. Retain explicit authorisation for public exposure and operator-controlled scheduling; no network or scheduler change follows. |
| IV — autonomy/consent | Current operator instructions and repository working agreements replace the historical mandatory branch/tier matrix for interactive agent work. Preserve scoped authorisation, withdrawal of consent, honest authorship and auditable changes. Old harness tiers, trailers and drift breakers are retained design material, not claimed active controls or newly enabled autonomy. |
| V — reversibility/audit | FOUND-009/011 and the persistence chapter control: verify the affected boundary, preserve unrelated work, and plan recovery for external effects. Git revert is not universal rollback. Historical crisis automation and LKG mechanisms remain proposed unless separately evidenced; do not claim they are deployed. |
| VI — protected targets | Public policy successors are authority, foundations, this handover, the compatibility profile and the repair/improvement profile. The legacy constitution, declaration and changelog remain preserved historical targets. Core daemons, dependencies, service units and autonomy configuration retain sensitive scope: explicit task authority is required, never inferred from a document rename. This approval covers the documentation handover, not unrestricted future changes to those targets. |
| VII — amendments | GOV-002/003/008 replace the old human-authored-only commit ceremony: operator decisions may be recorded and implemented by agents with honest attribution, explicit dispositions, version/prior-version references and a change record. Agents cannot ratify their own normative changes. Federation of amendment authority is not approved. |

Where retained profiles reproduce contradictory historical tier gates or amendment
procedures, this table takes precedence. Their technical recovery, integrity and
identity requirements are not discarded merely because the old governance
machinery is no longer the working agreement.

## Identity registry disposition

**HANDOVER-003 — Preserve existing assignments.** The identity profile's summary,
Appendix A and projection example now reconcile the already committed registry
1.4.6: powerd 519 (introduced in `52482560`) and mprisd 520 (`6c4d56df`).
They are session services, with reserved fixed identities; their user units do
not prove execution under those numeric accounts. Existing interaction service
name `interact` remains an explicit exception; the added Bus names are `power`
and `mpris`, not inferred `powerd` and `mprisd`.

The next unassigned daemon/shared number is 521, subject to both streams'
non-collision rules and host preflight. No account is created, removed, renumbered
or reclaimed. Observability service identities remain reserved despite retirement
of a deployment node. Generator availability and full live account equality are
not certified by reconciling the source tables.

## Runtime and publication are separate versions

**HANDOVER-004 — No silent runtime promotion.** The immutable runtime candidate
`spec-candidate-1245b44b-v1` remains pinned to its original 20 documents and
preparation-only manifest. It does not contain this handover or the corrected
registry. Web/Git readers use the canonical suite; runtime readers must inspect
their release identity and must not mistake that old candidate for suite 0.3.0.
A new manifest and explicit deployment are required to update runtime retrieval.
No legacy numeric alias, retained topic or fleet configuration changes here.

## Change record

- 0.3.0, amending suite 0.2.1: operator-approved authority transfer; constitutional
  dispositions recorded; existing UID assignments reconciled. No crate, wire,
  storage or installed release version is changed.
- The earlier six-row compatibility amendment remains in force without expansion.
