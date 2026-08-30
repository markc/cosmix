//! Song container — the root of the document model.
//!
//! A song represents a complete musical composition with multiple tracks,
//! tempo settings, and time signature information.

use crate::note::NoteId;
use crate::track::{Track, TrackId};
use crate::{DEFAULT_TEMPO, TICKS_PER_BEAT, ticks_to_seconds};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// Represents a complete song with multiple tracks.
///
/// The song maintains a list of tracks and global settings like tempo.
/// Supports unlimited tracks - memory is the only constraint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Song {
    /// Song name.
    pub name: String,

    /// Tempo in beats per minute.
    pub tempo: u32,

    /// Time signature numerator (beats per measure).
    pub time_sig_numerator: u8,

    /// Time signature denominator (beat unit, as power of 2).
    /// 4 means quarter note, 8 means eighth note, etc.
    pub time_sig_denominator: u8,

    /// Collection of tracks in the song.
    tracks: Vec<Track>,

    /// Next available MIDI channel for auto-assignment.
    /// Skips channel 9 (drums) for melodic tracks.
    next_channel: u8,

    /// Path to the SoundFont file used for playback.
    /// Stored as a string for cross-platform serialization compatibility.
    /// None means no SoundFont is explicitly associated (use default).
    ///
    /// `default` (without `skip_serializing_if`) so old JSON files missing
    /// the field still load, while bincode — which cannot represent a
    /// skipped field — always round-trips correctly.
    #[serde(default)]
    pub soundfont_path: Option<String>,
}

impl Song {
    /// Creates a new empty song with 120 BPM tempo and 4/4 time signature.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            tempo: DEFAULT_TEMPO,
            time_sig_numerator: 4,
            time_sig_denominator: 4,
            tracks: Vec::new(),
            next_channel: 0,
            soundfont_path: None,
        }
    }

    /// Creates a new song with a single default track.
    pub fn with_default_track(name: impl Into<String>) -> Self {
        let mut song = Self::new(name);
        song.add_track(Track::new("Track 1", 0));
        song
    }

    /// Sets the SoundFont path for this song, or None to clear.
    pub fn set_soundfont_path(&mut self, path: Option<impl AsRef<Path>>) {
        self.soundfont_path = path.map(|p| p.as_ref().to_string_lossy().into_owned());
    }

    /// Returns the SoundFont path for this song, if set.
    pub fn get_soundfont_path(&self) -> Option<&str> {
        self.soundfont_path.as_deref()
    }

    /// Returns the number of ticks per measure based on time signature.
    pub fn ticks_per_measure(&self) -> u32 {
        // Calculate based on time signature
        // For 4/4: 4 * 480 = 1920 ticks per measure
        // For 6/8: 6 * 240 = 1440 ticks per measure (eighth note = 240 ticks)
        let beat_ticks = TICKS_PER_BEAT * 4 / self.time_sig_denominator as u32;
        beat_ticks * self.time_sig_numerator as u32
    }

    /// Returns the total duration of the song in ticks.
    /// This is the maximum duration across all tracks.
    pub fn duration_ticks(&self) -> u32 {
        self.tracks
            .iter()
            .map(|t| t.duration_ticks())
            .max()
            .unwrap_or(0)
    }

    /// Returns the total duration of the song in seconds.
    pub fn duration_seconds(&self) -> f64 {
        ticks_to_seconds(self.duration_ticks(), self.tempo)
    }

    /// Adds a track to the song. Returns the TrackId of the added track.
    pub fn add_track(&mut self, track: Track) -> TrackId {
        let id = track.id;
        self.tracks.push(track);
        id
    }

    /// Creates and adds a new track with auto-assigned channel.
    /// Returns the TrackId of the created track.
    pub fn create_track(&mut self, name: impl Into<String>) -> TrackId {
        let channel = self.next_channel;
        // Skip drum channel (9) for melodic tracks
        self.next_channel = if self.next_channel == 8 {
            10
        } else if self.next_channel >= 15 {
            0 // Wrap around (multiple tracks can share channels)
        } else {
            self.next_channel + 1
        };

        let track = Track::new(name, channel);
        self.add_track(track)
    }

    /// Creates and adds a drum track on channel 9.
    /// Returns the TrackId of the created track.
    pub fn create_drum_track(&mut self, name: impl Into<String>) -> TrackId {
        let track = Track::new_drum_track(name);
        self.add_track(track)
    }

    /// Removes a track by its ID. Returns the removed track, or None if not found.
    pub fn remove_track(&mut self, id: TrackId) -> Option<Track> {
        let pos = self.tracks.iter().position(|t| t.id == id)?;
        Some(self.tracks.remove(pos))
    }

    /// Returns a reference to a track by its ID.
    pub fn get_track(&self, id: TrackId) -> Option<&Track> {
        self.tracks.iter().find(|t| t.id == id)
    }

    /// Returns a mutable reference to a track by its ID.
    pub fn get_track_mut(&mut self, id: TrackId) -> Option<&mut Track> {
        self.tracks.iter_mut().find(|t| t.id == id)
    }

    /// Returns a reference to a track by index.
    pub fn track_at(&self, index: usize) -> Option<&Track> {
        self.tracks.get(index)
    }

    /// Returns a mutable reference to a track by index.
    pub fn track_at_mut(&mut self, index: usize) -> Option<&mut Track> {
        self.tracks.get_mut(index)
    }

    /// Returns all tracks in the song.
    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    /// Returns an iterator over mutable track references.
    pub fn tracks_mut(&mut self) -> impl Iterator<Item = &mut Track> {
        self.tracks.iter_mut()
    }

    /// Returns the number of tracks in the song.
    pub fn track_count(&self) -> usize {
        self.tracks.len()
    }

    /// Moves a track to a new position in the track list.
    /// Returns true if the move was successful.
    pub fn move_track(&mut self, from: usize, to: usize) -> bool {
        if from >= self.tracks.len() || to >= self.tracks.len() {
            return false;
        }
        let track = self.tracks.remove(from);
        self.tracks.insert(to, track);
        true
    }

    /// Returns tracks that should be played (considering mute/solo states).
    ///
    /// If any track is soloed, only soloed tracks play.
    /// Otherwise, all non-muted tracks play.
    pub fn playable_tracks(&self) -> impl Iterator<Item = &Track> {
        let any_solo = self.tracks.iter().any(|t| t.solo);
        self.tracks.iter().filter(move |t| {
            if any_solo {
                t.solo && !t.muted
            } else {
                !t.muted
            }
        })
    }

    /// Finds a note by its ID across all tracks.
    /// Returns (TrackId, &Note) if found.
    pub fn find_note(&self, note_id: NoteId) -> Option<(TrackId, &crate::note::Note)> {
        for track in &self.tracks {
            if let Some(note) = track.get_note(note_id) {
                return Some((track.id, note));
            }
        }
        None
    }

    /// Calculates the measure and beat for a given tick position.
    /// Returns (measure, beat, tick_within_beat), measure and beat 1-indexed.
    pub fn tick_to_position(&self, tick: u32) -> (u32, u32, u32) {
        let ticks_per_measure = self.ticks_per_measure();
        let ticks_per_beat = TICKS_PER_BEAT;

        let measure = tick / ticks_per_measure + 1;
        let tick_in_measure = tick % ticks_per_measure;
        let beat = tick_in_measure / ticks_per_beat + 1;
        let tick_in_beat = tick_in_measure % ticks_per_beat;

        (measure, beat, tick_in_beat)
    }

    /// Converts a 1-indexed measure/beat position to ticks.
    pub fn position_to_tick(&self, measure: u32, beat: u32) -> u32 {
        let ticks_per_measure = self.ticks_per_measure();
        (measure - 1) * ticks_per_measure + (beat - 1) * TICKS_PER_BEAT
    }

    /// Advances the process-global NoteId/TrackId counters past every ID in
    /// this song, so IDs allocated after a load can never collide with loaded
    /// ones. Must be called by every deserialization path.
    fn reseed_id_counters(&self) {
        for track in &self.tracks {
            TrackId::bump_counter_past(track.id.as_u64());
            for note in track.notes() {
                NoteId::bump_counter_past(note.id.as_u64());
            }
        }
    }

    /// Serializes the song to a pretty-printed JSON string.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Loads a song from a JSON string.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        let song: Self = serde_json::from_str(json)?;
        song.reseed_id_counters();
        Ok(song)
    }

    /// Saves the song to a JSON file.
    pub fn save_to_file<P: AsRef<Path>>(&self, path: P) -> Result<(), std::io::Error> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        fs::write(path, json)
    }

    /// Loads a song from a JSON file.
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self, std::io::Error> {
        let json = fs::read_to_string(path)?;
        let song: Self = serde_json::from_str(&json)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        song.reseed_id_counters();
        Ok(song)
    }

    /// Saves the song to binary format (.oxm).
    ///
    /// Uses bincode for efficient serialization of numeric data. The serde
    /// shape matches miditui's `.oxm`, so its autosaves load here.
    pub fn save_to_binary<P: AsRef<Path>>(&self, path: P) -> Result<(), std::io::Error> {
        let data = bincode::serialize(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        fs::write(path, data)
    }

    /// Loads a song from binary format (.oxm).
    pub fn load_from_binary<P: AsRef<Path>>(path: P) -> Result<Self, std::io::Error> {
        let data = fs::read(path)?;
        let song: Self = bincode::deserialize(&data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        song.reseed_id_counters();
        Ok(song)
    }

    /// Imports a Standard MIDI File (.mid) as a new song.
    pub fn import_smf<P: AsRef<Path>>(path: P) -> Result<Self, crate::SmfImportError> {
        crate::import_smf(path)
    }

    /// Exports the song to a Standard MIDI File (.mid).
    ///
    /// Creates a Format 1 MIDI file with tempo, time signature, and all
    /// tracks. Mute/solo and the soundfont path ride in `CSMX`
    /// sequencer-specific meta events; note IDs are regenerated on import.
    pub fn export_smf<P: AsRef<Path>>(&self, path: P) -> Result<(), std::io::Error> {
        crate::export_smf(self, path)
    }

    /// Imports a midicomp `.asc` (SMF-as-text) file as a new song.
    pub fn import_asc<P: AsRef<Path>>(path: P) -> Result<Self, crate::SmfImportError> {
        crate::import_asc(path)
    }

    /// Exports the song to a midicomp `.asc` (SMF-as-text) file. Same
    /// fidelity as [`Self::export_smf`] — the text is the byte-exact
    /// midicomp rendering of that SMF.
    pub fn export_asc<P: AsRef<Path>>(&self, path: P) -> Result<(), std::io::Error> {
        crate::export_asc(self, path)
    }
}

impl Default for Song {
    fn default() -> Self {
        Self::with_default_track("Untitled Song")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_song_creation() {
        let song = Song::new("Test");
        assert_eq!(song.name, "Test");
        assert_eq!(song.tempo, 120);
        assert_eq!(song.track_count(), 0);
    }

    #[test]
    fn test_add_tracks() {
        let mut song = Song::new("Test");
        song.create_track("Track 1");
        song.create_track("Track 2");
        assert_eq!(song.track_count(), 2);
    }

    #[test]
    fn test_channel_assignment() {
        let mut song = Song::new("Test");
        for i in 0..16 {
            song.create_track(format!("Track {}", i + 1));
        }
        // Check that channel 9 was skipped (it's for drums)
        let channels: Vec<_> = song.tracks().iter().map(|t| t.channel).collect();
        assert!(!channels[..10].contains(&9)); // First 10 tracks skip channel 9
    }

    #[test]
    fn test_tick_position_conversion() {
        let song = Song::new("Test"); // 4/4 time

        // Tick 0 = Measure 1, Beat 1
        assert_eq!(song.tick_to_position(0), (1, 1, 0));

        // Tick 480 = Measure 1, Beat 2
        assert_eq!(song.tick_to_position(480), (1, 2, 0));

        // Tick 1920 = Measure 2, Beat 1
        assert_eq!(song.tick_to_position(1920), (2, 1, 0));
    }

    #[test]
    fn test_serialization() {
        let mut song = Song::new("Test");
        song.create_track("Piano");
        song.track_at_mut(0).unwrap().create_note(60, 100, 0, 480);

        let json = song.to_json().unwrap();
        let loaded = Song::from_json(&json).unwrap();

        assert_eq!(loaded.name, "Test");
        assert_eq!(loaded.track_count(), 1);
        assert_eq!(loaded.track_at(0).unwrap().note_count(), 1);
    }

    #[test]
    fn test_binary_roundtrip_with_no_soundfont() {
        // soundfont_path = None must survive bincode (the field is always
        // serialized — no skip_serializing_if).
        let mut song = Song::new("Test");
        song.create_track("Piano");
        song.track_at_mut(0).unwrap().create_note(60, 100, 0, 480);
        assert!(song.soundfont_path.is_none());

        let bytes = bincode::serialize(&song).unwrap();
        let loaded: Song = bincode::deserialize(&bytes).unwrap();
        assert_eq!(loaded.name, "Test");
        assert!(loaded.soundfont_path.is_none());
        assert_eq!(loaded.track_at(0).unwrap().note_count(), 1);
    }

    #[test]
    fn test_ids_reseeded_after_load() {
        // IDs allocated after a load must not collide with loaded IDs, even
        // though the counters are process-global and the loaded song may have
        // been written by a longer-lived process.
        let mut song = Song::new("Test");
        song.create_track("Piano");
        let track = song.track_at_mut(0).unwrap();
        for i in 0..8 {
            track.create_note(60, 100, i * 480, 480);
        }
        let json = song.to_json().unwrap();

        let mut loaded = Song::from_json(&json).unwrap();
        let existing: std::collections::HashSet<u64> = loaded
            .tracks()
            .iter()
            .flat_map(|t| t.notes().iter().map(|n| n.id.as_u64()))
            .collect();

        let new_id = loaded
            .track_at_mut(0)
            .unwrap()
            .create_note(62, 100, 4800, 480);
        assert!(!existing.contains(&new_id.as_u64()));

        let new_track_id = loaded.create_track("Second");
        assert_ne!(new_track_id, loaded.track_at(0).unwrap().id);
    }
}
