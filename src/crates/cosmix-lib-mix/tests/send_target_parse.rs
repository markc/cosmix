//! Parser-level pin of the bare hyphenated `send`/`emit`/`address` target.
//!
//! The integration gate (`cosmix-mix/tests/send_hyphenated_target.rs`)
//! proves the behaviour end-to-end through the binary and against a live
//! broker when one is reachable; this is the unit pin inside the library:
//! the exact AST the construct produces, with no process or bus involved.
//! Bus service names routinely contain hyphens (`comp-vt2`,
//! `quoin-shot-vt2`), and `-` is also the subtraction operator — the fix
//! reads a tight hyphen-joined word as ONE name in target position only,
//! and these tests hold both halves of that contract in place.

use cosmix_mix::ast::{BinOp, Expr, StmtKind};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;

fn parse_first_statement(source: &str) -> cosmix_mix::ast::Stmt {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().expect("source must lex");
    let mut program = Parser::new(tokens, source)
        .parse_program()
        .expect("source must parse");
    assert!(!program.is_empty(), "source produced no statements");
    program.remove(0)
}

/// `Expr` keeps no `PartialEq`, so pull the literal out by shape.
fn as_string_literal(expr: &Expr) -> &str {
    match expr {
        Expr::StringLiteral(s) => s,
        other => panic!("expected a string literal, got {other:?}"),
    }
}

fn as_binary_sub(expr: &Expr) -> (&Expr, &Expr) {
    match expr {
        Expr::BinaryOp {
            left,
            op: BinOp::Sub,
            right,
        } => (left, right),
        other => panic!("expected subtraction, got {other:?}"),
    }
}

/// The motivating line, verbatim: hyphenated bare target, dotted verb,
/// kwargs. The target must arrive whole — `comp-vt2`, never `comp` minus
/// `vt2` (the old parse died at RUNTIME with "cannot use 'comp' as
/// number").
#[test]
fn a_bare_hyphenated_target_parses_as_one_name() {
    let stmt = parse_first_statement("send comp-vt2 comp.input.pointer.move x=50 y=50");
    let StmtKind::Send { target, command, args } = &stmt.kind else {
        panic!("expected a Send statement, got {:?}", stmt.kind);
    };
    assert_eq!(as_string_literal(target), "comp-vt2");
    assert_eq!(as_string_literal(command), "comp.input.pointer.move");
    assert_eq!(args.len(), 2);
    assert_eq!(args[0].0, "x");
    assert_eq!(args[1].0, "y");
}

/// `emit` and `address` route through the same `parse_send_target`, and
/// the `$r = send …` expression form through its own caller — pin all
/// three so a regression in any one shows up here, not just in `send`.
#[test]
fn emit_address_and_send_expression_take_the_same_target_path() {
    let stmt = parse_first_statement("emit quoin-shot-vt2 shot.capture");
    let StmtKind::Emit { target, .. } = &stmt.kind else {
        panic!("expected an Emit statement, got {:?}", stmt.kind);
    };
    assert_eq!(as_string_literal(target), "quoin-shot-vt2");

    let stmt = parse_first_statement("address comp-nested\n  ping timeout=1\nend");
    let StmtKind::Address { target, .. } = &stmt.kind else {
        panic!("expected an Address statement, got {:?}", stmt.kind);
    };
    assert_eq!(as_string_literal(target), "comp-nested");

    let stmt = parse_first_statement("$r = send comp-vt2 ping timeout=1");
    let StmtKind::Assignment { name, value } = &stmt.kind else {
        panic!("expected an Assignment, got {:?}", stmt.kind);
    };
    assert_eq!(name, "r");
    let Expr::Send { target, .. } = value else {
        panic!("expected a send expression, got {value:?}");
    };
    assert_eq!(as_string_literal(target), "comp-vt2");
}

/// Subtraction is untouched EVERYWHERE except the tight-hyphen target
/// shape: a spaced `a - b` in target position still parses as the
/// subtraction it always was (and still fails at runtime on strings), and
/// an ordinary tight `a-b` EXPRESSION still parses as subtraction too.
#[test]
fn subtraction_keeps_its_meaning_outside_the_target_shape() {
    let stmt = parse_first_statement("send a - b ping");
    let StmtKind::Send { target, .. } = &stmt.kind else {
        panic!("expected a Send statement, got {:?}", stmt.kind);
    };
    let (left, right) = as_binary_sub(target);
    assert_eq!(as_string_literal(left), "a");
    assert_eq!(as_string_literal(right), "b");

    let stmt = parse_first_statement("$x = 10 - 4");
    let StmtKind::Assignment { value, .. } = &stmt.kind else {
        panic!("expected an Assignment, got {:?}", stmt.kind);
    };
    let (left, right) = as_binary_sub(value);
    assert!(matches!(left, Expr::NumberLiteral(n) if *n == 10.0));
    assert!(matches!(right, Expr::NumberLiteral(n) if *n == 4.0));

    // The old bug's exact shape, in NON-target position: still an
    // expression, still subtraction — the fix is scoped to target position.
    let stmt = parse_first_statement("$x = a-b");
    let StmtKind::Assignment { value, .. } = &stmt.kind else {
        panic!("expected an Assignment, got {:?}", stmt.kind);
    };
    let (left, right) = as_binary_sub(value);
    assert_eq!(as_string_literal(left), "a");
    assert_eq!(as_string_literal(right), "b");
}

/// A malformed-number segment (`007`, `1.2.3`) in the bare target used to
/// be refused by the LEXER (`ambiguous leading-zero number '007'`) before
/// the parser's hyphen scan could read the word — an error naming neither
/// `send` nor the service. It now lexes as a word segment in that position
/// only, for all three keywords.
#[test]
fn malformed_number_segments_in_the_bare_target_parse_whole() {
    for (src, want) in [
        ("send node-007 ping", "node-007"),
        ("send node-7-x ping", "node-7-x"),
        ("send svc-01 ping", "svc-01"),
        ("send a-1.2.3 ping", "a-1.2.3"),
    ] {
        let stmt = parse_first_statement(src);
        let StmtKind::Send { target, command, .. } = &stmt.kind else {
            panic!("expected a Send statement for {src:?}, got {:?}", stmt.kind);
        };
        assert_eq!(as_string_literal(target), want);
        assert_eq!(as_string_literal(command), "ping");
    }
    let stmt = parse_first_statement("emit node-007 ping");
    let StmtKind::Emit { target, .. } = &stmt.kind else {
        panic!("expected an Emit statement, got {:?}", stmt.kind);
    };
    assert_eq!(as_string_literal(target), "node-007");
    let stmt = parse_first_statement("address node-007\n  ping\nend");
    let StmtKind::Address { target, .. } = &stmt.kind else {
        panic!("expected an Address statement, got {:?}", stmt.kind);
    };
    assert_eq!(as_string_literal(target), "node-007");

    // Outside the bare target the refusal is unchanged.
    let err = Lexer::new("$x = 007").tokenize().expect_err("007 is still refused");
    assert!(format!("{err}").contains("ambiguous leading-zero number '007'"), "{err}");
}
