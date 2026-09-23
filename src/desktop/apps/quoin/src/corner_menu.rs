//! Bind the native menu to the latest config and the existing Bus worker.
use crate::config::ShellConfig;
use bevy::prelude::*;
use cosmix_shell::chrome::corner_menu::{
    CornerMenuExtraHook, CornerMenuRequest, MenuExtra, menu_items,
};
use cosmix_shell::core::{Corner, OutputKey};
use cosmix_shell::runtime::ShellFrameState;
use cosmix_shell_host::CornerMenuHook;
use ctk::bus::BusBridge;

pub(crate) fn install(app: &mut App) {
    app.insert_resource(CornerMenuHook(open))
        .insert_resource(CornerMenuExtraHook(invoke));
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

fn invoke(world: &mut World, item: MenuExtra) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0x4d_0000_0000);
    let Some(bridge) = world.get_resource::<BusBridge>() else {
        warn!("Corner menu action unavailable: Bus worker missing");
        return;
    };
    if let Err(error) = bridge.try_call(
        NEXT.fetch_add(1, Ordering::Relaxed),
        item.target,
        item.verb,
        Default::default(),
        serde_json::json!({"args": item.args}).to_string(),
    ) {
        warn!("Corner menu action refused: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_shell::core::{LogicalSize, ShellModel};
    use cosmix_shell::runtime::ShellRuntimePlugin;
    #[test]
    fn corner_menu_reads_latest_config_and_invokes_extra_through_bus() {
        let output = OutputKey::new("test-output").unwrap();
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
        open(app.world_mut(), &output, Corner::TopLeft);
        assert_eq!(app.world().resource::<CornerMenuRequest>().items.len(), 3);
        let mut config = ShellConfig::default();
        config.menu_items[Corner::TopLeft.summoned_edge().index()].push(crate::config::MenuItem {
            label: "Tools".into(),
            target: "tools".into(),
            verb: "tools.open".into(),
            args: vec!["main".into()],
        });
        app.insert_resource(config);
        open(app.world_mut(), &output, Corner::TopLeft);
        let item = app.world().resource::<CornerMenuRequest>().items[3].clone();
        let cosmix_shell::chrome::corner_menu::MenuAction::Extra(extra) = item.action else {
            panic!("expected extra");
        };
        let (bridge, peer) = ctk::bus::test_bridge("menu-test");
        app.insert_resource(bridge);
        invoke(app.world_mut(), extra);
        let calls = peer.drain_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].to, "tools");
        assert_eq!(calls[0].command, "tools.open");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&calls[0].body).unwrap(),
            serde_json::json!({"args":["main"]})
        );
    }
}
