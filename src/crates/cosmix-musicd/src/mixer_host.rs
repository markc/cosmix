use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};

use crate::render::RenderFormat;
use crate::rt_sched::{self, AudioRuntime, AudioWake};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use rtrb::{Consumer, Producer};
use serde_json::Value as Json;
use sha2::Digest as _;
use tracing::{error, info, warn};

use cosmix_mixer_schema::{
    LeafSnapshot, LeafValue, METER_FRAME_LEN, METER_LEVEL_LEAVES, MeterFrame,
    MixerSnapshotResponse, NUM_CHANNELS, SOURCE_PROFILE_STEM, WriteAck, WriteBusy, WriteReject,
    WriteRequest, WriteResponse, canonicalize_write, leaf_default,
};
pub use cosmix_props_core::revwrite::{RevWriteRequest, RevWriteResponse, RevWriteStore};
pub use cosmix_props_core::{PropPath, PropValue};

use crate::mixer::{
    BLOCK, Controls, MidiSynthBank, MixerEngine, NoteEvent, Region, SR, SongMeta, SourceProfile,
    StemBank, TrackSchedule, decode_wav_mono, now_mono_ns,
};

/// Bus rc for an accepted write.
pub const RC_OK: u8 = 0;
/// Bus rc for a **retryable** BUSY write (RT control ring momentarily full). The
/// `WriteResponse` body carries `status:"busy"`; a distinct rc lets a client fast
/// -path a retry without parsing the body.
pub const RC_BUSY: u8 = 4;
/// Bus rc for a durable rejection (validation / if_revision / auth).
pub const RC_REJECT: u8 = 10;

/// Live, read-only runtime telemetry exposed by the daemon property surface.
pub const RT_PRIORITY_PATH: &str = "mixer.rt_priority";
/// Soft `RLIMIT_RTTIME` protecting the audio thread, in microseconds; `0` when
/// the RT deadman switch is not armed. Read-only daemon-owned telemetry, like
/// the two leaves either side of it.
pub const RT_TIME_US_PATH: &str = "mixer.rt_time_us";
pub const BLOCK_FRAMES_PATH: &str = "mixer.block_frames";

/// Bound on the RT-side applied-notification backlog (each entry preserving its
/// TRUE latch frame until delivered to the return ring). The RT signals the
/// applied ring's (256) eventfd on every push, so the async side drains on the
/// wake rather than on a clock, and the RT latches at most one group per
/// 128-block, so this
/// only ever fills if the async side stalls badly — at which point we cannot keep
/// preserving true timestamps, so the run is flagged integrity-faulted rather than
/// corrupting a `dsp.applied` latch frame with a later block's value.
const APPLIED_BACKLOG_CAP: usize = 512;

// ===========================================================================
// Depth-1, latest-wins meter mailbox (seqlock) — RT writer, async reader.
// ===========================================================================

/// A single-writer / single-reader latest-wins mailbox for the encoded
/// 465-byte meter frame. Implemented as a **seqlock** over `[AtomicU8; 465]`:
/// an odd sequence brackets a write so a reader retries a torn snapshot.
///
/// Every sequence *and* byte operation is [`SeqCst`](Ordering::SeqCst) (BLOCKER-2
/// fix). Under one global total order, a reader that reads an even `s1`, then the
/// bytes, then an equal even `s2` is guaranteed a **whole** frame: any concurrent
/// write stores an odd sequence *before* its byte stores, so a reader that
/// observed a new byte necessarily observes `s2 >= s1 + 1 != s1` and retries — a
/// torn read can never satisfy `s1 == s2`. Relaxed byte atomics gave no such
/// guarantee (a stale even `s2` could pair with a partially-new snapshot). The
/// writer is wait-free; the whole cost is ~465 byte-ops per 60 Hz frame
/// (~27.9 k ops/s), negligible (Q8 "meter mailbox depth 1"; harness A.6).
pub struct MeterMailbox {
    /// Even = stable, odd = a write is in progress. Monotonic (wrapping).
    seq: AtomicU32,
    /// The latest encoded frame bytes, each an atomic for race-freedom.
    bytes: [AtomicU8; METER_FRAME_LEN],
    /// Set once the first frame is published (before that, `read` yields None).
    ready: AtomicBool,
}

#[allow(clippy::new_without_default)]
impl MeterMailbox {
    pub fn new() -> Self {
        MeterMailbox {
            seq: AtomicU32::new(0),
            bytes: std::array::from_fn(|_| AtomicU8::new(0)),
            ready: AtomicBool::new(false),
        }
    }

    /// RT writer: publish the latest encoded frame. Wait-free, alloc-free.
    pub fn publish(&self, frame: &[u8; METER_FRAME_LEN]) {
        // Single writer, so loading its own sequence is exact.
        let s = self.seq.load(Ordering::SeqCst);
        // Enter the write: mark the sequence odd, before any byte store.
        self.seq.store(s.wrapping_add(1), Ordering::SeqCst);
        for (slot, &b) in self.bytes.iter().zip(frame.iter()) {
            slot.store(b, Ordering::SeqCst);
        }
        // Leave the write: even again, after every byte store.
        self.seq.store(s.wrapping_add(2), Ordering::SeqCst);
        self.ready.store(true, Ordering::SeqCst);
    }

    /// Reader: the latest published frame, or `None` before the first publish.
    /// Retries on a torn read (writer mid-update); at 60 Hz vs 60 Hz this
    /// effectively never spins more than once.
    pub fn read(&self) -> Option<[u8; METER_FRAME_LEN]> {
        if !self.ready.load(Ordering::SeqCst) {
            return None;
        }
        loop {
            let s1 = self.seq.load(Ordering::SeqCst);
            if s1 & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let mut out = [0u8; METER_FRAME_LEN];
            for (dst, slot) in out.iter_mut().zip(self.bytes.iter()) {
                *dst = slot.load(Ordering::SeqCst);
            }
            let s2 = self.seq.load(Ordering::SeqCst);
            if s1 == s2 {
                return Some(out);
            }
        }
    }
}

/// Where completed meter frames go once a block finishes processing.
pub trait MeterSink: Send + 'static {
    fn publish(&mut self, frame: &MeterFrame);
}

/// Today's daemon path: encode to the 465-byte A.6 wire form and publish into
/// the depth-1 latest-wins seqlock mailbox — byte-identical to current behavior.
pub struct MailboxSink(pub Arc<MeterMailbox>);
impl MeterSink for MailboxSink {
    fn publish(&mut self, frame: &MeterFrame) {
        self.0.publish(&frame.encode());
    }
}

/// An in-process consumer's path: push the typed frame directly into an rtrb
/// ring, no A.6 encode/decode. Silently drops the frame if the ring is full
/// (latest-wins is achieved by the consumer draining and keeping only the
/// newest — NOT this sink's job to do that).
pub struct RingSink(pub rtrb::Producer<MeterFrame>);
impl MeterSink for RingSink {
    fn publish(&mut self, frame: &MeterFrame) {
        let _ = self.0.push(frame.clone());
    }
}

// ===========================================================================
// RT thread <-> async plumbing.
// ===========================================================================

/// One command from the async side to the RT audio thread. `Copy` so it moves
/// through the `rtrb` ring with no allocation. `SetControls` ships the **full**
/// authoritative [`Controls`] snapshot each time, so RT-side coalescing (keep
/// the newest) can never lose an intermediate leaf change — the newest snapshot
/// already carries every accumulated write.
// SetControls ships the full ~1.3 KB Controls snapshot; ResetLatch is tiny. The
// size gap is deliberate (full-snapshot shipping makes RT coalescing lossless),
// and control writes are infrequent, so boxing to shrink the ring element would
// add a heap alloc on the write path for no benefit.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Copy, Debug)]
pub enum RtCommand {
    SetControls {
        controls: Controls,
        revision: u64,
    },
    ResetLatch {
        meter: usize,
        revision: u64,
    },
    /// Transport seek (RTZ = `frame == 0`): reset the source clock/phase. Carries
    /// a revision so `applied_rev` reflects it (a `transport.position` write).
    Seek {
        frame: u64,
        revision: u64,
    },
    /// A live (unscheduled) note for a synth-profile channel — insert-mode
    /// preview, audible playing or stopped. Transient: carries no revision and
    /// never touches the leaf store or `applied_rev`. Ignored by non-synth
    /// profiles.
    NoteEvent {
        channel: usize,
        key: u8,
        vel: u8,
        on: bool,
    },
}

/// A freshly-built song bank plus WHY it is being swapped in. A whole-document
/// `Load` (File > Open / Bus `app.song.load`) must land stopped at frame zero,
/// so the RT thread applies the load barrier after installing it; an `Edit`
/// (piano-roll edit, undo/redo, SoundFont swap) preserves the playing state and
/// playhead. The displaced bank returns as a bare `Box` (no tag needed).
pub struct SongBankSwap {
    pub bank: Box<crate::mixer::MidiSynthBank>,
    /// `true` = a document load (barrier: stop + rewind to zero); `false` = a
    /// live edit (preserve transport).
    pub load: bool,
}

/// The RT-side half of the song-swap channel pair: freshly-built banks arrive
/// on `new_rx`; every displaced (or rejected) bank leaves on `old_tx` so its
/// deallocation happens off the audio thread. Create the pair with
/// [`song_swap_rings`].
pub struct SongSwap {
    pub new_rx: Consumer<SongBankSwap>,
    pub old_tx: Producer<Box<crate::mixer::MidiSynthBank>>,
    /// The ACTIVE bank's timeline length in frames, stored by the RT thread
    /// after every accepted swap (0 until then) — mirrored into the
    /// `transport.length` leaf so song edits that lengthen the song update
    /// seeks/footer/scrubber (same mechanism as [`StemSwap`]).
    pub active_length: Arc<std::sync::atomic::AtomicU64>,
}

/// Build the lock-free song-swap plumbing: returns `(new_tx, swap, old_rx)`.
/// The host pushes freshly-built banks into `new_tx` (see
/// [`song_bank_with`]), hands `swap` to [`RtState::with_song_swap`], and MUST
/// drain `old_rx` (dropping the returned banks) so old-bank deallocation
/// happens off the RT thread. Both rings share `capacity`; the RT side only
/// accepts a new bank when the return ring has a slot, so a bank can never be
/// stranded (or dropped) on the audio thread.
#[allow(clippy::type_complexity)]
pub fn song_swap_rings(
    capacity: usize,
) -> (
    Producer<SongBankSwap>,
    SongSwap,
    Consumer<Box<crate::mixer::MidiSynthBank>>,
) {
    let (new_tx, new_rx) = rtrb::RingBuffer::new(capacity);
    let (old_tx, old_rx) = rtrb::RingBuffer::new(capacity);
    (
        new_tx,
        SongSwap {
            new_rx,
            old_tx,
            active_length: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        },
        old_rx,
    )
}

/// The RT-side half of the STEM-bank swap pair (the region-edit path) —
/// the same discipline as [`SongSwap`]: a new bank is only accepted while
/// the return ring has a slot, so deallocation never happens on the audio
/// thread (and the displaced bank's audio `Arc`s are shared anyway).
pub struct StemSwap {
    pub new_rx: Consumer<Box<crate::mixer::StemBank>>,
    pub old_tx: Producer<Box<crate::mixer::StemBank>>,
    /// The ACTIVE bank's timeline length in frames, stored by the RT thread
    /// after every accepted swap (0 until the first swap). The host mirrors
    /// it into the `transport.length` leaf so seeks/footer track edits that
    /// extend the timeline.
    pub active_length: Arc<std::sync::atomic::AtomicU64>,
}

/// Build the stem-swap plumbing: `(new_tx, swap, old_rx)` — push rebuilt
/// banks into `new_tx`, hand `swap` to [`RtState::with_stem_swap`], and
/// drain `old_rx` (dropping returned banks) off-thread.
#[allow(clippy::type_complexity)]
pub fn stem_swap_rings(
    capacity: usize,
) -> (
    Producer<Box<crate::mixer::StemBank>>,
    StemSwap,
    Consumer<Box<crate::mixer::StemBank>>,
) {
    let (new_tx, new_rx) = rtrb::RingBuffer::new(capacity);
    let (old_tx, old_rx) = rtrb::RingBuffer::new(capacity);
    (
        new_tx,
        StemSwap {
            new_rx,
            old_tx,
            active_length: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        },
        old_rx,
    )
}

/// RT → async report: a revision was latched into the audio graph at
/// `sample_frame`. Drives the `dsp.applied` event.
#[derive(Clone, Copy, Debug)]
pub struct AppliedMsg {
    pub revision: u64,
    pub sample_frame: u64,
}

/// Shared control state behind a tokio `Mutex` (the write serialisation point —
/// `apply` order = server receive order = authoritative).
pub struct MixerCtl {
    /// The opt-in revisioned write facility (Q8).
    pub store: RevWriteStore,
    /// The authoritative control-coefficient mirror shipped to the RT thread.
    pub controls: Controls,
    /// Producer end of the control ring (RT holds the consumer).
    pub ctrl_tx: Producer<RtCommand>,
    /// revision → originating path, so a `dsp.applied {revision}` can name the
    /// path it applied. Pruned as revisions are reported applied (bounded).
    pub rev_path: BTreeMap<u64, PropPath>,
}

// ===========================================================================
// The RT audio engine (real cpal output, with a paced no-device fallback).
// ===========================================================================

/// All RT-thread-owned state: the DSP engine plus its lock-free links to the
/// async side. Moves either into the `!Send` cpal callback (real audio) or into
/// the paced fallback loop (no device) — it is never shared, so its `MixerEngine`
/// needs no synchronisation. `Send` (rings + `Arc`s are `Send`), so it can cross
/// into the audio thread / callback.
pub struct RtState<M: MeterSink> {
    /// The real DSP engine (frames carry flags=0, `mixer.engine="dsp"`).
    engine: MixerEngine,
    /// Control ring consumer (async producer → RT).
    ctrl_rx: Consumer<RtCommand>,
    /// Applied-revision return ring producer (RT → async `dsp.applied`).
    applied_tx: Producer<AppliedMsg>,
    /// Completed meter frame sink.
    sink: M,
    /// Live transport position in frames (RT writes each block; snapshot reads).
    transport_pos: Arc<AtomicU64>,
    /// Highest revision already captured into the applied backlog (advanced when a
    /// latch group is queued with its TRUE frame — not when finally delivered).
    last_reported_rev: u64,
    /// FIFO of applied latch groups awaiting delivery to `applied_tx`, each holding
    /// its **true** `sample_frame` so a stalled return ring never rewrites a
    /// timestamp with a later block's frame (MAJOR-2). Bounded by
    /// [`APPLIED_BACKLOG_CAP`]; overflow trips `applied_fault`.
    pending_applied: VecDeque<AppliedMsg>,
    /// Sticky run-integrity fault: the applied backlog overflowed, so a latch
    /// timestamp could no longer be preserved truthfully. benchd disqualifies the
    /// run. Latches `true`, never clears.
    applied_fault: Arc<AtomicBool>,
    /// Pre-sized meter-frame scratch (alloc-free hot path).
    scratch: Vec<MeterFrame>,
    /// Opt-in song-swap plumbing (the live-edit path); `None` on hosts that
    /// never re-song (the bench arms). See [`RtState::with_song_swap`].
    song_swap: Option<SongSwap>,
    /// Opt-in stem-bank swap plumbing (the region-edit path).
    stem_swap: Option<StemSwap>,
    /// Opt-in RT→async wake, signalled after a block pushes to `applied_tx` so
    /// the event publisher can sleep instead of polling the ring. `None` on the
    /// non-daemon hosts (tests, bench arms) that have no async side to wake.
    applied_wake: Option<Arc<AudioWake>>,
    /// Opt-in second command ring for transient commands issued OUTSIDE the
    /// revisioned write path (live [`RtCommand::NoteEvent`] previews from the
    /// editing UI). Drained alongside the main ring each block.
    aux_rx: Option<Consumer<RtCommand>>,
}

impl<M: MeterSink> RtState<M> {
    pub fn new(
        ctrl_rx: Consumer<RtCommand>,
        applied_tx: Producer<AppliedMsg>,
        sink: M,
        transport_pos: Arc<AtomicU64>,
        applied_fault: Arc<AtomicBool>,
        profile: SourceProfile,
    ) -> Self {
        // The real DSP engine on the chosen immutable source profile (frames carry
        // flags=0 for the benchmark profile, FLAG_NON_BENCH_SOURCE for stems;
        // `mixer.engine` is "dsp" for both).
        let engine = MixerEngine::with_profile(/* simulator = */ false, profile);
        let last_reported_rev = engine.applied_rev();
        RtState {
            engine,
            ctrl_rx,
            applied_tx,
            sink,
            transport_pos,
            last_reported_rev,
            pending_applied: VecDeque::with_capacity(APPLIED_BACKLOG_CAP),
            applied_fault,
            scratch: Vec::with_capacity(4),
            song_swap: None,
            stem_swap: None,
            applied_wake: None,
            aux_rx: None,
        }
    }

    /// Opt in to live song swapping (see [`song_swap_rings`]). The bench arms
    /// never call this — their profile stays frozen for the process lifetime.
    /// Opt in to live stem-bank swapping (see [`stem_swap_rings`]).
    pub fn with_stem_swap(mut self, swap: StemSwap) -> Self {
        self.stem_swap = Some(swap);
        self
    }

    pub fn with_song_swap(mut self, swap: SongSwap) -> Self {
        self.song_swap = Some(swap);
        self
    }

    /// Opt in to the RT→async wake: after a block pushes applied revisions onto
    /// the return ring, signal the event publisher instead of leaving it to
    /// discover the work on its next tick.
    pub fn with_applied_wake(mut self, wake: Arc<AudioWake>) -> Self {
        self.applied_wake = Some(wake);
        self
    }

    /// Opt in to a second command ring for transient (revision-free) commands
    /// — the editing UI's live note previews.
    pub fn with_aux_commands(mut self, rx: Consumer<RtCommand>) -> Self {
        self.aux_rx = Some(rx);
        self
    }

    /// Process one internal block of `frames` samples (≤ [`BLOCK`]). Drains and
    /// coalesces the control ring, latches once at the block boundary, processes
    /// the block (writing pre-clamp stereo into `audio` on the real path),
    /// publishes every completed meter frame, updates the live transport position,
    /// and reports a newly-latched high-water revision as `dsp.applied`. Alloc-free
    /// and lock-free — the audio-callback discipline.
    pub fn run_block(&mut self, frames: usize, audio: Option<(&mut [f32], &mut [f32])>) {
        // 0. Drain any pending song swaps (live edits + document loads). A new
        //    bank is only accepted while the return ring has a slot, so the
        //    displaced (or, on a non-synth profile, rejected) bank ALWAYS ships
        //    back — no deallocation ever happens on this thread. Record whether
        //    ANY drained swap was a document load: its barrier (force stopped at
        //    zero) fires below, AFTER the control drain, so a stale queued Play
        //    can't restart the just-loaded song before it renders.
        let mut song_load_swapped = false;
        if let Some(swap) = &mut self.song_swap {
            while swap.old_tx.slots() >= 1 {
                let Ok(mut swap_in) = swap.new_rx.pop() else {
                    break;
                };
                if self.engine.swap_midi_bank(&mut swap_in.bank) {
                    // Only a bank that actually INSTALLED arms the load barrier —
                    // a load-tagged bank rejected on a non-MidiSynth profile must
                    // never stop an unrelated (stem/benchmark) transport.
                    song_load_swapped |= swap_in.load;
                    if let Some(len) = self.engine.source_profile().transport_len_frames() {
                        swap.active_length
                            .store(len, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                let _ = swap.old_tx.push(swap_in.bank);
            }
        }
        if let Some(swap) = &mut self.stem_swap {
            // Coalesce a backlog to the NEWEST pending bank: swapping through
            // intermediates would clamp the transport at each intermediate
            // timeline length (an inaudible short-bank edit + its undo could
            // permanently rewind the playhead). Intermediates return unswapped.
            // The batch is taken ONLY when the return ring can hold all of it
            // (queued-1 intermediates + the displaced bank = `queued` slots) —
            // a partial take could install an intermediate as if it were
            // newest. Otherwise wait; the host drains returns every frame.
            let queued = swap.new_rx.slots();
            let mut newest: Option<Box<crate::mixer::StemBank>> = None;
            if queued > 0 && swap.old_tx.slots() >= queued {
                for _ in 0..queued {
                    let Ok(boxed) = swap.new_rx.pop() else {
                        break;
                    };
                    if let Some(previous) = newest.replace(boxed) {
                        let _ = swap.old_tx.push(previous);
                    }
                }
            }
            if let Some(mut boxed) = newest {
                if self.engine.swap_stem_bank(&mut boxed) {
                    if let Some(len) = self.engine.source_profile().transport_len_frames() {
                        swap.active_length
                            .store(len, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                let _ = swap.old_tx.push(boxed);
            }
        }

        // 1. Drain the control ring. Coalesce SetControls (keep the newest full
        //    snapshot); apply latch resets + seeks immediately; track the
        //    high-water revision across every drained command.
        let mut latest_controls: Option<Controls> = None;
        let mut max_rev = self.engine.applied_rev();
        while let Ok(cmd) = self.ctrl_rx.pop() {
            match cmd {
                RtCommand::SetControls { controls, revision } => {
                    latest_controls = Some(controls);
                    if revision > max_rev {
                        max_rev = revision;
                    }
                }
                RtCommand::ResetLatch { meter, revision } => {
                    self.engine.reset_latch(meter);
                    if revision > max_rev {
                        max_rev = revision;
                    }
                }
                RtCommand::Seek { frame, revision } => {
                    self.engine.seek(frame);
                    if revision > max_rev {
                        max_rev = revision;
                    }
                }
                RtCommand::NoteEvent {
                    channel,
                    key,
                    vel,
                    on,
                } => {
                    // Transient preview note: no revision, no store echo.
                    self.engine.live_note(channel, key, vel, on);
                }
            }
        }
        // 1b. Drain the transient aux ring (live previews). Only NoteEvent is
        //     meaningful here — revision-carrying commands belong on the main
        //     ring, so anything else is ignored rather than latched half-way.
        if let Some(aux) = &mut self.aux_rx {
            while let Ok(cmd) = aux.pop() {
                if let RtCommand::NoteEvent {
                    channel,
                    key,
                    vel,
                    on,
                } = cmd
                {
                    self.engine.live_note(channel, key, vel, on);
                }
            }
        }
        if let Some(c) = latest_controls {
            self.engine.set_controls(&c, max_rev);
        } else {
            self.engine.set_applied_rev(max_rev);
        }

        // 1c. Load barrier. A document load installed above (step 0) must render
        //     its FIRST block stopped at frame zero — even if this same block's
        //     control drain just latched a (stale, pre-load) Play. Applied here,
        //     after the control snapshot and before rendering, so no queued
        //     command can restart the just-loaded song. Edits never reach this.
        if song_load_swapped {
            self.engine.stop_at_zero();
        }

        // The absolute audio frame at which this block's revision took effect.
        let latch_frame = self.engine.sample_pos();

        // 2. Process one block; publish every completed meter frame.
        self.scratch.clear();
        match audio {
            Some((l, r)) => self.engine.process_block_audio(l, r, &mut self.scratch),
            None => self.engine.process_block(frames, &mut self.scratch),
        }
        for f in self.scratch.drain(..) {
            self.sink.publish(&f);
        }

        // 3. Live transport position (frames) for the transient read.
        self.transport_pos
            .store(self.engine.transport_frame(), Ordering::Relaxed);

        // 4. Report a newly-latched high-water revision as dsp.applied. The group
        //    is queued into the backlog with THIS block's TRUE `latch_frame` and
        //    only ever delivered with that frame — a stalled return ring can never
        //    rewrite it with a later block's frame (MAJOR-2). If the backlog is
        //    saturated we cannot preserve the timestamp, so we trip the sticky
        //    integrity fault rather than corrupt it.
        let cur_rev = self.engine.applied_rev();
        if cur_rev > self.last_reported_rev {
            if self.pending_applied.len() >= APPLIED_BACKLOG_CAP {
                self.applied_fault.store(true, Ordering::Relaxed);
            } else {
                self.pending_applied.push_back(AppliedMsg {
                    revision: cur_rev,
                    sample_frame: latch_frame,
                });
                self.last_reported_rev = cur_rev;
            }
        }
        // Drain the backlog FIFO into the return ring, stopping at the first full
        // push and keeping the rest (with their true frames) for a later block.
        let mut pushed = false;
        while let Some(&msg) = self.pending_applied.front() {
            if self.applied_tx.push(msg).is_ok() {
                self.pending_applied.pop_front();
                pushed = true;
            } else {
                break;
            }
        }
        // Wake the event publisher. One non-blocking 8-byte eventfd write, only
        // when this block actually enqueued something — the async side sleeps
        // the rest of the time instead of polling the ring.
        if pushed && let Some(w) = &self.applied_wake {
            w.signal();
        }
    }
}

/// Pick the cpal host, and name it for the log.
///
/// `MUSICD_AUDIO_HOST` selects explicitly: `jack` | `alsa` | `default`.
///
/// When the `jack` feature is compiled in, the default is to **prefer JACK**.
/// On a PipeWire box the default host reaches the server the long way round —
/// ALSA `default` → `libasound_module_pcm_pulse.so` → pipewire-pulse, two
/// translation layers and a losing fight with dmix over the raw PCM device —
/// whereas the JACK host lands on pipewire-jack directly. If the JACK server is
/// not reachable the default host is used instead, so this can only improve the
/// path, never remove one.
pub fn mixer_audio_host() -> (cpal::Host, &'static str) {
    let want = std::env::var("MUSICD_AUDIO_HOST")
        .unwrap_or_default()
        .to_ascii_lowercase();
    #[cfg(feature = "jack")]
    if want != "alsa" && want != "default" {
        match cpal::host_from_id(cpal::HostId::Jack) {
            Ok(h) if h.default_output_device().is_some() => return (h, "jack"),
            Ok(_) if want == "jack" => warn!(
                "musicd-mixer: MUSICD_AUDIO_HOST=jack was explicitly requested, but cpal's \
                 JACK host exposed no output device (it swallows JACK device-open failures); \
                 falling back to the default host"
            ),
            Ok(_) => warn!(
                "musicd-mixer: cpal's JACK host exposed no output device (the JACK/PipeWire-JACK \
                 server is unavailable or unusable); falling back to the default host"
            ),
            Err(e) if want == "jack" => warn!(
                "musicd-mixer: MUSICD_AUDIO_HOST=jack but the JACK host is unavailable ({e}); \
                 falling back to the default host"
            ),
            Err(e) => info!("musicd-mixer: JACK host unavailable ({e}); using the default host"),
        }
    }
    #[cfg(not(feature = "jack"))]
    if want == "jack" {
        warn!(
            "musicd-mixer: MUSICD_AUDIO_HOST=jack but this build has no `jack` feature \
             (rebuild with --features jack); using the default host"
        );
    }
    (cpal::default_host(), "default")
}

/// Spawn the dedicated `"musicd-mixer"` thread. It either owns the `!Send` cpal
/// output stream forever (real audio) or runs the paced no-output fallback — and
/// records which in `real_audio` so benchd/status can tell a real-audio run from a
/// headless one (BLOCKER-3). `audio_fault` latches if the cpal stream later errors.
pub fn spawn_rt_thread<M: MeterSink>(
    rt: RtState<M>,
    real_audio: Arc<AtomicBool>,
    audio_fault: Arc<AtomicBool>,
    runtime: Arc<AudioRuntime>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("musicd-mixer".into())
        .spawn(move || rt_thread_main(rt, real_audio, audio_fault, runtime))
        .expect("spawn musicd-mixer RT thread")
}

pub fn rt_thread_main<M: MeterSink>(
    rt: RtState<M>,
    real_audio: Arc<AtomicBool>,
    audio_fault: Arc<AtomicBool>,
    runtime: Arc<AudioRuntime>,
) {
    // Process-wide page locking, before any stream exists: pages stay resident
    // after first touch, but MCL_ONFAULT deliberately does not pre-fault cold
    // pages, so their first callback touch may still block. Failing soft is
    // deliberate — an unprivileged container must still start, with weaker
    // timing evidence stated explicitly.
    if rt_sched::mlock_enabled() {
        match rt_sched::lock_process_memory() {
            Ok(rt_sched::MemoryLockMode::CurrentAndFutureOnFault) => info!(
                "musicd-mixer: mlockall(MCL_CURRENT|MCL_FUTURE|MCL_ONFAULT) applied; \
                 pages stay resident after first touch"
            ),
            Ok(rt_sched::MemoryLockMode::CurrentOnFault) => warn!(
                "musicd-mixer: mlockall(MCL_CURRENT|MCL_ONFAULT) applied, but future mappings \
                 are deliberately NOT locked because RLIMIT_MEMLOCK is bounded; MCL_FUTURE \
                 could turn a later allocation into ENOMEM or stack growth into SIGSEGV"
            ),
            Err(e) => warn!(
                "musicd-mixer: mlockall failed ({e}); pages may fault in the audio callback \
                 (raise RLIMIT_MEMLOCK, or set MUSICD_MLOCK=0 to stop trying)"
            ),
        }
    }
    let (host, host_name) = mixer_audio_host();
    let Some(device) = host.default_output_device() else {
        warn!(
            "musicd-mixer: no audio output device on the {host_name} host — paced no-output \
             fallback (NOT a real-audio run; mixer.engine stays \"dsp\")"
        );
        paced_no_output(rt, runtime);
        return;
    };
    let Some((fmt, config, channels)) = mixer_output_config(&device) else {
        warn!("musicd-mixer: no {SR} Hz output config on the device — paced no-output fallback");
        paced_no_output(rt, runtime);
        return;
    };
    if !matches!(
        fmt,
        SampleFormat::F32 | SampleFormat::I16 | SampleFormat::U16
    ) {
        warn!("musicd-mixer: unsupported device sample format {fmt:?} — paced no-output fallback");
        paced_no_output(rt, runtime);
        return;
    }
    match build_mixer_stream(
        &device,
        fmt,
        &config,
        channels,
        rt,
        real_audio.clone(),
        audio_fault,
        runtime.clone(),
    ) {
        Ok(stream) => {
            if let Err(e) = stream.play() {
                error!(
                    "musicd-mixer: cpal stream.play() failed: {e}; RT thread exiting \
                     (control still served, meters idle)"
                );
                return;
            }
            real_audio.store(true, Ordering::Release);
            info!(
                "musicd-mixer: REAL cpal audio output active (host={host_name}, {SR} Hz, \
                 {channels} ch, {fmt:?}, buffer request {:?})",
                config.buffer_size
            );
            // One-shot settle: report what the audio path ACTUALLY got, once the
            // first callback has run. A `BufferSize::Fixed` request can be
            // refused or rounded and an RT promotion can be denied, so the
            // requested values above are not evidence — these are. Not a poll:
            // it runs once and then the thread parks for the process lifetime.
            report_audio_runtime_once(&runtime);
            // Own the `!Send` stream for the process lifetime (mirror play.rs).
            loop {
                std::thread::park();
            }
        }
        Err(e) => {
            // Residual: `default_output_device()` proves that cpal created a
            // JACK device, not that stream construction will succeed. JACK
            // opens a second client here, which the server can reject. This is
            // a logged no-output run (`real_audio` stays false), not a silent
            // one. Late fallback needs `RtState` to be recoverable, but it was
            // moved into the callback consumed by the failed build.
            error!(
                "musicd-mixer: building cpal stream failed: {e}; RT thread exiting \
                 (control still served, meters idle)"
            );
        }
    }
}

/// Wait briefly for the first audio callback, then log what the audio path
/// actually got exactly once. Bounded: gives up after [`RUNTIME_SETTLE_TRIES`]
/// short waits and says the callback never ran, which is itself the finding.
fn report_audio_runtime_once(runtime: &AudioRuntime) {
    // Residual: this description is deliberately one-shot, so callback-size
    // variation beginning later is not logged. `mixer.block_frames` remains the
    // durable live evidence and reports the updated maximum. That maximum is a
    // conservative latency bound, not a characterisation of the distribution.
    for _ in 0..RUNTIME_SETTLE_TRIES {
        if let Some(desc) = runtime.describe() {
            info!("musicd-mixer: audio path settled — {desc}");
            return;
        }
        std::thread::sleep(RUNTIME_SETTLE_WAIT);
    }
    // Check once more after the final wait: a callback arriving in the last
    // 50 ms belongs to the settle window and must not be reported as absent.
    if let Some(desc) = runtime.describe() {
        info!("musicd-mixer: audio path settled — {desc}");
        return;
    }
    warn!(
        "musicd-mixer: no audio callback within {:?} of stream.play() — the stream is open but \
         not pulling; treat this run as suspect",
        RUNTIME_SETTLE_WAIT * RUNTIME_SETTLE_TRIES
    );
}

/// How long each settle wait sleeps, and how many times. 20 × 50 ms = 1 s, long
/// enough for any sane backend to deliver a first block and short enough that a
/// dead stream is reported while the run is still starting.
const RUNTIME_SETTLE_WAIT: Duration = Duration::from_millis(50);
const RUNTIME_SETTLE_TRIES: u32 = 20;

/// The headless path: drive the engine block-by-block with no audio device,
/// pacing to real time so `capture_frame / SR` still tracks wall-clock. This
/// thread IS the audio path here, so it takes the RT promotion itself.
pub fn paced_no_output<M: MeterSink>(mut rt: RtState<M>, runtime: Arc<AudioRuntime>) {
    let outcome = runtime.prime_from_paced_path();
    match outcome.rt_priority {
        0 => {}
        -1 => warn!(
            "musicd-mixer: SCHED_FIFO refused for the paced fallback thread ({}); \
             this run is NOT RT-scheduled",
            std::io::Error::from_raw_os_error(outcome.rt_errno)
        ),
        p => info!("musicd-mixer: paced fallback thread promoted to SCHED_FIFO prio {p}"),
    }
    rt.engine.ensure_started();
    let frame0 = rt.engine.frame0_mono();
    loop {
        rt.run_block(BLOCK, None);
        pace_to(frame0, rt.engine.sample_pos());
    }
}

/// Frames per audio callback requested from the backend. Frozen alongside the
/// engine's fixed 48 kHz for the same reason: a benchmark that pins the sample
/// rate "for cross-run comparability" and then lets the host choose the block
/// size has left the more influential of the two knobs floating. The value the
/// backend actually grants is recorded separately — a request is not evidence.
pub const REQUESTED_BUFFER_FRAMES: u32 = 512;

/// Env override for [`REQUESTED_BUFFER_FRAMES`]. `0` restores cpal's
/// `BufferSize::Default` (whatever the host picks).
pub const ENV_BUFFER_FRAMES: &str = "MUSICD_BUFFER_FRAMES";

/// Pick a device output config that brackets the engine's fixed 48 kHz
/// (frozen for cross-run comparability), preferring a stereo config, and pin the
/// buffer size to [`REQUESTED_BUFFER_FRAMES`] when the device advertises a range
/// that contains it. `None` if the device cannot run at 48 kHz — a host that
/// then cannot host a real-audio benchmark run, so it falls back to the paced
/// no-output path.
pub fn mixer_output_config(
    device: &cpal::Device,
) -> Option<(SampleFormat, cpal::StreamConfig, usize)> {
    let want = SR;
    let pick = device
        .supported_output_configs()
        .ok()?
        .filter(|r| r.min_sample_rate() <= want && want <= r.max_sample_rate())
        .max_by_key(|r| (r.channels() >= 2) as u8)?;
    let buffer_size = *pick.buffer_size();
    let supported = pick.with_sample_rate(want);
    let fmt = supported.sample_format();
    let channels = (supported.channels() as usize).max(1);
    let mut config: cpal::StreamConfig = supported.into();
    config.buffer_size = requested_buffer_size(&buffer_size);
    Some((fmt, config, channels))
}

/// Turn the device's advertised buffer-size range into the request we make.
///
/// Only asks for a fixed size when the device advertises a range that actually
/// contains the target — an out-of-range `Fixed` is rejected outright by some
/// backends, which would cost a working stream to gain a fixed block. When the
/// range is unknown, or the override is `0`, the host keeps choosing.
pub fn requested_buffer_size(supported: &cpal::SupportedBufferSize) -> cpal::BufferSize {
    let want = match std::env::var(ENV_BUFFER_FRAMES) {
        Ok(s) => match s.trim().parse::<u32>() {
            Ok(0) => return cpal::BufferSize::Default,
            Ok(n) => n,
            Err(_) => REQUESTED_BUFFER_FRAMES,
        },
        Err(_) => REQUESTED_BUFFER_FRAMES,
    };
    match supported {
        cpal::SupportedBufferSize::Range { min, max } if *min <= want && want <= *max => {
            cpal::BufferSize::Fixed(want)
        }
        cpal::SupportedBufferSize::Range { min, max } => {
            // Clamp rather than give up: a device whose range excludes the
            // target still benefits from a pinned block, and the achieved value
            // is logged either way.
            cpal::BufferSize::Fixed(want.clamp(*min, *max))
        }
        cpal::SupportedBufferSize::Unknown => cpal::BufferSize::Default,
    }
}

/// Build the hard-real-time output stream for sample type `T`, driving the engine
/// from the cpal callback (mirrors `play.rs`): each callback pulls stereo from the
/// engine in ≤128-frame internal blocks and writes it **clamped** to `[-1, 1]`
/// into the device buffer (meters saw the pre-clamp signal). Alloc-free callback.
/// The error callback latches `audio_fault` and drops `real_audio` so a run that
/// loses audio mid-way is detectable + disqualifiable (MAJOR-3).
///
/// The FIRST callback also promotes **its own** thread to `SCHED_FIFO` and records
/// the achieved block size. That has to happen here, not on the spawning thread:
/// the cpal backend owns the callback thread, so promoting the thread that merely
/// builds the stream would leave the real audio path on `SCHED_OTHER`. One syscall,
/// once, published to readers by a Release/Acquire flag.
#[allow(clippy::too_many_arguments)]
pub fn build_mixer_stream<M: MeterSink>(
    device: &cpal::Device,
    fmt: SampleFormat,
    config: &cpal::StreamConfig,
    channels: usize,
    rt: RtState<M>,
    real_audio: Arc<AtomicBool>,
    audio_fault: Arc<AtomicBool>,
    runtime: Arc<AudioRuntime>,
) -> Result<cpal::Stream> {
    match fmt {
        SampleFormat::F32 => build_mixer_stream_t::<M, f32>(
            device,
            config,
            channels,
            rt,
            real_audio,
            audio_fault,
            runtime,
        ),
        SampleFormat::I16 => build_mixer_stream_t::<M, i16>(
            device,
            config,
            channels,
            rt,
            real_audio,
            audio_fault,
            runtime,
        ),
        SampleFormat::U16 => build_mixer_stream_t::<M, u16>(
            device,
            config,
            channels,
            rt,
            real_audio,
            audio_fault,
            runtime,
        ),
        other => Err(anyhow::anyhow!("unsupported sample format: {other:?}")),
    }
}

pub fn build_mixer_stream_t<M: MeterSink, T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    mut rt: RtState<M>,
    real_audio: Arc<AtomicBool>,
    audio_fault: Arc<AtomicBool>,
    runtime: Arc<AudioRuntime>,
) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let mut scratch_l = vec![0.0f32; BLOCK];
    let mut scratch_r = vec![0.0f32; BLOCK];
    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            let frames = data.len() / channels;
            // One-time RT setup, on the backend own callback thread.
            runtime.prime_from_callback(frames as u32);
            let mut done = 0;
            while done < frames {
                let n = (frames - done).min(BLOCK);
                rt.run_block(n, Some((&mut scratch_l[..n], &mut scratch_r[..n])));
                for i in 0..n {
                    let l = scratch_l[i].clamp(-1.0, 1.0);
                    let r = scratch_r[i].clamp(-1.0, 1.0);
                    let base = (done + i) * channels;
                    let frame = &mut data[base..base + channels];
                    if channels == 1 {
                        frame[0] = T::from_sample(0.5 * (l + r));
                    } else {
                        frame[0] = T::from_sample(l);
                        frame[1] = T::from_sample(r);
                        for s in frame[2..].iter_mut() {
                            *s = T::from_sample(0.0f32);
                        }
                    }
                }
                done += n;
            }
        },
        move |err| {
            // MAJOR-3: a cpal runtime error means real audio was lost mid-run.
            // Latch the sticky fault + drop real_audio so benchd can disqualify.
            error!("musicd-mixer audio stream error: {err}");
            real_audio.store(false, Ordering::Release);
            audio_fault.store(true, Ordering::Release);
        },
        None,
    )?;
    Ok(stream)
}

/// Sleep until `CLOCK_MONOTONIC` reaches the wall time of `sample_pos`, anchored
/// at the engine's `frame0_mono`. If already behind schedule, return immediately
/// (catch up). `u128` math keeps `sample_pos * 1e9` from overflowing.
pub fn pace_to(frame0_mono_ns: u64, sample_pos: u64) {
    let target = frame0_mono_ns as u128 + (sample_pos as u128 * 1_000_000_000u128) / SR as u128;
    let now = now_mono_ns() as u128;
    if target > now {
        std::thread::sleep(Duration::from_nanos((target - now) as u64));
    }
}

// ===========================================================================
// Value conversions: mixer.v1 LeafValue <-> props-core PropValue.
// ===========================================================================

pub fn leaf_to_prop(v: &LeafValue) -> PropValue {
    match v {
        LeafValue::Number(n) => PropValue::Float(*n),
        LeafValue::Bool(b) => PropValue::Bool(*b),
        LeafValue::Enum(s) => PropValue::String(s.clone()),
    }
}

pub fn prop_to_leaf(v: &PropValue) -> Option<LeafValue> {
    match v {
        PropValue::Float(n) => Some(LeafValue::Number(*n)),
        PropValue::Int(i) => Some(LeafValue::Number(*i as f64)),
        PropValue::UInt(u) => Some(LeafValue::Number(*u as f64)),
        PropValue::Bool(b) => Some(LeafValue::Bool(*b)),
        PropValue::String(s) => Some(LeafValue::Enum(s.clone())),
        _ => None,
    }
}

pub fn leaf_to_json(v: &LeafValue) -> Json {
    match v {
        LeafValue::Number(n) => serde_json::Number::from_f64(*n)
            .map(Json::Number)
            .unwrap_or(Json::Null),
        LeafValue::Bool(b) => Json::Bool(*b),
        LeafValue::Enum(s) => Json::String(s.clone()),
    }
}

// ===========================================================================
// Path helpers.
// ===========================================================================

/// Split `mixer.channels.{id}.{leaf}` → `(id, leaf)`, where `leaf` may itself be
/// dotted (`"meter.clip"`). `None` for non-channel or out-of-range paths.
pub fn split_channel(path: &str) -> Option<(usize, &str)> {
    let rest = path.strip_prefix("mixer.channels.")?;
    let (id_s, leaf) = rest.split_once('.')?;
    let id: usize = id_s.parse().ok()?;
    if id < NUM_CHANNELS && id.to_string() == id_s {
        Some((id, leaf))
    } else {
        None
    }
}

/// The meter record index (`0..32` strips, `32` master) for a `meter.clip` path.
pub fn meter_index(path: &str) -> Option<usize> {
    if path == "mixer.master.meter.clip" {
        return Some(NUM_CHANNELS);
    }
    match split_channel(path) {
        Some((id, "meter.clip")) => Some(id),
        _ => None,
    }
}

/// The `(record_index, field)` of a meter *level*/clip leaf, else `None`.
/// `field` ∈ {rms_l, rms_r, peak_l, peak_r, hold_l, hold_r, clip}.
pub fn meter_leaf(path: &str) -> Option<(usize, &'static str)> {
    let (idx, field) = if let Some((id, leaf)) = split_channel(path) {
        (id, leaf)
    } else {
        (NUM_CHANNELS, path.strip_prefix("mixer.master.")?)
    };
    let f = match field {
        "meter.rms_l" => "rms_l",
        "meter.rms_r" => "rms_r",
        "meter.peak_l" => "peak_l",
        "meter.peak_r" => "peak_r",
        "meter.hold_l" => "hold_l",
        "meter.hold_r" => "hold_r",
        "meter.clip" => "clip",
        _ => return None,
    };
    Some((idx, f))
}

/// Every `mixer.v1` leaf path this daemon exposes for `props.list`/`get`.
pub fn all_leaf_paths() -> Vec<String> {
    let mut v = Vec::new();
    for ch in 0..NUM_CHANNELS {
        for leaf in ["trim", "fader", "pan", "mute", "solo", "name", "meter.clip"] {
            v.push(format!("mixer.channels.{ch}.{leaf}"));
        }
        for m in METER_LEVEL_LEAVES {
            v.push(format!("mixer.channels.{ch}.{m}"));
        }
    }
    for leaf in ["fader", "mute", "meter.clip"] {
        v.push(format!("mixer.master.{leaf}"));
    }
    for m in METER_LEVEL_LEAVES {
        v.push(format!("mixer.master.{m}"));
    }
    v.push("transport.state".into());
    v.push("transport.position".into());
    v.push("transport.length".into());
    v.push("mixer.song.title".into());
    v.push("mixer.song.artist".into());
    v.push("mixer.song.copyright".into());
    v.push("mixer.schema_version".into());
    v.push("mixer.engine".into());
    v.push("mixer.source_profile".into());
    v.push("mixer.benchmark_eligible".into());
    v.push(RT_PRIORITY_PATH.into());
    v.push(RT_TIME_US_PATH.into());
    v.push(BLOCK_FRAMES_PATH.into());
    v
}

/// The control leaves the store seeds + tracks (mutable, non-meter-level): the
/// 163 hash leaves + `meter.clip` + `transport.position` + read-only text.
pub fn seed_leaves() -> Vec<String> {
    all_leaf_paths()
        .into_iter()
        .filter(|p| p != RT_PRIORITY_PATH && p != BLOCK_FRAMES_PATH && p != RT_TIME_US_PATH)
        .filter(|p| meter_leaf(p).map(|(_, f)| f == "clip").unwrap_or(true))
        .collect()
}

// ===========================================================================
// Applying a write (pure, testable core of `props.set`).
// ===========================================================================

/// Update the authoritative [`Controls`] mirror from one *canonical*
/// `(path, value)`. Only control coefficients move here; `meter.clip` (latch
/// reset), `name`, meter levels and `transport.position` (a revisioned RT seek,
/// handled separately in [`apply_write`]) do not affect a coefficient.
pub fn apply_leaf_to_controls(c: &mut Controls, path: &str, v: &LeafValue) {
    if let Some((id, leaf)) = split_channel(path) {
        let cc = &mut c.channels[id];
        match (leaf, v) {
            ("trim", LeafValue::Number(n)) => cc.trim_db = *n,
            ("fader", LeafValue::Number(n)) => cc.fader_db = *n,
            ("pan", LeafValue::Number(n)) => cc.pan = *n,
            ("mute", LeafValue::Bool(b)) => cc.mute = *b,
            ("solo", LeafValue::Bool(b)) => cc.solo = *b,
            _ => {}
        }
        return;
    }
    match (path, v) {
        ("mixer.master.fader", LeafValue::Number(n)) => c.master.fader_db = *n,
        ("mixer.master.mute", LeafValue::Bool(b)) => c.master.mute = *b,
        ("transport.state", LeafValue::Enum(s)) => c.playing = s == "playing",
        _ => {}
    }
}

/// Internal (trusted) store write of `transport.length` in seconds — the
/// host mirrors the RT-ACTIVE bank's timeline length after region-edit
/// swaps, so the seek clamp, footer and scrubber domain track edits that
/// extend (or shrink) the timeline. Flows to clients through the normal
/// revisioned changed stream.
pub fn store_set_transport_length(ctl: &mut MixerCtl, secs: f64) {
    let Ok(path) = PropPath::new("transport.length") else {
        return;
    };
    let _ = ctl.store.apply(
        RevWriteRequest::new(path, PropValue::Float(secs), "engine-length"),
        "engine",
    );
}

/// Clamp a canonical `transport.position` value to `[0, length_secs]` using the
/// seeded `transport.length` store leaf (FIX B). Only `transport.position`
/// Number values are affected; a non-positive/absent length (unbounded /
/// multitone) leaves the value unchanged. `length > 0.0` also excludes a NaN
/// length, so `clamp` never sees `min > max`.
pub fn clamp_position_to_length(store: &RevWriteStore, path: &str, value: LeafValue) -> LeafValue {
    if path != "transport.position" {
        return value;
    }
    let LeafValue::Number(secs) = value else {
        return value;
    };
    let length = PropPath::new("transport.length")
        .ok()
        .and_then(|p| store.get(&p))
        .and_then(prop_to_leaf)
        .and_then(|v| match v {
            LeafValue::Number(n) => Some(n),
            _ => None,
        })
        .unwrap_or(0.0);
    if length > 0.0 {
        LeafValue::Number(secs.clamp(0.0, length))
    } else {
        value
    }
}

/// Apply one write to the store + controls mirror, returning the Bus `(rc,
/// response)` and the [`RtCommand`] to ship to the audio thread (on accept).
/// Pure over its `&mut` state so it is unit-testable without a broker.
///
/// Ordering matters: authentication and canonicalisation (which never touch the
/// store) run first, so a malformed write gets its proper rejection even under
/// ring pressure; the RT-ring capacity reservation (`ring_has_slot`) is checked
/// **immediately before** `store.apply`, so an accepted write is always
/// enqueueable to the DSP thread (BLOCKER-1: "once accepted → enqueued to RT").
pub fn apply_write(
    store: &mut RevWriteStore,
    controls: &mut Controls,
    rev_path: &mut BTreeMap<u64, PropPath>,
    ring_has_slot: bool,
    source_id: &str,
    req: WriteRequest,
) -> (u8, WriteResponse, Option<RtCommand>) {
    // A rejection carrying the store's current (revision, value) for rebase.
    let reject = |store: &RevWriteStore, path: String, op_id: String, reason: String| {
        let pp = PropPath::new(&path).ok();
        let (cur_rev, cur_val) = current_state(store, pp.as_ref(), &path);
        (
            RC_REJECT,
            WriteResponse::Rejected(WriteReject {
                path,
                op_id,
                current_revision: cur_rev,
                current_value: cur_val,
                reason,
            }),
            None,
        )
    };

    // 0. Authentication (MAJOR 8): no anonymous fallback, and a non-empty op_id
    //    is mandatory. Neither touches the store.
    if source_id.is_empty() {
        return reject(
            store,
            req.path,
            req.op_id,
            "unauthenticated: writes require a broker-verified sender".into(),
        );
    }
    if req.op_id.trim().is_empty() {
        return reject(
            store,
            req.path,
            req.op_id,
            "empty op_id: a write must carry a non-empty op_id".into(),
        );
    }

    // These leaves are daemon-owned live telemetry rather than frozen
    // mixer-schema controls. They are known properties, but never writable.
    if req.path == RT_PRIORITY_PATH || req.path == BLOCK_FRAMES_PATH || req.path == RT_TIME_US_PATH
    {
        return reject(
            store,
            req.path,
            req.op_id,
            "read-only runtime telemetry leaf, not writable".into(),
        );
    }

    // 1. Canonicalise (MAJOR 5): type/mutability/clip/enum validation, then CLAMP
    //    into range + QUANTISE to the leaf grid. An out-of-range numeric write is
    //    clamped (e.g. fader=999 → +6 dB), NOT rejected — rejection is reserved for
    //    wrong-type / read-only / unknown-path / bad-enum / non-finite. The store
    //    then holds and acks the CANONICAL value (which may differ from the input).
    let canonical = match canonicalize_write(&req.path, &req.value) {
        Ok(c) => c,
        Err(reason) => return reject(store, req.path, req.op_id, reason),
    };

    // 1b. Trust boundary (FIX B): clamp a `transport.position` VALUE to
    //     `[0, length_secs]` for a FINITE source BEFORE it is stored/acked.
    //     `canonicalize_write` only bounds it to `0..f64::MAX`, so a crafted
    //     `xe=1e300` (a domain-blind renderer emits continuous values) would
    //     otherwise be stored + echoed as 1e300 — the readout shows an absurd
    //     time even though the RT seek frame is separately clamped. Clamping the
    //     value here makes the store, the ack echo, and the RT seek all agree at
    //     the end. Unbounded (length==0/multitone) leaves it unchanged (the RT
    //     advance saturates — FIX A).
    let canonical = clamp_position_to_length(store, &req.path, canonical);

    // 2. Parse the path.
    let pp = match PropPath::new(&req.path) {
        Ok(p) => p,
        Err(e) => return reject(store, req.path, req.op_id, format!("invalid path: {e}")),
    };

    // 3. Reserve an RT control-ring slot BEFORE mutating the store (BLOCKER 1). A
    //    full ring returns a retryable BUSY with NO store/revision change, so an
    //    accepted write is always enqueued to the DSP thread.
    if !ring_has_slot {
        return (
            RC_BUSY,
            WriteResponse::Busy(WriteBusy {
                path: req.path,
                op_id: req.op_id,
                reason: "RT control ring full — retry".into(),
            }),
            None,
        );
    }

    // 4. Revisioned apply (server order authoritative; if_revision gate). The
    //    store holds/echoes the CANONICAL value.
    let rev_req = RevWriteRequest {
        path: pp.clone(),
        value: leaf_to_prop(&canonical),
        op_id: req.op_id.clone(),
        if_revision: req.if_revision,
    };
    match store.apply(rev_req, source_id) {
        RevWriteResponse::Accepted(ack) => {
            let revision = ack.revision;
            apply_leaf_to_controls(controls, &req.path, &canonical);
            rev_path.insert(revision, pp);

            // meter.clip=false → per-meter latch reset; transport.position → a
            // revisioned RT seek (RTZ when 0); everything else → the full control
            // snapshot (RT coalescing keeps the newest).
            let command = if let Some(meter) = meter_index(&req.path) {
                RtCommand::ResetLatch { meter, revision }
            } else if req.path == "transport.position" {
                let frame = match &canonical {
                    LeafValue::Number(secs) => (secs.max(0.0) * SR as f64).round() as u64,
                    _ => 0,
                };
                RtCommand::Seek { frame, revision }
            } else {
                RtCommand::SetControls {
                    controls: *controls,
                    revision,
                }
            };

            // Q8(a): the synchronous ack carries the CONTROL revision only —
            // canonical value + authenticated source + op — never a DSP-applied
            // confirmation (that arrives later as dsp.applied).
            let response = WriteResponse::Accepted(WriteAck {
                revision,
                path: req.path,
                canonical_value: canonical,
                source_id: ack.source_id,
                op_id: req.op_id,
            });
            (RC_OK, response, Some(command))
        }
        RevWriteResponse::Rejected(rej) => {
            let cur_val = prop_to_leaf(&rej.current_value)
                .or_else(|| leaf_default(&req.path))
                .unwrap_or(LeafValue::Number(0.0));
            (
                RC_REJECT,
                WriteResponse::Rejected(WriteReject {
                    path: req.path,
                    op_id: req.op_id,
                    current_revision: rej.current_revision,
                    current_value: cur_val,
                    reason: rej.reason,
                }),
                None,
            )
        }
    }
}

/// The store's current `(revision, value)` for a path, as a `mixer.v1`
/// [`LeafValue`] (falling back to the schema default).
pub fn current_state(store: &RevWriteStore, pp: Option<&PropPath>, path: &str) -> (u64, LeafValue) {
    let rev = pp.map(|p| store.path_revision(p)).unwrap_or(0);
    let val = pp
        .and_then(|p| store.get(p))
        .and_then(prop_to_leaf)
        .or_else(|| leaf_default(path))
        .unwrap_or(LeafValue::Number(0.0));
    (rev, val)
}

/// Build the revisioned bootstrap snapshot (MAJOR 6): the global revision plus
/// every seeded control leaf's canonical value + per-path revision, captured under
/// the caller's `ctl` lock (atomic w.r.t. writes). `real_audio` tells a
/// reconnecting client / benchd whether a real cpal stream is driving the engine;
/// `audio_fault`/`applied_fault` surface the sticky run-integrity faults so benchd
/// can disqualify a run that lost audio or overflowed the applied backlog.
pub fn build_snapshot_response(
    ctl: &MixerCtl,
    runtime: &AudioRuntime,
    real_audio: bool,
    audio_fault: bool,
    applied_fault: bool,
    source_profile: &str,
    benchmark_eligible: bool,
) -> MixerSnapshotResponse {
    let runtime = runtime.view();
    let mut leaves: Vec<LeafSnapshot> = seed_leaves()
        .into_iter()
        .filter_map(|path| {
            let pp = PropPath::new(&path).ok()?;
            let revision = ctl.store.path_revision(&pp);
            let value = ctl
                .store
                .get(&pp)
                .and_then(prop_to_leaf)
                .or_else(|| leaf_default(&path))?;
            Some(LeafSnapshot {
                path,
                value,
                revision,
            })
        })
        .collect();
    leaves.extend([
        LeafSnapshot {
            path: RT_PRIORITY_PATH.to_string(),
            value: LeafValue::Number(runtime.rt_priority as f64),
            revision: 0,
        },
        LeafSnapshot {
            path: BLOCK_FRAMES_PATH.to_string(),
            value: LeafValue::Number(runtime.block_frames as f64),
            revision: 0,
        },
        LeafSnapshot {
            path: RT_TIME_US_PATH.to_string(),
            value: LeafValue::Number(runtime.rt_time_us as f64),
            revision: 0,
        },
    ]);
    MixerSnapshotResponse {
        revision: ctl.store.revision(),
        real_audio,
        audio_fault,
        applied_fault,
        source_profile: source_profile.to_string(),
        benchmark_eligible,
        leaves,
    }
}

/// Expand a reported high-water revision into one `(revision, path)` per pending
/// write with `revision <= up_to`, removing them from `rev_path`. Lossless: every
/// latched revision yields exactly one `dsp.applied` event (MAJOR 4).
pub fn drain_applied(rev_path: &mut BTreeMap<u64, PropPath>, up_to: u64) -> Vec<(u64, PropPath)> {
    let out: Vec<(u64, PropPath)> = rev_path
        .range(..=up_to)
        .map(|(r, p)| (*r, p.clone()))
        .collect();
    *rev_path = rev_path.split_off(&(up_to + 1));
    out
}

/// Seed the revisioned store with the mixer.v1 defaults at revision 0.
pub fn seed_store(
    store: &mut RevWriteStore,
    names: &[Option<String>; NUM_CHANNELS],
    transport_length_secs: f64,
    song: &SongMeta,
    source_profile_id: &str,
    benchmark_eligible: bool,
) {
    // The two read-only run-identity leaves are seeded FIRST with the ACTIVE
    // profile (seed is a no-op on an existing path, so the default loop's re-seed
    // is inert) — otherwise a stem run's snapshot would falsely report the
    // benchmark default.
    if let Ok(pp) = PropPath::new("mixer.source_profile") {
        store.seed(
            pp,
            PropValue::String(source_profile_id.to_string()),
            "default",
        );
    }
    if let Ok(pp) = PropPath::new("mixer.benchmark_eligible") {
        store.seed(pp, PropValue::Bool(benchmark_eligible), "default");
    }
    // Total transport length in seconds (0 = unbounded/multitone) — the scrubber's
    // extent. Seeded before the default loop (seed is a no-op on an existing path).
    if let Ok(pp) = PropPath::new("transport.length") {
        store.seed(pp, PropValue::Float(transport_length_secs), "default");
    }
    // Per-channel instrument names from the stem manifest (override the "Ch N"
    // default). Must precede the default loop for the same no-op-on-existing reason.
    for (ch, n) in names.iter().enumerate() {
        if let Some(n) = n
            && let Ok(pp) = PropPath::new(format!("mixer.channels.{ch}.name"))
        {
            store.seed(pp, PropValue::String(n.clone()), "default");
        }
    }
    // Session song metadata (the GUI footer) — seeded before the default loop so
    // a non-empty manifest field wins over the empty leaf_default. Empty strings
    // are still seeded (harmless; mixer-bench omits empty footer parts).
    for (path, val) in [
        ("mixer.song.title", &song.title),
        ("mixer.song.artist", &song.artist),
        ("mixer.song.copyright", &song.copyright),
    ] {
        if let Ok(pp) = PropPath::new(path) {
            store.seed(pp, PropValue::String(val.clone()), "default");
        }
    }
    for path in seed_leaves() {
        if let (Ok(pp), Some(def)) = (PropPath::new(&path), leaf_default(&path)) {
            store.seed(pp, leaf_to_prop(&def), "default");
        }
    }
}

// ===========================================================================
// Stem-session source profile: manifest load + preload (RT-safe).
// ===========================================================================

/// The on-disk stem manifest (`--stems <manifest.json>`) mapping mixer channels
/// to preloaded mono 48 kHz WAV stems. Everything named here is decoded, hash-
/// verified, and preloaded into `Vec<f32>` BEFORE the cpal stream starts — the RT
/// path only indexes the preloaded buffers.
///
/// ```json
/// {"schema":"stem-session.v1","sample_rate":48000,"length_frames":N,
///  "stems":[{"channel":0,"path":"kick.wav","sha256":"…"}]}
/// ```
#[derive(serde::Deserialize)]
pub struct StemManifest {
    /// Must equal [`SOURCE_PROFILE_STEM`] (`stem-session.v1`).
    schema: String,
    /// Must equal the engine rate ([`SR`] = 48 000).
    sample_rate: u32,
    /// Logical stem length in frames (shorter stems are zero-padded to this).
    length_frames: u64,
    /// Optional session song metadata (the GUI footer). Absent → empty strings.
    #[serde(default)]
    title: String,
    #[serde(default)]
    artist: String,
    #[serde(default)]
    copyright: String,
    stems: Vec<StemEntry>,
}

/// One channel→stem mapping in a [`StemManifest`].
#[derive(serde::Deserialize)]
pub struct StemEntry {
    /// Mixer channel index (`0..32`).
    channel: usize,
    /// WAV path — absolute, or relative to the manifest's own directory.
    path: String,
    /// Lowercase hex SHA-256 of the WAV file bytes (verified before decode).
    sha256: String,
    /// Optional instrument name for this channel (shown on the mixer strip via
    /// the `mixer.channels.N.name` leaf). Absent = the default "Ch N".
    #[serde(default)]
    name: Option<String>,
    /// Region list carried by a v2 session (never present in v1 JSON — the
    /// v2 loader fills it after its own deserialisation). `None` = the
    /// default full-length region; `Some(vec![])` = an EXPLICITLY silenced
    /// lane (its last region was deleted) — the two must stay distinct
    /// across save/load.
    #[serde(skip)]
    regions: Option<Vec<Region>>,
}

/// Load, verify, and preload a stem manifest into a [`StemBank`]. All I/O, hash
/// verification, WAV decoding, and allocation happen here — **before** the cpal
/// stream — so the RT path never touches the filesystem or the allocator.
/// Rejects: a wrong `schema` tag, a non-48 kHz `sample_rate`, a channel out of
/// range or declared twice, a file whose sha256 mismatches, a non-48 kHz / non-
/// mono WAV, or a stem longer than the declared `length_frames`. Channels absent
/// from the manifest stay silent.
pub fn load_stem_bank(manifest_path: &Path) -> Result<StemBank> {
    Ok(load_stem_session(manifest_path)?.0)
}

/// One stem file reference in a session document — everything the SAVER
/// needs to reproduce the entry. Paths are absolutised at load so a session
/// saved into a different directory keeps pointing at its audio.
#[derive(Clone, Debug)]
pub struct StemFileEntry {
    pub channel: usize,
    pub path: String,
    pub sha256: String,
    pub name: Option<String>,
}

/// The session-document metadata alongside a loaded [`StemBank`]: the stem
/// file references, the manifest (base) length, and the song header — what
/// `save_stem_session_mix` re-emits together with the live region document.
#[derive(Clone, Debug)]
pub struct StemSessionMeta {
    pub entries: Vec<StemFileEntry>,
    pub base_length_frames: u64,
    pub song: SongMeta,
}

/// `stem-session.v2` — the strict-data `.mix` session document: the v1
/// manifest fields plus an optional per-stem non-destructive `regions` list.
/// Serialised via cosmix-lib-mix's serde bridge (`to_conf_mix_string` /
/// `from_conf_mix_str`), so the substrate's one data format carries the
/// session. A stem without a `regions` key plays as one full-length region.
pub const SOURCE_PROFILE_STEM_V2: &str = "stem-session.v2";

#[derive(serde::Serialize, serde::Deserialize)]
struct StemSessionV2 {
    schema: String,
    sample_rate: u32,
    length_frames: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    artist: String,
    #[serde(default)]
    copyright: String,
    stems: Vec<StemEntryV2>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct StemEntryV2 {
    channel: u64,
    path: String,
    sha256: String,
    #[serde(default)]
    name: Option<String>,
    /// Absent = default full-length region; present-but-empty = explicitly
    /// silenced lane.
    #[serde(default)]
    regions: Option<Vec<RegionV2>>,
}

/// Serde image of [`Region`] (kept separate so the engine type stays free of
/// format concerns). `gain` defaults to unity, fades to zero.
#[derive(serde::Serialize, serde::Deserialize)]
struct RegionV2 {
    timeline_start: u64,
    source_start: u64,
    len: u64,
    #[serde(default = "unity_gain")]
    gain: f64,
    #[serde(default)]
    fade_in: u64,
    #[serde(default)]
    fade_out: u64,
}

fn unity_gain() -> f64 {
    1.0
}

impl From<&Region> for RegionV2 {
    fn from(region: &Region) -> Self {
        RegionV2 {
            timeline_start: region.timeline_start,
            source_start: region.source_start,
            len: region.len,
            gain: f64::from(region.gain),
            fade_in: region.fade_in,
            fade_out: region.fade_out,
        }
    }
}

impl From<&RegionV2> for Region {
    fn from(region: &RegionV2) -> Self {
        Region {
            timeline_start: region.timeline_start,
            source_start: region.source_start,
            len: region.len,
            gain: region.gain as f32,
            fade_in: region.fade_in,
            fade_out: region.fade_out,
        }
    }
}

/// Load a stem session — v1 JSON (`.json`) or v2 strict-data `.mix` — into a
/// [`StemBank`] plus its [`StemSessionMeta`]. See [`load_stem_bank`] for the
/// validation contract; v2 additionally applies each stem's region list.
pub fn load_stem_session(manifest_path: &Path) -> Result<(StemBank, StemSessionMeta)> {
    let text = std::fs::read_to_string(manifest_path)
        .map_err(|e| anyhow::anyhow!("read stem manifest {}: {e}", manifest_path.display()))?;
    let manifest: StemManifest = if manifest_path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("mix"))
    {
        let v2: StemSessionV2 = cosmix_mix::serde_de::from_conf_mix_str(&text)
            .map_err(|e| anyhow::anyhow!("parse session {}: {e}", manifest_path.display()))?;
        if v2.schema != SOURCE_PROFILE_STEM_V2 {
            anyhow::bail!(
                "session schema {:?} != required {:?}",
                v2.schema,
                SOURCE_PROFILE_STEM_V2
            );
        }
        StemManifest {
            schema: SOURCE_PROFILE_STEM.to_string(),
            sample_rate: v2.sample_rate,
            length_frames: v2.length_frames,
            title: v2.title,
            artist: v2.artist,
            copyright: v2.copyright,
            stems: v2
                .stems
                .into_iter()
                .map(|entry| {
                    Ok(StemEntry {
                        channel: usize::try_from(entry.channel).map_err(|_| {
                            anyhow::anyhow!("channel {} out of range", entry.channel)
                        })?,
                        path: entry.path,
                        sha256: entry.sha256,
                        name: entry.name,
                        regions: entry
                            .regions
                            .map(|regions| regions.iter().map(Region::from).collect()),
                    })
                })
                .collect::<Result<Vec<_>>>()?,
        }
    } else {
        let manifest: StemManifest = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("parse stem manifest {}: {e}", manifest_path.display()))?;
        if manifest.schema != SOURCE_PROFILE_STEM {
            anyhow::bail!(
                "stem manifest schema {:?} != required {:?}",
                manifest.schema,
                SOURCE_PROFILE_STEM
            );
        }
        manifest
    };
    if manifest.sample_rate != SR {
        anyhow::bail!(
            "stem manifest sample_rate {} != engine rate {SR}",
            manifest.sample_rate
        );
    }
    // Resolve relative stem paths against the manifest's directory.
    let base = manifest_path.parent().unwrap_or_else(|| Path::new("."));
    let mut stems: [Vec<f32>; NUM_CHANNELS] = std::array::from_fn(|_| Vec::new());
    let mut names: [Option<String>; NUM_CHANNELS] = std::array::from_fn(|_| None);
    let mut regions_by_channel: [Option<Vec<Region>>; NUM_CHANNELS] = std::array::from_fn(|_| None);
    let mut entries: Vec<StemFileEntry> = Vec::new();
    let mut seen = [false; NUM_CHANNELS];
    for entry in &manifest.stems {
        if entry.channel >= NUM_CHANNELS {
            anyhow::bail!(
                "stem channel {} out of range (0..{NUM_CHANNELS})",
                entry.channel
            );
        }
        if seen[entry.channel] {
            anyhow::bail!("stem channel {} declared more than once", entry.channel);
        }
        seen[entry.channel] = true;
        names[entry.channel] = entry.name.clone();
        let rel = Path::new(&entry.path);
        let full = if rel.is_absolute() {
            rel.to_path_buf()
        } else {
            base.join(rel)
        };
        let bytes = std::fs::read(&full)
            .map_err(|e| anyhow::anyhow!("read stem {}: {e}", full.display()))?;
        // Verify the content hash BEFORE decoding — a mismatch means the stem is
        // not the one the manifest pinned (a non-benchmark run must still be
        // reproducible from its manifest).
        let got = hex_lower(&sha2::Sha256::digest(&bytes));
        if !got.eq_ignore_ascii_case(entry.sha256.trim()) {
            anyhow::bail!(
                "stem {} sha256 mismatch: computed {got}, manifest {:?}",
                full.display(),
                entry.sha256
            );
        }
        let samples = decode_wav_mono(&bytes, &full.display().to_string())?;
        if samples.len() as u64 > manifest.length_frames {
            anyhow::bail!(
                "stem {} has {} frames, exceeds declared length_frames {}",
                full.display(),
                samples.len(),
                manifest.length_frames
            );
        }
        stems[entry.channel] = samples;
        regions_by_channel[entry.channel] = entry.regions.clone();
        // Absolutised (canonicalised — the file was just read, so it
        // resolves) so a later save into a DIFFERENT directory still points
        // at this audio even when the manifest path itself was relative.
        let absolute = full.canonicalize().unwrap_or_else(|_| {
            std::env::current_dir()
                .map(|cwd| cwd.join(&full))
                .unwrap_or_else(|_| full.clone())
        });
        entries.push(StemFileEntry {
            channel: entry.channel,
            path: absolute.display().to_string(),
            sha256: entry.sha256.clone(),
            name: entry.name.clone(),
        });
    }
    let song = SongMeta {
        title: manifest.title.clone(),
        artist: manifest.artist.clone(),
        copyright: manifest.copyright.clone(),
    };
    let mut bank = StemBank::new(stems, manifest.length_frames)
        .with_names(names)
        .with_song(song.clone());
    for (channel, regions) in regions_by_channel.into_iter().enumerate() {
        // Some(vec![]) is an explicitly silenced lane and MUST override the
        // default full-length region; None keeps the default.
        if let Some(regions) = regions {
            bank = bank.with_channel_regions(channel, regions);
        }
    }
    Ok((
        bank,
        StemSessionMeta {
            entries,
            base_length_frames: manifest.length_frames,
            song,
        },
    ))
}

/// Save a stem session as a `stem-session.v2` strict-data `.mix` document:
/// the file references from `meta` plus the LIVE per-channel region lists.
/// Sources on disk are never rewritten — the session document is the edit.
pub fn save_stem_session_mix(
    path: &Path,
    meta: &StemSessionMeta,
    regions_by_channel: &[Vec<Region>; NUM_CHANNELS],
) -> Result<()> {
    let session = StemSessionV2 {
        schema: SOURCE_PROFILE_STEM_V2.to_string(),
        sample_rate: SR,
        length_frames: meta.base_length_frames,
        title: meta.song.title.clone(),
        artist: meta.song.artist.clone(),
        copyright: meta.song.copyright.clone(),
        stems: meta
            .entries
            .iter()
            .map(|entry| StemEntryV2 {
                channel: entry.channel as u64,
                path: entry.path.clone(),
                sha256: entry.sha256.clone(),
                name: entry.name.clone(),
                // Always explicit on save: an empty list IS the document (a
                // deleted-last-region lane must not resurrect on reload).
                regions: Some(
                    regions_by_channel[entry.channel]
                        .iter()
                        .map(RegionV2::from)
                        .collect(),
                ),
            })
            .collect(),
    };
    let text = cosmix_mix::serde_ser::to_conf_mix_string(&session)
        .map_err(|e| anyhow::anyhow!("serialise session: {e}"))?;
    // Atomic-ish: write a UNIQUE sibling temp file, then rename over the
    // target — a failed/interrupted write can never destroy the previous
    // session, and concurrent savers cannot clobber each other's temp.
    let tmp = path.with_extension(format!(
        "mix.tmp.{}.{}",
        std::process::id(),
        crate::mixer::now_mono_ns()
    ));
    std::fs::write(&tmp, text)
        .map_err(|e| anyhow::anyhow!("write session {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        anyhow::anyhow!("finalise session {}: {e}", path.display())
    })
}

// ===========================================================================
// MIDI-synth source profile: song load → frame-keyed schedules (RT-safe).
// ===========================================================================

/// Release tail appended after the last note-off so envelopes ring out before
/// the transport EOF clamp: one second at the engine rate.
pub const SONG_RELEASE_TAIL_FRAMES: u64 = SR as u64;

/// Convert a cosmix-song [`Song`](cosmix_song::Song) into per-track frame-keyed
/// [`TrackSchedule`]s plus the transport length in frames. Pure (no soundfont,
/// no I/O): tick→frame math at the engine rate against the song's fixed tempo,
/// note-offs guaranteed at least one frame after their note-ons, events sorted
/// `(frame, on)` so same-frame offs precede ons.
///
/// Track N maps to mixer channel N (the strip owns level/pan/mute/solo — the
/// song's per-track volume/pan/mute/solo are NOT baked into the schedule; the
/// front-end maps them onto strip controls when it loads a song). Fails if the
/// song has more tracks than mixer channels or a zero tempo.
pub fn song_schedules(song: &cosmix_song::Song) -> Result<(Vec<TrackSchedule>, u64)> {
    if song.track_count() > NUM_CHANNELS {
        anyhow::bail!(
            "song has {} tracks, the mixer has {NUM_CHANNELS} channels",
            song.track_count()
        );
    }
    if song.tempo == 0 {
        anyhow::bail!("song tempo is 0 BPM");
    }
    let frames_per_tick =
        SR as f64 * 60.0 / (song.tempo as f64 * cosmix_song::TICKS_PER_BEAT as f64);
    let mut schedules = Vec::with_capacity(song.track_count());
    let mut last_off: u64 = 0;
    for track in song.tracks() {
        let mut events = Vec::with_capacity(track.note_count() * 2);
        for note in track.notes() {
            let on = (note.start_tick as f64 * frames_per_tick).round() as u64;
            let off = ((note.end_tick() as f64 * frames_per_tick).round() as u64).max(on + 1);
            events.push(NoteEvent {
                frame: on,
                on: true,
                key: note.pitch,
                vel: note.velocity,
            });
            events.push(NoteEvent {
                frame: off,
                on: false,
                key: note.pitch,
                vel: 0,
            });
            last_off = last_off.max(off);
        }
        events.sort_by_key(|e| (e.frame, e.on));
        schedules.push(TrackSchedule {
            channel: track.channel,
            program: track.program,
            name: Some(track.name.clone()),
            events,
        });
    }
    Ok((schedules, last_off + SONG_RELEASE_TAIL_FRAMES))
}

/// Load a song file by extension: `.json` / `.oxm` / `.mid` / `.midi` via
/// cosmix-song.
pub fn load_song(song_path: &Path) -> Result<cosmix_song::Song> {
    use cosmix_song::Song;

    let ext = song_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "json" => Song::load_from_file(song_path)
            .map_err(|e| anyhow::anyhow!("load song {}: {e}", song_path.display())),
        "oxm" => Song::load_from_binary(song_path)
            .map_err(|e| anyhow::anyhow!("load song {}: {e}", song_path.display())),
        "mid" | "midi" => Song::import_smf(song_path)
            .map_err(|e| anyhow::anyhow!("import SMF {}: {e}", song_path.display())),
        "asc" => Song::import_asc(song_path)
            .map_err(|e| anyhow::anyhow!("import asc {}: {e}", song_path.display())),
        other => anyhow::bail!(
            "unsupported song extension {other:?} (want .json, .oxm, .mid, .midi, .asc): {}",
            song_path.display()
        ),
    }
}

/// Build the [`MidiSynthBank`] for a loaded song. The soundfont comes from
/// `soundfont_path` when given, else from the song's own `soundfont_path`
/// field. All I/O, soundfont parsing, and voice-pool allocation happen here —
/// before (or off) the RT thread.
pub fn song_bank(song: &cosmix_song::Song, soundfont_path: Option<&Path>) -> Result<MidiSynthBank> {
    let sf_path = soundfont_path
        .map(Path::to_path_buf)
        .or_else(|| song.get_soundfont_path().map(std::path::PathBuf::from))
        .ok_or_else(|| {
            anyhow::anyhow!("no soundfont: pass one explicitly or set the song's soundfont_path")
        })?;
    let soundfont = crate::synth::load_soundfont(&sf_path)?;
    song_bank_with(song, Some(&soundfont))
}

/// [`song_bank`] with an already-loaded soundfont — the edit-loop path, where
/// the soundfont `Arc` is cached and only the schedules change.
pub fn song_bank_with(
    song: &cosmix_song::Song,
    soundfont: Option<&std::sync::Arc<rustysynth::SoundFont>>,
) -> Result<MidiSynthBank> {
    let (schedules, length_frames) = song_schedules(song)?;
    let meta = SongMeta {
        title: song.name.clone(),
        artist: String::new(),
        copyright: String::new(),
    };
    MidiSynthBank::build(soundfont, schedules, length_frames, meta)
}

/// Load a song file and build its bank in one step (the startup path).
pub fn load_song_bank(song_path: &Path, soundfont_path: Option<&Path>) -> Result<MidiSynthBank> {
    let song = load_song(song_path)?;
    song_bank(&song, soundfont_path)
}

/// Map a song's per-track mix state onto the initial strip [`Controls`]
/// (transport stopped). Track N → channel N:
///
/// - `volume` (MIDI CC7-style 0-127, 100 = the miditui default) → `fader_db`
///   via the GM-ish square law normalised so 100 → 0 dB: `40·log10(v/100)`,
///   clamped to the fader leaf range; `0` hard-mutes to the silence floor.
/// - `pan` (0-127, 64 = centre) → `pan ∈ [-1, 1]`.
/// - `muted`/`solo` map directly.
///
/// The synth schedule deliberately carries NONE of this (the strip owns the
/// mix), so a song load must seed these controls or every track plays at
/// unity centre.
pub fn song_initial_controls(song: &cosmix_song::Song) -> Controls {
    use cosmix_mixer_schema::{FADER_MAX_DB, FADER_MIN_DB};

    let mut controls = Controls::default();
    for (ch, track) in song.tracks().iter().enumerate().take(NUM_CHANNELS) {
        let cc = &mut controls.channels[ch];
        cc.fader_db = if track.volume == 0 {
            FADER_MIN_DB
        } else {
            (40.0 * (track.volume as f64 / 100.0).log10()).clamp(FADER_MIN_DB, FADER_MAX_DB)
        };
        cc.pan = ((track.pan as f64 - 64.0) / 63.0).clamp(-1.0, 1.0);
        cc.mute = track.muted;
        cc.solo = track.solo;
    }
    controls
}

/// Seed the per-channel strip-control leaves (`fader`/`pan`/`mute`/`solo`)
/// from an initial [`Controls`] snapshot. Values are canonicalised through the
/// schema (clamp + quantise) so the store holds exactly what a write would.
/// Must run BEFORE [`seed_store`]'s default loop (seed is first-write-wins).
pub fn seed_strip_controls(store: &mut RevWriteStore, controls: &Controls) {
    for (ch, cc) in controls.channels.iter().enumerate() {
        let leaves = [
            (
                format!("mixer.channels.{ch}.fader"),
                LeafValue::Number(cc.fader_db),
            ),
            (
                format!("mixer.channels.{ch}.pan"),
                LeafValue::Number(cc.pan),
            ),
            (
                format!("mixer.channels.{ch}.mute"),
                LeafValue::Bool(cc.mute),
            ),
            (
                format!("mixer.channels.{ch}.solo"),
                LeafValue::Bool(cc.solo),
            ),
        ];
        for (path, value) in leaves {
            if let (Ok(pp), Ok(canonical)) =
                (PropPath::new(&path), canonicalize_write(&path, &value))
            {
                store.seed(pp, leaf_to_prop(&canonical), "default");
            }
        }
    }
}

/// One file written by [`export_stem_session`], with its measured peak
/// (absolute, linear) and count of samples that exceeded ±1.0.
pub struct ExportedFile {
    pub path: PathBuf,
    pub peak: f32,
    pub clipped: u64,
}

/// The export job's terminal report.
pub struct StemExportReport {
    pub files: Vec<ExportedFile>,
    pub length_frames: u64,
}

fn measure(buffers: &[&[f32]]) -> (f32, u64) {
    let mut peak = 0.0f32;
    let mut clipped = 0u64;
    for buffer in buffers {
        for &sample in *buffer {
            let magnitude = sample.abs();
            if magnitude > peak {
                peak = magnitude;
            }
            if magnitude > 1.0 {
                clipped += 1;
            }
        }
    }
    (peak, clipped)
}

/// `chNN-name` filename slug (ASCII alphanumerics, lowercased).
fn stem_file_stem(channel: usize, name: Option<&str>) -> String {
    let slug: String = name
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if slug.is_empty() {
        format!("ch{channel:02}")
    } else {
        format!("ch{channel:02}-{slug}")
    }
}

/// Offline export of an edited stem session — the R3 contract:
///
/// - Inputs are a SNAPSHOT the caller captured at job start (sources +
///   regions + controls + names); edits racing the job cannot mix revisions.
/// - The master is the captured mix through the SAME engine the live path
///   runs (strip trim/fader/pan/mute/solo + master), `playing` forced on.
/// - Per-stem files carry region gain/fades only (no strip processing, no
///   mute/solo, no master), timeline-aligned: every file spans the same
///   `[0, length)` as the master — never per-file trimmed.
/// - `progress(done, total)` is called per block across all renders; return
///   `false` to cancel. On cancel or error every file this job wrote is
///   removed.
/// - Peaks/clip counts are measured and reported, never normalised.
#[allow(clippy::too_many_arguments)]
pub fn export_stem_session(
    out_dir: &Path,
    format: RenderFormat,
    sources: &[Arc<Vec<f32>>; NUM_CHANNELS],
    regions: &[Vec<Region>; NUM_CHANNELS],
    names: &[Option<String>; NUM_CHANNELS],
    base_length_frames: u64,
    controls: &Controls,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<StemExportReport> {
    use crate::render::{Channels, write_render};

    std::fs::create_dir_all(out_dir)
        .map_err(|e| anyhow::anyhow!("create export dir {}: {e}", out_dir.display()))?;
    // Everything renders into a job-owned temp dir; the final paths are
    // touched ONLY by the atomic publish renames after full success — a
    // cancel/failure can never truncate or delete a previous export.
    let tmp_dir = out_dir.join(format!(
        ".export.tmp.{}.{}",
        std::process::id(),
        crate::mixer::now_mono_ns()
    ));
    std::fs::create_dir_all(&tmp_dir)
        .map_err(|e| anyhow::anyhow!("create export temp dir {}: {e}", tmp_dir.display()))?;
    // RAII: the staging dir is removed on EVERY exit path, including panics —
    // unless DEFUSED (a failed rollback must preserve the backups it holds).
    struct TmpGuard {
        dir: PathBuf,
        defused: bool,
    }
    impl Drop for TmpGuard {
        fn drop(&mut self) {
            if !self.defused {
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }
    }
    let mut tmp_guard = TmpGuard {
        dir: tmp_dir.clone(),
        defused: false,
    };
    /// A whole-file sample buffer with FALLIBLE allocation — an oversized
    /// session degrades to an error, never an allocator abort.
    fn try_buffer(len: usize) -> Result<Vec<f32>> {
        let mut buffer = Vec::new();
        buffer
            .try_reserve_exact(len)
            .map_err(|_| anyhow::anyhow!("export buffer allocation failed ({len} frames)"))?;
        buffer.resize(len, 0.0);
        Ok(buffer)
    }
    let ext = match format {
        RenderFormat::Flac24 => "flac",
        _ => "wav",
    };
    let build_bank = || {
        let mut bank = StemBank::from_shared(sources.clone(), base_length_frames);
        for (channel, channel_regions) in regions.iter().enumerate() {
            bank = bank.with_channel_regions(channel, channel_regions.clone());
        }
        bank
    };
    let mut bank = build_bank();
    let length_frames = bank.length_frames();
    // Memory model: one whole-file f32 buffer at a time (sequential per
    // stem, L+R for the master) plus the writer's transient copy. Bounded by
    // an extent cap so a region flung far down the timeline cannot request
    // an absurd allocation and abort the process; streaming writers are the
    // future fix if hour-scale sessions become real.
    const MAX_EXPORT_FRAMES: u64 = 3600 * SR as u64; // 1 hour
    if length_frames > MAX_EXPORT_FRAMES {
        anyhow::bail!(
            "session length {length_frames} frames exceeds the {MAX_EXPORT_FRAMES}-frame \
             (1 hour) export cap"
        );
    }
    let stem_channels: Vec<usize> = (0..NUM_CHANNELS)
        .filter(|&ch| !regions[ch].is_empty() && !sources[ch].is_empty())
        .collect();
    let total_work = length_frames * (stem_channels.len() as u64 + 1);
    let mut done_work = 0u64;
    let mut staged: Vec<(PathBuf, PathBuf)> = Vec::new(); // (tmp, final)
    let mut files: Vec<ExportedFile> = Vec::new();

    let mut run = |files: &mut Vec<ExportedFile>,
                   staged: &mut Vec<(PathBuf, PathBuf)>|
     -> Result<bool> {
        const CHUNK: usize = 4096;
        for &ch in &stem_channels {
            let mut buffer = try_buffer(length_frames as usize)?;
            let mut offset = 0usize;
            while offset < buffer.len() {
                let n = (buffer.len() - offset).min(CHUNK);
                bank.render_channel_range(ch, offset as u64, &mut buffer[offset..offset + n]);
                offset += n;
                done_work += n as u64;
                if !progress(done_work, total_work) {
                    return Ok(false);
                }
            }
            let (peak, clipped) = measure(&[&buffer]);
            let name = format!("{}.{ext}", stem_file_stem(ch, names[ch].as_deref()));
            let tmp = tmp_dir.join(&name);
            write_render(&tmp, &buffer, &buffer, SR as i32, Channels::Mono, format)?;
            // Cancellation is honoured across the encode/write too.
            if !progress(done_work, total_work) {
                return Ok(false);
            }
            staged.push((tmp, out_dir.join(&name)));
            files.push(ExportedFile {
                path: out_dir.join(&name),
                peak,
                clipped,
            });
        }

        // Master: the captured mix through the real engine.
        let mut engine = MixerEngine::with_profile(false, SourceProfile::StemSession(build_bank()));
        let mut captured = *controls;
        captured.playing = true;
        engine.set_controls(&captured, 1);
        // Offline: the captured mix applies from frame 0 (no de-zipper ramp).
        engine.snap_smoothing();
        let frames = length_frames as usize;
        let mut left = try_buffer(frames)?;
        let mut right = try_buffer(frames)?;
        let mut meters = Vec::new();
        let mut done = 0usize;
        while done < frames {
            let n = (frames - done).min(BLOCK);
            engine.process_block_audio(
                &mut left[done..done + n],
                &mut right[done..done + n],
                &mut meters,
            );
            meters.clear();
            done += n;
            done_work += n as u64;
            if !progress(done_work, total_work) {
                return Ok(false);
            }
        }
        let (peak, clipped) = measure(&[&left, &right]);
        let name = format!("master.{ext}");
        let tmp = tmp_dir.join(&name);
        write_render(&tmp, &left, &right, SR as i32, Channels::Stereo, format)?;
        if !progress(done_work, total_work) {
            return Ok(false);
        }
        staged.push((tmp, out_dir.join(&name)));
        files.push(ExportedFile {
            path: out_dir.join(&name),
            peak,
            clipped,
        });
        Ok(true)
    };

    let outcome = run(&mut files, &mut staged);
    match outcome {
        Ok(true) => {
            // Publish, approximately transactionally: existing final files
            // are first moved ASIDE into the staging dir (backups), then the
            // new files renamed in. A mid-batch failure rolls the completed
            // replacements back from their backups, so the folder never ends
            // up a mixed old/new export. (Replacing a previous export of the
            // same session is the intended overwrite.)
            // Preflight: refuse to touch any final path that exists but is
            // not a regular file (a DIRECTORY named master.wav must never be
            // backed up into staging and then recursively deleted).
            for (_, final_path) in &staged {
                if let Ok(metadata) = std::fs::symlink_metadata(final_path) {
                    if !metadata.is_file() {
                        anyhow::bail!(
                            "{} exists and is not a regular file — refusing to replace it",
                            final_path.display()
                        );
                    }
                }
            }
            let mut backups: Vec<(PathBuf, PathBuf)> = Vec::new(); // (backup, final)
            let mut published: Vec<&(PathBuf, PathBuf)> = Vec::new();
            let mut publish = || -> Result<()> {
                for pair in &staged {
                    let (tmp, final_path) = pair;
                    if std::fs::symlink_metadata(final_path).is_ok() {
                        let backup = tmp_dir.join(format!(
                            "backup-{}",
                            final_path.file_name().unwrap_or_default().to_string_lossy()
                        ));
                        std::fs::rename(final_path, &backup).map_err(|e| {
                            anyhow::anyhow!("stage backup {}: {e}", final_path.display())
                        })?;
                        backups.push((backup, final_path.clone()));
                    }
                    std::fs::rename(tmp, final_path)
                        .map_err(|e| anyhow::anyhow!("publish {}: {e}", final_path.display()))?;
                    published.push(pair);
                }
                Ok(())
            };
            if let Err(error) = publish() {
                // Roll back: remove the new files already in place, restore
                // their backups. If ANY restoration fails, the staging dir
                // (holding the only copies) is PRESERVED and named in the
                // error instead of being cleaned up.
                // Every un-undone path is a rollback failure and gets named.
                let mut unrecovered: Vec<PathBuf> = Vec::new();
                for (_, final_path) in published {
                    if std::fs::remove_file(final_path).is_ok() {
                        continue;
                    }
                    // Only a definitive NotFound proves the path is gone; a
                    // metadata error (permissions, I/O fault) must still
                    // count as unrecovered.
                    let proven_absent = matches!(
                        std::fs::symlink_metadata(final_path),
                        Err(ref e) if e.kind() == std::io::ErrorKind::NotFound
                    );
                    if !proven_absent {
                        unrecovered.push(final_path.clone());
                    }
                }
                for (backup, final_path) in &backups {
                    if std::fs::rename(backup, final_path).is_err() {
                        unrecovered.push(final_path.clone());
                    }
                }
                if !unrecovered.is_empty() {
                    tmp_guard.defused = true;
                    let names: Vec<String> = unrecovered
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect();
                    anyhow::bail!(
                        "{error}; ADDITIONALLY rollback left these paths unrecovered: {} — \
                         backups preserved in {}",
                        names.join(", "),
                        tmp_dir.display()
                    );
                }
                return Err(error);
            }
            Ok(StemExportReport {
                files,
                length_frames,
            })
        }
        Ok(false) => anyhow::bail!("export cancelled"),
        Err(error) => Err(error),
    }
}

/// Render a song offline through the SAME 32-channel mixer engine the live
/// path runs — synth bank, strip coefficients, master pad and all — returning
/// the master stereo at [`SR`]. `controls.playing` is forced on. Deterministic
/// (a pure function of song + soundfont + controls); the basis for WAV export
/// and waveform displays, so what you see/export is exactly what you heard.
pub fn render_song_stereo(
    song: &cosmix_song::Song,
    soundfont: &std::sync::Arc<rustysynth::SoundFont>,
    controls: &Controls,
) -> Result<(Vec<f32>, Vec<f32>)> {
    let bank = song_bank_with(song, Some(soundfont))?;
    let frames = bank.length_frames() as usize;
    let mut engine =
        MixerEngine::with_profile(/* simulator = */ false, SourceProfile::MidiSynth(bank));
    let mut c = *controls;
    c.playing = true;
    engine.set_controls(&c, 1);
    // Offline: the captured mix applies from frame 0 (no de-zipper ramp —
    // a muted track would otherwise leak its first milliseconds).
    engine.snap_smoothing();

    let mut left = vec![0.0f32; frames];
    let mut right = vec![0.0f32; frames];
    let mut meters = Vec::new();
    let mut done = 0;
    while done < frames {
        let n = (frames - done).min(BLOCK);
        engine.process_block_audio(
            &mut left[done..done + n],
            &mut right[done..done + n],
            &mut meters,
        );
        meters.clear();
        done += n;
    }
    Ok((left, right))
}

/// Render each voiced channel of a song offline — the per-track lane data
/// for waveform displays. Returns `(channel, name, mono samples)` per song
/// track, in channel order; the signal is the raw track source (what the
/// strip receives, pre-trim/pan/fader). Deterministic, same total synth cost
/// as one master render.
pub fn render_song_channels(
    song: &cosmix_song::Song,
    soundfont: &std::sync::Arc<rustysynth::SoundFont>,
) -> Result<Vec<(usize, String, Vec<f32>)>> {
    let mut bank = song_bank_with(song, Some(soundfont))?;
    let frames = bank.length_frames() as usize;
    let names = bank.names().clone();
    let mut out = Vec::new();
    for (ch, name) in names.iter().enumerate() {
        if !bank.is_voiced(ch) {
            continue;
        }
        let mut buf = vec![0.0f32; frames];
        bank.render_channel(ch, &mut buf);
        let name = name.clone().unwrap_or_else(|| format!("Ch {ch}"));
        out.push((ch, name, buf));
    }
    Ok(out)
}

/// Render a song offline (see [`render_song_stereo`]) and write it as a
/// 16-bit stereo 48 kHz WAV. Samples are clamped to `[-1, 1]` exactly as the
/// live device write clamps.
pub fn export_song_wav(
    song: &cosmix_song::Song,
    soundfont: &std::sync::Arc<rustysynth::SoundFont>,
    controls: &Controls,
    path: &Path,
) -> Result<()> {
    let (left, right) = render_song_stereo(song, soundfont, controls)?;
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: SR,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)
        .map_err(|e| anyhow::anyhow!("create WAV {}: {e}", path.display()))?;
    for (l, r) in left.iter().zip(&right) {
        writer.write_sample((l.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)?;
        writer.write_sample((r.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)?;
    }
    writer
        .finalize()
        .map_err(|e| anyhow::anyhow!("finalize WAV {}: {e}", path.display()))?;
    Ok(())
}

/// Lowercase hex of a byte slice (no external hex dep).
pub fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_mixer_schema::{self as mixer, MeterRecord};
    use rtrb::RingBuffer;
    use std::path::PathBuf;

    fn fresh() -> (RevWriteStore, Controls, BTreeMap<u64, PropPath>) {
        let mut store = RevWriteStore::new();
        for path in seed_leaves() {
            if let (Ok(pp), Some(def)) = (PropPath::new(&path), leaf_default(&path)) {
                store.seed(pp, leaf_to_prop(&def), "default");
            }
        }
        (store, Controls::default(), BTreeMap::new())
    }

    fn wreq(path: &str, v: LeafValue, op: &str, if_rev: Option<u64>) -> WriteRequest {
        WriteRequest {
            path: path.into(),
            value: v,
            op_id: op.into(),
            if_revision: if_rev,
        }
    }

    #[test]
    fn meter_mailbox_latest_wins_roundtrip() {
        let mb = MeterMailbox::new();
        assert!(mb.read().is_none(), "no frame before first publish");
        let mut f = MeterFrame {
            seq: 7,
            capture_frame: 800,
            applied_rev: 3,
            frame0_mono: 123,
            flags: 0,
            records: [MeterRecord::default(); mixer::NUM_METERS],
        };
        mb.publish(&f.encode());
        let got = mb.read().expect("frame present");
        assert_eq!(MeterFrame::decode(&got).unwrap(), f);
        // Latest wins: a second publish overwrites.
        f.seq = 99;
        mb.publish(&f.encode());
        assert_eq!(MeterFrame::decode(&mb.read().unwrap()).unwrap().seq, 99);
    }

    #[test]
    fn accepted_write_bumps_revision_moves_control_and_ships_snapshot() {
        let (mut store, mut controls, mut rp) = fresh();
        let (rc, resp, cmd) = apply_write(
            &mut store,
            &mut controls,
            &mut rp,
            true,
            "sess-a",
            wreq(
                "mixer.channels.3.fader",
                LeafValue::Number(-6.0),
                "op1",
                None,
            ),
        );
        assert_eq!(rc, RC_OK);
        match resp {
            WriteResponse::Accepted(a) => {
                assert_eq!(a.revision, 1);
                assert_eq!(a.source_id, "sess-a");
                assert_eq!(a.op_id, "op1");
                assert_eq!(a.canonical_value, LeafValue::Number(-6.0));
            }
            _ => panic!("expected accept"),
        }
        assert_eq!(controls.channels[3].fader_db, -6.0);
        match cmd {
            Some(RtCommand::SetControls {
                controls: c,
                revision,
            }) => {
                assert_eq!(revision, 1);
                assert_eq!(c.channels[3].fader_db, -6.0);
            }
            _ => panic!("expected a full SetControls snapshot"),
        }
        assert_eq!(rp.get(&1).unwrap().as_str(), "mixer.channels.3.fader");
    }

    #[test]
    fn transport_state_playing_flips_the_engine_gate() {
        let (mut store, mut controls, mut rp) = fresh();
        assert!(!controls.playing);
        let (rc, _, cmd) = apply_write(
            &mut store,
            &mut controls,
            &mut rp,
            true,
            "s",
            wreq(
                "transport.state",
                LeafValue::Enum("playing".into()),
                "op",
                None,
            ),
        );
        assert_eq!(rc, RC_OK);
        assert!(controls.playing);
        assert!(matches!(cmd, Some(RtCommand::SetControls { controls: c, .. }) if c.playing));
    }

    #[test]
    fn meter_clip_write_becomes_a_reset_latch_command() {
        let (mut store, mut controls, mut rp) = fresh();
        // master clip reset
        let (rc, _, cmd) = apply_write(
            &mut store,
            &mut controls,
            &mut rp,
            true,
            "s",
            wreq(
                "mixer.master.meter.clip",
                LeafValue::Bool(false),
                "op",
                None,
            ),
        );
        assert_eq!(rc, RC_OK);
        assert!(matches!(cmd, Some(RtCommand::ResetLatch { meter, .. }) if meter == NUM_CHANNELS));
        // a channel clip reset targets that record index
        let (_, _, cmd2) = apply_write(
            &mut store,
            &mut controls,
            &mut rp,
            true,
            "s",
            wreq(
                "mixer.channels.5.meter.clip",
                LeafValue::Bool(false),
                "op2",
                None,
            ),
        );
        assert!(matches!(cmd2, Some(RtCommand::ResetLatch { meter, .. }) if meter == 5));
    }

    #[test]
    fn transport_position_write_becomes_a_seek_command() {
        let (mut store, mut controls, mut rp) = fresh();
        // RTZ: transport.position = 0 → Seek { frame: 0 }.
        let (rc, _, cmd) = apply_write(
            &mut store,
            &mut controls,
            &mut rp,
            true,
            "s",
            wreq("transport.position", LeafValue::Number(0.0), "op", None),
        );
        assert_eq!(rc, RC_OK);
        assert!(matches!(cmd, Some(RtCommand::Seek { frame: 0, .. })));
        // A non-zero seek maps seconds → frames (× SR).
        let (_, _, cmd2) = apply_write(
            &mut store,
            &mut controls,
            &mut rp,
            true,
            "s",
            wreq("transport.position", LeafValue::Number(2.0), "op2", None),
        );
        assert!(matches!(cmd2, Some(RtCommand::Seek { frame, .. }) if frame == 2 * SR as u64));
    }

    /// FIX B (trust boundary): a crafted `xe=1e300` write to a FINITE session
    /// stores + acks the CLAMPED length (not 1e300), and the RT seek lands at the
    /// end frame, so store / ack / RT all agree.
    #[test]
    fn transport_position_value_is_clamped_to_length_for_finite_source() {
        // Seed a FINITE transport.length (300 s) BEFORE the defaults (seed is a
        // no-op on an existing path, so this wins over the 0.0 default).
        let mut store = RevWriteStore::new();
        store.seed(
            PropPath::new("transport.length").unwrap(),
            PropValue::Float(300.0),
            "default",
        );
        for path in seed_leaves() {
            if let (Ok(pp), Some(def)) = (PropPath::new(&path), leaf_default(&path)) {
                store.seed(pp, leaf_to_prop(&def), "default");
            }
        }
        let mut controls = Controls::default();
        let mut rp = BTreeMap::new();

        let (rc, resp, cmd) = apply_write(
            &mut store,
            &mut controls,
            &mut rp,
            true,
            "s",
            wreq("transport.position", LeafValue::Number(1e300), "op", None),
        );
        assert_eq!(rc, RC_OK);
        // The ack echoes the CLAMPED value (300.0 s), not 1e300.
        match resp {
            WriteResponse::Accepted(ack) => {
                assert_eq!(
                    ack.canonical_value,
                    LeafValue::Number(300.0),
                    "ack echoes clamped value"
                )
            }
            other => panic!("expected Accepted, got {other:?}"),
        }
        // The RT seek lands at the end frame (300 s), not ~u64::MAX.
        assert!(
            matches!(cmd, Some(RtCommand::Seek { frame, .. }) if frame == (300.0 * SR as f64) as u64),
            "RT seek at the end frame"
        );
        // The STORE holds the clamped value too.
        let stored = store
            .get(&PropPath::new("transport.position").unwrap())
            .and_then(prop_to_leaf);
        assert_eq!(
            stored,
            Some(LeafValue::Number(300.0)),
            "store holds clamped value"
        );
    }

    /// FIX B: an UNBOUNDED (multitone, length==0) session leaves the value
    /// unclamped — the RT advance saturating-add (FIX A) is what prevents the
    /// overflow there, not a value clamp.
    #[test]
    fn transport_position_value_unclamped_for_unbounded_source() {
        // fresh() seeds transport.length = 0.0 (the unbounded default).
        let (mut store, mut controls, mut rp) = fresh();
        let (rc, resp, _cmd) = apply_write(
            &mut store,
            &mut controls,
            &mut rp,
            true,
            "s",
            wreq("transport.position", LeafValue::Number(12345.0), "op", None),
        );
        assert_eq!(rc, RC_OK);
        match resp {
            WriteResponse::Accepted(ack) => {
                assert_eq!(
                    ack.canonical_value,
                    LeafValue::Number(12345.0),
                    "unbounded: value unchanged"
                )
            }
            other => panic!("expected Accepted, got {other:?}"),
        }
    }

    #[test]
    fn invalid_write_is_rejected_without_a_command() {
        let (mut store, mut controls, mut rp) = fresh();
        // wrong type for a numeric leaf
        let (rc, resp, cmd) = apply_write(
            &mut store,
            &mut controls,
            &mut rp,
            true,
            "s",
            wreq("mixer.channels.0.fader", LeafValue::Bool(true), "op", None),
        );
        assert_eq!(rc, RC_REJECT);
        assert!(cmd.is_none());
        assert!(matches!(resp, WriteResponse::Rejected(_)));
        // read-only leaf
        let (rc2, _, cmd2) = apply_write(
            &mut store,
            &mut controls,
            &mut rp,
            true,
            "s",
            wreq(
                "mixer.channels.0.meter.rms_l",
                LeafValue::Number(-12.0),
                "op2",
                None,
            ),
        );
        assert_eq!(rc2, RC_REJECT);
        assert!(cmd2.is_none());
        // meter.clip=true is rejected (only false resets)
        let (rc3, _, _) = apply_write(
            &mut store,
            &mut controls,
            &mut rp,
            true,
            "s",
            wreq(
                "mixer.master.meter.clip",
                LeafValue::Bool(true),
                "op3",
                None,
            ),
        );
        assert_eq!(rc3, RC_REJECT);
        // unknown path
        let (rc4, _, _) = apply_write(
            &mut store,
            &mut controls,
            &mut rp,
            true,
            "s",
            wreq("bogus.leaf", LeafValue::Number(0.0), "op4", None),
        );
        assert_eq!(rc4, RC_REJECT);
    }

    #[test]
    fn out_of_range_write_clamps_not_rejects() {
        let (mut store, mut controls, mut rp) = fresh();
        // fader 999 → CLAMP to +6 dB, accepted (contract change; no longer a reject).
        let (rc, resp, cmd) = apply_write(
            &mut store,
            &mut controls,
            &mut rp,
            true,
            "s",
            wreq(
                "mixer.channels.0.fader",
                LeafValue::Number(999.0),
                "op",
                None,
            ),
        );
        assert_eq!(rc, RC_OK);
        assert!(cmd.is_some());
        match resp {
            WriteResponse::Accepted(a) => assert_eq!(a.canonical_value, LeafValue::Number(6.0)),
            _ => panic!("expected accept with clamped canonical value"),
        }
        assert_eq!(controls.channels[0].fader_db, 6.0);
        // master fader -999 → clamp to the -120 dB silence floor.
        let (rc2, resp2, _) = apply_write(
            &mut store,
            &mut controls,
            &mut rp,
            true,
            "s",
            wreq("mixer.master.fader", LeafValue::Number(-999.0), "op2", None),
        );
        assert_eq!(rc2, RC_OK);
        assert!(matches!(
            resp2,
            WriteResponse::Accepted(a) if a.canonical_value == LeafValue::Number(mixer::SILENCE_DB)
        ));
    }

    #[test]
    fn busy_when_ring_full_makes_no_state_change() {
        let (mut store, mut controls, mut rp) = fresh();
        let rev_before = store.revision();
        // ring_has_slot = false → retryable BUSY, no store/revision/controls change.
        let (rc, resp, cmd) = apply_write(
            &mut store,
            &mut controls,
            &mut rp,
            false,
            "sess",
            wreq(
                "mixer.channels.0.fader",
                LeafValue::Number(-6.0),
                "op",
                None,
            ),
        );
        assert_eq!(rc, RC_BUSY);
        assert!(cmd.is_none());
        assert!(matches!(resp, WriteResponse::Busy(_)));
        assert_eq!(
            store.revision(),
            rev_before,
            "BUSY must not bump the revision"
        );
        assert_eq!(
            store.path_revision(&PropPath::new("mixer.channels.0.fader").unwrap()),
            0,
            "BUSY must not write the path"
        );
        assert_eq!(
            controls.channels[0].fader_db, 0.0,
            "BUSY must not move controls"
        );
        // With a slot, the identical write is accepted → BUSY was purely transient.
        let (rc2, _, cmd2) = apply_write(
            &mut store,
            &mut controls,
            &mut rp,
            true,
            "sess",
            wreq(
                "mixer.channels.0.fader",
                LeafValue::Number(-6.0),
                "op",
                None,
            ),
        );
        assert_eq!(rc2, RC_OK);
        assert!(cmd2.is_some());
    }

    #[test]
    fn unauthenticated_or_empty_op_write_is_rejected() {
        let (mut store, mut controls, mut rp) = fresh();
        let rev0 = store.revision();
        // Empty broker identity → reject (no "anonymous" fallback), no state change.
        let (rc, resp, cmd) = apply_write(
            &mut store,
            &mut controls,
            &mut rp,
            true,
            "",
            wreq(
                "mixer.channels.0.fader",
                LeafValue::Number(-6.0),
                "op",
                None,
            ),
        );
        assert_eq!(rc, RC_REJECT);
        assert!(cmd.is_none());
        assert!(matches!(resp, WriteResponse::Rejected(_)));
        assert_eq!(store.revision(), rev0);
        // Empty op_id → reject.
        let (rc2, _, cmd2) = apply_write(
            &mut store,
            &mut controls,
            &mut rp,
            true,
            "sess",
            wreq("mixer.channels.0.fader", LeafValue::Number(-6.0), "", None),
        );
        assert_eq!(rc2, RC_REJECT);
        assert!(cmd2.is_none());
        assert_eq!(store.revision(), rev0);
    }

    #[test]
    fn if_revision_mismatch_rejects_with_current_state() {
        let (mut store, mut controls, mut rp) = fresh();
        apply_write(
            &mut store,
            &mut controls,
            &mut rp,
            true,
            "s",
            wreq("mixer.master.fader", LeafValue::Number(-3.0), "op1", None),
        );
        // Client still thinks fader sits at rev 0.
        let (rc, resp, cmd) = apply_write(
            &mut store,
            &mut controls,
            &mut rp,
            true,
            "s",
            wreq(
                "mixer.master.fader",
                LeafValue::Number(-6.0),
                "op2",
                Some(0),
            ),
        );
        assert_eq!(rc, RC_REJECT);
        assert!(cmd.is_none());
        match resp {
            WriteResponse::Rejected(r) => {
                assert_eq!(r.current_revision, 1);
                assert_eq!(r.current_value, LeafValue::Number(-3.0));
            }
            _ => panic!("expected reject"),
        }
    }

    #[test]
    fn snapshot_response_carries_global_and_per_path_revisions() {
        let (mut store, mut controls, mut rp) = fresh();
        apply_write(
            &mut store,
            &mut controls,
            &mut rp,
            true,
            "s",
            wreq(
                "mixer.channels.4.fader",
                LeafValue::Number(-6.0),
                "op",
                None,
            ),
        );
        apply_write(
            &mut store,
            &mut controls,
            &mut rp,
            true,
            "s",
            wreq("mixer.master.mute", LeafValue::Bool(true), "op2", None),
        );
        let ctl = MixerCtl {
            store,
            controls,
            ctrl_tx: RingBuffer::<RtCommand>::new(1).0,
            rev_path: rp,
        };

        let runtime = AudioRuntime::new(0);
        let pending = build_snapshot_response(
            &ctl,
            &runtime,
            false,
            false,
            false,
            "benchmark-multitone.v1",
            true,
        );
        let pending_leaf = |p: &str| pending.leaves.iter().find(|l| l.path == p).unwrap();
        assert_eq!(
            pending_leaf(RT_PRIORITY_PATH).value,
            LeafValue::Number(rt_sched::RT_PRIORITY_PENDING as f64)
        );
        assert_eq!(
            pending_leaf(BLOCK_FRAMES_PATH).value,
            LeafValue::Number(0.0)
        );
        runtime.prime_from_callback(512);
        let resp = build_snapshot_response(
            &ctl,
            &runtime,
            true,
            false,
            false,
            "benchmark-multitone.v1",
            true,
        );
        assert_eq!(resp.revision, 2, "global revision = last accepted write");
        assert!(resp.real_audio);
        assert!(
            !resp.audio_fault && !resp.applied_fault,
            "no faults on a clean snapshot"
        );
        assert_eq!(resp.source_profile, "benchmark-multitone.v1");
        assert!(resp.benchmark_eligible);
        let leaf = |p: &str| resp.leaves.iter().find(|l| l.path == p).unwrap();
        assert_eq!(
            leaf("mixer.channels.4.fader").value,
            LeafValue::Number(-6.0)
        );
        assert_eq!(leaf("mixer.channels.4.fader").revision, 1);
        assert_eq!(leaf("mixer.master.mute").value, LeafValue::Bool(true));
        assert_eq!(leaf("mixer.master.mute").revision, 2);
        // An untouched leaf sits at revision 0 (seeded default).
        assert_eq!(leaf("mixer.channels.0.trim").revision, 0);
        assert_eq!(leaf(RT_PRIORITY_PATH).value, LeafValue::Number(0.0));
        assert_eq!(leaf(BLOCK_FRAMES_PATH).value, LeafValue::Number(512.0));
        assert_eq!(leaf(RT_TIME_US_PATH).value, LeafValue::Number(0.0));
    }

    #[test]
    fn runtime_telemetry_properties_reject_writes_as_read_only() {
        let (mut store, mut controls, mut rp) = fresh();
        for path in [RT_PRIORITY_PATH, BLOCK_FRAMES_PATH, RT_TIME_US_PATH] {
            let (rc, response, command) = apply_write(
                &mut store,
                &mut controls,
                &mut rp,
                true,
                "s",
                wreq(path, LeafValue::Number(1.0), "op", None),
            );
            assert_eq!(rc, RC_REJECT);
            assert!(command.is_none());
            match response {
                WriteResponse::Rejected(reject) => assert!(reject.reason.contains("read-only")),
                other => panic!("expected read-only rejection, got {other:?}"),
            }
        }
    }

    #[test]
    fn dsp_applied_expands_every_pending_revision_through_high_water() {
        let (mut store, mut controls, mut rp) = fresh();
        // Three writes to distinct paths accepted before one block latches them.
        let writes = [
            ("mixer.channels.0.fader", LeafValue::Number(-6.0)),
            ("mixer.channels.1.pan", LeafValue::Number(0.5)),
            ("mixer.master.mute", LeafValue::Bool(true)),
        ];
        for (i, (p, v)) in writes.into_iter().enumerate() {
            apply_write(
                &mut store,
                &mut controls,
                &mut rp,
                true,
                "s",
                wreq(p, v, &format!("op{i}"), None),
            );
        }
        assert_eq!(rp.len(), 3);
        // One reported high-water (=3) expands to one event PER pending revision.
        let emitted = drain_applied(&mut rp, 3);
        assert_eq!(
            emitted.len(),
            3,
            "every latched revision <= high-water gets an event"
        );
        assert!(rp.is_empty(), "all pending revisions pruned");
        let paths: Vec<&str> = emitted.iter().map(|(_, p)| p.as_str()).collect();
        assert!(paths.contains(&"mixer.channels.0.fader"));
        assert!(paths.contains(&"mixer.channels.1.pan"));
        assert!(paths.contains(&"mixer.master.mute"));
    }

    #[test]
    fn applied_backlog_overflow_trips_integrity_fault() {
        // A 1-slot return ring that is never drained: each block latches a new
        // revision, the ring holds only the first, so the RT backlog grows until it
        // overflows and trips the STICKY integrity fault — rather than corrupting a
        // latch timestamp with a later block's frame (MAJOR-2).
        let (mut ctrl_tx, ctrl_rx) = RingBuffer::<RtCommand>::new(4);
        let (applied_tx, _applied_rx) = RingBuffer::<AppliedMsg>::new(1);
        let meters = Arc::new(MeterMailbox::new());
        let transport_pos = Arc::new(AtomicU64::new(0));
        let applied_fault = Arc::new(AtomicBool::new(false));
        let mut rt = RtState::new(
            ctrl_rx,
            applied_tx,
            MailboxSink(meters),
            transport_pos,
            applied_fault.clone(),
            SourceProfile::BenchmarkMultitone,
        );
        for rev in 1..=(APPLIED_BACKLOG_CAP as u64 + 4) {
            let _ = ctrl_tx.push(RtCommand::SetControls {
                controls: Controls::default(),
                revision: rev,
            });
            rt.run_block(1, None);
        }
        assert!(
            applied_fault.load(Ordering::Relaxed),
            "a saturated applied backlog must trip the sticky integrity fault"
        );
    }

    #[test]
    fn meter_mailbox_reader_never_sees_a_torn_frame() {
        use std::sync::atomic::AtomicBool as StdAtomicBool;
        use std::time::Instant;

        // A frame that is a pure function of a per-publish nonce (= its seq), so any
        // tear (bytes mixed across two publishes) is detectable: recompute the
        // expected frame from the decoded seq and assert byte-for-byte equality.
        fn frame_for(nonce: u32) -> MeterFrame {
            let lvl = mixer::to_centi_dbfs(-((nonce % 100) as f64));
            let mut recs = [MeterRecord::default(); mixer::NUM_METERS];
            for r in recs.iter_mut() {
                r.rms_l = lvl;
                r.rms_r = lvl;
                r.peak_l = lvl;
                r.peak_r = lvl;
                r.hold_l = lvl;
                r.hold_r = lvl;
                r.clip = (nonce % 4) as u8 & mixer::CLIP_VALID_MASK;
            }
            MeterFrame {
                seq: nonce,
                capture_frame: nonce as u64,
                applied_rev: nonce as u64,
                frame0_mono: nonce as u64,
                flags: 0,
                records: recs,
            }
        }

        // MeterMailbox is a seqlock over [AtomicU8; 465], all SeqCst.
        // publish = seq→odd, 465 xchg byte stores, seq→even.
        // read needs: seq even, 465 loads, seq unchanged.
        //
        // The original writer loop published with no pause, so the mailbox was
        // in the odd state ~99% of the time and the even window between publishes
        // was ~3 instructions wide; the reader could not fit 465 loads into it.
        // A snapshot succeeded only when something interrupted the writer at
        // exactly the gap (timer tick, IRQ, migration): the reader needed 200,000
        // successes, ~77/s observed ≈ the interrupt rate on a dedicated core,
        // taking ~2,600 s (~43 minutes).
        //
        // The fix: a 1μs sleep after each publish guarantees the reader a real
        // even window. The writer still publishes at ~100k/s (1μs sleep) so the
        // tear-detection value is intact.

        let mb = Arc::new(MeterMailbox::new());
        let stop = Arc::new(StdAtomicBool::new(false));
        let writer = {
            let (mb, stop) = (mb.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut n = 1u32;
                while !stop.load(Ordering::Relaxed) {
                    mb.publish(&frame_for(n).encode());
                    n = n.wrapping_add(1);
                    std::thread::sleep(std::time::Duration::from_micros(1));
                }
            })
        };
        let reader = {
            let mb = mb.clone();
            std::thread::spawn(move || {
                let start = Instant::now();
                const MAX_SNAPSHOTS: u32 = 10_000;
                const TIMEOUT_SECS: u64 = 60;
                for _ in 0..MAX_SNAPSHOTS {
                    if let Some(bytes) = mb.read() {
                        let f = MeterFrame::decode(&bytes).expect("a whole, valid A.6 frame");
                        assert_eq!(
                            f,
                            frame_for(f.seq),
                            "torn read: bytes mixed across publishes"
                        );
                    }
                    if start.elapsed().as_secs() > TIMEOUT_SECS {
                        panic!(
                            "test exceeded {}s timeout — likely livelock regression",
                            TIMEOUT_SECS
                        );
                    }
                }
            })
        };
        reader.join().unwrap();
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
    }

    /// Write a tiny synthetic 48 kHz WAV to `path` (`channels`-channel f32).
    fn write_wav(path: &Path, channels: u16, frames: u32) {
        let spec = hound::WavSpec {
            channels,
            sample_rate: SR,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for i in 0..frames {
            for _c in 0..channels {
                w.write_sample(0.05 * ((i % 5) as f32) - 0.1).unwrap();
            }
        }
        w.finalize().unwrap();
    }

    /// R3 export contract: per-stem files are sample-exact against the
    /// region renderer, the master respects the captured controls (mute),
    /// stems ignore mute, and cancellation removes everything written.
    #[test]
    fn stem_session_export_contract() {
        let dir = std::env::temp_dir().join(format!(
            "musicd-export-{}-{}",
            std::process::id(),
            now_mono_ns()
        ));
        let mut sources: [Arc<Vec<f32>>; NUM_CHANNELS] =
            std::array::from_fn(|_| Arc::new(Vec::new()));
        sources[0] = Arc::new(vec![0.5; 1000]);
        sources[1] = Arc::new(vec![0.25; 1000]);
        let mut regions: [Vec<Region>; NUM_CHANNELS] = std::array::from_fn(|_| Vec::new());
        regions[0] = vec![Region::full(1000)];
        regions[1] = vec![Region {
            timeline_start: 100,
            source_start: 0,
            len: 300,
            gain: 2.0,
            fade_in: 0,
            fade_out: 0,
        }];
        let mut names: [Option<String>; NUM_CHANNELS] = std::array::from_fn(|_| None);
        names[1] = Some("Bass Guitar".into());
        let mut controls = Controls::default();
        controls.channels[1].mute = true; // master must exclude ch1; its STEM must not.

        let report = export_stem_session(
            &dir,
            crate::render::RenderFormat::WavF32,
            &sources,
            &regions,
            &names,
            1000,
            &controls,
            &mut |_done, _total| true,
        )
        .expect("export succeeds");
        assert_eq!(report.length_frames, 1000);
        assert_eq!(report.files.len(), 3, "two stems + master");

        // Stem ch1: sample-exact vs the region renderer, mute ignored.
        let ch1 = report
            .files
            .iter()
            .find(|f| f.path.file_name().unwrap().to_str().unwrap() == "ch01-bassguitar.wav")
            .expect("named stem file");
        let mut reader = hound::WavReader::open(&ch1.path).unwrap();
        let samples: Vec<f32> = reader.samples::<f32>().map(|s| s.unwrap()).collect();
        assert_eq!(samples.len(), 1000, "stem spans the master duration");
        let mut expected = vec![0.0f32; 1000];
        let mut bank = StemBank::from_shared(sources.clone(), 1000)
            .with_channel_regions(0, regions[0].clone())
            .with_channel_regions(1, regions[1].clone());
        bank.render_channel_range(1, 0, &mut expected);
        assert_eq!(samples, expected, "sample-exact against Region::sample");
        assert!((ch1.peak - 0.5).abs() < 1e-6, "peak = 0.25 × gain 2.0");
        assert_eq!(ch1.clipped, 0);

        // Master: ch1 muted → identical to a ch0-only session's master.
        let master = report
            .files
            .iter()
            .find(|f| f.path.file_name().unwrap().to_str().unwrap() == "master.wav")
            .unwrap();
        let mut solo_regions: [Vec<Region>; NUM_CHANNELS] = std::array::from_fn(|_| Vec::new());
        solo_regions[0] = regions[0].clone();
        let dir2 = dir.join("solo");
        let report2 = export_stem_session(
            &dir2,
            crate::render::RenderFormat::WavF32,
            &sources,
            &solo_regions,
            &names,
            1000,
            &Controls::default(),
            &mut |_d, _t| true,
        )
        .unwrap();
        let master2 = report2
            .files
            .iter()
            .find(|f| f.path.file_name().unwrap().to_str().unwrap() == "master.wav")
            .unwrap();
        let read = |p: &Path| -> Vec<f32> {
            hound::WavReader::open(p)
                .unwrap()
                .samples::<f32>()
                .map(|s| s.unwrap())
                .collect()
        };
        assert_eq!(
            read(&master.path),
            read(&master2.path),
            "muted channel contributes nothing to the master"
        );

        // Cancellation removes everything written so far.
        let dir3 = dir.join("cancelled");
        let result = export_stem_session(
            &dir3,
            crate::render::RenderFormat::WavF32,
            &sources,
            &regions,
            &names,
            1000,
            &controls,
            &mut |done, _total| done < 1500,
        );
        assert!(result.is_err(), "cancelled export errors");
        assert!(
            std::fs::read_dir(&dir3).unwrap().next().is_none(),
            "no partial files survive a cancel"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// v2 `.mix` round trip: load a v1 session, save it as strict-data v2
    /// with edited regions, reload — regions, names, song and length carry;
    /// the reloaded bank plays the edited document.
    #[test]
    fn stem_session_v2_mix_round_trip() {
        let dir = std::env::temp_dir().join(format!(
            "musicd-stem-v2-{}-{}",
            std::process::id(),
            now_mono_ns()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("stem0.wav");
        write_wav(&wav, 1, 200);
        let sha = hex_lower(&sha2::Sha256::digest(std::fs::read(&wav).unwrap()));
        let v1 = dir.join("session.json");
        std::fs::write(
            &v1,
            format!(
                r#"{{"schema":"stem-session.v1","sample_rate":48000,"length_frames":500,
                     "title":"T","artist":"A",
                     "stems":[{{"channel":3,"path":"stem0.wav","sha256":"{sha}","name":"Bass"}}]}}"#
            ),
        )
        .unwrap();
        let (_bank, meta) = load_stem_session(&v1).expect("v1 loads");
        assert_eq!(meta.base_length_frames, 500);
        assert_eq!(meta.entries.len(), 1);

        // Edit: trim channel 3 to a 100-frame window at timeline 50.
        let mut regions: [Vec<Region>; NUM_CHANNELS] = std::array::from_fn(|_| Vec::new());
        regions[3] = vec![Region {
            timeline_start: 50,
            source_start: 20,
            len: 100,
            gain: 0.5,
            fade_in: 10,
            fade_out: 0,
        }];
        let v2 = dir.join("session.mix");
        save_stem_session_mix(&v2, &meta, &regions).expect("v2 saves");

        let (bank, meta2) = load_stem_session(&v2).expect("v2 loads");
        assert_eq!(meta2.base_length_frames, 500);
        assert_eq!(meta2.song.title, "T");
        assert_eq!(bank.names()[3].as_deref(), Some("Bass"));
        assert_eq!(
            bank.regions()[3],
            regions[3],
            "regions survive the round trip"
        );
        // (Region PLAYBACK is proven by the mixer.rs region tests; here the
        // document round trip is the contract.)

        // An EXPLICITLY silenced lane (last region deleted) stays silent
        // across the round trip — it must not resurrect as the default
        // full-length region.
        let silent: [Vec<Region>; NUM_CHANNELS] = std::array::from_fn(|_| Vec::new());
        save_stem_session_mix(&v2, &meta, &silent).expect("silent v2 saves");
        let (bank, _) = load_stem_session(&v2).expect("silent v2 loads");
        assert!(bank.regions()[3].is_empty(), "explicit silence survives");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_stem_bank_decodes_verifies_and_rejects() {
        use std::io::Write as _;

        // Unique temp dir (no tempfile dev-dep in this crate); cleaned at the end.
        let dir = std::env::temp_dir().join(format!(
            "musicd-stem-test-{}-{}",
            std::process::id(),
            now_mono_ns()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // A good mono stem (200 frames) + its sha256.
        let wav = dir.join("stem0.wav");
        write_wav(&wav, 1, 200);
        let sha = hex_lower(&sha2::Sha256::digest(std::fs::read(&wav).unwrap()));
        // A stereo stem the loader must reject via decode_wav_mono.
        let stereo = dir.join("stereo.wav");
        write_wav(&stereo, 2, 200);
        let sha_stereo = hex_lower(&sha2::Sha256::digest(std::fs::read(&stereo).unwrap()));

        let write_manifest = |name: &str, body: String| -> PathBuf {
            let p = dir.join(name);
            std::fs::File::create(&p)
                .unwrap()
                .write_all(body.as_bytes())
                .unwrap();
            p
        };

        // Good manifest → loads; channel 3 carries the stem; the profile is
        // non-benchmark and drives real DSP.
        let good = write_manifest(
            "good.json",
            format!(
                r#"{{"schema":"stem-session.v1","sample_rate":48000,"length_frames":500,
                     "stems":[{{"channel":3,"path":"stem0.wav","sha256":"{sha}"}}]}}"#
            ),
        );
        let bank = load_stem_bank(&good).expect("good manifest loads");
        assert_eq!(bank.loaded_channels(), 1);
        assert_eq!(bank.length_frames(), 500);
        let engine = MixerEngine::with_profile(false, SourceProfile::StemSession(bank));
        assert_eq!(engine.source_profile().id(), "stem-session.v1");
        assert!(!engine.source_profile().benchmark_eligible());

        // Each rejection path returns an Err (never a silent accept).
        let cases: [(&str, String); 6] = [
            // sha256 mismatch.
            (
                "bad_hash.json",
                r#"{"schema":"stem-session.v1","sample_rate":48000,"length_frames":500,
                    "stems":[{"channel":3,"path":"stem0.wav","sha256":"00"}]}"#
                    .to_string(),
            ),
            // wrong schema tag.
            (
                "bad_schema.json",
                format!(
                    r#"{{"schema":"benchmark-multitone.v1","sample_rate":48000,"length_frames":500,
                         "stems":[{{"channel":3,"path":"stem0.wav","sha256":"{sha}"}}]}}"#
                ),
            ),
            // non-48k manifest rate.
            (
                "bad_sr.json",
                format!(
                    r#"{{"schema":"stem-session.v1","sample_rate":44100,"length_frames":500,
                         "stems":[{{"channel":3,"path":"stem0.wav","sha256":"{sha}"}}]}}"#
                ),
            ),
            // channel out of range.
            (
                "bad_ch.json",
                format!(
                    r#"{{"schema":"stem-session.v1","sample_rate":48000,"length_frames":500,
                         "stems":[{{"channel":99,"path":"stem0.wav","sha256":"{sha}"}}]}}"#
                ),
            ),
            // stem longer than declared length_frames (200 > 100).
            (
                "too_long.json",
                format!(
                    r#"{{"schema":"stem-session.v1","sample_rate":48000,"length_frames":100,
                         "stems":[{{"channel":3,"path":"stem0.wav","sha256":"{sha}"}}]}}"#
                ),
            ),
            // a stereo WAV (rejected by the mono check in decode_wav_mono).
            (
                "stereo.json",
                format!(
                    r#"{{"schema":"stem-session.v1","sample_rate":48000,"length_frames":500,
                         "stems":[{{"channel":0,"path":"stereo.wav","sha256":"{sha_stereo}"}}]}}"#
                ),
            ),
        ];
        for (name, body) in cases {
            let p = write_manifest(name, body);
            assert!(load_stem_bank(&p).is_err(), "{name} must be rejected");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn song_schedules_maps_ticks_to_frames() {
        // 120 BPM at 48 kHz: one beat = 0.5 s = 24 000 frames = 480 ticks.
        let mut song = cosmix_song::Song::new("t");
        let tid = song.create_track("Piano");
        let track = song.get_track_mut(tid).unwrap();
        track.program = 7;
        track.create_note(60, 100, 480, 480); // beat 2, one beat long

        let (schedules, length) = song_schedules(&song).unwrap();
        assert_eq!(schedules.len(), 1);
        assert_eq!(schedules[0].program, 7);
        assert_eq!(schedules[0].name.as_deref(), Some("Piano"));
        let ev = &schedules[0].events;
        assert_eq!(ev.len(), 2);
        assert_eq!(
            (ev[0].frame, ev[0].on, ev[0].key, ev[0].vel),
            (24_000, true, 60, 100)
        );
        assert_eq!((ev[1].frame, ev[1].on), (48_000, false));
        assert_eq!(length, 48_000 + SONG_RELEASE_TAIL_FRAMES);
    }

    #[test]
    fn song_schedules_off_before_on_at_equal_frames_and_min_duration() {
        // Back-to-back same-pitch notes: the first note's off lands exactly on
        // the second note's on; sorted (frame, on) the off must come first.
        let mut song = cosmix_song::Song::new("t");
        let tid = song.create_track("x");
        let t = song.get_track_mut(tid).unwrap();
        t.create_note(60, 100, 0, 480);
        t.create_note(60, 100, 480, 480);
        let (schedules, _) = song_schedules(&song).unwrap();
        let ev = &schedules[0].events;
        assert_eq!(ev.len(), 4);
        assert_eq!((ev[1].frame, ev[1].on), (24_000, false));
        assert_eq!((ev[2].frame, ev[2].on), (24_000, true));

        // A zero-duration note still gets a ≥1-frame sounding window.
        let mut song2 = cosmix_song::Song::new("z");
        let tid2 = song2.create_track("x");
        song2.get_track_mut(tid2).unwrap().create_note(64, 90, 0, 0);
        let (schedules2, _) = song_schedules(&song2).unwrap();
        let ev2 = &schedules2[0].events;
        assert_eq!(ev2[0].frame, 0);
        assert_eq!(ev2[1].frame, 1);
    }

    #[test]
    fn song_initial_controls_maps_volume_pan_mute_solo() {
        let mut song = cosmix_song::Song::new("t");
        let tid = song.create_track("a");
        {
            let t = song.get_track_mut(tid).unwrap();
            t.volume = 100; // the miditui default → unity
            t.pan = 64; // centre
            t.muted = true;
            t.solo = true;
        }
        let tid2 = song.create_track("b");
        {
            let t = song.get_track_mut(tid2).unwrap();
            t.volume = 0; // hard mute → silence floor
            t.pan = 0; // full left
        }
        let c = song_initial_controls(&song);
        assert_eq!(c.channels[0].fader_db, 0.0);
        assert_eq!(c.channels[0].pan, 0.0);
        assert!(c.channels[0].mute);
        assert!(c.channels[0].solo);
        assert_eq!(c.channels[1].fader_db, cosmix_mixer_schema::FADER_MIN_DB);
        assert_eq!(c.channels[1].pan, -1.0);
        assert!(!c.playing, "a loaded song starts stopped");

        // Seeding stores the canonical (clamped/quantised) values.
        let mut store = RevWriteStore::new();
        seed_strip_controls(&mut store, &c);
        let pp = PropPath::new("mixer.channels.0.fader").unwrap();
        assert_eq!(store.get(&pp), Some(&PropValue::Float(0.0)));
        let pp = PropPath::new("mixer.channels.1.pan").unwrap();
        assert_eq!(store.get(&pp), Some(&PropValue::Float(-1.0)));
    }

    #[test]
    fn song_schedules_rejects_too_many_tracks_and_zero_tempo() {
        let mut big = cosmix_song::Song::new("big");
        for i in 0..NUM_CHANNELS + 1 {
            big.create_track(format!("t{i}"));
        }
        assert!(song_schedules(&big).is_err());

        let mut zero = cosmix_song::Song::new("zero");
        zero.tempo = 0;
        assert!(song_schedules(&zero).is_err());
    }

    // --- Load barrier (ADR §17): a load-tagged song swap lands stopped at zero ---

    /// 2 s @ 48 kHz — a finite transport length so a seek can land off zero.
    const TEST_LEN_FRAMES: u64 = 96_000;

    /// A voiceless (no-soundfont) synth bank with a finite length. Enough for
    /// the transport-state assertions; no soundfont fixture required.
    fn voiceless_bank() -> Box<crate::mixer::MidiSynthBank> {
        Box::new(
            crate::mixer::MidiSynthBank::build(
                None,
                Vec::new(),
                TEST_LEN_FRAMES,
                crate::mixer::SongMeta::default(),
            )
            .expect("voiceless bank builds"),
        )
    }

    fn playing_controls() -> Controls {
        Controls {
            playing: true,
            ..Default::default()
        }
    }

    #[allow(clippy::type_complexity)]
    fn midi_rt_with_swap() -> (
        RtState<MailboxSink>,
        rtrb::Producer<SongBankSwap>,
        rtrb::Consumer<Box<crate::mixer::MidiSynthBank>>,
        rtrb::Producer<RtCommand>,
    ) {
        let (ctrl_tx, ctrl_rx) = RingBuffer::<RtCommand>::new(8);
        let (applied_tx, _applied_rx) = RingBuffer::<AppliedMsg>::new(8);
        let meters = Arc::new(MeterMailbox::new());
        let transport_pos = Arc::new(AtomicU64::new(0));
        let applied_fault = Arc::new(AtomicBool::new(false));
        let bank = *voiceless_bank();
        let (new_tx, swap, old_rx) = song_swap_rings(2);
        let rt = RtState::new(
            ctrl_rx,
            applied_tx,
            MailboxSink(meters),
            transport_pos,
            applied_fault,
            SourceProfile::MidiSynth(bank),
        )
        .with_song_swap(swap);
        (rt, new_tx, old_rx, ctrl_tx)
    }

    #[test]
    fn load_tagged_swap_forces_stop_at_zero_over_a_stale_play() {
        let (mut rt, mut new_tx, mut old_rx, mut ctrl_tx) = midi_rt_with_swap();
        // Get the transport playing at a non-zero position.
        ctrl_tx
            .push(RtCommand::Seek {
                frame: 240,
                revision: 1,
            })
            .unwrap();
        ctrl_tx
            .push(RtCommand::SetControls {
                controls: playing_controls(),
                revision: 2,
            })
            .unwrap();
        rt.run_block(64, None);
        assert!(
            rt.engine.is_playing(),
            "transport is playing before the load"
        );
        assert!(
            rt.engine.transport_frame() > 0,
            "transport advanced to a non-zero playhead"
        );

        // Same block: install a LOAD-tagged bank AND latch a stale Play. The
        // barrier must win.
        new_tx
            .push(SongBankSwap {
                bank: voiceless_bank(),
                load: true,
            })
            .unwrap();
        ctrl_tx
            .push(RtCommand::SetControls {
                controls: playing_controls(),
                revision: 3,
            })
            .unwrap();
        rt.run_block(64, None);

        assert_eq!(
            rt.engine.transport_frame(),
            0,
            "a document load renders its first block stopped at frame zero"
        );
        assert!(
            !rt.engine.is_playing(),
            "the load barrier overrides a stale queued Play"
        );
        assert!(
            old_rx.pop().is_ok(),
            "the displaced bank ships back for off-RT deallocation"
        );
    }

    #[test]
    fn a_load_anywhere_in_a_multi_swap_batch_triggers_the_barrier() {
        let (mut rt, mut new_tx, mut old_rx, mut ctrl_tx) = midi_rt_with_swap();
        ctrl_tx
            .push(RtCommand::Seek {
                frame: 240,
                revision: 1,
            })
            .unwrap();
        ctrl_tx
            .push(RtCommand::SetControls {
                controls: playing_controls(),
                revision: 2,
            })
            .unwrap();
        rt.run_block(64, None);
        assert!(rt.engine.is_playing() && rt.engine.transport_frame() > 0);

        // One block drains BOTH: an edit then a load. The load anywhere in the
        // batch must arm the barrier (song_load_swapped OR-accumulates).
        new_tx
            .push(SongBankSwap {
                bank: voiceless_bank(),
                load: false,
            })
            .unwrap();
        new_tx
            .push(SongBankSwap {
                bank: voiceless_bank(),
                load: true,
            })
            .unwrap();
        rt.run_block(64, None);

        assert_eq!(
            rt.engine.transport_frame(),
            0,
            "a load anywhere in the swap batch lands stopped at zero"
        );
        assert!(!rt.engine.is_playing());
        // Both displaced banks ship back for off-RT dealloc.
        assert!(old_rx.pop().is_ok());
        assert!(old_rx.pop().is_ok());
    }

    #[test]
    fn edit_tagged_swap_preserves_playing_and_position() {
        let (mut rt, mut new_tx, mut old_rx, mut ctrl_tx) = midi_rt_with_swap();
        ctrl_tx
            .push(RtCommand::Seek {
                frame: 240,
                revision: 1,
            })
            .unwrap();
        ctrl_tx
            .push(RtCommand::SetControls {
                controls: playing_controls(),
                revision: 2,
            })
            .unwrap();
        rt.run_block(64, None);
        let before = rt.engine.transport_frame();
        assert!(before > 0 && rt.engine.is_playing());

        // An EDIT-tagged bank must NOT reset the transport.
        new_tx
            .push(SongBankSwap {
                bank: voiceless_bank(),
                load: false,
            })
            .unwrap();
        rt.run_block(64, None);

        assert!(
            rt.engine.is_playing(),
            "an edit preserves the playing state"
        );
        assert!(
            rt.engine.transport_frame() >= before,
            "an edit preserves (and advances) the playhead, never rewinds it"
        );
        assert!(old_rx.pop().is_ok());
    }
}
