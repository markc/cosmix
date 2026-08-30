//! Parser-aware audit of `${head}` interpolation sites across the Mix
//! corpus. Phase A of the bash-parity plan introduces env-fallback for
//! the brace form; this audit reports every interpolation head with a
//! conservative classification so reviewers can confirm that the
//! semantic change is invisible (head dominated by an unconditional
//! top-level scope assign) or the intentional fix (head only present
//! via env), and flag the residual middle-band where dominance can't
//! be proved statically.
//!
//! Run explicitly — never by `cargo test` default:
//!
//! ```sh
//! cargo test -p cosmix-lib-mix --test audit_interp_corpus \
//!   -- --ignored --nocapture
//! ```
//!
//! Roots are configured by `COSMIX_AUDIT_ROOTS` (colon-separated);
//! defaults to `$HOME/.cosmix/src:$HOME/.rc:$HOME/.gh:$HOME/.ns`.
//!
//! Categories (per rev 4 plan):
//!  - `scope-dominated` — head appears as a top-level unconditional
//!    `Assignment`/`Export` whose `Stmt` precedes every interpolation
//!    site that references it (in load order within the same file).
//!  - `scope-assigned-but-not-proven-dominating` — head has at least
//!    one assignment in the file but it's nested in an `If`/`For`/
//!    `While`/`Loop`/`FunctionDef`/`Select`/`TryCatch`/`Address`/`On`
//!    block, or it's a top-level assignment that comes after an interp
//!    site. Env-fallback may fire on that earlier site; manual review
//!    required.
//!  - `env-name-exists-now` — `std::env::var(head)` returns non-empty
//!    in the audit process env. Env-fallback fires; confirm intent.
//!  - `common-env-shaped` — UPPERCASE_WITH_UNDERSCORES naming
//!    convention but env not currently set. Likely env-fallback
//!    candidate; confirm intent or document.
//!  - `unknown` — none of the above; needs eyeballs.
//!
//! A given (file, line, head) appears in exactly one category. The
//! cascade order above is also the precedence order.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use cosmix_mix::ast::{Expr, FunctionBody, ParsePart, PathSeg, Stmt, StmtKind};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;
use cosmix_mix::token::StringPart;

#[derive(Debug, Clone)]
struct InterpSite {
    line: usize,
    head: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Category {
    ScopeDominated,
    ScopeAssignedNotProvenDominating,
    EnvNameExistsNow,
    CommonEnvShaped,
    Unknown,
}

impl Category {
    fn label(self) -> &'static str {
        match self {
            Category::ScopeDominated => "scope-dominated",
            Category::ScopeAssignedNotProvenDominating => {
                "scope-assigned-but-not-proven-dominating"
            }
            Category::EnvNameExistsNow => "env-name-exists-now",
            Category::CommonEnvShaped => "common-env-shaped",
            Category::Unknown => "unknown",
        }
    }
}

#[derive(Debug)]
struct Finding {
    file: PathBuf,
    line: usize,
    head: String,
    category: Category,
}

#[test]
#[ignore]
fn audit_interp_corpus() {
    let roots = audit_roots();
    let mut findings: Vec<Finding> = Vec::new();
    let mut parse_errors: Vec<(PathBuf, String)> = Vec::new();
    let mut files_walked: usize = 0;

    for root in &roots {
        walk_mix_files(root, &mut |path| {
            let source = match std::fs::read_to_string(path) {
                Ok(s) => s,
                Err(_) => return,
            };
            files_walked += 1;
            match audit_one(&source) {
                Ok(file_findings) => {
                    findings.extend(file_findings.into_iter().map(|(line, head, cat)| Finding {
                        file: path.to_path_buf(),
                        line,
                        head,
                        category: cat,
                    }));
                }
                Err(e) => parse_errors.push((path.to_path_buf(), e)),
            }
        });
    }

    print_report(&roots, files_walked, &findings, &parse_errors);
}

fn audit_roots() -> Vec<PathBuf> {
    let raw = std::env::var("COSMIX_AUDIT_ROOTS").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_default();
        format!(
            "{home}/.cosmix/src:{home}/.rc:{home}/.gh:{home}/.ns",
            home = home
        )
    });
    raw.split(':')
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .collect()
}

fn walk_mix_files(root: &Path, on_file: &mut dyn FnMut(&Path)) {
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let ftype = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ftype.is_symlink() {
            continue; // avoid loops and external roots
        }
        if ftype.is_dir() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && matches!(name, "target" | "node_modules" | ".git" | ".direnv")
            {
                continue;
            }
            walk_mix_files(&path, on_file);
            continue;
        }
        if !ftype.is_file() {
            continue;
        }
        if is_mix_file(&path) {
            on_file(&path);
        }
    }
}

fn is_mix_file(path: &Path) -> bool {
    if path.extension().and_then(|e| e.to_str()) == Some("mix") {
        return true;
    }
    // Extensionless executable Mix detection: file is executable AND
    // first line is a Mix shebang. Both signals required to avoid
    // false-positives on shell scripts and binary executables.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => return false,
        };
        if meta.permissions().mode() & 0o111 == 0 {
            return false;
        }
        let mut buf = [0u8; 128];
        let n = match std::fs::File::open(path)
            .and_then(|mut f| std::io::Read::read(&mut f, &mut buf))
        {
            Ok(n) => n,
            Err(_) => return false,
        };
        let head = std::str::from_utf8(&buf[..n]).unwrap_or("");
        let first = head.lines().next().unwrap_or("");
        if !first.starts_with("#!") {
            return false;
        }
        // Accept `#!/.../mix`, `#!/.../mix <args>`, and
        // `#!/usr/bin/env mix [args]`. Reject paths whose final
        // component is not `mix`.
        is_mix_shebang(first)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn is_mix_shebang(line: &str) -> bool {
    // Strip "#!" and any leading whitespace; tokenize on whitespace.
    let rest = line.trim_start_matches("#!").trim_start();
    let mut tokens = rest.split_whitespace();
    let prog = match tokens.next() {
        Some(p) => p,
        None => return false,
    };
    let last_seg = |s: &str| s.rsplit('/').next().unwrap_or(s).to_string();
    if last_seg(prog) == "env" {
        // env-style: next token is the interpreter
        if let Some(next) = tokens.next() {
            return last_seg(next) == "mix";
        }
        return false;
    }
    last_seg(prog) == "mix"
}

fn audit_one(source: &str) -> Result<Vec<(usize, String, Category)>, String> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().map_err(|e| e.to_string())?;
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program().map_err(|e| e.to_string())?;

    // First pass: collect every interp site (line, head) in load
    // order, walking expressions inside every Stmt regardless of
    // nesting.
    let mut sites: Vec<InterpSite> = Vec::new();
    for stmt in &stmts {
        collect_interp_sites(stmt, &mut sites);
    }

    // Second pass: build the dominance map. A head is dominated only
    // if it has a top-level unconditional `Assignment` or `Export`
    // whose statement line precedes every site in `sites` that
    // references that head. Top-level == directly in `stmts`, not
    // inside any nested block.
    let mut top_level_assign_lines: BTreeMap<String, usize> = BTreeMap::new();
    for stmt in &stmts {
        let head_assigned = match &stmt.kind {
            StmtKind::Assignment { name, .. } | StmtKind::Export { name, .. } => Some(name.clone()),
            _ => None,
        };
        if let Some(name) = head_assigned {
            // Keep the earliest top-level assignment line for the
            // head; subsequent re-assignments don't change dominance.
            top_level_assign_lines.entry(name).or_insert(stmt.line);
        }
    }
    // Also collect every assignment (anywhere, nested or top-level) to
    // distinguish "assigned but not proven dominating" from "unknown".
    let mut any_assign: BTreeSet<String> = BTreeSet::new();
    for stmt in &stmts {
        collect_all_assigns(stmt, &mut any_assign);
    }

    // Classify each site.
    let mut out: Vec<(usize, String, Category)> = Vec::new();
    for site in sites {
        let cat = classify(&site, &top_level_assign_lines, &any_assign);
        out.push((site.line, site.head, cat));
    }
    Ok(out)
}

fn classify(
    site: &InterpSite,
    top_level: &BTreeMap<String, usize>,
    any_assign: &BTreeSet<String>,
) -> Category {
    if let Some(&assign_line) = top_level.get(&site.head) {
        // Strict `<`: a same-line site like `$X = "${X}"` reads the
        // PRIOR binding of `X` (or nil → env-fallback), not the
        // assignment currently being evaluated. Same-line therefore
        // falls through to ScopeAssignedNotProvenDominating, where a
        // human reviewer can decide whether the prior binding is
        // dominant in their context.
        if assign_line < site.line {
            return Category::ScopeDominated;
        }
        return Category::ScopeAssignedNotProvenDominating;
    }
    if any_assign.contains(&site.head) {
        return Category::ScopeAssignedNotProvenDominating;
    }
    if std::env::var(&site.head)
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        return Category::EnvNameExistsNow;
    }
    if is_common_env_shaped(&site.head) {
        return Category::CommonEnvShaped;
    }
    Category::Unknown
}

fn is_common_env_shaped(head: &str) -> bool {
    if head.is_empty() {
        return false;
    }
    head.chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && head.chars().any(|c| c.is_ascii_uppercase())
}

fn collect_interp_sites(stmt: &Stmt, out: &mut Vec<InterpSite>) {
    let line = stmt.line;
    let mut visit_expr = |e: &Expr| collect_interp_in_expr(e, line, out);
    match &stmt.kind {
        StmtKind::Expression(e) => visit_expr(e),
        StmtKind::Assignment { value, .. } => visit_expr(value),
        StmtKind::FieldAssignment { value, .. } => visit_expr(value),
        StmtKind::IndexAssignment { index, value, .. } => {
            visit_expr(index);
            visit_expr(value);
        }
        StmtKind::PathAssignment { path, value, .. } => {
            for seg in path {
                if let PathSeg::Index(e) = seg {
                    visit_expr(e);
                }
            }
            visit_expr(value);
        }
        StmtKind::If {
            condition,
            then_body,
            else_ifs,
            else_body,
        } => {
            visit_expr(condition);
            for s in then_body {
                collect_interp_sites(s, out);
            }
            for (cond, body) in else_ifs {
                collect_interp_in_expr(cond, line, out);
                for s in body {
                    collect_interp_sites(s, out);
                }
            }
            if let Some(eb) = else_body {
                for s in eb {
                    collect_interp_sites(s, out);
                }
            }
        }
        StmtKind::For {
            start,
            end,
            step,
            body,
            ..
        } => {
            visit_expr(start);
            visit_expr(end);
            if let Some(s) = step {
                visit_expr(s);
            }
            for s in body {
                collect_interp_sites(s, out);
            }
        }
        StmtKind::ForEach { iterable, body, .. } => {
            visit_expr(iterable);
            for s in body {
                collect_interp_sites(s, out);
            }
        }
        StmtKind::While {
            condition, body, ..
        } => {
            visit_expr(condition);
            for s in body {
                collect_interp_sites(s, out);
            }
        }
        StmtKind::Loop { body, .. } => {
            for s in body {
                collect_interp_sites(s, out);
            }
        }
        StmtKind::Break(_)
        | StmtKind::Continue(_)
        | StmtKind::BreakIf(_, _)
        | StmtKind::ContinueIf(_, _) => {}
        StmtKind::FunctionDef { params, body, .. } => {
            // Param default exprs are evaluated when the function is
            // called with that arg missing — they're real interpolation
            // sites and must be in the audit's "every site" set.
            for p in params {
                if let Some(d) = &p.default {
                    collect_interp_in_expr(d, line, out);
                }
            }
            match body {
                FunctionBody::Block(body) => {
                    for s in body {
                        collect_interp_sites(s, out);
                    }
                }
                FunctionBody::Expression(e) => collect_interp_in_expr(e, line, out),
            }
        }
        StmtKind::Return(Some(e)) => visit_expr(e),
        StmtKind::Return(None) => {}
        StmtKind::Select {
            value,
            cases,
            otherwise,
        } => {
            visit_expr(value);
            for (cond, body) in cases {
                collect_interp_in_expr(cond, line, out);
                for s in body {
                    collect_interp_sites(s, out);
                }
            }
            if let Some(ob) = otherwise {
                for s in ob {
                    collect_interp_sites(s, out);
                }
            }
        }
        StmtKind::Print { args, .. } => {
            for a in args {
                collect_interp_in_expr(a, line, out);
            }
        }
        StmtKind::Parse { source, .. } => visit_expr(source),
        StmtKind::Die(e) => visit_expr(e),
        StmtKind::TryCatch {
            try_body,
            catch,
            finally_body,
        } => {
            for s in try_body {
                collect_interp_sites(s, out);
            }
            if let Some(c) = catch {
                for s in &c.body {
                    collect_interp_sites(s, out);
                }
            }
            if let Some(f) = finally_body {
                for s in f {
                    collect_interp_sites(s, out);
                }
            }
        }
        StmtKind::Export { value, .. } => visit_expr(value),
        StmtKind::Alias { name, command } => {
            if let Some(n) = name {
                visit_expr(n);
            }
            if let Some(c) = command {
                visit_expr(c);
            }
        }
        StmtKind::Send {
            target,
            command,
            args,
        }
        | StmtKind::Emit {
            target,
            command,
            args,
        } => {
            visit_expr(target);
            visit_expr(command);
            for (_k, v) in args {
                collect_interp_in_expr(v, line, out);
            }
        }
        StmtKind::Address { target, body } => {
            visit_expr(target);
            for s in body {
                collect_interp_sites(s, out);
            }
        }
        StmtKind::On { body, .. } => {
            for s in body {
                collect_interp_sites(s, out);
            }
        }
        StmtKind::Source { path } => visit_expr(path),
        StmtKind::Include { path } => visit_expr(path),
        StmtKind::Sh { command } => visit_expr(command),
        StmtKind::PipeToExternal { stmt, .. } => collect_interp_sites(stmt, out),
        StmtKind::Chain { left, right, .. } => {
            collect_interp_sites(left, out);
            collect_interp_sites(right, out);
        }
    }
}

fn collect_interp_in_expr(expr: &Expr, line: usize, out: &mut Vec<InterpSite>) {
    match expr {
        Expr::InterpolatedString(parts) | Expr::Heredoc(parts) => {
            for p in parts {
                if let StringPart::Variable(name) = p {
                    // dotted heads: head is the part before the first
                    // `.` (matches evaluator's `name.split('.').next()`).
                    let head = name.split('.').next().unwrap_or("").to_string();
                    if !head.is_empty() {
                        out.push(InterpSite { line, head });
                    }
                }
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_interp_in_expr(left, line, out);
            collect_interp_in_expr(right, line, out);
        }
        Expr::UnaryOp { operand, .. } => collect_interp_in_expr(operand, line, out),
        Expr::FunctionCall { args, .. } => {
            for a in args {
                collect_interp_in_expr(a, line, out);
            }
        }
        Expr::ValueCall { callee, args } => {
            collect_interp_in_expr(callee, line, out);
            for a in args {
                collect_interp_in_expr(a, line, out);
            }
        }
        Expr::Index { object, index } => {
            collect_interp_in_expr(object, line, out);
            collect_interp_in_expr(index, line, out);
        }
        Expr::FieldAccess { object, .. } => collect_interp_in_expr(object, line, out),
        Expr::ListLiteral(items) => {
            for i in items {
                collect_interp_in_expr(i, line, out);
            }
        }
        Expr::MapLiteral(kvs) => {
            for (_k, v) in kvs {
                collect_interp_in_expr(v, line, out);
            }
        }
        Expr::Send {
            target,
            command,
            args,
        } => {
            collect_interp_in_expr(target, line, out);
            collect_interp_in_expr(command, line, out);
            for (_k, v) in args {
                collect_interp_in_expr(v, line, out);
            }
        }
        Expr::Sh(inner) => collect_interp_in_expr(inner, line, out),
        Expr::FunctionLiteral { params, body } => {
            // Anonymous-function default exprs are evaluated when the
            // literal is called — same interpolation-site contract as
            // statement-position `function` defaults.
            for p in params {
                if let Some(d) = &p.default {
                    collect_interp_in_expr(d, line, out);
                }
            }
            match &**body {
                FunctionBody::Block(stmts) => {
                    for s in stmts {
                        collect_interp_sites(s, out);
                    }
                }
                FunctionBody::Expression(e) => collect_interp_in_expr(e, line, out),
            }
        }
        _ => {}
    }
}

fn collect_all_assigns(stmt: &Stmt, out: &mut BTreeSet<String>) {
    match &stmt.kind {
        StmtKind::Assignment { name, .. } | StmtKind::Export { name, .. } => {
            out.insert(name.clone());
        }
        StmtKind::If {
            then_body,
            else_ifs,
            else_body,
            ..
        } => {
            for s in then_body {
                collect_all_assigns(s, out);
            }
            for (_c, body) in else_ifs {
                for s in body {
                    collect_all_assigns(s, out);
                }
            }
            if let Some(eb) = else_body {
                for s in eb {
                    collect_all_assigns(s, out);
                }
            }
        }
        StmtKind::For { var, body, .. } => {
            out.insert(var.clone());
            for s in body {
                collect_all_assigns(s, out);
            }
        }
        StmtKind::ForEach {
            var,
            index_var,
            body,
            ..
        } => {
            out.insert(var.clone());
            if let Some(iv) = index_var {
                out.insert(iv.clone());
            }
            for s in body {
                collect_all_assigns(s, out);
            }
        }
        StmtKind::While { body, .. }
        | StmtKind::Loop { body, .. }
        | StmtKind::Address { body, .. } => {
            for s in body {
                collect_all_assigns(s, out);
            }
        }
        StmtKind::FunctionDef { params, body, .. } => {
            // Conservative: param names bind inside the function body
            // but the audit's `any_assign` set is file-wide, used only
            // to demote sites from `unknown` to
            // `scope-assigned-but-not-proven-dominating`. Including
            // params here keeps the audit conservative — a name that
            // happens to match a param won't be mis-classified as a
            // silent env-fallback candidate.
            for p in params {
                out.insert(p.name.clone());
            }
            match body {
                FunctionBody::Block(body) => {
                    for s in body {
                        collect_all_assigns(s, out);
                    }
                }
                FunctionBody::Expression(e) => collect_assigns_in_expr(e, out),
            }
        }
        StmtKind::Parse { parts, .. } => {
            // `parse "..." with $name "delim" $other` writes each
            // captured variable into scope. Same conservative reason
            // for including these as for function params.
            for p in parts {
                if let ParsePart::Variable(name) = p {
                    out.insert(name.clone());
                }
            }
        }
        StmtKind::Select {
            cases, otherwise, ..
        } => {
            for (_c, body) in cases {
                for s in body {
                    collect_all_assigns(s, out);
                }
            }
            if let Some(ob) = otherwise {
                for s in ob {
                    collect_all_assigns(s, out);
                }
            }
        }
        StmtKind::TryCatch {
            try_body,
            catch,
            finally_body,
        } => {
            if let Some(c) = catch {
                out.insert(c.var.clone());
                if let Some(ev) = &c.err_var {
                    out.insert(ev.clone());
                }
                for s in &c.body {
                    collect_all_assigns(s, out);
                }
            }
            for s in try_body {
                collect_all_assigns(s, out);
            }
            if let Some(f) = finally_body {
                for s in f {
                    collect_all_assigns(s, out);
                }
            }
        }
        StmtKind::On { body, .. } => {
            for s in body {
                collect_all_assigns(s, out);
            }
        }
        StmtKind::PipeToExternal { stmt, .. } => collect_all_assigns(stmt, out),
        StmtKind::Chain { left, right, .. } => {
            collect_all_assigns(left, out);
            collect_all_assigns(right, out);
        }
        _ => {}
    }
    // Per-stmt: also walk every expression child for anonymous
    // function literals — their params bind names that the file-wide
    // `any_assign` set should treat as conservatively-assigned. The
    // expression tree walker fires only on FunctionLiteral; cheap.
    visit_stmt_exprs(stmt, &mut |e| collect_assigns_in_expr(e, out));
}

/// Visit every `Expr` directly owned by `stmt` (one level only — the
/// expression walker recurses into nested Exprs itself). Used to feed
/// expression-side visitors (FunctionLiteral param collection) from
/// the statement-level audit traversal.
fn visit_stmt_exprs(stmt: &Stmt, f: &mut dyn FnMut(&Expr)) {
    match &stmt.kind {
        StmtKind::Expression(e)
        | StmtKind::Assignment { value: e, .. }
        | StmtKind::FieldAssignment { value: e, .. }
        | StmtKind::Export { value: e, .. }
        | StmtKind::Die(e)
        | StmtKind::Source { path: e }
        | StmtKind::Include { path: e }
        | StmtKind::Sh { command: e }
        | StmtKind::Parse { source: e, .. } => f(e),
        StmtKind::IndexAssignment { index, value, .. } => {
            f(index);
            f(value);
        }
        StmtKind::PathAssignment { path, value, .. } => {
            for seg in path {
                if let PathSeg::Index(e) = seg {
                    f(e);
                }
            }
            f(value);
        }
        StmtKind::If {
            condition,
            else_ifs,
            ..
        } => {
            f(condition);
            for (c, _b) in else_ifs {
                f(c);
            }
        }
        StmtKind::For {
            start, end, step, ..
        } => {
            f(start);
            f(end);
            if let Some(s) = step {
                f(s);
            }
        }
        StmtKind::ForEach { iterable, .. } => f(iterable),
        StmtKind::While { condition, .. } => f(condition),
        StmtKind::Return(Some(e)) => f(e),
        StmtKind::Select { value, cases, .. } => {
            f(value);
            for (c, _b) in cases {
                f(c);
            }
        }
        StmtKind::Print { args, .. } => {
            for a in args {
                f(a);
            }
        }
        StmtKind::Alias { name, command } => {
            if let Some(n) = name {
                f(n);
            }
            if let Some(c) = command {
                f(c);
            }
        }
        StmtKind::Send {
            target,
            command,
            args,
        }
        | StmtKind::Emit {
            target,
            command,
            args,
        } => {
            f(target);
            f(command);
            for (_k, v) in args {
                f(v);
            }
        }
        StmtKind::Address { target, .. } => f(target),
        StmtKind::FunctionDef { params, body, .. } => {
            for p in params {
                if let Some(d) = &p.default {
                    f(d);
                }
            }
            // Expression-form function bodies (`function f() = expr`)
            // are reachable as a single Expr; yield it so downstream
            // visitors can walk any nested FunctionLiteral params.
            // Block bodies recurse through collect_all_assigns instead.
            if let FunctionBody::Expression(e) = body {
                f(e);
            }
        }
        StmtKind::BreakIf(e, _) | StmtKind::ContinueIf(e, _) => f(e),
        // Variants with no direct Expr children (bodies are walked by
        // collect_all_assigns separately).
        StmtKind::Break(_)
        | StmtKind::Continue(_)
        | StmtKind::Return(None)
        | StmtKind::Loop { .. }
        | StmtKind::TryCatch { .. }
        | StmtKind::On { .. }
        | StmtKind::PipeToExternal { .. }
        | StmtKind::Chain { .. } => {}
    }
}

/// Recursively descend into `expr`, inserting `Param.name` for every
/// `Expr::FunctionLiteral` found. Mirrors `collect_interp_in_expr`'s
/// tree walk for the param-binding shape only.
fn collect_assigns_in_expr(expr: &Expr, out: &mut BTreeSet<String>) {
    match expr {
        Expr::FunctionLiteral { params, body } => {
            for p in params {
                out.insert(p.name.clone());
                if let Some(d) = &p.default {
                    collect_assigns_in_expr(d, out);
                }
            }
            match &**body {
                FunctionBody::Block(stmts) => {
                    for s in stmts {
                        collect_all_assigns(s, out);
                    }
                }
                FunctionBody::Expression(e) => collect_assigns_in_expr(e, out),
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_assigns_in_expr(left, out);
            collect_assigns_in_expr(right, out);
        }
        Expr::UnaryOp { operand, .. } => collect_assigns_in_expr(operand, out),
        Expr::FunctionCall { args, .. } => {
            for a in args {
                collect_assigns_in_expr(a, out);
            }
        }
        Expr::ValueCall { callee, args } => {
            collect_assigns_in_expr(callee, out);
            for a in args {
                collect_assigns_in_expr(a, out);
            }
        }
        Expr::Index { object, index } => {
            collect_assigns_in_expr(object, out);
            collect_assigns_in_expr(index, out);
        }
        Expr::FieldAccess { object, .. } => collect_assigns_in_expr(object, out),
        Expr::ListLiteral(items) => {
            for i in items {
                collect_assigns_in_expr(i, out);
            }
        }
        Expr::MapLiteral(kvs) => {
            for (_k, v) in kvs {
                collect_assigns_in_expr(v, out);
            }
        }
        Expr::Send {
            target,
            command,
            args,
        } => {
            collect_assigns_in_expr(target, out);
            collect_assigns_in_expr(command, out);
            for (_k, v) in args {
                collect_assigns_in_expr(v, out);
            }
        }
        Expr::Sh(inner) => collect_assigns_in_expr(inner, out),
        // Leaves (literals, variable refs, command-sub, interpolated
        // string parts) carry no param bindings.
        _ => {}
    }
}

fn print_report(
    roots: &[PathBuf],
    files_walked: usize,
    findings: &[Finding],
    parse_errors: &[(PathBuf, String)],
) {
    println!("== Mix interpolation audit (Phase A — env-fallback) ==");
    println!("Roots:");
    for r in roots {
        println!("  {}", r.display());
    }
    println!("Files walked: {}", files_walked);
    println!("Interp sites: {}", findings.len());
    println!("Parse errors: {}", parse_errors.len());
    println!();

    // Per-category counts and per-head tallies.
    let mut by_category: BTreeMap<&'static str, Vec<&Finding>> = BTreeMap::new();
    for f in findings {
        by_category.entry(f.category.label()).or_default().push(f);
    }
    println!("== Summary by category ==");
    for cat in [
        Category::ScopeDominated,
        Category::ScopeAssignedNotProvenDominating,
        Category::EnvNameExistsNow,
        Category::CommonEnvShaped,
        Category::Unknown,
    ] {
        let empty: Vec<&Finding> = Vec::new();
        let entries = by_category.get(cat.label()).unwrap_or(&empty);
        let mut head_counts: BTreeMap<&str, usize> = BTreeMap::new();
        for f in entries {
            *head_counts.entry(f.head.as_str()).or_insert(0) += 1;
        }
        println!(
            "  {:42} sites={:4}  distinct-heads={}",
            cat.label(),
            entries.len(),
            head_counts.len()
        );
    }
    println!();

    // Detailed listing for the manual-review categories. Skip the
    // dominated and env-shaped categories from the per-site list to
    // keep the report readable; their counts above are enough for
    // sign-off.
    println!("== Sites needing manual review ==");
    for cat in [
        Category::ScopeAssignedNotProvenDominating,
        Category::EnvNameExistsNow,
        Category::CommonEnvShaped,
        Category::Unknown,
    ] {
        let empty: Vec<&Finding> = Vec::new();
        let entries = by_category.get(cat.label()).unwrap_or(&empty);
        if entries.is_empty() {
            continue;
        }
        println!("--- {} ({} sites) ---", cat.label(), entries.len());
        for f in entries {
            println!("  {}:{}: ${{{}}}", f.file.display(), f.line, f.head);
        }
        println!();
    }

    if !parse_errors.is_empty() {
        println!("== Parse errors ==");
        for (p, e) in parse_errors {
            // First line of the error is usually enough for triage.
            let first = e.lines().next().unwrap_or("");
            println!("  {}: {}", p.display(), first);
        }
    }
}
