# cosmix-mprisd — MPRIS2 media-player state

**`cosmix-mprisd` is the event-driven MPRIS2 citizen for local media-player
state and delegated playback controls.** It turns the session D-Bus view into
the `mpris.*` Bus namespace, keyed state topics and ordered lifecycle events.

## What it does

- Enumerates session-bus names below `org.mpris.MediaPlayer2.*` and tracks
  `org.freedesktop.DBus.NameOwnerChanged` appearance, disappearance and owner
  turnover.
- Reads the `org.mpris.MediaPlayer2` root properties `Identity` and
  `DesktopEntry`.
- Reads `org.mpris.MediaPlayer2.Player` playback status, metadata, rate,
  volume and control capabilities, and watches its `PropertiesChanged` and
  `Seeked` signals.
- Selects one active player. The current active remains selected while it
  exists in `Playing`, `Paused` or `Stopped`; a player that was already playing
  does not retake active merely because the current active pauses. Only a new
  transition into `Playing` selects another player. When the active vanishes,
  the surviving player with the most recent `Playing` transition wins,
  regardless of its current playback state. Encoded-key (`p_<hex>`) order is
  used only when no surviving player has playing history. If several are
  already playing on first observation, that same order breaks the initial
  tie. Selection and Playing history identify an instance by encoded key plus
  owner epoch, not by key alone. Owner turnover therefore removes the old
  instance and applies the normal fallback before the replacement appears; a
  replacement first observed as `Playing` records a fresh Playing transition
  and can immediately become active.
- Keeps serving when no player exists. The players list is empty and active is
  `null`; no player process or D-Bus activation is required.

The signal matches are installed before the first snapshot. Player reads run
with at most eight in flight. Each completed player is published while slower
reads remain in flight, so one request reaching its 15-second deadline does not
delay quiet peers. This deadline bounds one request/reply exchange; it is not a
polling, retry or scheduling timer. If a request reaches that deadline, the
player is published with `unresponsive=true` and its remaining reads are
skipped; a player whose bounded scan otherwise cannot be constructed is also
published as unresponsive. `players.scan_revision` identifies the logical
scan. It is published with `players.scan_complete=false` before player reads
begin and remains the same through every incremental player publication;
completion publishes the same revision with `scan_complete=true`. Consumers
can therefore distinguish a mixed A-new/B-old player tree from a complete
generation.

Incremental publications affect only per-player property snapshots. Active
selection, most-recently-playing history, and `player.appeared`,
`player.vanished`, and active-change lifecycle edges are derived only when the
scan is complete. A partial map cannot clear or change `active`, and the final
lifecycle diff uses the previous complete scan rather than an incremental map.

The tracker drains the bounded raw signal stream alongside every D-Bus scan
and final owner query, but after 64 consecutive signals it polls the pending
reply first so a signal flood cannot starve scan completion or its request
deadline. No signal is discarded by that scheduling bound. Scan fencing is per
player: a signal from player A during A's read invalidates and retries A only,
while a quiet player B from the same scan can publish. Every retry rechecks all
players already accepted for that batch; an accepted player that raced during
a later retry becomes unresolved and is read again. After three consecutive
races, a player retains its previous complete snapshot with `stale=true`; its
next quiet scan replaces that snapshot and clears the flag. Owner changes still
fence the ownership view. Signals on the MPRIS path/interfaces are accepted
only from currently known MPRIS unique owners; unknown senders are ignored with
episode-limited logging, and departed-owner signal epochs are pruned after
their final events are processed. The full rescan interval is five minutes and
exists only as a missed-signal backstop. Owner-change arguments are retained in
a bounded FIFO, so even an appearance and disappearance completed between
clean scans remains an ordered lifecycle edge. Queue overflow enters the
publisher-loss gap contract below.

MPRIS signals `Position` only through `Seeked`; mprisd has no position timer and
does not poll it. At property-projection time, for either a read or a
publication, the daemon exposes the transient `computed_position_us` estimate
and `position_observation_age_us`, both in microseconds. The latter is the
monotonic age of the raw `Position` scan or `Seeked` basis, not a cross-process
timestamp. To project after receipt, start from `computed_position_us`; while
`playback_status=playing`, add local elapsed monotonic microseconds multiplied
by `rate`, and clamp to zero and `length_us` when present. The five-minute
backstop is a full-snapshot rescan, and refreshing `Position` as part of that
snapshot is not polling. Consumers must treat the result as an estimate, not a
timeline clock.

`PropertiesChanged` is decoded rather than treated only as a rescan wake. A
Player-interface body carrying `PlaybackStatus="Playing"` records a monotonic
Playing observation for that unique owner. The complete scan merges those
observations in signal-observation order into most-recently-playing history,
even if the sampled current status has returned to `Paused`; the sampled read
still determines the current `playback_status`.

## Running it

```sh
/opt/cosmix/bin/cosmix-mprisd serve
```

The supplied `cosmix-mprisd.service` is a systemd user unit because MPRIS and
the Cosmix Bus client belong to the graphical session. The SPEC-10
`cosmix-mprisd` UID/GID row reserves daemon and namespace identity; it is not
the runtime user of this session service.

On graceful shutdown the broker loop first stops admitting commands. The
control worker continues jobs whose absolute deadline has not passed and
returns `org.cosmix.Mpris.Expired` for queued jobs whose deadline has passed.
All command replies, including validation, no-player, busy and admitted-job
replies, run in one supervised set capped at 64 tasks; one slot is reserved so
capacity overflow itself can receive a counted `org.cosmix.Mpris.Busy` reply.
While serving, Bus command intake is drained unconditionally. Overflow beyond
64 in-flight replies is dropped immediately without a task or reply and counted
in `controls.dropped`. The Bus client's command channel is unbounded, so this
daemon intake policy enforces the memory bound. Drop logs appear once when an
episode starts, at most once per 60 seconds while it continues, and once when
admission resumes.
The responder drains for at most 60 seconds inside the unit's 75-second stop
budget. Only after that drain does mprisd close the broker client and await the
control worker; a task still present after the bounded drain is aborted.

## Bus verbs

The service name is `mpris`. Read verbs are:

| Verb | Purpose |
|---|---|
| `mpris.info` | Build identity, player names, active player and cumulative publisher loss |
| `mpris.ping` | Liveness reply (`{"pong":true}`) |
| `mpris.props.get` | Full snapshot, or one path supplied as `args.path` |
| `mpris.props.list` | Current leaf paths |
| `mpris.props.describe` | Schema for one path supplied as `args.path` |
| `mpris.props.watch` | Topic discovery and subscribe-before-read bootstrap instructions |

Control verbs delegate to MPRIS and do not mutate the tracked snapshot. Supply
`args.player` as either the complete well-known name or encoded player key; if
omitted, the active player is used. Admission uses a bounded queue, response
waiting runs outside Bus intake in the bounded supervised responder, and the
control worker checks the well-known name, unique owner, owner epoch and
tracker generation before calling the captured unique owner. If all regular
responder slots are occupied, the reserved slot returns
`org.cosmix.Mpris.Busy`. Once that slot is occupied, intake keeps draining and
drops further commands without reply until a response slot clears.
Admission gives every job one absolute 50-second
deadline. The worker drops a job dequeued after that deadline as
`org.cosmix.Mpris.Expired`. The owner lookup and delegated control call each use
the smaller of 15 seconds and the job's remaining budget; no second full
50-second budget is created. The response waiter ends at 52 seconds from
admission: two seconds after the job deadline so the worker's `Expired` or
`Timeout` verdict wins over a generic response timeout, but still inside
`NodedClient`'s 60-second caller-side outer bound measured before daemon
receipt. Every dispatched timeout carries `executed_by`;
otherwise it is the unique owner that received the call, or `null` when no call
was dispatched. Invalid `offset_us`, `volume` and player selectors return
`ok=false` with `org.freedesktop.DBus.Error.InvalidArgs`.

Owner fencing before dispatch is best-effort. D-Bus cannot make the owner
check and method call atomic; consumers that care about turnover must compare
`executed_by` with the owner they intended to control.

| Verb | MPRIS operation and arguments |
|---|---|
| `mpris.player.play` | `Play()` |
| `mpris.player.pause` | `Pause()` |
| `mpris.player.playpause` | `PlayPause()` |
| `mpris.player.next` | `Next()` |
| `mpris.player.previous` | `Previous()` |
| `mpris.player.stop` | `Stop()` |
| `mpris.player.seek` | `Seek(offset_us)`, requiring integer `args.offset_us` |
| `mpris.player.set_volume` | Set `Volume`, requiring finite non-negative `args.volume` |

## Property namespace

All properties are read-only. Optional metadata leaves are absent when the
player does not provide them.

| Path | Meaning |
|---|---|
| `players.list` | Currently published MPRIS well-known names in stable key order; consult the scan markers for completeness |
| `players.scan_revision` | Daemon-session monotonic logical scan revision; unchanged across its partial and complete publications |
| `players.scan_complete` | `false` while that revision contains mixed incremental player generations; `true` once every player is resolved, unresponsive or explicitly stale |
| `players.by_id.<key>.key` | Stable encoded player key |
| `players.by_id.<key>.name` | Complete MPRIS well-known name |
| `players.by_id.<key>.identity` | Root-interface `Identity` |
| `players.by_id.<key>.desktop_entry` | Optional root-interface `DesktopEntry` |
| `players.by_id.<key>.owner_epoch` | Daemon-session epoch for this name's unique owner |
| `players.by_id.<key>.unresponsive` | The bounded player scan could not complete; a 15-second request deadline skips remaining reads |
| `players.by_id.<key>.stale` | Previous complete snapshot retained after three consecutive per-player scan races |
| `players.by_id.<key>.playback_status` | `playing`, `paused`, `stopped` or `unknown` |
| `players.by_id.<key>.title` | `xesam:title` |
| `players.by_id.<key>.artists` | `xesam:artist` list |
| `players.by_id.<key>.album` | `xesam:album` |
| `players.by_id.<key>.length_us` | `mpris:length` in microseconds |
| `players.by_id.<key>.art_url` | `mpris:artUrl`; mprisd does not fetch the resource |
| `players.by_id.<key>.computed_position_us` | Transient estimated position; not polled |
| `players.by_id.<key>.position_observation_age_us` | Transient age of the position basis |
| `players.by_id.<key>.rate` | Playback rate used by the estimate |
| `players.by_id.<key>.volume` | MPRIS volume, where 1.0 is normal |
| `players.by_id.<key>.can_*` | `CanPlay`, `CanPause`, `CanGoNext`, `CanGoPrevious`, `CanSeek`, `CanControl` |
| `active.name` | Selected well-known name, or `null` |
| `active.key` | Selected encoded key, or `null` |
| `controls.dropped` | Cumulative commands dropped without reply after all 64 response slots were occupied |
| `lifecycle.props_level` | `L2` |
| `lifecycle.event_seq` | Daemon-session monotonic event sequence |
| `lifecycle.publisher_loss` | Cumulative publications lost during this daemon session |

`<key>` is a collision-free lowercase `p_<hex>` encoding of every byte in the
complete well-known name. It fits the SPEC-07 segment grammar and keeps
multiple instances distinct.

## Topics

| Topic | Class | Published for |
|---|---|---|
| `mpris.props.changed` | keyed state | Standard SPEC-07 old/new property deltas, keyed per player path |
| `mpris.player.appeared` | FIFO event | A well-known player name acquired an owner |
| `mpris.player.vanished` | FIFO event | A tracked player name lost its owner |
| `mpris.active.changed` | FIFO event | Active-player selection changed |

Domain-topic payloads contain `event`, `event_seq` and `data`;
`mpris.props.changed` uses the standard `{path,old,new,ts,cause}` shape plus an
`event_seq` header. A `Seeked` signal produces an explicit
`mpris.props.changed` position edge with `cause=mpris.seeked`; ordinary
computed-position passage is transient and produces no traffic. Consumers
subscribe on their Bus connection and then call `mpris.props.get`.
Lifecycle `data` includes `name`, `key`, the unique `owner`, and `player` when
a complete snapshot for that edge was observable. `player` is `null` for a
transient edge completed between scans; consumers should refresh state rather
than treating the event payload as authoritative.

Publication never back-pressures the D-Bus tracker. State topics coalesce
latest-wins by topic and full property path, with at most 1024 paths. A
multi-property `PropertiesChanged` batch therefore retains one update per
changed property without creating loss itself.
`mpris.player.appeared`, `mpris.player.vanished` and `mpris.active.changed`
share a bounded 64-item FIFO and remain ordered while loss-free. This preserves
the vanish → fallback-active → replacement-appearance → newly-Playing-active
edge sequence during active-owner turnover.

Any publication loss—latest-wins replacement, key cap, FIFO overflow, stale
generation, broker send error or timeout—advances the cumulative session loss
counter. The publisher discards its older backlog and emits one
`mpris.props.changed` control frame using interactd's convention: `gap=true`,
`lost_count`, and `cause=publisher.loss`. On a gap, discard projected state and
re-read `mpris.props.get`. Loss logs appear once when an episode starts and at
most once per 60 seconds while it continues; a successful publication ends the
episode.

Every outgoing item carries the tracker generation. The publisher checks it
immediately before its single send await and turns stale work into the same gap
contract. At most one accepted old-generation frame can already be in flight.
If the D-Bus tracker restarts, every dropped pending owner edge, `Seeked` or
Playing observation, and already-counted adapter loss advances cumulative
publisher loss; one gap frame is queued after reconnect and logged through the
same loss-episode limiter. Unique-owner epochs come from one process-global
counter that is monotonic for the process lifetime and saturates at
`u64::MAX`.

## Feature boundary

The default `cosmix` feature builds the zbus adapter and Bus citizen.
`cargo test -p cosmix-mprisd --no-default-features` builds and tests only the
pure snapshot/diff core, without zbus, Tokio or Cosmix transport dependencies.
