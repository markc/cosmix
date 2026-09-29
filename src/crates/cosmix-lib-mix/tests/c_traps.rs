//! C6/C8/C9/C10 (TODO-mix sweep): parse match status, `.fn` spelling,
//! and the collection-literal traps.

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
async fn parse_with_reports_a_match_status() {
    // C6: a matching template sets $parse_ok; a missing delimiter does
    // not (while keeping the lenient bindings).
    let out = run("parse \"a:b\" with $a \":\" $b\nprint($parse_ok)\n")
        .await
        .expect("match");
    assert!(out.contains("true"), "got: {out}");
    let out = run("parse \"abc\" with $a \":\" $b\nprint($parse_ok)\n")
        .await
        .expect("mismatch");
    assert!(out.contains("false"), "got: {out}");
}

#[tokio::test]
async fn dot_fn_reads_the_fn_key_not_function() {
    // C8: .fn must read the "fn" key; .function the "function" key.
    let out = run("print({\"fn\":1,\"function\":2}.fn)\n").await.expect(".fn");
    assert!(out.contains('1'), "got: {out}");
    let out = run("print({\"fn\":1,\"function\":2}.function)\n")
        .await
        .expect(".function");
    assert!(out.contains('2'), "got: {out}");
}
