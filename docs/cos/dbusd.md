# cosmix-dbusd — the D-Bus boundary daemon

**`cosmix-dbusd` is the one cosmix process that speaks D-Bus.** Per the
ADR *D-Bus is a foreign protocol — only `cosmix-dbusd` adapters speak it*
(2026-09-18), every other cosmix app and daemon talks ABP on the Bus; the
D-Bus world is reached — in both directions — through per-domain adapters
hosted here, the same way IMAP and HTTP live at the edge.

## What it does

- Hosts one adapter per D-Bus domain. An adapter owns exactly one Bus
  service (registered under its domain name: `notify`, `tray`, …) and one
  zbus connection for its whole run — both locals of its `run`, so an
  adapter that returns, fails or panics drops them and withdraws its
  names. Fault containment falls out of ownership: one adapter's death
  never touches another adapter or the daemon. One language limit: a
  panic inside `Drop` during a panic unwind aborts the whole process —
  containment covers one fault, not a second one during cleanup.
- Supervises every adapter in its own tokio task, observing the
  JoinHandle: a panic (`JoinError::is_panic`), an `Err` return, or an
  unexpected `Ok` return all record a failure, put the adapter in
  `backoff`, and relaunch it after an exponential schedule (1 s doubling
  to a 60 s cap; five healthy minutes reset the escalation). A
  consistently failing adapter stays in `backoff` forever — it never
  takes the daemon down. A run that never yields cannot be preempted:
  it is reported `stuck`, its Bus service and D-Bus names may stay held
  until the process restarts, and its lifecycle keeps answering
  commands.
- Dials the desktop session bus from `DBUS_SESSION_BUS_ADDRESS` per
  adapter. Unset or unreachable means that adapter sits in `backoff`
  with that reason; the daemon stays up and keeps answering `dbusd.*`.
- Serves the `dbusd` Bus control service (mesh-open; no caller
  authorization on any verb):
  - `dbusd.adapters` — every adapter's `{name, service, state, restarts,
    leaked_runs, last_error, since}` and the session-bus endpoint state.
  - `dbusd.adapter.restart {name}` / `dbusd.adapter.enable {name}` /
    `dbusd.adapter.disable {name}` — disable stops the adapter and
    releases its names (how a human hands, say, the Notifications name
    back to another daemon); enable and restart relaunch it. Unknown
    names get a refusal reply, never a panic.
  - `dbusd.props.{get,list,describe,watch}` — the uniform props surface
    over `dbusd.adapters.<name>.state|restarts|leaked_runs|last_error`.
  - `dbusd.ping`, `dbusd.info`.
- Publishes state changes as they happen — one `dbusd.adapter.changed`
  event plus `dbusd.props.changed` diffs per transition, both stamped
  with a per-daemon-session monotonic `event_seq`. Events can be
  dropped under backlog: a gap in `event_seq` says so, and the props
  (`dbusd.props.get`) are the truth. No polling anywhere: supervision
  is JoinHandle- and signal-driven.

Configuration is `~/.config/cosmix/dbusd.conf.mix`
(`enabled: ["notify", "tray"]`); an absent `enabled` means every
built-in adapter, an empty list means none. An absent file materialises
the defaults on disk; a file that exists but cannot be read or parsed
is fatal — the daemon exits rather than silently guessing the adapter
set. Unknown names are logged and ignored. Enable/disable verbs are
runtime-only — the config file is the persistent source.

## Adapter status

| state | meaning |
|---|---|
| `starting` | launched; has not yet reported serving |
| `running` | serving its domain |
| `backoff` | failed (error/panic/exit); waiting out the schedule |
| `disabled` | stopped by config or verb; names released |
| `stuck` | the run did not stop within the abort window — it may be CPU-bound or never yielding; its names may stay held until the process restarts |

`restarts` counts every relaunch in this daemon process (backoff
restarts, the restart verb, enable-after-disable). `last_error` is the
most recent failure reason, cleared when the adapter runs healthy
again. `leaked_runs` counts runs detached as `stuck` in this daemon
process and never resets: the next healthy run clears `last_error`,
but the leaked spinner may still be holding names and burning a
worker — the count is its visible trace.

## Running it

```sh
/opt/cosmix/bin/cosmix-dbusd serve
```

The supplied `cosmix-dbusd.service` is a systemd user unit: the Bus
citizen belongs to the graphical session, and the adapters dial the
desktop session bus (`unix:path=/run/cosmix-desk-dbus/bus`, set
explicitly because systemd user units do not inherit the session
environment). SIGTERM stops every adapter (releasing its D-Bus names)
and exits 0.

## Writing an adapter

An adapter implements the `Adapter` trait — `name()`, `bus_service()`,
and `async run(ctx)` — and registers as one line in
`citizen::builtin_adapters()`:

```rust,ignore
adapter_spec::<NotifyAdapter>(),
```

The context carries the session-bus endpoint, a per-launch stop signal,
a ready channel, and (under the `cosmix` feature) helpers that open the
adapter's own Bus connection and its own zbus connection. Keeping both
as locals of `run` is the whole containment story. The crate's core
(supervisor, backoff, registry, config) builds without the `cosmix`
feature and is unit-tested with a scripted fault adapter compiled only
under `cfg(test)`.

The `tray` adapter (`src/adapters/tray.rs`, documented above) is the
worked example; `notify` and `settings` follow, then folding mprisd and
powerd.

## The tray adapter

Cosmix is the system-tray **host**. The `tray` adapter owns
`org.kde.StatusNotifierWatcher` at `/StatusNotifierWatcher` on the
desktop session bus and turns StatusNotifierItems into an ABP domain.
If the watcher name is already owned when the adapter starts, the run
**fails with a clear error rather than replacing the owner** — a human
hands the name over by `dbusd.adapter.disable {name: "tray"}` on this
side and `enable` once the other owner has released it.

D-Bus side (inbound, per the SNI convention):

- `RegisterStatusNotifierItem` accepts both forms: a bus name (item at
  `/StatusNotifierItem`) or an object path (item on the caller's own
  connection — resolved the way KDE/libappindicator do). Everything
  checkable synchronously is refused with a D-Bus error, never an OK
  reply followed by a silent drop: `InvalidArgs` for a malformed name
  or path, `AccessDenied` unless the caller owns the bus name it
  registers (one client cannot squat the tray with other apps' items),
  `Failed` for an unowned name or a slow owner resolution (bounded at
  250 ms — the resolution runs in the interface on zbus's dispatch
  task, never in the run loop, so a flood of item signals cannot wedge
  the adapter behind it), `LimitsExceeded` past 64 items total or 8
  items per registering connection. Re-registering an already-tracked
  `(service, path)` replaces it — always allowed, even at the caps.
- `RegisterStatusNotifierHost` records the host (cosmix itself already
  registered as host at startup, which is what makes
  `IsStatusNotifierHostRegistered` true the whole time the adapter
  runs) and a first-time registration emits
  `StatusNotifierHostRegistered`. Host registrations are capped at 64
  and pruned when their connection vanishes. `ProtocolVersion` is 0.
- Item identity is the `(service, path)` pair: one connection may host
  several path-form indicators, each tracked and advertised
  separately. `RegisteredStatusNotifierItems` — and the
  `StatusNotifierItemRegistered` / `StatusNotifierItemUnregistered`
  signals — carry `service + path` for every item (e.g.
  `org.kde.StatusNotifierItem-1/StatusNotifierItem` or
  `:1.23/org/ayatana/NotificationItem/nm_applet`), matching KDE's
  watcher.
- Item lifetime is tracked by `NameOwnerChanged`: an item goes when its
  registered bus name is released or moves to another owner, or when
  its connection itself vanishes (a unique name losing its owner). A
  connection releasing an *unrelated* name (MPRIS, anything) reaps
  nothing. No polling anywhere.
- Item properties (`Id`, `Title`, `Category`, `Status`, `IconName`,
  `IconThemePath`, `AttentionIconName`, `ToolTip`, `Menu`,
  `ItemIsMenu`, `IconPixmap`) are read once at registration and
  refreshed on the item's `New*` signals, one typed `Get` per property
  — never `GetAll`: pixmap data inside a reply deserializes per byte,
  so every reply body is checked against a raw-size cap *before* any
  deserialization (4 MiB general, 1.25 MiB for the pixmap-bearing
  `IconPixmap`/`ToolTip` replies — zbus itself accepts messages up to
  128 MiB). An over-cap reply is refused undecoded: the pixmap is
  dropped, the refusal counted in `tray.info`'s `oversized_reads`, and
  the daemon's memory never amplifies. A pixmap entry only counts when
  `data.len() == width * height * 4` (ARGB32) and fits 1 MiB. `Status`
  defaults to `Active` when an item does not implement it; a `Menu`
  of `/NO_DBUSMENU` means no menu. Refreshes of one item are coalesced
  to at most one per 250 ms (a `NewIcon` flood cannot loop the item's
  own property reads; the freshest signal still converges). Every
  D-Bus call to an item runs under a timeout (2 s verbs, 3 s menu
  reads and property refresh), proxy builds included, so a hung app
  surfaces as a refusal and never wedges the adapter.

ABP side — the `tray` Bus service (mesh-open; no caller authorization):

- Props `tray.i<n>.{id,title,category,status,icon_name,icon_theme_path,
  attention_icon_name,tooltip,has_menu,item_is_menu,service,path}` plus
  `tray.count`. Keys `i<n>` are ordinals, stable for an item's lifetime
  and never reused; the leaves vanish with the item. `tooltip` is the
  ToolTip title (the description rides `tray.list`); the full per-item
  detail (tooltip text, menu path, pixmap size) is in `tray.list`.
- `IconPixmap` pixels never ride the props: `tray.i<n>.pixmap_width`
  and `pixmap_height` record the largest available size, and the
  `tray.icon {id}` verb serves the pixels as
  `{width, height, encoding: "argb32-network-order", argb_b64}` — the
  raw SNI bytes (ARGB32, network byte order, row-major, no padding),
  base64-encoded.
- Verbs dispatch concurrently (bounded at 8 in flight): an item
  hanging on its 2-3 s timeout delays only its own verb, never
  `tray.list` or `tray.props.get` behind it.
- Verbs: `tray.list`; `tray.activate {id, x?, y?}`,
  `tray.secondary_activate`, `tray.context_menu`, `tray.scroll {id,
  delta, orientation}` (horizontal|vertical); `tray.icon {id}`;
  `tray.menu {id}` → the item's com.canonical.dbusmenu layout —
  `AboutToShow(0)` first (the dbusmenu hook for lazily-built submenus),
  then `GetLayout(0, -1, propertyNames)` with the names pinned to the
  small set the adapter surfaces, `icon-data` excluded — as a JSON
  tree of `{id, label, enabled, visible, type, toggle_type,
  toggle_state, truncated, children}` nodes (absent `enabled`/`visible`
  are the dbusmenu default `true`; the tree is capped at 512 nodes and
  a cut tree carries `"truncated": true`); `tray.menu.click {id, item}`
  → dbusmenu `Event(item, "clicked")` with the spec's `(i s v u)`
  body — a single-variant data, a u32 timestamp. Unknown ids and bad
  arguments are refusals (rc 10), never panics.
- Events `tray.item.added` / `tray.item.changed` / `tray.item.removed`
  plus `tray.props.changed` diffs, each stamped with a per-adapter-run
  monotonic `event_seq` — one shape everywhere: a JSON number in verb
  and event bodies, a decimal string in `BusMessage` headers. A gap
  means events were dropped; re-read `tray.props.get`. `tray.info`
  carries the current counter, the host list, the `oversized_reads`
  refusal count and a bounded ring of recent events.
- A Bus (mesh) outage never ends the run: the broker reconnects the
  `tray` Bus client inside the run and the publisher keeps its diff
  baseline, so the next event re-diffs the whole outage window and
  items survive. Only the stop signal, session-bus death (observed on
  the zbus connection's closed signal) or a real internal fault ends a
  run.

Bounds: 64 items (8 per registering connection), 64 hosts, 4096 chars
per string, 1 MiB per stored pixmap (largest wins, `w*h*4` bytes
exactly), 4 MiB raw reply cap before any deserialization (1.25 MiB for
pixmap-bearing replies), 512 menu nodes per layout read, one refresh
per item per 250 ms.
