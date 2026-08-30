# Changelog

## 0.3.1 — 2026-07-27

- Consume trayd state through one atomic `GetSnapshot` call and preserve
  manager-qualified daemon identities in menu actions.
- Watch trayd bus-name ownership, replacing stale state on daemon loss and
  requesting a fresh snapshot after acquisition or restart.
- Make the initial pending-view to SNI-handle transition atomic.
- Render partial manager discovery errors and stalled-refresh warnings without
  hiding successfully discovered services.

## 0.3.0 — 2026-07-27

- Turn `cosmix-tray` into a thin StatusNotifierItem skin that renders state from
  `cosmix-trayd` and sends only allowlisted application and service identities.
- Remove all local discovery and process-control dependencies from the skin.
  Menu-open refreshes and `Changed` signals now cross the published D-Bus
  boundary shared with the forthcoming Plasma plasmoid.
- Render trayd connection and method failures explicitly instead of falling
  back to discovery or leaving a stale-looking menu.

## 0.2.1 — 2026-07-26

- Replace the borrowed `network-workgroup` placeholder with the canonical
  Lucide-derived CosMix network mark under the exact `dev.cosmix.tray` hicolor
  name, using Breeze `ColorScheme-Text` recolouring.
- Add hand-tuned 16, 22 and 24 pixel status icons so the canonical two-unit
  stroke remains crisp in small Plasma panels, plus scalable status and launcher
  variants.
- Identify the SNI as `SystemServices`, populate its tooltip with explicitly
  menu-open-sampled local noded state, and keep the item `Active` without
  attention or overlay state.
- Reshape the menu with themed section/status icons, state-aware daemon actions
  and each application's real desktop-entry icon.
- Mark daemon state with state icons (`dialog-ok`, `dialog-cancel`,
  `dialog-error`) rather than the `media-playback-*` action icons, which made a
  running unit wear the same glyph as its own disabled Start item; a failed unit
  now also offers Stop, which is how its leftover processes are cleared.
- Stop publishing icon names the active theme cannot resolve. An unresolvable
  name renders as blank space, which is worse than the generic fallback it
  displaced; `kiconfinder6` reported NOTFOUND for three of the four discovered
  application icons. Resolution follows the active theme's inheritance chain
  through `freedesktop-icons`, because scanning every installed theme keeps an
  Adwaita-only name that renders blank under Breeze.
- Read the icon theme the way KConfig does, with the whole precedence stack
  applied lowest-first and the first immutability marker freezing the result.
  The stack was measured, not assumed — a distinct value written into every
  candidate file, then the winner deleted and re-read, until `kreadconfig6` had
  named the full order:

  ```
  /etc/kde5rc
  $XDG_CONFIG_DIRS[last..first]/system.kdeglobals
  $XDG_CONFIG_HOME/system.kdeglobals
  $XDG_CONFIG_DIRS[last..first]/kdeglobals
  $XDG_CONFIG_HOME/kdeglobals
  ```

  The whole `system.kdeglobals` family sits below the whole `kdeglobals` family
  rather than interleaving per directory, and `/etc/kde5rc` is the machine-wide
  floor. That last file is assembled at runtime inside KConfig, so it does not
  appear in `strings` on the shipped library and an earlier round of this work
  wrongly concluded it was unused; `strace` shows the `access("/etc/kde5rc")`
  and a file bind-mounted there is honoured.
- Honour all three of KDE's immutability levels — key (`Theme[$i]`), group
  (`[Icons][$i]`) and whole-file (`[$i]`) — including a lock that carries no
  value of its own, which is the shape a managed machine actually uses to pin a
  theme chosen by a lower system file. Key markers are read as a list, so
  `Theme[$i][en_AU]` and `Theme[en_AU][$i]` both keep their lock.
- Take the locale for localised entries from `[Locale] Language` in the same
  config stack, which is where KConfig takes it from — not from `LC_ALL`,
  `LC_MESSAGES` or `LANG`. With `Theme[en_AU]` present and `LC_ALL=en_AU.utf8`
  exported, KConfig still returns the untagged value; it only honours the tagged
  one once `Language=en_AU` appears in kdeglobals. Only the first entry of that
  colon-separated list selects an entry. An entry tagged for a locale that does
  not apply is wholly inert — it supplies neither a value nor a lock, so a stray
  `Theme[$i][fr_FR]` in a system file no longer freezes the stack for everyone
  else. Every expectation is transcribed from `kreadconfig6` runs against a
  scratch config tree rather than from the documentation. Config reads are
  bounded and refuse anything that is not a regular file, which keeps a FIFO
  from blocking the refresh worker.
- Do not cache icon lookups. The cache is process-global and stores misses as
  well as hits, so in a session-long daemon one lookup made before an
  application's icon was installed pinned that name as unresolvable for the rest
  of the session. A theme *installed* mid-session is still invisible until the
  tray restarts — `freedesktop-icons` builds its theme registry once per
  process — and that limitation is now recorded at the lookup.
- Report `activating`, `deactivating`, `reloading` and any future systemd
  `ActiveState` as a distinct `changing` state instead of folding them into
  `inactive`. Folding them told the operator a starting daemon was stopped and
  took away Stop — the one action that cancels a hung start.
- Rename the reachability probe and every string it feeds from "mesh" to
  "noded". Both probe targets terminate at the local noded, so with WireGuard
  down and every peer unreachable the old wording still claimed the mesh was
  reachable.

## 0.1.0 — 2026-07-24

- Add the windowless `dev.cosmix.tray` Plasma StatusNotifierItem.
- Rebuild an event-driven menu on open with noded reachability, installed
  CosMix applications, systemd daemon controls and live journal launchers.
- Keep the tray engine-free by using ksni's blocking API with its async-io zbus
  driver — no Bevy anywhere and no direct Tokio dependency or runtime (tokio
  appears transitively via cosmix-config's dep tree only).
- Resolve the broker listener through the canonical Mix strict-data node config
  and probe its WG address before the same-port loopback fallback.
- Move menu discovery off the D-Bus path into one menu-triggered worker, serving
  the last completed snapshot while a refresh is in flight.
- Launch apps and log terminals in collected `app.slice` transient user units,
  with executable preflight and captured `systemd-run` failures.
- Apply Freedesktop Exec quoting and field-code expansion, bound desktop-file
  discovery, and enforce one tray instance with an exclusive runtime flock.
