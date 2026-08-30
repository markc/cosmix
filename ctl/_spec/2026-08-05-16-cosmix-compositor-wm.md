---
title: Cosmix Compositor — cosmix-comp
chapter: 16
version: 0.1.1
status: draft
date: 2026-08-05
companion: README.md
---

# Chapter 16 — The Cosmix Compositor

> Bring back the X backplane you've missed for thirty years, but make the
> viewport a physical camera, the workspaces mere bookmarks on the terrain,
> and every coordinate of it addressable over the bus — so a keybind, a
> script, or an agent can all fly the same sky.

## §0 Scope, provenance, and governing law

This chapter specifies `cosmix-comp`: architecture, render doctrine, the
canvas window-management model, decorations, UI tiers, the compositor's bus
surface, power doctrine, and guest compatibility. It documents *decided*
architecture backed by shipped evidence, plus *designed* architecture
scheduled within the North Star roadmap (each section marks which it is).

- **Governing law:** the Ch00 North Star (README v0.4.2). Every section
  below cites the principle it answers to. Conflicts resolve in the North
  Star's favour; changing that means amending Ch00 first.
- **Evidence base:** cos `50bf6e9` — cosmix-comp 0.12.1; TTY first light
  (rung D-3: kms-live frame pump, 4K BGRA @ 60 Hz, output ready 118 ms) and
  session-revocation containment (rung D-3.1: VT switch mid-frame → bounded
  exit ~290 ms, quiesce window sized to wgpu-hal's full 3×1 s bounded
  acquire chain). 655 tests, both feature ways.
- **Provenance:** drafted 2026-08-05 from the design session that produced
  README v0.4.0–v0.4.2. Decisions recorded here were made by Mark on that
  date.
- **Out of scope:** CTK's widget surface (Ch06 + toolkit docs), the ABP
  Display Protocol lane (Ch05), daemon identity (Ch10), furniture-tier
  applications (Ch17, deferred).

## §1 The three planes (decided; NS principles 1, 2)

`cosmix-comp` is one process containing three planes with strict ownership:

| Plane | Thread/home | Owns |
|---|---|---|
| **Protocol** | Smithay 0.7 on calloop | All authoritative state: surfaces, toplevels, focus, buffers, explicit sync, DRM/KMS session, libinput, the decoration frame model (§4), hit regions, canvas geometry (§3). |
| **Render** | Bevy 0.19 / wgpu 29 via `cosmix-wgpu-dmabuf`; presentation layer as sole KMS commit owner | Pixels. The scenic engine is a *view* of protocol-plane state, never an owner of it. |
| **Control** | `cosmix-comp-port`, an async task beside calloop | The bus: `comp.*` verbs, the property tree, event topics (§6). Attached to the protocol plane so the desktop stays addressable regardless of renderer state. |

**Invariants** (these are the North Star's decision tests, restated as
protocol law):

1. Nothing outside the protocol plane is authoritative (NS test 1).
2. No frame ever waits on the bus; comp runs flawlessly with no broker
   (NS test 2). The bridge uses bounded queues with drop-oldest; a standing
   test pins frame time under a flooded broker equal to frame time with no
   broker.
3. Client dmabufs import zero-copy; SHM uploads are the protocol plane's
   concern, surfaced to the render plane as upserts with layout records.

## §2 Render doctrine: Bevy-first, tripwire-admitted alternates (decided v0.4.1)

There is **one** render engine: the scenic ECS engine. All render effort
concentrates on it until real-world telemetry (via §6's property surface)
argues otherwise. Alternate engines — a utility renderer (Smithay GLES, or
software tiny-skia/pixman for VMs and headless) and per-frame direct
scanout — are **triggered, not scheduled**:

| Alternate | Tripwire |
|---|---|
| Utility/software renderer | Journaled wedge rate ≳ 1/month on supported hardware; or a Bevy upgrade stalled beyond a few weeks needing a desktop-side fix; or a real VM/headless deployment. |
| Direct scanout bypass | Measured fullscreen video/gaming power or latency that composition cannot meet. Deferral is free: dmabuf-feedback v4 is re-sendable, so scanout tranches can be added later without breaking clients. |
| Deeper power work | Idle battery draw above target *after* the reactive floor (§7) lands and is measured. |

**Seams kept warm** (near-zero cost now; what makes later engines additions
rather than rewrites): the presentation layer stays the sole KMS commit
owner and never fuses into Bevy internals; the frame model, hit regions,
and input live in protocol-plane data any renderer could draw; window
layout lives in protocol records, mirrored — never solely owned — by ECS
components.

**Failure posture until a tripwire fires:** bounded honest exit (D-3.1
containment) and fast restart. Demote-instead-of-exit arrives only with an
admitted alternate engine.

## §3 The canvas model (designed v0.4.2; NS principles 1, 4, 6, 7)

### §3.1 Model

Each output owns a **canvas**: a bounded, elastic 2D plane. Windows live at
world coordinates on it. The screen is a **camera** over it — literally: a
Bevy 2D camera per output; panning is camera translation, zoom is camera
scale, and animation, inertia, and interruptibility are inherited from the
engine. Canvas geometry (window world positions, place bounds, camera pose)
is protocol-plane state (§1 invariant 1); the ECS mirrors it.

**Workspaces are not containers; they are camera bookmarks.** A **place**
is a named, screen-sized (or larger) region on the canvas. A "workspace
switch" is the camera easing to a place. Snapping is *camera behaviour*,
not data structure: released near a place boundary, the camera settles onto
it magnetically (with hysteresis); panned deliberately, it parks anywhere.
One mechanism yields three experiences — discrete workspaces for keybind
users who never learn the canvas exists, free X-Y roaming edge to edge for
the spatial worker, and any scroll discipline (niri columns included) as a
mere camera-constraint policy.

### §3.2 Bounds and lifecycle

The canvas is **bounded because spatial memory needs edges** — a truly
infinite plane kills the minimap and the "up and to the left" instinct. It
is **elastic**: pushing windows past the frontier grows it; emptied margins
shrink it. Places spawn dynamically (drop a window past the frontier) and
**garbage-collect when empty** (GNOME's lesson). Layout policy is
**per-place**: floating, snap-assisted, tiled, fixed grid — each policy an
ordinary bus citizen issuing `comp.move` (NS principle 4).

**Per-place presentation attributes** (v0.1.1, the Amiga screen-mode
inheritance): each place record carries optional presentation overrides —
theme variant (the studio place is dark), scale, and fps/power policy —
applied as the camera settles onto the place. Three fields in the place
record; a capability no incumbent desktop offers cleanly.

### §3.3 Navigation vocabulary

- **Continuous:** Super+drag and 3/4-finger 2D gestures pan with inertia;
  Super+scroll zooms. **Edge-push panning** — the X-virtual-root gesture —
  is supported but ships **default-off with a dwell threshold and corner
  deadzones**: modern screen edges are load-bearing (Fitts targets,
  layer-shell panels, hot corners), so the push must be deliberate.
- **Discrete:** place jumps by number and name; directional steps
  (Super+arrows = adjacent place, the fvwm-pager feel); per-window "fly to"
  from the switcher or palette.
- **Zoom as the unifier:** far enough out, the canvas *is* the overview —
  live thumbnails, place labels, drag-on-the-map — not a mode, an altitude.
  The pager minimap, GNOME's overview, and Amiga screen-dragging are one
  scene at different zooms.
- **Peek/travel (v0.1.1, the Amiga screen-drag reborn):** drag down from
  the top edge (or hold a keybind) and the current view slides down,
  live-revealing a *beneath* place — a second camera into the scene,
  composited with offset. The gesture has continuous depth: **partial drag
  = peek** (glance at comms or the build place, hand still on your work;
  release snaps back), **past-threshold + release = travel** (commit the
  switch, exactly the Amiga screen-flip). Interruptible and springy;
  presentational z only, so the flat-at-rest invariant (§3.7) holds. Which
  place is beneath is policy: most-recently-visited by default, pinnable
  via `comp.place.beneath` — so a script or agent decides what lives under
  your fingertips.

### §3.4 Multi-output

Per-output canvases, not one shared mega-plane: mixed DPI and hotplug stay
sane; cross-output moves are explicit reparents between canvases.

### §3.5 Addressability (the part with no precedent)

- **Camera as property:** `comp.props.output.<n>.camera` = {x, y, zoom},
  readable and settable. `comp.camera.goto place:mail` or raw coordinates.
- **Places as semantic addresses:** agents and scripts place windows by
  meaning (`comp.window.move id place:build`); `comp.place.create
  name:comms` lets a Mix script assemble a project layout.
- **Topics:** `camera.moved`, `place.entered`, `place.created`,
  `window.placed` — a furniture pager renders the minimap from props
  without comp knowing pagers exist; agents react to spatial context.
- **Session restore:** serialize the canvas — geometry *and* meaning.
  A `.mix` arrangement file (places + members) is a shareable desktop
  layout an agent can apply on any node.

### §3.6 Compatibility and edge cases

- **Guests:** places project through `ext-workspace-v1` as flat workspaces
  for pagers, docks, portals (NS principle 6). Flat-earth view of a globe;
  cheap map projection.
- **Visibility/power:** frame-callback throttling becomes a camera-frustum
  test. Partially visible during a pan counts as visible; fully offscreen
  windows quiesce (§7).
- **Fullscreen:** pins the camera to that surface's place. (Future
  synergy: direct scanout, if admitted, requires a camera at rest exactly
  on a place — the settle behaviour provides this for free.)
- **Input:** pointer coordinates are camera-relative to clients; panning
  under a stationary pointer must synthesize motion events (solved in the
  frame model, not deferred to the renderer).

### §3.7 The depth doctrine: 2D model, 3D presentation (v0.1.1)

Every surviving use of 3D on the desktop is presentational; every failed
one (Compiz cube, Looking Glass, BumpTop, Task Gallery, Microsoft Bob's
rooms) used the third axis as *storage*. Cosmix admits 3D in exactly four
roles, and NS principle 1 enforces the boundary: the protocol plane's
layout model stays 2D + layer; whatever the renderer does with z is a view.

1. **Depth as meaning — the semantic layer ledger.** The protocol plane
   owns a small z vocabulary: the layer-shell stack (background / bottom /
   windows / top / overlay), SSD chrome, and the **trusted tier, topmost by
   law, not convention** — plus transient window states (*lifted* during
   drag, *focused-forward*, *receding*). The render plane translates these
   into presentational depth, shadows, and parallax (places at faintly
   different depths give the canvas landmarks during pans). Depth serves
   comprehension, never storage.
2. **Dimension in motion — with one hard invariant.** Overview zoom as a
   dolly-with-tilt, place jumps that bank, drag-lift, flip/genie effects:
   the scenic dividend (roadmap phase 3). The invariant that forecloses
   the graveyard: **flat at rest, dimensional in motion.** At rest the
   camera is orthographic and every window sits at identity transform,
   pixel-exact — perspective at rest destroys text rendering and
   fractional scaling. Every 3D flourish must settle back to the flat,
   crisp state.
3. **3D content.** Apps that are 3D render into their own buffers; the
   compositor is indifferent. *Depth-composited native content* (a CTK app
   handing comp a scene subtree rather than a flat buffer) is a research
   note only (§12).
4. **XR — a reserved output type.** An ECS scene viewed by cameras is
   already an XR compositor missing only an output type: a headset is a
   stereo camera pair with head tracking; places array around the user;
   the bounded canvas curves into a cylinder. **Zero work now**, but no
   phase-3 decision may assume exactly one flat camera per output — an
   output is a camera *pose set*, plural-capable.

### §3.8 Scene switchboards (v0.1.1, furniture-tier pattern — not compositor work)

A **scene switchboard** is a furniture-tier CTK/Bevy app rendering a 3D
scene — a studio, a writing den, an ops bridge — in which every object is a
semantic anchor bound to a bus verb: the mixing desk → `comp.camera.goto
place:studio` + focus the DAW; the rendered monitor shows the live desktop
via screencopy textures, one click to fly fullscreen. The scene is a 3D
*skin over the addressable desktop* — a client of the thesis, requiring no
compositor mode. A scene definition is a `.mix` arrangement file with
geometry (objects → verbs → places): shareable, agent-generatable
"workshopped workspaces."

Strategic note: the scene switchboard is the XR home environment (§3.7
role 4) prototyped on a flat monitor, built at furniture-tier cost,
cancellable without a trace. Prerequisites: §6 verbs (phase 2) and a
screencopy path. Priority: low; a post-phase-3 demo app — and the
canonical public showpiece, because the demo is not "a 3D room" but "this
room is a 40-line scene file scripting the desktop over the bus; make your
own."

## §4 Decorations: SSD policy, CSD tolerance (designed; NS principles 5, 6)

- **Protocol:** `zxdg_decoration_manager_v1` is already advertised and the
  handler exists (currently answering ClientSide unconditionally —
  handlers.rs:1015). The decided policy: answer **ServerSide** to every
  negotiation; tolerate CSD from guests that never negotiate (GTK). Crop
  thumbnails and window textures to `xdg_surface` window geometry so CSD
  shadow margins never leak into scenic features.
- **Frame model in the protocol plane:** frame geometry, the eight resize
  edges, button hit regions, hover/pressed state, double-click and drag
  zones are protocol-plane data (renderer-agnostic — the seam of §2).
  Configure sizes shrink by frame insets; buffer = content, geometry stays
  clean (the SSD dividend that keeps §3 textures and future scanout
  honest). Interactive move/resize uses Smithay's grab machinery.
  Resize-edge cursor shapes via cursor-shape-v1.
- **Style source, not widget host:** frames are drawn by the render plane
  from CTK's *theme tokens* (a small shared style crate — no `bevy_ui` in
  comp at this stage). Titles rasterize to small textures once per
  title/theme change (parley is already pinned in-tree), blitted by
  whatever engine paints. Frames restyle on `THEME_CHANGED_TOPIC` like
  every CTK app.
- **The frame is a bus surface:** server-drawn titlebars are compositor
  real estate — agent-activity indicators, an unsaved-work dot fed by
  semantic app state (§6.4), per-window affordances that exist for every
  app because no app implements them.
- **CTK prerequisite:** the Icon catalogue needs window-control glyphs
  (close ×, minimize −, maximize/restore □) — currently absent.

## §5 UI tiers: trusted vs furniture (decided; NS principle 5)

The test for any piece of desktop UI: **must it be unspoofable?**

- **Trusted tier (in comp, small, renderer-agnostic data model):** SSD
  frames; consent prompts (interactd-mediated agent authorization); lock
  and typed-confirmation surfaces; the command palette front-of-house
  (§9). These authenticate the system to the user; a client window can
  imitate any of them, so they must be compositor-drawn.
- **Furniture tier (layer-shell CTK clients):** panels, launcher,
  notifications, pager/minimap, docks. Ordinary processes: crash-isolated,
  independently released, full AccessKit (they are normal winit apps),
  pixel-identical theming via the shared theme system, engine-indifferent.
- **CTK embedding stance:** comp links the shared *style crate* now.
  Embedding `bevy_ui`/feathers into comp is deferred until the trusted
  tier needs real widgets (roadmap phase 3+), and even then the trusted
  tier stays small; furniture never moves in-process. Rationale: crash
  blast radius (the KDE/GNOME split-vs-monolith lesson), release-cadence
  decoupling (CTK iterates weekly; comp is evidence-gated), and the
  input-synthesis and a11y impedance of bevy_ui inside a KMS compositor.

## §6 The bus surface: `cosmix-comp-port` (designed; NS principles 2, 4)

### §6.1 Attachment

An async task beside calloop, sharing the ABP wire codec with `cosmix-lib-bus`
— **not** CTK's `AmpBridgePlugin` (that is a Bevy plugin; the desktop must
stay addressable whatever the render plane is doing). Registers with the
local `cosmix-noded` as service `comp` with build provenance (Ch10
identity rules apply; `comp` is a desktop component, not a daemon stem).

### §6.2 Staging (each its own rung)

1. **Presence:** register, provenance, `comp.info`, `comp.ping`.
2. **Read surface:** SPEC 12 property tree + change topics (§6.3).
3. **Verbs:** mutating operations behind an authorization gate — per-verb
   policy from day one (local-node callers first), so the agentic desktop
   inherits a real permission model instead of growing one later
   (Ch12c authz applies).

### §6.3 Property tree and topics (sketch; normative shape in a later delta)

```
comp.props.outputs.<n>.{mode, scale, camera{x,y,zoom}}
comp.props.canvas.<n>.{bounds, places[], frontier}
comp.props.windows.<id>.{title, app_id, place, geometry, focused, urgent}
comp.props.render.{engine, frame_ms, wedge_count, last_detach_reason}
comp.props.power.{profile, fps_cap, idle_state}
```

Topics: `comp.props.changed` (SPEC 12), plus `window.{opened,closed,
focused,urgent}`, `place.{created,entered,removed}`, `camera.moved`,
`output.{added,removed}`.

Telemetry note: `comp.props.render.*` and `comp.props.power.*` are the
evidence the §2 tripwires read. The bus lands early (roadmap phase 2)
*because* the render doctrine depends on its measurements.

### §6.4 Verbs (initial set)

`comp.windows.list` · `comp.window.{focus,move,close,place}` ·
`comp.place.{create,list,remove}` · `comp.camera.goto` ·
`comp.workspace.switch` (compat alias over places) · `comp.screenshot` ·
`comp.render.stats`. Semantic app control composes discovery (`noded.list`
pid↔service mapping) with app ports: ask an app about unsaved work before
close; tell `mail` to compose rather than synthesizing input.

## §7 Power doctrine (decided; NS principle 3)

Reactive everything: CTK's `DcsAppShellPlugin` defaults every app to
reactive `WinitSettings` (the `spike/wl-dnd` pattern, promoted; a
conformance test prevents shipping continuous mode). Comp's KMS path runs a
**demand-driven pump**: render only on damage or live animation; skip
atomic commits when the scene is static; frame clock armed off vblank only
while something moves. Offscreen windows (camera-frustum test, §3.6)
quiesce fully. Idle watts and wakeups/sec are standing journal numbers no
rung may regress; they are also the tripwire inputs of §2.

## §8 Window-management policy (designed; NS principles 4, 7)

Default: floating windows with Windows-grade snap ergonomics
(pointer-driven, discoverable) plus the per-place tiling toggle — both mere
layout policies writing the same protocol-plane layout records (rendered as
ECS transforms). Policy is hot-swappable: a tiling engine is a bus citizen
subscribing to window topics and issuing `comp.window.move`; the second WM
is somebody's Mix script, not a fork. Ship one coherent default; make
dissent cheap.

## §9 The command surface (designed; NS principle 5; roadmap phase 4)

One summonable, compositor-drawn (trusted-tier) palette with three depths:
type an app name → launch; type a verb → `cosmix-actions` and bus verbs,
discoverable and typed; type a sentence → `agentd`. The palette is also the
agent — and being trusted-tier, what it displays cannot be spoofed by a
client. It is the place the mesh becomes visible to a person who will never
learn what a bus is.

## §10 Guests and standard protocols (decided; NS principle 6)

Adopt wholesale: xdg-shell, xdg-decoration (§4), wlr-layer-shell (§5),
ext-workspace-v1 (§3.6), xdg-activation, portals, cursor-shape-v1,
fractional-scale-v1, XWayland. Bridge cheap theme signals (dark mode,
accent, cursor theme via the settings portal); refuse pixel-theming other
toolkits. Compatibility is a platform feature; coherence is a native-tier
feature. The bus is the value *above* this floor, never a replacement for
it.

## §11 Roadmap alignment and evidence discipline

Phases (Ch00): **0** finish the current arc (Rung F: compositor-handled
Ctrl+Alt+Fn; E-5 residuals) · **1** power floor (§7, with standing
measurements) · **2** bus in the core (§6 staging) · **3** scenic buildout —
client windows composited into the scene, SSD (§4), snap + per-place
tiling (§8), canvas + places + overview (§3), furniture as layer-shell
clients (§5), palette (§9) · **4** agentic desktop (agentd × §6.4 verbs ×
interactd consent — configuration, not construction, if 1–3 land as
specified). Alternates only by §2 tripwire.

Everything lands as rungs: journaled, live-evidence-backed, tested both
feature ways, with failure honest until machinery makes it survivable.

## §12 Open questions

1. Final names: *canvas* and *place* are working terms (candidates
   considered: backplane, field, deck; *screen* rejected — Amiga homage but
   collides with output). Decide before Ch17 furniture APIs freeze.
2. Edge-push tuning: dwell threshold, corner deadzone size, per-edge
   enablement — defaults need live-run feel, not armchair values.
3. Place identity across sessions and nodes: stable IDs vs names;
   collision rules for applied `.mix` arrangements.
4. `comp` service authz: which verbs are T-gated (Ch00 Article IV autonomy
   tiers) vs freely local — resolve with 12c when verbs land.
5. Trusted-tier rendering on a future utility engine: degraded-draw
   contract needs a spec delta if/when that tripwire fires.
6. Multi-seat: out of scope for 0.x; canvas-per-seat interactions unstudied.
7. Screencopy/live-texture access policy for privileged clients (the scene
   switchboard's monitor, the pager minimap): which protocol
   (wlr-screencopy, ext-image-copy-capture, or a comp-native texture
   grant), and how the authz gate (§6.2) scopes it.
8. Depth-composited native content (§3.7 role 3): whether a "scene
   subtree" surface type is ever worth a protocol — research only.
9. Peek "beneath" policy defaults (§3.3): most-recent vs pinned vs
   per-place chain; needs live-run feel alongside edge-push tuning (Q2).
