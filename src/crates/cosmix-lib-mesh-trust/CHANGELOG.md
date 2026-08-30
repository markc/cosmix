# Changelog

## 0.10.0 — 2026-08-16

- Interpret optional active-member `noded_port` as the authoritative signed
  broker endpoint, resolving absence to the canonical default 4200.
- Reject an active Bus member unless a present port is a JSON integer in
  `1..=65535`; route-free member classes continue to ignore endpoint fields.
- Include the resolved port in the typed routing view and its JSON report.

## 0.9.0 — 2026-08-16

- Whole-view-reject member names that fail the SPEC 01 §4.1 Bus-label grammar
  for every status before the authority baseline ratchets.
- Source the grammar from `cosmix_bus::bus::is_valid_label` as the single
  fleet-wide label authority.

## 0.8.0 — 2026-08-15

- Refuse generation-silent normal inventories after a node has advanced above
  recovery generation zero, closing replay of a pre-recovery higher-epoch
  payload after a lower-epoch recovery.
- Preserve the clean migration path: generation-silent normal inventories
  remain valid while the cached recovery generation is zero.
- Add the typed `NormalMissingRecoveryGeneration` verification error.
- Refuse an explicit `recovery:false`: the recovery key is presence-marked and
  normal payloads must omit it.

## 0.4.3 — 2026-07-23

- Add the canonical `ctk.actions` capability token, exact grant helper, and
  capability-set predicate for remote CTK application-action invocation.
- Document the local-default, remote-fail-closed posture pending authenticated
  non-wire-assertable caller provenance.
