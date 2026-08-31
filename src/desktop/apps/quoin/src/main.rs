//! Cosmix Quoin's real SCTK layer-shell host.

use std::time::Duration;

use bevy::feathers::{FeathersPlugins, dark_theme::create_dark_theme, theme::UiTheme};
use bevy::prelude::*;
use bevy::ui::{UiRect, percent, px};
use cosmix_shell::chrome::{
    QuoinChromePlugin, QuoinClock, QuoinContentBindings, QuoinPageContent, QuoinPageRegistry,
    QuoinPageSpec, spawn_quoin_chrome,
};
use cosmix_shell::core::{Edge, PanelInput, ShellModel};
use cosmix_shell::runtime::ShellFrameState;
use cosmix_shell_host::{LayerHostConfig, LayerPanelMounts, configure_layer_host};
use ctk::theme::{CtkThemePlugin, ThemeSpec, ThemeState, apply_theme, tokens};

const USAGE: &str = "usage: cosmix-quoin [--output NAME] [--smoke-all-panels]";

#[derive(Debug, Default)]
struct Cli {
    output: Option<String>,
    smoke_all_panels: bool,
}

#[derive(Debug)]
enum CliAction {
    Run(Cli),
    Help,
}

fn main() -> AppExit {
    let cli = match parse_cli(std::env::args().skip(1)) {
        Ok(CliAction::Run(cli)) => cli,
        Ok(CliAction::Help) => {
            println!("{USAGE}");
            return AppExit::Success;
        }
        Err(error) => {
            eprintln!("{error}");
            eprintln!("{USAGE}");
            eprintln!("QUOIN_LAYER_HOST_EXIT reason=invalid-cli");
            return AppExit::error();
        }
    };
    let registry = page_registry();
    let model_registry = registry.clone();
    let smoke_all_panels = cli.smoke_all_panels;
    let host = LayerHostConfig::new(cli.output, move |output, logical_size| {
        let mut model = ShellModel::new(
            output,
            logical_size,
            Duration::ZERO,
            Duration::from_millis(800),
            Duration::from_millis(200),
        )
        .expect("SCTK supplied valid positive output geometry");
        for edge in Edge::ALL {
            model.set_carousel(edge, model_registry.carousel(edge));
            if smoke_all_panels {
                model
                    .panel_input(edge, Duration::ZERO, PanelInput::Pin)
                    .expect("static smoke input is monotonic");
            }
        }
        model
    });

    let mut app = App::new();
    configure_layer_host(&mut app, host);
    app.insert_resource(registry)
        .add_plugins((
            FeathersPlugins,
            CtkThemePlugin::default(),
            QuoinChromePlugin,
        ))
        .add_systems(Startup, setup);
    app.run()
}

fn parse_cli(arguments: impl IntoIterator<Item = String>) -> Result<CliAction, String> {
    let mut cli = Cli::default();
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--output" => {
                if cli.output.is_some() {
                    return Err("--output may be supplied only once".to_owned());
                }
                let output = arguments
                    .next()
                    .ok_or_else(|| "--output requires a NAME".to_owned())?;
                if output.trim().is_empty() || output.starts_with('-') {
                    return Err("--output requires a non-empty NAME".to_owned());
                }
                cli.output = Some(output);
            }
            "--smoke-all-panels" => cli.smoke_all_panels = true,
            "--help" | "-h" => {
                return Ok(CliAction::Help);
            }
            _ => return Err(format!("unknown option: {argument}")),
        }
    }
    Ok(CliAction::Run(cli))
}

fn setup(
    mut commands: Commands,
    mounts: Res<LayerPanelMounts>,
    mut theme: ResMut<UiTheme>,
    mut theme_state: ResMut<ThemeState>,
    registry: Res<QuoinPageRegistry>,
    frame: Res<ShellFrameState>,
) {
    *theme = UiTheme(create_dark_theme());
    apply_theme(&mut theme, &mut theme_state, &ThemeSpec::builtin());

    let mut bindings = QuoinContentBindings::default();
    bindings.set(
        Edge::Bottom,
        vec![
            QuoinPageContent::new("launcher", bottom_launcher(&mut commands)),
            QuoinPageContent::new(
                "tasks",
                placeholder(
                    &mut commands,
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
                placeholder(
                    &mut commands,
                    "Navigation",
                    "Home\nApps\nFiles\nSettings",
                    false,
                ),
            ),
            QuoinPageContent::new(
                "places",
                placeholder(
                    &mut commands,
                    "Places",
                    "Desktop\nProjects\nDownloads",
                    false,
                ),
            ),
        ],
    );
    bindings.set(
        Edge::Right,
        vec![
            QuoinPageContent::new(
                "monitor",
                placeholder(
                    &mut commands,
                    "Monitoring",
                    "CPU  12%\nMemory  8.4 GiB\nMesh  healthy",
                    false,
                ),
            ),
            QuoinPageContent::new(
                "agents",
                placeholder(&mut commands, "Agents", "No active jobs", false),
            ),
        ],
    );
    bindings.set(
        Edge::Top,
        vec![
            QuoinPageContent::new(
                "status",
                placeholder(
                    &mut commands,
                    "Cosmix",
                    "Network online  •  Audio ready  •  Power balanced",
                    true,
                ),
            ),
            QuoinPageContent::new(
                "spaces",
                placeholder(&mut commands, "Spaces", "1  ●   2  ○   3  ○", true),
            ),
        ],
    );
    let props = registry
        .bind(&frame.0, bindings)
        .expect("Quoin content IDs match its validated registry");
    spawn_quoin_chrome(&mut commands, mounts.0, props);
}

fn page_registry() -> QuoinPageRegistry {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_accepts_output_and_smoke_flag_in_either_order() {
        let CliAction::Run(cli) = parse_cli([
            "--smoke-all-panels".to_owned(),
            "--output".to_owned(),
            "WL-1".to_owned(),
        ])
        .unwrap() else {
            panic!("valid run options returned help");
        };
        assert_eq!(cli.output.as_deref(), Some("WL-1"));
        assert!(cli.smoke_all_panels);
    }

    #[test]
    fn cli_rejects_missing_or_duplicate_output_name() {
        assert!(parse_cli(["--output".to_owned()]).is_err());
        assert!(
            parse_cli([
                "--output".to_owned(),
                "WL-1".to_owned(),
                "--output".to_owned(),
                "WL-2".to_owned(),
            ])
            .is_err()
        );
    }

    #[test]
    fn cli_error_and_help_paths_are_distinct() {
        assert_eq!(
            parse_cli(["--bogus".to_owned()]).unwrap_err(),
            "unknown option: --bogus"
        );
        assert!(matches!(
            parse_cli(["--help".to_owned()]),
            Ok(CliAction::Help)
        ));
    }
}
