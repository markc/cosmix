//! Local-only smoke test for CTK's reusable controls.

use bevy::feathers::{dark_theme::create_dark_theme, theme::UiTheme};
use bevy::picking::Pickable;
use bevy::prelude::*;
use ctk::prelude::*;

#[derive(Component)]
struct DemoMeter;

fn main() {
    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "CTK widget gallery".into(),
                resolution: (900, 540).into(),
                resizable: true,
                ..default()
            }),
            ..default()
        }))
        .add_plugins((FeathersPlugins, CtkThemePlugin::default(), CtkWidgetsPlugin))
        .add_systems(Startup, setup)
        .add_systems(Update, animate_meter)
        .add_observer(report_control_change)
        .run();
}

fn setup(mut commands: Commands, mut theme: ResMut<UiTheme>, mut theme_state: ResMut<ThemeState>) {
    *theme = UiTheme(create_dark_theme());
    apply_theme(&mut theme, &mut theme_state, &ThemeSpec::builtin());
    commands.spawn(Camera2d);

    let db_mapping = ValueMapping::piecewise([
        (0.0, -120.0),
        (0.10, -60.0),
        (0.25, -30.0),
        (0.50, -12.0),
        (0.75, 0.0),
        (1.0, 6.0),
    ])
    .expect("static fader mapping is valid");

    commands
        .spawn((
            Node {
                width: percent(100),
                min_height: percent(100),
                flex_direction: FlexDirection::Column,
                padding: UiRect::all(px(24)),
                row_gap: px(18),
                ..default()
            },
            bevy::feathers::theme::ThemeBackgroundColor(ctk::theme::tokens::SURFACE),
        ))
        .with_children(|root| {
            root.spawn((
                Text::new("CTK — generic desktop controls"),
                TextFont::from_font_size(24.0),
                bevy::feathers::theme::ThemeTextColor(ctk::theme::tokens::TEXT),
            ));
            root.spawn((
                Text::new(
                    "Pointer, keyboard focus and resize are live. Final edits are logged once.",
                ),
                TextFont::from_font_size(14.0),
                bevy::feathers::theme::ThemeTextColor(ctk::theme::tokens::TEXT_DIM),
            ));
            root.spawn((Node {
                flex_direction: FlexDirection::Row,
                flex_wrap: FlexWrap::Wrap,
                align_items: AlignItems::End,
                column_gap: px(26),
                row_gap: px(20),
                ..default()
            },))
                .with_children(|row| {
                    labelled(row, "Fader", |parent| {
                        parent.spawn(fader(NumericControlProps::new(
                            "gallery.fader",
                            -12.0,
                            ControlRange {
                                min: -120.0,
                                max: 6.0,
                                step: 0.1,
                                detent: Some(0.0),
                            },
                            db_mapping,
                        )));
                    });
                    labelled(row, "Knob", |parent| {
                        parent.spawn(knob(NumericControlProps::new(
                            "gallery.knob",
                            0.0,
                            ControlRange {
                                min: -1.0,
                                max: 1.0,
                                step: 1.0 / 512.0,
                                detent: Some(0.0),
                            },
                            ValueMapping::linear(-1.0, 1.0).unwrap(),
                        )));
                    });
                    labelled(row, "Meter", |parent| {
                        parent.spawn((
                            level_meter("gallery.meter", MeterValue::default()),
                            DemoMeter,
                        ));
                    });
                    labelled(row, "Toggle", |parent| {
                        parent.spawn(toggle_button("gallery.toggle")).with_child((
                            Text::new("MUTE"),
                            TextFont::from_font_size(12.0),
                            bevy::feathers::theme::ThemeTextColor(ctk::theme::tokens::TEXT),
                            Pickable::IGNORE,
                        ));
                    });
                });
        });
}

fn labelled(
    parent: &mut ChildSpawnerCommands,
    label: &str,
    spawn_control: impl FnOnce(&mut ChildSpawnerCommands),
) {
    parent
        .spawn((Node {
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Center,
            row_gap: px(8),
            ..default()
        },))
        .with_children(|column| {
            column.spawn((
                Text::new(label),
                TextFont::from_font_size(13.0),
                bevy::feathers::theme::ThemeTextColor(ctk::theme::tokens::TEXT_DIM),
            ));
            spawn_control(column);
        });
}

fn animate_meter(time: Res<Time>, mut meters: Query<&mut MeterValue, With<DemoMeter>>) {
    let phase = time.elapsed_secs();
    for mut meter in &mut meters {
        let left = (phase.sin() * 0.35 + 0.58).clamp(0.0, 1.0);
        let right = ((phase * 1.31).sin() * 0.28 + 0.52).clamp(0.0, 1.0);
        meter.lanes[0].level = left;
        meter.lanes[1].level = right;
        meter.lanes[0].peak = left;
        meter.lanes[1].peak = right;
    }
}

fn report_control_change(change: On<ControlChange>) {
    if change.is_final {
        info!(entity = ?change.source, value = change.value, "committed CTK control edit");
    }
}
