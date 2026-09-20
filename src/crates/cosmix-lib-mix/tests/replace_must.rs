//! `replace_must()` / `re_replace_must()` — the fail-loud edit twins (0.90.0).
//!
//! `replace()` returning the subject unchanged when the needle is absent is
//! the right contract for a transform and the wrong one for an EDIT. On
//! 2026-09-18 three `mix -c` edits missed their needle, `write_file` wrote
//! the input straight back, and one of them shipped a commit that did not
//! compile — with no signal at any step. These pin the signal.

use cosmix_mix::evaluator::{Evaluator, SharedBuf};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;

async fn run(source: &str) -> Result<String, String> {
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

#[tokio::test]
async fn present_needle_behaves_exactly_like_replace() {
    let out = run("print(replace_must(\"a-b-a\", \"a\", \"X\"))\n\
                   print(replace(\"a-b-a\", \"a\", \"X\"))\n\
                   print(re_replace_must(\"a1b2\", \"[0-9]\", \"#\"))\n")
        .await
        .unwrap();
    assert_eq!(out, "X-b-X\nX-b-X\na#b#\n");
}

#[tokio::test]
async fn absent_needle_raises_needle_absent() {
    let err = run("print(replace_must(\"hello\", \"zz\", \"X\"))\n")
        .await
        .expect_err("absent needle must raise");
    assert!(err.contains("does not occur"), "{err}");

    let err = run("print(re_replace_must(\"hello\", \"[0-9]+\", \"X\"))\n")
        .await
        .expect_err("non-matching pattern must raise");
    assert!(err.contains("does not occur"), "{err}");

    // ...and the tolerant originals are untouched — that is the whole point
    // of adding a twin rather than changing them.
    let out = run("print(replace(\"hello\", \"zz\", \"X\"))\n\
                   print(re_replace(\"hello\", \"[0-9]+\", \"X\"))\n")
        .await
        .unwrap();
    assert_eq!(out, "hello\nhello\n");
}

#[tokio::test]
async fn count_assertion_catches_both_too_many_and_too_few() {
    // `count: 1` on a two-occurrence subject — the filing case: an edit that
    // was meant to hit one site and silently rewrote two.
    let err = run("print(replace_must(\"a-b-a\", \"a\", \"X\", {count: 1}))\n")
        .await
        .expect_err("count mismatch must raise");
    assert!(err.contains("occurs 2 time(s)") && err.contains("not the 1"), "{err}");

    let err = run("print(replace_must(\"a-b-a\", \"a\", \"X\", {count: 3}))\n")
        .await
        .expect_err("count mismatch must raise");
    assert!(err.contains("occurs 2 time(s)"), "{err}");

    // The matching count is silent.
    let out = run("print(replace_must(\"a-b-a\", \"a\", \"X\", {count: 2}))\n\
                   print(re_replace_must(\"a1b2\", \"[0-9]\", \"#\", {count: 2}))\n")
        .await
        .unwrap();
    assert_eq!(out, "X-b-X\na#b#\n");
}

#[tokio::test]
async fn path_option_names_the_file_in_the_message() {
    // The 2026-09-18 case was a loop over several files; knowing WHICH edit
    // missed is most of the diagnosis.
    let err = run("print(replace_must(\"hello\", \"zz\", \"X\", {path: \"/tmp/main.rs\"}))\n")
        .await
        .expect_err("must raise");
    assert!(err.contains("in /tmp/main.rs"), "{err}");
}

#[tokio::test]
async fn bad_options_raise_rather_than_silently_disarming_the_check() {
    // A silently ignored `{counts: 2}` turns the assertion off at exactly
    // the moment the caller was being careful — the `mkdir({parents})` rule.
    for (src, needle) in [
        (
            "print(replace_must(\"aa\", \"a\", \"X\", {counts: 2}))\n",
            "unknown option",
        ),
        (
            "print(replace_must(\"aa\", \"a\", \"X\", {count: \"two\"}))\n",
            "must be a number",
        ),
        (
            "print(replace_must(\"aa\", \"a\", \"X\", {count: 1.5}))\n",
            "whole number",
        ),
        (
            "print(replace_must(\"aa\", \"a\", \"X\", \"count\"))\n",
            "options map",
        ),
        // An empty needle matches between every character, so the count is
        // meaningless and the splice is never what an edit meant.
        ("print(replace_must(\"aa\", \"\", \"X\"))\n", "empty needle"),
    ] {
        let err = run(src).await.unwrap_err();
        assert!(err.contains(needle), "{src} -> {err}");
    }
}

#[tokio::test]
async fn an_absurd_count_is_refused_rather_than_saturated() {
    // `as usize` SATURATES, so {count: 1e300} became usize::MAX and the
    // mismatch message quoted a number the caller never wrote:
    // "not the 18446744073709551615 asserted". Found by the GLM arm of the
    // 0.90.0 cold review.
    let err = run("print(replace_must(\"aa\", \"a\", \"b\", {count: 1e300}))\n")
        .await
        .expect_err("an out-of-range count must be refused");
    assert!(err.contains("0..=9007199254740992"), "{err}");
    assert!(
        !err.contains("18446744073709551615"),
        "must not quote the saturated value: {err}"
    );
    // The bound is far above anything real, so ordinary counts are intact.
    let out = run("print(replace_must(\"aa\", \"a\", \"b\", {count: 2}))\n")
        .await
        .unwrap();
    assert_eq!(out, "bb\n");
}
