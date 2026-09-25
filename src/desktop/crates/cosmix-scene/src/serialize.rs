//! Canonical authored AMP source. Never serialise a resolved tree here: that
//! loses expressions, omitted defaults and template definitions.
use crate::SceneDocument;
use serde_json::{Value, json};

/// Strict Mix string escaping, including fence delimiters and interpolation.
/// JSON alone is insufficient: Mix expands `${...}` and leading `~`.
fn quoted(value: &str) -> String {
    serde_json::to_string(value)
        .expect("string serialization")
        .replace('$', "\\u{24}")
        .replace('~', "\\u{7e}")
        .replace('`', "\\u{60}")
}

fn data(value: &Value) -> String {
    match value {
        Value::Null => "nil".into(),
        Value::String(s) => quoted(s),
        Value::Array(items) => format!("[{}]", items.iter().map(data).collect::<Vec<_>>().join(", ")),
        Value::Object(items) => format!("{{{}}}", items.iter().map(|(k, v)| {
            format!("{}: {}", quoted(k), data(v))
        }).collect::<Vec<_>>().join(", ")),
        _ => value.to_string(),
    }
}

pub fn to_source(document: &SceneDocument) -> String {
    let mut wire = format!("---\nscene: 1\nname: {}\ncitizen: {}\n", document.name, document.citizen);
    // Headers contain JSON, not Mix expressions.
    for (key, value) in [("window", &document.window), ("subscribe", &document.subscribe),
                         ("targets", &document.targets), ("model", &document.model)] {
        if let Some(value) = value { wire.push_str(&format!("{key}: {value}\n")); }
    }
    wire.push_str("---\n```mix\n");
    for (id, node) in &document.nodes {
        let mut ports = serde_json::Map::new();
        ports.insert("widget".into(), json!(node.widget));
        ports.extend(node.ports.iter().map(|(k, v)| (k.clone(), v.clone())));
        wire.push_str(&format!("{}: {}\n", quoted(id), data(&Value::Object(ports))));
    }
    wire.push_str("```\n");
    wire
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn authored_source_round_trip_preserves_expressions_templates_and_strings() {
        let mut doc = crate::parse(include_str!("../tests/fixtures/conformance.scene.md")).unwrap();
        let strings = ["${untrusted}", "~/literal", "```mix\nnot a fence", "雪 🦘", "\\\"\n\r\t\0", "= $model.caption"];
        for value in strings {
            let node = doc.nodes.values_mut().find(|node| node.widget == "text").unwrap();
            node.ports.insert("text".into(), json!(value));
            doc.model = Some(json!({"caption": "hello", "literal": value}));
            let source = to_source(&doc);
            let parsed = crate::parse(&source).unwrap();
            assert_eq!(parsed.name, doc.name);
            assert_eq!(parsed.citizen, doc.citizen);
            assert_eq!(parsed.model, doc.model);
            assert_eq!(parsed.window, doc.window);
            assert_eq!(parsed.subscribe, doc.subscribe);
            assert_eq!(parsed.targets, doc.targets);
            for (id, node) in &doc.nodes {
                assert_eq!(parsed.nodes[id].widget, node.widget);
                assert_eq!(parsed.nodes[id].ports, node.ports);
            }
            assert_eq!(to_source(&parsed), source);
        }
    }
}
