//! The renderer-neutral Mix Scenes P1 compiler.
//!
//! A scene is an AMP envelope whose body contains one strict-data `mix`
//! fence.  This crate deliberately stops at a resolved, closed schema: no
//! Bevy, CTK, event loop, or Bus transport is linked here.

use cosmix_bus::bus::parse_strict;
use cosmix_mix::{value::Value, MixError};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as JsonValue};
use std::collections::{HashMap, HashSet};

pub const MAX_DOCUMENT_BYTES: usize = 256 * 1024;
pub const MAX_NODES: usize = 2_000;
pub const MAX_ROWS: usize = 500;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Severity { Error, Warning }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: String,
    pub line: usize,
    pub message: String,
}

impl Diagnostic {
    fn error(code: impl Into<String>, line: usize, message: impl Into<String>) -> Self {
        Self { severity: Severity::Error, code: code.into(), line, message: message.into() }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SceneDocument {
    pub name: String,
    pub citizen: String,
    pub window: Option<JsonValue>,
    pub subscribe: Option<JsonValue>,
    pub targets: Option<JsonValue>,
    pub model: Option<JsonValue>,
    pub nodes: IndexMap<String, RawNode>,
    source: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RawNode {
    pub widget: String,
    pub ports: IndexMap<String, JsonValue>,
    pub line: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub family: String,
    pub ports: IndexMap<String, JsonValue>,
    pub line: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResolvedScene {
    pub name: String,
    pub window: Option<JsonValue>,
    pub subscribe: Option<JsonValue>,
    pub nodes: IndexMap<String, Node>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Insert { id: String, node: Node },
    Remove { id: String },
    SetPort { id: String, port: String, value: JsonValue },
    Reparent { id: String, parent: Option<String>, index: usize },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PortDescribe {
    pub path: String,
    #[serde(rename = "type")]
    pub ty: String,
    pub mutable: bool,
    pub sensitive: bool,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enum_values: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
}

#[derive(Clone, Copy)]
struct Port { name: &'static str, ty: &'static str, required: bool, default: Option<&'static str> }

const fn p(name: &'static str, ty: &'static str, required: bool, default: Option<&'static str>) -> Port {
    Port { name, ty, required, default }
}

fn schema(family: &str) -> Option<&'static [Port]> {
    static WINDOW: [Port; 5] = [p("kind", "string", true, None), p("edge", "string", false, Some("\"right\"")), p("title", "string", false, None), p("w", "number", false, None), p("h", "number", false, None)];
    static BOX: [Port; 4] = [p("children", "list", true, None), p("gap", "number", false, Some("0")), p("padding", "number", false, Some("0")), p("fill", "bool", false, Some("false"))];
    static ROW: [Port; 10] = [p("children", "list", true, None), p("gap", "number", false, Some("0")), p("padding", "number", false, Some("0")), p("fill", "bool", false, Some("false")), p("align", "string", false, Some("\"start\"")), p("height", "number", false, None), p("radius", "number", false, None), p("background", "string", false, None), p("hover", "string", false, None), p("on_click", "string", false, None)];
    static TEXT: [Port; 9] = [p("text", "string", true, None), p("size", "number", false, Some("13")), p("bold", "bool", false, Some("false")), p("mono", "bool", false, Some("false")), p("color", "string", false, None), p("elide", "bool", false, Some("false")), p("width", "number", false, None), p("fill", "bool", false, Some("false")), p("hidden", "bool", false, Some("false"))];
    static FIELD: [Port; 6] = [p("value", "string", true, None), p("placeholder", "string", false, None), p("width", "number", false, None), p("password", "bool", false, Some("false")), p("on_change", "string", false, None), p("on_submit", "string", false, None)];
    static BUTTON: [Port; 4] = [p("label", "string", true, None), p("tone", "string", false, Some("\"normal\"")), p("width", "number", false, None), p("on_click", "string", false, None)];
    static TOGGLE: [Port; 3] = [p("value", "bool", true, None), p("label", "string", true, None), p("on_change", "string", false, None)];
    static LIST: [Port; 8] = [p("rows", "list", true, None), p("row", "string", true, None), p("row_height", "number", true, None), p("gap", "number", false, Some("0")), p("max_rows", "number", false, None), p("fill", "bool", false, Some("false")), p("hidden_if_empty", "bool", false, Some("false")), p("on_click", "string", false, None)];
    static IMAGE: [Port; 3] = [p("src", "string", true, None), p("w", "number", false, None), p("h", "number", false, None)];
    static SPACER: [Port; 1] = [p("size", "number", false, None)];
    Some(match family { "window" => &WINDOW, "column" => &BOX, "row" => &ROW, "text" => &TEXT, "field" => &FIELD, "button" => &BUTTON, "toggle" => &TOGGLE, "list" => &LIST, "image" => &IMAGE, "spacer" => &SPACER, _ => return None })
}

pub fn describe(family: &str) -> Vec<PortDescribe> {
    schema(family).unwrap_or(&[]).iter().map(|x| PortDescribe { path: x.name.into(), ty: x.ty.into(), mutable: true, sensitive: x.name == "password", description: format!("{} port of {}", x.name, family), enum_values: (x.name == "tone").then(|| vec!["normal".into(), "danger".into(), "primary".into()]), default: x.default.and_then(|v| serde_json::from_str(v).ok()), min: None, max: None }).collect()
}

pub fn parse(source: &str) -> Result<SceneDocument, Vec<Diagnostic>> {
    let mut diagnostics = Vec::new();
    if source.len() > MAX_DOCUMENT_BYTES { diagnostics.push(Diagnostic::error("document-too-large", 1, "scene document exceeds 256 KiB")); return Err(diagnostics); }
    let msg = match parse_strict(source) { Ok(m) => m, Err(e) => { diagnostics.push(Diagnostic::error("envelope", 1, e.to_string())); return Err(diagnostics); } };
    for key in ["scene", "name", "citizen"] { if msg.get(key).is_none() { diagnostics.push(Diagnostic::error("missing-header", 1, format!("missing required header {key}"))); } }
    if msg.get("scene") != Some("1") { diagnostics.push(Diagnostic::error("scene-version", 1, "scene header must be 1")); }
    if let Some(name) = msg.get("name") { if !valid_name(name) { diagnostics.push(Diagnostic::error("invalid-name", 1, "name must match ^[a-z][a-z0-9-]{1,30}$")); } }
    let body_base = source[..source.find(&msg.body).unwrap_or(0)].lines().count();
    let fences: Vec<(usize, usize, String)> = fence_ranges(&msg.body);
    if fences.len() != 1 { diagnostics.push(Diagnostic::error("fence-count", body_base.max(1), "body must contain exactly one ```mix fence")); return Err(diagnostics); }
    let (open, _, interior) = fences[0].clone();
    let duplicate_root = source.lines().filter(|line| line.trim_start().starts_with("root:")).count() > 1;
    if duplicate_root { diagnostics.push(Diagnostic::error("duplicate-id", body_base + open, "duplicate node id root")); return Err(diagnostics); }
    let value = match cosmix_mix::parse_data(&interior) { Ok(v) => v, Err(e) => { diagnostics.push(mix_diagnostic(e, body_base + open)); return Err(diagnostics); } };
    let map = match &value { Value::Map(m) => m, _ => { diagnostics.push(Diagnostic::error("root-type", body_base + open + 1, "fence must contain a map of nodes")); return Err(diagnostics); } };
    let mut nodes: IndexMap<String, RawNode> = IndexMap::new();
    for (id, value) in map.iter() {
        let line = body_base + open + line_in(&interior, id).unwrap_or(1);
        let fields = match value { Value::Map(m) => m, _ => { diagnostics.push(Diagnostic::error("node-type", line, format!("node {id} must be a map"))); continue; } };
        let widget = match fields.get("widget").and_then(as_string) { Some(w) => w.to_string(), None => { diagnostics.push(Diagnostic::error("missing-widget", line, format!("node {id} is missing widget"))); continue; } };
        let ports = fields.iter().filter(|(k, _)| k.as_str() != "widget").map(|(k, v)| (k.clone(), json_value(v))).collect();
        nodes.insert(id.clone(), RawNode { widget, ports, line });
    }
    for (id, count) in duplicate_ids(source) { if count > 1 { diagnostics.push(Diagnostic::error("duplicate-id", body_base + open, format!("duplicate node id {id}"))); } }
    if !diagnostics.is_empty() { return Err(diagnostics); }
    let document = SceneDocument { name: msg.get("name").unwrap_or_default().into(), citizen: msg.get("citizen").unwrap_or_default().into(), window: header_json(msg.get("window"), &mut diagnostics, 1), subscribe: header_json(msg.get("subscribe"), &mut diagnostics, 1), targets: header_json(msg.get("targets"), &mut diagnostics, 1), model: header_json(msg.get("model"), &mut diagnostics, 1), nodes, source: source.into() };
    if !diagnostics.is_empty() { Err(diagnostics) } else { Ok(document) }
}

pub fn lint(doc: &SceneDocument) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    if doc.nodes.len() > MAX_NODES { out.push(Diagnostic::error("node-limit", 1, "scene has more than 2,000 nodes")); }
    let mut parents: HashMap<String, usize> = HashMap::new();
    for (id, node) in &doc.nodes {
        let Some(ports) = schema(&node.widget) else { out.push(Diagnostic::error("unknown-family", node.line, format!("unknown widget family {}", node.widget))); continue; };
        for (key, value) in &node.ports { if !ports.iter().any(|p| p.name == key) { out.push(Diagnostic::error("unknown-port", node.line, format!("unknown port {key} on {id}"))); } else if let Some(port) = ports.iter().find(|p| p.name == key) { if !type_matches(value, port.ty) { out.push(Diagnostic::error("port-type", node.line, format!("port {key} on {id} must be {}", port.ty))); } } }
        for port in ports { if port.required && !node.ports.contains_key(port.name) { out.push(Diagnostic::error("missing-port", node.line, format!("missing required port {} on {id}", port.name))); } }
        if let Some(children) = node.ports.get("children").and_then(|v| v.as_array()) { for (index, child) in children.iter().enumerate() { if let Some(child) = child.as_str() { if !doc.nodes.contains_key(child) { out.push(Diagnostic::error("dangling-child", node.line, format!("{id} refers to missing child {child}"))); } else { *parents.entry(child.into()).or_default() += 1; } } else { out.push(Diagnostic::error("child-type", node.line, format!("child {index} of {id} is not a string"))); } } }
        if node.widget == "list" { if let Some(rows) = node.ports.get("rows").and_then(|v| v.as_array()) { if rows.len() > MAX_ROWS { out.push(Diagnostic::error("row-limit", node.line, "list has more than 500 rows")); } }; if let Some(row) = node.ports.get("row").and_then(|v| v.as_str()) { if let Some(template) = doc.nodes.get(row) { if !["row", "column", "text", "spacer", "image"].contains(&template.widget.as_str()) { out.push(Diagnostic::error("invalid-template", template.line, "list row template has an invalid family")); }; if let Some(cells) = node.ports.get("rows").and_then(|v| v.as_array()).and_then(|v| v.first()).and_then(|v| v.get("cells")).and_then(|v| v.as_array()) { check_template(template, doc, cells.len(), &mut out, &mut HashSet::new()); } } else { out.push(Diagnostic::error("dangling-child", node.line, format!("{id} refers to missing row template {row}"))); } } }
    }
    if !doc.nodes.contains_key("root") { out.push(Diagnostic::error("root", 1, "scene must contain a node named root")); }
    if parents.values().any(|n| *n > 1) { out.push(Diagnostic::error("multiple-parents", 1, "a node has more than one parent")); }
    if has_cycle(doc) { out.push(Diagnostic::error("cycle", 1, "scene child graph contains a cycle")); }
    out
}

pub fn resolve(doc: &SceneDocument) -> Result<ResolvedScene, Vec<Diagnostic>> {
    let errors = lint(doc); if !errors.is_empty() { return Err(errors); }
    let nodes = doc.nodes.iter().map(|(id, raw)| { let ports = schema(&raw.widget).unwrap().iter().filter_map(|p| raw.ports.get(p.name).cloned().or_else(|| p.default.and_then(|v| serde_json::from_str(v).ok())).map(|v| (p.name.into(), v))).collect(); (id.clone(), Node { family: raw.widget.clone(), ports, line: raw.line }) }).collect();
    Ok(ResolvedScene { name: doc.name.clone(), window: doc.window.clone(), subscribe: doc.subscribe.clone(), nodes })
}

pub fn diff(old: &ResolvedScene, new: &ResolvedScene) -> Vec<Op> {
    let mut ops = Vec::new();
    for (id, node) in &old.nodes { if !new.nodes.contains_key(id) { ops.push(Op::Remove { id: id.clone() }); } else { let nn = &new.nodes[id]; for (port, value) in &nn.ports { if node.ports.get(port) != Some(value) { ops.push(Op::SetPort { id: id.clone(), port: port.clone(), value: value.clone() }); } } } }
    for (id, node) in &new.nodes {
        if !old.nodes.contains_key(id) { ops.push(Op::Insert { id: id.clone(), node: node.clone() }); }
        else if parent_of(old, id) != parent_of(new, id) { let (parent, index) = parent_of(new, id).map_or((None, 0), |(p, i)| (Some(p), i)); ops.push(Op::Reparent { id: id.clone(), parent, index }); }
    }
    ops
}

pub fn source(doc: &SceneDocument) -> &str { &doc.source }

fn valid_name(s: &str) -> bool { let b = s.as_bytes(); b.len() >= 2 && b.len() <= 31 && b[0].is_ascii_lowercase() && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-') }
fn as_string(v: &Value) -> Option<&str> { if let Value::String(s) = v { Some(s) } else { None } }
fn json_value(v: &Value) -> JsonValue { match v { Value::Nil => JsonValue::Null, Value::Bool(x) => json!(x), Value::Number(x) => json!(x), Value::String(x) => json!(x), Value::List(xs) => xs.iter().map(json_value).collect(), Value::Map(xs) => xs.iter().map(|(k, v)| (k.clone(), json_value(v))).collect(), _ => JsonValue::Null } }
fn header_json(v: Option<&str>, ds: &mut Vec<Diagnostic>, line: usize) -> Option<JsonValue> { v.map(|s| match serde_json::from_str(s) { Ok(v) => v, Err(_) => { ds.push(Diagnostic::error("header-json", line, "header JSON is invalid")); JsonValue::Null } }) }
fn line_in(text: &str, needle: &str) -> Option<usize> { text.lines().position(|l| l.trim_start().starts_with(&format!("{needle}:"))).map(|x| x + 1) }
fn duplicate_ids(text: &str) -> HashMap<String, usize> { let mut counts = HashMap::new(); for line in text.lines() { let trimmed = line.trim(); if let Some((id, _)) = trimmed.split_once(':') { let id = id.trim().trim_start_matches('{').trim(); if id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') { *counts.entry(id.to_string()).or_default() += 1; } } } counts }
fn type_matches(v: &JsonValue, ty: &str) -> bool { match ty { "string" => v.is_string(), "number" => v.is_number(), "bool" => v.is_boolean(), "list" => v.is_array(), "object" => v.is_object(), _ => true } }
fn parent_of(scene: &ResolvedScene, id: &str) -> Option<(String, usize)> { for (parent, node) in &scene.nodes { if let Some(children) = node.ports.get("children").and_then(|v| v.as_array()) { if let Some(index) = children.iter().position(|v| v.as_str() == Some(id)) { return Some((parent.clone(), index)); } } } None }
fn check_template(node: &RawNode, doc: &SceneDocument, cells: usize, out: &mut Vec<Diagnostic>, seen: &mut HashSet<String>) { if !seen.insert(node.line.to_string()) { return; }; if node.widget == "text" { if let Some(text) = node.ports.get("text").and_then(|v| v.as_str()) { let mut rest = text; while let Some(start) = rest.find("{cells[") { let after = &rest[start + 7..]; if let Some(end) = after.find("]}") { if after[..end].parse::<usize>().map_or(true, |i| i >= cells) { out.push(Diagnostic::error("cell-substitution", node.line, "{cells[i]} refers to a cell that is not present")); } rest = &after[end + 2..]; } else { break; } } } }; if let Some(children) = node.ports.get("children").and_then(|v| v.as_array()) { for child in children.iter().filter_map(|v| v.as_str()).filter_map(|id| doc.nodes.get(id)) { check_template(child, doc, cells, out, seen); } } }
fn mix_diagnostic(e: MixError, open: usize) -> Diagnostic { match e { MixError::StrictDataViolation { construct, line, hint } => Diagnostic::error("strict-data", open + line, format!("{construct}: {hint}")), MixError::ParseError { msg, span } | MixError::LexerError { msg, span } => Diagnostic::error("mix-parse", open + span.line, msg), _ => Diagnostic::error("mix-parse", open + 1, e.to_string()) } }
fn fence_ranges(body: &str) -> Vec<(usize, usize, String)> { let lines: Vec<&str> = body.lines().collect(); let mut result = Vec::new(); let mut i = 0; while i < lines.len() { if lines[i].trim() == "```mix" { let start = i + 1; i += 1; while i < lines.len() && lines[i].trim() != "```" { i += 1; } if i < lines.len() { result.push((start, i, lines[start..i].join("\n"))); } } i += 1; } result }
fn has_cycle(doc: &SceneDocument) -> bool { fn visit(id: &str, doc: &SceneDocument, active: &mut HashSet<String>, done: &mut HashSet<String>) -> bool { if active.contains(id) { return true; }; if done.contains(id) { return false; }; active.insert(id.into()); if let Some(n) = doc.nodes.get(id) { if let Some(cs) = n.ports.get("children").and_then(|v| v.as_array()) { for c in cs.iter().filter_map(|v| v.as_str()) { if visit(c, doc, active, done) { return true; } } } } active.remove(id); done.insert(id.into()); false } visit("root", doc, &mut HashSet::new(), &mut HashSet::new()) }

#[cfg(test)]
mod tests {
    use super::*;
    const FIXTURE: &str = include_str!("../tests/fixtures/clippanel.scene.md");
    #[test] fn fixture_round_trips() { let d = parse(FIXTURE).unwrap(); assert!(lint(&d).is_empty(), "{:?}", lint(&d)); let r = resolve(&d).unwrap(); assert!(diff(&r, &r).is_empty()); assert_eq!(source(&d), FIXTURE); }
    #[test] fn every_lint_class_has_a_diagnostic() { for (body, code) in [("x: {widget: \"bogus\"}", "unknown-family"), ("root: {widget: \"text\", text: \"x\", nope: true}", "unknown-port"), ("root: {widget: \"text\"}", "missing-port"), ("root: {widget: \"column\", children: [\"x\"]}", "dangling-child"), ("root: {widget: \"column\", children: [\"x\"]}\nx: {widget: \"column\", children: [\"root\"]}", "cycle")] { let s = format!("---\nscene: 1\nname: test-scene\ncitizen: c\n---\n```mix\n{body}\n```\n"); let d = parse(&s).unwrap(); assert!(lint(&d).iter().any(|x| x.code == code), "missing {code}"); } let duplicate = "---\nscene: 1\nname: test-scene\ncitizen: c\n---\n```mix\nroot: {widget: \"text\", text: \"a\"}\nroot: {widget: \"text\", text: \"b\"}\n```\n"; assert!(parse(duplicate).unwrap_err().iter().any(|x| x.code == "duplicate-id")); }
    #[test] fn hundred_rows_parse() { let mut rows = String::new(); for i in 0..100 { rows.push_str(&format!("{{id: \"r{i}\", cells: [\"{i}\"]}},")); } let s = format!("---\nscene: 1\nname: rows-test\ncitizen: c\n---\n```mix\nroot: {{widget: \"list\", row: \"tmpl\", row_height: 20, rows: [{rows}]}}\ntmpl: {{widget: \"text\", text: \"{{cells[0]}}\"}}\n```\n"); let start = std::time::Instant::now(); let d = parse(&s).unwrap(); assert!(start.elapsed() < std::time::Duration::from_millis(5)); assert!(lint(&d).is_empty()); }
}
