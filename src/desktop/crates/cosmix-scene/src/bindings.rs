use crate::{Diagnostic, Node, Port, ResolvedScene, SceneDocument, Severity, MAX_ROWS};
use cosmix_mix::ast::{Expr, StmtKind};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;
use cosmix_mix::value::Value;
use cosmix_mix::{eval_expr_string, expr_mode_check, CategoryAllowList, EvalLimits};
use cosmix_mix::token::{StringPart, Token};
use serde_json::{json, Value as JsonValue};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::time::{Duration, Instant};

pub(crate) const EVALUATION_BUDGET: Duration = Duration::from_millis(250);

#[cfg(test)]
thread_local! {
    pub(crate) static COMPILE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(crate) static EVALUATE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(crate) static MODEL_CONVERSION_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompiledBinding {
    pub source: String,
    pub deps: BTreeSet<String>,
    pub reads_item: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BindingSet {
    pub bindings: BTreeMap<String, CompiledBinding>,
    pub order: Vec<String>,
    pub diagnostics: Vec<Diagnostic>,
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
    #[cfg(test)]
    COMPILE_COUNT.with(|n| n.set(n.get() + 1));
    let mut set = BindingSet {
        bindings: BTreeMap::new(),
        order: Vec::new(),
        diagnostics: Vec::new(),
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
            if let Err(error) = expr_mode_check(&source) {
                let message = error.to_string();
                // A leading anonymous function is parsed as a statement by
                // Mix; classify this denied construct from tokens, not text.
                let function = Lexer::new(&source).tokenize().is_ok_and(|tokens| {
                    tokens.first().is_some_and(|t| matches!(t.token, Token::Function))
                });
                let code = if message.contains("not allowed in expression mode")
                    || function
                    || message.contains("not an expression: send")
                    || message.contains("not an expression: sh")
                    || message.contains("not an expression: for")
                    || message.contains("not an expression: while")
                    || message.contains("not an expression: loop")
                    || message.contains("not an expression: print") {
                    "binding-policy"
                } else {
                    "invalid-binding"
                };
                errors.push(Diagnostic::error(
                    code,
                    node.line,
                    format!("invalid binding on {path}: {message}"),
                ));
                continue;
            };
            let expr = parse_expression(&source).expect("static check accepted expression");
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
        set.diagnostics = errors;
        Ok(set)
    }
}

pub fn reevaluate(
    tree: &ResolvedScene,
    set: &BindingSet,
    path: &str,
    value: &JsonValue,
) -> Result<ReEval, Vec<Diagnostic>> {
    if serde_json::to_vec(value).map_or(true, |v| v.len() > crate::MAX_DOCUMENT_BYTES) {
        return Err(vec![Diagnostic::error("model-path", 1, "patch value too large")]);
    }
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
    // A sequence of individually small patches must not grow an unbounded
    // model before conversion/evaluation. The host also bounds authored ports
    // and metadata together with this model before committing a revision.
    if serde_json::to_vec(&next.model).map_or(true, |v| v.len() > crate::MAX_DOCUMENT_BYTES) {
        return Err(vec![Diagnostic::error("model-path", 1, "aggregate model too large")]);
    }
    let old = tree.clone();
    let mut diagnostics = Vec::new();
    let mut evaluated = Vec::new();
    let model = prepare_model(&next.model);
    let started = Instant::now();
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
        let Some((id, port)) = path.rsplit_once('.') else {
            continue;
        };
        let Some(node) = next.nodes.get_mut(id) else {
            continue;
        };
        let result = evaluate_budgeted(binding, &model, None, started, || evaluated.push(path.clone()))
            .and_then(|v| coerce_port(&v, crate::port_for(&node.family, port)));
        match result {
            Ok(v) => {
                set_port(&mut node.ports, port, v);
            }
            Err(code) => {
                diagnostics.push(Diagnostic::warning(
                    if code == "binding-type" { "binding-type" } else { "binding-eval" },
                    node.line,
                    format!("binding evaluation failed for {path}: {code}"),
                ));
            }
        }
    }
    let mut conflicts = Vec::new();
    for (id, node) in &next.nodes {
        crate::check_layout_bounds(id, node, &mut conflicts);
    }
    if !conflicts.is_empty() {
        return Err(conflicts);
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
    id: &str,
    node: &Node,
    set: &BindingSet,
    model: &JsonValue,
    item: &JsonValue,
) -> Result<Node, Diagnostic> {
    template_instantiate_with(id, node, set, item, &mut TemplateEvaluation::new(model))
}

/// One aggregate budget for a scene revision, shared by every list and row.
/// Kept outside renderer resources: Mix values are deliberately thread-local.
pub struct TemplateEvaluation {
    model: Value,
    started: Instant,
    remaining_nodes: usize,
}

pub const MAX_TEMPLATE_NODES: usize = 16_384;

impl TemplateEvaluation {
    pub fn new(model: &JsonValue) -> Self {
        let started = Instant::now();
        Self { model: prepare_model(model), started, remaining_nodes: MAX_TEMPLATE_NODES }
    }
}

/// Instantiate with a shared model conversion, elapsed-time and work budget.
pub fn template_instantiate_with(
    id: &str,
    node: &Node,
    set: &BindingSet,
    item: &JsonValue,
    evaluation: &mut TemplateEvaluation,
) -> Result<Node, Diagnostic> {
    if evaluation.remaining_nodes == 0 || evaluation.started.elapsed() >= EVALUATION_BUDGET {
        return Err(Diagnostic::warning("binding-eval", node.line, "template instantiation budget exhausted"));
    }
    evaluation.remaining_nodes -= 1;
    let mut out = node.clone();
    // Range by node instead of scanning every document binding for every cell.
    let prefix = format!("{id}.");
    for (path, binding) in set.bindings.range(prefix.clone()..) {
        if !path.starts_with(&prefix) { break; }
        let Some((binding_id, port)) = path.rsplit_once('.') else { continue };
        if binding_id != id { continue; }
        let value = evaluate_budgeted(binding, &evaluation.model, Some(item), evaluation.started, || {})
            .and_then(|v| coerce_port(&v, crate::port_for(&node.family, port)))
            .map_err(|code| {
                Diagnostic::warning(
                    if code == "binding-type" { "binding-type" } else { "binding-eval" },
                    node.line,
                    format!("template binding failed for {path}: {code}"),
                )
            })?;
        set_port(&mut out.ports, port, value);
    }
    let mut conflicts = Vec::new();
    crate::check_layout_bounds(id, &out, &mut conflicts);
    if let Some(conflict) = conflicts.into_iter().next() {
        return Err(conflict);
    }
    if evaluation.started.elapsed() >= EVALUATION_BUDGET {
        return Err(Diagnostic::warning("binding-eval", node.line, "template instantiation budget exhausted"));
    }
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

pub(crate) fn evaluate_budgeted(
    binding: &CompiledBinding,
    model: &Value,
    item: Option<&JsonValue>,
    started: Instant,
    on_evaluate: impl FnOnce(),
) -> Result<Value, String> {
    let remaining = EVALUATION_BUDGET.checked_sub(started.elapsed())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| "evaluation budget exhausted".to_string())?;
    on_evaluate();
    #[cfg(test)]
    EVALUATE_COUNT.with(|n| n.set(n.get() + 1));
    let mut globals = vec![("model", model.clone())];
    if let Some(item) = item {
        globals.push(("item", to_mix(item)));
    }
    let result = eval_expr_string(
        &binding.source,
        &globals,
        Some(Rc::new(CategoryAllowList::deny_all())),
        EvalLimits {
            max_string_len: Some(1 << 20),
            max_list_len: Some(MAX_ROWS),
            max_map_len: Some(1024),
            time_limit: Some(remaining.min(Duration::from_millis(50))),
            ..Default::default()
        },
    )
    .map_err(|e| e.to_string());
    // A final, non-yielding builtin may cross the shared deadline. Checking
    // only before the next binding would accept that value (or miss exhaustion
    // entirely on the last binding). Keep the old port instead.
    if started.elapsed() >= EVALUATION_BUDGET {
        return Err("evaluation budget exhausted".into());
    }
    result
}

pub(crate) fn coerce_port(value: &Value, port: Option<Port>) -> Result<Option<JsonValue>, String> {
    let Some(port) = port else {
        return Err("binding-type".into());
    };
    if matches!(value, Value::Nil) {
        return Ok(port.default.and_then(|x| serde_json::from_str(x).ok()).map(crate::normalize_number));
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
        Ok(Some(crate::normalize_number(j)))
    }
}

pub(crate) fn prepare_model(model: &JsonValue) -> Value {
    #[cfg(test)]
    MODEL_CONVERSION_COUNT.with(|n| n.set(n.get() + 1));
    to_mix(model)
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
    model: &Value,
    port: Port,
    started: Instant,
) -> Result<Option<JsonValue>, String> {
    let value = evaluate_budgeted(binding, model, None, started, || {})?;
    coerce_port(&value, Some(port))
}

pub(crate) fn set_port(ports: &mut indexmap::IndexMap<String, JsonValue>, port: &str, value: Option<JsonValue>) {
    if let Some(value) = value {
        ports.insert(port.into(), value);
    } else {
        ports.shift_remove(port);
    }
}

fn apply_model_patch(model: &mut JsonValue, parts: &[&str], value: &JsonValue) -> bool {
    if parts.is_empty() {
        if value.is_null() {
            *model = json!({});
            return true;
        }
        if !value.is_object() { return false; }
        *model = value.clone();
        return true;
    }
    if parts.iter().any(|p| p.is_empty()) {
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
        if value.is_null() && !map.contains_key(*part) {
            return true;
        }
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
pub(crate) fn template_ids(doc: &SceneDocument) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    fn visit(id: &str, doc: &SceneDocument, ids: &mut BTreeSet<String>) {
        if !ids.insert(id.into()) {
            return;
        }
        if let Some(node) = doc.nodes.get(id) {
            if node.widget == "list" && let Some(row) = node.ports.get("row").and_then(JsonValue::as_str) {
                visit(row, doc, ids);
            }
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
        Expr::InterpolatedString(parts) | Expr::Heredoc(parts) => {
            for part in parts {
                match part {
                    StringPart::Literal(_) => {}
                    StringPart::EnvVar(_) | StringPart::CommandSub(_) => *bad_root = true,
                    StringPart::Variable(spec) => {
                        // lib-mix's split_interp_coalesce is private. The first
                        // ?? / ?: separates the dotted head from a Mix payload.
                        let split = spec.as_bytes().windows(2).position(|w| w == b"??" || w == b"?:");
                        let head = split.map_or(spec.as_str(), |i| spec[..i].trim());
                        match head.split('.').next() {
                            Some("model") => { deps.insert(head.into()); }
                            Some("item") => *reads_item = true,
                            _ => *bad_root = true,
                        }
                        if let Some(i) = split {
                            let payload = spec[i + 2..].trim();
                            if !payload.is_empty() {
                                let expr = parse_expression(payload).expect("static check accepted interpolation");
                                walk_expr(&expr, deps, reads_item, bad_root, nondeterministic);
                            }
                        }
                    }
                }
            }
        }
        Expr::CommandSub(_) => *bad_root = true,
        Expr::NumberLiteral(_) | Expr::StringLiteral(_) | Expr::EscapedQuoteStringLiteral(_)
        | Expr::BoolLiteral(_) | Expr::NilLiteral => {}
    }
}
fn walk_stmt(
    s: &cosmix_mix::ast::Stmt,
    d: &mut BTreeSet<String>,
    i: &mut bool,
    b: &mut bool,
    n: &mut bool,
) {
    match &s.kind {
        StmtKind::Expression(e) | StmtKind::Assignment { value: e, .. }
        | StmtKind::FieldAssignment { value: e, .. } | StmtKind::Die(e)
        | StmtKind::Parse { source: e, .. } | StmtKind::BreakIf(e, _)
        | StmtKind::ContinueIf(e, _) => walk_expr(e, d, i, b, n),
        StmtKind::IndexAssignment { index, value, .. } => {
            walk_expr(index, d, i, b, n);
            walk_expr(value, d, i, b, n);
        }
        StmtKind::PathAssignment { path, value, .. } => {
            for seg in path {
                if let cosmix_mix::ast::PathSeg::Index(e) = seg {
                    walk_expr(e, d, i, b, n);
                }
            }
            walk_expr(value, d, i, b, n);
        }
        StmtKind::If { condition, then_body, else_ifs, else_body } => {
            walk_expr(condition, d, i, b, n);
            for s in then_body { walk_stmt(s, d, i, b, n); }
            for (condition, body) in else_ifs {
                walk_expr(condition, d, i, b, n);
                for s in body { walk_stmt(s, d, i, b, n); }
            }
            if let Some(body) = else_body {
                for s in body { walk_stmt(s, d, i, b, n); }
            }
        }
        StmtKind::Return(value) => {
            if let Some(e) = value { walk_expr(e, d, i, b, n); }
        }
        StmtKind::TryCatch { try_body, catch, finally_body } => {
            for s in try_body { walk_stmt(s, d, i, b, n); }
            if let Some(catch) = catch {
                for s in &catch.body { walk_stmt(s, d, i, b, n); }
            }
            if let Some(body) = finally_body {
                for s in body { walk_stmt(s, d, i, b, n); }
            }
        }
        StmtKind::Alias { name, command } => {
            for e in name.iter().chain(command.iter()) { walk_expr(e, d, i, b, n); }
        }
        StmtKind::Chain { left, right, .. } => {
            walk_stmt(left, d, i, b, n);
            walk_stmt(right, d, i, b, n);
        }
        StmtKind::Break(_) | StmtKind::Continue(_) => {}
        // These are rejected by expr_mode_check before this walk.
        StmtKind::FunctionDef { .. } | StmtKind::Send { .. } | StmtKind::Emit { .. }
        | StmtKind::Sh { .. } | StmtKind::On { .. } | StmtKind::Source { .. }
        | StmtKind::Include { .. } | StmtKind::PipeToExternal { .. }
        | StmtKind::For { .. } | StmtKind::ForEach { .. } | StmtKind::While { .. }
        | StmtKind::Loop { .. } | StmtKind::Select { .. } | StmtKind::Address { .. }
        | StmtKind::Export { .. } | StmtKind::Print { .. } => *b = true,
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
        Expr::Variable(v) if v == "model" || v == "item" => {}
        _ => walk_expr(expr, d, i, b, n),
    }
}
fn access_path(expr: &Expr, root: &str) -> Option<String> {
    match expr {
        Expr::Variable(v) if v == root => Some(root.into()),
        Expr::FieldAccess { object, field } => {
            access_path(object, root).map(|p| {
                if contains_index(object) { p } else { format!("{p}.{field}") }
            })
        }
        Expr::Index { object, .. } => access_path(object, root),
        _ => None,
    }
}
fn contains_index(expr: &Expr) -> bool {
    match expr {
        Expr::Index { .. } => true,
        Expr::FieldAccess { object, .. } => contains_index(object),
        _ => false,
    }
}

#[cfg(test)]
mod template_budget_tests {
    use super::*;

    #[test]
    fn all_rows_share_work_and_time_limits_and_one_model_conversion() {
        let doc = crate::parse("---\nscene: 1\nname: budget\ncitizen: test\nmodel: {\"prefix\":\"live \"}\n---\n```mix\nroot: {widget: \"list\", rows: [], row: \"t\", row_height: 20}\nt: {widget: \"text\", text: \"= $model.prefix .. $item.cells[0]\"}\n```\n").unwrap();
        let tree = crate::resolve(&doc).unwrap();
        let set = compile(&doc).unwrap();
        MODEL_CONVERSION_COUNT.with(|n| n.set(0));
        let mut evaluation = TemplateEvaluation::new(&tree.model);
        evaluation.remaining_nodes = 2;
        for value in ["one", "two"] {
            let node = template_instantiate_with("t", &tree.nodes["t"], &set,
                &json!({"id":value,"cells":[value]}), &mut evaluation).unwrap();
            assert_eq!(node.ports["text"], json!(format!("live {value}")));
        }
        assert_eq!(MODEL_CONVERSION_COUNT.with(|n| n.get()), 1);
        assert!(template_instantiate_with("t", &tree.nodes["t"], &set,
            &json!({"cells":["third"]}), &mut evaluation).is_err());
        let mut expired = TemplateEvaluation::new(&tree.model);
        expired.started = Instant::now() - EVALUATION_BUDGET;
        assert!(template_instantiate_with("t", &tree.nodes["t"], &set,
            &json!({"cells":["late"]}), &mut expired).is_err());
    }
}
