# cosmix-wallpaper

`cosmix-wallpaper` is a native Bevy background client. It uses a click-through
layer-shell surface on each output, obtains window geometry from comp over
the ABP Bus, and keeps simulation coordinates in output-local logical pixels.
The renderer is independent of the compositor and Quoin.

## User preferences

The `wallpaper` Bus service exposes `wallpaper.props.get`, `.list`, `.describe`
and `.set`, plus `.watch`. Get/describe accept `path`; list accepts `prefix`. Omitting a get
path returns the preference tree. Set accepts one leaf path and a typed value.
Writes accept registered local callers and admitted mesh services over ABP.
For mesh writes, the recipient's noded must stamp `broker_origin=mesh`, the
admitted `broker_peer` and originating `broker_service`; the sender must be
that peer's registered `bridge-PEER`. An origin label alone is insufficient.
Missing or conflicting identity fields fail without changing preferences.
These are broker-owned delivery fields, not parameters callers should supply.

| Path | Default | Accepted values |
| --- | --- | --- |
| `enabled` | `true` | Disabling removes the background surfaces; enabling recreates them |
| `paused` | `false` | Explicit pause, independent of automatic lock/scene pauses |
| `preset` | `ocean` | `ocean`, `ember`, `twilight` colour palettes |
| `flock.count` | `192` | Integer 0–1024 per output |
| `speed` | `80` | 5–300 logical pixels per second |
| `pointer.radius` | `110` | 0–600 logical pixels |
| `window.margin` | `12` | 0–100 logical pixels |
| `fps_limit` | `30` | Integer 1–60; compositor callbacks may reduce the actual rate |
| `seed` | `42` | Unsigned 64-bit integer; changing it restarts the seeded flock |

For example, save this as `wallpaper-settings.mix` and run
`mix --serve wallpaper-settings.mix --name wallpaper-settings` on the desktop
node. Serve mode registers the caller before running these writes; an anonymous
one-shot caller can read properties but cannot write them.

```mix
send wallpaper wallpaper.props.get
send wallpaper wallpaper.props.set path="preset" value="ember"
send wallpaper wallpaper.props.set path="flock.count" value=96
send wallpaper wallpaper.props.set path="paused" value=true
send wallpaper wallpaper.props.describe path="fps_limit"
send wallpaper wallpaper.status
print($result)
quit()
```

From an admitted mesh node, use the same registered caller and address the
desktop node, for example `send wallpaper.alpha.bus wallpaper.props.set
path="preset" value="ember"` (on one line).

A successful write returns `{path,old,new,persisted:true}`. Invalid paths,
types, ranges and failed writes return RC 10 without changing live preferences.
The file is staged and synced before atomic replacement; failed staging leaves
the previous complete file intact. Scene geometry, pointer samples and bird
positions are never saved.

Subscribe to the native `wallpaper.props.changed` topic before calling
`wallpaper.props.watch`. Watch accepts an optional valid preference path and
returns `{version:1,topic,instance,event_seq,scope:"",reconcile_ms:1000}`.
The shared topic always invalidates the whole preference tree; it does not
create a per-caller subscription. Notifications have the same envelope, with
an opaque process instance and a sequence that advances after a changed value
has been saved. Successful set replies include that instance and sequence.
No-op or rejected writes do not advance the sequence. Multiple writes can
coalesce into one notification; a full outbound queue retains one pending
invalidation for retry, and reconnect invalidates subscribers again.

Notifications are non-retained, best-effort hints. Re-read with `.get` after
watching, after a notification or reconnect, and every second to recover from
a missed final event. A new process instance resets the sequence. Do not
reconstruct preferences by counting events or treating the notification as a
snapshot. These counters and the instance are never persisted.

The default file is `$XDG_STATE_HOME/cosmix/wallpaper.json`, falling back to
`$HOME/.local/state/cosmix/wallpaper.json`. An absolute
`COSMIX_WALLPAPER_STATE_FILE` overrides it. The versioned JSON file contains
user preferences. Invalid or unsupported files are left intact and reported
in `wallpaper.status` under `persistence_error`; startup uses defaults until
the user supplies a valid preference write.

Status reports scene readiness, effective pause state, pointer readiness,
Bus counters, preferences and persistence errors. A property write being
accepted does not prove that a frame has been displayed: rendering remains
subject to the compositor's callbacks and output activity.

`metrics` contains process-lifetime `host_updates`, `submitted_frames`,
`failed_frames` and `simulation_steps` counters, plus cumulative
`update_wall_ns`, `max_update_wall_ns` and `simulation_wall_ns` timings.
Frame counts count each output submission; simulation steps sum across outputs.
Use counter differences over a measured interval for rates. Update timing
covers `app.update`, including renderer waits; simulation timing covers the
flock advance call. These are wall durations, not CPU measurements. Status
observes completed host updates, so the update answering that request is not
yet included. Counters reset when the wallpaper process restarts.

`bus.queues.channels` reports each bridge channel's current `depth`, bounded
`capacity` (or null for an unbounded channel), and process-lifetime
`sampled_peak`. `bus.queues.latest_topics` reports the current number of
coalesced topics and its sampled peak. Samples are taken around service updates
and on status requests; they can miss short bursts and are not atomic across
channels. They exclude worker-local pending requests and socket buffers.
`bus.dropped_messages` counts drops reported by the bridge, separately from
intentional latest-topic replacement. Peaks and drop counts reset on restart.

`scene.covered_outputs` lists outputs whose logical area is completely excluded
by window and panel bounds. `metrics.suspended_outputs` is the current number
of output surfaces suspended after a successful clear frame, not a cumulative
counter. Coverage can precede suspension while that frame is pending.

## Rendering and input

Quoin's right-hand **System** page includes background controls for all nine
preferences. Click a value to toggle it or cycle through common choices; the
seed control advances to a new seed. Mix can set any value within the advertised
ranges. Controls are disabled while a request is pending or wallpaper is
unavailable, and hidden pages cannot activate them. Values come from service
readback after saving. The existing Quoin Bus connection performs periodic
reconciliation and bounded request timeouts through a one-shot host deadline,
including while the panel is hidden.

The Background role has an empty input region, no keyboard interactivity and
no reserved workspace area. Birds use a bounded fixed-step simulation and
shared flat unlit mesh/material assets. Colour, density and speed changes do not grant
the background input focus. Rate changes retain the pending frame callback,
so they cannot force rendering through an occluded output.

When window and panel bounds cover an entire output, wallpaper hides its birds
and submits one cleared frame before suspending that output's simulation and
presentation. This follows the same geometric avoidance policy as ordinary
windows; it does not assume their pixels are opaque. Overlapping and tiled
rectangles count towards coverage, while narrow uncovered gaps remain active.
Other outputs continue animating. Bus reconciliation remains awake, so moving
or minimising a window resumes the exposed output without simulation catch-up.
Reconfiguration or replacement requires a fresh clear frame. Explicit pause,
disable and lock handling retain priority. Diagnostic capture keeps the required
frames available until its readback completes.

Closing one output's background role retires only that surface, after the
renderer has released its handles. Other outputs and Bus controls keep running.
A revoked role is not recreated on the same Wayland output object; unplugging
and reconnecting an output supplies a new identity and allows a new background,
even when its connector name is reused. Restarting wallpaper also clears these
process-local revocations.

The background host dispatches its main and render ECS schedules serially.
Small scenes otherwise spend substantial CPU time coordinating worker threads
between short systems. Asset work and explicitly parallel render preparation
can still use Bevy task pools; drawing remains GPU-backed. The extraction
schedule retains its deferred-command barrier before render preparation.
Bird visibility and transforms are marked changed only when their values change,
avoiding redundant propagation during stable visibility and control-only wakes.

Pointer interaction requires comp's pointer observation and a noded build
that stamps the owning service on reserved topic deliveries. The consumer
rejects unauthenticated, obsolete-instance and out-of-bounds samples; an
expired pointer cannot continue repelling the flock. Window geometry is
reconciled from authoritative snapshots, not inferred from application titles.

`--capture /absolute/path.png` explicitly captures this application's own
first output after 20 rendered frames. It does not capture other applications.
`--version` prints the app version.

Live Quoin control verification, daily session installation and broader desktop
acceptance testing are still being completed. This manual describes the implemented
client interfaces; it is not a claim that every deployment gate has passed.
