//! The roll's note model: a song loaded through `cosmix-song`, flattened into
//! one start-sorted note list with a viewport query both arms share.

use std::fmt;
use std::path::{Path, PathBuf};

use cosmix_song::{Song, TICKS_PER_BEAT};

use crate::layout::{ROLL_KEY_MARGIN, ROLL_MIN_KEY_SPAN};

/// Where the hub's `_bin/gen_dense_song.mix` writes the dense 32-track song,
/// relative to `$HOME`.
pub const DENSE_SONG_RELATIVE: &str = ".cache/cosmix-bench/studio-s0/dense-32-track.mid";

/// The dense song's default path, when `$HOME` is set.
pub fn default_song_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| Path::new(&home).join(DENSE_SONG_RELATIVE))
}

/// One note, in song ticks ([`TICKS_PER_BEAT`] per quarter note).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BenchNote {
    /// Index of the owning track; the colour family.
    pub track: u16,
    pub start: u32,
    pub length: u32,
    pub pitch: u8,
    pub velocity: u8,
}

impl BenchNote {
    pub fn end(&self) -> u32 {
        self.start.saturating_add(self.length)
    }
}

#[derive(Debug)]
pub enum LoadError {
    Io(PathBuf, std::io::Error),
    Parse(PathBuf, String),
    UnknownFormat(PathBuf),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::Io(path, error) => write!(f, "{}: {error}", path.display()),
            LoadError::Parse(path, error) => write!(f, "{}: {error}", path.display()),
            LoadError::UnknownFormat(path) => write!(
                f,
                "{}: unknown song format (expected .mid, .midi, .asc or .json)",
                path.display()
            ),
        }
    }
}

impl std::error::Error for LoadError {}

/// A flattened song.
#[derive(Clone, Debug, PartialEq)]
pub struct BenchSong {
    pub name: String,
    /// Sorted by `(start, track, pitch)`.
    pub notes: Vec<BenchNote>,
    pub track_count: usize,
    /// Timeline length: the last note end rounded up to a whole measure.
    pub length_ticks: u32,
    pub ticks_per_beat: u32,
    pub beats_per_measure: u32,
    pub tempo_bpm: u32,
    /// Lowest and highest key the roll shows (the song's range, padded).
    pub key_lo: u8,
    pub key_hi: u8,
    max_note_length: u32,
}

impl BenchSong {
    /// Load `.mid`/`.midi`, `.asc` or song `.json`.
    pub fn load(path: &Path) -> Result<Self, LoadError> {
        let extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_ascii_lowercase);
        let song = match extension.as_deref() {
            Some("mid" | "midi") => {
                Song::import_smf(path).map_err(|e| LoadError::Parse(path.into(), e.to_string()))?
            }
            Some("asc") => {
                Song::import_asc(path).map_err(|e| LoadError::Parse(path.into(), e.to_string()))?
            }
            Some("json") => Song::load_from_file(path).map_err(|e| {
                if e.kind() == std::io::ErrorKind::InvalidData {
                    LoadError::Parse(path.into(), e.to_string())
                } else {
                    LoadError::Io(path.into(), e)
                }
            })?,
            _ => return Err(LoadError::UnknownFormat(path.into())),
        };
        Ok(Self::from_song(&song))
    }

    /// Parse Standard MIDI File bytes.
    pub fn from_smf_bytes(bytes: &[u8], name: &str) -> Result<Self, String> {
        cosmix_song::import_smf_bytes(bytes, name)
            .map(|song| Self::from_song(&song))
            .map_err(|e| e.to_string())
    }

    pub fn from_song(song: &Song) -> Self {
        let mut notes: Vec<BenchNote> = song
            .tracks()
            .iter()
            .enumerate()
            .flat_map(|(track, t)| {
                t.notes().iter().map(move |note| BenchNote {
                    track: u16::try_from(track).unwrap_or(u16::MAX),
                    start: note.start_tick,
                    length: note.duration_ticks,
                    pitch: note.pitch,
                    velocity: note.velocity,
                })
            })
            .collect();
        notes.sort_by_key(|n| (n.start, n.track, n.pitch));

        let ticks_per_beat = TICKS_PER_BEAT * 4 / u32::from(song.time_sig_denominator.max(1));
        let beats_per_measure = u32::from(song.time_sig_numerator.max(1));
        let measure = ticks_per_beat * beats_per_measure;
        let last_end = notes.iter().map(BenchNote::end).max().unwrap_or(0);
        let length_ticks = last_end.div_ceil(measure).max(1) * measure;
        let max_note_length = notes.iter().map(|n| n.length).max().unwrap_or(0);

        let (lo, hi) = notes
            .iter()
            .fold((127u8, 0u8), |(lo, hi), n| (lo.min(n.pitch), hi.max(n.pitch)));
        let (key_lo, key_hi) = pad_key_range(if lo > hi { (48, 72) } else { (lo, hi) });

        Self {
            name: song.name.clone(),
            notes,
            track_count: song.track_count(),
            length_ticks,
            ticks_per_beat,
            beats_per_measure,
            tempo_bpm: song.tempo,
            key_lo,
            key_hi,
            max_note_length,
        }
    }

    pub fn ticks_per_measure(&self) -> u32 {
        self.ticks_per_beat * self.beats_per_measure
    }

    /// Key rows the roll shows.
    pub fn key_rows(&self) -> u32 {
        u32::from(self.key_hi - self.key_lo) + 1
    }

    /// Indices (into [`Self::notes`]) of notes overlapping `[start, end)` whose
    /// pitch is in `[key_lo, key_hi]`, in list order, at most `cap` of them.
    /// Returns how many further matches were left out.
    pub fn visible_into(
        &self,
        start: f64,
        end: f64,
        key_lo: u8,
        key_hi: u8,
        cap: usize,
        out: &mut Vec<u32>,
    ) -> usize {
        out.clear();
        // Nothing starting before this can reach `start`.
        let earliest = start - f64::from(self.max_note_length);
        let first = self
            .notes
            .partition_point(|note| f64::from(note.start) < earliest);
        let mut dropped = 0;
        for (index, note) in self.notes[first..].iter().enumerate() {
            if f64::from(note.start) >= end {
                break;
            }
            if f64::from(note.end()) <= start || note.pitch < key_lo || note.pitch > key_hi {
                continue;
            }
            if out.len() < cap {
                out.push((first + index) as u32);
            } else {
                dropped += 1;
            }
        }
        dropped
    }
}

/// Pad a key range by the roll margin and widen it to the minimum span.
fn pad_key_range((lo, hi): (u8, u8)) -> (u8, u8) {
    let mut lo = lo.saturating_sub(ROLL_KEY_MARGIN);
    let mut hi = hi.saturating_add(ROLL_KEY_MARGIN).min(127);
    while hi - lo < ROLL_MIN_KEY_SPAN && !(lo == 0 && hi == 127) {
        lo = lo.saturating_sub(1);
        hi = (hi + 1).min(127);
    }
    (lo, hi)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use cosmix_song::{Note, Track};

    /// A small generated song: `tracks` tracks of `bars` 4/4 bars, one note
    /// per eighth, pitches walking upward per track.
    pub(crate) fn small_song(tracks: usize, bars: u32) -> Song {
        let mut song = Song::new("bench-small");
        for track in 0..tracks {
            let mut t = Track::new(format!("T{track}"), track as u8);
            for step in 0..bars * 8 {
                let pitch = 40 + ((track as u32 * 5 + step) % 30) as u8;
                t.add_note(Note::new(pitch, 90, step * 240, 200 + track as u32 * 10));
            }
            song.add_track(t);
        }
        song
    }

    #[test]
    fn flattens_and_sorts_a_generated_song() {
        let bench = BenchSong::from_song(&small_song(3, 4));
        assert_eq!(bench.notes.len(), 3 * 4 * 8);
        assert_eq!(bench.track_count, 3);
        assert!(bench.notes.windows(2).all(|w| w[0].start <= w[1].start));
        assert_eq!(bench.ticks_per_beat, 480);
        assert_eq!(bench.beats_per_measure, 4);
        assert_eq!(bench.length_ticks, 4 * 1920);
        assert_eq!(bench.key_lo, 38);
        assert_eq!(bench.key_hi, 71);
        assert_eq!(bench.key_rows(), 34);
    }

    #[test]
    fn smf_bytes_round_trip_through_cosmix_song() {
        let song = small_song(4, 2);
        let direct = BenchSong::from_song(&song);
        let bytes = cosmix_song::export_smf_bytes(&song);
        let loaded = BenchSong::from_smf_bytes(&bytes, "bench-small").unwrap();
        let strip = |s: &BenchSong| {
            s.notes
                .iter()
                .map(|n| (n.start, n.length, n.pitch, n.velocity))
                .collect::<Vec<_>>()
        };
        assert_eq!(strip(&loaded), strip(&direct));
        assert_eq!(loaded.length_ticks, direct.length_ticks);
        assert!(BenchSong::from_smf_bytes(b"not midi", "x").is_err());
    }

    #[test]
    fn load_dispatches_on_extension() {
        let dir = std::env::temp_dir().join(format!("cosmix-bench-feed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let song = small_song(2, 1);
        let mid = dir.join("small.mid");
        song.export_smf(&mid).unwrap();
        let json = dir.join("small.json");
        song.save_to_file(&json).unwrap();
        assert_eq!(BenchSong::load(&mid).unwrap().notes.len(), 16);
        assert_eq!(BenchSong::load(&json).unwrap().notes.len(), 16);
        assert!(matches!(
            BenchSong::load(&dir.join("small.wav")),
            Err(LoadError::UnknownFormat(_))
        ));
        assert!(BenchSong::load(&dir.join("missing.mid")).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn empty_song_still_has_a_timeline_and_keys() {
        let bench = BenchSong::from_song(&Song::new("empty"));
        assert!(bench.notes.is_empty());
        assert_eq!(bench.length_ticks, 1920);
        assert!(bench.key_hi - bench.key_lo >= ROLL_MIN_KEY_SPAN);
    }

    #[test]
    fn visible_query_matches_brute_force() {
        let bench = BenchSong::from_song(&small_song(5, 8));
        let mut out = Vec::new();
        for (start, end, lo, hi) in [
            (0.0, 1920.0, 0, 127),
            (1000.0, 1100.0, 0, 127),
            (1919.5, 4000.0, 45, 60),
            (-500.0, 100.0, 0, 127),
            (15_000.0, 20_000.0, 0, 127),
        ] {
            let dropped = bench.visible_into(start, end, lo, hi, usize::MAX, &mut out);
            assert_eq!(dropped, 0);
            let expected: Vec<u32> = bench
                .notes
                .iter()
                .enumerate()
                .filter(|(_, n)| {
                    f64::from(n.start) < end
                        && f64::from(n.end()) > start
                        && (lo..=hi).contains(&n.pitch)
                })
                .map(|(i, _)| i as u32)
                .collect();
            assert_eq!(out, expected, "window {start}..{end} keys {lo}..={hi}");
        }
    }

    #[test]
    fn visible_query_caps_and_counts_the_rest() {
        let bench = BenchSong::from_song(&small_song(5, 8));
        let mut out = Vec::new();
        let total = bench.visible_into(0.0, 1e9, 0, 127, usize::MAX, &mut out);
        assert_eq!((total, out.len()), (0, bench.notes.len()));
        let dropped = bench.visible_into(0.0, 1e9, 0, 127, 10, &mut out);
        assert_eq!(out, (0..10).collect::<Vec<u32>>());
        assert_eq!(dropped, bench.notes.len() - 10);
    }

    #[test]
    fn key_padding_respects_the_midi_range() {
        assert_eq!(pad_key_range((60, 60)), (48, 72));
        assert_eq!(pad_key_range((0, 3)), (0, 24));
        assert_eq!(pad_key_range((126, 127)), (103, 127));
        assert_eq!(pad_key_range((0, 127)), (0, 127));
    }
}
