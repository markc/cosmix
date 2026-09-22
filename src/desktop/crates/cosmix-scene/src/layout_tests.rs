use super::*;
use serde_json::json;

fn document(body: &str) -> SceneDocument {
    parse(&format!(
        "---\nscene: 1\nname: layout\ncitizen: test\n---\n```mix\n{body}\n```\n"
    ))
    .unwrap()
}

#[test]
fn describe_advertises_exact_layout_vocabulary_without_new_defaults() {
    let common = [
        "align_self",
        "grow",
        "shrink",
        "basis",
        "min_width",
        "max_width",
        "min_height",
        "max_height",
    ];
    let containers = [
        "justify",
        "row_gap",
        "column_gap",
        "padding_top",
        "padding_right",
        "padding_bottom",
        "padding_left",
    ];
    for family in [
        "row", "column", "text", "field", "button", "toggle", "list", "image", "spacer",
    ] {
        let ports = describe(family).unwrap();
        for port in common {
            let description = ports.iter().find(|p| p.path == port).unwrap();
            assert!(
                description.default.is_none(),
                "new ports must not alter canonical legacy scenes"
            );
        }
        for port in containers {
            assert_eq!(
                ports.iter().any(|p| p.path == port),
                matches!(family, "row" | "column")
            );
        }
        assert_eq!(
            ports
                .iter()
                .find(|p| p.path == "align_self")
                .unwrap()
                .enum_values,
            Some(
                ["auto", "start", "center", "end", "stretch"]
                    .map(str::to_owned)
                    .to_vec()
            )
        );
    }
    let row = describe("row").unwrap();
    assert_eq!(
        row.iter()
            .find(|p| p.path == "justify")
            .unwrap()
            .enum_values,
        Some(
            ["start", "center", "end", "between", "around", "evenly"]
                .map(str::to_owned)
                .to_vec()
        )
    );
    assert!(
        !describe("window")
            .unwrap()
            .iter()
            .any(|p| common.contains(&p.path.as_str()))
    );
}

#[test]
fn legacy_and_explicit_conflicts_fail_loudly() {
    for body in [
        r#"root: {widget: "text", text: "x", fill: true, grow: 1}"#,
        r#"root: {widget: "text", text: "x", fill: false, shrink: 0}"#,
        r#"root: {widget: "text", text: "x", fill: true, basis: 40}"#,
        r#"root: {widget: "row", children: [], height: 40, grow: 1}"#,
        r#"root: {widget: "spacer", size: 40, basis: 20}"#,
        r#"root: {widget: "spacer", size: 40, shrink: 1}"#,
        r#"root: {widget: "text", text: "x", min_width: 80, max_width: 40}"#,
        r#"root: {widget: "text", text: "x", min_height: 80, max_height: 40}"#,
        "root: {widget: \"list\", rows: [], row: \"t\", row_height: 20, max_rows: 2, max_height: 80}\nt: {widget: \"row\", children: []}",
    ] {
        let doc = document(body);
        assert!(
            lint(&doc).iter().any(|d| d.code == "layout-conflict"),
            "{body}"
        );
        assert!(resolve(&doc).is_err(), "{body}");
    }
    // Existing legacy combinations retain their old meaning, including the
    // renderer's fixed-height-row exception to fill.
    for body in [
        r#"root: {widget: "row", children: [], fill: true, height: 40}"#,
        r#"root: {widget: "text", text: "x", fill: true, align_self: "end"}"#,
        r#"root: {widget: "row", children: [], padding: 4, padding_left: 8, gap: 4, column_gap: 12}"#,
    ] {
        resolve(&document(body)).unwrap();
    }
}

#[test]
fn new_ports_reject_bad_enums_types_and_negative_values() {
    for (ports, code) in [
        ("justify: \"space-between\"", "enum-value"),
        ("align_self: \"baseline\"", "enum-value"),
        ("grow: -1", "port-min"),
        ("shrink: -1", "port-min"),
        ("basis: \"auto\"", "port-type"),
        ("min_width: -1", "port-min"),
        ("max_height: -1", "port-min"),
        ("row_gap: -1", "port-min"),
        ("padding_left: -1", "port-min"),
        ("justify_content: \"center\"", "unknown-port"),
    ] {
        let doc = document(&format!("root: {{widget: \"row\", children: [], {ports}}}"));
        assert!(lint(&doc).iter().any(|d| d.code == code), "{ports}");
    }
}

#[test]
fn model_bound_constraints_are_checked_transactionally() {
    let mut doc =
        document(r#"root: {widget: "text", text: "x", min_width: "= $model.low", max_width: 80}"#);
    doc.model = Some(json!({"low": 40}));
    let tree = resolve(&doc).unwrap();
    let bindings = bindings::compile(&doc).unwrap();
    let errors = bindings::reevaluate(&tree, &bindings, "model.low", &json!(100)).unwrap_err();
    assert!(errors.iter().any(|d| d.code == "layout-conflict"));
    assert_eq!(tree.nodes["root"].ports["min_width"], json!(40.0));
    doc.model = Some(json!({"low": 100}));
    assert!(
        resolve(&doc)
            .unwrap_err()
            .iter()
            .any(|d| d.code == "layout-conflict")
    );
}

#[test]
fn template_bound_constraints_are_checked_after_item_evaluation() {
    let doc = document(
        r#"
root: {widget: "list", rows: [{id: "one", cells: ["x"]}], row: "label", row_height: 24}
label: {widget: "text", text: "x", min_width: "= $item.minimum", max_width: 80}
"#,
    );
    let tree = resolve(&doc).unwrap();
    let bindings = bindings::compile(&doc).unwrap();
    let error = bindings::template_instantiate(
        "label",
        &tree.nodes["label"],
        &bindings,
        &json!({}),
        &json!({"minimum":100}),
    )
    .unwrap_err();
    assert_eq!(error.code, "layout-conflict");
}
