# cosmix-quoin

## Experimental compositor host

Quoin 0.11.0 also exposes its application as a Rust library. Comp builds with
`native-quoin` can opt in with `COSMIX_COMP_HUD_PROBE=1` and
`COSMIX_COMP_NATIVE_QUOIN=1`. This first-output experiment renders the four
real Quoin panels, native Boing and application window textures in the same
Bevy renderer. Do not run the standalone Quoin executable in this mode: the
embedded application owns the same `shell` Bus service.

Both hosts reuse the page registry, content, chrome, theme, persistence and
semantic Bus commands. The compositor host supplies panel mounts, logical
geometry and pointer input. Panel movement uses the existing chrome animation;
there are no panel Wayland buffers or panel swapchains. Docked panel geometry
updates comp's usable area, publishes output/property changes and reconfigures
maximised windows including their decoration extents. Repeated unchanged work
areas do not trigger another resize or notification. Application
processes remain ordinary Wayland clients, with their existing comp-owned
window decorations and move/resize policy.

Shared scene geometry gives horizontal docks precedence: top and bottom span
the full output width; left and right start below the docked top panel and end
above the docked bottom panel. Hidden and pinned panels reserve no space.
In the embedded host, pinned and transiently revealed hidden panels draw and
receive pointer hits above docked panels. Edge ordering stays stable within
each class, including while hidden panels animate closed. The `dev-host`
normal-window tuning harness uses the same geometry and stacking bands:
docked panels at 110–140 and pinned/hidden overlays at 150–180.

The native host forwards panel mouse buttons, scrolling and grip gestures to
Bevy picking. A press beginning on a panel retains its release outside the
panel; an existing application/window drag retains ownership. Session locking
hides the native panels and cancels their pending pointer actions. Shell quit
delegates to the compositor lifecycle rather than exiting its render App.

This remains an opt-in integration experiment. It uses a continuously rendered
native Boing background and does not yet provide native equivalents of the
external showcase's scene-selection controls. Keyboard navigation, touch,
multiple outputs and idle wake scheduling need separate native-host acceptance
before this replaces the standalone shell. A compositor host must supply the
normal `COSMIX_QUOIN_LAUNCHER` Mix hook and its session environment when app
launches need to run under a separate desktop user.

## Standalone layer-shell host

`cosmix-quoin` is the Cosmix desktop furniture shell. It owns four independent
`zwlr_layer_surface_v1` panels — left, bottom, right and top — and renders the
existing Quoin chrome into one explicit Bevy window target per surface. The
installable application id is `dev.cosmix.quoin`, and every layer namespace
starts with it: each panel layer is `dev.cosmix.quoin.panel.<unique>` and the
corner menu `dev.cosmix.quoin-corner-menu.<unique>` (see
[holder control plane](#holder-control-plane)).

Quoin presents real layer-shell buffers through `cosmix-shell-host`,
`cosmix-shell` and SCTK. See [component versions](../VERSIONS.md) for the
current source versions. `cosmix-quoin-demo` remains a
feature-gated, non-installable normal-window tuning arm; it is not a
layer-shell client.

`QUOIN_BUS_READY service=NAME` reports the configured Bus identity (including
`--bus-service` overrides). It records the first connected event consumed by
the shell service, not successful presentation or continued event-loop progress.

## Core carousel registry

The host-neutral `cosmix-shell` carousel keeps config-declared names in order
and appends undeclared registrations to the tail. Empty declared slots are
skipped when paging or choosing a default page. Registration preserves the
page currently shown; activation selects and remembers a registered name.

Removing the shown page immediately selects its previous live neighbour,
otherwise its next. Removing the remembered selection resets that memory to
the primary. On the next reveal from hidden, `ShellModel` restores the remembered
name or defaults to the primary, skipping empty slots. Repeated reveals while
already visible retain the shown page, including a removal's neighbour landing.

`ShellModel::declare_carousel` reconciles declarations by name. Registered
content, current selection and remembered selection survive reordering. A tail
name promoted into config keeps its content in its new declared position.
Live names omitted from config follow the new declarations in their previous
relative order, preserving the order of remaining tail entries; omitted empty
slots disappear. Invalid declarations leave the registry unchanged. These are
core API contracts; scene mount and Bus lifecycle integration are separate.

## Output and scale

Version 1 owns exactly one output runtime. `--output NAME` selects the exact
complete SCTK `wl_output` advertising that name. A missing name is an error
which lists every advertised output. Without the option, Quoin selects the
first complete output in advertisement order. Every panel role is created
with that explicit `wl_output`, never the compositor default. Removing an
explicitly selected output unmaps and render-drains all four panels, drops
their protocol resources, then exits cleanly; a later output reusing the same
name is not the selected object. A default selection instead migrates to the
next complete output, or exits cleanly when none remains.
If a compositor advertises duplicate output names, explicit selection uses the
first complete match in advertisement order.

Layer protocol and viewport destination dimensions remain logical. Integer
output scale 1 or 2 renders physical buffers at `logical × scale` and applies
that integer with `wl_surface.set_buffer_scale`. With a fractional preferred
scale such as 1.25 (150/120), Quoin keeps buffer scale 1, renders
`ceil(logical × scale)` physical pixels and sets the `wp_viewport` destination
back to the configured logical size. Bevy stores the corresponding physical
resolution and scale override. Every configure and scale change is checked
against the renderer's negotiated maximum 2D texture dimension before surface
scaling requests or Bevy window mutation. SCTK acknowledges each configure
first, as required by the protocol. Either zero configure dimension falls back
to the planner-requested logical dimension; a compositor close or invalid
scale terminates the affected lifecycle cleanly rather than panicking.

## Map, presentation and replay

Each first map replays layer, anchor, size, exclusive zone, edge margin and
keyboard interactivity, then makes an initial bufferless commit. SCTK
acknowledges the compositor configure before the host accepts its logical
size. Only then does Quoin insert the retained Wayland raw handle, enable the
panel's explicitly targeted `Camera2d`, emit the Bevy create/resize/scale
messages, render, attach and present.

Unmap runs in the opposite lifetime order: remove `RawHandleWrapper`, disable
the camera, run one non-pipelined Bevy update to drain render-world removal,
then destroy the `zwlr_layer_surface_v1` and its `wl_surface`. Destroying the
objects prevents SCTK from acknowledging a configure queued for the old role
after the compositor has reset its configure state. The Bevy `Window`, camera
and chrome mount entities remain stable. Remap creates a fresh `wl_surface`
and layer role on the explicit output, recreates the retained raw-handle
owner, replays every property, and repeats the bufferless configure gate. A
successful remap therefore emits `WindowCreated` again before another buffer
can be presented. Teardown order is defined in one place and tested through a
probe.

Docked panels use `Top`, reserve their complete logical thickness and keep
protocol margin zero; chrome alone owns their transient slide. Pinned panels,
transiently revealed hidden panels and mapped-concealing panels use `Overlay`,
reserve zero and slide with their edge protocol margin. Pinning a transient
reveal changes neither layer nor reservation. Docking from hidden claims the full zone at
fraction zero while chrome supplies the only visual translation. Keyboard
policy is `OnDemand` for a mapped panel and `None` otherwise. Two keyboard
actions change it (see Keyboard below): the focus cycle and a named activation
request `Exclusive` on the one panel they move focus into, until the keyboard
lands there (then `OnDemand` again), and handing focus back sets every panel
to `None` until no panel holds the keyboard.
Chrome selects its translation owner from the last successfully committed
protocol mode, not the model's next desired mode. The host advances that latch
only after a commit, or after completed role destruction for unmap, so docking
and undocking cannot hand motion between protocol margin and chrome one frame
early. If the planner proves there are no protocol changes to an already
committed surface, the stored presentation can refresh its logical visibility
intent without a new commit. This never bypasses an outstanding configure.

The persistent modes are `Hidden`, `Pinned` and `Docked`. A separate
`transient_revealed` flag lets a hidden panel respond to pointer/corner holds
without changing its mode. Only transient visibility auto-hides. An idle hidden
panel is unmapped; during concealment it stays mapped until its slide finishes.
Hide, Escape and ordinary visibility toggle do not release either persistent
mode. Core `PinToggle` and `DockToggle` select their respective mode.
`PinToggle` from pinned releases into transient visibility with normal conceal
rules. `Undock` or `DockToggle` from docked keeps a transient reveal while held,
following the normal conceal rules once the holds end. Holders today are the
pointer in the hotspot or panel, or an active resize. With no holder, a deliberate
undock hides at once: conceal animation starts immediately, without grace.
`SetMode(Hidden)` conceals directly regardless of holds. The Rust model exposes
`set_mode(edge, at, mode)`; new
compositor corner-event discrimination is a separate integration.

Those holder rules are the model's **local** driver, used by the dev host and
whenever the compositor does not report its holder plane. While it does, the
model is **command-driven** (`ShellModel::set_holder_plane`, applied through
`ShellCommandKind::HolderPlane`): the compositor owns the pointer, focus and
popup holders and the 800 ms conceal delay, and its commands arrive as
`PanelInput::HolderReveal` (some holder holds the edge) and
`PanelInput::HolderConceal` (the last one released, its delay already served).
Corner and pointer membership then only record state — they neither reveal nor
arm grace, and no grace deadline is ever armed. A conceal waits only for a
local hold to end: an open corner menu, a resize, the startup intro, or an
explicit `show`/`toggle`, which a repeated verdict does not end but a release
after a hold (the pointer came and went) does. A deliberate hide from a
persistent mode (`SetMode(Hidden)`, an unheld undock) latches against the
compositor's verdict on the hidden report until it next reports the holders
released, so a still-open menu cannot reopen the panel it just hid. Hide,
Escape and toggle-off latch the same way while the compositor holds the edge,
because a replayed hidden report (a registry receipt, a gap) restates its
reveal, and also while the local membership shows the pointer inside before
comp has said so; that membership-only latch ends when the pointer leaves. A
held unpin or undock keeps its reveal until that verdict, or for at most the
800 ms grace: with no verdict by then it counts as unheld and conceals. Every mode
change re-sends the edge's report even when it nets to the one comp last
acknowledged (a pin and unpin in one pass, or both inside a retry backoff), so
that verdict always comes. Going command-driven drops
any local grace deadline, concealing at once a reveal the local membership no
longer holds; going back to local rules gives an unheld reveal its full grace
from the moment of the change. A
model replacement keeps the current driver.

## Event-driven wake contract

There is no fixed rendering tick or animation sleep. The calloop runner
coalesces work into one `app.update()` demand. `Idle` removes the model timer
and blocks on the Wayland file descriptor when no configure or frame request
is outstanding. `WakeAt` owns one replaceable absolute calloop timer.
`Animate` advances only from `wl_surface.frame` callbacks, with at most one
outstanding callback per mapped animating panel. The bottom clock's deadline
is content work, armed for the next wall-clock second only while the bottom
panel is mapped with its clock-bearing `launcher` page active; any other
bottom page (a citizen's scene page, for instance) or an unmapped panel arms
none. Unchanged clock text is not rewritten. Callbacks are generation-tagged, so late or expired callbacks are
ignored (the tag is a saturating 64-bit counter: reuse would need 2^64
requests, which no process lifetime reaches).

The clock follows the system timezone, or the process `TZ` override, and shows
local time with an explicit numeric UTC offset. Persist timezone preferences
through the operating system's timezone configuration; Quoin reuses that
configuration on subsequent launches.

Background preference reconciliation shares this deadline mechanism and
never polls while its page is hidden. Quoin reads the wallpaper snapshot once
per connection, again after each write, and again whenever the System page is
opened; while that page is visible it re-reads on a `wallpaper.props.changed`
(or `bg-showcase.props.changed`) invalidation, on a delivery gap in Quoin's own
Bus queue, and as a backstop 30 s after its last good read. While the page is
hidden an invalidation only marks the snapshot stale. The backstop exists
because noded drops a notification silently when a subscriber's queue is full,
and the last notice of a burst has no successor to reveal the loss: a visible
page can therefore show stale values until the next change, the 30 s backstop,
or a reopen. A failed read retries only while the page is visible, backing off
from 2 s to at most 30 s. Unchanged values do not rewrite widget text.

Keyboard repeat shares this wake layer. The active key owns one replaceable
absolute deadline; a due wake emits one coalesced repeat and arms the next
deadline from the actual wake time. There is no per-key thread, catch-up burst,
sleep loop or refresh timer. `Idle` therefore has no timer unless a real
repeat, configure or other bounded deadline is outstanding.

Two bounded one-shot liveness backstops share that single timer. The frame
backstop participates only while the policy is `Animate` and a frame callback
is outstanding. Its deadline is derived from the oldest request: one second
after that request, rounded up to a 250 ms boundary. It releases only callbacks
that are at least one second old and requests one coalesced update;
quantisation lets consecutive animated frames retain the same timer source.
A bufferless map independently arms a ten-second configure deadline; expiry
exits with a distinct abnormal reason. These are not ticks: no backstop remains
armed when its qualifying work is absent, and timely compositor replies
replace or remove the deadline before it can fire.

## Exit status and reasons

The final marker is `QUOIN_LAYER_HOST_EXIT reason=...`. `--help` is the one
successful path which prints usage and no marker. Error details replace `*` in
the patterns below.

| Status | Reason value or pattern | Meaning |
|---|---|---|
| 0 | no marker (`--help`) | Usage requested. |
| 0 | `signal-int`, `signal-term` | Clean SIGINT or SIGTERM drain. |
| 0 | `layer-surface-closed-{Left,Bottom,Right,Top}` | The compositor closed a live layer surface. |
| 0 | `selected-output-removed-no-replacement` | The selected output disappeared and no eligible replacement remained. |
| 0 | `bevy-app-exit`, `clean` | Clean application exit, or the defensive clean fallback. |
| non-zero | `invalid-cli` | Invalid command-line arguments. |
| non-zero | `wayland-connect-failed-*`, `wayland-registry-failed-*`, `wl-compositor-unavailable-*`, `layer-shell-unavailable-*` | Wayland connection, registry or required-global setup failed. |
| non-zero | `output-discovery-failed-*`, `requested-output-unavailable-*`, `no-complete-output`, `v1-output-limit-exceeded` | Output discovery or single-output selection failed. |
| non-zero | `raw-handle-failed-*`, `panel construction count was not four`, `forbidden-bevy-host-plugin-active`, `render-device-texture-limit-unavailable` | Initial surface/renderer setup failed an invariant. The texture-limit reason specifically means that no usable `RenderDevice` was present after Bevy finish; renderer initialisation failures retain Bevy's existing panic path. |
| non-zero | `output-replacement-failed-*`, `surface-plan-failed-*` | Output migration or surface reconciliation failed. |
| non-zero | `configure-out-of-range-*`, `configure-timeout-*` | A configure was invalid or did not arrive in time. |
| non-zero | `wake-deadline-stuck`, `wake-timer-failed-*` | Wake scheduling stopped making progress or its timer failed. |
| non-zero | `bevy-app-error` | Bevy requested an error exit. |
| non-zero | `calloop-create-failed-*`, `wayland-source-failed-*`, `signal-source-failed-*`, `signal-source-insert-failed-*`, `calloop-dispatch-failed-*`, `wayland-flush-failed-*` | Event-loop, signal integration, dispatch or Wayland flushing failed. |

## Bus identity and live power

Quoin registers the stable Bus service identity `shell`; its subscription
plane is `shell-sub`. `shell.ping` and `shell.info` provide presence and
discovery. Live panel state is read through the uniform
`shell.props.{get,list,describe}` surface under
`panels.<edge>.{visible,pinned,mode,width_px,page,pages,output}`.

`mode` is the precise mode signal: `hidden`, `pinned` (a persistent overlay
that reserves no space) or `docked` (reserves its full thickness), independent
of transient visibility. `pinned` is a read-compatibility shim: true for either
persistent `Pinned` or `Docked`, false for `Hidden` even while transiently
revealed. Existing Bus `pin` retains its reserving behaviour (`Docked`),
including the corner-addressed alias, and existing Bus `unpin` releases either
persistent mode into transient grace; follow it with `hide` to conceal. The
legacy pair is deliberately unchanged; the corner menu, keyboard bindings and
citizens that need to drive a mode explicitly use the precise verbs below.

The semantic verbs are `shell.panel.{show,hide,toggle,pin,unpin,dock,mode}`,
`shell.panel.page.{next,prev,set}` and `shell.quit`. They require a broker-stamped local,
registered caller and are translated to the same `ShellCommand` ingress used
by Quoin's controls. Replies acknowledge validation and enqueueing, not disk
persistence. `shell.quit` and the right Monitoring page's Quit Quoin button
request a successful Bevy exit through the normal render and surface drain
(`QUOIN_LAYER_HOST_EXIT reason=bevy-app-exit`). `shell.panel.resize` accepts
`edge` and `thickness_px` in the supported 120–500 range.

`shell.panel.dock` takes `edge` and enters `Docked` from any current mode —
the explicit docking route, since docking reflows the workspace and is never
a side effect of another verb. `shell.panel.mode` takes `edge` and
`mode=hidden|pinned|docked` and sets that persistent mode exactly: unlike the
legacy verbs it never leaves a transient reveal behind, and `mode=hidden`
conceals at once with no grace delay, because it is a deliberate action. Read
back `shell.props.get path="panels.<edge>.mode"` to verify the applied mode.

Sub-panels are addressed by their stable name, unique across every edge and
output. `shell.sub.register` takes `edge` and `name` (the owner is the
broker-attested caller) and fills a carousel slot without revealing or
selecting it; `shell.sub.remove` takes `name` and lands per the removal rule.

`shell.sub.activate` takes `name` and an optional `focus` (`true` or `false`,
a JSON boolean or the string; default `true`) and reveals, shows and focuses
that sub-panel (panel design §6). The carousel jumps to the page (no slide);
activation never changes the persisted mode.

- **Hidden edge, `focus=true`:** a transient reveal, and the panel asks for
  the keyboard exactly as a focus-cycle stop does — its layer requests
  exclusive keyboard interactivity until comp grants it, then drops back to
  on-demand. Until the keyboard lands Quoin holds the reveal with a `focus`
  hold on the panel's layer; once it lands Quoin releases that hold and comp's
  own focus holder keeps the panel. It ends when focus leaves the panel —
  including a click elsewhere, which works as soon as the keyboard has landed
  — on Escape, at the next focus-cycle stop, or on a hide, a corner action or
  a mode change. If comp never grants the keyboard (a session lock, a higher
  exclusive layer) the request lapses after 500 ms and the reveal it made
  ends with it (its hold is released and the panel hides): an open panel
  without the keyboard is one Escape cannot reach, since Escape goes to the
  application. An edge that was already showing when activated is left as
  it was.
- **Pinned or docked edge, `focus=true`:** the page switch, then the same
  keyboard request (and the same exits); the panel keeps the page when focus
  leaves.
- **`focus=false`:** the reveal or page switch alone — no keyboard request and
  no focus hold — for a caller that wants attention without taking the
  keyboard (a notification). A hidden edge's reveal then behaves like
  `shell.panel.show`: it stays until the pointer has come and gone or the
  panel is hidden.

The reply is
`{"accepted":true,"name":…,"edge":…,"output":…,"target":…,"focus":…}`:
`output` is where the sub-panel lives, and `target` the output the user is at
(the focused surface's, else the pointer's, as comp last reported them; `null`
when unknown). The pointer's output is refreshed only when keyboard focus
changes (or Quoin reconnects), so after the pointer alone crosses to another
output it can be stale. This Quoin runs one output, so `target` is reported
only: the sub-panel still shows on its own output. `accepted` is an
acceptance, not an application receipt: a name removed or replaced in the same
frame, after the reply, applies nothing (Quoin logs it). Refusals: an
unregistered name is refused exactly like `sub.remove`
(`sub-panel name 'NAME' is not registered`) — activation never creates; a
`focus` that is not a boolean is `focus must be true or false`. While the
compositor does not report its holder plane (below) nothing could hold the
reveal, so the verb is refused rather than shown and left to vanish:
`{"error_code":"ACTIVATION_UNAVAILABLE","error":"named activation unavailable: compositor holder plane not available","reason":"compositor holder plane not available","name":…}`.
That is the answer from a comp without the plane — every build before the one
that turns `input.corners.holders` true (restart C) — and from the embedded
host, which has no holder client; callers get the error, never a silent no-op.
When the leaf turns true Quoin re-reads it and the next activation is accepted.

Quoin 0.10.1 also accepts `shell.corner.{show,hide,toggle,pin,unpin}` with a
`corner` argument. These use the same panel state machine and caller checks:

| Corner | Controlled panel |
| --- | --- |
| `top-left` | Left |
| `bottom-left` | Bottom |
| `bottom-right` | Right |
| `top-right` | Top |

For example, a registered local Mix citizen can send
`send shell shell.corner.show corner="top-left"` or
`send shell shell.corner.pin corner="bottom-right"`.
These are semantic commands; they do not move the pointer or fabricate
compositor corner-observation notifications. Read back
`shell.props.get path="panels.left.visible"` to verify the applied state.

The Settings/Appearance panel's own controls come back as
`shell.settings.{scheme,motion,size}` (also listed by `shell.info`).
`shell.settings.scheme` takes `name` (a known scheme such as `forest`) and
applies the theme live through the same path as the chrome scheme dots,
persisting it for the next launch. `shell.settings.motion` takes
`motion slide|fade`: `slide` is accepted and written to the `carousel_motion`
field of the data-only `conf.mix`; `fade` is refused with
`MOTION_FADE_UNAVAILABLE` until the scene renderer can stack sibling
documents in one rectangle, and a `fade` authored directly in `conf.mix`
ingests but renders as slide (a `QUOIN_CONFIG` line says so at ingest, and
the panel's marks follow the ingested value). A motion write re-encodes the
whole `conf.mix`: other authored values are preserved, but comments and
formatting are not. `shell.settings.size` takes `edge` and `delta_px` and
enqueues the same resize commit an edge-drag completion produces, clamped to
the supported thickness range and the output budget. Like the scene and
sub-panel verbs, settings verbs from a stale Quoin connection are refused.
Read back `shell.props.get path="panels.<edge>.width_px"` to verify a size
change.

`shell.debug.status` exposes process-lifetime request/rejection counts,
accepted mutation counts, maximum dispatch time in microseconds, pending reply
count and connection state. It excludes model application, transport delay and
persistence; accepted mutations are not application receipts. Counters describe
requests before the status request itself. Debug-level `QUOIN_BUS_DISPATCH`
records mutation/rejection command names, return codes and dispatch time without
logging argument bodies. Existing `QUOIN_REVEAL`, `QUOIN_CONCEAL` and `QUOIN_PIN`
records report panel state-machine effects. Enable the module through the normal
Bevy log filter, for example `RUST_LOG=info,cosmix_quoin::bus_service=debug`.

The shell chrome avoids assigning unchanged text, display, transform and tab
index values during animation updates. This preserves Bevy's change detection
instead of repeatedly invalidating the same UI state. It does not change panel
animation or the layer host's surface creation/retirement rules.

### Background controls

The right-hand System page has nine controls for
[cosmix-wallpaper](cosmix-wallpaper.md): enabled, paused, palette, bird count,
speed, pointer radius, window margin, frame limit and seed. Boolean buttons
toggle, palette and numeric buttons cycle through common choices, and Seed
advances the deterministic scene seed. The wallpaper property API accepts the
full supported ranges.

Quoin reads and writes the `wallpaper` service through its existing Bus
connection. A successful write is followed by a fresh read before another
activation is allowed. Controls retain keyboard focus during this exchange;
hidden pages and unavailable services cannot activate them. Disconnects clear
cached values, and reconnection fetches the current preferences. Wallpaper
owns persistence, so Quoin and Mix control the same saved settings.

### Launch state and lifecycle

The bottom `launcher` page includes working Foot, Firefox and Thunderbird buttons.
They start `foot`, `firefox` and `thunderbird` through argv, report startup and failures,
and disable duplicate requests for each app while its launched process is running.
An open application does not block the other launchers. Hidden panels and
other carousel pages cannot activate these buttons. The other application names remain
static labels.

Operators can set `COSMIX_QUOIN_LAUNCHER` to an absolute Mix script path.
Quoin invokes `/opt/cosmix/bin/mix <script> <app>` without shell parsing,
where `<app>` is exactly `foot`, `firefox` or `thunderbird`.
The helper is responsible for its application's account, display and service
lifetime; a successful helper exit does not prove a window was mapped.
Process feedback wakes the UI without periodic polling. The default child
inherits Quoin's environment and service lifetime.

Quoin loads strict-data `$COSMIX_VAR/quoin.state.mix` before constructing its
initial model, using the shared path resolver (including its XDG fallback).
The v3 root contains exactly three fields: `version: 3`, `scheme`, and
`outputs` — a map from an output identity to that output's four edges. Each
output entry has exactly `left`, `bottom`, `right` and `top`; each edge has
exactly `thickness_px`, `mode` and `page`:

```text
{
  version: 3,
  scheme: "builtin",
  outputs: {
    "connector:DP-1": {
      left:   {thickness_px: 240, mode: "hidden", page: "nav"},
      bottom: {thickness_px: 60,  mode: "docked", page: "tasks"},
      right:  {thickness_px: 240, mode: "pinned", page: "monitor"},
      top:    {thickness_px: 32,  mode: "hidden", page: "status"}
    },
    "connector:HDMI-1": {
      left:   {thickness_px: 200, mode: "docked", page: "places"},
      bottom: {thickness_px: 60,  mode: "hidden", page: "tasks"},
      right:  {thickness_px: 240, mode: "hidden", page: "monitor"},
      top:    {thickness_px: 32,  mode: "hidden", page: "status"}
    }
  }
}
```

An output's key in the `outputs` map is its persistent identity, not its
geometry: today `connector:<name>`, where `<name>` is the connector name comp
reports for the output — the `outputs` props row name the layer host's
`OutputKey` mirrors. comp cannot yet supply EDID make/model/serial, so no
EDID-based identity tier exists; keys are opaque non-empty strings, so an
`edid:` tier can be added when comp grows EDID fields without another format
version. Real identities always carry a prefix, which keeps them clear of the
reserved `default` entry that legacy files migrate to. Only connector names
are persistent identities: an output the compositor has not named (the layer
host's `wl-output-<id>` fallback, the embedded host's pre-observation
placeholder) restores nothing, claims nothing and is never persisted —
protocol ids are reassigned across sessions and must not anchor state.

Modes are the strings `hidden`, `pinned` or `docked`. Thickness must be finite
and positive; unknown page IDs use the edge's default page. On restore, an
output with an entry under its own identity reuses it as-is. The strict
legacy five-field root (no version) with three-field edges containing
`pinned` booleans is migrated in memory — true becomes `docked`, false
becomes `hidden`, preserving thickness, page and scheme — as is a v2 root
(`version: 2` with the four edges at the top level). Both park their single
edge set under the reserved `default` output entry, and the first output to
restore claims it, re-filing the entry under its own identity; a different,
later output gets the default config instead. Only successfully parsed
legacy files migrate, and an output with no matching entry (and no unclaimed
`default`) restores nothing: it keeps the default config and gains no entry
until its first save. The next normal persistent mutation writes v3,
rewriting only the current output's entry — other outputs' remembered state
stays for reconnection. Loading and transient visibility do not rewrite the
file. A missing file is a first run: hidden defaults, one diagnostic line,
and persistence stays enabled so the first accepted mutation creates the
file. A file that exists but cannot be parsed (or read) also uses hidden
defaults with one diagnostic line, and disables persistence for the whole
session: a later mutation must not overwrite a file this Quoin never
successfully read. Fixing or removing the file restores persistence on the
next launch.

Accepted mode, page, scheme and completed resize changes save state after the Model stage,
using a temporary file and atomic rename. A same-output rebuild carries live
mode, page and thickness state; an output change keeps the replacement's
restored or default state, so live state never crosses outputs. Both smoke
modes skip restore, saving and the intro pulse.

A normal cold start transiently reveals hidden panels for two seconds, then releases
a temporary startup hold into normal 800 ms grace. This discovery pulse is
an explicit exception to compositor-only corner reveal. Real corner and
pointer membership remain independent and can keep panels revealed after
the pulse expires. Restored pins and docks keep their persistent modes. When the
model is command-driven the pulse ends without grace: the panel conceals at
once unless comp reports a holder.

`setup.mix --desktop` installs `dev.cosmix.quoin.desktop` into the user's XDG
applications directory, pointing at the installed checkout binary. Quit
completes the existing render/surface drain; the launcher adds no settle sleep.

The bottom carousel places `power` immediately after the clock-bearing
launcher page. It subscribes to `power.props.changed` before reading
`power.props.get`, snapshots again on reconnect, on a delivery gap, on a
change arriving while it holds no snapshot (a powerd that was down at connect
recovers on its first publication — no broker reconnect needed), and on a
stale event sequence while live (a restarted powerd republishes from 1), and
never polls. Before an authoritative snapshot it says `Power unavailable`; a
host without a battery says `No system battery`; a partial battery reading
names missing charge or state explicitly; a full reading renders only the
supplied percentage, state, time, rate and health fields. Missing values are
never rendered as zero.

## Interaction boundary

### Holder control plane

The standalone host reads the selected comp's read-only
`input.corners.holders` capability (`comp.props.get`) before sending
`comp.panel.mode` or `comp.panel.hold`; like every comp verb these are literal
commands addressed to the `--comp-service` instance. Missing, false or failed
reads leave the plane inactive and the model keeps its local rules. A comp
that reports `true` also enforces concealment on a stalled Quoin (below), so
the plane goes live with that comp build. Mode reports carry the Bus
connection `generation` only once the leaf has read `true`: that comp is the
one that knows the field. The deploy order is Quoin first (restart B), then
comp (restart C); a comp with the holder verbs but without enforcement (the
chunk 13/14 builds) was never shipped, and an older comp answers the leaf
read with an unknown path, which keeps the plane off.
Reconnects, comp arriving or leaving, delivery gaps (comp's gap frames and
client-side inbound drops) and a change to the leaf close the gate, re-read it
and replay the desired state. A registry receipt that finds comp still present
keeps the gate open and re-reads and replays in the background, since comp may
have re-registered in between. Actual mode reports precede popup acquisitions
from the corner-menu call sites. A focusing activation of a hidden edge
acquires a `focus` hold naming the panel's own layer token once that layer
maps (an acquisition that overtakes the mapping is refused and resent on the
next `surface.mapped`), and releases it when the keyboard lands on the panel
(comp's focus holder then holds it), when the keyboard request ends without
landing, when the reveal ends or when the edge goes persistent. All of these
are read from Quoin's own model, never from comp's focus events, so the order
in which comp's events and replies arrive cannot end the hold early. Comp's
`focus.changed` topic drives activation targeting only: each change starts
one `comp.props.get path=focus` read, then the `surfaces.s<id>.output` of the
focused surface and of the one under the pointer. No pointer lease is held.

A refused request is resent only on the event that can change the answer: a
layer mapping for surface and output refusals, a session-lock change for
`locked`, and a changed intent otherwise. A busy comp, a timeout or a transport
failure is retried once on a one-shot deadline whose delay doubles from 250 ms
to 8 s (a cap on the delay, not the attempts); nothing at all is sent while
that deadline is pending, and there is no polling. Comp keeps a hold while
Quoin reconnects or comp's own registration lapses, so an acquired hold is
remembered until its release is acknowledged: a menu that closes during either
outage is released afterwards, and an acknowledged release is never replayed.

Each panel gets a unique layer-shell namespace token at creation, exposed to
the client through `PanelLayerIdentities`. Requests carry that token as
`surface`, plus the raw output name and edge. Comp resolves the exact namespace
on that output, refusing a token that names more than one layer. Concealment
destroys the panel layer, so its mode report may
retain an unresolved token until the next reveal creates a new layer. A menu
can open with its panel hidden: popup holds therefore name the menu's own
unique layer token, exposed through `PopupLayerIdentity`. Releases accept that
same token even after Wayland destruction overtakes the Bus request. Client-local
Wayland object numbers cannot identify a surface across connections; an explicit
namespace token avoids that ambiguity, so no topmost-surface fallback is used.
Replacement panels and menus receive new tokens; delayed commands for old
tokens are ignored.

Comp's `<service>.panel.command` topic carries version 1, output, edge,
surface, an `action` of `reveal` or `conceal`, and `event_seq`. Quoin only accepts
them with current capability, connection generation, layer identity and an
advancing sequence. The gate drives the model: the host hands every change of
it to the model (`ShellCommandKind::HolderPlane`) after draining Bus events and
again after messages, so the model is command-driven before the first command
an opened gate admits and back on local rules the moment the gate closes.
Accepted commands become `HolderReveal`/`HolderConceal` inputs for that edge.
Comp answers every hidden mode report with its current verdict, which is how a
model that has just gone command-driven — after a reconnect, a gap or a comp
restart — learns whether anything still holds a panel it shows.

Comp tracks the holders per `(output, edge)` itself (shell design §4.3). The
**pointer** holder is acquired by dwelling in the edge's hotspot or by entering
the panel's layer (which exists only while the panel is visible) or a popup it
holds; any contact with those keeps it, and leaving them all starts the 800 ms
conceal delay, which re-entry — even an undwelled pass through the hotspot —
cancels. The **focus** holder is keyboard focus on the panel's layer or a held
popup, released at once when focus moves elsewhere. The **popup** holder is the
explicit `comp.panel.hold` Quoin sends for its corner menu, released at once by
Quoin's release or by the menu layer's destruction, whichever comes first; comp
records the keyboard focus the menu displaced when it takes focus (a toplevel or
a layer such as the panel) and restores it only when the menu's destruction is
what moved focus and focus is still where comp's fallback put it; focus the
user moved off the live menu is left alone, except into a nested held menu,
which restores focus back to its parent menu when it closes. The one timer is a one-shot armed
when a lingering pointer becomes the last holder and cancelled when any holder
returns; comp reconciles holders and the timer again after handling holder
requests in the same cycle, so a release never waits for an unrelated event.
Nothing polls. Pinned and
docked panels have no holders. The embedded host has no Wayland panel layers
and does not install this standalone transport adapter.

A slow, stopped or crashed Quoin cannot keep a panel shown, hold the keyboard
or leave holds behind; stale docked reservations are the part of shell design
§7 not yet covered. Comp identifies Quoin by the Wayland client of its layers,
never by a namespace token (Quoin adds 128 random bits to each token so it
cannot be guessed), and an edge is adopted only for a token Quoin itself
reported; another live client's layer under a copied token is refused
(`panel_owner_mismatch`, which Quoin retries on the next layer mapping or
registry receipt). Quoin sends nothing extra to stay alive: comp checks it
only when it has reason to — a panel still showing 1 s after comp's conceal,
or a click or key elsewhere while only Quoin's menu or launcher holds a
panel — by re-sending an unchanged layer configure that a live Quoin
acknowledges within a second, as SCTK does on its own. A Quoin that does not
answer is taken to be stopped: its menu and focus holds drop, its Exclusive
menu loses the keyboard, and its panel and menu are hidden and excluded from
input; nothing of any other client is touched. A conceal Quoin applies (the
layer unmapped) owes nothing, so a panel shown again at once stays. When
Quoin's Wayland connection dies comp drops everything it held; when its Bus
connection goes (comp watches noded's registry), or a report arrives from a
new Bus generation (mode reports carry `generation`), its holds end there. A
report lifts comp's exclusion, and a conceal still owed is owed again with a
fresh grace. Comp's read-only `input.corners.enforced.<edge>` and
`input.corners.held.<edge>` counts show the state.

### Corner input

Production reveal comes only from the compositor's semantic corner topics;
Quoin creates no corner hotspot surfaces. `--comp-service NAME` selects the
registered compositor instance (default `comp`), giving topic headers
`<service>.corner.entered`, `<service>.corner.left`, `<service>.corner.clicked.v2`,
`<service>.corner.clicked` and `<service>.output.changed`. Their inner commands
are the same suffixes without the service prefix. The same selection scopes the
carousel furniture's hotspot inset: Quoin observes `<service>.props.changed` and
reads `input.corners.deadzone_px` through a path-scoped `<service>.props.get`
(whose reply body is the bare value at that path), re-reading on a relevant
change, reconnect, delivery gap, or any registry observation that reports the
service registered — a restarted comp republishes no initial value. Until a
read lands, or while no cosmix comp is present, the inset falls back to 10 px,
mirroring comp's own default; a failed read keeps that fallback and logs one
`QUOIN_HOTSPOT_READ_FAILED` notice per run of failures. The compositor's
embedded Quoin host passes its own registered service name the same way.
LMB toggles **Pinned** (persistent overlay); from Docked it becomes Pinned.
Shift+LMB toggles **Docked** (reserves space) and Hidden. RMB opens the corner
menu. Ctrl/Alt/Super+LMB without Shift retain pinning. All use the
counter-clockwise mapping: TL→left,
BL→bottom, BR→right, TR→top. Each click is an impulse, independent of corner
membership; the model resolves the toggle from its current mode and persists
the change. Unpinning leaves transient reveal/grace to the panel model;
undocking with Shift+LMB does so only while the panel is held, otherwise it
starts concealment immediately. The menu's Hide conceals at once from any mode.
Horizontal panels (bottom, top) carry a paging chevron at each end, inset from
the panel ends by comp's corner-hotspot size, with the title and page dots as a
centred overlay across the content strip; vertical panels (left, right) keep a
`< [title] >` header with the chevrons inside it, the header's top grown by the
same inset. Only the chevrons are inset — page content fills the panel below the
header on a side edge, and the full width on a top/bottom edge with a single
page — and an edge with a single page shows no chevrons and no inset. Chevron paging slides the
carousel 300 ms, collapsing to zero under reduced motion; named jumps — the
dots, `panel.page.set`, activation, restore and a removal's landing — go
directly to the page without the slide. Headers carry no pin glyph or mode
button; change mode at the corner, in its menu, or through the precise mode
verbs.

The compositor publishes `corner.clicked.v2` with `button`, `kind: "brief"` and
`modifiers` captured at press time (always present, even when empty). Quoin
refuses a v2 body without `modifiers` — it comes from a compositor older than
this input model — and logs ERROR `quoin_corner_old_format_rejected` with
`field=modifiers` at counts 1, 2, 4, 8, … (counted separately from other decode
rejections); `kind: "hold"` is likewise refused for either button. Against such
a compositor RMB does nothing, unmodified LMB still pins through the legacy
topic below, and Shift+LMB pins instead of docking, because that compositor
emits legacy for every LMB. That holds only on a fresh Quoin connection: a
compositor rolled back in place, without Quoin reconnecting, leaves v2 marked
as seen (so legacy stays ignored) and rewinds sequences (so records drop as
stale), and no corner click acts until Quoin reconnects. The compositor consumes
engaged corner presses and their releases, cancelling pending actions on excess
movement or disengagement. Both buttons act on release; neither has a hold action.
The menu action calls `CornerMenuHook(fn(&mut World, &OutputKey, Corner))`.
The host always provides **Pin / Dock / Hide**, with the current mode checked
and disabled; Quoin appends `conf.mix`'s `menu_items` for that edge. Each mode
choice uses the same `SetMode` command as the precise mode verbs. Extra items
invoke their declared Bus target and verb, with the string list in `args`.
The menu uses the panel chrome theme tokens. Arrow keys or Tab select an item;
Return or Space chooses it. Escape, click-away and item choice close the menu,
end its local reveal hold and release its exclusive keyboard layer. Without the
holder plane comp's existing policy then focuses the top toplevel; with it, the
menu's popup holder returns focus to the surface the menu displaced. Pin and Dock
apply before the local hold is released, so concealment cannot race the choice.
Under local rules, opening a menu at a hidden corner does not itself reveal the
panel; with the holder plane the menu's popup hold does.

Both click topics are subscribed: legacy is the fallback that keeps the first
unmodified LMB working while the v2 subscription settles. Successful
subscription is not capability discovery: the broker accepts unpublished topics.
Until a valid v2 click arrives, legacy LMB toggles Pinned immediately. The
compositor emits each unmodified LMB's legacy record at sequence N and its v2
record at N+1, including when the v2 payload has `modifiers: []`;
the host maps both to N and admits that logical click once, in either delivery
order. Every modified click emits only v2 and keeps its own sequence, including
Ctrl/Alt/Super+LMB. On observing v2 it ignores all subsequent legacy clicks for that connection.
A sequence high-water mark also rejects duplicate/stale click records, before
output-map queueing. Reconnect clears preference and sequence state; ordinary
output refreshes and loss markers retain them. This relies on the compositor's
consecutive unmodified LMB pair and monotonically increasing observation stream, not a
timing window. It is not an exactly-once guarantee across a connection reset or
publisher sequence restart; a publisher restart requires a fresh host/connection.
High-water rejections emit `quoin_corner_sequence_rejected` WARNs at counts
1, 2, 4, 8, … with the received sequence, canonical sequence and high-water mark.
These include legitimate duplicates (including the first legacy/v2 pair);
persistent low sequences can indicate a compositor restart. Legacy clicks ignored
after v2 discovery do not increment this separate counter. The counter resets
with connection preference state. Automatic publisher-restart recovery is not
implemented: `info.instance` identifies the compositor process, but is absent
from the subscribed corner/output payloads and the host's `outputs` query.

Quoin subscribes to the corner and output topics before addressing the selected service
with the fixed `comp.props.get` request verb at `outputs`. It maps the topic's
stable `o_<slug>` output key through the public
row's raw `name` and accepts only the exact SCTK-selected output. Output-change
notices and each reconnect generation refresh the complete map. A topic gap,
disconnect, channel overflow, output replacement or shutdown clears every
corner hold conservatively. Broker absence or restart disables only corner
ingress: the layer surfaces, pointer controls and static smoke mode continue,
and reconnect is automatic and starts disengaged. Topic delivery authenticates
neither the original publisher nor its `from` header, so any local publisher
authorised for those topics can inject corner events.

Quoin opts into `cosmix-lib-client`'s bounded subscription receiver at 64
commands. The socket reader never waits for capacity; a full lane drops the new
frame and surfaces an overflow marker. Quoin treats that marker exactly like a
disconnect: synthesize left for all engagements, invalidate the slug map, and
refresh it before accepting mapped corner state again.
A lost click is a missed toggle and is never replayed or synthetically recovered.
If only a click is dropped at the host-to-runner queue, existing holds are retained.

Under the model's local rules, a compositor enter reveals and holds the
counter-clockwise edge (TL→left, BL→bottom, BR→right, TR→top). Matching left
starts the 800 ms grace only when the native pointer is also outside. When comp
reports the holder plane, these events only record membership and comp's
commands reveal and conceal instead (see the holder control plane above). Native SCTK pointer enter/leave supplies the second
hold; Bevy pointer button events drive both carousel chevrons and page
dots. Pin survives both leaves, and unpin outside both holds starts normal
grace. Wheel events are delivered to Bevy although current chrome does not
consume them.

SCTK installs the compositor xkb keymap and maps physical keys, logical keys,
text, modifiers and repeat into Bevy's input model. Focus loss, panel teardown
and keyboard capability loss synthesize releases for held keys, clear pressed
state and stop repeat. Compositor repeat settings are clamped to 1–125 Hz and a
50–2000 ms delay; each fired deadline advances strictly beyond both its prior
deadline and current model time. Because SCTK does not expose the raw XKB masks
needed to reinterpret repeated text safely, any compositor modifier callback
stops repeat for that press; release and press the key again to resume. Touch
down is attributed to the exact panel surface; motion and up retain that local
Bevy window attribution. Touch cancel,
teardown and capability loss emit cancellation and clear every held contact.
Quoin chooses the first advertised seat independently for pointer, keyboard
and touch, and fails each capability over after its selected seat is removed.

The pure `CornerDetector` remains a development-host tuning tool and is not a
production reveal source.

### Keyboard

`conf.mix` binds keys per edge and one focus-cycle key. They are all unbound by
default:

```mix
{bindings: {left: {pin: "Super+Shift+Left", dock: "Super+Shift+D", hide: nil},
            right: {dock: "Super+F2"},
            cycle_focus: "Super+Tab"}}
```

Chords are canonicalised at ingestion, so modifier order does not matter.
Every chord needs Ctrl, Alt or Super. A bare or Shift-only key would take
typing from the panel's own controls. Escape cannot be bound with any
modifiers, because any Escape reaching a panel is the panel's own Escape.
Duplicates, unknown keys, modifier-less chords and Escape chords are refused.
`Super+Escape` is also comp's reserved chord. The refusal is loud and the
previous configuration is kept.

Letters and digits match the key labelled with them in the current layout,
and named keys match by meaning, so a keypad arrow is `Left`. Any other key
falls back to its physical position, so `Ctrl+Shift+1` is the 1 key even
though it types `!`. Modifiers count as they were at the moment of the press.
A binding fires only on a fresh press. Key repeat does not fire it, and
neither does a key that was already held when a panel received focus. Such
keys have to be released and pressed again, so a cycle stop can never be
triggered by the chord that moved focus there.

- **pin / dock / hide** each send the precise mode command
  (`SetMode(pinned|docked|hidden)`), the same one the corner menu and
  `shell.panel.mode` send. They never use the legacy `pin`/`unpin` pair.
- **cycle_focus** moves the keyboard to the next visible pinned or docked
  panel on this output, in left, bottom, right, top order. After the last
  panel, focus goes back to the application. Transient reveals are not stops.
  A Wayland client cannot focus its own layer surface, so the cycle requests
  `Exclusive` interactivity on the target panel until the keyboard lands, then
  drops back to `OnDemand` (comp lets a demoted layer keep the keyboard it was
  granted). The panel keeps the keyboard until focus leaves it — a click
  elsewhere included — Escape, the next cycle stop, or until it unmaps. Named
  activation (`shell.sub.activate`) makes the same request. Comp grants the
  request only for a panel it is actually showing. If the keyboard has not
  arrived within 500 ms, the request is withdrawn, so a panel shown later
  never takes the keyboard on its own.
- **Escape** from a focused panel hides it only if it is a transient reveal.
  Only the focused panel receives it. If the pointer is still in the panel or
  its hotspot, the reveal latches: no hover can re-reveal it until the slide
  out has finished and the pointer is out of the hotspot. Pointer leaves and
  re-entries during the slide do not end the latch, because the slide itself
  moves the pointer off the surface. Escape on a pinned or docked panel
  changes neither mode nor reservation. In every case focus goes back: all
  panels refuse the keyboard, comp's policy focuses the top toplevel, and
  ordinary `OnDemand` resumes once Quoin sees that no panel has the keyboard.
  Exact restoration of the previous surface belongs to the compositor focus
  holder.

**Scope.** Quoin has no global key grab. A binding works only while one of
Quoin's own panels holds the keyboard, after a click into it or a cycle stop.
So a binding can never shadow an application's shortcut. It also means that a
binding cannot reveal a panel while an application is focused. That route
needs a chord grab in the compositor, which does not exist yet.

Keyboard actions target the output of the focused window, or the pointer's
output when no window has focus. The keys Quoin receives always come from its
own focused panel, so they target that panel's output. The pointer fallback
applies only to the future compositor-grabbed route.

Only while a request is still waiting for the keyboard is the panel
`Exclusive`, and a click elsewhere cannot take the keyboard then. Once the
keyboard has landed, a click elsewhere moves it away like from any
`OnDemand` panel, which ends the request.

The latch described above is the local one, used while comp does not report
the holder plane. A hide latches in the same way when the pointer is inside:
`hide`, a toggle-off, or a mode set to hidden, whether from a binding, the
menu or `shell.panel.mode`. When comp does report the holder plane, all of
them take the command-driven latch described earlier. The panel stays
concealed against comp's reveals until comp reports that the holders have been
released. If comp had not yet reported a hold, the latch ends when the pointer
leaves instead. A local latch that is still standing when comp starts
reporting the holder plane carries over as that pointer-only latch.

Stable transition markers are:

```text
QUOIN_REVEAL edge=left trigger=corner
QUOIN_REVEAL edge=left trigger=holders
QUOIN_CONCEAL edge=left reason=corner-left
QUOIN_CONCEAL edge=left reason=grace
QUOIN_CONCEAL edge=left reason=holders
QUOIN_MODE edge=left mode=pinned
QUOIN_MODE edge=left mode=docked
QUOIN_MODE edge=left mode=hidden
```

The edge is one of `left`, `bottom`, `right` or `top`. A marker is printed once
per real semantic transition. The `holders` forms come only from the
command-driven model. `--smoke-all-panels` starts all four panels
docked and retains the compatibility `QUOIN_PIN edge=... state=pinned` smoke
marker per edge before the existing four-surface
ready marker. Mutually exclusive `--smoke-hidden` starts them hidden and prints
`QUOIN_HIDDEN_READY panels=4` after the first complete hidden frame.

Chrome retains semantic AccessKit nodes, but disabling winit also removes its
platform AccessKit adapter. Platform accessibility is therefore deferred and
is not claimed by this host.

Planner and raw-owner tests run without a compositor. A fake-Wayland-peer wire
test was not added in this slice: it could check request ordering, but it
cannot exercise Bevy/wgpu's foreign WSI attach/present boundary, and building a
second miniature layer-shell compositor fixture solely for the partial claim
was not reasonable for this gate. The live nested compositor gate remains the
wire and presentation authority.

This arc vendors nothing and edits no Smithay source. Quoin consumes
`cosmix-comp`'s documented public layer-shell contract unchanged.

## Source gates

`setup.mix --desktop` first retains the desktop workspace release build used
for every other app, then rebuilds the installed `cosmix-quoin` binary from the
isolated shipping selection in `src/desktop/Cargo.toml`, using
`src/desktop/target/quoin-install` as its target directory. The demo target is
still skipped because it has `required-features = ["demo"]`.

The `cosmix-shell-host` test suite reads that same shipping selection and proves
its full locked, offline graph remains Wayland-only. Cargo does not expose a
dependency's active features through a consumer crate's `cfg`, so the test
inspects Cargo's forward feature graph directly, rejects multiple resolved
`winit` packages, clears inherited `CARGO_*` configuration, requires Wayland
and rejects X11:

```sh
cargo test -p cosmix-shell-host shipped_quoin_graph_has_wayland_without_x11
```

## Hardware-only deferrals

The source and nested gates cannot prove these real-session paths:

- the KMS four-panel path, including fuzzel retaining its `Exclusive` latch
  while Quoin's `None` and `OnDemand` panels redraw;
- texture-limit preflight against the real GPU's negotiated limit;
- scale 2 and a real 1.25 fractional scale end-to-end through
  `wp_fractional_scale` and viewporter;
- keyboard repeat pausing rather than bursting under KMS load; and
- VT switch and seat re-add on a real session.

## Background demos and capture

The right panel's **Demos** tab selects Bloom, Shapes, Boing or Boids through
the native `bg-showcase` Bus service. Camera toggles fixed/moving views for the
three 3D scenes; Boing also has a Kick button. An optional compositor F9 binding
can invoke the same `boing.kick` verb.

**Save screenshot** captures the complete selected output, including Quoin,
the cursor and open windows. **Record full screen MP4** starts a silent 30 fps
recording; click **Stop recording MP4** to finalise it. The native
`cosmix-capture` citizen must be running on that Wayland session. Recordings
have a five-minute ceiling. Pending and finalising states are distinct from
`complete`, which displays the saved file path. Files default to
`~/Videos/Cosmix` in the capture citizen's account, or its configured directory.

These controls share Quoin's existing asynchronous Bus bridge, reconcile after
reconnect and report unavailable services. Neither service publishes a change
notification for this state, so Quoin reads both statuses once per connection,
after each action and when the Demos tab opens, then at most every 5 s while
the tab stays visible; a hidden tab polls nothing. Failed reads back off from
2 s to at most 30 s, visible-only. They do not launch competing
background processes. Session supervision starts one showcase and one capture
citizen alongside the compositor. The existing System-page boids preferences
can target the showcase with `COSMIX_QUOIN_BACKGROUND_SERVICE=bg-showcase`;
the default `wallpaper` target supports the compatibility wallpaper binary.

## Host limits

This host deliberately supports one output runtime and one active device per
input capability. It does not mirror panels across several outputs; pointer,
keyboard and touch may come from different seats when the compositor splits
those capabilities. Use `--output NAME` when advertisement-order selection is
not appropriate.

On compositors binding `wl_keyboard` below version 4 and therefore sending no
`repeat_info`, SCTK 0.19.2 supplies no usable synthetic default, so keyboard
repeat is unavailable.

Destroy-and-recreate is required because the current compositor rejects an
acknowledgement for a configure serial retained across unmap. If the
compositor later tolerates stale post-unmap acknowledgements, a comp-side
follow-up could make the cheaper attach-NULL path safe; this host does not
assume that behaviour.

## Frame-stall diagnostics

Set `COSMIX_FRAME_TRACE=1` before launching Quoin, the compositor and a native
scene host to emit bounded `FRAME_TRACE` records to stderr. Tracing is off by
default. Records carry process identity, stage, monotonic start/end microseconds,
wall duration and caller-thread CPU duration. Monotonic timestamps correlate
processes on the same kernel; they are not cross-machine timestamps. Caller CPU
excludes parallel worker CPU. Acquisition markers bound `prepare_windows` but
may include other scheduler work; they are not GPU timestamps. Dispatch spans
include intentional idle waits.

Quoin and scene records separate main schedules, extraction, rendering and
window acquisition. Quoin also records panel create/configure/unmap and render
drains. Compositor records distinguish scene projection, rendering, GPU retirement,
page-flip waiting and capture readback/packing. Background submission cadence is
separate from physical output presentation and encoded video frame rate.

Render-graph markers additionally separate command recording from graph submission.
The host `render_tail` and compositor `comp_render_finalize` stages start after
the render graph and end after Bevy's render system. They include its final GPU
submission, window presentation and screenshot collection: a long tail does not
by itself identify a slow presentation call. Nested spans overlap; do not add
their durations together as independent frame costs. GPU-submission wall time
can include driver waits and is not a measurement of GPU execution time.

The vendored wgpu observer adds `wgpu_queue_submit`, `wgpu_surface_configure`,
`wgpu_surface_acquire` and `wgpu_surface_present`. These bracket the original API
calls, including any waits; an end record is not a success receipt. Surface
`subject` IDs are process-local and survive reconfiguration, while recreated
surfaces receive new IDs. Queue submissions use subject zero, not a queue ID.
The observer is installed only for enabled tracing and performs no log I/O.

Host `host_window_surface` records link a Bevy window (`subject`, Entity bits)
to a wgpu surface (`detail`). `host_window_submitted` counts acquired textures
consumed by Bevy for presentation, not displayed frames. Quoin's
`quoin_panel_window_{left,right,top,bottom}` markers identify the panel's Bevy
window. `quoin_panel_set_margin`, `quoin_panel_set_size`,
`quoin_panel_frame_request` and `quoin_panel_frame_done` use that same window
identity. These short event spans count placement, size and callback activity;
they do not measure UI content changes. Compare them against submissions to
identify redraws coinciding with movement before deciding they are redundant.

The observer's same-thread nesting is bounded to 16 API calls. Deeper nested
calls are suppressed without closing an outer span. Calls acquired before the
observer was installed may have surface identity zero. Do not combine IDs
between processes or sessions, or treat the count of submissions as scanout.

Logging uses bounded queues and non-blocking sends, with a roughly 65,536-record
process budget. Inspect sequence gaps and dropped/capped fields before treating
a trace as complete. Writer-thread I/O can block without blocking frame-thread
sends; initial recorder setup and diagnostic schedule markers still have overhead.
Exclude startup and compare a final tracing-disabled run. No text-entry or Bus
request bodies are recorded.

`COSMIX_QUOIN_PRESENT_MODE=fifo` is the default. The experimental
`auto-no-vsync` override changes all Quoin panel swapchains for a controlled
acquisition-wait comparison. It does not change the compositor's physical scanout
mode, remove GPU lifetime barriers or promise smooth presentation. Compare the
same panel sequence and stable pinned panels with capture disabled first.

For a useful stall investigation, compare a warmed-up hidden scene, each panel
show/hide, one pinned panel and all four pinned panels. Repeat with identical
scene quality settings, then change one setting at a time. Run capture as a
separate comparison. A nominal 60 fps limit or 30 fps MP4 header does not establish
fresh-frame delivery at that rate; use interval distributions and actual display
or capture completion counts. Keep GPU ownership, retirement and frame-callback
barriers intact while investigating waits.

Additional flow diagnostics include OS `tid`, `wgpu_queue_submit_inner`,
`wgpu_queue_deferred_actions`, and `wgpu_device_poll` (subject 0 = Poll,
1 = Wait). The compositor prefixes these stages with `comp_`. Overlapping a
queue call and another thread's device wait is correlation, not proof of lock
contention. These observations do not remove or shorten any GPU lifetime barrier.

`host_window_wayland` maps Entity bits to a client-local Wayland surface wire
ID, including when a panel's Wayland surface is recreated. `scene_frame_requested`
and `quoin_frame_requested` record surface wire ID and callback wire ID;
the corresponding `*_frame_received` records observe Rust event dispatch,
not socket arrival or successful acceptance of a callback generation. Pair wire
IDs with the client process and time because Wayland can reuse them.

Compositor `comp_surface_client` records carry SurfaceId, surface wire ID and
peer PID. `comp_surface_buffer` associates the surface with a process-local
buffer ObjectId hash. `comp_buffer_retain`, `comp_buffer_drop_owner` and
`comp_buffer_release_queued` follow ownership tokens; hashes are diagnostic
correlation values, not security identities. Release-queued observations are
protocol enqueue events, not explicit-sync signal completion. Likewise,
`comp_callback_done_queued` means a callback was enqueued, not delivered.

The `comp_pulse_{sent,skipped}_{busy,idle}` events expose the existing callback
cadence decision. Subject is submission count; detail and aux are coordinator
observed time and the previous deadline in microseconds. Their difference is
deadline lateness in that clock domain. A skipped busy pulse alone does not
establish a lost display frame; correlate it with actual callback dispatch and
client submissions. Cadence and rendering policy are unchanged by these probes.
