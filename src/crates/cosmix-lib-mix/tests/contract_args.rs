//! A2 (TODO-mix 2026-09-24): contract argument TYPE checks — always-on
//! for the side-effect classes (fs/process/network/bus), strict-mode-only
//! for pure builtins in the first release. The 0.93-era side effects were
//! verified then: mkdir(42) created a directory named `42`,
//! write_file(99, "x") wrote a file named `99`.

use cosmix_mix::evaluator::Evaluator;
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;

async fn run(src: &str, strict: bool) -> Result<String, String> {
    let mut lexer = Lexer::new(src);
    let tokens = lexer.tokenize().map_err(|e| e.to_string())?;
    let mut parser = Parser::new(tokens, src);
    let stmts = parser.parse_program().map_err(|e| e.to_string())?;
    let stdout = cosmix_mix::evaluator::SharedBuf::new();
    let stderr = cosmix_mix::evaluator::SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    if strict {
        eval.set_arity_mode(cosmix_mix::ArityMode::Strict);
    }
    eval.execute(&stmts).await.map_err(|e| e.to_string())?;
    Ok(stdout.to_string_lossy())
}

#[tokio::test]
async fn critical_class_types_gate_in_every_mode() {
    // exists(path: string) — FsRead — gates in BOTH modes: a number path
    // is a probe on the wrong target. The TYPE_MISMATCH code rides the
    // structured error (catch $m, $e → $e.code); the plain message names
    // the argument, the shape and the actual type.
    for strict in [false, true] {
        let err = run("print(exists(42))", strict)
            .await
            .expect_err("number path must raise");
        assert!(err.contains("must be string"), "mode {strict}: got: {err}");
        assert!(err.contains("got number"), "mode {strict}: got: {err}");
    }
}

#[tokio::test]
async fn pure_builtin_types_gate_only_under_strict_mode() {
    // len(v) — Pure — gates under strict mode only in this first
    // release; in compatible mode the gate stays silent and the call
    // falls through to len's own behavior (which refuses a number with
    // ITS message, not the contract gate's).
    let err = run("print(len(3))", true)
        .await
        .expect_err("strict mode gates pure types");
    assert!(err.contains("must be string | list | map | bytes | buffer"), "got: {err}");
    let err = run("print(len(3))", false)
        .await
        .expect_err("len(3) still fails — but on len's own terms");
    assert!(
        !err.contains("must be string | list | map | bytes | buffer"),
        "the contract gate must not fire in compatible mode: {err}"
    );
}

#[tokio::test]
async fn well_typed_calls_pass_in_both_modes() {
    for strict in [false, true] {
        run("print(exists(\".\"))", strict)
            .await
            .expect("string path passes");
        run("print(len([1, 2]))", strict)
            .await
            .expect("list arg passes");
    }
}
