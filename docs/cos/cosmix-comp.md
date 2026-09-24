# cosmix-comp

The `cosmix-input-probe` hardware gate client redraws on frame callbacks and
uses opaque XRGB buffers by default. `--translucent` instead uses premultiplied
half-alpha ARGB buffers without an opaque region, including after resize or
maximise. Add `--ssd` to request server decorations in either mode, allowing the occlusion gate to
compare opaque and translucent maximised windows through the same client path.

`cosmix-comp` is the Wayland compositor used by the Cosmix desktop. It can run
nested inside an existing Wayland session with `cosmix-comp --nested`, or use
the KMS backend on a system seat.

## Command-line options

`--help` prints the command usage. `--version` prints four lines of embedded
build provenance (the package line, full commit, enabled behaviour features,
and the cargo profile) and exits successfully before opening any device,
session, socket or Bus connection. It is recognised anywhere in the argument
vector, including after the `kms-live` subcommand, and wins when combined with
other options.

## KMS target-device changes (0.62.1)

A udev event for the authorised DRM device is no longer treated as proof that the
display was hotplugged. Switching to a text console makes the kernel modeset the
card for fbcon, which emits a udev `change`, so an ordinary VT round trip used to
manufacture a hotplug and cost the session every client it was hosting.

Only one case is now decided on the session thread: a `Removed` event while
`kms-live` is Active revokes authority, because the device being driven is gone
and that needs no query to establish. An `Added` event while Active is not
authority loss. Every `Changed` event, and every event of any kind while the
session is not Active — which includes the whole resume transition — is recorded
for diagnosis and left to verification.

That thread does no I/O at all, deliberately: it owns the libseat pause
acknowledgement and answers session commands under a three-second deadline, and a
synchronous DRM probe cannot be interrupted once it is inside the driver, so a
wedged probe there would stall the pause acknowledgement and lose the session
harder than the bug being fixed.

The identity invariant is unchanged and is enforced where the code already budgets
for a synchronous driver call. The resume path re-verifies the stable device path,
the device incarnation, the VT, DRM master state and connector presence before the
session can return to the glass, and refuses terminally when the connector is
absent — so a device that really went away cannot reach the glass by being
deferred. While the session is Active no resume is pending, and there a connector
that stops accepting frames is caught by presentation evidence rather than by a
connector query.

## Occlusion and frame callbacks (0.61.0)

Frame callbacks remain queued while a canonical surface family is provably
covered by opaque content on every intersecting output. Popups and subsurfaces
keep their family running whenever any member is exposed. This applies to
layer-shell backgrounds, ordinary toplevels and XWayland surface trees.
Minimised/off-workspace and session-lock gates retain their existing precedence;
callbacks before the first buffer are never withheld by occlusion.

Coverage uses applied opaque-region transactions and renderer-confirmed installed
content. A pending DMA-BUF replacement cannot lend its opaque region to an older
texture. Candidate bounds round outwards and occluders round inwards in output
physical coordinates, including fractional scales. Each output's actual camera
viewport and projection supply the origin and X/Y pixel ratios; missing or
generation-mismatched camera evidence permits callbacks. Unknown state, excessive
region fragmentation and stale scene revisions permit callbacks. Scene changes
invalidate coverage; the next existing frame opportunity drains retained
callbacks once with the current monotonic millisecond timestamp. Content-only
commits preserve coverage decisions: an occluder's applied and sampled content
sequences must each be greater than or equal to the sequence where its current
coverage inputs last changed, rather than equal to its latest content sequence.
That floor advances at applied opacity-region, format-opacity, buffer-size,
generation, transform or viewport changes. A pending import may retain older
content from the same coverage interval; content predating the floor cannot
occlude. No occlusion polling timer or victim commit is required.

Maximised SSD frames have square opaque chrome bands without outer/inner edge
antialiasing. Floating rounded chrome is unchanged. Fullscreen removes SSD;
neither maximise nor fullscreen makes translucent client pixels opaque. Reserved
work areas or exposed strips of wallpaper correctly prevent withholding.

Read `comp.props.get` paths `surfaces.s<id>.occluded`, `occlusion_reason`
(`unknown`, `exposed`, `opaque-coverage`) and `occlusion_revision`. These leaves
are mirrored under `windows.s<id>` where a window row exists. Layer and X11
surfaces remain observable under `surfaces`. `occlusion.counters` contains
compositor-wide `withheld_opportunities`, `resumes`, `recomputes` and
`conservative_fallbacks`; these read-only counters do not emit property changes.
Withheld opportunities count eligible root-tree pulse opportunities, not dropped
callbacks. `resumes` counts surface trees resumed, not individual callbacks:
one tree delivering several retained callbacks adds one. Resumes match delivered
callback identities against the retained set,
so destroying a child cannot turn an unrelated callback into a resume. They do not count transitions
to unknown visibility. While occluded, each surface retains at most 64 committed
callbacks: excess older callbacks receive `done` immediately (fail-open), without
discarding protocol objects. This cap does not override session-lock or workspace
gates. `comp_occlusion_transition` traces surface id, occluded flag and
revision; existing `comp_callback_done_queued` traces confirm callback delivery.

For single-output presentation reports, covered surfaces receive `discarded`
feedback only through the content sequence evidenced by that frame. Frame
callbacks remain queued independently. **0.61.0 limit:** multi-output callback
coverage is supported, but multi-output presentation retains the existing shared
content-report semantics. Unproven (generation-zero) outputs still count towards
this conservative multi-output gate: missing evidence cannot establish that the
shared report covers a single output. The existing origin-zero canvas predicate in
`frame_content` remains a presentation-only limit; coverage uses each output's
origin independently; rotated camera projections fail open. Rounded client clips
and partial opaque regions with a crop, transform or non-1:1 buffer-to-destination
ratio (including destination-only viewports and buffer-scale changes) are
conservatively excluded as occluders. Unioned regions covering the entire surface
retain clamped outer edges without artificial internal seams.

Coverage bounds come from layout, not the entity's actual transform. Future
renderer-side offsets or animations must feed occlusion or disable its proof.
Coverage also relies on draw order matching `SurfaceStackKey`, and chrome bands
remaining tied to the decoration root's visibility; extraction checks that root
before crediting bands. These renderer conventions are not enforced by a shared
geometry/order type. Clients must honour their opaque-region declarations and
pace rendering on callbacks for CPU savings. Native Quoin's continuous-render
setting is unchanged.

## KMS input dispatch fairness

Live KMS input routing yields after 16 libinput events or 2 ms, whichever
comes first. This keeps a batch well below a 60 Hz frame interval while
allowing cheap events to drain promptly. The time bound is cooperative:
one callback and libinput's own dispatch cannot be preempted.

A small opt-in vendored Smithay patch retains remaining events in libinput
in their original order. On budget exhaustion, its calloop `before_sleep`
hook requests synthetic readiness and a nonblocking poll on the next turn.
Real and synthetic readiness share one batch per turn. Each turn returns
through normal frame-command service, client commit dispatch and socket
flush; no input is dropped and no new fd edge is needed to finish a burst.

Live acceptance: with the Boing wallpaper animating, sustain heavy pointer
motion and bucket the frame trace at 200 ms. `comp_present`,
`comp_pulse_sent_busy` and `comp_callback_done_queued` must continue advancing
through the burst, with client buffers and frame callbacks serviced each
vblank. Offline checks cannot establish real-KMS presentation cadence.

## KMS hardware cursor

The `kms-live` build (with the default Bus, frame-capture and XWayland
features) uses an optional DRM cursor plane. `native-quoin` is not required.
Live KMS admission tries a compatible current CRTC first; other connectors'
`CRTC_ID` properties cannot evict it from that route. For an unrouted connector,
resume prefers its previous CRTC among free routes, then tries the remaining
free CRTCs in ascending object-ID order. Only after all free routes fail may
it reclaim a route claimed solely by disconnected connectors, again preferring
the previous CRTC and then ascending IDs. Such a selection logs the stale claim
and its former owners. Routes claimed by connected or unknown-status connectors
remain blocked, except for the current route. Rejected admission reports retain typed claim
diagnostics alongside other route and format/modifier failures.
The probe's `rejection_matrix` rows carry `kind` (`route` or `format`) and
`crtc_id`; route rows carry a `reason`, and format rows retain the capability
fields. Probe counters include format attempts on rejected routes.

Resume retries temporary route contention and transient DRM enumeration errors
through the existing three-attempt, 30-second recovery budget. Each failed
attempt returns to paused before retrying. Missing or disconnected connectors,
missing prior modes, unsupported atomic properties and incompatible formats
remain terminal. These retries cover target admission, before presentation;
they do not change atomic commit failure handling.
Connector probe failures use `kms-live-connector-scan-failed`; the
`kms-live-atomic-admission-failed` prefix is reserved for admission failures.

The live atomic presenter enumerates ARGB8888 cursor planes compatible with
the selected CRTC, checks their atomic properties, and queries the driver's
cursor width/height caps (64×64 for older drivers without these caps).

Cursor images use two transparent, pitch-aware ARGB dumb buffers. Image
changes pass an atomic test-only check before a synchronous cursor-plane
commit; hotspots, output scale, SHM source rectangles and transforms are
applied to the uploaded image and placement. For an admitted image, motion
retains only the latest desired position. The next primary atomic submission
appends the cursor plane properties, including its current framebuffer and
`CRTC_X`/`CRTC_Y`, to the same request. Both planes share one commit and one
primary pageflip completion. Submitted cursor coordinates advance only when
the ioctl succeeds; every busy retry picks up the latest desired position.

When the presentation pump proves the primary scene idle, it flushes pending
motion with a cursor-only `NONBLOCK` commit and no pageflip event. EBUSY keeps
the position pending for the next refresh-paced pump update, even if input
has stopped. Motion neither rotates cursor buffers nor advances the primary
scene revision. Image replacement, hide and teardown remain synchronised
with the cursor submission lock and the existing blocking image commits.
If a pending cursor commit makes the next primary commit busy, the presenter
retries at bounded 2 ms intervals within its original deadline; it does not
wait for a cursor pageflip event that was never requested.

Once hardware projection succeeds, the software cursor entity is hidden.
The transition renders once to remove its old pixels. Missing planes,
allocation/admission/commit errors restore the software path and log the
reason. Oversized or GPU-only DMA-BUF cursor images use software projection;
a later supported image can use the plane again. Cursor resources are
retired with the output and re-enumerated after resume; revoked generations
cannot submit cursor updates. Capture retains its separate cursor snapshot.

Offline tests cover cursor requests, pixel packing and scene fallback.
Driver acceptance and motion smoothness still require a live session check:
move the pointer over an animated wallpaper, change cursor shapes, drag a
window, toggle panels, and exercise display sleep/wake. Check the hardware
cursor log and frame trace; cursor motion alone should not create primary
scene revisions or `comp_render` work after the scene settles.

## Optional F9 Bus action

`--f9-bus <service> <verb>` arms an unmodified F9 press to send a native ABP
request with an empty argument object to a local Bus service. For example,
`--f9-bus bg-showcase boing.kick` connects the compositor key to the Boing
scene's physical impulse. Both nested and live KMS input use this binding;
the background surface keeps keyboard interactivity disabled. In a nested
session the compositor's host window must have focus.

The binding is absent unless explicitly configured, honours
`--no-keybindings`, and does not run while the session is locked. A held key
fires once until released. Delivery runs on a bounded worker, so waiting for
the service cannot stall keyboard input. Failed requests are logged and never
automatically retried. `--list-bindings --f9-bus <service> <verb>` includes the
armed binding. The flag requires a build with Bus support.

For small nested scene previews, `COSMIX_COMP_SERIAL_SCHEDULES=1` selects
single-threaded ECS schedule execution to reduce dispatch overhead. This is an
opt-in performance experiment; normal compositor scheduling is unchanged.

## Wayland fullscreen

Comp 0.51.1 honours `xdg_toplevel` fullscreen and unfullscreen requests.
Fullscreen uses the primary logical output's complete rectangle, including
space otherwise reserved for panels. The optional client output hint is not
selected yet. State and geometry change together when the client commits its
acknowledged configure; sending or acknowledging a configure alone does not
move the visible window.

Compositor decorations disappear in fullscreen. While that window has keyboard
focus, it is raised into the top band and native Quoin panels and hotspots are
hidden. Switching focus restores its previous stacking band and Quoin, allowing
other applications to remain usable. Leaving fullscreen restores the previous
normal geometry, or the maximised layout if it was maximised first. Restored
normal geometry is clamped to the current usable area after output changes.

Bus properties `windows.<id>.fullscreen` and `surfaces.<id>.fullscreen` expose
the committed state and emit normal property observation deltas. Application
commands such as `media.fullscreen` initiate the request through Wayland.

## Window switching and X11 placement

Comp 0.51.0 includes an opt-in `native-quoin` feature. Together with
`COSMIX_COMP_HUD_PROBE=1` and `COSMIX_COMP_NATIVE_QUOIN=1`, this embeds Quoin's
real panels alongside native Boing in the compositor renderer. Application
windows remain Wayland clients; their existing server-side decorations,
caption actions and interactive move/resize handling remain in comp.
Pinned native panels reserve usable space for maximised windows. See
[Quoin's compositor host](quoin.md#experimental-compositor-host) for scope and
remaining acceptance work.

Alt+Tab cycles forward and Alt+Shift+Tab cycles backward through mapped,
non-minimised managed windows in stable creation order. This is not an MRU
switcher or a visual switcher overlay. Both nested and KMS profiles support
these bindings when interception is enabled. Session locking and exclusive
keyboard layers retain priority.

Workspace chords (comp 0.59.0) live in both profiles beside
`restore-recent-minimized` and share one implementation with the
`comp.workspace.switch` / `comp.window.send_to_workspace` verbs:
`workspace-jump-<n>` (Super+`<n>`, n in 1..9) switches to workspace n;
`workspace-move-<n>` (Super+Shift+`<n>`) moves the focused window to n and
follows it; `workspace-next` / `workspace-prev` (Super+`]` / Super+`[`) step
by one and wrap at the ends. `bindings.table` lists them with xkb keysym
names: `Super+1`, `Super+Shift+1`, `Super+bracketright`. The matcher reads
the level-0 symbol, so Super+Shift+1 is still the digit, not `exclam`; on a
layout whose level 0 is not the digit the chord does not fire. A jump above
`workspaces.count` is a silent no-op (a key press has nobody to reply to; the
verb answers `invalid_value`). Under a session lock the chords reach the lock
surface, never the compositor. Under an exclusive layer the move chord is
withheld whole (no move, no switch) by the same gate as `send_to_workspace
{follow:true}` — a chord that will activate the window must not re-arrange
the desktop under a layer that owns the screen — while the jump and step
chords, like `comp.workspace.switch`, still switch.

X11 `_NET_ACTIVE_WINDOW` requests use the same managed-window admission and
focus path. Local automation is accepted without timestamp-based focus-stealing
prevention; source identifiers are not authentication. Unmapped, minimised and
override-redirect windows cannot be activated through this path.
The root `_NET_ACTIVE_WINDOW` property follows the compositor seat's managed
X11 focus, including mouse and keyboard switching. Native, lock or absent
focus clears it to zero; delayed X focus events cannot replace it with a root
or ancestor window ID. This keeps `xdotool windowactivate --sync` and
`getactivewindow` consistent with compositor focus.

Workspaces reach X11 clients as EWMH virtual desktops (comp 0.59.0). The
root `_NET_NUMBER_OF_DESKTOPS` and `_NET_CURRENT_DESKTOP` are rewritten on
every workspace switch and count change (and once when the XWM starts, over
the `1`/`0` the XWM writes at startup so `xprop -root` never sees them
absent); EWMH desktops are 0-based, so `workspaces.current` 3 reads as
`_NET_CURRENT_DESKTOP = 2`. Every managed X11 window carries
`_NET_WM_DESKTOP`, written at the moment it maps (the same edge that stamps
its workspace — a window that has only sent its MapRequest has no
workspace and no property yet), rewritten by every move and by a count
shrink that strands it. A client's `_NET_WM_DESKTOP` message is honoured as
a move — the window goes to that desktop WITHOUT switching, exactly like a
`windows.s<id>.workspace` write, and gets `_NET_WM_STATE_HIDDEN` if that
takes it off screen; `_NET_ACTIVE_WINDOW` remains the request that brings a
window on screen. Ignored, with a debug log: `0xFFFFFFFF` (all desktops —
comp has no sticky windows in 0.59.0), a desktop at or above the count, a
request while a session lock is active, and one for an override-redirect
window or a stale identity. A pager's `_NET_CURRENT_DESKTOP` root message
(`wmctrl -s N`, `xdotool set_desktop N`) is honoured as a switch of the
default output, exactly like `comp.workspace.switch {index: N + 1}`
without wrap; a desktop at or above the count and a request under a
session lock are ignored with a debug log. A pager's
`_NET_NUMBER_OF_DESKTOPS` root message (`wmctrl -n N`) is NOT honoured —
the count is `workspaces.count`, compositor-owned — and is dropped with a
debug log; the atom stays in `_NET_SUPPORTED` for the property, which is.
A KMS topology change that replaces the default output carries the
retiring output's current workspace to the replacing one (`workspaces.
current` is keyed by the default output, and a replugged monitor must not
change the desktop) and republishes the root pair; if the effective
workspace changed anyway (the only output went away, or came back under a
key that still held an older value) the visibility and suspended state of
every window are re-derived in one settle, so no reader — frame callbacks,
presentation, `windows.*` — can see a workspace the scene does not.
`xprop -root _NET_CURRENT_DESKTOP` /
`_NET_NUMBER_OF_DESKTOPS` and `xprop -id <xid> _NET_WM_DESKTOP` via
`xwayland.display` are the live checks (the nested workspace gate's rule
10, which also drives both client messages through `xdotool`); the offline
suite pins the atoms, the callbacks and the values the compositor asks the
XWM to write, never the X property itself. Note that a property write can
only fail with a dead X connection (the X protocol reports per-request
errors asynchronously): a failed write is a dying Xwayland generation, not
a stale property on a live window.

Initial X11 placement, including size-only configure requests before mapping,
respects reserved panel space. Reserved-area changes reflow managed X11 windows;
maximised windows use the usable area and fullscreen windows use the full
output. Restore geometry is kept until restoration, then clamped to the current
usable area. Override-redirect menus retain client-owned coordinates and do not
gain priority over top-layer panels or lock surfaces.

Intentional XWayland shutdown is marked before connection teardown. Expected
connection closure and exit code 1 are distinguished from runtime crashes;
protocol errors, signal deaths and timeout escalation remain errors. The child
reaper allows a two-second exit grace before kill/reap. It remains asynchronous:
compositor exit does not wait for a positive child-reaped acknowledgement.

## Bus control plane

The default `bus` feature gives the compositor an L2 Bus control plane.
The seat/KMS compositor registers as `comp`; `--nested` registers as
`comp-nested`. `--bus-service NAME` overrides either name and accepts
`^[a-z][a-z0-9-]{1,30}$`. A build without the `bus` feature rejects that flag
instead of silently ignoring it. The broker independently enforces the same
SPEC 10 service-name grammar at registration and rejects an invalid `from`
with Bus rc 10.

The control plane exposes these verbs:

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
- `comp.props.set {path,value,generation?}` mutates the six corner
  properties, `windows.s<id>.band`, `windows.s<id>.minimized`,
  `windows.s<id>.workspace`, `workspaces.count`, `workspaces.current`,
  `workspaces.o_<slug>.current`, `input.host.passthrough`, or
  `xwayland.enabled` and returns `{path,old,new}`; for the file-persisted
  `xwayland.enabled` the reply also carries `persisted` — `false` means the
  in-memory change and the changed event stand but the write to disk failed
  and the value will not survive restart. The optional `generation` fences a
  `windows.s<id>.*` write (see Window identity below); it is refused on any
  other path.
- `comp.pointer.watch` renews a three-second pointer observation lease
  and returns `{version:1,topic:"<service>.pointer.changed",lease_ms:3000}`.
  Subscribe before calling; renew about once per second while observation is
  wanted. The acknowledgement contains no pointer coordinates.
- `comp.region.select {output?,timeout_ms?}` selects a rectangle using native
  compositor furniture. See Region selection below.
- `comp.window.minimize {id,generation}` minimises one window, like its
  title-bar button. Both fields are required.
- `comp.window.restore {id?,generation?}` with no arguments restores the most
  recently minimised window, exactly like the `Super+Shift+M` binding; with
  `{id,generation}` (both required together) it restores that window. If the
  window is minimised, either form un-minimises it, switches to its
  workspace if that is not the current one, raises and focuses it (where
  the switch is not allowed — an exclusive layer, the VT switched away — it
  un-minimises without switching or focusing); if it is not minimised,
  nothing happens and the reply says `changed:false`.
- `comp.window.stats {id,generation | source}` and
  `comp.window.stats.reset {id,generation | source | nothing}` read and zero
  presentation statistics (see Presentation statistics below).
- `comp.window.focus`, `comp.window.raise`, `comp.window.close`,
  `comp.window.place` and `comp.window.wait` act on or wait for one window
  (see Window control below). `comp.windows.list` lists window rows.
- `comp.workspace.switch {index,output?,wrap?}` changes the output's current
  workspace and `comp.window.send_to_workspace {id,generation,index,follow?}`
  moves one window to a workspace (see Window control below).
- `comp.input.pointer.move`, `comp.input.pointer.button`,
  `comp.input.pointer.scroll`, `comp.input.key`, `comp.input.release_all` and
  `comp.input.sequence` inject input through the real seat (see Input
  injection below).

### Region selection

`comp.region.select` holds one pending reply while the seat selects a region.
Left-button drag selects; reverse drags are normalised and rounded outwards to
integer logical units. A click or zero-area drag keeps selection armed. Esc or
right-button press cancels. Unknown fields are rejected. `timeout_ms` defaults
to 30000 and accepts 1–55000: three further seconds bound clean-frame removal,
with a four-second responder margin, strictly below the 60-second long-verb cap.
Set the caller's Bus timeout above that total budget.

An explicit `output` restricts selection to that named output. Otherwise the
first left press chooses its output. Dragging across its edge clips the rectangle;
this does not stitch multiple outputs. Success (rc 0) is:

```json
{"version":1,"status":"selected","output":"Output-1","output_generation":42,"coordinate_space":"output-local-logical","region":{"x":100,"y":80,"width":640,"height":360}}
```

Coordinates are relative to the displayed output's top-left, before conversion
to physical pixels. Cancellation returns
`{"version":1,"status":"cancelled","reason":"escape"}` (or `right_button`);
timeout returns `{"version":1,"status":"timeout"}`. These are rc 0 outcomes.
Successful completion waits for a submitted frame without the overlay on the
selected output; an unrelated sleeping monitor does not delay that reply.
If the selected output disappears during cleanup, all surviving overlay outputs
must submit clean frames; no surviving output means success cannot be proven.
Cancellation, timeout and refusal restore focus and publish overlay removal, then
reply without waiting for presentation: they do not authorise a capture.
Failure to prove removal within the margin returns rc 10 `busy`, never success.

A second selector, an existing pointer/popup grab, touch sequence, native panel
drag, input sequence or window manipulation returns rc 10 `busy`. Output identity,
generation, geometry, scale or transform changing before a result is decided
returns `output_changed`. A decided result is preserved during cleanup.
A requested output name that does not exist returns `unknown_output`.
Session lock returns `locked`. These are lifecycle/correctness rules, not caller
permissions. Temporary seat focus does not deactivate or restack fullscreen windows.
Pointer constraints are released for selection and reconsidered on focus restoration.
VT/focus/device loss and a closed local responder clean up input ownership.
Touchscreen removal aborts even a pointer-driven selection with `busy`; KMS
session pause reports `output_changed` before generic input-loss cleanup.
Remote caller disappearance is bounded by the deadline; immediate remote request
cancellation is not currently propagated to the local responder.

The result contains geometry only. Pass `output` and `region` to
`capture.screenshot`; an agent that already knows its rectangle can call capture
directly. `output_generation` describes selection-time identity; capture's current
Wayland request does not carry that generation, so this is not an atomic
selection-to-capture topology fence.

Compositor log colours follow the stderr log sink's terminal status. Redirecting
stdout alone preserves colour; stderr pipes, files and journald receive plain text
in both KMS and nested modes.

### Minimise and restore

Minimise and restore reply `{id,generation,title,app_id,minimized,changed}`;
`changed:false` means the window was already in the requested state.
`comp.window.restore {}` with nothing to restore replies rc 10
`{"error":"not_found","minimized_count":N}`. While a session lock is active
both verbs, and writes to `windows.s<id>.minimized`, reply
`{"error":"locked"}`. An argument other than `id` or `generation` is refused
with `{"error":"invalid_args","field":"<name>","allowed":["id","generation"]}`,
so a typo such as `gen` cannot silently turn a fenced call into an unfenced
one.

No verb checks who the caller is. Any caller that noded delivers, local or from
the WireGuard mesh, can read, write and watch; being on the mesh is the whole
authorization. What the port still refuses is a malformed or mis-aimed request
(unknown path, wrong type, out-of-range value, a window that is not there).

The complete L2 read tree is:

```text
info.{service,version,backend,engine,instance}
outputs.o_<slug>.{name,default,x,y,width,height,scale,refresh_mhz,
                  usable.{x,y,width,height},
                  presentation.{clock_id,flags,flags_mask,refresh_us,frames,
                    interval_p50_us,interval_p99_us,since_us}}  (presentation: volatile)
surfaces.s<id>.{id,role,mapped,visible,x,y,width,height,band,sequence,
                tree_index,parent,output,title,app_id,focused,activated,
                maximized,fullscreen,minimized,workspace,decoration,
                layer.{stratum,interactivity,exclusive_zone,binding},foreign_id,
                generation}
windows.s<id>.{id,foreign_id,title,app_id,x,y,width,height,focused,
               maximized,fullscreen,minimized,output,band,generation,
               window_x,window_y,window_width,window_height,visible,pid,workspace,
               presentation.{presented,discarded,last_presented_us,
                 interval_p50_us,interval_p99_us,interval_max_us,
                 commit_to_present_p50_us,commit_to_present_p99_us,
                 input_to_present_p50_us,input_to_present_p99_us,
                 missed,refresh_us,since_us}}          (presentation: volatile)
sources.<id>.{output,registered_at_us,revision,registration,
              presentation.{<the window leaves>,upload_bytes_total,
                damage_px_total,upload_bytes_p50,upload_bytes_p99,
                damage_px_p50,damage_px_p99}}          (volatile)
workspaces.{count,current,o_<slug>.current,list}
stack
focus.{keyboard,exclusive_latch,pointer,pointer_grab,session_lock,
       window.{id,generation}}
decoration.{enabled,style}
bindings.{enabled,profile,table}
input.corners.{holders,enabled,deadzone_px,dwell_ms,velocity_max_px_s,affordance,discovery,
               enforced.{top,bottom,left,right},
               held.{top,bottom,left,right}}     (enforced, held: volatile)
input.host.passthrough            (nested backend only)
xwayland.{enabled,persist_path,display}
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

Window rows add seven read-only leaves. `generation` is the window's role
generation (below). `window_x`/`window_y` are the window-geometry origin and
`window_width`/`window_height` its extent, all in logical pixels. Use all four
for window screenshots that exclude client-side shadow margins. `x`/`y` remain
the buffer origin and `width`/`height` the buffer extent, including those margins.
Without explicit client geometry, all four `window_*` fields use the effective
committed surface-tree bounds, including mapped subsurfaces; shadow margins
cannot then be distinguished. If no geometry is cached, the fallback is the
root buffer with zero geometry offset and its full extent. `visible` is
effective on-screen visibility: use it to ask "is this on screen", and
`minimized` for the user's minimise state. `pid` is the process id of the
client's socket peer, or null when the compositor cannot read it. A client
reached through a proxy (waypipe, a sandbox, a PID namespace) reports the
proxy's or namespace's view, which may not be the application's own pid.
`focus.window.{id,generation}` names the managed window (xdg or X11) that
holds keyboard focus, both null when none does.

**Window identity.** A `wl_surface` keeps its `s<id>` when a client gives it a
new role, so an id alone can name a different window than the one a script
read. Every role assignment (including the role ending) takes a new,
never-reused `generation`. Unmapping and remapping the same role (a null
buffer, then a new one) keeps it; an X11 window that is associated again
counts as a new role.
Every surface row publishes it as `surfaces.s<id>.generation`, and window rows
repeat it. X11 windows have no `windows.*` row yet, so read their generation
from `surfaces.s<id>`. Treat `{id, generation}` as the window's identity:
the window verbs require both, and `comp.props.set` accepts `generation` on
any `windows.s<id>.*` path. A mismatch replies rc 10
`{"error":"stale_target","id","generation","current"}` and changes nothing.
The window verbs also reply `unknown_window`, `not_managed` or `not_mapped`
(each with `id`) when the target is not a mapped managed window. Moving or
resizing a window never changes its generation.

`windows.s<id>.minimized` accepts `true` or `false`. `true` minimises the
window; `false` restores that window (not the most recently minimised one),
takes it out of the restore order, and raises and focuses it. X11 windows are
accepted too and get the EWMH hidden state cleared. A write to a window that
does not exist or is not a mapped managed window replies `invalid_value`, like
the band leaf.

Workspaces (virtual desktops) are 1-based. `workspaces.count` (default 4,
`1..=16`) is the number of workspaces; shrinking it moves every window on a
removed workspace to the last remaining one and clamps every current.
`workspaces.current` is the default output's current workspace and
`workspaces.o_<slug>.current` the same value under the output's key (one
output today, so they mirror each other; a key that is not the default
output's — even one that exists under `outputs.*` — is refused with
`invalid_value` whose `range` says only the default output switches).
Writing either switches, exactly like `comp.workspace.switch`.
`workspaces.list` is read-only: one `{index,windows}` row per workspace,
`windows` counting the mapped managed toplevels on it — X11 windows
included, although they have no `windows.*` row, so a pager never shows a
workspace empty while an X11 window is on it. `windows.s<id>.workspace` is
the window's workspace; writing it moves the window there WITHOUT
switching, so a window moved off the current workspace reads
`visible:false, minimized:false` (use `visible` for on-screen, `minimized`
for the user's minimise state). A move never changes the window's
generation. `surfaces.s<id>.workspace` carries the same value for every
mapped managed toplevel, X11 windows included (they have no `windows.*`
row), and null for every other surface and for an unmapped one. A window
that unmaps and remaps joins the current workspace again. An
override-redirect X11 window (a menu, tooltip, dropdown, DND icon) is not a
window — no row, no workspace value, never movable — but it hides with the
workspace it mapped on, so an open menu does not outlive the switch that
hid its owner; it is back, still open, when that workspace is. Moving the
OWNER (0.59.1: Super+Shift+N, `send_to_workspace`, a
`windows.s<id>.workspace` write) carries its override-redirect children
along, resolved through `WM_TRANSIENT_FOR` — the menu is relabelled to the
same workspace as its owner, so it stays visible alongside it instead of
being left open and invisible on the workspace it mapped on; it still gets
no `_NET_WM_DESKTOP` of its own (that property is for managed windows). X11 clients see
the same model through EWMH: the root `_NET_NUMBER_OF_DESKTOPS` and
`_NET_CURRENT_DESKTOP` (0-based, so workspace 1 is desktop 0) follow every
switch and count change, every managed X11 window carries `_NET_WM_DESKTOP`
from the moment it maps, a client's own `_NET_WM_DESKTOP` message moves
its window exactly like a `windows.s<id>.workspace` write, and a pager's
`_NET_CURRENT_DESKTOP` root message switches exactly like
`comp.workspace.switch` (see Window switching and X11 placement). A refused or
no-op write publishes nothing and attributes nothing: the next unrelated
change keeps its own cause. All of these are watchable; the
changed events of a switch, move or count change carry the cause of the
write (`props.set`) or the verb. Values outside `1..=count` (0 included) and
non-integers are `invalid_value`; every write is `locked` while a session
lock is active.

Window band writes accept `bottom` or `normal`. They move the complete window
tree, including popups, behind normal windows or back into their normal band.
Assignments last for the current session. Use the canonical ID returned by
`comp.props.get`; aliases with leading zeroes are rejected. Other bands remain
reserved for layer-shell and session-lock roles. Corner activation takes
priority over client pointer constraints; reactivation waits for physical
pointer motion out of the corner.

### Window control

Every verb here names its window with `{id, generation}`, refuses a stale or
missing target as described under Window identity, and replies
`{"error":"locked"}` while a session lock is active. Each is recorded in the
frame trace as `comp_window_control` (subject the id; detail 1 minimize,
2 restore, 3 focus, 4 raise, 5 close, 6 place, 7 wait, 8 forced close,
9 workspace switch, 10 send to workspace; `comp.window.stats` and
`.stats.reset` reuse 7 and 8, a collision kept until 0.60 renumbers them).

Every mapped window is on one workspace and each output has a current one;
a window off its output's current workspace reads `visible:false,
minimized:false`, gets no frame callbacks and is never presented. Every
path that brings a window into view — `comp.window.focus`,
`comp.window.restore`, a client's xdg-activation, an X11
`_NET_ACTIVE_WINDOW` or un-minimise — switches to the window's workspace
first and never pulls the window across, in one settle with the keyboard
landing on that window and on no bystander in between (the same
preference `send_to_workspace {follow:true}` gives a followed window: the
highest window already on the arriving workspace never sees a
`wl_keyboard.enter` the activation then reverses); where the switch is not allowed
(a session lock, an exclusive layer, the VT switched away) the window is
not focused either, so the keyboard never lands on a window that is off
screen: `focus` replies with the reason, an activation request is ignored,
and a restore un-minimises without switching or focusing. A client's
xdg-activation of a MINIMISED window RESTORES it (0.59.1, KWin/GNOME
parity): exactly `comp.window.restore` for that window — un-minimise,
switch to its workspace where that is allowed, raise, focus, one settle —
and it leaves the `minimized_toplevels` MRU list truthful, the same as any
other restore. `_NET_ACTIVE_WINDOW` keeps the 0.59.0 refusal: a MINIMISED
window is not a valid X11 activation target
(`window_switch_candidate`), un-minimising is the separate request that
restores it. Two verbs drive the workspaces:

- `comp.workspace.switch {index,output?,wrap?}` makes `index` the output's
  current workspace: a 1-based number, or `"next"` / `"prev"` relative to
  the current one, which wrap at the ends unless `wrap:false`, when they are
  refused with `{"error":"at_end",output,from,count}` (`output` there is the
  `outputs` key, as in the success reply, whichever spelling the request
  used). `output` is an `outputs` key or output name and defaults to the
  default output (in 0.59.0 the only output with a switchable workspace;
  any other is `invalid_value`). A number outside `1..=count` (0 included)
  is `invalid_value` naming `index` with the range. The reply is
  `{output,from,to}`; a switch to the current workspace replies with
  `from == to` and does nothing. Minimise state is untouched: a minimised
  window on the arriving workspace stays minimised. The verb names no
  window but changes what is on screen, so a session lock refuses it
  (`locked`).
- `comp.window.send_to_workspace {id,generation,index,follow?}` moves the
  window to `index` (a number, or `"next"` / `"prev"` relative to the
  window's own workspace, always wrapping) without switching; the window
  becomes `visible:false, minimized:false` if it leaves the current
  workspace. With `follow:true` comp also switches to that workspace and
  activates the window — move and switch settle once, so no other window
  on either workspace takes the keyboard in between, whatever stacking
  band it is in — unless the switch is withheld by the same gate every
  bring-into-view path has: an exclusive layer owns the screen, the
  window is minimised, or it is not presentable while the VT is switched
  away. Then the move alone runs and nothing is activated. The reply
  gains `followed`, which is simply `workspaces.current == index` READ
  BACK after the attempt, not a claim that a switch or an activation
  happened: it is `false` when the gate held and the window's new
  workspace is not the current one, and it is `true` whenever the new
  workspace is the current one — including with no switch and no
  activation, when the window was already there and the gate held (a
  minimised window sent to the workspace it is on answers `followed:true`
  and stays minimised). With no default output there is nothing to switch
  and `current` reads 1, so a send to workspace 1 answers `followed:true`.
  The move has happened either way. The reply is `{id,generation,index}`
  (plus `followed` with `follow:true`). A move never changes the window's
  `generation`. Out-of-range indices are `invalid_value` and change
  nothing; the usual `{id,generation}` fence applies.
- `comp.window.focus {id,generation,raise?}` gives the window keyboard focus.
  With `raise` (the default) it also raises it and re-targets the pointer,
  exactly like Alt+Tab. The reply is `{id,generation,focused}`. When
  `focused` is false, `reason` says why, the first that holds in this
  order: `exclusive_layer` (an exclusive layer surface holds the keyboard),
  `minimized`, `not_presentable` (the VT is switched away), `not_visible`,
  or `refused`. The first three are what keep an off-workspace window's
  switch from running, so such a window names the gate that held it, not
  the visibility the switch would have given it; a refusal changes nothing
  and attributes nothing to the window.
- `comp.window.raise {id,generation}` raises the window within its band
  without focusing it. The reply is `{id,generation,raised}`. Raise is
  stacking only: it never switches workspace and never un-minimises. An
  off-workspace or minimised window is restacked in place and stays where it
  is; `raised` reports the stacking change alone. `comp.window.focus` and
  `comp.window.restore` are the verbs that bring a window into view.
- `comp.window.close {id,generation}` asks the client to close (xdg `close`,
  or X11 `WM_DELETE_WINDOW`) and replies `{closed:"polite"}` at once.
  - With `force:true` (and optional `timeout_ms`, default 3000, at most
    60000) the reply waits. The deadline runs from when the port admitted
    the request.
  - If the window's role ends first (the client destroyed it, or the id now
    names another role), the reply is `{closed:"gone",waited_ms}`.
  - A window that is only unmapped is still alive: an app that hides to a
    tray on close is not gone. If the same `{id,generation}` is still alive
    at the deadline, mapped or not, comp disconnects its client and replies
    `{closed:"killed",window:"mapped"|"unmapped",scope:"client",pid,windows,waited_ms}`.
  - The kill ends the whole client connection, so every window in
    `windows` goes with it. No kill happens if the caller has stopped
    waiting, or while a session lock is active (the reply is then `locked`).
  - An X11 window is never killed this way: its Wayland client is XWayland,
    and the window manager has no per-client kill. The polite close is sent
    and the verb replies at once with
    `{"error":"still_open","reason":"x11_kill_unsupported",polite_close_sent:true}`.
- `comp.window.place {id,generation,output?,x?,y?,width?,height?}` moves
  and/or resizes the window, with at least one field given.
  - `x`/`y` are output-local logical coordinates of the window-geometry
    origin (`window_x`/`window_y`). `output` is an `outputs` key or output
    name, and defaults to the window's own output. An absent coordinate
    keeps the window's offset within its output.
  - `width`/`height` request a window-geometry size, clamped to the client's
    minimum and maximum. An absent one keeps the window's current
    window-geometry size (what the client committed, not what comp last
    asked for), so a width-only or height-only place re-sends the current
    size for the other axis in the configure. For an xdg window this is a configure, so the size
    changes when the client answers: wait with `comp.window.wait
    {until:"size"}`.
  - The reply is
    `{id,generation,output,window_x,window_y,requested:{width,height}|null,configure_pending}`.
  - A maximised or fullscreen window is refused with
    `{"error":"invalid_state",maximized,fullscreen}`. An unknown output is
    `unknown_output`. A place that would leave the window wholly outside
    every output is refused with `{"error":"off_output",x,y,width,height}`.
- `comp.window.wait {match,until,width?,height?,timeout_ms?}` replies when a
  window reaches a state.
  - `match` is either `{id,generation?}` or
    `{app_id?,title?,title_contains?}` with at least one field. Name filters
    are at most 4096 bytes. With `id`, the wait is about that window; an id
    comp never handed out is refused with `unknown_window`. Without it, the
    wait is about the lowest-id mapped window whose names match.
  - `until` is `mapped`, `visible`, `presented` (a frame presented at or
    after the current mapping began; a late report of an earlier frame does
    not count), `size` (needs `width` and `height`, compared with the
    window-geometry size), `focused`, `unmapped` or `gone`. For a match
    without `id`, `unmapped` and `gone` mean no mapped window matches.
    `mapped` is workspace-blind; `visible` and `presented` need the
    window's workspace to be the current one, so a wait on a window that
    is off its workspace times out rather than resolving, and resolves once
    a switch (or `send_to_workspace {follow:true}`) brings it on screen.
  - `timeout_ms` defaults to 10000 and is at most 60000, counted from when
    the port admitted the request.
  - While a session lock is active a wait learns nothing it could not read
    from the (redacted) tree: only `gone` and `unmapped` for a named `id` can
    resolve; everything else waits for the unlock or the deadline, so a
    name-based wait that spans the whole lock ends with `timeout`.
  - A condition that already holds is answered at once. Otherwise comp
    checks after each dispatch cycle and sets one timer for the deadline;
    nothing polls.
  - The reply is `{window:<row>|null,until,waited_ms}`. `window` is the
    `windows.s<id>` row, or null for `unmapped` and `gone`. On the deadline
    the reply is `{"error":"timeout",until,waited_ms}`.
  - Waits and forced closes use the same eight-permit pool as
    `comp.input.sequence`.
- `comp.windows.list {app_id?,title?,title_contains?,visible?,workspace?}`
  returns `{windows:[<row>...]}` in id order, filtered by every given field
  (text filters at most 4096 bytes; `workspace` is an index in `1..=count`
  — above the count is `invalid_value`, not an empty list — `"current"` for
  the current workspace, or `"all"`, the default). The
  rows are the `windows.s<id>` rows. Like that tree, the list has no X11
  windows and is empty while a session lock is active.

Every argument object is checked for unknown fields
(`{"error":"invalid_args",field,allowed}`).

### Input injection

The `comp.input.*` verbs feed the seat exactly as a device does. Every event
enters the one seat entry point with user activity on, so these all apply
unchanged: bindings (an injected `Super+Shift+M` restores a window, and the
client never sees the M), pointer grabs and constraints, click-to-focus and
raise, idle notification, and the session lock. Under a session lock,
injected input reaches only the lock surface.

Event timestamps are CLOCK_MONOTONIC milliseconds, wrapping at 32 bits. That
is the clock `wp_presentation` reports, so a client can subtract an input
event time from a presentation time. Real input from the nested host window
uses the same clock.

| Verb | Arguments |
| --- | --- |
| `comp.input.pointer.move` | One of three forms. `{x,y,output?}`: output-local absolute; `output` is an `outputs` key or output name, and defaults to the default output. `{dx,dy}`: relative. `{window:{id,generation},x,y,require_hit?}`: relative to the window-geometry origin. Any form takes `corners?` (default `true`); `false` keeps the move from arming a hot corner. |
| `comp.input.pointer.button` | `{button?,action?}`. `button`: `left` (default), `right`, `middle`, or an evdev code `0x100..=0x2ff`. `action`: `press`, `release` or `click` (default). |
| `comp.input.pointer.scroll` | `{dx?,dy?,source?,v120?}`. At least one axis is required; an omitted axis stays absent. Positive `dy` scrolls down. `source`: `wheel` (default), `finger` or `continuous`; a zero on `finger` or `continuous` is an axis stop. `v120:{dx?,dy?}` sets wheel detents; without it, a wheel derives 120 per 15 units. |
| `comp.input.key` | `{key,action?,modifiers?}`. `key`: an XKB keysym name (`Return`, `a`, `F5`, `Super_L`) or an evdev code. `action`: `press`, `release` or `tap` (default). `modifiers`: any of `shift`, `ctrl`, `alt`, `super`, `altgr`, held around the key. A keysym that needs Shift gets Shift added. **Or** `{text}`, at most 256 characters. |
| `comp.input.release_all` | `{}` |
| `comp.input.sequence` | `{steps:[{verb,args?,delay_ms?}],interval_ms?}` |

Each single verb replies:

```text
{input_seq, injected_at_us, pointer:{output,x,y}|null, target:{id,generation}|null}
```

- `input_seq` increases by one per verb.
- `injected_at_us` is CLOCK_MONOTONIC microseconds.
- `pointer` is the cursor after the verb, in output-local coordinates.
- `target` is the root surface the seat now delivers to: pointer focus for
  pointer verbs, keyboard focus for key verbs. It can be a layer or lock
  surface, not only a window.

`text` maps each character through the live seat keymap, honouring Caps Lock
and the active layout. Only characters on the first two shift levels map; each
is typed as press and release, with Shift where needed. A newline types
Return. Characters that need AltGr (the third and fourth levels), a dead
key or a compose sequence are refused as unmappable. If any character cannot
be typed, nothing is sent and the reply is
`{"error":"unmappable","char","index"}`. While an input method holds the
keyboard, `text` is refused with `{"error":"ime_active"}`, because the IME
would turn the keys into something other than the text sent. Some input
methods hold that grab whenever a text field has focus; with one of those,
every `text` call is refused, so type with `comp.input.key` instead. An unknown key
name replies `{"error":"unknown_key","key"}`.

An absolute or window-relative move is real pointer motion, so moving into an
output corner arms the hot corner exactly as a mouse would; pass
`corners:false` to move without arming one. Such a move still leaves an
engaged corner and cancels a pending dwell.

One verb injects at most 4096 seat events (a whole sequence included; a text
character counts four, a key with modifiers two per key). A larger request is
refused before anything is sent with `invalid_value` naming the limit.

Every refusal is decided before anything is sent:
- `stale_target`, or another window-target error, for the `window` form;
- `occluded` when `require_hit` is true and the point is not on that window or
  its frame. `under` names the window actually at that point, or is null;
- `off_output` when `require_hit` is true and no output shows the point;
- `unknown_output`;
- `out_of_bounds`, with the output size, for a point outside the output.

The verbs are not refused while the session is locked; the seat decides where
the input goes.

`release_all` releases the keys and buttons that injection pressed and has not
released. It never releases anything a physical device holds. Taps and clicks
never leave a key or button down; only `action: press` holds one.

`comp.input.sequence` runs up to 256 steps in order:
- Each step is one of the single input verbs above, with its usual arguments.
- `delay_ms` is a wait before that step, on a compositor timer. It defaults to
  `interval_ms`, which defaults to 0. The delays together may total at most 60
  seconds.
- A drag is a `press`, some moves, and a `release`.
- The reply is `{steps:[<each step's reply>],elapsed_ms}`.
- A run yields to the event loop after every 256 injected events, so a long
  zero-delay stretch cannot fill a client's socket in one pass. A run is
  therefore not atomic even without delays: other runs and single verbs can
  interleave at those yields (and at every delay).
- If a step is refused, the run stops and gives up the keys and buttons it
  pressed. A hold is released only when no other owner (another run, or a
  single verb that pressed the same key) still holds it; an explicit release
  by anyone lets the key go for every owner.
  The reply is rc 10
  `{"error":"step_failed",index,verb,step:<the refusal>,completed:[...],released:true}`.
- If the caller stops waiting, the run also stops and releases its own holds.

Sequences use their own pool of eight permits. Each waits for its own delays
plus one second, not the two-second budget of other verbs. When all eight
permits are in use, a new sequence gets `busy`.

`input.host.passthrough` exists only on the nested backend; on KMS the path is
`unknown_path`. Setting it to `false` stops the host window's pointer motion,
buttons, scroll and keys reaching the seat, so the host cursor cannot
overwrite an injected position. Output resize and scale, pointer leave and
touch still pass. A host key or button pressed before the switch still gets
its release. A host focus loss releases only those host keys, never an
injected hold, and still resets compositor chrome state (a title-bar drag,
hover, cursor override). The value persists until it is set back to `true`
or the compositor exits; a script that turns it off should turn it on again.

The frame trace records each injected verb as `comp_input_injected`:
- subject: `input_seq`;
- detail: the verb kind (1 move, 2 button, 3 scroll, 4 key, 5 text,
  6 release_all);
- aux: the target id, or 0.

The compositor publishes non-retained messages under the registered service
namespace. The seat instance therefore uses `comp.*`, the default nested
instance uses `comp-nested.*`, and `--bus-service NAME` moves the complete
namespace to `NAME.*`. Every inner command is the unprefixed suffix shown
below, so handlers do not depend on the instance name.

| Topic | Inner command | Exact body |
| --- | --- | --- |
| `<service>.props.changed` | `props.changed` | `{path,old,new,ts,cause,event_seq}` |
| `<service>.surface.mapped` | `surface.mapped` | `{id,role,generation,app_id,title,foreign_id?,event_seq}` |
| `<service>.surface.unmapped` | `surface.unmapped` | `{id,role,generation,app_id,title,foreign_id?,event_seq}` |
| `<service>.focus.changed` | `focus.changed` | `{keyboard,previous,exclusive_latch,event_seq}` |
| `<service>.output.changed` | `output.changed` | `{output,geometry:{x,y,width,height},usable:{x,y,width,height},event_seq}` |
| `<service>.corner.entered` | `corner.entered` | `{output,corner,dwell_ms,event_seq}` |
| `<service>.corner.left` | `corner.left` | `{output,corner,dwell_ms,event_seq}` |
| `<service>.corner.clicked` | `corner.clicked` | `{output,corner,dwell_ms,event_seq}` |
| `<service>.corner.clicked.v2` | `corner.clicked.v2` | `{output,corner,button,kind,modifiers,dwell_ms,event_seq}` |
| `<service>.pointer.changed` | `pointer.changed` | `{version:1,instance,output,position,valid,timestamp_ms,event_seq}` |

Map edges carry the surface's role `generation` and its `app_id` and `title`
(null when the client set none, and null while a session lock is active). A
map edge reports the values at the end of the cycle; an unmap edge reports the
values from before the unmap. A window unmapped and mapped again under a new
role within one cycle emits both edges (unmap with the old generation, then
map with the new one). An XWayland window whose buffer arrived before
its map request now emits its map edge too.

For a stream of window changes (moves, resizes, state, focus), hold a
`comp.props.watch` and read the `windows.s<id>.*` `props.changed` frames, plus
`focus.changed`. There is no separate window topic. To wait for one
condition, use `comp.window.wait` rather than subscribing and hoping: the
topics are not retained, so an edge that happened before the subscription is
never delivered.

Engaged corners consume pointer presses and their matching releases. The v2
click topic reports `button: "left"|"right"` and `kind: "brief"`.
Both buttons emit on release, with no hold timer or hold action. `modifiers`
contains the active `shift`, `ctrl`, `alt` and `super` names captured at press
time in the compositor input path, even if they change before release. The field
is always present, including `modifiers: []`; its presence selects the new
mapping in Quoin. Other buttons are consumed without an action.
Movement further than `input.corners.deadzone_px` from the press position cancels
the pending action, even within the hotspot. Leaving the corner or resetting
engagement (including output changes, lock, or config changes) also cancels it.
Cancellation retains release ownership; returning to the corner cannot revive
the action.

The original `corner.clicked` topic retains its exact JSON body and emits only
successful unmodified LMB brief actions, on release. Every modified click,
including Ctrl/Alt/Super+LMB, emits only v2. V2 consumers should subscribe only
to `corner.clicked.v2` to avoid handling LMB twice. Consumers retaining an old-comp
fallback must deduplicate: each unmodified LMB emits legacy at sequence N immediately followed
by v2 at N+1, with the same output, corner and engagement dwell. Quoin uses this
pair to admit the first click once in either delivery order, then ignores legacy
after observing v2. The legacy sibling is required even with `modifiers: []`:
Quoin canonicalises the v2 sequence to N. Modified clicks keep their own sequence.
This versioning preserves old
shell-hosts with strict JSON decoding, but only partly. A host that predates v2
entirely sees legacy LMB and nothing else. A v2-aware host from before
`modifiers` existed is worse: its strict decoder rejects every v2 body, so it
never marks v2 as seen and acts only on legacy. On that skew RMB and every
Shift/Ctrl/Alt/Super+LMB click is lost silently; only unmodified LMB works.
Upgrade the shell-host before, or together with, the compositor.
Both versions carry engagement dwell, not press duration. Quoin routes LMB brief
to overlay pinning, Shift+LMB to docking, and RMB to the corner menu.
Ctrl/Alt/Super without Shift retain LMB pinning. LMB from docked becomes pinned;
Shift+LMB toggles docked/hidden. No bare button click docks a panel.

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
surface, focus, output, corner and pointer records. If it reaches `u64::MAX`, that value
is offered once and observation enters a terminal exhausted state rather than
reusing a sequence. The outbox is one bounded 256-entry lane. On overflow the
producer evicts one oldest record in fixed time and carries that record's loss
interval inside the next record it sends; if an evicted record already carries
loss, both intervals coalesce. Once the publisher learns an interval, it emits
a gap on each affected topic before the next record it publishes on that topic,
or during the idle flush when the lane drains empty. Survivors produced before
the carried loss reaches the publisher may therefore be published before its
gap. The gap's Bus header is `event_seq=<last lost seq>` (the coalesced
interval's last-lost sequence), which locates the hole, and its body is
`{gap:true,lost_count,cause:"outbox.overflow"}`. `lost_count` is the same
cumulative process-wide counter as `port.lost_count`, not a per-interval tally.
Consecutive intervals coalesce while pending, bounding gap traffic to at most
one gap per topic per published record plus the idle flush. A rejected or
timed-out publication discards its uncertain backlog and recovers under the
same ordering rule with `cause:"publisher.loss"`. A failed pending gap retries
immediately on broker connection-state edges and on a single one-shot backoff
timer (1 second, doubling to a 30-second cap); that timer exists only while the
gap remains pending. After either gap, read a fresh property tree.

Pointer observation reads the latest cursor state after protocol dispatch,
with at least 34 ms between samples. Input handlers do not serialize or wait
for the Bus. Renewing a lease requests a fresh sample even if the pointer is
stationary; expiry leaves no publication timer or motion history. Multiple
local observers share the bounded lease and publication lane.

Valid samples contain the output's advertised name and `position:{x,y}` in
output-local logical pixels. `timestamp_ms` is monotonic elapsed time within
the compositor instance, not wall-clock time. Locking, pausing the KMS session,
leaving the output or an invalid position produces
`valid:false,output:null,position:null`. Retained client output globals do not
keep a paused session's pointer valid. Pause and resume request fresh samples
within the existing rate limit even when coordinates stay unchanged. No key
or button data is included. These transient events are not retained by comp.

Noded reserves `<service>.pointer.changed` publication to its registered owner
and stamps `broker_service` on reserved event deliveries. Local consumers
must check that stamp and `broker_origin:local`, plus instance and sequence;
a topic header alone is not proof of publication. Keep only the latest
sample, clear stale samples on disconnection/lock, and advance the snapshot
fence when output geometry changes. Watch acknowledgements and samples may
arrive in either order over separate connections; keep a bounded early sample
with its receipt time until acknowledgement. A filtered global sequence need
not be consecutive.

The desktop's calloop 0.14 channel patch prevents unrelated child-source
tokens from generating new wakeups. This fixes a session-management busy loop
in the deferred libseat notifier; an inactive VT should wait for genuine
session/input/control events rather than occupy a CPU core.

Cursor projection compares resolved materials, transforms and visibility before
writing them. Unchanged built-in and SHM client cursors therefore avoid repeated
render-asset updates; new client content still invalidates its material binding
when the image handle is reused. DMA-BUF cursors retain per-update material
rebinding while their import owner reports pending render work. Live client-content
rendering can skip unchanged frames; active-VT CPU costs still require measurement.

The renderer also retains a protocol/cursor/asset/component scene revision independently of
capture subscriptions. KMS binds the extracted revision to an acquired frame
and reports it with a successful presentation; later scene changes cannot
relabel a retained frame. Idle admission combines this revision with asynchronous
asset/pipeline readiness and output lifecycle checks.

Main-world observers run after the standard `Last` schedule for installed image,
mesh, 2D material, shader and font assets. They track direct resource changes and
read asset events independently of Bevy's renderer. Late edits and their delayed
events each advance the revision; quiet asset maintenance does not. Bare asset
stores are observed without replacing them or requiring an event plugin. This
records demand, not GPU readiness or arbitrary component changes. Event-bearing
turns require extraction before messages expire.

The same post-Main observation schedule tracks the installed 2D scene's transform,
visibility, sprite, mesh/material binding, text layout/style and camera components.
Additions, changes, removals and despawns advance demand without consuming the
renderer’s removal notifications. This catches layout completion using glyphs
already present in an atlas. The observers run on one thread because they share
the revision writer. Camera and projection observers compare their rendered
values, so target-refresh bookkeeping does not perpetually demand another frame.
Global clear-colour changes also advance demand. New render features must extend
the component inventory.

An idle turn still runs Main, including input, protocol, layout and capture
maintenance. It skips extraction and rendering only for a ready output whose
known scene revision matches a settled, genuinely presented frame. Pending
render commands, acquired frames, capture/security presentations, DMA-BUF work
or quiescence prevent idle. Checking queued commands retains them in their owner
for normal processing or teardown. First-light animation and enabled diagnostic
capture/DMA-BUF probes keep full rendering. Idle services the device-error hook,
nonblocking device polling and Bevy's time handoff without acquiring another frame.

Live KMS readiness carries the output key and generation through startup and
resume. Both supervisors validate each complete frame-event batch against that
identity before watchdog, telemetry, security acknowledgement or callback effects.
A stale submission or cancellation therefore cannot renew output health or
acknowledge another output's presentation. Terminal render failures retain their
existing handling. Update requests also carry a sequence starting at one for
each ready generation. The renderer rejects stale or skipped requests; both
supervisors and pause reconciliation require the reply to match the exact
outstanding request. Reports distinguish a full Main/extract/render pass, with
its demand revision sampled after Main, from proven idle and lifecycle maintenance.
Neither maintenance nor an empty frame-event batch certifies healthy idle or
relaxes the submission watchdog.

Each active renderer update has its own two-second response deadline. The
coordinator waits against the earlier response or submission deadline; silent or
late updates cannot pulse clients or acknowledge security presentation. A timely
empty response does not extend the frame-submission budget. A response timeout
has the distinct `kms-live-update-response-timeout` diagnostic. Validated idle
suspends the submission requirement while keeping response checks active. New
demand starts a fixed submission deadline that further empty replies cannot
postpone. Idle waits use the interruptible coordinator mailbox at nominal output
refresh cadence; callbacks occur at most once per interval. Idle does not count
as a submitted frame or acknowledge security presentation. Registration, resume
scene draining and transition budgets are unchanged.

GPU asset preparation has a separate internal snapshot. It observes extracted
image, mesh and installed 2D material IDs without consuming Bevy's queues, and
retains pending replacements/removals until preparation reflects them. Successful
KMS frame reports include the post-preparation snapshot. Retry work cannot look
complete merely because no new extraction arrived. Pipeline compilation,
DMA-BUF ownership and the final idle-admission decision remain separate.

Pipeline settlement is sampled after drawing and capture, before presentation.
One additional queue-processing pass exposes newly queued pipelines and completed
asynchronous compilation; missing shaders/imports remain pending, while permanent
shader errors are reported separately. New entries or pipelines becoming ready
after drawing are flagged as requiring a later rendered frame. The snapshot is
attached only to successful KMS presentation and is cleared during output teardown.
This pass can start compilation and adds preparation cost; synchronous mode can
compile within the call. It does not loop until pipelines settle or prove every
intended draw was submitted. An independent 30-second settlement deadline prevents
fallback presentations from indefinitely hiding pending assets or pipelines.
The budget resets for a replacement output generation after resume.

The panel holder plane is two verbs, sent like every comp verb as the literal
command (`comp.panel.hold`, not `<service>.panel.hold`) addressed to the
selected service. `comp.panel.hold` takes `output` (raw connector name), `edge`
(`top`, `bottom`, `left`, `right`), `surface` (the layer-shell namespace token),
`holder` (`pointer`, `focus`, `popup`) and boolean `acquire`. `comp.panel.mode`
takes the same output/edge/surface address and `mode` (`hidden`, `pinned`,
`docked`). Both run at the stable observation dispatch boundary and return
`{"accepted":true,"surface":...}`. Malformed arguments are refused as
`invalid_args` naming the offending `field`, with the `allowed` list.
Acquisitions require a live layer on that output. Mode reports survive concealed
panel-layer destruction; releases match their recorded token even if the layer
has already gone, and a release for an edge comp holds no state for is a no-op.
Refusals are `unknown_output`, `unknown_panel_surface` (acquire with no layer),
`panel_output_mismatch` (the token's one layer is on another output),
`ambiguous_panel_surface` (the token names more than one layer, whatever their
order), `panel_owner_mismatch` (the token's layer belongs to a different live
Wayland client than the one that owns the edge) and `locked` (session lock).
Explicit requests are idempotent; persistent modes clear holders and ignore
acquisitions.

The namespace token is created by Quoin for each layer lifetime. It resolves
to comp's own surface identity without relying on client-local Wayland object
numbers or choosing the topmost layer. Popup holds name the menu's own layer
because the panel can be hidden. The association is re-resolved whenever a layer
maps or unmaps, so a mode report that overtakes its layer binds when the layer
maps; tokens are never reused. Namespaces are not authenticated: a client that
copies a token makes it ambiguous, which refuses rather than misdirects. For
the same reason, any enforcement that hides a panel or excludes its input must
act on the surface identity comp resolved from the token, never on a namespace
prefix match. Output
removal drops that output's state without a signal; Quoin rebuilds its panels
with fresh tokens when an output goes, so nothing stale is suppressed.
Besides the explicit holds, comp tracks two holders per reported panel itself
(shell design §4.3). The pointer holder is acquired by dwelling in the edge's
hotspot (the corner engaging) or by the pointer entering the panel's layer or a
held popup's; any contact with those, including an undwelled pass through the
hotspot, keeps it; leaving them all starts an 800 ms conceal delay that re-entry
cancels. The focus holder is keyboard focus on the panel's layer or a held
popup's. A held popup's layer being destroyed releases its hold at once, even
before the client's release arrives; the popup hold also records the keyboard
focus the popup displaced when it takes focus and restores it (toplevel or
layer) only when the popup's destruction moved focus and focus is still where
comp's fallback put it; focus moved off a live popup cancels the restoration,
except into another held popup (a nested menu), which restores back to it.
Membership is evaluated at the stable post-dispatch boundary, and again after
the cycle's Bus controls, so a command is
emitted only when an edge's verdict changes: `reveal` when the first holder
arrives, `conceal` when the last leaves — at once for focus and popup, after the
delay for the pointer. The delay is a single one-shot calloop timer, armed only
while a lingering pointer is the last holder of a hidden panel. A hidden
`comp.panel.mode` report always re-states the current verdict, so a client that
has just started following the commands learns it. Commands go out on
`<service>.panel.command` through the existing bounded observation outbox and
its gap reporting; the version-1 body contains `output`, `edge`, `surface` (the
panel's token when comp has one), `action` and `event_seq`.

Each edge belongs to one Quoin incarnation, identified by the Wayland client
of the first layer comp resolves for it — an identity comp attests itself,
unlike the token. A layer from a different client that is still connected is
refused as `panel_owner_mismatch` and never binds, even when it is the only
layer the (copied) token names; an edge whose owner has gone is taken over by
the next client with nothing of the old incarnation carried across. When the
owning client disconnects (Quoin crashed or exited), comp drops every explicit
hold it acquired — pointer, focus and popup — and the edge conceals by the
normal rules: the automatic pointer and focus holders are comp's own and
still apply.

Comp enforces its conceals (shell design §7: a slow or crashed shell must not
keep a panel shown or taking input). When a conceal ends a reveal comp itself
commanded and Quoin has not applied it 1 s later (its slide takes 200 ms),
comp hides the owner's layers it recorded for that edge — the panel layer and
any popup layer acquired for it — that are still mapped, and excludes them
from input. It does so through the same effective-visibility funnel as
minimising, so the layers stop rendering, stop being hit-tested, lose keyboard
and pointer focus and can no longer hold an exclusive keyboard grab; their
subsurfaces and popups go with them. Exclusion acts only on those surface ids,
never on a namespace or prefix match, so no other client's surface — a
foreign layer or toplevel on the same output — is touched. A first or
re-stated conceal (a hidden mode report) arms nothing: a panel comp never
revealed is shown by one of Quoin's own local holds, such as its startup
intro or an explicit show. Enforcement ends when comp reveals the edge again,
when the client unmaps or destroys the layer itself, on any mode report for
the edge (a live Quoin resynchronising after a stall, a Bus reconnect or a
restart applies the verdict the report draws on its own), on a persistent
mode, and with the owner's disconnect. Its grace shares the single one-shot
timer with the conceal delay.

The read-only `input.corners.holders` leaf is the switch clients gate on. It
reads `true`: the verbs, holder tracking, the conceal timer, enforcement on a
stalled client, disconnect cleanup and resynchronisation are all live, and
Quoin hands reveal/conceal over to comp when it reads it. Two families of
read-only, volatile leaves (served by `comp.props.get`/`list`/`describe`,
never in `props.changed`) report the plane per edge, summed over outputs:
`input.corners.enforced.{top,bottom,left,right}` counts the layers comp is
hiding and excluding right now, and `input.corners.held.{top,bottom,left,right}`
the explicit holds it records. A stalled-shell check reads them: with the
shell stopped (`SIGSTOP`) after a pointer reveal, the pointer's departure
conceals after 800 ms and `enforced.<edge>` reads 1 about a second later;
after `SIGCONT` the shell's own conceal or its next mode report returns it to
0, and `held.<edge>` returns to 0 once its menus have closed.

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
| `input.corners.deadzone_px` | `10.0` | `1.0..=256.0` logical px |
| `input.corners.dwell_ms` | `200` | `0..=5000` ms |
| `input.corners.velocity_max_px_s` | `1500.0` | `1.0..=20000.0` logical px/s |
| `input.corners.affordance` | `true` | boolean |
| `input.corners.discovery` | `false` | boolean |

`deadzone_px` is the hotspot: a square of that many logical units at each
output corner, so it is the same size on a 2x output as on a 1x one.

The compositor draws the hotspot affordance itself, above every client —
layer-shell panels included — and below the cursor, in the scheme accent. With `affordance` true it
shows the engaged hotspot while the pointer rests there, flashes it for
180 ms on every recognised release (the brief LMB or RMB that emits
`corner.clicked.v2`), and, while `discovery` is true, blinks every hotspot
slowly until the first engagement. That engagement sets `discovery` back to
`false` with cause `corner.entered`; a shell that reveals a panel another way
(keyboard) writes `false` itself. The compositor keeps no record of a first
run, so it never turns `discovery` on: a shell does, when its own state says
the user has not yet found the corners — Quoin writes `true` once, on the
launch that finds no `quoin.state.mix`, and creates that file when the
compositor accepts the write, so it is never requested again. `affordance: false` makes the corners
silent without changing detection. Nothing is drawn under a session lock.
The affordance renders only when what it draws changes — once on engage, once
on leave, a few quantised steps for a flash, two frames per 2 s blink — so a
settled corner leaves the renderer idle.

The affordance is ordinary on-screen furniture in the base layer, not a
cursor-plane overlay, so screenshots and screencasts include a hover square
or flash that is showing when they are taken. With the `embedded-quoin`
build feature, the embedded Quoin draws its panels as UI that composites
above the compositor's scene, so an embedded panel covering a corner hides
that corner's affordance; the production Quoin is a separate layer-shell
client and is unaffected, and the ordering belongs to the embedding work.
Squares are sized and placed in each output's own logical coordinates, but
the renderer currently places every output camera over one shared logical
canvas at one output scale — the same limit client placement has — so
mixed-scale, multi-output correctness depends on the renderer's multi-output
camera model, not on the affordance.

The mutable leaves are the six corner leaves, `windows.s<id>.band`,
`windows.s<id>.minimized`, `windows.s<id>.workspace`, `workspaces.count`,
`workspaces.current`, `workspaces.o_<slug>.current`, `input.host.passthrough`
(nested only) and `xwayland.enabled`. The corner, window and workspace
descriptors say `mutable:true` and
`persistence:"none"` (numeric leaves also carry the range above) and those
values live for the compositor process only. `xwayland.enabled` is the one
exception: its descriptor says
`persistence:"file"` — the value is read once at compositor startup (whether
to spawn XWayland at all; there is no live toggle) and a write persists it
for the NEXT startup into a per-socket file under the COSMIX etc tree, whose
resolved absolute path the read-only `xwayland.persist_path` leaf reports
and the compositor logs at startup. The `COSMIX_COMP_XWAYLAND` environment
variable (`0/false/off/no` or `1/true/on/yes`) overrides both the file and
the default at launch — the no-rebuild back-out that works even when the
props surface is unreachable. Unknown and immutable
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

All application errors use Bus rc 10. Every refusal body carries
`error_code` with the same value as `error` (`error` is kept as an alias for
0.58.x), so a Mix `send` receives the whole object: `$result.error_code`,
`$result.under` and so on. In addition to the write errors above,
read/dispatch errors include `unknown_path`, `busy` and `unknown_verb`, plus
`{"error":"too_large","limit_bytes":N,"hint":"read a subtree"}` when a
serialised reply would exceed the effective broker-path ceiling. `N` is
8,384,512 bytes: `min(16 MiB Bus message, 8 MiB single WebSocket frame)` minus
4 KiB of documented header/framing headroom. Immediately before sending, comp
also measures the actual canonical response headers, correlation id and
framing with the body and refuses any reply whose complete wire size would
exceed that ceiling. Replies are never truncated.

A `busy` reply to `comp.props.set` or a window verb means the reply missed its
two-second budget or the queue was full at admission. If the request had
already been queued, it can still be applied when the compositor drains the
queue. A caller that gets `busy` should read the state again (for example
`windows.s<id>.minimized`) before retrying, not retry blindly. An absent broker never
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

- `comp.surface.*` control verbs: focus, raise, close, move and resize are
  `comp.window.*` verbs on managed windows only;
- render timings, because they are metrics rather than properties; and
- a Bus screenshot verb, because it is a later control-plane slice; Arc 4's
  capture service is available through the Wayland protocol described below.

## Supported Wayland protocols

The compositor advertises the core compositor, subcompositor, seat, output,
shared-memory, DMA-BUF, explicit synchronisation, viewporter, fractional-scale,
presentation-time (see below), XDG shell and XDG decoration globals needed by its desktop
clients.

| Protocol | Version | Current support |
| --- | ---: | --- |
| `zwlr_layer_shell_v1` | 4 | Layer surfaces and layer popups map, arrange and configure through Smithay's `LayerMap`; protocol strata, keyboard interactivity, input regions and exclusive usable-area effects are supported. |
| `ext_idle_notifier_v1` | 2 | Per-seat notifications use Smithay's calloop timers; real pointer, keyboard, touch, pointer-gesture and tablet-tool activity resets the timeout and resumes an idle notification. Device-removal reconciliation does not count as activity. |
| `ext_foreign_toplevel_list_v1` | 1 | Mapped XDG toplevels expose stable mapping identifiers, title and app ID updates; unmap or destruction closes the handle, and late clients receive the current mapped set. |
| `ext_session_lock_v1` | 1 | Nested and live KMS modes support immediate output-sized lock-surface configures, secure blank-first presentation acknowledgement, lock-only input, VT pause/resume preservation and the locked/orphaned lifecycle. |
| `zwlr_screencopy_manager_v1` | 3 | Compatibility output capture into exact-layout `wl_shm` buffers, plus eligible whole-output v3 DMA-BUF destinations; includes clipped SHM regions, real damage waiting, exact cursor inclusion and presentation-timestamped nested or KMS completion. |
| `wp_presentation` | 2 | Nested mode, and live KMS in client-content mode with kernel page-flip times, vblank sequence and mode refresh (see Presentation feedback below). |

### Presentation feedback

`wp_presentation` reports when a client's commit was actually shown. The
global is advertised only once a backend has wired a frame reporter, so no
client waits on feedback nothing will resolve.

- **Clock:** CLOCK_MONOTONIC (`clock_id` 1), the same clock `frame_trace`
  uses.
- **Which commit a frame showed:** feedback is taken when the commit is
  applied, so a later commit cannot discard it while the earlier one is still
  on its way to the screen. Every new buffer the compositor hands to the
  renderer gets a content sequence number, and a presented frame reports, per
  surface, the sequence it actually sampled. Feedback for exactly that commit
  is `presented`. Commits older than it were replaced before any frame showed
  them, so they are `discarded`. Newer commits keep waiting. A commit without
  a new buffer carries the sequence of the content it leaves on screen, so it
  is presented with the next frame that shows that content. When one
  transaction applies several commits at once (a synchronised subsurface
  waiting for its parent, or a commit held by a blocker), each commit keeps
  its own feedback: a commit whose buffer a later commit in the same
  transaction replaced is `discarded`, even if the later commit asked for no
  feedback.
- **Re-uploads are not refusals:** when the compositor re-sends a surface's
  current content (a relayout or recovery) and that upload fails, the content
  already on screen stays shown and its feedback is not discarded. Only a
  failed newer commit is.
- **What "shown" means:** the surface is mapped and visible, lies at least
  partly on the output, and the frame samples that commit's texture.
  Occlusion by other windows is not checked, so a fully covered window still
  counts as shown.
- **Discarded without a frame:** unmap, minimise, destroy, a new role, a
  buffer the compositor or renderer refused, a surface that can no longer be
  drawn, a workspace switch or move that takes the window off the current
  workspace, and commits made before the surface was mapped (including an
  X11 window's commits before its map). A session lock needs no extra step: while
  locked, every frame treats the surfaces the lock hides as not shown. At most
  8 commits per surface wait; a faster client loses the oldest as `discarded`.
- **Nested backend:** the host compositor gives no presentation timing, so
  `tv` is CLOCK_MONOTONIC when the frame was handed to the host (not first
  photon), `flags` is 0, `seq` is 0 and `refresh` is 0 (unknown). It is
  reported only after the swapchain image was actually presented.
- **KMS backend:** advertised in client-content mode (not in
  `--first-light`, which draws no clients). A frame is reported only
  after its atomic commit's page-flip event arrived; a cancelled or failed
  flip reports nothing, and its commits keep waiting.
  - `tv` is the kernel's page-flip time. At startup comp asks DRM whether
    those stamps are CLOCK_MONOTONIC (`DRM_CAP_TIMESTAMP_MONOTONIC`). If they
    are not, each stamp is moved from CLOCK_REALTIME by the offset between
    the two clocks sampled when the event is read. If the capability query
    fails, `tv` is the time the event was read.
  - A MONOTONIC stamp may lie up to one refresh period in the future:
    vblank-helper drivers stamp the start of scanout, which the flip event
    can precede. Such a stamp is kept. A stamp further ahead is reported at
    read time without `hw_clock`; a second ahead, or three such flips in a
    row, and the capability is not trusted for the rest of the run.
  - `flags` are `vsync` (there are no async flips) and `hw_completion` (the
    flip-complete event), plus `hw_clock` only when `tv` is the kernel's own
    MONOTONIC stamp. Never `zero_copy`: client buffers are composited into
    scanout buffers, not scanned out directly.
  - A device without vblank support (virtio-gpu, simpledrm; probed with
    `DRM_IOCTL_CRTC_GET_SEQUENCE`, or seen as a sequence stuck at 0)
    completes flips on no vblank grid, so its frames carry only
    `hw_completion` and `seq` 0.
  - `seq` is the CRTC's vblank counter at the flip, extended per output
    beyond 32 bits so it only ever increases (across wraps, and across a
    counter that restarts on resume).
  - `refresh` is the scanned-out mode's period, `1e12 / refresh_mHz` ns.
  - Each flip is reported on the client output registered for its
    connector; a flip on an output that is not a client output resolves
    nothing. A flip that completed just before a VT switch is still
    reported.
  - With several outputs, each flip carries the frame's content and the first
    presented one wins (content is not yet tracked per output); content-source
    costs are counted with the first flip only.
  - A buffer the renderer fails to import is discarded as soon as the render
    world sees it, as on the nested backend.
- **SHM clients:** a surface counts as sampling its newest buffer once the
  GPU image is prepared. Bevy drops the old GPU image while a replacement is
  pending, so a pending upload is reported as not shown rather than stale.
- **DMA-BUF clients:** the import bridge replaces images in place and keeps
  the previous texture while a replacement is pending or after it failed.
  A frame therefore counts a commit as shown only when the bridge reports
  that commit's import as the installed one. A failed import is `discarded`,
  never presented.

In-process scene content (a Bevy plugin inside comp, not a Wayland client)
can be measured the same way: the plugin puts a `ContentSource` component on
its root entity and bumps `ContentSourceFrame.revision` for every content
update. Revisions shown in a presented frame count as presented, skipped ones
as discarded, and upload/damage costs are carried until a presented frame
reports them. Changing the id (re-inserting the component) re-registers the
source; a refused duplicate takes over the id when its holder goes.
`ContentSource.output` is reported as `sources.<id>.output`; the source is
measured on the output that reports the frame (nested has one).

### Presentation statistics

Every window, output and content source is measured, whether or not the
client asks for feedback. The statistics follow content, not feedback
objects: an update is one buffer handed to the renderer (a window) or one
revision (a content source). A frame that shows a newer update presents it;
the updates it skipped count as `discarded`.

- **Windows:** `windows.s<id>.presentation.*`. Subsurface updates count for
  their window; a frame that shows several surfaces of one window counts
  once. A new role (a new `generation`) starts from zero.
- **Shown, stalled, hidden.** A frame that samples a newer update presents
  it. A visible window whose newest update is not sampled yet (a texture
  still uploading) is stalled: its run continues, and the eventual
  presentation measures the whole gap and counts the vblanks it missed. A
  frame that hides a window (minimised, off the output, locked away, or not
  in the frame at all) discards the updates it did not show, as the feedback
  protocol does; showing the same content again later is not a new
  presentation.
- **Intervals** are measured only between two presentations while the window
  stays shown, so a minimised or hidden stretch is not one long interval. An
  idle client's gaps are included, so the interval leaves describe cadence
  only for a client that updates steadily.
- **`commit_to_present`** runs from the moment the compositor publishes the
  buffer to the renderer (inside the client's commit, after it is accepted)
  to the frame that showed it. For a content source it starts when comp
  first sees the revision.
- **`missed`** counts vblanks skipped while an update was waiting: for a
  fixed refresh `R`, a gap of `round(interval / R)` vblanks counts the skipped
  ones at or after the moment the oldest waiting update was committed. An
  idle client that commits late misses nothing. With an unknown or variable
  refresh (nested) `missed` is null, never 0: unmeasured is not perfect.
- **`input_to_present`** is the time from an injected input to the first
  presented update committed after it, within one second (a comp-side upper
  bound; a hide or reset drops the wait). The input goes to the window that
  owns the target surface, through subsurfaces and popups. A content
  source's update records it when it names the input's `input_seq`. These
  leaves stay null until the `comp.input.*` injection verbs record marks;
  every injection has its own increasing `input_seq`.
- **Outputs:** `outputs.o_<slug>.presentation.*` counts presented frames.
  `flags` names the newest frame's kind flags (`vsync`, `hw_clock`,
  `hw_completion`, `zero_copy`) and `flags_mask` is the same as a number;
  both are null before the first frame. `refresh_us` is null when the
  refresh is unknown (nested) or variable, never 0.
- **Single output for now:** a frame report names one output and a surface
  is either shown by it or not; per-output accounting of a window shown on
  two outputs is not split yet.
- **Rings** keep the newest 512 samples; `_p50`/`_p99` are nearest-rank
  percentiles over them, null without samples.
- All times are CLOCK_MONOTONIC microseconds; `since_us` is when counting
  started (the first update, registration, compositor start, or the last
  reset).

The presentation leaves and the whole `sources` subtree are **volatile**:
`get`, `list` and `describe` serve them (`describe` says `volatile: true`),
but `props.changed` never reports them, so a watched client presenting at
60 Hz does not flood the topic. Row add and remove events carry no
presentation leaves either. Reads compute these leaves only for the paths
they can reach, and property-change diffs never compute them.

Counts saturate. Updates are assumed to be numbered one apart (a surface's
buffers, a source's revisions); a jump counts every skipped number as
discarded.

`comp.window.stats {id, generation, samples?}` returns the window's leaves
plus the newest `samples` (default and maximum 512) of `intervals_us`,
`commit_to_present_us` and `input_to_present_us`. `{source, registration?,
samples?}` does the same for a content source and adds `upload_bytes` and
`damage_px` (per reported frame). `{id, generation}` and `{source}` are
mutually exclusive (`invalid_value`). `comp.window.stats.reset` takes the same
targets without `samples`, or no target to zero every window, output and
source; it replies `{reset, since_us, ...}`. Errors: the window errors of
`comp.window.*`, `unknown_source`, and `stale_target` with `registration` and
`current` when the source id was registered again. A session lock refuses the
window forms (`locked`); source reads and the global reset still work.

Each `comp.window.*` verb emits a `comp_window_control` trace record
(subject window id or 0, aux generation; detail 1 minimise / 2 restore /
3 focus / 4 raise / 5 close / 6 place / 7 wait and stats / 8 forced close
and stats reset / 9 workspace switch / 10 send to workspace — the 7/8
double use is renumbered in 0.60).

Content-source honesty limits: a source counts as presented when its entity
was visible in a presented frame; comp cannot tell whether the plugin's own
texture upload for that revision had finished. Flags and clock are the
frame's. Re-inserting the same id with another `output` keeps the original
registration and its output. Only the newest `consumed_input` is kept
between two reports.

Gate G1s: a build with the test-only `content-source-probe` feature adds a
nested quad registered as source `probe` that changes every frame for
`COSMIX_CONTENT_SOURCE_PROBE_REVISIONS` revisions (default 300), logs
`COSMIX_CONTENT_SOURCE_PROBE DONE revisions=… upload_bytes=… damage_px=…`,
holds for four seconds and despawns (`… DESPAWNED`).

## XWayland

The `xwayland` cargo feature is in the default set: every default build
supervises one rootless Xwayland instance and acts as its X11 window
manager. The runtime control is the `xwayland.enabled` property described
above (startup-read, file-persisted per socket) with the
`COSMIX_COMP_XWAYLAND` environment variable as the launch-time override —
the cargo feature is no longer the switch. The read-only `xwayland.display`
leaf reports the X display (`:N`) of the ready generation — the Bus-side
equivalent of the per-socket `DISPLAY` descriptor file, published at the
same moment and null while no generation serves X clients (watch it to know
when `xprop`/`xdotool` can connect). Normal X11 windows become managed toplevels on the existing
scene, buffer, focus, stacking and server-side-decoration paths: association
(via the xwayland-shell serial handshake) creates the window's surface
record, the map grant makes it eligible, and its first committed buffer
renders through exactly the renderer path a Wayland toplevel uses — the
renderer has no X11 branch. Title/class metadata, focus (including the X
`SetInputFocus`/`WM_TAKE_FOCUS` half), interactive and client-requested
move/resize, maximise/minimise/fullscreen, EWMH state mirroring (including
the virtual-desktop trio `_NET_NUMBER_OF_DESKTOPS` / `_NET_CURRENT_DESKTOP`
/ `_NET_WM_DESKTOP`, described under Window switching and X11 placement),
close via `WM_DELETE_WINDOW`, and cross-protocol stacking in the normal
band are supported.

`DISPLAY` is never set globally. After the XWM owns `WM_S0`, the compositor
atomically publishes a mode-0600 per-socket descriptor at
`$XDG_RUNTIME_DIR/cosmix-comp/<WAYLAND_DISPLAY>.xwayland.env` containing
`DISPLAY=:N` and the XWayland generation; launchers read it once and pass
`DISPLAY` explicitly to each X client. A missing `Xwayland` binary or a
failed start degrades to a fully working native-Wayland compositor with a
warning. An unexpected XWayland death destroys that generation's windows,
removes the descriptor and arms a single 60-second one-shot restart backstop;
one retry credit exists, and only a generation that then survives five
minutes restores it. There is no readiness or liveness polling anywhere in
the path.

**Override-redirect windows** — X11 menus, context menus, tooltips and
combo-box drop-downs — render (X-2a): they get a surface record on the
ordinary renderer path at their own absolute client coordinates (negative
origins included; the compositor never places, clamps, configures, grants
or decorates them — including for `_NET_WM_MOVERESIZE`, whose interactive
move/resize is refused for override-redirect windows), stack in the
normal band and are raised to its top when mapped (not tethered to a
parent and not kept on top afterwards), and acquire none of the managed
behaviours — no focus candidacy, no minimise/maximise, no
foreign-toplevel export. Keyboard focus deliberately never moves to an
override-redirect window — focus arbitration itself refuses them, so
neither a click nor a touchscreen tap on one changes focus: the X
client's own grab machinery routes keys. In the property tree they appear
under `surfaces.*` with `role:"x11-override-redirect"` (managed X11
windows are `"x11-toplevel"`); neither appears in `windows.*`, which
remains the xdg-toplevel projection. A window that changes its
override-redirect flag between map cycles transitions by record
destruction and rebirth in both directions. Known scope edges: relative
sibling restacks are ignored, a menu overlapping a layer-shell panel
draws under the panel, and dismissal is client-owned — a click on a
pure-Wayland surface is invisible to the X grab and dismissal then
depends on the client's grab-break handling.

**Still not supported, by design:**

- **Clipboard and primary selection** are not bridged in either direction;
  selection access is refused at the XWM (X-2b).
- **Drag-and-drop** across the X11/Wayland boundary (X-3).
- **HiDPI/fractional scaling** for X11 clients: the X11 client scale is held
  at 1 and RandR primary-output changes are only logged (X-3).
- **KMS qualification**: X11 rendering is proven on the nested backend; the
  live-KMS proof is a later slice (X-3).
- Relative X restack requests (`Above`/`Below`/`Bottom` siblings) are refused
  with a log; the compositor scene stays the stacking authority and only
  raise-to-top is honoured.

## Screen capture

SHM publication copies bounded chunks while continuing to dispatch clients.
Queued chunks keep the protocol loop runnable: they never wait for unrelated
input or surface commits between copies. Once publication finishes or is
cancelled, the loop resumes its normal blocking wait.
On x86-64 CPUs with SSE4.1, unrotated GPU readback uses aligned streaming loads
into cached CPU memory to avoid slow generic copies from write-combining
mappings. Other CPUs retain the portable copy path.
Capture reservations account for source, staging and converted pixel storage.
They are capped at 512 MiB per client and 1 GiB globally, allowing three
active full 4K SHM captures plus one retiring request per client. Request
count caps remain four per client and eight globally. This bounded memory
allowance overlaps readback latency without bypassing reservation accounting.

Arc 4 provides `wlr-screencopy-unstable-v1` output capture for existing
clients such as `grim`. A frame advertises one opaque `XRGB8888` shared-memory
layout with an exact `width * 4` stride. Whole-output and logically clipped
region requests are converted into displayed physical pixels; invalid or stale
outputs fail rather than falling back to another output. Plain copies are
force-presented, including on an otherwise idle output. Damage copies wait for
relevant base-scene damage, or cursor-only damage for a cursor-inclusive copy,
without forcing a frame. The bounded journal is manager-scoped; its baseline
advances only after `ready`, and history overflow conservatively reports the
full captured region.

Version 3 frames may additionally advertise a DMA-BUF destination after the
SHM `buffer` event and before `buffer_done`. The advertisement is immutable for
that frame and exists only for a whole-output request whose transform is
Normal, whose displayed and storage extents are equal, and whose underlying
render texture has an exact copy-compatible opaque format. Its modifiers are
the intersection of linux-dmabuf feedback and an exact Vulkan external-image
`TRANSFER_DST` import query at that extent. Versions 1 and 2 never receive the
DMA-BUF event; regions, transformed outputs and unsupported renderer states
remain SHM-only.

A submitted DMA-BUF must have exactly one plane and match the advertised
fourcc and extent. A kind, fourcc, extent or plane-count mismatch posts
`invalid_buffer` on the frame and never enters renderer admission. A modifier
outside the frame's stored transfer-destination set is instead an operational
miss and produces recoverable `failed`: linux-dmabuf feedback can legitimately
guide a client to a sampled-image modifier which this exact capture use cannot
import. Other operational misses—fd cloning, import/acquire, capacity,
deadline, cancellation, resize, completion or FOREIGN release—also produce one
`failed`. The compositor never sends damage or flags before discovering such a
failure.

A submitted `wl_buffer` does not carry the DRM device which allocated it.
Version 4 linux-dmabuf feedback steers compliant allocators to the renderer's
real `main_device`, and an import failure fails the frame, but the compositor
also fail-closes advertisement if bridge and feedback renderer identities ever
disagree. It does not pretend that `Dmabuf::node()` proves allocation identity. A
cross-device import which succeeds is a residual hardware risk and belongs to
the real-GBM gate.

Pixel completion and the matching output presentation form a two-part completion
latch: `ready` is sent only after both arrive for the same frame. For SHM the
completion half is mapped readback. For DMA-BUF, FOREIGN acquire is encoded in
the same wgpu command buffer as the copy; the worker waits for that exact
`SubmissionIndex`, submits the release barrier through wgpu's thread-safe queue,
then waits for the exact release submission. No capture ownership barrier uses
a raw queue submit or an infinite fence wait. Sampled-image ownership barriers
also enter wgpu-owned command buffers, so every renderer-queue submission has
one authority: `wgpu::Queue`. A single bounded completion
authority owns those destination jobs. It retries a transient 250 ms GPU wait
up to four bounded attempts, treats a full/disconnected job queue as terminal,
and never waits on the render or protocol thread. The shared terminal gate
closes the sole sender while excluding concurrent submissions, then drains the
definitively closed channel. Any terminal worker failure fails and safely
strands every live post-import job and clears future
screencopy DMA-BUF advertisements. Nested records
are bound to the exact acquired host window texture-view identity; a missing,
unconsumed or mismatched acquisition fails that capture instead of rebinding it
to a later presentation. Nested mode uses the completed host presentation time;
KMS retains the exact `tv_sec`/`tv_usec` from the
matching kernel page-flip event. The compositor bounds live and in-flight jobs, reserves
a byte budget before allocation, performs conversion away from the render and
protocol threads, then copies shared memory in bounded chunks across protocol
loop iterations. Each client may bind at most eight live screencopy managers,
with 64 live managers globally; exceeding either implementation limit is a fatal
protocol error on the new manager object. Cancellation is checked at the
protocol outbox, ECS queue, renderer-owned map and conversion worker. GPU map
errors become the frame's `failed` event. The byte
reservation remains charged until the last renderer-side request or
packed-result holder drops. Once the renderer submits its COPY_SRC readback,
the lease remains with the cancellable map job. Admission also requests a redraw, so a plain
copy is presented even when the output has no animation or other damage. Every
admitted copy has a five-second absolute request deadline: this is a deadline on
that one client operation, not a periodic compositor timer.

Main-world maintenance retires expired or cancelled admissions before rendering,
including PNG requests deferred for output readiness. It preserves live requests
and their original deadlines, and leaves submitted jobs with their completion
workers. Read-only demand checks match the capture source and KMS generation;
waiting for damage or holding a PNG completion slot does not by itself require a
render. The nested redirection path uses these checks to avoid preparing a target
for stale work; live KMS idle admission uses the same non-consuming checks.

Completion does not depend on a later render tick. The retirement worker sends
its result directly through the calloop-backed protocol command channel, which
wakes an idle protocol loop; the destination can therefore reach `ready` on a
static desktop without polling or a redraw timer. Teardown first puts every
live job into failed/strand mode and closes the sole job sender. It performs a
non-blocking worker acknowledgement check: an already-finished worker is
joined, while an unacknowledged worker is detached. If a driver call never
returns, that detached worker retains the in-flight job and all queued jobs,
including every import, buffer token and reporter, until it returns or the
process exits. The retained set is bounded by `MAX_IN_FLIGHT_CAPTURES`; renderer
teardown itself does not wait.
A pre-import failure releases the retained buffer token immediately. After an
acquire/copy submission, only successful copy retirement plus successful
FOREIGN hand-back may release it; an unprovable hand-back strands both import
and token. `fail_capture` cancels publication but cannot release that
renderer-owned half early. Sending `wl_buffer.release` after the client has
already destroyed its object relies on Wayland's inert-object send behaviour.

The existing SIGUSR1/evidence PNG path shares the renderer-owned RGBA snapshot,
deadline, cancellation sweep and conversion worker with wire capture. The PNG
and wire consumers then create their own packed BGRA buffers for their distinct
outputs. PNG capture retains its filename, atomic-rename and cadence contract;
an unavailable output or an encode/write task which starts after the deadline
releases the one-batch-in-flight latch. A genuinely blocked filesystem write
cannot be cancelled safely and remains outside this deadline guarantee.

PNG requests which reach KMS before their camera or frame token is ready are
retained for a later render frame within the original deadline. This does not
extend the deadline or add a timer. Terminal PNG failures emit a structured
reason, target and output path so a missing evidence image can be diagnosed;
the batch latch is released exactly once on completion or failure.
For unrotated output, pixel normalisation bulk-copies each mapped row into
CPU memory before any BGRA channel swizzle. This avoids slow scalar reads
from mapped GPU memory at high resolutions without extending capture deadlines.

The per-destination fd duplication and Vulkan image creation/bind syscalls run
on the render thread. This is intentionally retained for S-2: admission is
bounded to eight destinations per render batch, and moving import into the
worker would break same-frame copy-out. Hardware-gate runs should continue to
record this bounded syscall cost. Sampled-image cleanup now retains FOREIGN
release batches across frames and checks their exact submission with a zero
timeout. Acquire remains queue-ordered ahead of sampling. The retirement worker
also polls without holding wgpu's fence lock across GPU progress waits. See the
[asynchronous ownership contract](cosmix-wgpu-dmabuf.md#asynchronous-sampled-image-retirement-0150).
Flag either `failed to release DMA-BUF queue ownership` or
`DMA-BUF release completion unproven`: submission failure or an unproved release
after the 250 ms deadline strands the backing and withholds `wl_buffer.release`.
That cached image cannot be reused; another use requires a fresh import.

Live renderer reconstruction currently rebuilds the renderer, capture bridge,
advertisement registry and retirement worker together, leaving DMA-BUF
advertisement empty until the new render world republishes it. This whole
restart path is structurally pinned by `run_live_render_pump`, but remains an
explicit untested end-to-end path because the regression would require the
forbidden real-seat live pump.

`overlay_cursor=0` selects the cursor-free base. Every non-zero value selects an
inclusive copy using the retained default, chrome or client cursor asset with
its hotspot, clipping and hidden state. Nested mode copies its cursor-free scene
to the host swapchain and composites the capture-only overlay into a separate
temporary target, avoiding a doubled host cursor. The production cursor camera
renders into an independent transparent target, so it cannot clear or swap the
scene camera's base texture. SHM and imported DMA-BUF cursor assets remain
retained and are sampled from that target on the GPU in both nested and KMS
capture. KMS base copies precede the GPU overlay into scan-out and inclusive
copies follow it.

Nested redirect and cursor-composed textures use an unsuffixed BGRA8/RGBA8 base
with the matching sRGB view format. Rendering therefore keeps the sRGB view,
while DMA-BUF copies compare and copy the exact linear base format used by the
destination import.

Capture is deliberately default-open, including while the session is locked.
Lock and unlock change the capture epoch: stale work fails, while newly admitted
work captures the currently displayed lock surface or compositor-owned black
fallback. This is an agentic desktop policy rather than a portal permission
prompt.

KMS copies select the exact Ready `OutputKey` and generation, copy out within
that frame without retaining a slot or storing a destination token in the
scan-out pool, and latch completion against its
acquisition token and kernel page-flip timestamp. Pause, unplug, generation
replacement, cancellation and map failure fail the affected one-shot rather
than returning another output or stale pixels. `--first-light` keeps the same
capture feed and completion path while ignoring client scene content; every
changed animation frame marks full-output damage, so `copy_with_damage` wakes.
The wlr protocol is a compatibility surface; the planned
`ext-image-copy-capture-v1` implementation will become another consumer of the
same capture service. The automated nested acceptance gate uses `grim`; the
`cosmix-screencopy-probe` binary is a deadline-bounded manual diagnostic for the
advertised layout, non-zero SHM offset, guard bytes and non-black pixels. Its
`--dmabuf --drm-node PATH` mode waits for `buffer_done`, allocates an advertised
modifier with GBM, submits that destination, maps it only after `ready`, and
prints the presentation timestamp plus a content checksum.
DMA-BUF advertisements are republished from live view targets after every
output registration/re-registration and generation change. Reconstructing a
renderer reconstructs its bridge, completion worker and advertisement registry;
until that replacement has published fresh capabilities, new frames advertise
SHM only.
Automated tests cover readback, transforms, ordering and damage. A real Vulkan
render-attachment gate uses the production GPU cursor-composite pass, reads back
base and inclusive bytes, compares both with byte-exact references, and proves
that channel-swap and shifted-hotspot mutants fail. These gates prefer a Vulkan
fallback adapter. With `COSMIX_REQUIRE_FALLBACK_ADAPTER=1`, absence of one fails
the test (the CI rule); otherwise the gate runs on an available Vulkan adapter
and prints one line naming the adapter actually used. Physical driver behaviour,
real kernel page-flip clock provenance and end-to-end `grim -o` on KMS remain
manual-hardware-only checks. The automated Vulkan equivalence gate uses an
ordinary Vulkan COPY_DST texture; it does not prove real GBM allocation,
DMA-BUF import, cross-device behaviour or FOREIGN ownership on a physical
driver. Those are explicitly hardware-gated.

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
layer may grab normally. A latched layer that holds the keyboard (with no
popup keyboard grab active) and commits
`OnDemand` keeps the keyboard: the latch ends, the focus stays, and from then
on a click elsewhere moves it like any `OnDemand` layer (Quoin's panels ask
for the grab only until it lands). When a focused layer stops being eligible, focus
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

## Explicit-sync fault policy

A permanently faulted explicit-sync retirement pipeline restarts the
compositor loudly instead of degrading the session (since 0.57.0). Previously
a retirement fault withdrew the `linux-drm-syncobj-v1` global and the session
kept running on implicit sync — DMA-BUF clients could then be scanned out
mid-write, which shows as constant flicker, and nothing surfaced the
degradation. Now every fault path (asynchronous worker report, worker channel
closure, and the synchronous request-time fault) withdraws the global for the
brief teardown window, then requests a `RuntimeFailure` shutdown: under KMS
the process exits non-zero for its supervisor to restart with explicit sync
intact; nested, the scene fails loudly.

Transient stalls get bounded patience before that happens: the retirement
worker grants each batch one 3 s wait on its captured submission index (sized
to outlast a GPU engine-reset recovery) before a timeout is terminal — a
single fixed-target wait, not a retry loop, because re-invoking the wait
submits a fresh empty batch and chases a moving target. Structural faults
(adapter panic, wait failure) are terminal immediately.

Two read-only props leaves expose the state over the Bus:
`info.explicit_sync_advertised` (the protocol global is currently offered to
clients) and `info.explicit_sync_healthy` (the retirement pipeline has not
permanently faulted). Reads are computed from live state per request. They are
not covered by `comp.props.watch` diffs: after the loud-restart policy a
fault's unhealthy window is at most one dispatch cycle, so the leaves are
process-lifetime constants in practice — poll them, don't watch them.

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

Smithay's `X11Surface` has one additive test-support setter
(`set_wl_surface_offline`) that assigns the associated `wl_surface` directly.
The xwayland-shell serial handshake owns the real association; the setter
exists only so the compositor's deterministic tests can fabricate offline
X11 surfaces whose focus forwarding and metadata lookups still reach a real
`wl_surface`.

The vendored XWM also carries the EWMH virtual-desktop additions (comp
0.59.0): the `_NET_NUMBER_OF_DESKTOPS`, `_NET_CURRENT_DESKTOP` and
`_NET_WM_DESKTOP` atoms (interned, advertised in `_NET_SUPPORTED`, the root
pair written as `1`/`0` at WM start), `X11Wm::set_number_of_desktops` /
`X11Wm::set_current_desktop` for the root pair, `X11Surface::set_desktop`
for the per-window property (with a `desktop()` read-back of the last value
asked for, so the offline tests can see it through a dead connection), and
an `XwmHandler::desktop_request` callback dispatched for 32-bit
`_NET_WM_DESKTOP` client messages and an `XwmHandler::current_desktop_request`
callback for 32-bit `_NET_CURRENT_DESKTOP` root messages, both no-ops by
default: the compositor owns the desktop model and decides whether a
request becomes a move or a switch.

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

The vendored wgpu-core carries a hand-backport of upstream wgpu `385520f7`
("re-read the fence before the queue-empty assert in `Device::maintain`").
Without it, a timed-out fence wait racing another thread's queue-drain
panicked the retirement worker on a defensive assert — the trigger for the
explicit-sync fault policy above firing every session. Provenance and the
inherited caveats are in `src/desktop/vendor/README.md`.
