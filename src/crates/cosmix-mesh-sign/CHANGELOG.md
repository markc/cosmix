# Changelog

## 0.7.3 — 2026-08-16

- Refuse signing when an active Bus member carries a `noded_port` outside the
  shared strict JSON-integer `1..=65535` contract.
- Include each resolved ActiveBus `noded_port` in `verify --json` routing-view
  output, including the default 4200 for inventories that omit it.

## 0.7.2 — 2026-08-16

- Refuse inventories whose member names are not valid SPEC 01 §4.1 Bus labels
  before signing, using the shared fleet-wide label grammar.

## 0.7.1 — 2026-08-15

- Add `sign --recovery-generation <N>` to stamp the current recovery floor on
  a normal payload without setting `recovery:true`.
- Refuse recovery-generation values beyond Mix's exact-integer range and make
  `--recovery-generation` mutually exclusive with `--recovery`.
- Report both effective and payload-carried recovery generations in JSON and
  human output, and warn that `--recovery` cannot check fleet epoch history.
- Warn when normal signing emits a legacy generation-silent payload, and note
  on human verification that such a payload was checked only against the
  command's synthetic generation-zero state.
