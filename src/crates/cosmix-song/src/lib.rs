//! song.v1 — the editable song document model.
//!
//! Tracks and notes with tick-based timing, SMF (Standard MIDI File)
//! import/export, JSON + binary persistence, and snapshot undo history.
//! Pure domain crate: no audio, no UI, no cosmix deps.
//!
//! Lifted 2026-07-18 from [miditui](https://github.com/minimaxir/miditui)
//! (MIT, © Max Woolf) — the backend of the terminal DAW — with `Project`
//! renamed `Song`, UI selection state stripped from history snapshots, and
//! ID counters made stable across load. Engine/UI integration design:
//! `~/.cmctl/_doc/2026-07-18-midi-sequencer-design.md`.

mod asc;
pub mod history;
mod note;
mod smf_export;
mod smf_import;
mod song;
mod track;

pub use asc::{export_asc, import_asc};
pub use history::{HistoryManager, StateSnapshot};
pub use note::{Note, NoteId};
pub use smf_export::{export_smf, export_smf_bytes};
pub use smf_import::{SmfImportError, import_smf, import_smf_bytes};
pub use song::Song;
pub use track::{Track, TrackId};

/// Standard MIDI note names for display purposes.
/// Maps MIDI note number (0-127) to note name within an octave.
pub const NOTE_NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

/// Converts a MIDI note number (0-127) to a human-readable note name with
/// octave, e.g. "C4" or "F#5".
///
/// # Examples
///
/// ```
/// let name = cosmix_song::note_to_name(60); // Middle C
/// assert_eq!(name, "C4");
/// ```
pub fn note_to_name(note: u8) -> String {
    let octave = (note / 12) as i8 - 1; // MIDI octave convention
    let note_index = (note % 12) as usize;
    format!("{}{octave}", NOTE_NAMES[note_index])
}

/// Converts a note name like "C4" or "F#5" to a MIDI note number,
/// or None if invalid.
pub fn name_to_note(name: &str) -> Option<u8> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }

    // Find where the octave number starts
    let octave_start = name.chars().position(|c| c.is_ascii_digit() || c == '-')?;

    let note_part = &name[..octave_start];
    let octave_part = &name[octave_start..];

    let note_index = NOTE_NAMES.iter().position(|&n| n == note_part)?;
    let octave: i8 = octave_part.parse().ok()?;

    // MIDI note = (octave + 1) * 12 + note_index
    let midi_note = ((octave + 1) as i16 * 12 + note_index as i16) as u8;
    if midi_note <= 127 {
        Some(midi_note)
    } else {
        None
    }
}

/// Ticks per beat (quarter note) - standard MIDI resolution.
/// Higher values allow finer rhythmic precision.
pub const TICKS_PER_BEAT: u32 = 480;

/// Default tempo in beats per minute.
pub const DEFAULT_TEMPO: u32 = 120;

/// Converts ticks to seconds based on tempo (beats per minute).
pub fn ticks_to_seconds(ticks: u32, tempo: u32) -> f64 {
    let beats = ticks as f64 / TICKS_PER_BEAT as f64;
    beats * 60.0 / tempo as f64
}

/// Converts seconds to ticks based on tempo (beats per minute).
pub fn seconds_to_ticks(seconds: f64, tempo: u32) -> u32 {
    let beats = seconds * tempo as f64 / 60.0;
    (beats * TICKS_PER_BEAT as f64) as u32
}

/// Checks if a beat boundary exists within the tick range [tick, tick + zoom).
///
/// Used to correctly display beat markers even when scroll positions are not
/// aligned to beat boundaries (e.g., during auto-scroll in playback).
#[inline]
pub fn contains_beat(tick: u32, zoom: u32) -> bool {
    let next_beat = if tick.is_multiple_of(TICKS_PER_BEAT) {
        tick
    } else {
        ((tick / TICKS_PER_BEAT) + 1) * TICKS_PER_BEAT
    };
    next_beat < tick + zoom
}

/// Checks if a measure boundary exists within the tick range [tick, tick + zoom).
///
/// Used to correctly display measure markers even when scroll positions are not
/// aligned to measure boundaries (e.g., during auto-scroll in playback).
#[inline]
pub fn contains_measure(tick: u32, zoom: u32) -> bool {
    let measure_ticks = TICKS_PER_BEAT * 4;
    let next_measure = if tick.is_multiple_of(measure_ticks) {
        tick
    } else {
        ((tick / measure_ticks) + 1) * measure_ticks
    };
    next_measure < tick + zoom
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_note_to_name() {
        assert_eq!(note_to_name(60), "C4");
        assert_eq!(note_to_name(69), "A4");
        assert_eq!(note_to_name(0), "C-1");
        assert_eq!(note_to_name(127), "G9");
    }

    #[test]
    fn test_name_to_note() {
        assert_eq!(name_to_note("C4"), Some(60));
        assert_eq!(name_to_note("A4"), Some(69));
        assert_eq!(name_to_note("C-1"), Some(0));
    }

    #[test]
    fn test_tick_conversions() {
        // At 120 BPM, one beat = 0.5 seconds
        let ticks = TICKS_PER_BEAT; // One beat
        let seconds = ticks_to_seconds(ticks, 120);
        assert!((seconds - 0.5).abs() < 0.001);

        let converted_ticks = seconds_to_ticks(0.5, 120);
        assert_eq!(converted_ticks, TICKS_PER_BEAT);
    }
}
