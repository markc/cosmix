//! `do` is reserved and refused, not accepted.
//!
//! Before 0.98.0 the lexer had no `do` token, so `while … do … end` and
//! `for … do … end` "worked": `do` lexed as a bare identifier, parsed as
//! a harmless no-op expression statement, and the block still closed
//! with `end`. The bash reflex was silently absorbed — until the same
//! text in a `mix -c` one-liner misrouted to the shell and died with
//! ENOENT (TODO-mix.md, the `-c` shell-errno entry). Decision (Mark,
//! 2026-09-29): remove `do`, keep one grammar. It now lexes to
//! `Token::Do` and the parser raises an instructional error that names
//! the `end` form.
//!
//! `$do` stays a legal variable name — the `$` sigil is its own
//! namespace and the lexer reads it raw.

use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;

fn parse_error(source: &str) -> String {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().expect("lexes");
    Parser::new(tokens, source)
        .parse_program()
        .expect_err("must refuse `do`")
        .to_string()
}

#[test]
fn while_do_is_refused_with_the_end_form_named() {
    let err = parse_error("while true do\n  print(\"x\")\nend\n");
    assert!(err.contains("no `do` keyword"), "got: {err}");
    assert!(err.contains("blocks close with `end`"), "got: {err}");
}

#[test]
fn for_each_do_is_refused() {
    let err = parse_error("for each $x in [1, 2] do\n  print($x)\nend\n");
    assert!(err.contains("no `do` keyword"), "got: {err}");
}

#[test]
fn on_handler_do_is_refused() {
    let err = parse_error("on ping do\n  reply(\"pong\")\nend\n");
    assert!(err.contains("no `do` keyword"), "got: {err}");
}

#[test]
fn bare_do_statement_is_refused() {
    let err = parse_error("do\n");
    assert!(err.contains("no `do` keyword"), "got: {err}");
}

#[test]
fn sigil_variable_named_do_still_works() {
    let mut lexer = Lexer::new("$do = 1\nprint($do)\n");
    let tokens = lexer.tokenize().expect("$do lexes");
    let stmts = Parser::new(tokens, "$do = 1\nprint($do)\n")
        .parse_program()
        .expect("$do is a variable, not the refused keyword");
    assert_eq!(stmts.len(), 2);
}

#[test]
fn do_is_a_keyword_token_for_highlighting() {
    // `Token` itself is private to the lib, but the public highlight
    // surface proves the point: `do` classifies as Keyword, so it is
    // reserved — never a bare identifier.
    use cosmix_mix::lexer::TokenClass;
    let classes = cosmix_mix::lexer::highlight("do", cosmix_mix::lexer::MixFlavor::Script);
    assert_eq!(classes[0].1, TokenClass::Keyword);
}
