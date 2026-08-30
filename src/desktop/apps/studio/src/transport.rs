use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cosmix_mixer_schema::{
    DspApplied, LeafValue, MeterFrame, MixerSnapshotResponse, WriteRequest, NUM_CHANNELS,
};
use cosmix_musicd::mixer::{Controls, MidiSynthBank, SongMeta, SourceProfile, StemBank, SR};
use cosmix_musicd::mixer_host::{
    apply_write, build_snapshot_response, drain_applied, load_song, load_stem_session,
    prop_to_leaf, seed_store, seed_strip_controls, song_initial_controls, song_swap_rings,
    spawn_rt_thread, stem_swap_rings, store_set_transport_length, AppliedMsg, MixerCtl, RingSink,
    RtCommand, RtState, SongBankSwap,
};
use cosmix_musicd::rt_sched::AudioRuntime;
use ctk::transport::{
    ChangedEvent, MixerConnectionState, MixerTransport, TransportEvent, TransportMessage,
    TransportReply,
};
use rtrb::{Consumer, RingBuffer};

const CONTROL_RING_CAPACITY: usize = 1024;
const APPLIED_RING_CAPACITY: usize = 256;
// One second of 60 Hz frames (~30 KB). rtrb is SPSC, so the producer cannot
// overwrite-oldest: when full, the NEWEST frame is dropped and the consumer
// briefly sees stale data. The consumer drains fully every UI frame, so
// reaching this bound needs a ≥1 s UI stall, and staleness self-heals on the
// next 60 Hz publish (≤16.7 ms) — bounded divergence from the daemon's
// latest-wins mailbox, accepted for a bench instrument.
const METER_RING_CAPACITY: usize = 60;
const POSITION_PUBLISH_PERIOD: Duration = Duration::from_millis(100);
const GENERATION: u64 = 1;
/// Live-edit lanes: in-flight rebuilt banks (edits are ~1/s human-paced) and
/// transient preview notes (a burst is a handful of on/offs).
const SONG_SWAP_RING_CAPACITY: usize = 8;
/// Region-edit bank swaps are commit-paced (drag releases, key ops).
const STEM_SWAP_RING_CAPACITY: usize = 8;
const NOTE_RING_CAPACITY: usize = 64;

pub type SongMetadataSlot = Arc<Mutex<Option<(SongMeta, [Option<String>; NUM_CHANNELS])>>>;

pub enum InProcessSource {
    StemManifest(PathBuf),
    BenchmarkMultitone,
    /// A cosmix-song file (`.json`/`.oxm`/`.mid`) played through the
    /// `midi-synth.v1` profile; `soundfont` overrides the song's own path.
    Song {
        path: PathBuf,
        soundfont: Option<PathBuf>,
    },
    /// A bare `studio` launch: an EMPTY song session — everything loads
    /// afterwards through the native file requester. `soundfont` is the
    /// `--soundfont` override; otherwise the system GM banks are probed and
    /// a missing font just means silence until one is opened.
    Empty {
        soundfont: Option<PathBuf>,
    },
}

/// Preferred GM bank filenames, tried in order within each search directory.
const PREFERRED_SOUNDFONTS: &[&str] = &[
    "MuseScore_General.sf2",
    "FluidR3_GM_GS.sf2",
    "GeneralUser-GS.sf2",
    "FreePats-GM.sf2",
    "default.sf2",
];

/// The cosmix soundfont store (`$XDG_DATA_HOME/cosmix/musicd`, i.e.
/// `~/.local/share/cosmix/musicd`) — where musicd's fetch/render tooling
/// keeps the GM banks; and the common system location as a fallback.
fn soundfont_search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    if let Some(data_home) = data_home {
        dirs.push(data_home.join("cosmix/musicd"));
    }
    dirs.push(PathBuf::from("/usr/share/soundfonts"));
    dirs
}

/// Find a usable default soundfont for a bare launch: a preferred GM bank in
/// a known directory, else ANY `.sf2` there. `None` = silent until one is
/// opened via File > Open SoundFont.
fn probe_system_soundfont() -> Option<PathBuf> {
    for dir in soundfont_search_dirs() {
        for name in PREFERRED_SOUNDFONTS {
            let path = dir.join(name);
            if path.is_file() {
                return Some(path);
            }
        }
        // Any .sf2 in the directory (alphabetical, deterministic).
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let mut sf2: Vec<PathBuf> = entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("sf2")))
                .collect();
            sf2.sort();
            if let Some(path) = sf2.into_iter().next() {
                return Some(path);
            }
        }
    }
    None
}

/// The editing side-channel a `--song` launch hands the UI: the song document
/// plus the lock-free lanes into the running engine. Everything the piano-roll
/// edit loop needs that is NOT part of the revisioned write surface.
pub struct SongEditHandle {
    /// The authoritative song document (the UI owns edits + undo).
    pub song: cosmix_song::Song,
    /// The loaded soundfont (None = silent until one is opened), cached so
    /// an edit rebuild skips the file parse.
    pub soundfont: Option<std::sync::Arc<rustysynth::SoundFont>>,
    /// Where `soundfont` was loaded from — the editor's skip-reload identity
    /// (may differ from the song's `soundfont_path` under `--soundfont`).
    pub soundfont_source: Option<std::path::PathBuf>,
    /// Freshly-built banks go here (tagged load/edit); the RT thread swaps them in.
    pub bank_tx: rtrb::Producer<SongBankSwap>,
    /// Displaced banks come back here — pop and drop them (off-RT dealloc).
    pub bank_rx: rtrb::Consumer<Box<MidiSynthBank>>,
    /// Transient live-note previews (RtCommand::NoteEvent only).
    pub note_tx: rtrb::Producer<RtCommand>,
    /// UI → transport: the loaded song's display metadata + per-track names.
    /// The transport applies changes to the `mixer.song.*` /
    /// `mixer.channels.N.name` leaves as internal revisioned writes, so the
    /// footer and strip names FOLLOW File > Open Song instead of freezing at
    /// the launch-time seed.
    pub meta_slot: SongMetadataSlot,
}

/// One channel's waveform source for the arranger view: the LOD pyramid folded
/// from the decoded stem BEFORE the bank moved onto the RT thread — the UI
/// keeps the pyramid (~¼ of the stem's bytes), never the samples — plus the
/// channel's initial region list (the engine's non-destructive document).
pub struct WaveLaneSource {
    pub channel: usize,
    pub name: Option<String>,
    pub pyramid: ctk::wave::WavePyramid,
    pub regions: Vec<cosmix_musicd::mixer::Region>,
}

/// The region-editing side-channel a `--stems` launch hands the UI: shared
/// audio sources for zero-copy bank rebuilds, the initial document, and the
/// lock-free swap lanes into the running engine — the stem analogue of
/// [`SongEditHandle`].
pub struct StemEditParts {
    /// The padded per-channel audio, `Arc`-shared with the RT bank — a
    /// rebuild moves region metadata, never samples.
    pub sources: [std::sync::Arc<Vec<f32>>; NUM_CHANNELS],
    pub names: [Option<String>; NUM_CHANNELS],
    pub song: SongMeta,
    /// The manifest session length (the timeline may extend past it).
    pub base_length_frames: u64,
    /// The initial per-channel region document (one full region per stem).
    pub initial_regions: [Vec<cosmix_musicd::mixer::Region>; NUM_CHANNELS],
    /// The session document metadata (stem file references + song header) —
    /// what Save Session re-emits with the live region document.
    pub session: cosmix_musicd::mixer_host::StemSessionMeta,
    /// Freshly-rebuilt banks go here; the RT thread swaps them in.
    pub bank_tx: rtrb::Producer<Box<StemBank>>,
    /// Displaced banks come back here — pop and drop them (off-RT dealloc;
    /// their audio Arcs are shared, so a drop frees only region metadata).
    pub bank_rx: rtrb::Consumer<Box<StemBank>>,
}

/// The stem session's waveform lanes, handed to the UI at construction.
pub struct StemWaves {
    pub lanes: Vec<WaveLaneSource>,
    pub length_frames: u64,
    /// The live region-edit handle (always present on `--stems` launches).
    pub edit: Option<StemEditParts>,
}

impl StemWaves {
    fn from_bank(bank: &StemBank) -> Self {
        let lanes = bank
            .stems()
            .iter()
            .enumerate()
            .filter(|(_, samples)| !samples.is_empty())
            .map(|(channel, samples)| WaveLaneSource {
                channel,
                name: bank.names()[channel].clone(),
                pyramid: ctk::wave::WavePyramid::new(samples),
                regions: bank.regions()[channel].clone(),
            })
            .collect();
        Self {
            lanes,
            length_frames: bank.length_frames(),
            edit: None,
        }
    }
}

pub struct InProcessTransport {
    service_name: String,
    ctl: Mutex<MixerCtl>,
    applied_rx: Mutex<Consumer<AppliedMsg>>,
    meter_rx: Mutex<Consumer<MeterFrame>>,
    transport_pos: Arc<AtomicU64>,
    real_audio: Arc<AtomicBool>,
    audio_fault: Arc<AtomicBool>,
    applied_fault: Arc<AtomicBool>,
    /// RT-runtime observability shared with the engine thread (musicd 0.23.0's
    /// `spawn_rt_thread`/`build_snapshot_response` contract): the callback
    /// primes it with the negotiated block size, snapshots read it back.
    audio_runtime: Arc<AudioRuntime>,
    source_profile_id: &'static str,
    benchmark_eligible: bool,
    pending_events: VecDeque<TransportEvent>,
    pending_messages: VecDeque<TransportMessage>,
    first_poll: bool,
    last_position_emit: Instant,
    last_position_frame: u64,
    /// The RT-active stem bank's timeline length (region-edit swaps update
    /// it); mirrored into the `transport.length` leaf when it changes.
    engine_length: Option<Arc<AtomicU64>>,
    last_length_frames: u64,
    /// UI-deposited song metadata + track names (File > Open Song), applied
    /// to the read-only display leaves as internal revisioned writes.
    meta_slot: Option<SongMetadataSlot>,
    _rt_thread: Option<std::thread::JoinHandle<()>>,
}

impl InProcessTransport {
    /// Build the transport (and spawn its RT thread). A `--song` source also
    /// returns the [`SongEditHandle`] for the piano-roll edit loop; a
    /// `--stems` source returns [`StemWaves`] for the arranger view; every
    /// other slot is `None`.
    pub fn new(
        source: InProcessSource,
    ) -> Result<(Self, Option<SongEditHandle>, Option<StemWaves>), String> {
        let mut initial_controls = None;
        let mut edit_handle_parts = None;
        let mut stem_waves = None;
        let mut stem_swap = None;
        let profile = match source {
            InProcessSource::StemManifest(path) => {
                let (bank, session) = load_stem_session(&path)
                    .map_err(|error| format!("load stem session {}: {error}", path.display()))?;
                let mut waves = StemWaves::from_bank(&bank);
                let (bank_tx, swap, bank_rx) = stem_swap_rings(STEM_SWAP_RING_CAPACITY);
                waves.edit = Some(StemEditParts {
                    sources: bank.stems().clone(),
                    names: bank.names().clone(),
                    song: bank.song().clone(),
                    base_length_frames: session.base_length_frames,
                    initial_regions: bank.regions().clone(),
                    session,
                    bank_tx,
                    bank_rx,
                });
                stem_waves = Some(waves);
                stem_swap = Some((swap, bank.length_frames()));
                SourceProfile::StemSession(bank)
            }
            InProcessSource::BenchmarkMultitone => SourceProfile::BenchmarkMultitone,
            InProcessSource::Song { path, soundfont } => {
                let song = load_song(&path).map_err(|error| error.to_string())?;
                let sf_path = soundfont
                    .or_else(|| song.get_soundfont_path().map(PathBuf::from))
                    .ok_or_else(|| {
                        "no soundfont: pass --soundfont or set the song's soundfont_path"
                            .to_string()
                    })?;
                let sf = cosmix_musicd::synth::load_soundfont(&sf_path)
                    .map_err(|error| error.to_string())?;
                let bank = cosmix_musicd::mixer_host::song_bank_with(&song, Some(&sf))
                    .map_err(|error| format!("build synth bank {}: {error}", path.display()))?;
                // The schedule carries no mix state — map the song's per-track
                // volume/pan/mute/solo onto the strips.
                initial_controls = Some(song_initial_controls(&song));
                edit_handle_parts = Some((song, Some(sf), Some(sf_path), bank.length_frames()));
                SourceProfile::MidiSynth(bank)
            }
            InProcessSource::Empty { soundfont } => {
                // Bare launch: an empty song; the requester loads everything
                // afterwards. A `--soundfont` override or a system GM bank
                // makes the session audible immediately; neither existing
                // just means silence until File > Open SoundFont.
                let song = cosmix_song::Song::default();
                let sf_path = soundfont.or_else(probe_system_soundfont);
                let sf = match &sf_path {
                    Some(path) => Some(
                        cosmix_musicd::synth::load_soundfont(path)
                            .map_err(|error| error.to_string())?,
                    ),
                    None => None,
                };
                let bank = cosmix_musicd::mixer_host::song_bank_with(&song, sf.as_ref())
                    .map_err(|error| format!("build empty session: {error}"))?;
                edit_handle_parts = Some((song, sf, sf_path, bank.length_frames()));
                SourceProfile::MidiSynth(bank)
            }
        };
        let (mut transport, mut rt) = Self::assemble_with(profile, initial_controls);

        // The --stems path wires the live region-edit swap lanes before the
        // RT thread runs (mirroring the --song bank-swap wiring below).
        // Baselining the mirror at the seeded session length keeps a
        // length-preserving first edit from emitting a spurious change.
        if let Some((swap, base_length)) = stem_swap {
            transport.engine_length = Some(swap.active_length.clone());
            transport.last_length_frames = base_length;
            rt = rt.with_stem_swap(swap);
        }

        // The --song path wires the live-edit lanes before the RT thread runs.
        let edit_handle =
            if let Some((song, soundfont, soundfont_source, song_length)) = edit_handle_parts {
                let (bank_tx, swap, bank_rx) = song_swap_rings(SONG_SWAP_RING_CAPACITY);
                let (note_tx, note_rx) = RingBuffer::<RtCommand>::new(NOTE_RING_CAPACITY);
                // Song edits that lengthen the song update the transport.length
                // leaf through the same mirror the stems path uses; baselined at
                // the seeded length so an unchanged first swap stays silent.
                transport.engine_length = Some(swap.active_length.clone());
                transport.last_length_frames = song_length;
                rt = rt.with_song_swap(swap).with_aux_commands(note_rx);
                let meta_slot = Arc::new(Mutex::new(None));
                transport.meta_slot = Some(meta_slot.clone());
                Some(SongEditHandle {
                    song,
                    soundfont,
                    soundfont_source,
                    bank_tx,
                    bank_rx,
                    note_tx,
                    meta_slot,
                })
            } else {
                None
            };

        transport._rt_thread = Some(spawn_rt_thread(
            rt,
            transport.real_audio.clone(),
            transport.audio_fault.clone(),
            transport.audio_runtime.clone(),
        ));
        Ok((transport, edit_handle, stem_waves))
    }

    /// `initial_controls`, when given (a loaded song's per-track mix state), is
    /// seeded into the store (before the defaults), mirrored into
    /// `MixerCtl.controls`, and shipped to the RT engine as the first control
    /// snapshot — so the board, the write mirror, and the audio graph all agree
    /// from the first block.
    fn assemble_with(
        profile: SourceProfile,
        initial_controls: Option<Controls>,
    ) -> (Self, RtState<RingSink>) {
        let source_profile_id = profile.id();
        let benchmark_eligible = profile.benchmark_eligible();
        let (names, transport_length_secs, song): ([Option<String>; NUM_CHANNELS], f64, SongMeta) =
            match &profile {
                SourceProfile::StemSession(bank) => (
                    bank.names().clone(),
                    bank.length_frames() as f64 / SR as f64,
                    bank.song().clone(),
                ),
                SourceProfile::MidiSynth(bank) => (
                    bank.names().clone(),
                    bank.length_frames() as f64 / SR as f64,
                    bank.song().clone(),
                ),
                SourceProfile::BenchmarkMultitone => {
                    (std::array::from_fn(|_| None), 0.0, SongMeta::default())
                }
            };

        let (mut ctrl_tx, ctrl_rx) = RingBuffer::<RtCommand>::new(CONTROL_RING_CAPACITY);
        let (applied_tx, applied_rx) = RingBuffer::<AppliedMsg>::new(APPLIED_RING_CAPACITY);
        let (meter_tx, meter_rx) = RingBuffer::<MeterFrame>::new(METER_RING_CAPACITY);
        let transport_pos = Arc::new(AtomicU64::new(0));
        let real_audio = Arc::new(AtomicBool::new(false));
        let audio_fault = Arc::new(AtomicBool::new(false));
        let applied_fault = Arc::new(AtomicBool::new(false));
        // Deliberately non-RT (priority 0, "do not promote"), NOT the daemon's
        // configured_rt_priority(): 0.25.0's RLIMIT_RTTIME deadman terminates
        // the whole process on a wedged RT callback, which the supervised
        // daemon absorbs via Restart=on-failure — studio IS the GUI holding
        // unsaved work, and its callback ran SCHED_OTHER for its entire
        // pre-0.23 life. Giving studio RT is a product decision that needs a
        // supervision story first, not a side effect of tracking the API.
        let audio_runtime = Arc::new(AudioRuntime::new(0));
        let rt = RtState::new(
            ctrl_rx,
            applied_tx,
            RingSink(meter_tx),
            transport_pos.clone(),
            applied_fault.clone(),
            profile,
        );

        let mut store = Default::default();
        let seeded = initial_controls.is_some();
        let controls = initial_controls.unwrap_or_default();
        if seeded {
            // Before seed_store: seed is first-write-wins, so the song's mix
            // state must land before the default loop touches those leaves.
            seed_strip_controls(&mut store, &controls);
            // The engine starts on the A.8 defaults — ship the mapped snapshot
            // as the first command so the audio graph matches the store/mirror
            // from the first processed block (revision 0: a seed, not a write).
            let pushed = ctrl_tx.push(RtCommand::SetControls {
                controls,
                revision: 0,
            });
            assert!(pushed.is_ok(), "fresh control ring rejected initial seed");
        }
        seed_store(
            &mut store,
            &names,
            transport_length_secs,
            &song,
            source_profile_id,
            benchmark_eligible,
        );
        let ctl = MixerCtl {
            store,
            controls,
            ctrl_tx,
            rev_path: BTreeMap::new(),
        };
        let last_position_frame = transport_pos.load(Ordering::Relaxed);
        (
            Self {
                service_name: format!("{}-{}", crate::IDENTITY.slug, std::process::id()),
                ctl: Mutex::new(ctl),
                applied_rx: Mutex::new(applied_rx),
                meter_rx: Mutex::new(meter_rx),
                transport_pos,
                real_audio,
                audio_fault,
                applied_fault,
                audio_runtime,
                source_profile_id,
                benchmark_eligible,
                pending_events: VecDeque::new(),
                pending_messages: VecDeque::new(),
                first_poll: true,
                last_position_emit: Instant::now(),
                last_position_frame,
                engine_length: None,
                last_length_frames: 0,
                meta_slot: None,
                _rt_thread: None,
            },
            rt,
        )
    }

    fn queue_reply(&mut self, request_id: u64, reply: TransportReply) {
        self.pending_events.push_back(TransportEvent::Reply {
            request_id,
            result: Ok(reply),
            completed_at: Some(Instant::now()),
        });
    }

    fn snapshot(&mut self) -> MixerSnapshotResponse {
        let real_audio = self.real_audio.load(Ordering::Acquire);
        let audio_fault = self.audio_fault.load(Ordering::Acquire);
        let applied_fault = self.applied_fault.load(Ordering::Acquire);
        let runtime = self.audio_runtime.clone();
        build_snapshot_response(
            self.ctl.get_mut().expect("mixer control mutex poisoned"),
            &runtime,
            real_audio,
            audio_fault,
            applied_fault,
            self.source_profile_id,
            self.benchmark_eligible,
        )
    }

    fn collect_applied(&mut self) {
        while let Ok(message) = self
            .applied_rx
            .get_mut()
            .expect("applied ring mutex poisoned")
            .pop()
        {
            let expanded = drain_applied(
                &mut self
                    .ctl
                    .get_mut()
                    .expect("mixer control mutex poisoned")
                    .rev_path,
                message.revision,
            );
            for (revision, _) in expanded {
                self.pending_messages.push_back(TransportMessage::Applied {
                    generation: GENERATION,
                    applied: DspApplied {
                        revision,
                        sample_frame: message.sample_frame,
                    },
                });
            }
        }
    }

    fn collect_changed(&mut self) {
        let changed = self
            .ctl
            .get_mut()
            .expect("mixer control mutex poisoned")
            .store
            .drain_changed();
        for change in changed {
            let value = prop_to_leaf(&change.canonical_value)
                .expect("mixer store emitted a non-leaf property value");
            self.pending_messages.push_back(TransportMessage::Changed {
                generation: GENERATION,
                event: ChangedEvent {
                    path: change.path.as_str().to_string(),
                    revision: change.revision,
                    value,
                    source_id: Some(change.source_id),
                },
            });
        }
    }

    /// Apply UI-deposited song metadata + track names to the display leaves
    /// (`mixer.song.*`, `mixer.channels.N.name`) as internal revisioned
    /// writes — the footer and strip names follow File > Open Song through
    /// the ordinary changed stream instead of freezing at the launch seed.
    fn collect_song_meta(&mut self) {
        let Some(slot) = &self.meta_slot else { return };
        let Some((meta, names)) = slot.lock().expect("meta slot poisoned").take() else {
            return;
        };
        use cosmix_musicd::mixer_host::{PropPath, PropValue, RevWriteRequest};
        let mut ctl = self.ctl.lock().expect("mixer control mutex poisoned");
        let mut set = |path: String, value: PropValue| {
            if let Ok(pp) = PropPath::new(&path) {
                let _ = ctl
                    .store
                    .apply(RevWriteRequest::new(pp, value, "ui-meta"), "engine");
            }
        };
        set("mixer.song.title".into(), PropValue::String(meta.title));
        set("mixer.song.artist".into(), PropValue::String(meta.artist));
        set(
            "mixer.song.copyright".into(),
            PropValue::String(meta.copyright),
        );
        for (channel, name) in names.iter().enumerate() {
            set(
                format!("mixer.channels.{channel}.name"),
                PropValue::String(
                    name.clone()
                        .unwrap_or_else(|| format!("Ch {}", channel + 1)),
                ),
            );
        }
    }

    /// Mirror the RT-active bank's timeline length into the
    /// `transport.length` leaf (an internal revisioned write, so the change
    /// reaches clients through the ordinary changed stream) — seeks, footer
    /// and scrubber domain then track region edits that move the end.
    fn collect_engine_length(&mut self) {
        let Some(atomic) = &self.engine_length else {
            return;
        };
        let frames = atomic.load(Ordering::Relaxed);
        if frames == 0 || frames == self.last_length_frames {
            return;
        }
        self.last_length_frames = frames;
        let secs = frames as f64 / SR as f64;
        let mut ctl = self.ctl.lock().expect("mixer control mutex poisoned");
        store_set_transport_length(&mut ctl, secs);
    }

    fn collect_position(&mut self) {
        if self.last_position_emit.elapsed() < POSITION_PUBLISH_PERIOD {
            return;
        }
        // Advance the sampling clock on every boundary, moved or not —
        // mirroring event_loop's fixed ~10 Hz tick grid (an emission-gated
        // reset would emit immediately on the first movement after idle and
        // phase-lock to movement instead of the grid).
        self.last_position_emit = Instant::now();
        let frame = self.transport_pos.load(Ordering::Relaxed);
        if frame == self.last_position_frame {
            return;
        }
        self.last_position_frame = frame;
        let revision = self
            .ctl
            .get_mut()
            .expect("mixer control mutex poisoned")
            .store
            .revision();
        self.pending_messages.push_back(TransportMessage::Changed {
            generation: GENERATION,
            event: ChangedEvent {
                path: "transport.position".to_string(),
                revision,
                value: LeafValue::Number(frame as f64 / SR as f64),
                source_id: Some("engine".to_string()),
            },
        });
    }

    fn collect_latest_meter(&mut self) {
        let mut latest = None;
        let meter_rx = self.meter_rx.get_mut().expect("meter ring mutex poisoned");
        while let Ok(frame) = meter_rx.pop() {
            latest = Some(frame);
        }
        if let Some(frame) = latest {
            self.pending_messages.push_back(TransportMessage::Meter {
                generation: GENERATION,
                frame,
            });
        }
    }

    #[cfg(test)]
    fn new_headless(profile: SourceProfile) -> (Self, RtState<RingSink>) {
        Self::assemble_with(profile, None)
    }
}

impl MixerTransport for InProcessTransport {
    fn service_name(&self) -> &str {
        &self.service_name
    }

    fn issue_write(&mut self, request_id: u64, request: &WriteRequest) -> Result<(), String> {
        let response = {
            let ctl = self.ctl.get_mut().expect("mixer control mutex poisoned");
            let MixerCtl {
                store,
                controls,
                ctrl_tx,
                rev_path,
            } = ctl;
            let ring_has_slot = ctrl_tx.slots() >= 1;
            let (_, response, command) = apply_write(
                store,
                controls,
                rev_path,
                ring_has_slot,
                &self.service_name,
                request.clone(),
            );
            if let Some(command) = command {
                assert!(
                    ctrl_tx.push(command).is_ok(),
                    "mixer control ring filled after its slot reservation"
                );
            }
            response
        };
        self.queue_reply(request_id, TransportReply::Write(Ok(response)));
        Ok(())
    }

    fn request_snapshot(&mut self, request_id: u64) -> Result<(), String> {
        let snapshot = self.snapshot();
        self.queue_reply(request_id, TransportReply::Snapshot(snapshot));
        Ok(())
    }

    fn request_position(&mut self, request_id: u64) -> Result<(), String> {
        let seconds = self.transport_pos.load(Ordering::Relaxed) as f64 / SR as f64;
        self.queue_reply(request_id, TransportReply::Position(seconds));
        Ok(())
    }

    fn poll_events(&mut self, out: &mut Vec<TransportEvent>) {
        out.clear();
        if self.first_poll {
            self.first_poll = false;
            out.push(TransportEvent::Connection {
                state: MixerConnectionState::Connected,
                generation: GENERATION,
            });
        }
        out.extend(self.pending_events.drain(..));
    }

    fn poll_messages(&mut self, out: &mut Vec<TransportMessage>) {
        out.clear();
        self.collect_song_meta();
        self.collect_engine_length();
        self.collect_applied();
        self.collect_changed();
        self.collect_position();
        self.collect_latest_meter();
        out.extend(self.pending_messages.drain(..));
    }

    fn discard_backlog(&mut self) {
        self.pending_messages.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_mixer_schema::WriteResponse;
    use cosmix_musicd::mixer::{StemBank, BLOCK};
    use cosmix_musicd::mixer_host::{MailboxSink, MeterMailbox, MeterSink};
    use ctk::transport::TransportPoll;

    fn write(path: &str, value: LeafValue, op_id: &str, if_revision: Option<u64>) -> WriteRequest {
        WriteRequest {
            path: path.to_string(),
            value,
            op_id: op_id.to_string(),
            if_revision,
        }
    }

    fn poll_without_connection(transport: &mut InProcessTransport) -> TransportPoll {
        let mut poll = TransportPoll::default();
        transport.poll_events(&mut poll.events);
        transport.poll_messages(&mut poll.messages);
        poll.events.retain(|event| {
            !matches!(
                event,
                TransportEvent::Connection {
                    state: MixerConnectionState::Connected,
                    generation: GENERATION
                }
            )
        });
        poll
    }

    #[test]
    fn applied_watermark_expands_every_accepted_revision() {
        let (mut transport, mut rt) =
            InProcessTransport::new_headless(SourceProfile::BenchmarkMultitone);
        transport
            .issue_write(
                1,
                &write(
                    "mixer.channels.0.fader",
                    LeafValue::Number(-6.0),
                    "op-1",
                    None,
                ),
            )
            .unwrap();
        transport
            .issue_write(
                2,
                &write("mixer.channels.1.pan", LeafValue::Number(0.5), "op-2", None),
            )
            .unwrap();

        let before = poll_without_connection(&mut transport);
        assert!(!before
            .messages
            .iter()
            .any(|message| matches!(message, TransportMessage::Applied { .. })));

        rt.run_block(BLOCK, None);
        let after = poll_without_connection(&mut transport);
        let revisions: Vec<_> = after
            .messages
            .iter()
            .filter_map(|message| match message {
                TransportMessage::Applied { applied, .. } => Some(applied.revision),
                _ => None,
            })
            .collect();
        assert_eq!(revisions, vec![1, 2]);
    }

    #[test]
    fn stale_revision_is_rejected_with_current_state() {
        let (mut transport, _rt) =
            InProcessTransport::new_headless(SourceProfile::BenchmarkMultitone);
        transport
            .issue_write(
                1,
                &write(
                    "mixer.channels.0.fader",
                    LeafValue::Number(-6.0),
                    "op-1",
                    None,
                ),
            )
            .unwrap();
        let _ = poll_without_connection(&mut transport);
        transport
            .issue_write(
                2,
                &write(
                    "mixer.channels.0.fader",
                    LeafValue::Number(-12.0),
                    "op-2",
                    Some(0),
                ),
            )
            .unwrap();

        let poll = poll_without_connection(&mut transport);
        let response = poll.events.iter().find_map(|event| match event {
            TransportEvent::Reply {
                request_id: 2,
                result: Ok(TransportReply::Write(Ok(response))),
                ..
            } => Some(response),
            _ => None,
        });
        let Some(WriteResponse::Rejected(rejection)) = response else {
            panic!("expected a rejected write reply");
        };
        assert_eq!(rejection.current_revision, 1);
        assert_eq!(rejection.current_value, LeafValue::Number(-6.0));
    }

    #[test]
    fn full_control_ring_returns_busy_without_advancing_revision() {
        let (mut transport, _rt) =
            InProcessTransport::new_headless(SourceProfile::BenchmarkMultitone);
        let ctl = transport.ctl.get_mut().unwrap();
        while ctl.ctrl_tx.slots() > 0 {
            ctl.ctrl_tx
                .push(RtCommand::SetControls {
                    controls: Controls::default(),
                    revision: 0,
                })
                .unwrap();
        }
        let revision_before = ctl.store.revision();

        transport
            .issue_write(
                7,
                &write(
                    "mixer.channels.0.fader",
                    LeafValue::Number(-6.0),
                    "busy",
                    None,
                ),
            )
            .unwrap();
        let poll = poll_without_connection(&mut transport);
        assert!(poll.events.iter().any(|event| matches!(
            event,
            TransportEvent::Reply {
                request_id: 7,
                result: Ok(TransportReply::Write(Ok(WriteResponse::Busy(_)))),
                ..
            }
        )));
        assert_eq!(
            transport.ctl.get_mut().unwrap().store.revision(),
            revision_before
        );
    }

    struct ParitySink {
        ring: RingSink,
        mailbox: MailboxSink,
    }

    impl MeterSink for ParitySink {
        fn publish(&mut self, frame: &MeterFrame) {
            self.ring.publish(frame);
            self.mailbox.publish(frame);
        }
    }

    #[test]
    fn ring_and_mailbox_sinks_publish_byte_identical_meter_frames() {
        let (mut ctrl_tx, ctrl_rx) = RingBuffer::<RtCommand>::new(2);
        let (applied_tx, _applied_rx) = RingBuffer::<AppliedMsg>::new(2);
        let (meter_tx, mut meter_rx) = RingBuffer::<MeterFrame>::new(2);
        let mailbox = Arc::new(MeterMailbox::new());
        let sink = ParitySink {
            ring: RingSink(meter_tx),
            mailbox: MailboxSink(mailbox.clone()),
        };
        let controls = Controls {
            playing: true,
            ..Default::default()
        };
        ctrl_tx
            .push(RtCommand::SetControls {
                controls,
                revision: 1,
            })
            .unwrap();
        let mut rt = RtState::new(
            ctrl_rx,
            applied_tx,
            sink,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicBool::new(false)),
            SourceProfile::BenchmarkMultitone,
        );
        for _ in 0..7 {
            rt.run_block(BLOCK, None);
        }

        let ring_frame = meter_rx.pop().expect("ring sink meter frame");
        let mailbox_frame = mailbox.read().expect("mailbox sink meter frame");
        assert_eq!(ring_frame.encode(), mailbox_frame);
    }

    #[test]
    fn snapshot_uses_the_daemon_leaf_set_and_session_seeding() {
        let mut names: [Option<String>; NUM_CHANNELS] = std::array::from_fn(|_| None);
        names[0] = Some("Lead".to_string());
        let bank = StemBank::new(std::array::from_fn(|_| Vec::new()), SR as u64 * 5)
            .with_names(names)
            .with_song(SongMeta {
                title: "Test Song".to_string(),
                artist: "Test Artist".to_string(),
                copyright: "Test Copyright".to_string(),
            });
        let (mut transport, _rt) =
            InProcessTransport::new_headless(SourceProfile::StemSession(bank));
        transport.request_snapshot(9).unwrap();
        let poll = poll_without_connection(&mut transport);
        let snapshot = poll.events.iter().find_map(|event| match event {
            TransportEvent::Reply {
                request_id: 9,
                result: Ok(TransportReply::Snapshot(snapshot)),
                ..
            } => Some(snapshot),
            _ => None,
        });
        let snapshot = snapshot.expect("snapshot reply");
        let paths: std::collections::BTreeSet<_> = snapshot
            .leaves
            .iter()
            .map(|leaf| leaf.path.clone())
            .collect();
        // The snapshot is the seeded control leaves PLUS the three runtime-
        // observability leaves build_snapshot_response appends from the
        // AudioRuntime view (musicd 0.23.0/0.25.0) — seed_leaves() excludes
        // those on purpose (they are engine-reported, never store-seeded).
        let expected: std::collections::BTreeSet<_> = cosmix_musicd::mixer_host::seed_leaves()
            .into_iter()
            .chain([
                cosmix_musicd::mixer_host::RT_PRIORITY_PATH.to_string(),
                cosmix_musicd::mixer_host::RT_TIME_US_PATH.to_string(),
                cosmix_musicd::mixer_host::BLOCK_FRAMES_PATH.to_string(),
            ])
            .collect();
        assert_eq!(paths, expected);
        assert_eq!(leaf(snapshot, "transport.length"), &LeafValue::Number(5.0));
        assert_eq!(
            leaf(snapshot, "mixer.engine"),
            &LeafValue::Enum("dsp".into())
        );
        assert_eq!(
            leaf(snapshot, "mixer.song.title"),
            &LeafValue::Enum("Test Song".into())
        );
        assert_eq!(
            leaf(snapshot, "mixer.song.artist"),
            &LeafValue::Enum("Test Artist".into())
        );
        assert_eq!(
            leaf(snapshot, "mixer.song.copyright"),
            &LeafValue::Enum("Test Copyright".into())
        );
        assert!(!snapshot.benchmark_eligible);
        // Values, not just names: the headless engine thread never ran, so the
        // runtime leaves must report the PENDING sentinel — a snapshot built
        // from some other (fresh) runtime would report the same, which is why
        // the primed round below is the load-bearing half.
        assert_eq!(
            leaf(snapshot, cosmix_musicd::mixer_host::RT_PRIORITY_PATH),
            &LeafValue::Number(cosmix_musicd::rt_sched::RT_PRIORITY_PENDING as f64)
        );
        assert_eq!(
            leaf(snapshot, cosmix_musicd::mixer_host::BLOCK_FRAMES_PATH),
            &LeafValue::Number(0.0)
        );
        assert_eq!(
            leaf(snapshot, cosmix_musicd::mixer_host::RT_TIME_US_PATH),
            &LeafValue::Number(0.0)
        );
        // Prime the transport's OWN runtime (side-effect-free at priority 0 —
        // promote_current_thread(0) is Disabled, the deadman never arms) and
        // prove the next snapshot reflects it: pins that snapshot() reads
        // `self.audio_runtime`, not some fresh runtime. What this deliberately
        // does NOT prove: that spawn_rt_thread receives the same Arc — the
        // headless path never spawns the engine thread, so that wiring (a
        // one-expression clone in new()) is covered by review, not this test.
        transport.audio_runtime.prime_from_callback(512);
        transport.request_snapshot(10).unwrap();
        let poll = poll_without_connection(&mut transport);
        let primed = poll
            .events
            .iter()
            .find_map(|event| match event {
                TransportEvent::Reply {
                    request_id: 10,
                    result: Ok(TransportReply::Snapshot(snapshot)),
                    ..
                } => Some(snapshot),
                _ => None,
            })
            .expect("primed snapshot reply");
        assert_eq!(
            leaf(primed, cosmix_musicd::mixer_host::RT_PRIORITY_PATH),
            &LeafValue::Number(0.0),
            "priority 0 = RT promotion deliberately disabled, never pending"
        );
        assert_eq!(
            leaf(primed, cosmix_musicd::mixer_host::BLOCK_FRAMES_PATH),
            &LeafValue::Number(512.0)
        );
        assert_eq!(
            leaf(primed, cosmix_musicd::mixer_host::RT_TIME_US_PATH),
            &LeafValue::Number(0.0)
        );
    }

    fn leaf<'a>(snapshot: &'a MixerSnapshotResponse, path: &str) -> &'a LeafValue {
        &snapshot
            .leaves
            .iter()
            .find(|leaf| leaf.path == path)
            .unwrap_or_else(|| panic!("missing snapshot leaf {path}"))
            .value
    }

    #[test]
    fn position_progress_is_silent_until_frames_move_and_uses_store_revision() {
        let (mut transport, _rt) =
            InProcessTransport::new_headless(SourceProfile::BenchmarkMultitone);
        let _ = poll_without_connection(&mut transport);
        transport.last_position_emit = Instant::now() - POSITION_PUBLISH_PERIOD;
        let stopped = poll_without_connection(&mut transport);
        assert!(!stopped.messages.iter().any(|message| matches!(
            message,
            TransportMessage::Changed { event, .. } if event.path == "transport.position"
        )));

        // The stopped poll above landed ON a due boundary, so it must have
        // RESET the sampling clock (event_loop's fixed ~10 Hz grid ticks
        // whether or not the position moved). Movement arriving right after
        // therefore may NOT emit until the NEXT boundary — the discriminator
        // against a movement-phase-locked implementation, which would emit
        // immediately here.
        transport.transport_pos.store(SR as u64, Ordering::Relaxed);
        let phase_locked = poll_without_connection(&mut transport);
        assert!(!phase_locked.messages.iter().any(|message| matches!(
            message,
            TransportMessage::Changed { event, .. } if event.path == "transport.position"
        )));

        transport
            .issue_write(
                11,
                &write("mixer.channels.0.pan", LeafValue::Number(0.25), "rev", None),
            )
            .unwrap();
        let _ = poll_without_connection(&mut transport);
        transport.last_position_emit = Instant::now() - POSITION_PUBLISH_PERIOD;
        let moving = poll_without_connection(&mut transport);
        assert!(moving.messages.iter().any(|message| matches!(
            message,
            TransportMessage::Changed { event, .. }
                if event.path == "transport.position"
                    && event.revision == 1
                    && event.value == LeafValue::Number(1.0)
                    && event.source_id.as_deref() == Some("engine")
        )));
    }
}
