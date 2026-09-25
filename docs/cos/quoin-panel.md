# quoin-panel

Stage A keeps this citizen and its existing page IDs running alongside the
[scenes loader](scenes-loader.md). New templates use distinct names until
Stage B. Page selection and popup release advance from command replies and
applied state reads. `<host>.panel.changed` notifications are invalidation
hints; dropping one cannot strand an operation. Open replies acknowledge desired state;
`shown` reflects the last applied host snapshot. Saved popup pins are cleared
only after an applied release. Refresh bursts use one local 10 ms coalescing
deadline in the calling handler, with no Bus loopback continuation. Network
and volume arrive as Stage C's native events (`net_watch`, `audio_watch`); the
minute clock only redraws displayed time.

The Quoin bottom panel, application launcher, calendar and notifications
popup, written as [Mix Scenes](scenes.md). One Mix citizen
(`src/desktop/scripts/quoin-panel.mix`, Bus name `quoin-panel`) owns four
scene documents and loads them into the running Quoin (`shell`):

| scene | edge | what it is |
|---|---|---|
| `panel` | bottom, docked (legacy Bus `pin`) | launcher button · workspace pager · task buttons · tray icons · status applets · clock · peek-at-desktop |
| `launcher` | left | search field + every visible `.desktop` application, click launches |
| `calendar` | right | month grid (Monday first, today marked, neighbouring days greyed), month navigation, "Open calendar app" |
| `notes` | right | live notifications from the `notify` adapter, click dismisses, "Clear all" |

The layout follows Plasma's default bottom panel and Breeze Dark palette.
The clock is the big `09:05 pm` with `Wed 16 Sept` underneath; clicking it
opens the calendar. One popup is open at a time, like Plasma.

## Stage A file templates

The shipped `share/scenes/{launcher,calendar,notes}/` directories each contain
`scene.mix` (an AMP document with strict-data nodes), `template.conf.mix`
(strict metadata), and an executable `behaviour.mix`. Layout is static; all
changing content comes from `$model`. Calendar authors its six-by-seven grid
directly. Page IDs remain `scene-launcher`, `scene-calendar`, `scene-notes`.
The existing `quoin-panel.mix` continues serving the host during Stage A.
Test these templates in a nested host or under non-colliding scene names.

`mix share/scenes/install.mix [destination]` installs the shipped catalogue and
its shared `lib/` once. The default is `$XDG_DATA_HOME/cosmix/scenes` (falling
back to `$HOME/.local/share/cosmix/scenes`). It does not install user scenes or
enable them. Behaviours resolve helpers through `SCENES_LIB`, or the same XDG
data path, so installed scenes do not depend on a checkout. `lib/data.mix`
contains reusable clock/calendar/title/category helpers; `lib/models.mix`
builds model values and `lib/runtime.mix` handles Bus envelopes. The original
`scripts/lib/panel.mix` retains compatibility helpers for the running panel
and screenshot callers until their migration.

The loader executes one behaviour with `SCENE_NAME`, `SCENE_HOST`,
`SCENES_SERVICE`, `SCENE_GENERATION` and existing service overrides. The
behaviour uses `serve_name()` as its actual citizen identity. The loader must
route the installed document's `citizen` to that identity if it renames it;
the shipped documents name `scene-launcher`, `scene-calendar`, `scene-notes`.
`SCENE_GENERATION` is a positive integer. Metadata currently carries
`schema: 1`, `name`, `title` and `behaviour: "behaviour.mix"`.

Behaviours check `shell.scene.describe {family:"list"}` for `flow`'s
`horizontal` enum and boolean `hidden`, then send:

```text
scenes.ready {name,generation}
scenes.model {name,generation,value}  # complete model
scenes.open/close/toggle {name}      # popup ownership stays with the loader
```

They never load/unload scenes, change host configuration or manage popup pins.
W2's loader must reject stale generations, serialise complete models with
reloads, and restore the last accepted model on host reconnect. Directed UI
requests arrive as `$event.args = {scene,node,kind,value?,item?}`. Public verbs
are mesh-open. Refusals are nonzero with `{error_code,message}`; upstream
failures retain the original RC and reply under `context`.

`launcher.open/close`, `launcher` (toggle), `calendar.open/close`, `calendar`
and `notes.open/close`, `notes` delegate popup operations to the loader.
Launcher additionally supports `filter`, `cat`, `launch`, `launch_first`,
`launcher.launch`, `launcher.search` and `launcher.state`; calendar has
`cal_prev/next/today`, `open_calendar`, `calendar.state`; notes has `note_close`,
`notes_clear`, `notes.state`. The Stage A state verbs report behaviour data;
open/selected/pinned state belongs to the loader. Launcher result publication
checks a request sequence after the search and after icon resolution, including
A→B→A races. Enter only launches a result matching the current query/category.

Refreshes are queued by Bus events, with one pending self-event and no debounce
sleep. The launcher observes `apps.changed` (published by `apps.reload` and
icon-theme changes), property events and broker service-list changes. Notes
observes notification/property and service-list events. Both subscribe before
their first snapshot. `bus.connected` reannounces readiness and refreshes data.
Calendar's sole recurring deadline advances its displayed clock at the next
wall-clock minute; it never checks service state. The existing Mix sleep
primitive implements each single deadline, so a wall-clock adjustment during
that wait is reflected at the next wake, not immediately. Network/PipeWire
conversion belongs to the later panel work.

Run the deterministic gate with a process environment of `TZ=UTC`:
`mix src/desktop/scripts/tests/scene-template-test.mix`. `--record` refreshes
public captures. Rust binding, renderer, Taffy and click bridge tests run on
the build cluster. Live screenshots and W1/W2 integrated lifecycle/race tests
remain required before migrating the host.

## Data sources

Everything comes from an existing citizen or the kernel; the panel stores
nothing of its own.

- **Applications and icons** — the `apps` citizen (`apps.list`,
  `apps.icon`, `apps.launch`): XDG desktop entries and the icon-theme spec.
  Status icons ask for `breeze-dark` (override: `PANEL_ICON_THEME`) so they
  read on the dark panel.
- **Tasks and pager** — `comp.windows.list` and `comp.props.get
  workspaces`. A click focuses and raises a window, minimises the focused
  one, and restores a minimised one (Plasma's task semantics); pager
  buttons call `comp.workspace.switch`. Only the current workspace's
  windows are shown.
- **Tray** — the `tray` adapter (`tray.list`, `tray.activate`), shown only
  when an application publishes a StatusNotifierItem.
- **Notifications** — the `notify` adapter (`notify.list`,
  `notify.close`).
- **Network** — any non-loopback link with operstate `up`, read with
  `net_state()` (an rtnetlink dump, no D-Bus) at start and on every
  `net.changed` batch from a `net_watch({events:["link"]})` subscription.
- **Volume** — PipeWire's default sink through `audio_state()` (one `wpctl`
  call), re-read on every `audio.changed` batch from `audio_watch`; the applet
  is hidden when there is no default sink. Click toggles mute.
  `PIPEWIRE_RUNTIME_DIR` points at the login session's runtime directory. If
  PipeWire's server goes away the subscription reports `closed` and the panel
  subscribes again after 5 s, doubling to at most 5 minutes until it succeeds.
  See [desktop status events](../mix/system.md#desktop-status-events--net_watch-audio_watch).
  Behaviours use the same sources through `lib/runtime.mix`'s `status_watch`,
  `status_net`, `status_volume`, `status_fields` and `status_closed`.

## Legacy host update loop

The citizen subscribes to `<comp>.props.changed` (windows and workspaces),
the tray adapter's `item.added`/`item.removed`/`props.changed`, and the
notify adapter's `changed`/`props.changed`. A handler matches the
publisher's **inner** verb (`props.changed`), not the topic name. Bursts are
coalesced into one rebuild with a local 10 ms deadline. An `async`
handler (`clock.run`, kicked once by init) sleeps to each minute boundary
and redraws the time, one wake per minute. Init itself returns, so the
runtime's reserved verbs (`RELOAD`, `QUIT`, `INFO`, lifecycle props) are
served normally; on shutdown the loop is drained like any async handler.

When Quoin restarts, the next `scene.load` returns revision 1; the citizen
takes that as a fresh host and re-selects and re-pins its panel page.

Popups are one at a time. Each open/close bumps a generation, and a render
re-checks it after every Bus call — including between selecting the page and
pinning it, undoing the pin if a close landed in between — so a rebuild
already in flight does not leave a closed popup open. Every popup pin the
panel makes is recorded in `$XDG_STATE_HOME/cosmix/quoin-panel-pins.json`; at
start the citizen releases exactly those edges (a popup open when Quoin or
the citizen went down would otherwise return as a pinned native page). A
record is only dropped after a state read confirms both `pinned == false` and
`visible == false`. Quoin's `shell.panel.page.set`, `shell.panel.pin` and
`shell.panel.mode` replies carry `{accepted:true, applied:true, panels}` after
model application; hidden mode also waits for concealment to finish. A command
superseded before application is refused with `PANEL_NOT_APPLIED` and the current
snapshot. The citizen re-reads state after every command, including refusals
and timeouts; an applied reply remains usable if that read fails. Unconfirmed
releases retain their record and report rc 22.
Pending operations retry on state hints, subscription gaps, reconnects and
explicit opens/closes. Already-satisfied phases complete immediately.
The file remains a bare JSON array of edge strings. Its explicit compatibility
rule is that legacy Bus `unpin` releases both persistent modes, including dock
reservations migrated from legacy `pinned: true`. No version conversion is
needed for this record. Existing Bus `pin` still reserves space; switching
popup/capture callers to overlay pin intent is a separate API/caller chunk.
Pin state is per edge,
so a pin you set on a recorded edge after the citizen stopped is released
too; edges the panel never pinned are never touched.

Broker registration changes, subscription gaps and `bus.connected` re-seed
the compositor watch and recover recorded pins from state; returning shells
are remounted. A missing host at init leaves the citizen available until the
host appears. Panel notices include settled width, so resize motion emits
only the final width; reveal/conceal reports mapping changes, not each frame.
The previous five-minute recovery pass
is removed, and so is the clock's once-a-minute network/audio read: those
applets follow `net.changed` / `audio.changed`. Stage A keeps this legacy host
citizen running.
Panel migration belongs to Stage B.

## Limits

The launcher shows at most 500 applications (the host's list cap) and the
notifications popup the 50 newest; long names, descriptions and bodies are
cut with an ellipsis so the document stays inside the 256 KiB bound. User
text that contains `${` (strict data would read it as interpolation, and has
no escape for it) gets an invisible word joiner between `$` and `{`.

## Handlers

`launcher`, `filter`, `launch`, `launch_first`, `ws`, `task`, `tray`,
`calendar`, `cal_prev`, `cal_next`, `cal_today`, `open_calendar`, `notes`,
`note_close`, `notes_clear`, `vol_mute`, `peek` are scene handlers (the
host sends them `{scene, node, kind, value?, item?}`). `panel.refresh`
rebuilds every open scene on demand.

"Open calendar app" launches the first application in the `Calendar`
category, falling back to `thunderbird -calendar`.

### Agent verbs

Send these to `quoin-panel` with a JSON object body (an absent body means
`{}`). Success replies are JSON objects with rc 0. Bad arguments return rc 10
with `{error: "invalid_request", detail: "..."}`; unknown fields are rejected.
The scene click handlers above keep their toggle behaviour.

| Verb | Arguments | Reply |
|---|---|---|
| `launcher.open` | `{query?, category?}` | Same as `launcher.state` |
| `launcher.close` | `{}` | `{open: false}` |
| `launcher.state` | `{}` | `{open, shown, query, category, count, items}` |
| `launcher.search` | `{query, category?, limit?}` | `{query, category, count, items}` |
| `launcher.launch` | `{id}` | `{launched: id}` |
| `calendar.open`, `calendar.state` | `{}` | `{open, shown, month, year}` |
| `calendar.close` | `{}` | `{open: false}` |
| `notes.open`, `notes.state` | `{}` | `{open, shown, count}` |
| `notes.close` | `{}` | `{open: false}` |
| `popups.state` | `{}` | `{launcher, calendar, notes}` (booleans) |

Open and close are idempotent. Opening a closed popup closes the other popups
through the same generation-checked reveal/pin path as a click. Opening an
already-open launcher preserves omitted filters; opening a closed launcher
defaults them to empty strings. Numeric queries are converted to strings;
other query values and categories must be strings. Open truncates the query
to 128 characters and validates category against the launcher chips (case
sensitive; `""` means all). Search accepts any apps category string.
An already-open calendar retains its navigated month. Every open retries the
render. `open` records intent; `shown` records the last applied host snapshot,
so an accepted open may initially reply with `shown:false`. A refused close
returns rc 22 with
`{error: "release_failed", open: false, edge}`. Repeated closes retry pending
pin records, without releasing an edge another popup is using.
Popup switches also return rc 22 if releasing the previous popup fails;
the destination stays closed and another open retries the release.
Scene launch/calendar actions dismiss shell-revealed popups even when the
citizen has no open flag or pin record. A refused reveal is never pinned;
both clicks and agent opens leave it retryable.

Launcher items contain only `{id, name, generic_name, comment, icon, categories}`.
`count` is the full matching count, before truncation; state includes at most
50 items and uses the list cached for that exact query/category. Replies to
obsolete fetches are discarded. When the cache needs fetching, an open
launcher is re-rendered so its content and the reply stay together.
Search always calls `apps.list`, changes no UI or cached state,
and defaults category to `""` and limit to 50. Limit is a non-negative integer;
zero returns only the count. There is no additional search limit cap.
Launch requires a non-empty string id and closes the launcher only on success.
If launch succeeds but release fails, rc 22 includes `launched: id` alongside
the release error: retry `launcher.close`, not the launch.
Apps and shell rc 0–9 count as success. Transport failures return rc 20 and
`{error: "apps_transport", rc, reply}`; apps/broker refusals return rc 21 and
`{error: "apps_refused", rc, reply}` (including nested rc 14 `not_found`).
A non-list or malformed search reply returns rc 23 `apps_invalid_reply`.
Notes count is the full cached notification count, even when the
popup's 50-row display cap applies. Calendar month is 1–12 in local time.

`COSMIX=<checkout> mix src/desktop/scripts/tests/panel-bus-test.mix` tests the
production panel citizen over an isolated Bus broker with fixture dependencies.
It checks delayed cache replies, refused reveals and release retries, search
isolation, transport and peer rc handling, popup exclusivity, scene clicks and
argument validation. Children use a cleared environment and explicit PATH;
no live GUI is needed.

## Running it

```sh
mix --serve src/desktop/scripts/apps.mix --name apps
mix --serve src/desktop/scripts/quoin-panel.mix --name quoin-panel
```

`SCENE_HOST`, `COMP_SERVICE`, `APPS_SERVICE`, `TRAY_SERVICE` and
`NOTIFY_SERVICE` point it at a nested development desktop. The pure helpers
(clock and date text, the month grid, document emission) live in
`lib/panel.mix`; `mix tests/panel-test.mix` checks them in any timezone.

Needs a Quoin whose scene host loads absolute-path PNG/SVG icons, accepts
`{cells[i]}` in an image template, and supports chromeless pages
(`window.chrome: false`) — see [scenes](scenes.md).
