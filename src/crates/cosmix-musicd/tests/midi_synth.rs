//! Headless `midi-synth.v1` engine tests: frame-keyed note scheduling +
//! deterministic render through the 32-channel mixer.
//!
//! Needs a SoundFont, supplied out-of-band (same convention as render_corpus):
//! set `MUSICD_TEST_SF2=/path/to/bank.sf2`. Absent → the tests skip (printing a
//! note) rather than failing, so CI stays green without the asset.
//!
//! Run: `MUSICD_TEST_SF2=/path/FluidR3_GM_GS.sf2 cargo test -p cosmix-musicd \
//!        --test midi_synth`

use std::path::PathBuf;

use cosmix_mixer_schema::{FLAG_NON_BENCH_SOURCE, MeterFrame};
use cosmix_musicd::mixer::{
    BLOCK, Controls, MidiSynthBank, MixerEngine, NoteEvent, SongMeta, SourceProfile, TrackSchedule,
};
use cosmix_musicd::synth;

/// The test note: middle C from 0.1 s to 0.5 s at 48 kHz.
const NOTE_ON: u64 = 4_800;
const NOTE_OFF: u64 = 24_000;
const LENGTH: u64 = 72_000; // note off + 1 s tail

fn test_sf2() -> Option<PathBuf> {
    std::env::var_os("MUSICD_TEST_SF2")
        .map(PathBuf::from)
        .filter(|p| p.exists())
}

/// Build the single-track test bank: one note on channel 0.
fn test_bank(sf2: &PathBuf) -> MidiSynthBank {
    let sf = synth::load_soundfont(sf2).expect("load test soundfont");
    let schedules = vec![TrackSchedule {
        channel: 0,
        program: 0,
        name: Some("Test Piano".into()),
        events: vec![
            NoteEvent {
                frame: NOTE_ON,
                on: true,
                key: 60,
                vel: 100,
            },
            NoteEvent {
                frame: NOTE_OFF,
                on: false,
                key: 60,
                vel: 0,
            },
        ],
    }];
    MidiSynthBank::build(Some(&sf), schedules, LENGTH, SongMeta::default()).expect("build bank")
}

/// A playing engine on the test bank.
fn playing_engine(sf2: &PathBuf) -> MixerEngine {
    let mut engine = MixerEngine::with_profile(false, SourceProfile::MidiSynth(test_bank(sf2)));
    engine.set_controls(
        &Controls {
            playing: true,
            ..Default::default()
        },
        1,
    );
    engine
}

/// Drive `frames` samples through the real audio path, returning the master
/// stereo plus every completed meter frame.
fn render(engine: &mut MixerEngine, frames: usize) -> (Vec<f32>, Vec<f32>, Vec<MeterFrame>) {
    let mut l = vec![0.0f32; frames];
    let mut r = vec![0.0f32; frames];
    let mut meters = Vec::new();
    let mut done = 0;
    while done < frames {
        let n = (frames - done).min(BLOCK);
        engine.process_block_audio(&mut l[done..done + n], &mut r[done..done + n], &mut meters);
        done += n;
    }
    (l, r, meters)
}

fn rms(x: &[f32]) -> f64 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|s| *s as f64 * *s as f64).sum::<f64>() / x.len() as f64).sqrt()
}

#[test]
fn note_sounds_at_its_frame_and_nowhere_before() {
    let Some(sf2) = test_sf2() else {
        eprintln!("skipping: set MUSICD_TEST_SF2 to run midi-synth render tests");
        return;
    };
    let mut engine = playing_engine(&sf2);
    let (l, _r, meters) = render(&mut engine, 48_000);

    // Sample-accurate: exact digital silence before the note-on frame.
    assert!(
        l[..NOTE_ON as usize].iter().all(|s| *s == 0.0),
        "master must be exactly silent before the first note-on"
    );
    // Audible while the note sounds.
    let note_rms = rms(&l[NOTE_ON as usize..NOTE_OFF as usize]);
    assert!(
        note_rms > 1e-4,
        "note window must carry signal (rms = {note_rms})"
    );
    // Every frame is real DSP on a musical (non-benchmark) source.
    assert!(!meters.is_empty());
    for f in &meters {
        assert_eq!(f.flags, FLAG_NON_BENCH_SOURCE);
    }
}

#[test]
fn render_is_deterministic_across_runs() {
    let Some(sf2) = test_sf2() else {
        eprintln!("skipping: set MUSICD_TEST_SF2 to run midi-synth render tests");
        return;
    };
    let (l1, r1, _) = render(&mut playing_engine(&sf2), 48_000);
    let (l2, r2, _) = render(&mut playing_engine(&sf2), 48_000);
    assert!(
        l1 == l2 && r1 == r2,
        "midi-synth render must be byte-deterministic across runs"
    );
}

#[test]
fn seek_silences_voices_and_rebinds_the_schedule() {
    let Some(sf2) = test_sf2() else {
        eprintln!("skipping: set MUSICD_TEST_SF2 to run midi-synth render tests");
        return;
    };
    let mut engine = playing_engine(&sf2);
    // Play into the middle of the note so a voice is ringing.
    let _ = render(&mut engine, 10_000);

    // Seek past the note-off: the ringing voice dies, nothing re-fires.
    engine.seek(30_000);
    let (l, _r, _) = render(&mut engine, 4_800);
    let post_rms = rms(&l);
    assert!(
        post_rms < 1e-6,
        "after a seek past the note nothing may sound (rms = {post_rms})"
    );

    // Seek back to zero: silence again until the note-on frame, then signal.
    engine.seek(0);
    let (l2, _r2, _) = render(&mut engine, 9_600);
    assert!(
        l2[..NOTE_ON as usize].iter().all(|s| *s == 0.0),
        "replay after RTZ must be silent before the note-on"
    );
    let replay_rms = rms(&l2[NOTE_ON as usize..]);
    assert!(
        replay_rms > 1e-4,
        "replay after RTZ must sound (rms = {replay_rms})"
    );
}

#[test]
fn pause_rings_out_freezes_transport_and_resume_chases_the_note() {
    let Some(sf2) = test_sf2() else {
        eprintln!("skipping: set MUSICD_TEST_SF2 to run midi-synth render tests");
        return;
    };
    let mut engine = playing_engine(&sf2);
    let _ = render(&mut engine, 9_600); // into the note
    let pos = engine.transport_frame();

    // Pause: the transport freezes; voices are RELEASED (DAW stop convention)
    // and ring out through the free-running idle path instead of clicking off.
    engine.set_controls(
        &Controls {
            playing: false,
            ..Default::default()
        },
        2,
    );
    let (l, _r, _) = render(&mut engine, 48_000);
    assert_eq!(
        engine.transport_frame(),
        pos,
        "transport frozen while paused"
    );
    let head = rms(&l[..4_800]);
    let tail = rms(&l[43_200..]);
    assert!(
        tail < 1e-3,
        "release tail must have rung out within a second (tail rms = {tail})"
    );
    assert!(
        tail <= head,
        "paused output must decay (head {head}, tail {tail})"
    );

    // Resume mid-note: the resync CHASES the spanning note and re-fires it.
    engine.set_controls(
        &Controls {
            playing: true,
            ..Default::default()
        },
        3,
    );
    let (l2, _r2, _) = render(&mut engine, 4_800);
    assert!(
        rms(&l2) > 1e-4,
        "resume mid-note must re-fire the held note (chase)"
    );
}

#[test]
fn live_note_preview_sounds_while_stopped() {
    let Some(sf2) = test_sf2() else {
        eprintln!("skipping: set MUSICD_TEST_SF2 to run midi-synth render tests");
        return;
    };
    // Never started: transport stopped from construction.
    let mut engine = MixerEngine::with_profile(false, SourceProfile::MidiSynth(test_bank(&sf2)));
    engine.set_controls(&Controls::default(), 1);

    // Nothing sounds before the preview note.
    let (l0, _, _) = render(&mut engine, 4_800);
    assert!(l0.iter().all(|s| *s == 0.0), "idle synth starts silent");

    // Fire a live note: audible with the transport stopped.
    engine.live_note(0, 72, 100, true);
    let (l1, _, _) = render(&mut engine, 9_600);
    assert!(
        rms(&l1) > 1e-4,
        "live preview must sound while stopped (rms = {})",
        rms(&l1)
    );

    // Release it: decays away.
    engine.live_note(0, 72, 0, false);
    let (l2, _, _) = render(&mut engine, 48_000);
    assert!(
        rms(&l2[43_200..]) < 1e-3,
        "released preview note must ring out"
    );

    // A channel with no synth ignores live notes instead of panicking.
    engine.live_note(31, 60, 100, true);
    let (l3, _, _) = render(&mut engine, 4_800);
    assert!(rms(&l3) < 1e-3, "synthless channel stays silent");
}

#[test]
fn seek_into_a_held_note_chases_it() {
    let Some(sf2) = test_sf2() else {
        eprintln!("skipping: set MUSICD_TEST_SF2 to run midi-synth render tests");
        return;
    };
    let mut engine = playing_engine(&sf2);
    // Seek straight into the middle of the note without ever playing it.
    engine.seek(12_000);
    let (l, _r, _) = render(&mut engine, 4_800);
    assert!(
        rms(&l) > 1e-4,
        "seeking into a spanning note must sound it (chase), rms = {}",
        rms(&l)
    );
}

#[test]
fn swap_midi_bank_switches_the_song_and_keeps_the_transport() {
    let Some(sf2) = test_sf2() else {
        eprintln!("skipping: set MUSICD_TEST_SF2 to run midi-synth render tests");
        return;
    };
    let sf = synth::load_soundfont(&sf2).expect("load test soundfont");
    let mut engine = playing_engine(&sf2);
    let _ = render(&mut engine, 9_600);
    let pos = engine.transport_frame();

    // A replacement song: same window, a different key.
    let schedules = vec![TrackSchedule {
        channel: 0,
        program: 0,
        name: None,
        events: vec![
            NoteEvent {
                frame: NOTE_ON,
                on: true,
                key: 72,
                vel: 100,
            },
            NoteEvent {
                frame: NOTE_OFF,
                on: false,
                key: 72,
                vel: 0,
            },
        ],
    }];
    let mut replacement = Box::new(
        MidiSynthBank::build(Some(&sf), schedules, LENGTH, SongMeta::default()).expect("build"),
    );
    assert!(engine.swap_midi_bank(&mut replacement));
    assert_eq!(
        engine.transport_frame(),
        pos,
        "swap must preserve the transport position"
    );
    // `replacement` now holds the OLD bank (dealloc happens here, off-RT).

    // The new song's spanning note is chased on resync and sounds.
    let (l, _r, _) = render(&mut engine, 4_800);
    assert!(
        rms(&l) > 1e-4,
        "swapped-in song must sound at the preserved position"
    );

    // A non-synth profile rejects the swap.
    let mut other = MixerEngine::new(false);
    assert!(!other.swap_midi_bank(&mut replacement));
}

/// Offline render + WAV export: non-silent, deterministic, and the WAV
/// round-trips through hound with the expected shape.
#[cfg(feature = "mixer-host")]
#[test]
fn offline_render_and_wav_export() {
    use cosmix_musicd::mixer_host::{export_song_wav, render_song_stereo, song_initial_controls};

    let Some(sf2) = test_sf2() else {
        eprintln!("skipping: set MUSICD_TEST_SF2 to run midi-synth render tests");
        return;
    };
    let sf = synth::load_soundfont(&sf2).expect("load test soundfont");
    let mut song = cosmix_song::Song::new("Export");
    let tid = song.create_track("Piano");
    song.get_track_mut(tid)
        .unwrap()
        .create_note(60, 100, 0, 480);

    let controls = song_initial_controls(&song);
    let (l1, r1) = render_song_stereo(&song, &sf, &controls).expect("render");
    assert!(rms(&l1) > 1e-4, "offline render must be non-silent");
    let (l2, r2) = render_song_stereo(&song, &sf, &controls).expect("render again");
    assert!(l1 == l2 && r1 == r2, "offline render must be deterministic");

    // Per-channel lanes: the one voiced track renders non-silent under its
    // name; unvoiced channels are absent.
    let channels =
        cosmix_musicd::mixer_host::render_song_channels(&song, &sf).expect("render channels");
    assert_eq!(channels.len(), 1);
    assert_eq!(channels[0].0, 0);
    assert_eq!(channels[0].1, "Piano");
    assert!(
        rms(&channels[0].2) > 1e-4,
        "track lane must carry its notes"
    );
    assert_eq!(channels[0].2.len(), l1.len(), "lane length = song length");

    let path = std::env::temp_dir().join(format!("cosmix-song-export-{}.wav", std::process::id()));
    export_song_wav(&song, &sf, &controls, &path).expect("export wav");
    let reader = hound::WavReader::open(&path).expect("reopen wav");
    let spec = reader.spec();
    assert_eq!(spec.channels, 2);
    assert_eq!(spec.sample_rate, 48_000);
    assert_eq!(spec.bits_per_sample, 16);
    assert_eq!(reader.duration() as usize, l1.len());
    std::fs::remove_file(&path).ok();
}

/// The RT-side swap drain: a bank pushed into the song ring is applied by
/// `run_block` and the displaced bank comes back on the return ring.
#[cfg(feature = "mixer-host")]
#[test]
fn rt_state_song_swap_returns_the_old_bank() {
    use cosmix_musicd::mixer_host::{RingSink, RtCommand, RtState, SongBankSwap, song_swap_rings};
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, atomic::AtomicU64};

    let Some(sf2) = test_sf2() else {
        eprintln!("skipping: set MUSICD_TEST_SF2 to run midi-synth render tests");
        return;
    };
    let sf = synth::load_soundfont(&sf2).expect("load test soundfont");

    let (ctrl_tx, ctrl_rx) = rtrb::RingBuffer::<RtCommand>::new(16);
    let (applied_tx, _applied_rx) = rtrb::RingBuffer::new(16);
    let (meter_tx, _meter_rx) = rtrb::RingBuffer::new(16);
    let (mut new_tx, swap, mut old_rx) = song_swap_rings(4);
    let mut rt = RtState::new(
        ctrl_rx,
        applied_tx,
        RingSink(meter_tx),
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicBool::new(false)),
        SourceProfile::MidiSynth(test_bank(&sf2)),
    )
    .with_song_swap(swap);
    drop(ctrl_tx);

    let replacement = Box::new(
        MidiSynthBank::build(Some(&sf), Vec::new(), 0, SongMeta::default()).expect("build empty"),
    );
    new_tx
        .push(SongBankSwap {
            bank: replacement,
            load: false,
        })
        .expect("push new bank");
    rt.run_block(BLOCK, None);
    let old = old_rx.pop().expect("old bank returned off-RT");
    assert_eq!(old.voiced_channels(), 1, "the displaced bank came back");
}
