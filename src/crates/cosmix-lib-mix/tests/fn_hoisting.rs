//! Top-level `fn` hoisting (mix 0.63.0): every function declared at the
//! DIRECT top level of an invocation root is bound before the first
//! statement runs, so a forward call works and a helper referenced only
//! on a rare branch cannot die in production for sitting below its call
//! site. Top level ONLY — a `fn` nested in a block (an `if` branch, a
//! loop body) still binds when its branch executes, and function bodies
//! (`function_depth > 0`) are not hoisted. Mix `fn` captures nothing, so
//! an early binding cannot close over a not-yet-defined value.

use cosmix_mix::evaluator::{Evaluator, SharedBuf};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;
use cosmix_mix::run_capturing;

async fn output(source: &str) -> String {
    let (_, stdout, stderr) = run_capturing(source).await.expect("source should run");
    assert!(stderr.is_empty(), "unexpected stderr: {stderr}");
    stdout
}

async fn error_of(source: &str) -> String {
    match run_capturing(source).await {
        Ok((v, out, err)) => panic!("expected an error, got {v:?} (stdout={out:?} stderr={err:?})"),
        Err(e) => e.to_string(),
    }
}

// ---------- forward calls now work ----------

#[tokio::test]
async fn forward_call_at_top_level() {
    let out = output("print(f(2))\nfn f($x)\n  return $x * 2\nend\n").await;
    assert_eq!(out, "4\n");
}

#[tokio::test]
async fn forward_call_on_a_rare_branch() {
    // The production failure shape: the helper is only reached on a
    // branch, and sits below the call site.
    let out = output("if 1 == 1 then\n  print(g(3))\nend\nfn g($x)\n  return $x + 1\nend\n").await;
    assert_eq!(out, "4\n");
}

#[tokio::test]
async fn forward_call_through_another_hoisted_fn() {
    // `a` calls `b`; both sit below the call. Both are hoisted.
    let out =
        output("print(a())\nfn a()\n  return b() + 1\nend\nfn b()\n  return 10\nend\n").await;
    assert_eq!(out, "11\n");
}

#[tokio::test]
async fn mutual_recursion_with_call_between_the_defs() {
    // `even` is invoked after its own def but before `odd`'s statement
    // has executed — hoisting binds both up front.
    let src = "fn even($n)\n  if $n == 0 then\n    return 1\n  end\n  return odd($n - 1)\nend\nprint(even(4))\nfn odd($n)\n  if $n == 0 then\n    return 0\n  end\n  return even($n - 1)\nend\n";
    assert_eq!(output(src).await, "1\n");
}

// ---------- scope of the hoist: top level only ----------

#[tokio::test]
async fn fn_inside_an_if_branch_is_not_hoisted() {
    // Hoisting a conditionally-defined fn would make the definition
    // unconditional — meaning change. It still binds when the branch runs.
    let err = error_of("print(h(1))\nif 1 == 1 then\n  fn h($x)\n    return $x\n  end\nend\n").await;
    assert!(
        err.contains("undefined function"),
        "expected undefined function, got: {err}"
    );
    // …and once the branch has executed, the call works as before.
    let out =
        output("if 1 == 1 then\n  fn h($x)\n    return $x + 10\n  end\nend\nprint(h(1))\n").await;
    assert_eq!(out, "11\n");
}

#[tokio::test]
async fn fn_body_is_not_an_invocation_root_for_hoisting() {
    // A nested fn below a `return` in a function body stays unreachable:
    // bodies run at function_depth > 0 and are not pre-scanned.
    let err = error_of(
        "fn outer()\n  return inner()\n  fn inner()\n    return 5\n  end\nend\nprint(outer())\n",
    )
    .await;
    assert!(
        err.contains("undefined function"),
        "expected undefined function, got: {err}"
    );
}

// ---------- redefinition semantics are pinned ----------

#[tokio::test]
async fn duplicate_defs_between_defs_still_sees_the_earlier_one() {
    // Before any statement runs, a call sees the LAST hoisted definition;
    // between the two defs, the in-flow re-binding preserves the old
    // behaviour (the earlier definition); after both, the last wins.
    let src = "print(f())\nfn f()\n  return 1\nend\n$a = f()\nfn f()\n  return 2\nend\nprint($a .. \" \" .. f())\n";
    assert_eq!(output(src).await, "2\n1 2\n");
}

// ---------- diagnostics unchanged ----------

#[tokio::test]
async fn genuinely_undefined_function_still_errors() {
    let err = error_of("print(nosuchfn())\n").await;
    assert!(
        err.contains("undefined function 'nosuchfn'"),
        "got: {err}"
    );
}

#[tokio::test]
async fn hoisted_def_emits_definition_warnings_exactly_once() {
    // warn_dead_param_pushes fires on the in-flow FunctionDef arm, not in
    // the hoisting pre-pass — a hoisted definition must warn once, in the
    // same place as before hoisting existed.
    let source = "function fat_snare($drums)\n  push($drums, 1)\nend\n";
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().expect("lex");
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program().expect("parse");
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    eval.execute(&stmts).await.expect("eval");
    let e = stderr.to_string_lossy();
    assert_eq!(
        e.matches("mix: warning:").count(),
        1,
        "exactly one definition-time warning; got: {e}"
    );
}
