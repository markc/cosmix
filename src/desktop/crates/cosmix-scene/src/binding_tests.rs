use super::*;
use bindings::{compile, reevaluate, template_instantiate};

#[test]
fn horizontal_flow_and_hidden_are_described_and_type_checked() {
    for family in ["row", "column", "text", "image", "list", "field", "button", "toggle", "spacer"] {
        let ports = describe(family).unwrap();
        assert_eq!(ports.iter().filter(|p| p.path == "hidden").count(), 1);
        assert_eq!(ports.iter().find(|p| p.path == "hidden").unwrap().ty, "bool");
    }
    assert!(!describe("window").unwrap().iter().any(|p| p.path == "hidden"));
    let ports = describe("list").unwrap();
    assert_eq!(ports.iter().find(|p| p.path == "flow").unwrap().enum_values,
        Some(vec!["vertical".into(), "horizontal".into()]));
    let source = "root: {widget: \"list\", flow: \"horizontal\", align: \"center\", hidden: false, rows: [], row: \"t\", row_height: 20}\nt: {widget: \"row\", children: []}";
    assert!(resolve(&doc(source)).is_ok());
    assert!(lint(&doc(&source.replace("horizontal", "diagonal"))).iter().any(|d| d.code == "enum-value"));
    assert!(lint(&doc(&source.replace("hidden: false", "hidden: 1"))).iter().any(|d| d.code == "port-type"));
    for rows in ["[{id: \"\", cells: []}]", "[{id: \"x\", cells: []}, {id: \"x\", cells: []}]"] {
        assert!(lint(&doc(&source.replace("rows: []", &format!("rows: {rows}")))).iter().any(|d| d.code == "row-type"));
    }
}

fn doc(body: &str) -> SceneDocument {
    let fence = char::from(96).to_string().repeat(3);
    parse(&format!("---\nscene: 1\nname: test\ncitizen: c\n---\n{fence}mix\n{body}\n{fence}\n")).unwrap()
}

fn bound(source: &str) -> SceneDocument {
    let mut d = doc(r#"root: {widget: "text", text: "placeholder"}"#);
    d.nodes["root"].ports.insert("text".into(), json!(format!("= {source}")));
    d
}

#[test]
fn dotted_node_id_with_binding_does_not_panic() {
    let mut d = doc(r#"root: {widget: "column", children: ["a", "a.b"]}
a: {widget: "text", text: "= $model.label .. ' plain'"}
"a.b": {widget: "text", text: "= $model.label .. ' dotted'"}"#);
    d.model = Some(json!({"label": "first"}));
    assert!(lint(&d).is_empty());
    let tree = resolve(&d).unwrap();
    assert_eq!(tree.nodes["a"].ports["text"], json!("first plain"));
    assert_eq!(tree.nodes["a.b"].ports["text"], json!("first dotted"));
    let set = compile(&d).unwrap();
    let result = reevaluate(&tree, &set, "model.label", &json!("next")).unwrap();
    assert!(result.diagnostics.is_empty());
    assert_eq!(result.tree.nodes["a.b"].ports["text"], json!("next dotted"));
    assert_eq!(result.tree.nodes["a"].ports["text"], json!("next plain"));
    for (id, expected) in [("a", "next plain"), ("a.b", "next dotted")] {
        let node = template_instantiate(id, &tree.nodes[id], &set, &result.tree.model, &json!({})).unwrap();
        assert_eq!(node.ports["text"], json!(expected));
        assert!(!node.ports.contains_key("b.text"));
    }
}

#[test]
fn model_is_converted_once_per_evaluation_pass() {
    let mut d = doc(r#"root: {widget: "column", children: ["a", "b"]}
a: {widget: "text", text: "= $model.label"}
b: {widget: "text", text: "= $model.label"}"#);
    d.model = Some(json!({"label": "first", "unused": ["x".repeat(65536)]}));
    let before = bindings::MODEL_CONVERSION_COUNT.get();
    assert!(lint(&d).is_empty());
    let tree = resolve(&d).unwrap();
    assert_eq!(bindings::MODEL_CONVERSION_COUNT.get(), before + 1);
    let set = compile(&d).unwrap();
    let result = reevaluate(&tree, &set, "model.label", &json!("next")).unwrap();
    assert_eq!(bindings::MODEL_CONVERSION_COUNT.get(), before + 2);
    assert!(result.diagnostics.is_empty());
    assert_eq!(result.evaluated, ["a.text", "b.text"]);
    for id in ["a", "b"] {
        assert_eq!(tree.nodes[id].ports["text"], json!("first"));
        assert_eq!(result.tree.nodes[id].ports["text"], json!("next"));
    }
}

#[test]
fn template_instantiate_sees_live_model() {
    let mut d = doc("root: {widget: \"list\", rows: [], row: \"t\", row_height: 1}\nt: {widget: \"row\", children: [\"a\",\"b\"]}\na: {widget: \"text\", text: \"= $model.total .. $item.cells[0]\"}\nb: {widget: \"text\", text: \"= $model.total\"}");
    d.model = Some(json!({"total":"old"}));
    let set = compile(&d).unwrap();
    let tree = reevaluate(&resolve(&d).unwrap(), &set, "model.total", &json!("new")).unwrap().tree;
    let item = json!({"cells":[" row"]});
    assert_eq!(template_instantiate("a", &tree.nodes["a"], &set, &tree.model, &item).unwrap().ports["text"], json!("new row"));
    assert_eq!(template_instantiate("b", &tree.nodes["b"], &set, &tree.model, &item).unwrap().ports["text"], json!("new"));
}

#[test]
fn interpolated_binding_has_deps() {
    for source in [r#""Hi ${model.user.name}""#, "<<END\nHi ${model.user.name}\nEND"] {
        let mut d = bound(source);
        d.model = Some(json!({"user":{"name":"old"}}));
        let set = compile(&d).unwrap();
        assert_eq!(set.bindings["root.text"].deps, ["model.user.name".into()].into());
        let r = reevaluate(&resolve(&d).unwrap(), &set, "model.user.name", &json!("new")).unwrap();
        assert_eq!(r.evaluated, ["root.text"]);
        assert!(r.tree.nodes["root"].ports["text"].as_str().unwrap().contains("Hi new"));
    }
    let d = bound(r#""${model.missing ?? $model.fallback}""#);
    let set = compile(&d).unwrap();
    assert_eq!(set.bindings["root.text"].deps, ["model.missing".into(), "model.fallback".into()].into());
    let r = reevaluate(&resolve(&d).unwrap(), &set, "model.fallback", &json!("new")).unwrap();
    assert_eq!(r.tree.nodes["root"].ports["text"], json!("new"));
}

#[test]
fn unknown_root_is_policy_error() {
    for source in ["$foo.bar", "$foo[0]", r#""${HOME}""#, r#""${model.x ?? $foo.bar}""#] {
        let d = bound(source);
        let ds = compile(&d).unwrap_err();
        assert!(ds.iter().any(|d| d.code == "binding-policy"), "{source}: {ds:?}");
        assert_eq!(ds[0].line, d.nodes["root"].line);
    }
}

#[test]
fn denied_construct_is_compile_error() {
    for source in ["function ($x) = 1", "sleep(1)", "$(id)", "$model.f()", "$model.x.unknown_method()", "sh 'id'", "send 'x' y", "print('x')"] {
        let ds = compile(&bound(source)).unwrap_err();
        assert!(ds.iter().any(|d| d.code == "binding-policy"), "{source}: {ds:?}");
    }
    for source in ["$model.a = 1", "1; 2", "("] {
        let ds = compile(&bound(source)).unwrap_err();
        assert!(ds.iter().any(|d| d.code == "invalid-binding"), "{source}: {ds:?}");
    }
    for source in [
        format!("{}1{}", "abs(".repeat(260), ")".repeat(260)),
        format!("{}1", "true ? 1 : ".repeat(260)),
    ] {
        assert!(compile(&bound(&source)).unwrap_err().iter().any(|d| d.code == "invalid-binding"));
    }
}

#[test]
fn coalesce_default_works() {
    for source in [r#"$model.subtitle ?? "n/a""#, r#"$model.subtitle ? 1 / 0 : "n/a""#] {
        let d = bound(source);
        assert!(lint(&d).is_empty());
        assert_eq!(resolve(&d).unwrap().nodes["root"].ports["text"], json!("n/a"));
    }
}

#[test]
fn failed_binding_at_load_takes_default() {
    let d = doc(r#"root: {widget: "text", text: "= 1 / 0", size: "= 1 / 0"}"#);
    let ds = lint(&d);
    assert_eq!(ds.iter().filter(|d| d.code == "binding-eval").count(), 2);
    let tree = resolve(&d).unwrap();
    assert_eq!(tree.nodes["root"].ports["size"], json!(13.0));
    assert!(!tree.nodes["root"].ports.contains_key("text"));
}

#[test]
fn nil_removes_port_and_missing_patch_parent_stays_absent() {
    let mut d = bound("$model.x");
    d.model = Some(json!({"x":"old"}));
    let set = compile(&d).unwrap();
    let tree = resolve(&d).unwrap();
    let r = reevaluate(&tree, &set, "model.x", &JsonValue::Null).unwrap();
    assert!(!r.tree.nodes["root"].ports.contains_key("text"));
    assert_eq!(r.changed, [("root.text".into(), JsonValue::Null)]);
    let r = reevaluate(&r.tree, &set, "model.absent.child", &JsonValue::Null).unwrap();
    assert_eq!(r.tree.model, json!({}));
}

#[test]
fn model_path_diagnostic() {
    let d = bound("$model.x");
    let set = compile(&d).unwrap();
    let tree = resolve(&d).unwrap();
    for path in ["x", "model", "model.", "model..x"] {
        assert_eq!(reevaluate(&tree, &set, path, &json!(1)).unwrap_err()[0].code, "model-path");
    }
    let ds = reevaluate(&tree, &set, "model.x", &json!("x".repeat(MAX_DOCUMENT_BYTES))).unwrap_err();
    assert_eq!(ds[0].code, "model-path");
    assert_eq!(ds[0].message, "patch value too large");
    assert_eq!(tree.model, json!({}));
}

#[test]
fn v0_serialisation_unchanged() {
    #[derive(Serialize)]
    struct V0<'a> {
        name: &'a str,
        citizen: &'a str,
        window: &'a Option<JsonValue>,
        subscribe: &'a Option<JsonValue>,
        nodes: &'a IndexMap<String, Node>,
        templates: &'a Vec<String>,
    }
    for source in [include_str!("../tests/fixtures/static.scene.md"), include_str!("../tests/fixtures/conformance.scene.md"), include_str!("../tests/fixtures/clippanel.scene.md")] {
        let tree = resolve(&parse(source).unwrap()).unwrap();
        let v0 = V0 { name: &tree.name, citizen: &tree.citizen, window: &tree.window, subscribe: &tree.subscribe, nodes: &tree.nodes, templates: &tree.templates };
        let old = serde_json::to_string(&v0).unwrap();
        assert_eq!(serde_json::to_string(&tree).unwrap(), old);
        assert_eq!(serde_json::from_str::<ResolvedScene>(&old).unwrap(), tree);
    }
    let tree = resolve(&parse(include_str!("../tests/fixtures/clippanel.scene.md")).unwrap()).unwrap();
    let wire = serde_json::to_value(&tree).unwrap();
    // v0/main marks only list row roots, not their descendants.
    assert_eq!(wire["templates"], json!(["entry_row"]));
    assert_eq!(wire["nodes"]["entry_row"]["is_template"], json!(true));
    for id in ["r_id", "r_bytes", "r_age", "r_prev"] {
        assert_eq!(wire["nodes"][id]["is_template"], json!(false));
    }
}

#[test]
fn nondeterministic_warning_uses_ast_once() {
    for source in ["time () .. ''", r#""${model.x ?? time ()}""#] {
        assert_eq!(lint(&bound(source)).iter().filter(|d| d.code == "binding-nondeterministic").count(), 1);
    }
    assert!(lint(&bound(r#""time(""#)).is_empty());
}

#[test]
fn nested_list_templates_share_reachability() {
    let d = doc("root: {widget: \"list\", rows: [], row: \"outer\", row_height: 1}\nouter: {widget: \"row\", children: [\"nested\"]}\nnested: {widget: \"list\", rows: [], row: \"inner\", row_height: 1}\ninner: {widget: \"row\", children: [\"label\"]}\nlabel: {widget: \"text\", text: \"= $item.cells[0]\"}");
    assert!(lint(&d).is_empty(), "{:?}", lint(&d));
    let set = compile(&d).unwrap();
    assert!(set.bindings["label.text"].reads_item);
    let tree = resolve(&d).unwrap();
    assert_eq!(tree.templates, ["inner", "outer"]);
    for id in ["outer", "inner"] {
        assert!(tree.nodes[id].is_template);
    }
    for id in ["nested", "label"] {
        assert!(!tree.nodes[id].is_template);
    }
}

#[test]
fn load_budget_bounds_total_time() {
    // An already-expired budget is independent of CPU speed and Mix optimisations.
    // Keep the document (and its preparation cache) inside the override's scope.
    bindings::with_evaluation_budget(std::time::Duration::ZERO, || {
        let mut d = bound("'new'");
        for i in 1..MAX_NODES {
            d.nodes.insert(format!("n{i}"), d.nodes["root"].clone());
        }
        let start = std::time::Instant::now();
        let prepared = prepare(&d);
        assert!(start.elapsed() < std::time::Duration::from_secs(3), "{:?}", start.elapsed());
        assert!(prepared.diagnostics.iter().any(|d| d.code == "binding-eval" && d.message.contains("evaluation budget exhausted")));
        assert!(!prepared.values.contains_key(&format!("n{}.text", MAX_NODES - 1)));
        let tree = resolve(&d).unwrap();
        assert!(!tree.nodes[&format!("n{}", MAX_NODES - 1)].ports.contains_key("text"));
    });
}

#[test]
fn lint_and_resolve_compile_and_evaluate_once() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SceneDocument>();
    let mut d = bound("$model.x");
    d.model = Some(json!({"x":"first"}));
    let before = (bindings::COMPILE_COUNT.get(), bindings::EVALUATE_COUNT.get());
    assert!(lint(&d).is_empty());
    assert_eq!(resolve(&d).unwrap().nodes["root"].ports["text"], json!("first"));
    assert_eq!((bindings::COMPILE_COUNT.get(), bindings::EVALUATE_COUNT.get()), (before.0 + 1, before.1 + 1));
    d.model = Some(json!({"x":"second"}));
    assert_eq!(resolve(&d).unwrap().nodes["root"].ports["text"], json!("second"));
    assert_eq!((bindings::COMPILE_COUNT.get(), bindings::EVALUATE_COUNT.get()), (before.0 + 2, before.1 + 2));
    d.nodes["root"].ports.insert("text".into(), json!("= 1 / 0"));
    assert!(lint(&d).iter().any(|d| d.code == "binding-eval"));
    assert!(!resolve(&d).unwrap().nodes["root"].ports.contains_key("text"));
}

#[test]
fn reevaluation_budget_keeps_last_good() {
    let mut d = bound("$model.expensive ? 'new' : 'old'");
    for i in 1..MAX_NODES {
        d.nodes.insert(format!("n{i}"), d.nodes["root"].clone());
    }
    let set = compile(&d).unwrap();
    let mut tree = resolve(&d).unwrap();
    // Seed every last-good port, including any initial-load budget excess.
    for node in tree.nodes.values_mut() { node.ports.insert("text".into(), json!("old")); }
    let start = std::time::Instant::now();
    let result = bindings::with_evaluation_budget(std::time::Duration::ZERO, || {
        reevaluate(&tree, &set, "model.expensive", &json!(true)).unwrap()
    });
    assert!(start.elapsed() < std::time::Duration::from_secs(2), "{:?}", start.elapsed());
    assert!(result.diagnostics.iter().any(|d| d.code == "binding-eval" && d.message.contains("evaluation budget exhausted")));
    assert!(result.evaluated.len() < MAX_NODES);
    assert_eq!(result.tree.nodes[&format!("n{}", MAX_NODES - 1)].ports["text"], json!("old"));
}

#[test]
fn if_expression_statement_dependencies_are_walked() {
    let d = bound("(if true then $model.a = $model.b; $model.a else $model.c end)");
    let set = compile(&d).unwrap();
    assert_eq!(set.bindings["root.text"].deps, ["model.a".into(), "model.b".into(), "model.c".into()].into());
}
