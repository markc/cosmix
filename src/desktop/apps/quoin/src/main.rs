//! Cosmix Quoin's real SCTK layer-shell host.

mod bus_service;
mod desktop_font;
mod launcher;
mod power;
mod state;

use std::time::Duration;

use bevy::feathers::{FeathersPlugins, dark_theme::create_dark_theme, theme::UiTheme};
use bevy::prelude::*;
use bevy::ui::{UiRect, percent, px};
use bus_service::{QuoinPowerText, ShellBusPlugin};
use cosmix_shell::chrome::{
    QuoinChromePlugin, QuoinClock, QuoinContentBindings, QuoinPageContent, QuoinPageRegistry,
    QuoinPageSpec, spawn_quoin_chrome,
};
use cosmix_shell::core::{ConcealReason, Edge, PanelEffect, PanelInput, RevealTrigger, ShellModel};
use cosmix_shell::runtime::{ShellEffects, ShellFrameState, ShellRuntimeSet};
use cosmix_shell_host::{LayerHostConfig, LayerHostWake, LayerPanelMounts, configure_layer_host};
use ctk::bus::{
    BusBridgeConfig, BusBridgePlugin, BusWorkerWake, provenance_from_build, resolve_noded_url,
};
use ctk::theme::{
    CtkMonospace, CtkThemePlugin, Mode, Scheme, ThemeSpec, ThemeState, TypographySpec, apply_theme,
    tokens,
};

const USAGE: &str =
    "usage: cosmix-quoin [--output NAME] [--comp-service NAME] [--smoke-all-panels|--smoke-hidden]";

#[derive(Debug)]
struct Cli {
    output: Option<String>,
    smoke_all_panels: bool,
    smoke_hidden: bool,
    comp_service: String,
}

impl Default for Cli {
    fn default() -> Self {
        Self {
            output: None,
            smoke_all_panels: false,
            smoke_hidden: false,
            comp_service: "comp".to_owned(),
        }
    }
}

#[derive(Resource)]
struct SmokeState {
    all_panels: bool,
    hidden: bool,
    emitted: bool,
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
    let state_store = state::StateStore::startup(cli.smoke_all_panels || cli.smoke_hidden);
    let restored = state_store.snapshot();
    let model_registry = registry.clone();
    let smoke_all_panels = cli.smoke_all_panels;
    let smoke_hidden = cli.smoke_hidden;
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
        if !smoke_all_panels && !smoke_hidden {
            restored.restore(&mut model);
            model.start_intro(Duration::from_secs(2));
        }
        model
    })
    .with_comp_service(cli.comp_service);

    let mut app = App::new();
    configure_layer_host(&mut app, host);
    let wake = app.world().resource::<LayerHostWake>().callback();
    let mut bus = BusBridgeConfig::new("shell", resolve_noded_url());
    bus.provenance = provenance_from_build(cosmix_buildinfo::build_info!());
    bus.subscriptions.push("power.props.changed".to_owned());
    bus.inbound_prefixes.push("shell.".to_owned());
    bus.worker_wake = Some(BusWorkerWake::new(wake));
    app.insert_resource(registry)
        .insert_resource(state_store)
        .insert_resource(SmokeState {
            all_panels: smoke_all_panels,
            hidden: smoke_hidden,
            emitted: false,
        })
        .add_plugins((
            BusBridgePlugin::new(bus),
            FeathersPlugins,
            CtkThemePlugin::default(),
            QuoinChromePlugin,
            ShellBusPlugin,
            launcher::LauncherPlugin,
        ))
        .add_systems(Startup, setup)
        .add_systems(Update, log_transitions.in_set(ShellRuntimeSet::Host))
        .add_systems(
            Update,
            state::persist_transitions.in_set(ShellRuntimeSet::Host),
        );
    app.run()
}

fn parse_cli(arguments: impl IntoIterator<Item = String>) -> Result<CliAction, String> {
    let mut cli = Cli::default();
    let mut comp_service_seen = false;
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
            "--smoke-hidden" => cli.smoke_hidden = true,
            "--comp-service" => {
                if comp_service_seen {
                    return Err("--comp-service may be supplied only once".to_owned());
                }
                let service = arguments
                    .next()
                    .ok_or_else(|| "--comp-service requires a NAME".to_owned())?;
                if !valid_service_name(&service) {
                    return Err("--comp-service requires a canonical service NAME".to_owned());
                }
                cli.comp_service = service;
                comp_service_seen = true;
            }
            "--help" | "-h" => {
                return Ok(CliAction::Help);
            }
            _ => return Err(format!("unknown option: {argument}")),
        }
    }
    if cli.smoke_all_panels && cli.smoke_hidden {
        return Err("--smoke-all-panels and --smoke-hidden are mutually exclusive".to_owned());
    }
    Ok(CliAction::Run(cli))
}

fn valid_service_name(name: &str) -> bool {
    (2..=31).contains(&name.len())
        && name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn edge_name(edge: Edge) -> &'static str {
    match edge {
        Edge::Left => "left",
        Edge::Bottom => "bottom",
        Edge::Right => "right",
        Edge::Top => "top",
    }
}

fn log_transitions(
    effects: Res<ShellEffects>,
    frame: Res<ShellFrameState>,
    mut smoke: ResMut<SmokeState>,
) {
    for effect in &effects.0 {
        let edge = edge_name(effect.edge);
        match effect.effect {
            PanelEffect::ResizeCompleted => println!("QUOIN_RESIZE_COMPLETED edge={edge}"),
            PanelEffect::Reveal {
                trigger: RevealTrigger::Corner,
            } => println!("QUOIN_REVEAL edge={edge} trigger=corner"),
            PanelEffect::Conceal {
                reason: ConcealReason::CornerLeft,
            } => println!("QUOIN_CONCEAL edge={edge} reason=corner-left"),
            PanelEffect::Conceal {
                reason: ConcealReason::Grace,
            } => println!("QUOIN_CONCEAL edge={edge} reason=grace"),
            PanelEffect::Pin { pinned } => println!(
                "QUOIN_PIN edge={edge} state={}",
                if pinned { "pinned" } else { "unpinned" }
            ),
        }
    }
    if smoke.emitted {
        return;
    }
    if smoke.all_panels {
        for edge in Edge::ALL {
            println!("QUOIN_PIN edge={} state=pinned", edge_name(edge));
        }
        smoke.emitted = true;
    } else if smoke.hidden
        && Edge::ALL
            .into_iter()
            .all(|edge| !frame.0.panel(edge).mapped)
    {
        println!("QUOIN_HIDDEN_READY panels=4");
        smoke.emitted = true;
    }
}

fn setup(
    mut commands: Commands,
    mounts: Res<LayerPanelMounts>,
    mut theme: ResMut<UiTheme>,
    mut theme_state: ResMut<ThemeState>,
    registry: Res<QuoinPageRegistry>,
    frame: Res<ShellFrameState>,
    state_store: Res<state::StateStore>,
) {
    // Furniture reads as furniture in dark mode against a desktop; the shipped
    // default is Ocean/Dark, overridden by a persisted scheme when present.
    let scheme = state_store
        .scheme()
        .and_then(Scheme::from_name)
        .unwrap_or(Scheme::Ocean);
    let mut spec = ThemeSpec::from_scheme(scheme, Mode::Dark);
    // Match the host desktop's UI font where it can be read; otherwise CTK's
    // built-in typography stands (a machine with no KDE config still looks
    // right). Point sizes are converted to logical px; output scale is applied
    // downstream, so this stays scale-free.
    if let Some(font) = desktop_font::detect() {
        spec.typography = TypographySpec {
            family: font.family,
            body_px: font.body_px,
        };
    }
    *theme = UiTheme(create_dark_theme());
    apply_theme(&mut theme, &mut theme_state, &spec);

    let mut bindings = QuoinContentBindings::default();
    bindings.set(
        Edge::Bottom,
        vec![
            QuoinPageContent::new("launcher", bottom_launcher(&mut commands)),
            QuoinPageContent::new("power", bottom_power(&mut commands)),
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
            QuoinPageContent::new("nav", left_page(&mut commands, "nav")),
            QuoinPageContent::new("places", left_page(&mut commands, "places")),
            QuoinPageContent::new("info", left_page(&mut commands, "info")),
        ],
    );
    bindings.set(
        Edge::Right,
        vec![
            QuoinPageContent::new("monitor", system_page(&mut commands)),
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
        pages(&[("nav", "Apps"), ("places", "Places"), ("info", "Info")]),
        pages(&[
            ("launcher", "Launcher"),
            ("power", "Power"),
            ("tasks", "Tasks"),
        ]),
        pages(&[("monitor", "System"), ("agents", "Agents")]),
        pages(&[("status", "Status"), ("spaces", "Spaces")]),
    )
    .expect("static page registry is valid")
}

fn system_page(commands: &mut Commands) -> Entity {
    let root = placeholder(commands, "System", "Colour scheme", false);
    let quit = cosmix_shell::chrome::quoin_quit_button(commands, Edge::Right, "monitor");
    let dots = commands
        .spawn(Node {
            flex_wrap: FlexWrap::Wrap,
            column_gap: px(8),
            row_gap: px(8),
            ..default()
        })
        .id();
    for scheme in Scheme::ALL {
        let dot = cosmix_shell::chrome::scheme_dot(commands, Edge::Right, "monitor", scheme)
            .expect("static system page ID is valid");
        commands.entity(dots).add_child(dot);
    }
    commands.entity(root).add_children(&[dots, quit]);
    root
}

fn left_page(commands: &mut Commands, page: &str) -> Entity {
    use cosmix_shell::chrome::{NavLinkTarget, navlink};
    let root = commands
        .spawn(Node {
            width: percent(100),
            min_width: px(0),
            flex_direction: FlexDirection::Column,
            padding: UiRect::all(px(6)),
            row_gap: px(4),
            ..default()
        })
        .id();
    for (icon, label, target) in [
        ("⊞", "Apps", "nav"),
        ("⌂", "Places", "places"),
        ("ℹ", "Info", "info"),
    ] {
        let link = navlink(
            commands,
            Edge::Left,
            page,
            icon,
            label,
            NavLinkTarget::Page(target.into()),
        )
        .expect("static navigation IDs are valid");
        commands.entity(root).add_child(link);
    }
    if page == "places" {
        for (icon, label) in [
            ("⌂", "Home"),
            ("🗀", "Projects"),
            ("↓", "Downloads"),
            ("⚙", "Quoin cfg"),
        ] {
            let link = navlink(
                commands,
                Edge::Left,
                page,
                icon,
                label,
                NavLinkTarget::Intent,
            )
            .expect("static places ID is valid");
            commands.entity(root).add_child(link);
        }
    } else {
        let body = if page == "info" {
            concat!(
                "Quoin ",
                env!("CARGO_PKG_VERSION"),
                "\nFour-edge desktop shell"
            )
        } else {
            "Konsole\nFirefox\nDolphin\nKate"
        };
        let copy = commands
            .spawn((
                Text::new(body),
                TextFont::from_font_size(12.0),
                bevy::feathers::theme::ThemeTextColor(tokens::TEXT_DIM),
            ))
            .id();
        commands.entity(root).add_child(copy);
    }
    root
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
    let thunderbird = launcher::button(commands, launcher::LauncherApp::Thunderbird);
    let foot = launcher::button(commands, launcher::LauncherApp::Foot);
    let apps = commands
        .spawn((
            Text::new("Konsole  ·  Firefox  ·  Dolphin  ·  Kate"),
            TextFont::from_font_size(13.0),
            bevy::feathers::theme::ThemeTextColor(tokens::TEXT),
        ))
        .id();
    let clock = commands
        .spawn((
            Text::new("--:--:-- UTC"),
            TextFont::from_font_size(13.0),
            bevy::feathers::theme::ThemeTextColor(tokens::TEXT),
            // A proportional face makes clock digits jitter each second; the
            // monospace role keeps them on a fixed advance while staying
            // size-managed by CTK.
            CtkMonospace,
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
        .add_children(&[apps, foot, thunderbird, clock])
        .id()
}

fn bottom_power(commands: &mut Commands) -> Entity {
    let heading = commands
        .spawn((
            Text::new("Power"),
            TextFont::from_font_size(14.0),
            bevy::feathers::theme::ThemeTextColor(tokens::TEXT),
        ))
        .id();
    let reading = commands
        .spawn((
            Text::new("Power unavailable"),
            TextFont::from_font_size(13.0),
            bevy::feathers::theme::ThemeTextColor(tokens::TEXT_DIM),
            QuoinPowerText,
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
        .add_children(&[heading, reading])
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
        assert_eq!(cli.comp_service, "comp");
    }

    #[test]
    fn cli_accepts_shape_amended_service_and_rejects_smoke_conflict() {
        let CliAction::Run(cli) = parse_cli([
            "--comp-service".to_owned(),
            "comp-nested".to_owned(),
            "--smoke-hidden".to_owned(),
        ])
        .unwrap() else {
            panic!("valid run options returned help");
        };
        assert_eq!(cli.comp_service, "comp-nested");
        assert!(cli.smoke_hidden);
        assert!(parse_cli(["--smoke-hidden".to_owned(), "--smoke-all-panels".to_owned()]).is_err());
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
