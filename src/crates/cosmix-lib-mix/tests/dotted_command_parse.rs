//! Keywords after a command-name dot are literal name segments.
use cosmix_mix::ast::{Expr, StmtKind};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;

#[test]
fn keyword_segments_in_handlers_and_sends_are_literal() {
    for name in ["a.next", "a.end.b", "a.if", "a.for.b"] {
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
    let source = "end";
    let tokens = Lexer::new(source).tokenize().unwrap();
    assert!(Parser::new(tokens, source).parse_program().is_err());
}
