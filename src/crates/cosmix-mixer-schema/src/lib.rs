//! `mixer.v1` — the domain contract for the display-renderer mixer bake-off.
//!
//! This crate is the durable, renderer-independent artifact the bake-off ADR
//! (`_decisions/2026-07-14-display-renderer-bakeoff.md`, §8) names as the
//! keystone: it outlives whichever renderer wins.
//!
//! **Consumers:** `musicd` (the engine), `cosmix-mixer-bench` (the declarer),
//! `cosmix-benchd`, and legitimate domain agents. **It MUST NOT be a dependency
//! of any `cosmix-disp-*` renderer** — B1/Q1: renderers know only the generic
//! `ui.*` schema and must never link or hard-code `mixer.v1`. Renderer final
//! state is collected generically and mapped back to domain paths by the
//! declarer/benchd before [`final_state_hash`].
//!
//! Owns the whole frozen `mixer.v1` contract: the complete property-leaf schema
//! ([`leaf_spec`]) + calibration, the fixed-width **465-byte A.6 meter frame**
//! codec (explicit little-endian, `magic_ver`-checked — §8 Q8, NOT postcard),
//! centi-dBFS conversion, the validating final-state hash over the 163 mutable
//! non-transient leaves, and the write wire. The DSP algorithm and the
//! source-frequency profile (`SEMI[]`) live in the engine; the calibration
//! *values* are pinned here.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The schema tag carried at `mixer/schema_version`.
pub const SCHEMA_VERSION: &str = "mixer.v1";

/// 32 mono input strips.
pub const NUM_CHANNELS: usize = 32;
/// One meter record per input strip plus the stereo master = 33.
pub const NUM_METERS: usize = NUM_CHANNELS + 1;

// --------------------------------------------------------------------------
// Frozen calibration + ranges (A.8 / A.1) — the authoritative definitions.
// --------------------------------------------------------------------------

/// dB value denoting exact silence / `-inf`. It is the **fader/master lower
/// bound and the meter default**; the engine hard-mutes at the floor (§8 Q8).
/// It is NOT a control default — trim/fader/pan all default to `0.0` (A.8).
pub const SILENCE_DB: f64 = -120.0;

/// Trim range and quantum.
pub const TRIM_MIN_DB: f64 = -18.0;
pub const TRIM_MAX_DB: f64 = 18.0;
/// Fader (channel + master) range.
pub const FADER_MIN_DB: f64 = SILENCE_DB;
pub const FADER_MAX_DB: f64 = 6.0;
/// dB control quantum (trim, fader).
pub const DB_QUANTUM: f64 = 0.1;
/// Pan range and quantum (equal-power). Pinned at 1/512 (A.1).
pub const PAN_MIN: f64 = -1.0;
pub const PAN_MAX: f64 = 1.0;
pub const PAN_QUANTUM: f64 = 1.0 / 512.0;
/// Meter level range and on-demand-snapshot quantum.
pub const METER_MIN_DB: f64 = SILENCE_DB;
pub const METER_MAX_DB: f64 = 24.0;
pub const METER_QUANTUM: f64 = 0.01;

/// Per-source headroom applied to the seeded multitone generators (A.8).
pub const SRC_HEADROOM: f64 = 0.25;
/// Master output pad, dB (A.8).
pub const MASTER_PAD_DB: f64 = -12.0;

// --------------------------------------------------------------------------
// Level codec — i16 centi-dBFS (A.6): `round(dB * 100)`, clamped.
// --------------------------------------------------------------------------

/// Lowest encodable level (`-120.00 dB`).
pub const CENTI_DB_MIN: i16 = -12_000;
/// Highest encodable level (`+24.00 dB`).
pub const CENTI_DB_MAX: i16 = 2_400;

/// Encode a dB level to i16 centi-dBFS with clamping (A.6). `NaN` maps to the floor.
pub fn to_centi_dbfs(db: f64) -> i16 {
    if db.is_nan() {
        return CENTI_DB_MIN;
    }
    let c = (db * 100.0).round();
    if c < CENTI_DB_MIN as f64 {
        CENTI_DB_MIN
    } else if c > CENTI_DB_MAX as f64 {
        CENTI_DB_MAX
    } else {
        c as i16
    }
}

/// Decode i16 centi-dBFS back to dB.
pub fn from_centi_dbfs(c: i16) -> f64 {
    c as f64 / 100.0
}

// --------------------------------------------------------------------------
// A.6 meter frame — header 36 + body (33 × 13) = 465 bytes, explicit LE.
// --------------------------------------------------------------------------

/// Bytes per meter record: six i16 levels (12) + one clip byte (1).
pub const METER_RECORD_LEN: usize = 13;
/// Header bytes: magic_ver(4) seq(4) capture_frame(8) applied_rev(8) frame0_mono(8) flags(4).
pub const METER_HEADER_LEN: usize = 36;
/// Total fixed frame length. Asserted `== 465` at compile time and in tests.
pub const METER_FRAME_LEN: usize = METER_HEADER_LEN + NUM_METERS * METER_RECORD_LEN;

const _: () = assert!(METER_FRAME_LEN == 465);
const _: () = assert!(METER_HEADER_LEN == 36);
const _: () = assert!(METER_RECORD_LEN == 13);

/// Frame magic + version. `b"MX1"` little-endian in the low three bytes, version
/// `0x01` in the high byte. Encoders always write this; decoders MUST verify it.
pub const METER_MAGIC_VER: u32 = u32::from_le_bytes([b'M', b'X', b'1', 0x01]);

/// `flags` bit 0: frame produced by the scaffold **simulator**, never the real
/// DSP engine (m18 guard; a ranked chart may never contain simulator frames).
pub const FLAG_SIMULATOR: u32 = 0x0000_0001;
/// `flags` bit 1: frame produced with a **non-benchmark source profile** — the
/// `stem-session.v1` musical profile, never the frozen deterministic multitone
/// benchmark. Set on **every** frame while the stem profile is active. benchd
/// admits a run to a ranked chart only when it uses the benchmark profile
/// (`mixer.source_profile == "benchmark-multitone.v1"`), reads
/// `mixer.benchmark_eligible == true`, and carries **neither** [`FLAG_SIMULATOR`]
/// nor this bit on any frame — so a musical run can never be mistaken for a
/// benchmark run.
pub const FLAG_NON_BENCH_SOURCE: u32 = 0x0000_0002;
/// Reserved `flags` bits (all but the defined flag bits).
pub const FLAG_RESERVED_MASK: u32 = !(FLAG_SIMULATOR | FLAG_NON_BENCH_SOURCE);
/// Valid clip bits: bit 0 = left clipped, bit 1 = right clipped.
pub const CLIP_VALID_MASK: u8 = 0b0000_0011;

/// One stereo meter record. Wire order is A.6: `rms_l, rms_r, peak_l, peak_r,
/// hold_l, hold_r, clip`. [`Default`] is **silence** (all levels at the floor),
/// not `0 dB`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MeterRecord {
    pub rms_l: i16,
    pub rms_r: i16,
    pub peak_l: i16,
    pub peak_r: i16,
    pub hold_l: i16,
    pub hold_r: i16,
    /// Bit 0 = left clipped, bit 1 = right clipped (latched; `meter/clip=false` resets).
    pub clip: u8,
}

impl Default for MeterRecord {
    fn default() -> Self {
        MeterRecord {
            rms_l: CENTI_DB_MIN,
            rms_r: CENTI_DB_MIN,
            peak_l: CENTI_DB_MIN,
            peak_r: CENTI_DB_MIN,
            hold_l: CENTI_DB_MIN,
            hold_r: CENTI_DB_MIN,
            clip: 0,
        }
    }
}

/// The batched 60 Hz meter frame (A.6). Records `[0..32]` are input strips,
/// record `[32]` is the stereo master. `magic_ver` is not a field — it is always
/// [`METER_MAGIC_VER`] on the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MeterFrame {
    pub seq: u32,
    /// Absolute, continuously-advancing audio-frame clock at meter capture (NOT
    /// transport/source position; not constrained to any multiple).
    pub capture_frame: u64,
    /// Highest control revision actually latched by RT and reflected in this frame.
    pub applied_rev: u64,
    /// `CLOCK_MONOTONIC` nanoseconds of audio-frame 0 — lets a consumer convert
    /// `capture_frame`/`applied_frame` to wall time (benchd input→DSP correlation).
    pub frame0_mono: u64,
    pub flags: u32,
    pub records: [MeterRecord; NUM_METERS],
}

/// Meter-frame decode failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    BadLength(usize),
    BadMagic(u32),
    ReservedClipBits { record: usize, clip: u8 },
    LevelOutOfRange { record: usize },
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::BadLength(n) => {
                write!(f, "meter frame must be {METER_FRAME_LEN} bytes, got {n}")
            }
            DecodeError::BadMagic(got) => {
                write!(
                    f,
                    "bad meter-frame magic_ver: {got:#010x} != {METER_MAGIC_VER:#010x}"
                )
            }
            DecodeError::ReservedClipBits { record, clip } => {
                write!(f, "record {record}: reserved clip bits set ({clip:#04x})")
            }
            DecodeError::LevelOutOfRange { record } => {
                write!(
                    f,
                    "record {record}: a level is outside [{CENTI_DB_MIN},{CENTI_DB_MAX}]"
                )
            }
        }
    }
}
impl std::error::Error for DecodeError {}

impl MeterFrame {
    /// Encode to the fixed 465-byte little-endian frame. Always writes the
    /// canonical `magic_ver`.
    pub fn encode(&self) -> [u8; METER_FRAME_LEN] {
        let mut b = [0u8; METER_FRAME_LEN];
        b[0..4].copy_from_slice(&METER_MAGIC_VER.to_le_bytes());
        b[4..8].copy_from_slice(&self.seq.to_le_bytes());
        b[8..16].copy_from_slice(&self.capture_frame.to_le_bytes());
        b[16..24].copy_from_slice(&self.applied_rev.to_le_bytes());
        b[24..32].copy_from_slice(&self.frame0_mono.to_le_bytes());
        b[32..36].copy_from_slice(&self.flags.to_le_bytes());
        let mut o = METER_HEADER_LEN;
        for r in &self.records {
            b[o..o + 2].copy_from_slice(&r.rms_l.to_le_bytes());
            b[o + 2..o + 4].copy_from_slice(&r.rms_r.to_le_bytes());
            b[o + 4..o + 6].copy_from_slice(&r.peak_l.to_le_bytes());
            b[o + 6..o + 8].copy_from_slice(&r.peak_r.to_le_bytes());
            b[o + 8..o + 10].copy_from_slice(&r.hold_l.to_le_bytes());
            b[o + 10..o + 12].copy_from_slice(&r.hold_r.to_le_bytes());
            b[o + 12] = r.clip;
            o += METER_RECORD_LEN;
        }
        b
    }

    /// Decode a fixed 465-byte frame, verifying magic, clip bits and level range.
    pub fn decode(b: &[u8; METER_FRAME_LEN]) -> Result<MeterFrame, DecodeError> {
        let rd_u32 = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let rd_u64 = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        let rd_i16 = |o: usize| i16::from_le_bytes(b[o..o + 2].try_into().unwrap());

        let magic_ver = rd_u32(0);
        if magic_ver != METER_MAGIC_VER {
            return Err(DecodeError::BadMagic(magic_ver));
        }
        let mut records = [MeterRecord::default(); NUM_METERS];
        let mut o = METER_HEADER_LEN;
        for (i, r) in records.iter_mut().enumerate() {
            r.rms_l = rd_i16(o);
            r.rms_r = rd_i16(o + 2);
            r.peak_l = rd_i16(o + 4);
            r.peak_r = rd_i16(o + 6);
            r.hold_l = rd_i16(o + 8);
            r.hold_r = rd_i16(o + 10);
            r.clip = b[o + 12];
            if r.clip & !CLIP_VALID_MASK != 0 {
                return Err(DecodeError::ReservedClipBits {
                    record: i,
                    clip: r.clip,
                });
            }
            for lvl in [r.rms_l, r.rms_r, r.peak_l, r.peak_r, r.hold_l, r.hold_r] {
                if !(CENTI_DB_MIN..=CENTI_DB_MAX).contains(&lvl) {
                    return Err(DecodeError::LevelOutOfRange { record: i });
                }
            }
            o += METER_RECORD_LEN;
        }
        Ok(MeterFrame {
            seq: rd_u32(4),
            capture_frame: rd_u64(8),
            applied_rev: rd_u64(16),
            frame0_mono: rd_u64(24),
            flags: rd_u32(32),
            records,
        })
    }

    /// True if produced by the scaffold simulator (m18 guard).
    pub fn is_simulator(&self) -> bool {
        self.flags & FLAG_SIMULATOR != 0
    }
}

impl TryFrom<&[u8]> for MeterFrame {
    type Error = DecodeError;
    fn try_from(s: &[u8]) -> Result<Self, DecodeError> {
        let arr: &[u8; METER_FRAME_LEN] =
            s.try_into().map_err(|_| DecodeError::BadLength(s.len()))?;
        MeterFrame::decode(arr)
    }
}

// --------------------------------------------------------------------------
// The complete mixer.v1 leaf schema (A.1) + the validating hash (A.7).
// --------------------------------------------------------------------------

/// The five mutable non-transient control leaves per input channel.
pub const CHANNEL_CONTROL_LEAVES: [&str; 5] = ["trim", "fader", "pan", "mute", "solo"];
/// The two mutable non-transient master control leaves.
pub const MASTER_CONTROL_LEAVES: [&str; 2] = ["fader", "mute"];
/// The allowed `transport.state` enum values.
pub const TRANSPORT_STATES: [&str; 2] = ["stopped", "playing"];
/// The allowed `mixer.engine` enum values (`dsp` = real engine, `simulator` = scaffold).
/// Both source profiles run the real DSP engine, so `mixer.engine` is `"dsp"` for
/// both — the benchmark/musical split is carried by `mixer.source_profile`.
pub const ENGINE_VALUES: [&str; 2] = ["dsp", "simulator"];
/// The `benchmark-multitone.v1` source-profile id: the frozen deterministic
/// seeded-multitone benchmark (byte-identical output; the ONLY benchmark-eligible
/// profile).
pub const SOURCE_PROFILE_BENCHMARK: &str = "benchmark-multitone.v1";
/// The `stem-session.v1` source-profile id: preloaded per-channel musical stems
/// indexed by the transport clock. Real DSP, but NON-benchmark.
pub const SOURCE_PROFILE_STEM: &str = "stem-session.v1";
/// The `midi-synth.v1` source-profile id: per-track rustysynth SoundFont voices
/// driven by a frame-keyed note schedule (the sequencer lane — see
/// `~/.cmctl/_doc/2026-07-18-midi-sequencer-design.md`). Real DSP, but
/// NON-benchmark.
pub const SOURCE_PROFILE_MIDI_SYNTH: &str = "midi-synth.v1";
/// The allowed `mixer.source_profile` enum values (read-only; reports the active
/// profile chosen once at startup).
pub const SOURCE_PROFILE_VALUES: [&str; 3] = [
    SOURCE_PROFILE_BENCHMARK,
    SOURCE_PROFILE_STEM,
    SOURCE_PROFILE_MIDI_SYNTH,
];
/// The six per-side meter level leaves.
pub const METER_LEVEL_LEAVES: [&str; 6] = [
    "meter.rms_l",
    "meter.rms_r",
    "meter.peak_l",
    "meter.peak_r",
    "meter.hold_l",
    "meter.hold_r",
];

/// The 163 mutable, non-transient leaves the final-state hash covers, in fixed
/// path order: `32 × 5` channel leaves, then `master.{fader,mute}`, then
/// `transport.state`. `transport.position` is excluded (M11).
pub fn hash_leaves() -> Vec<String> {
    let mut v = Vec::with_capacity(163);
    for ch in 0..NUM_CHANNELS {
        for leaf in CHANNEL_CONTROL_LEAVES {
            v.push(format!("mixer.channels.{ch}.{leaf}"));
        }
    }
    for leaf in MASTER_CONTROL_LEAVES {
        v.push(format!("mixer.master.{leaf}"));
    }
    v.push("transport.state".to_string());
    v
}

/// The declared type class of a leaf.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeafType {
    Number,
    Bool,
    Enum,
    /// Free text (channel names, schema tag); read-only, not part of the hash.
    Text,
}

/// A canonical scalar leaf value. Serialises as a bare JSON scalar
/// (number/bool/string) — never serde's default tagged-enum form. `Enum` and
/// text share the `String` payload; the leaf's [`LeafType`] disambiguates.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LeafValue {
    Number(f64),
    Bool(bool),
    Enum(String),
}

/// The frozen spec of one leaf: type, range, quantum (Number), and whether it is
/// writable (`mutable`) and excluded from snapshots (`transient`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LeafSpec {
    pub ty: LeafType,
    pub min: f64,
    pub max: f64,
    pub quantum: f64,
    pub mutable: bool,
    pub transient: bool,
}

const fn num(min: f64, max: f64, quantum: f64, mutable: bool, transient: bool) -> LeafSpec {
    LeafSpec {
        ty: LeafType::Number,
        min,
        max,
        quantum,
        mutable,
        transient,
    }
}
const fn boolean(mutable: bool, transient: bool) -> LeafSpec {
    LeafSpec {
        ty: LeafType::Bool,
        min: 0.0,
        max: 0.0,
        quantum: 0.0,
        mutable,
        transient,
    }
}

/// Split `mixer.channels.{id}.{rest}` with `id < 32`, else `None`.
fn channel_leaf(path: &str) -> Option<(usize, &str)> {
    let rest = path.strip_prefix("mixer.channels.")?;
    let (id_s, leaf) = rest.split_once('.')?;
    let id: usize = id_s.parse().ok()?;
    // Reject non-canonical ids ("01", "+1", " 0") so there is one path per channel.
    if id >= NUM_CHANNELS || id.to_string() != id_s {
        return None;
    }
    Some((id, leaf))
}

/// The full frozen spec for any valid `mixer.v1` leaf, validating exact topology
/// (`mixer.channels.{0..31}.…`, `mixer.master.…`, `transport.…`, `mixer.…`).
/// Unknown paths, out-of-range channel ids, and wrong leaves (e.g.
/// `mixer.master.solo`) return `None`.
pub fn leaf_spec(path: &str) -> Option<LeafSpec> {
    if let Some((_, leaf)) = channel_leaf(path) {
        return match leaf {
            "trim" => Some(num(TRIM_MIN_DB, TRIM_MAX_DB, DB_QUANTUM, true, false)),
            "fader" => Some(num(FADER_MIN_DB, FADER_MAX_DB, DB_QUANTUM, true, false)),
            "pan" => Some(num(PAN_MIN, PAN_MAX, PAN_QUANTUM, true, false)),
            "mute" | "solo" => Some(boolean(true, false)),
            "name" => Some(LeafSpec {
                ty: LeafType::Text,
                min: 0.0,
                max: 0.0,
                quantum: 0.0,
                mutable: false,
                transient: false,
            }),
            "meter.clip" => Some(boolean(true, true)),
            m if METER_LEVEL_LEAVES.contains(&m) => {
                Some(num(METER_MIN_DB, METER_MAX_DB, METER_QUANTUM, false, true))
            }
            _ => None,
        };
    }
    match path {
        "mixer.master.fader" => Some(num(FADER_MIN_DB, FADER_MAX_DB, DB_QUANTUM, true, false)),
        "mixer.master.mute" => Some(boolean(true, false)),
        "mixer.master.meter.clip" => Some(boolean(true, true)),
        p if p
            .strip_prefix("mixer.master.")
            .is_some_and(|m| METER_LEVEL_LEAVES.contains(&m)) =>
        {
            Some(num(METER_MIN_DB, METER_MAX_DB, METER_QUANTUM, false, true))
        }
        "transport.state" => Some(LeafSpec {
            ty: LeafType::Enum,
            min: 0.0,
            max: 0.0,
            quantum: 0.0,
            mutable: true,
            transient: false,
        }),
        "transport.position" => Some(num(0.0, f64::MAX, 0.0, true, true)),
        // Read-only total transport length in seconds (0 = unbounded). The GUI
        // scrubber's extent; never part of the run hash.
        "transport.length" => Some(num(0.0, f64::MAX, 0.0, false, false)),
        // Read-only session song metadata (the GUI footer). Free text, seeded
        // from the stem manifest; empty when absent.
        "mixer.song.title" | "mixer.song.artist" | "mixer.song.copyright" => Some(LeafSpec {
            ty: LeafType::Text,
            min: 0.0,
            max: 0.0,
            quantum: 0.0,
            mutable: false,
            transient: false,
        }),
        "mixer.schema_version" => Some(LeafSpec {
            ty: LeafType::Text,
            min: 0.0,
            max: 0.0,
            quantum: 0.0,
            mutable: false,
            transient: false,
        }),
        "mixer.engine" => Some(LeafSpec {
            ty: LeafType::Enum,
            min: 0.0,
            max: 0.0,
            quantum: 0.0,
            mutable: false,
            transient: false,
        }),
        // Read-only run-identity leaves: the active source profile + whether the
        // run is benchmark-eligible. Both immutable at runtime (never writable).
        "mixer.source_profile" => Some(LeafSpec {
            ty: LeafType::Enum,
            min: 0.0,
            max: 0.0,
            quantum: 0.0,
            mutable: false,
            transient: false,
        }),
        "mixer.benchmark_eligible" => Some(boolean(false, false)),
        _ => None,
    }
}

/// The canonicalisation quantum for a `Number` leaf (0 for non-Number).
pub fn quantum(path: &str) -> f64 {
    leaf_spec(path).map(|s| s.quantum).unwrap_or(0.0)
}

/// Validate a value against its leaf's frozen spec (type/range/quantum/enum),
/// **regardless of mutability** — used for hashing and read-back. A `Text` leaf
/// carries its string as [`LeafValue::Enum`].
pub fn validate_value(path: &str, v: &LeafValue) -> Result<(), String> {
    let spec = leaf_spec(path).ok_or_else(|| format!("unknown mixer.v1 leaf {path}"))?;
    match (spec.ty, v) {
        (LeafType::Number, LeafValue::Number(n)) => {
            if !n.is_finite() {
                return Err(format!("{path}: non-finite value"));
            }
            if *n < spec.min || *n > spec.max {
                return Err(format!(
                    "{path}: {n} out of range [{}, {}]",
                    spec.min, spec.max
                ));
            }
            if spec.quantum > 0.0 {
                let steps = (n / spec.quantum).round();
                if (steps * spec.quantum - n).abs() > spec.quantum * 1e-6 {
                    return Err(format!(
                        "{path}: {n} not aligned to quantum {}",
                        spec.quantum
                    ));
                }
            }
            Ok(())
        }
        (LeafType::Bool, LeafValue::Bool(_)) => Ok(()),
        (LeafType::Enum, LeafValue::Enum(s)) => {
            let allowed =
                leaf_enum_values(path).ok_or_else(|| format!("{path}: no enum domain"))?;
            if allowed.contains(&s.as_str()) {
                Ok(())
            } else {
                Err(format!("{path}: invalid enum {s:?}"))
            }
        }
        (LeafType::Text, LeafValue::Enum(_)) => Ok(()),
        (ty, got) => Err(format!("{path}: expected {ty:?}, got {got:?}")),
    }
}

/// Validate a **write**: the leaf must be `mutable`; `meter.clip` accepts only
/// `false` (latch reset, never a client-set clip); then [`validate_value`].
pub fn validate_write(path: &str, v: &LeafValue) -> Result<(), String> {
    let spec = leaf_spec(path).ok_or_else(|| format!("unknown mixer.v1 leaf {path}"))?;
    if !spec.mutable {
        return Err(format!("{path}: read-only leaf, not writable"));
    }
    if path.ends_with("meter.clip") && v != &LeafValue::Bool(false) {
        return Err(format!(
            "{path}: meter.clip write must be false (latch reset)"
        ));
    }
    validate_value(path, v)
}

/// **Canonicalise a write** (Q8/A.5 "canonical value"): the domain layer's single
/// entry point for accepting a control write. It performs the same *structural*
/// rejections as [`validate_write`] — unknown path, non-mutable (read-only) leaf,
/// wrong type, non-finite number, a `meter.clip` set to anything but `false`, an
/// out-of-domain enum — but then, for an in-type value, it makes the value **valid
/// by construction** rather than rejecting an out-of-range one:
///
/// - **Clamp** a `Number` to its leaf's `[min, max]` (so `fader = 999` → `+6 dB`,
///   `fader = -999` → the `-120 dB` silence floor — a clamp, **not** a rejection).
/// - **Quantise** to the leaf's quantum (dB grid, pan 1/512), but only when the
///   input is genuinely off-grid — an already-aligned value (e.g. `-6.0 dB`) is
///   left byte-identical so the ack echoes a clean float, not a `…0003` artefact.
/// - **Normalise** `-0.0` to `0.0`.
///
/// The returned [`LeafValue`] therefore satisfies [`validate_value`] by
/// construction; the daemon stores and acks *this* canonical value (which may
/// differ from the request), and the client adopts it (correctness prerequisite
/// for the final-state hash — H10). Rejection is reserved for wrong-type /
/// non-mutable / unknown-path / non-finite / bad-enum / `meter.clip = true`.
pub fn canonicalize_write(path: &str, v: &LeafValue) -> Result<LeafValue, String> {
    let spec = leaf_spec(path).ok_or_else(|| format!("unknown mixer.v1 leaf {path}"))?;
    if !spec.mutable {
        return Err(format!("{path}: read-only leaf, not writable"));
    }
    match (spec.ty, v) {
        (LeafType::Number, LeafValue::Number(n)) => {
            if !n.is_finite() {
                return Err(format!("{path}: non-finite value"));
            }
            // 1. Clamp into range (out-of-range is clamped, never rejected).
            let mut x = n.clamp(spec.min, spec.max);
            // 2. Quantise to the leaf grid, but leave an already-aligned input
            //    untouched so a clean float (e.g. -6.0) does not acquire a
            //    float-rounding tail. Re-clamp after snapping (maxima are on the
            //    grid, so this only guards the pathological case).
            if spec.quantum > 0.0 {
                let snapped = (x / spec.quantum).round() * spec.quantum;
                if (snapped - x).abs() > spec.quantum * 1e-6 {
                    x = snapped.clamp(spec.min, spec.max);
                }
            }
            // 3. Normalise negative zero.
            if x == 0.0 {
                x = 0.0;
            }
            Ok(LeafValue::Number(x))
        }
        (LeafType::Bool, LeafValue::Bool(b)) => {
            if path.ends_with("meter.clip") && *b {
                return Err(format!(
                    "{path}: meter.clip write must be false (latch reset)"
                ));
            }
            Ok(LeafValue::Bool(*b))
        }
        (LeafType::Enum, LeafValue::Enum(s)) => {
            let allowed =
                leaf_enum_values(path).ok_or_else(|| format!("{path}: no enum domain"))?;
            if allowed.contains(&s.as_str()) {
                Ok(LeafValue::Enum(s.clone()))
            } else {
                Err(format!("{path}: invalid enum {s:?}"))
            }
        }
        // `Text` leaves are all read-only (caught by `!mutable` above); any other
        // (declared-type, got-value) pairing is a hard type mismatch.
        (ty, got) => Err(format!("{path}: expected {ty:?}, got {got:?}")),
    }
}

/// Deterministic canonical text for a *validated* leaf value: `Number` → integer
/// count of quanta; `Bool` → `0`/`1`; `Enum` → the literal.
pub fn canonical_repr(path: &str, v: &LeafValue) -> String {
    match v {
        LeafValue::Bool(b) => (if *b { "1" } else { "0" }).to_string(),
        LeafValue::Enum(s) => s.clone(),
        LeafValue::Number(n) => {
            let q = quantum(path);
            let quantised = (n / q).round() as i64;
            quantised.to_string()
        }
    }
}

/// SHA-256 over the 163 mutable non-transient leaves in fixed order, each as
/// `path\0canonical_repr\n` (A.7). Every leaf is **validated** first, so a
/// wrong-typed or out-of-range value can never hash to a legitimate digest.
/// Errors on any missing or invalid leaf.
pub fn final_state_hash(values: &BTreeMap<String, LeafValue>) -> Result<[u8; 32], String> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for path in hash_leaves() {
        let v = values
            .get(&path)
            .ok_or_else(|| format!("final_state_hash: missing leaf {path}"))?;
        validate_value(&path, v)?;
        h.update(path.as_bytes());
        h.update([0u8]);
        h.update(canonical_repr(&path, v).as_bytes());
        h.update(b"\n");
    }
    Ok(h.finalize().into())
}

/// The allowed enum values for a leaf, for `PropDescribe` generation. `None`
/// for non-enum or unknown paths.
pub fn leaf_enum_values(path: &str) -> Option<&'static [&'static str]> {
    match path {
        "transport.state" => Some(&TRANSPORT_STATES),
        "mixer.engine" => Some(&ENGINE_VALUES),
        "mixer.source_profile" => Some(&SOURCE_PROFILE_VALUES),
        _ => None,
    }
}

/// The frozen default value of any valid `mixer.v1` leaf (A.8), for
/// `PropDescribe` generation and snapshot init. `None` for unknown paths.
/// Controls default to `0.0`/`false`; meter levels to silence; transport to
/// `stopped`/`0`; names to `""` (empty — an unnamed channel shows only its bare
/// track number; musicd seeds named channels from the manifest).
pub fn leaf_default(path: &str) -> Option<LeafValue> {
    leaf_spec(path)?;
    if let Some((_id, leaf)) = channel_leaf(path) {
        return Some(match leaf {
            "trim" | "fader" | "pan" => LeafValue::Number(0.0),
            "mute" | "solo" | "meter.clip" => LeafValue::Bool(false),
            "name" => LeafValue::Enum(String::new()),
            _ => LeafValue::Number(SILENCE_DB), // meter levels
        });
    }
    Some(match path {
        "mixer.master.fader" => LeafValue::Number(0.0),
        "mixer.master.mute" | "mixer.master.meter.clip" => LeafValue::Bool(false),
        "transport.state" => LeafValue::Enum("stopped".to_string()),
        "transport.position" => LeafValue::Number(0.0),
        "transport.length" => LeafValue::Number(0.0),
        // Song metadata leaves default empty (musicd seeds them from the
        // manifest when present).
        "mixer.song.title" | "mixer.song.artist" | "mixer.song.copyright" => {
            LeafValue::Enum(String::new())
        }
        "mixer.schema_version" => LeafValue::Enum(SCHEMA_VERSION.to_string()),
        "mixer.engine" => LeafValue::Enum("dsp".to_string()),
        // Default run identity = the benchmark profile (eligible); the daemon
        // overrides both at startup when `--stems` selects the musical profile.
        "mixer.source_profile" => LeafValue::Enum(SOURCE_PROFILE_BENCHMARK.to_string()),
        "mixer.benchmark_eligible" => LeafValue::Bool(true),
        _ => LeafValue::Number(SILENCE_DB), // master meter levels
    })
}

/// The complete default control state (the 163 hash leaves) per A.8.
pub fn default_state() -> BTreeMap<String, LeafValue> {
    hash_leaves()
        .into_iter()
        .map(|p| {
            let v = leaf_default(&p).expect("hash leaf has a default");
            (p, v)
        })
        .collect()
}

// --------------------------------------------------------------------------
// Write wire (§8 Q8) — stable JSON serialisation.
// --------------------------------------------------------------------------

/// A control write. `if_revision` gates it on an expected revision (optimistic
/// concurrency); `None` = unconditional.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WriteRequest {
    pub path: String,
    pub value: LeafValue,
    pub op_id: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub if_revision: Option<u64>,
}

/// The acknowledgement of an accepted write: the control revision only — the
/// value's RT application is reported later by [`DspApplied`] (Q8).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WriteAck {
    pub revision: u64,
    pub path: String,
    pub canonical_value: LeafValue,
    /// Authenticated source identity of the writer.
    pub source_id: String,
    pub op_id: String,
}

/// A rejected write carries the server's current authoritative revision + value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WriteReject {
    pub path: String,
    pub op_id: String,
    pub current_revision: u64,
    pub current_value: LeafValue,
    pub reason: String,
}

/// A **retryable** backpressure response: the write was *not* applied and the
/// store was *not* mutated — the RT control ring was momentarily full, so the
/// server refused rather than accept a write it could not enqueue to the DSP
/// thread (the "once accepted → enqueued to RT" invariant). Unlike
/// [`WriteReject`] the client should **retry the same write** (its op is still
/// pending), not rebase.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WriteBusy {
    pub path: String,
    pub op_id: String,
    pub reason: String,
}

/// The response to a [`WriteRequest`]. `Busy` is transient backpressure (retry);
/// `Rejected` is a durable refusal (rebase / give up).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum WriteResponse {
    Accepted(WriteAck),
    Rejected(WriteReject),
    Busy(WriteBusy),
}

/// Emitted by the RT thread when a control revision is actually latched into the
/// audio graph, at `sample_frame`. Lets benchd compute input→DSP latency.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DspApplied {
    pub revision: u64,
    pub sample_frame: u64,
}

/// One leaf's canonical state in a [`MixerSnapshotResponse`]: its current value
/// plus the authoritative revision at which that value was last written (0 for a
/// never-written / default-seeded leaf).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LeafSnapshot {
    pub path: String,
    pub value: LeafValue,
    pub revision: u64,
}

/// A revisioned bootstrap snapshot (Q8 reconnect contract): the global monotonic
/// `revision` plus every control leaf's canonical value + per-path revision,
/// captured atomically. A reconnecting client subscribes first, then reads this
/// snapshot, then discards any queued change whose revision is `<= revision` —
/// closing the subscribe/snapshot race so its cache initialises consistently.
///
/// `real_audio` reports whether the engine is driving a real cpal output stream
/// (`true`) or a paced no-device fallback (`false`) — a real-audio bake-off run
/// requires the cpal path, so benchd can gate on this alongside `mixer.engine`.
///
/// The two **sticky run-integrity faults** must be checked *throughout* a run and
/// at completion, not just at startup — a run that trips either is disqualifiable:
/// `audio_fault` = the cpal stream raised a runtime error and real audio was lost
/// mid-run (`real_audio` also drops to `false`); `applied_fault` = the RT
/// applied-revision backlog overflowed, so a `dsp.applied` latch timestamp could
/// no longer be preserved truthfully. Both latch `true` and never clear.
///
/// `source_profile` names the immutable startup source profile
/// ([`SOURCE_PROFILE_BENCHMARK`], [`SOURCE_PROFILE_STEM`], or
/// [`SOURCE_PROFILE_MIDI_SYNTH`]) and
/// `benchmark_eligible` is `true` only for the benchmark profile — the top-level
/// mirror of the `mixer.source_profile` / `mixer.benchmark_eligible` leaves, so
/// benchd can disqualify a musical run without walking the leaf list. A run is
/// benchmark-admissible only when `benchmark_eligible` and no frame carries
/// [`FLAG_SIMULATOR`] or [`FLAG_NON_BENCH_SOURCE`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MixerSnapshotResponse {
    pub revision: u64,
    pub real_audio: bool,
    pub audio_fault: bool,
    pub applied_fault: bool,
    /// The active source profile id (`mixer.source_profile`).
    pub source_profile: String,
    /// `true` only for the benchmark profile (`mixer.benchmark_eligible`).
    pub benchmark_eligible: bool,
    pub leaves: Vec<LeafSnapshot>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_len_is_exactly_465() {
        assert_eq!(METER_FRAME_LEN, 465);
        assert_eq!(METER_HEADER_LEN, 36);
        assert_eq!(METER_RECORD_LEN, 13);
        assert_eq!(NUM_METERS, 33);
    }

    #[test]
    fn meter_record_default_is_silence() {
        let d = MeterRecord::default();
        assert_eq!(d.rms_l, CENTI_DB_MIN);
        assert_eq!(d.peak_r, CENTI_DB_MIN);
        assert_eq!(d.hold_l, CENTI_DB_MIN);
        assert_eq!(d.clip, 0);
    }

    #[test]
    fn encode_len_and_roundtrip() {
        let mut f = MeterFrame {
            seq: 12_345,
            capture_frame: 987_654,
            applied_rev: 10_432,
            frame0_mono: 123_456_789_000,
            flags: FLAG_SIMULATOR,
            records: [MeterRecord::default(); NUM_METERS],
        };
        for (i, r) in f.records.iter_mut().enumerate() {
            r.rms_l = to_centi_dbfs(-(i as f64) - 3.0);
            r.peak_l = to_centi_dbfs(-(i as f64));
            r.hold_l = to_centi_dbfs(1.0);
            r.clip = (i % 4) as u8 & CLIP_VALID_MASK;
        }
        let bytes = f.encode();
        assert_eq!(bytes.len(), 465);
        assert_eq!(MeterFrame::decode(&bytes).expect("decode"), f);
        assert!(f.is_simulator());
        assert_eq!(MeterFrame::try_from(&bytes[..]).expect("tryfrom"), f);
    }

    #[test]
    fn golden_wire_offsets_lock_full_layout() {
        let mut f = MeterFrame {
            seq: 0x1122_3344,
            capture_frame: 0x0102_0304_0506_0708,
            applied_rev: 0x1112_1314_1516_1718,
            frame0_mono: 0x2122_2324_2526_2728,
            flags: 0x0000_0001,
            records: [MeterRecord::default(); NUM_METERS],
        };
        f.records[0] = MeterRecord {
            rms_l: 0x0101,
            rms_r: 0x0202,
            peak_l: 0x0303,
            peak_r: 0x0404,
            hold_l: 0x0505,
            hold_r: 0x0606,
            clip: 0x03,
        };
        let b = f.encode();
        assert_eq!(&b[0..4], &METER_MAGIC_VER.to_le_bytes(), "magic @0");
        assert_eq!(&b[4..8], &0x1122_3344u32.to_le_bytes(), "seq @4");
        assert_eq!(
            &b[8..16],
            &0x0102_0304_0506_0708u64.to_le_bytes(),
            "capture_frame @8"
        );
        assert_eq!(
            &b[16..24],
            &0x1112_1314_1516_1718u64.to_le_bytes(),
            "applied_rev @16"
        );
        assert_eq!(
            &b[24..32],
            &0x2122_2324_2526_2728u64.to_le_bytes(),
            "frame0_mono @24"
        );
        assert_eq!(&b[32..36], &0x0000_0001u32.to_le_bytes(), "flags @32");
        // Record 0 body @36: A.6 order rms,rms,peak,peak,hold,hold,clip.
        assert_eq!(&b[36..38], &0x0101i16.to_le_bytes(), "rms_l @36");
        assert_eq!(&b[38..40], &0x0202i16.to_le_bytes(), "rms_r @38");
        assert_eq!(&b[40..42], &0x0303i16.to_le_bytes(), "peak_l @40");
        assert_eq!(&b[42..44], &0x0404i16.to_le_bytes(), "peak_r @42");
        assert_eq!(&b[44..46], &0x0505i16.to_le_bytes(), "hold_l @44");
        assert_eq!(&b[46..48], &0x0606i16.to_le_bytes(), "hold_r @46");
        assert_eq!(b[48], 0x03, "clip @48");
    }

    #[test]
    fn decode_rejects_bad_magic_length_and_bits() {
        let mut bytes = [0u8; METER_FRAME_LEN];
        bytes[0..4].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        assert_eq!(
            MeterFrame::decode(&bytes),
            Err(DecodeError::BadMagic(0xDEAD_BEEF))
        );

        assert_eq!(
            MeterFrame::try_from(&[0u8; 10][..]),
            Err(DecodeError::BadLength(10))
        );

        let mut good = MeterFrame {
            seq: 0,
            capture_frame: 0,
            applied_rev: 0,
            frame0_mono: 0,
            flags: 0,
            records: [MeterRecord::default(); NUM_METERS],
        };
        good.records[5].clip = 0x80;
        assert_eq!(
            MeterFrame::decode(&good.encode()),
            Err(DecodeError::ReservedClipBits {
                record: 5,
                clip: 0x80
            })
        );
    }

    #[test]
    fn centi_dbfs_clamps_rounds_and_handles_nan() {
        assert_eq!(to_centi_dbfs(0.0), 0);
        assert_eq!(to_centi_dbfs(-6.0), -600);
        assert_eq!(to_centi_dbfs(-999.0), CENTI_DB_MIN);
        assert_eq!(to_centi_dbfs(999.0), CENTI_DB_MAX);
        assert_eq!(to_centi_dbfs(f64::NAN), CENTI_DB_MIN);
        assert!((from_centi_dbfs(to_centi_dbfs(-12.34)) - -12.34).abs() < 0.005);
    }

    #[test]
    fn leaf_spec_validates_exact_topology() {
        assert!(leaf_spec("mixer.channels.0.trim").is_some());
        assert!(leaf_spec("mixer.channels.31.solo").is_some());
        assert!(
            leaf_spec("mixer.channels.17.meter.rms_l")
                .unwrap()
                .transient
        );
        assert!(leaf_spec("mixer.channels.17.meter.clip").unwrap().mutable);
        assert!(leaf_spec("mixer.master.fader").is_some());
        assert!(leaf_spec("transport.position").unwrap().transient);
        // Rejected topology:
        assert!(
            leaf_spec("mixer.channels.99.fader").is_none(),
            "ch id >= 32"
        );
        assert!(
            leaf_spec("mixer.master.solo").is_none(),
            "master has no solo"
        );
        assert!(leaf_spec("bogus.trim").is_none());
        assert!(leaf_spec("mixer.channels.x.trim").is_none());
    }

    #[test]
    fn hash_leaves_are_163_in_order() {
        let leaves = hash_leaves();
        assert_eq!(leaves.len(), 163);
        assert_eq!(leaves[0], "mixer.channels.0.trim");
        assert_eq!(leaves[159], "mixer.channels.31.solo");
        assert_eq!(leaves[160], "mixer.master.fader");
        assert_eq!(leaves[162], "transport.state");
    }

    #[test]
    fn validation_rejects_wrong_type_range_and_alignment() {
        assert!(validate_value("mixer.channels.0.trim", &LeafValue::Bool(false)).is_err());
        assert!(validate_value("mixer.channels.0.trim", &LeafValue::Number(99.0)).is_err());
        assert!(validate_value("mixer.channels.0.trim", &LeafValue::Number(1.05)).is_err());
        assert!(validate_value("mixer.channels.0.fader", &LeafValue::Number(f64::NAN)).is_err());
        assert!(validate_value("transport.state", &LeafValue::Enum("spinning".into())).is_err());
        assert!(validate_value("mixer.channels.99.fader", &LeafValue::Number(0.0)).is_err());
        assert!(validate_value("mixer.channels.0.pan", &LeafValue::Number(64.0 / 512.0)).is_ok());
        assert!(validate_value("mixer.channels.0.pan", &LeafValue::Number(0.001)).is_err());
        assert!(validate_value("mixer.channels.0.fader", &LeafValue::Number(-6.0)).is_ok());
        assert!(validate_value("transport.state", &LeafValue::Enum("playing".into())).is_ok());
    }

    #[test]
    fn validate_write_requires_mutable_and_clip_reset() {
        // Read-only leaves reject writes but validate as values (read-back).
        assert!(validate_write("mixer.channels.0.meter.rms_l", &LeafValue::Number(-12.0)).is_err());
        assert!(validate_value("mixer.channels.0.meter.rms_l", &LeafValue::Number(-12.0)).is_ok());
        assert!(validate_write("mixer.engine", &LeafValue::Enum("dsp".into())).is_err());
        assert!(validate_write("mixer.channels.0.name", &LeafValue::Enum("x".into())).is_err());
        assert!(
            validate_write("mixer.schema_version", &LeafValue::Enum("mixer.v1".into())).is_err()
        );
        // meter.clip: only false (latch reset) may be written.
        assert!(validate_write("mixer.channels.0.meter.clip", &LeafValue::Bool(true)).is_err());
        assert!(validate_write("mixer.channels.0.meter.clip", &LeafValue::Bool(false)).is_ok());
        // Control leaves are writable.
        assert!(validate_write("mixer.channels.0.fader", &LeafValue::Number(-6.0)).is_ok());
    }

    #[test]
    fn leaf_default_and_enum_metadata() {
        assert_eq!(
            leaf_default("mixer.channels.0.fader"),
            Some(LeafValue::Number(0.0))
        );
        assert_eq!(
            leaf_default("mixer.channels.0.meter.rms_l"),
            Some(LeafValue::Number(SILENCE_DB))
        );
        assert_eq!(
            leaf_default("mixer.channels.4.name"),
            Some(LeafValue::Enum(String::new()))
        );
        assert_eq!(
            leaf_default("mixer.master.meter.peak_l"),
            Some(LeafValue::Number(SILENCE_DB))
        );
        assert_eq!(
            leaf_default("transport.state"),
            Some(LeafValue::Enum("stopped".into()))
        );
        assert_eq!(
            leaf_default("mixer.schema_version"),
            Some(LeafValue::Enum("mixer.v1".into()))
        );
        assert_eq!(leaf_default("bogus"), None);
        // Song metadata: read-only text leaves, default empty.
        for p in [
            "mixer.song.title",
            "mixer.song.artist",
            "mixer.song.copyright",
        ] {
            let spec = leaf_spec(p).unwrap_or_else(|| panic!("missing spec {p}"));
            assert_eq!(spec.ty, LeafType::Text, "{p} is text");
            assert!(!spec.mutable, "{p} is read-only");
            assert!(!spec.transient, "{p} is snapshot-persistent");
            assert_eq!(
                leaf_default(p),
                Some(LeafValue::Enum(String::new())),
                "{p} default empty"
            );
        }
        assert!(
            leaf_spec("mixer.song.bogus").is_none(),
            "unknown song leaf rejected"
        );
        assert_eq!(
            leaf_enum_values("transport.state"),
            Some(&TRANSPORT_STATES[..])
        );
        assert_eq!(leaf_enum_values("mixer.engine"), Some(&ENGINE_VALUES[..]));
        assert_eq!(leaf_enum_values("mixer.channels.0.fader"), None);
    }

    #[test]
    fn channel_ids_reject_aliases() {
        assert!(leaf_spec("mixer.channels.0.trim").is_some());
        assert!(
            leaf_spec("mixer.channels.01.trim").is_none(),
            "leading-zero alias"
        );
        assert!(leaf_spec("mixer.channels.00.trim").is_none());
    }

    #[test]
    fn final_state_hash_golden_and_sensitive() {
        let mut m = default_state();
        // Sanity: A.8 default faders are 0.0 dB, not silence.
        assert_eq!(m["mixer.channels.0.fader"], LeafValue::Number(0.0));
        assert_eq!(m["mixer.master.fader"], LeafValue::Number(0.0));

        let h1 = final_state_hash(&m).expect("complete default");
        let golden = "5d1c673b6b74e69dac41bd9a855d915d8346280fb7160f239dabfc00e15ea200";
        assert_eq!(hex(&h1), golden, "default-state digest drifted");

        m.insert(
            "mixer.channels.17.fader".to_string(),
            LeafValue::Number(-6.0),
        );
        assert_ne!(
            final_state_hash(&m).expect("ok"),
            h1,
            "a changed leaf changes the hash"
        );
    }

    #[test]
    fn final_state_hash_errors_on_missing_or_invalid() {
        assert!(final_state_hash(&BTreeMap::new()).is_err());
        let mut m = default_state();
        m.insert(
            "mixer.channels.0.trim".to_string(),
            LeafValue::Number(999.0),
        );
        assert!(final_state_hash(&m).is_err());
    }

    #[test]
    fn write_wire_golden_json() {
        assert_eq!(
            serde_json::to_string(&LeafValue::Number(-6.0)).unwrap(),
            "-6.0"
        );
        assert_eq!(
            serde_json::to_string(&LeafValue::Bool(false)).unwrap(),
            "false"
        );
        assert_eq!(
            serde_json::to_string(&LeafValue::Enum("stopped".into())).unwrap(),
            "\"stopped\""
        );

        let req = WriteRequest {
            path: "mixer.channels.17.fader".into(),
            value: LeafValue::Number(-6.0),
            op_id: "op-1".into(),
            if_revision: Some(42),
        };
        let jreq = serde_json::to_string(&req).unwrap();
        assert_eq!(
            jreq,
            r#"{"path":"mixer.channels.17.fader","value":-6.0,"op_id":"op-1","if_revision":42}"#
        );
        assert_eq!(serde_json::from_str::<WriteRequest>(&jreq).unwrap(), req);

        let acc = WriteResponse::Accepted(WriteAck {
            revision: 43,
            path: "mixer.channels.17.fader".into(),
            canonical_value: LeafValue::Number(-6.0),
            source_id: "bench".into(),
            op_id: "op-1".into(),
        });
        let jacc = serde_json::to_string(&acc).unwrap();
        assert_eq!(
            jacc,
            r#"{"status":"accepted","revision":43,"path":"mixer.channels.17.fader","canonical_value":-6.0,"source_id":"bench","op_id":"op-1"}"#
        );
        assert_eq!(serde_json::from_str::<WriteResponse>(&jacc).unwrap(), acc);

        let rej = WriteResponse::Rejected(WriteReject {
            path: "mixer.channels.17.fader".into(),
            op_id: "op-1".into(),
            current_revision: 44,
            current_value: LeafValue::Number(0.0),
            reason: "if_revision mismatch".into(),
        });
        let jrej = serde_json::to_string(&rej).unwrap();
        assert_eq!(
            jrej,
            r#"{"status":"rejected","path":"mixer.channels.17.fader","op_id":"op-1","current_revision":44,"current_value":0.0,"reason":"if_revision mismatch"}"#
        );
        assert_eq!(serde_json::from_str::<WriteResponse>(&jrej).unwrap(), rej);

        let dsp = DspApplied {
            revision: 43,
            sample_frame: 96_000,
        };
        let jdsp = serde_json::to_string(&dsp).unwrap();
        assert_eq!(jdsp, r#"{"revision":43,"sample_frame":96000}"#);
        assert_eq!(serde_json::from_str::<DspApplied>(&jdsp).unwrap(), dsp);
    }

    fn hex(b: &[u8; 32]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn num(v: &LeafValue) -> f64 {
        match v {
            LeafValue::Number(n) => *n,
            _ => panic!("expected Number, got {v:?}"),
        }
    }

    #[test]
    fn canonicalize_clamps_out_of_range_instead_of_rejecting() {
        // Over the max → clamp to +6 dB (clean float, not a rounding tail).
        let c = canonicalize_write("mixer.channels.0.fader", &LeafValue::Number(999.0)).unwrap();
        assert_eq!(c, LeafValue::Number(6.0));
        // Under the min → clamp to the -120 dB silence floor.
        let c = canonicalize_write("mixer.master.fader", &LeafValue::Number(-999.0)).unwrap();
        assert_eq!(c, LeafValue::Number(SILENCE_DB));
        // Pan out of range → clamp to +1.0 (1/512-grid, exact).
        let c = canonicalize_write("mixer.channels.3.pan", &LeafValue::Number(5.0)).unwrap();
        assert_eq!(c, LeafValue::Number(1.0));
    }

    #[test]
    fn canonicalize_quantises_off_grid_but_preserves_aligned() {
        // Already aligned → byte-identical (no float tail).
        assert_eq!(
            canonicalize_write("mixer.channels.0.fader", &LeafValue::Number(-6.0)).unwrap(),
            LeafValue::Number(-6.0)
        );
        // Off-grid trim → snapped to the nearest 0.1 dB step (canonical_repr 11).
        let c = canonicalize_write("mixer.channels.0.trim", &LeafValue::Number(1.05)).unwrap();
        assert!((num(&c) - 1.1).abs() < 1e-9, "1.05 → 1.1, got {}", num(&c));
        assert_eq!(canonical_repr("mixer.channels.0.trim", &c), "11");
        // Off-grid pan → snapped to the 1/512 grid.
        let c = canonicalize_write("mixer.channels.0.pan", &LeafValue::Number(0.001)).unwrap();
        assert!(validate_value("mixer.channels.0.pan", &c).is_ok());
    }

    #[test]
    fn canonicalize_still_rejects_structural_errors() {
        // Read-only / non-mutable leaves.
        assert!(
            canonicalize_write("mixer.channels.0.meter.rms_l", &LeafValue::Number(-12.0)).is_err()
        );
        assert!(canonicalize_write("mixer.engine", &LeafValue::Enum("dsp".into())).is_err());
        assert!(canonicalize_write("mixer.channels.0.name", &LeafValue::Enum("x".into())).is_err());
        // Unknown path.
        assert!(canonicalize_write("bogus.leaf", &LeafValue::Number(0.0)).is_err());
        // Wrong type.
        assert!(canonicalize_write("mixer.channels.0.fader", &LeafValue::Bool(true)).is_err());
        // Non-finite.
        assert!(
            canonicalize_write("mixer.channels.0.fader", &LeafValue::Number(f64::NAN)).is_err()
        );
        // meter.clip = true (only false resets).
        assert!(canonicalize_write("mixer.master.meter.clip", &LeafValue::Bool(true)).is_err());
        assert!(canonicalize_write("mixer.master.meter.clip", &LeafValue::Bool(false)).is_ok());
        // Bad enum.
        assert!(
            canonicalize_write("transport.state", &LeafValue::Enum("spinning".into())).is_err()
        );
        assert!(canonicalize_write("transport.state", &LeafValue::Enum("playing".into())).is_ok());
    }

    #[test]
    fn busy_and_snapshot_wire_golden_json() {
        let busy = WriteResponse::Busy(WriteBusy {
            path: "mixer.channels.1.fader".into(),
            op_id: "op-9".into(),
            reason: "control ring full".into(),
        });
        let j = serde_json::to_string(&busy).unwrap();
        assert_eq!(
            j,
            r#"{"status":"busy","path":"mixer.channels.1.fader","op_id":"op-9","reason":"control ring full"}"#
        );
        assert_eq!(serde_json::from_str::<WriteResponse>(&j).unwrap(), busy);

        let snap = MixerSnapshotResponse {
            revision: 7,
            real_audio: true,
            audio_fault: false,
            applied_fault: false,
            source_profile: SOURCE_PROFILE_BENCHMARK.into(),
            benchmark_eligible: true,
            leaves: vec![LeafSnapshot {
                path: "mixer.master.fader".into(),
                value: LeafValue::Number(-3.0),
                revision: 4,
            }],
        };
        let j = serde_json::to_string(&snap).unwrap();
        assert_eq!(
            j,
            r#"{"revision":7,"real_audio":true,"audio_fault":false,"applied_fault":false,"source_profile":"benchmark-multitone.v1","benchmark_eligible":true,"leaves":[{"path":"mixer.master.fader","value":-3.0,"revision":4}]}"#
        );
        assert_eq!(
            serde_json::from_str::<MixerSnapshotResponse>(&j).unwrap(),
            snap
        );
    }

    #[test]
    fn non_bench_source_flag_is_distinct_bit() {
        // A distinct flag bit, disjoint from the simulator bit.
        assert_eq!(FLAG_NON_BENCH_SOURCE, 0x0000_0002);
        assert_eq!(FLAG_SIMULATOR & FLAG_NON_BENCH_SOURCE, 0);
        // Both defined bits are excluded from the reserved mask.
        assert_eq!(
            FLAG_RESERVED_MASK & (FLAG_SIMULATOR | FLAG_NON_BENCH_SOURCE),
            0
        );
        // A frame carrying the non-bench flag still encodes + decodes cleanly.
        let f = MeterFrame {
            seq: 1,
            capture_frame: 800,
            applied_rev: 0,
            frame0_mono: 0,
            flags: FLAG_NON_BENCH_SOURCE,
            records: [MeterRecord::default(); NUM_METERS],
        };
        let decoded = MeterFrame::decode(&f.encode()).unwrap();
        assert_eq!(decoded.flags, FLAG_NON_BENCH_SOURCE);
        assert!(!decoded.is_simulator());
    }

    #[test]
    fn source_profile_and_eligible_leaves_are_readonly() {
        // Both new run-identity leaves exist, are read-only, and non-transient.
        for path in ["mixer.source_profile", "mixer.benchmark_eligible"] {
            let spec = leaf_spec(path).unwrap_or_else(|| panic!("{path} has a spec"));
            assert!(!spec.mutable, "{path} must be read-only");
            assert!(!spec.transient, "{path} must be non-transient");
            // Read-only ⇒ any write is refused.
            let v = leaf_default(path).unwrap();
            assert!(
                validate_write(path, &v).is_err(),
                "{path} must reject writes"
            );
        }
        // source_profile is an enum over exactly the two profile ids.
        assert_eq!(
            leaf_enum_values("mixer.source_profile"),
            Some(&SOURCE_PROFILE_VALUES[..])
        );
        assert!(
            validate_value(
                "mixer.source_profile",
                &LeafValue::Enum(SOURCE_PROFILE_STEM.into())
            )
            .is_ok()
        );
        assert!(validate_value("mixer.source_profile", &LeafValue::Enum("bogus".into())).is_err());
        // Defaults describe a benchmark-eligible run.
        assert_eq!(
            leaf_default("mixer.source_profile"),
            Some(LeafValue::Enum(SOURCE_PROFILE_BENCHMARK.into()))
        );
        assert_eq!(
            leaf_default("mixer.benchmark_eligible"),
            Some(LeafValue::Bool(true))
        );
        // The new read-only leaves are NOT part of the 163-leaf final-state hash.
        assert!(!hash_leaves().iter().any(|p| p == "mixer.source_profile"));
        assert!(
            !hash_leaves()
                .iter()
                .any(|p| p == "mixer.benchmark_eligible")
        );
    }
}
