use super::*;

const FIXTURES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../scripts/tests/fixtures/scenes"
);
const TEMPLATES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../../share/scenes");

fn resolve(source: &str, model: Option<Value>) -> ResolvedScene {
    let mut doc = cosmix_scene::parse(source).unwrap();
    if let Some(model) = model {
        doc.model = Some(model);
    }
    let diagnostics = cosmix_scene::lint(&doc);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    cosmix_scene::resolve(&doc).unwrap()
}

fn visible_node(
    id: &str,
    nodes: &BTreeMap<String, SceneNode>,
    lists: &BTreeMap<String, ListData>,
) -> Option<Value> {
    let node = &nodes[id];
    if flag(node, "hidden") || (flag(node, "hidden_if_empty") && rows(node).is_empty()) {
        return None;
    }
    let mut ports = node.ports.clone();
    ports.shift_remove("hidden");
    ports.shift_remove("children");
    let children: Vec<_> = children(node)
        .filter_map(|child| visible_node(child, nodes, lists))
        .collect();
    let instances: Vec<_> = if node.family == "list" {
        rows(node)
            .iter()
            .map(|item| {
                let instantiated = &lists[id].instances[item["id"].as_str().unwrap()];
                visible_node(text(node, "row"), instantiated, lists).unwrap()
            })
            .collect()
    } else {
        Vec::new()
    };
    Some(
        json!({"id":id,"family":node.family,"ports":ports,"children":children,"instances":instances}),
    )
}

#[test]
fn exact_old_captures_match_static_scene_and_live_model_trees() {
    let cases: Value =
        serde_json::from_str(&std::fs::read_to_string(format!("{FIXTURES}/cases.json")).unwrap())
            .unwrap();
    for case in cases.as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let name = case["name"].as_str().unwrap();
        let old = resolve(
            &std::fs::read_to_string(format!("{FIXTURES}/{id}.scene.mix")).unwrap(),
            None,
        );
        let model = serde_json::from_str(
            &std::fs::read_to_string(format!("{FIXTURES}/{id}.model.json")).unwrap(),
        )
        .unwrap();
        let new = resolve(
            &std::fs::read_to_string(format!("{TEMPLATES}/{name}/scene.mix")).unwrap(),
            Some(model),
        );
        assert_eq!(page_id(&new), format!("scene-{name}"));
        assert_eq!(new.window, old.window);
        let old_lists = prepare_lists(&old).unwrap();
        let new_lists = prepare_lists(&new).unwrap();
        let old_nodes = old
            .nodes
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let new_nodes = new
            .nodes
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        // The sole structural difference is the hidden notes alternative;
        // Display::None removes it, including its surrounding gap allocation.
        assert_eq!(
            visible_node("root", &old_nodes, &old_lists),
            visible_node("root", &new_nodes, &new_lists),
            "{id}"
        );
    }
}

fn sample(flow: &str) -> ResolvedScene {
    resolve(
        &format!(
            r#"---
scene: 1
name: repeated
citizen: scene-example
model: {{"prefix":"live ","hide":false}}
---
```mix
root: {{widget: "list", flow: "{flow}", gap: 7, align: "center", row: "row", row_height: 30, on_click: "choose", rows: [{{id: "a@b:c", cells: ["Alpha"]}}, {{id: "second", cells: ["Beta"]}}]}}
row: {{widget: "row", children: ["label"], on_click: "ignored", hidden: "= $model.hide"}}
label: {{widget: "text", text: "= $model.prefix .. $item.cells[0]"}}
```
"#
        ),
        None,
    )
}

fn mounted(world: &mut World, tree: &ResolvedScene) -> Mounted {
    Mounted {
        revision: 0,
        tree: ResolvedScene {
            nodes: Default::default(),
            ..tree.clone()
        },
        page: world.spawn_empty().id(),
        edge: Edge::Right,
        registered: false,
        nodes: BTreeMap::new(),
    }
}

#[test]
fn horizontal_rows_retain_entities_across_reorder_and_use_live_model() {
    let mut tree = sample("horizontal");
    let mut world = World::new();
    let mut mounted = mounted(&mut world, &tree);
    apply(&mut world, &mut mounted, &tree);
    let list = mounted.nodes["root"].root;
    let flow = &world.get::<FlowRows>(list).unwrap().0;
    let first = flow["a@b:c"];
    let second = flow["second"];
    let row = world.get::<RowInstances>(first).unwrap().views["row"]
        .0
        .root;
    let label = world.get::<RowInstances>(first).unwrap().views["label"]
        .0
        .label
        .unwrap();
    assert_eq!(world.get::<Text>(label).unwrap().0, "live Alpha");
    assert_eq!(world.get::<Node>(row).unwrap().width, Val::Auto);
    assert_eq!(world.get::<Node>(list).unwrap().column_gap, px(7));
    assert_eq!(
        world.get::<Node>(list).unwrap().align_items,
        AlignItems::Center
    );
    assert_eq!(world.get::<Node>(list).unwrap().height, Val::Auto);
    tree.model["prefix"] = json!("updated ");
    tree.model["hide"] = json!(true);
    tree.nodes["root"].ports["rows"]
        .as_array_mut()
        .unwrap()
        .reverse();
    apply(&mut world, &mut mounted, &tree);
    assert_eq!(
        world
            .get::<Children>(list)
            .unwrap()
            .iter()
            .collect::<Vec<_>>(),
        vec![second, first]
    );
    assert_eq!(
        world.get::<RowInstances>(first).unwrap().views["row"]
            .0
            .root,
        row
    );
    assert_eq!(world.get::<Text>(label).unwrap().0, "updated Alpha");
    assert_eq!(world.get::<Node>(row).unwrap().display, Display::None);
    assert_eq!(world.get::<Node>(first).unwrap().display, Display::None);
    let binding = world.get::<Binding>(row).unwrap();
    assert_eq!(binding.click.as_deref(), Some("choose"));
    assert_eq!(
        binding.payload("click", None),
        json!({"scene":"repeated","node":"root","kind":"click","item":{"id":"a@b:c","cells":["Alpha"]}})
    );
}

#[test]
fn vertical_rebind_retains_identity_and_binding_results_are_literal() {
    let mut tree = sample("vertical");
    let mut world = World::new();
    let mut mounted = mounted(&mut world, &tree);
    apply(&mut world, &mut mounted, &tree);
    let model = ListModel(mounted.nodes["root"].list.as_ref().unwrap().clone());
    let content = world.spawn_empty().id();
    model.bind(&mut world, content, 0);
    let row = world.get::<RowInstances>(content).unwrap().views["row"]
        .0
        .root;
    assert_eq!(world.get::<Node>(row).unwrap().width, percent(100));
    tree.model["prefix"] = json!("{cells[0]} ");
    apply(&mut world, &mut mounted, &tree);
    model.bind(&mut world, content, 0);
    assert_eq!(
        world.get::<RowInstances>(content).unwrap().views["row"]
            .0
            .root,
        row
    );
    let label = world.get::<RowInstances>(content).unwrap().views["label"]
        .0
        .label
        .unwrap();
    assert_eq!(world.get::<Text>(label).unwrap().0, "{cells[0]} Alpha");
    model.bind(&mut world, content, 1);
    assert!(
        world.get_entity(row).is_err(),
        "recycled content loses the old item's identity"
    );
    assert_eq!(
        world
            .get::<Binding>(content)
            .unwrap()
            .item
            .as_ref()
            .unwrap()["id"],
        "second"
    );
}

#[test]
fn emitted_click_request_contains_list_item_and_actual_citizen() {
    let mut tree = sample("horizontal");
    tree.nodes.get_mut("root").unwrap().ports["rows"][0]["extra"] =
        json!({"action":"open", "generation":7});
    let data = prepare_lists(&tree).unwrap();
    let mut world = World::new();
    let content = world.spawn_empty().id();
    bind_row(
        &mut world,
        content,
        &data["root"],
        &rows(&tree.nodes["root"])[0],
    );
    let (bridge, peer) = ctk::bus::test_bridge("shell-test");
    let expected = json!({
        "scene": "repeated", "node": "root", "kind": "click",
        "item": {"id": "a@b:c", "cells": ["Alpha"], "extra": {"action":"open", "generation":7}},
    });
    let label = world.get::<RowInstances>(content).unwrap().views["label"]
        .0
        .label
        .unwrap();
    world.insert_resource(bridge);
    world.init_resource::<Events>();
    world.add_observer(row_click);
    world.flush();
    use bevy::camera::NormalizedRenderTarget;
    use bevy::picking::{
        backend::HitData,
        pointer::{Location, PointerButton, PointerId},
    };
    use bevy::window::WindowRef;
    // An unhandled secondary click propagates all the way to the window.
    // Using `label` as the window creates a traversal cycle back into the row.
    let window = world.spawn(Window::default()).id();
    let location = Location {
        target: NormalizedRenderTarget::Window(WindowRef::Entity(window).normalize(None).unwrap()),
        position: Vec2::ZERO,
    };
    // Start on the actual Text descendant: propagation must reach the row,
    // use its owning list binding, then stop before sending a duplicate.
    world.trigger(Pointer::new(
        PointerId::Mouse,
        location.clone(),
        Click {
            button: PointerButton::Primary,
            hit: HitData::new(label, 0.0, None, None),
            duration: Duration::ZERO,
            count: 1,
        },
        label,
    ));
    let calls = peer.drain_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].to, "scene-example");
    assert_eq!(calls[0].command, "choose");
    assert_eq!(
        serde_json::from_str::<Value>(&calls[0].body).unwrap(),
        expected
    );
    world.trigger(Pointer::new(
        PointerId::Mouse,
        location,
        Click {
            button: PointerButton::Secondary,
            hit: HitData::new(label, 0.0, None, None),
            duration: Duration::ZERO,
            count: 1,
        },
        label,
    ));
    assert!(peer.drain_calls().is_empty());
}

#[test]
fn total_instantiation_budget_is_shared_across_lists_and_failure_is_atomic() {
    let rows: Vec<_> = (0..500)
        .map(|i| json!({"id":i.to_string(),"cells":[]}))
        .collect();
    let labels: Vec<_> = (0..16).map(|i| format!("label{i}")).collect();
    let mut body = format!(
        "root: {{widget: \"column\", children: [\"a\",\"b\"]}}\na: {{widget: \"list\", row: \"entry\", row_height: 20, rows: {}}}\nb: {{widget: \"list\", row: \"entry\", row_height: 20, rows: {}}}\nentry: {{widget: \"row\", children: {}}}\n",
        json!(rows),
        json!(rows),
        json!(labels)
    );
    for id in labels {
        body.push_str(&format!("{id}: {{widget: \"text\", text: \"literal\"}}\n"));
    }
    let source = format!("---\nscene: 1\nname: budget\ncitizen: test\n---\n```mix\n{body}```\n");
    let tree = resolve(&source, None);
    // Each list alone has 8,500 nodes. Together they exceed 16,384.
    assert!(prepare_lists(&tree).is_err());
    let good = sample("horizontal");
    let mut world = World::new();
    let mut mounted = mounted(&mut world, &good);
    apply(&mut world, &mut mounted, &good);
    let root = mounted.nodes["root"].root;
    apply(&mut world, &mut mounted, &tree);
    assert_eq!(mounted.tree, good);
    assert_eq!(mounted.nodes["root"].root, root);
    assert!(world.get_entity(root).is_ok());
    let mut store = SceneStore::default();
    let stable = "---\nscene: 1\nname: budget\ncitizen: test\n---\n```mix\nroot: {widget: \"text\", text: \"last good\"}\n```\n";
    store
        .request(cosmix_shell::runtime::SceneVerb::Load, stable, &Value::Null)
        .unwrap();
    let revision = store.scenes["budget"].revision;
    let refusal = store
        .request(
            cosmix_shell::runtime::SceneVerb::Load,
            &source,
            &Value::Null,
        )
        .unwrap_err();
    assert_eq!(refusal["error_code"], "scene_template");
    assert!(refusal["message"].is_string());
    assert_eq!(store.scenes["budget"].revision, revision);
    assert_eq!(
        store.scenes["budget"].tree.nodes["root"].ports["text"],
        "last good"
    );
}

#[test]
fn captured_launcher_icon_and_missing_icon_use_the_real_renderer() {
    let source = std::fs::read_to_string(format!("{TEMPLATES}/launcher/scene.mix")).unwrap();
    let mut model: Value = serde_json::from_str(
        &std::fs::read_to_string(format!("{FIXTURES}/launcher-filtered.model.json")).unwrap(),
    )
    .unwrap();
    let icon = format!("{FIXTURES}/example.svg");
    model["rows"][0]["cells"][2] = json!(icon);
    let tree = resolve(&source, Some(model));
    let data = prepare_lists(&tree).unwrap();
    let mut world = World::new();
    let content = world.spawn_empty().id();
    bind_row(
        &mut world,
        content,
        &data["list"],
        &rows(&tree.nodes["list"])[0],
    );
    let image = world.get::<RowInstances>(content).unwrap().views["app_icon"]
        .0
        .root;
    let handle = world.get::<ImageNode>(image).unwrap().image.clone();
    assert_eq!(
        Some(handle.clone()),
        icons::load(&mut world, &icon, 32.0, 32.0)
    );
    let size = world
        .resource::<Assets<Image>>()
        .get(&handle)
        .unwrap()
        .size();
    assert_eq!(size, UVec2::splat(32));
    bind_row(
        &mut world,
        content,
        &data["list"],
        &rows(&tree.nodes["list"])[1],
    );
    let missing = world.get::<RowInstances>(content).unwrap().views["app_icon"]
        .0
        .root;
    assert!(world.get::<ImageNode>(missing).is_none());
    assert_eq!(world.get::<Node>(missing).unwrap().width, px(32));
}
