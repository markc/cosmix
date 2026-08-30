//! Interact GUI: the persistent CTK presenter for interactd dialogs.

mod mapping;
mod presenter;

use bevy::feathers::{dark_theme::create_dark_theme, theme::UiTheme, FeathersPlugins};
use bevy::prelude::*;
use cosmix_app_identity::AppIdentity;
use cosmix_interaction_schema::TOPIC_INTERACT_PROPS_CHANGED;
use ctk::prelude::{
    apply_theme, provenance_from_build, resolve_app_theme, resolve_noded_url, BusBridgeConfig,
    BusBridgePlugin, AppPortPlugin, CtkThemePlugin, FileRequesterPlugin, InteractionPlugin,
    ThemeState, THEME_CHANGED_TOPIC,
};

pub(crate) const IDENTITY: AppIdentity = AppIdentity {
    slug: "interactgui",
    display_name: "Interact GUI",
};
pub(crate) const BUS_SERVICE_NAME: &str = "interact-gui";

fn main() {
    let noded_url = resolve_noded_url();
    let mut bridge = BusBridgeConfig::new(BUS_SERVICE_NAME, noded_url);
    bridge.provenance = provenance_from_build(cosmix_buildinfo::build_info!());
    bridge.subscriptions = vec![
        TOPIC_INTERACT_PROPS_CHANGED.to_string(),
        THEME_CHANGED_TOPIC.to_string(),
    ];
    bridge.latest_topics = vec![];
    bridge.observation = false;

    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: IDENTITY.display_name.into(),
                name: Some(IDENTITY.app_id()),
                resolution: (760, 420).into(),
                resizable: true,
                ..default()
            }),
            ..default()
        }))
        .add_plugins((
            FeathersPlugins,
            CtkThemePlugin::default(),
            InteractionPlugin,
            FileRequesterPlugin,
            BusBridgePlugin::new(bridge),
            AppPortPlugin::new(IDENTITY.display_name, IDENTITY.slug),
            presenter::PresenterPlugin,
        ))
        .add_systems(Startup, setup)
        .run();
}

fn setup(mut commands: Commands, mut theme: ResMut<UiTheme>, mut theme_state: ResMut<ThemeState>) {
    *theme = UiTheme(create_dark_theme());
    apply_theme(&mut theme, &mut theme_state, &resolve_app_theme(None));
    commands.spawn(Camera2d);

    // TODO(phase-3a-followup): hide the primary window while no dialog or
    // progress card is active.
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_interaction_schema::is_bus_service_name;

    #[test]
    fn identity_uses_the_exact_broker_authorised_service_name() {
        assert_eq!(BUS_SERVICE_NAME, "interact-gui");
        assert!(is_bus_service_name(BUS_SERVICE_NAME));
        assert_eq!(IDENTITY.slug, "interactgui");
        assert!(IDENTITY.validate().is_ok());
        assert_eq!(IDENTITY.app_id(), "dev.cosmix.interactgui");
    }

    #[test]
    fn bridge_configuration_preserves_every_props_event() {
        let mut bridge = BusBridgeConfig::new(BUS_SERVICE_NAME, "ws://test.invalid/ws");
        bridge.provenance = provenance_from_build(cosmix_buildinfo::build_info!());
        bridge.subscriptions = vec![
            TOPIC_INTERACT_PROPS_CHANGED.to_string(),
            THEME_CHANGED_TOPIC.to_string(),
        ];
        bridge.latest_topics = vec![];
        bridge.observation = false;

        assert_eq!(bridge.service_name, BUS_SERVICE_NAME);
        assert_eq!(
            bridge.subscriptions,
            [TOPIC_INTERACT_PROPS_CHANGED, THEME_CHANGED_TOPIC]
        );
        assert!(bridge.latest_topics.is_empty());
        assert!(!bridge.observation);
    }
}
