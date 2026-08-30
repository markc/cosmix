//! MIX-W2305 — `index_of()` / `byte_index_of()` used bare as a truth value.
//!
//! These return `-1` for "not found" and `0` for "found at the first
//! position". Mix treats `0` as falsy and every non-zero number (including
//! `-1`) as truthy, so a bare call in a condition is wrong on BOTH branches:
//! absent reads as present, and present-at-0 reads as absent.
//!
//! Their 1-based twins (`pos` etc.) are safe in the same position because
//! their not-found sentinel is `0` — which is precisely what makes this easy
//! to walk into after using them.
//!
//! The rule is deliberately narrow: only a BARE call in boolean position is
//! flagged. Any explicit comparison is already correct and must stay silent,
//! in line with the analyzer's false-positives-near-zero bias.

use cosmix_mix::analyzer::{AnalyzerConfig, Severity, analyze};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;

fn warnings(source: &str) -> Vec<String> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().expect("lex");
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program().expect("parse");
    analyze(&stmts, None, &AnalyzerConfig::default())
        .diagnostics
        .into_iter()
        .filter(|d| d.code == "MIX-W2305" && d.severity == Severity::Warning)
        .map(|d| d.message)
        .collect()
}

fn assert_flagged(source: &str) {
    let w = warnings(source);
    assert_eq!(
        w.len(),
        1,
        "expected exactly one W2305 for:\n{source}\ngot: {w:?}"
    );
}

fn assert_clean(source: &str) {
    let w = warnings(source);
    assert!(w.is_empty(), "expected no W2305 for:\n{source}\ngot: {w:?}");
}

// --- must warn -------------------------------------------------------------

#[test]
fn flags_if_condition() {
    assert_flagged(r#"if index_of("abc", "z") then print("x") end"#);
}

#[test]
fn flags_byte_index_of() {
    assert_flagged(r#"if byte_index_of("abc", "z") then print("x") end"#);
}

#[test]
fn flags_while_condition() {
    assert_flagged("while index_of(\"abc\", \"z\")\n  break\nend");
}

#[test]
fn flags_expression_position_if() {
    assert_flagged(r#"$x = if index_of("abc", "z") then 1 else 2 end"#);
}

#[test]
fn flags_ternary_condition() {
    assert_flagged(r#"$y = index_of("abc", "z") ? 1 : 2"#);
}

/// `not` and the boolean operators propagate condition position.
#[test]
fn flags_under_not() {
    assert_flagged(r#"if not index_of("abc", "z") then print("x") end"#);
}

#[test]
fn flags_under_and_or() {
    assert_flagged(r#"if index_of("abc", "z") and true then print("x") end"#);
    assert_flagged(r#"if false or index_of("abc", "z") then print("x") end"#);
}

#[test]
fn flags_inside_nested_body() {
    assert_flagged("for $i = 1 to 2\n  if index_of(\"abc\", \"z\") then print(\"x\") end\nend");
}

// --- must stay silent ------------------------------------------------------

#[test]
fn allows_explicit_comparison() {
    assert_clean(r#"if index_of("abc", "z") >= 0 then print("x") end"#);
    assert_clean(r#"if index_of("abc", "z") != -1 then print("x") end"#);
    assert_clean(r#"if index_of("abc", "z") == -1 then print("x") end"#);
}

#[test]
fn allows_contains() {
    assert_clean(r#"if contains("abc", "z") then print("x") end"#);
}

/// The 1-based family is correct in a condition — flagging it would be the
/// false positive that makes the rule untrustworthy.
#[test]
fn allows_pos_family() {
    assert_clean(r#"if pos("z", "abc") then print("x") end"#);
    assert_clean(r#"if lastpos("z", "abc") then print("x") end"#);
    assert_clean(r#"if byte_pos("z", "abc") then print("x") end"#);
}

/// Value position is not condition position.
#[test]
fn allows_plain_assignment_and_args() {
    assert_clean(r#"$n = index_of("abc", "z")"#);
    assert_clean(r#"print(index_of("abc", "z"))"#);
}
