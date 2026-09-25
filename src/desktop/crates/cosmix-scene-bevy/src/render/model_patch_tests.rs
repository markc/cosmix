use super::*;
use cosmix_shell::runtime::SceneVerb;

#[test]
fn model_patch_template_refusal_preserves_all_accepted_state() {
    let source = r#"---
scene: 1
name: template-patch
citizen: behaviour
model: {"prefix":"before ","rows":[{"id":"one","cells":["row"],"hidden":false}]}
---
```mix
root: {widget: "list", rows: "= $model.rows", row: "item", row_height: 24}
item: {widget: "text", text: "= $model.prefix .. $item.cells[0]", hidden: "= $item.hidden"}
```
"#;
    let mut store = SceneStore::default();
    store
        .request(SceneVerb::Load, source, &Value::Null)
        .unwrap();
    let before = store.scenes["template-patch"].tree.clone();
    let bindings = store.scenes["template-patch"].bindings.clone();
    let authored = cosmix_scene::to_source(&store.scenes["template-patch"].document);
    let revision = store.scenes["template-patch"].revision;
    for (path, value) in [
        (
            "model.rows",
            json!([{"id":"one","cells":["row"],"hidden":"invalid"}]),
        ),
        (
            "model",
            json!({"prefix":"candidate ","rows":[{"id":"one","cells":["row"],"hidden":"invalid"}]}),
        ),
    ] {
        let error = store
            .request(
                SceneVerb::Patch,
                "",
                &json!({
                    "scene":"template-patch", "path":path, "value":value,
                }),
            )
            .unwrap_err();
        assert_eq!(error["error_code"], "scene_template");
        let entry = &store.scenes["template-patch"];
        assert_eq!(entry.tree, before);
        assert_eq!(entry.bindings, bindings);
        assert_eq!(cosmix_scene::to_source(&entry.document), authored);
        assert_eq!(entry.revision, revision);
        assert_eq!(store.revisions["template-patch"], revision);
    }
    store
        .request(
            SceneVerb::Patch,
            "",
            &json!({
                "scene":"template-patch", "path":"model.prefix", "value":"after ",
            }),
        )
        .unwrap();
    let entry = &store.scenes["template-patch"];
    assert_eq!(entry.revision, revision + 1);
    assert_eq!(
        prepare_lists(&entry.tree).unwrap()["root"].instances["one"]["item"].ports["text"],
        "after row"
    );
}

#[test]
fn model_patch_cannot_exceed_the_shared_template_node_budget() {
    let labels: Vec<_> = (0..16).map(|i| format!("label{i}")).collect();
    let mut body = format!(
        "root: {{widget: \"column\", children: [\"a\",\"b\"]}}\na: {{widget: \"list\", row: \"entry\", row_height: 20, rows: \"= $model.rows\"}}\nb: {{widget: \"list\", row: \"entry\", row_height: 20, rows: \"= $model.rows\"}}\nentry: {{widget: \"row\", children: {}}}\n",
        json!(labels)
    );
    for id in labels {
        body.push_str(&format!("{id}: {{widget: \"text\", text: \"literal\"}}\n"));
    }
    let source = format!(
        "---\nscene: 1\nname: budget-patch\ncitizen: behaviour\nmodel: {{\"rows\":[]}}\n---\n```mix\n{body}```\n"
    );
    let mut store = SceneStore::default();
    store
        .request(SceneVerb::Load, &source, &Value::Null)
        .unwrap();
    let before = store.scenes["budget-patch"].tree.clone();
    let revision = store.scenes["budget-patch"].revision;
    let rows: Vec<_> = (0..500)
        .map(|i| json!({"id":i.to_string(), "cells":[]}))
        .collect();
    for (path, value) in [("model.rows", json!(rows)), ("model", json!({"rows":rows}))] {
        let error = store
            .request(
                SceneVerb::Patch,
                "",
                &json!({
                    "scene":"budget-patch", "path":path, "value":value,
                }),
            )
            .unwrap_err();
        assert_eq!(error["error_code"], "scene_template");
        assert_eq!(store.scenes["budget-patch"].tree, before);
        assert_eq!(store.scenes["budget-patch"].revision, revision);
        assert_eq!(store.revisions["budget-patch"], revision);
    }
}

#[test]
fn model_patch_refreshes_existing_list_model_and_bound_instances() {
    let source = r#"---
scene: 1
name: model-list
citizen: behaviour
model: {"rows":[{"id":"one","cells":["before"]}]}
---
```mix
root: {widget: "list", rows: "= $model.rows", row: "item", row_height: 24}
item: {widget: "text", text: "{cells[0]}"}
```
"#;
    let mut store = SceneStore::default();
    store
        .request(SceneVerb::Load, source, &Value::Null)
        .unwrap();
    let tree = &store.scenes["model-list"].tree;
    let mut world = World::new();
    let mut mounted = Mounted {
        revision: 0,
        tree: ResolvedScene {
            nodes: Default::default(),
            ..tree.clone()
        },
        page: world.spawn_empty().id(),
        edge: scene_edge(tree),
        registered: false,
        nodes: BTreeMap::new(),
    };
    apply(&mut world, &mut mounted, tree);
    let model = ListModel(mounted.nodes["root"].list.as_ref().unwrap().clone());
    let row = world.spawn_empty().id();
    model.bind(&mut world, row, 0);
    assert!(
        world
            .query::<&Text>()
            .iter(&world)
            .any(|text| text.0 == "before")
    );
    store
        .request(
            SceneVerb::Patch,
            "",
            &json!({"scene":"model-list", "path":"model.rows",
        "value":[{"id":"one","cells":["after"]},{"id":"two","cells":["second"]}]}),
        )
        .unwrap();
    apply(&mut world, &mut mounted, &store.scenes["model-list"].tree);
    assert!(Arc::ptr_eq(
        &model.0,
        mounted.nodes["root"].list.as_ref().unwrap()
    ));
    assert_eq!(model.len(), 2);
    model.bind(&mut world, row, 0);
    assert!(
        world
            .query::<&Text>()
            .iter(&world)
            .any(|text| text.0 == "after")
    );
    assert!(
        !world
            .query::<&Text>()
            .iter(&world)
            .any(|text| text.0 == "before")
    );
    store
        .request(
            SceneVerb::Patch,
            "",
            &json!({"scene":"model-list", "path":"model", "value":{"rows":[]}}),
        )
        .unwrap();
    apply(&mut world, &mut mounted, &store.scenes["model-list"].tree);
    assert_eq!(model.len(), 0);
}
