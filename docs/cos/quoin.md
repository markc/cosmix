# cosmix-quoin

`cosmix-quoin` is the Cosmix desktop furniture shell. It owns four independent
`zwlr_layer_surface_v1` panels — left, bottom, right and top — and renders the
existing Quoin chrome into one explicit Bevy window target per surface. The
installable application id and layer namespace are `dev.cosmix.quoin`.

The first Arc 3 slice presents real layer-shell buffers through
`cosmix-shell-host` 0.1.6 and SCTK 0.19.2. `cosmix-quoin-demo` remains a
feature-gated, non-installable normal-window tuning arm; it is not a
layer-shell client.

## Output and scale

Version 1 owns exactly one output runtime. `--output NAME` selects an exact
complete SCTK output; without it, Quoin selects the first complete output in
advertisement order. Every panel role is created with that explicit
`wl_output`, never the compositor default. Removing the selected output closes
and render-drains its surfaces; Quoin then creates a fresh singleton runtime
on the next complete output, or exits cleanly when no requested replacement
remains.

Layer protocol dimensions remain logical. Integer output scale uses
`wl_surface.set_buffer_scale`; when fractional-scale and viewporter are
available, Quoin keeps buffer scale 1, renders `ceil(logical × scale)` pixels
and sets the viewport destination back to the configured logical size. Bevy
stores the corresponding physical resolution and scale override.

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

Two bounded one-shot liveness backstops share that single timer. While any
frame callback is outstanding, its deadline is derived from the oldest
request: one second after that request, rounded up to a 250 ms boundary. It
releases only callbacks that are at least one second old and requests one
coalesced update. The deadline persists across `Idle` and `WakeAt` until every
outstanding request is answered or expired; quantisation lets consecutive
frames retain the same timer source. A bufferless map also arms a ten-second
configure deadline; expiry exits with a distinct abnormal reason. These are
not ticks: no backstop remains armed when no request is in flight, and timely
compositor replies replace or remove the deadline before it can fire.

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
| non-zero | `raw-handle-failed-*`, `panel construction count was not four`, `forbidden-bevy-host-plugin-active` | Initial surface/renderer setup failed an invariant. |
| non-zero | `output-replacement-failed-*`, `surface-plan-failed-*` | Output migration or surface reconciliation failed. |
| non-zero | `configure-out-of-range-*`, `configure-timeout-*` | A configure was invalid or did not arrive in time. |
| non-zero | `wake-deadline-stuck`, `wake-timer-failed-*` | Wake scheduling stopped making progress or its timer failed. |
| non-zero | `bevy-app-error` | Bevy requested an error exit. |
| non-zero | `calloop-create-failed-*`, `wayland-source-failed-*`, `signal-source-failed-*`, `signal-source-insert-failed-*`, `calloop-dispatch-failed-*`, `wayland-flush-failed-*` | Event-loop, signal integration, dispatch or Wayland flushing failed. |

## Current slice boundary

Slice 1 intentionally has no pointer or keyboard bridge, no corner surfaces
and no Bus adapter. `--smoke-all-panels` starts all four panels mapped and
pinned for the presentation gate. Interim 1×1 corner hotspots arrive in slice
2; they are not a source in slice 1. The permanent corner source is the
compositor's semantic Bus topics.

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

## Known limits

Motion ownership handoff mid-motion may show a single-frame position jump
until slice 2.

Destroy-and-recreate is required because the current compositor rejects an
acknowledgement for a configure serial retained across unmap. If the
compositor later tolerates stale post-unmap acknowledgements, a comp-side
follow-up could make the cheaper attach-NULL path safe; this host does not
assume that behaviour.
