//! Regression tests for the evaluator/value safety fixes:
//! deadline + interrupt coverage on the compiled For fast paths, NaN
//! loop bounds, fixed-operand-stack depth gating, iterative deep
//! `Value` clone/drop, the `Bytes` size cap, the `export` capability
//! gate, and `data_encode` control-character escaping.

use cosmix_mix::evaluator::{Evaluator, SharedBuf};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;
use cosmix_mix::value::Value;
use cosmix_mix::{CapabilityClass, CategoryAllowList, EvalLimits};
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Parse + run `source`, applying `configure` to the evaluator first.
/// Returns Ok(stdout) or Err(error message). Same harness as
/// `tests/limits.rs`.
async fn run(source: &str, configure: impl FnOnce(&mut Evaluator)) -> Result<String, String> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().map_err(|e| e.to_string())?;
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program().map_err(|e| e.to_string())?;
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    configure(&mut eval);
    eval.execute(&stmts).await.map_err(|e| e.to_string())?;
    Ok(stdout.to_string_lossy())
}

// ---------------------------------------------------------------------------
// Knob C on the compiled For fast paths — these loops run native Rust
// to completion, so without the throttled in-loop poll the time limit
// would never fire (the per-statement poll is only re-entered after
// the loop finishes).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn deadline_bounds_compiled_numeric_for_loop() {
    // `$sum = $sum + $i * 2 - 1` compiles to NumOpcode bytecode; 1e9
    // iterations would run for minutes without the in-loop poll.
    let src = "$sum = 0\nfor $i = 1 to 1000000000\n  $sum = $sum + $i * 2 - 1\nend\nprint($sum)\n";
    let started = Instant::now();
    let err = run(src, |e| {
        e.set_limits(EvalLimits {
            time_limit: Some(Duration::from_millis(50)),
            ..Default::default()
        })
    })
    .await
    .expect_err("numeric fast-path loop must hit the deadline");
    assert!(err.contains("deadline exceeded"), "got: {err}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "deadline fired but only after {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn deadline_bounds_self_concat_for_loop() {
    // `$s = $s .. ""` drives the in-place self-concat fast path without
    // growing the string (so only the deadline can stop it).
    let src = "$s = \"\"\nfor $i = 1 to 1000000000\n  $s = $s .. \"\"\nend\nprint(\"done\")\n";
    let started = Instant::now();
    let err = run(src, |e| {
        e.set_limits(EvalLimits {
            time_limit: Some(Duration::from_millis(50)),
            ..Default::default()
        })
    })
    .await
    .expect_err("self-concat fast-path loop must hit the deadline");
    assert!(err.contains("deadline exceeded"), "got: {err}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "deadline fired but only after {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn self_concat_cap_error_rolls_back_the_append() {
    // A max_string_len violation on the in-place self-concat fast path must
    // ROLL BACK (truncate to the pre-append length) before raising, so a
    // caught error leaves $s exactly as it was — matching generic `..` which
    // checks the new Value BEFORE storing it.
    let out = run(
        "$s = \"ab\"\ntry\n  $s = $s .. \"cde\"\ncatch $e\n  print($s)\nend\n",
        |e| {
            e.set_limits(EvalLimits {
                max_string_len: Some(4),
                ..Default::default()
            })
        },
    )
    .await
    .expect("string cap error should be catchable");
    assert_eq!(
        out.trim(),
        "ab",
        "a failed self-concat must leave the old value intact"
    );
}

// ---------------------------------------------------------------------------
// NaN loop bounds — both direction guards (`step > 0` / `step < 0`)
// and both progress guards (`i > end` / `i < end`) are false for NaN,
// so without the up-front check every For path loops forever.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn nan_for_bound_errors() {
    // NaN is manufactured via sqrt(-1) — math propagates IEEE-754 NaN, while
    // to_number("nan") now returns nil (string coercion rejects non-finite
    // spellings; see Value::to_number).
    let src = "$end = sqrt(-1)\nfor $i = 1 to $end\n  $x = $i\nend\nprint(\"done\")\n";
    let err = run(src, |_| {})
        .await
        .expect_err("NaN end bound must error, not loop forever");
    assert!(
        err.contains("loop bound") && err.contains("got NaN"),
        "got: {err}"
    );
}

#[tokio::test]
async fn nan_for_step_errors() {
    // sqrt(-1) for NaN — see nan_for_bound_errors.
    let src = "$step = sqrt(-1)\nfor $i = 1 to 10 step $step\n  $x = $i\nend\nprint(\"done\")\n";
    let err = run(src, |_| {})
        .await
        .expect_err("NaN step must error, not loop forever");
    assert!(
        err.contains("loop step") && err.contains("got NaN"),
        "got: {err}"
    );
}

// ---------------------------------------------------------------------------
// Fixed-operand-stack depth gating — a right-nested numeric expression
// with more than NUM_STACK_SLOTS live operands used to index past the
// fixed [f64; 16] stacks in eval_num_program / eval_fib_program and
// panic; the builders now refuse to compile it (AST fallback computes
// the correct value).
// ---------------------------------------------------------------------------

/// `$v - ($v - ($v - …))` with `wraps` levels of right-nesting; an even
/// number of wraps telescopes to 0 regardless of `$v`.
fn right_nested(var: &str, wraps: usize) -> String {
    let mut expr = format!("{var} - {var}");
    for _ in 0..wraps {
        expr = format!("{var} - ({expr})");
    }
    expr
}

#[tokio::test]
async fn deep_right_nesting_in_for_body_falls_back_instead_of_panicking() {
    let expr = right_nested("$i", 20); // 21 live operands > 16 slots
    let src = format!("$sum = 1\nfor $i = 1 to 3\n  $sum = {expr}\nend\nprint($sum)\n");
    let out = run(&src, |_| {}).await.expect("AST fallback computes it");
    assert_eq!(out.trim(), "0");
}

#[tokio::test]
async fn deep_right_nesting_in_fib_bytecode_falls_back_instead_of_panicking() {
    // Recursive call + deep nesting in the else branch: the fib
    // bytecode VM would execute the nesting with the call result live.
    let expr = right_nested("$n", 20);
    let src = format!(
        "function f($n)\n  if $n < 1 then\n    return 0\n  end\n  return f($n - 1) + ({expr})\nend\nprint(f(3))\n"
    );
    let out = run(&src, |_| {}).await.expect("AST fallback computes it");
    assert_eq!(out.trim(), "0");
}

#[tokio::test]
async fn shallow_nesting_still_computes_on_fast_path() {
    // Control: a fast-path-eligible loop body still computes correctly.
    let src = "$sum = 0\nfor $i = 1 to 10\n  $sum = $sum + $i * 2 - 1\nend\nprint($sum)\n";
    let out = run(src, |_| {}).await.expect("fast path computes");
    assert_eq!(out.trim(), "100");
}

// ---------------------------------------------------------------------------
// Deep Value nesting — iterative Clone/Drop must survive depths where
// the derived (recursive) impls overflowed the native stack.
// ---------------------------------------------------------------------------

#[test]
fn deep_nested_list_clone_and_drop_survive() {
    // O(1) per level to build; 200k deep would SIGSEGV the recursive
    // clone and drop on any realistic thread stack.
    let mut v = Value::Number(0.0);
    for _ in 0..200_000 {
        v = Value::list(vec![v]);
    }
    let cloned = v.clone();
    drop(cloned);
    drop(v);
}

#[test]
fn deep_nested_map_clone_and_drop_survive() {
    let mut v = Value::Number(0.0);
    for _ in 0..200_000 {
        let mut m = cosmix_mix::IndexMap::new();
        m.insert("k".to_string(), v);
        v = Value::map(m);
    }
    let cloned = v.clone();
    drop(cloned);
    drop(v);
}

#[test]
fn mix_deep_nesting_survives_eval() {
    // Mix-source variant on a deliberately small (512 KiB) stack: each
    // `$x = [$x]` iteration clones the old value and drops the
    // replaced one, so a recursive Clone/Drop overflowed well before
    // 8000 levels here. Depth is kept moderate because the per-
    // iteration clone makes the loop O(n²) overall.
    let handle = std::thread::Builder::new()
        .stack_size(512 * 1024)
        .spawn(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("runtime");
            rt.block_on(async {
                let src = "$x = nil\nfor $i = 1 to 8000\n  $x = [$x]\nend\nprint(\"ok\")\n";
                let out = run(src, |_| {}).await.expect("deep nesting evaluates");
                assert_eq!(out.trim(), "ok");
            });
        })
        .expect("spawn 512KiB thread");
    handle
        .join()
        .expect("512KiB-stack thread overflowed — deep Value clone/drop regressed to recursion");
}

// ---------------------------------------------------------------------------
// Knob D — Bytes size cap (byte-buffer analogue of max_string_len)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn collection_cap_bytes() {
    let src = "$b = string_to_bytes(\"abcdefghij\")\nprint(\"ok\")\n";
    let err = run(src, |e| {
        e.set_limits(EvalLimits {
            max_string_len: Some(4),
            ..Default::default()
        })
    })
    .await
    .expect_err("oversized Bytes must hit the cap");
    assert!(
        err.contains("bytes length") && err.contains("exceeds limit"),
        "got: {err}"
    );
}

#[tokio::test]
async fn bytes_within_cap_pass() {
    let src = "$b = string_to_bytes(\"ab\")\nprint(\"ok\")\n";
    let out = run(src, |e| {
        e.set_limits(EvalLimits {
            max_string_len: Some(4),
            ..Default::default()
        })
    })
    .await
    .expect("small Bytes pass the cap");
    assert_eq!(out.trim(), "ok");
}

// ---------------------------------------------------------------------------
// `export` is Process-gated — env mutation is process-global state and
// `std::env::set_var` is UB under concurrent getenv, so a multi-
// threaded embedder must be able to deny it cleanly.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn export_denied_without_process_capability() {
    let err = run("export MIX_EVAL_SAFETY_TEST = \"x\"\n", |e| {
        e.set_capability_policy(Rc::new(CategoryAllowList::new(&[CapabilityClass::FsRead])))
    })
    .await
    .expect_err("export must be denied without the Process capability");
    assert!(
        err.contains("capability denied") && err.contains("export"),
        "got: {err}"
    );
    assert!(
        std::env::var("MIX_EVAL_SAFETY_TEST").is_err(),
        "denied export must not have touched the environment"
    );
}

#[tokio::test]
async fn export_allowed_with_process_capability() {
    let out = run(
        "export MIX_EVAL_SAFETY_TEST_OK = \"yes\"\nprint(env(\"MIX_EVAL_SAFETY_TEST_OK\"))\n",
        |e| {
            e.set_capability_policy(Rc::new(CategoryAllowList::new(&[
                CapabilityClass::Process,
                CapabilityClass::Env,
            ])))
        },
    )
    .await
    .expect("Process allowed → export runs");
    assert_eq!(out.trim(), "yes");
}

// ---------------------------------------------------------------------------
// data_encode escapes ALL control characters as \u{XXXX} and the
// strict-data round-trip still holds.
// ---------------------------------------------------------------------------

#[test]
fn data_encode_control_chars_roundtrip() {
    let original = Value::String("a\u{0}b\u{7}c\u{9F}d\u{7F}e\nf".to_string());
    let wrapped = Value::list(vec![original.clone()]);
    let encoded = wrapped.to_mix_data_string().expect("encodable");
    // Raw controls must not appear in the serialized form (the \n is
    // its own escape, also non-raw).
    assert!(
        !encoded.chars().any(|c| c.is_control()),
        "raw control char leaked into: {encoded:?}"
    );
    assert!(encoded.contains("\\u{0}"), "NUL not escaped: {encoded:?}");
    assert!(encoded.contains("\\u{7}"), "BEL not escaped: {encoded:?}");
    assert!(
        encoded.contains("\\u{9F}"),
        "U+009F not escaped: {encoded:?}"
    );
    let parsed = cosmix_mix::parse_data(&encoded).expect("re-parse");
    // Value's PartialEq is `false` for List == List by design, so
    // compare the inner string.
    let Value::List(items) = &parsed else {
        panic!("expected a list, got {parsed:?}");
    };
    assert_eq!(items[0], original);
}

// ---------------------------------------------------------------------------
// write_mix integral formatting — only when the i64 cast round-trips.
// ---------------------------------------------------------------------------

#[test]
fn write_mix_huge_integral_float_not_saturated() {
    // 1e19 > i64::MAX; the old `n == n.floor()` gate printed it as
    // i64::MAX (9223372036854775807).
    assert_eq!(Value::Number(1e19).to_mix_string(), "10000000000000000000");
    assert_eq!(Value::Number(42.0).to_mix_string(), "42");
    assert_eq!(Value::Number(1.5).to_mix_string(), "1.5");
}

// ---------------------------------------------------------------------------
// 2026-07 audit batch: index-WRITE signed resolution + non-container errors,
// modulo-by-zero guard, scientific-notation number literals.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn negative_index_write_hits_from_end() {
    // Was: `-1 as usize` saturated to 0, corrupting element 0 ([99,2,3]).
    let out = run("$l = [1,2,3]\n$l[-1] = 99\nprint(\"\" .. $l)\n", |_| {})
        .await
        .expect("negative write");
    assert_eq!(out.trim(), "[1, 2, 99]");
}

#[tokio::test]
async fn negative_index_write_middle_element() {
    let out = run("$l = [1,2,3,4,5]\n$l[-2] = 99\nprint(\"\" .. $l)\n", |_| {})
        .await
        .expect("negative write");
    assert_eq!(out.trim(), "[1, 2, 3, 99, 5]");
}

#[tokio::test]
async fn out_of_range_index_write_errors() {
    // Was silently dropped; now a loud error.
    let err = run("$l = [1,2,3]\n$l[99] = 5\n", |_| {})
        .await
        .expect_err("out-of-range write must error");
    assert!(err.contains("out of range"), "got: {err}");
}

#[tokio::test]
async fn index_assign_into_scalar_errors() {
    // Was silently dropped; now a typed error.
    let err = run("$x = 5\n$x[\"k\"] = 9\n", |_| {})
        .await
        .expect_err("index-assign into a number must error");
    assert!(
        err.contains("cannot index-assign into number"),
        "got: {err}"
    );
}

#[tokio::test]
async fn for_loop_fast_path_negative_index_write() {
    // The compiled single-IndexAssignment for-loop fast path shares the
    // same assign helper, so negative/out-of-range handling matches.
    let out = run(
        "$l = [0,0,0]\nfor $i = 0 to 2\n  $l[$i] = $i * 10\nend\nprint(\"\" .. $l)\n",
        |_| {},
    )
    .await
    .expect("fast-path write");
    assert_eq!(out.trim(), "[0, 10, 20]");
}

#[tokio::test]
async fn modulo_by_zero_errors_like_division() {
    // Was: `5 % 0` silently returned NaN while `5 / 0` errored.
    let err = run("print(5 % 0)\n", |_| {})
        .await
        .expect_err("modulo by zero must error");
    assert!(err.contains("modulo by zero"), "got: {err}");
    // Non-zero modulo still works.
    let out = run("print(17 % 5)\n", |_| {}).await.expect("mod");
    assert_eq!(out.trim(), "2");
}

#[tokio::test]
async fn scientific_notation_number_literals() {
    for (src, want) in [
        ("print(1e6)\n", "1000000"),
        ("print(1.5e3)\n", "1500"),
        ("print(2e-3)\n", "0.002"),
        ("print(0e5)\n", "0"),
        ("print(1E3)\n", "1000"),
    ] {
        let out = run(src, |_| {}).await.expect("sci literal");
        assert_eq!(out.trim(), want, "for {src:?}");
    }
}

#[tokio::test]
async fn exponent_does_not_eat_euler_builtin() {
    // `e()` (euler constant) must not be swallowed by a preceding number's
    // exponent lexing: `2e` with no digit stays Number(2) then `e`.
    let out = run("print(2 * e())\n", |_| {}).await.expect("euler");
    assert!(out.trim().starts_with("5.436"), "got: {out}");
}

#[tokio::test]
async fn leading_zero_still_rejected_with_exponent() {
    // The exponent is lexed AFTER the leading-zero check, so `07e2` is
    // still the ambiguous-leading-zero error, not a valid 700.
    let err = run("print(07e2)\n", |_| {})
        .await
        .expect_err("07e2 must stay a leading-zero error");
    assert!(err.contains("leading-zero"), "got: {err}");
}

#[tokio::test]
async fn nan_index_write_errors_not_corrupts() {
    // NaN would `as i64`→0 and overwrite element 0 (same shape as the
    // old negative-index bug). Must be a loud error instead.
    let err = run("$l = [1,2,3]\n$l[sqrt(-1)] = 99\n", |_| {})
        .await
        .expect_err("NaN index write must error");
    assert!(
        err.contains("assignment index") && err.contains("got NaN"),
        "got: {err}"
    );
}

/// READS stay lenient — the documented contract (collections.md: an
/// out-of-range index "returns nil (never an error)"; fractional indexes
/// truncate). 0.59.0's strictness applies to index WRITES only; review
/// round 1 caught the sweep leaking onto this path and it was restored.
#[tokio::test]
async fn index_reads_stay_lenient() {
    let out = run(
        "$l = [1,2,3]\nprint(\"[\" .. $l[1000000000000000000000000000000] .. \"]\")\n",
        |_| {},
    )
    .await
    .expect("oob read is nil, not an error"); // nil renders as "nil" in concat
    assert_eq!(out.trim(), "[nil]");
    let out = run("$l = [1,2,3]\nprint($l[1.5])\n", |_| {})
        .await
        .expect("fractional read truncates");
    assert_eq!(out.trim(), "2");
    let out = run("print(length(slice([1,2,3], 0.5, 2)))\n", |_| {})
        .await
        .expect("slice clamps");
    assert_eq!(out.trim(), "2");
    let out = run(
        "print(length(take([1,2], 1000000000000000000000000000000)))\n",
        |_| {},
    )
    .await
    .expect("take clamps");
    assert_eq!(out.trim(), "2");
}

#[tokio::test]
async fn string_index_on_list_write_errors_matching_read() {
    // Read path only indexes a list with Value::Number; the write path
    // now matches (a numeric-string index is a type error, not a coerce).
    let err = run("$l = [1,2,3]\n$l[\"1\"] = 9\n", |_| {})
        .await
        .expect_err("string list index write must error");
    assert!(err.contains("cannot index list with string"), "got: {err}");
}

#[tokio::test]
async fn evaluator_numeric_fallbacks_raise_on_unparseable_values() {
    for (source, expected) in [
        (
            "for $i = \"bad\" to 2\n  print($i)\nend\n",
            "for-loop start must be a number",
        ),
        (
            "for $i = 1 to \"bad\"\n  print($i)\nend\n",
            "for-loop end must be a number",
        ),
        (
            "for $i = 1 to 2 step \"bad\"\n  print($i)\nend\n",
            "for-loop step must be a number",
        ),
        ("sleep(\"bad\")\n", "sleep(): argument 1 must be a number"),
        ("exit(\"bad\")\n", "exit(): argument 1 must be a number"),
        ("print(\"bad\" / 2)\n", "cannot use 'bad' as number"),
        ("print(2 / \"bad\")\n", "cannot use 'bad' as number"),
        ("print(\"bad\" % 2)\n", "cannot use 'bad' as number"),
        ("print(2 % \"bad\")\n", "cannot use 'bad' as number"),
    ] {
        let err = run(source, |_| {})
            .await
            .expect_err("unparseable numeric value must raise");
        assert!(err.contains(expected), "source {source:?}: got {err}");
    }
}

#[tokio::test]
async fn evaluator_numeric_strings_still_coerce() {
    let output = run(
        "$sum = 0\nfor $i = \"1\" to \"3\" step \"1\"\n  $sum = $sum + $i\nend\nprint($sum)\nprint(\"8\" / \"2\")\nsleep(\"0\")\n",
        |_| {},
    )
    .await
    .expect("numeric strings remain valid");
    assert_eq!(output, "6\n4\n");
}
