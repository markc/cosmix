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
there are no panel Wayland buffers or panel swapchains. Pinned panel geometry
updates comp's usable area, publishes output/property changes and reconfigures
maximised windows including their decoration extents. Repeated unchanged work
areas do not trigger another resize or notification. Application
processes remain ordinary Wayland clients, with their existing comp-owned
window decorations and move/resize policy.

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
installable application id and layer namespace are `dev.cosmix.quoin`.

Quoin presents real layer-shell buffers through `cosmix-shell-host`,
`cosmix-shell` and SCTK. See [component versions](../VERSIONS.md) for the
current source versions. `cosmix-quoin-demo` remains a
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

There is no fixed rendering tick or animation sleep. The calloop runner
coalesces work into one `app.update()` demand. `Idle` removes the model timer
and blocks on the Wayland file descriptor when no configure or frame request
is outstanding. `WakeAt` owns one replaceable absolute calloop timer.
`Animate` advances only from `wl_surface.frame` callbacks, with at most one
outstanding callback per mapped animating panel. The visible bottom clock's
one-second deadline is content work and disappears when that panel is
unmapped. Callbacks are generation-tagged, so late or expired callbacks are
ignored (the tag is a saturating 64-bit counter: reuse would need 2^64
requests, which no process lifetime reaches).

The clock follows the system timezone, or the process `TZ` override, and shows
local time with an explicit numeric UTC offset. Persist timezone preferences
through the operating system's timezone configuration; Quoin reuses that
configuration on subsequent launches.

Background preference reconciliation shares this deadline mechanism. It
requests a fresh wallpaper snapshot once per second, including while panels
are hidden, and also responds to Bus invalidations. Unchanged values do not
rewrite widget text.

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
(`QUOIN_LAYER_HOST_EXIT reason=bevy-app-exit`). `shell.panel.resize` accepts
`edge` and `thickness_px` in the supported 120–500 range.

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
reconnect and report unavailable services. They do not launch competing
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
