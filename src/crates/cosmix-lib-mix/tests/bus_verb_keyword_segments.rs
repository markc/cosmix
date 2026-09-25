//! Parser pin: a dotted Bus name may contain Mix keywords.
//!
//! Bus verbs are dotted identifiers owned by the services that define them,
//! so `ced.select`, `x.if`, `a.print.end` are ordinary names. The lexer is
//! context-free and emits keyword tokens for those segments; before 0.96.2
//! the dotted-name loops only took bareword segments, so `send ced
//! ced.select anchor=0` died with "unexpected token Dot". These tests hold
//! every bare-name position (send/emit command, `$r = send`, address body,
//! dotted target, `on` header) and the regression half: ordinary uses of the
//! same keywords still parse as before.

use cosmix_mix::ast::{Expr, Stmt, StmtKind};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;

fn parse(source: &str) -> Vec<Stmt> {
    let tokens = Lexer::new(source).tokenize().expect("source must lex");
    Parser::new(tokens, source)
        .parse_program()
        .unwrap_or_else(|e| panic!("{source:?} must parse: {e:?}"))
}

fn parse_err(source: &str) -> bool {
    let Ok(tokens) = Lexer::new(source).tokenize() else {
        return true;
    };
    Parser::new(tokens, source).parse_program().is_err()
}

fn lit(expr: &Expr) -> &str {
    match expr {
        Expr::StringLiteral(s) => s,
        other => panic!("expected a string literal, got {other:?}"),
    }
}

fn send_parts(stmt: &Stmt) -> (&str, &str, Vec<&str>) {
    let StmtKind::Send {
        target,
        command,
        args,
    } = &stmt.kind
    else {
        panic!("expected Send, got {:?}", stmt.kind);
    };
    (
        lit(target),
        lit(command),
        args.iter().map(|(k, _)| k.as_str()).collect(),
    )
}

const KEYWORD_VERBS: &[&str] = &[
    "ced.select",
    "x.if",
    "x.for",
    "x.print",
    "x.end",
    "x.on",
    "edit.select",
    "a.if.for",
    "a.print.end.on",
    "x.fn",
    "x.function",
    "x.eq",
];

#[test]
fn send_command_accepts_keyword_segments() {
    for verb in KEYWORD_VERBS {
        let src = format!("send svc {verb} anchor=0");
        let prog = parse(&src);
        assert_eq!(prog.len(), 1, "{src}");
        let (target, command, args) = send_parts(&prog[0]);
        assert_eq!(target, "svc");
        assert_eq!(command, *verb, "{src}");
        assert_eq!(args, vec!["anchor"], "{src}");
    }
}

#[test]
fn the_motivating_line_parses() {
    let prog = parse("send ced ced.select anchor=0");
    assert_eq!(send_parts(&prog[0]), ("ced", "ced.select", vec!["anchor"]));
}

#[test]
fn emit_send_expression_and_address_body_accept_keyword_segments() {
    let prog = parse("emit x x.if.for a=1");
    let StmtKind::Emit { command, .. } = &prog[0].kind else {
        panic!("expected Emit, got {:?}", prog[0].kind);
    };
    assert_eq!(lit(command), "x.if.for");

    let prog = parse("$r = send ced ced.select anchor=0");
    let StmtKind::Assignment { value, .. } = &prog[0].kind else {
        panic!("expected Assignment, got {:?}", prog[0].kind);
    };
    let Expr::Send { command, .. } = value else {
        panic!("expected send expression, got {value:?}");
    };
    assert_eq!(lit(command), "ced.select");

    // Body lines, including one LED by `end.` — that is a verb, not the
    // block's closing `end`.
    let prog = parse("address ced\n  ced.select anchor=0\n  end.of.line x=1\n  print.page\nend");
    let StmtKind::Address { target, body } = &prog[0].kind else {
        panic!("expected Address, got {:?}", prog[0].kind);
    };
    assert_eq!(lit(target), "ced");
    let verbs: Vec<&str> = body
        .iter()
        .map(|s| match &s.kind {
            StmtKind::Send { command, .. } => lit(command),
            other => panic!("expected Send, got {other:?}"),
        })
        .collect();
    assert_eq!(verbs, vec!["ced.select", "end.of.line", "print.page"]);
}

#[test]
fn keyword_led_dotted_verbs_and_targets() {
    let prog = parse("send svc select.all");
    assert_eq!(send_parts(&prog[0]), ("svc", "select.all", vec![]));

    let prog = parse("send print.x if.y a=1");
    assert_eq!(send_parts(&prog[0]), ("print.x", "if.y", vec!["a"]));

    let prog = parse("send noded.if.bus noded.ping");
    assert_eq!(send_parts(&prog[0]), ("noded.if.bus", "noded.ping", vec![]));
}

#[test]
fn on_header_accepts_keyword_segments() {
    for verb in KEYWORD_VERBS.iter().chain(&["select.all", "print.page"]) {
        let src = format!("on {verb}\n  reply(\"ok\")\nend");
        let prog = parse(&src);
        let StmtKind::On { command, body, .. } = &prog[0].kind else {
            panic!("expected On, got {:?}", prog[0].kind);
        };
        assert_eq!(command, verb, "{src}");
        assert_eq!(body.len(), 1, "{src}");
    }
    // Trailer still recognised after a keyword segment.
    let prog = parse("on ced.select desc \"Select a range\" async\n  reply(1)\nend");
    let StmtKind::On {
        command,
        doc,
        is_async,
        ..
    } = &prog[0].kind
    else {
        panic!("expected On");
    };
    assert_eq!(command, "ced.select");
    assert_eq!(doc.as_deref(), Some("Select a range"));
    assert!(*is_async);
}

/// A BARE keyword verb stays refused — only dotted names are taken.
#[test]
fn bare_keyword_verbs_still_refused() {
    assert!(parse_err("send svc select"));
    assert!(parse_err("on select\n  reply(1)\nend"));
    // The quoted spelling remains the way through.
    let prog = parse("send svc \"select\"");
    assert_eq!(send_parts(&prog[0]).1, "select");
}

/// Regression: the same keywords in ordinary positions mean what they did.
#[test]
fn ordinary_keyword_uses_still_parse() {
    let prog = parse(
        "$m = {select: 1, if: 2}\n\
         if $m.select == 1 then\n  print(\"one\")\nend\n\
         for $i = 1 to 3\n  print($i)\nend\n\
         select $m.if\nwhen 2 then\n  print(\"two\")\notherwise\n  print(\"no\")\nend\n\
         on topic.delivery\n  print($event.body)\nend\n\
         print($m.if)",
    );
    let kinds: Vec<&str> = prog.iter().map(|s| s.kind.trace_name()).collect();
    assert_eq!(kinds.len(), 7, "{kinds:?}");
    assert!(matches!(prog[1].kind, StmtKind::If { .. }));
    assert!(matches!(prog[2].kind, StmtKind::For { .. }));
    assert!(matches!(prog[3].kind, StmtKind::Select { .. }));

    // `send … end`-terminated forms on one line keep ending the block.
    let prog = parse("if true then send svc a.b x=1 end");
    assert_eq!(prog.len(), 1);
    // A kwarg named after a keyword is still a kwarg, not a verb segment.
    let prog = parse("send maild mailbox.move to=1 if=2");
    assert_eq!(
        send_parts(&prog[0]),
        ("maild", "mailbox.move", vec!["to", "if"])
    );
}
