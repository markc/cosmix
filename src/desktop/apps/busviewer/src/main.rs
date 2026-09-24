//! BusViewer — a native CTK browser for the local ABP Bus.
mod bus;
mod ui;

use bevy::{
    feathers::FeathersPlugins,
    prelude::*,
    winit::{UpdateMode, WinitSettings},
};
use ctk::prelude::*;
use std::time::Duration;

fn main() {
    // --version/-V: answer and exit 0 before any other side effect.
    cosmix_buildinfo::exit_on_version!();
    let mut url = configured_noded_url();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" => {
                println!("busviewer [--noded-url URL]\nBrowse local ABP services and HELP verbs, inspect mesh membership, and call verbs with optional JSON bodies. Uses the shared node configuration by default.");
                return;
            }
            "--noded-url" => {
                url = Some(
                    args.next()
                        .unwrap_or_else(|| usage_error("--noded-url requires a URL")),
                )
            }
            _ => usage_error(&format!("Unknown option: {arg}")),
        }
    }
    let mut app = App::new();
    app.insert_resource(bus::Bus::start(url))
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "BusViewer".into(),
                name: Some("dev.cosmix.busviewer".into()),
                resolution: (1400, 1400).into(),
                ..default()
            }),
            ..default()
        }));
    configure(&mut app);
    app.run();
}

fn configure(app: &mut App) {
    app.insert_resource(ctk::theme::CtkThemeMode(ctk::theme::Mode::Dark))
        .init_resource::<ui::Browser>()
        .add_plugins((
            FeathersPlugins,
            CtkThemePlugin::default(),
            CtkWidgetsPlugin,
            MenuBarPlugin,
            TreeViewPlugin,
            DcsShellPlugin,
            ModalCapturePlugin,
            ctk::interaction::InteractionPlugin,
            CtkTextAreaPlugin,
        ))
        .insert_resource(WinitSettings {
            focused_mode: UpdateMode::reactive(Duration::from_millis(50)),
            unfocused_mode: UpdateMode::reactive_low_power(Duration::from_millis(200)),
        })
        .add_systems(Startup, ui::setup)
        .add_systems(
            Update,
            (ui::receive, ui::search, ui::rebuild_tree, ui::paint).chain(),
        )
        .add_observer(ui::on_expand)
        .add_observer(ui::on_activate)
        .add_observer(ui::on_menu);
}

fn usage_error(message: &str) -> ! {
    eprintln!("busviewer: {message}");
    std::process::exit(2)
}
