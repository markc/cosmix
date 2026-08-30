use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use cosmix_mix::ast::{Expr, FunctionBody, PathSeg, Stmt, StmtKind};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;
use cosmix_mix::stats::{BUILTIN_NAMES, CANONICAL_KEYWORDS, HOF_NAMES};

#[derive(Default)]
struct Coverage {
    files: usize,
    builtins: HashMap<String, u64>,
    keywords: HashMap<String, u64>,
    dynamic_calls: u64,
}

pub fn run(root: &Path) -> i32 {
    match scan(root) {
        Ok(coverage) => {
            print_report(root, &coverage);
            0
        }
        Err(errors) => {
            for error in errors {
                eprintln!("mix stats coverage: {error}");
            }
            1
        }
    }
}

fn scan(root: &Path) -> Result<Coverage, Vec<String>> {
    let mut paths = Vec::new();
    let mut errors = Vec::new();
    collect_mix_files(root, true, &mut paths, &mut errors);
    paths.sort();
    let builtins: HashSet<&str> = BUILTIN_NAMES
        .iter()
        .chain(HOF_NAMES.iter())
        .copied()
        .collect();
    let mut coverage = Coverage::default();
    for path in paths {
        let source = match std::fs::read_to_string(&path) {
            Ok(source) => source,
            Err(error) => {
                errors.push(format!("{}: {error}", path.display()));
                continue;
            }
        };
        let tokens = match Lexer::new(&source).tokenize() {
            Ok(tokens) => tokens,
            Err(error) => {
                errors.push(format!("{}: {error}", path.display()));
                continue;
            }
        };
        let mut parser = Parser::new(tokens, &source);
        let statements = match parser.parse_program() {
            Ok(statements) => statements,
            Err(error) => {
                errors.push(format!("{}: {error}", path.display()));
                continue;
            }
        };
        coverage.files += 1;
        walk_statements(&statements, &builtins, &mut coverage);
    }
    if errors.is_empty() {
        Ok(coverage)
    } else {
        Err(errors)
    }
}

fn collect_mix_files(
    root: &Path,
    follow_named_root: bool,
    paths: &mut Vec<PathBuf>,
    errors: &mut Vec<String>,
) {
    let metadata = match if follow_named_root {
        std::fs::metadata(root)
    } else {
        std::fs::symlink_metadata(root)
    } {
        Ok(metadata) => metadata,
        Err(error) => {
            errors.push(format!("{}: {error}", root.display()));
            return;
        }
    };
    if !follow_named_root && metadata.file_type().is_symlink() {
        return;
    }
    if metadata.is_file() {
        if root.extension().is_some_and(|ext| ext == "mix") {
            paths.push(root.to_path_buf());
        }
        return;
    }
    if !metadata.is_dir() {
        return;
    }
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => {
            errors.push(format!("{}: {error}", root.display()));
            return;
        }
    };
    let mut children = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => children.push(entry.path()),
            Err(error) => errors.push(format!("{}: {error}", root.display())),
        }
    }
    children.sort();
    for child in children {
        collect_mix_files(&child, false, paths, errors);
    }
}

fn count(map: &mut HashMap<String, u64>, name: &str) {
    let value = map.entry(name.to_string()).or_insert(0);
    *value = value.saturating_add(1);
}

fn walk_statements(stmts: &[Stmt], builtins: &HashSet<&str>, coverage: &mut Coverage) {
    for stmt in stmts {
        for keyword in stmt.kind.stats_keywords().into_iter().flatten() {
            count(&mut coverage.keywords, keyword);
        }
        match &stmt.kind {
            StmtKind::Expression(expr) => walk_expr(expr, builtins, coverage),
            StmtKind::Assignment { value, .. } | StmtKind::Export { value, .. } => {
                walk_expr(value, builtins, coverage)
            }
            StmtKind::FieldAssignment { value, .. } => walk_expr(value, builtins, coverage),
            StmtKind::IndexAssignment { index, value, .. } => {
                walk_expr(index, builtins, coverage);
                walk_expr(value, builtins, coverage);
            }
            StmtKind::PathAssignment { path, value, .. } => {
                for segment in path {
                    if let PathSeg::Index(expr) = segment {
                        walk_expr(expr, builtins, coverage);
                    }
                }
                walk_expr(value, builtins, coverage);
            }
            StmtKind::If {
                condition,
                then_body,
                else_ifs,
                else_body,
            } => {
                walk_expr(condition, builtins, coverage);
                walk_statements(then_body, builtins, coverage);
                for (condition, body) in else_ifs {
                    walk_expr(condition, builtins, coverage);
                    walk_statements(body, builtins, coverage);
                }
                if let Some(body) = else_body {
                    walk_statements(body, builtins, coverage);
                }
            }
            StmtKind::For {
                start,
                end,
                step,
                body,
                ..
            } => {
                walk_expr(start, builtins, coverage);
                walk_expr(end, builtins, coverage);
                if let Some(step) = step {
                    walk_expr(step, builtins, coverage);
                }
                walk_statements(body, builtins, coverage);
            }
            StmtKind::ForEach { iterable, body, .. } => {
                walk_expr(iterable, builtins, coverage);
                walk_statements(body, builtins, coverage);
            }
            StmtKind::While {
                condition, body, ..
            } => {
                walk_expr(condition, builtins, coverage);
                walk_statements(body, builtins, coverage);
            }
            StmtKind::Loop { body, .. } | StmtKind::On { body, .. } => {
                walk_statements(body, builtins, coverage)
            }
            StmtKind::BreakIf(expr, _) | StmtKind::ContinueIf(expr, _) => {
                walk_expr(expr, builtins, coverage)
            }
            StmtKind::FunctionDef { params, body, .. } => {
                for param in params {
                    if let Some(default) = &param.default {
                        walk_expr(default, builtins, coverage);
                    }
                }
                walk_function_body(body, builtins, coverage);
            }
            StmtKind::Return(expr) => {
                if let Some(expr) = expr {
                    walk_expr(expr, builtins, coverage);
                }
            }
            StmtKind::Select {
                value,
                cases,
                otherwise,
            } => {
                walk_expr(value, builtins, coverage);
                for (case, body) in cases {
                    walk_expr(case, builtins, coverage);
                    walk_statements(body, builtins, coverage);
                }
                if let Some(body) = otherwise {
                    walk_statements(body, builtins, coverage);
                }
            }
            StmtKind::Print { args, .. } => {
                for arg in args {
                    walk_expr(arg, builtins, coverage);
                }
            }
            StmtKind::Parse { source, .. }
            | StmtKind::Die(source)
            | StmtKind::Source { path: source }
            | StmtKind::Include { path: source }
            | StmtKind::Sh { command: source } => walk_expr(source, builtins, coverage),
            StmtKind::TryCatch {
                try_body,
                catch,
                finally_body,
            } => {
                walk_statements(try_body, builtins, coverage);
                if let Some(catch) = catch {
                    walk_statements(&catch.body, builtins, coverage);
                }
                if let Some(body) = finally_body {
                    walk_statements(body, builtins, coverage);
                }
            }
            StmtKind::Alias { name, command } => {
                for expr in [name, command].into_iter().flatten() {
                    walk_expr(expr, builtins, coverage);
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
                walk_expr(target, builtins, coverage);
                walk_expr(command, builtins, coverage);
                for (_, expr) in args {
                    walk_expr(expr, builtins, coverage);
                }
            }
            StmtKind::Address { target, body } => {
                walk_expr(target, builtins, coverage);
                walk_statements(body, builtins, coverage);
            }
            StmtKind::PipeToExternal { stmt, .. } => {
                walk_statements(std::slice::from_ref(stmt), builtins, coverage)
            }
            StmtKind::Chain { left, right, .. } => {
                walk_statements(std::slice::from_ref(left), builtins, coverage);
                walk_statements(std::slice::from_ref(right), builtins, coverage);
            }
            StmtKind::Break(_) | StmtKind::Continue(_) => {}
        }
    }
}

fn walk_function_body(body: &FunctionBody, builtins: &HashSet<&str>, coverage: &mut Coverage) {
    match body {
        FunctionBody::Block(stmts) => walk_statements(stmts, builtins, coverage),
        FunctionBody::Expression(expr) => walk_expr(expr, builtins, coverage),
    }
}

fn walk_expr(expr: &Expr, builtins: &HashSet<&str>, coverage: &mut Coverage) {
    match expr {
        Expr::FunctionCall { name, args } => {
            if builtins.contains(name.as_str()) {
                count(&mut coverage.builtins, name);
            }
            for arg in args {
                walk_expr(arg, builtins, coverage);
            }
        }
        Expr::ValueCall { callee, args } => {
            coverage.dynamic_calls = coverage.dynamic_calls.saturating_add(1);
            walk_expr(callee, builtins, coverage);
            for arg in args {
                walk_expr(arg, builtins, coverage);
            }
        }
        Expr::MethodCall { object, args, .. } => {
            walk_expr(object, builtins, coverage);
            for arg in args {
                walk_expr(arg, builtins, coverage);
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            walk_expr(left, builtins, coverage);
            walk_expr(right, builtins, coverage);
        }
        Expr::UnaryOp { operand, .. } => walk_expr(operand, builtins, coverage),
        Expr::Ternary {
            cond,
            then_branch,
            else_branch,
        } => {
            walk_expr(cond, builtins, coverage);
            walk_expr(then_branch, builtins, coverage);
            walk_expr(else_branch, builtins, coverage);
        }
        Expr::If(ifx) => {
            count(&mut coverage.keywords, "if");
            walk_expr(&ifx.condition, builtins, coverage);
            walk_statements(&ifx.then_body, builtins, coverage);
            for (condition, body) in &ifx.else_ifs {
                walk_expr(condition, builtins, coverage);
                walk_statements(body, builtins, coverage);
            }
            if let Some(body) = &ifx.else_body {
                walk_statements(body, builtins, coverage);
            }
        }
        Expr::FunctionLiteral { params, body } => {
            count(&mut coverage.keywords, "function");
            for param in params {
                if let Some(default) = &param.default {
                    walk_expr(default, builtins, coverage);
                }
            }
            walk_function_body(body, builtins, coverage);
        }
        Expr::Index { object, index } => {
            walk_expr(object, builtins, coverage);
            walk_expr(index, builtins, coverage);
        }
        Expr::FieldAccess { object, .. } => walk_expr(object, builtins, coverage),
        Expr::ListLiteral(items) => {
            for item in items {
                walk_expr(item, builtins, coverage);
            }
        }
        Expr::MapLiteral(entries) => {
            for (_, value) in entries {
                walk_expr(value, builtins, coverage);
            }
        }
        Expr::Send {
            target,
            command,
            args,
        } => {
            count(&mut coverage.keywords, "send");
            walk_expr(target, builtins, coverage);
            walk_expr(command, builtins, coverage);
            for (_, value) in args {
                walk_expr(value, builtins, coverage);
            }
        }
        Expr::Sh(inner) => {
            count(&mut coverage.keywords, "sh");
            walk_expr(inner, builtins, coverage);
        }
        Expr::NumberLiteral(_)
        | Expr::StringLiteral(_)
        | Expr::EscapedQuoteStringLiteral(_)
        | Expr::BoolLiteral(_)
        | Expr::NilLiteral
        | Expr::InterpolatedString(_)
        | Expr::Heredoc(_)
        | Expr::Variable(_)
        | Expr::CommandSub(_) => {}
    }
}

fn print_report(root: &Path, coverage: &Coverage) {
    println!("Static authorship coverage — {}", root.display());
    println!("{} .mix file(s)", coverage.files);
    println!("Used builtins:");
    print_counts(&coverage.builtins);
    println!("Never-authored builtins:");
    for name in BUILTIN_NAMES.iter().chain(HOF_NAMES.iter()) {
        if !coverage.builtins.contains_key(*name) {
            println!("  {name}");
        }
    }
    println!("Used keywords:");
    print_counts(&coverage.keywords);
    println!("Never-authored keywords:");
    for name in CANONICAL_KEYWORDS {
        if !coverage.keywords.contains_key(*name) {
            println!("  {name}");
        }
    }
    println!("Dynamic calls not classifiable: {}", coverage.dynamic_calls);
}

fn print_counts(map: &HashMap<String, u64>) {
    let mut values: Vec<_> = map.iter().collect();
    values.sort_by(|a, b| a.0.cmp(b.0));
    for (name, count) in values {
        println!("  {name:<32} {count}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_uses_parser_and_ignores_builtin_names_in_strings_and_comments() {
        let dir = std::env::temp_dir().join(format!("mix-coverage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("fixture.mix"),
            "-- fake print(len(x))\n\
             $x = \"len(map($f, $xs))\"\n\
             $ys = map([1], function($item) = length([$item]))\n\
             $up = \"abc\".upper()\n\
             $dynamic = $f(1)\n\
             print(length([1]))\n",
        )
        .unwrap();
        let coverage = scan(&dir).unwrap();
        assert_eq!(coverage.builtins.get("length"), Some(&2));
        assert_eq!(coverage.builtins.get("map"), Some(&1));
        assert_eq!(coverage.builtins.get("upper"), Some(&1));
        assert!(!coverage.builtins.contains_key("len"));
        assert_eq!(coverage.keywords.get("print"), Some(&1));
        assert_eq!(coverage.keywords.get("function"), Some(&1));
        assert_eq!(coverage.dynamic_calls, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
