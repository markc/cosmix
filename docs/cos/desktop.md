# CosMix Desktop — the desktop surface

The CosMix desktop is a **native Wayland compositor plus a family of Bevy
applications**, all Bus citizens. It lives in its own Cargo workspace at
`$COSMIX/src/desktop/` (separate because it pins its own toolchain and vendors
patched Smithay/wgpu) and is built with `setup.mix --desktop`.

This page is the orientation. The compositor's protocol coverage has its own
page: [cosmix-comp](cosmix-comp.md).

## What it is

| Part | Crate | What it is |
|---|---|---|
| Compositor | `cosmix-comp` | Smithay protocol frontend with Bevy 0.19 / wgpu rendering. Runs nested inside an existing Wayland session (`--nested`) or on a KMS seat. |
| Toolkit | `ctk` | Cosmix Tool Kit — the shared Bus-citizen Bevy widget toolkit every desktop app builds on (`bevy_ui` + `bevy_feathers`). |
| Decorations | `cosmix-deco` | Server-side decoration theme engine: chrome styles, tokens, layout and hit-testing. Dependency-free. |
| Design system | `cosmix-design` | Headless compiler and family-schema registry for the `.mix` design source — one styling contract for chrome and app widgets. |
| Shell | `cosmix-shell` | Pure core and host seams for the Quoin desktop shell. |
| GPU bridge | `cosmix-wgpu-dmabuf` | DMA-BUF to Bevy/wgpu import for zero-copy client buffers. |
| Apps | `apps/*` | Studio (DAW / mixer), FileMgr, Mail, Tower (mesh mission control), Tray + trayd, Interact GUI (native presenter for `interactd` dialogs). Registry and identity rules in `src/desktop/APPS.md`. |

## How it hangs together

`cosmix-comp` is one process with three planes and strict ownership:

- **Protocol** — Smithay on calloop owns all authoritative state: surfaces,
  toplevels, focus, buffers, explicit sync, the DRM/KMS session, libinput,
  decorations and canvas geometry.
- **Render** — Bevy / wgpu paints. The scene is a *view* of protocol-plane
  state, never an owner of it; client DMA-BUFs import zero-copy.
- **Control** — the Bus: `comp.*` verbs, the property tree and event topics.
  No frame ever waits on the bus; the compositor runs with no broker at all.

Apps follow the same model. A CTK app draws itself at 60 fps but keeps an
addressable Bus command port (the ARexx idea): it answers `app.describe`,
can opt in to `app.controls.list/get/set` so an agent enumerates and drives
its native controls without screen scraping, and registers as
`<slug>-<engine>-<pid>`. Application chrome — menu bar, toolbar, sidebars,
centre, status row — is one composed `DcsAppShell` component that apps fill
by slot and never assemble by hand.

## What it replaced

Before 2026-07 the desktop was a **markdown-over-Bus display lane**: producers
sent `ui.window` messages and a domain-blind CPU renderer (`cosmix-disp-skia`,
tiny-skia + cosmic-text) painted them. That lane was retired by the
2026-07-18 control-plane decision — the Bus controls apps, it does not paint
pixels — and archived on 2026-07-20 together with `cosmix-lib-display` and the
`ui.*` vocabulary. Nothing of it is in this repository; the source survives on
the frozen `markc/cos` repo under the git tag `amp-display-archive`, and the
documentation page was removed from this site on 2026-08-31. Any material
describing `disp-skia`, `cosmix-deskd`, `cosmix-disp-wgpu` or `ui.panel` as the
current desktop is historical.

## Running it

```sh
cd $COSMIX/src/desktop
cargo run --release -p cosmix-comp -- --nested     # inside an existing Wayland session
cargo run --release -p ctk --example widget_gallery
```

The compositor's supported Wayland protocols, stacking rules and session-lock
behaviour are documented on [cosmix-comp](cosmix-comp.md).

## See also

- [cosmix-comp](cosmix-comp.md) — protocol globals, layer strata, session lock
- [overview](overview.md) — the daemon family the desktop sits on
- [noded](noded.md) — the Bus broker every app and the compositor talk to
- [interactd](cosmix-interactd/README.md) — system-to-human dialogs, presented natively by Interact GUI
