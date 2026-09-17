//! The mixer view: every channel strip plus the master, built from CTK's
//! stock controls and wrapped into rows (see `cosmix_bench_feed::layout`).
//!
//! CTK's own `spawn_channel_strip_styled` cannot be used: it asserts the
//! 32-channel musicd cap and binds every control to the musicd mixer store.
//! The strips here follow its compact skeleton with the same widgets, minus
//! the trim knob and the value readouts the bake-off surface does not have.

use bevy::feathers::theme::{ThemeBackgroundColor, ThemeTextColor, ThemeToken};
use bevy::prelude::*;
use bevy::text::{Justify, TextLayout};
use bevy::ui::{Checked, Overflow};
use cosmix_bench_feed::layout::{
    BUTTON_FONT, BUTTON_GAP, BUTTON_HEIGHT, BUTTON_MIN_WIDTH, FADER_METER_GAP, FADER_WIDTH,
    KNOB_SIZE, METER_WIDTH, NAME_BOX_HEIGHT, NAME_FONT, NUMBER_FONT, ROW_GAP, STRIP_GAP,
    STRIP_PADDING, STRIP_SECTION_GAP, STRIP_WIDTH, strip_rows,
};
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
    let mut entities = MixerEntities {
        feed,
        faders: Vec::new(),
        meters: Vec::new(),
        pans: Vec::new(),
        mutes: Vec::new(),
        solos: Vec::new(),
        last_tick: None,
    };
    let mut rows = Vec::new();
    for slots in strip_rows(config.strips, config.size.0 as f32) {
        let strips: Vec<Entity> = slots
            .map(|slot| spawn_strip(&mut commands, &feed, slot, &mut entities))
            .collect();
        let row = commands
            .spawn(Node {
                flex_direction: FlexDirection::Row,
                column_gap: px(STRIP_GAP),
                align_items: AlignItems::Stretch,
                flex_grow: 1.0,
                flex_basis: px(0),
                min_height: px(0),
                ..default()
            })
            .add_children(&strips)
            .id();
        rows.push(row);
    }
    commands
        .spawn((
            Node {
                width: percent(100),
                height: percent(100),
                flex_direction: FlexDirection::Column,
                row_gap: px(ROW_GAP),
                ..default()
            },
            ThemeBackgroundColor(tokens::SURFACE),
        ))
        .add_children(&rows);
    commands.insert_resource(entities);
}

fn text(content: impl Into<String>, size: f32, token: ThemeToken) -> impl Bundle {
    (
        Text::new(content),
        TextFont::from_font_size(size),
        ThemeTextColor(token),
        Pickable::IGNORE,
    )
}

/// An empty box that holds a missing control's place on the master strip.
fn spacer(width: f32, height: f32) -> Node {
    Node {
        width: px(width),
        height: px(height),
        ..default()
    }
}

/// Let a widget follow its row's height instead of its spawn-time height
/// (CTK's fader and meter internals are percent-based).
fn stretch_to_row_height(commands: &mut Commands, entity: Entity) {
    commands
        .entity(entity)
        .entry::<Node>()
        .and_modify(|mut node| node.height = percent(100));
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

fn toggle(commands: &mut Commands, id: String, label: &str, on: bool) -> Entity {
    let mut entity = commands.spawn(toggle_button_sized(id, BUTTON_MIN_WIDTH, BUTTON_HEIGHT));
    entity.with_child(text(label, BUTTON_FONT, tokens::TEXT));
    if on {
        entity.insert(Checked);
    }
    entity.id()
}

fn spawn_strip(
    commands: &mut Commands,
    feed: &MixerFeed,
    slot: usize,
    out: &mut MixerEntities,
) -> Entity {
    let state = feed.strip(slot);
    let master = feed.is_master(slot);
    let inner_width = STRIP_WIDTH - 2.0 * STRIP_PADDING;

    let number = commands
        .spawn(text(
            state
                .number
                .map_or_else(|| " ".to_owned(), |n| n.to_string()),
            NUMBER_FONT,
            tokens::TEXT_DIM,
        ))
        .id();
    // One word per line, centred, clipped to the fixed two-line box.
    let name = commands
        .spawn((
            text(state.name.replace(' ', "\n"), NAME_FONT, tokens::TEXT),
            TextLayout {
                justify: Justify::Center,
                ..default()
            },
        ))
        .id();
    let name_box = commands
        .spawn(Node {
            width: px(inner_width),
            height: px(NAME_BOX_HEIGHT),
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            overflow: Overflow::clip(),
            ..default()
        })
        .add_child(name)
        .id();

    let pan = (!master).then(|| {
        commands
            .spawn(knob_sized(
                pan_props(format!("strip-{slot}-pan"), state.pan),
                KNOB_SIZE,
            ))
            .id()
    });
    let pan_slot = pan.unwrap_or_else(|| commands.spawn(spacer(KNOB_SIZE, KNOB_SIZE)).id());

    let fader = commands
        .spawn(fader_sized(
            fader_props(format!("strip-{slot}-fader"), feed.fader_db(slot, 0)),
            FADER_WIDTH,
            100.0,
        ))
        .id();
    let meter = commands
        .spawn(level_meter_sized(
            format!("strip-{slot}-meter"),
            meter_value(&MeterFrame::default()),
            METER_WIDTH,
            100.0,
        ))
        .id();
    stretch_to_row_height(commands, fader);
    stretch_to_row_height(commands, meter);
    let fader_row = commands
        .spawn(Node {
            flex_direction: FlexDirection::Row,
            column_gap: px(FADER_METER_GAP),
            align_items: AlignItems::End,
            justify_content: JustifyContent::Center,
            flex_grow: 1.0,
            min_height: px(0),
            ..default()
        })
        .add_children(&[meter, fader])
        .id();

    let mute = toggle(commands, format!("strip-{slot}-mute"), "M", state.mute);
    let solo = (!master).then(|| toggle(commands, format!("strip-{slot}-solo"), "S", state.solo));
    let solo_slot =
        solo.unwrap_or_else(|| commands.spawn(spacer(BUTTON_MIN_WIDTH, BUTTON_HEIGHT)).id());
    let buttons = commands
        .spawn(Node {
            flex_direction: FlexDirection::Column,
            row_gap: px(BUTTON_GAP),
            align_items: AlignItems::Center,
            ..default()
        })
        .add_children(&[mute, solo_slot])
        .id();

    out.faders.push(fader);
    out.meters.push(meter);
    out.pans.push(pan);
    out.mutes.push(mute);
    out.solos.push(solo);

    commands
        .spawn((
            Node {
                width: px(STRIP_WIDTH),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                row_gap: px(STRIP_SECTION_GAP),
                padding: UiRect::all(px(STRIP_PADDING)),
                ..default()
            },
            ThemeBackgroundColor(if master {
                tokens::MASTER_PANEL
            } else {
                tokens::PANEL
            }),
        ))
        .add_children(&[number, name_box, pan_slot, fader_row, buttons])
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
