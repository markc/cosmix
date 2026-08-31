# cosmix-quoin

`cosmix-quoin` is the Cosmix desktop furniture shell. It owns four independent
`zwlr_layer_surface_v1` panels — left, bottom, right and top — and renders the
existing Quoin chrome into one explicit Bevy window target per surface. The
installable application id and layer namespace are `dev.cosmix.quoin`.

The first Arc 3 slice presents real layer-shell buffers through
`cosmix-shell-host` 0.1.0 and SCTK 0.19.2. `cosmix-quoin-demo` remains a
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
then attach a null buffer and commit. Layer-shell resets role state after an
unmap, so remap replays every property and repeats the bufferless configure
gate before another buffer can be presented.

Pinned panels use `Top`, reserve their complete logical thickness and keep
protocol margin zero; chrome alone owns their transient slide. Revealed and
mapped-concealing panels use `Overlay`, reserve zero and slide with their edge
protocol margin. This also covers pin-from-hidden: the full zone exists at
fraction zero while chrome supplies the only visual translation. Keyboard
policy maps only to `None` or `OnDemand`; Quoin never requests `Exclusive`.

## Event-driven wake contract

There is no polling, refresh tick or fixed animation sleep. The calloop runner
coalesces work into one `app.update()` demand. `Idle` removes the timer and
blocks on the Wayland file descriptor. `WakeAt` owns one replaceable absolute
calloop timer. `Animate` advances only from `wl_surface.frame` callbacks, with
at most one outstanding callback per mapped animating panel. The visible
bottom clock's one-second deadline is content work and disappears when that
panel is unmapped.

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
