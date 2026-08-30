# cosmix-song

`cosmix-song` is the `song.v1` editable song document model: MIDI tracks and
notes with tick-based timing, Standard MIDI File import and export, JSON and
binary persistence, midicomp text conversion, and snapshot undo history. It is
a library in the `cos` layer of the `bus <- mix <- cos` dependency chain. It
does not depend directly on `bus` or `mix`; its only local crate dependency is
`cosmix-midicomp`, used for `.asc` conversion.

## Synopsis

```rust
use cosmix_song::{Song, TICKS_PER_BEAT};

let mut song = Song::new("Example");
let track_id = song.create_track("Piano");
let track = song.get_track_mut(track_id).unwrap();

track.create_note(60, 100, 0, TICKS_PER_BEAT);
track.create_note(64, 90, TICKS_PER_BEAT, TICKS_PER_BEAT / 2);

assert_eq!(song.track_count(), 1);
assert_eq!(song.duration_ticks(), TICKS_PER_BEAT + TICKS_PER_BEAT / 2);
```

The crate is a domain library. It provides no audio engine, user interface,
daemon, command-line interface, configuration format, or Bus verbs.

## Document model

### `Song`

`Song` is the root document type. It stores:

- A name and tempo in beats per minute.
- A time-signature numerator and denominator.
- An ordered collection of tracks.
- An optional SoundFont path associated with playback.

`Song::new` creates an empty song at 120 BPM in 4/4 time.
`Song::with_default_track` also creates one melodic track.
`Song::default` names that document `Untitled Song`.

Track operations include:

- `add_track`, `create_track`, and `create_drum_track`.
- `remove_track`, `move_track`, and indexed or ID-based lookup.
- `tracks`, `tracks_mut`, `track_count`, and `find_note`.
- `playable_tracks`, which applies mute and solo state.

Melodic tracks receive MIDI channels automatically. Channel 9 is reserved for
drums. Automatic allocation wraps after channel 15, so tracks may share a
channel.

Timing operations report song duration, calculate ticks per measure, and
convert between ticks and one-indexed measure and beat positions.

### `Track`

`Track` represents one MIDI track. Its public fields hold the stable `TrackId`,
name, MIDI channel, program, volume, pan, mute state, and solo state. Its notes
remain sorted by start tick when inserted through `add_note` or `create_note`.

`Track::new` clamps the channel to 0 through 15 and defaults to program 0,
volume 100, centred pan 64, and no mute or solo. `Track::new_drum_track` selects
General MIDI drum channel 9.

Track editing and queries include:

- Add, create, remove, and look up notes by `NoteId`.
- Borrow all notes or scan notes overlapping a half-open tick range.
- Find notes active at one tick.
- Report note count and duration, or clear all notes.
- Quantise note starts to the nearest non-zero tick grid.
- Transpose all notes and report how many could not remain in MIDI range.

### `Note`

`Note` stores a stable `NoteId`, pitch, velocity, start tick, and duration.
`Note::new` clamps pitch and velocity to the MIDI range 0 through 127.

The note API calculates a saturating end tick, tests half-open range overlap
and activity, duplicates a note with a new ID, transposes within MIDI range,
and shifts its start time with saturation at zero.

`NoteId` and `TrackId` are process-global monotonic identifiers. JSON and binary
load paths advance their counters beyond every loaded identifier before new
objects are allocated.

## Timing and note helpers

The internal resolution is `TICKS_PER_BEAT`, fixed at 480 ticks per quarter
note. `DEFAULT_TEMPO` is 120 BPM.

The crate exports:

- `ticks_to_seconds` and `seconds_to_ticks` for constant-tempo conversion.
- `NOTE_NAMES`, `note_to_name`, and `name_to_note` for MIDI note names.
- `contains_beat` and `contains_measure` for detecting boundaries in half-open
  tick windows.

Note naming follows the MIDI octave convention: MIDI note 60 is `C4`, and note
0 is `C-1`. Parsing accepts the sharp names present in `NOTE_NAMES`; it does not
normalise flats or letter case.

## Persistence

`Song` derives Serde serialization and supports two native persistence forms.

| Form | Write | Read |
|---|---|---|
| Pretty JSON string | `Song::to_json` | `Song::from_json` |
| Pretty JSON file | `Song::save_to_file` | `Song::load_from_file` |
| Bincode `.oxm` file | `Song::save_to_binary` | `Song::load_from_binary` |

The binary Serde field sequence is compatible with the `.oxm` shape inherited
from miditui. The optional SoundFont field defaults when absent from older JSON
documents.

## Standard MIDI Files

SMF operations are available as free functions and, for file paths, as `Song`
methods:

| Operation | File API | Byte API |
|---|---|---|
| Import | `import_smf`, `Song::import_smf` | `import_smf_bytes` |
| Export | `export_smf`, `Song::export_smf` | `export_smf_bytes` |

Import accepts SMF Format 0 and Format 1 with metrical timing. It rescales
source ticks to 480 ticks per beat and splits MIDI data into song tracks by
channel. It imports note on and note off events, track names, program changes,
tempo, time signature, volume controller 7, and pan controller 10.

The importer ignores other MIDI events and drops tracks that contain no notes.
An unclosed note receives a one-beat duration. Overlapping notes with the same
channel and pitch are matched in first-in, first-out order. If import produces
no note-bearing tracks, the resulting song contains one empty default track.

SMF Format 2 and SMPTE timecode timing return
`SmfImportError::UnsupportedFormat`. File I/O and parse failures use the other
`SmfImportError` variants.

Export always writes SMF Format 1:

- Track 0 contains the song name, tempo, and time signature.
- Each song track becomes one MIDI track with its name, program, volume, pan,
  notes, and note-off events.
- The file resolution is 480 ticks per quarter note.

The exporter stores `soundfont_path`, mute, and solo state in versioned `CSMX`
sequencer-specific meta events because standard SMF has no native fields for
them. The importer restores recognised `CSMX` data and ignores unknown versions
or kinds. Note IDs are regenerated after an SMF round trip.

## Midicomp text

The `.asc` form is the text representation provided by `cosmix-midicomp`.
`import_asc` and `Song::import_asc` compile text to SMF bytes before normal
import. `export_asc` and `Song::export_asc` render normal SMF export bytes as
text.

ASC therefore has the same model fidelity and limitations as SMF, including
the `CSMX` fields. File-based import names the song from the file stem.

## Undo and redo

The public `history` module exports `HistoryManager` and `StateSnapshot`.
A snapshot clones the complete `Song` and carries a short operation
description. It deliberately excludes user-interface selection state.

`HistoryManager` maintains bounded undo and redo stacks of eight snapshots
each. `push_undo` records the pre-operation state and clears redo history.
Callers performing redo use `push_undo_preserve_redo` so remaining redo states
survive. The manager exposes pop, availability, count, and clear operations;
the caller remains responsible for applying the returned `Song`.

## Cargo features

The crate defines no Cargo features.

## Dependencies

`serde` and `serde_json` provide the document and metadata encodings. `bincode`
provides `.oxm` persistence. `midly` provides tolerant SMF parsing. The local
`cosmix-midicomp` library provides `.asc` encoding and decoding with its default
features disabled.
