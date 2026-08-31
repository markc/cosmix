//! Runnable Q-0 Quoin normal-window tuning harness.
//!
//! This is intentionally not a layer-shell client. F11 switches to
//! borderless fullscreen so physical output corners can be used for tuning.

use std::time::Duration;

use bevy::feathers::{FeathersPlugins, dark_theme::create_dark_theme, theme::UiTheme};
use bevy::prelude::*;
use bevy::ui::{UiRect, percent, px};
use bevy::window::PrimaryWindow;
use bevy::winit::{UpdateMode, WinitSettings};
use cosmix_shell::chrome::{
    QuoinChromePlugin, QuoinChromeProps, QuoinClock, QuoinContentBindings, QuoinPageContent,
    QuoinPageRegistry, QuoinPageSpec, spawn_quoin_chrome,
};
use cosmix_shell::core::{CornerDetectorConfig, Edge, LogicalSize, OutputKey, ShellModel};
use cosmix_shell::dev_host::{DevShellHostConfig, DevShellHostPlugin, IDLE_WAIT, spawn_dev_host};
use cosmix_shell::runtime::{ShellFrameState, ShellRuntimePlugin};
use ctk::theme::{CtkThemePlugin, ThemeSpec, ThemeState, apply_theme, tokens};

const OUTPUT: &str = "quoin-dev-window";

#[allow(dead_code)]
fn main() {
    let size = LogicalSize::new(1440.0, 900.0).expect("static demo geometry is valid");
    let output = OutputKey::new(OUTPUT).expect("static output key is valid");
    let registry = page_registry();
    let mut model = ShellModel::new(
        output,
        size,
        Duration::ZERO,
        Duration::from_millis(350),
        Duration::from_millis(180),
    )
    .expect("static panel configuration is valid");
    for edge in Edge::ALL {
        model.set_carousel(edge, registry.carousel(edge));
    }

    App::new()
        .insert_resource(WinitSettings {
            // Keep this finite: bevy_winit 0.19 state.rs:711 leaves a stale
            // WaitUntil armed when Instant::checked_add overflows on MAX.
            focused_mode: UpdateMode::reactive(IDLE_WAIT),
            unfocused_mode: UpdateMode::reactive_low_power(IDLE_WAIT),
        })
        .insert_resource(registry)
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "Cosmix Quoin — Q-0 dev harness (F11 fullscreen)".into(),
                resolution: (1440, 900).into(),
                resizable: true,
                ..default()
            }),
            ..default()
        }))
        .add_plugins((
            FeathersPlugins,
            CtkThemePlugin::default(),
            ShellRuntimePlugin::new(model),
            QuoinChromePlugin,
            DevShellHostPlugin,
        ))
        .add_systems(Startup, setup)
        .run();
}

fn setup(
    mut commands: Commands,
    windows: Query<Entity, With<PrimaryWindow>>,
    mut theme: ResMut<UiTheme>,
    mut theme_state: ResMut<ThemeState>,
    registry: Res<QuoinPageRegistry>,
    frame: Res<ShellFrameState>,
) {
    *theme = UiTheme(create_dark_theme());
    apply_theme(&mut theme, &mut theme_state, &ThemeSpec::builtin());
    commands.spawn(Camera2d);

    let window = windows.single().expect("demo has one primary window");
    let logical_size = LogicalSize::new(1440.0, 900.0).expect("static geometry is valid");
    let mounts = spawn_dev_host(
        &mut commands,
        window,
        DevShellHostConfig {
            output: OutputKey::new(OUTPUT).expect("static output key is valid"),
            logical_size,
            corner: CornerDetectorConfig::new(12.0, Duration::from_millis(200), 1500.0)
                .expect("plan seed values are valid"),
        },
    );

    let props = quoin_props(&mut commands, &registry, &frame);
    spawn_quoin_chrome(&mut commands, mounts, props);
}

pub(crate) fn quoin_props(
    commands: &mut Commands,
    registry: &QuoinPageRegistry,
    frame: &ShellFrameState,
) -> QuoinChromeProps {
    let mut bindings = QuoinContentBindings::default();
    bindings.set(
        Edge::Bottom,
        vec![
            QuoinPageContent::new("launcher", bottom_launcher(commands)),
            QuoinPageContent::new(
                "tasks",
                placeholder(
                    commands,
                    "Task strip",
                    "Studio  •  Mail  •  Files  •  Terminal",
                    true,
                ),
            ),
        ],
    );
    bindings.set(
        Edge::Left,
        vec![
            QuoinPageContent::new(
                "nav",
                placeholder(commands, "Navigation", "Home\nApps\nFiles\nSettings", false),
            ),
            QuoinPageContent::new(
                "places",
                placeholder(commands, "Places", "Desktop\nProjects\nDownloads", false),
            ),
        ],
    );
    bindings.set(
        Edge::Right,
        vec![
            QuoinPageContent::new(
                "monitor",
                placeholder(
                    commands,
                    "Monitoring",
                    "CPU  12%\nMemory  8.4 GiB\nMesh  healthy",
                    false,
                ),
            ),
            QuoinPageContent::new(
                "agents",
                placeholder(commands, "Agents", "No active jobs", false),
            ),
        ],
    );
    bindings.set(
        Edge::Top,
        vec![
            QuoinPageContent::new(
                "status",
                placeholder(
                    commands,
                    "Cosmix",
                    "Network online  •  Audio ready  •  Power balanced",
                    true,
                ),
            ),
            QuoinPageContent::new(
                "spaces",
                placeholder(commands, "Spaces", "1  ●   2  ○   3  ○", true),
            ),
        ],
    );
    registry
        .bind(&frame.0, bindings)
        .expect("Quoin content IDs match its validated registry")
}

pub(crate) fn page_registry() -> QuoinPageRegistry {
    let pages = |values: &[(&str, &str)]| {
        values
            .iter()
            .map(|(id, title)| QuoinPageSpec::new(*id, *title))
            .collect()
    };
    QuoinPageRegistry::new(
        pages(&[("nav", "Navigation"), ("places", "Places")]),
        pages(&[("launcher", "Launcher"), ("tasks", "Tasks")]),
        pages(&[("monitor", "Monitoring"), ("agents", "Agents")]),
        pages(&[("status", "Status"), ("spaces", "Spaces")]),
    )
    .expect("static page registry is valid")
}

fn placeholder(commands: &mut Commands, title: &str, body: &str, horizontal: bool) -> Entity {
    let heading = commands
        .spawn((
            Text::new(title),
            TextFont::from_font_size(14.0),
            bevy::feathers::theme::ThemeTextColor(tokens::TEXT),
        ))
        .id();
    let copy = commands
        .spawn((
            Text::new(body),
            TextFont::from_font_size(12.0),
            bevy::feathers::theme::ThemeTextColor(tokens::TEXT_DIM),
        ))
        .id();
    commands
        .spawn(Node {
            width: percent(100),
            height: percent(100),
            min_width: px(0),
            min_height: px(0),
            flex_direction: if horizontal {
                FlexDirection::Row
            } else {
                FlexDirection::Column
            },
            align_items: AlignItems::Center,
            justify_content: JustifyContent::SpaceBetween,
            padding: UiRect::all(px(10)),
            row_gap: px(8),
            column_gap: px(18),
            ..default()
        })
        .add_children(&[heading, copy])
        .id()
}

fn bottom_launcher(commands: &mut Commands) -> Entity {
    let apps = commands
        .spawn((
            Text::new("⌘  Launcher    Studio    Mail    Files    Terminal"),
            TextFont::from_font_size(13.0),
            bevy::feathers::theme::ThemeTextColor(tokens::TEXT),
        ))
        .id();
    let clock = commands
        .spawn((
            Text::new("--:--:-- UTC"),
            TextFont::from_font_size(13.0),
            bevy::feathers::theme::ThemeTextColor(tokens::TEXT),
            QuoinClock,
        ))
        .id();
    commands
        .spawn(Node {
            width: percent(100),
            height: percent(100),
            min_width: px(0),
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            justify_content: JustifyContent::SpaceBetween,
            padding: UiRect::axes(px(14), px(8)),
            ..default()
        })
        .add_children(&[apps, clock])
        .id()
}
