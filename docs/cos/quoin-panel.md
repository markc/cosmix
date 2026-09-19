# quoin-panel

The Quoin bottom panel, application launcher, calendar and notifications
popup, written as [Mix Scenes](scenes.md). One Mix citizen
(`src/desktop/scripts/quoin-panel.mix`, Bus name `quoin-panel`) owns four
scene documents and loads them into the running Quoin (`shell`):

| scene | edge | what it is |
|---|---|---|
| `panel` | bottom, pinned | launcher button · workspace pager · task buttons · tray icons · status applets · clock · peek-at-desktop |
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
re-checks it after every Bus call, so a rebuild already in flight can never
reopen a popup the user has closed. Every popup pin the panel makes is
recorded in `$XDG_STATE_HOME/cosmix/quoin-panel-pins.json`; at start the
citizen releases exactly those (a popup open when Quoin or the citizen went
down would otherwise return as a pinned native page). Pins you made
yourself are never touched.

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
