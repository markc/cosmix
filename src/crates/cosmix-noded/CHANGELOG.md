# Changelog

## 0.13.0 — 2026-08-16

- Reconcile outbound mesh transports against each signed-routing reload before
  publishing the new authority, retiring revoked generations and failing their
  pending RPCs immediately.
- Fence queued mesh ingress by peer and connection generation through final
  local delivery, and report reload, retirement, in-flight fate, connect
  timeout, and listener divergence/convergence as structured events.
- Bound shared WebSocket connection attempts to five seconds and make an
  unexpected broker task exit fail the daemon process for systemd restart.

## 0.12.0 — 2026-08-16

- Route verified ActiveBus members to their signed `mesh_ip:noded_port`
  endpoint, retaining 4200 only as the signed-schema default when absent.
- Report signed ports through `noded.peers`, compare `/etc` hints against the
  signed value, and warn without disabling routing when the signed self port
  differs from the listener actually bound.

## 0.11.0 — 2026-08-16

- Cut verified cross-node routing over to signed ActiveBus membership and
  signed mesh addresses, excluding the canonical local node by name and using
  the fleet-default broker port until signed endpoints land.
- Retain the derived `/etc` roster, including custom ports, only as the
  explicitly-labelled fallback for a never-Verified boot.
- Publish authority posture and routes as one hot-swapped snapshot, report the
  active table through `noded.peers`, and bind each dispatched message to one
  resolved target.
- Report the verified local member classification as `authority.self` in
  `noded.peers`, including why a signed table is empty after local removal.
- Warn when a signed ActiveBus member has a non-default `/etc` broker port that
  signed routing must ignore until endpoints become authoritative.
- Make mesh connection reuse endpoint-aware and generation-safe without
  changing the existing slice-2 session-grandfathering contract.

## 0.10.0 — 2026-08-15

- Refuse generation-silent normal inventories once the persisted recovery
  generation is above zero, while retaining generation-zero compatibility for
  the fleet migration.
- Surface the typed mesh-trust verification reason through the existing
  fail-closed authority posture.

## 0.7.0 — 2026-07-24

- Add the SPEC 02 §4.2 `noded.observe.start`, `noded.observe.stop`, and
  `noded.observe.event` operator surface and advertise
  `extensions.observe = "1.0"`.
- Gate observation on a registered same-node connection plus anchored
  `[observe].allowed_services` patterns. The default remains empty and
  fail-closed.
- Add per-subscription drop-oldest rings, count/byte/filter limits, monotonic
  sequence and drop accounting, stop/disconnect fences, recursive broker-side
  redaction, payload omission, and original-correlation preservation.
- Keep the zero-subscriber route cost to one relaxed atomic load end-to-end,
  including mesh wire retention/request clones and broker-command
  canonicalisation; match borrowed metadata before cloning, render outside
  the observation mutex, and batch-drain up to 1 MiB per subscription on each
  2 ms wake.
- Apply a conservative whole-payload policy table before the recursive
  denylist for registration/admission, auth, login, and token verb families.
- Observe rejected broker commands, admission and orphan responses, synthetic
  route-error responses, and enforce-refused registrations with canonical
  connection identities and truthful outcomes.
- Migrate the built-in AMP logger from deprecated `noded.tap` to a dedicated
  metadata-only `log-observe-<pid>` subscription. Operators enabling the
  built-in logger must allow `log-observe-*`.
- This is `0.7.0`, rather than a `0.6.x` patch, because it adds a public
  broker extension and operator configuration surface.

## 0.6.15 — 2026-07-24

- Complete the SPEC 02 §4.3 invariant across every citizen delivery path.
  Topic inner envelopes are stripped and stamped from the publisher socket
  before live fan-out or retention, so replay preserves publish-time origin.
  Correlated local and remote-mesh responses are likewise stripped and
  restamped from the responder delivery class before forwarding.

## 0.6.14 — 2026-07-24

- Introduce broker-owned origin stamping on direct service request/event
  delivery with
  `broker_origin: local|mesh`, stripping and overwriting all client-supplied
  spellings. Locality is derived from the connection source using noded's
  existing loopback/own-bind-IP classifier.
- This is the SPEC 02 §4.3 recipient-side provenance marker; gated consumers
  deliberately fail closed when used with an older broker.
