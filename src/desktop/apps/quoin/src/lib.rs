//! Cosmix Quoin's real SCTK layer-shell host.

mod activation;
mod bus_service;
pub mod config;
mod corner_menu;
mod desktop_font;
pub mod embedded;
#[cfg(test)]
mod font_tests;
mod holders;
mod hotspot;
mod keyboard;
mod settings;
mod state;

use std::time::Duration;

use bevy::feathers::{FeathersPlugins, dark_theme::create_dark_theme, theme::UiTheme};
use bevy::prelude::*;
use bus_service::ShellBusPlugin;
use cosmix_shell::chrome::{
    QuoinChromePlugin, QuoinContentBindings, QuoinPageRegistry, spawn_quoin_chrome,
};
use cosmix_shell::core::{ConcealReason, Edge, PanelEffect, PanelInput, RevealTrigger, ShellModel};
use cosmix_shell::runtime::{ShellEffects, ShellFrameState, ShellRuntimeSet};
use cosmix_shell_host::{LayerHostConfig, LayerHostWake, LayerPanelMounts, configure_layer_host};
use ctk::bus::{
    BusBridgeConfig, BusBridgePlugin, BusWorkerWake, provenance_from_build, resolve_noded_url,
};
use ctk::theme::{
    CtkThemePlugin, Mode, Scheme, ThemeSpec, ThemeState, TypographySpec, apply_theme,
};

const USAGE: &str = "usage: cosmix-quoin [--output NAME] [--comp-service NAME] [--bus-service NAME] [--smoke-all-panels|--smoke-hidden]";

#[derive(Debug)]
struct Cli {
    output: Option<String>,
    smoke_all_panels: bool,
    smoke_hidden: bool,
    comp_service: String,
    bus_service: String,
}

impl Default for Cli {
    fn default() -> Self {
        Self {
            output: None,
            smoke_all_panels: false,
            smoke_hidden: false,
            comp_service: "comp".to_owned(),
            bus_service: "shell".to_owned(),
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

pub fn run_layer_host() -> AppExit {
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
    let config = config::startup_config(cli.smoke_all_panels || cli.smoke_hidden);
    let registry = startup_page_registry(&config);
    let state_store = state::StateStore::startup(cli.smoke_all_panels || cli.smoke_hidden);
    let restore_saved = state_store.shared_saved();
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
        model.suppress_empty_edges(model_registry.declarations_only());
        for edge in Edge::ALL {
            model.set_carousel(edge, model_registry.carousel(edge));
            if smoke_all_panels {
                model
                    .panel_input(edge, Duration::ZERO, PanelInput::Dock)
                    .expect("static smoke input is monotonic");
            }
        }
        if !smoke_all_panels && !smoke_hidden {
            // Restore through the store's shared handle so claiming the
            // migrated default-output entry reaches the next save.
            state::StateStore::restore_shared(&restore_saved, &mut model);
            model.start_intro(Duration::from_secs(2));
        }
        model
    })
    .with_comp_service(cli.comp_service.clone());

    let mut app = App::new();
    configure_layer_host(&mut app, host);
    let wake = app.world().resource::<LayerHostWake>().callback();
    let mut bus = BusBridgeConfig::new(cli.bus_service, resolve_noded_url());
    hotspot::install(&mut app, &mut bus, cli.comp_service.clone());
    hotspot::arm_first_run(&mut app, state_store.first_run());
    activation::install(&mut app, &mut bus, cli.comp_service.clone());
    holders::install(&mut app, &mut bus, cli.comp_service);
    bus.provenance = provenance_from_build(cosmix_buildinfo::build_info!());
    // Broker service-registry diffs: the citizen-disconnect notification
    // sub-panel ownership keys on (the `services.registered` leaf).
    bus.subscriptions.push("noded.props.changed".to_owned());
    bus.inbound_prefixes.push("shell.".to_owned());
    bus.max_inbound_body_bytes = cosmix_scene::MAX_DOCUMENT_BYTES;
    bus.worker_wake = Some(BusWorkerWake::new(wake));
    configure_content(
        &mut app,
        bus,
        registry,
        state_store,
        smoke_all_panels,
        smoke_hidden,
        config,
    );
    config::install(&mut app, smoke_all_panels || smoke_hidden);
    keyboard::install(&mut app);
    corner_menu::install(&mut app);
    app.run()
}

fn configure_content(
    app: &mut App,
    bus: BusBridgeConfig,
    registry: QuoinPageRegistry,
    state_store: state::StateStore,
    all_panels: bool,
    hidden: bool,
    config: config::ShellConfig,
) {
    app.insert_resource(registry)
        .insert_resource(config)
        .insert_resource(state_store)
        .insert_resource(SmokeState {
            all_panels,
            hidden,
            emitted: false,
        })
        .add_plugins((
            BusBridgePlugin::new(bus),
            FeathersPlugins,
            CtkThemePlugin::default(),
            QuoinChromePlugin,
            ShellBusPlugin,
            cosmix_scene_bevy::ScenePlugin,
        ))
        .add_systems(Startup, setup)
        .add_systems(Update, log_transitions.in_set(ShellRuntimeSet::Host))
        .add_systems(
            Update,
            state::persist_transitions.in_set(ShellRuntimeSet::Host),
        );
    settings::install(app, all_panels || hidden);
}

fn parse_cli(arguments: impl IntoIterator<Item = String>) -> Result<CliAction, String> {
    let mut cli = Cli::default();
    let mut comp_service_seen = false;
    let mut bus_service_seen = false;
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--bus-service" => {
                if bus_service_seen {
                    return Err("--bus-service may be supplied only once".into());
                }
                let service = arguments
                    .next()
                    .ok_or_else(|| "--bus-service requires a NAME".to_owned())?;
                if !valid_service_name(&service) {
                    return Err("--bus-service requires a canonical service NAME".into());
                }
                cli.bus_service = service;
                bus_service_seen = true;
            }
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
    edge.as_str()
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
            PanelEffect::Reveal {
                trigger: RevealTrigger::Holders,
            } => println!("QUOIN_REVEAL edge={edge} trigger=holders"),
            PanelEffect::Conceal {
                reason: ConcealReason::CornerLeft,
            } => println!("QUOIN_CONCEAL edge={edge} reason=corner-left"),
            PanelEffect::Conceal {
                reason: ConcealReason::Grace,
            } => println!("QUOIN_CONCEAL edge={edge} reason=grace"),
            PanelEffect::Conceal {
                reason: ConcealReason::Holders,
            } => println!("QUOIN_CONCEAL edge={edge} reason=holders"),
            PanelEffect::ModeChanged { mode } => {
                println!("QUOIN_MODE edge={edge} mode={}", mode.as_str())
            }
        }
    }
    if smoke.emitted {
        return;
    }
    if smoke.all_panels {
        for edge in Edge::ALL {
            if frame.0.panel(edge).mode == cosmix_shell::core::PanelMode::Docked {
                println!("QUOIN_PIN edge={} state=pinned", edge_name(edge));
            }
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
    mounts: (
        Option<Res<LayerPanelMounts>>,
        Option<Res<embedded::EmbeddedPanelMounts>>,
    ),
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
        .and_then(|scheme| Scheme::from_name(&scheme))
        .unwrap_or(Scheme::Ocean);
    let mut spec = ThemeSpec::from_scheme(scheme, Mode::Dark);
    // Optional Plasma import overrides the shared desktop font tokens.
    if let Some(font) = desktop_font::detect() {
        spec.typography = TypographySpec {
            family: font.family,
            body_px: font.body_px,
            weight: font.weight,
            ..Default::default()
        };
    }
    *theme = UiTheme(create_dark_theme());
    apply_theme(&mut theme, &mut theme_state, &spec);

    let bindings = QuoinContentBindings::default();
    let props = registry
        .bind(&frame.0, bindings)
        .expect("Quoin content IDs match its validated registry");
    let mounts = mounts
        .1
        .map(|m| m.0)
        .or_else(|| mounts.0.map(|m| m.0))
        .expect("Quoin requires a panel host");
    spawn_quoin_chrome(&mut commands, mounts, props);
}

fn startup_page_registry(config: &config::ShellConfig) -> QuoinPageRegistry {
    QuoinPageRegistry::declared(&config.panels).expect("accepted declarations are valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Registered IDs for state-migration and model-only tests. These stand in
    /// for scene registrations; no fixture constructs native page content.
    pub(crate) fn fixture_registry() -> QuoinPageRegistry {
        use cosmix_shell::chrome::QuoinPageSpec;
        let pages = |ids: &[&str]| ids.iter().map(|id| QuoinPageSpec::new(*id, *id)).collect();
        QuoinPageRegistry::new(
            pages(&["nav", "places", "info"]),
            pages(&["launcher", "power", "tasks"]),
            pages(&["monitor", "demos", "agents"]),
            pages(&["status", "spaces"]),
        )
        .unwrap()
    }

    #[test]
    fn declared_startup_binds_an_empty_frame() {
        let config = config::ShellConfig::parse(r#"{panels: {bottom: ["scene-panel"]}}"#).unwrap();
        let registry = startup_page_registry(&config);
        let mut model = ShellModel::new(
            cosmix_shell::core::OutputKey::new("test-output").unwrap(),
            cosmix_shell::core::LogicalSize::new(1000.0, 800.0).unwrap(),
            Duration::ZERO,
            Duration::from_millis(800),
            Duration::from_millis(200),
        )
        .unwrap();
        for edge in Edge::ALL {
            model.set_carousel(edge, registry.carousel(edge));
            assert!(model.carousel(edge).page_ids().is_empty());
        }
        let bindings = QuoinContentBindings::default();
        registry
            .bind(
                &cosmix_shell::runtime::ShellFrame::from_model(&model),
                bindings,
            )
            .unwrap();
    }

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
