use super::*;

fn body(flow: &str) -> String {
    format!(
        r#"
root: {{widget: "column", fill: true, children: ["sentinel", "list"]}}
sentinel: {{widget: "text", text: "Measured template rows"}}
list: {{widget: "list", flow: "{flow}", gap: 9, align: "center", row: "entry", row_height: 32, rows: [{{id: "a", cells: ["A"]}}, {{id: "b", cells: ["A much longer label"]}}]}}
entry: {{widget: "row", height: 32, padding: 4, hidden: "= $item.hide ?? false", children: ["label"]}}
label: {{widget: "text", text: "= $item.cells[0]"}}
"#
    )
}

fn instances(harness: &mut Harness) -> BTreeMap<String, (Entity, Entity)> {
    let world = harness.app.world_mut();
    world
        .query::<&RowInstances>()
        .iter(world)
        .map(|row| {
            (
                row.key.clone(),
                (
                    row.views["entry"].0.root,
                    row.views["label"].0.label.unwrap(),
                ),
            )
        })
        .collect()
}

fn rect(harness: &Harness, entity: Entity) -> Rect {
    let world = harness.app.world();
    let node = world.get::<ComputedNode>(entity).unwrap();
    logical_rect(
        node,
        world.get::<UiGlobalTransform>(entity).unwrap(),
        node.border_box(),
    )
}

#[test]
fn horizontal_flow_measures_natural_widths_gap_and_retains_rows() {
    let mut harness = Harness::load(&body("horizontal"));
    let before = instances(&mut harness);
    let a = rect(&harness, before["a"].0);
    let b = rect(&harness, before["b"].0);
    assert!(a.width() > 0.0 && b.width() > a.width());
    close("horizontal gap", b.min.x - a.max.x, 9.0);
    close("horizontal alignment", b.center().y, a.center().y);
    harness.patch(
        "list.rows",
        json!([{"id":"b","cells":["A much longer label"]},{"id":"a","cells":["A"]}]),
    );
    let after = instances(&mut harness);
    assert_eq!(before, after, "reordering keeps template and text entities");
    let a = rect(&harness, after["a"].0);
    let b = rect(&harness, after["b"].0);
    close("reordered gap", a.min.x - b.max.x, 9.0);
    harness.patch(
        "list.rows",
        json!([{"id":"b","cells":["A much longer label"],"hide":true},{"id":"a","cells":["A"]}]),
    );
    let a = rect(&harness, after["a"].0);
    let list = rect(&harness, harness.entity("list"));
    close("hidden row leaves no gap", a.min.x, list.min.x);
}

#[test]
fn virtual_list_real_schedule_retains_content_on_model_rebind_and_reorder() {
    let mut harness = Harness::load(&body("vertical"));
    let before = instances(&mut harness);
    assert_eq!(before.len(), 2, "the real VirtualList must bind both rows");
    harness.patch(
        "list.rows",
        json!([{"id":"b","cells":["Rebound B"]},{"id":"a","cells":["Rebound A"]}]),
    );
    let after = instances(&mut harness);
    assert_eq!(
        before, after,
        "CTK must preserve content, not merely the outer row shell"
    );
    assert_eq!(
        harness.app.world().get::<Text>(after["a"].1).unwrap().0,
        "Rebound A"
    );
    assert!(rect(&harness, after["b"].0).min.y < rect(&harness, after["a"].0).min.y);
    harness.patch(
        "list.rows",
        json!([{"id":"replacement","cells":["Replacement"]}]),
    );
    assert!(harness.app.world().get_entity(before["a"].0).is_err());
    assert!(harness.app.world().get_entity(before["b"].0).is_err());
}

/// `shell.scene.layout` reads the same engine rectangles this harness
/// measures: document nodes and every realised row, keyed by item id.
#[test]
fn scene_layout_reports_engine_rects_for_nodes_and_realised_rows() {
    for flow in ["horizontal", "vertical"] {
        let mut harness = Harness::load(&body(flow));
        let layout = crate::scene_layout(harness.app.world_mut(), "layout", None).unwrap();
        assert_eq!(layout["scene"], "layout");
        assert_eq!(layout["applied_revision"], layout["revision"]);
        for id in ["root", "sentinel", "list"] {
            let expected = rect(&harness, harness.entity(id));
            let got = &layout["nodes"][id];
            let context = format!("{flow} node {id}");
            close(&context, got["x"].as_f64().unwrap() as f32, expected.min.x);
            close(&context, got["y"].as_f64().unwrap() as f32, expected.min.y);
            close(&context, got["w"].as_f64().unwrap() as f32, expected.width());
            close(&context, got["h"].as_f64().unwrap() as f32, expected.height());
            assert_eq!(got["hidden"], false, "{context}");
        }
        let contents: Vec<(String, Entity)> = {
            let world = harness.app.world_mut();
            world
                .query::<(Entity, &RowInstances)>()
                .iter(world)
                .map(|(entity, row)| (row.key.clone(), entity))
                .collect()
        };
        assert_eq!(contents.len(), 2, "{flow}: both rows realised");
        for (key, entity) in contents {
            let expected = rect(&harness, entity);
            let got = &layout["instances"]["list"][key.as_str()];
            let context = format!("{flow} row {key}");
            close(&context, got["x"].as_f64().unwrap() as f32, expected.min.x);
            close(&context, got["y"].as_f64().unwrap() as f32, expected.min.y);
            close(&context, got["w"].as_f64().unwrap() as f32, expected.width());
            close(&context, got["h"].as_f64().unwrap() as f32, expected.height());
            assert!(expected.width() > 0.0 && expected.height() > 0.0, "{context}");
        }
        let narrowed = crate::scene_layout(harness.app.world_mut(), "layout", Some("list")).unwrap();
        assert_eq!(
            narrowed["nodes"].as_object().unwrap().keys().collect::<Vec<_>>(),
            ["list"]
        );
        assert_eq!(narrowed["instances"]["list"].as_object().unwrap().len(), 2);
        for (scene, node) in [("layout", Some("nope")), ("absent", None)] {
            let error = crate::scene_layout(harness.app.world_mut(), scene, node).unwrap_err();
            assert_eq!(error["error_code"], "NOT_FOUND");
        }
    }
}

#[test]
fn layout_hidden_follows_display_none_up_to_the_scene_page() {
    let mut world = World::new();
    let page = world.spawn(Node::default()).id();
    let folded = world
        .spawn(Node {
            display: Display::None,
            ..default()
        })
        .id();
    let inside = world.spawn(Node::default()).id();
    let shown = world.spawn(Node::default()).id();
    world.entity_mut(page).add_children(&[folded, shown]);
    world.entity_mut(folded).add_child(inside);
    assert!(hidden_below(&world, inside, Some(page)));
    assert!(hidden_below(&world, folded, Some(page)));
    assert!(!hidden_below(&world, shown, Some(page)));
    // Beyond the page is the host's business (an unmapped dialog), not a
    // node's own visibility.
    let host = world
        .spawn(Node {
            display: Display::None,
            ..default()
        })
        .id();
    world.entity_mut(host).add_child(page);
    assert!(!hidden_below(&world, shown, Some(page)));
}
