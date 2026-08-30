//! Nested lvalue assignment — `$m[$u]["k"] = v`, `$m.a.b = v`,
//! `$l[0][1] = v` (mix 0.33.0).
//!
//! Before 0.33.0 Mix accepted exactly ONE accessor on an assignment
//! target; a second was a parse error, whatever the kind. The workaround
//! (read the inner container out, mutate it, write it back) also made
//! every write deep-copy the inner container — O(N²) when building a map
//! of maps.
//!
//! The rules under test:
//! - 2+ accessors parse and write through, for any mix of `.field` /
//!   `[expr]`, on maps and lists alike.
//! - A missing or nil intermediate is auto-created as a MAP, but only
//!   when the next accessor is a name or a string key. A numeric index is
//!   ambiguous (list slot or map key?) and is refused rather than guessed.
//! - An existing scalar intermediate is never silently replaced.
//! - A list is never extended by assignment, at any depth.
//! - A failed write auto-creates nothing: the statement is all-or-nothing,
//!   because a script can `catch` the error and keep running.
//! - CoW still isolates aliases at every level.

use cosmix_mix::evaluator::{Evaluator, SharedBuf};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;
use cosmix_mix::{DEFAULT_RECURSION_LIMIT, EvalLimits};

async fn run(source: &str) -> Result<String, String> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().map_err(|e| e.to_string())?;
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program().map_err(|e| e.to_string())?;
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    match eval.execute(&stmts).await {
        Ok(_) => Ok(stdout.to_string_lossy()),
        Err(e) => Err(e.to_string()),
    }
}

async fn ok(source: &str) -> String {
    run(source).await.expect("script should succeed")
}

async fn err(source: &str) -> String {
    run(source).await.expect_err("script should raise")
}

// ---------- the shapes that used to be parse errors ----------

#[tokio::test]
async fn index_index() {
    let out = ok("$m = {}\n$m[\"u\"] = {}\n$m[\"u\"][\"k\"] = 1\nprint($m)\n").await;
    assert_eq!(out.trim(), "{u: {k: 1}}");
}

#[tokio::test]
async fn field_field() {
    let out = ok("$m = { u: { k: 1 } }\n$m.u.k = 2\nprint($m)\n").await;
    assert_eq!(out.trim(), "{u: {k: 2}}");
}

#[tokio::test]
async fn field_then_index_and_back() {
    let out = ok("$m = { u: {} }\n$m.u[\"j\"] = 3\n$m[\"u\"].q = 4\nprint($m)\n").await;
    assert_eq!(out.trim(), "{u: {j: 3, q: 4}}");
}

#[tokio::test]
async fn list_element_incl_negative_index() {
    let out = ok("$l = [[1, 2], [3]]\n$l[0][1] = 99\n$l[-1][0] = 7\nprint($l)\n").await;
    assert_eq!(out.trim(), "[[1, 99], [7]]");
}

#[tokio::test]
async fn deep_path_four_levels() {
    let out = ok("$m = {}\n$m.a.b.c.d = \"deep\"\nprint($m)\n").await;
    assert_eq!(out.trim(), "{a: {b: {c: {d: deep}}}}");
}

#[tokio::test]
async fn index_expressions_may_be_computed() {
    let out = ok("$m = {}\n$k = \"g\"\n$m[$k .. \"1\"][to_string(2 * 2)] = 1\nprint($m)\n").await;
    assert_eq!(out.trim(), "{g1: {4: 1}}");
}

// ---------- auto-vivification ----------

#[tokio::test]
async fn vivifies_missing_intermediate_for_a_string_key() {
    let out = ok("$m = {}\n$m[\"g1\"][\"k\"] = 1\nprint($m)\n").await;
    assert_eq!(out.trim(), "{g1: {k: 1}}");
}

#[tokio::test]
async fn vivifies_a_nil_intermediate() {
    let out = ok("$m = { a: nil }\n$m[\"a\"][\"k\"] = 1\nprint($m)\n").await;
    assert_eq!(out.trim(), "{a: {k: 1}}");
}

#[tokio::test]
async fn refuses_to_guess_list_vs_map_for_a_numeric_index() {
    // `$m["a"][0] = 1` could mean a list slot or the map key "0". Mix
    // refuses rather than freezing one interpretation into the data.
    let e = err("$m = {}\n$m[\"a\"][0] = 1\n").await;
    assert!(e.contains("cannot auto-create"), "got: {e}");
    assert!(e.contains("[0]"), "names the offending accessor; got: {e}");
}

#[tokio::test]
async fn explicit_container_makes_the_numeric_index_fine() {
    let out = ok("$m = { a: [0, 0] }\n$m[\"a\"][1] = 9\nprint($m)\n").await;
    assert_eq!(out.trim(), "{a: [0, 9]}");
}

// ---------- refusals ----------

#[tokio::test]
async fn never_overwrites_an_existing_scalar_intermediate() {
    let e = err("$m = { a: 5 }\n$m[\"a\"][\"b\"] = 1\n").await;
    assert!(e.contains("cannot index-assign into number"), "got: {e}");
}

#[tokio::test]
async fn a_list_is_never_extended_by_a_nested_write() {
    let e = err("$m = { a: [1] }\n$m[\"a\"][5] = 9\n").await;
    assert!(e.contains("out of range"), "got: {e}");
}

#[tokio::test]
async fn field_access_on_a_list_is_an_error() {
    let e = err("$m = { a: [1] }\n$m.a.b = 1\n").await;
    assert!(e.contains("list"), "got: {e}");
}

// ---------- atomicity ----------

#[tokio::test]
async fn a_failed_write_vivifies_nothing() {
    // The path dies at the final `[2]` (numeric index into a container
    // that would have to be auto-created). None of the maps it walked
    // through may survive — a script can catch this and carry on.
    let out = ok("$m = {}\ntry\n  $m[\"a\"][\"b\"][\"c\"][2] = 1\ncatch $e\n  print(\"raised\")\nend\nprint($m)\n").await;
    assert_eq!(out.trim(), "raised\n{}");
}

async fn run_capped(source: &str, max_map: usize) -> Result<String, String> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().map_err(|e| e.to_string())?;
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program().map_err(|e| e.to_string())?;
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    eval.set_limits(EvalLimits {
        max_map_len: Some(max_map),
        recursion_limit: DEFAULT_RECURSION_LIMIT,
        ..Default::default()
    });
    match eval.execute(&stmts).await {
        Ok(_) => Ok(stdout.to_string_lossy()),
        Err(e) => Err(e.to_string()),
    }
}

#[tokio::test]
async fn a_cap_violation_does_not_grow_the_map_past_its_limit() {
    // The cap is preflighted, not checked after the insert. Before the
    // fix the insert ran first and the error left the map ALREADY over
    // the limit for anyone who caught it.
    let out = run_capped(
        "$m = { a: { x: 1 } }\ntry\n  $m[\"a\"][\"y\"] = 2\ncatch $e\n  print(\"raised\")\nend\nprint($m)\n",
        1,
    )
    .await
    .expect("the catch swallows the cap error");
    assert_eq!(out.trim(), "raised\n{a: {x: 1}}");
}

#[tokio::test]
async fn a_cap_that_forbids_the_first_key_vivifies_nothing() {
    // max_map = 0: even the single key a fresh map would receive is over
    // the cap. The validator must catch that BEFORE the mutator creates
    // the map — otherwise the caught error leaves `[{}]` behind.
    let out = run_capped(
        "$l = [nil]\ntry\n  $l[0].x = 1\ncatch $e\n  print(\"raised\")\nend\nprint($l)\n",
        0,
    )
    .await
    .expect("the catch swallows the cap error");
    assert_eq!(out.trim(), "raised\n[nil]");

    // Same, one level up: a missing MAP key that would be vivified.
    let out = run_capped(
        "$m = {}\ntry\n  $m[\"a\"][\"b\"] = 1\ncatch $e\n  print(\"raised\")\nend\nprint($m)\n",
        0,
    )
    .await
    .expect("the catch swallows the cap error");
    assert_eq!(out.trim(), "raised\n{}");
}

// ---------- the `push` self-assign fast path ----------

#[tokio::test]
async fn push_through_a_path_appends_in_place() {
    let out =
        ok("$m = { a: [1] }\n$m[\"a\"] = push($m[\"a\"], 2)\n$m.a = push($m.a, 3)\nprint($m)\n")
            .await;
    assert_eq!(out.trim(), "{a: [1, 2, 3]}");
}

#[tokio::test]
async fn push_fast_path_still_honours_copy_on_write() {
    // In place is only sound while nothing else can observe the list.
    let out = ok(
        "$m = { a: [1] }\n$alias = $m\n$m[\"a\"] = push($m[\"a\"], 2)\nprint($alias)\nprint($m)\n",
    )
    .await;
    assert_eq!(out.trim(), "{a: [1]}\n{a: [1, 2]}");
}

#[tokio::test]
async fn push_fast_path_declines_when_it_is_not_an_exact_self_assign() {
    // Different key on each side → must NOT be treated as self-assign.
    let out = ok("$m = { a: [1], b: [9] }\n$m[\"a\"] = push($m[\"b\"], 2)\nprint($m)\n").await;
    assert_eq!(out.trim(), "{a: [9, 2], b: [9]}");
    // `.k` and `["k"]` are different segment kinds, but both denote the
    // same slot, so the generic path must still produce the right answer.
    let out = ok("$m = { a: [1] }\n$m.a = push($m[\"a\"], 2)\nprint($m)\n").await;
    assert_eq!(out.trim(), "{a: [1, 2]}");
    // Target isn't a list → generic path (and its error).
    let e = err("$m = { a: 5 }\n$m[\"a\"] = push($m[\"a\"], 2)\n").await;
    assert!(!e.is_empty());
}

#[tokio::test]
async fn push_fast_path_respects_the_list_cap() {
    let mut lexer = Lexer::new("$m = { a: [1] }\n$m[\"a\"] = push($m[\"a\"], 2)\nprint($m)\n");
    let tokens = lexer.tokenize().expect("lex");
    let mut parser = Parser::new(tokens, "x");
    let stmts = parser.parse_program().expect("parse");
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    eval.set_limits(EvalLimits {
        max_list_len: Some(1),
        recursion_limit: DEFAULT_RECURSION_LIMIT,
        ..Default::default()
    });
    let err = eval.execute(&stmts).await.expect_err("cap must fire");
    assert!(err.to_string().contains("exceeds limit"), "got: {err}");
}

// ---------- the `"*"` wildcard is a READ fallback only ----------

#[tokio::test]
async fn wildcard_default_is_not_a_write_target() {
    // `$m["*"]` answers reads of missing keys. A nested write to a
    // missing key must create that key, NOT mutate the wildcard entry.
    let out = ok(
        "$m = { \"*\": \"default\" }\n$m.missing.k = 1\nprint($m[\"missing\"])\nprint($m[\"*\"])\n",
    )
    .await;
    assert_eq!(out.trim(), "{k: 1}\ndefault");
}

// ---------- copy-on-write ----------

#[tokio::test]
async fn nested_write_does_not_leak_through_an_alias() {
    let out =
        ok("$m = { a: { b: 1 } }\n$alias = $m\n$m[\"a\"][\"b\"] = 99\nprint($alias)\nprint($m)\n")
            .await;
    assert_eq!(out.trim(), "{a: {b: 1}}\n{a: {b: 99}}");
}

#[tokio::test]
async fn nested_list_write_does_not_leak_through_an_alias() {
    let out = ok("$l = [[1]]\n$copy = $l\n$l[0][0] = 42\nprint($copy)\nprint($l)\n").await;
    assert_eq!(out.trim(), "[[1]]\n[[42]]");
}

#[tokio::test]
async fn a_previously_extracted_inner_map_is_unaffected() {
    let out = ok("$m = { a: { b: 1 } }\n$inner = $m[\"a\"]\n$m.a.b = 7\nprint($inner)\n").await;
    assert_eq!(out.trim(), "{b: 1}");
}

// ---------- the single-accessor forms are untouched ----------

#[tokio::test]
async fn single_accessor_still_uses_the_old_nodes() {
    let out = ok("$m = {}\n$m[\"k\"] = 1\n$m.f = 2\n$cfg = {}\n$cfg.* = \"dflt\"\nprint($m)\nprint($cfg[\"anything\"])\n").await;
    assert_eq!(out.trim(), "{k: 1, f: 2}\ndflt");
}

#[tokio::test]
async fn field_assign_into_a_non_map_now_raises() {
    // Regression: before 0.33.0 this SILENTLY discarded the write, while
    // the index form correctly raised.
    let e = err("$x = 5\n$x.f = 1\n").await;
    assert!(e.contains("cannot index-assign into number"), "got: {e}");
}

// ---------- expressions that must NOT be parsed as lvalues ----------

#[tokio::test]
async fn method_calls_and_reads_still_parse() {
    let out = ok(concat!(
        "$m = { a: { b: \"x\" } }\n",
        "print($m.a.b)\n", // nested READ
        "print($m[\"a\"][\"b\"])\n",
        "$fns = { f: function($x) = $x * 2 }\n",
        "print($fns.f(21))\n", // map member holding a function
        "$l = [3, 1]\n",
        "print(len($l))\n",
    ))
    .await;
    assert_eq!(out.trim(), "x\nx\n42\n2");
}

#[tokio::test]
async fn statement_position_dispatch_is_unchanged_from_0_32() {
    // 0.33.0 must be dispatch-NEUTRAL. Pinning the pre-existing wart so a
    // future accessor-chain edit can't quietly move it: in STATEMENT
    // position the first `.name(` is a ValueCall on the field (member
    // semantics), not a UFCS method call — so `$l.push(4)` raises here,
    // exactly as it does in 0.32.3, even though `push($l, 4)` works and
    // UFCS works in expression position. (Fixing that inconsistency is a
    // dispatch decision, deliberately NOT bundled into this change.)
    let e = err("$l = [3, 1]\n$l.push(4)\n").await;
    assert!(e.contains("cannot access field 'push' on list"), "got: {e}");
}

#[tokio::test]
async fn statement_leading_member_call_still_calls_the_member() {
    // Regression (found in cold review): the accessor-chain collector
    // must NOT break on the FIRST `.name(`. If it does, `$m.f(1)` as a
    // STATEMENT stops being a ValueCall on the map member and becomes a
    // MethodCall, which dispatches name-first (UFCS) — silently calling a
    // global `f` instead of `$m.f`.
    let out = ok(concat!(
        "function f($x)\n",
        "  die \"GLOBAL f ran — dispatch regressed\"\n",
        "end\n",
        "$m = { f: function($x) = 1 }\n",
        "$m.f(1)\n",
        "print(\"MEMBER\")\n",
    ))
    .await;
    assert_eq!(out.trim(), "MEMBER");
}

#[tokio::test]
async fn mid_chain_method_call_still_dispatches_ufcs() {
    // The other half of the same rule: a method call that is NOT the
    // first accessor keeps its pre-0.33.0 UFCS dispatch.
    let out = ok("$m = { a: [3, 1] }\n$m.a.push(9)\nprint($m)\n").await;
    // push through a non-variable is a dead mutation (MIX-E1501) — the
    // point here is only that dispatch is unchanged, not that it works.
    assert_eq!(out.trim(), "{a: [3, 1]}");
}

#[tokio::test]
async fn nested_read_in_a_ternary_still_parses() {
    let out =
        ok("$m = { a: { b: \"x\" } }\n$r = $m.a.b == \"x\" ? \"yes\" : \"no\"\nprint($r)\n").await;
    assert_eq!(out.trim(), "yes");
}

// ---------- the point of the exercise ----------

#[tokio::test]
async fn grouping_a_map_of_maps_stays_linear() {
    // The workaround deep-copied the bucket on every write. This is the
    // shape that made it O(N²); here it must merely be correct — the
    // timing evidence lives in the journal, not in a flaky unit test.
    let out = ok(concat!(
        "$m = {}\n",
        "$i = 0\n",
        "while $i < 200 do\n",
        "  $m[\"g\" .. ($i % 4)][to_string($i)] = $i\n",
        "  $i = $i + 1\n",
        "end\n",
        "print(len($m))\n",
        "print(len($m[\"g0\"]))\n",
    ))
    .await;
    assert_eq!(out.trim(), "4\n50");
}

// ---------- the fast path must not be a hole in the sandbox ----------
//
// Every one of these was found in cold review: a fast path that skips a
// gate the generic path enforces is a security bug, not an optimisation.

/// Denies exactly one builtin by name; everything else is allowed.
struct DenyByName(&'static str);

impl cosmix_mix::evaluator::CapabilityPolicy for DenyByName {
    fn check_builtin(&self, name: &str) -> Result<(), String> {
        if name == self.0 {
            return Err(format!("{name} is denied"));
        }
        Ok(())
    }
    fn check_class(&self, _class: cosmix_mix::CapabilityClass) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
async fn push_fast_path_obeys_the_capability_policy() {
    // With push denied, the fast path must raise like any other push —
    // not quietly append behind the policy's back.
    let src = "$m = { a: [] }\n$m.a = push($m.a, 1)\nprint($m)\n";
    let mut lexer = Lexer::new(src);
    let tokens = lexer.tokenize().expect("lex");
    let mut parser = Parser::new(tokens, src);
    let stmts = parser.parse_program().expect("parse");
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    eval.set_capability_policy(std::rc::Rc::new(DenyByName("push")));
    let err = eval
        .execute(&stmts)
        .await
        .expect_err("a denied push must not run through the fast path");
    assert!(err.to_string().contains("denied"), "got: {err}");
}

#[tokio::test]
async fn push_fast_path_enforces_the_full_recursive_size_check() {
    // The generic path size-checks push's whole result recursively. The
    // fast path checked only the destination list's LENGTH, so an
    // oversized string sailed through a limit that would otherwise stop it.
    let src = "$m = { a: [] }\n$m.a = push($m.a, \"1234\")\nprint($m)\n";
    let mut lexer = Lexer::new(src);
    let tokens = lexer.tokenize().expect("lex");
    let mut parser = Parser::new(tokens, src);
    let stmts = parser.parse_program().expect("parse");
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    eval.set_limits(EvalLimits {
        max_string_len: Some(3),
        recursion_limit: DEFAULT_RECURSION_LIMIT,
        ..Default::default()
    });
    let err = eval
        .execute(&stmts)
        .await
        .expect_err("an oversized string must be rejected on either path");
    assert!(err.to_string().contains("exceeds limit"), "got: {err}");
}

#[tokio::test]
async fn interpolated_coalesce_default_falls_back_to_generic_semantics() {
    // `${x ?? expr}` parses and EVALUATES arbitrary Mix, so it can rebind
    // the very container the fast path is about to mutate. Classifying it
    // as sync/pure let the fast path append to the REBOUND list: this
    // produced {a: [99, 2]} where the generic semantics give {a: [1, 2]}.
    let out = ok(concat!(
        "$m = { a: [1] }\n",
        "function rebind()\n",
        "  $m.a = [99]\n",
        "  return 2\n",
        "end\n",
        "$m.a = push($m.a, \"${missing ?? rebind()}\")\n",
        "print($m)\n",
    ))
    .await;
    assert_eq!(out.trim(), "{a: [1, 2]}");
}
