//! File-level .asc round-trip: real paths through `Song::import_asc` /
//! `Song::export_asc`, including a rich corpus file from the midicomp
//! examples (markers, lyrics, tempo changes) to prove foreign meta events
//! don't break import.

use cosmix_song::Song;
use std::path::PathBuf;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cosmix-song-asc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

#[test]
fn export_then_import_asc_files() {
    let mut song = Song::with_default_track("file-rt");
    song.soundfont_path = Some("/tmp/x.sf2".into());
    let track = song.track_at_mut(0).unwrap();
    track.muted = true;
    track.create_note(60, 100, 0, 480);

    let path = scratch("file-rt.asc");
    song.export_asc(&path).unwrap();

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("MFile"), "not midicomp text: {text}");
    assert!(text.contains("SeqSpec"), "seqspec missing: {text}");

    let back = Song::import_asc(&path).unwrap();
    assert_eq!(back.name, "file-rt"); // named from the file stem
    assert_eq!(back.soundfont_path.as_deref(), Some("/tmp/x.sf2"));
    assert!(back.track_at(0).unwrap().muted);
    assert_eq!(back.track_at(0).unwrap().note_count(), 1);
}

#[test]
fn import_asc_from_midicomp_corpus() {
    // A corpus .asc rich in meta events (markers, lyrics, tempo changes).
    let corpus = manifest_dir().join("../cosmix-midicomp/examples/songs/ambient-drift.asc");
    let song = Song::import_asc(&corpus).unwrap();
    assert!(song.track_count() > 0);
    assert!(song.tracks().iter().any(|t| t.note_count() > 0));
    // First tempo in the file is 60 BPM (1_000_000 usec/beat); the importer
    // keeps the last one seen (1_200_000 → 50 BPM).
    assert_eq!(song.tempo, 50);
}

/// Run-time manifest directory rather than the `env!`-baked one: cargo exports
/// `CARGO_MANIFEST_DIR` into the test process, and that names the tree cargo is
/// actually running in, whereas `env!` records whichever tree last *compiled*
/// the binary. The two diverge when one `CARGO_TARGET_DIR` is shared across
/// several git worktrees of this repo — cargo writes workspace-relative paths
/// into its dep-info, so an artefact built in a sibling worktree is judged
/// fresh and rerun here, still pointing at that tree's fixtures. Falls back to
/// the compile-time value when the binary is run outside cargo.
fn manifest_dir() -> std::path::PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")))
}
