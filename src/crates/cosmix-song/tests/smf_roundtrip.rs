//! End-to-end SMF roundtrip: export a song to a .mid file, re-import it,
//! and check that the musical content survives.

use cosmix_song::Song;

fn temp_path(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("cosmix-song-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

#[test]
fn export_then_import_preserves_notes() {
    let mut song = Song::new("Roundtrip");
    song.tempo = 90;
    song.time_sig_numerator = 3;
    song.time_sig_denominator = 4;

    let tid = song.create_track("Piano");
    {
        let track = song.get_track_mut(tid).unwrap();
        track.program = 5;
        track.volume = 90;
        track.pan = 32;
        track.create_note(60, 100, 0, 480);
        track.create_note(64, 90, 480, 240);
        track.create_note(67, 80, 960, 960);
    }

    let path = temp_path("roundtrip.mid");
    song.export_smf(&path).unwrap();

    let imported = Song::import_smf(&path).unwrap();
    std::fs::remove_file(&path).ok();

    assert_eq!(imported.tempo, 90);
    assert_eq!(imported.time_sig_numerator, 3);
    assert_eq!(imported.time_sig_denominator, 4);
    assert_eq!(imported.track_count(), 1);

    let track = imported.track_at(0).unwrap();
    assert_eq!(track.program, 5);
    assert_eq!(track.volume, 90);
    assert_eq!(track.pan, 32);
    assert_eq!(track.note_count(), 3);

    let notes = track.notes();
    assert_eq!(
        (
            notes[0].pitch,
            notes[0].velocity,
            notes[0].start_tick,
            notes[0].duration_ticks
        ),
        (60, 100, 0, 480)
    );
    assert_eq!(
        (
            notes[1].pitch,
            notes[1].velocity,
            notes[1].start_tick,
            notes[1].duration_ticks
        ),
        (64, 90, 480, 240)
    );
    assert_eq!(
        (
            notes[2].pitch,
            notes[2].velocity,
            notes[2].start_tick,
            notes[2].duration_ticks
        ),
        (67, 80, 960, 960)
    );
}

#[test]
fn multi_track_export_import() {
    let mut song = Song::new("Multi");
    let t1 = song.create_track("Lead"); // channel 0
    let t2 = song.create_track("Bass"); // channel 1
    song.get_track_mut(t1).unwrap().create_note(72, 100, 0, 480);
    song.get_track_mut(t2).unwrap().create_note(36, 110, 0, 960);

    let path = temp_path("multi.mid");
    song.export_smf(&path).unwrap();

    let imported = Song::import_smf(&path).unwrap();
    std::fs::remove_file(&path).ok();

    assert_eq!(imported.track_count(), 2);
    let channels: Vec<u8> = imported.tracks().iter().map(|t| t.channel).collect();
    assert!(channels.contains(&0));
    assert!(channels.contains(&1));
}

#[test]
fn overlapping_same_pitch_notes_survive_roundtrip() {
    // Two overlapping middle-Cs on one channel produce interleaved
    // On/On/Off/Off events in the SMF; a single-slot (channel, pitch)
    // importer map drops one of them (the miditui bug — lost 18 of 491
    // notes on the epic_final_boss.mid example). The FIFO importer keeps
    // both.
    let mut song = Song::new("Overlap");
    let tid = song.create_track("Piano");
    {
        let track = song.get_track_mut(tid).unwrap();
        track.create_note(60, 100, 0, 960); // 0-960
        track.create_note(60, 80, 480, 960); // 480-1440, overlaps the first
        track.create_note(64, 90, 0, 480); // control: non-overlapping
    }

    let path = temp_path("overlap.mid");
    song.export_smf(&path).unwrap();

    let imported = Song::import_smf(&path).unwrap();
    std::fs::remove_file(&path).ok();

    let track = imported.track_at(0).unwrap();
    assert_eq!(track.note_count(), 3);

    // FIFO matching: the earliest NoteOff closes the earliest NoteOn, so
    // both start ticks and both end ticks survive as written.
    let mut c_notes: Vec<(u32, u32)> = track
        .notes()
        .iter()
        .filter(|n| n.pitch == 60)
        .map(|n| (n.start_tick, n.end_tick()))
        .collect();
    c_notes.sort_unstable();
    assert_eq!(c_notes, vec![(0, 960), (480, 1440)]);
}
