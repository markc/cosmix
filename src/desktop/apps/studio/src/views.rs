//! The shared menu bar + the three switchable views (Mixer / Waves / Piano
//! Roll), and the arranger-style waves display: per-channel
//! waveform lanes painted from LOD pyramids (`ctk::wave`), with a time ruler,
//! zoom-at-cursor (ctrl+wheel), horizontal scroll (wheel), and a playhead
//! following the same extrapolated transport clock as the scrubber.
//!
//! Song waveform renders run on worker threads with `mpsc` channels back to
//! poll systems, leaving the UI and audio threads untouched.

use std::collections::VecDeque;
use std::sync::mpsc::{channel, Receiver};
use std::sync::Mutex;

use bevy::color::Alpha;
use bevy::ecs::change_detection::DetectChanges;
use bevy::ecs::observer::On;
use bevy::ecs::schedule::IntoScheduleConfigs;
use bevy::ecs::system::SystemParam;
use bevy::image::Image;
use bevy::input::mouse::MouseWheel;
use bevy::input::ButtonInput;
use bevy::picking::events::{Cancel as PointerCancel, Click, Drag, DragEnd, DragStart, Pointer};
use bevy::prelude::{
    default, App, Assets, Color, Commands, Component, Display, Entity, KeyCode, MessageReader,
    MessageWriter, Messages, Node, Plugin, PositionType, Query, Res, ResMut, Resource, Text,
    TextColor, TextFont, Update, Window, With, Without,
};
use bevy::ui::widget::ImageNode;
use bevy::ui::{percent, px, ComputedNode, ComputedUiRenderTargetInfo, UiGlobalTransform, UiScale};
use bevy::window::PrimaryWindow;
use cosmix_actions::{studio as ids, theme as theme_ids, ActionId};
use cosmix_mixer_schema::{FADER_MAX_DB, FADER_MIN_DB, NUM_CHANNELS};
use cosmix_musicd::mixer::{Region, StemBank, SR};
use ctk::mixer::{TransportSeekGesture, TransportSeekGesturePhase};
use ctk::prelude::{
    channel_color, default_fader_mapping, hfader_sized, level_meter_sized, toggle_button_sized,
    transport_is_playing, ActionRequest, ControlMeta, ControlRange, FileRequest, Icon, MenuDef,
    MenuItemDef, MeterValue, MixerBinding, MixerMeterBinding, ModalCapture, MusicdMixerState,
    NumericControlProps, ThemeState, TransportPosition, TransportSeekRequest, WavePyramid,
    WaveRegion,
};
use ctk::wave::{
    format_ruler_secs, paint_region_lane, paint_ruler, ruler_ticks, RegionLanePaintParams,
};

use crate::action::{ActionRoute, BoardInputSystems, ConsumedShortcutInputs};
use crate::editor::SongEditor;
use crate::transport::{StemEditParts, StemWaves};

/// Canonical menu ids consumed by this module's action-bus reader.
pub(crate) const HANDLED_MENU_ACTION_IDS: &[ActionId] = &[
    ids::MENU_VIEW_MIXER,
    ids::MENU_VIEW_WAVES,
    ids::MENU_VIEW_ROLL,
    ids::MENU_ZOOM_IN,
    ids::MENU_ZOOM_OUT,
    ids::MENU_ZOOM_FIT,
];

#[derive(Clone, Copy)]
enum ViewMenuAction {
    Mixer,
    Waves,
    Roll,
    ZoomIn,
    ZoomOut,
    ZoomFit,
}

impl ViewMenuAction {
    fn from_action(action: ActionId) -> Option<Self> {
        match action {
            ids::MENU_VIEW_MIXER => Some(Self::Mixer),
            ids::MENU_VIEW_WAVES => Some(Self::Waves),
            ids::MENU_VIEW_ROLL => Some(Self::Roll),
            ids::MENU_ZOOM_IN => Some(Self::ZoomIn),
            ids::MENU_ZOOM_OUT => Some(Self::ZoomOut),
            ids::MENU_ZOOM_FIT => Some(Self::ZoomFit),
            _ => None,
        }
    }
}

pub(crate) fn handles_menu_action(action: ActionId) -> bool {
    let executable = ViewMenuAction::from_action(action).is_some();
    debug_assert_eq!(HANDLED_MENU_ACTION_IDS.contains(&action), executable);
    executable
}

/// The menus every view shares.
pub fn menu_defs() -> Vec<MenuDef> {
    vec![
        MenuDef {
            label: "File".into(),
            items: vec![
                MenuItemDef::new(ids::MENU_SONG_OPEN.as_str(), "Open Song...")
                    .with_icon(Icon::FolderOpen),
                MenuItemDef::new(ids::MENU_SONG_SAVE.as_str(), "Save Song As...")
                    .with_icon(Icon::FileMusic),
                MenuItemDef::new(ids::MENU_SF_OPEN.as_str(), "Open SoundFont...")
                    .with_icon(Icon::FolderOpen),
                MenuItemDef::new(ids::MENU_WAV_EXPORT.as_str(), "Export WAV...")
                    .with_icon(Icon::Download),
                MenuItemDef::new(ids::MENU_SESSION_SAVE.as_str(), "Save Session As...")
                    .with_icon(Icon::HardDrive),
                MenuItemDef::new(
                    ids::MENU_SESSION_EXPORT_WAV.as_str(),
                    "Export Session Audio (WAV)...",
                )
                .with_icon(Icon::Download),
                MenuItemDef::new(
                    ids::MENU_SESSION_EXPORT_FLAC.as_str(),
                    "Export Session Audio (FLAC)...",
                )
                .with_icon(Icon::Download),
                MenuItemDef::new(ids::MENU_SETTINGS.as_str(), "Settings").with_icon(Icon::Menu),
            ],
        },
        MenuDef {
            label: "View".into(),
            items: vec![
                MenuItemDef::new(ids::MENU_VIEW_MIXER.as_str(), "Mixer").with_icon(Icon::Grid),
                MenuItemDef::new(ids::MENU_VIEW_WAVES.as_str(), "Waves").with_icon(Icon::FileMusic),
                MenuItemDef::new(ids::MENU_VIEW_ROLL.as_str(), "Piano Roll").with_icon(Icon::Music),
                MenuItemDef::new(ids::MENU_ZOOM_IN.as_str(), "Zoom In").with_icon(Icon::Search),
                MenuItemDef::new(ids::MENU_ZOOM_OUT.as_str(), "Zoom Out").with_icon(Icon::Search),
                MenuItemDef::new(ids::MENU_ZOOM_FIT.as_str(), "Zoom Fit")
                    .with_icon(Icon::MoveHorizontal),
            ],
        },
        MenuDef {
            label: "Themes".into(),
            items: vec![
                MenuItemDef::new(theme_ids::MODE_TOGGLE.as_str(), "Dark Mode").with_icon(Icon::Eye),
                MenuItemDef::new(theme_ids::SCHEME_OCEAN.as_str(), "Ocean").with_icon(Icon::Grid),
                MenuItemDef::new(theme_ids::SCHEME_CRIMSON.as_str(), "Crimson")
                    .with_icon(Icon::Grid),
                MenuItemDef::new(theme_ids::SCHEME_STONE.as_str(), "Stone").with_icon(Icon::Grid),
                MenuItemDef::new(theme_ids::SCHEME_FOREST.as_str(), "Forest").with_icon(Icon::Grid),
                MenuItemDef::new(theme_ids::SCHEME_SUNSET.as_str(), "Sunset").with_icon(Icon::Grid),
                MenuItemDef::new(theme_ids::SCHEME_MONO.as_str(), "Mono").with_icon(Icon::Grid),
            ],
        },
    ]
}

/// Transient user-facing status: every save/load/export/error surfaces
/// here (the transport-bar message with a decay), not only on stderr.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StatusSeverity {
    Info,
    Ok,
    Error,
}

/// How long a status message stays visible.
const STATUS_DECAY: std::time::Duration = std::time::Duration::from_secs(4);

#[derive(Resource, Default)]
pub struct StatusLine {
    /// The transient toast on the top row: fades after [`STATUS_DECAY`].
    transient: Option<(StatusSeverity, String, std::time::Instant)>,
    /// The sticky last-event echo on the bottom (song-footer) row: persists
    /// until the next event, so activity stays glanceable after the toast fades.
    persistent: Option<(StatusSeverity, String)>,
}

impl StatusLine {
    pub fn set(&mut self, severity: StatusSeverity, message: impl Into<String>) {
        let message = message.into();
        self.persistent = Some((severity, message.clone()));
        self.transient = Some((severity, message, std::time::Instant::now()));
    }
    pub fn info(&mut self, message: impl Into<String>) {
        self.set(StatusSeverity::Info, message);
    }
    pub fn ok(&mut self, message: impl Into<String>) {
        self.set(StatusSeverity::Ok, message);
    }
    /// Errors also go to stderr so headless logs keep the full story.
    pub fn error(&mut self, message: impl Into<String>) {
        let message = message.into();
        eprintln!("studio: {message}");
        self.set(StatusSeverity::Error, message);
    }
}

/// The text colour for a severity — shared by the transient toast and the
/// persistent footer echo so the two rows read as one feedback channel.
fn severity_color(severity: StatusSeverity, theme: &bevy::feathers::theme::UiTheme) -> Color {
    match severity {
        StatusSeverity::Info => ctk::theme::ctk_color(theme, &ctk::theme::tokens::TEXT_DIM),
        StatusSeverity::Ok => ctk::theme::ctk_color(theme, &ctk::theme::tokens::METER_GREEN),
        StatusSeverity::Error => ctk::theme::ctk_color(theme, &ctk::theme::tokens::METER_RED),
    }
}

/// Marks the transient status message Text (sits beside the link status).
#[derive(Component)]
pub struct StatusMessageText;

/// Marks the persistent activity echo on the song-footer row (bottom).
#[derive(Component)]
pub struct ActivityText;

/// A one-shot startup message (what the session opened with), surfaced on
/// the status line once the UI exists, then removed.
#[derive(Resource)]
pub struct StartupStatus(pub String);

fn show_startup_status(
    startup: Option<Res<StartupStatus>>,
    mut status: ResMut<StatusLine>,
    mut commands: Commands,
) {
    if let Some(startup) = startup {
        status.info(startup.0.clone());
        commands.remove_resource::<StartupStatus>();
    }
}

/// Render + decay the transient status message (top row, beside the link).
fn update_status_line(
    mut status: ResMut<StatusLine>,
    theme: Res<bevy::feathers::theme::UiTheme>,
    mut texts: Query<(&mut Text, &mut TextColor), With<StatusMessageText>>,
) {
    let live = match &status.transient {
        Some((_, _, shown)) if shown.elapsed() < STATUS_DECAY => true,
        Some(_) => {
            status.transient = None;
            false
        }
        None => false,
    };
    for (mut text, mut color) in &mut texts {
        match (&status.transient, live) {
            (Some((severity, message, _)), true) => {
                if text.0 != *message {
                    text.0 = message.clone();
                }
                color.0 = severity_color(*severity, &theme);
            }
            _ => {
                if !text.0.is_empty() {
                    text.0.clear();
                }
            }
        }
    }
}

/// Render the persistent activity echo (bottom row). Unlike the toast it never
/// decays — the last event stays visible so the operator can always see what
/// studio (or a remote agent over Bus) most recently did. Dimmed a touch so it
/// reads as an echo, not a live alert.
fn update_activity_line(
    status: Res<StatusLine>,
    theme: Res<bevy::feathers::theme::UiTheme>,
    mut texts: Query<(&mut Text, &mut TextColor), With<ActivityText>>,
) {
    let Some((severity, message)) = &status.persistent else {
        return;
    };
    let want = severity_color(*severity, &theme).with_alpha(0.8);
    for (mut text, mut color) in &mut texts {
        if text.0 != *message {
            text.0 = message.clone();
        }
        if color.0 != want {
            color.0 = want;
        }
    }
}

/// Which of the three content views is showing.
#[derive(Resource, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ActiveView {
    #[default]
    Mixer,
    Waves,
    PianoRoll,
}

/// Marks a view's root container; [`apply_view`] flips `Display` on these.
#[derive(Component)]
pub struct ViewRoot(pub ActiveView);

/// Marks the waves view container the arranger structure spawns into.
#[derive(Component)]
pub struct WavesContainer;

/// Marks any entity of the spawned arranger structure (despawned wholesale on
/// a lane-set change).
#[derive(Component)]
struct WavesLane;

/// The ruler row (tick texture + labels live under it).
#[derive(Component)]
struct WavesRulerRow;

/// Present from ruler DragStart through DragEnd/Cancel. It also suppresses the
/// Click Bevy emits on release after a drag, keeping click and gesture seeks
/// mutually exclusive.
#[derive(Component)]
struct RulerDragActive;

/// The ruler's tick-texture image node.
#[derive(Component)]
struct WavesRulerImage;

/// One major-tick time label on the ruler.
#[derive(Component)]
struct WavesRulerLabel;

/// The lanes body — the zoom/scroll viewport all lane rows stack inside.
#[derive(Component)]
struct WavesBody;

/// One lane's waveform image node; the index into [`WaveLanes::lanes`].
#[derive(Component)]
struct WavesLaneImage(usize);

/// The arranger playhead line.
#[derive(Component)]
struct WavesPlayhead;

/// The draggable knob riding the top of the playhead (in the ruler row); drag
/// it to scrub the transport, like the footer scrubber's handle.
#[derive(Component)]
struct WavesPlayheadKnob;

/// The "rendering waves" overlay (covers the container while a song re-render
/// is in flight) and its spinning glyph.
#[derive(Component)]
pub struct WavesSpinner;
#[derive(Component)]
pub struct WavesSpinnerGlyph;

/// Knob width (px); half of it centres the knob on the playhead line.
const PLAYHEAD_KNOB_PX: f32 = 12.0;

/// The vertically scrolling row holding the header column + canvas.
#[derive(Component)]
struct WavesScroll;

/// A header cell or canvas row belonging to lane `.0` — height synced from
/// [`Arranger::lane_heights`].
#[derive(Component)]
struct LaneRowIndex(usize);

/// The drag strip at the bottom of a header cell that resizes lane `.0`.
#[derive(Component)]
struct LaneResizeHandle(usize);

/// Original logical height while a lane-resize pointer gesture is retained.
/// Kept on the handle so modal capture can restore an in-flight resize rather
/// than persisting the last previewed height.
#[derive(Component)]
struct LaneResizeActive(f32);

/// One arranger lane: mixer channel (colour), display name, LOD pyramid, and
/// the lane's region list (the engine's non-destructive document, converted
/// to ctk's display type).
pub struct WaveLane {
    channel: u32,
    name: String,
    pyramid: WavePyramid,
    regions: Vec<WaveRegion>,
}

/// Engine region → display region (ctk stays engine-crate-free).
fn to_wave_region(region: &cosmix_musicd::mixer::Region) -> WaveRegion {
    WaveRegion {
        timeline_start: region.timeline_start,
        source_start: region.source_start,
        len: region.len,
        gain: region.gain,
        fade_in: region.fade_in,
        fade_out: region.fade_out,
        selected: false,
    }
}

/// The arranger's waveform sources. Present from startup on a `--stems`
/// launch; (re)inserted by the song render worker on `--song` launches.
#[derive(Resource)]
pub struct WaveLanes {
    lanes: Vec<WaveLane>,
    length_frames: u64,
    /// Bumped only when the LANE SET changes (fresh session / re-render) —
    /// the structure system keys on this, so region edits (which mutate
    /// `lanes[*].regions` in place) repaint without a structure rebuild
    /// that would despawn headers and clear the selection.
    structure_rev: u32,
}

impl WaveLanes {
    pub fn from_stems(waves: StemWaves) -> Self {
        Self {
            lanes: waves
                .lanes
                .into_iter()
                .map(|lane| WaveLane {
                    channel: lane.channel as u32,
                    name: lane
                        .name
                        .unwrap_or_else(|| format!("Ch {}", lane.channel + 1)),
                    regions: lane.regions.iter().map(to_wave_region).collect(),
                    pyramid: lane.pyramid,
                })
                .collect(),
            length_frames: waves.length_frames,
            structure_rev: 0,
        }
    }
}

/// A menu-issued zoom step, consumed by the paint system (the only place
/// the canvas width is known).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ZoomCmd {
    In,
    Out,
    Fit,
}

/// Arranger viewport state. `frames_per_px == 0` means "not yet laid out" —
/// the first paint fits the whole session to the body width.
#[derive(Resource, Default)]
struct Arranger {
    frames_per_px: f64,
    scroll_frame: f64,
    painted: Option<PaintKey>,
    pending_zoom: VecDeque<ZoomCmd>,
    /// Per-lane row heights (logical px), drag-adjustable via the handle at
    /// the bottom of each header cell.
    lane_heights: Vec<f32>,
    /// The selected region as (lane index, region index), if any.
    selected: Option<(usize, usize)>,
    /// A single lane whose texture needs repainting (drag preview) without
    /// invalidating the whole viewport paint.
    dirty_lane: Option<usize>,
}

/// What the last texture paint was keyed on; any change repaints.
#[derive(Clone, Copy, PartialEq)]
struct PaintKey {
    fpp_bits: u64,
    scroll_bits: u64,
    width: u32,
    lanes: usize,
    selected: Option<(usize, usize)>,
    theme_revision: u64,
}

/// Channel identity retained beside a lane-header name so a theme revision can
/// rederive its tint without rebuilding the lane controls or textures.
#[derive(Component)]
struct LaneHeaderName(u32);

/// Maximum zoom-in: level 0 of the pyramid, 8 frames per pixel column
/// (6000 px per second at 48 kHz — well past arranger territory).
const MIN_FRAMES_PER_PX: f64 = 8.0;
/// Ruler strip height.
const RULER_PX: u32 = 18;
/// The track-header column width (name, M/S, gain, meter — Ardour-style).
const HEADER_PX: f32 = 172.0;
/// Default / minimum / maximum lane row height (logical px). The minimum
/// keeps the header's name + M/S/fader row visible above the 5px resize
/// strip (24px would clip the controls and leave them shadowed by it).
const LANE_H_DEFAULT: f32 = 64.0;
const LANE_H_MIN: f32 = 44.0;
const LANE_H_MAX: f32 = 240.0;
/// Vertical wheel scroll per line, in logical px.
const VSCROLL_PX_PER_LINE: f32 = 40.0;
/// Zoom step for the View-menu items.
const MENU_ZOOM_STEP: f64 = 1.5;
/// Zoom factor per wheel line with ctrl held.
const ZOOM_PER_LINE: f64 = 1.25;
/// Horizontal scroll per wheel line, in pixels of the current view.
const SCROLL_PX_PER_LINE: f64 = 60.0;

/// Song-render cache state (`--song` launches only). `rendered_rev` /
/// `pending_rev` track the editor's document revision so edits invalidate the
/// lanes; the worker sends back ready-folded pyramids.
#[derive(Resource, Default)]
struct Waves {
    rendered_rev: Option<u64>,
    pending_rev: Option<u64>,
    rx: Option<Mutex<Receiver<Vec<WaveLane>>>>,
}

/// Vertical resolution of a lane texture; the UI scales it to the lane node.
const LANE_PX: u32 = 96;

pub struct ViewsPlugin;

impl Plugin for ViewsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ActiveView>()
            .add_message::<ActionRequest>()
            .init_resource::<ConsumedShortcutInputs>()
            .init_resource::<Waves>()
            .init_resource::<Arranger>()
            .init_resource::<EditDrag>()
            .init_resource::<StatusLine>()
            .init_resource::<ExportJob>()
            .add_observer(on_ruler_drag_cancel)
            .add_systems(Update, on_menu.in_set(ActionRoute))
            .add_systems(
                Update,
                cancel_board_gestures_on_modal.after(ctk::prelude::FileRequesterSystems),
            )
            .add_systems(
                Update,
                (
                    apply_view,
                    waves_kick,
                    waves_spinner,
                    restyle_lane_header_names,
                    update_status_line,
                    update_activity_line,
                    show_startup_status,
                    poll_export_job,
                    export_cancel_key.in_set(BoardInputSystems),
                ),
            )
            // Chained: receive publishes the lane resource (deferred insert),
            // structure replaces the spawned nodes (deferred despawn/spawn),
            // and paint caches a key against the CURRENT nodes — each step
            // must see the previous one's commands applied, or paint records
            // its key against nodes that are about to be replaced and the
            // fresh ones stay blank until the next viewport change.
            .add_systems(
                Update,
                (
                    waves_receive,
                    arranger_structure,
                    // This ingestion system must run while captured so its
                    // system-local wheel cursor advances; it owns its guard.
                    arranger_input.after(ctk::prelude::ModalCaptureSystems),
                    arranger_edit_keys.in_set(BoardInputSystems),
                    service_region_swap,
                    apply_lane_heights,
                    arranger_paint,
                    arranger_playhead,
                )
                    .chain(),
            );
    }
}

/// Eagerly abandon retained board gestures as soon as modal capture becomes
/// authoritative. Pointer observers also self-abort: this scheduled cleanup
/// releases idle retained gestures promptly, but cannot by itself be ordered
/// ahead of every reactive `Drag` / `DragEnd` observer in the opening frame.
// Bevy systems expose each independently borrowed resource/query as a parameter.
#[allow(clippy::too_many_arguments)]
fn cancel_board_gestures_on_modal(
    capture: Res<ModalCapture>,
    editor: Option<Res<RegionEditor>>,
    lanes: Option<ResMut<WaveLanes>>,
    mut arranger: ResMut<Arranger>,
    mut edit_drag: ResMut<EditDrag>,
    active_resizes: Query<(Entity, &LaneResizeHandle, &LaneResizeActive)>,
    active_rulers: Query<Entity, With<RulerDragActive>>,
    mut commands: Commands,
) {
    if !capture.is_captured() {
        return;
    }

    if let (Some(editor), Some(mut lanes)) = (editor, lanes) {
        cancel_region_drag(&mut edit_drag, &editor, &mut lanes, &mut arranger);
    } else {
        // An edit drag only exists when both resources exist. Clear a stale
        // marker defensively if either was removed during teardown.
        edit_drag.0 = None;
    }

    for (entity, handle, active) in &active_resizes {
        restore_lane_resize(handle, active, &mut arranger);
        commands.entity(entity).remove::<LaneResizeActive>();
    }

    for source in &active_rulers {
        commands.entity(source).remove::<RulerDragActive>();
        commands.trigger(TransportSeekGesture {
            source,
            phase: TransportSeekGesturePhase::Cancel,
        });
    }
}

/// Pointer observers run reactively and can see a request after its producer
/// has written the message but before `FileRequesterSystems` ingests it.
/// Treat that pending request as capture so a retained `DragEnd` cannot commit
/// in the narrow pre-ingestion window.
fn board_pointer_input_captured(capture: &ModalCapture, requests: &Messages<FileRequest>) -> bool {
    capture.is_captured() || !requests.is_empty()
}

/// Flip view containers when [`ActiveView`] changes (and once at startup).
fn apply_view(active: Res<ActiveView>, mut views: Query<(&ViewRoot, &mut Node)>) {
    if !active.is_changed() {
        return;
    }
    for (root, mut node) in &mut views {
        node.display = if root.0 == *active {
            Display::Flex
        } else {
            Display::None
        };
    }
}

/// Dispatch view choices. File choices are handled by `file_io` so CTK path
/// selection stays separate from Studio's document operations.
fn on_menu(
    mut requests: MessageReader<ActionRequest>,
    mut active: ResMut<ActiveView>,
    mut arranger: ResMut<Arranger>,
    editor: Option<Res<SongEditor>>,
    lanes: Option<Res<WaveLanes>>,
    mut status: ResMut<StatusLine>,
) {
    for request in requests.read() {
        if !handles_menu_action(request.action) {
            continue;
        }
        let action = ViewMenuAction::from_action(request.action)
            .expect("handled view-menu action must have an executable arm");
        match action {
            ViewMenuAction::Mixer => *active = ActiveView::Mixer,
            ViewMenuAction::Waves => {
                // Stems carry their lanes from startup; songs render them lazily.
                if editor.is_none() && lanes.is_none() {
                    status.error("the waves view needs a --stems or --song session");
                    return;
                }
                *active = ActiveView::Waves;
            }
            ViewMenuAction::Roll => {
                if editor.is_none() {
                    status.error("the piano roll needs a --song session");
                    return;
                }
                *active = ActiveView::PianoRoll;
            }
            ViewMenuAction::ZoomIn | ViewMenuAction::ZoomOut | ViewMenuAction::ZoomFit => {
                if *active != ActiveView::Waves {
                    status.info("zoom applies to the waves view");
                    return;
                }
                arranger.pending_zoom.push_back(match action {
                    ViewMenuAction::ZoomIn => ZoomCmd::In,
                    ViewMenuAction::ZoomOut => ZoomCmd::Out,
                    ViewMenuAction::ZoomFit => ZoomCmd::Fit,
                    _ => unreachable!("only zoom actions enter this arm"),
                });
            }
        }
    }
}

/// Start a waveform render when the waves view is showing and its cache is
/// stale (song edited, soundfont swapped, or never rendered).
fn waves_kick(active: Res<ActiveView>, editor: Option<Res<SongEditor>>, mut waves: ResMut<Waves>) {
    if *active != ActiveView::Waves || waves.rx.is_some() {
        return;
    }
    let Some(editor) = editor else { return };
    if waves.rendered_rev == Some(editor.revision()) {
        return;
    }
    // No soundfont = silent tracks: nothing to render, and re-kicking every
    // frame would spin — mark the revision rendered with empty lanes.
    let Some(sf) = editor.soundfont().cloned() else {
        waves.rendered_rev = Some(editor.revision());
        return;
    };
    let song = editor.song().clone();
    let (tx, rx) = channel();
    waves.pending_rev = Some(editor.revision());
    waves.rx = Some(Mutex::new(rx));
    std::thread::spawn(move || {
        let lanes = cosmix_musicd::mixer_host::render_song_channels(&song, &sf)
            .map(|tracks| {
                tracks
                    .into_iter()
                    .map(|(channel, name, samples)| WaveLane {
                        channel: channel as u32,
                        name,
                        // Rendered song channels are un-editable in v1
                        // (stems-only scope): one full-length region.
                        regions: vec![WaveRegion {
                            timeline_start: 0,
                            source_start: 0,
                            len: samples.len() as u64,
                            gain: 1.0,
                            fade_in: 0,
                            fade_out: 0,
                            selected: false,
                        }],
                        pyramid: WavePyramid::new(&samples),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let _ = tx.send(lanes);
    });
}

/// Receive the song worker's finished pyramids and publish them as the
/// arranger's lane set (the structure/paint systems react to the resource).
fn waves_receive(
    mut waves: ResMut<Waves>,
    previous: Option<Res<WaveLanes>>,
    mut commands: Commands,
) {
    let Some(rx) = &waves.rx else { return };
    let received = rx.lock().expect("waves channel poisoned").try_recv();
    let Ok(lanes) = received else { return };
    waves.rx = None;
    waves.rendered_rev = waves.pending_rev.take();

    let length_frames = lanes
        .iter()
        .map(|lane| lane.pyramid.len_frames() as u64)
        .max()
        .unwrap_or(0);
    commands.insert_resource(WaveLanes {
        lanes,
        length_frames,
        structure_rev: previous.map_or(0, |p| p.structure_rev.wrapping_add(1)),
    });
}

/// (Re)build the arranger structure — ruler row, lane rows, playhead — when
/// the lane set appears or changes. Textures are painted separately.
// Bevy systems expose each independently borrowed resource/query as a parameter.
#[allow(clippy::too_many_arguments)]
fn arranger_structure(
    lanes: Option<Res<WaveLanes>>,
    theme: Res<bevy::feathers::theme::UiTheme>,
    mut arranger: ResMut<Arranger>,
    containers: Query<Entity, With<WavesContainer>>,
    old: Query<Entity, With<WavesLane>>,
    active_rulers: Query<Entity, (With<WavesRulerRow>, With<RulerDragActive>)>,
    old_scroll: Query<&bevy::ui::ScrollPosition, With<WavesScroll>>,
    mut built_rev: bevy::prelude::Local<Option<u32>>,
    mut commands: Commands,
) {
    let Some(lanes) = lanes else { return };
    // Keyed on the STRUCTURE revision, not plain change detection: region
    // edits mutate the resource every gesture and must not despawn the view.
    if *built_rev == Some(lanes.structure_rev) {
        return;
    }
    *built_rev = Some(lanes.structure_rev);
    let accent = ctk::theme::ctk_color(&theme, &ctk::theme::tokens::CONTROL_ACTIVE);
    // A rebuild (song re-render) must not jump the lane stack back to the
    // top — carry the vertical scroll across, like zoom/scroll/heights.
    let saved_scroll = old_scroll
        .iter()
        .next()
        .map(|scroll| scroll.0)
        .unwrap_or_default();
    // The ruler is rebuilt with the lane structure. Cancel an in-progress
    // gesture before its owner entity disappears or CTK ownership would leak.
    for source in &active_rulers {
        commands.trigger(TransportSeekGesture {
            source,
            phase: TransportSeekGesturePhase::Cancel,
        });
    }
    for entity in &old {
        commands.entity(entity).despawn();
    }
    let Some(container) = containers.iter().next() else {
        return;
    };
    // Force a repaint; keep the current zoom/scroll (a song re-render after an
    // edit shouldn't jump the viewport). Lane heights persist across rebuilds
    // of the same lane set; a different lane count resets them. Selection
    // indices belong to the OLD lane set — always cleared.
    arranger.painted = None;
    arranger.selected = None;
    if arranger.lane_heights.len() != lanes.lanes.len() {
        arranger.lane_heights = vec![LANE_H_DEFAULT; lanes.lanes.len()];
    }

    commands.entity(container).with_children(|parent| {
        // Top row: a corner spacer over the header column, then the ruler —
        // the ruler spans ONLY the canvas, so its click/paint math and the
        // canvas viewport share one x-axis by construction.
        parent
            .spawn((
                Node {
                    flex_direction: bevy::ui::FlexDirection::Row,
                    width: percent(100),
                    height: px(RULER_PX as f32),
                    flex_shrink: 0.0,
                    margin: bevy::ui::UiRect::bottom(px(1)),
                    ..default()
                },
                WavesLane,
            ))
            .with_children(|top| {
                top.spawn(Node {
                    width: px(HEADER_PX),
                    flex_shrink: 0.0,
                    height: percent(100),
                    ..default()
                });
                top.spawn((
                    Node {
                        position_type: PositionType::Relative,
                        flex_grow: 1.0,
                        height: percent(100),
                        ..default()
                    },
                    WavesRulerRow,
                ))
                .observe(on_ruler_click)
                .observe(on_ruler_drag_start)
                .observe(on_ruler_drag)
                .observe(on_ruler_drag_end)
                .with_children(|ruler| {
                    ruler.spawn((
                        Node {
                            position_type: PositionType::Absolute,
                            left: px(0),
                            top: px(0),
                            width: percent(100),
                            height: percent(100),
                            ..default()
                        },
                        ImageNode::default().with_mode(bevy::ui::widget::NodeImageMode::Stretch),
                        WavesRulerImage,
                    ));
                    // The scrub-knob handle — a small tab flush with the bottom
                    // of the ruler so it joins the playhead line below it,
                    // riding the playhead x (positioned by `arranger_playhead`),
                    // hidden until the transport has a base. Click-through
                    // (`IGNORE`): the ruler row owns the drag/seek, so the tab
                    // never blocks a scrub and can't obscure the ruler ticks.
                    ruler.spawn((
                        Node {
                            position_type: PositionType::Absolute,
                            bottom: px(0),
                            width: px(PLAYHEAD_KNOB_PX),
                            height: px((RULER_PX / 2) as f32),
                            display: Display::None,
                            border_radius: bevy::ui::BorderRadius::top(px(2)),
                            ..default()
                        },
                        bevy::feathers::theme::ThemeBackgroundColor(
                            ctk::theme::tokens::CONTROL_ACTIVE,
                        ),
                        bevy::picking::Pickable::IGNORE,
                        WavesPlayheadKnob,
                    ));
                });
            });
        // Body: a vertically scrolling row holding the header column and the
        // wave canvas side by side — one scroll offset keeps them in
        // register. FlexStart lets the columns take their CONTENT height
        // (the sum of lane heights) so tall sessions actually overflow.
        parent
            .spawn((
                Node {
                    flex_direction: bevy::ui::FlexDirection::Row,
                    align_items: bevy::ui::AlignItems::FlexStart,
                    width: percent(100),
                    flex_grow: 1.0,
                    min_height: px(0),
                    overflow: bevy::ui::Overflow::scroll_y(),
                    ..default()
                },
                bevy::ui::ScrollPosition(saved_scroll),
                WavesLane,
                WavesScroll,
            ))
            .with_children(|body_row| {
                body_row
                    .spawn(Node {
                        flex_direction: bevy::ui::FlexDirection::Column,
                        width: px(HEADER_PX),
                        flex_shrink: 0.0,
                        ..default()
                    })
                    .with_children(|headers| {
                        for (index, lane) in lanes.lanes.iter().enumerate() {
                            spawn_lane_header(headers, lane, index, &arranger, accent);
                        }
                    });
                body_row
                    .spawn((
                        Node {
                            position_type: PositionType::Relative,
                            flex_direction: bevy::ui::FlexDirection::Column,
                            flex_grow: 1.0,
                            overflow: bevy::ui::Overflow::clip(),
                            ..default()
                        },
                        WavesBody,
                    ))
                    .with_children(|body| {
                        for (index, _lane) in lanes.lanes.iter().enumerate() {
                            body.spawn((
                                Node {
                                    position_type: PositionType::Relative,
                                    width: percent(100),
                                    height: px(arranger.lane_heights[index]),
                                    flex_shrink: 0.0,
                                    margin: bevy::ui::UiRect::bottom(px(1)),
                                    ..default()
                                },
                                LaneRowIndex(index),
                            ))
                            .observe(on_lane_click)
                            .observe(on_lane_drag_start)
                            .observe(on_lane_drag)
                            .observe(on_lane_drag_end)
                            .with_child((
                                Node {
                                    position_type: PositionType::Absolute,
                                    left: px(0),
                                    top: px(0),
                                    width: percent(100),
                                    height: percent(100),
                                    ..default()
                                },
                                ImageNode::default()
                                    .with_mode(bevy::ui::widget::NodeImageMode::Stretch),
                                WavesLaneImage(index),
                            ));
                        }
                        // Last child = drawn on top of every lane, spanning
                        // the full content height (it scrolls with the
                        // lanes). Picking-transparent so a click exactly on
                        // the line still selects the region under it.
                        body.spawn((
                            Node {
                                position_type: PositionType::Absolute,
                                left: px(0),
                                top: px(0),
                                width: px(1),
                                height: percent(100),
                                ..default()
                            },
                            bevy::feathers::theme::ThemeBackgroundColor(
                                ctk::theme::tokens::CONTROL_ACTIVE,
                            ),
                            bevy::picking::Pickable::IGNORE,
                            WavesPlayhead,
                        ));
                    });
            });
    });
}

/// One Ardour-style track header cell: coloured name, M/S, a compact gain
/// fader, and the channel's live meter. Every control binds the SAME
/// revisioned `mixer.channels.N.*` leaves the strips use, so header and
/// mixer views stay in lockstep; freshly spawned bindings are seeded from
/// authoritative state by ctk (`seed_added_bindings`).
fn spawn_lane_header(
    headers: &mut bevy::ecs::relationship::RelatedSpawnerCommands<'_, bevy::prelude::ChildOf>,
    lane: &WaveLane,
    index: usize,
    arranger: &Arranger,
    accent: Color,
) {
    let base = format!("mixer.channels.{}", lane.channel);
    headers
        .spawn((
            Node {
                position_type: PositionType::Relative,
                flex_direction: bevy::ui::FlexDirection::Row,
                width: percent(100),
                // Fixed height, identical to the canvas row — kept in sync by
                // apply_lane_heights.
                height: px(arranger.lane_heights[index]),
                flex_shrink: 0.0,
                margin: bevy::ui::UiRect::bottom(px(1)),
                padding: bevy::ui::UiRect::axes(px(6), px(3)),
                column_gap: px(6),
                align_items: bevy::ui::AlignItems::Stretch,
                overflow: bevy::ui::Overflow::clip(),
                ..default()
            },
            bevy::feathers::theme::ThemeBackgroundColor(ctk::theme::tokens::PANEL),
            LaneRowIndex(index),
        ))
        .with_children(|cell| {
            cell.spawn(Node {
                flex_direction: bevy::ui::FlexDirection::Column,
                flex_grow: 1.0,
                min_width: px(0),
                row_gap: px(3),
                ..default()
            })
            .with_children(|column| {
                column.spawn((
                    Text::new(lane.name.clone()),
                    TextFont::from_font_size(11.0),
                    TextColor(channel_color(lane.channel, accent)),
                    LaneHeaderName(lane.channel),
                    // Single line, clipped by the cell — a wrapping name must
                    // not change this cell's intrinsic height.
                    bevy::text::TextLayout {
                        linebreak: bevy::text::LineBreak::NoWrap,
                        ..default()
                    },
                ));
                column
                    .spawn(Node {
                        flex_direction: bevy::ui::FlexDirection::Row,
                        column_gap: px(4),
                        align_items: bevy::ui::AlignItems::Center,
                        ..default()
                    })
                    .with_children(|controls| {
                        for (label, leaf) in [("M", "mute"), ("S", "solo")] {
                            controls
                                .spawn((
                                    toggle_button_sized(
                                        format!("lane-{}-{leaf}", lane.channel),
                                        20.0,
                                        15.0,
                                    ),
                                    MixerBinding::boolean(format!("{base}.{leaf}")),
                                ))
                                .with_child((
                                    Text::new(label),
                                    TextFont::from_font_size(9.0),
                                    bevy::feathers::theme::ThemeTextColor(ctk::theme::tokens::TEXT),
                                ));
                        }
                        controls.spawn((
                            hfader_sized(
                                NumericControlProps::new(
                                    format!("lane-{}-gain", lane.channel),
                                    0.0,
                                    ControlRange {
                                        min: FADER_MIN_DB as f32,
                                        max: FADER_MAX_DB as f32,
                                        step: 0.1,
                                        detent: Some(0.0),
                                    },
                                    default_fader_mapping(),
                                ),
                                72.0,
                                15.0,
                            ),
                            MixerBinding::number(format!("{base}.fader")),
                            ControlMeta::unit("dB"),
                        ));
                    });
            });
            cell.spawn((
                level_meter_sized(
                    format!("lane-{}-meter", lane.channel),
                    MeterValue::default(),
                    10.0,
                    34.0,
                ),
                MixerMeterBinding(lane.channel as usize),
            ));
            // Bottom-edge drag strip: resizes this lane (header + waveform
            // row together, via apply_lane_heights).
            cell.spawn((
                Node {
                    position_type: PositionType::Absolute,
                    left: px(0),
                    bottom: px(0),
                    width: percent(100),
                    height: px(5),
                    ..default()
                },
                LaneResizeHandle(index),
            ))
            .observe(on_lane_resize_start)
            .observe(on_lane_resize)
            .observe(on_lane_resize_end);
        });
}

fn restyle_lane_header_names(
    theme: Res<bevy::feathers::theme::UiTheme>,
    theme_state: Res<ThemeState>,
    mut applied_revision: bevy::prelude::Local<Option<u64>>,
    mut names: Query<(&LaneHeaderName, &mut TextColor)>,
) {
    if *applied_revision == Some(theme_state.revision) {
        return;
    }
    *applied_revision = Some(theme_state.revision);
    let accent = ctk::theme::ctk_color(&theme, &ctk::theme::tokens::CONTROL_ACTIVE);
    for (name, mut colour) in &mut names {
        colour.0 = channel_color(name.0, accent);
    }
}

/// Click a lane to select the region under the pointer (topmost = last of
/// the sorted overlaps), or clear the selection on empty timeline. R2 edit
/// gestures will act on this selection.
fn on_lane_click(
    click: On<Pointer<Click>>,
    capture: Res<ModalCapture>,
    requests: Res<Messages<FileRequest>>,
    rows: Query<(
        &LaneRowIndex,
        &ComputedNode,
        &ComputedUiRenderTargetInfo,
        &UiGlobalTransform,
    )>,
    lanes: Option<Res<WaveLanes>>,
    mut arranger: ResMut<Arranger>,
    ui_scale: Res<UiScale>,
) {
    if board_pointer_input_captured(&capture, &requests) {
        return;
    }
    let Ok((row, computed, target, transform)) = rows.get(click.entity) else {
        return;
    };
    let Some(lanes) = lanes else { return };
    let Some(lane) = lanes.lanes.get(row.0) else {
        return;
    };
    if arranger.frames_per_px <= 0.0 {
        return;
    }
    let Some(normalised) = computed.normalize_point(
        *transform,
        click.pointer_location.position * target.scale_factor() / ui_scale.0,
    ) else {
        return;
    };
    let fraction = (f64::from(normalised.x) + 0.5).clamp(0.0, 1.0);
    let width = f64::from(computed.size().x * computed.inverse_scale_factor());
    let frame = (arranger.scroll_frame + fraction * width * arranger.frames_per_px).max(0.0) as u64;
    let hit = lane
        .regions
        .iter()
        .enumerate()
        .rev()
        .find(|(_, region)| region.timeline_start <= frame && frame < region.timeline_end())
        .map(|(region_idx, _)| (row.0, region_idx));
    if arranger.selected != hit {
        arranger.selected = hit;
    }
}

/// Paint one lane's region texture (selection flag applied to a per-paint
/// copy of its few regions) and hand the image to its node.
#[allow(clippy::too_many_arguments)]
fn repaint_lane_image(
    lane: &WaveLane,
    lane_index: usize,
    selected: Option<(usize, usize)>,
    scroll_frame: f64,
    frames_per_px: f64,
    width_px: u32,
    bg: Color,
    wave_color: Color,
    images: &mut Assets<Image>,
    node: &mut ImageNode,
) {
    let mut regions = lane.regions.clone();
    if let Some((sel_lane, sel_region)) = selected {
        if sel_lane == lane_index {
            if let Some(region) = regions.get_mut(sel_region) {
                region.selected = true;
            }
        }
    }
    node.image = images.add(paint_region_lane(RegionLanePaintParams {
        pyramid: &lane.pyramid,
        regions: &regions,
        start_frame: scroll_frame,
        frames_per_px,
        width: width_px,
        height: LANE_PX,
        color: wave_color,
        background: bg,
    }));
}

/// Region x-span in lane pixels under the current viewport.
fn region_px_span(region: &Region, arranger: &Arranger) -> (f64, f64) {
    let x0 = (region.timeline_start as f64 - arranger.scroll_frame) / arranger.frames_per_px;
    let x1 = ((region.timeline_start + region.len) as f64 - arranger.scroll_frame)
        / arranger.frames_per_px;
    (x0, x1)
}

/// Begin a region drag: resolve the region + handle under the pointer
/// (topmost of the sorted overlaps; ±8px around an edge grabs a trim
/// handle, alt turns a body drag into a slip) and select it.
// Bevy observers expose event context and each independent ECS borrow separately.
#[allow(clippy::too_many_arguments)]
fn on_lane_drag_start(
    drag: On<Pointer<bevy::picking::events::DragStart>>,
    capture: Res<ModalCapture>,
    requests: Res<Messages<FileRequest>>,
    rows: Query<(
        &LaneRowIndex,
        &ComputedNode,
        &ComputedUiRenderTargetInfo,
        &UiGlobalTransform,
    )>,
    lanes: Option<Res<WaveLanes>>,
    editor: Option<Res<RegionEditor>>,
    keys: Res<ButtonInput<KeyCode>>,
    mut arranger: ResMut<Arranger>,
    mut edit_drag: ResMut<EditDrag>,
    ui_scale: Res<UiScale>,
) {
    if board_pointer_input_captured(&capture, &requests) {
        return;
    }
    let Ok((row, computed, target, transform)) = rows.get(drag.entity) else {
        return;
    };
    let (Some(lanes), Some(editor)) = (lanes, editor) else {
        return;
    };
    let Some(lane) = lanes.lanes.get(row.0) else {
        return;
    };
    if arranger.frames_per_px <= 0.0 {
        return;
    }
    let Some(normalised) = computed.normalize_point(
        *transform,
        drag.pointer_location.position * target.scale_factor() / ui_scale.0,
    ) else {
        return;
    };
    let width = f64::from(computed.size().x * computed.inverse_scale_factor());
    let pointer_px = (f64::from(normalised.x) + 0.5).clamp(0.0, 1.0) * width;

    let channel = lane.channel as usize;
    let alt = keys.pressed(KeyCode::AltLeft) || keys.pressed(KeyCode::AltRight);
    let ctrl = keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight);
    // Vertical fraction inside the lane row: the top strip of an edge grab
    // is the fade corner.
    let pointer_fy = (normalised.y + 0.5).clamp(0.0, 1.0);
    let hit = editor.document[channel]
        .iter()
        .enumerate()
        .rev()
        .find_map(|(idx, region)| {
            let (x0, x1) = region_px_span(region, &arranger);
            // Narrow regions shrink their trim zones (a quarter each) so the
            // body stays grabbable for move/slip at any zoom.
            let grab = EDGE_GRAB_PX.min((x1 - x0) / 4.0).max(0.0);
            if pointer_px < x0 - grab || pointer_px > x1 + grab {
                return None;
            }
            let top = pointer_fy < FADE_CORNER_FRACTION;
            let mode = if (pointer_px - x0).abs() <= grab {
                if top {
                    DragMode::FadeIn
                } else {
                    DragMode::TrimStart
                }
            } else if (pointer_px - x1).abs() <= grab {
                if top {
                    DragMode::FadeOut
                } else {
                    DragMode::TrimEnd
                }
            } else if ctrl {
                DragMode::Gain
            } else if alt {
                DragMode::Slip
            } else {
                DragMode::Move
            };
            Some((idx, *region, mode))
        });
    let Some((region_idx, origin, mode)) = hit else {
        edit_drag.0 = None;
        return;
    };
    arranger.selected = Some((row.0, region_idx));
    edit_drag.0 = Some(DragOp {
        lane: row.0,
        channel,
        region_idx,
        mode,
        origin,
        preview: origin,
    });
}

/// Live drag preview: gesture applied to the DISPLAY document only (the
/// engine hears the edit on release), snapped to the visible ruler grid
/// unless Shift is held.
// Bevy observers expose event context and each independent ECS borrow separately.
#[allow(clippy::too_many_arguments)]
fn on_lane_drag(
    drag: On<Pointer<bevy::picking::events::Drag>>,
    capture: Res<ModalCapture>,
    requests: Res<Messages<FileRequest>>,
    rows: Query<&LaneRowIndex>,
    editor: Option<Res<RegionEditor>>,
    lanes: Option<ResMut<WaveLanes>>,
    keys: Res<ButtonInput<KeyCode>>,
    mut arranger: ResMut<Arranger>,
    mut edit_drag: ResMut<EditDrag>,
    ui_scale: Res<UiScale>,
) {
    let (Some(editor), Some(mut lanes)) = (editor, lanes) else {
        return;
    };
    if board_pointer_input_captured(&capture, &requests) {
        cancel_region_drag(&mut edit_drag, &editor, &mut lanes, &mut arranger);
        return;
    }
    let Ok(row) = rows.get(drag.entity) else {
        return;
    };
    let Some(op) = &mut edit_drag.0 else { return };
    if op.lane != row.0 || arranger.frames_per_px <= 0.0 {
        return;
    }
    let dframes = f64::from(drag.distance.x / ui_scale.0) * arranger.frames_per_px;
    let dy_px = f64::from(drag.distance.y / ui_scale.0);
    let shift = keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight);
    let snap = (!shift).then(|| {
        ctk::wave::ruler_minor_step_secs(arranger.frames_per_px / f64::from(SR)) * f64::from(SR)
    });
    let source_len = editor.sources[op.channel].len() as u64;
    op.preview = apply_gesture(op.origin, op.mode, dframes, dy_px, source_len, snap);
    if let Some(display) = lanes
        .lanes
        .get_mut(op.lane)
        .and_then(|lane| lane.regions.get_mut(op.region_idx))
    {
        *display = to_wave_region(&op.preview);
        // Only THIS lane's texture changed — targeted repaint, not a full
        // viewport invalidation every pointer event.
        arranger.dirty_lane = Some(op.lane);
    }
}

/// Commit the drag: one undoable document step + live engine swap; the
/// selection follows the region to its post-sort index.
fn on_lane_drag_end(
    _drag: On<Pointer<bevy::picking::events::DragEnd>>,
    capture: Res<ModalCapture>,
    requests: Res<Messages<FileRequest>>,
    editor: Option<ResMut<RegionEditor>>,
    lanes: Option<ResMut<WaveLanes>>,
    mut arranger: ResMut<Arranger>,
    mut edit_drag: ResMut<EditDrag>,
) {
    let (Some(mut editor), Some(mut lanes)) = (editor, lanes) else {
        return;
    };
    if board_pointer_input_captured(&capture, &requests) {
        cancel_region_drag(&mut edit_drag, &editor, &mut lanes, &mut arranger);
        return;
    }
    let Some(op) = edit_drag.0.take() else { return };
    if op.preview == op.origin {
        return;
    }
    let mut new_document = editor.document.clone();
    let Some(slot) = new_document[op.channel].get_mut(op.region_idx) else {
        return;
    };
    // The document may have changed under the drag (a keyboard delete/undo
    // mid-gesture): commit ONLY if the slot still holds the region this drag
    // started from, else abandon and re-sync the display.
    if *slot != op.origin {
        sync_region_display(&editor, &mut lanes, &mut arranger);
        return;
    }
    *slot = op.preview;
    editor.commit(new_document);
    let new_idx = editor.document[op.channel]
        .iter()
        .position(|region| *region == op.preview);
    arranger.selected = new_idx.map(|idx| (op.lane, idx));
    sync_region_display(&editor, &mut lanes, &mut arranger);
}

/// Region keyboard ops in the waves view: Del deletes the selected region,
/// S splits it at the playhead, ctrl+Z / ctrl+shift+Z undo / redo.
#[derive(SystemParam)]
struct ArrangerEditKeyParams<'w> {
    active: Res<'w, ActiveView>,
    consumed: Res<'w, ConsumedShortcutInputs>,
    editor: Option<ResMut<'w, RegionEditor>>,
    lanes: Option<ResMut<'w, WaveLanes>>,
    arranger: ResMut<'w, Arranger>,
    edit_drag: ResMut<'w, EditDrag>,
    transport: Res<'w, TransportPosition>,
    state: Res<'w, MusicdMixerState>,
}

fn arranger_edit_keys(mut params: ArrangerEditKeyParams) {
    if *params.active != ActiveView::Waves {
        return;
    }
    let (Some(mut editor), Some(mut lanes)) = (params.editor.take(), params.lanes.take()) else {
        return;
    };
    for event in params.consumed.unclaimed_presses() {
        let undo = event.physical == KeyCode::KeyZ
            && event.raw.modifiers.control
            && !event.raw.modifiers.shift;
        let redo = event.physical == KeyCode::KeyZ
            && event.raw.modifiers.control
            && event.raw.modifiers.shift;
        if undo || redo {
            let moved = if redo { editor.redo() } else { editor.undo() };
            if moved {
                // Any in-flight drag now references a stale document; cancel it
                // (the DragEnd guard would refuse it anyway).
                params.edit_drag.0 = None;
                params.arranger.selected = None;
                sync_region_display(&editor, &mut lanes, &mut params.arranger);
            }
            continue;
        }

        let Some((lane_idx, region_idx)) = params.arranger.selected else {
            continue;
        };
        let Some(channel) = lanes.lanes.get(lane_idx).map(|lane| lane.channel as usize) else {
            continue;
        };

        if matches!(event.physical, KeyCode::Delete | KeyCode::Backspace) {
            if region_idx < editor.document[channel].len() {
                let mut new_document = editor.document.clone();
                new_document[channel].remove(region_idx);
                editor.commit(new_document);
                params.edit_drag.0 = None;
                params.arranger.selected = None;
                sync_region_display(&editor, &mut lanes, &mut params.arranger);
            }
        } else if event.physical == KeyCode::KeyS && !event.raw.modifiers.control {
            let playhead_frame = (params.transport.live_seconds(
                transport_is_playing(&params.state),
                lanes.length_frames as f64 / f64::from(SR),
            ) * f64::from(SR)) as u64;
            let Some(&origin) = editor.document[channel].get(region_idx) else {
                continue;
            };
            if let Some((head, tail)) = split_region(origin, playhead_frame) {
                let mut new_document = editor.document.clone();
                new_document[channel][region_idx] = head;
                new_document[channel].push(tail);
                editor.commit(new_document);
                params.edit_drag.0 = None;
                let new_idx = editor.document[channel]
                    .iter()
                    .position(|region| *region == head);
                params.arranger.selected = new_idx.map(|idx| (lane_idx, idx));
                sync_region_display(&editor, &mut lanes, &mut params.arranger);
            }
        }
    }
}

fn on_lane_resize_start(
    drag: On<Pointer<DragStart>>,
    capture: Res<ModalCapture>,
    requests: Res<Messages<FileRequest>>,
    handles: Query<&LaneResizeHandle>,
    arranger: Res<Arranger>,
    mut commands: Commands,
) {
    if board_pointer_input_captured(&capture, &requests) {
        return;
    }
    let Ok(handle) = handles.get(drag.entity) else {
        return;
    };
    let Some(&height) = arranger.lane_heights.get(handle.0) else {
        return;
    };
    commands
        .entity(drag.entity)
        .insert(LaneResizeActive(height));
}

/// Drag the strip at the bottom of a header cell to resize its lane. Like
/// wheel ingestion, this retained pointer observer owns its modal guard: a
/// scheduled run condition cannot prevent an observer commit in the same
/// frame that modal capture opens.
fn on_lane_resize(
    drag: On<Pointer<bevy::picking::events::Drag>>,
    capture: Res<ModalCapture>,
    requests: Res<Messages<FileRequest>>,
    handles: Query<(&LaneResizeHandle, Option<&LaneResizeActive>)>,
    mut arranger: ResMut<Arranger>,
    ui_scale: Res<UiScale>,
    mut commands: Commands,
) {
    let Ok((handle, active)) = handles.get(drag.entity) else {
        return;
    };
    if board_pointer_input_captured(&capture, &requests) {
        if let Some(active) = active {
            restore_lane_resize(handle, active, &mut arranger);
            commands.entity(drag.entity).remove::<LaneResizeActive>();
        }
        return;
    }
    let Some(height) = arranger.lane_heights.get_mut(handle.0) else {
        return;
    };
    // Drag deltas are screen px; lane heights are UI logical px.
    *height = (*height + drag.delta.y / ui_scale.0).clamp(LANE_H_MIN, LANE_H_MAX);
}

fn on_lane_resize_end(
    drag: On<Pointer<DragEnd>>,
    capture: Res<ModalCapture>,
    requests: Res<Messages<FileRequest>>,
    handles: Query<(&LaneResizeHandle, Option<&LaneResizeActive>)>,
    mut arranger: ResMut<Arranger>,
    mut commands: Commands,
) {
    let Ok((handle, active)) = handles.get(drag.entity) else {
        return;
    };
    if board_pointer_input_captured(&capture, &requests) {
        if let Some(active) = active {
            restore_lane_resize(handle, active, &mut arranger);
        }
    }
    commands.entity(drag.entity).remove::<LaneResizeActive>();
}

fn restore_lane_resize(
    handle: &LaneResizeHandle,
    active: &LaneResizeActive,
    arranger: &mut Arranger,
) {
    if let Some(height) = arranger.lane_heights.get_mut(handle.0) {
        *height = active.0;
    }
}

/// Sync lane row heights (header cells AND canvas rows) from
/// [`Arranger::lane_heights`] whenever they change.
fn apply_lane_heights(arranger: Res<Arranger>, mut rows: Query<(&LaneRowIndex, &mut Node)>) {
    if !arranger.is_changed() {
        return;
    }
    for (row, mut node) in &mut rows {
        if let Some(height) = arranger.lane_heights.get(row.0) {
            let target = px(*height);
            if node.height != target {
                node.height = target;
            }
        }
    }
}

// ===========================================================================
// Region editing (R2): document + undo + live engine swap + gestures.
// ===========================================================================

/// The region-edit document and its engine loop (present on `--stems`
/// launches). The UI owns the document + undo history; every commit rebuilds
/// a bank from the SHARED audio `Arc`s (metadata-only cost) and swaps it
/// into the RT thread through the lock-free rings.
#[derive(Resource)]
pub struct RegionEditor {
    sources: [std::sync::Arc<Vec<f32>>; NUM_CHANNELS],
    names: [Option<String>; NUM_CHANNELS],
    song: cosmix_musicd::mixer::SongMeta,
    base_length_frames: u64,
    /// The session document metadata (stem file references + song header).
    session: cosmix_musicd::mixer_host::StemSessionMeta,
    /// rtrb halves are `Send` but `!Sync` — uncontended `Mutex`es satisfy
    /// `Resource`, the same pattern as [`SongEditor`].
    bank_tx: Mutex<rtrb::Producer<Box<StemBank>>>,
    bank_rx: Mutex<rtrb::Consumer<Box<StemBank>>>,
    document: [Vec<Region>; NUM_CHANNELS],
    undo: Vec<[Vec<Region>; NUM_CHANNELS]>,
    redo: Vec<[Vec<Region>; NUM_CHANNELS]>,
    /// A rebuilt bank the ring refused (RT stalled) — re-pushed every frame
    /// until it lands (newest document wins if further commits arrive), so
    /// the engine can never PERMANENTLY diverge from the committed document.
    pending_bank: Option<Box<StemBank>>,
}

const UNDO_DEPTH: usize = 256;

impl RegionEditor {
    pub fn new(parts: StemEditParts) -> Self {
        Self {
            sources: parts.sources,
            names: parts.names,
            song: parts.song,
            base_length_frames: parts.base_length_frames,
            session: parts.session,
            bank_tx: Mutex::new(parts.bank_tx),
            bank_rx: Mutex::new(parts.bank_rx),
            document: parts.initial_regions,
            undo: Vec::new(),
            redo: Vec::new(),
            pending_bank: None,
        }
    }

    /// Write the live document as a `stem-session.v2` `.mix` file.
    /// The export job's input snapshot: shared sources, the live document,
    /// names and the base length — captured at job start.
    #[allow(clippy::type_complexity)]
    pub(crate) fn export_snapshot(
        &self,
    ) -> (
        [std::sync::Arc<Vec<f32>>; NUM_CHANNELS],
        [Vec<Region>; NUM_CHANNELS],
        [Option<String>; NUM_CHANNELS],
        u64,
    ) {
        (
            self.sources.clone(),
            self.document.clone(),
            self.names.clone(),
            self.base_length_frames,
        )
    }

    pub(crate) fn save_session(&self, path: &std::path::Path) -> Result<(), String> {
        cosmix_musicd::mixer_host::save_stem_session_mix(path, &self.session, &self.document)
            .map_err(|error| error.to_string())
    }

    /// The live timeline length (manifest length or the furthest region end).
    fn length_frames(&self) -> u64 {
        self.document
            .iter()
            .flatten()
            .map(|region| region.timeline_start.saturating_add(region.len))
            .max()
            .unwrap_or(0)
            .max(self.base_length_frames)
    }

    /// Replace the document (one undoable step) and swap the engine.
    fn commit(&mut self, mut new_document: [Vec<Region>; NUM_CHANNELS]) {
        for regions in &mut new_document {
            regions.sort_by_key(|region| region.timeline_start);
        }
        self.undo
            .push(std::mem::replace(&mut self.document, new_document));
        if self.undo.len() > UNDO_DEPTH {
            self.undo.remove(0);
        }
        self.redo.clear();
        self.swap_engine();
    }

    fn undo(&mut self) -> bool {
        let Some(previous) = self.undo.pop() else {
            return false;
        };
        self.redo
            .push(std::mem::replace(&mut self.document, previous));
        self.swap_engine();
        true
    }

    fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else {
            return false;
        };
        self.undo.push(std::mem::replace(&mut self.document, next));
        self.swap_engine();
        true
    }

    /// Rebuild a bank from the shared sources + current document and ship it
    /// to the RT thread; drain (and drop) any displaced banks that came back.
    fn swap_engine(&mut self) {
        self.service_swap_rings();
        let mut bank = StemBank::from_shared(self.sources.clone(), self.base_length_frames)
            .with_names(self.names.clone())
            .with_song(self.song.clone());
        for (channel, regions) in self.document.iter().enumerate() {
            bank = bank.with_channel_regions(channel, regions.clone());
        }
        let mut tx = self.bank_tx.lock().expect("stem bank ring poisoned");
        match tx.push(Box::new(bank)) {
            Ok(()) => self.pending_bank = None,
            Err(rtrb::PushError::Full(bank)) => {
                if self.pending_bank.is_none() {
                    eprintln!("studio: region swap ring full — retrying until the engine drains");
                }
                // Newest document wins; an older refused bank is dropped.
                self.pending_bank = Some(bank);
            }
        }
    }

    /// Ring upkeep, every frame: drain returned (displaced) banks so the RT
    /// side always has return slots, and re-push a previously refused bank.
    fn service_swap_rings(&mut self) {
        {
            let mut rx = self.bank_rx.lock().expect("stem bank ring poisoned");
            while rx.pop().is_ok() {}
        }
        if let Some(bank) = self.pending_bank.take() {
            let mut tx = self.bank_tx.lock().expect("stem bank ring poisoned");
            if let Err(rtrb::PushError::Full(bank)) = tx.push(bank) {
                self.pending_bank = Some(bank);
            }
        }
    }
}

/// Every-frame ring upkeep (return-bank draining + refused-swap retry),
/// surfacing stall transitions on the status line.
fn service_region_swap(
    editor: Option<ResMut<RegionEditor>>,
    mut status: ResMut<StatusLine>,
    mut was_pending: bevy::prelude::Local<bool>,
) {
    if let Some(mut editor) = editor {
        editor.service_swap_rings();
        let pending = editor.pending_bank.is_some();
        if pending && !*was_pending {
            status.info("engine busy — edit queued");
        } else if !pending && *was_pending {
            status.ok("queued edit applied");
        }
        *was_pending = pending;
    }
}

/// A running session-audio export: progress feed + cancel flag. One at a
/// time; Esc cancels (partial files are removed by the renderer).
#[derive(Resource, Default)]
pub struct ExportJob(pub(crate) Option<ExportJobState>);

pub(crate) struct ExportJobState {
    pub(crate) rx: Mutex<Receiver<ExportMsg>>,
    pub(crate) cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Whether the worker actually honours the cancel flag (the song-WAV
    /// worker renders in one call and cannot).
    pub(crate) cancellable: bool,
}

pub(crate) enum ExportMsg {
    Progress(u64, u64),
    Done(cosmix_musicd::mixer_host::StemExportReport),
    Failed(String),
    /// A simple terminal note from lighter export workers (song WAV).
    Simple {
        ok: bool,
        message: String,
    },
}

/// Capture the CURRENT mix from the authoritative leaf mirror — the export
/// job's controls snapshot (the same values the strips display).
pub(crate) fn controls_from_state(state: &MusicdMixerState) -> cosmix_musicd::mixer::Controls {
    use cosmix_mixer_schema::LeafValue;
    let number = |path: &str| match state.value(path) {
        Some(LeafValue::Number(n)) => Some(*n),
        _ => None,
    };
    let boolean = |path: &str| matches!(state.value(path), Some(LeafValue::Bool(true)));
    let mut controls = cosmix_musicd::mixer::Controls::default();
    for ch in 0..NUM_CHANNELS {
        let base = format!("mixer.channels.{ch}");
        let channel = &mut controls.channels[ch];
        if let Some(v) = number(&format!("{base}.trim")) {
            channel.trim_db = v;
        }
        if let Some(v) = number(&format!("{base}.fader")) {
            channel.fader_db = v;
        }
        if let Some(v) = number(&format!("{base}.pan")) {
            channel.pan = v;
        }
        channel.mute = boolean(&format!("{base}.mute"));
        channel.solo = boolean(&format!("{base}.solo"));
    }
    if let Some(v) = number("mixer.master.fader") {
        controls.master.fader_db = v;
    }
    controls.master.mute = boolean("mixer.master.mute");
    controls
}

/// Drain the export worker's feed into the status line; clear the job on a
/// terminal message with the peak/clip report.
fn poll_export_job(mut job: ResMut<ExportJob>, mut status: ResMut<StatusLine>) {
    let Some(state) = &job.0 else { return };
    let mut terminal: Option<ExportMsg> = None;
    let mut disconnected = false;
    {
        let rx = state.rx.lock().expect("export channel poisoned");
        loop {
            match rx.try_recv() {
                Ok(ExportMsg::Progress(done, total)) => {
                    status.info(format!("exporting {}%", done * 100 / total.max(1)));
                }
                Ok(other) => {
                    terminal = Some(other);
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // The worker died without a terminal message (panic) —
                    // never leave the job slot occupied forever.
                    disconnected = true;
                    break;
                }
            }
        }
    }
    if disconnected && terminal.is_none() {
        status.error("export worker died unexpectedly");
        job.0 = None;
        return;
    }
    match terminal {
        Some(ExportMsg::Done(report)) => {
            let peak = report.files.iter().map(|f| f.peak).fold(0.0f32, f32::max);
            let clipped: u64 = report.files.iter().map(|f| f.clipped).sum();
            let peak_db = if peak > 0.0 {
                20.0 * peak.log10()
            } else {
                f32::NEG_INFINITY
            };
            let clip_note = if clipped > 0 {
                format!(" — {clipped} CLIPPED samples")
            } else {
                String::new()
            };
            status.ok(format!(
                "exported {} files, peak {peak_db:.1} dBFS{clip_note}",
                report.files.len()
            ));
            job.0 = None;
        }
        Some(ExportMsg::Failed(error)) => {
            status.error(format!("export: {error}"));
            job.0 = None;
        }
        Some(ExportMsg::Simple { ok, message }) => {
            if ok {
                status.ok(message);
            } else {
                status.error(message);
            }
            job.0 = None;
        }
        _ => {}
    }
}

/// Esc cancels a running export (the renderer removes partial files).
fn export_cancel_key(
    consumed: Res<ConsumedShortcutInputs>,
    job: Res<ExportJob>,
    mut status: ResMut<StatusLine>,
) {
    if let Some(state) = &job.0 {
        if consumed
            .unclaimed_presses()
            .any(|event| event.physical == KeyCode::Escape)
        {
            if state.cancellable {
                state
                    .cancel
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                status.info("cancelling export...");
            } else {
                status.info("this export cannot be cancelled");
            }
        }
    }
}

/// Push the editor's document into the display lanes + force a repaint.
/// Region edits mutate `WaveLanes` in place (`structure_rev` untouched), so
/// the structure — and the selection with it — survives.
fn sync_region_display(editor: &RegionEditor, lanes: &mut WaveLanes, arranger: &mut Arranger) {
    for lane in &mut lanes.lanes {
        lane.regions = editor.document[lane.channel as usize]
            .iter()
            .map(to_wave_region)
            .collect();
    }
    lanes.length_frames = editor.length_frames();
    arranger.painted = None;
}

/// Abandon a display-only region preview and restore it from the authoritative
/// editor document. Taking the operation first makes repeated modal/pointer
/// cancellation harmless.
fn cancel_region_drag(
    edit_drag: &mut EditDrag,
    editor: &RegionEditor,
    lanes: &mut WaveLanes,
    arranger: &mut Arranger,
) {
    if edit_drag.0.take().is_some() {
        sync_region_display(editor, lanes, arranger);
    }
}

/// An in-flight region drag: which region, which handle, and the preview.
#[derive(Resource, Default)]
struct EditDrag(Option<DragOp>);

struct DragOp {
    lane: usize,
    channel: usize,
    region_idx: usize,
    mode: DragMode,
    origin: Region,
    preview: Region,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DragMode {
    Move,
    TrimStart,
    TrimEnd,
    Slip,
    /// Vertical drag with ctrl held: region gain (up = louder).
    Gain,
    /// Horizontal drag from a region's top-left corner: fade-in length.
    FadeIn,
    /// Horizontal drag from a region's top-right corner: fade-out length.
    FadeOut,
}

/// Fraction of the lane height (from the top) that turns an edge grab into a
/// fade-corner grab.
const FADE_CORNER_FRACTION: f32 = 0.35;
/// Gain drag scale: pixels of vertical travel per 20 dB.
const GAIN_PX_PER_20DB: f64 = 240.0;

/// Pixel distance from a region edge that grabs a trim handle.
const EDGE_GRAB_PX: f64 = 8.0;

/// Apply a drag of `dframes` (horizontal) / `dy_px` (vertical) to `origin`
/// under `mode`. Pure — clamps keep the region inside the timeline
/// (start ≥ 0), non-empty (len ≥ 1), and its source window inside the stem
/// (`source_len`); fades clamp into the region, gain into [0, 8]. `snap`
/// (frames) quantises the MOVED timeline boundary to the ruler grid; slips,
/// fades and gain never snap.
fn apply_gesture(
    origin: Region,
    mode: DragMode,
    dframes: f64,
    dy_px: f64,
    source_len: u64,
    snap: Option<f64>,
) -> Region {
    let mut region = origin;
    let snap_to = |value: f64| match snap {
        Some(step) if step > 0.0 => (value / step).round() * step,
        _ => value,
    };
    match mode {
        DragMode::Move => {
            let target = snap_to(origin.timeline_start as f64 + dframes).max(0.0);
            region.timeline_start = target as u64;
        }
        DragMode::TrimStart => {
            // Shift the start; source_start follows so the audio stays put.
            let target = snap_to(origin.timeline_start as f64 + dframes);
            let shift = (target - origin.timeline_start as f64)
                .max(-(origin.source_start.min(origin.timeline_start) as f64))
                .min(origin.len as f64 - 1.0);
            let shift = shift as i64;
            region.timeline_start = origin.timeline_start.saturating_add_signed(shift);
            region.source_start = origin.source_start.saturating_add_signed(shift);
            region.len = origin.len.saturating_add_signed(-shift).max(1);
        }
        DragMode::TrimEnd => {
            let target = snap_to((origin.timeline_start + origin.len) as f64 + dframes);
            let max_len = source_len.saturating_sub(origin.source_start).max(1);
            let new_len = (target - origin.timeline_start as f64).max(1.0) as u64;
            region.len = new_len.clamp(1, max_len);
        }
        DragMode::Slip => {
            let max_start = source_len.saturating_sub(origin.len);
            let target = (origin.source_start as f64 - dframes).clamp(0.0, max_start as f64);
            region.source_start = target as u64;
        }
        DragMode::Gain => {
            // Up (negative screen dy) = louder; exponential (dB-linear).
            let factor = 10f64.powf(-dy_px / GAIN_PX_PER_20DB);
            region.gain = ((f64::from(origin.gain) * factor).clamp(0.0, 8.0)) as f32;
        }
        DragMode::FadeIn => {
            region.fade_in = (origin.fade_in as f64 + dframes).clamp(0.0, origin.len as f64) as u64;
        }
        DragMode::FadeOut => {
            region.fade_out =
                (origin.fade_out as f64 - dframes).clamp(0.0, origin.len as f64) as u64;
        }
    }
    region
}

/// Split `origin` at absolute timeline `frame` (must be strictly inside).
/// The head keeps the fade-in, the tail the fade-out — abutting halves play
/// the original signal seamlessly (fades excepted).
fn split_region(origin: Region, frame: u64) -> Option<(Region, Region)> {
    if frame <= origin.timeline_start || frame >= origin.timeline_start + origin.len {
        return None;
    }
    let head_len = frame - origin.timeline_start;
    let head = Region {
        len: head_len,
        fade_out: 0,
        ..origin
    };
    let tail = Region {
        timeline_start: frame,
        source_start: origin.source_start + head_len,
        len: origin.len - head_len,
        fade_in: 0,
        ..origin
    };
    Some((head, tail))
}

/// Resolve a ruler click into a transport seek: pointer x → fraction of the
/// ruler → frame under the viewport's zoom/scroll → seconds, routed through
/// ctk's revisioned seek path (the same machinery as a scrubber commit).
// Bevy observers expose event context and each independent ECS borrow separately.
#[allow(clippy::too_many_arguments)]
fn on_ruler_click(
    click: On<Pointer<Click>>,
    capture: Res<ModalCapture>,
    requests: Res<Messages<FileRequest>>,
    rulers: Query<BodyGeometry<'_>, With<WavesRulerRow>>,
    active_drags: Query<(), With<RulerDragActive>>,
    arranger: Res<Arranger>,
    lanes: Option<Res<WaveLanes>>,
    ui_scale: Res<UiScale>,
    mut seeks: MessageWriter<TransportSeekRequest>,
) {
    if board_pointer_input_captured(&capture, &requests) {
        return;
    }
    // Bevy emits Click before DragEnd on pointer release. The drag lifecycle
    // owns that release; it must not also become a discrete seek.
    if active_drags.get(click.entity).is_ok() {
        return;
    }
    let Some(lanes) = lanes else { return };
    let Some(seconds) = ruler_seek_seconds(
        click.entity,
        click.pointer_location.position,
        &rulers,
        &arranger,
        &lanes,
        ui_scale.0,
    ) else {
        return;
    };
    seeks.write(TransportSeekRequest { seconds });
}

// Bevy observers expose event context and each independent ECS borrow separately.
#[allow(clippy::too_many_arguments)]
fn on_ruler_drag_start(
    drag: On<Pointer<DragStart>>,
    capture: Res<ModalCapture>,
    requests: Res<Messages<FileRequest>>,
    rulers: Query<BodyGeometry<'_>, With<WavesRulerRow>>,
    arranger: Res<Arranger>,
    lanes: Option<Res<WaveLanes>>,
    ui_scale: Res<UiScale>,
    mut commands: Commands,
) {
    if board_pointer_input_captured(&capture, &requests) {
        return;
    }
    let Some(lanes) = lanes else { return };
    let Some(seconds) = ruler_seek_seconds(
        drag.entity,
        drag.pointer_location.position,
        &rulers,
        &arranger,
        &lanes,
        ui_scale.0,
    ) else {
        return;
    };
    commands.entity(drag.entity).insert(RulerDragActive);
    commands.trigger(TransportSeekGesture {
        source: drag.entity,
        phase: TransportSeekGesturePhase::Begin { seconds },
    });
}

/// Drag anywhere on the ruler (or the playhead knob riding it) to scrub through
/// CTK's owned, latest-wins position gesture.
// Bevy observers expose event context and each independent ECS borrow separately.
#[allow(clippy::too_many_arguments)]
fn on_ruler_drag(
    drag: On<Pointer<Drag>>,
    capture: Res<ModalCapture>,
    requests: Res<Messages<FileRequest>>,
    active_drags: Query<(), With<RulerDragActive>>,
    rulers: Query<BodyGeometry<'_>, With<WavesRulerRow>>,
    arranger: Res<Arranger>,
    lanes: Option<Res<WaveLanes>>,
    ui_scale: Res<UiScale>,
    mut commands: Commands,
) {
    if board_pointer_input_captured(&capture, &requests) {
        if active_drags.get(drag.entity).is_ok() {
            commands.entity(drag.entity).remove::<RulerDragActive>();
            commands.trigger(TransportSeekGesture {
                source: drag.entity,
                phase: TransportSeekGesturePhase::Cancel,
            });
        }
        return;
    }
    let Some(lanes) = lanes else { return };
    let Some(seconds) = ruler_seek_seconds(
        drag.entity,
        drag.pointer_location.position,
        &rulers,
        &arranger,
        &lanes,
        ui_scale.0,
    ) else {
        return;
    };
    commands.trigger(TransportSeekGesture {
        source: drag.entity,
        phase: TransportSeekGesturePhase::Update { seconds },
    });
}

// Bevy observers expose event context and each independent ECS borrow separately.
#[allow(clippy::too_many_arguments)]
fn on_ruler_drag_end(
    drag: On<Pointer<DragEnd>>,
    capture: Res<ModalCapture>,
    requests: Res<Messages<FileRequest>>,
    active_drags: Query<(), With<RulerDragActive>>,
    rulers: Query<BodyGeometry<'_>, With<WavesRulerRow>>,
    arranger: Res<Arranger>,
    lanes: Option<Res<WaveLanes>>,
    ui_scale: Res<UiScale>,
    mut commands: Commands,
) {
    // Cancel and DragEnd are both terminal. Whichever arrives second must not
    // submit another terminal phase after the marker has been removed.
    if active_drags.get(drag.entity).is_err() {
        return;
    }
    if board_pointer_input_captured(&capture, &requests) {
        commands.entity(drag.entity).remove::<RulerDragActive>();
        commands.trigger(TransportSeekGesture {
            source: drag.entity,
            phase: TransportSeekGesturePhase::Cancel,
        });
        return;
    }
    let phase = lanes
        .as_deref()
        .and_then(|lanes| {
            ruler_seek_seconds(
                drag.entity,
                drag.pointer_location.position,
                &rulers,
                &arranger,
                lanes,
                ui_scale.0,
            )
        })
        .map_or(TransportSeekGesturePhase::Cancel, |seconds| {
            TransportSeekGesturePhase::Commit { seconds }
        });
    commands.entity(drag.entity).remove::<RulerDragActive>();
    commands.trigger(TransportSeekGesture {
        source: drag.entity,
        phase,
    });
}

/// Cancel is global because Bevy targets `Pointer<Cancel>` at the entity under
/// the pointer, not the entity where the drag began.
///
/// Deferred touch limitations: simultaneous touch pointers still share this
/// entity-owned marker, and Bevy emits no cancel when a cancelled touch hovers
/// nothing. A pointer-id-aware gesture lifecycle is the Phase-1 fix; the
/// arranger rebuild remains the recovery net for the no-hover case.
fn on_ruler_drag_cancel(
    _cancel: On<Pointer<PointerCancel>>,
    active_drags: Query<Entity, With<RulerDragActive>>,
    mut commands: Commands,
) {
    let Ok(source) = active_drags.single() else {
        return;
    };
    commands.entity(source).remove::<RulerDragActive>();
    commands.trigger(TransportSeekGesture {
        source,
        phase: TransportSeekGesturePhase::Cancel,
    });
}

fn ruler_seek_seconds(
    ruler: Entity,
    pointer_position: bevy::math::Vec2,
    rulers: &Query<BodyGeometry<'_>, With<WavesRulerRow>>,
    arranger: &Arranger,
    lanes: &WaveLanes,
    ui_scale: f32,
) -> Option<f64> {
    let (computed, transform, target) = rulers.get(ruler).ok()?;
    if arranger.frames_per_px <= 0.0 {
        return None;
    }
    let normalised = computed.normalize_point(
        *transform,
        pointer_position * target.scale_factor() / ui_scale,
    )?;
    let fraction = (f64::from(normalised.x) + 0.5).clamp(0.0, 1.0);
    let width = body_logical_width(computed);
    let frame = arranger.scroll_frame + fraction * width * arranger.frames_per_px;
    Some((frame / f64::from(SR)).clamp(0.0, lanes.length_frames as f64 / f64::from(SR)))
}

/// The lanes-body geometry needed by input/paint/playhead: logical width plus
/// the pieces `normalize_point` wants for cursor mapping.
type BodyGeometry<'a> = (
    &'a ComputedNode,
    &'a UiGlobalTransform,
    &'a ComputedUiRenderTargetInfo,
);

fn body_logical_width(computed: &ComputedNode) -> f64 {
    f64::from(computed.size().x * computed.inverse_scale_factor())
}

/// Clamp zoom so one screen shows at most the whole session (plus nothing)
/// and at most level-0 detail; clamp scroll into the session.
fn clamp_view(arranger: &mut Arranger, length_frames: u64, width: f64) {
    if width <= 0.0 {
        return;
    }
    let fit = (length_frames as f64 / width).max(MIN_FRAMES_PER_PX);
    arranger.frames_per_px = arranger.frames_per_px.clamp(MIN_FRAMES_PER_PX, fit);
    let max_scroll = (length_frames as f64 - width * arranger.frames_per_px).max(0.0);
    arranger.scroll_frame = arranger.scroll_frame.clamp(0.0, max_scroll);
}

/// Continuous-input ingestion boundary for the waves view. It always runs and
/// drains its system-local [`MessageReader<MouseWheel>`], even while a modal
/// captures input; skipping it on the close frame would replay stale
/// wheel input into the arranger. Plain wheel scrolls and ctrl+wheel zooms.
// Bevy systems expose each independently borrowed resource/query as a parameter.
#[allow(clippy::too_many_arguments)]
fn arranger_input(
    active: Res<ActiveView>,
    capture: Res<ctk::prelude::ModalCapture>,
    lanes: Option<Res<WaveLanes>>,
    mut arranger: ResMut<Arranger>,
    mut wheel: MessageReader<MouseWheel>,
    keys: Res<ButtonInput<KeyCode>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    bodies: Query<BodyGeometry<'_>, With<WavesBody>>,
    containers: Query<BodyGeometry<'_>, With<WavesContainer>>,
    mut scrollers: Query<(&mut bevy::ui::ScrollPosition, &ComputedNode), With<WavesScroll>>,
    ui_scale: Res<UiScale>,
) {
    // Modal capture owns input: wheel/zoom must not reach the arranger under it.
    if capture.is_captured() {
        wheel.clear();
        return;
    }
    let (Some(lanes), Ok((computed, transform, target))) = (lanes, bodies.single()) else {
        wheel.clear();
        return;
    };
    if *active != ActiveView::Waves || arranger.frames_per_px <= 0.0 {
        wheel.clear();
        return;
    }
    let width = body_logical_width(computed);
    if width <= 0.0 {
        return;
    }
    // The pointer, normalised into the waves container (ruler + lanes) —
    // node-centred [-0.5, 0.5] on both axes when inside it. Wheel input is
    // only the timeline's when the pointer is actually over it; a scroll over
    // the menu or the transport footer must not move the viewport.
    let over_container = windows
        .single()
        .ok()
        .and_then(|window| window.cursor_position())
        .and_then(|cursor| {
            containers.single().ok().and_then(|(node, tf, info)| {
                node.normalize_point(*tf, cursor * info.scale_factor() / ui_scale.0)
            })
        })
        .is_some_and(|n| n.x.abs() <= 0.5 && n.y.abs() <= 0.5);
    if !over_container {
        wheel.clear();
        return;
    }
    // Lines regardless of unit: a pixel-unit tick (touchpad) is worth its
    // fraction of a 20 px line. Modes, Ardour-style, by precedence:
    // ctrl+wheel zooms at the cursor, shift+wheel scrolls the timeline, a
    // plain vertical wheel scrolls the lane stack; a genuine horizontal
    // wheel always scrolls the timeline.
    let (mut lines_y, mut lines_x) = (0.0f64, 0.0f64);
    for message in wheel.read() {
        let scale = match message.unit {
            bevy::input::mouse::MouseScrollUnit::Line => 1.0,
            bevy::input::mouse::MouseScrollUnit::Pixel => 1.0 / 20.0,
        };
        lines_y += f64::from(message.y) * scale;
        lines_x += f64::from(message.x) * scale;
    }
    if lines_y == 0.0 && lines_x == 0.0 {
        return;
    }
    let ctrl = keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight);
    let shift = keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight);

    if ctrl {
        let lines = lines_y + lines_x;
        // Cursor x as a 0..1 fraction of the body (centre when unknown).
        let fraction = windows
            .single()
            .ok()
            .and_then(|window| window.cursor_position())
            .and_then(|cursor| {
                computed.normalize_point(*transform, cursor * target.scale_factor() / ui_scale.0)
            })
            .map_or(0.5, |normalised| f64::from(normalised.x) + 0.5)
            .clamp(0.0, 1.0);
        let anchor_frame = arranger.scroll_frame + fraction * width * arranger.frames_per_px;
        arranger.frames_per_px *= ZOOM_PER_LINE.powf(-lines);
        clamp_view(&mut arranger, lanes.length_frames, width);
        arranger.scroll_frame = anchor_frame - fraction * width * arranger.frames_per_px;
        clamp_view(&mut arranger, lanes.length_frames, width);
        return;
    }

    let timeline_lines = if shift { lines_y + lines_x } else { lines_x };
    if timeline_lines != 0.0 {
        arranger.scroll_frame -= timeline_lines * SCROLL_PX_PER_LINE * arranger.frames_per_px;
        clamp_view(&mut arranger, lanes.length_frames, width);
    }
    if !shift && lines_y != 0.0 {
        // Plain vertical wheel: scroll the lane stack. Bevy clamps only the
        // EFFECTIVE scroll (ComputedNode), not this component — clamp here
        // so wheeling past an edge never builds a dead zone that has to be
        // unwound before the view moves again.
        for (mut scroll, computed) in &mut scrollers {
            let logical = computed.inverse_scale_factor();
            let max = ((computed.content_size.y - computed.size().y) * logical).max(0.0);
            scroll.0.y = (scroll.0.y - lines_y as f32 * VSCROLL_PX_PER_LINE).clamp(0.0, max);
        }
    }
}

/// Repaint the lane + ruler textures whenever the viewport key changes. A
/// paint is O(width × lanes) pyramid folds — cheap enough to run on any
/// zoom/scroll/resize step.
// Bevy systems expose each independently borrowed resource/query as a parameter.
#[allow(clippy::too_many_arguments)]
fn arranger_paint(
    active: Res<ActiveView>,
    theme: Res<bevy::feathers::theme::UiTheme>,
    theme_state: Res<ThemeState>,
    lanes: Option<Res<WaveLanes>>,
    mut arranger: ResMut<Arranger>,
    bodies: Query<&ComputedNode, With<WavesBody>>,
    mut images: ResMut<Assets<Image>>,
    mut lane_images: Query<(&WavesLaneImage, &mut ImageNode), Without<WavesRulerImage>>,
    mut ruler_images: Query<&mut ImageNode, With<WavesRulerImage>>,
    ruler_rows: Query<Entity, With<WavesRulerRow>>,
    old_labels: Query<Entity, With<WavesRulerLabel>>,
    mut commands: Commands,
) {
    if *active != ActiveView::Waves {
        return;
    }
    let (Some(lanes), Ok(computed)) = (lanes, bodies.single()) else {
        return;
    };
    let width = body_logical_width(computed);
    if width < 1.0 {
        return;
    }
    if arranger.frames_per_px <= 0.0 {
        // First layout: fit the whole session.
        arranger.frames_per_px = f64::MAX;
        arranger.scroll_frame = 0.0;
    }
    // Resolve the fit BEFORE consuming a menu zoom, so a Zoom In issued
    // before the first paint steps in from the fitted view instead of being
    // clamped straight back to it (and lost).
    clamp_view(&mut arranger, lanes.length_frames, width);
    // Menu-issued zoom, applied here where the width is known; steps anchor
    // the viewport centre.
    while let Some(zoom) = arranger.pending_zoom.pop_front() {
        let centre_frame = arranger.scroll_frame + 0.5 * width * arranger.frames_per_px;
        match zoom {
            ZoomCmd::In => arranger.frames_per_px /= MENU_ZOOM_STEP,
            ZoomCmd::Out => arranger.frames_per_px *= MENU_ZOOM_STEP,
            ZoomCmd::Fit => {
                arranger.frames_per_px = f64::MAX;
                arranger.scroll_frame = 0.0;
            }
        }
        clamp_view(&mut arranger, lanes.length_frames, width);
        if zoom != ZoomCmd::Fit {
            arranger.scroll_frame = centre_frame - 0.5 * width * arranger.frames_per_px;
        }
    }
    clamp_view(&mut arranger, lanes.length_frames, width);

    let key = PaintKey {
        fpp_bits: arranger.frames_per_px.to_bits(),
        scroll_bits: arranger.scroll_frame.to_bits(),
        width: width as u32,
        lanes: lanes.lanes.len(),
        selected: arranger.selected,
        theme_revision: theme_state.revision,
    };
    if arranger.painted == Some(key) {
        // Drag preview: repaint only the dirtied lane.
        if let Some(dirty) = arranger.dirty_lane.take() {
            for (lane_image, mut node) in &mut lane_images {
                if lane_image.0 != dirty {
                    continue;
                }
                if let Some(lane) = lanes.lanes.get(dirty) {
                    repaint_lane_image(
                        lane,
                        dirty,
                        arranger.selected,
                        arranger.scroll_frame,
                        arranger.frames_per_px,
                        width as u32,
                        ctk::theme::ctk_color(&theme, &ctk::theme::tokens::PANEL),
                        channel_color(
                            lane.channel,
                            ctk::theme::ctk_color(&theme, &ctk::theme::tokens::CONTROL_ACTIVE),
                        ),
                        &mut images,
                        &mut node,
                    );
                }
            }
        }
        return;
    }
    arranger.dirty_lane = None;
    // A selection-only change repaints just the lanes losing/gaining the
    // highlight (and leaves the ruler alone) — a click must not re-upload
    // every lane texture.
    let affected_lanes: Option<[Option<usize>; 2]> = match arranger.painted {
        Some(previous)
            if PaintKey {
                selected: key.selected,
                ..previous
            } == key =>
        {
            Some([
                previous.selected.map(|(lane, _)| lane),
                key.selected.map(|(lane, _)| lane),
            ])
        }
        _ => None,
    };
    arranger.painted = Some(key);

    let width_px = width as u32;
    for (lane_image, mut node) in &mut lane_images {
        if let Some(affected) = &affected_lanes {
            if !affected.contains(&Some(lane_image.0)) {
                continue;
            }
        }
        let Some(lane) = lanes.lanes.get(lane_image.0) else {
            continue;
        };
        repaint_lane_image(
            lane,
            lane_image.0,
            arranger.selected,
            arranger.scroll_frame,
            arranger.frames_per_px,
            width_px,
            ctk::theme::ctk_color(&theme, &ctk::theme::tokens::PANEL),
            channel_color(
                lane.channel,
                ctk::theme::ctk_color(&theme, &ctk::theme::tokens::CONTROL_ACTIVE),
            ),
            &mut images,
            &mut node,
        );
    }
    if affected_lanes.is_some() {
        return;
    }

    let secs_per_px = arranger.frames_per_px / f64::from(SR);
    let ticks = ruler_ticks(arranger.scroll_frame / f64::from(SR), secs_per_px, width_px);
    for mut node in &mut ruler_images {
        node.image = images.add(paint_ruler(
            &ticks,
            width_px,
            RULER_PX,
            ctk::theme::ctk_color(&theme, &ctk::theme::tokens::TEXT_DIM),
            ctk::theme::ctk_color(&theme, &ctk::theme::tokens::PANEL),
        ));
    }
    for label in &old_labels {
        commands.entity(label).despawn();
    }
    if let Some(row) = ruler_rows.iter().next() {
        commands.entity(row).with_children(|parent| {
            for &(x, t) in &ticks.major {
                parent.spawn((
                    Node {
                        position_type: PositionType::Absolute,
                        left: px(x + 3.0),
                        top: px(1),
                        ..default()
                    },
                    Text::new(format_ruler_secs(t, ticks.major_step_secs)),
                    TextFont::from_font_size(10.0),
                    bevy::feathers::theme::ThemeTextColor(ctk::theme::tokens::TEXT_DIM),
                    WavesRulerLabel,
                ));
            }
        });
    }
}

/// Move the playhead to the live extrapolated transport clock (the same value
/// the scrubber and piano roll follow), paging the viewport forward when
/// playback runs off the right edge — but never yanking a viewport the user
/// has scrolled far away.
// Bevy systems expose each independently borrowed resource/query as a parameter.
#[allow(clippy::too_many_arguments)]
fn arranger_playhead(
    active: Res<ActiveView>,
    lanes: Option<Res<WaveLanes>>,
    mut arranger: ResMut<Arranger>,
    transport: Res<TransportPosition>,
    state: Res<MusicdMixerState>,
    bodies: Query<&ComputedNode, With<WavesBody>>,
    mut playheads: Query<&mut Node, With<WavesPlayhead>>,
    mut knobs: Query<&mut Node, (With<WavesPlayheadKnob>, Without<WavesPlayhead>)>,
) {
    if *active != ActiveView::Waves {
        return;
    }
    let (Some(lanes), Ok(computed)) = (lanes, bodies.single()) else {
        return;
    };
    let width = body_logical_width(computed);
    if width <= 0.0 || arranger.frames_per_px <= 0.0 {
        return;
    }
    // Hold the last position across connection/epoch resets instead of
    // snapping to 0 while the base re-establishes (the scrubber holds the
    // same way).
    if !transport.has_base() {
        return;
    }
    let playing = transport_is_playing(&state);
    let length_secs = lanes.length_frames as f64 / f64::from(SR);
    let frame = transport.live_seconds(playing, length_secs) * f64::from(SR);
    let mut x = (frame - arranger.scroll_frame) / arranger.frames_per_px;

    // Page forward when playback crosses the right edge of a viewport the
    // playhead was just in (within one page) — Ardour-style paging without a
    // follow toggle that fights manual inspection elsewhere in the session.
    if playing && x > width && x < 2.0 * width {
        arranger.scroll_frame = frame - 0.05 * width * arranger.frames_per_px;
        clamp_view(&mut arranger, lanes.length_frames, width);
        x = (frame - arranger.scroll_frame) / arranger.frames_per_px;
    }

    let visible = (0.0..=width).contains(&x);
    for mut node in &mut playheads {
        if visible {
            node.display = Display::Flex;
            node.left = px(x as f32);
        } else {
            node.display = Display::None;
        }
    }
    // The knob shares the ruler's x-axis with the body, so the same x centres
    // it on the line (minus half its width).
    for mut node in &mut knobs {
        if visible {
            node.display = Display::Flex;
            node.left = px(x as f32 - PLAYHEAD_KNOB_PX / 2.0);
        } else {
            node.display = Display::None;
        }
    }
}

/// Spawn the loading overlay (hidden): a full-cover panel over the waves
/// container with a centred spinner + label. The returned entity is added as a
/// child of the waves container by the caller. It is NOT marked [`WavesLane`],
/// so `arranger_structure`'s rebuild (which despawns only `WavesLane`s) leaves
/// it in place across song re-renders.
pub fn spawn_waves_spinner(commands: &mut Commands) -> Entity {
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                left: px(0),
                top: px(0),
                width: percent(100),
                height: percent(100),
                align_items: bevy::ui::AlignItems::Center,
                justify_content: bevy::ui::JustifyContent::Center,
                display: Display::None,
                ..default()
            },
            // Opaque enough to hide the stale (outgoing song's) waves beneath.
            bevy::feathers::theme::ThemeBackgroundColor(ctk::theme::tokens::SURFACE),
            bevy::ui::GlobalZIndex(50),
            WavesSpinner,
        ))
        .with_children(|overlay| {
            overlay
                .spawn(Node {
                    flex_direction: bevy::ui::FlexDirection::Row,
                    column_gap: px(10),
                    align_items: bevy::ui::AlignItems::Center,
                    ..default()
                })
                .with_children(|row| {
                    row.spawn((
                        Text::new("|"),
                        TextFont::from_font_size(22.0),
                        bevy::feathers::theme::ThemeTextColor(ctk::theme::tokens::CONTROL_ACTIVE),
                        WavesSpinnerGlyph,
                    ));
                    row.spawn((
                        Text::new("Rendering waves..."),
                        TextFont::from_font_size(16.0),
                        bevy::feathers::theme::ThemeTextColor(ctk::theme::tokens::TEXT),
                    ));
                });
        })
        .id()
}

/// Show the loading overlay while a song re-render is in flight (the waves
/// worker thread — `waves.rx` is `Some`), and spin its glyph. The overlay hides
/// the moment `waves_receive` publishes the finished lanes, so the fresh waves
/// appear rendered, not mid-build. The playhead is reset to zero separately by
/// the load's `AudioIntent::Reset`.
fn waves_spinner(
    active: Res<ActiveView>,
    waves: Res<Waves>,
    time: Res<bevy::time::Time>,
    mut accum: bevy::prelude::Local<f32>,
    mut frame: bevy::prelude::Local<usize>,
    mut overlays: Query<&mut Node, With<WavesSpinner>>,
    mut glyphs: Query<&mut Text, With<WavesSpinnerGlyph>>,
) {
    let loading = *active == ActiveView::Waves && waves.rx.is_some();
    for mut node in &mut overlays {
        let want = if loading {
            Display::Flex
        } else {
            Display::None
        };
        if node.display != want {
            node.display = want;
        }
    }
    if !loading {
        return;
    }
    *accum += time.delta_secs();
    if *accum >= 0.1 {
        *accum = 0.0;
        *frame = (*frame + 1) % 4;
        let glyph = ["|", "/", "-", "\\"][*frame];
        for mut text in &mut glyphs {
            text.0 = glyph.to_string();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use bevy::app::TaskPoolPlugin;
    use bevy::camera::NormalizedRenderTarget;
    use bevy::feathers::theme::UiTheme;
    use bevy::input_focus::InputFocus;
    use bevy::math::Vec2;
    use bevy::picking::backend::HitData;
    use bevy::picking::pointer::{Location, PointerButton, PointerId};
    use bevy::window::WindowRef;
    use cosmix_mixer_schema::WriteRequest;
    use ctk::prelude::{
        ApplyTheme, CtkThemePlugin, FileRequest, FileRequestId, FileRequesterPlugin,
        FileRequesterSystems, MixerConnectionState, MixerTransport, ModalCapture, Mode,
        MusicdMixerPlugin, Scheme, ThemeSpec, TransportEvent, TransportMessage,
    };

    #[test]
    fn live_menu_definitions_use_every_canonical_action_id_once() {
        let actual: Vec<_> = menu_defs()
            .into_iter()
            .flat_map(|menu| menu.items)
            .map(|item| item.id)
            .collect();
        let expected: Vec<_> = ids::MENU_ACTION_IDS
            .iter()
            .map(|action| action.as_str())
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn arranger_paint_cache_invalidates_on_theme_revision() {
        let before = PaintKey {
            fpp_bits: 1,
            scroll_bits: 2,
            width: 3,
            lanes: 4,
            selected: None,
            theme_revision: 8,
        };
        assert!(
            before
                != PaintKey {
                    theme_revision: 9,
                    ..before
                }
        );
    }

    #[test]
    fn lane_header_name_restyles_without_a_structure_rebuild() {
        let mut app = App::new();
        app.add_plugins(CtkThemePlugin::default())
            .add_systems(Update, restyle_lane_header_names);
        let name = app
            .world_mut()
            .spawn((LaneHeaderName(3), TextColor(Color::NONE)))
            .id();

        app.world_mut()
            .write_message(ApplyTheme(ThemeSpec::builtin()));
        app.update();
        app.update();
        let first = app.world().get::<TextColor>(name).unwrap().0;

        app.world_mut()
            .write_message(ApplyTheme(ThemeSpec::from_scheme(
                Scheme::Crimson,
                Mode::Light,
            )));
        app.update();
        app.update();
        let second = app.world().get::<TextColor>(name).unwrap().0;
        let theme = app.world().resource::<bevy::feathers::theme::UiTheme>();
        assert_ne!(first, second);
        assert_eq!(
            second,
            channel_color(
                3,
                ctk::theme::ctk_color(theme, &ctk::theme::tokens::CONTROL_ACTIVE)
            )
        );
        assert!(app.world().get_entity(name).is_ok());
    }

    #[test]
    fn repeated_zoom_requests_accumulate_in_order() {
        let mut app = App::new();
        app.insert_resource(ActiveView::Waves)
            .init_resource::<Arranger>()
            .init_resource::<StatusLine>()
            .add_message::<ActionRequest>()
            .add_systems(Update, on_menu);
        for action in [ids::MENU_ZOOM_IN, ids::MENU_ZOOM_IN, ids::MENU_ZOOM_OUT] {
            app.world_mut().write_message(ActionRequest {
                action,
                source: ctk::prelude::Source::Key,
                args: Default::default(),
                invocation_focus: None,
            });
        }

        app.update();

        let pending: Vec<_> = app
            .world()
            .resource::<Arranger>()
            .pending_zoom
            .iter()
            .copied()
            .collect();
        assert!(matches!(
            pending.as_slice(),
            [ZoomCmd::In, ZoomCmd::In, ZoomCmd::Out]
        ));
    }

    #[test]
    fn action_request_consumers_cover_every_canonical_menu_id_once() {
        assert!(HANDLED_MENU_ACTION_IDS
            .iter()
            .copied()
            .all(handles_menu_action));
        assert!(crate::file_io::HANDLED_MENU_ACTION_IDS
            .iter()
            .copied()
            .all(crate::file_io::handles_menu_action));
        assert!(crate::settings::HANDLED_MENU_ACTION_IDS
            .iter()
            .copied()
            .all(crate::settings::handles_menu_action));
        assert!(crate::action::THEME_MENU_ACTION_IDS
            .iter()
            .copied()
            .all(crate::action::handles_theme_action));

        let mut actual: Vec<_> = HANDLED_MENU_ACTION_IDS
            .iter()
            .chain(crate::file_io::HANDLED_MENU_ACTION_IDS)
            .chain(crate::settings::HANDLED_MENU_ACTION_IDS)
            .chain(crate::action::THEME_MENU_ACTION_IDS)
            .copied()
            .collect();
        assert_eq!(actual.len(), ids::MENU_ACTION_IDS.len());
        actual.sort_unstable();

        let mut expected = ids::MENU_ACTION_IDS.to_vec();
        expected.sort_unstable();
        assert_eq!(actual, expected);
    }

    struct RecordingTransport {
        writes: Arc<Mutex<Vec<WriteRequest>>>,
    }

    impl MixerTransport for RecordingTransport {
        fn service_name(&self) -> &str {
            "studio-ruler-cancel-test"
        }

        fn issue_write(&mut self, _request_id: u64, request: &WriteRequest) -> Result<(), String> {
            self.writes.lock().unwrap().push(request.clone());
            Ok(())
        }

        fn request_snapshot(&mut self, _request_id: u64) -> Result<(), String> {
            Ok(())
        }

        fn request_position(&mut self, _request_id: u64) -> Result<(), String> {
            Ok(())
        }

        fn poll_events(&mut self, _out: &mut Vec<TransportEvent>) {}

        fn poll_messages(&mut self, _out: &mut Vec<TransportMessage>) {}

        fn discard_backlog(&mut self) {}
    }

    fn region(timeline_start: u64, source_start: u64, len: u64) -> Region {
        Region {
            timeline_start,
            source_start,
            len,
            gain: 1.0,
            fade_in: 0,
            fade_out: 0,
        }
    }

    fn queue_test_requester(app: &mut App, id: u64) {
        let mut request = FileRequest::open_file(FileRequestId(id), "Open");
        request.initial_directory = Some(std::env::temp_dir());
        app.world_mut().write_message(request);
    }

    fn open_test_requester(app: &mut App, id: u64) {
        queue_test_requester(app, id);
        app.update();
        assert!(app.world().resource::<ModalCapture>().is_captured());
    }

    fn pointer_drag_end(target: Entity) -> Pointer<DragEnd> {
        Pointer::new(
            PointerId::Mouse,
            Location {
                target: NormalizedRenderTarget::Window(
                    WindowRef::Entity(target).normalize(None).unwrap(),
                ),
                position: Vec2::ZERO,
            },
            DragEnd {
                button: PointerButton::Primary,
                distance: Vec2::new(20.0, 0.0),
            },
            target,
        )
    }

    fn region_editor(origin: Region) -> RegionEditor {
        let sources = std::array::from_fn(|_| Arc::new(vec![0.0; 2_000]));
        let names = std::array::from_fn(|_| None);
        let mut document: [Vec<Region>; NUM_CHANNELS] = std::array::from_fn(|_| Vec::new());
        document[0].push(origin);
        let (bank_tx, _bank_sink) = rtrb::RingBuffer::<Box<StemBank>>::new(2);
        let (_bank_return, bank_rx) = rtrb::RingBuffer::<Box<StemBank>>::new(2);
        RegionEditor {
            sources,
            names,
            song: Default::default(),
            base_length_frames: 2_000,
            session: cosmix_musicd::mixer_host::StemSessionMeta {
                entries: Vec::new(),
                base_length_frames: 2_000,
                song: Default::default(),
            },
            bank_tx: Mutex::new(bank_tx),
            bank_rx: Mutex::new(bank_rx),
            document,
            undo: Vec::new(),
            redo: Vec::new(),
            pending_bank: None,
        }
    }

    #[test]
    fn modal_drag_end_restores_region_preview_without_committing() {
        let origin = region(100, 0, 500);
        let preview = region(300, 0, 500);
        let samples = vec![0.0; 2_000];
        let mut app = App::new();
        app.add_plugins(TaskPoolPlugin::default())
            .init_resource::<InputFocus>()
            .init_resource::<UiTheme>()
            .init_resource::<ButtonInput<KeyCode>>()
            .add_plugins(FileRequesterPlugin)
            .insert_resource(region_editor(origin))
            .insert_resource(WaveLanes {
                lanes: vec![WaveLane {
                    channel: 0,
                    name: "Test".to_string(),
                    pyramid: WavePyramid::new(&samples),
                    regions: vec![to_wave_region(&preview)],
                }],
                length_frames: 2_000,
                structure_rev: 0,
            })
            .insert_resource(Arranger {
                lane_heights: vec![LANE_H_DEFAULT],
                ..default()
            })
            .insert_resource(EditDrag(Some(DragOp {
                lane: 0,
                channel: 0,
                region_idx: 0,
                mode: DragMode::Move,
                origin,
                preview,
            })));
        let lane = app.world_mut().spawn(LaneRowIndex(0)).id();
        app.world_mut().entity_mut(lane).observe(on_lane_drag_end);

        queue_test_requester(&mut app, 501);
        assert!(
            !app.world().resource::<ModalCapture>().is_captured(),
            "the regression must exercise the pre-ingestion observer window"
        );
        app.world_mut().trigger(pointer_drag_end(lane));
        app.update();
        assert!(app.world().resource::<ModalCapture>().is_captured());

        let editor = app.world().resource::<RegionEditor>();
        assert_eq!(editor.document[0], vec![origin]);
        assert!(editor.undo.is_empty(), "modal DragEnd must not commit");
        assert!(app.world().resource::<EditDrag>().0.is_none());
        let display = &app.world().resource::<WaveLanes>().lanes[0].regions[0];
        assert_eq!(display.timeline_start, origin.timeline_start);
        assert_eq!(display.source_start, origin.source_start);
        assert_eq!(display.len, origin.len);
    }

    #[test]
    fn requester_open_eagerly_releases_ruler_seek_ownership() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let mut app = App::new();
        app.add_plugins(bevy::MinimalPlugins)
            .init_resource::<InputFocus>()
            .init_resource::<UiTheme>()
            .init_resource::<ButtonInput<KeyCode>>()
            .init_resource::<Arranger>()
            .init_resource::<EditDrag>()
            .add_plugins(FileRequesterPlugin)
            .add_plugins(MusicdMixerPlugin::with_transport(Box::new(
                RecordingTransport {
                    writes: Arc::clone(&writes),
                },
            )))
            .add_systems(
                Update,
                cancel_board_gestures_on_modal.after(FileRequesterSystems),
            );
        app.finish();
        app.cleanup();
        {
            let mut state = app.world_mut().resource_mut::<MusicdMixerState>();
            state.connection = MixerConnectionState::Connected;
            state.ready = true;
        }

        let ruler = app.world_mut().spawn((WavesRulerRow, RulerDragActive)).id();
        app.world_mut().trigger(TransportSeekGesture {
            source: ruler,
            phase: TransportSeekGesturePhase::Begin { seconds: 12.0 },
        });

        open_test_requester(&mut app, 502);
        assert!(!app.world().entity(ruler).contains::<RulerDragActive>());

        // Behavioural proof that CTK released transport.position: a later
        // source-less application seek is accepted instead of colliding.
        app.world_mut()
            .write_message(TransportSeekRequest { seconds: 24.0 });
        app.update();
        assert_eq!(writes.lock().unwrap().len(), 1);
    }

    #[test]
    fn off_ruler_pointer_cancel_releases_ruler_seek_ownership() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let mut app = App::new();
        app.add_plugins(bevy::MinimalPlugins)
            .add_plugins(MusicdMixerPlugin::with_transport(Box::new(
                RecordingTransport {
                    writes: Arc::clone(&writes),
                },
            )))
            .add_observer(on_ruler_drag_cancel);
        app.finish();
        app.cleanup();
        {
            let mut state = app.world_mut().resource_mut::<MusicdMixerState>();
            state.connection = MixerConnectionState::Connected;
            state.ready = true;
        }

        let ruler = app.world_mut().spawn((WavesRulerRow, RulerDragActive)).id();
        app.world_mut().trigger(TransportSeekGesture {
            source: ruler,
            phase: TransportSeekGesturePhase::Begin { seconds: 12.0 },
        });

        // Bevy sends Cancel to the currently hovered entity, which need not be
        // the ruler that owns the active drag.
        let other = app.world_mut().spawn_empty().id();
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            Location {
                target: NormalizedRenderTarget::Window(
                    WindowRef::Entity(other).normalize(None).unwrap(),
                ),
                position: Vec2::ZERO,
            },
            PointerCancel {
                hit: HitData::new(Entity::PLACEHOLDER, 0.0, None, None),
            },
            other,
        ));
        app.world_mut().flush();

        assert!(!app.world().entity(ruler).contains::<RulerDragActive>());

        // The public behavioural proof that CTK released transport.position:
        // a source-less discrete seek is accepted and reaches the transport.
        app.world_mut()
            .write_message(TransportSeekRequest { seconds: 24.0 });
        app.update();
        assert_eq!(writes.lock().unwrap().len(), 1);
    }

    #[test]
    fn move_snaps_and_clamps_at_zero() {
        let origin = region(1000, 0, 500);
        let moved = apply_gesture(origin, DragMode::Move, 240.0, 0.0, 10_000, Some(480.0));
        assert_eq!(moved.timeline_start, 1440, "snapped to the 480 grid");
        assert_eq!((moved.source_start, moved.len), (0, 500), "move only");
        let moved = apply_gesture(origin, DragMode::Move, -5000.0, 0.0, 10_000, None);
        assert_eq!(moved.timeline_start, 0, "clamped at the timeline origin");
    }

    #[test]
    fn trim_start_shifts_source_and_len_together() {
        let origin = region(1000, 200, 500);
        let trimmed = apply_gesture(origin, DragMode::TrimStart, 100.0, 0.0, 10_000, None);
        assert_eq!(trimmed.timeline_start, 1100);
        assert_eq!(trimmed.source_start, 300);
        assert_eq!(trimmed.len, 400);
        // Left extension is bounded by the source head (source_start 200).
        let extended = apply_gesture(origin, DragMode::TrimStart, -500.0, 0.0, 10_000, None);
        assert_eq!(extended.timeline_start, 800);
        assert_eq!(extended.source_start, 0);
        assert_eq!(extended.len, 700);
        // Right shrink keeps at least one frame.
        let sliver = apply_gesture(origin, DragMode::TrimStart, 5000.0, 0.0, 10_000, None);
        assert_eq!(sliver.len, 1);
    }

    #[test]
    fn trim_end_bounded_by_source_window_and_one_frame() {
        let origin = region(1000, 9_000, 500);
        let longer = apply_gesture(origin, DragMode::TrimEnd, 5000.0, 0.0, 10_000, None);
        assert_eq!(longer.len, 1000, "len capped at the source tail");
        let shorter = apply_gesture(origin, DragMode::TrimEnd, -5000.0, 0.0, 10_000, None);
        assert_eq!(shorter.len, 1);
    }

    #[test]
    fn slip_slides_the_source_window_without_moving_the_region() {
        let origin = region(1000, 200, 500);
        let slipped = apply_gesture(origin, DragMode::Slip, 150.0, 0.0, 10_000, Some(480.0));
        assert_eq!(slipped.timeline_start, 1000);
        assert_eq!(slipped.len, 500);
        assert_eq!(slipped.source_start, 50, "slip is unsnapped and inverted");
        let pinned = apply_gesture(origin, DragMode::Slip, 10_000.0, 0.0, 10_000, None);
        assert_eq!(pinned.source_start, 0);
        let pinned = apply_gesture(origin, DragMode::Slip, -20_000.0, 0.0, 10_000, None);
        assert_eq!(
            pinned.source_start, 9_500,
            "clamped so the window stays inside"
        );
    }

    #[test]
    fn gain_and_fade_gestures_clamp() {
        let origin = region(1000, 200, 500);
        // Drag up 240px = +20 dB → ×10.
        let louder = apply_gesture(origin, DragMode::Gain, 0.0, -240.0, 10_000, None);
        assert!((louder.gain - 10.0).abs() < 1e-4 || louder.gain == 8.0);
        assert_eq!(louder.gain, 8.0, "gain clamps at ×8");
        let quieter = apply_gesture(origin, DragMode::Gain, 0.0, 240.0, 10_000, None);
        assert!((quieter.gain - 0.1).abs() < 1e-4);
        // Fades clamp into the region and never snap.
        let fade = apply_gesture(origin, DragMode::FadeIn, 123.0, 0.0, 10_000, Some(480.0));
        assert_eq!(fade.fade_in, 123);
        let fade = apply_gesture(origin, DragMode::FadeIn, 9999.0, 0.0, 10_000, None);
        assert_eq!(fade.fade_in, 500);
        let fade = apply_gesture(origin, DragMode::FadeOut, -321.0, 0.0, 10_000, None);
        assert_eq!(fade.fade_out, 321, "fade-out grows dragging left");
        let fade = apply_gesture(origin, DragMode::FadeOut, 50.0, 0.0, 10_000, None);
        assert_eq!(fade.fade_out, 0);
        // Geometry untouched by gain/fade gestures.
        assert_eq!(
            (louder.timeline_start, louder.source_start, louder.len),
            (1000, 200, 500)
        );
    }

    #[test]
    fn split_is_seamless_and_keeps_outer_fades() {
        let mut origin = region(1000, 200, 500);
        origin.fade_in = 40;
        origin.fade_out = 60;
        let (head, tail) = split_region(origin, 1200).expect("inside");
        assert_eq!(
            (head.timeline_start, head.source_start, head.len),
            (1000, 200, 200)
        );
        assert_eq!(
            (tail.timeline_start, tail.source_start, tail.len),
            (1200, 400, 300)
        );
        assert_eq!((head.fade_in, head.fade_out), (40, 0));
        assert_eq!((tail.fade_in, tail.fade_out), (0, 60));
        assert!(
            split_region(origin, 1000).is_none(),
            "boundary is not inside"
        );
        assert!(split_region(origin, 1500).is_none());
    }
}
