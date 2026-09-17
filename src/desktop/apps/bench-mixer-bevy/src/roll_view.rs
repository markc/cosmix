//! The roll view: a scrollable, zoomable piano roll of the dense song.
//!
//! CTK's `piano_roll` widget cannot carry this song: it spawns one UI node
//! per note for the whole song (131,072 here), has no zoom or scroll, and
//! requires the musicd mixer plugin for its playhead. This view keeps CTK's
//! look (theme track well, black-key row shading, beat/measure lines,
//! `channel_color` per track) but draws only the notes in the viewport from
//! reused node pools, so a scroll or zoom rewrites positions instead of
//! spawning and despawning nodes. Geometry and the visible set come from
//! `cosmix_bench_feed::roll`, shared with the iced arm.

use bevy::feathers::theme::{ThemeBackgroundColor, UiTheme};
use bevy::input::mouse::{MouseScrollUnit, MouseWheel};
use bevy::prelude::*;
use bevy::ui::{ComputedUiRenderTargetInfo, Overflow, UiGlobalTransform};
use bevy::window::{PrimaryWindow, RequestRedraw};
use cosmix_bench_feed::layout::{ROLL_BEAT_ALPHA, ROLL_BLACK_KEY_ALPHA, ROLL_MEASURE_ALPHA};
use cosmix_bench_feed::roll::{ROLL_WHEEL_SCROLL, ROLL_WHEEL_ZOOM, Rect};
use cosmix_bench_feed::{BenchSong, GridLine, Mode, RollViewport, roll_script};
use ctk::prelude::{ThemeState, channel_color};
use ctk::theme::tokens;

use crate::Bench;

/// A wheel tick in pixel units is worth this fraction of a line.
const PIXELS_PER_LINE: f32 = 20.0;

pub struct RollViewPlugin {
    pub song: BenchSong,
}

impl Plugin for RollViewPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(RollState::new(self.song.clone()))
            .add_systems(Startup, spawn_roll)
            .add_systems(Update, (roll_input, follow_script, draw_roll).chain());
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct DrawKey {
    view: RollViewport,
    width: f32,
    height: f32,
    theme_revision: u64,
}

#[derive(Resource)]
pub struct RollState {
    pub song: BenchSong,
    pub view: RollViewport,
    drawn: Option<DrawKey>,
    last_tick: Option<u64>,
    visible: Vec<u32>,
    grid: Vec<GridLine>,
    warned_dropped: bool,
}

impl RollState {
    pub fn new(song: BenchSong) -> Self {
        Self {
            view: RollViewport::initial(&song),
            song,
            drawn: None,
            last_tick: None,
            visible: Vec::new(),
            grid: Vec::new(),
            warned_dropped: false,
        }
    }
}

#[derive(Component)]
struct RollCanvas;

#[derive(Component)]
struct PoolItem;

/// Reused child nodes under one layer.
struct Pool {
    parent: Entity,
    items: Vec<Entity>,
    /// Items currently displayed (the first `shown` of `items`).
    shown: usize,
}

impl Pool {
    fn new(parent: Entity) -> Self {
        Self {
            parent,
            items: Vec::new(),
            shown: 0,
        }
    }
}

/// The three layers, back to front.
#[derive(Resource)]
struct RollLayers {
    rows: Pool,
    grid: Pool,
    notes: Pool,
}

fn spawn_roll(mut commands: Commands) {
    let layer = || {
        (
            Node {
                position_type: PositionType::Absolute,
                left: px(0),
                top: px(0),
                width: percent(100),
                height: percent(100),
                ..default()
            },
            Pickable::IGNORE,
        )
    };
    let rows = commands.spawn(layer()).id();
    let grid = commands.spawn(layer()).id();
    let notes = commands.spawn(layer()).id();
    let canvas = commands
        .spawn((
            Node {
                position_type: PositionType::Relative,
                width: percent(100),
                flex_grow: 1.0,
                min_height: px(0),
                overflow: Overflow::clip(),
                ..default()
            },
            ThemeBackgroundColor(tokens::TRACK),
            RollCanvas,
        ))
        .add_children(&[rows, grid, notes])
        .id();
    commands
        .spawn((
            Node {
                width: percent(100),
                height: percent(100),
                flex_direction: FlexDirection::Column,
                ..default()
            },
            ThemeBackgroundColor(tokens::SURFACE),
        ))
        .add_child(canvas);
    commands.insert_resource(RollLayers {
        rows: Pool::new(rows),
        grid: Pool::new(grid),
        notes: Pool::new(notes),
    });
}

/// The viewport after `lines` wheel lines (positive is wheel-up): with ctrl
/// wheel-up zooms in about `anchor`, otherwise wheel-up scrolls back.
pub fn wheel_view(
    view: RollViewport,
    song: &BenchSong,
    lines: f64,
    ctrl: bool,
    anchor: f64,
) -> RollViewport {
    if lines == 0.0 {
        view
    } else if ctrl {
        view.zoomed(song, anchor, ROLL_WHEEL_ZOOM.powf(-lines))
    } else {
        view.scrolled(song, -lines * ROLL_WHEEL_SCROLL)
    }
}

/// Wheel input over the roll. Roll mode's script owns the viewport, so input
/// is drained without effect there.
#[allow(clippy::too_many_arguments)] // Bevy system parameters.
fn roll_input(
    bench: Res<Bench>,
    mut state: ResMut<RollState>,
    mut wheel: MessageReader<MouseWheel>,
    keys: Res<ButtonInput<KeyCode>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    canvas: Query<(&ComputedNode, &UiGlobalTransform, &ComputedUiRenderTargetInfo), With<RollCanvas>>,
    ui_scale: Res<UiScale>,
) {
    if bench.config.mode == Mode::Roll {
        wheel.clear();
        return;
    }
    let lines: f32 = wheel
        .read()
        .map(|message| {
            let scale = match message.unit {
                MouseScrollUnit::Line => 1.0,
                MouseScrollUnit::Pixel => 1.0 / PIXELS_PER_LINE,
            };
            (message.y + message.x) * scale
        })
        .sum();
    if lines == 0.0 {
        return;
    }
    let Ok((node, transform, target)) = canvas.single() else {
        return;
    };
    let Some(pointer) = windows
        .single()
        .ok()
        .and_then(|window| window.cursor_position())
        .and_then(|cursor| {
            node.normalize_point(*transform, cursor * target.scale_factor() / ui_scale.0)
        })
        .filter(|n| n.x.abs() <= 0.5 && n.y.abs() <= 0.5)
    else {
        return;
    };
    let ctrl = keys.any_pressed([KeyCode::ControlLeft, KeyCode::ControlRight]);
    let anchor = f64::from(pointer.x) + 0.5;
    let RollState { song, view, .. } = &mut *state;
    *view = wheel_view(*view, song, f64::from(lines), ctrl, anchor);
}

fn follow_script(bench: Res<Bench>, mut state: ResMut<RollState>) {
    if bench.config.mode != Mode::Roll {
        return;
    }
    let tick = bench.tick();
    if state.last_tick == Some(tick) {
        return;
    }
    state.last_tick = Some(tick);
    let view = roll_script(&state.song, tick);
    state.view = view;
}

pub fn rect_node(rect: Rect) -> Node {
    Node {
        position_type: PositionType::Absolute,
        left: px(rect.x),
        top: px(rect.y),
        width: px(rect.w),
        height: px(rect.h),
        ..default()
    }
}

/// Bring a pool to exactly `wanted`: reuse existing items, spawn the
/// shortfall, hide the surplus that was showing.
fn sync_pool(
    pool: &mut Pool,
    wanted: impl Iterator<Item = (Node, Color)>,
    commands: &mut Commands,
    items: &mut Query<(&mut Node, &mut BackgroundColor), With<PoolItem>>,
) {
    let mut count = 0;
    for (node, color) in wanted {
        if let Some(&entity) = pool.items.get(count) {
            if let Ok((mut current, mut background)) = items.get_mut(entity) {
                current.set_if_neq(node);
                background.set_if_neq(BackgroundColor(color));
            }
        } else {
            let entity = commands
                .spawn((
                    node,
                    BackgroundColor(color),
                    Pickable::IGNORE,
                    PoolItem,
                    ChildOf(pool.parent),
                ))
                .id();
            pool.items.push(entity);
        }
        count += 1;
    }
    for &entity in &pool.items[count..pool.shown.max(count)] {
        if let Ok((mut node, _)) = items.get_mut(entity) {
            node.display = Display::None;
        }
    }
    pool.shown = count;
}

/// Redraw when the viewport, canvas size or theme changed.
#[allow(clippy::too_many_arguments)] // Bevy system parameters.
fn draw_roll(
    mut state: ResMut<RollState>,
    theme: Res<UiTheme>,
    theme_state: Res<ThemeState>,
    canvas: Query<&ComputedNode, With<RollCanvas>>,
    layers: Option<ResMut<RollLayers>>,
    mut items: Query<(&mut Node, &mut BackgroundColor), With<PoolItem>>,
    mut redraw: MessageWriter<RequestRedraw>,
    mut commands: Commands,
) {
    let (Some(mut layers), Ok(computed)) = (layers, canvas.single()) else {
        return;
    };
    let size = computed.size() * computed.inverse_scale_factor();
    if size.x <= 0.0 || size.y <= 0.0 {
        // Layout has not run yet; come back once it has, even when idle.
        redraw.write(RequestRedraw);
        return;
    }
    let key = DrawKey {
        view: state.view,
        width: size.x,
        height: size.y,
        theme_revision: theme_state.revision,
    };
    if state.drawn == Some(key) {
        return;
    }
    state.drawn = Some(key);

    let accent = theme.color(&tokens::CONTROL_ACTIVE);
    let ink = theme.color(&tokens::TEXT);
    let RollState {
        song,
        view,
        visible,
        grid,
        warned_dropped,
        ..
    } = &mut *state;
    let track_colors: Vec<Color> = (0..song.track_count.max(1))
        .map(|track| channel_color(track as u32, accent))
        .collect();
    let (width, height) = (size.x, size.y);

    let shade = ink.with_alpha(ROLL_BLACK_KEY_ALPHA);
    let rows = view.black_key_rows(height).map(|(_, y, h)| {
        (
            rect_node(Rect {
                x: 0.0,
                y,
                w: width,
                h,
            }),
            shade,
        )
    });
    sync_pool(&mut layers.rows, rows, &mut commands, &mut items);

    view.gridlines_into(song, grid);
    let lines = grid.iter().map(|line| {
        let alpha = if line.measure {
            ROLL_MEASURE_ALPHA
        } else {
            ROLL_BEAT_ALPHA
        };
        (
            rect_node(Rect {
                x: view.tick_x(f64::from(line.tick), width).floor(),
                y: 0.0,
                w: 1.0,
                h: height,
            }),
            ink.with_alpha(alpha),
        )
    });
    sync_pool(&mut layers.grid, lines, &mut commands, &mut items);

    let dropped = view.visible_notes_into(song, visible);
    if dropped > 0 && !*warned_dropped {
        *warned_dropped = true;
        eprintln!("bench-mixer-bevy: viewport over the note cap; {dropped} notes not drawn");
    }
    let notes = visible.iter().map(|index| {
        let note = &song.notes[*index as usize];
        (
            rect_node(view.note_rect(note, width, height)),
            track_colors[usize::from(note.track) % track_colors.len()],
        )
    });
    sync_pool(&mut layers.notes, notes, &mut commands, &mut items);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, View, add_bench_plugins};
    use bevy::winit::WinitSettings;
    use cosmix_bench_feed::DEFAULT_SEED;

    /// A generated song: 2 tracks, 8 bars of eighth notes, loaded through
    /// the SMF path the app uses.
    fn song() -> BenchSong {
        let mut doc = cosmix_song::Song::new("roll-test");
        for track in 0..2u8 {
            let mut t = cosmix_song::Track::new(format!("T{track}"), track);
            for step in 0..64u32 {
                t.add_note(cosmix_song::Note::new(50 + (step % 12) as u8, 100, step * 240, 200));
            }
            doc.add_track(t);
        }
        BenchSong::from_smf_bytes(&cosmix_song::export_smf_bytes(&doc), "roll-test").unwrap()
    }

    #[test]
    fn test_song_loads() {
        let song = song();
        assert_eq!(song.notes.len(), 128);
        assert_eq!(song.length_ticks, 8 * 1920);
    }

    #[test]
    fn wheel_scrolls_and_zooms() {
        let song = song();
        let view = RollViewport::initial(&song);
        assert_eq!(wheel_view(view, &song, 0.0, true, 0.5), view);
        let forward = wheel_view(view, &song, -1.0, false, 0.5);
        assert_eq!(forward.start, view.span * ROLL_WHEEL_SCROLL);
        assert_eq!(forward.span, view.span);
        let back = wheel_view(forward, &song, 1.0, false, 0.5);
        assert_eq!(back.start, 0.0);
        let zoomed_in = wheel_view(view, &song, 1.0, true, 0.0);
        assert!((zoomed_in.span - view.span / ROLL_WHEEL_ZOOM).abs() < 1e-9);
        let zoomed_out = wheel_view(view, &song, -1.0, true, 0.0);
        assert!((zoomed_out.span - view.span * ROLL_WHEEL_ZOOM).abs() < 1e-9);
    }

    #[test]
    fn note_nodes_use_the_shared_geometry() {
        let song = song();
        let view = RollViewport::initial(&song);
        let note = song.notes[3];
        let node = rect_node(view.note_rect(&note, 1600.0, 800.0));
        let rect = view.note_rect(&note, 1600.0, 800.0);
        assert_eq!(node.left, px(rect.x));
        assert_eq!(node.top, px(rect.y));
        assert_eq!(node.width, px(rect.w));
        assert_eq!(node.height, px(rect.h));
        assert_eq!(node.position_type, PositionType::Absolute);
    }

    #[test]
    fn roll_mode_follows_the_script_headless() {
        let mut app = App::new();
        app.set_error_handler(bevy::ecs::error::ignore)
            .add_plugins(MinimalPlugins)
            .init_resource::<WinitSettings>();
        let song = song();
        add_bench_plugins(
            &mut app,
            Config {
                mode: Mode::Roll,
                view: View::Roll,
                strips: 64,
                seed: DEFAULT_SEED,
                song: None,
                scripted_drag: true,
            },
            Some(song.clone()),
        );
        app.finish();
        app.cleanup();
        for tick in [0, 150, 300] {
            app.world_mut().resource_mut::<Bench>().forced_tick = Some(tick);
            app.update();
            assert_eq!(
                app.world().resource::<RollState>().view,
                roll_script(&song, tick)
            );
        }
    }
}
