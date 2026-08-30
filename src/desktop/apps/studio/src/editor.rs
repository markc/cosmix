//! The song-editing loop: piano-roll clicks → document edits → engine bank
//! swaps, with snapshot undo and live note preview.
//!
//! Ownership: this module owns the authoritative [`cosmix_song::Song`] and its
//! [`HistoryManager`]. Every edit (1) snapshots for undo, (2) mutates the
//! document, (3) rebuilds the render model the ctk piano-roll draws, and (4)
//! rebuilds the synth bank off the audio thread and ships it over the swap
//! ring — the engine keeps playing through the swap, transport preserved.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use bevy::ecs::observer::On;
use bevy::input::keyboard::KeyCode;
use bevy::prelude::{App, IntoScheduleConfigs, Plugin, Res, ResMut, Resource, Update};
use cosmix_mixer_schema::NUM_CHANNELS;
use cosmix_musicd::mixer_host::{song_bank_with, RtCommand, SongBankSwap};
use cosmix_song::{HistoryManager, Note, StateSnapshot, TICKS_PER_BEAT};
use ctk::prelude::{PianoRollClick, PianoRollModel, PianoRollNote};

use crate::action::{BoardInputSystems, ConsumedShortcutInputs};
use crate::transport::{SongEditHandle, SongMetadataSlot};

/// New notes snap to this grid (a 16th at the song's tempo).
const SNAP_DIVISOR: u32 = 4;
/// New notes last one beat.
const NEW_NOTE_TICKS: u32 = TICKS_PER_BEAT;
const NEW_NOTE_VELOCITY: u8 = 100;
/// Preview notes release after this long.
const PREVIEW_HOLD: Duration = Duration::from_millis(300);
/// New notes land on this track (piano-roll track selection is a later pass).
const EDIT_TRACK: usize = 0;

/// The synth-bank swap ring was full when a transactional load tried to submit
/// its bank — the RT thread is wedged or badly backed up. The caller aborts the
/// load with no document change (`code:"busy"` over Bus).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BankRingFull;

/// The editing state. Ring producers/consumers are SPSC (`Send`, not `Sync`),
/// so they sit behind uncontended `Mutex`es to satisfy `Resource` — the same
/// pattern `InProcessTransport` uses.
#[derive(Resource)]
pub struct SongEditor {
    song: cosmix_song::Song,
    history: HistoryManager,
    /// None = no font loaded yet (bare launch) — the session is silent and
    /// every rebuild produces voiceless tracks until File > Open SoundFont.
    soundfont: Option<std::sync::Arc<rustysynth::SoundFont>>,
    /// Where `soundfont` was actually loaded from — the skip-reload identity
    /// (the song document's `soundfont_path` may lag or differ, e.g. after a
    /// failed load or a `--soundfont` override).
    soundfont_source: Option<std::path::PathBuf>,
    /// UI → transport deposit for song metadata + track names (footer/strip
    /// display leaves follow document loads).
    meta_slot: SongMetadataSlot,
    bank_tx: Mutex<rtrb::Producer<SongBankSwap>>,
    bank_rx: Mutex<rtrb::Consumer<Box<cosmix_musicd::mixer::MidiSynthBank>>>,
    note_tx: Mutex<rtrb::Producer<RtCommand>>,
    /// Preview notes awaiting their scheduled release.
    pending_offs: Vec<(Instant, usize, u8)>,
    /// Bumped on every document change shipped to the engine — cache
    /// invalidation for derived views (the waves render).
    revision: u64,
}

impl SongEditor {
    pub fn new(handle: SongEditHandle) -> Self {
        SongEditor {
            song: handle.song,
            history: HistoryManager::new(),
            soundfont: handle.soundfont,
            soundfont_source: handle.soundfont_source,
            meta_slot: handle.meta_slot,
            bank_tx: Mutex::new(handle.bank_tx),
            bank_rx: Mutex::new(handle.bank_rx),
            note_tx: Mutex::new(handle.note_tx),
            pending_offs: Vec::new(),
            revision: 0,
        }
    }

    pub fn song(&self) -> &cosmix_song::Song {
        &self.song
    }

    pub fn soundfont(&self) -> Option<&std::sync::Arc<rustysynth::SoundFont>> {
        self.soundfont.as_ref()
    }

    /// See [`SongEditor::revision`].
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Replace the whole document (File → Open). History is cleared — the new
    /// song is a fresh timeline. The caller follows with `write_model` +
    /// `ship_bank`.
    pub fn replace_song(&mut self, song: cosmix_song::Song) {
        self.song = song;
        self.history.clear();
        self.deposit_meta();
    }

    /// Deposit the current document's display metadata for the transport to
    /// apply to the footer/strip leaves.
    fn deposit_meta(&self) {
        let meta = cosmix_musicd::mixer::SongMeta {
            title: self.song.name.clone(),
            artist: String::new(),
            copyright: String::new(),
        };
        let mut names: [Option<String>; NUM_CHANNELS] = std::array::from_fn(|_| None);
        for (channel, track) in self.song.tracks().iter().enumerate() {
            if channel < NUM_CHANNELS {
                names[channel] = Some(track.name.clone());
            }
        }
        *self.meta_slot.lock().expect("meta slot poisoned") = Some((meta, names));
    }

    /// Where the current soundfont was actually loaded from.
    pub fn soundfont_source(&self) -> Option<&std::path::Path> {
        self.soundfont_source.as_deref()
    }

    /// Swap the soundfont, remembering `source` as the loaded-font identity.
    /// With `record_on_song`, the path is also written to the song document
    /// so every save format carries the font actually loaded (pass false
    /// when the document already names it). The caller follows with
    /// `ship_bank` to re-voice the running engine.
    pub fn set_soundfont(
        &mut self,
        soundfont: std::sync::Arc<rustysynth::SoundFont>,
        source: &std::path::Path,
        record_on_song: bool,
    ) {
        self.soundfont = Some(soundfont);
        self.soundfont_source = Some(source.to_path_buf());
        if record_on_song {
            self.song.set_soundfont_path(Some(source));
        }
    }

    /// Write-model + ship-bank in one step — the path every whole-document
    /// change (open, soundfont swap, undo) takes.
    pub fn resync(&mut self, model: &mut PianoRollModel) {
        self.write_model(model);
        self.ship_bank();
    }

    /// Fill the piano-roll render model from the song document.
    pub fn write_model(&self, model: &mut PianoRollModel) {
        let tempo = self.song.tempo.max(1);
        let tick_secs = 60.0 / (tempo as f32 * TICKS_PER_BEAT as f32);
        model.notes.clear();
        for (channel, track) in self.song.tracks().iter().enumerate() {
            for note in track.notes() {
                model.notes.push(PianoRollNote {
                    id: note.id.as_u64(),
                    channel: channel as u32,
                    key: note.pitch,
                    start_secs: note.start_tick as f32 * tick_secs,
                    dur_secs: note.duration_ticks.max(1) as f32 * tick_secs,
                });
            }
        }
        // The rendered domain matches the engine's transport length: last
        // note-off + the same 1 s release tail song_schedules appends.
        model.length_secs = self.song.duration_seconds() as f32
            + cosmix_musicd::mixer_host::SONG_RELEASE_TAIL_FRAMES as f32
                / cosmix_musicd::mixer::SR as f32;
        model.beat_secs = 60.0 / tempo as f32;
        model.beats_per_measure = self.song.time_sig_numerator.max(1) as u32;
    }

    /// Rebuild the synth bank from the current document and ship it to the
    /// engine. Drains returned banks first (their dealloc happens here).
    pub fn ship_bank(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        {
            let mut rx = self.bank_rx.lock().expect("bank return ring poisoned");
            while rx.pop().is_ok() {}
        }
        match song_bank_with(&self.song, self.soundfont.as_ref()) {
            Ok(bank) => {
                let mut tx = self.bank_tx.lock().expect("bank ring poisoned");
                // An EDIT: the RT thread preserves the playing state + playhead.
                if tx
                    .push(SongBankSwap {
                        bank: Box::new(bank),
                        load: false,
                    })
                    .is_err()
                {
                    // 8 in-flight edits with a wedged RT thread — drop this
                    // rebuild; the next edit ships a fresh full document.
                    eprintln!("studio: song swap ring full, edit not shipped");
                }
            }
            Err(error) => eprintln!("studio: rebuild synth bank: {error}"),
        }
    }

    // --- Transactional load path (File > Open Song / Bus app.song.load) -------
    //
    // A whole-document LOAD must not half-apply: the old `open_song` committed
    // the document, THEN `ship_bank` built the bank — so a build/ring failure
    // left the document swapped but the engine on the old bank, yet reported
    // success. Loads instead build the candidate bank FIRST (`build_bank`),
    // submit it (`submit_bank`), and only then commit the document
    // (`commit_loaded_song`). Any failure before the commit leaves the editor
    // and engine on the previous song. The edit path keeps `ship_bank`.

    /// Build a synth bank from a CANDIDATE document + soundfont without mutating
    /// the editor — the first fallible step of a transactional load. `soundfont`
    /// is the font the bank will voice with (the newly loaded one, or the
    /// current one when the load keeps it).
    pub fn build_bank(
        &self,
        song: &cosmix_song::Song,
        soundfont: Option<&std::sync::Arc<rustysynth::SoundFont>>,
    ) -> Result<Box<cosmix_musicd::mixer::MidiSynthBank>, String> {
        song_bank_with(song, soundfont)
            .map(Box::new)
            .map_err(|error| error.to_string())
    }

    /// Ship an already-built bank to the engine, draining returns first. Bumps
    /// the revision on success. `Err(BankRingFull)` = the swap ring is full (RT
    /// thread wedged/backed up); the caller must NOT have committed the document.
    pub fn submit_bank(
        &mut self,
        bank: Box<cosmix_musicd::mixer::MidiSynthBank>,
    ) -> Result<(), BankRingFull> {
        {
            let mut rx = self.bank_rx.lock().expect("bank return ring poisoned");
            while rx.pop().is_ok() {}
        }
        let mut tx = self.bank_tx.lock().expect("bank ring poisoned");
        // A LOAD: the RT thread applies the barrier (stop + rewind to zero) when
        // it installs this bank, so a fresh song never renders at a stale playhead.
        tx.push(SongBankSwap { bank, load: true })
            .map_err(|_| BankRingFull)?;
        drop(tx);
        self.revision = self.revision.wrapping_add(1);
        Ok(())
    }

    /// Commit a freshly loaded document (+ optional new soundfont) into the
    /// editor and render model, AFTER its bank has been built and submitted.
    /// Clears history — a load is a fresh timeline. Does NOT ship a bank; the
    /// caller already did, transactionally.
    pub fn commit_loaded_song(
        &mut self,
        song: cosmix_song::Song,
        soundfont: Option<(std::sync::Arc<rustysynth::SoundFont>, std::path::PathBuf)>,
        model: &mut PianoRollModel,
    ) {
        // Set the document + clear history + deposit metadata (a load is a fresh
        // timeline) — the same commit `replace_song` performs, reused here.
        self.replace_song(song);
        if let Some((font, source)) = soundfont {
            self.set_soundfont(font, &source, false);
        }
        self.write_model(model);
    }

    fn preview(&mut self, channel: usize, key: u8) {
        let mut tx = self.note_tx.lock().expect("note ring poisoned");
        let _ = tx.push(RtCommand::NoteEvent {
            channel,
            key,
            vel: NEW_NOTE_VELOCITY,
            on: true,
        });
        self.pending_offs
            .push((Instant::now() + PREVIEW_HOLD, channel, key));
    }
}

pub struct SongEditorPlugin;

impl Plugin for SongEditorPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ConsumedShortcutInputs>()
            .add_observer(on_piano_roll_click)
            .add_systems(
                Update,
                (
                    release_preview_notes,
                    sync_model_once,
                    // Board-scoped: undo/redo doesn't fire under a modal.
                    undo_redo_keys.in_set(BoardInputSystems),
                ),
            );
    }
}

/// Seed the piano-roll model on the first frame (edits keep it fresh after).
fn sync_model_once(
    editor: Option<Res<SongEditor>>,
    mut model: ResMut<PianoRollModel>,
    mut seeded: bevy::prelude::Local<bool>,
) {
    if *seeded {
        return;
    }
    if let Some(editor) = editor {
        editor.write_model(&mut model);
        *seeded = true;
    }
}

/// A click on the roll: on a note → delete it; on empty grid → add a snapped
/// note on the edit track (with an audible preview). Both directions snapshot
/// for undo first.
fn on_piano_roll_click(
    click: On<PianoRollClick>,
    editor: Option<ResMut<SongEditor>>,
    mut model: ResMut<PianoRollModel>,
) {
    let Some(mut editor) = editor else { return };
    if let Some(id) = click.existing {
        let Some((track_idx, note_id)) = find_note(&editor.song, id) else {
            return;
        };
        let snapshot = StateSnapshot::new(&editor.song, "Delete note");
        editor.history.push_undo(snapshot);
        if let Some(track) = editor.song.track_at_mut(track_idx) {
            track.remove_note(note_id);
        }
    } else {
        let tempo = editor.song.tempo.max(1);
        let tick = (click.secs as f64 * tempo as f64 / 60.0 * TICKS_PER_BEAT as f64) as u32;
        let snap = TICKS_PER_BEAT / SNAP_DIVISOR;
        let tick = (tick + snap / 2) / snap * snap;
        let snapshot = StateSnapshot::new(&editor.song, "Add note");
        editor.history.push_undo(snapshot);
        let Some(track) = editor.song.track_at_mut(EDIT_TRACK) else {
            return;
        };
        track.add_note(Note::new(
            click.key,
            NEW_NOTE_VELOCITY,
            tick,
            NEW_NOTE_TICKS,
        ));
        editor.preview(EDIT_TRACK, click.key);
    }
    editor.write_model(&mut model);
    editor.ship_bank();
}

/// Ctrl+Z / Ctrl+Shift+Z (also Ctrl+Y): snapshot-based undo/redo, then push
/// the restored document to the roll and the engine.
fn undo_redo_keys(
    consumed: Res<ConsumedShortcutInputs>,
    editor: Option<ResMut<SongEditor>>,
    mut model: ResMut<PianoRollModel>,
) {
    let Some(mut editor) = editor else { return };
    let mut changed = false;
    for event in consumed.unclaimed_presses() {
        let undo = event.physical == KeyCode::KeyZ
            && event.raw.modifiers.control
            && !event.raw.modifiers.shift;
        let redo = event.raw.modifiers.control
            && ((event.physical == KeyCode::KeyZ && event.raw.modifiers.shift)
                || event.physical == KeyCode::KeyY);
        if undo {
            if let Some(snapshot) = editor.history.pop_undo() {
                let current = StateSnapshot::new(&editor.song, snapshot.description.clone());
                editor.history.push_redo(current);
                editor.song = snapshot.song;
                changed = true;
            }
        } else if redo {
            if let Some(snapshot) = editor.history.pop_redo() {
                let current = StateSnapshot::new(&editor.song, snapshot.description.clone());
                editor.history.push_undo_preserve_redo(current);
                editor.song = snapshot.song;
                changed = true;
            }
        }
    }
    if !changed {
        return;
    }
    editor.write_model(&mut model);
    editor.ship_bank();
}

/// Release preview notes whose hold expired.
fn release_preview_notes(editor: Option<ResMut<SongEditor>>) {
    let Some(mut editor) = editor else { return };
    if editor.pending_offs.is_empty() {
        return;
    }
    let now = Instant::now();
    let due: Vec<(usize, u8)> = {
        let pending = &mut editor.pending_offs;
        let mut due = Vec::new();
        pending.retain(|(at, channel, key)| {
            if *at <= now {
                due.push((*channel, *key));
                false
            } else {
                true
            }
        });
        due
    };
    let mut tx = editor.note_tx.lock().expect("note ring poisoned");
    for (channel, key) in due {
        let _ = tx.push(RtCommand::NoteEvent {
            channel,
            key,
            vel: 0,
            on: false,
        });
    }
}

/// Locate a note by its raw id across tracks → `(track_index, NoteId)`.
fn find_note(song: &cosmix_song::Song, raw_id: u64) -> Option<(usize, cosmix_song::NoteId)> {
    for (idx, track) in song.tracks().iter().enumerate() {
        if let Some(note) = track.notes().iter().find(|n| n.id.as_u64() == raw_id) {
            return Some((idx, note.id));
        }
    }
    None
}

/// A bare `SongEditor` for tests, named document, empty history, no soundfont,
/// and fresh capacity-2 swap rings (the return consumer is dropped, so the bank
/// ring fills after two `submit_bank` pushes — the ring-full path). Module-level
/// + `pub(crate)` so sibling modules' tests (e.g. `song_load`) can build one.
#[cfg(test)]
pub(crate) fn test_editor(song_name: &str) -> SongEditor {
    let (bank_tx, _bank_sink) = rtrb::RingBuffer::<SongBankSwap>::new(2);
    let (_bank_return, bank_rx) =
        rtrb::RingBuffer::<Box<cosmix_musicd::mixer::MidiSynthBank>>::new(2);
    let (note_tx, _note_sink) = rtrb::RingBuffer::<RtCommand>::new(2);
    SongEditor {
        song: cosmix_song::Song::new(song_name),
        history: HistoryManager::new(),
        soundfont: None,
        soundfont_source: None,
        meta_slot: std::sync::Arc::new(Mutex::new(None)),
        bank_tx: Mutex::new(bank_tx),
        bank_rx: Mutex::new(bank_rx),
        note_tx: Mutex::new(note_tx),
        pending_offs: Vec::new(),
        revision: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use bevy::app::TaskPoolPlugin;
    use bevy::ecs::message::MessageWriter;
    use bevy::feathers::theme::UiTheme;
    use bevy::input::ButtonInput;
    use bevy::input_focus::InputFocus;
    use ctk::prelude::{
        FileRequest, FileRequestId, FileRequesterPlugin, FileRequesterSystems, ModalCapture,
        MusicdMixerState, TransportSeekRequest,
    };

    use crate::action::{ActionPlugin, ShortcutInputQueue};

    #[derive(Resource)]
    struct PendingFileRequest(Option<FileRequest>);

    fn produce_file_request(
        mut pending: ResMut<PendingFileRequest>,
        mut requests: MessageWriter<FileRequest>,
    ) {
        if let Some(request) = pending.0.take() {
            requests.write(request);
        }
    }

    fn editor_with_undo() -> SongEditor {
        let (bank_tx, _bank_sink) = rtrb::RingBuffer::<SongBankSwap>::new(2);
        let (_bank_return, bank_rx) =
            rtrb::RingBuffer::<Box<cosmix_musicd::mixer::MidiSynthBank>>::new(2);
        let (note_tx, _note_sink) = rtrb::RingBuffer::<RtCommand>::new(2);
        let mut history = HistoryManager::new();
        history.push_undo(StateSnapshot::new(
            &cosmix_song::Song::new("before undo"),
            "test undo",
        ));
        SongEditor {
            song: cosmix_song::Song::new("current document"),
            history,
            soundfont: None,
            soundfont_source: None,
            meta_slot: Arc::new(Mutex::new(None)),
            bank_tx: Mutex::new(bank_tx),
            bank_rx: Mutex::new(bank_rx),
            note_tx: Mutex::new(note_tx),
            pending_offs: Vec::new(),
            revision: 0,
        }
    }

    #[test]
    fn same_tick_file_request_is_ingested_before_undo_gate() {
        let mut request = FileRequest::open_file(FileRequestId(301), "Open");
        request.initial_directory = Some(std::env::temp_dir());

        let mut app = App::new();
        app.add_plugins(TaskPoolPlugin::default())
            .init_resource::<InputFocus>()
            .init_resource::<UiTheme>()
            .init_resource::<ButtonInput<KeyCode>>()
            .insert_resource(PendingFileRequest(Some(request)))
            .insert_resource(MusicdMixerState::default())
            .init_resource::<PianoRollModel>()
            .add_plugins(FileRequesterPlugin)
            .add_message::<TransportSeekRequest>()
            .add_plugins((ActionPlugin, SongEditorPlugin))
            .insert_resource(editor_with_undo())
            .add_systems(Update, produce_file_request.before(FileRequesterSystems));

        app.world_mut().resource_mut::<ShortcutInputQueue>().push(
            cosmix_actions::RawInput::pressed(
                cosmix_actions::Key::Character('Z'),
                cosmix_actions::Modifiers {
                    control: true,
                    ..Default::default()
                },
            ),
            KeyCode::KeyZ,
            1,
        );

        app.update();

        assert!(app.world().resource::<ModalCapture>().is_captured());
        assert_eq!(
            app.world().resource::<SongEditor>().song.name,
            "current document",
            "undo_redo_keys must not run after the same-frame requester opens"
        );
    }

    #[test]
    fn transactional_bank_submit_fills_ring_then_reports_full() {
        let mut editor = test_editor("doc");
        let song = cosmix_song::Song::new("candidate");
        // build_bank is pure — it does not mutate or ship.
        let bank = editor.build_bank(&song, None).expect("empty bank builds");
        assert_eq!(
            editor.revision(),
            0,
            "build_bank must not bump the revision"
        );
        // The capacity-2 ring (return consumer dropped) accepts two, then is full.
        assert_eq!(editor.submit_bank(bank), Ok(()));
        let bank = editor.build_bank(&song, None).unwrap();
        assert_eq!(editor.submit_bank(bank), Ok(()));
        let bank = editor.build_bank(&song, None).unwrap();
        assert_eq!(editor.submit_bank(bank), Err(BankRingFull));
        assert_eq!(
            editor.revision(),
            2,
            "revision bumps only on a successful submit, not the full one"
        );
    }

    #[test]
    fn commit_loaded_song_swaps_document_and_clears_history() {
        let mut editor = test_editor("old document");
        editor.history.push_undo(StateSnapshot::new(
            &cosmix_song::Song::new("before"),
            "test undo",
        ));
        assert!(editor.history.can_undo());
        let mut model = PianoRollModel::default();
        editor.commit_loaded_song(cosmix_song::Song::new("loaded document"), None, &mut model);
        assert_eq!(editor.song.name, "loaded document");
        assert!(
            !editor.history.can_undo(),
            "a whole-document load is a fresh timeline"
        );
    }
}
