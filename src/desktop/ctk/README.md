# CTK — Cosmix Tool Kit

The shared **Bus-citizen Bevy widget toolkit** every CosMix Desktop app
builds on (`desktop/apps/*` — see `../APPS.md` for the app registry and
identity scheme). Built on Bevy 0.19 `bevy_ui` + `bevy_feathers`; a member
of the single `desktop/` cargo workspace.

CTK apps draw themselves at 60fps but keep an addressable Bus command port
(the ARexx model): local interaction stays in ECS, continuous controls
stream throttled latest-wins writes mid-gesture, release commits one
revisioned write, inbound writes reconcile without echo.

Every CTK app **answers** `app.describe` on that port (`ctk-app-control.v0`,
draft), reporting its component slug and engine arm; apps register as
`<slug>-<engine>-<pid>`. The widget-derived `app.controls.list/get/set`
surface is opt-in: install `WidgetControlPlugin` alongside `AppPortPlugin`
when an app wants addressable controls. A `set target=<control id>` then
drives the control through the app's ordinary write pipeline. Apps using the
shared action registry can separately install `ActionPortPlugin` for
`actions.list`, `actions.describe`, and authorised `action.invoke`; remote
invocation remains closed until authenticated mesh provenance is available.
For example, the `mixer_strip` example installs `AppControlPlugin` and
registers as `mixer-bevy-<pid>`:

```
send "mixer-bevy-<pid>" app.controls.set target="channel-0-fader" value="-12.5"
```

Application chrome (menu bar + toolbar + DCS sidebars + centre + status)
is ONE composed component: `DcsAppShell` (`src/dcs_app_shell.rs`). Apps
inject content entities into slots and never build or patch shell
structure — contract, procedure, and hierarchy in
`_doc/2026-07-25-dcs-app-shell.md` (control repo).

## Layout

- Flat crate: `src/` holds the library modules — `widgets`, `bus` bridge,
  `mixer` client, `theme`, `icons`, `actions` (cargo features), plus
  `app_dirs` (XDG-polite per-app state roots keyed on the component slug)
  and `identity` (`AppIdentity`: slug → `dev.cosmix.<slug>` app id).
- `examples/widget_gallery.rs` — the widget showcase;
  `examples/mixer_strip.rs` — one live channel strip.
- `assets/`, `_plan/` — shared assets and date-prefixed plans.

## Run

```bash
cd ~/.cos/desktop
cargo run --release -p ctk --features mixer,bus --example mixer_strip -- ws://<broker>:4200/ws
cargo run --release -p ctk --example widget_gallery
```

`mixer_strip` requires a cosmix broker (`cosmix-noded`) and
`cosmix-musicd mixer-serve` on the mesh.

## License

MIT — see [LICENSE](../../LICENSE).
