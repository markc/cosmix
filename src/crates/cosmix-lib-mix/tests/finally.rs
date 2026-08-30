//! `finally` clause (0.30.0, decision D6): runs on every exit path of a
//! `try` — normal completion, caught error, and propagating
//! error/return/break/continue/exit — except panic(). A finally error
//! overrides the pending outcome with a displaced error as `cause` when
//! that outcome was itself an error.

use cosmix_mix::error::MixError;
use cosmix_mix::evaluator::{Evaluator, SharedBuf};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;

async fn run(source: &str) -> Result<String, MixError> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize()?;
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program()?;
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    eval.execute(&stmts).await?;
    Ok(stdout.to_string_lossy())
}

async fn run_ok(source: &str) -> String {
    run(source).await.expect("script should succeed")
}

async fn run_exit(source: &str) -> (i32, String) {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().unwrap();
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program().unwrap();
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr));
    let err = eval.execute(&stmts).await.expect_err("exit must unwind");
    let MixError::ExitRequest { code } = err else {
        panic!("expected ExitRequest, got {err:?}");
    };
    (code, stdout.to_string_lossy())
}

#[tokio::test]
async fn finally_runs_on_normal_completion() {
    let out =
        run_ok("try\n  print(\"body\")\nfinally\n  print(\"cleanup\")\nend\nprint(\"after\")\n")
            .await;
    assert_eq!(out, "body\ncleanup\nafter\n");
}

#[tokio::test]
async fn finally_runs_on_caught_error() {
    let out = run_ok(
        "try\n  die(\"boom\")\ncatch $m\n  print(\"caught: \" .. $m)\nfinally\n  print(\"cleanup\")\nend\nprint(\"after\")\n",
    )
    .await;
    assert_eq!(out, "caught: boom\ncleanup\nafter\n");
}

#[tokio::test]
async fn finally_runs_then_error_propagates_without_catch() {
    // try/finally with NO catch: finally runs, error still propagates.
    let src = "try\n  die(\"boom\")\nfinally\n  print(\"cleanup\")\nend\nprint(\"unreached\")\n";
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let tokens = Lexer::new(src).tokenize().unwrap();
    let stmts = Parser::new(tokens, src).parse_program().unwrap();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    let err = eval.execute(&stmts).await.expect_err("propagates");
    assert_eq!(stdout.to_string_lossy(), "cleanup\n");
    assert!(err.to_string().contains("boom"));
}

#[tokio::test]
async fn finally_runs_on_return_from_function() {
    let out = run_ok(
        "function f()\n  try\n    return \"early\"\n  finally\n    print(\"cleanup\")\n  end\nend\nprint(f())\n",
    )
    .await;
    assert_eq!(out, "cleanup\nearly\n");
}

#[tokio::test]
async fn finally_runs_on_break() {
    let out = run_ok(
        "for $i = 1 to 3\n  try\n    if $i == 2 then break end\n    print(\"body \" .. $i)\n  finally\n    print(\"fin \" .. $i)\n  end\nend\nprint(\"done\")\n",
    )
    .await;
    assert_eq!(out, "body 1\nfin 1\nfin 2\ndone\n");
}

#[tokio::test]
async fn finally_runs_on_continue() {
    let out = run_ok(
        "for $i = 1 to 3\n  try\n    if $i == 2 then continue end\n    print(\"body \" .. $i)\n  finally\n    print(\"fin \" .. $i)\n  end\nend\nprint(\"done\")\n",
    )
    .await;
    // fin runs for every iteration incl. the continued one; body skips i=2.
    assert_eq!(out, "body 1\nfin 1\nfin 2\nbody 3\nfin 3\ndone\n");
}

#[tokio::test]
async fn exit_request_runs_finally() {
    let (code, out) =
        run_exit("try\n  print(\"body\")\n  exit(3)\nfinally\n  print(\"FINALLY RAN\")\nend\n")
            .await;
    assert_eq!(code, 3);
    assert_eq!(out, "body\nFINALLY RAN\n");
}

#[tokio::test]
async fn exit_request_crosses_function_boundary() {
    let (code, out) = run_exit(
        "function stop_now()\n  exit(4)\nend\ntry\n  stop_now()\nfinally\n  print(\"cleanup\")\nend\n",
    )
    .await;
    assert_eq!(code, 4);
    assert_eq!(out, "cleanup\n");
}

#[tokio::test]
async fn nested_finally_order_is_innermost_first() {
    let (code, out) = run_exit(
        "try\n  try\n    exit(5)\n  finally\n    print(\"inner\")\n  end\nfinally\n  print(\"outer\")\nend\n",
    )
    .await;
    assert_eq!(code, 5);
    assert_eq!(out, "inner\nouter\n");
}

#[tokio::test]
async fn catch_does_not_catch_exit_request() {
    let (code, out) = run_exit(
        "try\n  exit(6)\ncatch $m\n  print(\"CAUGHT: \" .. $m)\nfinally\n  print(\"cleanup\")\nend\n",
    )
    .await;
    assert_eq!(code, 6);
    assert_eq!(out, "cleanup\n");
}

#[tokio::test]
async fn loop_does_not_treat_exit_request_as_break() {
    let (code, out) = run_exit(
        "for $i = 1 to 3\n  try\n    print(\"iteration \" .. $i)\n    exit(7)\n  finally\n    print(\"cleanup \" .. $i)\n  end\nend\nprint(\"after loop\")\n",
    )
    .await;
    assert_eq!(code, 7);
    assert_eq!(out, "iteration 1\ncleanup 1\n");
}

#[tokio::test]
async fn exit_inside_catch_unwinds_through_finally() {
    let (code, out) = run_exit(
        "try\n  die(\"original\")\ncatch $m\n  print(\"caught: \" .. $m)\n  exit(8)\nfinally\n  print(\"cleanup\")\nend\n",
    )
    .await;
    assert_eq!(code, 8);
    assert_eq!(out, "caught: original\ncleanup\n");
}

#[tokio::test]
async fn exit_from_finally_replaces_pending_exit() {
    let (code, out) =
        run_exit("try\n  exit(2)\nfinally\n  print(\"replacing\")\n  exit(9)\nend\n").await;
    assert_eq!(code, 9);
    assert_eq!(out, "replacing\n");
}

#[tokio::test]
async fn finally_error_overrides_exit_without_control_flow_cause() {
    let out = run_ok(
        "try\n  try\n    exit(2)\n  finally\n    raise(\"CLEANUP_FAILED\", \"cleanup broke\")\n  end\ncatch $m, $e\n  print($e.code)\n  print($e.cause)\nend\nprint(\"after\")\n",
    )
    .await;
    assert_eq!(out, "CLEANUP_FAILED\nnil\nafter\n");
}

#[tokio::test]
async fn inline_try_finally_parses() {
    // codex 0.30 review: `finally` must be an inline statement terminator.
    let out = run_ok("try print(\"body\") finally print(\"fin\") end\n").await;
    assert_eq!(out, "body\nfin\n");
    let out = run_ok("try print(\"b\") catch $m print(\"c\") finally print(\"f\") end\n").await;
    assert_eq!(out, "b\nf\n");
}

#[tokio::test]
async fn finally_error_overrides_with_cause() {
    let out = run_ok(
        "try\n  die(\"original\")\ncatch $m, $e\n  print(\"outer caught: \" .. $m)\nend\ntry\n  try\n    die(\"original\")\n  finally\n    raise(\"CLEANUP_FAILED\", \"cleanup broke\")\n  end\ncatch $m, $e\n  print($e.code)\n  print($e.cause.message)\nend\n",
    )
    .await;
    assert_eq!(out, "outer caught: original\nCLEANUP_FAILED\noriginal\n");
}

#[tokio::test]
async fn bare_try_end_is_a_parse_error() {
    let src = "try\n  print(1)\nend\n";
    let tokens = Lexer::new(src).tokenize().unwrap();
    let err = Parser::new(tokens, src)
        .parse_program()
        .expect_err("no catch/finally");
    assert!(err.to_string().contains("catch and/or finally"), "{err}");
}

#[tokio::test]
async fn catch_still_works_without_finally() {
    let out = run_ok("try\n  die(\"x\")\ncatch $m\n  print(\"c: \" .. $m)\nend\n").await;
    assert_eq!(out, "c: x\n");
}
