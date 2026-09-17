//! The mixer view: every channel strip plus the master, built from CTK's
//! stock controls and placed at the rectangles `cosmix_bench_feed::layout`
//! resolves, so the iced arm and the bake-off driver read the same geometry.
//!
//! CTK's own `spawn_channel_strip_styled` cannot be used: it asserts the
//! 32-channel musicd cap and binds every control to the musicd mixer store.
//! The strips here carry the same widgets, minus the trim knob and the value
//! readouts the bake-off surface does not have.

use bevy::feathers::theme::{ThemeBackgroundColor, ThemeTextColor, ThemeToken};
use bevy::prelude::*;
use bevy::text::{Justify, LineBreak, TextLayout};
use bevy::ui::{Checked, Overflow};
use cosmix_bench_feed::layout::{BUTTON_FONT, Layout, NAME_FONT, Rect, StripLayout};
use cosmix_bench_feed::{FADER_MAX_DB, FADER_MIN_DB, MeterFrame, MixerFeed};
use ctk::prelude::{
    ControlRange, MeterLane as CtkMeterLane, MeterValue, NumericControlProps, SetControlValue,
    ValueMapping, default_fader_mapping, fader_sized, knob_sized, level_meter_sized,
    toggle_button_sized,
};
use ctk::theme::tokens;

use crate::Bench;

pub struct MixerViewPlugin;

impl Plugin for MixerViewPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_mixer)
            .add_systems(Update, apply_feed_tick);
    }
}

/// The per-slot entities the feed writes to, indexed by slot (master last).
#[derive(Resource)]
pub struct MixerEntities {
    pub feed: MixerFeed,
    pub faders: Vec<Entity>,
    pub meters: Vec<Entity>,
    pub pans: Vec<Option<Entity>>,
    pub mutes: Vec<Entity>,
    pub solos: Vec<Option<Entity>>,
    /// The last tick written, so a frame woken by input alone writes nothing.
    last_tick: Option<u64>,
}

fn spawn_mixer(mut commands: Commands, bench: Res<Bench>) {
    let config = &bench.config;
    let feed = MixerFeed::new(config.seed, config.strips, config.mode);
    let layout = Layout::new((config.size.0 as f32, config.size.1 as f32), config.strips);
    let mut entities = MixerEntities {
        feed,
        faders: Vec::new(),
        meters: Vec::new(),
        pans: Vec::new(),
        mutes: Vec::new(),
        solos: Vec::new(),
        last_tick: None,
    };
    let strips: Vec<Entity> = layout
        .slots
        .iter()
        .map(|strip| spawn_strip(&mut commands, &feed, strip, &mut entities))
        .collect();
    commands
        .spawn((
            Node {
                position_type: PositionType::Relative,
                width: percent(100),
                height: percent(100),
                ..default()
            },
            ThemeBackgroundColor(tokens::SURFACE),
        ))
        .add_children(&strips);
    commands.insert_resource(MixerLayout(layout));
    commands.insert_resource(entities);
}

/// The resolved board, kept so tests and any later input wiring read the same
/// rectangles the widgets were placed at.
#[derive(Resource)]
pub struct MixerLayout(pub Layout);

/// An absolutely-placed node at `rect`, in the coordinates of a parent that
/// spans the same space (the board root, or a strip panel for its children).
fn placed(rect: Rect, origin: (f32, f32)) -> Node {
    Node {
        position_type: PositionType::Absolute,
        left: px(rect.x - origin.0),
        top: px(rect.y - origin.1),
        width: px(rect.w),
        height: px(rect.h),
        ..default()
    }
}

/// Place an already-spawned widget (a CTK bundle brings its own size) at
/// `rect` inside a parent whose origin is `origin`.
fn place(commands: &mut Commands, entity: Entity, rect: Rect, origin: (f32, f32)) {
    commands
        .entity(entity)
        .entry::<Node>()
        .and_modify(move |mut node| {
            node.position_type = PositionType::Absolute;
            node.left = px(rect.x - origin.0);
            node.top = px(rect.y - origin.1);
            node.width = px(rect.w);
            node.height = px(rect.h);
        });
}

fn text(content: impl Into<String>, size: f32, token: ThemeToken) -> impl Bundle {
    (
        Text::new(content),
        TextFont::from_font_size(size),
        ThemeTextColor(token),
        Pickable::IGNORE,
    )
}

fn fader_props(id: String, db: f32) -> NumericControlProps {
    NumericControlProps::new(
        id,
        db,
        ControlRange {
            min: FADER_MIN_DB,
            max: FADER_MAX_DB,
            step: 0.1,
            detent: Some(0.0),
        },
        default_fader_mapping(),
    )
}

fn pan_props(id: String, pan: f32) -> NumericControlProps {
    NumericControlProps::new(
        id,
        pan,
        ControlRange {
            min: -1.0,
            max: 1.0,
            step: 1.0 / 512.0,
            detent: Some(0.0),
        },
        ValueMapping::linear(-1.0, 1.0).expect("static pan mapping is valid"),
    )
}

fn toggle(
    commands: &mut Commands,
    id: String,
    label: &str,
    on: bool,
    rect: Rect,
    origin: (f32, f32),
) -> Entity {
    let mut entity = commands.spawn(toggle_button_sized(id, rect.w, rect.h));
    entity.with_child(text(label, BUTTON_FONT, tokens::TEXT));
    if on {
        entity.insert(Checked);
    }
    let entity = entity.id();
    place(commands, entity, rect, origin);
    entity
}

/// Spawn one strip's panel and widgets at the rectangles `strip` gives.
fn spawn_strip(
    commands: &mut Commands,
    feed: &MixerFeed,
    strip: &StripLayout,
    out: &mut MixerEntities,
) -> Entity {
    let slot = strip.slot;
    let state = feed.strip(slot);
    let origin = (strip.rect.x, strip.rect.y);

    // One line, centred, clipped to the strip.
    let name = commands
        .spawn((
            Node {
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                overflow: Overflow::clip(),
                ..placed(strip.name, origin)
            },
            Pickable::IGNORE,
        ))
        .with_child((
            text(state.name.clone(), NAME_FONT, tokens::TEXT),
            TextLayout {
                justify: Justify::Center,
                linebreak: LineBreak::NoWrap,
                ..default()
            },
        ))
        .id();

    let pan = strip.knob.map(|rect| {
        let entity = commands
            .spawn(knob_sized(
                pan_props(format!("strip-{slot}-pan"), state.pan),
                rect.w,
            ))
            .id();
        place(commands, entity, rect, origin);
        entity
    });

    let fader = commands
        .spawn(fader_sized(
            fader_props(format!("strip-{slot}-fader"), feed.fader_db(slot, 0)),
            strip.fader.w,
            strip.fader.h,
        ))
        .id();
    place(commands, fader, strip.fader, origin);
    let meter = commands
        .spawn(level_meter_sized(
            format!("strip-{slot}-meter"),
            meter_value(&MeterFrame::default()),
            strip.meter.w,
            strip.meter.h,
        ))
        .id();
    place(commands, meter, strip.meter, origin);

    let mute = toggle(
        commands,
        format!("strip-{slot}-mute"),
        "M",
        state.mute,
        strip.mute,
        origin,
    );
    let solo = strip.solo.map(|rect| {
        toggle(
            commands,
            format!("strip-{slot}-solo"),
            "S",
            state.solo,
            rect,
            origin,
        )
    });

    out.faders.push(fader);
    out.meters.push(meter);
    out.pans.push(pan);
    out.mutes.push(mute);
    out.solos.push(solo);

    let mut children = vec![name, fader, meter, mute];
    children.extend(pan);
    children.extend(solo);
    commands
        .spawn((
            placed(strip.rect, (0.0, 0.0)),
            ThemeBackgroundColor(if strip.is_master {
                tokens::MASTER_PANEL
            } else {
                tokens::PANEL
            }),
        ))
        .add_children(&children)
        .id()
}

pub fn meter_value(frame: &MeterFrame) -> MeterValue {
    let lane = |i: usize| {
        let lane = frame.lanes[i];
        CtkMeterLane {
            level: lane.level,
            peak: lane.peak,
            hold: lane.hold,
            clipped: lane.clipped,
        }
    };
    MeterValue {
        lanes: [lane(0), lane(1)],
        lane_count: 2,
    }
}

/// Write the feed's state for this frame's tick: every meter, and the
/// scripted fader in drag mode. Nothing is written twice for one tick, and a
/// meter whose reading did not change is left untouched.
fn apply_feed_tick(
    bench: Res<Bench>,
    mixer: Option<ResMut<MixerEntities>>,
    mut meters: Query<&mut MeterValue>,
    mut commands: Commands,
) {
    let Some(mut mixer) = mixer else { return };
    let tick = bench.tick();
    if mixer.last_tick == Some(tick) {
        return;
    }
    let first = mixer.last_tick.is_none();
    mixer.last_tick = Some(tick);
    let feed = mixer.feed;
    if feed.mode().animates_meters() {
        for (slot, entity) in mixer.meters.iter().enumerate() {
            if let Ok(mut meter) = meters.get_mut(*entity) {
                let next = meter_value(&feed.meter(slot, tick));
                meter.set_if_neq(next);
            }
        }
    }
    if bench.config.scripted_drag
        && !first
        && let Some(drag) = feed.drag_sample(tick)
    {
        commands.trigger(SetControlValue {
            source: mixer.faders[drag.strip],
            value: drag.db,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, View, add_bench_plugins};
    use bevy::winit::WinitSettings;
    use cosmix_bench_feed::{DEFAULT_SEED, Mode, fader_position};
    use ctk::prelude::{ControlValue, Fader, Knob, LevelMeter, ToggleButton};

    fn app(mode: Mode, strips: usize) -> App {
        let mut app = App::new();
        app.set_error_handler(bevy::ecs::error::ignore)
            .add_plugins(MinimalPlugins)
            .init_resource::<WinitSettings>();
        add_bench_plugins(
            &mut app,
            Config {
                mode,
                view: View::Mixer,
                strips,
                seed: DEFAULT_SEED,
                song: None,
                size: (1024, 576),
                scripted_drag: true,
            },
            None,
        );
        app.finish();
        app.cleanup();
        app
    }

    fn set_tick(app: &mut App, tick: u64) {
        app.world_mut().resource_mut::<Bench>().forced_tick = Some(tick);
    }

    fn count<C: Component>(app: &mut App) -> usize {
        app.world_mut()
            .query_filtered::<(), With<C>>()
            .iter(app.world())
            .count()
    }

    #[test]
    fn ctk_taper_matches_the_feed_taper() {
        let mapping = default_fader_mapping();
        for step in 0..=252 {
            let db = FADER_MIN_DB + step as f32 * 0.5;
            let ctk = mapping.to_position(db);
            assert!(
                (ctk - fader_position(db)).abs() < 1e-5,
                "{db} dB: ctk {ctk} feed {}",
                fader_position(db)
            );
        }
    }

    #[test]
    fn board_has_every_control() {
        let mut app = app(Mode::Idle, 64);
        set_tick(&mut app, 0);
        app.update();
        assert_eq!(count::<Fader>(&mut app), 65);
        assert_eq!(count::<LevelMeter>(&mut app), 65);
        assert_eq!(count::<Knob>(&mut app), 64);
        assert_eq!(count::<ToggleButton>(&mut app), 64 * 2 + 1);
        let mixer = app.world().resource::<MixerEntities>();
        assert_eq!(mixer.faders.len(), 65);
        assert!(mixer.pans[64].is_none() && mixer.solos[64].is_none());

        // Initial control state is the feed's.
        let feed = mixer.feed;
        let (faders, pans, mutes) = (
            mixer.faders.clone(),
            mixer.pans.clone(),
            mixer.mutes.clone(),
        );
        for slot in 0..65 {
            let state = feed.strip(slot);
            let world = app.world();
            let fader = world.get::<ControlValue>(faders[slot]).unwrap().0;
            assert!((fader - state.fader_db).abs() < 1e-3, "slot {slot}");
            if let Some(pan) = pans[slot] {
                assert!((world.get::<ControlValue>(pan).unwrap().0 - state.pan).abs() < 1e-6);
            }
            assert_eq!(world.get::<Checked>(mutes[slot]).is_some(), state.mute);
        }
    }

    #[test]
    fn widgets_sit_at_the_shared_layout_rects() {
        let mut app = app(Mode::Idle, 64);
        set_tick(&mut app, 0);
        app.update();
        let layout = &app.world().resource::<MixerLayout>().0;
        assert_eq!(layout.rows, 3);
        let mixer = app.world().resource::<MixerEntities>();
        let (faders, meters, mutes) = (
            mixer.faders.clone(),
            mixer.meters.clone(),
            mixer.mutes.clone(),
        );
        // Widgets are strip-relative; the strip panel is at the board origin.
        for slot in [0usize, 21, 22, 64] {
            let strip = layout.slot(slot);
            let origin = (strip.rect.x, strip.rect.y);
            for (entity, rect) in [
                (faders[slot], strip.fader),
                (meters[slot], strip.meter),
                (mutes[slot], strip.mute),
            ] {
                let node = app.world().get::<Node>(entity).unwrap();
                assert_eq!(node.left, px(rect.x - origin.0), "slot {slot}");
                assert_eq!(node.top, px(rect.y - origin.1), "slot {slot}");
                assert_eq!(node.width, px(rect.w), "slot {slot}");
                assert_eq!(node.height, px(rect.h), "slot {slot}");
            }
        }
        // The drag point the driver aims at is inside the dragged fader.
        let point = layout.fader_point(32, 0.75);
        let fader = layout.slot(32).fader;
        assert!(point.0 > fader.x && point.0 < fader.right());
        assert!(point.1 > fader.y && point.1 < fader.bottom());
    }

    #[test]
    fn idle_meters_never_change() {
        let mut app = app(Mode::Idle, 4);
        for tick in [0, 1, 2, 50] {
            set_tick(&mut app, tick);
            app.update();
        }
        let meters = app.world().resource::<MixerEntities>().meters.clone();
        for entity in meters {
            assert_eq!(
                *app.world().get::<MeterValue>(entity).unwrap(),
                meter_value(&MeterFrame::default())
            );
        }
    }

    #[test]
    fn animated_meters_follow_the_feed() {
        let mut app = app(Mode::Meters, 8);
        for tick in [0, 1, 7] {
            set_tick(&mut app, tick);
            app.update();
            let mixer = app.world().resource::<MixerEntities>();
            let (feed, meters) = (mixer.feed, mixer.meters.clone());
            for (slot, entity) in meters.into_iter().enumerate() {
                assert_eq!(
                    *app.world().get::<MeterValue>(entity).unwrap(),
                    meter_value(&feed.meter(slot, tick)),
                    "slot {slot} tick {tick}"
                );
            }
        }
    }

    #[test]
    fn scripted_drag_moves_one_fader() {
        let mut app = app(Mode::Drag, 8);
        set_tick(&mut app, 0);
        app.update();
        let mixer = app.world().resource::<MixerEntities>();
        let (feed, faders) = (mixer.feed, mixer.faders.clone());
        let strip = feed.drag_strip().unwrap();
        for tick in [5, 20, 45, 60] {
            set_tick(&mut app, tick);
            app.update();
            let value = app.world().get::<ControlValue>(faders[strip]).unwrap().0;
            assert_eq!(value, feed.drag_sample(tick).unwrap().db, "tick {tick}");
            let other = app.world().get::<ControlValue>(faders[0]).unwrap().0;
            assert!((other - feed.strip(0).fader_db).abs() < 1e-3);
        }
    }
}
