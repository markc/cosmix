# CosMix Compositor Theme System — Design

Status: Phase 0 landed 2026-08-08 · Target: `desktop/crates/cosmix-deco` +
integration points in `cosmix-comp` and `ctk` · Companion code: this crate,
17 unit tests + doctest on rustc 1.95 / edition 2024.

## 1. Where we start (what the code says today)

Reading the tree as of `cosmix-comp` 0.15.8:

- **ctk already owns a theme system.** `ctk/src/theme.rs` defines `ThemeSpec`
  = `Scheme` (ocean/crimson/stone/forest/sunset/mono) × `Mode` (light/dark) ×
  19 colour tokens × `CtkThemeMetrics` × typography, with Oklch colour math
  shared with the web design system. Resolution is layered
  (`built-in ← shared theme.conf.mix ← per-app override`), persistence is
  strict-data `.mix` under `cosmix_config::store::config_dir()`, and live
  convergence is event-driven via ABP topic `theme.changed`
  (`ctk/src/theme_sync.rs`), with focus-gained file reload as the missed-wake
  backstop.
- **The compositor draws no chrome.** `compositor_scene.rs` maps each Wayland
  surface to a Bevy entity (`SpriteMesh` + `SpriteMaterial`, `ChildOf`
  parenting for subsurfaces/popups). `protocol/handlers.rs` implements
  `XdgDecorationHandler` but unconditionally answers
  `configure_client_side_decoration` — every client is told to decorate
  itself.
- **Apps draw their own shell.** ctk's `DcsAppShell` gives ctk-native apps a
  menu bar/toolbar/sidebars; foreign clients (the "live apps that actually
  work" — congratulations again) bring whatever CSD their toolkit ships.

The consequence: the theme system for the compositor should be an *extension
of the existing ctk model*, not a sibling. One shared selection, one file, one
change-notification lane — plus one new axis.

## 2. The model: chrome style is a third axis

A window's decorated look is fully determined by the triple:

    (ChromeStyle, Scheme, Mode)
     ^new         ^existing ctk axes, reused verbatim

- `ChromeStyle ∈ {mac, win11, cosmix}` describes *shape and behaviour*:
  button side, order, silhouette, titlebar height, corner radius, shadow,
  hover/glyph rules, title alignment.
- `Scheme × Mode` describes *palette*. The mac and win11 styles are faithful
  reproductions and take only `Mode` (their palettes are the platform's — an
  "ocean-tinted Windows titlebar" would defeat the point of a lookalike). The
  cosmix style consumes both: its neutrals follow `Mode` and everything alive
  (focus border, hover fills) derives from the scheme's Oklch accent hue —
  the same hue table as ctk's built-in palettes, so a desktop-wide scheme
  switch re-tints titlebars and ctk widgets in lockstep with zero
  coordination.

Faithfulness note: mac/win11 are *lookalikes for muscle-memory comfort*, not
pixel forgeries. We reproduce metrics and behaviour (46px caption targets,
cluster-hover glyphs, `#C42B1C` close hover) but draw everything procedurally
— no Apple/Microsoft assets, no GPL aurorae SVGs, MIT-clean.

## 3. Crate boundary

New workspace member `crates/cosmix-deco`, **dependency-free by design**
(pure data + math), so it is usable from cosmix-comp, unit tests, and future
tooling (theme preview app, docs generator) without dragging Bevy in. It
contains:

| Module | Contents |
|---|---|
| `geom` | `Vec2`/`Rect`/`Srgba` minis + `oklch()` conversion (ctk-compatible) |
| `theme` | `ChromeStyle`, `Scheme`/`Mode` mirrors, `DecoTheme` (metrics + colors + `ButtonCluster`), focus/button-state resolution |
| `presets` | `mac(mode)`, `win11(mode)`, `cosmix(scheme, mode)`, `resolve(style, scheme, mode)` |
| `layout` | `DecoExtents`, `ChromeLayout::compute`, `hit_test → ChromePart` |

Two mirrors exist deliberately: `deco::{Scheme, Mode}` duplicate
`ctk::theme::{Scheme, Mode}` (same names, same wire strings). cosmix-comp must
not depend on ctk (that would pull feathers/UI into the compositor), and ctk
should not depend on the compositor's theme crate. The honest fix is a later
`cosmix-theme-core` crate both re-export; until then the string round-trip
through `theme.conf.mix` is the contract, which is already true for the web
design system. Decision to make at integration time.

All values in `cosmix-deco` are **logical pixels**; the compositor multiplies
by output scale at render time (fractional scale works because everything is
procedural — no bitmap assets to blur).

> **Amendment 2026-08-08 (Mark):** the default chrome style is **mac**, not
> cosmix — light-mode traffic lights on a near-white titlebar is the fleet
> default look. `ChromeStyle::default()` and `Mode::default()` (= `Light`)
> both encode this: the default triple resolves to the mac light look.
>
> **Divergence resolved (Mark, 2026-08-08):** ctk's compiled-in no-config
> fallback (`ThemeSpec::builtin()`) is Ocean/**Light** as of ctk 0.47.1,
> matching deco's default triple — both layers now agree on the no-config
> look. Per-crate `Default`s remain conveniences, never the runtime
> fallback: the integration still resolves ONE `(chrome, scheme, mode)`
> triple from `theme.conf.mix` and feeds both layers from it.

## 4. Configuration and selection

Extend the *existing* shared theme file rather than inventing a second one.
`theme.conf.mix` gains two optional keys:

    chrome = mac             # mac | win11 | cosmix          (default: mac)
    ssd = on                 # on | off — master SSD switch  (default: on)

`ThemeFile` in ctk is all-`Option` fields, so the additions are
backward-compatible for new binaries reading old files. One check needed at
integration: confirm the strict-data `.mix` parser *ignores* unknown keys for
**old** binaries reading **new** files; if it rejects them, the chrome keys go
in a sibling `chrome.conf.mix` next to it instead (same directory, same
watch). This is the only open compatibility question.

Live switching reuses the existing lane end-to-end: Settings (or `mix` CLI)
writes the file → broadcasts `theme.changed` on ABP → cosmix-comp's listener
re-runs `presets::resolve` and swaps a `Res<DecoTheme>` — every decoration
entity repaints via change detection. The compositor also re-reads on its
file-watch backstop (`backend/watch.rs` already establishes the pattern).
Scheme/mode changes ride the same message they do today; chrome changes are
just one more field.

Per-app overrides come free: the resolution order `built-in ← shared ←
per-app` already exists in ctk; the compositor applies the shared layer only
(a compositor has no "current app" — per-window behaviour is §6's
negotiation, not theming).

## 5. Scene integration (cosmix-comp side)

One new plugin, `DecorationPlugin`, owning:

- `Res<DecoTheme>` — the resolved theme, swapped on change.
- Per-toplevel: a `DecoRoot` entity at the outer-frame origin, with the
  client surface translated by `ChromeLayout::content_offset()` inside it
  (the frame extends *above and around* the client — the titlebar is never
  drawn over client pixels), and children in z-order: shadow quad (behind),
  frame/titlebar quad, title text, three button quads (+ glyph meshes).
  `ChromeLayout::compute` provides every rect in frame space; a window move
  is still one transform write on the root. The coordinate-space contract
  (client/xdg geometry vs outer frame vs shadow) is documented at the top of
  `layout.rs` and must be applied exactly once.
- Rounded corners: the window mask (`corner_radius`) applies to the union of
  titlebar + content. Cheapest correct path: SDF rounded-rect in the existing
  `SpriteMaterial` shader family (a `corner_radius` uniform + coverage alpha),
  which also gives the shadow quad its soft SDF falloff for free. Client
  content corners are masked the same way — this is the one place SSD touches
  client pixels, and only as a mask, never a repaint.
- Focus: the compositor already tracks keyboard focus for the seat; the
  decoration systems read it per-window into `Focus::{Focused, Unfocused}`
  and resolve every colour through `DecoTheme` accessors
  (`titlebar_fill(focus)`, `buttons.close.fill(state, focus)`, …).
- Damage: decoration repaints are ordinary Bevy material/visibility changes;
  they ride the existing render path. No new damage plumbing.

What deliberately does **not** exist: per-window theme choice, titlebar
widgets/applets, gradient/texture skins. Tokens and procedural drawing only —
that discipline is what made ctk's "reskin = palette edit, never a code
sweep" true, and it should stay true for window chrome.

## 6. Protocol integration

- **Negotiation.** `configure_client_side_decoration` becomes
  `configure_decoration`: when `ssd = on`, answer `ServerSide` to clients that
  bind `zxdg_toplevel_decoration_v1` (honouring an explicit `ClientSide`
  request — GTK apps that insist keep their CSD, and get no second titlebar).
  Clients that never bind the protocol keep CSD exactly as today, so the
  current fleet is unaffected until they opt in. `ssd = off` restores today's
  behaviour wholesale.
- **Geometry.** SSD chrome lives *outside* the client's xdg window geometry:
  configure sizes stay client-space (what the client calls its window), and
  the compositor's outer frame = chrome size + `DecoExtents::of(theme)`,
  where chrome size = `max(committed client size,
  ChromeLayout::min_content_size)` — equal to client size + extents
  whenever the client is at or above the minimum, larger only for
  undersized committed buffers (size damage/position math from
  `ChromeLayout::window`, never from the raw extents equation).
  The extents convert between the two spaces in both directions
  (`extents_roundtrip` pins exactness on integer sizes); tiling/maximise math
  uses the outer-frame size internally but never leaks it into configure
  events. Shadows are outside both, per spec.
- **Input.** `route_input_event` gains one early step for pointer events on
  decorated windows: transform to window-local logical coords, call
  `ChromeLayout::hit_test`:
  - `Content` → forward to the client (today's path, unchanged).
  - `TitlebarDrag` → press starts an interactive move (compositor-side grab —
    the inverse of the client-initiated `xdg_toplevel.move`); double-press
    toggles maximize.
  - `Button(kind)` → press/release with hover state fed back into the
    decoration entities; release-inside triggers close/minimize/maximize on
    the `ToplevelSurface`.
  - `Resize(edge)` → press starts an interactive resize; `ResizeEdge` maps
    1:1 onto `xdg_toplevel::ResizeEdge`, and the edge selects the cursor
    shape via the seat's cursor-shape device.
  Hit-test priority (buttons → titlebar → content → visible frame border /
  uncovered interior → outside resize band) is pinned by
  `hit_test_priorities`. Resize hits come from the invisible band just
  *outside* the visible edge (mac/win11 feel) and from any in-window point
  that is neither titlebar nor committed client content (win11's 1px
  hairline, or interior a smaller-than-minimum committed buffer leaves
  uncovered) — compositor-owned pixels are never forwarded to the client.

## 7. The three launch themes

| | `mac` | `win11` | `cosmix` |
|---|---|---|---|
| Buttons | traffic lights, **left**, 12px circles, 8px gap | **right**, flat 46×32 full-height targets | **right**, 16px circles, 6px gap |
| Order (edge-first) | close · min · max | close · max · min | close · max · min |
| Glyphs | on cluster hover only | always | always |
| Close affordance | red fill is the identity | idle-invisible, `#C42B1C` hover | red hover on neutral circle |
| Titlebar | 28px, centred title | 32px, leading title | 30px, leading title |
| Corners | 12px | 8px | 10px |
| Border | none | 1px hairline | 1px **scheme-accent when focused** |
| Shadow | large, soft, high (40/0.45) | tight (22/0.30) | medium (28/0.35) |
| Uses `Scheme` | no | no | yes — accent hue drives border + hovers |
| Unfocused | lights grey out | text mutes | accent border drops to neutral |

Behaviour details worth their bytes: mac buttons re-light on hover even when
the window is unfocused (both platforms do this); win11's close pressed state
is the hover red at 90%; cosmix `mono` stays fully achromatic (chroma 0) so
the greyscale scheme survives into chrome. All pinned by preset tests.

## 8. Phased roadmap

- **Phase 0 — this crate.** Land `cosmix-deco` as a workspace member. Pure
  data/math, 17 tests, no compositor changes. Reviewable in isolation.
  **Done 2026-08-08.**
- **Phase 1 — static chrome.** `DecorationPlugin` draws titlebar + buttons +
  border for SSD-negotiated toplevels, focused/unfocused only (no pointer
  states). Flat corners (skip the SDF mask initially), no shadow. Behind
  `ssd = on`, default **off** until Phase 2 lands.
- **Phase 2 — input.** Hit-testing wired into `route_input_event`: move,
  resize, button actions, hover/pressed visuals, double-click maximize,
  per-edge cursor shapes. Flip the default to `ssd = on`.
- **Phase 3 — fidelity.** SDF rounded corners + soft shadows in the material,
  title text (ctk typography, `title_slot` rect), maximize/restore glyph
  swap, mac cluster-hover glyph behaviour.
- **Phase 4 — live theme lane.** ABP `theme.changed` listener + file-watch
  backstop in the compositor; Settings UI grows the chrome picker; the
  `switch` verbs land in `mix`.
- **Later, explicitly deferred:** blur behind translucent titlebars (the
  Better-Blur analogue — the dmabuf/wgpu pipeline can do it, but it's pure
  polish), snap-layout flyout on win11 maximize hover, per-window style
  overrides, third-party theme files (the `DecoTheme` struct is already the
  file format waiting to happen — don't ship it until two internal styles
  prove the schema).

## 9. Why this shape

The compositor gets *faithful lookalikes* without asset licensing risk, the
native style gets *scheme integration for free*, and the whole thing stays
inside the design discipline the codebase already enforces: tokens over
special cases, one authoritative file, event-driven convergence with a reload
backstop, and math that is testable without a running compositor — 758 tests
in cosmix-comp say that last property is house style.
