---
title: Conformance and delivery gates
chapter: 13
version: 0.1.2
status: draft
date: 2026-09-05
---

# Conformance and delivery gates

## Evidence record

**CONF-001 — Claims need reproducible scope.** A conformance record identifies:
requirement IDs; source commit; dirty-tree status; compiler/runtime version;
features/target; command; result; relevant test names; and remaining limitations.
Source inspection, a test present in the tree, a passing test run and a deployed
probe are four different evidence levels.

**CONF-002 — Publish differences.** Keep discrepancy rows until an accepted spec
amendment or verified implementation closes them. A broad label such as “L4” or
“implemented” cannot override a missing query option or security gate.

## Delivery sequence

| Priority | Deliverable | Exit gate |
| --- | --- | --- |
| P0 | Source-pinned current-state ledger and section map | Every old chapter/heading has a destination or explicit retained historical disposition |
| P0 | Replacement draft and public sanitisation | Complete reading order; stable IDs; no private values; conflicts visible |
| P0 | Independent review and one fix round | No unresolved blocker presented as an accepted guarantee; targeted fix verification recorded |
| P1 | Public website integration, initially draft-only | Links/routes generated and tested; old references remain traceable; no authority change from publication alone |
| P1 | Runtime spec-distribution migration | Explicit legacy ID registry; compatibility test; configured directory and publication lifecycle verified—no silent numeric reassignment |
| P1 | Authority cutover, accepted by Mark — the documentation half was accepted 2026-09-05 ([authority handover](authority-handover.md)); the runtime-identity work in this row remains per HANDOVER-004 | Prior runtime identity gate passed or old runtime suite explicitly labelled historical for every consumer; baseline-to-HEAD changes dispositioned; private suite becomes archive/pointer only |
| P1 | Security/correctness discrepancy work | Lexical Actor/Capability change reconciled at 0.3.0; provenance, owner policy, supported-operation rejection and contextual/race tests remain; cursorless replay discrepancy explicitly dispositioned |
| P2 | Reliability and breadth | Accepted secrets/queue/delivery/audit limitations accurately exposed; malformed-history recovery, daemon adoption and desktop context gaps reconciled |
| Pre-GA | Release conformance matrix | Main and desktop gates, supported deployment probes, compatibility/migration evidence and whole-suite review |

The relative order of individual implementation fixes depends on exploitability
and affected consumers, not merely document order. This refactor does not authorise
unrelated Rust changes or production deployments.

## Documentation cutover

**CONF-003 — Keep a recoverable migration.** Preserve the old suite and its
history privately. Publish only fresh sanitised documents. Maintain a private
old-heading → new-requirement/disposition map. Update entry points and working
agreements together; do not leave two suites claiming current authority.

**CONF-004 — Runtime IDs are a separate gate.** Baseline noded discovery recognises
`NN_*.md` filenames and searches legacy `_spec` locations unless configured.
Reordered `NN-*.md` public files are deliberately not legacy numeric entries.
An explicit directory permits exact-name retrieval, but does not provide automatic
numeric discovery, deployment or live updates. Do not point production numeric
consumers at the new suite until identity mapping is designed and tested.

**CONF-005 — Concurrent code is reconciled, not overwritten.** Audit a fixed
commit, then record later relevant commits before claiming current coverage.
Uncommitted edits are excluded unless explicitly reviewed as a separate snapshot.
Documentation integration must preserve another session's source and index changes.

## Required checks

- Unique requirement definitions, consistent chapter metadata and valid local links.
- Complete old-file and heading inventory; no missing MUST silently summarised away.
- Pinned public source references resolving to real paths at the baseline.
- Public hygiene gate on the exact publishable diff, without weakening its rules.
- Website generator/router agreement and repeatable generated output.
- No source-code changes or implied live deployment from this documentation task.
- Review findings and residuals recorded privately; public limitations remain visible.

No fresh Rust test run is asserted by this edition. The private audit records
which documentation checks and reviews actually ran.
