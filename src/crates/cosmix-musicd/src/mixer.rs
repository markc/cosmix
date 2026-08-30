//! The narrow 32-channel bake-off mixer engine (D4) + a headless SIMULATOR.
//!
//! This is the `musicd`-owned half of the display-renderer mixer bake-off
//! (`_decisions/2026-07-14-display-renderer-bakeoff.md` §D; harness-design
//! companion appendix D). A deliberately narrow mixer: 48 kHz, fixed 128-frame
//! blocks, 32 mono buses, per-channel trim / equal-power pan / fader / mute /
//! solo, a stereo sum into a master fader + mute, and per-channel + master
//! RMS/peak metering with engine-authoritative peak-hold and a latched clip
//! flag.
//!
//! The 32 sources come from one of two **immutable startup source profiles**
//! ([`SourceProfile`]), chosen once at construction and never mutated — so a
//! musical run can never be mistaken for a benchmark run:
//! - [`SourceProfile::BenchmarkMultitone`] — **deterministic seeded multitone
//!   generators** ([`SEMI`]); no audio file, no RNG, byte-identical output across
//!   runs / renderers / the DSP-vs-simulator split. The ONLY benchmark-eligible
//!   profile.
//! - [`SourceProfile::StemSession`] — preloaded per-channel mono 48 kHz stems
//!   ([`StemBank`]) indexed by the transport clock; musical, and flagged
//!   NON-benchmark ([`FLAG_NON_BENCH_SOURCE`] on every frame).
//!
//! The DSP contract (the 465-byte A.6 [`MeterFrame`], the frozen calibration
//! constants, the leaf schema, the write wire) lives in the renderer-independent
//! keystone crate [`cosmix_mixer_schema`]. This module owns only the *algorithm*
//! and the frozen source-frequency profile ([`SEMI`]).
//!
//! # Scope of this scaffold
//!
//! What ships here is the pure, headless **block processor + meter assembly +
//! deterministic sources**, plus a standalone [`run_simulator`] loop that drives
//! it and prints / writes [`MeterFrame`]s with [`FLAG_SIMULATOR`] set. That is
//! the decision-D7 "now" dry-run backend: it needs no cpal audio device and no
//! Bus broker, so it builds and runs on any headless CI node.
//!
//! Deliberately **left as documented TODO stubs** (owned by later bake-off
//! sessions, harness-design §D3/§D4/§D6/§D9):
//! - the `!Send` cpal RT thread (`"musicd-mixer"`, alloc-free callback, triple
//!   buffer / seqlock telemetry mailbox) — this module's [`MixerEngine`] is the
//!   pure processor that thread would drive;
//! - the revisioned `props.set` write surface + `daemon.rs` routing;
//! - the 60 Hz `tokio::interval` meter publisher (`musicd.mixer.meters`).
//!
//! # RT discipline
//!
//! Mirrors `play.rs`: the hot path ([`MixerEngine::process_block`]) performs **no
//! heap allocation** — meter accumulators are fixed arrays mutated in place, the
//! per-channel fundamentals are precomputed once, and a completed [`MeterFrame`]
//! is a plain-old-data value (a fixed `[MeterRecord; 33]`) pushed into a
//! caller-owned, caller-sized `Vec`.

use std::sync::Arc;

use cosmix_mixer_schema::{
    CLIP_VALID_MASK, FLAG_NON_BENCH_SOURCE, FLAG_SIMULATOR, MASTER_PAD_DB, MeterFrame, MeterRecord,
    NUM_CHANNELS, NUM_METERS, SILENCE_DB, SOURCE_PROFILE_BENCHMARK, SOURCE_PROFILE_MIDI_SYNTH,
    SOURCE_PROFILE_STEM, SRC_HEADROOM, to_centi_dbfs,
};
use rustysynth::{SoundFont, Synthesizer};

/// Engine sample rate (Hz). Frozen for cross-run comparability (harness §D3).
pub const SR: u32 = 48_000;
/// Engine sample rate as `f64`, for the source closed form.
const SR_F: f64 = 48_000.0;
/// Fixed DSP block size in frames (decision D4). The real RT thread chunks a
/// variable cpal buffer into blocks of exactly this many frames.
pub const BLOCK: usize = 128;
/// Samples per meter frame: `48000 / 60`. **Does not divide [`BLOCK`]** — the
/// meter window is accumulated across 128-blocks and cut when the sample counter
/// crosses the next boundary (harness §D3(d)).
pub const METER_PERIOD: u64 = 800;

/// The frozen source-frequency profile: `f0(ch) = 55 · 2^(SEMI[ch]/12)` Hz.
///
/// A fixed, **non-adjacent**, strictly-increasing `[i8; 32]` (minimum gap 2
/// semitones), so every channel has a distinct known fundamental and adjacent
/// channels never sit a semitone apart — which keeps a per-channel Goertzel
/// bandpass at `f0` separable (the input→audible detector, harness §D5). The
/// alternating 2/3-semitone spacing spans ~55 Hz…~4.7 kHz so the 3rd harmonic of
/// even the top channel stays below Nyquist. **Committed to the benchmark
/// profile** — changing it invalidates cross-run comparability.
pub const SEMI: [i8; NUM_CHANNELS] = [
    0, 2, 5, 7, 10, 12, 15, 17, 20, 22, 25, 27, 30, 32, 35, 37, 40, 42, 45, 47, 50, 52, 55, 57, 60,
    62, 65, 67, 70, 72, 75, 77,
];

/// Peak-hold decay applied once per meter frame (~ -0.01 dB/frame @ 60 Hz → a
/// visible ~1 s hold). Engine-authoritative so every consumer shows the same
/// hold regardless of dropped frames (harness §A.6 / H7).
const HOLD_DECAY: f32 = 0.9988;

/// Control-coefficient de-zipper ramp length in samples: on any target change a
/// resolved linear coefficient reaches its new target in EXACTLY this many samples
/// (≈5 ms @48k), **independent of the delta magnitude** — a fixed-DURATION linear
/// ramp, not a fixed per-sample step. (A fixed step made a large boost take longer
/// than a small nudge — the smoothing time then leaked the gain delta, which
/// corrupts input→audible measurement.) Per (channel, coeff) the engine keeps
/// `inc = Δ / RAMP_SAMPLES` and a `remaining` countdown; the coefficient snaps
/// exactly onto the target on the final sample. Linear (harness §D7 permits it;
/// one-pole not required) — reaching the target *exactly* keeps the seeded-source
/// benchmark deterministic and lets mute/solo resolve to true silence. The
/// canonical value still jumps instantly with its revision (Q8/§D7: "applied" =
/// the block-boundary latch of the target); only the audible coefficient trails
/// over this fixed ramp.
const RAMP_SAMPLES: u16 = 240;
const RAMP_SAMPLES_F: f32 = 240.0;

// ---------------------------------------------------------------------------
// Canonical control state (A.1 subset the engine consumes)
// ---------------------------------------------------------------------------

/// One input strip's mutable controls. dB units match the `mixer.v1` leaves.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChannelControl {
    pub trim_db: f64,
    pub fader_db: f64,
    pub pan: f64,
    pub mute: bool,
    pub solo: bool,
}

impl Default for ChannelControl {
    fn default() -> Self {
        // A.8 defaults: unity trim/fader (0 dB), centre pan, unmuted, unsoloed.
        ChannelControl {
            trim_db: 0.0,
            fader_db: 0.0,
            pan: 0.0,
            mute: false,
            solo: false,
        }
    }
}

/// The master strip's controls.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MasterControl {
    pub fader_db: f64,
    pub mute: bool,
}

impl Default for MasterControl {
    fn default() -> Self {
        MasterControl {
            fader_db: 0.0,
            mute: false,
        }
    }
}

/// The full canonical control snapshot the engine latches. `playing` gates
/// source advance (transport stopped ⇒ silence, meters fall to the floor).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Controls {
    pub channels: [ChannelControl; NUM_CHANNELS],
    pub master: MasterControl,
    pub playing: bool,
}

impl Default for Controls {
    fn default() -> Self {
        // transport.state defaults to "stopped" (A.8) → not playing.
        Controls {
            channels: [ChannelControl::default(); NUM_CHANNELS],
            master: MasterControl::default(),
            playing: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Source profiles (chosen once at startup, never mutated)
// ---------------------------------------------------------------------------

/// A preloaded bank of per-channel mono 48 kHz stems for the stem-session source
/// profile. **All** decode / verification / allocation happens at construction
/// (before the cpal stream starts); the RT path only *indexes* the preloaded
/// `Vec<f32>`s, so the audio callback never touches the filesystem, a lock, or
/// the allocator. A channel with no stem is an empty `Vec` = silent.
pub struct StemBank {
    /// Per channel: the preloaded mono f32 samples (empty = a silent channel).
    /// IMMUTABLE after construction — every edit is region metadata; the
    /// source audio is never rewritten (the icedaw non-destructive model).
    /// `Arc` so the edit loop rebuilds banks off-thread by SHARING the audio
    /// (a rebuild moves region metadata, never samples).
    stems: [Arc<Vec<f32>>; NUM_CHANNELS],
    /// Per channel: the regions that place windows of the source on the
    /// timeline. Construction synthesises ONE full-length region per
    /// non-empty stem, so an unedited session plays bit-identically to the
    /// pre-region engine. Kept sorted by `timeline_start`.
    regions: [Vec<Region>; NUM_CHANNELS],
    /// Per channel: (last sampled frame, index of the first region that can
    /// still contribute at/after that frame). Playback is overwhelmingly
    /// sequential, so skipping the contiguous DEAD PREFIX (regions whose
    /// `timeline_end <= frame`) makes the per-frame walk O(1) amortised no
    /// matter how many splits precede the playhead; a backward seek resets
    /// the cursor. Pure cache — never observable in the output.
    cursors: [(u64, usize); NUM_CHANNELS],
    /// The session length from the manifest (all stems padded to this).
    base_length_frames: u64,
    /// The live timeline length: `base_length_frames` or the furthest region
    /// end, whichever is later — an edit that moves audio past the manifest
    /// length extends the transport instead of being clamped unreachable.
    length_frames: u64,
    /// Per-channel instrument name from the stem manifest (None = keep the
    /// default "Ch N"). Surfaced to the GUI via the mixer.channels.N.name leaf.
    names: [Option<String>; NUM_CHANNELS],
    /// Session song metadata from the manifest, surfaced to the GUI footer via
    /// the `mixer.song.*` leaves. Empty when the manifest omits it.
    song: SongMeta,
}

/// Session song metadata (the GUI footer): title + artist + copyright. Read from
/// the stem manifest; empty strings when a field is absent.
#[derive(Clone, Debug, Default)]
pub struct SongMeta {
    pub title: String,
    pub artist: String,
    pub copyright: String,
}

/// One non-destructive region: a window of a channel's immutable source
/// placed on the timeline. All units are frames at [`SR`], timeline and
/// source advancing 1:1 (no stretch in v1). Every edit — trim, move, split,
/// slip — is arithmetic on these fields; the source audio never changes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Region {
    /// First timeline frame the region occupies.
    pub timeline_start: u64,
    /// First source frame the region reads (the "slip" offset).
    pub source_start: u64,
    /// Region length in frames.
    pub len: u64,
    /// Linear gain multiplier applied to the whole region.
    pub gain: f32,
    /// Fade-in length in frames (linear ramp from 0 at the region's first
    /// frame; curve shapes arrive with the editing UI).
    pub fade_in: u64,
    /// Fade-out length in frames (linear ramp to 0 at the region's last frame).
    pub fade_out: u64,
}

impl Region {
    /// The whole source placed at the timeline origin — the region an
    /// unedited stem gets, playing bit-identically to a plain buffer read.
    pub fn full(len: u64) -> Self {
        Region {
            timeline_start: 0,
            source_start: 0,
            len,
            gain: 1.0,
            fade_in: 0,
            fade_out: 0,
        }
    }

    /// One past the region's last timeline frame.
    pub fn timeline_end(&self) -> u64 {
        self.timeline_start.saturating_add(self.len)
    }

    /// This region's contribution at an absolute timeline `frame`, reading
    /// `source` (the channel's immutable buffer). Zero outside the region or
    /// past the source's end. RT-safe: arithmetic + one bounds-checked index.
    #[inline]
    fn sample(&self, source: &[f32], frame: u64) -> f32 {
        if frame < self.timeline_start || frame >= self.timeline_end() {
            return 0.0;
        }
        let pos = frame - self.timeline_start;
        let Some(idx) = self.source_start.checked_add(pos) else {
            return 0.0;
        };
        let Some(&value) = source.get(idx as usize) else {
            return 0.0;
        };
        // Linear fades, multiplied when they overlap (a region shorter than
        // fade_in + fade_out attenuates through both ramps).
        let mut mix = self.gain;
        if self.fade_in > 0 && pos < self.fade_in {
            mix *= pos as f32 / self.fade_in as f32;
        }
        if self.fade_out > 0 {
            let remaining = self.len - 1 - pos;
            if remaining < self.fade_out {
                mix *= remaining as f32 / self.fade_out as f32;
            }
        }
        value * mix
    }
}

impl StemBank {
    /// Assemble a bank from per-channel decoded samples, zero-padding every
    /// non-empty stem to `length_frames` (empty channels stay silent). Panics if
    /// a supplied stem is longer than `length_frames` — the loader must reject
    /// that inconsistency before calling here.
    pub fn new(mut stems: [Vec<f32>; NUM_CHANNELS], length_frames: u64) -> Self {
        let len = length_frames as usize;
        for s in stems.iter_mut() {
            if s.is_empty() {
                continue;
            }
            assert!(
                s.len() <= len,
                "stem ({} frames) longer than length_frames ({len})",
                s.len()
            );
            if s.len() < len {
                s.resize(len, 0.0);
            }
        }
        Self::from_shared(stems.map(Arc::new), length_frames)
    }

    /// Assemble a bank from ALREADY-PADDED shared sources — the edit loop's
    /// rebuild constructor: region metadata changes, the audio `Arc`s are
    /// shared with the previous bank. Panics on an unpadded non-empty source
    /// (rebuilds must come from a bank built by [`StemBank::new`]).
    pub fn from_shared(sources: [Arc<Vec<f32>>; NUM_CHANNELS], length_frames: u64) -> Self {
        for source in &sources {
            assert!(
                source.is_empty() || source.len() == length_frames as usize,
                "shared source ({} frames) not padded to length_frames ({length_frames})",
                source.len()
            );
        }
        let regions = std::array::from_fn(|ch| {
            if sources[ch].is_empty() {
                Vec::new()
            } else {
                vec![Region::full(sources[ch].len() as u64)]
            }
        });
        StemBank {
            stems: sources,
            regions,
            cursors: [(0, 0); NUM_CHANNELS],
            base_length_frames: length_frames,
            length_frames,
            names: std::array::from_fn(|_| None),
            song: SongMeta::default(),
        }
    }

    /// Replace one channel's region list (builder — the editing loop rebuilds
    /// banks off-thread and swaps them in, so none of this runs on the RT
    /// thread). Sanitises the metadata the RT walk will trust: zero-length
    /// regions are dropped, non-finite gains muted, and the list sorted by
    /// `timeline_start`; the timeline length is re-derived so audio moved
    /// past the manifest length stays reachable.
    pub fn with_channel_regions(mut self, ch: usize, mut regions: Vec<Region>) -> Self {
        regions.retain(|region| region.len > 0);
        for region in &mut regions {
            if !region.gain.is_finite() {
                region.gain = 0.0;
            }
        }
        regions.sort_by_key(|region| region.timeline_start);
        self.regions[ch] = regions;
        self.cursors = [(0, 0); NUM_CHANNELS];
        self.length_frames = self
            .regions
            .iter()
            .flatten()
            .map(Region::timeline_end)
            .max()
            .unwrap_or(0)
            .max(self.base_length_frames);
        self
    }

    /// Per-channel region lists (sorted by `timeline_start`).
    pub fn regions(&self) -> &[Vec<Region>; NUM_CHANNELS] {
        &self.regions
    }

    /// Attach per-channel instrument names (builder — keeps `new`'s signature so
    /// existing callers/tests are untouched).
    pub fn with_names(mut self, names: [Option<String>; NUM_CHANNELS]) -> Self {
        self.names = names;
        self
    }

    /// Attach session song metadata (builder — keeps `new`'s signature so
    /// existing callers/tests are untouched).
    pub fn with_song(mut self, song: SongMeta) -> Self {
        self.song = song;
        self
    }

    /// Per-channel instrument names (None = default "Ch N").
    pub fn names(&self) -> &[Option<String>; NUM_CHANNELS] {
        &self.names
    }

    /// Session song metadata (the GUI footer).
    pub fn song(&self) -> &SongMeta {
        &self.song
    }

    /// The logical stem length in frames (all non-empty stems padded to this).
    pub fn length_frames(&self) -> u64 {
        self.length_frames
    }

    /// The number of channels carrying a stem (non-silent).
    pub fn loaded_channels(&self) -> usize {
        self.stems.iter().filter(|s| !s.is_empty()).count()
    }

    /// The decoded per-channel stem buffers (empty = silent channel).
    /// Read-only, `Arc`-shared: waveform displays fold these BEFORE the bank
    /// moves onto the RT thread, and the edit loop clones the `Arc`s to
    /// rebuild banks without copying audio.
    pub fn stems(&self) -> &[Arc<Vec<f32>>; NUM_CHANNELS] {
        &self.stems
    }

    /// Offline: render one channel's region mix over consecutive absolute
    /// frames into `out` — the same additive semantics the RT path plays
    /// (the per-stem export basis; sample-exact against [`Self::sample`]).
    pub fn render_channel_range(&mut self, ch: usize, start_frame: u64, out: &mut [f32]) {
        for (i, sample) in out.iter_mut().enumerate() {
            *sample = self.sample(ch, start_frame + i as u64);
        }
    }

    /// One channel's sample at an absolute transport frame: the additive sum
    /// of every region covering `frame` (overlaps sum — the icedaw model).
    /// RT-safe — arithmetic + bounds-checked indexes over preloaded data; no
    /// alloc, lock, or syscall. The list is sorted by `timeline_start`: the
    /// walk starts past the dead prefix (cursor, O(1) amortised while the
    /// transport is sequential) and stops at the first region starting after
    /// `frame`. An unedited channel (one full region) costs one comparison +
    /// one index, matching the pre-region engine. `&mut` only for the cursor
    /// cache — the audible output is a pure function of (regions, frame).
    ///
    /// Accepted residual: a long-lived early region pins the cursor in front
    /// of any EXPIRED regions that overlap it, leaving their ~2 comparisons
    /// each in the walk — Θ(regions) only for a channel stacking many dead
    /// overlaps under one everlasting bed. Stems-session edits (splits,
    /// trims) don't produce that shape; if a future workload does, the fix
    /// is block-level active-set selection (or an interval index) in the R2
    /// editing arc, not more cursor cleverness here.
    #[inline]
    fn sample(&mut self, ch: usize, frame: u64) -> f32 {
        let source = &self.stems[ch];
        let regions = &self.regions[ch];
        let (last_frame, mut first_live) = self.cursors[ch];
        if frame < last_frame {
            first_live = 0;
        }
        while regions
            .get(first_live)
            .is_some_and(|region| region.timeline_end() <= frame)
        {
            first_live += 1;
        }
        self.cursors[ch] = (frame, first_live);

        let mut acc = 0.0;
        for region in &regions[first_live..] {
            if region.timeline_start > frame {
                break;
            }
            acc += region.sample(source, frame);
        }
        acc
    }
}

/// The engine's source profile — which family of 32 sources it synthesises,
/// chosen ONCE at construction and never mutated at runtime, so a musical run can
/// never be mistaken for a benchmark run. The two profiles are strictly separate:
/// benchmark frames are bit-for-bit the frozen multitone output and carry no
/// source flag; stem frames set [`FLAG_NON_BENCH_SOURCE`] and read
/// `benchmark_eligible = false`.
// The StemSession variant embeds the preloaded StemBank (a `[Vec<f32>; 32]`) and
// is far larger than the unit BenchmarkMultitone variant. The gap is deliberate:
// this enum is a startup singleton constructed and moved into the RT engine
// exactly once (never pushed through a ring or stored in a collection), and
// keeping the bank inline avoids an extra pointer hop per sample on the RT hot
// path — boxing to shrink the enum would buy nothing.
#[allow(clippy::large_enum_variant)]
pub enum SourceProfile {
    /// `benchmark-multitone.v1` — the frozen deterministic seeded multitone
    /// generators ([`SEMI`]); byte-identical output, the ONLY benchmark-eligible
    /// profile.
    BenchmarkMultitone,
    /// `stem-session.v1` — preloaded per-channel mono 48 kHz stems indexed by the
    /// transport clock. Musical, NON-benchmark.
    StemSession(StemBank),
    /// `midi-synth.v1` — per-track rustysynth SoundFont voices driven by a
    /// frame-keyed note schedule against the transport clock (the sequencer
    /// lane). Musical, NON-benchmark.
    MidiSynth(MidiSynthBank),
}

impl SourceProfile {
    /// True only for the benchmark profile.
    pub fn is_benchmark(&self) -> bool {
        matches!(self, SourceProfile::BenchmarkMultitone)
    }

    /// The `mixer.source_profile` enum literal (schema-owned).
    pub fn id(&self) -> &'static str {
        match self {
            SourceProfile::BenchmarkMultitone => SOURCE_PROFILE_BENCHMARK,
            SourceProfile::StemSession(_) => SOURCE_PROFILE_STEM,
            SourceProfile::MidiSynth(_) => SOURCE_PROFILE_MIDI_SYNTH,
        }
    }

    /// `mixer.benchmark_eligible`: TRUE only for the benchmark profile.
    pub fn benchmark_eligible(&self) -> bool {
        self.is_benchmark()
    }

    /// The finite transport length in frames, or `None` for an unbounded source
    /// (the multitone runs forever). The engine clamps seeks + EOF advance to
    /// this so an out-of-range seek lands at the end, never at `u64::MAX`, and
    /// the published position never exceeds the total.
    pub fn transport_len_frames(&self) -> Option<u64> {
        match self {
            SourceProfile::BenchmarkMultitone => None,
            SourceProfile::StemSession(bank) => {
                let n = bank.length_frames();
                if n > 0 { Some(n) } else { None }
            }
            SourceProfile::MidiSynth(bank) => {
                let n = bank.length_frames();
                if n > 0 { Some(n) } else { None }
            }
        }
    }
}

/// Decode WAV bytes into a mono f32 buffer, requiring 48 kHz mono (the stem
/// contract). Int WAVs (16/24/32-bit) are normalised to `[-1, 1)`; float WAVs are
/// taken verbatim. Pure (`hound` only) and preload-time — never called on the RT
/// path. `ctx` names the source for error messages. Rejects any non-48 kHz or
/// non-mono input.
pub fn decode_wav_mono(bytes: &[u8], ctx: &str) -> anyhow::Result<Vec<f32>> {
    let mut reader = hound::WavReader::new(std::io::Cursor::new(bytes))
        .map_err(|e| anyhow::anyhow!("{ctx}: decode WAV header: {e}"))?;
    let spec = reader.spec();
    if spec.sample_rate != SR {
        anyhow::bail!(
            "{ctx}: sample rate {} Hz != required {SR} Hz mono",
            spec.sample_rate
        );
    }
    if spec.channels != 1 {
        anyhow::bail!("{ctx}: {} channels, stems must be mono", spec.channels);
    }
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<std::result::Result<_, _>>()
            .map_err(|e| anyhow::anyhow!("{ctx}: decode f32 samples: {e}"))?,
        hound::SampleFormat::Int => {
            // Normalise the native bit depth to [-1, 1): divide by 2^(bits-1).
            let scale = 1.0f32 / (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 * scale))
                .collect::<std::result::Result<_, _>>()
                .map_err(|e| anyhow::anyhow!("{ctx}: decode int samples: {e}"))?
        }
    };
    // Reject non-finite float samples (finding #10): a hash-valid float WAV can
    // encode NaN/inf, which `hound` decodes verbatim. Left unchecked it would
    // feed the audio callback + meter accumulators (`sumsq += s*s` → NaN poisons
    // the RMS window; peak/clip comparisons silently go false). Fail preload
    // loudly instead. (The Int arm's `v as f32 * scale` is always finite.)
    if let Some(i) = samples.iter().position(|s| !s.is_finite()) {
        anyhow::bail!("{ctx}: non-finite sample (NaN/inf) at frame {i}");
    }
    Ok(samples)
}

// ---------------------------------------------------------------------------
// MIDI-synth source profile (song playback through per-track rustysynth)
// ---------------------------------------------------------------------------

/// One scheduled note boundary in the frame domain: a note-on (`on = true`) or
/// note-off for `key` at absolute transport `frame`. Schedules sort by
/// `(frame, on)` so a same-frame note-off precedes the note-on (same-pitch
/// retrigger stays clean).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoteEvent {
    pub frame: u64,
    pub on: bool,
    /// MIDI key (0-127).
    pub key: u8,
    /// Note-on velocity (0-127); ignored for note-offs.
    pub vel: u8,
}

/// The soundfont-free schedule for one song track: which MIDI channel/program
/// its synth speaks, plus the frame-domain note events. Built OFF the RT thread
/// (from a cosmix-song `Song` by `mixer_host::song_schedules`, or directly in
/// tests); [`MidiSynthBank::build`] turns a set of these into live per-track
/// synthesizers.
#[derive(Clone, Debug, Default)]
pub struct TrackSchedule {
    /// MIDI channel (0-15; 9 = GM percussion) the synth voices this track on.
    pub channel: u8,
    /// MIDI program (instrument) selected once at build time.
    pub program: u8,
    /// Strip name surfaced via the `mixer.channels.N.name` leaf.
    pub name: Option<String>,
    /// Frame-domain note events (sorted defensively at build).
    pub events: Vec<NoteEvent>,
}

/// One song track's live synth voice: a dedicated rustysynth [`Synthesizer`]
/// plus its event cursor and a one-block mono output cache. The engine's
/// per-sample `source()` pulls from `buf`; crossing a block boundary renders
/// the next 128 frames with **sample-accurate** event dispatch (the block is
/// rendered in segments split at event frames).
struct SynthTrack {
    synth: Synthesizer,
    /// MIDI channel the events are voiced on (0-15 as i32 for rustysynth).
    channel: i32,
    /// Frame-sorted note events (offs before ons at equal frames).
    events: Vec<NoteEvent>,
    /// Index of the first event not yet dispatched.
    cursor: usize,
    /// Mono (L+R)/2 cache for frames `[buf_start, buf_start + BLOCK)`.
    buf: [f32; BLOCK],
    scratch_l: [f32; BLOCK],
    scratch_r: [f32; BLOCK],
    /// First frame cached in `buf`; `None` = invalid (before first render, or
    /// after a seek/idle invalidation).
    buf_start: Option<u64>,
    /// Read position within `buf` while the transport is STOPPED (the idle /
    /// live-preview path renders free-running blocks, not transport frames).
    idle_pos: usize,
}

impl SynthTrack {
    /// Render the 128 frames starting at `start` into `buf`. A non-sequential
    /// `start` (first block, or any jump — a seek, or resuming after idle
    /// rendering) silences every voice, rebinds the event cursor, and
    /// **chases**: notes whose on/off window spans `start` are re-fired, so
    /// seeking or resuming into the middle of a held note sounds it (the DAW
    /// note-chase convention) instead of waiting for the next event.
    /// Alloc-free: rustysynth's voice pool and internal buffers are allocated
    /// at construction; the chase scan uses a stack array.
    fn fill_block(&mut self, start: u64) {
        let sequential = self.buf_start.is_some_and(|b| b + BLOCK as u64 == start);
        if !sequential {
            self.synth.note_off_all(true);
            self.cursor = self.events.partition_point(|e| e.frame < start);
            // Chase: replay the on/off history up to the cursor per key (a
            // mono-per-key approximation — overlapping same-key notes collapse
            // to the newest) and re-fire whatever is still held at `start`.
            let mut held: [Option<u8>; 128] = [None; 128];
            for e in &self.events[..self.cursor] {
                held[e.key as usize & 0x7F] = if e.on { Some(e.vel) } else { None };
            }
            for (key, vel) in held.iter().enumerate() {
                if let Some(vel) = vel {
                    self.synth.note_on(self.channel, key as i32, *vel as i32);
                }
            }
        }
        let end = start + BLOCK as u64;
        let mut pos: usize = 0;
        while pos < BLOCK {
            let now = start + pos as u64;
            // Dispatch every event scheduled exactly at this frame (sorted, so
            // any note-off precedes a same-frame note-on).
            while let Some(e) = self.events.get(self.cursor) {
                if e.frame != now {
                    break;
                }
                if e.on {
                    self.synth.note_on(self.channel, e.key as i32, e.vel as i32);
                } else {
                    self.synth.note_off(self.channel, e.key as i32);
                }
                self.cursor += 1;
            }
            // Render up to the next in-block event (sample-accurate segments).
            let seg_end = match self.events.get(self.cursor) {
                Some(e) if e.frame < end => (e.frame - start) as usize,
                _ => BLOCK,
            };
            self.synth.render(
                &mut self.scratch_l[pos..seg_end],
                &mut self.scratch_r[pos..seg_end],
            );
            pos = seg_end;
        }
        for ((b, l), r) in self
            .buf
            .iter_mut()
            .zip(&self.scratch_l)
            .zip(&self.scratch_r)
        {
            *b = 0.5 * (l + r);
        }
        self.buf_start = Some(start);
        self.idle_pos = 0;
    }

    /// One sample of the STOPPED-transport path: free-running synth output
    /// (live-preview notes and release tails), no schedule dispatch, no cursor
    /// movement. Renders a fresh block whenever the idle read position wraps,
    /// and invalidates the transport cache so resuming play re-syncs.
    #[inline]
    fn idle_sample(&mut self) -> f32 {
        if self.idle_pos == 0 {
            self.synth.render(&mut self.scratch_l, &mut self.scratch_r);
            for ((b, l), r) in self
                .buf
                .iter_mut()
                .zip(&self.scratch_l)
                .zip(&self.scratch_r)
            {
                *b = 0.5 * (l + r);
            }
            self.buf_start = None;
        }
        let s = self.buf[self.idle_pos];
        self.idle_pos = (self.idle_pos + 1) % BLOCK;
        s
    }
}

/// The MIDI-synth source bank: one rustysynth voice per song track, mapped 1:1
/// onto mixer channels (channel N = song track N), each **mono-summed** —
/// rustysynth's reverb/chorus are disabled and its pan is left centred, so the
/// mixer strip owns level, pan, mute/solo and space. Everything that allocates
/// (soundfont parse, voice pools, event vectors) happens in [`build`](Self::build)
/// before the cpal stream starts; the RT path only dispatches events and renders
/// into preallocated buffers. Output is a pure function of
/// `(soundfont, schedules, transport frames)` — deterministic across runs.
pub struct MidiSynthBank {
    /// Index = mixer channel. `None` only for channels beyond the song's
    /// track count (no synth, silent strip).
    tracks: Vec<Option<SynthTrack>>,
    /// Transport length in frames (last note-off + release tail).
    length_frames: u64,
    /// Per-channel strip names (None = the default "Ch N").
    names: [Option<String>; NUM_CHANNELS],
    /// Song metadata for the GUI footer.
    song: SongMeta,
}

impl MidiSynthBank {
    /// Build the bank: one dry 48 kHz [`Synthesizer`] per non-empty schedule,
    /// sharing `soundfont`, with the track's program latched via a MIDI program
    /// change. Fails if more schedules than mixer channels are supplied or a
    /// synthesizer rejects the settings.
    pub fn build(
        soundfont: Option<&Arc<SoundFont>>,
        schedules: Vec<TrackSchedule>,
        length_frames: u64,
        song: SongMeta,
    ) -> anyhow::Result<Self> {
        if schedules.len() > NUM_CHANNELS {
            anyhow::bail!(
                "song has {} tracks, the mixer has {NUM_CHANNELS} channels",
                schedules.len()
            );
        }
        let settings = crate::synth::make_settings(
            SR as i32,
            crate::synth::DEFAULT_MAX_POLYPHONY,
            /* reverb_chorus = */ false,
        );
        let mut names: [Option<String>; NUM_CHANNELS] = std::array::from_fn(|_| None);
        let mut tracks: Vec<Option<SynthTrack>> = Vec::with_capacity(schedules.len());
        for (ch, mut sched) in schedules.into_iter().enumerate() {
            names[ch] = sched.name.take();
            // No soundfont loaded yet (the empty-start flow): the track keeps
            // its name/schedule but renders SILENT until a font arrives and
            // the bank is rebuilt — never an error.
            let Some(soundfont) = soundfont else {
                tracks.push(None);
                continue;
            };
            // A track with no notes still gets a synth: it can be voiced live
            // (insert-mode preview / future recording) at any time.
            sched.events.sort_by_key(|e| (e.frame, e.on));
            let mut synth = Synthesizer::new(soundfont, &settings)
                .map_err(|e| anyhow::anyhow!("create synthesizer for channel {ch}: {e}"))?;
            let channel = (sched.channel & 0x0F) as i32;
            synth.process_midi_message(channel, 0xC0, sched.program as i32, 0);
            tracks.push(Some(SynthTrack {
                synth,
                channel,
                events: sched.events,
                cursor: 0,
                buf: [0.0; BLOCK],
                scratch_l: [0.0; BLOCK],
                scratch_r: [0.0; BLOCK],
                buf_start: None,
                idle_pos: 0,
            }));
        }
        Ok(MidiSynthBank {
            tracks,
            length_frames,
            names,
            song,
        })
    }

    /// Per-channel strip names (None = default "Ch N").
    pub fn names(&self) -> &[Option<String>; NUM_CHANNELS] {
        &self.names
    }

    /// Song metadata (the GUI footer).
    pub fn song(&self) -> &SongMeta {
        &self.song
    }

    /// Transport length in frames (last note-off + release tail).
    pub fn length_frames(&self) -> u64 {
        self.length_frames
    }

    /// The number of channels carrying a voiced (non-empty) track.
    pub fn voiced_channels(&self) -> usize {
        self.tracks.iter().flatten().count()
    }

    /// Whether a channel carries a synth (i.e. maps to a song track).
    pub fn is_voiced(&self, ch: usize) -> bool {
        matches!(self.tracks.get(ch), Some(Some(_)))
    }

    /// Offline: render one channel's mono source over consecutive frames from
    /// 0 into `out`. Returns `false` (leaving `out` untouched) for a channel
    /// with no synth. The per-track waveform-display path — the same signal
    /// the mixer strip receives, pre-trim/pan/fader.
    pub fn render_channel(&mut self, ch: usize, out: &mut [f32]) -> bool {
        if !self.is_voiced(ch) {
            return false;
        }
        for (frame, sample) in out.iter_mut().enumerate() {
            *sample = self.sample(ch, frame as u64);
        }
        true
    }

    /// One channel's mono sample at an absolute transport frame, rendering the
    /// containing 128-frame block on first touch. RT-safe after construction.
    #[inline]
    fn sample(&mut self, ch: usize, frame: u64) -> f32 {
        let Some(Some(track)) = self.tracks.get_mut(ch) else {
            return 0.0;
        };
        match track.buf_start {
            Some(b) if frame >= b && frame - b < BLOCK as u64 => track.buf[(frame - b) as usize],
            _ => {
                track.fill_block(frame);
                track.buf[0]
            }
        }
    }

    /// Seek invalidation: silence every voice immediately and drop the block
    /// caches, so the next `sample()` rebinds each cursor at the new position.
    fn invalidate(&mut self) {
        for t in self.tracks.iter_mut().flatten() {
            t.synth.note_off_all(true);
            t.buf_start = None;
        }
    }

    /// One channel's mono sample on the STOPPED-transport path: free-running
    /// synth output (live-preview notes + release tails), no schedule
    /// dispatch. RT-safe.
    #[inline]
    fn sample_idle(&mut self, ch: usize) -> f32 {
        match self.tracks.get_mut(ch) {
            Some(Some(track)) => track.idle_sample(),
            _ => 0.0,
        }
    }

    /// Fire a live (unscheduled) note on a channel's synth — insert-mode
    /// preview. Audible from the next rendered block, playing or stopped.
    /// Channels beyond the song's track count have no synth and ignore this.
    fn live_note(&mut self, ch: usize, key: u8, vel: u8, on: bool) {
        if let Some(Some(track)) = self.tracks.get_mut(ch) {
            if on {
                track.synth.note_on(track.channel, key as i32, vel as i32);
            } else {
                track.synth.note_off(track.channel, key as i32);
            }
        }
    }

    /// Transport-stop convention: release every voice (natural envelope
    /// release, not an immediate cut) so stopping rings out instead of
    /// clicking, then nothing re-fires until play resumes or a live note
    /// arrives.
    fn release_all(&mut self) {
        for t in self.tracks.iter_mut().flatten() {
            t.synth.note_off_all(false);
        }
    }
}

// ---------------------------------------------------------------------------
// dB / pan helpers
// ---------------------------------------------------------------------------

/// dB → linear gain. The silence floor ([`SILENCE_DB`]) hard-mutes to `0.0`
/// (harness §A.2 / §D3(a): `fader_db <= FADER_MIN_DB ? 0.0 : …`).
#[inline]
fn db_to_lin(db: f64) -> f64 {
    if db <= SILENCE_DB {
        0.0
    } else {
        10f64.powf(db / 20.0)
    }
}

/// Linear amplitude → dB, floored at the silence value (never `-inf`/`NaN`).
#[inline]
fn lin_to_db(x: f64) -> f64 {
    if x <= 0.0 {
        SILENCE_DB
    } else {
        (20.0 * x.log10()).max(SILENCE_DB)
    }
}

/// Equal-power pan law: `pan ∈ [-1, 1]` → `(left, right)` gains. Centre gives
/// `cos(π/4) = sin(π/4) ≈ 0.707` on both sides (constant power).
#[inline]
fn equal_power_pan(pan: f64) -> (f64, f64) {
    let theta = (pan.clamp(-1.0, 1.0) + 1.0) * 0.5 * std::f64::consts::FRAC_PI_2;
    (theta.cos(), theta.sin())
}

/// One channel's **frozen** deterministic multitone source at an absolute
/// sample frame (harness §D5), given its precomputed fundamental `f0`. Pure,
/// phase-accumulator-free, transport-position driven. **Byte-identical** across
/// runs — the benchmark signal path; must never change without invalidating
/// cross-run comparability (guarded by the golden regression test).
#[inline]
fn multitone(f0: f64, frame: u64) -> f32 {
    let t = frame as f64 / SR_F;
    use std::f64::consts::TAU;
    let mut s = 0.60 * (TAU * f0 * t).sin();
    s += 0.25 * (TAU * f0 * 2.0 * t).sin();
    s += 0.12 * (TAU * f0 * 3.0 * t).sin();
    (s * SRC_HEADROOM) as f32
}

// ---------------------------------------------------------------------------
// Per-meter accumulator (RMS / peak / peak-hold / clip)
// ---------------------------------------------------------------------------

/// One stereo meter's running accumulation over the current 800-sample window.
/// `sumsq`/`n`/`peak` reset each frame; `hold` and `clip` persist (latched).
#[derive(Clone, Copy, Debug)]
struct MeterAccum {
    sumsq_l: f64,
    sumsq_r: f64,
    n: u32,
    peak_l: f32,
    peak_r: f32,
    hold_l: f32,
    hold_r: f32,
    clip: u8,
}

impl MeterAccum {
    const fn new() -> Self {
        MeterAccum {
            sumsq_l: 0.0,
            sumsq_r: 0.0,
            n: 0,
            peak_l: 0.0,
            peak_r: 0.0,
            hold_l: 0.0,
            hold_r: 0.0,
            clip: 0,
        }
    }

    /// Feed one post-pan stereo sample. Meters see the **pre-clamp** signal so
    /// the clip latch is truthful (harness §D3(c)).
    #[inline]
    fn feed(&mut self, l: f32, r: f32) {
        self.sumsq_l += l as f64 * l as f64;
        self.sumsq_r += r as f64 * r as f64;
        self.n += 1;
        let al = l.abs();
        let ar = r.abs();
        if al > self.peak_l {
            self.peak_l = al;
        }
        if ar > self.peak_r {
            self.peak_r = ar;
        }
        if al >= 1.0 {
            self.clip |= 0b01;
        }
        if ar >= 1.0 {
            self.clip |= 0b10;
        }
    }

    /// Finalise the window into a wire [`MeterRecord`] and reset the per-window
    /// accumulators. Peak-hold decays then latches this window's peak; the clip
    /// bits stay latched (cleared only by an explicit `meter.clip = false`).
    fn finalize(&mut self) -> MeterRecord {
        let rms_l = rms_db(self.sumsq_l, self.n);
        let rms_r = rms_db(self.sumsq_r, self.n);
        self.hold_l = (self.hold_l * HOLD_DECAY).max(self.peak_l);
        self.hold_r = (self.hold_r * HOLD_DECAY).max(self.peak_r);
        let rec = MeterRecord {
            rms_l: to_centi_dbfs(rms_l),
            rms_r: to_centi_dbfs(rms_r),
            peak_l: to_centi_dbfs(lin_to_db(self.peak_l as f64)),
            peak_r: to_centi_dbfs(lin_to_db(self.peak_r as f64)),
            hold_l: to_centi_dbfs(lin_to_db(self.hold_l as f64)),
            hold_r: to_centi_dbfs(lin_to_db(self.hold_r as f64)),
            clip: self.clip & CLIP_VALID_MASK,
        };
        self.sumsq_l = 0.0;
        self.sumsq_r = 0.0;
        self.n = 0;
        self.peak_l = 0.0;
        self.peak_r = 0.0;
        rec
    }

    /// Reset the latched state (peak-hold + clip) — the `meter.clip = false`
    /// / reset-defaults path.
    fn reset_latches(&mut self) {
        self.hold_l = 0.0;
        self.hold_r = 0.0;
        self.clip = 0;
    }
}

/// Window RMS (linear → dB), floored at silence for an empty window.
#[inline]
fn rms_db(sumsq: f64, n: u32) -> f64 {
    if n == 0 {
        return SILENCE_DB;
    }
    lin_to_db((sumsq / n as f64).sqrt())
}

// ---------------------------------------------------------------------------
// The block processor
// ---------------------------------------------------------------------------

/// The pure 32-channel mixer processor. Holds the latched per-channel linear
/// coefficients, the 33 meter accumulators (32 strips + master), the absolute
/// sample clock, and the frame sequence counter. Drive it with
/// [`process_block`](Self::process_block); reconfigure with
/// [`set_controls`](Self::set_controls).
pub struct MixerEngine {
    /// Precomputed fundamentals `55 · 2^(SEMI[ch]/12)` (Hz).
    f0: [f64; NUM_CHANNELS],
    /// **Target** post-pan left/right coefficients per channel
    /// (`trim·fader·gate·pan`), latched instantly by [`set_controls`].
    tgt_l: [f32; NUM_CHANNELS],
    tgt_r: [f32; NUM_CHANNELS],
    /// **Smoothed** post-pan coefficients — the RT-owned shadows that slew toward
    /// `tgt_l/tgt_r` each sample (de-zipper; §D7). Never shared, never atomic.
    cur_l: [f32; NUM_CHANNELS],
    cur_r: [f32; NUM_CHANNELS],
    /// Target + smoothed master coefficient (`master_pad · fader · gate`).
    master_tgt: f32,
    master_cur: f32,
    /// Per-sample linear increment while ramping (`Δ / RAMP_SAMPLES`), per coeff.
    inc_l: [f32; NUM_CHANNELS],
    inc_r: [f32; NUM_CHANNELS],
    master_inc: f32,
    /// Remaining ramp samples per coefficient (0 = settled exactly at target).
    rem_l: [u16; NUM_CHANNELS],
    rem_r: [u16; NUM_CHANNELS],
    master_rem: u16,
    playing: bool,
    /// 33 meter accumulators: `[0..32]` strips, `[32]` master.
    accum: [MeterAccum; NUM_METERS],
    /// Absolute, continuously-advancing sample-frame clock — the **meter capture
    /// clock** (`capture_frame`), advancing every processed sample regardless of
    /// transport. Paired with `frame0_mono` for input→DSP wall-clock correlation.
    sample_pos: u64,
    /// **Transport** position in frames: the source clock, advancing only while
    /// `playing`. Drives the deterministic sources (so RTZ / seek resets phase);
    /// distinct from `sample_pos` (M7).
    transport_frame: u64,
    /// Samples accumulated into the current (unfinished) meter window.
    window_n: u64,
    seq: u32,
    /// The authoritative server-assigned control revision currently latched
    /// (allocated by props-core, latched via [`set_controls`]; never engine-local).
    applied_rev: u64,
    /// `CLOCK_MONOTONIC` ns of sample-frame 0. A construction-time fallback that
    /// [`ensure_started`](Self::ensure_started) overrides to the true first-block
    /// (first-callback) instant, so `capture_frame / SR` tracks wall-clock from
    /// when audio actually begins flowing (BLOCKER-3 / §D9).
    frame0_mono: u64,
    /// Whether `frame0_mono` has been pinned to the first processed block.
    started: bool,
    /// Whether emitted frames carry [`FLAG_SIMULATOR`].
    simulator: bool,
    /// The immutable source profile (benchmark multitone vs preloaded stems),
    /// fixed at construction. Selects [`source`](Self::source) and whether every
    /// frame carries [`FLAG_NON_BENCH_SOURCE`].
    profile: SourceProfile,
}

impl MixerEngine {
    /// Build an engine at [`SR`] on the **benchmark multitone** source profile.
    /// `simulator` sets [`FLAG_SIMULATOR`] on every emitted frame (the D7 dry-run
    /// backend). Starts with the A.8 default control state (transport stopped ⇒
    /// silent until [`set_controls`] enables `playing`). For the musical profile
    /// use [`with_profile`](Self::with_profile).
    pub fn new(simulator: bool) -> Self {
        Self::with_profile(simulator, SourceProfile::BenchmarkMultitone)
    }

    /// Build an engine at [`SR`] with an explicit immutable source `profile`. The
    /// profile is chosen once here and never mutated — the stem-session profile
    /// therefore sets [`FLAG_NON_BENCH_SOURCE`] on every frame and reads
    /// `benchmark_eligible = false`, so a musical run can never enter a chart.
    pub fn with_profile(simulator: bool, profile: SourceProfile) -> Self {
        let mut f0 = [0.0f64; NUM_CHANNELS];
        for (ch, f) in f0.iter_mut().enumerate() {
            *f = 55.0 * 2f64.powf(SEMI[ch] as f64 / 12.0);
        }
        let mut eng = MixerEngine {
            f0,
            tgt_l: [0.0; NUM_CHANNELS],
            tgt_r: [0.0; NUM_CHANNELS],
            cur_l: [0.0; NUM_CHANNELS],
            cur_r: [0.0; NUM_CHANNELS],
            master_tgt: 0.0,
            master_cur: 0.0,
            inc_l: [0.0; NUM_CHANNELS],
            inc_r: [0.0; NUM_CHANNELS],
            master_inc: 0.0,
            rem_l: [0; NUM_CHANNELS],
            rem_r: [0; NUM_CHANNELS],
            master_rem: 0,
            playing: false,
            accum: [MeterAccum::new(); NUM_METERS],
            sample_pos: 0,
            transport_frame: 0,
            window_n: 0,
            seq: 0,
            applied_rev: 0,
            frame0_mono: now_mono_ns(),
            started: false,
            simulator,
            profile,
        };
        // The A.8 default control state latches at authoritative revision 0, and
        // the smoothed shadows start already *at* that target (no startup ramp
        // from silence — the initial state is settled).
        eng.set_controls(&Controls::default(), 0);
        eng.snap_smoothing();
        eng
    }

    /// Latch a new control snapshot: recompute every per-channel **target**
    /// coefficient and the master target, and latch the **authoritative
    /// server-assigned** `revision` into `applied_rev`. Per ADR §8 Q8(a), the
    /// revision is allocated by the props-core revisioned write facility (the
    /// server), *not* the RT engine — the engine never manufactures or increments
    /// one, it only records which authoritative revision it has latched (the value
    /// a later `dsp.applied {revision, sample_frame}` reports). The audible
    /// coefficient then slews toward the new target over [`RAMP_SAMPLES`] inside
    /// [`process_block`](Self::process_block) (de-zipper; §D7) — the target itself
    /// jumps instantly, which is what "applied" means.
    pub fn set_controls(&mut self, c: &Controls, revision: u64) {
        let any_solo = c.channels.iter().any(|ch| ch.solo);
        for ch in 0..NUM_CHANNELS {
            let cc = &c.channels[ch];
            let gate = if cc.mute {
                0.0
            } else if any_solo {
                if cc.solo { 1.0 } else { 0.0 }
            } else {
                1.0
            };
            let trim = db_to_lin(cc.trim_db);
            let fader = db_to_lin(cc.fader_db);
            let (pan_l, pan_r) = equal_power_pan(cc.pan);
            let nl = (trim * fader * gate * pan_l) as f32;
            let nr = (trim * fader * gate * pan_r) as f32;
            // Arm a fresh fixed-duration ramp ONLY when the target actually moved
            // (the full-snapshot latch recomputes every coefficient, but an
            // unchanged one must not restart its ramp).
            if nl != self.tgt_l[ch] {
                self.inc_l[ch] = (nl - self.cur_l[ch]) / RAMP_SAMPLES_F;
                self.rem_l[ch] = RAMP_SAMPLES;
                self.tgt_l[ch] = nl;
            }
            if nr != self.tgt_r[ch] {
                self.inc_r[ch] = (nr - self.cur_r[ch]) / RAMP_SAMPLES_F;
                self.rem_r[ch] = RAMP_SAMPLES;
                self.tgt_r[ch] = nr;
            }
        }
        let master_gate = if c.master.mute { 0.0 } else { 1.0 };
        // MASTER_PAD folds into the master fader (A.8) so a full unity 32-ch mix
        // sits with headroom instead of clipping from t=0.
        let nm = (db_to_lin(c.master.fader_db + MASTER_PAD_DB) * master_gate) as f32;
        if nm != self.master_tgt {
            self.master_inc = (nm - self.master_cur) / RAMP_SAMPLES_F;
            self.master_rem = RAMP_SAMPLES;
            self.master_tgt = nm;
        }
        // Transport stop edge (playing → stopped): a synth source releases its
        // voices (DAW stop convention) so the stop rings out; the idle path
        // then renders only the tails + any live-preview notes.
        if self.playing
            && !c.playing
            && let SourceProfile::MidiSynth(bank) = &mut self.profile
        {
            bank.release_all();
        }
        self.playing = c.playing;
        self.applied_rev = revision;
    }

    /// Advance every smoothed coefficient one sample along its fixed-duration ramp,
    /// snapping exactly onto the target on the final sample. Alloc-free; called
    /// once per processed sample.
    #[inline]
    fn step_smoothing(&mut self) {
        for ch in 0..NUM_CHANNELS {
            if self.rem_l[ch] > 0 {
                self.rem_l[ch] -= 1;
                if self.rem_l[ch] == 0 {
                    self.cur_l[ch] = self.tgt_l[ch];
                } else {
                    self.cur_l[ch] += self.inc_l[ch];
                }
            }
            if self.rem_r[ch] > 0 {
                self.rem_r[ch] -= 1;
                if self.rem_r[ch] == 0 {
                    self.cur_r[ch] = self.tgt_r[ch];
                } else {
                    self.cur_r[ch] += self.inc_r[ch];
                }
            }
        }
        if self.master_rem > 0 {
            self.master_rem -= 1;
            if self.master_rem == 0 {
                self.master_cur = self.master_tgt;
            } else {
                self.master_cur += self.master_inc;
            }
        }
    }

    /// Force the smoothed shadows onto their targets and cancel any in-flight ramp
    /// — used at construction so the initial control state is settled from sample 0,
    /// and by the OFFLINE renderers after their one `set_controls` (an export must
    /// apply the captured mix from frame 0; the de-zipper ramp would leak the first
    /// milliseconds of a muted channel).
    pub(crate) fn snap_smoothing(&mut self) {
        self.cur_l = self.tgt_l;
        self.cur_r = self.tgt_r;
        self.master_cur = self.master_tgt;
        self.rem_l = [0; NUM_CHANNELS];
        self.rem_r = [0; NUM_CHANNELS];
        self.master_rem = 0;
    }

    /// Pin `frame0_mono` to the first processed block (the first cpal callback on
    /// the real path), so `capture_frame / SR` tracks wall-clock from when audio
    /// actually starts flowing. Idempotent; called at the top of each process
    /// pass and safe to call explicitly before reading [`frame0_mono`].
    pub fn ensure_started(&mut self) {
        if !self.started {
            self.frame0_mono = now_mono_ns();
            self.started = true;
        }
    }

    /// Seek the transport (RTZ = `frame == 0`): reset the source clock, which — as
    /// the sources are a pure function of `transport_frame` — resets their phase.
    /// The absolute meter clock (`sample_pos`) is untouched (M7). The target is
    /// clamped to `[0, length_frames]` for a finite source (trust boundary): a
    /// domain-blind renderer — or a crafted `xe` write with an absurd value —
    /// canonicalizes under `0..f64::MAX`, so without this a huge seek would land
    /// at ~`u64::MAX` and silence the transport indefinitely. Unbounded sources
    /// (multitone) are left as-is.
    pub fn seek(&mut self, frame: u64) {
        self.transport_frame = match self.profile.transport_len_frames() {
            Some(len) => frame.min(len),
            None => frame,
        };
        // A synth source is stateful: silence ringing voices and drop the block
        // caches so the event cursors rebind at the new position.
        if let SourceProfile::MidiSynth(bank) = &mut self.profile {
            bank.invalidate();
        }
    }

    /// Force a hard stop at frame zero — the RT half of a song LOAD. Clears the
    /// playing gate and rewinds to zero; `seek(0)` invalidates the (already
    /// swapped-in) synth bank, so its event cursors rebind at zero and no voice
    /// rings. Unlike a `set_controls` stop this is a standalone op with no
    /// musical ring-out: [`super::mixer_host::RtState::run_block`] applies it at
    /// the load barrier AFTER the control drain, so a stale queued Play cannot
    /// restart the just-loaded song before it renders.
    pub fn stop_at_zero(&mut self) {
        self.playing = false;
        self.seek(0);
    }

    /// Absolute sample clock (meter `capture_frame`; frames processed since start).
    pub fn sample_pos(&self) -> u64 {
        self.sample_pos
    }

    /// Live transport position in frames (source clock; advances only while
    /// playing). Exposed for the transient `transport.position` read (M7).
    pub fn transport_frame(&self) -> u64 {
        self.transport_frame
    }

    /// Whether the transport is playing (song audio renders; otherwise the idle
    /// path runs). Mirrors [`transport_frame`](Self::transport_frame) for tests
    /// and the load-barrier assertions.
    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// The highest authoritative control revision currently latched into the
    /// audio graph (the value a `dsp.applied` reports and every emitted
    /// [`MeterFrame`] carries as `applied_rev`).
    pub fn applied_rev(&self) -> u64 {
        self.applied_rev
    }

    /// Advance the latched high-water revision *without* changing any control
    /// coefficient — for a block whose only drained commands were latch resets
    /// (`meter.clip = false`), which still carry authoritative revisions the
    /// `applied_rev` high-water must reflect. Never moves the revision
    /// backwards.
    pub fn set_applied_rev(&mut self, rev: u64) {
        if rev > self.applied_rev {
            self.applied_rev = rev;
        }
    }

    /// Clear one meter's peak-hold + clip latch (the per-path
    /// `mixer.channels.{id}.meter.clip = false` / `mixer.master.meter.clip`
    /// reset). `meter` is the record index: `0..32` input strips, `32` master.
    /// Out-of-range indices are ignored.
    pub fn reset_latch(&mut self, meter: usize) {
        if let Some(a) = self.accum.get_mut(meter) {
            a.reset_latches();
        }
    }

    /// The `CLOCK_MONOTONIC` ns stamp of sample-frame 0 (for input→DSP
    /// correlation; harness §D9).
    pub fn frame0_mono(&self) -> u64 {
        self.frame0_mono
    }

    /// Clear every meter's peak-hold + clip latch (reset-defaults / `meter.clip`
    /// reset path).
    pub fn reset_latches(&mut self) {
        for a in &mut self.accum {
            a.reset_latches();
        }
    }

    /// The immutable source profile chosen at construction.
    pub fn source_profile(&self) -> &SourceProfile {
        &self.profile
    }

    /// One channel's source sample at an absolute transport frame, dispatched on
    /// the immutable profile: the frozen benchmark multitone, a preloaded stem,
    /// or a synth-track render. `&mut` because the synth source is stateful (it
    /// renders and caches one block at a time); the other two arms stay pure.
    #[inline]
    fn source(&mut self, ch: usize, frame: u64) -> f32 {
        match &mut self.profile {
            SourceProfile::BenchmarkMultitone => multitone(self.f0[ch], frame),
            SourceProfile::StemSession(bank) => bank.sample(ch, frame),
            SourceProfile::MidiSynth(bank) => bank.sample(ch, frame),
        }
    }

    /// One channel's source sample while the transport is STOPPED. The pure
    /// profiles are silent (transport stopped ⇒ silence, unchanged); a synth
    /// source free-runs so live-preview notes and stop-release tails sound.
    #[inline]
    fn source_idle(&mut self, ch: usize) -> f32 {
        match &mut self.profile {
            SourceProfile::BenchmarkMultitone | SourceProfile::StemSession(_) => 0.0,
            SourceProfile::MidiSynth(bank) => bank.sample_idle(ch),
        }
    }

    /// Fire a live (unscheduled) note — insert-mode preview, playing or
    /// stopped. No-op on non-synth profiles and channels without a synth.
    pub fn live_note(&mut self, ch: usize, key: u8, vel: u8, on: bool) {
        if let SourceProfile::MidiSynth(bank) = &mut self.profile {
            bank.live_note(ch, key, vel, on);
        }
    }

    /// Swap in a freshly-built synth bank (a song edit), returning the old
    /// bank in `bank` — the caller ships it back off-thread for deallocation.
    /// The profile FAMILY is immutable: on a non-MidiSynth profile this is a
    /// no-op returning `false` (and `bank` still holds the new bank). The
    /// transport position is preserved (clamped to the new length); the
    /// incoming bank starts cold and re-syncs at the current frame on the next
    /// sample. Alloc-free: a `mem::swap` of the bank values.
    pub fn swap_midi_bank(&mut self, bank: &mut MidiSynthBank) -> bool {
        let SourceProfile::MidiSynth(current) = &mut self.profile else {
            return false;
        };
        std::mem::swap(current, bank);
        if let Some(len) = self.profile.transport_len_frames() {
            self.transport_frame = self.transport_frame.min(len);
        }
        true
    }

    /// Swap in a freshly-built stem bank (the region-edit path — same audio
    /// `Arc`s, new region metadata). Rejected (false, `bank` untouched) on a
    /// non-stem profile. The transport clamps into the new timeline length.
    pub fn swap_stem_bank(&mut self, bank: &mut StemBank) -> bool {
        let SourceProfile::StemSession(current) = &mut self.profile else {
            return false;
        };
        std::mem::swap(current, bank);
        if let Some(len) = self.profile.transport_len_frames() {
            self.transport_frame = self.transport_frame.min(len);
        }
        true
    }

    /// Process `frames` samples, appending every completed [`MeterFrame`] to
    /// `out` (typically 0 or 1 for `frames <= BLOCK`). No audio is produced — this
    /// is the headless path (simulator, paced no-device fallback, tests). `out` is
    /// caller-owned and caller-sized; the hot loop allocates nothing.
    pub fn process_block(&mut self, frames: usize, out: &mut Vec<MeterFrame>) {
        self.process_samples(frames, None, out);
    }

    /// Process `frames` samples **and write the pre-clamp master stereo** into
    /// `audio_l`/`audio_r` (each exactly `frames` long) — the real cpal path. The
    /// caller clamps to `[-1, 1]` only at the device write, so meters (fed here)
    /// see the pre-clamp signal and the clip latch stays truthful (§D3(c)).
    /// Alloc-free and lock-free; every completed [`MeterFrame`] is appended to
    /// `out`.
    pub fn process_block_audio(
        &mut self,
        audio_l: &mut [f32],
        audio_r: &mut [f32],
        out: &mut Vec<MeterFrame>,
    ) {
        let n = audio_l.len().min(audio_r.len());
        self.process_samples(n, Some((&mut audio_l[..n], &mut audio_r[..n])), out);
    }

    /// The shared block core. `audio`, when `Some`, receives the pre-clamp master
    /// stereo (`frames`-long slices); when `None` no audio is produced. Per sample
    /// it slews the smoothed coefficients toward their targets (de-zipper),
    /// synthesises the deterministic sources against the **transport** clock, taps
    /// the post-pan meters, sums through the master coefficient, advances both the
    /// absolute (`sample_pos`) and transport clocks, and cuts a meter frame on each
    /// 800-sample boundary.
    fn process_samples(
        &mut self,
        frames: usize,
        mut audio: Option<(&mut [f32], &mut [f32])>,
        out: &mut Vec<MeterFrame>,
    ) {
        self.ensure_started();
        for i in 0..frames {
            // De-zipper: advance every resolved coefficient one fixed-duration step.
            self.step_smoothing();

            let mut acc_l = 0.0f32;
            let mut acc_r = 0.0f32;
            for ch in 0..NUM_CHANNELS {
                let s = if self.playing {
                    self.source(ch, self.transport_frame)
                } else {
                    self.source_idle(ch)
                };
                let l = s * self.cur_l[ch];
                let r = s * self.cur_r[ch];
                acc_l += l;
                acc_r += r;
                // Meter tap is post-pan, pre-master (harness §D3 "stereo post-pan").
                self.accum[ch].feed(l, r);
            }
            let out_l = acc_l * self.master_cur;
            let out_r = acc_r * self.master_cur;
            // Master meter = post-master-fader stereo sum, pre-clamp.
            self.accum[NUM_CHANNELS].feed(out_l, out_r);
            if let Some((lbuf, rbuf)) = &mut audio {
                lbuf[i] = out_l;
                rbuf[i] = out_r;
            }

            self.sample_pos += 1;
            if self.playing {
                // `saturating_add` (not `+= 1`): an UNBOUNDED source doesn't clamp
                // a seek, so a crafted seek to ~u64::MAX then advancing would
                // overflow (panic in debug / wrap in release). Saturate instead.
                self.transport_frame = self.transport_frame.saturating_add(1);
                // EOF clamp: never advance the transport past a finite source
                // length, so the published position (and the M:SS readout) can't
                // exceed the total (e.g. 6:00 / 5:06). Unbounded sources run on.
                if let Some(len) = self.profile.transport_len_frames()
                    && self.transport_frame > len
                {
                    self.transport_frame = len;
                }
            }
            self.window_n += 1;
            if self.window_n >= METER_PERIOD {
                out.push(self.cut_frame());
                self.window_n = 0;
            }
        }
    }

    /// Finalise the 33 accumulators into one wire frame and advance `seq`.
    fn cut_frame(&mut self) -> MeterFrame {
        let mut records = [MeterRecord::default(); NUM_METERS];
        for (i, rec) in records.iter_mut().enumerate() {
            *rec = self.accum[i].finalize();
        }
        // Flags are two independent guard axes: FLAG_SIMULATOR (scaffold vs real
        // DSP) and FLAG_NON_BENCH_SOURCE (musical stems vs the frozen benchmark).
        let mut flags = 0u32;
        if self.simulator {
            flags |= FLAG_SIMULATOR;
        }
        if !self.profile.is_benchmark() {
            flags |= FLAG_NON_BENCH_SOURCE;
        }
        let frame = MeterFrame {
            seq: self.seq,
            capture_frame: self.sample_pos,
            applied_rev: self.applied_rev,
            frame0_mono: self.frame0_mono,
            flags,
            records,
        };
        self.seq = self.seq.wrapping_add(1);
        frame
    }
}

/// `CLOCK_MONOTONIC` nanoseconds — a **real** monotonic anchor, never wall time.
/// `frame0_mono` correlates input→DSP against the same clock the RT thread and
/// trace spans read, so it is immune to NTP steps, suspend/resume, and any
/// wall-clock jump (a `SystemTime`/`UNIX_EPOCH` stamp is not, and would corrupt
/// the correlation the instant the clock is stepped). Not part of any
/// deterministic hash. Returns 0 only on the effectively-impossible syscall
/// failure.
pub(crate) fn now_mono_ns() -> u64 {
    // SAFETY: `ts` is a fully-initialised `timespec` passed by exclusive
    // reference; `clock_gettime` only writes it and returns 0 on success.
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    if rc != 0 {
        return 0;
    }
    (ts.tv_sec as u64)
        .wrapping_mul(1_000_000_000)
        .wrapping_add(ts.tv_nsec as u64)
}

// ---------------------------------------------------------------------------
// Standalone SIMULATOR runner (decision D7 "now" dry-run backend)
// ---------------------------------------------------------------------------

/// Run the mixer engine headless as the scaffold SIMULATOR: drive the real block
/// processor with the deterministic multitone sources (all strips at unity, the
/// full 32-channel mix, transport playing), generate `frames` meter frames with
/// [`FLAG_SIMULATOR`] set, optionally print a summary every `print_every` frames,
/// and optionally write the raw concatenated 465-byte A.6 frames to `out_path`.
///
/// No cpal audio device and no Bus broker are touched — this builds and runs on
/// any headless node. The full Bus `props.set` surface + 60 Hz publisher are a
/// documented follow-up (module docs).
pub fn run_simulator(
    frames: u64,
    print_every: u64,
    out_path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    // Mandatory non-benchmark labelling (harness §D8): a run driven by this
    // must never enter a performance chart.
    eprintln!("WARN: MIXER SIMULATOR — non-benchmark, renderer scaffold only");

    let mut engine = MixerEngine::new(/* simulator = */ true);
    // Full 32-channel unity mix, transport running. In the real daemon the
    // server-side write facility allocates this revision; the scaffold stands in
    // with the first authoritative revision after the default (0) latch.
    let controls = Controls {
        playing: true,
        ..Default::default()
    };
    engine.set_controls(&controls, 1);

    let mut produced = 0u64;
    let mut scratch: Vec<MeterFrame> = Vec::with_capacity(4);
    // Only retain encoded bytes when we are actually writing them.
    let mut wire: Vec<u8> = Vec::new();
    if out_path.is_some() {
        wire.reserve((frames as usize).saturating_mul(cosmix_mixer_schema::METER_FRAME_LEN));
    }

    println!(
        "mixer-sim: {} channels @ {} Hz, {}-frame blocks, {} samples/meter-frame; generating {} frame(s)",
        NUM_CHANNELS, SR, BLOCK, METER_PERIOD, frames
    );

    'outer: while produced < frames {
        scratch.clear();
        engine.process_block(BLOCK, &mut scratch);
        for f in scratch.drain(..) {
            if out_path.is_some() {
                wire.extend_from_slice(&f.encode());
            }
            produced += 1;
            let should_print = print_every > 0
                && (produced == 1 || produced.is_multiple_of(print_every) || produced == frames);
            if should_print {
                print_frame_summary(&f);
            }
            if produced >= frames {
                break 'outer;
            }
        }
    }

    if let Some(path) = out_path {
        std::fs::write(path, &wire)
            .map_err(|e| anyhow::anyhow!("write frames {}: {e}", path.display()))?;
        println!(
            "wrote {} frame(s) × {} B = {} B → {}",
            produced,
            cosmix_mixer_schema::METER_FRAME_LEN,
            wire.len(),
            path.display()
        );
    }

    println!(
        "mixer-sim: done — {} simulator frame(s), FLAG_SIMULATOR set (mixer/engine = \"simulator\")",
        produced
    );
    Ok(())
}

/// Print one human-readable summary line for a frame: the master meter + a
/// couple of representative channels, in dB.
fn print_frame_summary(f: &MeterFrame) {
    use cosmix_mixer_schema::from_centi_dbfs;
    let m = &f.records[NUM_CHANNELS]; // master
    let clip = if m.clip == 0 { "" } else { " CLIP" };
    println!(
        "seq {:>6} @frame {:>9} rev {:>4} sim={} | master rms {:>6.1}/{:>6.1} dB peak {:>6.1}/{:>6.1} hold {:>6.1}/{:>6.1}{}",
        f.seq,
        f.capture_frame,
        f.applied_rev,
        f.is_simulator(),
        from_centi_dbfs(m.rms_l),
        from_centi_dbfs(m.rms_r),
        from_centi_dbfs(m.peak_l),
        from_centi_dbfs(m.peak_r),
        from_centi_dbfs(m.hold_l),
        from_centi_dbfs(m.hold_r),
        clip,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_mixer_schema::CENTI_DB_MIN;

    /// Drive N samples, collecting every emitted frame.
    fn run_samples(engine: &mut MixerEngine, samples: u64) -> Vec<MeterFrame> {
        let mut out = Vec::new();
        let mut left = samples;
        while left > 0 {
            let n = left.min(BLOCK as u64) as usize;
            engine.process_block(n, &mut out);
            left -= n as u64;
        }
        out
    }

    #[test]
    fn semi_profile_is_distinct_and_non_adjacent() {
        for w in SEMI.windows(2) {
            assert!(
                w[1] - w[0] >= 2,
                "SEMI must be non-adjacent (gap >= 2): {w:?}"
            );
        }
        // Top fundamental's 3rd harmonic must stay below Nyquist.
        let f0_max = 55.0 * 2f64.powf(*SEMI.last().unwrap() as f64 / 12.0);
        assert!(
            f0_max * 3.0 < SR as f64 / 2.0,
            "3rd harmonic of top channel aliases"
        );
    }

    #[test]
    fn meter_cadence_cuts_on_800_boundaries() {
        let mut engine = MixerEngine::new(true);
        let controls = Controls {
            playing: true,
            ..Default::default()
        };
        engine.set_controls(&controls, 1);

        // 800 does not divide 128; exercise the cross-block accumulation.
        let frames = run_samples(&mut engine, METER_PERIOD * 5 + 37);
        assert_eq!(frames.len(), 5, "5 full meter windows in 4037 samples");
        for (i, f) in frames.iter().enumerate() {
            assert_eq!(f.seq, i as u32);
            assert_eq!(f.capture_frame, METER_PERIOD * (i as u64 + 1));
            assert!(f.is_simulator());
        }
    }

    #[test]
    fn frames_roundtrip_through_the_schema_codec() {
        let mut engine = MixerEngine::new(true);
        let controls = Controls {
            playing: true,
            ..Default::default()
        };
        engine.set_controls(&controls, 1);
        let frames = run_samples(&mut engine, METER_PERIOD * 3);
        assert_eq!(frames.len(), 3);
        for f in &frames {
            let bytes = f.encode();
            assert_eq!(bytes.len(), cosmix_mixer_schema::METER_FRAME_LEN);
            let decoded = MeterFrame::decode(&bytes).expect("decode");
            assert_eq!(&decoded, f);
            assert_eq!(decoded.flags, FLAG_SIMULATOR);
        }
    }

    #[test]
    fn full_unity_mix_is_calibrated_no_clip_with_headroom() {
        // The whole point of SRC_HEADROOM + MASTER_PAD (M10): a full 32-channel
        // unity mix must sit below clip with real headroom, so meters carry test
        // signal instead of pinning + latching clip from t=0.
        let mut engine = MixerEngine::new(false);
        let controls = Controls {
            playing: true,
            ..Default::default()
        };
        engine.set_controls(&controls, 1);
        // Warm past the first window's transient, then measure a steady window.
        let frames = run_samples(&mut engine, METER_PERIOD * 20);
        let m = &frames.last().unwrap().records[NUM_CHANNELS];
        assert_eq!(m.clip, 0, "master must not clip a unity mix");
        // Master RMS in a sane band (nominal ≈ -18 dBFS with the frozen consts).
        assert!(m.rms_l > CENTI_DB_MIN, "master must carry signal");
        let rms_db = cosmix_mixer_schema::from_centi_dbfs(m.rms_l);
        assert!(
            (-30.0..=-6.0).contains(&rms_db),
            "master RMS {rms_db} dB outside the calibrated headroom band"
        );
        // Peak below 0 dBFS = genuine headroom before clip.
        assert!(m.peak_l < 0, "master peak should sit below 0 dBFS");
    }

    #[test]
    fn solo_mutes_non_soloed_channels() {
        let mut engine = MixerEngine::new(false);
        let mut controls = Controls {
            playing: true,
            ..Default::default()
        };
        controls.channels[3].solo = true;
        engine.set_controls(&controls, 1);
        let frames = run_samples(&mut engine, METER_PERIOD * 2);
        let last = frames.last().unwrap();
        // Soloed channel carries signal; a non-soloed one is silent.
        assert!(
            last.records[3].rms_l > CENTI_DB_MIN,
            "soloed channel audible"
        );
        assert_eq!(
            last.records[0].rms_l, CENTI_DB_MIN,
            "non-soloed channel silent"
        );
    }

    #[test]
    fn mute_silences_a_channel() {
        let mut engine = MixerEngine::new(false);
        let mut controls = Controls {
            playing: true,
            ..Default::default()
        };
        controls.channels[7].mute = true;
        engine.set_controls(&controls, 1);
        let frames = run_samples(&mut engine, METER_PERIOD * 2);
        let last = frames.last().unwrap();
        assert_eq!(last.records[7].rms_l, CENTI_DB_MIN, "muted channel silent");
        assert!(
            last.records[6].rms_l > CENTI_DB_MIN,
            "neighbour still audible"
        );
    }

    #[test]
    fn transport_stopped_is_silent() {
        let mut engine = MixerEngine::new(false);
        // default controls: playing = false
        let frames = run_samples(&mut engine, METER_PERIOD * 2);
        for f in &frames {
            for rec in &f.records {
                assert_eq!(rec.rms_l, CENTI_DB_MIN);
                assert_eq!(rec.peak_l, CENTI_DB_MIN);
            }
        }
    }

    #[test]
    fn transport_frame_advances_only_while_playing_and_seek_resets() {
        let mut engine = MixerEngine::new(false);
        // Play one window: the meter clock and the transport clock advance together.
        engine.set_controls(
            &Controls {
                playing: true,
                ..Default::default()
            },
            1,
        );
        run_samples(&mut engine, METER_PERIOD);
        assert_eq!(engine.sample_pos(), METER_PERIOD);
        assert_eq!(
            engine.transport_frame(),
            METER_PERIOD,
            "transport advances while playing"
        );

        // Stop: the absolute meter clock keeps advancing, transport freezes.
        engine.set_controls(
            &Controls {
                playing: false,
                ..Default::default()
            },
            2,
        );
        run_samples(&mut engine, METER_PERIOD);
        assert_eq!(
            engine.sample_pos(),
            METER_PERIOD * 2,
            "capture clock always advances"
        );
        assert_eq!(
            engine.transport_frame(),
            METER_PERIOD,
            "transport frozen while stopped"
        );

        // RTZ: reset the transport (source-phase) clock without disturbing capture.
        engine.seek(0);
        assert_eq!(engine.transport_frame(), 0);
        assert_eq!(engine.sample_pos(), METER_PERIOD * 2);
    }

    #[test]
    fn coefficient_smoothing_is_fixed_duration_regardless_of_delta() {
        // The master coefficient must reach its new target in EXACTLY RAMP_SAMPLES
        // samples, independent of how large the change is (fixed DURATION, not a
        // fixed per-sample step). Verified white-box on the smoothed shadow.
        for fader_db in [-3.0f64, -40.0] {
            let mut engine = MixerEngine::new(false);
            let c = Controls {
                master: MasterControl {
                    fader_db,
                    mute: false,
                },
                ..Default::default()
            };
            engine.set_controls(&c, 1);
            let tgt = engine.master_tgt;
            assert_ne!(
                engine.master_cur, tgt,
                "ramp not started yet ({fader_db} dB)"
            );
            let mut scratch = Vec::new();
            // One sample short of the ramp end: still moving, not yet at target.
            engine.process_block(RAMP_SAMPLES as usize - 1, &mut scratch);
            assert_ne!(
                engine.master_cur, tgt,
                "must not reach target before {RAMP_SAMPLES} samples ({fader_db} dB)"
            );
            assert_eq!(engine.master_rem, 1);
            // The final sample snaps exactly onto the target.
            engine.process_block(1, &mut scratch);
            assert_eq!(
                engine.master_cur, tgt,
                "reaches target at exactly {RAMP_SAMPLES} samples ({fader_db} dB)"
            );
            assert_eq!(engine.master_rem, 0, "ramp settled");
        }
    }

    #[test]
    fn process_block_audio_writes_clamped_range_stereo() {
        // The real cpal path: the engine writes pre-clamp master stereo the
        // callback clamps. A full unity mix must produce non-silent output that,
        // once clamped, stays within [-1, 1].
        let mut engine = MixerEngine::new(false);
        engine.set_controls(
            &Controls {
                playing: true,
                ..Default::default()
            },
            1,
        );
        let mut l = vec![0.0f32; BLOCK];
        let mut r = vec![0.0f32; BLOCK];
        let mut meters = Vec::new();
        let mut any_signal = false;
        // Run several blocks so the meter frame + audible signal are established.
        for _ in 0..10 {
            engine.process_block_audio(&mut l, &mut r, &mut meters);
            for (&sl, &sr) in l.iter().zip(r.iter()) {
                assert!(sl.is_finite() && sr.is_finite());
                if sl.clamp(-1.0, 1.0).abs() > 1e-4 || sr.clamp(-1.0, 1.0).abs() > 1e-4 {
                    any_signal = true;
                }
            }
        }
        assert!(
            any_signal,
            "real audio path must produce a non-silent stereo signal"
        );
        assert_eq!(engine.sample_pos(), BLOCK as u64 * 10);
    }

    /// A dependency-free deterministic fingerprint over every meter frame,
    /// **excluding** `frame0_mono` (the only non-deterministic field — a live
    /// CLOCK_MONOTONIC stamp). FNV-1a over seq/capture_frame/applied_rev/flags +
    /// all 33 records' six levels + clip byte. Any DSP or wire change moves it.
    fn frames_fingerprint(frames: &[MeterFrame]) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut mix = |bytes: &[u8]| {
            for &b in bytes {
                h ^= b as u64;
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        for f in frames {
            mix(&f.seq.to_le_bytes());
            mix(&f.capture_frame.to_le_bytes());
            mix(&f.applied_rev.to_le_bytes());
            mix(&f.flags.to_le_bytes());
            for r in &f.records {
                for lvl in [r.rms_l, r.rms_r, r.peak_l, r.peak_r, r.hold_l, r.hold_r] {
                    mix(&lvl.to_le_bytes());
                }
                mix(&[r.clip]);
            }
        }
        h
    }

    #[test]
    fn benchmark_multitone_output_is_golden() {
        // GOLDEN regression: the benchmark profile's meter output over the first
        // 10 windows must stay byte-identical (cross-run comparability, harness
        // §D3/§D8). A change to the multitone algorithm, calibration, metering,
        // or wire layout breaks this — recompute the golden ONLY on a deliberate,
        // reviewed benchmark change.
        let mut engine = MixerEngine::new(false); // benchmark profile, real DSP
        engine.set_controls(
            &Controls {
                playing: true,
                ..Default::default()
            },
            1,
        );
        let frames = run_samples(&mut engine, METER_PERIOD * 10);
        assert_eq!(frames.len(), 10);
        // The benchmark profile is real DSP with the benchmark source: NO flags.
        for f in &frames {
            assert_eq!(
                f.flags, 0,
                "benchmark real-DSP frames carry neither guard flag"
            );
        }
        assert_eq!(
            frames_fingerprint(&frames),
            0xbacf_a1e7_1234_f94a,
            "benchmark multitone output drifted from the frozen golden"
        );
    }

    /// A synthetic 48 kHz mono f32 WAV of `n` frames whose sample `i` is a fixed
    /// function of `i` (a gentle ramp), decoded back through the real
    /// [`decode_wav_mono`] path — so the test exercises decode, not just synth.
    fn synth_stem(n: usize) -> Vec<f32> {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: SR,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut buf = std::io::Cursor::new(Vec::<u8>::new());
        {
            let mut w = hound::WavWriter::new(&mut buf, spec).unwrap();
            for i in 0..n {
                w.write_sample(0.1 + 0.2 * ((i % 7) as f32)).unwrap();
            }
            w.finalize().unwrap();
        }
        decode_wav_mono(buf.get_ref(), "synthetic").expect("decode synthetic stem")
    }

    /// Finding #10 regression: a hash-valid float WAV carrying a non-finite
    /// (NaN/inf) sample must be rejected at decode, never fed to the RT engine /
    /// meter accumulators.
    #[test]
    fn decode_wav_mono_rejects_non_finite() {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: SR,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut buf = std::io::Cursor::new(Vec::<u8>::new());
        {
            let mut w = hound::WavWriter::new(&mut buf, spec).unwrap();
            w.write_sample(0.5f32).unwrap();
            w.write_sample(f32::NAN).unwrap();
            w.write_sample(f32::INFINITY).unwrap();
            w.finalize().unwrap();
        }
        let err = decode_wav_mono(buf.get_ref(), "nan-stem").unwrap_err();
        assert!(
            err.to_string().contains("non-finite"),
            "expected a non-finite rejection, got: {err}"
        );
    }

    #[test]
    fn stem_session_plays_indexed_by_transport_and_flags_non_bench() {
        // Channel 0 gets a 400-frame stem; channel 1 gets none (silent). A stem
        // longer than the requested read is zero-padded to length_frames = 900.
        let mut stems: [Vec<f32>; NUM_CHANNELS] = std::array::from_fn(|_| Vec::new());
        stems[0] = synth_stem(400);
        let bank = StemBank::new(stems, 900);
        assert_eq!(bank.loaded_channels(), 1, "only channel 0 carries a stem");
        assert_eq!(bank.length_frames(), 900);

        let mut engine = MixerEngine::with_profile(false, SourceProfile::StemSession(bank));
        assert!(!engine.source_profile().is_benchmark());

        // The stem samples come out on channel 0 and are index-driven by the
        // transport clock — the raw source at frame f is exactly synth_stem[f].
        let stem = synth_stem(400);
        assert_eq!(engine.source(0, 0), stem[0]);
        assert_eq!(engine.source(0, 399), stem[399]);
        // Past the (zero-padded) stem but within length_frames: silence.
        assert_eq!(engine.source(0, 500), 0.0);
        // Past length_frames (EOF): silence.
        assert_eq!(engine.source(0, 100_000), 0.0);
        // A channel with no stem is always silent.
        assert_eq!(engine.source(1, 0), 0.0);
        assert_eq!(engine.source(1, 10), 0.0);

        // Drive playback: channel 0 must carry signal; a stem-less channel + the
        // master (a mix of 32 near-silent channels) stay well below the floor for
        // channel 1. Every frame must set FLAG_NON_BENCH_SOURCE and NOT
        // FLAG_SIMULATOR (real DSP, musical source).
        engine.set_controls(
            &Controls {
                playing: true,
                ..Default::default()
            },
            1,
        );
        let frames = run_samples(&mut engine, METER_PERIOD * 2);
        // The 400-frame stem sounds in the first meter window (frames 0..800);
        // the second window (frames 800..1600) is past the stem, hence silent.
        let first = &frames[0];
        assert!(
            first.records[0].rms_l > CENTI_DB_MIN,
            "channel 0 stem is audible"
        );
        assert_eq!(
            first.records[1].rms_l, CENTI_DB_MIN,
            "stem-less channel 1 silent"
        );
        assert_eq!(
            frames[1].records[0].rms_l, CENTI_DB_MIN,
            "past-EOF window is silent (stem ended)"
        );
        for f in &frames {
            assert_eq!(
                f.flags, FLAG_NON_BENCH_SOURCE,
                "stem frames flag non-benchmark"
            );
            assert!(
                !f.is_simulator(),
                "stem profile is real DSP, not the simulator"
            );
        }

        // Seek/RTZ drives the stem sample-synchronously via the transport clock.
        engine.seek(0);
        assert_eq!(engine.transport_frame(), 0);
        assert_eq!(engine.source(0, engine.transport_frame()), stem[0]);
    }

    #[test]
    fn stem_bank_zero_pads_shorter_stems_to_equal_length() {
        // Two stems of unequal natural length, both padded to length_frames so
        // they stay sample-synchronous; the shorter reads its zero tail as 0.0.
        let mut stems: [Vec<f32>; NUM_CHANNELS] = std::array::from_fn(|_| Vec::new());
        stems[0] = synth_stem(100);
        stems[5] = synth_stem(250);
        let mut bank = StemBank::new(stems, 250);
        assert_eq!(bank.loaded_channels(), 2);
        // Channel 0's padded tail (frames 100..250) is silence; channel 5 plays.
        assert_eq!(bank.sample(0, 150), 0.0);
        assert_ne!(bank.sample(5, 150), 0.0);
        // Both silent past the shared logical end.
        assert_eq!(bank.sample(0, 250), 0.0);
        assert_eq!(bank.sample(5, 250), 0.0);
    }

    /// R0 identity: an unedited bank (the synthesised full-length region per
    /// stem) plays bit-identically to a plain buffer read at every frame.
    #[test]
    fn default_regions_play_bit_identical_to_the_source() {
        let mut stems: [Vec<f32>; NUM_CHANNELS] = std::array::from_fn(|_| Vec::new());
        stems[0] = synth_stem(300);
        let reference = {
            let mut padded = stems[0].clone();
            padded.resize(300, 0.0);
            padded
        };
        let mut bank = StemBank::new(stems, 300);
        for (frame, &expected) in reference.iter().enumerate() {
            assert_eq!(bank.sample(0, frame as u64), expected);
        }
        assert_eq!(bank.sample(0, 300), 0.0);
    }

    /// Trim + move + slip: a region window reads `source_start..` placed at
    /// `timeline_start`, silent outside its window.
    #[test]
    fn region_window_offsets_and_silence() {
        let mut stems: [Vec<f32>; NUM_CHANNELS] = std::array::from_fn(|_| Vec::new());
        stems[0] = synth_stem(300);
        let source = stems[0].clone();
        let mut bank = StemBank::new(stems, 300).with_channel_regions(
            0,
            vec![Region {
                timeline_start: 50,
                source_start: 120,
                len: 40,
                gain: 1.0,
                fade_in: 0,
                fade_out: 0,
            }],
        );
        assert_eq!(bank.sample(0, 49), 0.0);
        assert_eq!(bank.sample(0, 50), source[120]);
        assert_eq!(bank.sample(0, 89), source[159]);
        assert_eq!(bank.sample(0, 90), 0.0);
    }

    /// Split invariance: two abutting regions sharing one source play the
    /// original signal across the seam.
    #[test]
    fn split_regions_are_seamless() {
        let mut stems: [Vec<f32>; NUM_CHANNELS] = std::array::from_fn(|_| Vec::new());
        stems[0] = synth_stem(300);
        let source = stems[0].clone();
        let split_at = 137;
        let mut bank = StemBank::new(stems, 300).with_channel_regions(
            0,
            vec![
                Region {
                    timeline_start: split_at,
                    source_start: split_at,
                    len: 300 - split_at,
                    ..Region::full(300)
                },
                Region {
                    len: split_at,
                    ..Region::full(300)
                },
            ],
        );
        // with_channel_regions sorts, so the out-of-order input above also
        // proves the sort; playback must equal the unsplit source everywhere.
        for (frame, &expected) in source.iter().enumerate() {
            assert_eq!(bank.sample(0, frame as u64), expected, "frame {frame}");
        }
    }

    /// Additive overlap (the chosen engine semantics): two regions covering
    /// one frame sum their contributions.
    #[test]
    fn overlapping_regions_sum() {
        let mut stems: [Vec<f32>; NUM_CHANNELS] = std::array::from_fn(|_| Vec::new());
        stems[0] = vec![0.25; 100];
        let mut bank = StemBank::new(stems, 100).with_channel_regions(
            0,
            vec![
                Region::full(100),
                Region {
                    timeline_start: 40,
                    source_start: 0,
                    len: 20,
                    gain: 2.0,
                    fade_in: 0,
                    fade_out: 0,
                },
            ],
        );
        assert_eq!(bank.sample(0, 10), 0.25);
        assert_eq!(bank.sample(0, 50), 0.25 + 0.5);
        assert_eq!(bank.sample(0, 70), 0.25);
    }

    /// Gain and linear fades: silent first/last frame, half-way mid-ramp,
    /// unity in the body; fades multiply when they overlap.
    #[test]
    fn region_gain_and_fades_ramp_linearly() {
        let mut stems: [Vec<f32>; NUM_CHANNELS] = std::array::from_fn(|_| Vec::new());
        stems[0] = vec![1.0; 100];
        let mut bank = StemBank::new(stems, 100).with_channel_regions(
            0,
            vec![Region {
                timeline_start: 0,
                source_start: 0,
                len: 100,
                gain: 0.5,
                fade_in: 10,
                fade_out: 20,
            }],
        );
        assert_eq!(bank.sample(0, 0), 0.0);
        assert_eq!(bank.sample(0, 5), 0.5 * 0.5);
        assert_eq!(bank.sample(0, 50), 0.5);
        assert_eq!(bank.sample(0, 89), 0.5 * (10.0 / 20.0));
        assert_eq!(bank.sample(0, 99), 0.0);
    }

    /// The dead-prefix cursor is a pure cache: sampling backwards after
    /// playing past a region's end must replay it exactly (seek-back reset).
    #[test]
    fn region_cursor_resets_on_backward_seek() {
        let mut stems: [Vec<f32>; NUM_CHANNELS] = std::array::from_fn(|_| Vec::new());
        stems[0] = synth_stem(300);
        let source = stems[0].clone();
        let mut bank = StemBank::new(stems, 300).with_channel_regions(
            0,
            vec![
                Region {
                    len: 100,
                    ..Region::full(300)
                },
                Region {
                    timeline_start: 200,
                    source_start: 200,
                    len: 100,
                    ..Region::full(300)
                },
            ],
        );
        // Play past the first region's end so the cursor skips it...
        assert_eq!(bank.sample(0, 250), source[250]);
        // ...then seek back inside it: the cursor must rewind, not silence it.
        assert_eq!(bank.sample(0, 50), source[50]);
        assert_eq!(bank.sample(0, 150), 0.0, "the gap stays silent");
    }

    /// Moving audio past the manifest length extends the live timeline (and
    /// pulling it back restores the base) — edited tails stay reachable.
    #[test]
    fn region_edits_rederive_the_timeline_length() {
        let mut stems: [Vec<f32>; NUM_CHANNELS] = std::array::from_fn(|_| Vec::new());
        stems[0] = vec![1.0; 100];
        let bank = StemBank::new(stems, 100);
        assert_eq!(bank.length_frames(), 100);
        let mut bank = bank.with_channel_regions(
            0,
            vec![Region {
                timeline_start: 500,
                source_start: 0,
                len: 100,
                gain: 1.0,
                fade_in: 0,
                fade_out: 0,
            }],
        );
        assert_eq!(bank.length_frames(), 600);
        assert_eq!(bank.sample(0, 550), 1.0);
        let bank = bank.with_channel_regions(0, vec![Region::full(100)]);
        assert_eq!(bank.length_frames(), 100, "shrinks back to the base");
    }

    /// The edit loop's rebuild: from_shared over the original bank's Arcs +
    /// new regions plays the edit, shares the audio, and swaps in live.
    #[test]
    fn shared_rebuild_and_engine_swap() {
        let mut stems: [Vec<f32>; NUM_CHANNELS] = std::array::from_fn(|_| Vec::new());
        stems[0] = synth_stem(300);
        let bank = StemBank::new(stems, 300);
        let sources = bank.stems().clone();
        let source0 = sources[0].clone();
        let mut engine = MixerEngine::with_profile(false, SourceProfile::StemSession(bank));

        // Rebuild off-"thread": same audio Arcs, region moved to frame 100.
        let mut rebuilt = Box::new(StemBank::from_shared(sources, 300).with_channel_regions(
            0,
            vec![Region {
                timeline_start: 100,
                source_start: 0,
                len: 300,
                gain: 1.0,
                fade_in: 0,
                fade_out: 0,
            }],
        ));
        assert!(engine.swap_stem_bank(&mut rebuilt));
        // The displaced bank came back sharing the same audio.
        assert!(Arc::ptr_eq(&rebuilt.stems()[0], &source0));
        // And playback follows the NEW regions: silence before 100, the
        // source's first frame at 100.
        assert_eq!(engine.source(0, 50), 0.0);
        assert_eq!(engine.source(0, 100), source0[0]);
    }

    /// Builder sanitising: zero-length regions are dropped and non-finite
    /// gains muted before the RT thread ever trusts the metadata.
    #[test]
    fn region_builder_sanitises_metadata() {
        let mut stems: [Vec<f32>; NUM_CHANNELS] = std::array::from_fn(|_| Vec::new());
        stems[0] = vec![1.0; 100];
        let mut bank = StemBank::new(stems, 100).with_channel_regions(
            0,
            vec![
                Region {
                    len: 0,
                    ..Region::full(100)
                },
                Region {
                    gain: f32::NAN,
                    ..Region::full(100)
                },
            ],
        );
        assert_eq!(bank.regions()[0].len(), 1, "zero-length region dropped");
        assert_eq!(bank.sample(0, 50), 0.0, "NaN gain muted, not propagated");
    }

    /// A region window reaching past its source's end reads the overrun as
    /// silence (never a panic, never garbage).
    #[test]
    fn region_past_source_end_is_silent() {
        let mut stems: [Vec<f32>; NUM_CHANNELS] = std::array::from_fn(|_| Vec::new());
        stems[0] = vec![1.0; 100];
        let mut bank = StemBank::new(stems, 100).with_channel_regions(
            0,
            vec![Region {
                timeline_start: 0,
                source_start: 80,
                len: 60,
                gain: 1.0,
                fade_in: 0,
                fade_out: 0,
            }],
        );
        assert_eq!(bank.sample(0, 19), 1.0);
        assert_eq!(bank.sample(0, 20), 0.0);
        assert_eq!(bank.sample(0, 59), 0.0);
    }

    /// Trust boundary (MAJOR): a seek beyond the finite source length clamps to
    /// `length_frames`, and an absurd seek value (as a domain-blind renderer
    /// could canonicalize under `0..f64::MAX`) does NOT land at ~u64::MAX and
    /// silence the transport — it lands at the end.
    #[test]
    fn seek_clamps_to_length_for_finite_source() {
        let mut stems: [Vec<f32>; NUM_CHANNELS] = std::array::from_fn(|_| Vec::new());
        stems[0] = synth_stem(400);
        let bank = StemBank::new(stems, 900);
        let mut engine = MixerEngine::with_profile(false, SourceProfile::StemSession(bank));

        // In-range seek is exact.
        engine.seek(500);
        assert_eq!(engine.transport_frame(), 500);
        // Beyond the end clamps to length_frames.
        engine.seek(10_000);
        assert_eq!(engine.transport_frame(), 900);
        // The crafted-value path (value 1e300 → secs*SR saturates to u64::MAX):
        // clamps to length_frames, never u64::MAX.
        let crafted = (1e300_f64 * SR as f64).round() as u64;
        assert_eq!(
            crafted,
            u64::MAX,
            "the saturating cast reproduces the attack input"
        );
        engine.seek(crafted);
        assert_eq!(
            engine.transport_frame(),
            900,
            "huge seek lands at the end, not u64::MAX"
        );
    }

    /// EOF (MINOR): playing past the source end clamps the transport at
    /// `length_frames`, so the published position never exceeds the total (no
    /// "6:00 / 5:06" readout).
    #[test]
    fn playback_clamps_transport_at_eof_for_finite_source() {
        let mut stems: [Vec<f32>; NUM_CHANNELS] = std::array::from_fn(|_| Vec::new());
        stems[0] = synth_stem(400);
        let bank = StemBank::new(stems, 900);
        let mut engine = MixerEngine::with_profile(false, SourceProfile::StemSession(bank));
        engine.set_controls(
            &Controls {
                playing: true,
                ..Default::default()
            },
            1,
        );
        // Drive well past the 900-frame end.
        let _ = run_samples(&mut engine, 2_000);
        assert_eq!(
            engine.transport_frame(),
            900,
            "transport clamps at length_frames instead of running past EOF"
        );
    }

    /// The multitone (unbounded) source is NOT clamped — it seeks + advances
    /// freely (no finite length).
    #[test]
    fn unbounded_source_seek_and_advance_not_clamped() {
        let mut engine = MixerEngine::new(true); // benchmark multitone
        assert_eq!(engine.source_profile().transport_len_frames(), None);
        engine.seek(10_000_000);
        assert_eq!(
            engine.transport_frame(),
            10_000_000,
            "unbounded seek is exact"
        );
        engine.set_controls(
            &Controls {
                playing: true,
                ..Default::default()
            },
            1,
        );
        let _ = run_samples(&mut engine, 100);
        assert_eq!(
            engine.transport_frame(),
            10_000_100,
            "unbounded transport keeps advancing"
        );
    }

    /// FIX A (MAJOR crash): an unbounded source seeked to ~u64::MAX then advanced
    /// must saturate, not overflow (`+= 1` panics in debug / wraps in release).
    #[test]
    fn unbounded_source_advance_saturates_at_u64_max() {
        let mut engine = MixerEngine::new(true); // unbounded multitone
        engine.seek(u64::MAX);
        assert_eq!(
            engine.transport_frame(),
            u64::MAX,
            "unbounded seek is exact"
        );
        engine.set_controls(
            &Controls {
                playing: true,
                ..Default::default()
            },
            1,
        );
        // Advancing past u64::MAX must NOT overflow — it saturates.
        let _ = run_samples(&mut engine, 100);
        assert_eq!(
            engine.transport_frame(),
            u64::MAX,
            "advance saturates at u64::MAX"
        );
    }
}
