//! Assignment statements must never become any operand of `&&` / `||`.

use cosmix_mix::MixError;
use cosmix_mix::ast::StmtKind;
use cosmix_mix::error::assignment_chain_parse_message;
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;

fn parse(source: &str) -> Result<Vec<cosmix_mix::ast::Stmt>, MixError> {
    let tokens = Lexer::new(source).tokenize().expect("source must lex");
    Parser::new(tokens, source).parse_program()
}

#[test]
fn every_assignment_shape_rejects_both_operators_in_every_chain_position() {
    let assignments = [
        "$x = true",
        "$m.ok = true",
        "$m[\"ok\"] = true",
        "$m.inner.ok = true",
    ];

    for assignment in assignments {
        for operator in ["&&", "||"] {
            for position in 0..3 {
                let mut operands = [
                    "print(\"first\")".to_string(),
                    "print(\"middle\")".to_string(),
                    "print(\"last\")".to_string(),
                ];
                operands[position] = assignment.to_string();
                let source = operands.join(&format!(" {operator} "));
                let expected_column = if position == 2 {
                    source.rfind(operator).unwrap() + 1
                } else {
                    source.find(operator).unwrap() + 1
                };
                let error = parse(&source).expect_err("assignment chain must be rejected");
                assert_eq!(
                    error.to_string(),
                    format!(
                        "Parse error at line 1:{expected_column}: {}",
                        assignment_chain_parse_message(operator)
                    ),
                    "source: {source}"
                );
                match error {
                    MixError::AssignmentChainParseError {
                        operator: actual,
                        span,
                    } => {
                        assert_eq!(actual, operator, "source: {source}");
                        assert_eq!(span.line, 1, "source: {source}");
                        assert_eq!(
                            span.column, expected_column,
                            "span must point at the connector that admitted the assignment in {source}"
                        );
                    }
                    other => {
                        panic!("expected AssignmentChainParseError for {source}, got {other:?}")
                    }
                }
            }
        }
    }
}

#[test]
fn assignment_operands_are_rejected_inside_nested_statements_and_pipelines() {
    for source in [
        "if true then\n  print(\"gate\") && $x = false\nend",
        "print(\"gate\") && $x = false | /usr/bin/cat",
    ] {
        match parse(source).expect_err("nested assignment chain must be rejected") {
            MixError::AssignmentChainParseError { operator, span } => {
                assert_eq!(operator, "&&", "source: {source}");
                if source.starts_with("if") {
                    assert_eq!(span.line, 2, "source: {source}");
                } else {
                    assert_eq!(span.line, 1, "source: {source}");
                }
            }
            other => panic!("expected typed assignment-chain error, got {other:?}"),
        }
    }
}

#[test]
fn non_assignment_mix_chains_keep_both_operators_and_newline_continuation() {
    for operator in ["&&", "||"] {
        for source in [
            format!("print(\"left\") {operator} print(\"right\")"),
            format!("print(\"left\")\n{operator}\nprint(\"right\")"),
            format!("print(\"left\") {operator}\nprint(\"right\")"),
        ] {
            let stmts = parse(&source).expect("ordinary statement chain must parse");
            assert_eq!(stmts.len(), 1, "source: {source}");
            assert!(matches!(stmts[0].kind, StmtKind::Chain { .. }));
        }
    }
}

/// `export x = v` and the `alias n = c` DEFINE form bind a value, so they hide
/// the same falsy operand behind a green chain that `$x = v` did — the keyword
/// is the only difference. The alias QUERY and LIST forms bind nothing and
/// must stay legal chain operands.
#[test]
fn value_binding_keyword_forms_are_assignments_too() {
    for operator in ["&&", "||"] {
        for binding in ["export x = false", "alias xx = \"false\""] {
            for source in [
                format!("{binding} {operator} print(\"tail\")"),
                format!("print(\"head\") {operator} {binding}"),
            ] {
                let error = parse(&source).expect_err(&format!("{source:?} must be rejected"));
                assert!(
                    error.is_assignment_chain_parse_error(),
                    "{source:?} produced {error}"
                );
            }
        }
    }
}

#[test]
fn non_binding_alias_forms_stay_legal_chain_operands() {
    for source in [
        "alias somename || print(\"tail\")",
        "alias || print(\"tail\")",
        "print(\"head\") && alias somename",
    ] {
        let stmts = parse(source).unwrap_or_else(|e| panic!("{source:?} must parse: {e}"));
        assert!(
            matches!(stmts[0].kind, StmtKind::Chain { .. }),
            "{source:?} must stay a chain"
        );
    }
}

/// The terse `function f() = expr` form binds a value with `=` exactly as
/// `export`/`alias` do, and hid the same falsy operand: `function f() =
/// false || fallback()` bound `false`, dropped the fallback, and exited 0.
#[test]
fn terse_expression_bodied_function_defs_are_assignments_too() {
    for operator in ["&&", "||"] {
        for source in [
            format!("function f() = false {operator} print(\"tail\")"),
            format!("print(\"head\") {operator} function f() = false"),
        ] {
            let error = parse(&source).expect_err(&format!("{source:?} must be rejected"));
            assert!(
                error.is_assignment_chain_parse_error(),
                "{source:?} produced {error}"
            );
        }
    }
}

/// …but the BLOCK form binds no `=` expression and is not assignment-shaped,
/// so it stays a legal operand. Rejecting every binder would be a materially
/// broader policy than the one documented in `mix man syntax`.
#[test]
fn block_bodied_function_defs_stay_legal_chain_operands() {
    for source in [
        "function f() return 1 end && print(\"tail\")",
        "print(\"head\") || function f() return 1 end",
    ] {
        let stmts = parse(source).unwrap_or_else(|e| panic!("{source:?} must parse: {e}"));
        assert!(
            matches!(stmts[0].kind, StmtKind::Chain { .. }),
            "{source:?} must stay a chain"
        );
    }
}
