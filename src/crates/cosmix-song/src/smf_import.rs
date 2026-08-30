//! Standard MIDI File (SMF) import.
//!
//! Imports .mid and .midi files into the song document model.
//! Supports SMF Format 0 (single track) and Format 1 (multi-track) files.
//!
//! # Limitations
//!
//! - Only note on/off events are imported as notes
//! - Tempo and time signature are read from the first track (or global events)
//! - Program changes set the track instrument
//! - Volume (CC7) and Pan (CC10) are imported
//! - `CSMX` sequencer-specific events restore soundfont path and mute/solo
//!   (see [`crate::smf_export::seqspec`])
//! - Other MIDI events (pitch bend, aftertouch, etc.) are ignored

use crate::{Note, Song, TICKS_PER_BEAT, Track};
use midly::{Format, Smf, Timing, TrackEventKind};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// Errors that can occur during SMF import.
#[derive(Debug)]
pub enum SmfImportError {
    /// File could not be read
    IoError(std::io::Error),
    /// MIDI parsing failed
    ParseError(String),
    /// Unsupported MIDI format or timing
    UnsupportedFormat(String),
}

impl std::fmt::Display for SmfImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SmfImportError::IoError(e) => write!(f, "IO error: {e}"),
            SmfImportError::ParseError(e) => write!(f, "MIDI parse error: {e}"),
            SmfImportError::UnsupportedFormat(e) => write!(f, "Unsupported format: {e}"),
        }
    }
}

impl std::error::Error for SmfImportError {}

impl From<std::io::Error> for SmfImportError {
    fn from(e: std::io::Error) -> Self {
        SmfImportError::IoError(e)
    }
}

/// State for tracking active notes during import.
/// Key is (channel, pitch); the value is a FIFO of (start_tick, velocity) so
/// overlapping same-pitch notes don't clobber each other — a NoteOff closes
/// the earliest still-open note. (miditui's original single-slot map silently
/// dropped a note per overlap.)
type ActiveNotes = HashMap<(u8, u8), Vec<(u32, u8)>>;

/// Everything harvested from one SMF track: the Song tracks it splits into
/// (one per MIDI channel) plus any global fields its meta events carried.
#[derive(Default)]
struct ParsedTrack {
    tracks: Vec<Track>,
    tempo: Option<u32>,
    time_sig: Option<(u8, u8)>,
    /// From a global `CSMX` seqspec event (normally in the tempo track).
    soundfont_path: Option<String>,
}

/// Imports a Standard MIDI File and creates a Song.
pub fn import_smf<P: AsRef<Path>>(path: P) -> Result<Song, SmfImportError> {
    let path = path.as_ref();
    let data = fs::read(path)?;

    // Use filename as the song name
    let song_name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Imported MIDI");

    import_smf_bytes(&data, song_name)
}

/// Imports Standard-MIDI-File bytes and creates a Song named `song_name`.
pub fn import_smf_bytes(data: &[u8], song_name: &str) -> Result<Song, SmfImportError> {
    let smf = Smf::parse(data).map_err(|e| SmfImportError::ParseError(e.to_string()))?;

    // Get ticks per beat from header
    let source_ticks_per_beat = match smf.header.timing {
        Timing::Metrical(tpb) => tpb.as_int() as u32,
        Timing::Timecode(_, _) => {
            return Err(SmfImportError::UnsupportedFormat(
                "SMPTE timecode timing not supported".to_string(),
            ));
        }
    };

    let mut song = Song::new(song_name);

    // Default tempo and time signature (will be overwritten if found in MIDI)
    let mut tempo: u32 = 120;
    let mut time_sig_num: u8 = 4;
    let mut time_sig_denom: u8 = 4;
    let mut soundfont_path: Option<String> = None;

    // Process tracks based on format
    match smf.header.format {
        Format::SingleTrack | Format::Parallel => {
            // Format 0: Single track with all channels
            // Format 1: First track is usually tempo/meta, rest are music
            let is_format_1 = smf.header.format == Format::Parallel;

            for (track_idx, track) in smf.tracks.iter().enumerate() {
                // For Format 1, first track is typically tempo/meta only
                let is_tempo_track = is_format_1 && track_idx == 0;

                // Parse the track
                let parsed = parse_track(track, track_idx, source_ticks_per_beat, is_tempo_track)?;

                // Update global fields from tempo track or first occurrence
                if let Some(t) = parsed.tempo {
                    tempo = t;
                }
                if let Some((num, denom)) = parsed.time_sig {
                    time_sig_num = num;
                    time_sig_denom = denom;
                }
                if let Some(sf) = parsed.soundfont_path {
                    soundfont_path = Some(sf);
                }

                if !is_tempo_track || !parsed.tracks.is_empty() {
                    for imported_track in parsed.tracks {
                        song.add_track(imported_track);
                    }
                }
            }
        }
        Format::Sequential => {
            return Err(SmfImportError::UnsupportedFormat(
                "Format 2 (sequential) MIDI files not supported".to_string(),
            ));
        }
    }

    song.tempo = tempo;
    song.time_sig_numerator = time_sig_num;
    song.time_sig_denominator = time_sig_denom;
    song.soundfont_path = soundfont_path;

    // If no tracks were created, add an empty default track
    if song.track_count() == 0 {
        song.add_track(Track::new("Track 1", 0));
    }

    Ok(song)
}

/// Closes the earliest still-open note for (ch, pitch) at `end_tick`, adding
/// it to the channel's track. No-op if nothing is open for that key.
fn close_note(
    active_notes: &mut ActiveNotes,
    channel_tracks: &mut HashMap<u8, Track>,
    ch: u8,
    pitch: u8,
    end_tick: u32,
) {
    if let Some(open) = active_notes.get_mut(&(ch, pitch)) {
        if open.is_empty() {
            return;
        }
        let (start_tick, velocity) = open.remove(0);
        let duration = end_tick.saturating_sub(start_tick).max(1);
        if let Some(track) = channel_tracks.get_mut(&ch) {
            track.add_note(Note::new(pitch, velocity, start_tick, duration));
        }
    }
}

/// Parses a single MIDI track and returns track data plus any global info.
fn parse_track(
    track: &[midly::TrackEvent],
    track_idx: usize,
    source_ticks_per_beat: u32,
    is_tempo_track: bool,
) -> Result<ParsedTrack, SmfImportError> {
    use crate::smf_export::seqspec;

    // Track state per channel
    let mut channel_tracks: HashMap<u8, Track> = HashMap::new();
    let mut active_notes: ActiveNotes = HashMap::new();
    let mut tempo: Option<u32> = None;
    let mut time_sig: Option<(u8, u8)> = None;
    let mut track_name: Option<String> = None;
    let mut soundfont_path: Option<String> = None;
    // From per-track CSMX seqspec events, keyed by MIDI channel (last one
    // wins per channel). The channel key keeps the flags attached to the
    // right track even when a foreign tool merged our tracks into one.
    let mut track_flags: HashMap<u8, (bool, bool)> = HashMap::new();

    // Current absolute tick position
    let mut current_tick: u32 = 0;

    for event in track {
        // Advance tick by delta time, scaling to our internal resolution
        let delta_scaled = scale_ticks(event.delta.as_int(), source_ticks_per_beat);
        current_tick += delta_scaled;

        match event.kind {
            TrackEventKind::Meta(meta) => {
                match meta {
                    midly::MetaMessage::TrackName(name_bytes) => {
                        if let Ok(name) = std::str::from_utf8(name_bytes) {
                            track_name = Some(name.to_string());
                        }
                    }
                    midly::MetaMessage::Tempo(tempo_val) => {
                        // tempo_val is microseconds per beat. The BPM filter
                        // also rejects an absurd usec value that would floor to
                        // 0 BPM (a zero tempo would poison later tick math).
                        let usec_per_beat = tempo_val.as_int();
                        if let Some(bpm) = 60_000_000u32
                            .checked_div(usec_per_beat)
                            .filter(|bpm| *bpm > 0)
                        {
                            tempo = Some(bpm);
                        }
                    }
                    midly::MetaMessage::TimeSignature(num, denom_power, _, _) => {
                        // denom_power is power of 2 (e.g., 2 means quarter note)
                        let denom = 1u8 << denom_power;
                        time_sig = Some((num, denom));
                    }
                    midly::MetaMessage::SequencerSpecific(data) => {
                        // CSMX events carry Song fields with no SMF home;
                        // anything unrecognised is silently skipped.
                        match seqspec::parse(data) {
                            Some((seqspec::KIND_GLOBAL, body)) => {
                                if let Ok(global) = serde_json::from_slice::<seqspec::Global>(body)
                                {
                                    soundfont_path = global.soundfont_path;
                                }
                            }
                            Some((seqspec::KIND_TRACK, body)) => {
                                if let Ok(flags) =
                                    serde_json::from_slice::<seqspec::TrackFlags>(body)
                                {
                                    track_flags.insert(flags.channel, (flags.muted, flags.solo));
                                }
                            }
                            _ => {}
                        }
                    }
                    _ => {} // Ignore other meta events
                }
            }
            TrackEventKind::Midi { channel, message } => {
                let ch = channel.as_int();

                // Ensure we have a track for this channel, using entry API
                channel_tracks.entry(ch).or_insert_with(|| {
                    let name = track_name
                        .clone()
                        .unwrap_or_else(|| format!("Track {}", track_idx + 1));
                    let mut new_track = Track::new(&name, ch);
                    new_track.channel = ch;
                    new_track
                });

                match message {
                    midly::MidiMessage::NoteOn { key, vel } => {
                        let pitch = key.as_int();
                        let velocity = vel.as_int();

                        if velocity > 0 {
                            // Note on - record start
                            active_notes
                                .entry((ch, pitch))
                                .or_default()
                                .push((current_tick, velocity));
                        } else {
                            // Note on with velocity 0 = note off
                            close_note(
                                &mut active_notes,
                                &mut channel_tracks,
                                ch,
                                pitch,
                                current_tick,
                            );
                        }
                    }
                    midly::MidiMessage::NoteOff { key, vel: _ } => {
                        let pitch = key.as_int();
                        close_note(
                            &mut active_notes,
                            &mut channel_tracks,
                            ch,
                            pitch,
                            current_tick,
                        );
                    }
                    midly::MidiMessage::ProgramChange { program } => {
                        if let Some(track) = channel_tracks.get_mut(&ch) {
                            track.program = program.as_int();
                        }
                    }
                    midly::MidiMessage::Controller { controller, value } => {
                        let cc = controller.as_int();
                        let val = value.as_int();

                        if let Some(track) = channel_tracks.get_mut(&ch) {
                            match cc {
                                7 => track.volume = val, // Volume
                                10 => track.pan = val,   // Pan
                                _ => {}                  // Ignore other CCs
                            }
                        }
                    }
                    _ => {} // Ignore other MIDI messages
                }
            }
            _ => {} // Ignore SysEx and other events
        }
    }

    // Close any remaining active notes (in case MIDI file is incomplete)
    for ((ch, pitch), open) in active_notes {
        for (start_tick, velocity) in open {
            if let Some(track) = channel_tracks.get_mut(&ch) {
                // Use a default duration of 1 beat for unclosed notes
                let duration = TICKS_PER_BEAT;
                track.add_note(Note::new(pitch, velocity, start_tick, duration));
            }
        }
    }

    // Convert HashMap to Vec, sorted by channel
    let mut tracks: Vec<Track> = channel_tracks.into_values().collect();
    tracks.sort_by_key(|t| t.channel);

    // Apply per-track CSMX flags to the matching channel-split track
    for track in &mut tracks {
        if let Some(&(muted, solo)) = track_flags.get(&track.channel) {
            track.muted = muted;
            track.solo = solo;
        }
    }

    // Drop note-less tracks. A MIDI track with no notes is never playable in
    // the mixer/arranger; in practice it is a label/description track — an SMF
    // `TrackName` plus a stray channel event (some tracker exports emit the
    // song title / copyright / comment lines this way, each as its own track
    // carrying only a centred pitch-bend). Keeping them spawned one empty lane
    // per line of copyright text. Subsumes the old tempo-track special case.
    let _ = is_tempo_track;
    tracks.retain(|t| !t.notes().is_empty());

    Ok(ParsedTrack {
        tracks,
        tempo,
        time_sig,
        soundfont_path,
    })
}

/// Scales ticks from source resolution to our internal resolution (TICKS_PER_BEAT).
fn scale_ticks(source_ticks: u32, source_tpb: u32) -> u32 {
    if source_tpb == TICKS_PER_BEAT {
        source_ticks
    } else {
        // Scale: (source_ticks * TICKS_PER_BEAT) / source_tpb
        // Use u64 to avoid overflow
        ((source_ticks as u64 * TICKS_PER_BEAT as u64) / source_tpb as u64) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scale_ticks() {
        // Same resolution
        assert_eq!(scale_ticks(480, 480), 480);

        // Double resolution source
        assert_eq!(scale_ticks(960, 960), 480);

        // Half resolution source
        assert_eq!(scale_ticks(240, 240), 480);

        // Different resolution
        assert_eq!(scale_ticks(120, 120), 480);
    }

    /// Assemble a Format-1 SMF from raw per-track event byte slices.
    fn smf_format1(division: u16, tracks: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"MThd");
        out.extend_from_slice(&6u32.to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes()); // format 1
        out.extend_from_slice(&(tracks.len() as u16).to_be_bytes());
        out.extend_from_slice(&division.to_be_bytes());
        for data in tracks {
            out.extend_from_slice(b"MTrk");
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            out.extend_from_slice(data);
        }
        out
    }

    #[test]
    fn note_less_label_tracks_are_dropped() {
        // Reproduces Walthius-style tracker exports (e.g. island.mid): each
        // song-description line is its own SMF track carrying a `TrackName`
        // meta plus a centred pitch-bend on channel 0 — and no notes.
        let tempo = [
            0x00, 0xFF, 0x51, 0x03, 0x07, 0xA1, 0x20, 0x00, 0xFF, 0x2F, 0x00,
        ];
        let note = [
            0x00, 0x90, 0x3C, 0x64, // NoteOn  ch0 pitch60 vel100
            0x60, 0x80, 0x3C, 0x00, // NoteOff ch0 pitch60 (delta 96)
            0x00, 0xFF, 0x2F, 0x00, // EndOfTrack
        ];
        let label = [
            0x00, 0xFF, 0x03, 0x05, b'L', b'A', b'B', b'E', b'L', // TrackName "LABEL"
            0x00, 0xE0, 0x00, 0x40, // Pitch bend ch0, centred
            0x00, 0xFF, 0x2F, 0x00, // EndOfTrack
        ];
        let bytes = smf_format1(96, &[&tempo, &note, &label]);

        let song = import_smf_bytes(&bytes, "test").expect("import");
        assert_eq!(
            song.track_count(),
            1,
            "only the note-bearing track survives"
        );
        assert!(!song.tracks()[0].notes().is_empty());
        assert!(
            !song.tracks().iter().any(|t| t.name == "LABEL"),
            "note-less label track must be dropped"
        );
    }
}
