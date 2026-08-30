//! P4 of the mix tokenizer fuzz/property corpus
//! (_doc/planned/mix-tokenizer-fuzz-corpus.md in the cosmix hub): property
//! coverage of the EVALUATOR — the hardest slice, because evaluation has side
//! effects and can fail to terminate. We make it safe two ways:
//!
//!   1. Effects sandboxed BY CONSTRUCTION — the generator emits only a pure
//!      subset: literals, arithmetic/logical/comparison ops, `..` concat, and
//!      the pure builtins (length/join/split/substr/contains/to_number/trim/
//!      is_empty/pos). It NEVER emits an I/O builtin (run/ssh_run/write_file/
//!      http/…), `send`/Bus, `source`, or user functions. A bare
//!      `Evaluator::new()` with no Bus/shell handler and no prelude has no live
//!      I/O surface for these programs to reach.
//!   2. Termination BY CONSTRUCTION — no `while`, no user functions (so no
//!      recursion / stack overflow), and `for each` only over small literal
//!      lists and never nested, so every program halts in a bounded number of
//!      steps with bounded allocation. There is no execution watchdog because
//!      there is nothing for it to bound; if `while`/functions are ever added to
//!      the generator, a cooperative-interrupt timeout (via `interrupt_flag`)
//!      must be added with them.
//!
//! The oracle is simply: evaluation NEVER panics. Any input is allowed to
//! return `Ok` or a structured `MixError` — only an actual panic (e.g. an
//! unguarded arithmetic op, an index slip, a `.unwrap()` on attacker-shaped
//! data) fails the test.

use cosmix_mix::ast::Stmt;
use cosmix_mix::evaluator::Evaluator;
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;
use proptest::prelude::*;

/// A pure, depth-bounded Mix expression.
fn expr() -> impl Strategy<Value = String> {
    let leaf = prop_oneof![
        (0i64..50).prop_map(|n| n.to_string()),
        "\"[a-z ]{0,4}\"".prop_map(|s| s),
        Just("true".to_string()),
        Just("false".to_string()),
        Just("nil".to_string()),
        Just("$x".to_string()),
        Just("$y".to_string()),
        Just("$z".to_string()),
        Just("$i".to_string()),
        Just("[1, 2, 3]".to_string()),
        Just("[]".to_string()),
        Just("{a: 1, b: 2}".to_string()),
    ];
    // depth 3, ~24 nodes max, ~3 children/branch — bounds AST size so the
    // recursive evaluator can't be driven to a stack overflow.
    leaf.prop_recursive(3, 24, 3, |inner| {
        prop_oneof![
            (
                inner.clone(),
                prop::sample::select(vec![
                    "+", "-", "*", "/", "%", "..", "==", "!=", "<", ">", "and", "or"
                ]),
                inner.clone(),
            )
                .prop_map(|(a, op, b)| format!("({a} {op} {b})")),
            inner.clone().prop_map(|e| format!("not ({e})")),
            (
                prop::sample::select(vec!["length", "to_number", "trim", "is_empty"]),
                inner.clone(),
            )
                .prop_map(|(f, e)| format!("{f}({e})")),
            inner.clone().prop_map(|e| format!("join({e}, \", \")")),
            inner.clone().prop_map(|e| format!("split({e}, \" \")")),
            inner.clone().prop_map(|e| format!("contains({e}, \"a\")")),
            inner.clone().prop_map(|e| format!("substr({e}, 0, 2)")),
            inner.prop_map(|e| format!("pos(\"a\", {e})")),
        ]
    })
}

/// A non-looping statement: bare expression, assignment, or `if`.
fn simple_stmt() -> impl Strategy<Value = String> {
    prop_oneof![
        expr(),
        (prop::sample::select(vec!["x", "y", "z"]), expr())
            .prop_map(|(v, e)| format!("${v} = {e}")),
        (expr(), expr(), expr())
            .prop_map(|(c, a, b)| format!("if ({c}) then\n  {a}\nelse\n  {b}\nend")),
    ]
}

/// A top-level statement: a simple statement or a single (never nested) bounded
/// `for each`. The iterable is always a small LITERAL list (never `$z`, which an
/// earlier statement could reassign) so the loop bound is fixed by construction.
/// No `do` — it is not a Mix loop keyword (the lexer has no `Token::Do`); the
/// grammar is `for each $i in <list>` <newline> <body> `end`.
fn stmt() -> impl Strategy<Value = String> {
    prop_oneof![
        simple_stmt(),
        (
            prop::sample::select(vec!["[1, 2, 3]", "[]", "[10, 20]", "[1, 2, 3, 4]"]),
            simple_stmt(),
        )
            .prop_map(|(list, body)| format!("for each $i in {list}\n  {body}\nend")),
    ]
}

/// A whole program: the bound-everything prelude (so var refs always resolve)
/// plus 1..3 statements.
fn program() -> impl Strategy<Value = String> {
    prop::collection::vec(stmt(), 1..4).prop_map(|stmts| {
        format!(
            "$x = 3\n$y = \"ab\"\n$z = [1, 2, 3]\n$i = 0\n{}",
            stmts.join("\n")
        )
    })
}

/// Evaluate a parsed program on a current-thread runtime. A panic in the eval
/// future propagates through `block_on` to this (test) thread, failing the
/// proptest case; `Ok` and a structured `MixError` are both acceptable.
fn run_eval(stmts: &[Stmt]) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let local = tokio::task::LocalSet::new();
    rt.block_on(local.run_until(async {
        let mut eval = Evaluator::new();
        let _ = eval.execute(stmts).await;
    }));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn eval_pure_subset_never_panics(src in program()) {
        // The generator emits only valid Mix, so lex+parse MUST succeed. Asserting
        // it (rather than silently skipping a failure) keeps the property from
        // going vacuous and surfaces any generator bug loudly — the evaluator runs
        // on every case.
        let mut lx = Lexer::new(&src);
        let tokens = lx
            .tokenize()
            .map_err(|e| TestCaseError::fail(format!("lex failed: {e:?} for {src:?}")))?;
        let stmts = Parser::new(tokens, &src)
            .parse_program()
            .map_err(|e| TestCaseError::fail(format!("parse failed: {e:?} for {src:?}")))?;
        // Only a PANIC fails — Ok or a structured MixError are both acceptable.
        run_eval(&stmts);
    }
}
