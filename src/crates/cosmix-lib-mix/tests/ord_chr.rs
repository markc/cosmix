//! `ord()` / `chr()` — codepoint ↔ character (0.90.0).
//!
//! Before these, the only way to ask what a string held was
//! `bytes_to_hex(string_to_bytes($s))`, which answers in UTF-8 BYTES, not
//! codepoints — so `ord("é")` had no spelling at all. They take the
//! `\u{...}` literal's validity rule exactly: surrogates and >0x10FFFF are
//! not characters and must raise rather than round-trip to a wrong one.

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
async fn ord_and_chr_cover_ascii_latin1_and_bmp() {
    let out = run("print(ord(\"A\"))\nprint(ord(\"é\"))\nprint(ord(\"❤\"))\n\
                   print(chr(65))\nprint(chr(233))\nprint(chr(10084))\n")
        .await
        .unwrap();
    assert_eq!(out, "65\n233\n10084\nA\né\n❤\n");
}

#[tokio::test]
async fn ord_reads_the_first_character_not_the_first_byte() {
    // The distinction the byte route could not make: "éa" is 3 bytes, and
    // its first BYTE is 0xC3, which is not a codepoint of anything.
    let out = run("print(ord(\"éa\"))\nprint(chr(ord(\"😀\")))\n")
        .await
        .unwrap();
    assert_eq!(out, "233\n😀\n");
}

#[tokio::test]
async fn chr_round_trips_with_the_unicode_escape() {
    // chr() is the runtime twin of `\u{...}` — the pair must agree, and
    // chr(0) must produce a real NUL (not "", which is why ord("") raises
    // rather than answering 0).
    let out = run("print(chr(0x27))\nprint(\"\\u{27}\")\nprint(len(chr(0)))\n")
        .await
        .unwrap();
    assert_eq!(out, "'\n'\n1\n");
}

#[tokio::test]
async fn ord_of_empty_raises_rather_than_answering_zero() {
    let err = run("print(ord(\"\"))\n")
        .await
        .expect_err("ord(\"\") must raise");
    assert!(err.contains("empty string"), "{err}");
}

#[tokio::test]
async fn chr_refuses_surrogates_and_out_of_range() {
    for (src, needle) in [
        ("print(chr(0xD800))\n", "surrogate"),
        ("print(chr(0xDFFF))\n", "surrogate"),
        ("print(chr(0x110000))\n", "0x10FFFF"),
        ("print(chr(-1))\n", "whole codepoint"),
        // f64 arithmetic makes a fractional codepoint reachable; `as u32`
        // would have truncated it to a plausible wrong character.
        ("print(chr(65.5))\n", "whole codepoint"),
    ] {
        let err = run(src).await.unwrap_err();
        assert!(err.contains(needle), "{src} -> {err}");
    }
}
