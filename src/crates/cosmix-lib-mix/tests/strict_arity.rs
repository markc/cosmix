//! Strict arity mode (0.29.0, decision D5): ArityMode::Strict raises
//! ARITY_MISMATCH before entering a user function and enforces builtin
//! contract arity; Compatible (default) keeps missing->nil /
//! extra-ignored.

use cosmix_mix::error::MixError;
use cosmix_mix::evaluator::{ArityMode, Evaluator, SharedBuf};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;

async fn run_mode(source: &str, mode: ArityMode) -> Result<String, MixError> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize()?;
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program()?;
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    eval.set_arity_mode(mode);
    eval.execute(&stmts).await?;
    Ok(stdout.to_string_lossy())
}

async fn strict_ok(source: &str) -> String {
    match run_mode(source, ArityMode::Strict).await {
        Ok(out) => out,
        Err(e) => panic!("strict script should succeed, got: {e}"),
    }
}

#[tokio::test]
async fn compatible_mode_unchanged() {
    let src = "function f($a, $b)\n  return \"\" .. $a .. \"/\" .. $b\nend\nprint(f(1))\nprint(f(1, 2, 3))\n";
    let out = run_mode(src, ArityMode::Compatible).await.unwrap();
    assert_eq!(out, "1/nil\n1/2\n");
}

#[tokio::test]
async fn strict_raises_on_missing_and_extra() {
    for (call, expect_range) in [("f(1)", "2"), ("f(1, 2, 3)", "2")] {
        let src = format!(
            "function f($a, $b)\n  return $a\nend\ntry\n  {call}\ncatch $m, $e\n  print($e.code .. \" \" .. (pos(\"expected {expect_range} argument\", $m) > 0))\nend\n"
        );
        assert_eq!(strict_ok(&src).await, "ARITY_MISMATCH true\n", "{call}");
    }
}

#[tokio::test]
async fn strict_respects_defaults() {
    let src = "function f($a, $b = 9)\n  return \"\" .. $a .. \"/\" .. $b\nend\nprint(f(1))\nprint(f(1, 2))\ntry\n  f()\ncatch $m, $e\n  print($e.code)\nend\n";
    assert_eq!(strict_ok(src).await, "1/9\n1/2\nARITY_MISMATCH\n");
}

#[tokio::test]
async fn strict_rejects_builtin_surplus() {
    // A plain pure builtin, plus run_stream's genuinely-surplus THIRD
    // argument. run_stream's classic case — the silently-ignored `timeout`
    // that motivated D5 — stopped being an arity question in v0.51.0, when it
    // gained an {env, clear_env, cwd} opts map: `timeout` is now refused BY
    // NAME as OPTION_INVALID, in every mode rather than only under
    // --strict-arity. That is strictly stronger, so the case is kept here as
    // the third arm rather than dropped.
    let src = "try\n  upper(\"a\", \"b\")\ncatch $m, $e\n  print($e.code)\nend\ntry\n  run_stream([\"true\"], {}, 5)\ncatch $m, $e\n  print($e.code)\nend\ntry\n  run_stream([\"true\"], {timeout: 5})\ncatch $m, $e\n  print($e.code)\nend\n";
    assert_eq!(
        strict_ok(src).await,
        "ARITY_MISMATCH\nARITY_MISMATCH\nOPTION_INVALID\n"
    );
}

#[tokio::test]
async fn strict_accepts_run_stream_options_map() {
    // run_stream's contract max arity moved 1 -> 2 in v0.51.0. Under strict
    // arity the pre-0.51.0 contract REJECTS this call, so the test fails
    // against a build that never gained the options map — unlike a
    // compatible-mode call, which such a build would run while quietly
    // dropping the map. The env probe proves the option is applied, not
    // merely accepted.
    let src = "print(run_stream([\"/bin/sh\", \"-c\", \"test x$MIX_SA_PROBE = xy\"], {env: {MIX_SA_PROBE: \"y\"}}))\n";
    assert_eq!(strict_ok(src).await, "0\n");
}

#[tokio::test]
async fn strict_allows_exact_and_variadic_calls() {
    let src = "print(upper(\"a\"))\nprint(fmt(\"%s-%s\", 1, 2))\nprint(min(3, 1, 2))\n";
    assert_eq!(strict_ok(src).await, "A\n1-2\n1\n");
}

#[tokio::test]
async fn strict_honors_exact_arity_sets() {
    let src = "print(random(1, 5) >= 1)\ntry\n  random(1)\ncatch $m, $e\n  print($e.code)\nend\n";
    assert_eq!(strict_ok(src).await, "true\nARITY_MISMATCH\n");
}

#[tokio::test]
async fn strict_rejects_inline_builtin_surplus() {
    // codex release review MAJOR: inline special forms (pop/sleep/
    // printf/db/jmap/...) bypassed the later dispatch-branch checks.
    // The gate now runs at the top of the FunctionCall arm.
    let src = "$xs = [1, 2]\ntry\n  pop($xs, 99)\ncatch $m, $e\n  print($e.code)\nend\ntry\n  sleep(0, 99)\ncatch $m, $e\n  print($e.code)\nend\ntry\n  printf()\ncatch $m, $e\n  print($e.code)\nend\n";
    assert_eq!(
        strict_ok(src).await,
        "ARITY_MISMATCH\nARITY_MISMATCH\nARITY_MISMATCH\n"
    );
}

#[tokio::test]
async fn strict_min_arity_with_non_trailing_default() {
    // codex release review MAJOR: a required param after a defaulted
    // one — min must be "index past last required", so 2 args required.
    let src = "function f($a = 1, $b)\n  return $b\nend\ntry\n  f(9)\ncatch $m, $e\n  print($e.code)\nend\nprint(f(9, 8))\n";
    assert_eq!(strict_ok(src).await, "ARITY_MISMATCH\n8\n");
}

#[tokio::test]
async fn compatible_builtin_surplus_still_tolerated() {
    let out = run_mode("print(upper(\"a\", \"junk\"))\n", ArityMode::Compatible)
        .await
        .unwrap();
    assert_eq!(out, "A\n");
}
