//! Bind the native menu to the latest config and the existing Bus worker.
use crate::config::ShellConfig;
use bevy::prelude::*;
use cosmix_shell::chrome::corner_menu::{
    CornerMenuActionHook, CornerMenuRequest, MenuAction, MenuExtra, menu_items,
};
use cosmix_shell::core::{Corner, OutputKey};
use cosmix_shell::runtime::ShellFrameState;
use cosmix_shell_host::CornerMenuHook;
use ctk::bus::BusBridge;
use serde_json::{Value, json};

pub(crate) fn install(app: &mut App) {
    app.insert_resource(CornerMenuHook(open))
        .insert_resource(CornerMenuActionHook(invoke));
}

fn open(world: &mut World, output: &OutputKey, corner: Corner) {
    let edge = corner.summoned_edge();
    let extras = world
        .get_resource::<ShellConfig>()
        .map(|config| {
            config.menu_items[edge.index()]
                .iter()
                .map(|item| MenuExtra {
                    label: item.label.clone(),
                    target: item.target.clone(),
                    verb: item.verb.clone(),
                    args: item.args.clone(),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let Some(frame) = world.get_resource::<ShellFrameState>() else {
        return;
    };
    let items = menu_items(frame.0.panel(edge).mode, &extras);
    world.insert_resource(CornerMenuRequest {
        output: output.clone(),
        corner,
        items,
    });
}

/// The scenes loader's Bus name: `SCENES_SERVICE`, else `scenes`.
fn scenes_service() -> String {
    // Trimmed: " scenes " must route to `scenes`, not a padded dead name.
    std::env::var("SCENES_SERVICE")
        .ok()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "scenes".to_owned())
}

/// `(to, command, body)` for a chosen non-mode item. A config extra carries
/// `{"args":[…]}`; the built-in "Edit panels…" opens the shipped Scene Editor
/// with body exactly `{"safe":true}` (scene-editor plan §4.3 Q1).
fn bus_call(action: MenuAction, scenes: String) -> Option<(String, String, Value)> {
    match action {
        MenuAction::Mode(_) => None,
        MenuAction::Extra(extra) => Some((extra.target, extra.verb, json!({"args": extra.args}))),
        MenuAction::EditPanels => {
            Some((scenes, "scenes.editor.open".to_owned(), json!({"safe": true})))
        }
    }
}

fn invoke(world: &mut World, action: MenuAction) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0x4d_0000_0000);
    let Some((to, command, body)) = bus_call(action, scenes_service()) else {
        return;
    };
    let Some(bridge) = world.get_resource::<BusBridge>() else {
        warn!("Corner menu action unavailable: Bus worker missing");
        return;
    };
    if let Err(error) = bridge.try_call(
        NEXT.fetch_add(1, Ordering::Relaxed),
        to,
        command,
        Default::default(),
        body.to_string(),
    ) {
        warn!("Corner menu action refused: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_shell::chrome::corner_menu::EDIT_PANELS_LABEL;
    use cosmix_shell::core::{LogicalSize, ShellModel};
    use cosmix_shell::runtime::ShellRuntimePlugin;

    fn menu_app(output: &OutputKey) -> App {
        let model = ShellModel::new(
            output.clone(),
            LogicalSize::new(1000.0, 800.0).unwrap(),
            Default::default(),
            std::time::Duration::from_millis(800),
            std::time::Duration::from_millis(200),
        )
        .unwrap();
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, ShellRuntimePlugin::new(model)));
        app
    }

    #[test]
    fn corner_menu_reads_latest_config_and_invokes_extra_through_bus() {
        let output = OutputKey::new("test-output").unwrap();
        let mut app = menu_app(&output);
        open(app.world_mut(), &output, Corner::TopLeft);
        assert_eq!(app.world().resource::<CornerMenuRequest>().items.len(), 4);
        let mut config = ShellConfig::default();
        config.menu_items[Corner::TopLeft.summoned_edge().index()].push(crate::config::MenuItem {
            label: "Tools".into(),
            target: "tools".into(),
            verb: "tools.open".into(),
            args: vec!["main".into()],
        });
        app.insert_resource(config);
        open(app.world_mut(), &output, Corner::TopLeft);
        let item = app.world().resource::<CornerMenuRequest>().items[4].clone();
        assert!(matches!(item.action, MenuAction::Extra(_)), "expected extra");
        let (bridge, peer) = ctk::bus::test_bridge("menu-test");
        app.insert_resource(bridge);
        invoke(app.world_mut(), item.action);
        let calls = peer.drain_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].to, "tools");
        assert_eq!(calls[0].command, "tools.open");
        assert_eq!(
            serde_json::from_str::<Value>(&calls[0].body).unwrap(),
            json!({"args":["main"]})
        );
    }

    #[test]
    fn edit_panels_is_on_every_corner_and_sends_safe_open_to_the_loader() {
        let output = OutputKey::new("test-output").unwrap();
        let mut app = menu_app(&output);
        let (bridge, peer) = ctk::bus::test_bridge("menu-test");
        app.insert_resource(bridge);
        for with_extras in [false, true] {
            let mut config = ShellConfig::default();
            if with_extras {
                for edge in cosmix_shell::core::Edge::ALL {
                    config.menu_items[edge.index()].push(crate::config::MenuItem {
                        label: "Tools".into(),
                        target: "tools".into(),
                        verb: "tools.open".into(),
                        args: vec![],
                    });
                }
            }
            app.insert_resource(config);
            for corner in Corner::ALL {
                open(app.world_mut(), &output, corner);
                let items = app.world().resource::<CornerMenuRequest>().items.clone();
                let edit: Vec<_> = items
                    .iter()
                    .filter(|item| item.action == MenuAction::EditPanels)
                    .collect();
                assert_eq!(edit.len(), 1, "{corner:?} extras={with_extras}");
                assert_eq!(edit[0].label, EDIT_PANELS_LABEL);
                assert_eq!(items.len(), if with_extras { 5 } else { 4 });
                invoke(app.world_mut(), edit[0].action.clone());
                let calls = peer.drain_calls();
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].to, scenes_service());
                assert_eq!(calls[0].command, "scenes.editor.open");
                // Exactly {"safe":true}: not a config extra's {"args":[…]}.
                assert_eq!(calls[0].body, r#"{"safe":true}"#);
            }
        }
    }

    #[test]
    fn edit_panels_targets_the_configured_loader_service() {
        assert_eq!(
            bus_call(MenuAction::EditPanels, "scenes-gate".into()),
            Some((
                "scenes-gate".to_owned(),
                "scenes.editor.open".to_owned(),
                json!({"safe": true})
            ))
        );
        assert_eq!(
            bus_call(
                MenuAction::Mode(cosmix_shell::core::PanelMode::Pinned),
                "scenes".into()
            ),
            None
        );
    }
}
