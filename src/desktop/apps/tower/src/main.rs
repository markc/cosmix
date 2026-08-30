//! CosMix Tower: a native CTK mesh node atlas and same-node control surface.

mod bus;
mod app_port;
mod atlas;
mod config;
mod confirm;
mod identity;
mod inspector;
mod lifecycle;
mod model;
mod panes;
mod props;
mod topology;
mod traffic;
mod ui;

use std::path::PathBuf;

use bevy::asset::AssetPlugin;
use bevy::prelude::*;
use bevy::winit::{UpdateMode, WinitSettings};
use ctk::prelude::*;

use identity::IDENTITY;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let noded_url = app_port::parse_noded_url(&args)
        .unwrap_or_else(|error| {
            eprintln!("tower: {error}");
            std::process::exit(2);
        })
        .unwrap_or_else(ctk::prelude::resolve_noded_url);
    let app_dirs = AppDirs::resolve(IDENTITY.slug).unwrap_or_else(|| {
        eprintln!("tower: no absolute app-data root is available");
        std::process::exit(1);
    });
    let asset_root = prepare_data_root(&app_dirs).unwrap_or_else(|error| {
        eprintln!("tower: {error}");
        app_dirs.cache()
    });
    let theme_dir = app_dirs.config();

    let mut app = App::new();
    app.add_plugins(
        DefaultPlugins
            .set(AssetPlugin {
                file_path: asset_root.to_string_lossy().into_owned(),
                ..default()
            })
            .set(WindowPlugin {
                primary_window: Some(Window {
                    title: IDENTITY.display_name.into(),
                    name: Some(IDENTITY.app_id()),
                    resolution: (1600, 960).into(),
                    resizable: true,
                    ..default()
                }),
                ..default()
            }),
    )
    .add_plugins((SvgPlugin, FeathersPlugins));
    add_runtime_plugins(&mut app, Some(theme_dir), noded_url);
    app.insert_resource(WinitSettings {
        focused_mode: UpdateMode::reactive(std::time::Duration::from_millis(100)),
        unfocused_mode: UpdateMode::reactive_low_power(std::time::Duration::from_millis(500)),
    })
    .add_systems(Startup, ui::setup)
    .run();
}

fn add_runtime_plugins(app: &mut App, theme_dir: Option<PathBuf>, noded_url: String) {
    let lifecycle_config = theme_dir.as_ref().map(|dir| dir.join("nodes.conf.mix"));
    let state_config = theme_dir.as_ref().map(|dir| dir.join("state.conf.mix"));
    app.add_plugins((
        CtkThemePlugin::new(theme_dir),
        CtkWidgetsPlugin,
        DcsAppShellPlugin,
        TreeViewPlugin,
        TopologyCanvasPlugin,
        ctk::interaction::InteractionPlugin,
        app_port::TowerAppPortPlugin::new(noded_url),
        config::TowerPersistencePlugin::new(state_config),
        confirm::ConfirmationPlugin,
        atlas::AtlasPlugin,
        lifecycle::LifecyclePlugin::new(lifecycle_config),
        ui::TowerUiPlugin,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_runtime_plugin_stack_initialises_headless() {
        let mut app = App::new();
        app.set_error_handler(bevy::ecs::error::ignore)
            .add_plugins(MinimalPlugins);
        add_runtime_plugins(
            &mut app,
            None,
            "ws://127.0.0.1:1/tower-schedule-test".to_owned(),
        );
        app.finish();
        app.cleanup();
        app.update();
    }
}
