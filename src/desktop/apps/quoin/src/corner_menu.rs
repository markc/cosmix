//! Bind the native menu to the latest config and the existing Bus worker.
use crate::config::ShellConfig;
use bevy::prelude::*;
use cosmix_shell::chrome::corner_menu::{
    CornerMenuActionHook, CornerMenuRequest, MenuAction, MenuExtra, confirm_items, menu_items,
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

/// Where the last corner menu opened: a confirm step reopens there.
#[derive(Resource, Clone)]
struct MenuAnchor {
    output: OutputKey,
    corner: Corner,
}

fn open(world: &mut World, output: &OutputKey, corner: Corner) {
    world.insert_resource(MenuAnchor {
        output: output.clone(),
        corner,
    });
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
                    confirm: item.confirm.clone(),
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
        MenuAction::Mode(_) | MenuAction::Inert => None,
        MenuAction::Extra(extra) => Some((extra.target, extra.verb, json!({"args": extra.args}))),
        MenuAction::EditPanels => {
            Some((scenes, "scenes.editor.open".to_owned(), json!({"safe": true})))
        }
    }
}

/// The session-control citizen's Bus name: `DESKTOP_SESSION_SERVICE`, else
/// `desktop-session`.
fn session_service() -> String {
    std::env::var("DESKTOP_SESSION_SERVICE")
        .ok()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "desktop-session".to_owned())
}

/// The built-in session actions `shell.session.confirm` opens a confirm step
/// for: `(label, verb, question)`.
fn session_action(action: &str) -> Option<(&'static str, &'static str, &'static str)> {
    match action {
        "restart" => Some((
            "Restart session",
            "desktop.session.restart",
            "Restart the session? Every window closes; agent sessions resume.",
        )),
        "leave" => Some((
            "Leave seat",
            "desktop.session.leave",
            "Leave this seat? The desktop keeps running on its VT.",
        )),
        _ => None,
    }
}

/// The names `shell.session.confirm` accepts, for its refusal message.
pub(crate) const SESSION_ACTIONS: [&str; 2] = ["restart", "leave"];

/// The confirm step for a built-in session action, at `corner` of `output`:
/// the question, the action (a `desktop.session.*` call to the session-control
/// citizen), then Cancel. `None` for an unknown action.
pub(crate) fn session_confirm_request(
    action: &str,
    output: OutputKey,
    corner: Corner,
) -> Option<CornerMenuRequest> {
    let (label, verb, question) = session_action(action)?;
    let extra = MenuExtra {
        label: label.to_owned(),
        target: session_service(),
        verb: verb.to_owned(),
        args: Vec::new(),
        confirm: None,
    };
    Some(CornerMenuRequest {
        output,
        corner,
        items: confirm_items(question, label, MenuAction::Extra(extra)),
    })
}

/// A chosen extra that asks to be confirmed reopens the menu as its confirm
/// step, where the menu last opened; the verb is only called from there.
fn confirm_request(world: &World, extra: &MenuExtra) -> Option<CornerMenuRequest> {
    let question = extra.confirm.as_ref()?;
    let anchor = world.get_resource::<MenuAnchor>()?;
    let confirmed = MenuExtra {
        confirm: None,
        ..extra.clone()
    };
    Some(CornerMenuRequest {
        output: anchor.output.clone(),
        corner: anchor.corner,
        items: confirm_items(question, &extra.label, MenuAction::Extra(confirmed)),
    })
}

fn invoke(world: &mut World, action: MenuAction) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0x4d_0000_0000);
    if let MenuAction::Extra(extra) = &action
        && extra.confirm.is_some()
    {
        // Never the verb itself: without an anchor (no menu ever opened) the
        // step cannot be shown, and the choice does nothing.
        if let Some(request) = confirm_request(world, extra) {
            world.insert_resource(request);
        } else {
            warn!("Corner menu confirm step unavailable: no menu anchor");
        }
        return;
    }
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
            confirm: None,
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
                        confirm: None,
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

    /// A config extra with `confirm` never calls its verb when chosen: it
    /// reopens the menu, where it opened, as Question / action / Cancel, and
    /// only the action row calls the verb.
    #[test]
    fn a_confirming_extra_calls_its_verb_only_from_the_confirm_step() {
        use cosmix_shell::chrome::corner_menu::CANCEL_LABEL;
        let output = OutputKey::new("test-output").unwrap();
        let mut app = menu_app(&output);
        let mut config = ShellConfig::default();
        config.menu_items[Corner::BottomLeft.summoned_edge().index()].push(crate::config::MenuItem {
            label: "Leave seat…".into(),
            target: "desktop-session".into(),
            verb: "desktop.session.leave".into(),
            args: vec![],
            confirm: Some("Leave this seat?".into()),
        });
        app.insert_resource(config);
        let (bridge, peer) = ctk::bus::test_bridge("menu-test");
        app.insert_resource(bridge);
        open(app.world_mut(), &output, Corner::BottomLeft);
        let chosen = app.world_mut().remove_resource::<CornerMenuRequest>().unwrap().items[4].clone();
        assert_eq!(chosen.label, "Leave seat…");
        invoke(app.world_mut(), chosen.action);
        assert!(peer.drain_calls().is_empty(), "a confirming extra called its verb directly");
        let step = app.world_mut().remove_resource::<CornerMenuRequest>().expect("confirm step");
        assert_eq!((step.output.clone(), step.corner), (output.clone(), Corner::BottomLeft));
        assert_eq!(
            step.items.iter().map(|i| (i.label.as_str(), i.checked)).collect::<Vec<_>>(),
            [("Leave this seat?", true), ("Leave seat…", false), (CANCEL_LABEL, false)]
        );
        // Cancel does nothing.
        invoke(app.world_mut(), step.items[2].action.clone());
        assert!(peer.drain_calls().is_empty());
        assert!(!app.world().contains_resource::<CornerMenuRequest>());
        // The action row calls the verb, once, with the extra's body.
        invoke(app.world_mut(), step.items[1].action.clone());
        let calls = peer.drain_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!((calls[0].to.as_str(), calls[0].command.as_str()), ("desktop-session", "desktop.session.leave"));
        assert_eq!(serde_json::from_str::<Value>(&calls[0].body).unwrap(), json!({"args": []}));
        assert!(!app.world().contains_resource::<CornerMenuRequest>(), "the confirmed choice reopened the menu");
    }

    #[test]
    fn session_confirm_request_is_question_action_cancel() {
        let output = OutputKey::new("test-output").unwrap();
        for (action, verb) in [("restart", "desktop.session.restart"), ("leave", "desktop.session.leave")] {
            let request = session_confirm_request(action, output.clone(), Corner::TopRight).unwrap();
            assert_eq!(request.corner, Corner::TopRight);
            assert_eq!(request.items.len(), 3);
            assert!(request.items[0].checked);
            let MenuAction::Extra(extra) = &request.items[1].action else {
                panic!("{action}: the action row is not a Bus call");
            };
            assert_eq!((extra.target.as_str(), extra.verb.as_str(), extra.confirm.as_ref()), ("desktop-session", verb, None));
            assert_eq!(request.items[2].action, MenuAction::Inert);
        }
        assert!(session_confirm_request("reboot", output, Corner::TopLeft).is_none());
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
