use super::*;
use cosmix_shell::runtime::SceneVerb;

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
    store.request(SceneVerb::Load, source, &Value::Null).unwrap();
    let tree = &store.scenes["model-list"].tree;
    let mut world = World::new();
    let mut mounted = Mounted {
        revision: 0,
        tree: ResolvedScene { nodes: Default::default(), ..tree.clone() },
        page: world.spawn_empty().id(),
        edge: scene_edge(tree),
        registered: false,
        nodes: BTreeMap::new(),
    };
    apply(&mut world, &mut mounted, tree);
    let model = ListModel(mounted.nodes["root"].list.as_ref().unwrap().clone());
    let row = world.spawn_empty().id();
    model.bind(&mut world, row, 0);
    assert!(world.query::<&Text>().iter(&world).any(|text| text.0 == "before"));
    store.request(SceneVerb::Patch, "", &json!({"scene":"model-list", "path":"model.rows",
        "value":[{"id":"one","cells":["after"]},{"id":"two","cells":["second"]}]})).unwrap();
    apply(&mut world, &mut mounted, &store.scenes["model-list"].tree);
    assert!(Arc::ptr_eq(&model.0, mounted.nodes["root"].list.as_ref().unwrap()));
    assert_eq!(model.len(), 2);
    model.bind(&mut world, row, 0);
    assert!(world.query::<&Text>().iter(&world).any(|text| text.0 == "after"));
    assert!(!world.query::<&Text>().iter(&world).any(|text| text.0 == "before"));
    store.request(SceneVerb::Patch, "", &json!({"scene":"model-list", "path":"model", "value":{"rows":[]}})).unwrap();
    apply(&mut world, &mut mounted, &store.scenes["model-list"].tree);
    assert_eq!(model.len(), 0);
}
