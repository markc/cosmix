//! The CTK piano-roll — a note grid + live playhead over the mixer transport.
//!
//! The widget is **document-agnostic**: it renders a [`PianoRollModel`]
//! resource (plain seconds-domain note views the app derives from its own song
//! document) and emits [`PianoRollClick`]s; the app owns editing semantics
//! (which track, snapping, undo) and writes a fresh model back after every
//! edit. The playhead follows the same extrapolated transport clock as the
//! scrubber ([`TransportPosition`]), so the two never disagree.
//!
//! Rendering is plain `bevy_ui` nodes, percent-positioned against the grid, so
//! the roll reflows with the window with no per-frame layout math: rebuilds
//! happen only when the model changes (an edit), the playhead is one node
//! whose `left` moves per frame.

use bevy::app::{App, Plugin, Update};
use bevy::ecs::change_detection::DetectChanges;
use bevy::ecs::entity::Entity;
use bevy::ecs::event::EntityEvent;
use bevy::ecs::observer::On;
use bevy::ecs::query::With;
use bevy::ecs::resource::Resource;
use bevy::ecs::system::{Commands, Local, Query, Res};
use bevy::feathers::theme::ThemeBackgroundColor;
use bevy::picking::events::{Click, Pointer};
use bevy::picking::Pickable;
use bevy::prelude::{
    default, Alpha, BackgroundColor, Color, Component, ComputedNode, ComputedUiRenderTargetInfo,
    Node, PositionType, UiGlobalTransform, UiScale,
};
use bevy::ui::{percent, px};

use crate::mixer::{
    transport_is_playing, transport_length_secs, MusicdMixerState, TransportPosition,
};

/// One rendered note: a seconds-domain view of a song note. `id` round-trips
/// through [`PianoRollClick`] so the app can map a click back to its document.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PianoRollNote {
    pub id: u64,
    /// Colour family (the owning track / mixer channel).
    pub channel: u32,
    /// MIDI key (0-127).
    pub key: u8,
    pub start_secs: f32,
    pub dur_secs: f32,
}

/// The piano-roll's render model. The app rewrites this after every edit (and
/// once at load); the widget rebuilds its note nodes on change detection.
#[derive(Resource, Default)]
pub struct PianoRollModel {
    pub notes: Vec<PianoRollNote>,
    /// Horizontal domain: the roll spans `[0, length_secs]`.
    pub length_secs: f32,
    /// Beat period in seconds (0 = no beat gridlines).
    pub beat_secs: f32,
    /// Beats per measure (measure gridlines are drawn brighter).
    pub beats_per_measure: u32,
}

/// A pointer click on the roll, resolved to the musical domain. `existing` is
/// the note under the pointer (its [`PianoRollNote::id`]) when the click hit a
/// note node — the app decides what a click means (toggle, select, …).
#[derive(EntityEvent, Clone, Copy, Debug, PartialEq)]
pub struct PianoRollClick {
    /// The grid entity the click resolved against.
    #[event_target]
    pub grid: Entity,
    /// MIDI key row under the pointer.
    pub key: u8,
    /// Unsnapped time under the pointer, seconds.
    pub secs: f32,
    /// The note under the pointer, if the click landed on one.
    pub existing: Option<u64>,
}

/// The clickable note canvas. Carries the vertical key window the current
/// rebuild rendered, so click hit-testing and note layout always agree.
#[derive(Component)]
pub struct PianoRollGrid {
    pub key_lo: u8,
    pub key_hi: u8,
    length_secs: f32,
}

/// Marker: a rebuild-owned child of the grid (note, row shade, gridline).
#[derive(Component)]
struct PianoRollRebuildNode;

/// Marker + payload: a rendered note node.
#[derive(Component)]
struct PianoRollNoteNode {
    id: u64,
}

/// Marker: the playhead line.
#[derive(Component)]
struct PianoRollPlayhead;

/// Entities returned by [`spawn_piano_roll`].
pub struct PianoRollEntities {
    pub root: Entity,
    pub grid: Entity,
}

/// Minimum key rows rendered, so sparse songs don't blow single rows up to
/// comic height.
const MIN_KEY_SPAN: u8 = 24;
/// Rows padded above/below the songs' actual key range.
const KEY_MARGIN: u8 = 2;
/// Gridline cap: past this many beat lines only measures are drawn; past this
/// many measure lines none are (a long song would otherwise spawn thousands of
/// 1-px nodes).
const MAX_GRIDLINES: u32 = 400;

/// Per-channel colour: a stable golden-angle hue walk, readable on the dark
/// board without a theme dependency. Shared identity across every view that
/// colours by track (the roll's notes, the waves view's lanes).
/// A per-channel colour derived from the theme `accent`: distinct SHADES within
/// the scheme's family, not a full-wheel rainbow. The hue fans a bounded ±55°
/// around the accent and the lightness spreads across a band (the main
/// separator), distributed with the golden ratio so any track count stays
/// well-spread. So an Ocean scheme yields cool blues/teals/purples, a Crimson
/// scheme warm reds/oranges/pinks, etc. Channel 0 IS the accent; a greyscale
/// (Mono) scheme — accent chroma 0 — yields near-greys separated only by
/// lightness, by design.
pub fn channel_color(channel: u32, accent: Color) -> Color {
    let ok = bevy::color::Oklcha::from(accent);
    if channel == 0 {
        return Color::oklch(ok.lightness, ok.chroma, ok.hue);
    }
    // Low-discrepancy fraction in [0, 1) — even spread for any channel count.
    let g = (channel as f32 * 0.618_034).fract();
    let hue = (ok.hue + (g - 0.5) * 110.0).rem_euclid(360.0);
    let lightness = (ok.lightness + (g - 0.5) * 0.30).clamp(0.35, 0.92);
    Color::oklch(lightness, ok.chroma, hue)
}

/// Spawn the piano-roll: a full-width panel of `height_px` holding the note
/// canvas + playhead. Returns the root (place it in your layout) and the grid
/// (observe [`PianoRollClick`] on it, or globally).
pub fn spawn_piano_roll(commands: &mut Commands, height_px: f32) -> PianoRollEntities {
    let grid = commands
        .spawn((
            Node {
                position_type: PositionType::Relative,
                width: percent(100),
                height: percent(100),
                overflow: bevy::ui::Overflow::clip(),
                ..default()
            },
            ThemeBackgroundColor(crate::theme::tokens::TRACK),
            PianoRollGrid {
                key_lo: 48,
                key_hi: 48 + MIN_KEY_SPAN,
                length_secs: 0.0,
            },
        ))
        .id();
    // The playhead is spawned once and only ever moves — it survives rebuilds.
    let playhead = commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                left: percent(0),
                top: percent(0),
                width: px(1),
                height: percent(100),
                ..default()
            },
            ThemeBackgroundColor(crate::theme::tokens::METER_RED),
            Pickable::IGNORE,
            PianoRollPlayhead,
        ))
        .id();
    commands.entity(grid).add_children(&[playhead]);

    let root = commands
        .spawn((Node {
            width: percent(100),
            height: px(height_px),
            ..default()
        },))
        .add_children(&[grid])
        .id();
    PianoRollEntities { root, grid }
}

/// Widget systems: model-driven rebuild, playhead follow, click resolution.
/// Requires [`crate::mixer::MusicdMixerPlugin`] (transport state + clock).
pub struct PianoRollPlugin;

impl Plugin for PianoRollPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PianoRollModel>()
            .init_resource::<crate::theme::ThemeState>()
            .add_observer(on_grid_click)
            .add_systems(Update, (rebuild_on_model_change, follow_playhead));
    }
}

/// Rebuild the grid's children (row shades, gridlines, notes) whenever the
/// model changes. The playhead node is untouched.
fn rebuild_on_model_change(
    model: Res<PianoRollModel>,
    theme: Res<bevy::feathers::theme::UiTheme>,
    theme_state: Res<crate::theme::ThemeState>,
    mut last_theme_revision: Local<u64>,
    mut grids: Query<(Entity, &mut PianoRollGrid)>,
    stale: Query<Entity, With<PianoRollRebuildNode>>,
    mut commands: Commands,
) {
    let theme_changed = *last_theme_revision != theme_state.revision;
    if !model.is_changed() && !theme_changed {
        return;
    }
    *last_theme_revision = theme_state.revision;
    let accent = crate::theme::ctk_color(&theme, &crate::theme::tokens::CONTROL_ACTIVE);
    let grid_color = crate::theme::ctk_color(&theme, &crate::theme::tokens::TEXT);
    for entity in &stale {
        commands.entity(entity).despawn();
    }
    for (grid_entity, mut grid) in &mut grids {
        // Vertical window: the song's key range, padded, floored to a sane span.
        let (mut lo, mut hi) = model
            .notes
            .iter()
            .fold((127u8, 0u8), |(lo, hi), n| (lo.min(n.key), hi.max(n.key)));
        if lo > hi {
            (lo, hi) = (48, 72); // empty song: centre around the octaves at C3..C5
        }
        lo = lo.saturating_sub(KEY_MARGIN);
        hi = (hi + KEY_MARGIN).min(127);
        while hi - lo < MIN_KEY_SPAN {
            lo = lo.saturating_sub(1);
            hi = (hi + 1).min(127);
            if lo == 0 && hi == 127 {
                break;
            }
        }
        grid.key_lo = lo;
        grid.key_hi = hi;
        grid.length_secs = model.length_secs;
        let rows = (hi - lo + 1) as f32;
        let length = model.length_secs.max(f32::EPSILON);

        commands.entity(grid_entity).with_children(|parent| {
            // Black-key row shading — the pitch landmarks.
            for key in lo..=hi {
                if matches!(key % 12, 1 | 3 | 6 | 8 | 10) {
                    let row_from_top = (hi - key) as f32;
                    parent.spawn((
                        Node {
                            position_type: PositionType::Absolute,
                            left: percent(0),
                            top: percent(row_from_top / rows * 100.0),
                            width: percent(100),
                            height: percent(100.0 / rows),
                            ..default()
                        },
                        BackgroundColor(grid_color.with_alpha(0.035)),
                        Pickable::IGNORE,
                        PianoRollRebuildNode,
                    ));
                }
            }

            // Beat / measure gridlines, capped for long songs.
            if model.beat_secs > 0.0 {
                let beats = (model.length_secs / model.beat_secs) as u32;
                let per_measure = model.beats_per_measure.max(1);
                let (step, measures_only) = if beats <= MAX_GRIDLINES {
                    (1, false)
                } else if beats / per_measure <= MAX_GRIDLINES {
                    (per_measure, true)
                } else {
                    (0, true)
                };
                if step > 0 {
                    for beat in (step..=beats).step_by(step as usize) {
                        let is_measure = beat % per_measure == 0;
                        let alpha = if is_measure { 0.16 } else { 0.07 };
                        if measures_only && !is_measure {
                            continue;
                        }
                        parent.spawn((
                            Node {
                                position_type: PositionType::Absolute,
                                left: percent(beat as f32 * model.beat_secs / length * 100.0),
                                top: percent(0),
                                width: px(1),
                                height: percent(100),
                                ..default()
                            },
                            BackgroundColor(grid_color.with_alpha(alpha)),
                            Pickable::IGNORE,
                            PianoRollRebuildNode,
                        ));
                    }
                }
            }

            // The notes. Pickable (NOT Pickable::IGNORE): a click on a note
            // bubbles to the grid with the note as original target, which is
            // how `on_grid_click` resolves `existing`.
            for note in &model.notes {
                if note.key < lo || note.key > hi {
                    continue;
                }
                let row_from_top = (hi - note.key) as f32;
                parent.spawn((
                    Node {
                        position_type: PositionType::Absolute,
                        left: percent(note.start_secs / length * 100.0),
                        top: percent(row_from_top / rows * 100.0 + 0.5),
                        width: percent((note.dur_secs / length * 100.0).max(0.15)),
                        height: percent((100.0 / rows - 1.0).max(1.0)),
                        ..default()
                    },
                    BackgroundColor(channel_color(note.channel, accent)),
                    PianoRollNoteNode { id: note.id },
                    PianoRollRebuildNode,
                ));
            }
        });
    }
}

/// Move the playhead to the live extrapolated transport clock — the same
/// value the scrubber follows, so the two lines never diverge.
fn follow_playhead(
    transport: Res<TransportPosition>,
    state: Res<MusicdMixerState>,
    model: Res<PianoRollModel>,
    mut playheads: Query<&mut Node, With<PianoRollPlayhead>>,
) {
    let length = model.length_secs.max(f32::EPSILON);
    // The roll spans the SONG's length (which may differ from the store's
    // transport.length leaf); clamp into the rendered domain.
    let live = transport.live_seconds(
        transport_is_playing(&state),
        transport_length_secs(&state) as f64,
    ) as f32;
    let frac = (live / length).clamp(0.0, 1.0);
    for mut node in &mut playheads {
        node.left = percent(frac * 100.0);
    }
}

/// Resolve a pointer click against the grid geometry into a musical-domain
/// [`PianoRollClick`] (bubbled note clicks carry `existing`).
fn on_grid_click(
    click: On<Pointer<Click>>,
    grids: Query<(
        &PianoRollGrid,
        &ComputedNode,
        &ComputedUiRenderTargetInfo,
        &UiGlobalTransform,
    )>,
    notes: Query<&PianoRollNoteNode>,
    ui_scale: Res<UiScale>,
    mut commands: Commands,
) {
    let Ok((grid, node, target, transform)) = grids.get(click.entity) else {
        return;
    };
    if grid.length_secs <= 0.0 {
        return;
    }
    let Some(normalised) = node.normalize_point(
        *transform,
        click.pointer_location.position * target.scale_factor() / ui_scale.0,
    ) else {
        return;
    };
    // normalize_point yields node-centred [-0.5, 0.5] on both axes.
    let fx = (normalised.x + 0.5).clamp(0.0, 1.0);
    let fy_top = (normalised.y + 0.5).clamp(0.0, 1.0);
    let rows = (grid.key_hi - grid.key_lo + 1) as f32;
    let row = ((fy_top * rows) as u8).min(grid.key_hi - grid.key_lo);
    let key = grid.key_hi - row;
    let secs = fx * grid.length_secs;
    let existing = notes.get(click.original_event_target()).ok().map(|n| n.id);
    commands.trigger(PianoRollClick {
        grid: click.entity,
        key,
        secs,
        existing,
    });
}
