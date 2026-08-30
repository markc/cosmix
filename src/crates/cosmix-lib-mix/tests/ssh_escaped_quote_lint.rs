//! MIX-W2306 — escaped quotes in an `ssh_run` / `ssh_must` command literal.
//!
//! `\"` in that position is a high-signal sign that Mix source has been
//! packed into a command string which the remote login shell will parse again.
//! The rule is deliberately literal-only: computed commands, argv transport,
//! `ssh_mix`, and single-quoted strings containing ordinary `"` stay quiet.

use cosmix_mix::analyzer::{AnalyzerConfig, Severity, analyze};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;

fn warnings(source: &str) -> Vec<(String, Option<String>)> {
    let tokens = Lexer::new(source).tokenize().expect("lex");
    let stmts = Parser::new(tokens, source).parse_program().expect("parse");
    analyze(&stmts, None, &AnalyzerConfig::default())
        .diagnostics
        .into_iter()
        .filter(|d| d.code == "MIX-W2306" && d.severity == Severity::Warning)
        .map(|d| (d.message, d.hint))
        .collect()
}

fn assert_flagged(source: &str) {
    let warnings = warnings(source);
    assert_eq!(
        warnings.len(),
        1,
        "expected exactly one W2306 for:\n{source}\ngot: {warnings:?}"
    );
    assert!(warnings[0].0.contains("remote shell re-parses it"));
    assert!(warnings[0].0.contains("`ssh_mix` with a heredoc"));
    assert_eq!(
        warnings[0].1.as_deref(),
        Some("see `mix man remote` for the `ssh_mix` + heredoc pattern")
    );
}

fn assert_clean(source: &str) {
    let warnings = warnings(source);
    assert!(
        warnings.is_empty(),
        "expected no W2306 for:\n{source}\ngot: {warnings:?}"
    );
}

#[test]
fn flags_ssh_run_literal_with_escaped_quote() {
    assert_flagged(r#"$r = ssh_run("alpha", "print(\"remote\")")"#);
}

#[test]
fn flags_ssh_must_when_available() {
    assert_flagged(r#"$out = ssh_must("alpha", "print(\"remote\")")"#);
}

#[test]
fn flags_deeply_nested_real_world_escape_shape() {
    assert_flagged(
        r#"$r = ssh_run("alpha", "print(run(\"/opt/cosmix/bin/mix -c 'print(which(\\\"sh\\\"))'\"))")"#,
    );
}

#[test]
fn simple_and_single_quoted_commands_stay_quiet() {
    assert_clean(r#"$r = ssh_run("alpha", "hostname")"#);
    assert_clean(r#"$r = ssh_run("alpha", 'print("remote")')"#);
}

#[test]
fn safe_remote_builtins_stay_quiet() {
    assert_clean(r#"$r = ssh_exec("alpha", ["print", "\"remote\""])"#);
    assert_clean(r#"$r = ssh_mix("alpha", "print(\"remote\")")"#);
}

#[test]
fn non_command_arguments_and_computed_commands_stay_quiet() {
    assert_clean(r#"$r = ssh_run("alpha\"alias", "hostname")"#);
    assert_clean(r#"$r = ssh_run("alpha", "print(" .. "\"remote\"")"#);
    assert_clean(r#"$r = ssh_run("alpha", "hostname", {cwd: "dir\"name"})"#);
}
