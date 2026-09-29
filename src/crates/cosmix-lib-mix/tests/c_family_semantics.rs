//! C-family semantics (TODO-mix sweep C1, C4): nil arithmetic raises
//! instead of silently stringifying; duplicate literal map keys are a
//! parse error instead of a silent overwrite.

use cosmix_mix::evaluator::Evaluator;
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;

async fn run(src: &str) -> Result<String, String> {
    let mut lexer = Lexer::new(src);
    let tokens = lexer.tokenize().map_err(|e| e.to_string())?;
    let mut parser = Parser::new(tokens, src);
    let stmts = parser.parse_program().map_err(|e| e.to_string())?;
    let stdout = cosmix_mix::evaluator::SharedBuf::new();
    let stderr = cosmix_mix::evaluator::SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    eval.execute(&stmts).await.map_err(|e| e.to_string())?;
    Ok(stdout.to_string_lossy())
}

#[tokio::test]
async fn nil_arithmetic_raises() {
    let err = run("print(nil + 1)\n").await.expect_err("nil + 1 must raise");
    assert!(err.contains("nil is not a number"), "got: {err}");
    // The `??` guard is the fix the message teaches (bound-nil shape).
    let out = run("$n = nil\nprint(($n ?? 0) + 1)\n").await.expect("guarded arithmetic");
    assert!(out.contains("1"), "got: {out}");
    // String concat stays `..`; nil via `..` is untouched.
    let out = run("print(\"x\" .. nil)\n").await.expect(".. concat");
    assert!(out.contains("xnil"), "got: {out}");
}

#[tokio::test]
async fn duplicate_map_literal_keys_are_a_parse_error() {
    let err = run("{ok: false, ok: true}\n").await.expect_err("duplicate key must fail");
    assert!(err.contains("duplicate map key 'ok'"), "got: {err}");
    // merge() stays the deliberate last-wins spelling.
    let out = run("print(merge({ok: false}, {ok: true}).ok)\n").await.expect("merge");
    assert!(out.contains("true"), "got: {out}");
}
