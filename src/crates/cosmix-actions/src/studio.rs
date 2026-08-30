//! Canonical Studio action ids shared by its checked-in keymap and Phase 3.
//!
//! Studio will import this module when its menus and closed action enum migrate
//! to `ActionId`; keeping the constants here makes the default keymap test
//! structural rather than maintaining another copied string list.

use crate::{ActionId, theme};

/// Toggle Studio transport playback.
pub const TRANSPORT_TOGGLE: ActionId = ActionId::from_static("transport.toggle");
/// Start Studio transport playback.
pub const TRANSPORT_START: ActionId = ActionId::from_static("transport.start");
/// Stop Studio transport playback and rewind to zero.
pub const TRANSPORT_STOP: ActionId = ActionId::from_static("transport.stop");
/// Pause Studio transport playback at its current position.
pub const TRANSPORT_PAUSE: ActionId = ActionId::from_static("transport.pause");
/// Open a song.
pub const MENU_SONG_OPEN: ActionId = ActionId::from_static("song-open");
/// Save a song under a chosen path.
pub const MENU_SONG_SAVE: ActionId = ActionId::from_static("song-save");
/// Save the full Studio session.
pub const MENU_SESSION_SAVE: ActionId = ActionId::from_static("session-save");
/// Export session audio as WAV.
pub const MENU_SESSION_EXPORT_WAV: ActionId = ActionId::from_static("session-export-wav");
/// Export session audio as FLAC.
pub const MENU_SESSION_EXPORT_FLAC: ActionId = ActionId::from_static("session-export-flac");
/// Open a SoundFont.
pub const MENU_SF_OPEN: ActionId = ActionId::from_static("sf-open");
/// Export a song render as WAV.
pub const MENU_WAV_EXPORT: ActionId = ActionId::from_static("wav-export");
/// Switch to the mixer view.
pub const MENU_VIEW_MIXER: ActionId = ActionId::from_static("view-mixer");
/// Switch to the waveform view.
pub const MENU_VIEW_WAVES: ActionId = ActionId::from_static("view-waves");
/// Switch to the piano-roll view.
pub const MENU_VIEW_ROLL: ActionId = ActionId::from_static("view-roll");
/// Zoom the waveform view in.
pub const MENU_ZOOM_IN: ActionId = ActionId::from_static("view-zoom-in");
/// Zoom the waveform view out.
pub const MENU_ZOOM_OUT: ActionId = ActionId::from_static("view-zoom-out");
/// Fit the waveform view to its content.
pub const MENU_ZOOM_FIT: ActionId = ActionId::from_static("view-zoom-fit");
/// Open Studio settings.
pub const MENU_SETTINGS: ActionId = ActionId::from_static("settings");
/// Close the active Studio settings modal.
pub const SETTINGS_CLOSE: ActionId = ActionId::from_static("settings.close");
/// Activate the focused control in the Studio settings modal.
pub const SETTINGS_ACTIVATE: ActionId = ActionId::from_static("settings.activate");

/// Every current Studio menu action, in menu declaration order.
pub const MENU_ACTION_IDS: [ActionId; 21] = [
    MENU_SONG_OPEN,
    MENU_SONG_SAVE,
    MENU_SF_OPEN,
    MENU_WAV_EXPORT,
    MENU_SESSION_SAVE,
    MENU_SESSION_EXPORT_WAV,
    MENU_SESSION_EXPORT_FLAC,
    MENU_SETTINGS,
    MENU_VIEW_MIXER,
    MENU_VIEW_WAVES,
    MENU_VIEW_ROLL,
    MENU_ZOOM_IN,
    MENU_ZOOM_OUT,
    MENU_ZOOM_FIT,
    theme::MODE_TOGGLE,
    theme::SCHEME_OCEAN,
    theme::SCHEME_CRIMSON,
    theme::SCHEME_STONE,
    theme::SCHEME_FOREST,
    theme::SCHEME_SUNSET,
    theme::SCHEME_MONO,
];

/// Global and modal Studio actions expected in the checked-in keymap asset.
pub const DEFAULT_KEYMAP_ACTION_IDS: [ActionId; 24] = [
    TRANSPORT_TOGGLE,
    MENU_SONG_OPEN,
    MENU_SONG_SAVE,
    MENU_SF_OPEN,
    MENU_WAV_EXPORT,
    MENU_SESSION_SAVE,
    MENU_SESSION_EXPORT_WAV,
    MENU_SESSION_EXPORT_FLAC,
    MENU_SETTINGS,
    MENU_VIEW_MIXER,
    MENU_VIEW_WAVES,
    MENU_VIEW_ROLL,
    MENU_ZOOM_IN,
    MENU_ZOOM_OUT,
    MENU_ZOOM_FIT,
    SETTINGS_CLOSE,
    SETTINGS_ACTIVATE,
    theme::MODE_TOGGLE,
    theme::SCHEME_OCEAN,
    theme::SCHEME_CRIMSON,
    theme::SCHEME_STONE,
    theme::SCHEME_FOREST,
    theme::SCHEME_SUNSET,
    theme::SCHEME_MONO,
];
