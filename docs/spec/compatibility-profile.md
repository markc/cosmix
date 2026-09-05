---
title: Accepted pre-GA security and delivery compatibility profile
version: 0.2.1
amends: compat-0.2.0
status: accepted
date: 2026-09-05
---

# Accepted pre-GA security and delivery compatibility profile

Mark Constable authorised weakening the disputed security and delivery guarantees
to match current code on 2026-09-05. This is a scoped normative amendment to the
replacement candidate, not by itself acceptance of the entire suite or a
deployment change. (The whole suite was subsequently accepted the same day by
the [authority handover](authority-handover.md); this profile remains its
explicit accepted exception set, unexpanded.)

| Contract | Accepted guarantee | Superseded stronger guarantee |
| --- | --- | --- |
| [PROP-014](06-properties.md) | Base read plus secrets capability reveals secret values without another selector | Explicit per-request secret selection and default privileged redaction |
| [PROP-020 / STORE-010](07-persistence-recovery.md) | Best-effort live notifications; bounded retained replay can assist recovery | Contiguous live delivery, at-least-once delivery and durable per-subscriber acknowledgement cursors |
| [BROKER-012](05-broker-topics.md) | Full ordinary outbound queues drop the new message and retain the connection | Drop-oldest and mandatory slow-consumer disconnect in that lane |
| [STORE-008](07-persistence-recovery.md) | Independent HMAC over canonical value and nseq | Chained, complete-history or full-event-metadata authentication |
| [MESH-008](08-mesh-trust.md) | Genesis-derived boot trust; no persisted adopted multi-key anchors | Restart-persistent in-band multi-key trust adoption |
| [MESH-018](08-mesh-trust.md) | Never-verified compatibility roster fallback, subject to separate admission posture | Universal refusal of unverified remote routing |

These stronger guarantees are no longer current-profile conformance obligations.
They may return as separately approved, versioned enhancements. This is not an
automatic rule to ratify every bug or future regression as correct behaviour.

## Evidence and limits

The original source audit is pinned to `96d12fdf`. The relevant dispatcher and
authority changes through `4d2f1ebb77af51d8bbd08cb18f4e7070cebb58ac` were checked:
dispatcher behaviour is unchanged (only a test actor constructor changed), and
authority.rs is unchanged. Current mutation routing retains capability-based
secret inclusion. Source references remain in the linked chapters. This targeted
check does not advance the entire suite's baseline or certify a deployed system.

No permission checks, protected-topic rules, accepted-state rollback protection,
atomic record/history commits or enforced-admission gates are waived. No blanket
constitutional amendment, unrelated feature reduction, public cutover or runtime
ID remapping follows from this decision. Existing historical specs remain intact;
their conflicting clauses are superseded only within this named candidate profile.

Consumers needing stronger confidentiality, delivery or trust properties cannot
assume them: choose an appropriate additional mechanism or wait for an explicitly
specified stronger profile. In particular, a lost final event can remain unnoticed
without reconciliation, privileged read payloads can contain secrets, and an audit
digest alone cannot prove complete or untampered event history.

## Change record

- 0.2.1, amending compat-0.2.0: title/heading aligned; framing updated to note
  the same-day whole-suite acceptance by the authority handover. The six
  guarantees are unchanged and unexpanded.
- 0.2.0: first public publication (commit `3eadff30`) of the operator-accepted
  six-guarantee amendment; prior candidate lineage is recorded in the private
  audit ledger.
