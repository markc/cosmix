//! The dead-push-into-a-by-value-parameter diagnostic (mix 0.21.9).
//!
//! `push($p, x)` inside a helper mutates a throwaway copy of a by-value
//! parameter — the caller never sees it. When the mutation is provably
//! lost (result discarded AND `$p` never read again), the evaluator emits
//! a one-shot stderr warning at function-definition time. The whole point
//! is ZERO false positives, so most of these tests assert SILENCE.

use cosmix_mix::evaluator::{Evaluator, SharedBuf};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;

async fn stderr_of(source: &str) -> String {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().expect("lex");
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program().expect("parse");
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    eval.execute(&stmts).await.expect("eval");
    stderr.to_string_lossy()
}

fn warn_count(s: &str) -> usize {
    s.matches("mix: warning:").count()
}

// ---------- fires (genuine dead mutation) ----------

#[tokio::test]
async fn warns_on_dead_push_into_param() {
    // The recorded "ate the snare" pattern: helper only pushes into a
    // param, never returns or reads it.
    let e =
        stderr_of("function fat_snare($drums, $t)\n  push($drums, 1)\n  push($drums, 2)\nend\n")
            .await;
    assert_eq!(warn_count(&e), 2, "one warning per dead push; got: {e}");
    assert!(e.contains("push($drums"));
    assert!(e.contains("concat"));
}

#[tokio::test]
async fn warns_on_dead_push_inside_a_loop() {
    let e = stderr_of("function f($p)\n  for each $x in [1, 2, 3]\n    push($p, $x)\n  end\nend\n")
        .await;
    assert_eq!(warn_count(&e), 1, "got: {e}");
    assert!(e.contains("push($p"));
}

#[tokio::test]
async fn warns_only_on_the_dead_param_not_the_used_one() {
    let e = stderr_of("function f($a, $b)\n  push($a, 1)\n  return $b\nend\n").await;
    assert_eq!(warn_count(&e), 1, "got: {e}");
    assert!(e.contains("push($a"));
    assert!(!e.contains("push($b"));
}

// ---------- silent (zero false positives) ----------

#[tokio::test]
async fn silent_when_param_is_returned() {
    let e = stderr_of("function build($p)\n  push($p, 1)\n  return $p\nend\n").await;
    assert_eq!(warn_count(&e), 0, "returning the list is legit; got: {e}");
}

#[tokio::test]
async fn silent_when_param_is_read_after() {
    let e = stderr_of("function f($p)\n  push($p, 1)\n  print(length($p))\nend\n").await;
    assert_eq!(warn_count(&e), 0, "got: {e}");
}

#[tokio::test]
async fn silent_when_push_result_is_used() {
    let e = stderr_of("function f($p)\n  $y = push($p, 1)\n  return $y\nend\n").await;
    assert_eq!(
        warn_count(&e),
        0,
        "capturing push's result is legit; got: {e}"
    );
}

#[tokio::test]
async fn silent_when_param_passed_onward() {
    let e = stderr_of("function other($x) return length($x) end\nfunction f($p)\n  push($p, 1)\n  other($p)\nend\n").await;
    assert_eq!(warn_count(&e), 0, "got: {e}");
}

#[tokio::test]
async fn silent_for_local_non_param_list() {
    let e =
        stderr_of("function f()\n  $local = []\n  push($local, 1)\n  return $local\nend\n").await;
    assert_eq!(
        warn_count(&e),
        0,
        "a local is not a by-value parameter; got: {e}"
    );
}

#[tokio::test]
async fn silent_when_push_target_is_not_the_param() {
    // $p is the VALUE being pushed onto a local accumulator — a read, not
    // a mutation target.
    let e = stderr_of("function f($p)\n  $acc = []\n  push($acc, $p)\n  return $acc\nend\n").await;
    assert_eq!(warn_count(&e), 0, "got: {e}");
}

#[tokio::test]
async fn silent_when_param_read_inside_a_capturing_lambda() {
    // A lambda captures the enclosing scope for reads, so mentioning $p
    // there is a genuine read and must suppress.
    let e =
        stderr_of("function f($p)\n  push($p, 1)\n  $g = fn() = length($p)\n  return $g()\nend\n")
            .await;
    assert_eq!(warn_count(&e), 0, "got: {e}");
}

#[tokio::test]
async fn silent_for_pop_and_shift() {
    // pop/shift return the removed element (often the point); not flagged.
    let e = stderr_of("function f($p)\n  pop($p)\n  shift($p)\nend\n").await;
    assert_eq!(warn_count(&e), 0, "got: {e}");
}

#[tokio::test]
async fn silent_when_body_sources_code() {
    // `source` runs in this scope and could read $p; bail to stay safe.
    let e = stderr_of("function f($p)\n  push($p, 1)\n  source \"/nonexistent-xyzzy.mix\"\nend\nprint(\"done\")\n").await;
    assert_eq!(warn_count(&e), 0, "got: {e}");
}

#[tokio::test]
async fn silent_for_push_inside_a_handler_body() {
    // An `on` handler body runs LATER in a fresh handler frame, not this
    // function's frame — a push there is not a dead mutation of the param.
    let e = stderr_of(
        "function install($g)\n  on score.tick\n    push($g, 1)\n  end\nend\nprint(\"ok\")\n",
    )
    .await;
    assert_eq!(
        warn_count(&e),
        0,
        "handler body is not the function's scope; got: {e}"
    );
}

// ---------- one-shot ----------

#[tokio::test]
async fn dedups_same_definition_across_a_loop() {
    let e = stderr_of("for $i = 1 to 5\n  function f($p)\n    push($p, 1)\n  end\nend\n").await;
    assert_eq!(warn_count(&e), 1, "same site warns once per run; got: {e}");
}
