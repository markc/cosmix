//! 09-25 entry: string interpolation can now take an index or a call —
//! `${a[0]}`, `${m.k}`, `${f()}`, `${m.k[1]}` — instead of reporting
//! "undefined variable '$a[0]'".

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
async fn interpolation_takes_indexes_and_calls() {
    let out = run("$a = [10, 20]\nprint(\"x${a[0]}y\")\n").await.expect("index");
    assert!(out.contains("x10y"), "got: {out}");

    let out = run("$m = {k: \"K\"}\nprint(\"v=${m.k}\")\n").await.expect("field");
    assert!(out.contains("v=K"), "got: {out}");

    let out = run("fn f()\n  return \"F\"\nend\nprint(\"c=${f()}\")\n")
        .await
        .expect("call");
    assert!(out.contains("c=F"), "got: {out}");

    let out = run("$m = {k: [7, 8]}\nprint(\"n=${m.k[1]}\")\n").await.expect("field+index");
    assert!(out.contains("n=8"), "got: {out}");

    // Coalescing still works alongside the new shapes.
    let out = run("$a = [1]\nprint(\"d=${a[9] ?? \"fallback\"}\")\n")
        .await
        .expect("coalesce");
    assert!(out.contains("d=fallback"), "got: {out}");
}
