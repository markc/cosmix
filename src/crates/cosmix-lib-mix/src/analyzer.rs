//! Semantic analyzer — the engine behind `mix lint` (0.29.0, decision
//! record D3).
//!
//! Sits between parse and evaluate, usable by embedders independently
//! of the CLI. Diagnoses the defect classes that `mix --check` (syntax
//! only) let through in the CT-provisioning worker: definitely
//! undefined variables, undefined callables, builtin/user-function
//! arity mismatches, duplicate parameters/definitions, unreachable
//! statements, discarded must-use results, and statically resolvable
//! `require()` failures — plus a capability inventory of the script.
//!
//! Design bias: **false positives near zero**. Mix is dynamic
//! (`source`/`include` load code at runtime, `${...}` interpolation
//! falls back to the process environment, blocks don't scope, bareword
//! calls can resolve to function-valued variables), so every rule is
//! deliberately conservative:
//!
//! - A variable read is flagged ONLY when the name is assigned nowhere
//!   in its visible universe (function body + file top level for
//!   function code; the whole file for top-level code) — lexical order
//!   is NOT considered, matching "no block scoping, globals visible
//!   from functions".
//! - `${name}` interpolation is never flagged (env fallback).
//! - All-digit names (`$1`...) and the runtime-injected `rc`/`result`/
//!   `status`/`event`/`_` are always declared.
//! - A `source`/`include` anywhere suppresses the undefined checks
//!   entirely (still reported once as MIX-W2401) — the loaded file can
//!   define anything.
//! - A bareword call whose name matches ANY assigned variable is not
//!   flagged (function-valued-variable dispatch).
//! - Calls inside `address ... end` blocks are sends, never undefined.
//! - `MethodCall`/`ValueCall` are dynamic dispatch — skipped.

use std::collections::{HashMap, HashSet};

use crate::ast::{BinOp, ChainOp, Expr, FunctionBody, Param, PathSeg, Stmt, StmtKind, UnaryOp};
use crate::builtin_info::{FieldInfo, TypeShape};
use crate::builtins;
use crate::evaluator::INLINE_SPECIAL_FORMS;
use crate::scope::param_arity;
use crate::token::StringPart;

/// Diagnostic severity — the D3 wire values are lowercase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    /// Advisory (0.63.0): rendered and counted separately, NEVER gates —
    /// `--deny-warnings` ignores notes. The severity a deprecation is
    /// born at; promotion to `Warning` keeps the code, only the
    /// severity moves.
    Note,
}

impl Severity {
    pub fn wire_name(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Note => "note",
        }
    }
}

/// One lint finding. `code` is a permanent identifier — never reused,
/// never semantically repurposed — from three namespaces:
///   MIX-E1xxx  errors (the letter encodes the fixed severity)
///   MIX-W2xxx  warnings that were BORN warnings
///   MIX-D3xxx  deprecations and release-transition advisories — a
///              severity-INDEPENDENT namespace: a deprecation's
///              severity is by design not fixed (it starts as `note`
///              and may be promoted to `warning` in a later release
///              with the code unchanged), so its letter must not
///              claim one.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub code: &'static str,
    pub severity: Severity,
    pub file: Option<String>,
    /// 1-based statement line; `None` when unknown.
    pub line: Option<usize>,
    /// Always `None` today (statements carry line precision only).
    pub column: Option<usize>,
    pub message: String,
    pub hint: Option<String>,
}

/// Analyzer inputs beyond the AST.
#[derive(Debug, Clone, Default)]
pub struct AnalyzerConfig {
    /// Names declared external (`--allow-global NAME`).
    pub allow_globals: Vec<String>,
    /// Callables declared external (`--allow-function NAME`), e.g.
    /// embedder extensions.
    pub allow_functions: Vec<String>,
}

/// The result of one file's analysis.
#[derive(Debug, Clone, Default)]
pub struct Analysis {
    pub diagnostics: Vec<Diagnostic>,
    /// Kebab capability classes the script's builtin calls exercise
    /// (plus "process" for shell constructs and "bus" for messaging) —
    /// reported as data, not warnings (D3).
    pub capabilities: Vec<&'static str>,
}

/// Runtime-injected variable names a lint must treat as declared.
const INJECTED_VARS: &[&str] = &["rc", "result", "status", "event", "_"];

/// Function names defined by the embedded prelude (parsed once).
pub fn prelude_function_names() -> &'static HashSet<String> {
    use std::sync::OnceLock;
    static NAMES: OnceLock<HashSet<String>> = OnceLock::new();
    NAMES.get_or_init(|| {
        let src = include_str!("../std/prelude.mix");
        let mut out = HashSet::new();
        if let Ok(tokens) = crate::lexer::Lexer::new(src).tokenize()
            && let Ok(stmts) = crate::parser::Parser::new(tokens, src).parse_program()
        {
            collect_function_defs(&stmts, &mut out);
        }
        out
    })
}

/// Recursively collect every `FunctionDef` name (any nesting depth —
/// definitions execute wherever control flow reaches them, and lint
/// does not model reachability).
fn collect_function_defs(stmts: &[Stmt], out: &mut HashSet<String>) {
    walk_stmts(stmts, &mut |stmt| {
        if let StmtKind::FunctionDef { name, .. } = &stmt.kind {
            out.insert(name.clone());
        }
    });
}

/// Generic statement walker: visits every statement at every nesting
/// depth: statement-kind bodies (if/loops/try/on/address/select) AND
/// statement lists embedded in this statement's EXPRESSIONS
/// (if-expression branches, block-lambda bodies, parameter defaults).
/// The fact-gathering passes (definitions, includes, capabilities,
/// binders) must see every executable statement, wherever it hides.
fn walk_stmts(stmts: &[Stmt], visit: &mut dyn FnMut(&Stmt)) {
    for stmt in stmts {
        visit(stmt);
        for body in stmt_bodies(&stmt.kind) {
            walk_stmts(body, visit);
        }
        // Fact-gathering passes (defs, includes, capabilities) want
        // EVERY executable statement, so descend into lambda bodies too.
        walk_stmt_exprs(stmt, &mut |expr| {
            for_each_embedded_stmt_list(expr, true, &mut |body| walk_stmts(body, visit));
        });
    }
}

/// Visit every statement list embedded inside an expression TREE —
/// `if`-expression branches and, when `into_lambdas`, block-lambda
/// bodies + lambda parameter-default statements. `if`-branch
/// statements run in the CURRENT scope (always visited); lambda bodies
/// run in a fresh function frame, so the *variable-binding* universe
/// pass excludes them (`into_lambdas=false`) while the fact-gathering
/// passes include them (codex convergence review, MAJOR: lambda-local
/// bindings must not leak into the file universe).
fn for_each_embedded_stmt_list(expr: &Expr, into_lambdas: bool, visit: &mut dyn FnMut(&[Stmt])) {
    match expr {
        Expr::If(ifexpr) => {
            for_each_embedded_stmt_list(&ifexpr.condition, into_lambdas, visit);
            visit(&ifexpr.then_body);
            for (c, b) in &ifexpr.else_ifs {
                for_each_embedded_stmt_list(c, into_lambdas, visit);
                visit(b);
            }
            if let Some(b) = &ifexpr.else_body {
                visit(b);
            }
        }
        Expr::FunctionLiteral { params, body } => {
            if !into_lambdas {
                return;
            }
            for p in params {
                if let Some(d) = &p.default {
                    for_each_embedded_stmt_list(d, into_lambdas, visit);
                }
            }
            match &**body {
                FunctionBody::Block(stmts) => visit(stmts),
                FunctionBody::Expression(e) => for_each_embedded_stmt_list(e, into_lambdas, visit),
            }
        }
        // walk_expr_children skips FunctionLiteral bodies and Expr::If —
        // both handled above — so this reaches the rest of the tree
        // without double-visiting.
        _ => walk_expr_children(expr, &mut |c| {
            for_each_embedded_stmt_list(c, into_lambdas, visit)
        }),
    }
}

/// Every nested statement list of a statement kind.
fn stmt_bodies(kind: &StmtKind) -> Vec<&[Stmt]> {
    match kind {
        StmtKind::If {
            then_body,
            else_ifs,
            else_body,
            ..
        } => {
            let mut out: Vec<&[Stmt]> = vec![then_body];
            for (_, b) in else_ifs {
                out.push(b);
            }
            if let Some(b) = else_body {
                out.push(b);
            }
            out
        }
        StmtKind::For { body, .. }
        | StmtKind::ForEach { body, .. }
        | StmtKind::While { body, .. }
        | StmtKind::Loop { body, .. }
        | StmtKind::On { body, .. }
        | StmtKind::Address { body, .. } => vec![body],
        StmtKind::TryCatch {
            try_body,
            catch,
            finally_body,
        } => {
            let mut out: Vec<&[Stmt]> = vec![try_body];
            if let Some(c) = catch {
                out.push(&c.body);
            }
            if let Some(f) = finally_body {
                out.push(f);
            }
            out
        }
        StmtKind::Select {
            cases, otherwise, ..
        } => {
            let mut out: Vec<&[Stmt]> = cases.iter().map(|(_, body)| body.as_slice()).collect();
            if let Some(b) = otherwise {
                out.push(b);
            }
            out
        }
        // FunctionDef bodies are handled by the function-scope pass;
        // walk them here too so nested defs/binders are discovered by
        // universe collection (callers that must not descend filter on
        // the visit side).
        StmtKind::FunctionDef { body, .. } => match body {
            FunctionBody::Block(stmts) => vec![stmts],
            FunctionBody::Expression(_) => vec![],
        },
        _ => vec![],
    }
}

/// Collect every name a statement list can BIND (assignments, loop
/// vars, catch vars, parse captures, exports) at any depth, including
/// inside nested function definitions when `into_functions` is true.
fn collect_bound_names(stmts: &[Stmt], into_functions: bool, out: &mut HashSet<String>) {
    for stmt in stmts {
        match &stmt.kind {
            StmtKind::Assignment { name, .. }
            | StmtKind::Export { name, .. }
            | StmtKind::FieldAssignment { object: name, .. }
            | StmtKind::IndexAssignment { object: name, .. }
            | StmtKind::PathAssignment { root: name, .. } => {
                // Field/index assignment requires the object to exist at
                // runtime, but "assigned anywhere" is the universe rule;
                // treating the object as bound keeps `$m.x = 1` after a
                // dynamic construction FP-free. The read-side check still
                // catches wholly-unknown names used in expressions.
                out.insert(name.clone());
            }
            StmtKind::For { var, .. } => {
                out.insert(var.clone());
            }
            StmtKind::ForEach { var, index_var, .. } => {
                out.insert(var.clone());
                if let Some(iv) = index_var {
                    out.insert(iv.clone());
                }
            }
            StmtKind::TryCatch { catch: Some(c), .. } => {
                out.insert(c.var.clone());
                if let Some(ev) = &c.err_var {
                    out.insert(ev.clone());
                }
            }
            StmtKind::Parse { parts, .. } => {
                for part in parts {
                    if let crate::ast::ParsePart::Variable(name) = part {
                        out.insert(name.clone());
                    }
                }
            }
            StmtKind::FunctionDef { .. } if !into_functions => continue,
            _ => {}
        }
        for body in stmt_bodies(&stmt.kind) {
            if matches!(stmt.kind, StmtKind::FunctionDef { .. }) && !into_functions {
                continue;
            }
            collect_bound_names(body, into_functions, out);
        }
        // A top-level `$x = if cond then $y = 1 ... end` binds $x AND
        // (in the taken branch) $y — if-branches run in the CURRENT
        // scope, so their bindings join this universe. Lambda bodies do
        // NOT (a fresh frame — `into_lambdas=false`), so a lambda-local
        // binding never masks a top-level undefined read (codex
        // convergence review, MAJOR).
        if into_functions || !matches!(stmt.kind, StmtKind::FunctionDef { .. }) {
            walk_stmt_exprs(stmt, &mut |expr| {
                for_each_embedded_stmt_list(expr, false, &mut |body| {
                    collect_bound_names(body, into_functions, out)
                });
            });
        }
    }
}

/// True when any `source`/`include` statement exists (a dynamic code
/// barrier: the loaded file can define arbitrary globals/functions).
fn has_dynamic_include(stmts: &[Stmt]) -> (bool, Option<usize>) {
    let mut found = None;
    walk_stmts(stmts, &mut |stmt| {
        if found.is_none()
            && matches!(
                stmt.kind,
                StmtKind::Source { .. } | StmtKind::Include { .. }
            )
        {
            found = Some(stmt.line);
        }
    });
    (found.is_some(), found)
}

/// The whole-file analyzer entry point.
pub fn analyze(stmts: &[Stmt], file: Option<&str>, cfg: &AnalyzerConfig) -> Analysis {
    let mut a = Analysis::default();
    let ctx = FileContext::build(stmts, file, cfg);

    // W2401 + undefined-check suppression on dynamic includes.
    if let (true, line) = has_dynamic_include(stmts) {
        a.diagnostics.push(Diagnostic {
            code: "MIX-W2401",
            severity: Severity::Warning,
            file: ctx.file.clone(),
            line,
            column: None,
            message: "source/include loads code at runtime — undefined-name analysis disabled"
                .to_string(),
            hint: Some("prefer require() for statically analyzable modules".to_string()),
        });
    }

    check_duplicates_and_arity_defs(stmts, &ctx, &mut a);
    check_unreachable(stmts, &ctx, &mut a);
    check_requires(stmts, &ctx, &mut a);
    check_scope(
        stmts,
        &ctx,
        &mut a,
        &ctx.top_level_names,
        /* in_address */ false,
        // A script's FINAL statement is its value: `cosmix_mix::run`
        // returns it, and embedders (webd handlers) end a file with
        // `merge($base, $extra)` to return the merged map. The CLI
        // discards it, so a trailing dead mutation there goes unflagged —
        // a deliberate false NEGATIVE, taken because the alternative is a
        // false POSITIVE on an error-severity rule, and this analyzer's
        // stated bias is false-positives-near-zero.
        /* block_is_value */
        true,
    );
    check_recurring_silent_bugs(stmts, &ctx, &mut a);
    check_release_transition_advisories(stmts, &ctx, &mut a);
    collect_capabilities(stmts, &mut a);
    a
}

/// Immutable per-file facts shared by the passes.
struct FileContext {
    file: Option<String>,
    /// Every name assigned/bound anywhere at any depth (the "assigned
    /// anywhere" universe base for top-level code) plus injected +
    /// allow-listed names.
    top_level_names: HashSet<String>,
    /// User function names defined anywhere + prelude + allow-listed.
    known_callables: HashSet<String>,
    /// name → (min, max) for names with exactly ONE definition.
    user_fn_arity: HashMap<String, (usize, usize)>,
    /// Undefined-name checks suppressed (dynamic include present).
    dynamic: bool,
}

impl FileContext {
    fn build(stmts: &[Stmt], file: Option<&str>, cfg: &AnalyzerConfig) -> FileContext {
        let mut top_level_names = HashSet::new();
        // The TOP-LEVEL bound universe: blocks don't scope and
        // definition order is runtime order (no read-before-assign
        // rule), so any binding in the top-level scope chain — incl.
        // if-expression branches, which run in the current scope —
        // makes the name plausible. Function/lambda bodies run in
        // isolated frames and CANNOT write a top-level name, so their
        // local bindings are excluded (`into_functions=false`); a
        // top-level read of a function-local name is a real nil-read
        // bug and is flagged (codex convergence review, MAJOR). Each
        // function/lambda body gets its OWN universe in the scope pass.
        collect_bound_names(stmts, false, &mut top_level_names);
        for v in INJECTED_VARS {
            top_level_names.insert((*v).to_string());
        }
        for g in &cfg.allow_globals {
            top_level_names.insert(g.clone());
        }

        let mut defs = HashSet::new();
        collect_function_defs(stmts, &mut defs);
        let mut fn_arity: HashMap<String, Vec<(usize, usize)>> = HashMap::new();
        walk_stmts(stmts, &mut |stmt| {
            if let StmtKind::FunctionDef { name, params, .. } = &stmt.kind {
                fn_arity
                    .entry(name.clone())
                    .or_default()
                    .push(param_arity(params));
            }
        });
        let user_fn_arity = fn_arity
            .into_iter()
            .filter(|(_, v)| v.len() == 1)
            .map(|(k, v)| (k, v[0]))
            .collect();

        let mut known_callables: HashSet<String> = defs;
        known_callables.extend(prelude_function_names().iter().cloned());
        known_callables.extend(cfg.allow_functions.iter().cloned());

        let (dynamic, _) = has_dynamic_include(stmts);
        FileContext {
            file: file.map(str::to_string),
            top_level_names,
            known_callables,
            user_fn_arity,
            dynamic,
        }
    }
}

fn is_positional(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_digit())
}

/// Bare `$name` spellings retained inside a heredoc literal part. `${`
/// and `$(` have already been split into non-literal parts by the lexer,
/// but keep those exclusions explicit here so this check owns its full
/// syntax contract.
fn bare_heredoc_vars(literal: &str) -> Vec<&str> {
    let bytes = literal.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'$'
            || i + 1 == bytes.len()
            || matches!(bytes[i + 1], b'{' | b'(')
            || !(bytes[i + 1].is_ascii_alphanumeric() || bytes[i + 1] == b'_')
        {
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut end = start + 1;
        while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
            end += 1;
        }
        let name = &literal[start..end];
        if !is_positional(name) {
            out.push(name);
        }
        i = end;
    }
    out
}

fn diag(
    ctx: &FileContext,
    code: &'static str,
    severity: Severity,
    line: usize,
    message: String,
    hint: Option<String>,
) -> Diagnostic {
    Diagnostic {
        code,
        severity,
        file: ctx.file.clone(),
        line: (line > 0).then_some(line),
        column: None,
        message,
        hint,
    }
}

// ── E1301 / E1302 ────────────────────────────────────────────────────

fn check_duplicates_and_arity_defs(stmts: &[Stmt], ctx: &FileContext, a: &mut Analysis) {
    // Duplicate params on every function definition + lambda.
    walk_stmts(stmts, &mut |stmt| {
        if let StmtKind::FunctionDef { name, params, .. } = &stmt.kind {
            check_dup_params(name, params, stmt.line, ctx, a);
        }
        walk_stmt_exprs(stmt, &mut |expr| {
            if let Expr::FunctionLiteral { params, .. } = expr {
                check_dup_params("<lambda>", params, stmt.line, ctx, a);
            }
        });
    });
    // Duplicate definitions within ONE statement list (same scope).
    fn per_block(stmts: &[Stmt], ctx: &FileContext, a: &mut Analysis) {
        let mut seen: HashMap<&str, usize> = HashMap::new();
        for stmt in stmts {
            if let StmtKind::FunctionDef { name, .. } = &stmt.kind {
                if let Some(first) = seen.get(name.as_str()) {
                    a.diagnostics.push(diag(
                        ctx,
                        "MIX-E1302",
                        Severity::Error,
                        stmt.line,
                        format!("duplicate definition of function '{name}' (first defined at line {first})"),
                        Some("the later definition silently replaces the earlier one".to_string()),
                    ));
                } else {
                    seen.insert(name.as_str(), stmt.line);
                }
            }
            for body in stmt_bodies(&stmt.kind) {
                per_block(body, ctx, a);
            }
            // Statement lists embedded in expressions (if-expression
            // branches, lambda bodies) are their own scopes — check
            // each for duplicate definitions (codex convergence review).
            walk_stmt_exprs(stmt, &mut |expr| {
                for_each_embedded_stmt_list(expr, true, &mut |body| per_block(body, ctx, a));
            });
        }
    }
    per_block(stmts, ctx, a);
}

fn check_dup_params(
    name: &str,
    params: &[Param],
    line: usize,
    ctx: &FileContext,
    a: &mut Analysis,
) {
    let mut seen = HashSet::new();
    for p in params {
        if !seen.insert(p.name.as_str()) {
            a.diagnostics.push(diag(
                ctx,
                "MIX-E1301",
                Severity::Error,
                line,
                format!("duplicate parameter '${}' in {}", p.name, name),
                None,
            ));
        }
    }
}

// ── W2101 unreachable ────────────────────────────────────────────────

fn terminates_block(kind: &StmtKind) -> bool {
    match kind {
        StmtKind::Return(_) | StmtKind::Die(_) | StmtKind::Break(_) | StmtKind::Continue(_) => true,
        StmtKind::Expression(Expr::FunctionCall { name, args: _ }) => {
            name == "exit" || name == "panic"
        }
        _ => false,
    }
}

fn check_unreachable(stmts: &[Stmt], ctx: &FileContext, a: &mut Analysis) {
    fn per_block(stmts: &[Stmt], ctx: &FileContext, a: &mut Analysis) {
        let mut dead_after: Option<usize> = None;
        for stmt in stmts {
            if let Some(term_line) = dead_after {
                a.diagnostics.push(diag(
                    ctx,
                    "MIX-W2101",
                    Severity::Warning,
                    stmt.line,
                    format!("unreachable statement (control flow ends at line {term_line})"),
                    None,
                ));
                break; // one finding per block is enough
            }
            if terminates_block(&stmt.kind) {
                dead_after = Some(stmt.line);
            }
            for body in stmt_bodies(&stmt.kind) {
                per_block(body, ctx, a);
            }
            // Unreachable code inside if-expression branches / lambda
            // bodies — each is its own control-flow block (codex
            // convergence review).
            walk_stmt_exprs(stmt, &mut |expr| {
                for_each_embedded_stmt_list(expr, true, &mut |body| per_block(body, ctx, a));
            });
        }
    }
    per_block(stmts, ctx, a);
}

// ── E1401 / E1402 require() ──────────────────────────────────────────

fn check_requires(stmts: &[Stmt], ctx: &FileContext, a: &mut Analysis) {
    let base_dir = ctx
        .file
        .as_deref()
        .and_then(|f| std::path::Path::new(f).parent().map(|p| p.to_path_buf()));
    walk_stmts(stmts, &mut |stmt| {
        walk_stmt_exprs(stmt, &mut |expr| {
            let Expr::FunctionCall { name, args } = expr else {
                return;
            };
            if name != "require" {
                return;
            }
            let Some(Expr::StringLiteral(path)) = args.first() else {
                return; // dynamic path — out of scope for static checks
            };
            let p = std::path::Path::new(path);
            let resolved = if p.is_absolute() {
                p.to_path_buf()
            } else if let Some(dir) = &base_dir {
                dir.join(p)
            } else {
                p.to_path_buf()
            };
            if !resolved.is_file() {
                a.diagnostics.push(diag(
                    ctx,
                    "MIX-E1401",
                    Severity::Error,
                    stmt.line,
                    format!(
                        "require: module '{path}' not found (resolved: {})",
                        resolved.display()
                    ),
                    None,
                ));
                return;
            }
            match std::fs::read_to_string(&resolved) {
                Err(e) => {
                    a.diagnostics.push(diag(
                        ctx,
                        "MIX-E1401",
                        Severity::Error,
                        stmt.line,
                        format!("require: module '{path}' unreadable: {e}"),
                        None,
                    ));
                }
                Ok(src) => {
                    let parsed = crate::lexer::Lexer::new(&src)
                        .tokenize()
                        .and_then(|t| crate::parser::Parser::new(t, &src).parse_program());
                    if let Err(e) = parsed {
                        a.diagnostics.push(diag(
                            ctx,
                            "MIX-E1402",
                            Severity::Error,
                            stmt.line,
                            format!("require: module '{path}' is invalid: {e}"),
                            None,
                        ));
                    }
                }
            }
        });
    });
}

// ── scope pass: E1101 / E1102 / E1201 / E1202 / W2201 ───────────────

/// Does this statement hand its body's value onward as its own?
///
/// `if`/`select` yield the taken branch's value; `try`/`catch` yield the
/// body's; an `address` block yields its last statement's. Loops do not
/// (`for`/`while`/`loop` run their body N times and yield nothing an
/// expression can consume). Used to decide whether a trailing dead
/// mutation inside such a body is really dead.
fn stmt_propagates_value(kind: &StmtKind) -> bool {
    matches!(
        kind,
        StmtKind::If { .. }
            | StmtKind::Select { .. }
            | StmtKind::TryCatch { .. }
            | StmtKind::Address { .. }
    )
}

fn check_scope(
    stmts: &[Stmt],
    ctx: &FileContext,
    a: &mut Analysis,
    names: &HashSet<String>,
    in_address: bool,
    // Does this block's LAST statement supply a value someone consumes?
    // True only for `if`-EXPRESSION branches. Function and lambda block
    // bodies return nil unless they `return` explicitly (verified against
    // the binary), and statement bodies (if/for/while/try) are not values
    // at all — so in those, a trailing dead mutation is just dead.
    block_is_value: bool,
) {
    for (idx, stmt) in stmts.iter().enumerate() {
        let last_in_block = idx + 1 == stmts.len();
        // A trailing statement can only be "the block's value" when the
        // block actually has one.
        let result_consumed = last_in_block && block_is_value;
        // W2201: a discarded must-use operation as a bare expression
        // statement (skip the last statement of a block — it may be the
        // block's value).
        if let StmtKind::Expression(Expr::FunctionCall { name, .. }) = &stmt.kind
            && !last_in_block
            && let Some(info) = builtins::builtin_info_of(name)
            && info.contract.effects.must_use
        {
            a.diagnostics.push(diag(
                ctx,
                "MIX-W2201",
                Severity::Warning,
                stmt.line,
                format!(
                    "result of {name}() is discarded — its failure signal is in the returned value"
                ),
                Some(format!(
                    "bind it: $r = {name}(...) and branch on the result"
                )),
            ));
        }

        // E1501 / E1502: a statement whose whole effect is provably lost.
        // Both are ERRORS, not warnings: unlike a discarded must-use
        // result (which merely drops a failure signal), these do nothing
        // at all, and the script reads as though they did.
        if let StmtKind::Expression(Expr::FunctionCall { name, args }) = &stmt.kind
            && !result_consumed
        {
            match name.as_str() {
                // push/pop/shift mutate a list IN PLACE, and can only
                // reach the caller's list through a bare variable slot.
                // Given any other first argument — `push($m[$k], $v)`,
                // `push($m.a, $v)` — they mutate a temporary copy and the
                // write is lost. (A by-value PARAMETER is a bare variable,
                // so it stays with the 0.21.9 dead-push diagnostic and is
                // not double-reported here.)
                "push" | "pop" | "shift"
                    if args
                        .first()
                        .is_some_and(|a| !matches!(a, Expr::Variable(_))) =>
                {
                    // The remedy differs by builtin, and getting it wrong
                    // CORRUPTS data: `push` returns the appended list, so
                    // it can be assigned back — but `pop`/`shift` return
                    // the removed ELEMENT, so `$m[$k] = pop($m[$k])` would
                    // replace the list with that element.
                    let hint = if name == "push" {
                        "assign the result back: $m[$k] = push($m[$k], ...) — push returns the \
                         appended list when its first argument is not a variable"
                            .to_string()
                    } else {
                        format!(
                            "{name}() returns the REMOVED ELEMENT, not the list — do not assign it \
                             back over the list. Hoist first: $l = $m[$k]; $x = {name}($l); \
                             $m[$k] = $l"
                        )
                    };
                    a.diagnostics.push(diag(
                        ctx,
                        "MIX-E1501",
                        Severity::Error,
                        stmt.line,
                        format!(
                            "{name}() here mutates a temporary copy — the write is lost. It can \
                             only mutate a list held in a bare variable."
                        ),
                        Some(hint),
                    ));
                }
                // Pure transforms: they RETURN the new container and
                // change nothing in place, so a bare call is a no-op.
                "delete" | "merge" => {
                    a.diagnostics.push(diag(
                        ctx,
                        "MIX-E1502",
                        Severity::Error,
                        stmt.line,
                        format!(
                            "{name}() does not mutate — it returns a new value, and this result is \
                             discarded, so the statement does nothing"
                        ),
                        Some(format!("assign it back: $m = {name}($m, ...)")),
                    ));
                }
                _ => {}
            }
        }

        // Expression-level checks for THIS statement — full tree, so a
        // nested `$typo` inside a concatenation is seen (lambda bodies
        // excepted; check_expr gives those their own universe).
        walk_stmt_exprs(stmt, &mut |expr| {
            check_expr_tree(expr, stmt.line, ctx, a, names, in_address);
        });

        // Recurse into bodies. Function definitions get their own
        // universe (params + body binders + whole-file universe);
        // address blocks suppress unknown-callable checks.
        match &stmt.kind {
            StmtKind::FunctionDef { params, body, .. } => {
                let mut fn_names: HashSet<String> = names.clone();
                for p in params {
                    fn_names.insert(p.name.clone());
                }
                // Param defaults evaluate in the callee frame (params +
                // file universe visible) — scope-check them.
                for p in params {
                    if let Some(d) = &p.default {
                        check_expr_tree(d, stmt.line, ctx, a, &fn_names, in_address);
                    }
                }
                match body {
                    FunctionBody::Block(body_stmts) => {
                        collect_bound_names(body_stmts, true, &mut fn_names);
                        check_scope(body_stmts, ctx, a, &fn_names, in_address, false);
                    }
                    FunctionBody::Expression(e) => {
                        let line = stmt.line;
                        check_expr_tree(e, line, ctx, a, &fn_names, in_address);
                    }
                }
            }
            StmtKind::Address { body, .. } => {
                check_scope(body, ctx, a, names, true, result_consumed);
            }
            _ => {
                // A statement that PROPAGATES its body's value is only a
                // value itself when its own result is consumed — so the
                // exemption has to travel down with it. Without this,
                // `$r = if c then (if c2 then delete($m,"k") else $m end)
                // else $m end` false-positives on the inner branch, whose
                // value really does become `$r`. Loop bodies (and a
                // `finally`) are never values; over-including `finally`
                // here would only cost a missed diagnostic, never a false
                // one.
                let body_is_value = result_consumed && stmt_propagates_value(&stmt.kind);
                for body in stmt_bodies(&stmt.kind) {
                    check_scope(body, ctx, a, names, in_address, body_is_value);
                }
            }
        }
    }
}

/// Expression checks that need the scope universe: variable reads,
/// callable resolution, arity.
fn check_expr(
    expr: &Expr,
    line: usize,
    ctx: &FileContext,
    a: &mut Analysis,
    names: &HashSet<String>,
    in_address: bool,
) {
    match expr {
        Expr::Heredoc(parts) => {
            for part in parts {
                let StringPart::Literal(literal) = part else {
                    continue;
                };
                for name in bare_heredoc_vars(literal) {
                    if names.contains(name) {
                        a.diagnostics.push(diag(
                            ctx,
                            "MIX-W2402",
                            Severity::Warning,
                            line,
                            format!(
                                "`${name}` in heredoc is not interpolated — did you mean `${{{name}}}`?"
                            ),
                            Some(format!(
                                "literal `${name}` output requires no change"
                            )),
                        ));
                    }
                }
            }
        }
        Expr::Variable(name) => {
            if !ctx.dynamic
                && !is_positional(name)
                && !names.contains(name)
                && !ctx.known_callables.contains(name)
            {
                a.diagnostics.push(diag(
                    ctx,
                    "MIX-E1101",
                    Severity::Error,
                    line,
                    format!("undefined variable '${name}' (assigned nowhere in this file)"),
                    Some(format!(
                        "assign it, use env(\"{name}\") for environment values, or pass --allow-global {name}"
                    )),
                ));
            }
        }
        Expr::FunctionCall { name, args } => {
            // Lambda passed to a HOF? Its body is checked by
            // check_expr_tree via walk_expr below — here we resolve the
            // NAME. Unknown-callable rules (E1102):
            let is_builtin_name = builtins::builtin_info_of(name).is_some()
                || INLINE_SPECIAL_FORMS.contains(&name.as_str());
            if !is_builtin_name
                && !ctx.dynamic
                && !in_address
                && !ctx.known_callables.contains(name)
                && !names.contains(name)
            {
                a.diagnostics.push(diag(
                    ctx,
                    "MIX-E1102",
                    Severity::Error,
                    line,
                    format!("undefined function '{name}' (defined nowhere in this file)"),
                    Some(format!(
                        "define it, or pass --allow-function {name} if an embedder provides it"
                    )),
                ));
            }
            // E1201: builtin contract arity (exact-arity sets honored).
            if let Some(info) = builtins::builtin_info_of(name)
                && !info.contract.accepts_arity(args.len())
            {
                a.diagnostics.push(diag(
                    ctx,
                    "MIX-E1201",
                    Severity::Error,
                    line,
                    format!(
                        "{name}() called with {} argument(s); contract is {}",
                        args.len(),
                        info.signature()
                    ),
                    None,
                ));
            }
            // E1202: user-function arity when uniquely defined and not
            // shadowed by a function-valued variable.
            if !is_builtin_name
                && !names.contains(name)
                && let Some((min, max)) = ctx.user_fn_arity.get(name)
                && (args.len() < *min || args.len() > *max)
            {
                a.diagnostics.push(diag(
                    ctx,
                    "MIX-E1202",
                    Severity::Error,
                    line,
                    format!(
                        "{name}() called with {} argument(s); definition takes {}",
                        args.len(),
                        if min == max {
                            min.to_string()
                        } else {
                            format!("{min}..{max}")
                        }
                    ),
                    None,
                ));
            }
        }
        Expr::FunctionLiteral { params, body, .. } => {
            // A lambda gets its own universe: enclosing names (capture
            // semantics are stricter at runtime, but lint stays
            // conservative) + params + body binders. Param defaults are
            // evaluated in that inner universe.
            let mut inner: HashSet<String> = names.clone();
            for p in params {
                inner.insert(p.name.clone());
            }
            for p in params {
                if let Some(d) = &p.default {
                    check_expr_tree(d, line, ctx, a, &inner, in_address);
                }
            }
            match &**body {
                FunctionBody::Block(stmts) => {
                    let mut with_bound = inner.clone();
                    collect_bound_names(stmts, true, &mut with_bound);
                    check_scope(stmts, ctx, a, &with_bound, in_address, false);
                }
                FunctionBody::Expression(e) => {
                    check_expr_tree(e, line, ctx, a, &inner, in_address);
                }
            }
        }
        Expr::If(ifexpr) => {
            // Expression-position `if`: check the condition and recurse
            // the scope pass into every branch's statement list (same
            // universe — branches don't scope). check_expr_tree does NOT
            // descend here (walk_expr_children skips Expr::If), so this
            // is the sole visitor of its condition + branches.
            check_expr_tree(&ifexpr.condition, line, ctx, a, names, in_address);
            check_scope(&ifexpr.then_body, ctx, a, names, in_address, true);
            for (c, b) in &ifexpr.else_ifs {
                check_expr_tree(c, line, ctx, a, names, in_address);
                check_scope(b, ctx, a, names, in_address, true);
            }
            if let Some(b) = &ifexpr.else_body {
                check_scope(b, ctx, a, names, in_address, true);
            }
        }
        _ => {}
    }
}

/// Walk one expression tree (NOT descending into FunctionLiteral
/// bodies — check_expr handles those with their own universe) applying
/// check_expr to every node.
fn check_expr_tree(
    expr: &Expr,
    line: usize,
    ctx: &FileContext,
    a: &mut Analysis,
    names: &HashSet<String>,
    in_address: bool,
) {
    check_expr(expr, line, ctx, a, names, in_address);
    walk_expr_children(expr, &mut |child| {
        check_expr_tree(child, line, ctx, a, names, in_address);
    });
}

/// Visit the direct child expressions of a node. FunctionLiteral bodies
/// are deliberately NOT visited (they carry their own scope universe).
fn walk_expr_children(expr: &Expr, visit: &mut dyn FnMut(&Expr)) {
    match expr {
        Expr::BinaryOp { left, right, .. } => {
            visit(left);
            visit(right);
        }
        Expr::UnaryOp { operand, .. } => visit(operand),
        Expr::Ternary {
            cond,
            then_branch,
            else_branch,
        } => {
            visit(cond);
            visit(then_branch);
            visit(else_branch);
        }
        Expr::FunctionCall { args, .. } => {
            for arg in args {
                visit(arg);
            }
        }
        Expr::ValueCall { callee, args } => {
            visit(callee);
            for arg in args {
                visit(arg);
            }
        }
        Expr::MethodCall { object, args, .. } => {
            visit(object);
            for arg in args {
                visit(arg);
            }
        }
        Expr::Index { object, index } => {
            visit(object);
            visit(index);
        }
        Expr::FieldAccess { object, .. } => visit(object),
        Expr::ListLiteral(items) => {
            for item in items {
                visit(item);
            }
        }
        Expr::MapLiteral(entries) => {
            for (_, v) in entries {
                visit(v);
            }
        }
        Expr::Send { target, args, .. } => {
            visit(target);
            for (_, v) in args {
                visit(v);
            }
        }
        Expr::Sh(inner) => visit(inner),
        // Expr::If carries statement lists, not child expressions —
        // handled wholly by check_expr / for_each_embedded_stmt_list,
        // so it visits NOTHING here (visiting the condition would
        // double-check it).
        Expr::If(_) => {}
        _ => {}
    }
}

/// Visit every top-level expression of ONE statement (not nested
/// statement bodies — the scope pass recurses those itself), applying
/// the full-tree checker.
fn walk_stmt_exprs(stmt: &Stmt, visit: &mut dyn FnMut(&Expr)) {
    let mut go = |e: &Expr| visit(e);
    match &stmt.kind {
        StmtKind::Expression(e)
        | StmtKind::Die(e)
        | StmtKind::Source { path: e }
        | StmtKind::Include { path: e } => go(e),
        StmtKind::Assignment { value, .. } | StmtKind::Export { value, .. } => go(value),
        StmtKind::FieldAssignment { value, .. } => go(value),
        StmtKind::IndexAssignment { index, value, .. } => {
            go(index);
            go(value);
        }
        StmtKind::PathAssignment { path, value, .. } => {
            for seg in path {
                if let PathSeg::Index(e) = seg {
                    go(e);
                }
            }
            go(value);
        }
        StmtKind::If {
            condition,
            else_ifs,
            ..
        } => {
            go(condition);
            for (c, _) in else_ifs {
                go(c);
            }
        }
        StmtKind::For {
            start, end, step, ..
        } => {
            go(start);
            go(end);
            if let Some(s) = step {
                go(s);
            }
        }
        StmtKind::ForEach { iterable, .. } => go(iterable),
        StmtKind::While { condition, .. } => go(condition),
        StmtKind::BreakIf(c, _) | StmtKind::ContinueIf(c, _) => go(c),
        StmtKind::Return(Some(e)) => go(e),
        StmtKind::Select { value, cases, .. } => {
            go(value);
            for (case_value, _) in cases {
                go(case_value);
            }
        }
        StmtKind::Print { args, .. } => {
            for e in args {
                go(e);
            }
        }
        StmtKind::Parse { source, .. } => go(source),
        StmtKind::Send { target, args, .. } | StmtKind::Emit { target, args, .. } => {
            go(target);
            for (_, v) in args {
                go(v);
            }
        }
        StmtKind::Address { target, .. } => go(target),
        StmtKind::Alias { name, command } => {
            if let Some(e) = name {
                go(e);
            }
            if let Some(e) = command {
                go(e);
            }
        }
        StmtKind::Sh { command } => go(command),
        StmtKind::PipeToExternal { stmt: inner, .. } => walk_stmt_exprs(inner, visit),
        StmtKind::Chain { left, right, .. } => {
            walk_stmt_exprs(left, visit);
            walk_stmt_exprs(right, visit);
        }
        _ => {}
    }
}

// ── W2301..W2306 recurring silent-result traps ─────────────────────

#[derive(Clone, Copy)]
enum ProvenValue {
    List,
    Map,
    BuiltinResult(&'static str),
}

fn check_recurring_silent_bugs(stmts: &[Stmt], ctx: &FileContext, a: &mut Analysis) {
    check_proven_value_flow(stmts, ctx, a, &mut HashMap::new());
    check_assignment_chains(stmts, ctx, a);
    check_implicit_nil_calls(stmts, ctx, a);
    check_truthiness_traps(stmts, ctx, a);
    check_ssh_escaped_quotes(stmts, ctx, a);
}

/// Flag the narrow source shape that signals Mix source is being nested in an
/// ssh command string and will be parsed again by the remote login shell.
/// Provenance from the parser lets this stay quiet for a single-quoted string
/// that merely contains ordinary double quotes.
fn check_ssh_escaped_quotes(stmts: &[Stmt], ctx: &FileContext, a: &mut Analysis) {
    fn check_expr(expr: &Expr, line: usize, ctx: &FileContext, a: &mut Analysis) {
        if let Expr::FunctionCall { name, args } = expr
            && matches!(name.as_str(), "ssh_run" | "ssh_must")
            && matches!(args.get(1), Some(Expr::EscapedQuoteStringLiteral(_)))
        {
            a.diagnostics.push(diag(
                ctx,
                "MIX-W2306",
                Severity::Warning,
                line,
                format!(
                    "`{name}` string contains escaped quotes — the remote shell re-parses it; use `ssh_mix` with a heredoc to ship source verbatim"
                ),
                Some("see `mix man remote` for the `ssh_mix` + heredoc pattern".to_string()),
            ));
        }
        walk_expr_children(expr, &mut |child| check_expr(child, line, ctx, a));
    }

    walk_stmts(stmts, &mut |stmt| {
        walk_stmt_exprs(stmt, &mut |expr| check_expr(expr, stmt.line, ctx, a));
    });
}

// ── MIX-D3xxx — deprecations and release-transition advisories ───────
//
// All emitted at `Severity::Note` in release A (0.63.0): visible in
// every lint run, never gating. The five regex/grep codes D3001–D3005
// promote to `Warning` in release A.1 (codes unchanged) once the fleet
// inventory reads zero, and the names are deleted in release B. The
// pos-family codes D3008–D3011 stay notes until THEIR count reads zero,
// whenever that is. D3006/D3007 are release-transition watch notes for
// the A.1 behaviour flips (map two-var binding; map/list `==` raising),
// retired in A.1. Static, analyzer-surface only — never emitted at
// runtime (the `done`/`next` runtime-warning path is the anti-pattern:
// it would print on every execution of ~800 live call sites).
//
// Coverage: every spelling. A member-call of a BUILTIN name desugars to
// a FunctionCall at parse time (parser.rs `method_desugars_to_ufcs`),
// so `$s.regex_match(..)` is seen exactly like the bare call — pinned
// by the lint_notes CLI tests.

/// Pattern-first regex/grep names → their subject-first 0.63.0 twins.
const DEPRECATED_REGEX_CALLS: &[(&str, &str, &str)] = &[
    ("regex_match", "MIX-D3001", "re_match(s, pattern)"),
    (
        "regex_find",
        "MIX-D3002",
        "re_find(s, pattern) — NOTE: re_find returns CODEPOINT offsets where regex_find returns byte offsets; adjust offset arithmetic when migrating",
    ),
    ("regex_replace", "MIX-D3003", "re_replace(s, pattern, replacement)"),
    ("regex_split", "MIX-D3004", "re_split(s, pattern)"),
    ("grep", "MIX-D3005", "grep_lines(text, pattern)"),
];

/// REXX-style 1-based needle-first search family: declared legacy, not
/// scheduled for deletion — the note points migrants at the replacements.
const LEGACY_POS_CALLS: &[(&str, &str, &str)] = &[
    (
        "pos",
        "MIX-D3008",
        "contains() for yes/no, index_of() / after() / split_once() for positions",
    ),
    (
        "lastpos",
        "MIX-D3009",
        "last_index_of() / before_last() / after_last()",
    ),
    ("byte_pos", "MIX-D3010", "byte_index_of()"),
    (
        "byte_lastpos",
        "MIX-D3011",
        "byte-offset search has no 0-based last-occurrence twin yet; see strings.md",
    ),
];

fn check_release_transition_advisories(stmts: &[Stmt], ctx: &FileContext, a: &mut Analysis) {
    // Pos-family calls already covered by the sharper composed-form note
    // below (substr/slice over a pos-family call in ONE expression) —
    // suppressed from the generic note so a site gets one note, not two.
    let mut composed: HashSet<*const Expr> = HashSet::new();

    fn check_expr(
        expr: &Expr,
        line: usize,
        ctx: &FileContext,
        a: &mut Analysis,
        composed: &mut HashSet<*const Expr>,
    ) {
        if let Expr::FunctionCall { name, args } = expr {
            if let Some((_, code, repl)) =
                DEPRECATED_REGEX_CALLS.iter().find(|(n, _, _)| n == name)
            {
                a.diagnostics.push(diag(
                    ctx,
                    code,
                    Severity::Note,
                    line,
                    format!("`{name}` is pattern-first legacy: use `{repl}` (subject first)"),
                    Some(
                        "the five regex/grep legacy names are deleted in a later release; \
                         see `mix man regex`"
                            .to_string(),
                    ),
                ));
            } else if let Some((_, code, repl)) =
                LEGACY_POS_CALLS.iter().find(|(n, _, _)| n == name)
                && !composed.contains(&(expr as *const Expr))
            {
                a.diagnostics.push(diag(
                    ctx,
                    code,
                    Severity::Note,
                    line,
                    format!("`{name}` is declared legacy (1-based, needle-first): prefer {repl}"),
                    Some(
                        "legacy search names stay until their fleet count reads zero — \
                         migrate opportunistically; see `mix man strings`"
                            .to_string(),
                    ),
                ));
            }
            // Sharper composed-form note: `substr($s, pos(..) ± n)` /
            // `slice($s, pos(..))` in one expression — the 1-based/0-based
            // off-by-one trap. One-expression-deep only, by design: a
            // `$p = pos(..); substr($s, $p)` split is caught by the plain
            // pos-family note above instead.
            if matches!(name.as_str(), "substr" | "slice" | "grapheme_substr") {
                for arg in args.iter().skip(1) {
                    let mut found: Option<(*const Expr, &'static str, &'static str)> = None;
                    let mut scan = |e: &Expr| {
                        if let Expr::FunctionCall { name: inner, .. } = e
                            && let Some((n, code, _)) =
                                LEGACY_POS_CALLS.iter().find(|(n, _, _)| n == inner)
                            && found.is_none()
                        {
                            found = Some((e as *const Expr, code, n));
                        }
                    };
                    scan(arg);
                    walk_expr_children(arg, &mut |child| scan(child));
                    // Emit only on first insertion: a NESTED substr
                    // (`substr($s, 1 + substr($s, pos(..), 2), 3)`) rescans
                    // the same pos node from both levels — one site, one
                    // note (GLM review of d73304a6, finding 1).
                    if let Some((pos_ptr, code, pos_name)) = found
                        && composed.insert(pos_ptr)
                    {
                        a.diagnostics.push(diag(
                            ctx,
                            code,
                            Severity::Note,
                            line,
                            format!(
                                "`{name}(.., {pos_name}(..) ..)` composes a 1-based position \
                                 into a 0-based index — the off-by-one trap"
                            ),
                            Some(
                                "use after() / before() / split_once() instead of \
                                 position arithmetic"
                                    .to_string(),
                            ),
                        ));
                    }
                }
            }
        }
        // Blind-spot arms (GLM review of d73304a6, finding 2):
        // `walk_expr_children` deliberately skips if-expression internals
        // and lambda internals (they belong to the embedded-stmt-list
        // walker), so an advisory pass that promises every spelling must
        // reach the CONDITIONS and the expression-shaped lambda parts
        // itself. Branch/lambda BODIES that are statement lists arrive
        // via advisory_walk's embedded-list pass — no double visits.
        match expr {
            Expr::If(ifexpr) => {
                check_expr(&ifexpr.condition, line, ctx, a, composed);
                for (c, _) in &ifexpr.else_ifs {
                    check_expr(c, line, ctx, a, composed);
                }
            }
            Expr::FunctionLiteral { params, body } => {
                for p in params {
                    if let Some(d) = &p.default {
                        check_expr(d, line, ctx, a, composed);
                    }
                }
                if let FunctionBody::Expression(e) = &**body {
                    check_expr(e, line, ctx, a, composed);
                }
            }
            _ => {}
        }
        walk_expr_children(expr, &mut |child| check_expr(child, line, ctx, a, composed));
    }

    /// Self-contained statement walker: unlike the shared `walk_stmts`
    /// geometry (built for narrow heuristics), an advisory pass that
    /// promises coverage must also see (a) statements wrapped by
    /// `| external` pipes and `&&`/`||` chains, (b) named-fn parameter
    /// defaults and `= expr` bodies, and (c) the statement lists embedded
    /// in expressions (if-expression branches, block-lambda bodies) —
    /// each exactly once (GLM review of d73304a6, finding 2).
    fn advisory_stmt(
        stmt: &Stmt,
        ctx: &FileContext,
        a: &mut Analysis,
        composed: &mut HashSet<*const Expr>,
    ) {
        match &stmt.kind {
            StmtKind::PipeToExternal { stmt: inner, .. } => {
                advisory_stmt(inner, ctx, a, composed);
                return;
            }
            StmtKind::Chain { left, right, .. } => {
                advisory_stmt(left, ctx, a, composed);
                advisory_stmt(right, ctx, a, composed);
                return;
            }
            // MIX-D3006 — watch note for the A.1 map-binding flip: every
            // two-variable iteration, both spellings (`for each $i, $x`
            // and the 0.63.0 bare `for $i, $x`). Static typing cannot
            // tell a map iterable from a list, so the note makes every
            // site visible for one release cycle; retired in A.1 when
            // the binding flips.
            StmtKind::ForEach {
                index_var: Some(_), ..
            } => {
                a.diagnostics.push(diag(
                    ctx,
                    "MIX-D3006",
                    Severity::Note,
                    stmt.line,
                    "two-variable loop binds (index, item) for every iterable today; in \
                     release A.1 a MAP iterable binds (key, value) instead"
                        .to_string(),
                    Some(
                        "over a list nothing changes; if this can iterate a map and uses \
                         the first variable as a number, migrate to the one-variable key \
                         form now"
                            .to_string(),
                    ),
                ));
            }
            // Named-fn parameter defaults and `= expr` bodies:
            // walk_stmt_exprs has no FunctionDef arm and stmt_bodies
            // returns nothing for an Expression body.
            StmtKind::FunctionDef { params, body, .. } => {
                for p in params {
                    if let Some(d) = &p.default {
                        check_expr(d, stmt.line, ctx, a, composed);
                    }
                }
                if let FunctionBody::Expression(e) = body {
                    check_expr(e, stmt.line, ctx, a, composed);
                }
            }
            _ => {}
        }
        walk_stmt_exprs(stmt, &mut |expr| {
            check_expr(expr, stmt.line, ctx, a, composed)
        });
        for body in stmt_bodies(&stmt.kind) {
            for s in body {
                advisory_stmt(s, ctx, a, composed);
            }
        }
        walk_stmt_exprs(stmt, &mut |expr| {
            for_each_embedded_stmt_list(expr, true, &mut |body| {
                for s in body {
                    advisory_stmt(s, ctx, a, composed);
                }
            })
        });
    }

    for stmt in stmts {
        advisory_stmt(stmt, ctx, a, &mut composed);
    }
}

/// Builtins whose "not found" sentinel is `-1` and whose "found at the
/// first position" answer is `0`. In a boolean context both answers are
/// backwards, because Mix treats `0` as falsy and every non-zero number —
/// including `-1` — as truthy:
///
/// ```text
/// if index_of("abc", "z")   -- -1, TRUTHY  -> "not found" reads as found
/// if index_of("abc", "a")   --  0, FALSY   -> "found at 0" reads as absent
/// ```
///
/// Their 1-based twins (`pos`, `lastpos`, `byte_pos`, `byte_lastpos`) are
/// safe in the same position, since their not-found sentinel is `0` and so
/// is falsy — which is exactly why this trap is easy to walk into after
/// using those.
const MINUS_ONE_SENTINEL_BUILTINS: &[&str] = &["index_of", "byte_index_of"];

/// Flag a `-1`-sentinel builtin used directly as a truth value.
///
/// Deliberately narrow, in line with this analyzer's false-positives-near-zero
/// bias: only a BARE call in boolean position is reported. Any comparison
/// (`index_of(..) >= 0`, `!= -1`) is already correct code and is untouched,
/// because the call is then an operand of the comparison rather than the
/// condition itself.
fn check_truthiness_traps(stmts: &[Stmt], ctx: &FileContext, a: &mut Analysis) {
    fn check_cond(expr: &Expr, line: usize, ctx: &FileContext, a: &mut Analysis) {
        match expr {
            Expr::FunctionCall { name, .. }
                if MINUS_ONE_SENTINEL_BUILTINS.contains(&name.as_str()) =>
            {
                a.diagnostics.push(diag(
                    ctx,
                    "MIX-W2305",
                    Severity::Warning,
                    line,
                    format!(
                        "`{name}()` in a condition is backwards: -1 (not found) is truthy, 0 (found at the first position) is falsy"
                    ),
                    Some(format!(
                        "use contains() for the yes/no question, or compare explicitly: {name}(..) >= 0"
                    )),
                ));
            }
            // Boolean operators propagate the condition position to their
            // operands: `if index_of(..) and $x` has the same bug.
            Expr::UnaryOp {
                op: UnaryOp::Not,
                operand,
            } => check_cond(operand, line, ctx, a),
            Expr::BinaryOp {
                left,
                op: BinOp::And | BinOp::Or,
                right,
            } => {
                check_cond(left, line, ctx, a);
                check_cond(right, line, ctx, a);
            }
            _ => {}
        }
    }

    fn walk(stmts: &[Stmt], ctx: &FileContext, a: &mut Analysis) {
        for stmt in stmts {
            match &stmt.kind {
                StmtKind::If {
                    condition,
                    else_ifs,
                    ..
                } => {
                    check_cond(condition, stmt.line, ctx, a);
                    for (c, _) in else_ifs {
                        check_cond(c, stmt.line, ctx, a);
                    }
                }
                StmtKind::While { condition, .. } => check_cond(condition, stmt.line, ctx, a),
                StmtKind::BreakIf(c, _) | StmtKind::ContinueIf(c, _) => {
                    check_cond(c, stmt.line, ctx, a)
                }
                _ => {}
            }

            // Expression-position conditions: `$x = if index_of(..) then`,
            // and the ternary `index_of(..) ? a : b`.
            walk_stmt_exprs(stmt, &mut |expr| match expr {
                Expr::If(ifexpr) => {
                    check_cond(&ifexpr.condition, stmt.line, ctx, a);
                    for (c, _) in &ifexpr.else_ifs {
                        check_cond(c, stmt.line, ctx, a);
                    }
                }
                Expr::Ternary { cond, .. } => check_cond(cond, stmt.line, ctx, a),
                _ => {}
            });

            for body in stmt_bodies(&stmt.kind) {
                walk(body, ctx, a);
            }
        }
    }

    walk(stmts, ctx, a);
}

/// Statement-order facts only. Branch facts never merge back into their
/// parent and function frames start empty; both choices deliberately trade
/// missed warnings for freedom from dynamic-flow false positives.
fn check_proven_value_flow(
    stmts: &[Stmt],
    ctx: &FileContext,
    a: &mut Analysis,
    facts: &mut HashMap<String, ProvenValue>,
) {
    for stmt in stmts {
        walk_stmt_exprs(stmt, &mut |expr| {
            check_proven_expr(expr, stmt.line, ctx, a, facts)
        });

        match &stmt.kind {
            StmtKind::FunctionDef { params, body, .. } => {
                let mut inner = HashMap::new();
                for param in params {
                    if let Some(default) = &param.default {
                        check_proven_expr(default, stmt.line, ctx, a, &inner);
                    }
                }
                match body {
                    FunctionBody::Block(body) => check_proven_value_flow(body, ctx, a, &mut inner),
                    FunctionBody::Expression(expr) => {
                        check_proven_expr(expr, stmt.line, ctx, a, &inner)
                    }
                }
            }
            _ => {
                let mut inner = facts.clone();
                invalidate_child_binders(&stmt.kind, &mut inner);
                for body in stmt_bodies(&stmt.kind) {
                    check_proven_value_flow(body, ctx, a, &mut inner.clone());
                }
            }
        }

        invalidate_nested_writes(stmt, facts);
        match &stmt.kind {
            StmtKind::Assignment { name, value } | StmtKind::Export { name, value } => {
                if let Some(proven) = proven_value(value) {
                    facts.insert(name.clone(), proven);
                } else {
                    facts.remove(name);
                }
            }
            StmtKind::FieldAssignment { object, .. } | StmtKind::IndexAssignment { object, .. } => {
                facts.remove(object);
            }
            StmtKind::PathAssignment { root, .. } => {
                facts.remove(root);
            }
            StmtKind::Parse { parts, .. } => {
                for part in parts {
                    if let crate::ast::ParsePart::Variable(name) = part {
                        facts.remove(name);
                    }
                }
            }
            StmtKind::Source { .. } | StmtKind::Include { .. } => facts.clear(),
            _ => {}
        }
    }
}

fn invalidate_nested_writes(stmt: &Stmt, facts: &mut HashMap<String, ProvenValue>) {
    if matches!(stmt.kind, StmtKind::FunctionDef { .. }) {
        return;
    }
    let mut written = HashSet::new();
    for body in stmt_bodies(&stmt.kind) {
        collect_bound_names(body, false, &mut written);
    }
    walk_stmt_exprs(stmt, &mut |expr| {
        for_each_embedded_stmt_list(expr, false, &mut |body| {
            collect_bound_names(body, false, &mut written)
        });
    });
    for name in written {
        facts.remove(&name);
    }
}

fn invalidate_child_binders(kind: &StmtKind, facts: &mut HashMap<String, ProvenValue>) {
    match kind {
        StmtKind::For { var, .. } => {
            facts.remove(var);
        }
        StmtKind::ForEach { var, index_var, .. } => {
            facts.remove(var);
            if let Some(name) = index_var {
                facts.remove(name);
            }
        }
        StmtKind::TryCatch { catch: Some(c), .. } => {
            facts.remove(&c.var);
            if let Some(name) = &c.err_var {
                facts.remove(name);
            }
        }
        _ => {}
    }
}

fn proven_value(expr: &Expr) -> Option<ProvenValue> {
    match expr {
        Expr::ListLiteral(_) => Some(ProvenValue::List),
        Expr::MapLiteral(_) => Some(ProvenValue::Map),
        Expr::FunctionCall { name, .. } if builtin_result_fields(name).is_some() => Some(
            ProvenValue::BuiltinResult(builtins::builtin_info_of(name)?.name),
        ),
        _ => None,
    }
}

fn expr_is_proven_list(expr: &Expr, facts: &HashMap<String, ProvenValue>) -> bool {
    matches!(expr, Expr::ListLiteral(_))
        || matches!(expr, Expr::Variable(name) if matches!(facts.get(name), Some(ProvenValue::List)))
}

/// Map OR list, literal or straight-line-proven — the D3007 operand test.
fn expr_is_proven_collection(expr: &Expr, facts: &HashMap<String, ProvenValue>) -> bool {
    matches!(expr, Expr::ListLiteral(_) | Expr::MapLiteral(_))
        || matches!(expr, Expr::Variable(name)
            if matches!(facts.get(name), Some(ProvenValue::List | ProvenValue::Map)))
}

fn builtin_result_fields(name: &str) -> Option<&'static [FieldInfo]> {
    let info = builtins::builtin_info_of(name)?;
    match info.contract.returns {
        TypeShape::Map { fields, .. } if !fields.is_empty() => Some(fields),
        _ => None,
    }
}

fn result_origin<'a>(
    expr: &'a Expr,
    facts: &'a HashMap<String, ProvenValue>,
) -> Option<(&'static str, &'static [FieldInfo])> {
    let name = match expr {
        Expr::FunctionCall { name, .. } => builtins::builtin_info_of(name)?.name,
        Expr::Variable(name) => match facts.get(name)? {
            ProvenValue::BuiltinResult(name) => name,
            ProvenValue::List | ProvenValue::Map => return None,
        },
        _ => return None,
    };
    Some((name, builtin_result_fields(name)?))
}

fn check_proven_expr(
    expr: &Expr,
    line: usize,
    ctx: &FileContext,
    a: &mut Analysis,
    facts: &HashMap<String, ProvenValue>,
) {
    if let Expr::BinaryOp {
        left,
        op: BinOp::Add,
        right,
    } = expr
        && (expr_is_proven_list(left, facts) || expr_is_proven_list(right, facts))
    {
        a.diagnostics.push(diag(
            ctx,
            "MIX-W2301",
            Severity::Warning,
            line,
            "`+` stringifies lists instead of joining them".to_string(),
            Some("use concat(list_a, list_b) or push(list, value)".to_string()),
        ));
    }

    // MIX-D3007 — watch note for the A.1 equality flip: `==`/`!=` where
    // an operand is PROVEN a map or list (a literal, or a variable
    // straight-line-assigned one) is always false/true today — Value's
    // PartialEq has no structural arm. Best-effort by design: only
    // proven operands are seen (the same statement-order facts as
    // W2301), so an untraceable `$a == $b` passes silently — the runtime
    // raise in A.1 is the real fix, this note is the courtesy.
    if let Expr::BinaryOp { left, op, right } = expr
        && matches!(op, BinOp::Eq | BinOp::NotEq)
        && (expr_is_proven_collection(left, facts) || expr_is_proven_collection(right, facts))
    {
        let op_str = if matches!(op, BinOp::Eq) { "==" } else { "!=" };
        let answer = if matches!(op, BinOp::Eq) {
            "false"
        } else {
            "true"
        };
        a.diagnostics.push(diag(
            ctx,
            "MIX-D3007",
            Severity::Note,
            line,
            format!(
                "`{op_str}` on a map or list is always {answer} — structural comparison \
                 needs deep_eq(a, b)"
            ),
            Some(
                "release A.1 makes map/list `==`/`!=` raise TYPE_ERROR instead of \
                 silently answering; migrate now"
                    .to_string(),
            ),
        ));
    }

    match expr {
        Expr::Index { object, index } => {
            if let Expr::StringLiteral(key) = &**index {
                check_builtin_result_key(object, key, line, ctx, a, facts);
            }
        }
        Expr::FieldAccess { object, field } => {
            check_builtin_result_key(object, field, line, ctx, a, facts);
        }
        Expr::FunctionLiteral { params, body } => {
            let inner = HashMap::new();
            for param in params {
                if let Some(default) = &param.default {
                    check_proven_expr(default, line, ctx, a, &inner);
                }
            }
            match &**body {
                FunctionBody::Block(stmts) => {
                    check_proven_value_flow(stmts, ctx, a, &mut HashMap::new())
                }
                FunctionBody::Expression(expr) => check_proven_expr(expr, line, ctx, a, &inner),
            }
            return;
        }
        Expr::If(ifexpr) => {
            check_proven_expr(&ifexpr.condition, line, ctx, a, facts);
            check_proven_value_flow(&ifexpr.then_body, ctx, a, &mut facts.clone());
            for (condition, body) in &ifexpr.else_ifs {
                check_proven_expr(condition, line, ctx, a, facts);
                check_proven_value_flow(body, ctx, a, &mut facts.clone());
            }
            if let Some(body) = &ifexpr.else_body {
                check_proven_value_flow(body, ctx, a, &mut facts.clone());
            }
            return;
        }
        _ => {}
    }
    walk_expr_children(expr, &mut |child| {
        check_proven_expr(child, line, ctx, a, facts)
    });
}

fn check_builtin_result_key(
    object: &Expr,
    key: &str,
    line: usize,
    ctx: &FileContext,
    a: &mut Analysis,
    facts: &HashMap<String, ProvenValue>,
) {
    let Some((builtin, fields)) = result_origin(object, facts) else {
        return;
    };
    if fields.iter().any(|field| field.name == key) {
        return;
    }
    let valid = fields.iter().map(|field| field.name).collect::<Vec<_>>();
    let suffix = format!("_{key}");
    let suffix_matches = valid
        .iter()
        .copied()
        .filter(|candidate| candidate.ends_with(&suffix))
        .collect::<Vec<_>>();
    let candidates = if suffix_matches.is_empty() {
        valid.clone()
    } else {
        suffix_matches
    };
    let closest = candidates
        .iter()
        .min_by_key(|candidate| edit_distance(key, candidate))
        .copied();
    let hint = closest
        .map(|candidate| format!("use '{candidate}'; documented keys: {}", valid.join(", ")));
    a.diagnostics.push(diag(
        ctx,
        "MIX-W2304",
        Severity::Warning,
        line,
        format!("{builtin}() result has no documented key '{key}'"),
        hint,
    ));
}

fn edit_distance(left: &str, right: &str) -> usize {
    let right_chars = right.chars().collect::<Vec<_>>();
    let mut row = (0..=right_chars.len()).collect::<Vec<_>>();
    for (i, lc) in left.chars().enumerate() {
        let mut next = vec![i + 1];
        for (j, rc) in right_chars.iter().enumerate() {
            next.push(
                (row[j] + usize::from(lc != *rc))
                    .min(row[j + 1] + 1)
                    .min(next[j] + 1),
            );
        }
        row = next;
    }
    row[right_chars.len()]
}

/// Defence-in-depth for embedders using the public AST + `analyze()` API.
/// Source text cannot reach this shape because the parser rejects assignments
/// in every chain operand before constructing the `Chain` node.
fn check_assignment_chains(stmts: &[Stmt], ctx: &FileContext, a: &mut Analysis) {
    for stmt in stmts {
        check_assignment_chain_stmt(stmt, ctx, a);
        for body in stmt_bodies(&stmt.kind) {
            check_assignment_chains(body, ctx, a);
        }
        walk_stmt_exprs(stmt, &mut |expr| {
            for_each_embedded_stmt_list(expr, true, &mut |body| {
                check_assignment_chains(body, ctx, a)
            });
        });
    }
}

fn check_assignment_chain_stmt(stmt: &Stmt, ctx: &FileContext, a: &mut Analysis) {
    match &stmt.kind {
        StmtKind::Chain { left, op, right } => {
            if is_assignment_chain_operand(left) || is_assignment_chain_operand(right) {
                let sym = match op {
                    ChainOp::And => "&&",
                    ChainOp::Or => "||",
                };
                a.diagnostics.push(diag(
                    ctx,
                    "MIX-W2303",
                    Severity::Warning,
                    stmt.line,
                    format!("assignment used as an operand of a `{sym}` statement chain"),
                    Some(
                        "use `and`/`or` inside the assigned expression, or split the assignment and shell-style chain into separate statements"
                            .to_string(),
                    ),
                ));
            }
            check_assignment_chain_stmt(left, ctx, a);
            check_assignment_chain_stmt(right, ctx, a);
        }
        StmtKind::PipeToExternal { stmt, .. } => check_assignment_chain_stmt(stmt, ctx, a),
        _ => {}
    }
}

fn is_assignment_chain_operand(stmt: &Stmt) -> bool {
    match &stmt.kind {
        StmtKind::Assignment { .. }
        | StmtKind::FieldAssignment { .. }
        | StmtKind::IndexAssignment { .. }
        | StmtKind::PathAssignment { .. } => true,
        // Kept in lockstep with the parser predicate of the same name:
        // `export x = v` and the `alias n = c` DEFINE form bind a value.
        StmtKind::Export { .. } => true,
        StmtKind::Alias {
            command: Some(_), ..
        } => true,
        // …as does the terse `function f() = expr` form. The BLOCK form
        // binds no `=` expression and stays legal.
        StmtKind::FunctionDef {
            body: FunctionBody::Expression(_),
            ..
        } => true,
        StmtKind::PipeToExternal { stmt, .. } => is_assignment_chain_operand(stmt),
        _ => false,
    }
}

fn check_implicit_nil_calls(stmts: &[Stmt], ctx: &FileContext, a: &mut Analysis) {
    let mut definitions: HashMap<String, Vec<bool>> = HashMap::new();
    walk_stmts(stmts, &mut |stmt| {
        if let StmtKind::FunctionDef { name, body, .. } = &stmt.kind {
            let ends_in_expression = matches!(
                body,
                FunctionBody::Block(body)
                    if matches!(body.last().map(|s| &s.kind), Some(StmtKind::Expression(expr))
                        if !expression_never_returns(expr))
                        && !block_has_value_return(body)
            );
            definitions
                .entry(name.clone())
                .or_default()
                .push(ends_in_expression);
        }
    });
    let bad = definitions
        .into_iter()
        .filter_map(|(name, defs)| (defs == [true]).then_some(name))
        .collect::<HashSet<_>>();
    if bad.is_empty() {
        return;
    }

    // A same-named variable or parameter can redirect bareword dispatch
    // to a function value. Suppress the rule for that name anywhere in the
    // file rather than guessing which dynamic value wins at a call site.
    let mut shadowable = HashSet::new();
    collect_bound_names(stmts, true, &mut shadowable);
    walk_stmts(stmts, &mut |stmt| {
        if let StmtKind::FunctionDef { params, .. } = &stmt.kind {
            shadowable.extend(params.iter().map(|param| param.name.clone()));
        }
    });
    check_used_call_block(stmts, false, &bad, &shadowable, ctx, a);
}

fn expression_never_returns(expr: &Expr) -> bool {
    matches!(expr, Expr::FunctionCall { name, .. }
        if name == "raise"
            || builtins::builtin_info_of(name).is_some_and(|info| info.contract.effects.terminates))
}

fn block_has_value_return(stmts: &[Stmt]) -> bool {
    for stmt in stmts {
        if matches!(stmt.kind, StmtKind::Return(Some(_))) {
            return true;
        }
        if !matches!(stmt.kind, StmtKind::FunctionDef { .. })
            && stmt_bodies(&stmt.kind)
                .into_iter()
                .any(block_has_value_return)
        {
            return true;
        }
        let mut embedded_return = false;
        walk_stmt_exprs(stmt, &mut |expr| {
            for_each_embedded_stmt_list(expr, false, &mut |body| {
                embedded_return |= block_has_value_return(body)
            });
        });
        if embedded_return {
            return true;
        }
    }
    false
}

fn check_used_call_block(
    stmts: &[Stmt],
    block_value_used: bool,
    bad: &HashSet<String>,
    shadowable: &HashSet<String>,
    ctx: &FileContext,
    a: &mut Analysis,
) {
    for (index, stmt) in stmts.iter().enumerate() {
        let expression_used = block_value_used && index + 1 == stmts.len();
        check_used_call_stmt(stmt, expression_used, bad, shadowable, ctx, a);
    }
}

fn check_used_call_stmt(
    stmt: &Stmt,
    expression_used: bool,
    bad: &HashSet<String>,
    shadowable: &HashSet<String>,
    ctx: &FileContext,
    a: &mut Analysis,
) {
    match &stmt.kind {
        StmtKind::Expression(expr) => {
            check_used_call_expr(expr, expression_used, stmt.line, bad, shadowable, ctx, a)
        }
        StmtKind::FunctionDef { params, body, .. } => {
            for param in params {
                if let Some(default) = &param.default {
                    check_used_call_expr(default, true, stmt.line, bad, shadowable, ctx, a);
                }
            }
            match body {
                FunctionBody::Block(body) => {
                    check_used_call_block(body, false, bad, shadowable, ctx, a)
                }
                FunctionBody::Expression(expr) => {
                    check_used_call_expr(expr, true, stmt.line, bad, shadowable, ctx, a)
                }
            }
            return;
        }
        StmtKind::Chain { left, right, .. } => {
            check_used_call_stmt(left, false, bad, shadowable, ctx, a);
            check_used_call_stmt(right, false, bad, shadowable, ctx, a);
            return;
        }
        StmtKind::PipeToExternal { stmt, .. } => {
            check_used_call_stmt(stmt, false, bad, shadowable, ctx, a);
            return;
        }
        _ => walk_stmt_exprs(stmt, &mut |expr| {
            check_used_call_expr(expr, true, stmt.line, bad, shadowable, ctx, a)
        }),
    }
    for body in stmt_bodies(&stmt.kind) {
        check_used_call_block(body, false, bad, shadowable, ctx, a);
    }
}

fn check_used_call_expr(
    expr: &Expr,
    used: bool,
    line: usize,
    bad: &HashSet<String>,
    shadowable: &HashSet<String>,
    ctx: &FileContext,
    a: &mut Analysis,
) {
    if let Expr::FunctionCall { name, .. } = expr
        && used
        && bad.contains(name)
        && !shadowable.contains(name)
        && builtins::builtin_info_of(name).is_none()
    {
        a.diagnostics.push(diag(
            ctx,
            "MIX-W2302",
            Severity::Warning,
            line,
            format!("result of {name}() is used, but its block body implicitly returns nil"),
            Some(format!("add `return` before {name}()'s final expression")),
        ));
    }
    match expr {
        Expr::If(ifexpr) => {
            check_used_call_expr(&ifexpr.condition, true, line, bad, shadowable, ctx, a);
            check_used_call_block(&ifexpr.then_body, used, bad, shadowable, ctx, a);
            for (condition, body) in &ifexpr.else_ifs {
                check_used_call_expr(condition, true, line, bad, shadowable, ctx, a);
                check_used_call_block(body, used, bad, shadowable, ctx, a);
            }
            if let Some(body) = &ifexpr.else_body {
                check_used_call_block(body, used, bad, shadowable, ctx, a);
            }
        }
        Expr::FunctionLiteral { params, body } => {
            for param in params {
                if let Some(default) = &param.default {
                    check_used_call_expr(default, true, line, bad, shadowable, ctx, a);
                }
            }
            match &**body {
                FunctionBody::Block(body) => {
                    check_used_call_block(body, false, bad, shadowable, ctx, a)
                }
                FunctionBody::Expression(expr) => {
                    check_used_call_expr(expr, true, line, bad, shadowable, ctx, a)
                }
            }
        }
        _ => walk_expr_children(expr, &mut |child| {
            check_used_call_expr(child, true, line, bad, shadowable, ctx, a)
        }),
    }
}

// ── capabilities inventory ───────────────────────────────────────────

fn collect_capabilities(stmts: &[Stmt], a: &mut Analysis) {
    let mut caps: HashSet<&'static str> = HashSet::new();
    walk_stmts(stmts, &mut |stmt| {
        match &stmt.kind {
            StmtKind::Sh { .. } | StmtKind::PipeToExternal { .. } => {
                caps.insert("process");
            }
            StmtKind::Send { .. }
            | StmtKind::Emit { .. }
            | StmtKind::Address { .. }
            | StmtKind::On { .. } => {
                caps.insert("bus");
            }
            _ => {}
        }
        walk_stmt_exprs(stmt, &mut |expr| {
            collect_expr_caps(expr, &mut caps);
        });
    });
    let mut list: Vec<&'static str> = caps.into_iter().filter(|c| *c != "pure").collect();
    list.sort_unstable();
    a.capabilities = list;
}

fn collect_expr_caps(expr: &Expr, caps: &mut HashSet<&'static str>) {
    match expr {
        Expr::FunctionCall { name, args } => {
            if let Some(info) = builtins::builtin_info_of(name) {
                caps.insert(info.capability.as_str());
                for cc in info.contract.cond_caps {
                    if args.iter().any(
                        |arg| matches!(arg, Expr::MapLiteral(entries) if entries.iter().any(|(k, _)| k == cc.option)),
                    ) {
                        caps.insert(cc.capability.as_str());
                    }
                }
            }
            for arg in args {
                collect_expr_caps(arg, caps);
            }
        }
        Expr::Sh(_) | Expr::CommandSub(_) => {
            caps.insert("process");
        }
        Expr::Send { .. } => {
            caps.insert("bus");
        }
        Expr::FunctionLiteral { body, .. } => {
            // A BLOCK body's statements are reached by the caller's
            // walk_stmts descent (into_lambdas=true), so re-walking here
            // is redundant AND made nested lambdas O(2^depth) (codex
            // convergence review, MAJOR). Only an EXPRESSION body (not a
            // statement) needs collecting here.
            if let FunctionBody::Expression(e) = &**body {
                collect_expr_caps(e, caps);
            }
        }
        _ => {
            walk_expr_children(expr, &mut |child| collect_expr_caps(child, caps));
        }
    }
}
