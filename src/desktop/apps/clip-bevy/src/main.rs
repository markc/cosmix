//! Event-driven clipboard panel; smoke mode never initialises Bevy.
mod bus;
mod ui;

use bevy::{feathers::FeathersPlugins, prelude::*, window::ExitCondition, winit::WinitSettings};
use ctk::prelude::*;

fn main() {
    if let Ok(mode) = std::env::var("CLIPPANEL_SMOKE") {
        if !mode.is_empty() {
            std::process::exit(bus::smoke(&mode));
        }
    }
    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(ui::window()),
            exit_condition: ExitCondition::DontExit,
            ..default()
        }))
        .insert_resource(WinitSettings::desktop_app())
        .insert_resource(ClearColor(ui::colour(0x1b1d23)))
        .insert_resource(ctk::theme::CtkThemeMode(ctk::theme::Mode::Dark))
        .add_plugins((
            FeathersPlugins,
            CtkThemePlugin::default(),
            CtkWidgetsPlugin,
            CtkTextFieldPlugin,
        ))
        .init_resource::<ui::Panel>()
        .add_systems(Startup, (ui::start_bus, ui::setup).chain())
        .add_systems(
            Update,
            (ui::receive, ui::search, ui::paint, ui::hover).chain(),
        )
        .add_observer(ui::activate)
        .run();
}
