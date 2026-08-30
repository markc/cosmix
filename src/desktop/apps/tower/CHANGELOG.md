# Changelog

## 0.5.2 — 2026-07-28

- Subscribe to CTK's local `theme.changed` lane through its dedicated inbox,
  outside Tower's retained atlas queues, for immediate cross-app convergence.

## 0.4.0 — 2026-07-25

- Animate fresh, declared local topology edges from live observation traffic
  with bounded intensity pulses and decay that disables itself once every edge
  is idle. Stale peer/citizen caches cannot authorise a specific pulse.
- Map ambiguous and undeclared traffic to an explicit local-node activity
  indicator rather than dropping it or fabricating topology.
- Persist named traffic filters, the current/last-active filter, topology
  pan/zoom and DCS pane visibility, pinning, width and active panels in an
  atomic, schema-versioned `state.conf.mix`.
- Add saved-filter create/select/delete controls. Filters referring to absent
  services remain valid and simply match no traffic.
- Keep graph membership and edges entirely inventory/peer-derived: persisted
  state contains view coordinates only, never topology data.
- Require an explicit state schema and load any sanitised catalogue read-only;
  retry failed writes and give pending state a bounded shutdown flush.

## 0.3.0 — 2026-07-24

- Add the event-driven Traffic pane over SPEC 02 §4.2
  `noded.observe.start`, `.event`, and `.stop`.
- Bound live scrollback and the independent pause buffer to 2048 events each,
  surface broker and client drop counts, and render at most 128 table rows.
- Add verb-glob, exact-service, direction and redacted-body controls; changing
  a filter fences the old subscription before starting its replacement.
- Subscribe only while the Traffic pane is visible, stop on close/exit, and
  create a fresh subscription after every CTK bridge reconnect without resume
  or polling.
- Run observation on CTK's dedicated third connection and dispatch stop over
  its reserved lane, independent of the bounded atlas queue. A wire outage is
  shown as stop-pending with broker disconnect cleanup named as the backstop;
  app shutdown gives stop a bounded best-effort flush.

## 0.2.0 — 2026-07-24

- Add the same-node citizen inspector over `app.describe`, `actions.*` and
  `app.controls.*`, retaining per-result observation times.
- Add confirmation-gated `action.invoke` and `app.controls.set`; control values
  are re-read only after a set or explicit refresh.
- Add per-node `cosmix-*.service` discovery and Start/Stop/Restart over bounded
  SSH argv workers, with confirmation dialogs for stop and restart.
- Fence confirmed app mutations to the selected process identity at dispatch,
  reject duplicate target operations while queued or in flight, and purge
  queued mutations when the citizen dies, changes identity, or is deselected.
- Revalidate in-flight mutations again when their replies arrive; stale
  replies are discarded without updating inspector results or re-reading a
  control. Citizens lacking both `pid` and `started_at` are identity-unknown
  and cannot alias across roster refreshes.
- Fence queued SSH work to its verified inventory epoch and a 30-second TTL;
  list all remote services without shell globs, then retain only strictly
  validated `cosmix-*.service` unit names locally.
- Bound inspector payloads, textual metadata, list cardinality and total
  rendered ECS entities; oversized or omitted data is reported explicitly.
- Require CTK's broker-stamped same-node provenance gate for app mutation.

## 0.1.0 — 2026-07-24

- Add the native CosMix Tower identity, metadata and CTK application shell.
- Add a read-only node atlas over existing `noded.inventory`, `noded.peers`,
  `noded.info`, `noded.list` and local property verbs.
- Add startup/reconnect bootstrap, explicit mesh refresh, bounded AMP fan-out
  and connection/refresh epoch rejection for stale responses.
- Subscribe to the existing local `world.noded`, `world.indexd`,
  `world.musicd` and `interact.props.changed` topics without recurring polling.
- Render one topology entity per inventory member and only the local peer edges
  explicitly returned by `noded.peers`.
- Preserve last-observed remote data as `unknown` after read failures and show
  `maild`/`webd` as namespace-required.
