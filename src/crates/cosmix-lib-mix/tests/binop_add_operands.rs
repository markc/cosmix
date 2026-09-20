//! `+` operand typing (0.90.0).
//!
//! Until 0.90.0 the `+` fallback stringified EVERY value type, so
//! `["a"] + ["b"]` was the string `[a][b]` with rc 0 and nothing failed
//! until far from the cause. These pin both halves: the collection/function
//! operands now raise, and the scalar coercions that real scripts depend on
//! are untouched.

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
async fn collection_operands_raise_and_name_the_builtin() {
    // The four reproducers from the 2026-09-17 filing, each of which
    // produced a plausible-looking STRING before this change.
    let err = run("print([\"a\"] + [\"b\"])\n")
        .await
        .expect_err("list + list must raise");
    assert!(err.contains("concat(a, b)"), "must name concat: {err}");

    let err = run("print({a: 1} + {b: 2})\n")
        .await
        .expect_err("map + map must raise");
    assert!(err.contains("merge(a, b)"), "must name merge: {err}");

    // Mixed shapes — EITHER operand is enough, deliberately wider than the
    // `==` rule, which needs both.
    for src in ["print([1] + 2)\n", "print(1 + [2])\n"] {
        let err = run(src).await.expect_err("mixed collection + must raise");
        assert!(
            err.contains("not defined for") && err.contains(".."),
            "must point at `..` for text: {err}"
        );
    }

    // Bytes and a function value take the same path.
    let err = run("print(string_to_bytes(\"a\") + \"b\")\n")
        .await
        .expect_err("bytes + must raise");
    assert!(err.contains("not defined for"), "{err}");

    let err = run("$f = fn($x) $x end\nprint($f + 1)\n")
        .await
        .expect_err("function + must raise");
    assert!(err.contains("not defined for"), "{err}");
}

#[tokio::test]
async fn scalar_addition_and_the_string_fallback_are_unchanged() {
    // The legacy coercions every fleet script leans on. `nil` stays a
    // SCALAR on purpose: `nil + 1` is "nil1", its own footgun but not this
    // one, and moving it would break absent-key-into-a-message lines
    // data-dependently.
    let out = run("print(1 + 2)\nprint(\"a\" + \"b\")\nprint(\"3\" + 4)\n\
                   print(true + 1)\nprint(nil + 1)\n")
        .await
        .unwrap();
    assert_eq!(out, "3\nab\n7\n2\nnil1\n");
}

#[tokio::test]
async fn indexing_a_collection_then_adding_still_works() {
    // The overwhelmingly common fleet shape (`$TALLY["pass"] + 1`) —
    // the operand is the ELEMENT, a scalar, not the container.
    let out = run("$t = {pass: 1}\n$t[\"pass\"] = $t[\"pass\"] + 1\nprint($t[\"pass\"])\n\
                   $l = [10, 20]\nprint($l[0] + $l[1])\n")
        .await
        .unwrap();
    assert_eq!(out, "2\n30\n");
}
