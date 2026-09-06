# cosmix-quoin

`cosmix-quoin` is the Cosmix desktop furniture shell. It owns four independent
`zwlr_layer_surface_v1` panels — left, bottom, right and top — and renders the
existing Quoin chrome into one explicit Bevy window target per surface. The
installable application id and layer namespace are `dev.cosmix.quoin`.

Arc 3 presents real layer-shell buffers through `cosmix-shell-host` 0.4.0,
`cosmix-shell` 0.5.0 and SCTK 0.19.2. `cosmix-quoin` is 0.5.0;
`cosmix-quoin-demo` remains a
feature-gated, non-installable normal-window tuning arm; it is not a
layer-shell client.

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

Pinned panels use `Top`, reserve their complete logical thickness and keep
protocol margin zero; chrome alone owns their transient slide. Revealed and
mapped-concealing panels use `Overlay`, reserve zero and slide with their edge
protocol margin. This also covers pin-from-hidden: the full zone exists at
fraction zero while chrome supplies the only visual translation. Keyboard
policy maps only to `None` or `OnDemand`; Quoin never requests `Exclusive`.
Chrome selects its translation owner from the last successfully committed
protocol mode, not the model's next desired mode. The host advances that latch
only after a commit, or after completed role destruction for unmap, so pin and
unpin cannot hand motion between protocol margin and chrome one frame early.

## Event-driven wake contract

There is no polling, refresh tick or fixed animation sleep. The calloop runner
coalesces work into one `app.update()` demand. `Idle` removes the model timer
and blocks on the Wayland file descriptor when no configure or frame request
is outstanding. `WakeAt` owns one replaceable absolute calloop timer.
`Animate` advances only from `wl_surface.frame` callbacks, with at most one
outstanding callback per mapped animating panel. The visible bottom clock's
one-second deadline is content work and disappears when that panel is
unmapped. Callbacks are generation-tagged, so late or expired callbacks are
ignored (the tag is a saturating 64-bit counter: reuse would need 2^64
requests, which no process lifetime reaches).

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
`panels.<edge>.{visible,pinned,width_px,page,pages,output}`.

The semantic verbs are `shell.panel.{show,hide,toggle,pin,unpin}`,
`shell.panel.page.{next,prev,set}` and `shell.quit`. They require a broker-stamped local,
registered caller and are translated to the same `ShellCommand` ingress used
by Quoin's controls. Replies acknowledge validation and enqueueing, not disk
persistence. `shell.quit` and the right Monitoring page's Quit Quoin button
request a successful Bevy exit through the normal render and surface drain
(`QUOIN_LAYER_HOST_EXIT reason=bevy-app-exit`). There is no resize, arrangement, geometry or
synthetic pointer/corner verb in this slice.

### Launch state and lifecycle

The bottom `launcher` page includes working Foot and Thunderbird buttons. They
start `foot` and `thunderbird` through argv, report process startup and failures,
and disable duplicate requests for each app while its launched process is running.
An open Thunderbird does not block Foot. Hidden panels and
other carousel pages cannot activate either button. The other application names remain
static labels.

Operators can set `COSMIX_QUOIN_LAUNCHER` to an absolute Mix script path.
Quoin invokes `/opt/cosmix/bin/mix <script> foot` or
`/opt/cosmix/bin/mix <script> thunderbird` without shell parsing.
The helper is responsible for its application's account, display and service
lifetime; a successful helper exit does not prove a window was mapped.
Process feedback wakes the UI without periodic polling. The default child
inherits Quoin's environment and service lifetime.

Quoin loads strict-data `$COSMIX_VAR/quoin.state.mix` before constructing its
initial model, using the shared path resolver (including its XDG fallback).
The root map contains `scheme` and `left`, `bottom`, `right`, `top` maps with
`thickness_px`, `pinned` and stable `page` IDs. Thickness must be finite and
positive; missing or invalid files use defaults with one diagnostic line.
Unknown page IDs use the edge's default page. Scheme is retained unchanged
until theme controls are implemented.

Accepted pin and page changes save the current state after the Model stage,
using a temporary file and atomic rename. Output migration carries live
pin, page and thickness state; it never reloads disk state. Both smoke modes
skip restore, saving and the intro pulse.

A normal cold start reveals unpinned panels for two seconds, then releases
a temporary startup hold into normal 800 ms grace. This discovery pulse is
an explicit exception to compositor-only corner reveal. Real corner and
pointer membership remain independent and can keep panels revealed after
the pulse expires. Restored pins remain pinned.

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

Production reveal comes only from the compositor's semantic corner topics;
Quoin creates no corner hotspot surfaces. `--comp-service NAME` selects the
registered compositor instance (default `comp`), giving topic headers
`<service>.corner.entered`, `<service>.corner.left`, `<service>.corner.clicked` and
`<service>.output.changed`. Their inner commands remain the unprefixed
`corner.entered`, `corner.left`, `corner.clicked` and `output.changed`.
The compositor emits `corner.clicked` on a left-button press on an engaged
corner; the client toggles the clockwise edge's panel pin (TL→left, BL→bottom,
BR→right, TR→top). Each click is an impulse, independent of corner membership;
the model resolves the toggle from its current pin state and persists the
change. Unpinning leaves the panel revealed and arms grace when no hold remains.
The visible header pin control remains available as a fallback.

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

A compositor enter reveals and holds the clockwise edge (TL→left, BL→bottom,
BR→right, TR→top). Matching left starts the 800 ms grace only when the native
pointer is also outside. Native SCTK pointer enter/leave supplies the second
hold; Bevy pointer button events drive pin, both carousel chevrons and page
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

Stable transition markers are:

```text
QUOIN_REVEAL edge=left trigger=corner
QUOIN_CONCEAL edge=left reason=corner-left
QUOIN_CONCEAL edge=left reason=grace
QUOIN_PIN edge=left state=pinned
QUOIN_PIN edge=left state=unpinned
```

The edge is one of `left`, `bottom`, `right` or `top`. A marker is printed once
per real semantic transition. `--smoke-all-panels` starts all four panels
pinned and prints one pinned marker per edge before the existing four-surface
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

## Known limits

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
