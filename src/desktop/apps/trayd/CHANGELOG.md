# Changelog

## 0.6.1 — 2026-08-15

- Treat GitHub's live-verified post-authentication forced-command rejection as
  a successful probe. Other forge-specific replies remain future work until
  their command paths can be verified against a real host.

## 0.6.0 — 2026-08-15

- Add an event-driven, symlink-safe SSH host and public-key catalogue with
  bounded parsing, Include/alias preflights, visible per-entry errors and
  cached `ssh-keygen` fingerprints.
- Add identity-only `ConnectSshHost` and asynchronous `ProbeSshHosts` actions,
  with an absolute trayd-resolved ssh path, four bounded probe workers, stale
  result fencing and connect-predictive first-contact host-key enrolment.
- Publish revision-consistent SSH snapshots, pressure properties, typed errors
  and `SshChanged` lifecycle notifications without timers or polling.
- Add identity-only SSH host creation, editing, trash, restore and purge with
  per-field OpenSSH-safe validation, catalogued-key enforcement, private
  exclusive creation and no-replace collision handling.
- Harden the pre-ship SSH plane with child-watch re-arming, bounded probe
  diagnostics, trusted helper resolution, fresh admission checks, accurate
  Include parsing, non-gating mode warnings and catalogue-cap enforcement.

## 0.5.0 — 2026-08-15

- BREAKING: replace UUID-backed script directories and strict-data sidecars
  with human-named, extensionless `~/.local/mix/<name>` executables whose
  filename is the D-Bus script identity; trash lives under `.trash/`.
- Create scripts with a Mix shebang, private executable mode and a discoverable
  leading `-- description: <text>` header while deriving timestamps from file
  metadata; free-text names are slugified into filename-safe script ids.
- Discover safe private files dropped directly into `~/.local/mix`, making the
  same catalogue usable from trayd and the operator's shell (`chmod 700` is
  required for shell execution of a drop-in), and migrate both legacy UUID
  directories and the intermediate flat `.mix` layout on startup.

## 0.4.2 — 2026-08-15

- Fix a launch race that made every Mix run die at systemd step FDS
  (status 202): the waiter thread captured only the run id, so the pinned
  script and working-directory descriptors were dropped when `start()`
  returned, before the user manager resolved `OpenFile=/proc/<pid>/fd/N`.
  The waiter now holds the whole run request until the unit exits.

## 0.4.1 — 2026-08-15

- Filter tombstoned inventory records out of the Bus roster at ingestion:
  they are closed leases kept for the trust layer, not live members, so the
  tray's node list and count show the live mesh only. Inactive members
  remain visible.

## 0.4.0 — 2026-08-15

- BREAKING: the D-Bus surface is Bus-named — `OpenBusSession`,
  `UpdateBusSession`, `KeepBusSessionAlive`, `CloseBusSession`,
  `RefreshBusRoster`, `GetBusSnapshot`, and the
  `dev.cosmix.trayd.Error.*Bus*` error names replace their Amp-named
  predecessors, and wire values say `bus` (renames landed 2026-08-06 in the
  AMP→Bus cutover but shipped unbumped; this release carries them). COS-lane
  methods (`GetSnapshot`, `Refresh`, `LaunchApp`, `ControlDaemon`,
  `OpenLogs`) are unchanged.

## 0.3.4 — 2026-07-29

- Unblock the zbus executor (dedicated executor task) so snapshot and
  traffic delivery cannot starve; fixes the frozen AMP tab.

## 0.3.3 — 2026-07-28

- Replace Mix catalogue watcher Boolean ownership with a generation hand-off
  that records catalogue operations arriving during failure publication and
  claims the replacement watcher without a lost wake.

## 0.3.2 — 2026-07-28

- Carry AMP filter epochs on snapshots and traffic batches so clients can
  fence in-flight rows from superseded filters, and require systemd Manager
  subscription before relying on `JobRemoved` reconnect edges.
- Carry each Mix run's next output sequence in snapshots, pin both script and
  working directory descriptors, and retry and surface transient-unit cleanup.
- Reconcile orphan Mix units before publishing the D-Bus name under a private
  singleton lock, and stabilise persistent inotify watches parent-first without
  a scan/watch creation gap.

## 0.3.1 — 2026-07-28

- Bound and UTF-8-frame the Mix runner event lane, account for publication
  queue loss, reconcile orphan transient units at startup, and stop units when
  post-spawn supervision setup fails.
- Make Mix store traversal symlink-safe with `openat2`, serialise run/catalogue
  mutations, validate purge entries before atomic tombstoning, and close the
  inotify scan/watch gap with event-driven re-arming.
- Replace sub-five-minute AMP reconnect clocks with systemd lifecycle edges
  plus a five-minute backstop; use exact lease deadlines and fence stale
  traffic on effective-filter and last-close transitions.
- Subscribe to D-Bus owner loss before publishing the trayd name and validate
  each caller again after lease insertion.

## 0.3.0 — 2026-07-28

- Add a trayd-owned, UUID-addressed Mix script catalogue under the user's
  XDG data directory, with lazy private directory creation, strict-data schema
  v1 metadata, symlink refusal and inotify-driven external-edit discovery.
- Add identity-only create, metadata update, edit, trash, restore and purge
  methods; script text remains in private files and external editing is routed
  through the hardcoded desktop opener.
- Run scripts through bounded transient systemd user units, with four-way
  concurrency, stop-by-run identity, 32 retained runs and separate bounded
  stdout/stderr tails.
- Publish revision-consistent Mix snapshots, typed errors, lifecycle signals
  and coalesced output batches capped at 16 chunks and 64 KiB.

## 0.2.0 — 2026-07-28

- Add sender-bound AMP leases with owner-loss cleanup, a ten-minute expiry
  backstop, and unioned direction, verb and redacted-body filters.
- Bridge verified `noded.inventory` membership and retained `world.noded`
  local services through a bounded, revision-consistent D-Bus snapshot.
- Run independently supervised roster and observation broker connections on a
  dedicated Tokio thread, with generation and subscription fences on reconnect,
  filter replacement and final close.
- Publish bounded, coalesced AMP traffic batches without polling or blocking
  the session D-Bus executor.

## 0.1.1 — 2026-07-27

- Query and merge both local systemd managers, retain manager scope in each
  daemon identity, and route controls and journal views to that exact manager.
- Add atomic `GetSnapshot` reads while retaining individual D-Bus properties.
- Coalesce overlapping refresh requests into one follow-up pass and expose a
  warning when a later request observes an implausibly long-running pass.
- Strengthen deployment identity tests across Cargo, D-Bus activation,
  introspection and systemd metadata.

## 0.1.0 — 2026-07-27

- Split state discovery and actions out of `cosmix-tray` into a session D-Bus
  daemon whose published interface is the contract for SNI and Plasma skins.
- Enforce the authority boundary in trayd: clients identify an application slug
  or discovered service unit, and can never supply an argv or command line.
- Ship D-Bus and systemd user activation metadata; bus-name ownership is the
  daemon singleton and the service lives for the graphical session.
- Remove remote SSH discovery and control. Trayd controls only the local systemd
  instance and warns when the retired `control_host` setting remains configured.
