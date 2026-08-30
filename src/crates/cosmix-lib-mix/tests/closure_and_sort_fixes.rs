//! Regression guards for the 0.21.7 fixes:
//!   1. Free-variable closure capture — a lambda defined inside a
//!      function snapshots only the frame vars its body references, not
//!      the whole enclosing frame. This fixed the `sort_by` "hang" on
//!      large event data (a per-element key lambda was deep-cloning the
//!      entire in-scope score on every call). These tests pin the
//!      *behaviour* the whole-frame snapshot used to give, so the leaner
//!      capture can't silently drift.
//!   2. `sort()` sorts homogeneous number lists numerically (was
//!      lexical: `[3,1,20]` -> `[1,20,3]`).
//!   3. `unique_by` membership via HashSet (was O(n^2)); order unchanged.

use cosmix_mix::evaluator::{Evaluator, SharedBuf};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;

async fn run(source: &str) -> String {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().expect("lex");
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program().expect("parse");
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    eval.execute(&stmts)
        .await
        .unwrap_or_else(|e| panic!("eval error: {e}"));
    stdout.to_string_lossy().trim_end().to_string()
}

async fn run_res(source: &str) -> Result<String, String> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().map_err(|e| e.to_string())?;
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program().map_err(|e| e.to_string())?;
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    eval.execute(&stmts).await.map_err(|e| e.to_string())?;
    Ok(stdout.to_string_lossy().trim_end().to_string())
}

#[tokio::test]
async fn closure_captures_enclosing_local() {
    let out = run(
        "function o()\n  $mult = 10\n  $f = fn($x) = $x * $mult\n  return $f(5)\nend\nprint(o())\n",
    )
    .await;
    assert_eq!(out, "50");
}

#[tokio::test]
async fn closure_read_then_assign_sees_enclosing_value() {
    // Body both reads and reassigns a var whose name exists in the
    // enclosing frame: it must observe the enclosing value on first read.
    let out = run("function o()\n  $x = 100\n  $f = fn()\n    $x = $x + 1\n    return $x\n  end\n  return $f()\nend\nprint(o())\n").await;
    assert_eq!(out, "101");
}

#[tokio::test]
async fn nested_closures_capture_transitively() {
    let out = run("function o()\n  $foo = 7\n  $g = fn($a) = fn($b) = $a + $b + $foo\n  $add = $g(10)\n  return $add(3)\nend\nprint(o())\n").await;
    assert_eq!(out, "20");
}

#[tokio::test]
async fn interpolation_reference_is_captured() {
    let out = run("function o()\n  $greeting = \"hi\"\n  $f = fn() = \"${greeting} world\"\n  return $f()\nend\nprint(o())\n").await;
    assert_eq!(out, "hi world");
}

#[tokio::test]
async fn param_default_captures_enclosing_free_var() {
    let out = run(
        "function o()\n  $base = 99\n  $f = fn($y = $base) = $y\n  return $f()\nend\nprint(o())\n",
    )
    .await;
    assert_eq!(out, "99");
}

#[tokio::test]
async fn escaping_closure_keeps_capture_after_frame_returns() {
    let out =
        run("function make($n)\n  return fn($x) = $x + $n\nend\n$a = make(5)\nprint($a(10))\n")
            .await;
    assert_eq!(out, "15");
}

#[tokio::test]
async fn captured_list_keeps_value_semantics() {
    // Mutating a captured list inside the lambda must not leak to the
    // enclosing binding (Mix lists pass by value).
    let out = run("function o()\n  $lst = [1, 2, 3]\n  $f = fn()\n    push($lst, 99)\n    return length($lst)\n  end\n  $a = $f()\n  return $a .. \",\" .. length($lst)\nend\nprint(o())\n").await;
    assert_eq!(out, "4,3");
}

#[tokio::test]
async fn empty_capture_still_resolves_global_live() {
    // A depth>0 lambda whose only free ref is a global captures nothing
    // (empty snapshot, kept as `Some({})` so the lambda stays isolated on
    // the call_function path) yet must still read the global live.
    let out =
        run("$G = 42\nfunction o()\n  $f = fn($x) = $x + $G\n  return $f(8)\nend\nprint(o())\n")
            .await;
    assert_eq!(out, "50");
}

#[tokio::test]
async fn pure_param_lambda_ignores_large_enclosing_scope() {
    // The perf fix in behavioural form: a large value in the enclosing
    // frame is NOT part of a pure-param lambda's capture. With the old
    // whole-frame snapshot this deep-cloned $huge on every key call; the
    // fix must both be correct AND not depend on $huge at all. If the
    // O(n*frame) blow-up regressed, sorting inside the big scope would
    // stall this test.
    let out = run("function render($tracks)\n  $sizes = []\n  for each $trk in $tracks\n    $sorted = sort_by($trk, fn($e) = $e[\"t\"])\n    push($sizes, $sorted[0][\"t\"])\n  end\n  return join($sizes, \",\")\nend\n$tracks = []\n$ti = 0\nfor $ti = 0 to 5\n  $trk = []\n  $i = 0\n  for $i = 0 to 799\n    push($trk, { t: (800 - $i), s: \"On ch=1 n=60 v=100 pad=xxxxxxxxxxxxxxxx\" })\n  end\n  push($tracks, $trk)\nend\nprint(render($tracks))\n").await;
    assert_eq!(out, "1,1,1,1,1,1");
}

#[tokio::test]
async fn empty_capture_lambda_stays_isolated_from_caller_frame() {
    // A depth>0 lambda that closes over nothing local must still resolve a
    // free name to the GLOBAL, never to a same-named local in whatever
    // dynamic caller happens to invoke it. Regression for the unsafe
    // `empty snapshot -> None` collapse, which would have let the frameless
    // sync path run the body against `caller()`'s live frame.
    let out = run("$G = \"global\"\nfunction make()\n  return fn() = $G\nend\n$f = make()\nfunction caller()\n  $G = \"caller-local\"\n  return $f()\nend\nprint(caller())\n").await;
    assert_eq!(out, "global");
}

#[tokio::test]
async fn sort_with_nan_keeps_finite_values_ordered() {
    // `sqrt(-1)` is NaN; `total_cmp` is a true total order so the sort
    // completes (no panic) AND the finite values come out numerically
    // ordered. All four elements are retained (NaN not dropped/duplicated),
    // and filtering NaN out (`NaN != NaN`) leaves `[1, 2, 3]`. With the old
    // `partial_cmp().unwrap_or(Equal)` comparator NaN compared Equal to
    // everything, so the finite ordering was unspecified — this pins it.
    let out = run("$s = sort([3, sqrt(-1), 1, 2])\nprint(length($s))\nprint(join(filter($s, fn($x) = $x == $x), \",\"))\n").await;
    assert_eq!(out, "4\n1,2,3");
}

#[tokio::test]
async fn sort_orders_numbers_numerically() {
    let out = run("print(join(sort([3, 1, 20, 100, 2]), \",\"))\n").await;
    assert_eq!(out, "1,2,3,20,100");
}

#[tokio::test]
async fn sort_negatives_and_floats_numeric() {
    let out = run("print(join(sort([-3, -1, -20, 5, 0]), \",\"))\nprint(join(sort([3.5, 1.2, 2.8, 10.0]), \",\"))\n").await;
    assert_eq!(out, "-20,-3,-1,0,5\n1.2,2.8,3.5,10");
}

#[tokio::test]
async fn sort_strings_and_mixed_stay_stringwise() {
    // String lists keep lexical order; a mixed list keeps the historical
    // stringwise ordering (only homogeneous number lists changed).
    let out = run("print(join(sort([\"banana\", \"apple\", \"cherry\"]), \",\"))\nprint(join(sort([2, \"a\", 1]), \",\"))\n").await;
    assert_eq!(out, "apple,banana,cherry\n1,2,a");
}

#[tokio::test]
async fn concat_joins_lists_one_level_only() {
    // Unlike `flat`, concat does NOT recurse into nested elements.
    let out = run(
        "print(\"\" .. concat([1, 2], [3, 4], [5]))\nprint(\"\" .. concat([[1, 2]], [[3, 4]]))\n",
    )
    .await;
    assert_eq!(out, "[1, 2, 3, 4, 5]\n[[1, 2], [3, 4]]");
}

#[tokio::test]
async fn concat_accumulates_a_sequence_across_helpers() {
    // The by-value-safe accumulation pattern: helper RETURNS a list, the
    // caller concats it in — no push-into-a-param data loss.
    let out = run("function bar($b)\n  return [$b * 10, $b * 10 + 1]\nend\n$t = []\nfor each $b in [0, 1, 2]\n  $t = concat($t, bar($b))\nend\nprint(\"\" .. $t)\n").await;
    assert_eq!(out, "[0, 1, 10, 11, 20, 21]");
}

#[tokio::test]
async fn concat_errors_loudly_on_non_list_or_too_few_args() {
    assert!(run_res("print(concat([1], 5))\n").await.is_err());
    assert!(run_res("print(concat([1]))\n").await.is_err());
}

#[tokio::test]
async fn unique_by_dedups_preserving_first_seen_order() {
    let out = run("$u = unique_by([{id: 5}, {id: 1}, {id: 5}, {id: 9}, {id: 1}], fn($e) = $e[\"id\"])\nprint(length($u) .. \":\" .. join(map($u, fn($e) = $e[\"id\"]), \",\"))\n").await;
    assert_eq!(out, "3:5,1,9");
}
