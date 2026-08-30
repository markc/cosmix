//! head()/tail() — the no-slurp file line readers (v0.28.1) — plus the
//! line_count() streaming rewrite that landed with them. Fixtures are
//! written with std::fs so the tests control bytes exactly (trailing
//! newline or not, CRLF, invalid UTF-8, >1-block files).

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

async fn run_err(source: &str) -> String {
    run_mix(source)
        .await
        .expect_err("script must return a runtime error, not succeed")
}

/// Unique fixture path per test so parallel tests never collide.
fn fixture(name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("mix-head-tail-{}-{}", std::process::id(), name));
    std::fs::write(&path, bytes).expect("write fixture");
    path
}

// ---------------------------------------------------------------------------
// head
// ---------------------------------------------------------------------------

#[tokio::test]
async fn head_first_n_lines() {
    let p = fixture("head-basic", b"one\ntwo\nthree\nfour\n");
    let out = run_ok(&format!(r#"print(join(head("{}", 2), "|"))"#, p.display())).await;
    assert_eq!(out, "one|two");
}

#[tokio::test]
async fn head_default_is_ten() {
    let body: String = (1..=12).map(|i| format!("l{i}\n")).collect();
    let p = fixture("head-default", body.as_bytes());
    let out = run_ok(&format!(r#"print(length(head("{}")))"#, p.display())).await;
    assert_eq!(out, "10");
}

#[tokio::test]
async fn head_n_past_eof_returns_all() {
    let p = fixture("head-past-eof", b"a\nb\n");
    let out = run_ok(&format!(r#"print(join(head("{}", 99), "|"))"#, p.display())).await;
    assert_eq!(out, "a|b");
}

#[tokio::test]
async fn head_zero_and_empty_file() {
    let p = fixture("head-zero", b"a\nb\n");
    let out = run_ok(&format!(r#"print(length(head("{}", 0)))"#, p.display())).await;
    assert_eq!(out, "0");
    let empty = fixture("head-empty", b"");
    let out = run_ok(&format!(r#"print(length(head("{}")))"#, empty.display())).await;
    assert_eq!(out, "0");
}

#[tokio::test]
async fn head_strips_crlf_like_read_lines() {
    let p = fixture("head-crlf", b"a\r\nb\r\n");
    let out = run_ok(&format!(r#"print(join(head("{}", 2), "|"))"#, p.display())).await;
    assert_eq!(out, "a|b");
}

// ---------------------------------------------------------------------------
// tail
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tail_last_n_lines() {
    let p = fixture("tail-basic", b"one\ntwo\nthree\nfour\n");
    let out = run_ok(&format!(r#"print(join(tail("{}", 2), "|"))"#, p.display())).await;
    assert_eq!(out, "three|four");
}

#[tokio::test]
async fn tail_no_trailing_newline() {
    let p = fixture("tail-no-nl", b"a\nb\nc");
    let out = run_ok(&format!(r#"print(join(tail("{}", 2), "|"))"#, p.display())).await;
    assert_eq!(out, "b|c");
}

#[tokio::test]
async fn tail_trailing_newline_no_phantom_empty_line() {
    let p = fixture("tail-nl", b"a\nb\nc\n");
    let out = run_ok(&format!(r#"print(join(tail("{}", 2), "|"))"#, p.display())).await;
    assert_eq!(out, "b|c");
}

#[tokio::test]
async fn tail_n_past_eof_returns_all() {
    let p = fixture("tail-past-eof", b"a\nb\n");
    let out = run_ok(&format!(r#"print(join(tail("{}", 99), "|"))"#, p.display())).await;
    assert_eq!(out, "a|b");
}

#[tokio::test]
async fn tail_zero_and_empty_file() {
    let p = fixture("tail-zero", b"a\nb\n");
    let out = run_ok(&format!(r#"print(length(tail("{}", 0)))"#, p.display())).await;
    assert_eq!(out, "0");
    let empty = fixture("tail-empty", b"");
    let out = run_ok(&format!(r#"print(length(tail("{}")))"#, empty.display())).await;
    assert_eq!(out, "0");
}

#[tokio::test]
async fn tail_strips_crlf_like_read_lines() {
    let p = fixture("tail-crlf", b"a\r\nb\r\nc\r\n");
    let out = run_ok(&format!(r#"print(join(tail("{}", 2), "|"))"#, p.display())).await;
    assert_eq!(out, "b|c");
}

/// File larger than the 64 KiB backwards-read block: the loop must walk
/// multiple blocks and still return exactly the last N lines.
#[tokio::test]
async fn tail_multi_block_file() {
    let body: String = (1..=20_000).map(|i| format!("line-{i}\n")).collect();
    assert!(body.len() > 128 * 1024, "fixture must span multiple blocks");
    let p = fixture("tail-multiblock", body.as_bytes());
    let out = run_ok(&format!(r#"print(join(tail("{}", 3), "|"))"#, p.display())).await;
    assert_eq!(out, "line-19998|line-19999|line-20000");
    // head on the same file streams from the front.
    let out = run_ok(&format!(r#"print(join(head("{}", 2), "|"))"#, p.display())).await;
    assert_eq!(out, "line-1|line-2");
}

/// Multi-byte content survives the backwards block walk (no torn-char
/// spurious UTF-8 error on ordinary multibyte files).
#[tokio::test]
async fn head_tail_multibyte_lines() {
    let body = "早い\n猫🐈\n犬🐕\n".repeat(8000); // ~350 KB, > one block
    let p = fixture("multibyte", body.as_bytes());
    let out = run_ok(&format!(r#"print(join(tail("{}", 2), "|"))"#, p.display())).await;
    assert_eq!(out, "猫🐈|犬🐕");
    let out = run_ok(&format!(r#"print(join(head("{}", 2), "|"))"#, p.display())).await;
    assert_eq!(out, "早い|猫🐈");
}

// ---------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------

#[tokio::test]
async fn head_tail_missing_file_is_catchable_error() {
    let err = run_err(r#"print(head("/nonexistent/mix-head-test", 1))"#).await;
    assert!(err.contains("head"), "error should name the builtin: {err}");
    let err = run_err(r#"print(tail("/nonexistent/mix-tail-test", 1))"#).await;
    assert!(err.contains("tail"), "error should name the builtin: {err}");
}

#[tokio::test]
async fn head_tail_reject_bad_n() {
    let p = fixture("bad-n", b"a\n");
    for call in [
        format!(r#"head("{}", -1)"#, p.display()),
        format!(r#"head("{}", 1.5)"#, p.display()),
        format!(r#"tail("{}", "5")"#, p.display()),
    ] {
        let err = run_err(&format!("print({call})")).await;
        assert!(
            err.contains("n must be a non-negative"),
            "bad n must be rejected loudly: {err}"
        );
    }
}

#[tokio::test]
async fn tail_invalid_utf8_in_returned_region_errors() {
    let p = fixture("tail-bad-utf8", b"ok\n\xff\xfe bad\n");
    let err = run_err(&format!(r#"print(tail("{}", 2))"#, p.display())).await;
    assert!(err.contains("not valid UTF-8"), "got: {err}");
}

/// Invalid UTF-8 in the collected-but-not-returned overshoot must NOT
/// poison clean returned lines (Codex review finding, 0.28.1).
#[tokio::test]
async fn tail_binary_outside_returned_region_is_fine() {
    let p = fixture("tail-bad-utf8-outside", b"\xff\xfe junk\nclean1\nclean2\n");
    let out = run_ok(&format!(r#"print(join(tail("{}", 2), "|"))"#, p.display())).await;
    assert_eq!(out, "clean1|clean2");
}

/// str::lines parity: a lone \r on an unterminated final line is
/// preserved — only the \r of a \r\n pair is stripped (Codex review
/// finding, 0.28.1).
#[tokio::test]
async fn tail_lone_cr_on_final_line_preserved() {
    let p = fixture("tail-lone-cr", b"a\nb\r");
    let out = run_ok(&format!(
        r#"print(join(tail("{}", 2), "|") .. "END")"#,
        p.display()
    ))
    .await;
    assert_eq!(out, "a|b\rEND");
}

// ---------------------------------------------------------------------------
// line_count — streaming rewrite parity
// ---------------------------------------------------------------------------

#[tokio::test]
async fn line_count_parity_with_str_lines() {
    for (name, bytes, want) in [
        ("lc-nl", b"a\nb\nc\n".as_slice(), "3"),
        ("lc-no-nl", b"a\nb\nc".as_slice(), "3"),
        ("lc-empty", b"".as_slice(), "0"),
        ("lc-one", b"x".as_slice(), "1"),
    ] {
        let p = fixture(name, bytes);
        let out = run_ok(&format!(r#"print(line_count("{}"))"#, p.display())).await;
        assert_eq!(out, want, "fixture {name}");
    }
}

/// The rewrite is byte-oriented: non-UTF-8 files now count instead of
/// erroring (previously read_to_string rejected them).
#[tokio::test]
async fn line_count_works_on_binary() {
    let p = fixture("lc-binary", b"\xff\xfe\n\x00\x01\n");
    let out = run_ok(&format!(r#"print(line_count("{}"))"#, p.display())).await;
    assert_eq!(out, "2");
}
