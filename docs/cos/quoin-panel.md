# quoin-panel

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
- **Network** — `/sys/class/net/*/operstate` (no D-Bus).
- **Volume** — PipeWire's default sink through `wpctl`; the applet is hidden
  when there is no default sink. Click toggles mute. `PIPEWIRE_RUNTIME_DIR`
  points at the login session's runtime directory.

## Events, not polling

The citizen subscribes to `<comp>.props.changed` (windows and workspaces),
the tray adapter's `item.added`/`item.removed`/`props.changed`, and the
notify adapter's `changed`/`props.changed`. A handler matches the
publisher's **inner** verb (`props.changed`), not the topic name. Bursts are
coalesced into one rebuild. The only clock is the wall clock: an `async`
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
record is only dropped after a single applied `shell.props.get` subtree snapshot
confirms both `pinned == false` and `visible == false`. An enqueue acknowledgement
is insufficient. The existing bounded hide loop is retained; failure or an
unconfirmed release leaves the record for the existing startup/recovery pass.
The file remains a bare JSON array of edge strings. Its explicit compatibility
rule is that legacy Bus `unpin` releases both persistent modes, including dock
reservations migrated from legacy `pinned: true`. No version conversion is
needed for this record. Existing Bus `pin` still reserves space; switching
popup/capture callers to overlay pin intent is a separate API/caller chunk.
Pin state is per edge,
so a pin you set on a recorded edge after the citizen stopped is released
too; edges the panel never pinned are never touched.

Once every five minutes the clock tick also re-seeds the compositor watch
and refetches windows, tray and notifications — a backstop for an event
missed while comp restarted, not a poll (every change still arrives as an
event).

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
render. `open` records intent; `shown` records whether the latest render had
its load, page selection, reveal and pin accepted, not a compositor frame
confirmation. A failed close returns rc 22 with
`{error: "release_failed", open: false, edge}`. Repeated closes retry pending
pin records, without releasing an edge another popup is using.

Launcher items contain only `{id, name, generic_name, comment, icon, categories}`.
`count` is the full matching count, before truncation; state includes at most
50 items and uses the list cached for that exact query/category. Replies to
obsolete fetches are discarded. When the cache needs fetching, an open
launcher is re-rendered so its content and the reply stay together.
Search always calls `apps.list`, changes no UI or cached state,
and defaults category to `""` and limit to 50. Limit is a non-negative integer;
zero returns only the count. There is no additional search limit cap.
Launch requires a non-empty string id and closes the launcher only on success.
Apps rc 0–9 counts as success. Transport failures return rc 20 and
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
