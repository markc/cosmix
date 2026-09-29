//! 09-24 entries: a nil env value removes the variable from the child
//! (the `env -u` form, all three process builtins through RunArgvOpts),
//! and the fs `remove`/`remove_dir` refuse a non-string path.

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
async fn nil_env_value_removes_the_variable() {
    // `env` prints the child environment; DISPLAY=:5 must be GONE.
    let out = run(
        "$r = run_argv([\"/usr/bin/env\"], {env: {DISPLAY: \":5\"}})\nprint(contains($r.stdout, \"DISPLAY=:5\"))\n",
    )
    .await
    .expect("set runs");
    assert!(out.contains("true"), "set env: {out}");
    let out = run(
        "$r = run_argv([\"/usr/bin/env\"], {env: {DISPLAY: nil}})\nprint(contains($r.stdout, \"DISPLAY=\"))\n",
    )
    .await
    .expect("unset runs");
    assert!(out.contains("false"), "nil env value must remove DISPLAY: {out}");
}

#[tokio::test]
async fn remove_refuses_a_non_string_path() {
    // The structured CODE is TYPE_MISMATCH (catchable via $e.code); the
    // legacy display renders the message without the code prefix.
    let err = run("remove({a: 1})\n").await.expect_err("must raise");
    assert!(err.contains("path must be a string"), "got: {err}");
    let err = run("remove_dir([1, 2])\n").await.expect_err("must raise");
    assert!(err.contains("path must be a string"), "got: {err}");
}
