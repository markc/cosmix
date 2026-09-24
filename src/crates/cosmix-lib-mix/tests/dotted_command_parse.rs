//! Keywords after a command-name dot are literal name segments.
use cosmix_mix::ast::{Expr, StmtKind};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;

#[test]
fn keyword_segments_in_handlers_and_sends_are_literal() {
    for name in ["a.next", "a.end.b", "a.if", "a.for.b", "a.fn", "lib.fn", "a.function"] {
        for source in [format!("on {name}\nend"), format!("send svc {name}")] {
            let tokens = Lexer::new(&source).tokenize().unwrap();
            let program = Parser::new(tokens, &source).parse_program().unwrap();
            match &program[0].kind {
                StmtKind::On { command, .. } => assert_eq!(command, name),
                StmtKind::Send { command: Expr::StringLiteral(command), .. } => {
                    assert_eq!(command, name);
                }
                other => panic!("expected literal command, got {other:?}"),
            }
        }
    }
}

#[test]
fn bare_keyword_is_not_a_command_statement() {
    for source in ["end", "on next\nend", "send svc next", "on a.\nend"] {
        let tokens = Lexer::new(source).tokenize().unwrap();
        assert!(Parser::new(tokens, source).parse_program().is_err(), "{source}");
    }
}

#[test]
fn keyword_segments_do_not_consume_handler_terminators() {
    for name in ["a.end", "a.then"] {
        let source = format!("on {name}\n  reply(\"ok\")\nend");
        let tokens = Lexer::new(&source).tokenize().unwrap();
        let program = Parser::new(tokens, &source).parse_program().unwrap();
        assert_eq!(program.len(), 1);
        let StmtKind::On { command, body, .. } = &program[0].kind else {
            panic!("expected one handler");
        };
        assert_eq!(command, name);
        assert_eq!(body.len(), 1);
        assert!(matches!(&body[0].kind, StmtKind::Expression(Expr::FunctionCall { name, .. }) if name == "reply"));
    }
}

#[test]
fn keyword_segments_in_emit_address_and_alias_are_literal() {
    for name in ["a.next", "a.end", "a.then", "a.fn"] {
        for source in [
            format!("emit svc {name}"),
            format!("address svc\n{name}\nend"),
            format!("alias {name} = \"print(1)\""),
        ] {
            let tokens = Lexer::new(&source).tokenize().unwrap();
            let program = Parser::new(tokens, &source).parse_program().unwrap();
            assert_eq!(program.len(), 1);
            let command = match &program[0].kind {
                StmtKind::Emit { command, .. } => command,
                StmtKind::Alias { name: Some(name), .. } => name,
                StmtKind::Address { body, .. } => {
                    assert_eq!(body.len(), 1);
                    let StmtKind::Send { command, .. } = &body[0].kind else {
                        panic!("expected implicit send");
                    };
                    command
                }
                other => panic!("unexpected statement: {other:?}"),
            };
            assert!(matches!(command, Expr::StringLiteral(value) if value == name));
        }
    }
}

#[test]
fn expression_keyword_fields_keep_field_access_semantics() {
    for (source, expected) in [
        ("$result = $x.end", "end"),
        ("$result = foo().then", "then"),
        ("$result = $x.fn", "function"),
    ] {
        let tokens = Lexer::new(source).tokenize().unwrap();
        let program = Parser::new(tokens, source).parse_program().unwrap();
        let StmtKind::Assignment { value: Expr::FieldAccess { object, field }, .. } = &program[0].kind else {
            panic!("expected field access: {program:?}");
        };
        assert_eq!(field, expected);
        if expected == "then" {
            assert!(matches!(object.as_ref(), Expr::FunctionCall { name, args } if name == "foo" && args.is_empty()));
        } else {
            assert!(matches!(object.as_ref(), Expr::Variable(name) if name == "x"));
        }
    }
}
