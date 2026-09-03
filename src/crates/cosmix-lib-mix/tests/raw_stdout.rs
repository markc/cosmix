//! `write_stdout` / `write_stderr` / `print_raw` / `eprint_raw` (v0.65.0) —
//! the byte-exact output family.
//!
//! These assertions compare **raw bytes**, not lines. That matters: the
//! `tests/scripts/*.mix` harness compares `output.trim().lines()`, which is
//! blind to exactly the two properties this family exists to provide — the
//! ABSENCE of a trailing newline and the ABSENCE of a separator between
//! arguments. A script test would pass just as happily against a `print`
//! that still appended "\n", so the feature needs a test at this level.

use cosmix_mix::evaluator::{Evaluator, SharedBuf};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;

/// Run a script, returning the exact (stdout, stderr) bytes.
async fn run_bytes(source: &str) -> Result<(Vec<u8>, Vec<u8>), String> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().map_err(|e| e.to_string())?;
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program().map_err(|e| e.to_string())?;

    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    let outcome = eval.execute(&stmts).await;
    let out = (stdout.take(), stderr.take());
    match outcome {
        Ok(_) => Ok(out),
        Err(e) => Err(e.to_string()),
    }
}

async fn out_bytes(source: &str) -> Vec<u8> {
    run_bytes(source)
        .await
        .unwrap_or_else(|e| panic!("script failed: {e}\n{source}"))
        .0
}

#[tokio::test]
async fn appends_no_newline_and_no_separator() {
    // Three writes, five arguments, and the result is nine bytes with
    // nothing between or after them. `print` would have produced
    // "a b\nc\n" for the same values.
    assert_eq!(out_bytes("write_stdout(\"a\", \"b\")\n").await, b"ab");
    assert_eq!(
        out_bytes("write_stdout(\"a\")\nwrite_stdout(\"b\", \"c\")\n").await,
        b"abc"
    );
    // The contrast, in the same test, so the claim is comparative and not
    // just an assertion about one function in isolation.
    assert_eq!(out_bytes("print \"a\", \"b\"\n").await, b"a b\n");
}

#[tokio::test]
async fn bytes_go_out_verbatim_not_as_the_placeholder() {
    // The bytes here are not valid UTF-8 in any order: 0xFF, 0xFE and a
    // truncated two-byte sequence 0xC3 0x28. Nothing may re-encode them,
    // and nothing may substitute U+FFFD.
    let src = "write_stdout(bytes_from([0x00, 0xFF, 0xFE, 0xC3, 0x28]))\n";
    assert_eq!(out_bytes(src).await, vec![0x00, 0xFF, 0xFE, 0xC3, 0x28]);
    // A buffer is the same, and is NOT frozen or copied through a string.
    assert_eq!(
        out_bytes("write_stdout(buffer([0xFF, 0x00]))\n").await,
        vec![0xFF, 0x00]
    );
    // This is the whole reason the family cannot just be `print` minus the
    // newline: `print` renders a bytes value as its placeholder.
    assert_eq!(
        out_bytes("print(bytes_from([0xFF, 0x00]))\n").await,
        b"<bytes:2>\n"
    );
}

#[tokio::test]
async fn every_other_value_renders_exactly_as_print_does() {
    // The ONE contract, stated as an equality: for every value where
    // `print` is meaningful, write_stdout($x) is print($x) without the
    // newline. If a future edit makes this family stricter or looser about
    // some type, this test says so.
    for expr in [
        "\"text\"",
        "42",
        "-1.5",
        "true",
        "nil",
        "[1, 2]",
        "{k: \"v\"}",
    ] {
        let raw = out_bytes(&format!("write_stdout({expr})\n")).await;
        let printed = out_bytes(&format!("print({expr})\n")).await;
        assert_eq!(
            printed,
            [raw.clone(), b"\n".to_vec()].concat(),
            "write_stdout({expr}) must equal print({expr}) minus the newline"
        );
    }
}

#[tokio::test]
async fn raw_spellings_are_aliases_not_variants() {
    // print_raw/eprint_raw are the same builtin under another name. Pinned
    // as an EQUALITY over a mixed argument list, so a divergence in either
    // direction (rendering, separator, byte handling) fails here — the
    // "two families, one letter apart" trap this release set out to avoid.
    let args = "\"s\", 7, bytes_from([0xFF]), [1, 2], nil";
    assert_eq!(
        out_bytes(&format!("write_stdout({args})\n")).await,
        out_bytes(&format!("print_raw({args})\n")).await
    );
    let (_, err_write) = run_bytes(&format!("write_stderr({args})\n")).await.unwrap();
    let (_, err_alias) = run_bytes(&format!("eprint_raw({args})\n")).await.unwrap();
    assert_eq!(err_write, err_alias);
    // ...and the two streams carry the same bytes as each other.
    assert_eq!(out_bytes(&format!("write_stdout({args})\n")).await, err_write);
}

#[tokio::test]
async fn streams_do_not_cross() {
    let (out, err) = run_bytes("write_stdout(\"O\")\nwrite_stderr(\"E\")\n")
        .await
        .unwrap();
    assert_eq!(out, b"O");
    assert_eq!(err, b"E");
}

#[tokio::test]
async fn zero_args_raises_rather_than_writing_nothing() {
    let err = run_bytes("write_stdout()\n").await.unwrap_err();
    assert!(err.contains("at least one value"), "{err}");
}

/// A sink whose every write fails with a chosen `ErrorKind`.
///
/// `SharedBuf` can never fail, so without this the whole error contract —
/// that a dropped write RAISES rather than being swallowed, with the right
/// code, under ALL FOUR names — was pinned only for stdout-through-a-real-
/// pipe (`IO_BROKEN_PIPE` in `cosmix-mix/tests/stdio_filter.rs`).
/// `IO_WRITE_FAILED` had no test at all, and the alias equality was pinned
/// on the success path only: `print_raw` could have quietly swallowed errors
/// while `write_stdout` raised, and nothing would have gone red.
#[derive(Clone)]
struct FailingSink(std::io::ErrorKind);

impl std::io::Write for FailingSink {
    fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::new(self.0, "sink refused the write"))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::new(self.0, "sink refused the flush"))
    }
}

/// Run with BOTH sinks failing, and return the error string.
async fn run_failing(source: &str, kind: std::io::ErrorKind) -> String {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().expect("lex");
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program().expect("parse");
    let mut eval = Evaluator::with_output(
        Box::new(FailingSink(kind)),
        Box::new(FailingSink(kind)),
    );
    eval.execute(&stmts)
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_else(|| panic!("a failed write must raise, but the script succeeded: {source}"))
}

#[tokio::test]
async fn a_failed_write_raises_under_every_name() {
    // All four names, both streams, both codes. The `$err.code` is read
    // inside the script and re-raised so the assertion is on the CODE the
    // language exposes, not on a Rust-side type.
    for name in ["write_stdout", "write_stderr", "print_raw", "eprint_raw"] {
        for (kind, code) in [
            (std::io::ErrorKind::BrokenPipe, "IO_BROKEN_PIPE"),
            (std::io::ErrorKind::PermissionDenied, "IO_WRITE_FAILED"),
        ] {
            let src = format!(
                "try\n  {name}(\"x\")\ncatch $m, $e\n  die(\"CODE=\" .. $e.code)\nend\n"
            );
            let err = run_failing(&src, kind).await;
            assert!(
                err.contains(&format!("CODE={code}")),
                "{name} with {kind:?} should raise {code}, got: {err}"
            );
        }
    }
}

#[tokio::test]
async fn print_still_swallows_what_this_family_raises() {
    // The stated contrast, made a test: `print` and `printf` discard a write
    // error and carry on. If someone "fixes" that for consistency, this goes
    // red and the docs get revisited deliberately rather than by accident.
    let mut lexer = Lexer::new("print(\"a\")\nprintf(\"%s\", \"b\")\n$ok = 1\n");
    let tokens = lexer.tokenize().expect("lex");
    let mut parser = Parser::new(tokens, "");
    let stmts = parser.parse_program().expect("parse");
    let mut eval = Evaluator::with_output(
        Box::new(FailingSink(std::io::ErrorKind::BrokenPipe)),
        Box::new(FailingSink(std::io::ErrorKind::BrokenPipe)),
    );
    assert!(
        eval.execute(&stmts).await.is_ok(),
        "print/printf must still swallow write errors"
    );
}

#[tokio::test]
async fn round_trip_through_a_bytes_value_is_exact() {
    // The end-to-end property, at library level: bytes that cannot survive
    // a UTF-8 decode go in and come out unchanged. The process-level twin
    // (real fd 0 -> real fd 1, via the built binary) is
    // crates/cosmix-mix/tests/stdio_filter.rs.
    let src = concat!(
        "$b = bytes_from([0x00, 0x01, 0xFF, 0xFE, 0x20, 0xC3, 0x28, 0x0A])\n",
        "write_stdout($b)\n",
    );
    assert_eq!(
        out_bytes(src).await,
        vec![0x00, 0x01, 0xFF, 0xFE, 0x20, 0xC3, 0x28, 0x0A]
    );
}
