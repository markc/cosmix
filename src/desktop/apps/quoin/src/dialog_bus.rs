//! Dialog and layout verbs (scene-editor plan §4.3 Q2): `shell.dialog.show`,
//! `shell.dialog.hide` and `shell.scene.layout`. They live here, not in
//! `bus_service.rs`, so Stage Q1 (which owns `bus_service.rs`) and Stage Q2
//! (which owns this file and `embedded.rs`) never edit the same file.
//!
//! `service_bus` hands each request over ([`DialogRequests::defer`]); [`answer`]
//! replies in the Model stage of the same update, with world access:
//! the layout verb reads the engine's `ComputedNode`/`UiGlobalTransform`,
//! which no Bus system parameter can reach. The request/reply/refusal shapes
//! are frozen in `src/desktop/scripts/tests/fixtures/scene-editor/shell-verbs.json`.
//! Like every Bus verb they are mesh-open with no owner gate; what stays
//! unconditional is correctness (the scene exists and is a dialog).
//!
//! Visibility is state in [`QuoinDialog`]; every change of `dialog.visible` or
//! `dialog.scene` reaches `shell.props.get` and `shell.panel.changed` through
//! [`notice`], so the loader sees a hide as a new revision.

use bevy::prelude::*;
use cosmix_scene_bevy::SceneStore;
use cosmix_shell::chrome::dialog::QuoinDialog;
use cosmix_shell::host::panel_layout;
use cosmix_shell::runtime::{ShellFrameState, ShellRuntimeSet};
use ctk::bus::{BusBridge, InboundRequest};
use serde_json::{Value, json};

/// Commands this module answers.
pub(crate) const VERBS: &[&str] = &["shell.dialog.show", "shell.dialog.hide", "shell.scene.layout"];

pub(crate) fn handles(command: &str) -> bool {
    VERBS.contains(&command)
}

/// Requests `service_bus` handed over this update, plus replies that met a
/// full outbound channel (answered once, retried as answered: a retried show
/// must not report `applied:false` for the change it made).
#[derive(Resource, Default)]
pub(crate) struct DialogRequests {
    queued: Vec<InboundRequest>,
    unsent: Vec<(InboundRequest, u8, String)>,
}

impl DialogRequests {
    pub(crate) fn defer(&mut self, request: InboundRequest) {
        self.queued.push(request);
    }
}

/// Answer in Model: after scene reconciliation has mirrored the seat, and
/// before Presentation, so a show or hide reaches this update's chrome and
/// `panel.changed` rather than waiting for another wake.
pub(crate) fn install(app: &mut App) {
    app.init_resource::<DialogRequests>()
        .add_systems(Update, answer.in_set(ShellRuntimeSet::Model));
}

/// `dialog` in `shell.props.get` and `shell.panel.changed`: `null` while no
/// dialog is loaded, else `{scene, visible, w, h, output}`.
pub(crate) fn notice(dialog: Option<&QuoinDialog>) -> Value {
    dialog.and_then(QuoinDialog::notice).map_or(Value::Null, |notice| {
        json!({
            "scene": notice.scene,
            "visible": notice.visible,
            "w": notice.w,
            "h": notice.h,
            "output": notice.output,
        })
    })
}

/// The same seat as a props-tree value, for the `dialog` leaf of
/// `shell.props.get`.
pub(crate) fn notice_prop(dialog: Option<&QuoinDialog>) -> cosmix_props_core::PropValue {
    use cosmix_props_core::PropValue;
    dialog.and_then(QuoinDialog::notice).map_or(PropValue::Null, |notice| {
        PropValue::Object(
            [
                ("scene".to_owned(), PropValue::from(notice.scene)),
                ("visible".to_owned(), PropValue::from(notice.visible)),
                ("w".to_owned(), PropValue::from(f64::from(notice.w))),
                ("h".to_owned(), PropValue::from(f64::from(notice.h))),
                ("output".to_owned(), PropValue::from(notice.output)),
            ]
            .into(),
        )
    })
}

fn answer(world: &mut World) {
    let (queued, unsent) = {
        let mut requests = world.resource_mut::<DialogRequests>();
        (
            std::mem::take(&mut requests.queued),
            std::mem::take(&mut requests.unsent),
        )
    };
    let mut replies = unsent;
    for request in queued {
        let args = request_args(&request);
        let (rc, body) = respond(world, &request.command, &args);
        replies.push((request, rc, body));
    }
    let mut retry = Vec::new();
    {
        let bridge = world.resource::<BusBridge>();
        for (request, rc, body) in replies {
            if let Err(error) = bridge.try_respond(&request, rc, body.clone()) {
                if bridge.worker_is_gone() {
                    warn!(command = request.command.as_str(), "shell Bus worker has stopped; dropping reply ({error})");
                } else {
                    retry.push((request, rc, body));
                }
            }
        }
    }
    world.resource_mut::<DialogRequests>().unsent = retry;
}

/// The JSON body is the argument map, as for every `shell.*` verb.
fn request_args(request: &InboundRequest) -> Value {
    serde_json::from_str(&request.body).unwrap_or(Value::Null)
}

fn refusal(code: &str, message: String, scene: Option<&str>) -> (u8, String) {
    let mut body = json!({"error_code": code, "message": message});
    if let Some(scene) = scene {
        body["scene"] = json!(scene);
    }
    (10, body.to_string())
}

/// Answer one verb against the live world. Public to the crate for tests.
pub(crate) fn respond(world: &mut World, command: &str, args: &Value) -> (u8, String) {
    let Some(scene) = args["scene"].as_str().filter(|scene| !scene.trim().is_empty()) else {
        return refusal("INVALID_ARGUMENT", "scene is required".into(), None);
    };
    let kind = world.resource::<SceneStore>().is_dialog(scene);
    match command {
        "shell.dialog.show" | "shell.dialog.hide" => {
            match kind {
                None => {
                    return refusal(
                        "NOT_FOUND",
                        format!("no scene named {scene} is loaded"),
                        Some(scene),
                    );
                }
                Some(false) => {
                    return refusal(
                        "NOT_DIALOG",
                        format!("scene {scene} is an edge page, not a dialog"),
                        Some(scene),
                    );
                }
                Some(true) => {}
            }
            let show = command == "shell.dialog.show";
            if show {
                // It maps on the selected output, wherever it was loaded.
                let output = world.resource::<ShellFrameState>().0.geometry.output.clone();
                world
                    .resource_mut::<SceneStore>()
                    .retarget_dialog(scene, &output);
            }
            // Reconcile mirrors the seat each update; take this update's
            // retarget now so the host maps on the right output.
            let seat = world.resource::<SceneStore>().dialog_seat().cloned();
            let mut dialog = world.resource_mut::<QuoinDialog>();
            if dialog.seat != seat {
                dialog.set_seat(seat);
            }
            let applied = if show { dialog.show(scene) } else { dialog.hide(scene) };
            match applied {
                Some(applied) => (
                    0,
                    json!({"scene": scene, "visible": show, "applied": applied}).to_string(),
                ),
                // A dialog scene that does not hold the seat (an unowned
                // load) has nothing a host could map.
                None => refusal(
                    "NOT_FOUND",
                    format!("dialog scene {scene} does not hold the dialog seat"),
                    Some(scene),
                ),
            }
        }
        "shell.scene.layout" => {
            let node = args["node"].as_str();
            let mut body = match cosmix_scene_bevy::scene_layout(world, scene, node) {
                Ok(body) => body,
                Err(error) => return (10, error.to_string()),
            };
            let (visible, surface) = if kind == Some(true) {
                dialog_surface(world, scene)
            } else {
                panel_surface(world, scene)
            };
            body["visible"] = json!(visible);
            body["surface"] = surface;
            if !visible {
                // An unmapped scene has no on-screen geometry to report.
                body["nodes"] = json!({});
                body["instances"] = json!({});
            } else {
                let offset = if kind == Some(true) {
                    dialog_root_origin(world)
                } else {
                    embedded_panel_origin(world, scene)
                };
                if offset != Vec2::ZERO {
                    shift_rects(&mut body, offset);
                }
            }
            body["chrome"] = frame_controls(world, kind == Some(true) && visible);
            (0, body.to_string())
        }
        _ => refusal("UNKNOWN_COMMAND", format!("{command} is not a dialog verb"), None),
    }
}

/// Rects are relative to the scene's surface. On the layer host the dialog
/// chrome root fills its own window, so its origin is zero; in comp's
/// renderer the root sits at the dialog's output position. Measuring the
/// root keeps one rule for both hosts.
fn dialog_root_origin(world: &World) -> Vec2 {
    world
        .resource::<QuoinDialog>()
        .root
        .and_then(|root| {
            let node = world.get::<bevy::ui::ComputedNode>(root)?;
            let transform = world.get::<bevy::ui::UiGlobalTransform>(root)?;
            Some(transform.transform_point2(node.border_box().min) * node.inverse_scale_factor)
        })
        .unwrap_or(Vec2::ZERO)
}

/// Quoin's own frame controls on a mapped dialog, measured from the engine
/// like node rects and in the same surface coordinates: `{close:{x,y,w,h}}`
/// for the ×, or `{}` when there is none (unmapped, `chrome:false`, or an
/// edge scene).
fn frame_controls(world: &World, mapped_dialog: bool) -> Value {
    let dialog = world.resource::<QuoinDialog>();
    let close = mapped_dialog
        .then_some(())
        .filter(|()| dialog.seat.as_ref().is_some_and(|seat| seat.chrome))
        .and_then(|()| dialog.root)
        .and_then(|root| world.get::<cosmix_shell::chrome::dialog::QuoinDialogParts>(root))
        .and_then(|parts| {
            let node = world.get::<bevy::ui::ComputedNode>(parts.close)?;
            let transform = world.get::<bevy::ui::UiGlobalTransform>(parts.close)?;
            let border = node.border_box();
            let min = transform.transform_point2(border.min) * node.inverse_scale_factor;
            let max = transform.transform_point2(border.max) * node.inverse_scale_factor;
            Some((min - dialog_root_origin(world), max - min))
        });
    match close {
        Some((at, size)) => json!({"close": {"x": at.x, "y": at.y, "w": size.x, "h": size.y}}),
        None => json!({}),
    }
}

/// In comp's renderer every panel shares the output camera; a layer-host
/// panel is its own window and needs no shift.
fn embedded_panel_origin(world: &World, scene: &str) -> Vec2 {
    if !world.contains_resource::<crate::embedded::EmbeddedOutput>() {
        return Vec2::ZERO;
    }
    let Some((_, edge)) = world.resource::<SceneStore>().edge_page(scene) else {
        return Vec2::ZERO;
    };
    let rect = panel_layout(&world.resource::<ShellFrameState>().0).panels[edge.index()];
    Vec2::new(rect.x, rect.y)
}

fn shift_rects(body: &mut Value, offset: Vec2) {
    let mut shift = |rect: &mut Value| {
        for (key, delta) in [("x", offset.x), ("y", offset.y)] {
            if let Some(value) = rect[key].as_f64() {
                rect[key] = json!(value - f64::from(delta));
            }
        }
    };
    if let Some(nodes) = body["nodes"].as_object_mut() {
        nodes.values_mut().for_each(&mut shift);
    }
    if let Some(lists) = body["instances"].as_object_mut() {
        for rows in lists.values_mut() {
            if let Some(rows) = rows.as_object_mut() {
                rows.values_mut().for_each(&mut shift);
            }
        }
    }
}

/// A dialog is on screen while it is visible and its host has placed it
/// (`origin` is set by the layer host on map and by the embedded host).
fn dialog_surface(world: &World, scene: &str) -> (bool, Value) {
    let dialog = world.resource::<QuoinDialog>();
    let Some(seat) = dialog.seat.as_ref().filter(|seat| seat.scene == scene) else {
        return (false, Value::Null);
    };
    let origin = dialog.origin.unwrap_or(Vec2::ZERO);
    let visible = dialog.visible && dialog.origin.is_some();
    (
        visible,
        json!({
            "kind": "dialog", "edge": null, "output": seat.output.as_str(),
            "x": origin.x, "y": origin.y, "w": seat.w, "h": seat.h,
        }),
    )
}

/// An edge scene is on screen while its panel is mapped with its page active.
fn panel_surface(world: &World, scene: &str) -> (bool, Value) {
    let Some((page, edge)) = world.resource::<SceneStore>().edge_page(scene) else {
        return (false, Value::Null);
    };
    let frame = &world.resource::<ShellFrameState>().0;
    let panel = frame.panel(edge);
    let rect = panel_layout(frame).panels[edge.index()];
    let visible = panel.mapped && panel.active_page_id.as_deref() == Some(page.as_str());
    (
        visible,
        json!({
            "kind": "panel", "edge": edge.as_str(), "output": frame.geometry.output.as_str(),
            "x": rect.x, "y": rect.y, "w": rect.width, "h": rect.height,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_shell::core::{LogicalSize, OutputKey, ShellModel};
    use cosmix_shell::runtime::{SceneVerb, ShellRuntimePlugin, SubPanelRegistryState};
    use std::time::Duration;

    const EDITOR: &str = "---\nscene: 1\nname: editor\ncitizen: scene-editor\nwindow: {\"chrome\":true,\"h\":620,\"kind\":\"dialog\",\"title\":\"Scene Editor\",\"w\":880}\n---\n```mix\nroot: {widget: \"column\", children: [\"caption\"]}\ncaption: {widget: \"text\", text: \"Scene Editor\"}\n```\n";
    const PANEL: &str = "---\nscene: 1\nname: panel\ncitizen: scene-panel\nwindow: {\"kind\":\"edge\",\"edge\":\"bottom\",\"h\":52}\n---\n```mix\nroot: {widget: \"row\", children: [\"clock\"]}\nclock: {widget: \"text\", text: \"12:00\"}\n```\n";

    fn fixture() -> Value {
        serde_json::from_str(include_str!(
            "../../../scripts/tests/fixtures/scene-editor/shell-verbs.json"
        ))
        .unwrap()
    }

    fn world() -> App {
        let model = ShellModel::new(
            OutputKey::new("DP-1").unwrap(),
            LogicalSize::new(1920.0, 1080.0).unwrap(),
            Duration::ZERO,
            Duration::from_millis(800),
            Duration::from_millis(200),
        )
        .unwrap();
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, ShellRuntimePlugin::new(model)))
            .init_resource::<SceneStore>()
            .init_resource::<QuoinDialog>()
            .init_resource::<SubPanelRegistryState>();
        for source in [EDITOR, PANEL] {
            load(app.world_mut(), source);
        }
        // Mirror the seat as reconcile would.
        let seat = app.world().resource::<SceneStore>().dialog_seat().cloned();
        app.world_mut().resource_mut::<QuoinDialog>().set_seat(seat);
        app
    }

    fn load(world: &mut World, source: &str) {
        world.resource_scope(|world, mut store: Mut<SceneStore>| {
            world.resource_scope(|world, mut registry: Mut<SubPanelRegistryState>| {
                let output = world.resource::<ShellFrameState>().0.geometry.output.clone();
                let (bridge, _peer) = ctk::bus::test_bridge("shell");
                let (rc, body) = store.dispatch(
                    SceneVerb::Load,
                    "",
                    &json!({"source": source, "model_generation": 1}),
                    &bridge,
                    &mut cosmix_scene_bevy::SceneMount {
                        registry: &mut registry.0,
                        output: &output,
                        owner: "scenes",
                        accepted_at: 1,
                    },
                );
                assert_eq!(rc, 0, "{body}");
            });
        });
    }

    fn call(world: &mut World, command: &str, args: Value) -> (u8, Value) {
        let (rc, body) = respond(world, command, &args);
        (rc, serde_json::from_str(&body).unwrap())
    }

    #[test]
    fn show_and_hide_are_idempotent_with_applied_and_match_the_fixture() {
        let fixture = fixture();
        let mut app = world();
        let world = app.world_mut();
        for (command, expect_applied) in [
            ("shell.dialog.show", true),
            ("shell.dialog.show", false),
            ("shell.dialog.hide", true),
            ("shell.dialog.hide", false),
        ] {
            let (rc, body) = call(world, command, fixture[command]["request"].clone());
            assert_eq!(rc, 0);
            let mut expected = fixture[command]["reply"].clone();
            expected["applied"] = json!(expect_applied);
            assert_eq!(body, expected, "{command}");
        }
        assert!(world.resource::<SceneStore>().dialog_seat().is_some(), "hide keeps the scene");
        assert!(!world.resource::<QuoinDialog>().visible);
    }

    #[test]
    fn show_refusals_match_the_fixture_codes() {
        let fixture = fixture();
        let mut app = world();
        let world = app.world_mut();
        for command in ["shell.dialog.show", "shell.dialog.hide"] {
            let (rc, body) = call(world, command, json!({"scene":"absent"}));
            assert_eq!((rc, &body["error_code"]), (10, &fixture[command]["refusals"]["NOT_FOUND"]["error_code"]));
            let (rc, body) = call(world, command, json!({"scene":"panel"}));
            assert_eq!((rc, &body["error_code"]), (10, &fixture[command]["refusals"]["NOT_DIALOG"]["error_code"]));
            assert_eq!(body["scene"], "panel");
        }
        let (rc, body) = call(world, "shell.dialog.show", json!({}));
        assert_eq!((rc, body["error_code"].as_str()), (10, Some("INVALID_ARGUMENT")));
    }

    #[test]
    fn a_show_maps_on_the_selected_output() {
        let mut app = world();
        let world = app.world_mut();
        let hdmi = OutputKey::new("HDMI-A-1").unwrap();
        world.resource_mut::<SceneStore>().retarget_dialog("editor", &hdmi);
        call(world, "shell.dialog.show", json!({"scene":"editor"}));
        let dialog = world.resource::<QuoinDialog>();
        assert_eq!(dialog.output().map(OutputKey::as_str), Some("DP-1"));
        assert!(dialog.visible);
    }

    #[test]
    fn layout_is_empty_while_unmapped_and_reports_the_dialog_surface() {
        let fixture = fixture();
        let mut app = world();
        let world = app.world_mut();
        let (rc, body) = call(world, "shell.scene.layout", json!({"scene":"editor"}));
        assert_eq!(rc, 0);
        let unmapped = &fixture["shell.scene.layout"]["reply_unmapped"];
        assert_eq!(body["visible"], false);
        assert_eq!(body["nodes"], json!({}));
        assert_eq!(body["instances"], json!({}));
        assert_eq!(body["surface"]["kind"], unmapped["surface"]["kind"]);
        assert_eq!(body["surface"]["edge"], Value::Null);
        assert_eq!((body["surface"]["w"].as_f64(), body["surface"]["h"].as_f64()), (Some(880.0), Some(620.0)));
        // Shown but not yet placed by a host: still not on screen.
        call(world, "shell.dialog.show", json!({"scene":"editor"}));
        let (_, body) = call(world, "shell.scene.layout", json!({"scene":"editor"}));
        assert_eq!(body["visible"], false);
        world.resource_mut::<QuoinDialog>().origin = Some(Vec2::new(520.0, 204.0));
        let (_, body) = call(world, "shell.scene.layout", json!({"scene":"editor"}));
        assert_eq!(body["visible"], true);
        assert_eq!((body["surface"]["x"].as_f64(), body["surface"]["y"].as_f64()), (Some(520.0), Some(204.0)));
        let keys: Vec<_> = fixture["shell.scene.layout"]["reply"].as_object().unwrap().keys().cloned().collect();
        let got: Vec<_> = body.as_object().unwrap().keys().cloned().collect();
        assert_eq!(got, keys, "reply carries exactly the fixture's fields");
        let (rc, body) = call(world, "shell.scene.layout", json!({"scene":"absent"}));
        assert_eq!((rc, body["error_code"].as_str()), (10, Some("NOT_FOUND")));
        // An edge scene reports its panel surface.
        let (rc, body) = call(world, "shell.scene.layout", json!({"scene":"panel"}));
        assert_eq!(rc, 0);
        assert_eq!((body["surface"]["kind"].as_str(), body["surface"]["edge"].as_str()), (Some("panel"), Some("bottom")));
    }

    #[test]
    fn notice_is_null_without_a_dialog_and_carries_the_fixture_fields() {
        assert_eq!(notice(None), Value::Null);
        assert_eq!(notice(Some(&QuoinDialog::default())), Value::Null);
        let app = world();
        let world = app.world();
        let props: Value = serde_json::from_str(include_str!(
            "../../../scripts/tests/fixtures/scene-editor/props-snapshots.json"
        ))
        .unwrap();
        assert_eq!(
            notice(Some(world.resource::<QuoinDialog>())),
            props["props_get"]["dialog"],
            "hidden editor on DP-1"
        );
        assert_eq!(
            Value::from(&notice_prop(Some(world.resource::<QuoinDialog>()))),
            props["props_get"]["dialog"],
            "props.get carries the same value"
        );
    }

    #[test]
    fn deferred_requests_are_answered_through_the_bridge() {
        let (bridge, peer) = ctk::bus::test_bridge("shell");
        let mut app = world();
        let world = app.world_mut();
        world.insert_resource(bridge);
        world.init_resource::<DialogRequests>();
        let request = InboundRequest {
            connection_generation: 1,
            from: "peer".into(),
            command: "shell.dialog.show".into(),
            headers: Default::default(),
            body: json!({"scene":"editor"}).to_string(),
            reply_id: Some("1".into()),
        };
        world.resource_mut::<DialogRequests>().defer(request);
        answer(world);
        let replies = peer.drain_responses();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].rc, 0);
        let body: Value = serde_json::from_str(&replies[0].body).unwrap();
        assert_eq!(body, json!({"scene":"editor","visible":true,"applied":true}));
    }
}
