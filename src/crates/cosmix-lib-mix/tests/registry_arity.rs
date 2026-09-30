//! A7 (TODO-mix 2026-09-24): the arity test GENERATED from the registry.
//! Every finite contract must reject `max + 1` and (where `min >= 1`)
//! `min - 1` under strict mode, BEFORE any side effect; exact-arity sets
//! reject the first count above their largest accepted. The old
//! `arity_enforced` pinned only data_encode([]) / data_parse([]) — this
//! one runs the whole registry, so a drifted contract row fails here
//! instead of surprising a caller at runtime.

use cosmix_mix::evaluator::Evaluator;
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;

async fn run_strict_code(src: &str) -> Result<String, String> {
    let mut lexer = Lexer::new(src);
    let tokens = lexer.tokenize().map_err(|e| e.to_string())?;
    let mut parser = Parser::new(tokens, src);
    let stmts = parser.parse_program().map_err(|e| e.to_string())?;
    let stdout = cosmix_mix::evaluator::SharedBuf::new();
    let stderr = cosmix_mix::evaluator::SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    eval.set_arity_mode(cosmix_mix::ArityMode::Strict);
    eval.execute(&stmts).await.map_err(|e| e.to_string())?;
    Ok(stdout.to_string_lossy())
}

/// The error CODE a strict-arity violation carries, via `catch $m, $e`.
async fn arity_code(name: &str, n: usize) -> Result<String, String> {
    let args: Vec<String> = (0..n).map(|_| "nil".to_string()).collect();
    run_strict_code(&format!(
        "try\n  {name}({})\ncatch $m, $e\n  print($e.code)\nend\n",
        args.join(", ")
    ))
    .await
}

#[tokio::test]
async fn every_finite_contract_rejects_surplus_and_deficit() {
    let mut checked = 0usize;
    for info in cosmix_mix::builtins::builtin_entries() {
        let name = info.name;
        // Statement-only contracts (send/emit/address/on/reply) and
        // evaluator specials are not callable as functions.
        if !cosmix_mix::builtins::is_builtin(name) {
            continue;
        }
        if let Some(set) = info.contract.exact_arities {
            let max = *set.iter().max().expect("non-empty exact set");
            let code = arity_code(name, max + 1)
                .await
                .unwrap_or_else(|e| panic!("{name}({}) did not produce a catchable error: {e}", max + 1));
            assert_eq!(
                code.trim(),
                "ARITY_MISMATCH",
                "{name}: {max}+1 args must be ARITY_MISMATCH, got: {code:?}"
            );
            checked += 1;
            continue;
        }
        let min = info.contract.arity_min();
        let Some(max) = info.contract.arity_max() else {
            continue; // variadic — no upper bound to violate
        };
        let code = arity_code(name, max + 1)
            .await
            .unwrap_or_else(|e| panic!("{name}({}) did not produce a catchable error: {e}", max + 1));
        assert_eq!(
            code.trim(),
            "ARITY_MISMATCH",
            "{name}: {max}+1 args must be ARITY_MISMATCH, got: {code:?}"
        );
        if min >= 1 {
            let code = arity_code(name, min - 1)
                .await
                .unwrap_or_else(|e| panic!("{name}({}) did not produce a catchable error: {e}", min - 1));
            assert_eq!(
                code.trim(),
                "ARITY_MISMATCH",
                "{name}: {}-1 args must be ARITY_MISMATCH, got: {code:?}",
                min
            );
        }
        checked += 1;
    }
    // The registry is large and only grows — pin the floor so a broken
    // iterator (empty table) can never make this test vacuously green.
    assert!(checked > 200, "registry iteration produced only {checked} checks");
}

#[tokio::test]
async fn rejection_happens_before_the_side_effect() {
    // The sentinel: a surplus-arg remove() must be refused BEFORE the
    // file is touched, not after deleting it.
    let dir = std::env::temp_dir().join(format!("mix-a7-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("tmp dir");
    let victim = dir.join("victim");
    std::fs::write(&victim, "keep").expect("write victim");
    let src = format!(
        "try\n  remove(\"{}\", nil)\ncatch $m, $e\n  print($e.code)\nend\n",
        victim.display()
    );
    let out = run_strict_code(&src).await.expect("catchable");
    assert_eq!(out.trim(), "ARITY_MISMATCH");
    assert!(victim.exists(), "the file must survive the refusal");
    std::fs::remove_dir_all(&dir).ok();
}
