---
title: Authority, evidence and change
chapter: 0
version: 0.2.1
status: draft
date: 2026-09-05
---

# Authority, evidence and change

## What this suite means

**GOV-001 — Separate intent from evidence.** A requirement states intended
behaviour. An implementation note states observed source behaviour at a named
revision. Tests demonstrate only their exercised cases. Deployment needs separate
evidence. None of these may stand in for the others.

The baseline for this edition is commit
`96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be` of `markc/cosmix`. The source
audit did not execute the Rust test suites or inspect production installations.
The complete 47-file committed delta through
`4d2f1ebb77af51d8bbd08cb18f4e7070cebb58ac` was subsequently reconciled.
Unchanged source references retain their original pin; changed behaviour names
the newer revision. Uncommitted work is excluded. This is source coverage, not
a fresh correctness proof or test/deployment attestation.

| Label | Meaning |
| --- | --- |
| Implemented / source-backed | Relevant code exists at the cited revision; no fresh execution implied |
| Tested | A named command/test, revision, environment and result are recorded |
| Partial | Some specified behaviour exists; the missing portion is identified |
| Proposed / deferred | Intended future work, not a current capability |
| Conflict / discrepancy | Source and an existing requirement disagree; neither is silently discarded |
| Historical / superseded | Retained for traceability, not an instruction to recreate abandoned architecture |
| Unverified | Available evidence is insufficient |

Chapter `status: draft` applies to the replacement document. It does not invalidate
previously accepted requirements quoted within it. Mark Constable is the acceptance
authority for this pre-GA refactor. Acceptance must explicitly disposition changes
to those requirements; drafting and reviewing agents cannot ratify their own
normative amendments.

Suite 0.3.0 is now accepted as the canonical pre-GA baseline. The
[authority handover](authority-handover.md) records the constitutional dispositions,
protected-target mapping and limits. It controls over historical procedures copied
into retained profiles; technical evidence labels remain unchanged.

## Authority and change control

**GOV-002 — No silent normative changes.** Every changed MUST, wire value,
identity rule, trust assumption, persistence guarantee or lifecycle transition
needs an explicit retained/clarified/changed/deferred/superseded disposition and
compatibility analysis. Editing prose is not evidence that a discrepancy is fixed.

**GOV-003 — Stable references.** Requirement IDs survive file moves. Removed IDs
remain tombstoned in the change record and are never reassigned. Chapter numbers
are editorial; legacy runtime identifiers require their own versioned registry.

**GOV-004 — One canonical suite after acceptance.** Public-safe architecture and
contracts belong in `markc/cosmix/docs/spec/`, served at `/spec/` on cosmix.dev.
Private overlays may configure a deployment,
record private decisions or restrict actions, but must link to—not fork—the
public contract. Private archives preserve historical evidence without competing
for current authority.

Current authorised task instructions and repository working agreements govern
what an implementation agent may do. A historical specification is not authority
to destroy unrelated working-tree changes, reveal secrets or expand task scope.
Conflicting project mandates must be surfaced rather than silently reconciled.

## Names

**GOV-005 — Naming is not protocol migration.** Use Cosmix for the project,
Bus for the family, ABP for Agent Bus Protocol, and Mix for the language/shell.
The August 2026 AMP-to-Bus crate/path rename and ABP expansion correction are
separate changes. The prose correction alone changed no wire encoding/version.
Historical quotations, old commit paths and genuine identifiers retain their names.

## Evidence and publication gates

**GOV-006 — Reviewable evidence.** Every substantive current-state claim names
the relevant source module and baseline. Claims of tests passing must include the
actual run, not copy a previous session's report. Unknowns remain visible.

**GOV-007 — Public sanitisation.** Public material must exclude credentials,
private host/address/domain mappings, operator home paths, deployment overlays,
private review transcripts and operational logs. Use synthetic examples. A public
copy must be independently screened; moving a private document is not sanitisation.

**GOV-008 — Bounded review, explicit residuals.** Review the complete candidate
once independently. Apply one consolidated substantive-fix round. Verify only the
specific fixes in the final check. An unresolved blocker stays a blocker; the
review limit is not permission to publish a known-bad normative contract.

## Pre-GA maintenance

The suite version governs publication; chapter versions identify changed documents
within it: any committed content change to a chapter bumps that chapter's
version in the same suite revision, and the suite change record in the README
names every chapter that moved (accepted meta-documents carry their own
records). Neither substitutes for crate, wire or storage versions.
(0.2.1 records this chapter's suite-0.3.0 acceptance edits, whose bump was
applied late — at suite 0.3.1 — and acknowledged there.)
Use patch revisions for corrections without behavioural change; explain any
contract change and its compatibility impact explicitly, regardless of version
number. Before GA, a minor revision can reorganise chapters, but must preserve ID
mapping and accepted guarantees. Review drift when affected code changes; perform
a whole-suite checkpoint before release milestones or another broad refactor.
