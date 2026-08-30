//! Studio's single song-load command path.
//!
//! Both local `File > Open Song` and (Phase 4) the Bus `app.song.load` verb
//! produce one [`LoadSongCommand`]; [`apply_song_load`] is the SOLE owner of the
//! transactional load and the only place `AudioIntent::Reset` is emitted for a
//! document load. A load is transactional: the candidate bank is built and
//! submitted BEFORE the document is committed, so a parse/build/ring failure
//! leaves the editor and engine on the previous song (see `editor.rs`).

use std::path::PathBuf;

use bevy::prelude::*;
use cosmix_musicd::mixer_host::load_song;
use cosmix_musicd::synth::load_soundfont;
use ctk::prelude::PianoRollModel;

use crate::action::{ActionProduce, AudioIntent};
use crate::editor::SongEditor;
use crate::views::StatusLine;

/// Where a load was requested from. Phase 4 adds an `Bus { … }` variant carrying
/// the reply-correlation ticket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadSource {
    Local,
}

/// One requested song load. `path` is already authorised/resolved by the ingress
/// (a local picker grant today; a Phase-4 Bus root check later) — this command
/// does NOT itself perform filesystem authorisation.
#[derive(Message, Debug, Clone)]
pub struct LoadSongCommand {
    pub path: PathBuf,
    pub source: LoadSource,
}

/// How the load's soundfont resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundfontOutcome {
    /// No font warning: the song's font loaded, or the song reuses the current
    /// one, or the song names none and the current font is kept cleanly.
    Loaded,
    /// The song named a font that failed to load; the current font was kept.
    KeptCurrent,
}

/// Why a load failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SongLoadErrCode {
    /// Path missing/unreadable/unparseable, bank could not be built, or no song
    /// document exists in this session. `code:"load"` over Bus (rc 10).
    Load,
    /// The swap ring was full — RT thread backed up; retryable. `code:"busy"` (rc 11).
    Busy,
}

/// The transactional load succeeded.
#[derive(Debug, Clone)]
pub struct SongLoadOk {
    /// The Bus reply maps this to `soundfont:"loaded"|"kept_current"`.
    pub soundfont: SoundfontOutcome,
    pub warnings: Vec<String>,
}

/// The transactional load failed; no document change occurred.
#[derive(Debug, Clone)]
pub struct SongLoadErr {
    /// The Bus reply maps this to rc 10 `load` / rc 11 `busy`.
    pub code: SongLoadErrCode,
    pub message: String,
}

/// The typed result of a load. Local ingress maps it to the status line;
/// Phase-4 Bus will map it to the verb reply.
#[derive(Message, Debug, Clone)]
pub struct SongLoadOutcome {
    pub source: LoadSource,
    pub path: PathBuf,
    pub result: Result<SongLoadOk, SongLoadErr>,
}

pub struct SongLoadPlugin;

impl Plugin for SongLoadPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<LoadSongCommand>()
            .add_message::<SongLoadOutcome>()
            // Same set as the file-result producer, ordered after it, so a load
            // command produced this frame is applied this frame and its Reset
            // reaches ActionApply without an extra frame.
            .add_systems(
                Update,
                apply_song_load
                    .in_set(ActionProduce)
                    .after(crate::file_io::apply_file_results),
            )
            .add_systems(Update, report_local_song_load.after(apply_song_load));
    }
}

/// The one system that applies a song load. Transactional: build + submit the
/// bank before committing the document.
fn apply_song_load(
    mut commands: MessageReader<LoadSongCommand>,
    editor: Option<ResMut<SongEditor>>,
    model: Option<ResMut<PianoRollModel>>,
    mut audio: MessageWriter<AudioIntent>,
    mut outcomes: MessageWriter<SongLoadOutcome>,
) {
    let requests: Vec<LoadSongCommand> = commands.read().cloned().collect();
    if requests.is_empty() {
        return;
    }
    let (Some(mut editor), Some(mut model)) = (editor, model) else {
        // No piano-roll document in this launch (e.g. a --stems session).
        for command in requests {
            outcomes.write(SongLoadOutcome {
                source: command.source,
                path: command.path,
                result: Err(SongLoadErr {
                    code: SongLoadErrCode::Load,
                    message: "no song document in this session".into(),
                }),
            });
        }
        return;
    };
    for command in requests {
        let result = load_one(&mut editor, &mut model, &command.path, |_| true);
        if result.is_ok() {
            // A freshly loaded song is a full transport reset (stop + rewind).
            // Emitted here, once, only on a committed load.
            audio.write(AudioIntent::Reset);
        }
        outcomes.write(SongLoadOutcome {
            source: command.source,
            path: command.path,
            result,
        });
    }
}

/// The transactional load, pure of ECS messaging so it unit-tests without a
/// World: parse → resolve soundfont → build bank → submit bank → commit
/// document. Any failure before the commit leaves the editor unchanged. The
/// caller emits `AudioIntent::Reset` on `Ok`.
pub(crate) fn load_one(
    editor: &mut SongEditor,
    model: &mut PianoRollModel,
    path: &std::path::Path,
    soundfont_authorized: impl Fn(&std::path::Path) -> bool,
) -> Result<SongLoadOk, SongLoadErr> {
    // 1. Parse the document (no editor mutation yet).
    let song = load_song(path).map_err(|error| SongLoadErr {
        code: SongLoadErrCode::Load,
        message: format!("open song: {error}"),
    })?;

    // 2. Resolve the soundfont (mirrors the retired open_song semantics): load
    //    the song's font only if it differs from the current one; a failure —
    //    or, for a remote load, an embedded font path OUTSIDE the authorised
    //    roots — keeps the current font with a warning and NEVER opens the
    //    denied path. `soundfont_authorized` is `|_| true` for a local grant.
    let mut warnings = Vec::new();
    // Owned so no editor borrow lingers into the &mut submit below.
    let kept = editor
        .soundfont_source()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "none".into());
    let new_font = song
        .get_soundfont_path()
        .map(PathBuf::from)
        .filter(|source| Some(source.as_path()) != editor.soundfont_source())
        .and_then(|source| {
            if !soundfont_authorized(&source) {
                warnings.push(format!(
                    "song soundfont {} is outside the authorised roots; keeping {kept}",
                    source.display(),
                ));
                return None;
            }
            match load_soundfont(&source) {
                Ok(font) => Some((font, source)),
                Err(error) => {
                    warnings.push(format!(
                        "song soundfont {}: {error}; keeping {kept}",
                        source.display(),
                    ));
                    None
                }
            }
        });
    let soundfont = if warnings.is_empty() {
        SoundfontOutcome::Loaded
    } else {
        SoundfontOutcome::KeptCurrent
    };

    // 3. Build the candidate bank BEFORE any commit, with the font it will voice
    //    with (the newly loaded one, else the current one). Immutable editor
    //    borrow is scoped so the submit below can take &mut.
    let bank = {
        let effective = new_font
            .as_ref()
            .map(|(font, _)| font)
            .or_else(|| editor.soundfont());
        editor.build_bank(&song, effective)
    }
    .map_err(|error| SongLoadErr {
        code: SongLoadErrCode::Load,
        message: format!("rebuild synth bank: {error}"),
    })?;

    // 4. Submit the bank. A full ring aborts the load with NO document change.
    editor.submit_bank(bank).map_err(|_| SongLoadErr {
        code: SongLoadErrCode::Busy,
        message: "song swap ring full".into(),
    })?;

    // 5. Commit — the bank is in flight, now converge the document + model.
    editor.commit_loaded_song(song, new_font, model);

    Ok(SongLoadOk {
        soundfont,
        warnings,
    })
}

/// Map a local load's outcome to the status line (parity with the retired
/// open_song status messages). Bus-source outcomes are handled by the verb
/// reply in Phase 4 and ignored here.
fn report_local_song_load(
    mut outcomes: MessageReader<SongLoadOutcome>,
    status: Option<ResMut<StatusLine>>,
) {
    let Some(mut status) = status else {
        return;
    };
    for outcome in outcomes.read() {
        if outcome.source != LoadSource::Local {
            continue;
        }
        match &outcome.result {
            Ok(ok) => match ok.warnings.first() {
                Some(warning) => {
                    status.error(format!("opened {} — {warning}", outcome.path.display()))
                }
                None => status.ok(format!("opened {}", outcome.path.display())),
            },
            Err(error) => status.error(error.message.clone()),
        }
    }
}

/// Swap the active soundfont and re-voice the running song. The re-voice ships
/// as an EDIT (not a load), so the transport is PRESERVED — the font changes
/// live without stopping playback. Shared by local `File > Open SoundFont` and
/// the Bus `app.soundfont.load` verb. `Err` (soundfont can't be loaded) leaves
/// the current font untouched.
///
/// Accepted edit-path tradeoffs (parity with the local picker, so the Bus reply
/// matches what a local user gets): the fallible ship is `resync`/`ship_bank`,
/// which logs a ring-full/rebuild failure rather than returning it — so `Ok`
/// (and the verb's `loaded:true`) means "font loaded + editor updated + swap
/// enqueued as an edit", self-healing on the next edit; and `record_on_song`
/// writes the font path into the document (a later save carries it).
pub(crate) fn apply_soundfont(
    editor: &mut SongEditor,
    model: &mut PianoRollModel,
    path: &std::path::Path,
) -> Result<(), String> {
    let soundfont = load_soundfont(path).map_err(|error| format!("open soundfont: {error}"))?;
    editor.set_soundfont(soundfont, path, true);
    editor.resync(model);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_editor;

    /// Save a named `Song` to a temp `.json` that `load_song` can read back.
    fn temp_song(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "studio-song-load-{}-{}.json",
            std::process::id(),
            name.replace(' ', "-"),
        ));
        cosmix_song::Song::new(name)
            .save_to_file(&path)
            .expect("write temp song");
        path
    }

    #[test]
    fn successful_load_commits_the_document() {
        let mut editor = test_editor("old song");
        let mut model = PianoRollModel::default();
        let path = temp_song("new song");

        let result = load_one(&mut editor, &mut model, &path, |_| true);

        assert!(matches!(
            result,
            Ok(SongLoadOk {
                soundfont: SoundfontOutcome::Loaded,
                ..
            })
        ));
        assert_eq!(editor.song().name, "new song", "document committed");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn parse_failure_leaves_the_document_unchanged() {
        let mut editor = test_editor("old song");
        let mut model = PianoRollModel::default();
        // A path that does not exist / cannot parse.
        let path = std::env::temp_dir().join("studio-song-load-does-not-exist.json");
        std::fs::remove_file(&path).ok();

        let result = load_one(&mut editor, &mut model, &path, |_| true);

        assert!(matches!(
            result,
            Err(SongLoadErr {
                code: SongLoadErrCode::Load,
                ..
            })
        ));
        assert_eq!(
            editor.song().name,
            "old song",
            "a parse failure must not touch the document"
        );
    }

    #[test]
    fn missing_soundfont_keeps_current_font_with_a_warning() {
        let mut editor = test_editor("old song");
        let mut model = PianoRollModel::default();
        // A song that references a soundfont which cannot be loaded.
        let mut song = cosmix_song::Song::new("song with bad font");
        song.set_soundfont_path(Some("/nonexistent/studio-test-font.sf2"));
        let path = std::env::temp_dir().join(format!(
            "studio-song-load-{}-badfont.json",
            std::process::id()
        ));
        song.save_to_file(&path).expect("write temp song");

        let ok = load_one(&mut editor, &mut model, &path, |_| true)
            .expect("the song still loads when its referenced font is missing");

        assert_eq!(ok.soundfont, SoundfontOutcome::KeptCurrent);
        assert!(
            !ok.warnings.is_empty(),
            "a missing referenced font produces a warning"
        );
        assert_eq!(
            editor.song().name,
            "song with bad font",
            "the document still commits; only the font is kept"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_denied_soundfont_keeps_current_and_still_loads() {
        // The Bus soundfont gate: an embedded font the caller may NOT open is
        // never loaded — the song still commits (rc=0) keeping the current font,
        // with a warning. Modelled with a gate that denies every font.
        let mut editor = test_editor("old song");
        let mut model = PianoRollModel::default();
        let mut song = cosmix_song::Song::new("song with a walled font");
        song.set_soundfont_path(Some("/anywhere/font.sf2"));
        let path = std::env::temp_dir().join(format!(
            "studio-song-load-{}-denied-sf.json",
            std::process::id()
        ));
        song.save_to_file(&path).unwrap();

        let ok = load_one(&mut editor, &mut model, &path, |_| false)
            .expect("the song loads even when its font is denied");

        assert_eq!(ok.soundfont, SoundfontOutcome::KeptCurrent);
        assert!(
            ok.warnings
                .iter()
                .any(|warning| warning.contains("outside the authorised roots")),
            "a denied font produces a policy warning"
        );
        assert_eq!(editor.song().name, "song with a walled font");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_failed_load_emits_no_transport_reset() {
        use crate::action::AudioIntent;

        #[derive(Resource, Default)]
        struct ResetLog(usize);
        fn record_resets(mut intents: MessageReader<AudioIntent>, mut log: ResMut<ResetLog>) {
            for intent in intents.read() {
                if matches!(intent, AudioIntent::Reset) {
                    log.0 += 1;
                }
            }
        }

        let mut app = App::new();
        app.add_message::<AudioIntent>()
            .init_resource::<ResetLog>()
            .init_resource::<PianoRollModel>()
            .add_plugins(SongLoadPlugin)
            .add_systems(Update, record_resets.after(apply_song_load));

        // Editor whose swap ring is already full → the load fails Busy.
        let mut editor = test_editor("old song");
        for _ in 0..2 {
            let bank = editor
                .build_bank(&cosmix_song::Song::new("filler"), None)
                .unwrap();
            editor.submit_bank(bank).unwrap();
        }
        app.insert_resource(editor);

        let path = temp_song("attempt-no-reset");
        app.world_mut().write_message(LoadSongCommand {
            path: path.clone(),
            source: LoadSource::Local,
        });
        app.update();

        assert_eq!(
            app.world().resource::<ResetLog>().0,
            0,
            "a failed (busy) load must not emit a transport Reset"
        );
        assert_eq!(
            app.world().resource::<SongEditor>().song().name,
            "old song",
            "and the document is unchanged"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn full_swap_ring_aborts_the_load_with_no_document_change() {
        let mut editor = test_editor("old song");
        let mut model = PianoRollModel::default();
        // Fill the capacity-2 swap ring so the load's submit fails.
        for _ in 0..2 {
            let bank = editor
                .build_bank(&cosmix_song::Song::new("filler"), None)
                .unwrap();
            editor.submit_bank(bank).unwrap();
        }
        let path = temp_song("attempted");

        let result = load_one(&mut editor, &mut model, &path, |_| true);

        assert!(
            matches!(
                result,
                Err(SongLoadErr {
                    code: SongLoadErrCode::Busy,
                    ..
                })
            ),
            "a full ring is a retryable busy failure"
        );
        assert_eq!(
            editor.song().name,
            "old song",
            "the transactional guarantee: a busy submit must not commit the document"
        );
        std::fs::remove_file(&path).ok();
    }
}
