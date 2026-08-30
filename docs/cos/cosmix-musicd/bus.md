# cosmix-musicd Bus service

## Name

`musicd` — Bus service for MIDI rendering and playback, or for the 32-channel mixer.

## Modes

`cosmix-musicd serve` and `cosmix-musicd mixer-serve` both register as `musicd`. They are alternative service modes and must not run together on one broker.

`serve` provides MIDI render, playback, SoundFont, status, and read-only property verbs.

`mixer-serve` provides revisioned mixer properties, transport control, snapshots, and real-time telemetry.

Both modes require the `cosmix` Cargo feature.

## Main service verbs

Commands use the `musicd.` prefix. Request fields are supplied as the Bus argument object.

### musicd.render

Renders a MIDI file without requiring an output device.

Arguments:

| Field | Required | Meaning |
|---|---:|---|
| `midi_path` | Yes | Input Standard MIDI File path. A leading `~/` is expanded. |
| `out_path` | No | Relative output name below the daemon render directory. |
| `sample_rate` | No | Output sample rate in hertz. |
| `gain` | No | Output gain multiplier. |
| `soundfont` | No | SoundFont name relative to the daemon state directory. |
| `channels` | No | `stereo`, `mono`, `left`, or `right`; defaults to `stereo`. |
| `format` | No | `wav16`, `wav24`, `wavf32`, or `flac24`; defaults to `wav16`. |

The daemon confines `out_path` below `state_dir/renders`. Absolute paths, parent traversal, and empty names are rejected.

A successful response contains:

```json
{
  "rendered": true,
  "out_path": "rendered output path",
  "duration_s": 12.5,
  "frames": 551250,
  "sample_rate": 44100,
  "peak": 0.8,
  "clipped": false
}
```

### musicd.play

Starts MIDI playback through the default audio output device.

Arguments:

| Field | Required | Meaning |
|---|---:|---|
| `midi_path` | Yes | Input Standard MIDI File path. |
| `gain` | No | Playback gain multiplier. |
| `soundfont` | No | SoundFont name relative to the daemon state directory. |

The request fails when no output device is available. A successful response reports `playing`, duration, sample rate, and MIDI path.

### musicd.stop

Stops current playback. The response reports `stopped: true` and whether playback had been active.

### musicd.status

Returns SoundFont state, device availability, playback state and position, render and play counters, uptime, and sample rate.

### musicd.soundfonts

Lists `.sf2` files directly inside the daemon state directory. The response identifies the configured default, the loaded bank, and each available file's name, path, and byte count.

### musicd.load_soundfont

Loads a SoundFont already present in the daemon state directory.

Arguments:

| Field | Required | Meaning |
|---|---:|---|
| `name` | No | Relative SoundFont name. |
| `path` | No | Relative SoundFont path; takes precedence over `name`. |

When both fields are absent, the configured SoundFont name is used. Absolute paths and traversal outside the state directory are rejected.

## Main service properties

The main service implements:

- `musicd.props.get`
- `musicd.props.list`
- `musicd.props.describe`
- `musicd.props.watch`

The property tree contains:

| Group | Leaves |
|---|---|
| `config` | `state_dir`, `soundfont_name`, `soundfont_url`, `sample_rate`, `max_polyphony` |
| `lifecycle` | `started_at`, `uptime_s`, `health`, `props_level`, `soundfont_loaded`, `soundfont_error`, `device_available` |
| `playback` | `playing`, `current_midi`, `position_s`, `duration_s`, `renders_total`, `plays_total` |

`lifecycle.uptime_s` and `playback.position_s` are transient.

`musicd.props.watch` returns the `musicd.props.changed` topic. The daemon publishes a retained full snapshot on `world.musicd`. A 1 Hz loop publishes changes for non-transient leaves.

## Mixer service verbs

`mixer-serve` accepts:

- `musicd.props.get`
- `musicd.props.list`
- `musicd.props.describe`
- `musicd.props.watch`
- `musicd.props.set`
- `musicd.mixer.snapshot`

### musicd.props.set

The request body is a mixer write with these fields:

```json
{
  "path": "mixer.channels.0.fader",
  "value": -6.0,
  "op_id": "operation-1",
  "if_revision": null
}
```

Writes require a broker-verified sender and a non-empty `op_id`.

The service validates type, mutability, enum membership, and finite numeric values. Numeric values are clamped and quantised to the leaf schema. `if_revision`, when present, provides optimistic concurrency.

Return codes are:

| Code | Meaning |
|---:|---|
| `0` | Accepted and assigned a control revision. |
| `4` | Busy; the real-time control ring is full and the caller may retry. |
| `10` | Rejected for authentication, validation, or revision mismatch. |

An accepted reply contains the revision, path, canonical value, authenticated source identity, and operation identifier. DSP application is reported later on the applied topic.

Writing `false` to a `meter.clip` leaf resets that meter's latched clip state. Writing `transport.position` performs a revisioned seek and is clamped to the finite session length when one exists.

### musicd.mixer.snapshot

Returns a revisioned bootstrap snapshot of seeded mixer leaves plus run-integrity state. It includes whether real audio is active and whether audio or applied-event faults have occurred.

Clients subscribe before reading the snapshot, then discard change events whose revision is not newer than the snapshot revision.

## Mixer property tree

There are 32 channel strips numbered `0` through `31`.

Each `mixer.channels.N` group contains:

- `trim`
- `fader`
- `pan`
- `mute`
- `solo`
- `name`
- `meter.rms_l`
- `meter.rms_r`
- `meter.peak_l`
- `meter.peak_r`
- `meter.hold_l`
- `meter.hold_r`
- `meter.clip`

The master group contains `fader`, `mute`, the six meter level leaves, and `meter.clip`.

The remaining leaves are:

- `transport.state`
- `transport.position`
- `transport.length`
- `mixer.song.title`
- `mixer.song.artist`
- `mixer.song.copyright`
- `mixer.schema_version`
- `mixer.engine`
- `mixer.source_profile`
- `mixer.benchmark_eligible`

`transport.state` accepts `stopped` or `playing`. Pan uses `-1` for left and `+1` for right. Meter levels use dBFS; trim and fader values use dB.

Use `musicd.props.describe` for each leaf's mutability, range, default, enum values, unit, and transient status.

## Mixer topics

`musicd.props.watch` returns all mixer topic names and the snapshot verb.

| Topic | Content |
|---|---|
| `musicd.mixer.changed` | Coalesced per-path control changes, including revisions. |
| `musicd.mixer.applied` | DSP latch events containing `revision` and `sample_frame`. |
| `musicd.mixer.meters` | Base64-encoded 465-byte meter frames at 60 Hz. |

Meter publication is latest-wins and non-retained. Applied events preserve the real-time latch frame for accepted revisions.

## Configuration

`serve --config <path>` reads the named `.conf.mix` file.

Without an explicit path, `serve` checks `/etc/cosmix/musicd/config.conf.mix`, then the user-mode `musicd` service configuration.

The settings consumed by this crate are:

| Setting | Purpose |
|---|---|
| `state_dir` | Holds the active SoundFont and the `renders` output directory. |
| `soundfont_name` | Default SoundFont filename within `state_dir`. |
| `soundfont_url` | Source used for first-run download. |
| `soundfont_sha256` | Expected SHA-256 digest for download verification. |
| `sample_rate` | Default render and playback sample rate. |
| `gain` | Default render and playback gain. |
| `max_polyphony` | Maximum synthesiser voice count. |

The crate does not define a separate configuration file for `mixer-serve`; its source profile is selected by command-line options.

## SoundFont initialisation

At startup, `serve` ensures `state_dir/soundfont_name` exists and matches the configured SHA-256 digest. A matching file is reused.

A missing or mismatched file is fetched to a temporary `.new` path, checked while streaming, and atomically renamed after verification. Failed downloads remove the partial file.

Fetching and parsing run outside the broker registration path. The service remains available in degraded state when initialisation fails, while render and play requests report the SoundFont error.

## Stem-session source

`mixer-serve --stems <path>` preloads a stem session before starting the audio thread.

Version 1 is JSON with schema `stem-session.v1`. It declares a 48 kHz frame length and a set of channel, WAV path, SHA-256, and optional display-name entries.

Version 2 is a strict-data `.mix` document with schema `stem-session.v2`. It adds optional non-destructive regions containing timeline start, source start, length, gain, fade-in, and fade-out values.

Relative WAV paths resolve against the session file. Missing channels remain silent. Duplicate or out-of-range channels, hash mismatches, non-mono or non-48 kHz WAV files, and stems longer than the declared session are rejected.

