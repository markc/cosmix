//! CoW (card 4, 0.28.0) — semantics pins and safety tests.
//!
//! `Value::List/Map/Bytes` payloads became `Rc`-backed copy-on-write.
//! Everything here pins OBSERVABLE behaviour to what 0.27.0 did (probed
//! before implementation): sharing must never be visible from Mix.
//! The deep-nest drop tests exercise the Rc-aware iterative `Drop`.

use cosmix_mix::evaluator::{Evaluator, SharedBuf};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;
use cosmix_mix::value::Value;

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

// ── Aliasing / mutation matrix ──────────────────────────────────────

#[tokio::test]
async fn copy_then_mutate_either_side_isolates() {
    let out = run("$a = [1, 2]\n$b = $a\npush($b, 3)\nprint($a)\nprint($b)\n\
                   $c = [1]\n$d = $c\npush($c, 9)\nprint($c)\nprint($d)\n")
    .await
    .unwrap();
    assert_eq!(out, "[1, 2]\n[1, 2, 3]\n[1, 9]\n[1]\n");
}

#[tokio::test]
async fn map_copy_isolation_field_and_index_assign() {
    let out = run("$m1 = {x: 1}\n$m2 = $m1\n$m2.x = 9\n$m2[\"y\"] = 2\n\
                   print($m1)\nprint($m2)\n")
    .await
    .unwrap();
    assert_eq!(out, "{x: 1}\n{x: 9, y: 2}\n");
}

#[tokio::test]
async fn list_index_assign_isolation_and_pop_shift() {
    let out = run(
        "$a = [1, 2, 3]\n$b = $a\n$b[0] = 99\nprint($a)\nprint($b)\n\
                   $p = pop($b)\nprint($p)\nprint($a)\n\
                   shift($b)\nprint($b)\nprint($a)\n",
    )
    .await
    .unwrap();
    assert_eq!(out, "[1, 2, 3]\n[99, 2, 3]\n3\n[1, 2, 3]\n[2]\n[1, 2, 3]\n");
}

// Nested map-held list: `push($c.l, 2)` is the documented no-op shape
// (feedback_mix_push_to_map_held_list_noop) — pinned unchanged (probed
// on 0.27.0: BOTH stay {l: [1]}).
#[tokio::test]
async fn nested_map_held_list_push_stays_noop() {
    let out = run("$o = {l: [1]}\n$c = $o\npush($c.l, 2)\nprint($o)\nprint($c)\n")
        .await
        .unwrap();
    assert_eq!(out, "{l: [1]}\n{l: [1]}\n");
}

// Function param mutation stays caller-invisible (value semantics; the
// 0.21.9 dead-push warning contract is covered by dead_push_diagnostic.rs).
#[tokio::test]
async fn param_mutation_stays_caller_invisible() {
    let out = run("function f($xs)\n  push($xs, 99)\n  return len($xs)\nend\n\
                   $l = [1]\nprint(f($l))\nprint($l)\n")
    .await
    .unwrap();
    assert_eq!(out, "2\n[1]\n");
}

// ForEach snapshot semantics: body mutation of the source doesn't
// extend the iteration (probed 0.27.0: 3 iterations, list grows to 6).
#[tokio::test]
async fn foreach_snapshots_the_iterable() {
    let out = run("$l = [1, 2, 3]\n$c = 0\nfor each $x in $l\n  push($l, 9)\n  $c = $c + 1\nend\nprint($c)\nprint(len($l))\n")
        .await
        .unwrap();
    assert_eq!(out, "3\n6\n");
}

// Self-reference shapes must snapshot, never cycle (an Rc cycle would
// leak and print would never terminate). RHS is evaluated into a live
// local before mutation, so make_mut always sees count ≥ 2 and copies.
#[tokio::test]
async fn self_assign_shapes_snapshot_not_cycle() {
    let out = run("$x = [0]\n$x[0] = $x\nprint($x)\n\
                   $y = [1]\npush($y, $y)\nprint($y)\n\
                   $m = {a: 1}\n$m.s = $m\nprint($m)\n")
    .await
    .unwrap();
    assert_eq!(out, "[[0]]\n[1, [1]]\n{a: 1, s: {a: 1}}\n");
}

// ── Equality pins (no ptr_eq fast path — container equality is always
// false in Mix, even self-compare; a ptr_eq shortcut would flip these) ──

#[tokio::test]
async fn container_equality_stays_false_even_shared() {
    let out = run("$l = [1]\nprint($l == $l)\n$m = $l\nprint($l == $m)\n\
                   print({a: 1} == {a: 1})\n\
                   $b1 = string_to_bytes(\"abc\")\n$b2 = string_to_bytes(\"abc\")\nprint($b1 == $b2)\n")
        .await
        .unwrap();
    // bytes: CONTENT equality across distinct allocations stays true.
    assert_eq!(out, "false\nfalse\nfalse\ntrue\n");
}

// ── Deep-nest drop safety (the Rc-aware iterative Drop) ────────────

fn deep_list(depth: usize) -> Value {
    let mut v = Value::list(vec![Value::Number(0.0)]);
    for _ in 0..depth {
        v = Value::list(vec![v]);
    }
    v
}

// Last-owner flatten must fire on the SECOND drop of an aliased chain
// (the first is a shared O(1) decrement).
#[test]
fn deep_nested_drop_through_alias() {
    let handle = std::thread::Builder::new()
        .stack_size(512 * 1024)
        .spawn(|| {
            let v = deep_list(200_000);
            let a = v.clone();
            drop(v); // shared root: refcount decrement, no descent
            drop(a); // sole owner: iterative flatten — must not overflow
        })
        .unwrap();
    handle
        .join()
        .expect("aliased deep drop must not overflow the stack");
}

// A clone of an inner level held while the outer drops: the flatten
// walks down to the shared middle, leaves it intact, and the middle's
// own (last-owner) drop flattens the rest.
#[test]
fn deep_nested_drop_shared_middle() {
    let handle = std::thread::Builder::new()
        .stack_size(512 * 1024)
        .spawn(|| {
            let inner = deep_list(100_000);
            let outer = {
                let mut v = inner.clone();
                for _ in 0..100_000 {
                    v = Value::list(vec![v]);
                }
                v
            };
            drop(outer); // flattens down TO the shared middle, stops there
            drop(inner); // now sole owner: flattens the rest
        })
        .unwrap();
    handle
        .join()
        .expect("shared-middle deep drop must not overflow the stack");
}

// make_mut on a shared deep chain clones ONE level only (children are
// Rc bumps) — no recursive blowup.
#[test]
fn make_mut_on_deep_chain_is_one_level() {
    let handle = std::thread::Builder::new()
        .stack_size(512 * 1024)
        .spawn(|| {
            let v = deep_list(200_000);
            let mut w = v.clone();
            if let Value::List(rc) = &mut w {
                std::rc::Rc::make_mut(rc).push(Value::Number(1.0));
            }
            drop(w);
            drop(v);
        })
        .unwrap();
    handle
        .join()
        .expect("make_mut on a deep chain must be one-level");
}

// ── Size assertion (informational — may be relaxed deliberately) ────

#[test]
fn value_stays_small() {
    // Rc payloads shrink the largest inline payload; a regrowth past 32
    // bytes should be a conscious decision, not an accident.
    assert!(
        std::mem::size_of::<Value>() <= 32,
        "Value grew past 32 bytes: {}",
        std::mem::size_of::<Value>()
    );
}
