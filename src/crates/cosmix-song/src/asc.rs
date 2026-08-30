//! midicomp `.asc` (SMF-as-text) import/export.
//!
//! `.asc` is the plain-text SMF representation of the `cosmix-midicomp`
//! tool — byte-exact with `.mid` in both directions. Import compiles the
//! text to SMF bytes and runs the normal SMF import; export runs the normal
//! SMF export and decompiles the bytes to text. Fidelity is therefore
//! identical to `.mid`, including the `CSMX` seqspec fields
//! (soundfont path, mute/solo) — see [`crate::smf_export::seqspec`].

use crate::{SmfImportError, Song, import_smf_bytes, smf_export::export_smf_bytes};
use cosmix_midicomp::{Options, decode_smf_to_text, encode_text_to_smf};
use std::path::Path;

/// Imports a midicomp `.asc` text file and creates a Song.
pub fn import_asc<P: AsRef<Path>>(path: P) -> Result<Song, SmfImportError> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path)?;

    let smf = encode_text_to_smf(&text, &Options::default())
        .map_err(|e| SmfImportError::ParseError(format!("asc compile: {e}")))?;

    let song_name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Imported MIDI");

    import_smf_bytes(&smf, song_name)
}

/// Exports a song to a midicomp `.asc` text file.
pub fn export_asc<P: AsRef<Path>>(song: &Song, path: P) -> std::io::Result<()> {
    let smf = export_smf_bytes(song);
    // Our own export bytes are always well-formed; a malformed flag here is
    // an internal bug, not an I/O condition worth a partial file.
    let (text, malformed) =
        decode_smf_to_text(&smf, &Options::default()).map_err(std::io::Error::other)?;
    if malformed {
        return Err(std::io::Error::other(
            "internal error: exported SMF did not decode cleanly",
        ));
    }
    std::fs::write(path, text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Note, Track};

    /// Song fields → SMF bytes → .asc text → SMF bytes → Song, asserting the
    /// CSMX-carried fields and note data survive the full text round-trip.
    #[test]
    fn asc_round_trip_preserves_song_fields() {
        let mut song = Song::new("rt");
        song.tempo = 96;
        song.time_sig_numerator = 3;
        song.time_sig_denominator = 8;
        song.soundfont_path = Some("/tmp/test.sf2".to_string());

        let mut track = Track::new("lead", 0);
        track.program = 42;
        track.volume = 90;
        track.pan = 32;
        track.muted = true;
        track.solo = false;
        track.add_note(Note::new(60, 100, 0, 480));
        track.add_note(Note::new(64, 90, 480, 240));
        song.add_track(track);

        let mut solo_track = Track::new("bass", 1);
        solo_track.solo = true;
        solo_track.add_note(Note::new(36, 110, 0, 960));
        song.add_track(solo_track);

        // Song → SMF → text → SMF → Song (all in memory)
        let smf = export_smf_bytes(&song);
        let (text, malformed) = decode_smf_to_text(&smf, &Options::default()).unwrap();
        assert!(!malformed);
        let smf2 = encode_text_to_smf(&text, &Options::default()).unwrap();
        let back = import_smf_bytes(&smf2, "rt").unwrap();

        assert_eq!(back.tempo, 96);
        assert_eq!(back.time_sig_numerator, 3);
        assert_eq!(back.time_sig_denominator, 8);
        assert_eq!(back.soundfont_path.as_deref(), Some("/tmp/test.sf2"));

        assert_eq!(back.track_count(), 2);
        let lead = back.track_at(0).unwrap();
        assert_eq!(lead.program, 42);
        assert_eq!(lead.volume, 90);
        assert_eq!(lead.pan, 32);
        assert!(lead.muted);
        assert!(!lead.solo);
        assert_eq!(lead.note_count(), 2);
        assert_eq!(lead.notes()[0].pitch, 60);
        assert_eq!(lead.notes()[0].duration_ticks, 480);

        let bass = back.track_at(1).unwrap();
        assert!(bass.solo);
        assert!(!bass.muted);
        assert_eq!(bass.note_count(), 1);
    }

    /// A song with no CSMX-worthy state writes no seqspec events at all.
    #[test]
    fn plain_song_writes_no_seqspec() {
        let mut song = Song::new("plain");
        let mut track = Track::new("t", 0);
        track.add_note(Note::new(60, 100, 0, 480));
        song.add_track(track);

        let smf = export_smf_bytes(&song);
        let (text, _) = decode_smf_to_text(&smf, &Options::default()).unwrap();
        assert!(!text.contains("SeqSpec"), "unexpected seqspec in: {text}");
    }
}
