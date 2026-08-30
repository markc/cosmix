//! v0.54.0 — zero-padding in `fmt` (`%0Nd`) and the optional fill-character
//! argument on `lpad`/`rpad`/`lpad_w`/`rpad_w`.
//!
//! Both gaps were found writing a real script: `fmt("%012d", epoch)` silently
//! space-padded, so a lexicographic sort by zero-padded epoch didn't sort, and
//! `lpad` had no way to pad with anything but a space. The workaround was
//! `repeat("0", n - length(s)) .. s`.
//!
//! The behaviours worth locking down are the ones a naive implementation gets
//! wrong: sign placement on negative numbers, `-` overriding `0`, `0` being
//! ignored for strings, and a wide fill silently breaking cell alignment.

use cosmix_mix::evaluator::{Evaluator, SharedBuf};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;

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

async fn run_err(source: &str) -> String {
    run_mix(source)
        .await
        .expect_err("script must return a runtime error, not succeed")
}

// ---------------------------------------------------------------------------
// fmt: %0Nd zero-padding
// ---------------------------------------------------------------------------

/// The original motivating case: a 10-digit epoch padded to 12 so that a
/// lexicographic sort orders by time.
#[tokio::test]
async fn fmt_zero_pad_epoch() {
    assert_eq!(
        run_ok(r#"print(fmt("%012d", 1755000000))"#).await,
        "001755000000"
    );
}

#[tokio::test]
async fn fmt_zero_pad_basic() {
    assert_eq!(run_ok(r#"print(fmt("%05d", 42))"#).await, "00042");
}

/// Sign must stay LEFT of the zeros. A fill-char + right-align implementation
/// produces "000-42" here, which is the classic way to get this wrong.
#[tokio::test]
async fn fmt_zero_pad_negative_keeps_sign_first() {
    assert_eq!(run_ok(r#"print(fmt("%05d", -42))"#).await, "-0042");
}

/// POSIX: `-` overrides `0`. Flags are accepted in either order.
#[tokio::test]
async fn fmt_left_align_overrides_zero_pad() {
    assert_eq!(run_ok(r#"print(fmt("%-05d|", 42))"#).await, "42   |");
    assert_eq!(run_ok(r#"print(fmt("%0-5d|", 42))"#).await, "42   |");
}

#[tokio::test]
async fn fmt_zero_pad_float_with_precision() {
    assert_eq!(run_ok(r#"print(fmt("%08.2f", 3.14159))"#).await, "00003.14");
}

#[tokio::test]
async fn fmt_zero_pad_negative_float_keeps_sign_first() {
    assert_eq!(
        run_ok(r#"print(fmt("%08.2f", -3.14159))"#).await,
        "-0003.14"
    );
}

/// `%0Ns` is undefined for strings in C; we ignore the flag and space-pad.
#[tokio::test]
async fn fmt_zero_flag_ignored_for_strings() {
    assert_eq!(run_ok(r#"print(fmt("%05s|", "ab"))"#).await, "   ab|");
}

/// Plain width must be untouched by the flag parsing rewrite.
#[tokio::test]
async fn fmt_plain_width_unchanged() {
    assert_eq!(run_ok(r#"print(fmt("%5d|", 42))"#).await, "   42|");
    assert_eq!(run_ok(r#"print(fmt("%-5s|", "ab"))"#).await, "ab   |");
}

// ---------------------------------------------------------------------------
// lpad / rpad: optional fill character
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lpad_rpad_with_fill() {
    assert_eq!(run_ok(r#"print(lpad("7", 4, "0"))"#).await, "0007");
    assert_eq!(run_ok(r#"print(rpad("7", 4, "."))"#).await, "7...");
}

/// Two-arg form must keep padding with spaces — this is the compatibility
/// guarantee for every existing caller.
#[tokio::test]
async fn lpad_rpad_default_fill_still_space() {
    assert_eq!(
        run_ok(r#"print("[" .. lpad("ab", 5) .. "]")"#).await,
        "[   ab]"
    );
    assert_eq!(
        run_ok(r#"print("[" .. rpad("ab", 5) .. "]")"#).await,
        "[ab   ]"
    );
}

/// Saturating: already at or beyond the width returns unchanged, never truncated.
#[tokio::test]
async fn lpad_saturates_never_truncates() {
    assert_eq!(run_ok(r#"print(lpad("abcdef", 3, "0"))"#).await, "abcdef");
}

#[tokio::test]
async fn pad_fill_must_be_single_char() {
    let e = run_err(r#"print(lpad("7", 4, "ab"))"#).await;
    assert!(e.contains("exactly one character"), "unexpected error: {e}");
    let e = run_err(r#"print(lpad("7", 4, ""))"#).await;
    assert!(e.contains("exactly one character"), "unexpected error: {e}");
}

#[tokio::test]
async fn pad_rejects_extra_args() {
    run_err(r#"print(lpad("7", 4, "0", "x"))"#).await;
}

// ---------------------------------------------------------------------------
// lpad_w / rpad_w: fill must be one DISPLAY CELL
// ---------------------------------------------------------------------------

/// A 2-cell glyph padded to 6 cells takes 4 single-cell fills, not 4 codepoints.
#[tokio::test]
async fn pad_w_fills_by_display_cells() {
    assert_eq!(run_ok(r#"print(lpad_w("日本", 6, "."))"#).await, "..日本");
    assert_eq!(
        run_ok(r#"print(rpad_w("日本", 6, ".") .. "|")"#).await,
        "日本..|"
    );
}

/// A wide fill would overshoot by one cell per pad char, so it is rejected
/// rather than silently mis-aligning the column it exists to align.
#[tokio::test]
async fn pad_w_rejects_wide_fill() {
    let e = run_err(r#"print(lpad_w("7", 4, "漢"))"#).await;
    assert!(e.contains("display cell"), "unexpected error: {e}");
}

#[tokio::test]
async fn pad_w_default_fill_still_space() {
    assert_eq!(
        run_ok(r#"print(rpad_w("日本", 6) .. "|")"#).await,
        "日本  |"
    );
}
