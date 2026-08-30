//! Live channel-zero `musicd` control surface.
//!
//! The `--smoke-write` / `--smoke-stream` measurement harness lives in
//! [`ctk::mixer::smoke`] (shared verbatim by the board binaries and the fused
//! bench arm); this example only inserts the run resources.

use bevy::feathers::{dark_theme::create_dark_theme, theme::UiTheme};
use bevy::prelude::*;
use ctk::prelude::*;

#[derive(Resource)]
struct SmokeConfig(bool);

#[derive(Resource)]
struct StreamConfig(bool);

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let noded_url = args
        .iter()
        .find(|arg| arg.starts_with("ws://") || arg.starts_with("wss://"))
        .cloned()
        .unwrap_or_else(|| "ws://127.0.0.1:4200/ws".to_string());
    let smoke_enabled = args.iter().any(|arg| arg == "--smoke-write");
    let stream_enabled = args.iter().any(|arg| arg == "--smoke-stream");

    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "CTK · musicd channel 1".into(),
                resolution: (620, 680).into(),
                resizable: true,
                ..default()
            }),
            ..default()
        }))
        .add_plugins((
            FeathersPlugins,
            CtkThemePlugin::default(),
            ChromePlugin,
            CtkWidgetsPlugin,
            MusicdMixerPlugin::new(noded_url),
            AppControlPlugin::new("CTK · musicd channel 1", "mixer"),
        ))
        .insert_resource(SmokeConfig(smoke_enabled))
        .insert_resource(StreamConfig(stream_enabled))
        .add_systems(Startup, setup)
        .run();
}

fn setup(
    mut commands: Commands,
    mut theme: ResMut<UiTheme>,
    mut theme_state: ResMut<ThemeState>,
    smoke_cfg: Res<SmokeConfig>,
    stream: Res<StreamConfig>,
) {
    *theme = UiTheme(create_dark_theme());
    apply_theme(&mut theme, &mut theme_state, &ThemeSpec::builtin());
    commands.spawn(Camera2d);

    let strip = spawn_channel_strip(&mut commands, 0);
    if smoke_cfg.0 {
        commands.insert_resource(smoke::SmokeRun::new(strip.fader));
    }
    if stream.0 {
        commands.insert_resource(smoke::StreamRun::new(strip.fader));
    }
    let status = spawn_status_text(&mut commands, "Link: connecting");
    let note = commands
        .spawn((
            Text::new(
                "Drags stream throttled live-audition writes (~60Hz); release commits the final revisioned value; remote writes reconcile silently.",
            ),
            TextFont::from_font_size(13.0),
            bevy::feathers::theme::ThemeTextColor(ctk::theme::tokens::TEXT_DIM),
            Node { max_width: px(330), ..default() },
        ))
        .id();

    commands
        .spawn((
            Node {
                width: percent(100),
                min_height: percent(100),
                flex_direction: FlexDirection::Row,
                flex_wrap: FlexWrap::Wrap,
                align_items: AlignItems::Start,
                column_gap: px(28),
                row_gap: px(20),
                padding: UiRect::all(px(24)),
                ..default()
            },
            bevy::feathers::theme::ThemeBackgroundColor(ctk::theme::tokens::SURFACE),
        ))
        .with_children(|root| {
            root.spawn((Node {
                flex_direction: FlexDirection::Column,
                row_gap: px(14),
                ..default()
            },))
                .add_children(&[status, note]);
            let root_entity = root.target_entity();
            root.commands_mut()
                .entity(strip.root)
                .insert(ChildOf(root_entity));
        });
}
