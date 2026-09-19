//! Renderer-neutral Mix Scenes v0 parser, registry, resolver and diff.
//!
//! Numeric ports are normalised to JSON `f64` values, so the wire form of
//! `13` is `13.0`; P2 and `scene.get` consumers must accept that form.
//! Incremental application order is Remove, Insert, SetPort, then Reparent.
//! A removed port is represented by `SetPort { value: null }` (clear).

#![allow(clippy::collapsible_if)]

use cosmix_bus::bus::parse_strict;
use cosmix_mix::{MixError, value::Value};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

pub mod bindings;
#[cfg(test)]
mod binding_tests;

pub const MAX_DOCUMENT_BYTES: usize = 256 * 1024;
pub const MAX_NODES: usize = 2_000;
pub const MAX_ROWS: usize = 500;
pub const ALL_CODES: &[&str] = &[
    "document-too-large",
    "envelope",
    "missing-header",
    "scene-version",
    "invalid-name",
    "fence-count",
    "mix-parse",
    "strict-data",
    "duplicate-id",
    "root-type",
    "node-type",
    "missing-widget",
    "invalid-id",
    "header-json",
    "node-limit",
    "unknown-family",
    "unknown-port",
    "port-type",
    "enum-value",
    "port-min",
    "missing-port",
    "dangling-child",
    "child-type",
    "window-kind",
    "row-limit",
    "row-type",
    "invalid-template",
    "cell-substitution",
    "missing-root",
    "multiple-parents",
    "window-disagreement",
    "orphan-node",
    "cycle",
    "invalid-binding",
    "binding-policy",
    "binding-not-allowed",
    "binding-nondeterministic",
    "binding-eval",
    "binding-type",
    "model-path",
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Severity {
    Error,
    Warning,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: String,
    pub line: usize,
    pub message: String,
}
impl Diagnostic {
    fn error(c: impl Into<String>, l: usize, m: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            code: c.into(),
            line: l,
            message: m.into(),
        }
    }
    fn warning(c: impl Into<String>, l: usize, m: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            code: c.into(),
            line: l,
            message: m.into(),
        }
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
    preparation: PreparationCache,
}

// Cache is not document identity. Public fields remain editable; prepare
// compares every input it uses before reusing the compilation/evaluation.
#[derive(Debug, Default)]
struct PreparationCache(std::sync::Mutex<Option<CachedPreparation>>);
impl Clone for PreparationCache {
    fn clone(&self) -> Self {
        Self(std::sync::Mutex::new(self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()))
    }
}
impl PartialEq for PreparationCache {
    fn eq(&self, _other: &Self) -> bool { true }
}
#[derive(Clone, Debug)]
struct CachedPreparation {
    nodes: IndexMap<String, RawNode>,
    model: Option<JsonValue>,
    window: Option<JsonValue>,
    prepared: PreparedBindings,
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
    pub is_template: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResolvedScene {
    pub name: String,
    pub citizen: String,
    pub window: Option<JsonValue>,
    pub subscribe: Option<JsonValue>,
    pub nodes: IndexMap<String, Node>,
    pub templates: Vec<String>,
    #[serde(default = "empty_model", skip_serializing_if = "is_empty_model")]
    pub model: JsonValue,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bindings: BTreeMap<String, String>,
}
fn empty_model() -> JsonValue { json!({}) }
fn is_empty_model(value: &JsonValue) -> bool { value.as_object().is_some_and(|m| m.is_empty()) }

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Op {
    Insert {
        id: String,
        parent: Option<String>,
        index: usize,
        node: Node,
    },
    Remove {
        id: String,
    },
    SetPort {
        id: String,
        port: String,
        value: JsonValue,
    },
    Reparent {
        id: String,
        parent: Option<String>,
        index: usize,
    },
    SetScene {
        field: String,
        value: JsonValue,
    },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PortDescribe {
    pub path: String,
    #[serde(rename = "type")]
    pub ty: String,
    pub mutable: bool,
    pub sensitive: bool,
    pub description: String,
    #[serde(rename = "enum", skip_serializing_if = "Option::is_none")]
    pub enum_values: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
}
#[derive(Clone, Copy)]
struct Port {
    name: &'static str,
    ty: &'static str,
    required: bool,
    default: Option<&'static str>,
    enum_values: &'static [&'static str],
    min: Option<f64>,
}
const fn p(n: &'static str, t: &'static str, r: bool, d: Option<&'static str>) -> Port {
    Port {
        name: n,
        ty: t,
        required: r,
        default: d,
        enum_values: &[],
        min: None,
    }
}
const fn pe(n: &'static str, d: &'static str, e: &'static [&'static str]) -> Port {
    Port {
        name: n,
        ty: "string",
        required: false,
        default: Some(d),
        enum_values: e,
        min: None,
    }
}
const fn pnd(n: &'static str, d: &'static str, m: Option<f64>) -> Port {
    Port {
        name: n,
        ty: "number",
        required: false,
        default: Some(d),
        enum_values: &[],
        min: Some(match m {
            Some(value) => value,
            None => 0.0,
        }),
    }
}
const fn pn(n: &'static str, r: bool, m: Option<f64>) -> Port {
    Port {
        name: n,
        ty: "number",
        required: r,
        default: None,
        enum_values: &[],
        min: Some(match m {
            Some(value) => value,
            None => 0.0,
        }),
    }
}
fn schema(f: &str) -> Option<&'static [Port]> {
    static EDGE: &[&str] = &["right", "left", "top", "bottom"];
    static ALIGN: &[&str] = &["start", "center", "end", "stretch"];
    static TONE: &[&str] = &["normal", "danger", "primary"];
    static KIND: &[&str] = &["edge"];
    static WINDOW: [Port; 5] = [
        Port {
            name: "kind",
            ty: "string",
            required: true,
            default: None,
            enum_values: KIND,
            min: None,
        },
        pe("edge", "\"right\"", EDGE),
        p("title", "string", false, None),
        pn("w", false, None),
        pn("h", false, None),
    ];
    static BOX: [Port; 4] = [
        p("children", "list", true, None),
        pnd("gap", "0", None),
        pnd("padding", "0", None),
        p("fill", "bool", false, Some("false")),
    ];
    static ROW: [Port; 10] = [
        p("children", "list", true, None),
        pnd("gap", "0", None),
        pnd("padding", "0", None),
        p("fill", "bool", false, Some("false")),
        Port {
            name: "align",
            ty: "string",
            required: false,
            default: Some("\"start\""),
            enum_values: ALIGN,
            min: None,
        },
        pn("height", false, None),
        pn("radius", false, None),
        p("background", "string", false, None),
        p("hover", "string", false, None),
        p("on_click", "string", false, None),
    ];
    static TEXT: [Port; 9] = [
        p("text", "string", true, None),
        pnd("size", "13", Some(0.0)),
        p("bold", "bool", false, Some("false")),
        p("mono", "bool", false, Some("false")),
        p("color", "string", false, None),
        p("elide", "bool", false, Some("false")),
        pn("width", false, None),
        p("fill", "bool", false, Some("false")),
        p("hidden", "bool", false, Some("false")),
    ];
    static FIELD: [Port; 6] = [
        p("value", "string", true, None),
        p("placeholder", "string", false, None),
        pn("width", false, None),
        p("password", "bool", false, Some("false")),
        p("on_change", "string", false, None),
        p("on_submit", "string", false, None),
    ];
    static BUTTON: [Port; 4] = [
        p("label", "string", true, None),
        Port {
            name: "tone",
            ty: "string",
            required: false,
            default: Some("\"normal\""),
            enum_values: TONE,
            min: None,
        },
        pn("width", false, Some(0.0)),
        p("on_click", "string", false, None),
    ];
    static TOGGLE: [Port; 3] = [
        p("value", "bool", true, None),
        p("label", "string", true, None),
        p("on_change", "string", false, None),
    ];
    static LIST: [Port; 8] = [
        p("rows", "list", true, None),
        p("row", "string", true, None),
        pn("row_height", true, Some(0.0)),
        pnd("gap", "0", None),
        pn("max_rows", false, Some(1.0)),
        p("fill", "bool", false, Some("false")),
        p("hidden_if_empty", "bool", false, Some("false")),
        p("on_click", "string", false, None),
    ];
    static IMAGE: [Port; 3] = [
        p("src", "string", true, None),
        pn("w", false, Some(0.0)),
        pn("h", false, Some(0.0)),
    ];
    static SPACER: [Port; 1] = [pn("size", false, None)];
    Some(match f {
        "window" => &WINDOW,
        "column" => &BOX,
        "row" => &ROW,
        "text" => &TEXT,
        "field" => &FIELD,
        "button" => &BUTTON,
        "toggle" => &TOGGLE,
        "list" => &LIST,
        "image" => &IMAGE,
        "spacer" => &SPACER,
        _ => return None,
    })
}
pub fn describe(f: &str) -> Option<Vec<PortDescribe>> {
    schema(f).map(|ps| {
        ps.iter()
            .map(|p| PortDescribe {
                path: p.name.into(),
                ty: p.ty.into(),
                mutable: true,
                sensitive: p.name == "value" && f == "field",
                description: format!("{} port of {}", p.name, f),
                enum_values: (!p.enum_values.is_empty())
                    .then(|| p.enum_values.iter().map(|x| (*x).into()).collect()),
                default: p
                    .default
                    .and_then(|x| serde_json::from_str(x).ok())
                    .map(normalize_number),
                min: p.min,
                max: None,
            })
            .collect()
    })
}

pub(crate) fn port_for(family: &str, name: &str) -> Option<Port> {
    schema(family)?.iter().find(|p| p.name == name).copied()
}

pub(crate) fn check_port_value(id: &str, line: usize, p: Port, v: &JsonValue, out: &mut Vec<Diagnostic>) {
    let k = p.name;
    if !type_matches(v, p.ty) {
        out.push(Diagnostic::error("port-type", line, format!("port {k} on {id} must be {}", p.ty)));
    }
    if !p.enum_values.is_empty() && v.as_str().is_some_and(|x| !p.enum_values.contains(&x)) {
        out.push(Diagnostic::error("enum-value", line, format!("invalid value for {k} on {id}")));
    }
    if let Some(min) = p.min {
        let exclusive = k == "row_height";
        if v.as_f64().is_some_and(|x| if exclusive { x <= min } else { x < min }) {
            out.push(Diagnostic::error("port-min", line, format!("port {k} on {id} must be {} {min}", if exclusive { ">" } else { ">=" })));
        }
    }
    if k == "rows" {
        validate_rows(v, line, out);
    }
}

pub(crate) fn normalize_number(v: JsonValue) -> JsonValue { normalize_number_inner(v) }

fn normalize_number_inner(v: JsonValue) -> JsonValue {
    if let Some(n) = v.as_f64() {
        serde_json::Number::from_f64(n).map(JsonValue::Number).unwrap_or(JsonValue::Null)
    } else if let Some(a) = v.as_array() { JsonValue::Array(a.iter().cloned().map(normalize_number_inner).collect())
    } else if let Some(o) = v.as_object() { JsonValue::Object(o.iter().map(|(k, v)| (k.clone(), normalize_number_inner(v.clone()))).collect())
    } else { v }
}

pub(crate) fn port_changes(old: &ResolvedScene, new: &ResolvedScene) -> Vec<(String, JsonValue)> {
    let mut out = Vec::new();
    for (id, node) in &new.nodes {
        if let Some(previous) = old.nodes.get(id) {
            for (port, value) in &node.ports {
                if previous.ports.get(port) != Some(value) { out.push((format!("{id}.{port}"), value.clone())); }
            }
            for port in previous.ports.keys() {
                if !node.ports.contains_key(port) { out.push((format!("{id}.{port}"), JsonValue::Null)); }
            }
        }
    }
    out
}
pub fn parse(source: &str) -> Result<SceneDocument, Vec<Diagnostic>> {
    let mut ds = Vec::new();
    if source.len() > MAX_DOCUMENT_BYTES {
        return Err(vec![Diagnostic::error(
            "document-too-large",
            1,
            "scene document exceeds 256 KiB",
        )]);
    }
    let msg = match parse_strict(source) {
        Ok(m) => m,
        Err(e) => return Err(vec![Diagnostic::error("envelope", 1, e.to_string())]),
    };
    for k in ["scene", "name", "citizen"] {
        if msg.get(k).is_none() {
            ds.push(Diagnostic::error(
                "missing-header",
                1,
                format!("missing required header {k}"),
            ));
        }
    }
    if msg.get("scene") != Some("1") {
        ds.push(Diagnostic::error(
            "scene-version",
            1,
            "scene header must be 1",
        ));
    }
    if let Some(n) = msg.get("name") {
        if !valid_name(n) {
            ds.push(Diagnostic::error("invalid-name", 1, "invalid scene name"));
        }
    }
    let bs = source
        .find("\n---\n")
        .map(|i| i + 5)
        .unwrap_or(source.len());
    let body = msg.body.clone();
    let base = source[..bs].lines().count();
    let fs = fence_ranges(&body);
    if fs.len() != 1 {
        let line = fs.get(1).map_or(base + 1, |(open, _, _)| base + open);
        ds.push(Diagnostic::error(
            "fence-count",
            line,
            "body must contain exactly one ```mix fence",
        ));
        return Err(ds);
    }
    let (open, _, interior) = fs[0].clone();
    let value = match cosmix_mix::parse_data(&interior) {
        Ok(v) => v,
        Err(e) => {
            ds.push(mix_diagnostic(e, base + open));
            return Err(ds);
        }
    };
    let map = match &value {
        Value::Map(m) => m,
        _ => {
            ds.push(Diagnostic::error(
                "root-type",
                base + open + 1,
                "fence must contain a map of nodes",
            ));
            return Err(ds);
        }
    };
    let mut nodes = IndexMap::new();
    for (id, v) in map.iter() {
        let line = base + open + line_in(&interior, id).unwrap_or(1);
        if id.contains('@') {
            ds.push(Diagnostic::error(
                "invalid-id",
                line,
                "@ is reserved for template instance ids",
            ));
        }
        let fields = match v {
            Value::Map(m) => m,
            _ => {
                ds.push(Diagnostic::error(
                    "node-type",
                    line,
                    format!("node {id} must be a map"),
                ));
                continue;
            }
        };
        let widget = match fields.get("widget").and_then(as_string) {
            Some(x) => x.to_string(),
            None => {
                ds.push(Diagnostic::error(
                    "missing-widget",
                    line,
                    format!("node {id} is missing widget"),
                ));
                continue;
            }
        };
        nodes.insert(
            id.clone(),
            RawNode {
                widget,
                ports: fields
                    .iter()
                    .filter(|(k, _)| k.as_str() != "widget")
                    .map(|(k, v)| (k.clone(), json_value(v)))
                    .collect(),
                line,
            },
        );
    }
    if !ds.is_empty() {
        return Err(sorted(ds));
    }
    let document = SceneDocument {
        name: msg.get("name").unwrap_or_default().into(),
        citizen: msg.get("citizen").unwrap_or_default().into(),
        window: header_json(msg.get("window"), &mut ds, 1),
        subscribe: header_json(msg.get("subscribe"), &mut ds, 1),
        targets: header_json(msg.get("targets"), &mut ds, 1),
        model: header_json(msg.get("model"), &mut ds, 1),
        nodes,
        source: source.into(),
        preparation: PreparationCache::default(),
    };
    if ds.is_empty() {
        Ok(document)
    } else {
        Err(sorted(ds))
    }
}
pub fn lint(doc: &SceneDocument) -> Vec<Diagnostic> {
    prepare(doc).diagnostics
}

// One compilation and one evaluation pass, shared by lint and resolve.
#[derive(Clone, Debug)]
struct PreparedBindings {
    set: bindings::BindingSet,
    values: BTreeMap<String, Option<JsonValue>>,
    diagnostics: Vec<Diagnostic>,
}

fn prepare(doc: &SceneDocument) -> PreparedBindings {
    let mut cache = doc.preparation.0.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(cached) = cache.as_ref()
        && cached.nodes == doc.nodes && cached.model == doc.model && cached.window == doc.window {
        return cached.prepared.clone();
    }
    let mut prepared = PreparedBindings {
        set: bindings::BindingSet::default(),
        values: BTreeMap::new(),
        diagnostics: Vec::new(),
    };
    match bindings::compile(doc) {
        Ok(set) => {
            prepared.diagnostics.extend(set.diagnostics.clone());
            prepared.set = set;
        }
        Err(ds) => prepared.diagnostics.extend(ds),
    }
    let mut out = lint_structure(doc);
    out.append(&mut prepared.diagnostics);
    if !out.iter().any(|d| d.severity == Severity::Error) {
        let model = doc.model.clone().unwrap_or_else(empty_model);
        let started = std::time::Instant::now();
        for path in &prepared.set.order {
            let binding = &prepared.set.bindings[path];
            if binding.reads_item { continue; }
            let (id, port) = path.split_once('.').unwrap();
            let node = &doc.nodes[id];
            let Some(schema_port) = port_for(&node.widget, port) else { continue };
            match bindings::evaluate_for_resolve(binding, &model, schema_port, started) {
                Ok(value) => { prepared.values.insert(path.clone(), value); }
                Err(message) => out.push(Diagnostic::warning(
                    if message == "binding-type" { "binding-type" } else { "binding-eval" },
                    node.line,
                    format!("binding evaluation failed for {path}: {message}"),
                )),
            }
        }
    }
    prepared.diagnostics = sorted(out);
    *cache = Some(CachedPreparation {
        nodes: doc.nodes.clone(),
        model: doc.model.clone(),
        window: doc.window.clone(),
        prepared: prepared.clone(),
    });
    prepared
}

fn lint_structure(doc: &SceneDocument) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    if doc.source.len() > MAX_DOCUMENT_BYTES {
        out.push(Diagnostic::error(
            "document-too-large",
            1,
            "scene document exceeds 256 KiB",
        ));
    }
    if doc.nodes.len() > MAX_NODES {
        out.push(Diagnostic::error(
            "node-limit",
            1,
            "scene has more than 2,000 nodes",
        ));
    }
    let mut parents: HashMap<String, usize> = HashMap::new();
    for (id, n) in &doc.nodes {
        if id.contains('@') {
            out.push(Diagnostic::error(
                "invalid-id",
                n.line,
                "@ is reserved for template instance ids",
            ));
        }
        let Some(ps) = schema(&n.widget) else {
            out.push(Diagnostic::error(
                "unknown-family",
                n.line,
                format!("unknown widget family {}", n.widget),
            ));
            continue;
        };
        for (k, v) in &n.ports {
            let Some(p) = ps.iter().find(|p| p.name == k) else {
                out.push(Diagnostic::error(
                    "unknown-port",
                    n.line,
                    format!("unknown port {k} on {id}"),
                ));
                continue;
            };
            if bindings::binding_source(v).is_none() {
                check_port_value(id, n.line, *p, v, &mut out);
            }
        }
        for p in ps {
            if p.required && !n.ports.contains_key(p.name) {
                out.push(Diagnostic::error(
                    "missing-port",
                    n.line,
                    format!("missing required port {} on {id}", p.name),
                ));
            }
        }
        if let Some(cs) = n.ports.get("children").and_then(JsonValue::as_array) {
            for (i, c) in cs.iter().enumerate() {
                if let Some(c) = c.as_str() {
                    if !doc.nodes.contains_key(c) {
                        out.push(Diagnostic::error(
                            "dangling-child",
                            n.line,
                            format!("{id} refers to missing child {c}"),
                        ));
                    } else {
                        *parents.entry(c.into()).or_default() += 1;
                    }
                } else {
                    out.push(Diagnostic::error(
                        "child-type",
                        n.line,
                        format!("child {i} of {id} is not a string"),
                    ));
                }
            }
        }
        if n.widget == "window" && n.ports.get("kind").and_then(JsonValue::as_str) != Some("edge") {
            out.push(Diagnostic::error(
                "window-kind",
                n.line,
                "window kind must be edge in v0",
            ));
        }
        if n.widget == "list" {
            if let Some(row) = n.ports.get("row").and_then(JsonValue::as_str) {
                if doc.nodes.values().any(|parent| {
                    parent
                        .ports
                        .get("children")
                        .and_then(JsonValue::as_array)
                        .is_some_and(|cs| cs.iter().any(|c| c.as_str() == Some(row)))
                }) {
                    out.push(Diagnostic::error(
                        "invalid-template",
                        n.line,
                        "list row template must not also be a child",
                    ));
                }
                match doc.nodes.get(row) {
                    Some(t) => {
                        check_template(row, t, doc, minimum_cells(n), &mut out, &mut HashSet::new())
                    }
                    None => out.push(Diagnostic::error(
                        "dangling-child",
                        n.line,
                        format!("missing row template {row}"),
                    )),
                }
            }
        }
    }
    if !doc.nodes.contains_key("root") {
        out.push(Diagnostic::error(
            "missing-root",
            1,
            "scene must contain a node named root",
        ));
    }
    if parents.values().any(|n| *n > 1) {
        out.push(Diagnostic::error(
            "multiple-parents",
            1,
            "a node has more than one parent",
        ));
    }
    if let Some(w) = &doc.window {
        for n in doc.nodes.values().filter(|n| n.widget == "window") {
            if ["kind", "edge", "title", "w", "h"]
                .iter()
                .any(|k| n.ports.get(*k) != w.get(*k))
            {
                out.push(Diagnostic::error(
                    "window-disagreement",
                    n.line,
                    "window envelope and window node disagree",
                ));
            }
        }
    }
    let reach = reachable(doc);
    for id in doc.nodes.keys() {
        if id != "root" && !reach.contains(id) {
            out.push(Diagnostic::warning(
                "orphan-node",
                doc.nodes[id].line,
                format!("node {id} is unreachable from root"),
            ));
        }
    }
    if has_cycle(doc) {
        out.push(Diagnostic::error(
            "cycle",
            1,
            "scene child graph contains a cycle",
        ));
    }
    sorted(out)
}
pub fn resolve(doc: &SceneDocument) -> Result<ResolvedScene, Vec<Diagnostic>> {
    let prepared = prepare(doc);
    if prepared.diagnostics.iter().any(|d| d.severity == Severity::Error) {
        return Err(prepared.diagnostics);
    }
    let ts = bindings::template_ids(doc);
    let binding_set = prepared.set;
    let mut nodes = IndexMap::new();
    for (id, r) in &doc.nodes {
        let Some(ps) = schema(&r.widget) else {
            return Err(vec![Diagnostic::error(
                "unknown-family",
                r.line,
                "unknown widget family",
            )]);
        };
        let mut ports: IndexMap<String, JsonValue> = ps
            .iter()
            .filter_map(|p| {
                r.ports
                    .get(p.name)
                    .filter(|v| bindings::binding_source(v).is_none())
                    .and_then(|v| bindings::escaped_literal(v).or_else(|| Some(v.clone())))
                    .or_else(|| p.default.and_then(|v| serde_json::from_str(v).ok()))
                    .map(|v| (p.name.into(), normalize_number(v)))
            })
            .collect();
        for p in ps {
            if let Some(value) = prepared.values.get(&format!("{id}.{}", p.name)) {
                bindings::set_port(&mut ports, p.name, value.clone());
            }
        }
        nodes.insert(
            id.clone(),
            Node {
                family: r.widget.clone(),
                ports,
                line: r.line,
                is_template: ts.contains(id),
            },
        );
    }
    let mut templates: Vec<_> = ts.into_iter().collect();
    templates.sort();
    Ok(ResolvedScene {
        name: doc.name.clone(),
        citizen: doc.citizen.clone(),
        window: doc.window.clone(),
        subscribe: doc.subscribe.clone(),
        nodes,
        templates,
        model: doc.model.clone().unwrap_or_else(|| json!({})),
        bindings: binding_set.bindings.into_iter().map(|(k, v)| (k, v.source)).collect(),
    })
}
pub fn diff(old: &ResolvedScene, new: &ResolvedScene) -> Vec<Op> {
    let mut ops = Vec::new();
    for id in old.nodes.keys() {
        if !new.nodes.contains_key(id) {
            ops.push(Op::Remove { id: id.clone() });
        }
    }
    for (id, n) in &new.nodes {
        if let Some(o) = old.nodes.get(id) {
            let keys: BTreeSet<_> = o.ports.keys().chain(n.ports.keys()).collect();
            for k in keys {
                let v = n.ports.get(k).cloned().unwrap_or(JsonValue::Null);
                if o.ports.get(k) != Some(&v) {
                    ops.push(Op::SetPort {
                        id: id.clone(),
                        port: k.clone(),
                        value: v,
                    });
                }
            }
        } else {
            let (p, i) = parent_of(new, id).map_or((None, 0), |(p, i)| (Some(p), i));
            ops.push(Op::Insert {
                id: id.clone(),
                parent: p,
                index: i,
                node: n.clone(),
            });
        }
    }
    for id in new.nodes.keys() {
        if old.nodes.contains_key(id) && parent_of(old, id) != parent_of(new, id) {
            let (p, i) = parent_of(new, id).map_or((None, 0), |(p, i)| (Some(p), i));
            ops.push(Op::Reparent {
                id: id.clone(),
                parent: p,
                index: i,
            });
        }
    }
    for (field, a, b) in [
        ("name", json!(old.name), json!(new.name)),
        ("citizen", json!(old.citizen), json!(new.citizen)),
        (
            "window",
            old.window.clone().unwrap_or(JsonValue::Null),
            new.window.clone().unwrap_or(JsonValue::Null),
        ),
        (
            "subscribe",
            old.subscribe.clone().unwrap_or(JsonValue::Null),
            new.subscribe.clone().unwrap_or(JsonValue::Null),
        ),
    ] {
        if a != b {
            ops.push(Op::SetScene {
                field: field.into(),
                value: b,
            });
        }
    }
    ops
}
pub fn source(d: &SceneDocument) -> &str {
    &d.source
}
fn valid_name(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 2
        && b.len() <= 31
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}
fn as_string(v: &Value) -> Option<&str> {
    if let Value::String(s) = v {
        Some(s)
    } else {
        None
    }
}
fn json_value(v: &Value) -> JsonValue {
    match v {
        Value::Nil => JsonValue::Null,
        Value::Bool(x) => json!(x),
        Value::Number(x) => json!(x),
        Value::String(x) => json!(x),
        Value::List(xs) => xs.iter().map(json_value).collect(),
        Value::Map(xs) => xs.iter().map(|(k, v)| (k.clone(), json_value(v))).collect(),
        _ => JsonValue::Null,
    }
}
fn header_json(v: Option<&str>, ds: &mut Vec<Diagnostic>, line: usize) -> Option<JsonValue> {
    v.map(|s| match serde_json::from_str(s) {
        Ok(v) => normalize_number(v),
        Err(_) => {
            ds.push(Diagnostic::error(
                "header-json",
                line,
                "header JSON is invalid",
            ));
            JsonValue::Null
        }
    })
}
fn line_in(t: &str, id: &str) -> Option<usize> {
    t.lines()
        .position(|l| {
            l.trim_start()
                .strip_prefix(id)
                .is_some_and(|rest| rest.trim_start().starts_with(':'))
        })
        .map(|x| x + 1)
}
fn type_matches(v: &JsonValue, t: &str) -> bool {
    match t {
        "string" => v.is_string(),
        "number" => v.is_number(),
        "bool" => v.is_boolean(),
        "list" => v.is_array(),
        "object" => v.is_object(),
        _ => true,
    }
}
fn validate_rows(value: &JsonValue, line: usize, o: &mut Vec<Diagnostic>) {
    if let Some(rs) = value.as_array() {
        if rs.len() > MAX_ROWS {
            o.push(Diagnostic::error(
                "row-limit",
                line,
                "list has more than 500 rows",
            ));
        }
        for r in rs {
            if r.get("id").and_then(JsonValue::as_str).is_none()
                || r.get("cells")
                    .and_then(JsonValue::as_array)
                    .is_none_or(|cs| cs.iter().any(|c| !c.is_string()))
            {
                o.push(Diagnostic::error(
                    "row-type",
                    line,
                    "each row must contain string id and string cells",
                ));
            }
        }
    }
}
fn minimum_cells(n: &RawNode) -> usize {
    n.ports
        .get("rows")
        .and_then(JsonValue::as_array)
        .map(|rs| {
            rs.iter()
                .filter_map(|r| r.get("cells").and_then(JsonValue::as_array).map(Vec::len))
                .min()
                .unwrap_or(usize::MAX)
        })
        .unwrap_or(usize::MAX)
}
fn check_template(
    id: &str,
    n: &RawNode,
    d: &SceneDocument,
    cells: usize,
    o: &mut Vec<Diagnostic>,
    seen: &mut HashSet<String>,
) {
    if !seen.insert(id.to_owned()) {
        return;
    }
    if !["row", "column", "text", "spacer", "image", "list"].contains(&n.widget.as_str()) {
        o.push(Diagnostic::error(
            "invalid-template",
            n.line,
            "list row template has an invalid family",
        ));
        return;
    }
    for (k, v) in &n.ports {
        if let Some(s) = v.as_str() {
            if s.contains("{cells[") && k != "text" {
                o.push(Diagnostic::error(
                    "cell-substitution",
                    n.line,
                    "cell substitution is allowed only in text.text",
                ));
            }
            if k == "text" {
                for (start, _) in s.match_indices("{cells[") {
                    if let Some(e) = s[start + 7..].find("]}") {
                        if s[start + 7..start + 7 + e]
                            .parse::<usize>()
                            .map_or(true, |i| i >= cells)
                        {
                            o.push(Diagnostic::error(
                                "cell-substitution",
                                n.line,
                                "cell index is not present",
                            ));
                        }
                    }
                }
            }
        }
    }
    if let Some(cs) = n.ports.get("children").and_then(JsonValue::as_array) {
        for id in cs.iter().filter_map(JsonValue::as_str) {
            if let Some(c) = d.nodes.get(id) {
                check_template(id, c, d, cells, o, seen);
            }
        }
    }
}
fn fence_ranges(b: &str) -> Vec<(usize, usize, String)> {
    let l: Vec<_> = b.lines().collect();
    let mut r = Vec::new();
    let mut i = 0;
    while i < l.len() {
        if l[i].trim() == "```mix" {
            let s = i + 1;
            i += 1;
            while i < l.len() && l[i].trim() != "```" {
                i += 1;
            }
            if i < l.len() {
                r.push((s, i, l[s..i].join("\n")));
            }
        }
        i += 1;
    }
    r
}
fn parent_of(s: &ResolvedScene, id: &str) -> Option<(String, usize)> {
    for (p, n) in &s.nodes {
        if let Some(cs) = n.ports.get("children").and_then(JsonValue::as_array) {
            if let Some(i) = cs.iter().position(|v| v.as_str() == Some(id)) {
                return Some((p.clone(), i));
            }
        }
    }
    None
}
fn reachable(d: &SceneDocument) -> HashSet<String> {
    let mut s = HashSet::new();
    fn go(id: &str, d: &SceneDocument, s: &mut HashSet<String>) {
        if !s.insert(id.into()) {
            return;
        }
        if let Some(n) = d.nodes.get(id) {
            if let Some(cs) = n.ports.get("children").and_then(JsonValue::as_array) {
                for c in cs.iter().filter_map(JsonValue::as_str) {
                    go(c, d, s);
                }
            }
        }
    }
    if d.nodes.contains_key("root") {
        go("root", d, &mut s);
    }
    s.extend(bindings::template_ids(d));
    s
}
fn has_cycle(d: &SceneDocument) -> bool {
    fn go(
        id: &str,
        d: &SceneDocument,
        a: &mut HashSet<String>,
        done: &mut HashSet<String>,
    ) -> bool {
        if a.contains(id) {
            return true;
        }
        if done.contains(id) {
            return false;
        }
        a.insert(id.into());
        if let Some(n) = d.nodes.get(id) {
            if let Some(cs) = n.ports.get("children").and_then(JsonValue::as_array) {
                for c in cs.iter().filter_map(JsonValue::as_str) {
                    if go(c, d, a, done) {
                        return true;
                    }
                }
            }
        }
        a.remove(id);
        done.insert(id.into());
        false
    }
    let mut a = HashSet::new();
    let mut done = HashSet::new();
    d.nodes.keys().any(|id| go(id, d, &mut a, &mut done))
}
fn mix_diagnostic(e: MixError, o: usize) -> Diagnostic {
    match e {
        MixError::StrictDataViolation {
            construct,
            line,
            hint,
        } => {
            let c = if construct.starts_with("duplicate map key") {
                "duplicate-id"
            } else {
                "strict-data"
            };
            Diagnostic::error(c, o + line, format!("{construct}: {hint}"))
        }
        MixError::ParseError { msg, span }
        | MixError::LexerError { msg, span }
        | MixError::IncompleteInput { msg, span } => {
            Diagnostic::error("mix-parse", o + span.line, msg)
        }
        MixError::AssignmentChainParseError { operator, span } => Diagnostic::error(
            "mix-parse",
            o + span.line,
            format!("assignment chain operator {operator}"),
        ),
        _ => Diagnostic::error("mix-parse", o + 1, e.to_string()),
    }
}
fn sorted(mut d: Vec<Diagnostic>) -> Vec<Diagnostic> {
    d.sort_by(|a, b| a.line.cmp(&b.line).then(a.code.cmp(&b.code)));
    d
}

#[cfg(test)]
mod tests {
    #![allow(clippy::useless_conversion)]
    use super::*;
    const C: &str = include_str!("../tests/fixtures/clippanel.scene.md");
    const F: &str = include_str!("../tests/fixtures/conformance.scene.md");
    const S: &str = include_str!("../tests/fixtures/static.scene.md");
    fn doc(b: &str) -> SceneDocument {
        parse(&format!(
            "---\nscene: 1\nname: test\ncitizen: c\n---\n```mix\n{b}\n```\n"
        ))
        .unwrap()
    }
    #[test]
    fn fixture_round_trips() {
        for s in [C, F, S] {
            let d = parse(s).unwrap();
            assert!(lint(&d).is_empty(), "{:?}", lint(&d));
            let r = resolve(&d).unwrap();
            assert!(diff(&r, &r).is_empty());
        }
    }
    #[test]
    fn resolved_defaults() {
        let n = &resolve(&doc("root: {widget: \"text\", text: \"x\"}"))
            .unwrap()
            .nodes["root"];
        for (k, v) in [
            ("size", json!(13.0)),
            ("bold", json!(false)),
            ("mono", json!(false)),
            ("elide", json!(false)),
            ("fill", json!(false)),
            ("hidden", json!(false)),
        ] {
            assert_eq!(n.ports[k], v);
        }
    }
    #[test]
    fn resolved_defaults_cover_all_families() {
        let d = doc(
            "root: {widget: \"column\", children: [\"win\",\"row\",\"field\",\"button\",\"toggle\",\"list\",\"image\",\"spacer\"]}\nwin: {widget: \"window\", kind: \"edge\"}\nrow: {widget: \"row\", children: [\"text\"]}\ntext: {widget: \"text\", text: \"x\"}\nfield: {widget: \"field\", value: \"x\"}\nbutton: {widget: \"button\", label: \"x\"}\ntoggle: {widget: \"toggle\", value: false, label: \"x\"}\nlist: {widget: \"list\", rows: [], row: \"template\", row_height: 1}\ntemplate: {widget: \"row\", children: []}\nimage: {widget: \"image\", src: \"x\"}\nspacer: {widget: \"spacer\"}",
        );
        let r = resolve(&d).unwrap();
        for family in [
            "window", "column", "row", "text", "field", "button", "toggle", "list", "image",
            "spacer",
        ] {
            assert!(r.nodes.values().any(|n| n.family == family), "{family}");
        }
        for (id, expected) in [
            ("win", [("edge", json!("right"))].as_slice()),
            (
                "root",
                [
                    ("gap", json!(0.0)),
                    ("padding", json!(0.0)),
                    ("fill", json!(false)),
                ]
                .as_slice(),
            ),
            (
                "row",
                [
                    ("gap", json!(0.0)),
                    ("padding", json!(0.0)),
                    ("fill", json!(false)),
                    ("align", json!("start")),
                ]
                .as_slice(),
            ),
            (
                "text",
                [
                    ("size", json!(13.0)),
                    ("bold", json!(false)),
                    ("mono", json!(false)),
                    ("elide", json!(false)),
                    ("fill", json!(false)),
                    ("hidden", json!(false)),
                ]
                .as_slice(),
            ),
            ("field", [("password", json!(false))].as_slice()),
            ("button", [("tone", json!("normal"))].as_slice()),
            (
                "list",
                [
                    ("gap", json!(0.0)),
                    ("fill", json!(false)),
                    ("hidden_if_empty", json!(false)),
                ]
                .as_slice(),
            ),
        ] {
            for (port, value) in expected {
                assert_eq!(r.nodes[id].ports.get(*port), Some(value), "{id}.{port}");
            }
        }
        assert!(r.nodes["spacer"].ports.is_empty());
    }

    fn numeric_probe(family: &str, port: &str, value: f64) -> Vec<Diagnostic> {
        let mut nodes = IndexMap::new();
        let mut ports = IndexMap::new();
        for (name, value) in [
            ("kind", json!("edge")),
            ("children", json!([])),
            ("text", json!("x")),
            ("value", json!("x")),
            ("label", json!("x")),
            ("rows", json!([])),
            ("row", json!("template")),
            ("row_height", json!(1.0)),
            ("src", json!("x")),
        ] {
            ports.insert(name.into(), value);
        }
        ports.insert(port.into(), json!(value));
        nodes.insert(
            "root".into(),
            RawNode {
                widget: family.into(),
                ports,
                line: 1,
            },
        );
        if family == "list" {
            nodes.insert(
                "template".into(),
                RawNode {
                    widget: "row".into(),
                    ports: [("children".into(), json!([]))].into_iter().collect(),
                    line: 1,
                },
            );
        }
        lint(&SceneDocument {
            name: "test".into(),
            citizen: "c".into(),
            window: None,
            subscribe: None,
            targets: None,
            model: None,
            nodes,
            source: String::new(),
            preparation: PreparationCache::default(),
        })
    }

    #[test]
    fn numeric_port_boundaries_are_table_driven() {
        for family in [
            "window", "column", "row", "text", "field", "button", "list", "image", "spacer",
        ] {
            for port in schema(family).unwrap().iter().filter(|p| p.ty == "number") {
                let zero = numeric_probe(family, port.name, 0.0);
                let negative = numeric_probe(family, port.name, -1.0);
                if port.name == "row_height" {
                    assert!(zero.iter().any(|d| d.code == "port-min"));
                    assert!(
                        !numeric_probe(family, port.name, 0.5)
                            .iter()
                            .any(|d| d.code == "port-min")
                    );
                } else if port.name == "max_rows" {
                    assert!(zero.iter().any(|d| d.code == "port-min"));
                    assert!(
                        !numeric_probe(family, port.name, 1.0)
                            .iter()
                            .any(|d| d.code == "port-min")
                    );
                } else {
                    assert!(!zero.iter().any(|d| d.code == "port-min"));
                }
                assert!(
                    negative.iter().any(|d| d.code == "port-min"),
                    "{family}.{port_name}",
                    port_name = port.name
                );
            }
        }
    }

    #[test]
    fn window_disagreement_checks_all_windows_and_fields() {
        for (field, value) in [
            ("kind", "floating"),
            ("edge", "left"),
            ("title", "wrong"),
            ("w", "10"),
            ("h", "20"),
        ] {
            let source = r#"---
scene: 1
name: test
citizen: c
window: {"kind":"edge","edge":"right","title":"ok","w":1,"h":2}
---
```mix
root: {widget: "column", children: ["a","b"]}
a: {widget: "window", kind: "edge", edge: "right", title: "ok", w: 1, h: 2}
b: {widget: "window", kind: "edge", edge: "right", title: "ok", w: 1, h: 2}
```
"#
            .to_string();
            let mut d = parse(&source).unwrap();
            d.nodes["b"].ports.insert(
                field.into(),
                match field {
                    "kind" | "edge" | "title" => json!(value),
                    _ => json!(value.parse::<f64>().unwrap()),
                },
            );
            assert!(
                lint(&d).iter().any(|x| x.code == "window-disagreement"),
                "{field}"
            );
        }
    }

    #[test]
    fn non_list_row_does_not_seed_template_reachability() {
        let d = doc(
            "root: {widget: \"text\", text: \"x\", row: \"template\"}\ntemplate: {widget: \"text\", text: \"x\"}",
        );
        assert!(
            lint(&d)
                .iter()
                .any(|x| x.code == "orphan-node" && x.message.contains("template"))
        );
    }

    #[test]
    fn list_template_child_check_scans_all_nodes() {
        let d = doc(
            "root: {widget: \"column\", children: [\"list\",\"holder\"]}\nlist: {widget: \"list\", rows: [], row: \"template\", row_height: 1}\nholder: {widget: \"column\", children: [\"template\"]}\ntemplate: {widget: \"row\", children: []}",
        );
        assert!(lint(&d).iter().any(|x| x.code == "invalid-template"));
    }
    #[test]
    fn fixture_equals_hub() {
        let h = std::env::var("COSMIX_HUB").unwrap_or_else(|_| {
            std::env::var("HOME").unwrap_or_else(|_| "/home/user".into()) + "/.ctl"
        });
        let p = std::path::Path::new(&h).join("_lib/scenes/clippanel/clippanel.scene.md");
        if !p.exists() {
            println!("fixture hub absent: {}", p.display());
            return;
        }
        assert_eq!(std::fs::read_to_string(p).unwrap(), C);
    }
    #[test]
    fn numeric_default_diff_is_empty() {
        let a = resolve(&doc("root: {widget: \"text\", text: \"x\"}")).unwrap();
        let b = resolve(&doc("root: {widget: \"text\", text: \"x\", size: 13}")).unwrap();
        assert!(diff(&a, &b).is_empty());
    }
    #[test]
    fn graph_and_lint_regressions() {
        let d = doc(
            "root: {widget: \"column\", children: [\"a\"]}\na: {widget: \"text\", text: \"x\"}\nb: {widget: \"column\", children: [\"c\"]}\nc: {widget: \"column\", children: [\"b\"]}",
        );
        let codes: HashSet<_> = lint(&d).into_iter().map(|x| x.code).collect();
        assert!(codes.contains("cycle") && codes.contains("orphan-node"));
    }
    #[test]
    fn diff_covers_exact_ops_from_resolved_documents() {
        let a = resolve(&doc("root: {widget: \"column\", children: [\"x\",\"y\"]}\nx: {widget: \"text\", text: \"x\", color: \"red\"}\ny: {widget: \"text\", text: \"y\"}")).unwrap();
        let b = resolve(&parse("---\nscene: 1\nname: newer\ncitizen: d\nwindow: {\"kind\":\"edge\"}\nsubscribe: [\"x\"]\n---\n```mix\nroot: {widget: \"column\", children: [\"z\",\"x\"]}\nx: {widget: \"text\", text: \"z\"}\nz: {widget: \"text\", text: \"new\"}\n```").unwrap()).unwrap();
        assert_eq!(
            diff(&a, &b),
            vec![
                Op::Remove { id: "y".into() },
                Op::SetPort {
                    id: "root".into(),
                    port: "children".into(),
                    value: json!(["z", "x"])
                },
                Op::SetPort {
                    id: "x".into(),
                    port: "color".into(),
                    value: JsonValue::Null
                },
                Op::SetPort {
                    id: "x".into(),
                    port: "text".into(),
                    value: json!("z")
                },
                Op::Insert {
                    id: "z".into(),
                    parent: Some("root".into()),
                    index: 0,
                    node: b.nodes["z"].clone()
                },
                Op::Reparent {
                    id: "x".into(),
                    parent: Some("root".into()),
                    index: 1
                },
                Op::SetScene {
                    field: "name".into(),
                    value: json!("newer")
                },
                Op::SetScene {
                    field: "citizen".into(),
                    value: json!("d")
                },
                Op::SetScene {
                    field: "window".into(),
                    value: json!({"kind":"edge"})
                },
                Op::SetScene {
                    field: "subscribe".into(),
                    value: json!(["x"])
                },
            ]
        );
    }
    #[test]
    fn every_lint_class_has_a_diagnostic() {
        let valid =
            |body: &str| format!("---\nscene: 1\nname: test\ncitizen: c\n---\n```mix\n{body}\n```");
        let cases = vec![
            ("document-too-large", "x".repeat(MAX_DOCUMENT_BYTES + 1)),
            ("envelope", "not an envelope".into()),
            ("missing-header", "---\nscene: 1\n---\n```mix\nroot: {widget: \"text\", text: \"x\"}\n```\n".into()),
            ("scene-version", valid("root: {widget: \"text\", text: \"x\"}").replacen("scene: 1", "scene: 2", 1)),
            ("invalid-name", valid("root: {widget: \"text\", text: \"x\"}").replacen("name: test", "name: Bad", 1)),
            ("fence-count", "---\nscene: 1\nname: test\ncitizen: c\n---\n```mix\nroot: {widget: \"text\", text: \"x\"}\n```\n```mix\na: {widget: \"text\", text: \"x\"}\n```\n".into()),
            ("fence-count", "---\nscene: 1\nname: test\ncitizen: c\n---\nno fence\n".into()),
            ("mix-parse", valid("root: {widget: \"text\", text: \"x\"}\n\"unterminated").into()),
            ("strict-data", valid("root: {widget: \"text\", text: \"x\"}\nvalue: \"a\" ..\n  \"b\"").into()),
            ("duplicate-id", valid("root: {widget: \"text\", text: \"x\"}\nroot: {widget: \"text\", text: \"y\"}").into()),
            ("duplicate-id", valid("root: {widget: \"column\", children: [\"child\"]}\nchild: {widget: \"text\", text: \"x\"}\nchild: {widget: \"text\", text: \"y\"}").into()),
            ("root-type", valid("[\"x\"]").into()),
            ("node-type", valid("root: \"x\"").into()),
            ("missing-widget", valid("root: {}").into()),
            ("invalid-id", valid("root: {widget: \"column\", children: [\"bad@id\"]}\n\"bad@id\": {widget: \"text\", text: \"x\"}").into()),
            ("header-json", valid("root: {widget: \"text\", text: \"x\"}").replacen("citizen: c", "citizen: c\nwindow: nope", 1)),
            ("node-limit", valid(&(0..=MAX_NODES).map(|i| format!("n{i}: {{widget: \"text\", text: \"x\"}}\n")).collect::<String>())),
            ("unknown-family", valid("root: {widget: \"unknown\"}").into()),
            ("unknown-port", valid("root: {widget: \"text\", text: \"x\", nope: 1}").into()),
            ("port-type", valid("root: {widget: \"text\", text: 1}").into()),
            ("enum-value", valid("root: {widget: \"button\", label: \"x\", tone: \"bad\"}").into()),
            ("port-min", valid("root: {widget: \"list\", rows: [], row: \"t\", row_height: 0}\nt: {widget: \"row\", children: []}").into()),
            ("missing-port", valid("root: {widget: \"text\"}").into()),
            ("dangling-child", valid("root: {widget: \"column\", children: [\"gone\"]}").into()),
            ("child-type", valid("root: {widget: \"column\", children: [1]}").into()),
            ("window-kind", valid("root: {widget: \"window\", kind: \"floating\"}").into()),
            ("row-limit", valid("root: {widget: \"list\", rows: [], row: \"t\", row_height: 1}\nt: {widget: \"row\", children: []}").into()),
            ("row-type", valid("root: {widget: \"list\", rows: [{}], row: \"t\", row_height: 1}\nt: {widget: \"row\", children: []}").into()),
            ("invalid-template", valid("root: {widget: \"list\", rows: [], row: \"t\", row_height: 1}\nt: {widget: \"button\", label: \"x\"}").into()),
            ("cell-substitution", valid("root: {widget: \"list\", rows: [{id: \"1\", cells: [\"x\"]}], row: \"t\", row_height: 1}\nt: {widget: \"row\", children: [\"x\"]}\nx: {widget: \"text\", text: \"{cells[1]}\"}").into()),
            ("missing-root", valid("x: {widget: \"text\", text: \"x\"}").into()),
            ("multiple-parents", valid("root: {widget: \"column\", children: [\"a\", \"b\"]}\na: {widget: \"column\", children: [\"x\"]}\nb: {widget: \"column\", children: [\"x\"]}\nx: {widget: \"text\", text: \"x\"}").into()),
            ("window-disagreement", valid("root: {widget: \"window\", kind: \"edge\", title: \"node\"}").replacen("citizen: c", "citizen: c\nwindow: {\"kind\":\"edge\",\"title\":\"header\"}", 1)),
            ("orphan-node", valid("root: {widget: \"text\", text: \"x\"}\nother: {widget: \"text\", text: \"y\"}").into()),
            ("cycle", valid("root: {widget: \"column\", children: [\"a\"]}\na: {widget: \"column\", children: [\"root\"]}").into()),
            ("invalid-binding", valid("root: {widget: \"text\", text: \"= \"}").into()),
            ("binding-policy", valid("root: {widget: \"text\", text: \"= send \\\"x\\\" y\"}").into()),
            ("binding-not-allowed", valid("root: {widget: \"column\", children: \"= $model.children\"}").into()),
            ("binding-nondeterministic", valid("root: {widget: \"text\", text: \"= time()\"}").into()),
            ("binding-eval", valid("root: {widget: \"text\", text: \"= 1 / 0\"}").into()),
            ("binding-type", valid("root: {widget: \"text\", text: \"= 1\"}").into()),
            ("model-path", valid("root: {widget: \"text\", text: \"x\"}").into()),
        ];
        let table: HashSet<_> = cases.iter().map(|(code, _)| *code).collect();
        for code in ALL_CODES {
            assert!(table.contains(code), "missing table entry: {code}");
        }
        for (expected, source) in cases {
            let diagnostics = match parse(&source) {
                Ok(mut d) if expected == "row-limit" => {
                    let rows = (0..=MAX_ROWS)
                        .map(|i| json!({"id": i.to_string(), "cells": ["x"]}))
                        .collect();
                    d.nodes["root"]
                        .ports
                        .insert("rows".into(), JsonValue::Array(rows));
                    lint(&d)
                }
                Ok(d) if expected == "model-path" => {
                    let tree = resolve(&d).unwrap();
                    bindings::reevaluate(&tree, &bindings::compile(&d).unwrap(), "wrong.x", &json!(1)).unwrap_err()
                }
                Ok(d) => lint(&d),
                Err(d) => d,
            };
            assert!(
                diagnostics.iter().any(|d| d.code == expected),
                "{expected}: {diagnostics:?}"
            );
        }
    }
    #[test]
    fn hundred_rows_parse() {
        let mut body = String::from("root: {widget: \"list\", rows: [");
        for i in 0..100 {
            if i > 0 {
                body.push(',');
            }
            body.push_str(&format!("{{id: \"{i}\", cells: [\"x\"]}}"));
        }
        body.push_str("], row: \"t\", row_height: 1}\nt: {widget: \"row\", children: []}");
        let start = std::time::Instant::now();
        assert!(
            parse(&format!(
                "---\nscene: 1\nname: test\ncitizen: c\n---\n```mix\n{body}\n```\n"
            ))
            .is_ok()
        );
        assert!(
            start.elapsed().as_millis() < 5,
            "100-row parse took {:?}",
            start.elapsed()
        );
    }
    #[test]
    fn ragged_rows_reject_missing_cell() {
        let d = doc(
            "root: {widget: \"list\", rows: [{id: \"a\", cells: [\"x\"]}, {id: \"b\", cells: []}], row: \"t\", row_height: 1}\nt: {widget: \"row\", children: [\"x\"]}\nx: {widget: \"text\", text: \"{cells[0]}\"}",
        );
        let codes: HashSet<_> = lint(&d).into_iter().map(|x| x.code).collect();
        assert!(codes.contains("cell-substitution"));
    }

    #[test]
    fn binding_compile_rejects_bad_syntax_and_policy() {
        let d = doc("root: {widget: \"text\", text: \"= send \\\"x\\\" y\"}");
        let diagnostics = lint(&d);
        assert!(diagnostics.iter().any(|x| x.code == "binding-policy" && x.line > 0));
        let d = doc("root: {widget: \"text\", text: \"= ($model.x\"}");
        assert!(lint(&d).iter().any(|x| x.code == "invalid-binding"));
        let d = doc("root: {widget: \"text\", text: \"= $model.x\\n$model.y\"}");
        assert!(lint(&d).iter().any(|x| x.code == "invalid-binding"));
    }

    #[test]
    fn no_v0_fixture_port_starts_with_equals() {
        for source in [C, F, S] {
            let d = parse(source).unwrap();
            assert!(d.nodes.values().flat_map(|n| n.ports.values()).all(|v| !v.as_str().is_some_and(|s| s.starts_with("= "))));
        }
    }

    #[test]
    fn literal_leading_equals_escape() {
        let r = resolve(&doc("root: {widget: \"text\", text: \"== x\"}\na: {widget: \"text\", text: \"=x\"}")).unwrap();
        assert_eq!(r.nodes["root"].ports["text"], json!("= x"));
        assert_eq!(r.nodes["a"].ports["text"], json!("=x"));
    }

    #[test]
    fn binding_deps_are_syntactic() {
        let d = doc("root: {widget: \"text\", text: \"= $model.a.b .. $model.c\"}");
        let set = bindings::compile(&d).unwrap();
        assert_eq!(set.bindings["root.text"].deps, ["model.a.b", "model.c"].into_iter().map(String::from).collect());
        let d = doc("root: {widget: \"text\", text: \"= $model.m[$model.k].x\"}");
        let set = bindings::compile(&d).unwrap();
        assert_eq!(set.bindings["root.text"].deps, ["model.m", "model.k"].into_iter().map(String::from).collect());
        let d = doc("root: {widget: \"list\", rows: [], row: \"t\", row_height: 1}\nt: {widget: \"text\", text: \"= $item.cells[0]\"}");
        assert!(bindings::compile(&d).unwrap().bindings["t.text"].reads_item);
        let d = doc("root: {widget: \"text\", text: \"= $item.cells[0]\"}");
        assert!(bindings::compile(&d).unwrap_err().iter().any(|d| d.code == "binding-policy"));
    }

    #[test]
    fn model_patch_reevaluates_exactly_the_dirty_set() {
        let d = doc("root: {widget: \"column\", children: [\"a\",\"b\",\"c\"]}\na: {widget: \"text\", text: \"= $model.a\"}\nb: {widget: \"text\", text: \"= $model.b\"}\nc: {widget: \"text\", text: \"static\"}");
        let set = bindings::compile(&d).unwrap();
        let tree = resolve(&d).unwrap();
        let result = bindings::reevaluate(&tree, &set, "model.a", &json!("new")).unwrap();
        assert_eq!(result.evaluated, vec!["a.text"]);
    }

    #[test]
    fn ancestor_descendant_dirty_relation() {
        let d = doc("root: {widget: \"column\", children: [\"a\",\"b\"]}\na: {widget: \"text\", text: \"= $model.a.b\"}\nb: {widget: \"text\", text: \"= $model.a\"}");
        let set = bindings::compile(&d).unwrap();
        let tree = resolve(&d).unwrap();
        assert_eq!(bindings::reevaluate(&tree, &set, "model.a", &json!({"b":"x"})).unwrap().evaluated, vec!["a.text", "b.text"]);
        assert!(bindings::reevaluate(&tree, &set, "model.z", &json!(1)).unwrap().evaluated.is_empty());
    }

    #[test]
    fn eval_error_keeps_last_good_and_reports() {
        let mut d = doc("root: {widget: \"text\", text: \"= $model.fail ? 1 / 0 : $model.title\"}");
        d.model = Some(json!({"fail":false, "title":"last good"}));
        let set = bindings::compile(&d).unwrap();
        let tree = resolve(&d).unwrap();
        let result = bindings::reevaluate(&tree, &set, "model.fail", &json!(true)).unwrap();
        assert_eq!(result.tree.nodes["root"].ports["text"], json!("last good"));
        assert!(result.diagnostics.iter().any(|x| x.code == "binding-eval"));
    }

    #[test]
    fn load_evaluates_against_envelope_model() {
        let mut d = doc("root: {widget: \"text\", text: \"= $model.title\"}");
        d.model = Some(json!({"title":"hello"}));
        assert_eq!(resolve(&d).unwrap().nodes["root"].ports["text"], json!("hello"));
        let set = bindings::compile(&d).unwrap();
        let patched = bindings::reevaluate(&resolve(&d).unwrap(), &set, "model.title", &json!("patched")).unwrap();
        assert_eq!(patched.tree.nodes["root"].ports["text"], json!("patched"));
        assert_eq!(resolve(&d).unwrap().nodes["root"].ports["text"], json!("hello"));
    }

    #[test]
    fn binding_type_mismatch_keeps_last_good() {
        let mut d = doc("root: {widget: \"text\", text: \"= $model.value\"}");
        d.model = Some(json!({"value":"last good"}));
        let set = bindings::compile(&d).unwrap();
        let tree = resolve(&d).unwrap();
        let result = bindings::reevaluate(&tree, &set, "model.value", &json!(3)).unwrap();
        assert!(result.diagnostics.iter().any(|x| x.code == "binding-type"));
        assert_eq!(result.tree.nodes["root"].ports["text"], json!("last good"));
        assert!(result.changed.is_empty());
    }

    #[test]
    fn structural_ports_reject_bindings() {
        let d = doc("root: {widget: \"column\", children: \"= $model.children\"}");
        assert!(lint(&d).iter().any(|x| x.code == "binding-not-allowed"));
    }

    #[test]
    fn reeval_result_revalidated() {
        for (body, port, good, bad) in [
            (r#"root: {widget: "button", label: "x", tone: "= $model.value"}"#, "tone", json!("normal"), json!("bad")),
            (r#"root: {widget: "text", text: "x", size: "= $model.value"}"#, "size", json!(13), json!(-1)),
            ("root: {widget: \"list\", rows: \"= $model.value\", row: \"t\", row_height: 1}\nt: {widget: \"row\", children: []}", "rows", json!([]), json!([{}])),
        ] {
            let mut d = doc(body);
            d.model = Some(json!({"value":good}));
            let set = bindings::compile(&d).unwrap();
            let tree = resolve(&d).unwrap();
            let result = bindings::reevaluate(&tree, &set, "model.value", &bad).unwrap();
            assert_eq!(result.tree.nodes["root"].ports[port], tree.nodes["root"].ports[port]);
            assert_eq!(result.diagnostics.len(), 1);
            assert_eq!(result.diagnostics[0].code, "binding-type");
            assert!(result.changed.is_empty());
        }
    }

    #[test]
    fn noop_reeval_diff_is_empty() {
        let mut d = doc("root: {widget: \"text\", text: \"= $model.title\"}");
        d.model = Some(json!({"title":"x"}));
        let set = bindings::compile(&d).unwrap();
        let tree = resolve(&d).unwrap();
        assert!(bindings::reevaluate(&tree, &set, "model.title", &json!("x")).unwrap().changed.is_empty());
    }

    #[test]
    fn clock_one_hz() {
        let d = doc("root: {widget: \"text\", text: \"= $model.now\"}");
        let set = bindings::compile(&d).unwrap();
        let tree = resolve(&d).unwrap();
        let a = bindings::reevaluate(&tree, &set, "model.now", &json!("one")).unwrap();
        assert_eq!(a.tree.nodes["root"].ports["text"], json!("one"));
        assert_eq!(a.evaluated, vec!["root.text"]);
        let b = bindings::reevaluate(&a.tree, &set, "model.now", &json!("two")).unwrap();
        assert_eq!(b.tree.nodes["root"].ports["text"], json!("two"));
        assert_eq!(b.evaluated, vec!["root.text"]);
    }

    #[test]
    fn five_hundred_rows_patch_costs_two_bindings() {
        let d = doc("root: {widget: \"column\", children: [\"list\",\"count\"]}\nlist: {widget: \"list\", rows: \"= $model.entries\", row: \"template\", row_height: 1}\ncount: {widget: \"text\", text: \"= $model.entries[0].id\"}\ntemplate: {widget: \"row\", children: []}");
        let set = bindings::compile(&d).unwrap();
        let tree = resolve(&d).unwrap();
        let entries: Vec<_> = (0..500).map(|i| json!({"id": i.to_string(), "cells": ["x"]})).collect();
        let result = bindings::reevaluate(&tree, &set, "model.entries", &JsonValue::Array(entries)).unwrap();
        assert_eq!(result.evaluated, vec!["list.rows", "count.text"]);
    }

    #[test]
    fn template_instantiate_binds_item() {
        let d = doc("root: {widget: \"list\", rows: [], row: \"template\", row_height: 1}\ntemplate: {widget: \"row\", children: [\"text\",\"static\"]}\ntext: {widget: \"text\", text: \"= $item.cells[0]\"}\nstatic: {widget: \"text\", text: \"static\"}");
        let set = bindings::compile(&d).unwrap();
        let tree = resolve(&d).unwrap();
        let item = json!({"cells":["row"]});
        let result = bindings::template_instantiate("text", &tree.nodes["text"], &set, &tree.model, &item).unwrap();
        assert_eq!(result.ports["text"], json!("row"));
        let result = bindings::template_instantiate("static", &tree.nodes["static"], &set, &tree.model, &item).unwrap();
        assert_eq!(result.ports["text"], json!("static"));
    }

}
