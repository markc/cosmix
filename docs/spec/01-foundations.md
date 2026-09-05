---
title: Foundational principles
chapter: 1
version: 0.2.1
status: draft
date: 2026-09-05
---

# Foundational principles

These principles carry forward the ownership and safety intent of the historical
constitution and architectural index. Enforcement status is separate: this audit
does not establish that a complete constitutional policy engine exists.

## Ownership and legibility

**FOUND-001 — Operator-owned state.** Canonical application data and credentials
remain under operator control. External replication, credentials handling and
remote actions require the configured authority for that deployment and task.
An available network connection is not permission to send private data.

**FOUND-002 — Reconstructible operation.** Interfaces, configuration and state
transitions must be inspectable through documented contracts. Discovery should
expose enough structure for both people and agents to act without guessing.
Private production values are not part of a public protocol specification.

## Architecture

**FOUND-003 — State and presentation have different owners.** Protocol/substrate
state is authoritative for its domain. UI views derive from it. Compositor-local
frame and buffer state stays with the compositor; a remote control interface does
not become a rendering dependency.

**FOUND-004 — Keep Bus out of the frame path.** Rendering and input-critical
mechanisms must not require a broker round trip. Bus carries control, observation
and policy, not per-frame pixels. See [desktop](11-desktop.md).

**FOUND-005 — Mechanism and policy remain separable.** Put reusable domain logic
in libraries, expose it through typed daemon contracts, and orchestrate external
processes with Mix. Do not encode deployment-specific policy into shared types.

**FOUND-006 — Coherent defaults, explicit alternatives.** Support standard
Wayland contracts where applicable and a coherent default toolkit. Alternative
clients are allowed through explicit interfaces, not by duplicating authority.
Trusted, security-sensitive surfaces require a stronger boundary than decorative
or replaceable desktop furniture.

**FOUND-007 — Evidence before architectural expansion.** Prefer event-driven
wakeups and measured idle cost. Additional renderers, bypass paths and orchestration
layers need a demonstrated requirement and tests of the new boundary. Aspirational
canvas/places designs are not proof of existing runtime functionality.

## Safety and autonomy

**FOUND-008 — Authorisation is separate from parsing.** A well-formed path, actor
token or capability string does not prove existence, identity, permission or current
state. Check these at the responsible environment/transaction boundary.

**FOUND-009 — Fail safely without collateral rollback.** Scope edits and rollback
to the authorised operation. Preserve unrelated work and durable records. Never use
a whole-tree reset as an autonomous error handler. Data and schema changes require
an explicit recovery plan; a Git revert alone cannot reverse external effects.

**FOUND-010 — Autonomy cannot waive correctness.** The operator may configure
unattended work or human checkpoints. Either mode must preserve authenticated
identity, permission checks, bounded resource use, race-safe state transitions and
auditable outcomes. Human ceremony is not a substitute for technical enforcement.

**FOUND-011 — No invented enforcement.** Historical T0–T3 tiers, mandatory Git
trailers, drift breakers, constitutional path maps and crisis machinery must not
be advertised as active protections without corresponding configuration, code and
execution evidence. Their policy status requires reconciliation with current
working agreements before adoption.

## Policy disposition

The old constitution prescribed exact dependency versions, a fixed mesh subnet,
specific external-action prohibitions and human-only amendment paths. Current
source and operating agreements differ in places. This edition preserves their
sovereignty/safety intent but does not quietly ratify replacements for those exact
rules. Their detailed disposition is now recorded in the accepted
[authority handover](authority-handover.md). Acceptance of the development baseline
does not certify dependency compliance or enable historical harness machinery.
