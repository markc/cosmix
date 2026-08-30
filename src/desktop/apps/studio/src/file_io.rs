//! Studio file intents and their application-level load/save behaviour.
//!
//! CTK owns path selection only (the native requester corrects extensions
//! and confirms overwrites against the CORRECTED name before returning).
//! Studio interprets the selected path: song/soundfont loads, session
//! saves, and the audio-export jobs — every outcome reported through the
//! transport-bar status line.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::sync::Mutex;

use bevy::ecs::message::{MessageReader, MessageWriter};
use bevy::ecs::schedule::IntoScheduleConfigs;
use bevy::ecs::system::SystemParam;
use bevy::input_focus::{FocusCause, InputFocus};
use bevy::prelude::{App, Plugin, Query, Res, ResMut, Resource, Update};
use cosmix_actions::{studio as ids, ActionId};
use cosmix_musicd::mixer_host::{export_song_wav, song_initial_controls};
use cosmix_musicd::render::RenderFormat;
use ctk::prelude::{
    ActionRequest, FileFilter, FileRequest, FileRequestId, FileRequestOutcome, FileRequestResult,
    MusicdMixerState, PianoRollModel,
};

use crate::action::{ActionRoute, CaptureEstablishedThisFrame};
use crate::editor::SongEditor;
use crate::views::{
    controls_from_state, ExportJob, ExportJobState, ExportMsg, RegionEditor, StatusLine,
};

/// Canonical menu ids consumed by this module's action-bus reader.
pub(crate) const HANDLED_MENU_ACTION_IDS: &[ActionId] = &[
    ids::MENU_SONG_OPEN,
    ids::MENU_SONG_SAVE,
    ids::MENU_SF_OPEN,
    ids::MENU_WAV_EXPORT,
    ids::MENU_SESSION_SAVE,
    ids::MENU_SESSION_EXPORT_WAV,
    ids::MENU_SESSION_EXPORT_FLAC,
];

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum FileAction {
    OpenSong,
    SaveSong,
    OpenSoundfont,
    ExportWav,
    SaveSession,
    ExportSessionWav,
    ExportSessionFlac,
}

impl FileAction {
    fn from_action(action: ActionId) -> Option<Self> {
        match action {
            ids::MENU_SONG_OPEN => Some(Self::OpenSong),
            ids::MENU_SONG_SAVE => Some(Self::SaveSong),
            ids::MENU_SF_OPEN => Some(Self::OpenSoundfont),
            ids::MENU_WAV_EXPORT => Some(Self::ExportWav),
            ids::MENU_SESSION_SAVE => Some(Self::SaveSession),
            ids::MENU_SESSION_EXPORT_WAV => Some(Self::ExportSessionWav),
            ids::MENU_SESSION_EXPORT_FLAC => Some(Self::ExportSessionFlac),
            _ => None,
        }
    }

    fn request_id(self) -> FileRequestId {
        FileRequestId(match self {
            Self::OpenSong => 1,
            Self::SaveSong => 2,
            Self::OpenSoundfont => 3,
            Self::ExportWav => 4,
            Self::SaveSession => 5,
            Self::ExportSessionWav => 6,
            Self::ExportSessionFlac => 7,
        })
    }

    fn from_request_id(id: FileRequestId) -> Option<Self> {
        match id.0 {
            1 => Some(Self::OpenSong),
            2 => Some(Self::SaveSong),
            3 => Some(Self::OpenSoundfont),
            4 => Some(Self::ExportWav),
            5 => Some(Self::SaveSession),
            6 => Some(Self::ExportSessionWav),
            7 => Some(Self::ExportSessionFlac),
            _ => None,
        }
    }

    fn session_export_format(self) -> Option<RenderFormat> {
        match self {
            Self::ExportSessionWav => Some(RenderFormat::Wav16),
            Self::ExportSessionFlac => Some(RenderFormat::Flac24),
            _ => None,
        }
    }
}

pub(crate) fn handles_menu_action(action: ActionId) -> bool {
    let executable = FileAction::from_action(action).is_some();
    debug_assert_eq!(HANDLED_MENU_ACTION_IDS.contains(&action), executable);
    executable
}

#[derive(Resource, Default)]
pub(crate) struct FileIoState {
    last_directories: HashMap<FileAction, PathBuf>,
}

pub struct FileIoPlugin;

impl Plugin for FileIoPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<FileIoState>()
            .init_resource::<CaptureEstablishedThisFrame>()
            .init_resource::<MusicdMixerState>()
            .init_resource::<ExportJob>()
            .add_message::<ActionRequest>()
            .add_message::<FileRequest>()
            .add_message::<FileRequestResult>()
            .add_message::<crate::song_load::LoadSongCommand>()
            .add_systems(Update, on_file_actions.in_set(ActionRoute))
            .add_systems(
                Update,
                apply_file_results.in_set(crate::action::ActionProduce),
            );
    }
}

#[derive(SystemParam)]
struct MenuInvocationFocus<'w, 's> {
    entities: Query<'w, 's, ()>,
    focus: ResMut<'w, InputFocus>,
}

#[derive(SystemParam)]
struct FileActionParams<'w, 's> {
    actions: MessageReader<'w, 's, ActionRequest>,
    editor: Option<Res<'w, SongEditor>>,
    region_editor: Option<Res<'w, RegionEditor>>,
    state: Res<'w, FileIoState>,
    status: ResMut<'w, StatusLine>,
    invocation: MenuInvocationFocus<'w, 's>,
    requests: MessageWriter<'w, FileRequest>,
    capture: ResMut<'w, CaptureEstablishedThisFrame>,
}

fn on_file_actions(mut params: FileActionParams) {
    for request in params.actions.read() {
        if !handles_menu_action(request.action) {
            continue;
        }
        let action = FileAction::from_action(request.action)
            .expect("handled file-menu action must have an executable arm");
        // Menu chrome is deliberately non-focusable and clears focus on press.
        // Restore the recorded invoker before validation as well as requester
        // launch, so a rejected action does not strand focus on the window.
        if let Some(invocation_focus) = request.invocation_focus {
            if params.invocation.entities.contains(invocation_focus) {
                params
                    .invocation
                    .focus
                    .set(invocation_focus, FocusCause::Navigated);
            } else {
                params.invocation.focus.clear();
            }
        }
        let needs_region_editor = matches!(
            action,
            FileAction::SaveSession | FileAction::ExportSessionWav | FileAction::ExportSessionFlac
        );
        if needs_region_editor {
            if params.region_editor.is_none() {
                params
                    .status
                    .error("session actions need a --stems session");
                continue;
            }
        } else if params.editor.is_none() {
            params.status.error("file actions need a song session");
            continue;
        }
        params.requests.write(request_for(
            action,
            params.state.last_directories.get(&action).cloned(),
        ));
        params.capture.mark_request(request);
    }
}

/// The cosmix soundfont store (`$XDG_DATA_HOME/cosmix/musicd`, i.e.
/// `~/.local/share/cosmix/musicd`) — the default place to browse for `.sf2`
/// banks until a more canonical asset location is settled.
fn cosmix_soundfont_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .map(|data_home| data_home.join("cosmix/musicd"))
}

fn request_for(action: FileAction, initial_directory: Option<PathBuf>) -> FileRequest {
    let (mut request, filter) = match action {
        FileAction::OpenSong => (
            FileRequest::open_file(action.request_id(), "Open song"),
            Some(FileFilter::new(
                "Songs",
                ["mid", "midi", "asc", "json", "oxm"],
            )),
        ),
        FileAction::SaveSong => (
            FileRequest::save_file(action.request_id(), "Save song as"),
            Some(FileFilter::new(
                "Songs",
                ["mid", "midi", "asc", "json", "oxm"],
            )),
        ),
        FileAction::OpenSoundfont => (
            FileRequest::open_file(action.request_id(), "Open SoundFont"),
            Some(FileFilter::new("SoundFonts", ["sf2", "sf3", "sfz"])),
        ),
        FileAction::ExportWav => (
            FileRequest::save_file(action.request_id(), "Export WAV"),
            Some(FileFilter::new("WAV audio", ["wav"])),
        ),
        FileAction::SaveSession => (
            FileRequest::save_file(action.request_id(), "Save session as"),
            Some(FileFilter::new("Stem sessions", ["mix"])),
        ),
        FileAction::ExportSessionWav => (
            FileRequest::select_directory(action.request_id(), "Export session audio (WAV)"),
            None,
        ),
        FileAction::ExportSessionFlac => (
            FileRequest::select_directory(action.request_id(), "Export session audio (FLAC)"),
            None,
        ),
    };
    request.initial_directory = initial_directory.or_else(|| match action {
        // No remembered directory yet → open the soundfont browser where the
        // cosmix GM banks live (`~/.local/share/cosmix/musicd`).
        FileAction::OpenSoundfont => cosmix_soundfont_dir().filter(|dir| dir.is_dir()),
        _ => None,
    });
    request.filters.extend(filter);
    match action {
        FileAction::SaveSong => {
            request.suggested_name = Some("song.json".into());
            request.default_extension = Some("json".into());
        }
        FileAction::ExportWav => {
            request.suggested_name = Some("song.wav".into());
            request.default_extension = Some("wav".into());
            request.enforce_extension = true;
        }
        FileAction::SaveSession => {
            request.suggested_name = Some("session.mix".into());
            request.default_extension = Some("mix".into());
            // The requester corrects the extension BEFORE its overwrite
            // confirmation, so the prompt always covers the real final name.
            request.enforce_extension = true;
        }
        _ => {}
    }
    request
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_file_results(
    mut results: MessageReader<FileRequestResult>,
    mut state: ResMut<FileIoState>,
    editor: Option<ResMut<SongEditor>>,
    model: Option<ResMut<PianoRollModel>>,
    region_editor: Option<Res<RegionEditor>>,
    mixer_state: Res<MusicdMixerState>,
    mut export_job: ResMut<ExportJob>,
    mut status: ResMut<StatusLine>,
    mut loads: MessageWriter<crate::song_load::LoadSongCommand>,
) {
    let selected: Vec<_> = results
        .read()
        .filter_map(|result| {
            let action = FileAction::from_request_id(result.id)?;
            match &result.outcome {
                FileRequestOutcome::Selected(paths) => paths.first().cloned().map(|path| {
                    let remembered = match action.session_export_format() {
                        Some(_) => Some(path.as_path()),
                        None => path.parent(),
                    };
                    if let Some(directory) = remembered {
                        state
                            .last_directories
                            .insert(action, directory.to_path_buf());
                    }
                    (action, path)
                }),
                FileRequestOutcome::Cancelled => None,
                FileRequestOutcome::Failed(error) => {
                    status.error(format!("file requester: {error}"));
                    None
                }
            }
        })
        .collect();

    for (action, path) in &selected {
        match action {
            FileAction::SaveSession => {
                if let Some(region_editor) = &region_editor {
                    match region_editor.save_session(path) {
                        Ok(()) => status.ok(format!("session saved {}", path.display())),
                        Err(error) => status.error(format!("save session: {error}")),
                    }
                }
            }
            FileAction::ExportSessionWav | FileAction::ExportSessionFlac => {
                let Some(format) = action.session_export_format() else {
                    continue;
                };
                let Some(region_editor) = &region_editor else {
                    continue;
                };
                spawn_session_export(
                    region_editor,
                    &mixer_state,
                    &mut export_job,
                    &mut status,
                    path.clone(),
                    format,
                );
            }
            _ => {}
        }
    }

    let (Some(mut editor), Some(mut model)) = (editor, model) else {
        return;
    };
    for (action, path) in selected {
        match action {
            FileAction::OpenSong => {
                // The load itself (parse → transactional bank swap → document
                // commit → Reset) is owned by `apply_song_load`; both this local
                // picker and the Bus verb converge on one LoadSongCommand.
                loads.write(crate::song_load::LoadSongCommand {
                    path,
                    source: crate::song_load::LoadSource::Local,
                });
            }
            FileAction::SaveSong => save_song(&path, &editor, &mut status),
            FileAction::OpenSoundfont => {
                open_soundfont(&path, &mut editor, &mut model, &mut status)
            }
            FileAction::ExportWav => export_wav(path, &editor, &mut export_job, &mut status),
            FileAction::SaveSession
            | FileAction::ExportSessionWav
            | FileAction::ExportSessionFlac => {}
        }
    }
}

/// Snapshot the document + mix NOW (edits racing the job cannot mix
/// revisions) and render on a worker, reporting through the export job.
fn spawn_session_export(
    region_editor: &RegionEditor,
    mixer_state: &MusicdMixerState,
    export_job: &mut ExportJob,
    status: &mut StatusLine,
    out_dir: PathBuf,
    format: RenderFormat,
) {
    if let Some(active) = &export_job.0 {
        status.error(if active.cancellable {
            "an export is already running (Esc cancels it)"
        } else {
            "an export is already running"
        });
        return;
    }
    let (sources, regions, names, base_length) = region_editor.export_snapshot();
    let controls = controls_from_state(mixer_state);
    let (tx, rx) = channel();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cancel_flag = cancel.clone();
    std::thread::spawn(move || {
        let mut last_sent = 0u64;
        let result = cosmix_musicd::mixer_host::export_stem_session(
            &out_dir,
            format,
            &sources,
            &regions,
            &names,
            base_length,
            &controls,
            &mut |done, total| {
                // ~1% progress granularity keeps the channel quiet.
                if done.saturating_sub(last_sent) > total / 100 || done == total {
                    last_sent = done;
                    let _ = tx.send(ExportMsg::Progress(done, total));
                }
                !cancel_flag.load(std::sync::atomic::Ordering::Relaxed)
            },
        );
        let _ = tx.send(match result {
            Ok(report) => ExportMsg::Done(report),
            Err(error) => ExportMsg::Failed(error.to_string()),
        });
    });
    export_job.0 = Some(ExportJobState {
        rx: Mutex::new(rx),
        cancel,
        cancellable: true,
    });
    status.info("exporting session audio...");
}

/// Returns `true` if a song was loaded (so the caller can reset the transport).
fn save_song(path: &Path, editor: &SongEditor, status: &mut StatusLine) {
    let result = match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("oxm") => editor.song().save_to_binary(path),
        Some("mid" | "midi") => editor.song().export_smf(path),
        Some("asc") => editor.song().export_asc(path),
        _ => editor.song().save_to_file(path),
    };
    match result {
        Ok(()) => status.ok(format!("saved {}", path.display())),
        Err(error) => status.error(format!("save song: {error}")),
    }
}

fn open_soundfont(
    path: &Path,
    editor: &mut SongEditor,
    model: &mut PianoRollModel,
    status: &mut StatusLine,
) {
    match crate::song_load::apply_soundfont(editor, model, path) {
        Ok(()) => status.ok(format!("soundfont {}", path.display())),
        Err(error) => status.error(error),
    }
}

/// One-shot song render on a worker; occupies the (non-cancellable) export
/// job slot so completion/failure reach the status line.
fn export_wav(
    path: PathBuf,
    editor: &SongEditor,
    export_job: &mut ExportJob,
    status: &mut StatusLine,
) {
    if let Some(active) = &export_job.0 {
        status.error(if active.cancellable {
            "an export is already running (Esc cancels it)"
        } else {
            "an export is already running"
        });
        return;
    }
    let Some(soundfont) = editor.soundfont().cloned() else {
        status.error("no SoundFont loaded - File > Open SoundFont first");
        return;
    };
    let song = editor.song().clone();
    let controls = song_initial_controls(&song);
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let message = match export_song_wav(&song, &soundfont, &controls, &path) {
            Ok(()) => ExportMsg::Simple {
                ok: true,
                message: format!("exported {}", path.display()),
            },
            Err(error) => ExportMsg::Simple {
                ok: false,
                message: format!("export wav: {error}"),
            },
        };
        let _ = tx.send(message);
    });
    export_job.0 = Some(ExportJobState {
        rx: Mutex::new(rx),
        cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        cancellable: false,
    });
    status.info("exporting WAV...");
}

#[cfg(test)]
mod tests {
    use super::*;
    use ctk::prelude::FileRequestMode;

    #[test]
    fn rejected_request_restores_focus_and_does_not_abort_later_valid_request() {
        let mut app = App::new();
        app.init_resource::<InputFocus>()
            .init_resource::<StatusLine>()
            .insert_resource(crate::editor::test_editor("test song"))
            .add_message::<FileRequest>()
            .add_plugins(FileIoPlugin);

        let board_control = app.world_mut().spawn_empty().id();
        app.world_mut().resource_mut::<InputFocus>().clear();

        // No RegionEditor is installed, so the first request fails. The song
        // editor makes the second request valid in the same reader batch.
        app.world_mut().write_message(ActionRequest {
            action: ids::MENU_SESSION_SAVE,
            source: ctk::prelude::Source::Menu,
            args: Default::default(),
            invocation_focus: Some(board_control),
        });
        app.world_mut().write_message(ActionRequest {
            action: ids::MENU_SONG_OPEN,
            source: ctk::prelude::Source::Menu,
            args: Default::default(),
            invocation_focus: Some(board_control),
        });
        app.update();

        assert_eq!(
            app.world().resource::<InputFocus>().get(),
            Some(board_control)
        );
        assert!(app
            .world()
            .resource::<CaptureEstablishedThisFrame>()
            .is_marked());
        assert_eq!(
            app.world()
                .resource::<bevy::ecs::message::Messages<FileRequest>>()
                .len(),
            1
        );
    }

    #[test]
    fn requests_express_save_extension_policy() {
        let song = request_for(FileAction::SaveSong, Some(PathBuf::from("songs")));
        assert_eq!(song.mode, FileRequestMode::SaveFile);
        assert_eq!(song.initial_directory, Some(PathBuf::from("songs")));
        assert_eq!(song.default_extension.as_deref(), Some("json"));
        assert!(!song.enforce_extension);

        let wav = request_for(FileAction::ExportWav, None);
        assert_eq!(wav.default_extension.as_deref(), Some("wav"));
        assert!(wav.enforce_extension);

        let session = request_for(FileAction::SaveSession, None);
        assert_eq!(session.default_extension.as_deref(), Some("mix"));
        assert!(session.enforce_extension);

        let export = request_for(FileAction::ExportSessionFlac, None);
        assert_eq!(export.mode, FileRequestMode::SelectDirectory);
        assert!(export.filters.is_empty());
    }
}
