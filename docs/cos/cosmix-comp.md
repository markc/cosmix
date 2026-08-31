# cosmix-comp

`cosmix-comp` is the Wayland compositor used by the Cosmix desktop. It can run
nested inside an existing Wayland session with `cosmix-comp --nested`, or use
the KMS backend on a system seat.

## Supported Wayland protocols

The compositor advertises the core compositor, subcompositor, seat, output,
shared-memory, DMA-BUF, explicit synchronisation, viewporter, fractional-scale,
presentation-time, XDG shell and XDG decoration globals needed by its desktop
clients.

| Protocol | Version | Current support |
| --- | ---: | --- |
| `zwlr_layer_shell_v1` | 4 | Layer surfaces and layer popups map, arrange and configure through Smithay's `LayerMap`; protocol strata, keyboard interactivity, input regions and exclusive usable-area effects are supported. |
| `ext_idle_notifier_v1` | 2 | Per-seat notifications use Smithay's calloop timers; real pointer, keyboard, touch, pointer-gesture and tablet-tool activity resets the timeout and resumes an idle notification. Device-removal reconciliation does not count as activity. |
| `ext_foreign_toplevel_list_v1` | 1 | Mapped XDG toplevels expose stable mapping identifiers, title and app ID updates; unmap or destruction closes the handle, and late clients receive the current mapped set. |
| `ext_session_lock_v1` | 1 | Nested mode supports immediate output-sized lock-surface configures, secure blank-first presentation acknowledgement, lock-only input and the locked/orphaned lifecycle. KMS requests receive `finished` until displayed-frame evidence is connected in the KMS session-lock slice. |

Layer surfaces stack in the protocol order: Background, Bottom, normal XDG
toplevels and popups, Top, then Overlay. Raising a surface changes its order
only within its stratum. A layer popup stays in its parent's stratum, including
when the parent changes layer. Session-lock surfaces use a sixth Lock stratum
above Overlay. An opaque compositor-owned black element sits below Lock and
above every client stratum while a lock is active.

`keyboard_interactivity=None` layers receive pointer and touch input but never
take keyboard focus or raise; their popups can use pointer/touch grabs but not a
keyboard grab. `OnDemand` layers take focus on a pointer press or first touch.
The highest stacked mapped `Exclusive` layer latches keyboard focus until it
unmaps, is destroyed, or commits a non-Exclusive policy. Normal-window clicks
may still raise their window while that latch is active. Installing or
transferring an Exclusive latch, changing interactivity, or otherwise moving
keyboard focus dismisses any active XDG popup keyboard grab before the arbiter
sets its chosen focus. While an Exclusive latch is held, an unrelated popup
grab request is denied with `popup_done`, because xdg-shell requires the
topmost grabbing popup to own keyboard focus; a popup belonging to the latched
layer may grab normally. When a focused layer stops being eligible, focus
moves to the next Exclusive layer, otherwise to the highest visible normal
toplevel, or to no surface when neither exists. Keyboard focus inside an
Exclusive layer's own active popup grab satisfies the layer's latch: ordinary
panel redraws neither pull focus back to the layer root nor dismiss its menu.

Committed `wl_surface` input regions participate in hit testing for every
surface role. An empty region makes a panel click-through, and regions are
clipped to the surface's presented buffer bounds. A committed region change,
map or unmap retargets a stationary pointer after its complete Smithay surface
transaction applies. Committed stack-band, subsurface-order and LayerMap
geometry changes use the same retargeting path. Synchronized descendants
therefore produce one atomic leave/enter transition and one hit test for the
transaction, never intermediate targets from partially applied sibling state.
If rearrangement moves the currently focused surface without changing its
identity, the compositor sends one motion with the corrected surface-local
coordinates. Output resize and KMS topology changes batch layer arrangement,
usable-area derivation and window clamping before that single reconciliation.
Up to 256 region operations are retained exactly; larger regions use their
added rectangles' bounding box so protocol-thread hit testing stays bounded.

Exclusive zones reduce the usable output rectangle. New-window cascade
origins, maximised sizes and normal restore clamping all use that rectangle and
are recalculated after layer map, unmap or destruction and after output size or
KMS topology changes. Layers are arranged before maximised windows are
reconfigured, so their wire configure uses the current usable rectangle.

Layer shell honours the requested output, including an explicit
`wl_output`. A request with no output uses the backend's default output; if no
output exists, the layer surface is closed and is never mapped.

## Session locking

An accepted nested lock enters `Locking`, immediately removes ordinary client
content from the renderer roster and installs the opaque black security scene.
The compositor sends `locked` only after the nested renderer acquires a
swapchain image containing that epoch, submits the frame, calls the winit/wgpu
present path, and waits for the submitted GPU work to complete. A Bevy schedule
turn is not presentation evidence: a minimised, occluded, skipped or failed
frame leaves the epoch pending and `locked` withheld. The blank itself satisfies
the barrier, so a slow or absent client lock surface never delays a frame that
is actually presented. KMS lock requests fail closed with `finished` until the
KMS backend can report a real displayed output and epoch; that wiring belongs
to the next implementation slice.

Each physical output accepts one lock surface for the current generation.
Its first configure is immediate and exactly matches the logical output; a
buffer may map only after acknowledging the current configure and must have
the acknowledged dimensions. Every responding commit is checked against its
effective buffer: an empty first commit is a null-buffer error, while an empty
commit after resize revalidates the retained buffer against the new size. A
surface with any earlier attach or commit history cannot become a lock surface.
Lock surfaces and their subsurface trees receive input above any Exclusive
layer. Ordinary surfaces may continue committing, but receive no renderer
publication, frame callbacks, focus or input until unlock. Lock entry also
purges ordinary deltas already queued for the renderer. Unlock or a pre-locked
abort sends complete upserts for every presentable surface, so unchanged static
windows are recreated as well as listed in the roster.

Entering `Locking` dismisses popups, ends pointer and drag-and-drop grabs,
cancels touch, reconciles pressed keys, hides the client cursor, clears
data-device focus and suppresses ordinary compositor bindings. The KMS VT
switch binding remains the sole exception. Foreign-toplevel handles close at
entry and neither update nor replay while locked; unlock reannounces mapped
toplevels with the same identifiers. Physical input still resets idle
notifications. If the owner dies before `locked`, locking aborts. If it dies
after `locked`, the compositor becomes orphan-locked, removes the dead lock
surfaces and retains the opaque blank while swallowing input. Blank-only areas
and holes in a lock surface's input region swallow pointer, keyboard and touch;
the compositor does not scan hidden window chrome there, start move/resize or
caption-button grabs, dispatch close, or reveal a chrome cursor.

The global is deliberately advertised to every connected client: any client
may request the lock. The accepted generation is nevertheless bound to the
exact `ext_session_lock_v1` resource, so a rejected object from the same client
cannot create surfaces or unlock it. Destroying the accepted resource during
`Locking` aborts safely. An orphaned lock never auto-unlocks and persists until
the compositor exits.

## Vendored changes

The vendored Smithay layer-surface handle has an additive `reset_after_unmap`
helper so the compositor can clear Smithay's private configure queue while
applying layer-shell's protocol-mandated post-unmap state reset. Smithay's
`CompositorHandler` also has an additive `transaction_applied` callback so
pointer hit testing can observe one complete synchronized-surface transaction
instead of each surface's intermediate state. Its pointer handle has one
additive no-focus-restore grab teardown used when a grabbed surface disappears
inside that transaction, preventing stale cached focus from being replayed
before the final hit test.
Smithay's foreign-toplevel list has one additive constructor accepting a
compositor-provided identifier. This lets the identifier include Cosmix's
surface identity, mapping generation and an unpredictable per-compositor
instance nonce while protocol dispatch and replay remain entirely delegated
to Smithay.
Smithay's session-lock surface has an additive serial-returning configure
method. This lets the common Cosmix configure gate record the exact immediate
and resize serials while Smithay's default initial send observes that the
pending state was already consumed and does not duplicate it. Its session-lock
handler also has additive originating-lock, lock-object destruction,
lock-surface destruction and construction-history hooks. These bind surface
creation and `Locking` lifetime to the accepted resource while letting Cosmix's
attach/commit ledger enforce `AlreadyConstructed`.

The vendored session-lock implementation also carries four marked fixes:

- invalid `unlock_and_destroy` returns after posting `InvalidUnlock`, so a
  rejected object cannot fall through to the compositor's unlock handler;
- `AlreadyConstructed` validation happens before the output is inserted into
  Smithay's duplicate registry, preventing failed constructions from leaking
  entries; and
- each output registration records its owning lock-surface and lock object, so
  abort retires only that generation and a stale destructor cannot erase a
  newer generation's registration; and
- commit validation tracks the effective retained buffer, rejecting null first
  commits and stale-sized buffers after a newly acknowledged resize.

The associated output resource is retained until lock-surface destruction,
valid unlock or generation abort so Smithay's resource duplicate registry and
Cosmix's physical-output ownership map are both released while the
compositor-owned blank remains.
