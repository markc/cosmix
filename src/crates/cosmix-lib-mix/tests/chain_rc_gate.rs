//! M-2 (mix 0.60.0): the `&&` / `||` statement chain gates on `$rc`, and a
//! corrupted `$rc` RAISES instead of silently reading as success.
//!
//! The old gate was `to_number().unwrap_or(0.0) as i64`, which answered
//! something ARBITRARY for every corrupt shape, never an error. Whatever
//! coerced-then-truncated to 0 read as SUCCESS: non-coercible values and
//! unparseable strings ("inf" included — to_number's finite filter
//! rejects it), `false`, NaN, fractions inside (-1, 1), and numeric
//! strings like "0"/"0.5". Whatever truncated to non-zero read as
//! failure: `true`, Number infinities, "1", 1.5. Either way a verdict
//! fabricated from garbage. Raising stops both chains running a right
//! side off corruption (codex D/M decision, thread 01a01fa3).

use cosmix_mix::evaluator::{Evaluator, SharedBuf};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;

/// Parse + run `source`, returning Ok(stdout) or Err(error message).
async fn run_mix(source: &str) -> Result<String, String> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().map_err(|e| e.to_string())?;
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program().map_err(|e| e.to_string())?;
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    eval.execute(&stmts).await.map_err(|e| e.to_string())?;
    Ok(stdout.to_string_lossy())
}

async fn run_ok(source: &str) -> String {
    run_mix(source)
        .await
        .unwrap_or_else(|e| panic!("script failed: {e}\nsource:\n{source}"))
        .trim_end()
        .to_string()
}

/// Absent `$rc` means SUCCESS: pure Mix statements like `print` never set
/// it, so a cold `print(...) && next` must run its right-hand side.
#[tokio::test]
async fn absent_rc_is_success() {
    let out = run_ok("print(\"a\") && print(\"b\")\n").await;
    assert_eq!(out, "a\nb");
    // ...and || correspondingly does NOT run its right side.
    let out = run_ok("print(\"a\") || print(\"never\")\n").await;
    assert_eq!(out, "a");
}

/// A real failure rc (whole non-zero Number) short-circuits `&&` and takes
/// the `||` branch — the normal contract, unchanged.
#[tokio::test]
async fn whole_number_rc_gates_normally() {
    let out =
        run_ok("sh \"false\"\nprint(\"lhs\") && print(\"skipped\") || print(\"or-ran\")\n").await;
    assert_eq!(out, "lhs\nor-ran");
    let out = run_ok("sh \"true\"\nprint(\"lhs\") && print(\"and-ran\")\n").await;
    assert_eq!(out, "lhs\nand-ran");
    // Negative rc bands (bus transport codes -1..-3) are whole numbers and
    // read as failure, never as corruption.
    let out = run_ok("$rc = 0 - 2\nprint(\"lhs\") || print(\"or-ran\")\n").await;
    assert_eq!(out, "lhs\nor-ran");
}

/// The documented scope corner (operators.md "absent $rc" bullet): with no
/// pre-existing global $rc, an rc-setting statement inside a function
/// writes a FUNCTION-LOCAL $rc that dies with the call — the chain then
/// reads absent = success off a buried failure. Once a global exists,
/// in-function setters update it and the gate sees it. Pre-existing
/// behaviour, surfaced by the 0.60.0 GLM review; pinned so a future
/// "fix" of either half is a deliberate act, not drift.
#[tokio::test]
async fn function_local_rc_discard_is_the_documented_corner() {
    // No global $rc yet: p()'s failure rc is discarded -> absent -> success.
    let out = run_ok("function p()\n  sh \"false\"\nend\np() && print(\"ran\")\n").await;
    assert_eq!(out, "ran");
    // A pre-existing global: the in-function setter updates it, gate sees 1.
    let out = run_ok(
        "sh \"true\"\nfunction q()\n  sh \"false\"\nend\nq() && print(\"skipped\") || print(\"or-ran\")\n",
    )
    .await;
    assert_eq!(out, "or-ran");
}

/// A non-Number `$rc` raises TYPE_MISMATCH — on BOTH operators, because
/// the hazard is symmetric.
#[tokio::test]
async fn non_number_rc_raises_type_mismatch() {
    for (src, what) in [
        (
            "$rc = \"garbage\"\nprint(\"x\") && print(\"y\")\n",
            "string &&",
        ),
        (
            "$rc = \"garbage\"\nprint(\"x\") || print(\"y\")\n",
            "string ||",
        ),
        ("$rc = true\nprint(\"x\") && print(\"y\")\n", "bool &&"),
        ("$rc = true\nprint(\"x\") || print(\"y\")\n", "bool ||"),
        // Numeric STRINGS are corruption here too: the documented invariant
        // is that $rc is a number, so "0" arriving means something upstream
        // stringified state — silently reading it as success hides that.
        (
            "$rc = \"0\"\nprint(\"x\") && print(\"y\")\n",
            "numeric string",
        ),
    ] {
        let err = run_mix(src).await.expect_err(what);
        assert!(
            err.contains("chain condition") && err.contains("must be a number"),
            "{what}: {err}"
        );
    }
}

/// A non-finite or fractional Number raises VALUE_OUT_OF_RANGE — the old
/// cast read NaN as 0, which is success.
#[tokio::test]
async fn non_whole_rc_raises_value_out_of_range() {
    for (src, what) in [
        ("$rc = sqrt(0 - 1)\nprint(\"x\") && print(\"y\")\n", "NaN"),
        ("$rc = 1.5\nprint(\"x\") && print(\"y\")\n", "fractional"),
        ("$rc = exp(1000)\nprint(\"x\") || print(\"y\")\n", "inf ||"),
    ] {
        let err = run_mix(src).await.expect_err(what);
        assert!(
            err.contains("chain condition") && err.contains("finite whole number"),
            "{what}: {err}"
        );
    }
}

/// The raise is a normal catchable Mix error with the structured code.
#[tokio::test]
async fn corrupt_rc_error_is_catchable_with_code() {
    let out = run_ok(
        "$rc = \"bad\"\ntry\n  print(\"x\") && print(\"y\")\ncatch $m, $e\n  print($e.code)\nend\n",
    )
    .await;
    assert_eq!(out, "x\nTYPE_MISMATCH");
    let out = run_ok(
        "$rc = 1.5\ntry\n  print(\"x\") && print(\"y\")\ncatch $m, $e\n  print($e.code)\nend\n",
    )
    .await;
    assert_eq!(out, "x\nVALUE_OUT_OF_RANGE");
}
