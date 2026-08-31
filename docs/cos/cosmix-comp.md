# cosmix-comp

`cosmix-comp` is the Wayland compositor used by the Cosmix desktop. It can run
nested inside an existing Wayland session with `cosmix-comp --nested`, or use
the KMS backend on a system seat.

## Bus control plane

The default `bus` feature gives the compositor an L2 Bus control plane.
The seat/KMS compositor registers as `comp`; `--nested` registers as
`comp-nested`. `--bus-service NAME` overrides either name and accepts
`^[a-z][a-z0-9-]{1,30}$`. A build without the `bus` feature rejects that flag
instead of silently ignoring it. The broker independently enforces the same
SPEC 10 service-name grammar at registration and rejects an invalid `from`
with Bus rc 10.

The control plane exposes seven verbs:

- `comp.ping` returns `{"pong":true}` without taking a compositor snapshot.
- `comp.info` returns service/build/backend provenance plus output and surface
  counts and the property event counters.
- `comp.props.get path?` returns one leaf or subtree, or the complete tree when
  `path` is omitted.
- `comp.props.list prefix?` returns leaf paths. A prefix is matched by complete
  path segments, never by string prefix.
- `comp.props.describe path` returns leaf metadata (`type`, `mutable`,
  `sensitive`, description, optional `format`/`enum`/`range`/`persistence`, and
  owner) or an object subtree with its immediate children.
- `comp.props.watch` seeds the property-change baseline and returns
  `{topic:"<service>.props.changed",event_seq,lost_count}`, where `service` is
  the name this compositor instance actually registered. The reply is truthful
  only for a caller that subscribed to that topic before calling `watch` and
  remains subscribed.
- `comp.props.set {path,value}` mutates one of the four corner properties and
  returns `{path,old,new}`.

The complete L2 read tree is:

```text
info.{service,version,backend,engine,instance}
outputs.o_<slug>.{name,default,x,y,width,height,scale,refresh_mhz,
                  usable.{x,y,width,height}}
surfaces.s<id>.{id,role,mapped,visible,x,y,width,height,band,sequence,
                tree_index,parent,output,title,app_id,focused,activated,
                maximized,minimized,decoration,
                layer.{stratum,interactivity,exclusive_zone,binding},foreign_id}
windows.s<id>.{id,foreign_id,title,app_id,x,y,width,height,focused,
               maximized,minimized,output}
stack
focus.{keyboard,exclusive_latch,pointer,pointer_grab,session_lock}
decoration.{enabled,style}
bindings.{enabled,profile,table}
input.corners.{enabled,deadzone_px,dwell_ms,velocity_max_px_s}
port.{level,event_seq,lost_count,queue_depth,reply_timeouts,publish_timeouts,
      slug_collisions,broker}
```

Surface keys are `s` plus the decimal session-local surface ID. Output keys are
`o_` plus the lower-case output name with each non-alphanumeric character
replaced by `_`; the raw output name remains in `name`. If output names collide
after slugging, the first output wins, each omitted output increments
`port.slug_collisions`, and the compositor logs each collision at debug level.
`band` includes `background`, `bottom`, `normal`, `top`, `overlay` and `lock`.
`stack` contains mapped roots from top to bottom. `windows` is a projection of
mapped XDG toplevels. `port.level` is `L2`. `port.event_seq` is the live global
sequence watermark across every topic, and `port.lost_count` is cumulative.
`port.broker` is driven by connection-state edges and is `connected` or
`retrying`. `port.reply_timeouts` and `port.publish_timeouts` count their
separate bounded lanes; both abandon a sink wait after two seconds.

The compositor publishes non-retained messages under the registered service
namespace. The seat instance therefore uses `comp.*`, the default nested
instance uses `comp-nested.*`, and `--bus-service NAME` moves the complete
namespace to `NAME.*`. Every inner command is the unprefixed suffix shown
below, so handlers do not depend on the instance name.

| Topic | Inner command | Exact body |
| --- | --- | --- |
| `<service>.props.changed` | `props.changed` | `{path,old,new,ts,cause,event_seq}` |
| `<service>.surface.mapped` | `surface.mapped` | `{id,role,foreign_id?,event_seq}` |
| `<service>.surface.unmapped` | `surface.unmapped` | `{id,role,foreign_id?,event_seq}` |
| `<service>.focus.changed` | `focus.changed` | `{keyboard,previous,exclusive_latch,event_seq}` |
| `<service>.output.changed` | `output.changed` | `{output,geometry:{x,y,width,height},usable:{x,y,width,height},event_seq}` |
| `<service>.corner.entered` | `corner.entered` | `{output,corner,dwell_ms,event_seq}` |
| `<service>.corner.left` | `corner.left` | `{output,corner,dwell_ms,event_seq}` |

For a reliable property bootstrap: subscribe to the instance topic (for
example `comp.props.changed` on the seat or `comp-nested.props.changed` when
nested), call `comp.props.watch`, verify its returned topic, then read the
required tree or subtree. The watcher is itself the subscriber: while that
subscription remains active, noded cannot send `topic.idle` for its subscriber
generation. An idle delivered in the same control batch as `watch` therefore
belongs to a previous generation; the next zero-to-one `topic.active` re-seeds
the baseline. Mix handlers match the suffix: `on props.changed`,
`on surface.mapped`, `on focus.changed`, and so on. Changes are reduced after
each complete protocol dispatch. A leaf therefore appears at most once per
cycle, with its cycle-start `old`, final `new`, lexical path order and one of
`wayland.map`, `wayland.unmap`, `wayland.focus`, `output.geometry`,
`layer.arrange`, `session.lock` or `props.set` as `cause`. Operational `port.*`
leaves are readable but are not self-published as property changes.

Keyed row creation and removal are row-granular: an appearing
`surfaces.s<id>`, `windows.s<id>` or `outputs.o_<slug>` emits one frame at the
row path with `old:null,new:<full row>`, and removal emits the inverse.
Mutations within an existing row remain leaf-granular.

The sequence is process-global, strictly increasing and shared by property,
surface, focus, output and corner records. If it reaches `u64::MAX`, that value
is offered once and observation enters a terminal exhausted state rather than
reusing a sequence. If the bounded 256-entry outbox overflows, it replaces the
oldest queued prefix with one in-order loss-interval marker. A gap is published
to every topic represented by that exact interval before its next surviving
record (at most seven gap publications per interval): Bus header
`event_seq=<last lost seq>` (the interval's last-lost sequence) and body
`{gap:true,lost_count,cause:"outbox.overflow"}`, where `lost_count` is the
same cumulative process-wide counter as `port.lost_count`, not a per-interval
tally. A rejected or timed-out
publication discards its uncertain backlog and recovers with the same body
using `cause:"publisher.loss"`. After either gap, read a fresh property tree.

Hot-corner detection is compositor-side and uses the current logical output.
It emits one `entered`, then one `left` on deadzone exit, output or geometry
change, session lock, disable, or config invalidation. `corner` is `tl`, `tr`,
`bl` or `br`; `left` repeats the dwell measured by `entered`. Fast transit is
not accepted until a velocity-qualified dwell, while continued slow outward
motion constrained by the output edge can enter early. Defaults and inclusive
ranges are:

| Property | Default | Range |
| --- | ---: | ---: |
| `input.corners.enabled` | `true` | boolean |
| `input.corners.deadzone_px` | `12.0` | `1.0..=256.0` logical px |
| `input.corners.dwell_ms` | `200` | `0..=5000` ms |
| `input.corners.velocity_max_px_s` | `1500.0` | `1.0..=20000.0` logical px/s |

These are the only mutable leaves. Their descriptors say `mutable:true` and
`persistence:"none"`; numeric leaves also carry the range above. Values live
for the compositor process only. Writes are admitted only when noded supplied
exactly one case-insensitive `broker_origin` header whose value is `local`,
the caller has a canonical registered service name, and the wire contains no
`source_peer`, `permissions` or `signed_ident` claim. Otherwise the reply is
rc 10 `{"error":"not_local"}` before calloop admission. Unknown and immutable
paths return `unknown_path` and `read_only`; type/range failures return
`{error:"invalid_value",path,expected,range}`. All four path/type/range checks
run on the worker before admission and are repeated on calloop as
defence-in-depth, so invalid writes consume no ingress or responder permit. A
no-op write replies normally without a change record.

`focus.session_lock` is `none`, `locking`, `locked`, `orphaned` or `unlocking`.
While a session lock is active, the read tree applies the same
`WaylandState::session_lock_active` presentation boundary as the renderer and
foreign-toplevel publication: ordinary surfaces retain ids, roles, bands and
geometry, but report `visible=false` and null `title`/`app_id`; `windows` is
empty. During the KMS unlock window (`normal_scene_restricted()`), the read
tree stays redacted with `focus.session_lock="unlocking"` until the
compositor's own presentation predicate lifts, at the same moment the renderer
resumes. Unlock then restores the ordinary projection.

All application errors use Bus rc 10. In addition to the write errors above,
read/dispatch errors include `unknown_path`, `busy` and `unknown_verb`, plus
`{"error":"too_large","limit_bytes":N,"hint":"read a subtree"}` when a
serialised reply would exceed the effective broker-path ceiling. `N` is
8,384,512 bytes: `min(16 MiB Bus message, 8 MiB single WebSocket frame)` minus
4 KiB of documented header/framing headroom. Immediately before sending, comp
also measures the actual canonical response headers, correlation id and
framing with the body and refuses any reply whose complete wire size would
exceed that ceiling. Replies are never truncated. An absent broker never
delays compositor startup: the port thread reports `retrying` and reconnects
independently. A registration rejection (collision, invalid SPEC 10 name or
admission) is logged once, ends the port worker without renaming, and leaves
the compositor running.

The broker client lives on the named `cosmix-comp-port` OS thread with its own
current-thread Tokio runtime. At most 16 accepted controls/reads cross a bounded calloop
channel. The calloop callback only stages requests; after the current protocol
transaction and popup cleanup, one owned snapshot is built and shared by all
requests in that dispatch. Snapshot admission is released before reply I/O.
Replies and publications use separate lanes and two-second deadlines, so a
stalled topic sink does not stop incoming commands. Full-tree JSON is
serialised once per snapshot on the blocking pool, with only one full-tree
serialisation active process-wide; requests share the resulting string. Subtree
reads serialise only the selected value.

The semantic observation reducer carries typed rows and scalar values across
the bounded outbox; only the worker constructs topic JSON. Successful offers
wake the publisher with an event notification, which drains the outbox to
empty. There is no publisher polling timer or idle tick source. `topic.idle`
drops the property baseline and a later `topic.active` seeds one at the next
stable service point; both lifecycle directions coalesce latest-wins if the
ingress is temporarily full.

The 16,384-surface cap bounds tree cardinality, not reply bytes. A full tree can
still serialise far beyond the wire allowance, so comp measures the cached
full-tree bytes once and returns `too_large`; callers can read a leaf or subtree
from the same snapshot. Single-flight serialisation prevents same-snapshot
multiplication.

Absent by design after P-1:

- `comp.surface.*` control verbs, because focus/raise/close operations arrive in P-2 and
  move/resize in P-3;
- render timings, because they are metrics rather than properties; and
- screenshot, because it waits for the capture service seam after Arc 4.

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
| `ext_session_lock_v1` | 1 | Nested and live KMS modes support immediate output-sized lock-surface configures, secure blank-first presentation acknowledgement, lock-only input, VT pause/resume preservation and the locked/orphaned lifecycle. |
| `zwlr_screencopy_manager_v1` | 3 | Compatibility output capture into exact-layout `wl_shm` buffers, including clipped regions, damage requests and presentation-timestamped completion; S-1a fails cursor-inclusive requests. |

## Screen capture

Arc 4 S-1a provides `wlr-screencopy-unstable-v1` output capture for existing
clients such as `grim`. A frame advertises one opaque `XRGB8888` shared-memory
layout with an exact `width * 4` stride. Whole-output and logically clipped
region requests are converted into displayed physical pixels; invalid or stale
outputs fail rather than falling back to another output. Plain copies are
force-presented, including on an otherwise idle output. Damage copies complete
on the next composited frame and report a full capture rectangle in S-1a.

Pixel readback and the matching output presentation form a two-part completion
latch: `ready` is sent only after both arrive and carries the real nested
presentation timestamp. The compositor bounds live and in-flight jobs, reserves
a byte budget before allocation, performs conversion away from the render and
protocol threads, then copies shared memory in bounded chunks across protocol
loop iterations. Each client may bind at most eight live screencopy managers,
with 64 live managers globally; exceeding either implementation limit is a fatal
protocol error on the new manager object. Cancellation is checked at the
protocol outbox, ECS queue, Screenshot observer and conversion worker. The byte
reservation remains charged until the last renderer-side request or
packed-result holder drops. Once Bevy has extracted a Screenshot, its lease is
held separately from the deadline-swept entity until Bevy reports completion:
stalled GPU maps are charged until they resolve, and admission fails closed once
the eight-completion limit is full. Admission also requests a redraw, so a plain
copy is presented even when the output has no animation or other damage. Every
admitted copy has a five-second absolute request deadline: this is a deadline on
that one client operation, not a periodic compositor timer.

The existing SIGUSR1/evidence PNG path shares the captured RGBA snapshot,
deadline, cancellation sweep and conversion worker with wire capture. The PNG
and wire consumers then create their own packed BGRA buffers for their distinct
outputs. PNG capture retains its filename, atomic-rename and cadence contract;
an absent Screenshot or an encode/write task which starts after the deadline
releases the one-batch-in-flight latch. A genuinely blocked filesystem write
cannot be cancelled safely and remains outside this deadline guarantee.

S-1a still uses Bevy's Screenshot primitive. A GPU map error therefore panics
inside Bevy's internal task; the S-1b renderer-owned readback replaces that path
and converts map errors into the frame's `failed` event.

S-1a accepts only `overlay_cursor=0`. A non-zero cursor request receives
`failed` before any pixels are queued because the compositor cannot yet reproduce
the retained client/chrome cursor image, hotspot and hidden state exactly. The
retained cursor asset and ordered pre/post-overlay split are S-1b work; S-1a
stops rather than drawing a fabricated arrow. `grim` sends zero by default.

Capture is deliberately default-open, including while the session is locked.
Lock and unlock change the capture epoch: stale work fails, while newly admitted
work captures the currently displayed lock surface or compositor-owned black
fallback. This is an agentic desktop policy rather than a portal permission
prompt.

S-1a proves the complete nested path and keeps the v3 global advertised on KMS.
Every S-1a KMS snapshot reports `readback_supported=false`, so every otherwise
valid KMS copy receives `failed`; S-1b adds physical KMS readback and page-flip
timestamps, persistent real-damage tracking and the exact cursor-overlay split.
The wlr protocol is a compatibility surface; the planned
`ext-image-copy-capture-v1` implementation will become another consumer of the
same capture service. The automated nested acceptance gate uses `grim`; the
`cosmix-screencopy-probe` binary is a deadline-bounded manual diagnostic for the
advertised layout, non-zero shm offset, guard bytes and non-black pixels.

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

An accepted lock enters `Locking`, immediately removes ordinary client
content from the renderer roster and installs the opaque black security scene.
The compositor sends `locked` only after the nested renderer acquires a
swapchain image containing that epoch, submits the frame, calls the winit/wgpu
present path, and waits for the submitted GPU work to complete. A Bevy schedule
turn is not presentation evidence: a minimised, occluded, skipped or failed
frame leaves the epoch pending and `locked` withheld. The blank itself satisfies
the barrier, so a slow or absent client lock surface never delays a frame that
is actually presented.

Live KMS uses the corresponding atomic-display authority boundary. The renderer
captures the security epoch when it acquires the output image, carries the exact
output key and render generation through the submission, and acknowledges it
only when atomic presentation returns `Displayed`. `OutputReady`, an enqueued
frame, a frame-clock pulse, cancellation and a failed commit are not security
presentation evidence. Every output in the epoch must report its exact current
generation before `locked` is sent.

Lock ownership lives on the Wayland protocol thread and is independent of DRM,
seat and VT authority. Losing authority never unlocks: `Locking`, `Locked` and
`OrphanedLocked` survive the pause. Resume creates a fresh epoch, installs the
opaque lock scene before the output becomes usable, and withholds client frame
callbacks and physical input until that epoch is displayed. Before selecting
its resume policy, the coordinator asks the protocol thread whether `Locking`,
`Locked` or `OrphanedLocked` is active. Any active lock disables seamless
resume on every output: the retained pre-pause framebuffer is discarded rather
than page-flipped, and the first submitted frame after authority returns is a
freshly rendered opaque lock surface or black fallback. A lock output with no
current lock surface remains black. Unlock received while paused changes the
protocol state exactly once but keeps the black scene and input gate in place;
after authority returns, the compositor displays an ordinary-scene epoch before
restoring focus, input and normal client delivery.
Keys, pointer buttons, scroll sequences and touches held by the lock scene are
ended before its surfaces are retired. Input arriving behind the display gate
is quarantined until its matching physical release, so no held gesture or
release crosses into the restored ordinary focus.

The live KMS `Ctrl-Alt-F1` through `Ctrl-Alt-F12` bindings are evaluated before
the presentation gate. They are compositor-only and never reach a Wayland
client; every other key, pointer and touch event remains gated until secure
presentation completes. Gated key presses advance Smithay's private XKB state
so the VT chord can be recognised. If the presentation barrier opens before a
matching release, quarantine still feeds that release through the same
intercepted XKB path while suppressing client delivery; pressed keys and
modifiers therefore cannot leak across the barrier. Physical key releases
matching a synthetic pause release are quarantined by device and keycode, and
pause clears both physical and suppressed touch-slot state.

An output removed or replaced while locked does not change lock ownership. Its
exact Smithay `(output, lock-surface, lock)` registration is retired, and a new
output starts with the compositor-owned black scene until the lock client maps a
new surface through the normal `wl_output` lifecycle. The KMS transition logs
stable harness markers: `session-lock-kms-resume-blank-first`,
`session-lock-kms-normal-exposure-held`,
`session-lock-kms-{initial,resume,unlock}-epoch-displayed`, and
`session-lock-kms-normal-exposure-restored`. The actual display path also emits
exactly one marker for each output's first successful flip after resume:
`session-lock-kms-resume-first-flip scene=<lock|blank|retained|client>
output=<key> epoch=<presentation-epoch|none> generation=<generation>`. A locked
resume is valid only with `lock` or `blank` and the epoch announced by that
resume; `retained`, `client`, an absent epoch or a mismatched epoch are
fail-closed evidence of exposure.

The private `desk_vt_run.mix --arm session-lock-vt` arm brackets a real VT run
with the existing recovery timer and snapshots. It launches the release
`cosmix-lock-probe`, which creates a solid SHM lock surface for each output and
prints `COSMIX_LOCK_PROBE checkpoint=...` records, then proves initial lock,
VT away/back blank-first resume, delayed normal exposure and displayed-epoch
unlock. It requires the safe per-output first-flip marker and requests a KMS
texture-view PNG through the compositor's SIGUSR1 frame-capture path; the image
must contain only the solid lock colour or black fallback. The arm scopes
resume markers after a journal cursor captured while away and matches
the resume-start epoch as well as the output. It accepts the probe's terminal
unlock markers after either an active observation or an exact successful exit;
mid-run lock markers still require the probe to be alive. The arm is
intentionally a manual hardware gate; ordinary development only builds the
helper and lints the script.

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
The session-lock registry also exposes a narrow exact-surface retirement helper
for KMS output replacement; it removes only the originating protocol object and
does not alter the accepted lock generation.

The vendored session-lock implementation also carries five marked fixes:

- invalid `unlock_and_destroy` returns after posting `InvalidUnlock`, so a
  rejected object cannot fall through to the compositor's unlock handler;
- a valid `unlock_and_destroy` consumes its locked state before calling the
  compositor, so the unlock transition can occur only once even while visible
  restoration is deferred across a VT pause;
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
