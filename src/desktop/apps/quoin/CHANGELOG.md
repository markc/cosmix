# Changelog

## 0.5.0 — 2026-09-02

- Ship the Bus service `shell`: open read surface (`shell.ping`, `shell.info`,
  `shell.props.*`) plus locally-authorized semantic panel/page verbs, and the
  power page fed by powerd telemetry (snapshot + `power.props.changed`,
  rendered honestly — missing values are never shown as zero or as a confident
  "No system battery").
- Power telemetry self-heals on a healthy connection: a change or gap arriving
  while unavailable restarts the sync keyed on the message's own connection
  generation, and a stale-sequence change while live is treated as a possible
  powerd restart and triggers a re-snapshot (state-driven; no timers).
- Full outbound channels defer rather than drop: an unsendable snapshot
  request or reply is retried on a later update instead of dead-ending the
  display or leaving the peer hanging. A DEAD worker is not the same case and
  is no longer treated as one — nothing will ever drain that queue and no
  `Fatal` event can arrive to clear a stash, so the reply is dropped with a
  warning and the power display settles on "Power unavailable" instead of
  re-firing dead sends every frame for the life of the process.
