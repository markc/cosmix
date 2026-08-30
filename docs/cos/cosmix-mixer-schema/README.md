# cosmix-mixer-schema

`cosmix-mixer-schema` is the renderer-independent Rust contract for the
`mixer.v1` domain. It defines the mixer property schema, frozen calibration,
meter-frame codec, write messages, snapshots, and final-state hash used by
mixer engines and domain-aware clients. The crate lives in the `cos` layer of
the `bus <- mix <- cos` dependency chain, but deliberately has no dependency on
Bus, Mix, or another Cosmix crate. Generic `cosmix-disp-*` renderers must not
depend on it.

## Synopsis

The Cargo package is named `cosmix-mixer-schema`. Rust code imports it as
`cosmix_mixer_schema`.

The crate exposes all items from its library root. It has no public submodules.

## Domain constants

`SCHEMA_VERSION` is `mixer.v1`. The topology contains 32 mono input channels
and one stereo master, so meter frames carry 33 `MeterRecord` values.

| Constant | Value | Meaning |
| --- | ---: | --- |
| `NUM_CHANNELS` | 32 | Mono input strips |
| `NUM_METERS` | 33 | Input meters plus the master |
| `SILENCE_DB` | -120.0 dB | Exact silence and fader floor |
| `TRIM_MIN_DB` / `TRIM_MAX_DB` | -18.0 / 18.0 dB | Trim range |
| `FADER_MIN_DB` / `FADER_MAX_DB` | -120.0 / 6.0 dB | Channel and master fader range |
| `DB_QUANTUM` | 0.1 dB | Trim and fader step |
| `PAN_MIN` / `PAN_MAX` | -1.0 / 1.0 | Pan range |
| `PAN_QUANTUM` | 1/512 | Pan step |
| `METER_MIN_DB` / `METER_MAX_DB` | -120.0 / 24.0 dB | Meter range |
| `METER_QUANTUM` | 0.01 dB | Snapshot meter step |
| `SRC_HEADROOM` | 0.25 | Seeded source headroom |
| `MASTER_PAD_DB` | -12.0 dB | Master output pad |

Control defaults are `0.0` for trim, fader, and pan, and `false` for boolean
controls. The `-120.0 dB` silence value is the fader floor and meter default,
not the default fader position.

## Property schema

`leaf_spec(path)` returns a `LeafSpec` for an exact valid path. It rejects
unknown leaves, non-canonical channel identifiers, and channel identifiers
outside `0..31`.

`LeafSpec` reports the `LeafType`, numeric range, quantum, mutability, and
transience of a leaf. `LeafType` distinguishes `Number`, `Bool`, `Enum`, and
read-only `Text` values.

`LeafValue` is an untagged Serde scalar. Its `Number(f64)`, `Bool(bool)`, and
`Enum(String)` variants serialise as bare JSON numbers, booleans, and strings.
The string variant also carries text leaves.

The channel surface is:

| Path suffix | Type | Mutable | Transient |
| --- | --- | --- | --- |
| `trim` | Number | yes | no |
| `fader` | Number | yes | no |
| `pan` | Number | yes | no |
| `mute` | Bool | yes | no |
| `solo` | Bool | yes | no |
| `name` | Text | no | no |
| `meter.rms_l`, `meter.rms_r` | Number | no | yes |
| `meter.peak_l`, `meter.peak_r` | Number | no | yes |
| `meter.hold_l`, `meter.hold_r` | Number | no | yes |
| `meter.clip` | Bool | reset only | yes |

Channel paths take the form `mixer.channels.{id}.{suffix}`. Channel identifiers
use canonical decimal notation: for example, `0` is valid and `00` is not.

The master exposes `fader`, `mute`, the six meter levels, and `meter.clip`
under `mixer.master`. It does not expose trim, pan, solo, or name leaves.

Other valid leaves are:

| Path | Type | Mutable | Transient |
| --- | --- | --- | --- |
| `transport.state` | Enum | yes | no |
| `transport.position` | Number | yes | yes |
| `transport.length` | Number | no | no |
| `mixer.song.title` | Text | no | no |
| `mixer.song.artist` | Text | no | no |
| `mixer.song.copyright` | Text | no | no |
| `mixer.schema_version` | Text | no | no |
| `mixer.engine` | Enum | no | no |
| `mixer.source_profile` | Enum | no | no |
| `mixer.benchmark_eligible` | Bool | no | no |

`transport.state` accepts `stopped` and `playing`. `mixer.engine` accepts `dsp`
and `simulator`. `mixer.source_profile` accepts
`benchmark-multitone.v1`, `stem-session.v1`, and `midi-synth.v1`.

`leaf_enum_values(path)` returns an enum domain. `leaf_default(path)` returns
the frozen default. `default_state()` constructs the complete default state for
the 163 leaves covered by the final-state hash.

## Validation and canonicalisation

`validate_value(path, value)` checks topology, type, finite numeric values,
range, quantum alignment, and enum membership. It validates read-back values
without considering whether the leaf is writable.

`validate_write(path, value)` additionally requires a mutable leaf.
`meter.clip` is writable only as `false`, which resets the latched clip state.

`canonicalize_write(path, value)` is the accepting path for control writes. It
rejects unknown, read-only, wrongly typed, non-finite, and invalid enum values,
and rejects `meter.clip = true`. It clamps numbers to the leaf range, snaps
off-grid values to the leaf quantum, preserves aligned values, and normalises
negative zero. The result passes `validate_value` and is the value to store and
acknowledge.

`quantum(path)` reports a numeric leaf's quantum, or zero for a non-number or
unknown path. `canonical_repr(path, value)` formats a validated value for
hashing: numbers become integer quantum counts, booleans become `0` or `1`,
and enums remain literal strings.

## Final-state hash

`hash_leaves()` returns the fixed ordered set of 163 mutable, non-transient
leaves: five controls for each of 32 channels, master fader and mute, then
transport state.

`transport.position`, meters, metadata, schema identity, engine identity, and
source-profile identity do not participate.

`final_state_hash(values)` validates every required value and computes SHA-256
over each ordered entry as `path`, a NUL byte, its canonical representation,
and a newline. It fails when a required leaf is missing or invalid and returns
the digest as `[u8; 32]`.

## Meter frame

`MeterFrame` encodes the batched meter stream as a fixed 465-byte
little-endian frame. `MeterRecord` holds six signed centi-dBFS levels and one
clip byte. Records `0..31` belong to input channels; record `32` belongs to the
master.

| Region | Size | Fields |
| --- | ---: | --- |
| Header | 36 bytes | magic/version, sequence, capture frame, applied revision, frame-zero monotonic time, flags |
| Record | 13 bytes | RMS L/R, peak L/R, hold L/R, clip |
| Body | 429 bytes | 33 records |
| Total | 465 bytes | Header plus body |

`MeterFrame::encode()` always writes `METER_MAGIC_VER`.
`MeterFrame::decode()` returns `DecodeError` after verifying the magic/version,
valid clip bits, and every level's range. `TryFrom<&[u8]>` also checks the fixed
frame length.

`FLAG_SIMULATOR` marks simulator output. `FLAG_NON_BENCH_SOURCE` marks a frame
from a non-benchmark source profile. `FLAG_RESERVED_MASK` identifies all other
flag bits. `MeterFrame::is_simulator()` tests the simulator bit.

`to_centi_dbfs(db)` rounds dB to signed centi-dBFS and clamps it to
`-12000..=2400`; `NaN` maps to the floor. `from_centi_dbfs(value)` converts the
integer representation back to dB.

## Write and snapshot wire

The write wire uses stable Serde JSON structures.

`WriteRequest` carries a path, scalar value, operation identifier, and optional
expected revision. An absent `if_revision` makes the request unconditional.

`WriteResponse` is internally tagged by `status` and has three variants:

- `Accepted(WriteAck)` returns the new revision, canonical value, source
  identity, and operation identifier.
- `Rejected(WriteReject)` returns the current revision and value with a durable
  refusal reason.
- `Busy(WriteBusy)` reports transient backpressure; the write is not applied
  and can be retried.

`LeafSnapshot` carries one canonical leaf value and its last-written revision.
`MixerSnapshotResponse` carries an atomic global revision, run and audio health
flags, source-profile identity, benchmark eligibility, and the leaf snapshot.
Its sticky `audio_fault` and `applied_fault` fields report loss of real audio
and loss of trustworthy applied-revision timing. `DspApplied` reports when the
real-time audio graph latched a control revision.

## Cargo surface

The crate defines no Cargo feature flags.

Its runtime dependencies are:

| Dependency | Purpose |
| --- | --- |
| `serde` | JSON-compatible scalar and wire type serialisation |
| `sha2` | SHA-256 final-state hashing |

`serde_json` is used only by the crate's wire-format tests.
