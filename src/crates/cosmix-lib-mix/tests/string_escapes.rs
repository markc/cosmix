//! Double-quoted string escapes (0.90.0) — `\xHH`, `\0`, `\a \b \f \v`.
//!
//! Before 0.90.0 every one of these was kept LITERALLY (`"isn\x27t"`
//! printed `isn\x27t`) and lint said nothing, so a habit from every other
//! language became silently wrong output that no gate saw — a `replace()`
//! wrote a literal `isn\x27t` into a committed journal entry (2026-09-17).
//!
//! The other half of the entry is the compatibility table: `\uXXXX`
//! unbraced, single quotes and heredocs all keep their rules BY DESIGN,
//! and these pin that they still do.

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
async fn hex_escape_decodes_exactly_two_digits() {
    // The filing case, plus the ASCII pair.
    let out = run("print(\"isn\\x27t\")\nprint(\"\\x41\\x42\")\n")
        .await
        .unwrap();
    assert_eq!(out, "isn't\nAB\n");
}

#[tokio::test]
async fn hex_escape_above_7f_is_the_codepoint_not_a_raw_byte() {
    // A Mix String is UTF-8, so `\xff` MUST be U+00FF (two bytes on the
    // wire), never the single byte 0xFF — which would not be valid UTF-8.
    // `bytes_from_hex` is the byte route and keeps its own spelling.
    let out = run("print(\"\\xff\" == chr(255))\nprint(len(\"\\xff\"))\nprint(byte_length(\"\\xff\"))\n")
        .await
        .unwrap();
    assert_eq!(out, "true\n1\n2\n");
}

#[tokio::test]
async fn short_hex_escape_stays_literal_rather_than_failing_to_lex() {
    // Deliberately NOT a lex error: a regex pattern meaning the engine's
    // own `\x` must not become a hard failure of the whole file. Lint
    // reports it (MIX-W2405).
    let out = run("print(\"\\x4\")\nprint(\"\\xzz\")\n").await.unwrap();
    assert_eq!(out, "\\x4\n\\xzz\n");
}

#[tokio::test]
async fn nul_and_the_control_escapes() {
    // `\0` is NUL exactly, never the start of an octal escape — octal is
    // ambiguous next to digits, so `"\012"` is NUL then the text "12".
    let out = run("print(len(\"a\\0b\"))\nprint(len(\"\\012\"))\n\
                   print(\"\\a\" == chr(7))\nprint(\"\\b\" == chr(8))\n\
                   print(\"\\f\" == chr(12))\nprint(\"\\v\" == chr(11))\n")
        .await
        .unwrap();
    assert_eq!(out, "3\n3\ntrue\ntrue\ntrue\ntrue\n");
}

#[tokio::test]
async fn the_deliberate_literals_are_untouched() {
    // Unbraced `\u` stays literal BY DESIGN (it protects embedded JSON and
    // `C:\users`); a doubled backslash is a literal backslash; single
    // quotes take only `\'` and `\\`; heredocs keep their own rules.
    let out = run("print(\"json \\uABCD\")\nprint(\"C:\\users\")\nprint(\"re \\\\d+\")\n\
                   print('raw \\x27 here')\nprint(\"\\u{27}\")\n")
        .await
        .unwrap();
    assert_eq!(
        out,
        "json \\uABCD\nC:\\users\nre \\d+\nraw \\x27 here\n'\n"
    );

    let out = run("$h = <<END\nheredoc \\x27 and \\0 stay literal\nEND\nprint($h)\n")
        .await
        .unwrap();
    assert_eq!(out, "heredoc \\x27 and \\0 stay literal\n");
}
