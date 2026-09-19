use crate::{Diagnostic, Node, Port, ResolvedScene, SceneDocument, Severity, MAX_ROWS};
use cosmix_mix::ast::{Expr, StmtKind};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;
use cosmix_mix::value::Value;
use cosmix_mix::{eval_expr_string, CategoryAllowList, EvalLimits, MAX_EXPR_DEPTH};
use serde_json::{json, Value as JsonValue};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq)]
pub struct CompiledBinding {
    pub source: String,
    pub deps: BTreeSet<String>,
    pub reads_item: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BindingSet {
    pub bindings: BTreeMap<String, CompiledBinding>,
    pub model: JsonValue,
    pub order: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ReEval {
    pub tree: ResolvedScene,
    pub diagnostics: Vec<Diagnostic>,
    pub changed: Vec<(String, JsonValue)>,
    pub evaluated: Vec<String>,
}

pub(crate) fn binding_source(value: &JsonValue) -> Option<String> {
    let s = value.as_str()?;
    if s == "=" {
        Some(String::new())
    } else if s.starts_with("== ") {
        None
    } else if s
        .strip_prefix('=')
        .is_some_and(|rest| rest.chars().next().is_some_and(char::is_whitespace))
    {
        Some(s[1..].trim().into())
    } else {
        None
    }
}

pub(crate) fn escaped_literal(value: &JsonValue) -> Option<JsonValue> {
    value
        .as_str()
        .and_then(|s| s.strip_prefix("== ").map(|x| json!(format!("= {x}"))))
}

pub fn compile(doc: &SceneDocument) -> Result<BindingSet, Vec<Diagnostic>> {
    let mut set = BindingSet {
        bindings: BTreeMap::new(),
        model: doc.model.clone().unwrap_or_else(|| json!({})),
        order: Vec::new(),
    };
    let mut errors = Vec::new();
    let templates = template_ids(doc);
    for (id, node) in &doc.nodes {
        for (port, value) in &node.ports {
            let Some(source) = binding_source(value) else {
                continue;
            };
            let path = format!("{id}.{port}");
            set.order.push(path.clone());
            if matches!(port.as_str(), "children" | "row" | "widget") {
                errors.push(Diagnostic::error(
                    "binding-not-allowed",
                    node.line,
                    format!("binding is not allowed on {path}"),
                ));
                continue;
            }
            let parsed = parse_expression(&source);
            let Ok(expr) = parsed else {
                let code = if source.trim_start().starts_with("send ") {
                    "binding-policy"
                } else {
                    "invalid-binding"
                };
                errors.push(Diagnostic::error(
                    code,
                    node.line,
                    format!("invalid binding on {path}"),
                ));
                continue;
            };
            let mut deps = BTreeSet::new();
            let mut reads_item = false;
            let mut bad_root = false;
            let mut nondeterministic = false;
            walk_expr(
                &expr,
                &mut deps,
                &mut reads_item,
                &mut bad_root,
                &mut nondeterministic,
            );
            if bad_root || (reads_item && !templates.contains(id)) {
                errors.push(Diagnostic::error(
                    "binding-policy",
                    node.line,
                    format!("binding on {path} reads a disallowed root"),
                ));
            }
            if nondeterministic {
                errors.push(Diagnostic::warning(
                    "binding-nondeterministic",
                    node.line,
                    format!("binding on {path} calls time()"),
                ));
            }
            if expr_depth(&expr, 0) > MAX_EXPR_DEPTH {
                errors.push(Diagnostic::error(
                    "invalid-binding",
                    node.line,
                    format!("binding on {path} is too deep"),
                ));
            }
            set.bindings.insert(
                path,
                CompiledBinding {
                    source,
                    deps,
                    reads_item,
                },
            );
        }
    }
    if errors.iter().any(|d| d.severity == Severity::Error) {
        Err(errors)
    } else {
        Ok(set)
    }
}

pub fn reevaluate(
    tree: &ResolvedScene,
    set: &BindingSet,
    path: &str,
    value: &JsonValue,
) -> Result<ReEval, Vec<Diagnostic>> {
    let parts: Vec<_> = path.split('.').collect();
    if parts.is_empty() || parts.iter().any(|p| p.is_empty()) || parts[0] != "model" {
        return Err(vec![Diagnostic::error(
            "model-path",
            1,
            "model patch path must start with model",
        )]);
    }
    let mut next = tree.clone();
    if !apply_model_patch(&mut next.model, &parts[1..], value) {
        return Err(vec![Diagnostic::error(
            "model-path",
            1,
            "model patch path must contain map keys",
        )]);
    }
    let old = tree.clone();
    let mut diagnostics = Vec::new();
    let mut evaluated = Vec::new();
    for path in &set.order {
        let Some(binding) = set.bindings.get(path) else {
            continue;
        };
        if binding.reads_item
            || !binding
                .deps
                .iter()
                .any(|dep| related(dep, &parts.join(".")))
        {
            continue;
        }
        evaluated.push(path.clone());
        let Some((id, port)) = path.split_once('.') else {
            continue;
        };
        let Some(node) = next.nodes.get_mut(id) else {
            continue;
        };
        let authored = node.ports.get(port).cloned();
        let result = evaluate(binding, &next.model, None)
            .map_err(|_| "binding-eval".to_string())
            .and_then(|v| coerce_port(&v, crate::port_for(&node.family, port)));
        match result {
            Ok(v) => {
                node.ports.insert(port.into(), v);
            }
            Err(code) => {
                diagnostics.push(Diagnostic::warning(
                    code.clone(),
                    node.line,
                    format!("binding evaluation failed for {path}"),
                ));
                if let Some(v) = authored {
                    node.ports.insert(port.into(), v);
                }
            }
        }
    }
    let changed = crate::port_changes(&old, &next);
    Ok(ReEval {
        tree: next,
        diagnostics,
        changed,
        evaluated,
    })
}

pub fn template_instantiate(
    node: &Node,
    set: &BindingSet,
    item: &JsonValue,
) -> Result<Node, Diagnostic> {
    let mut out = node.clone();
    for (path, binding) in &set.bindings {
        let Some((_id, port)) = path.split_once('.') else {
            continue;
        };
        if !binding.reads_item {
            continue;
        }
        if !node.ports.contains_key(port) {
            continue;
        }
        let value = evaluate(binding, &set.model, Some(item))
            .and_then(|v| coerce_port(&v, crate::port_for(&node.family, port)))
            .map_err(|code| {
                Diagnostic::warning(
                    code,
                    node.line,
                    format!("template binding failed for {path}"),
                )
            })?;
        out.ports.insert(port.into(), value);
    }
    out.ports.shift_remove("__model");
    Ok(out)
}

fn parse_expression(source: &str) -> Result<Expr, cosmix_mix::MixError> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize()?;
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program()?;
    if stmts.len() != 1 {
        return Err(cosmix_mix::MixError::RuntimeError {
            span: None,
            msg: "expected one expression".into(),
        });
    }
    match stmts.into_iter().next().unwrap().kind {
        StmtKind::Expression(expr) => Ok(expr),
        _ => Err(cosmix_mix::MixError::RuntimeError {
            span: None,
            msg: "expected expression".into(),
        }),
    }
}

pub(crate) fn evaluate(
    binding: &CompiledBinding,
    model: &JsonValue,
    item: Option<&JsonValue>,
) -> Result<Value, String> {
    if binding
        .source
        .chars()
        .any(|c| matches!(c, '+' | '-' | '*' | '/'))
        && binding
            .deps
            .iter()
            .any(|dep| lookup_path(model, dep).is_none())
    {
        return Err("binding-eval".into());
    }
    let mut globals = vec![("model", to_mix(model))];
    if let Some(item) = item {
        globals.push(("item", to_mix(item)));
    }
    eval_expr_string(
        &binding.source,
        &globals,
        Some(Rc::new(CategoryAllowList::deny_all())),
        EvalLimits {
            max_string_len: Some(1 << 20),
            max_list_len: Some(MAX_ROWS),
            max_map_len: Some(1024),
            time_limit: Some(Duration::from_millis(50)),
            ..Default::default()
        },
    )
    .map_err(|e| e.to_string())
}

fn lookup_path<'a>(model: &'a JsonValue, path: &str) -> Option<&'a JsonValue> {
    let mut value = model;
    for part in path.strip_prefix("model.").unwrap_or("").split('.') {
        if part.is_empty() {
            return Some(value);
        }
        value = value.get(part)?;
    }
    Some(value)
}

pub(crate) fn coerce_port(value: &Value, port: Option<Port>) -> Result<JsonValue, String> {
    let Some(port) = port else {
        return Err("binding-type".into());
    };
    if matches!(value, Value::Nil) {
        return Ok(port
            .default
            .map(|x| serde_json::from_str(x).unwrap_or(JsonValue::Null))
            .unwrap_or(JsonValue::Null));
    }
    let ok = matches!(
        (port.ty, value),
        ("string", Value::String(_))
            | ("number", Value::Number(_))
            | ("bool", Value::Bool(_))
            | ("list", Value::List(_))
            | ("object", Value::Map(_))
    );
    if !ok {
        return Err("binding-type".into());
    }
    let j = from_mix(value).ok_or_else(|| "binding-type".to_string())?;
    let mut ds = Vec::new();
    crate::check_port_value("bound", 1, port, &j, &mut ds);
    if ds.iter().any(|d| d.severity == Severity::Error) {
        Err("binding-type".into())
    } else {
        Ok(crate::normalize_number(j))
    }
}

fn to_mix(value: &JsonValue) -> Value {
    match value {
        JsonValue::Null => Value::Nil,
        JsonValue::Bool(v) => Value::Bool(*v),
        JsonValue::Number(v) => Value::Number(v.as_f64().unwrap_or(0.0)),
        JsonValue::String(v) => Value::String(v.clone()),
        JsonValue::Array(values) => Value::list(values.iter().map(to_mix).collect()),
        JsonValue::Object(values) => {
            Value::map(values.iter().map(|(k, v)| (k.clone(), to_mix(v))).collect())
        }
    }
}

fn from_mix(value: &Value) -> Option<JsonValue> {
    Some(match value {
        Value::Nil => JsonValue::Null,
        Value::Bool(v) => JsonValue::Bool(*v),
        Value::Number(v) => serde_json::Number::from_f64(*v).map(JsonValue::Number)?,
        Value::String(v) => JsonValue::String(v.clone()),
        Value::List(values) => {
            JsonValue::Array(values.iter().map(from_mix).collect::<Option<_>>()?)
        }
        Value::Map(values) => JsonValue::Object(
            values
                .iter()
                .map(|(k, v)| Some((k.clone(), from_mix(v)?)))
                .collect::<Option<_>>()?,
        ),
        _ => return None,
    })
}

pub(crate) fn evaluate_for_resolve(
    binding: &CompiledBinding,
    model: &JsonValue,
    port: Port,
) -> Result<JsonValue, String> {
    let value = evaluate(binding, model, None)?;
    coerce_port(&value, Some(port))
}

fn apply_model_patch(model: &mut JsonValue, parts: &[&str], value: &JsonValue) -> bool {
    if parts.is_empty() || parts.iter().any(|p| p.is_empty()) {
        return false;
    }
    if !model.is_object() {
        *model = json!({});
    }
    let mut current = model;
    for part in &parts[..parts.len() - 1] {
        let Some(map) = current.as_object_mut() else {
            return false;
        };
        current = map.entry(*part).or_insert_with(|| json!({}));
        if !current.is_object() {
            return false;
        }
    }
    let Some(map) = current.as_object_mut() else {
        return false;
    };
    if value.is_null() {
        map.remove(parts[parts.len() - 1]);
    } else {
        map.insert(parts[parts.len() - 1].into(), value.clone());
    }
    true
}

fn related(dep: &str, patch: &str) -> bool {
    dep == patch
        || dep.starts_with(&(patch.to_owned() + "."))
        || patch.starts_with(&(dep.to_owned() + "."))
}
fn template_ids(doc: &SceneDocument) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    fn visit(id: &str, doc: &SceneDocument, ids: &mut BTreeSet<String>) {
        if !ids.insert(id.into()) {
            return;
        }
        if let Some(node) = doc.nodes.get(id) {
            if let Some(children) = node.ports.get("children").and_then(JsonValue::as_array) {
                for child in children.iter().filter_map(JsonValue::as_str) {
                    visit(child, doc, ids);
                }
            }
        }
    }
    for row in doc
        .nodes
        .values()
        .filter(|n| n.widget == "list")
        .filter_map(|n| n.ports.get("row").and_then(JsonValue::as_str))
    {
        visit(row, doc, &mut ids);
    }
    ids
}

fn walk_expr(
    expr: &Expr,
    deps: &mut BTreeSet<String>,
    reads_item: &mut bool,
    bad_root: &mut bool,
    nondeterministic: &mut bool,
) {
    match expr {
        Expr::Variable(v) => {
            if v == "item" {
                *reads_item = true
            } else if v == "model" {
                deps.insert("model".into());
            } else {
                *bad_root = true
            }
        }
        Expr::FieldAccess { object, .. } => {
            if let Some(path) = access_path(expr, "model") {
                deps.insert(path);
            } else if access_path(expr, "item").is_some() {
                *reads_item = true;
            }
            walk_access_indices(object, deps, reads_item, bad_root, nondeterministic);
        }
        Expr::Index { object, index } => {
            if let Some(path) = access_path(object, "model") {
                deps.insert(path);
            } else if access_path(object, "item").is_some() {
                *reads_item = true;
            }
            walk_access_indices(object, deps, reads_item, bad_root, nondeterministic);
            walk_expr(index, deps, reads_item, bad_root, nondeterministic);
        }
        Expr::FunctionCall { name, args } => {
            if name == "time" {
                *nondeterministic = true;
            }
            for a in args {
                walk_expr(a, deps, reads_item, bad_root, nondeterministic);
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            walk_expr(left, deps, reads_item, bad_root, nondeterministic);
            walk_expr(right, deps, reads_item, bad_root, nondeterministic);
        }
        Expr::UnaryOp { operand, .. } => {
            walk_expr(operand, deps, reads_item, bad_root, nondeterministic)
        }
        Expr::Ternary {
            cond,
            then_branch,
            else_branch,
        } => {
            walk_expr(cond, deps, reads_item, bad_root, nondeterministic);
            walk_expr(then_branch, deps, reads_item, bad_root, nondeterministic);
            walk_expr(else_branch, deps, reads_item, bad_root, nondeterministic);
        }
        Expr::ListLiteral(xs) => {
            for x in xs {
                walk_expr(x, deps, reads_item, bad_root, nondeterministic)
            }
        }
        Expr::MapLiteral(xs) => {
            for (_, x) in xs {
                walk_expr(x, deps, reads_item, bad_root, nondeterministic)
            }
        }
        Expr::ValueCall { callee, args } => {
            walk_expr(callee, deps, reads_item, bad_root, nondeterministic);
            for a in args {
                walk_expr(a, deps, reads_item, bad_root, nondeterministic);
            }
        }
        Expr::MethodCall { object, args, .. } => {
            walk_expr(object, deps, reads_item, bad_root, nondeterministic);
            for a in args {
                walk_expr(a, deps, reads_item, bad_root, nondeterministic);
            }
        }
        Expr::Send {
            target,
            command,
            args,
        } => {
            *bad_root = true;
            walk_expr(target, deps, reads_item, bad_root, nondeterministic);
            walk_expr(command, deps, reads_item, bad_root, nondeterministic);
            for (_, a) in args {
                walk_expr(a, deps, reads_item, bad_root, nondeterministic);
            }
        }
        Expr::Sh(x) => {
            *bad_root = true;
            walk_expr(x, deps, reads_item, bad_root, nondeterministic);
        }
        Expr::If(x) => {
            walk_expr(&x.condition, deps, reads_item, bad_root, nondeterministic);
            for s in &x.then_body {
                walk_stmt(s, deps, reads_item, bad_root, nondeterministic);
            }
            for (c, b) in &x.else_ifs {
                walk_expr(c, deps, reads_item, bad_root, nondeterministic);
                for s in b {
                    walk_stmt(s, deps, reads_item, bad_root, nondeterministic);
                }
            }
            if let Some(b) = &x.else_body {
                for s in b {
                    walk_stmt(s, deps, reads_item, bad_root, nondeterministic);
                }
            }
        }
        Expr::FunctionLiteral { body, .. } => match body.as_ref() {
            cosmix_mix::ast::FunctionBody::Expression(e) => {
                walk_expr(e, deps, reads_item, bad_root, nondeterministic)
            }
            cosmix_mix::ast::FunctionBody::Block(b) => {
                for s in b {
                    walk_stmt(s, deps, reads_item, bad_root, nondeterministic)
                }
            }
        },
        _ => {}
    }
}
fn walk_stmt(
    s: &cosmix_mix::ast::Stmt,
    d: &mut BTreeSet<String>,
    i: &mut bool,
    b: &mut bool,
    n: &mut bool,
) {
    if let StmtKind::Expression(e) = &s.kind {
        walk_expr(e, d, i, b, n);
    }
}
fn walk_access_indices(
    expr: &Expr,
    d: &mut BTreeSet<String>,
    i: &mut bool,
    b: &mut bool,
    n: &mut bool,
) {
    match expr {
        Expr::Index { object, index } => {
            walk_access_indices(object, d, i, b, n);
            walk_expr(index, d, i, b, n);
        }
        Expr::FieldAccess { object, .. } => walk_access_indices(object, d, i, b, n),
        _ => {}
    }
}
fn access_path(expr: &Expr, root: &str) -> Option<String> {
    match expr {
        Expr::Variable(v) if v == root => Some(root.into()),
        Expr::FieldAccess { object, field } => {
            access_path(object, root).map(|p| format!("{p}.{field}"))
        }
        Expr::Index { object, .. } => access_path(object, root),
        _ => None,
    }
}
fn expr_depth(expr: &Expr, depth: usize) -> usize {
    let d = depth + 1;
    match expr {
        Expr::BinaryOp { left, right, .. } => d.max(expr_depth(left, d)).max(expr_depth(right, d)),
        Expr::UnaryOp { operand, .. } => d.max(expr_depth(operand, d)),
        Expr::FieldAccess { object, .. } | Expr::Index { object, .. } => {
            d.max(expr_depth(object, d))
        }
        Expr::ListLiteral(xs) => xs.iter().map(|x| expr_depth(x, d)).max().unwrap_or(d),
        Expr::MapLiteral(xs) => xs.iter().map(|(_, x)| expr_depth(x, d)).max().unwrap_or(d),
        _ => d,
    }
}
