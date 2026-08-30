//! `require()` module loader (0.27.0, Card 3 of the 4-feature rebuild).
//!
//! Covers the regenerated card3-require spec's test plan: the FIX 1
//! harvest (Scope::new(), never with_shared), caller protection, the
//! module_env sibling/private calling convention, per-call snapshot
//! semantics, the once-only cache + cycle detection, the explicit
//! `return`-only export override, prelude exclusion, the MethodCall
//! expression-position member call + pinned UFCS precedence, and the
//! bareword Function-variable dispatch fallback.

use cosmix_mix::CategoryAllowList;
use cosmix_mix::builtins::CapabilityClass;
use cosmix_mix::evaluator::{Evaluator, SharedBuf};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;
use std::path::{Path, PathBuf};

fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mix_require_test_{}", name));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Write `main.mix` into `dir`, run it with the file context set (so
/// `require` resolves module paths script-relative), and return
/// captured stdout. Errors return the error string.
async fn run_in_dir(dir: &Path, main_src: &str) -> Result<String, String> {
    run_in_dir_with(dir, main_src, false, |_| {}).await
}

async fn run_in_dir_with(
    dir: &Path,
    main_src: &str,
    with_prelude: bool,
    configure: impl FnOnce(&mut Evaluator),
) -> Result<String, String> {
    let main_path = dir.join("main.mix");
    std::fs::write(&main_path, main_src).unwrap();
    let mut lexer = Lexer::new(main_src);
    let tokens = lexer.tokenize().map_err(|e| e.to_string())?;
    let mut parser = Parser::new(tokens, main_src);
    let stmts = parser.parse_program().map_err(|e| e.to_string())?;
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    eval.set_file(main_path.to_string_lossy().to_string());
    configure(&mut eval);
    if with_prelude {
        eval.load_prelude().await;
    }
    eval.execute(&stmts).await.map_err(|e| e.to_string())?;
    Ok(stdout.to_string_lossy())
}

fn write_module(dir: &Path, name: &str, src: &str) {
    std::fs::write(dir.join(name), src).unwrap();
}

// T-fix1 — the flagship: module top-level $vars AND fns are visible
// through the exports map (proves the Scope::new() harvest; a
// with_shared harvest would return nil for $version).
#[tokio::test]
async fn exports_carry_top_level_vars_and_fns() {
    let dir = test_dir("flagship");
    write_module(
        &dir,
        "strutil.mix",
        "$version = \"1.0\"\n\
         function shout($s)\n  return upper($s) .. \"!\"\nend\n",
    );
    let out = run_in_dir(
        &dir,
        "$m = require(\"strutil.mix\")\n\
         print($m.version)\n\
         print($m.shout(\"hey\"))\n\
         print($m[\"shout\"](\"lo\"))\n\
         $f = $m.shout\n\
         print($f(\"x\"))\n",
    )
    .await
    .unwrap();
    assert_eq!(out, "1.0\nHEY!\nLO!\nX!\n");
}

// T1 — caller protection: the caller's same-named fn and $var survive,
// and the module never sees the caller's state.
#[tokio::test]
async fn caller_scope_and_registry_are_untouched() {
    let dir = test_dir("caller_protect");
    write_module(
        &dir,
        "m.mix",
        "$version = \"module\"\n\
         function ident()\n  return \"module-fn\"\nend\n\
         function free_name()\n  return type($caller_secret)\nend\n",
    );
    let out = run_in_dir(
        &dir,
        "$version = \"caller\"\n\
         $caller_secret = 42\n\
         function ident()\n  return \"caller-fn\"\nend\n\
         $m = require(\"m.mix\")\n\
         print($version)\n\
         print(ident())\n\
         print($m.version)\n\
         print($m.ident())\n\
         print($m[\"ident\"]())\n\
         print($m.free_name())\n",
    )
    .await
    .unwrap();
    // Pinned semantics:
    // - `$m.ident()` UFCS-dispatches to the CALLER's `ident` (global
    //   beats member — probe P3/P7); the index form forces the member.
    // - A module-fn free name that is NOT a module top-level resolves
    //   against the requiring program's globals at call time (named
    //   functions see globals live) — documented in the man page.
    assert_eq!(
        out,
        "caller\ncaller-fn\nmodule\ncaller-fn\nmodule-fn\nnumber\n"
    );
}

// T2 + A2 — `_`-privates are not exported but callable from exported
// fns, and module siblings win over the CALLER's same-named functions.
#[tokio::test]
async fn privates_hidden_but_callable_and_siblings_beat_caller_registry() {
    let dir = test_dir("privates");
    write_module(
        &dir,
        "m.mix",
        "$_sep = \"-\"\n\
         function _squash($s)\n  return replace($s, \"  \", \" \")\nend\n\
         function slugify($s)\n  return lower(replace(_squash($s), \" \", $_sep))\nend\n",
    );
    let out = run_in_dir(
        &dir,
        "function _squash($s)\n  return \"CALLER-CLOBBER\"\nend\n\
         $m = require(\"m.mix\")\n\
         print($m.slugify(\"Hello  World\"))\n\
         print(contains(keys($m), \"_squash\"))\n\
         print(contains(keys($m), \"_sep\"))\n",
    )
    .await
    .unwrap();
    assert_eq!(out, "hello-world\nfalse\nfalse\n");
}

// T3 — sibling calls at depth (a→b→c) + mutual recursion through the
// shared module_env.
#[tokio::test]
async fn sibling_calls_at_depth_and_mutual_recursion() {
    let dir = test_dir("depth");
    write_module(
        &dir,
        "m.mix",
        "function c()\n  return \"c\"\nend\n\
         function b()\n  return \"b\" .. c()\nend\n\
         function a()\n  return \"a\" .. b()\nend\n\
         function is_even($n)\n  if $n == 0 then\n    return true\n  end\n  return is_odd($n - 1)\nend\n\
         function is_odd($n)\n  if $n == 0 then\n    return false\n  end\n  return is_even($n - 1)\nend\n",
    );
    let out = run_in_dir(
        &dir,
        "$m = require(\"m.mix\")\nprint($m.a())\nprint($m.is_even(6))\n",
    )
    .await
    .unwrap();
    assert_eq!(out, "abc\ntrue\n");
}

// T4 — module-level vars are per-call value snapshots: writes inside a
// call do not persist across calls.
#[tokio::test]
async fn module_vars_are_per_call_snapshots() {
    let dir = test_dir("snapshot");
    write_module(
        &dir,
        "m.mix",
        "$counter = 1\n\
         function bump()\n  $counter = $counter + 1\n  return $counter\nend\n",
    );
    let out = run_in_dir(
        &dir,
        "$m = require(\"m.mix\")\nprint($m.bump())\nprint($m.bump())\nprint($m.counter)\n",
    )
    .await
    .unwrap();
    assert_eq!(out, "2\n2\n1\n");
}

// T5 — function-valued top-level vars are rewrapped with the env
// (callable, and they see module state), including via extract-then-call.
#[tokio::test]
async fn lambda_vars_carry_the_module_env() {
    let dir = test_dir("lambda_env");
    write_module(
        &dir,
        "m.mix",
        "$greeting = \"hi\"\n\
         $greet = function($who)\n  return $greeting .. \" \" .. $who\nend\n",
    );
    let out = run_in_dir(&dir, "$m = require(\"m.mix\")\nprint($m.greet(\"mark\"))\n")
        .await
        .unwrap();
    assert_eq!(out, "hi mark\n");
}

// T6 + A4 — ONLY an explicit top-level `return` overrides auto-export;
// an incidental non-nil last statement does not.
#[tokio::test]
async fn return_override_and_incidental_last_value() {
    let dir = test_dir("ret_override");
    write_module(&dir, "answer.mix", "return 42\n");
    write_module(
        &dir,
        "picked.mix",
        "function hidden()\n  return \"h\"\nend\n\
         return { only: function() return \"picked\" end }\n",
    );
    // Last statement is a non-nil expression — auto-export must still win.
    write_module(
        &dir,
        "incidental.mix",
        "function real()\n  return \"real\"\nend\n\
         len(\"xyz\")\n",
    );
    let out = run_in_dir(
        &dir,
        "print(require(\"answer.mix\"))\n\
         $p = require(\"picked.mix\")\n\
         print($p.only())\n\
         print(contains(keys($p), \"hidden\"))\n\
         $i = require(\"incidental.mix\")\n\
         print($i.real())\n",
    )
    .await
    .unwrap();
    assert_eq!(out, "42\npicked\nfalse\nreal\n");
}

// T7 + A3 — frame-noise names and (un-redefined) prelude functions are
// not exported; a module redefining a prelude name exports its own.
#[tokio::test]
async fn noise_and_prelude_names_excluded_from_exports() {
    let dir = test_dir("prelude_excl");
    write_module(
        &dir,
        "m.mix",
        "function mine()\n  return sum([1, 2, 3])\nend\n\
         function sum($xs)\n  return 99\nend\n",
    );
    let out = run_in_dir_with(
        &dir,
        "$m = require(\"m.mix\")\n\
         print(contains(keys($m), \"lines\"))\n\
         print(contains(keys($m), \"avg\"))\n\
         print(contains(keys($m), \"rc\"))\n\
         print($m[\"sum\"]([1, 2, 3]))\n\
         print($m.mine())\n",
        true, // program loads the prelude → modules see it replayed
        |_| {},
    )
    .await
    .unwrap();
    // The module's OWN `sum` (fresh Rc ≠ the prelude's) IS exported —
    // called via the index form ($m.sum(...) would UFCS-dispatch to the
    // caller's prelude `sum`, the pinned precedence) — and `mine`'s
    // bareword `sum(...)` resolves to the module's via the env.
    assert_eq!(out, "false\nfalse\nfalse\n99\n99\n");
}

// T8 — once-only cache across two relative spellings of one canonical
// path (module top-level side effect runs exactly once).
#[tokio::test]
async fn cached_once_per_canonical_path() {
    let dir = test_dir("cache_once");
    write_module(&dir, "m.mix", "print(\"LOADED\")\n$v = 7\n");
    let out = run_in_dir(
        &dir,
        "$a = require(\"m.mix\")\n$b = require(\"./m.mix\")\nprint($a.v + $b.v)\n",
    )
    .await
    .unwrap();
    assert_eq!(
        out.matches("LOADED").count(),
        1,
        "module body must run once: {out:?}"
    );
    assert!(out.contains("14"));
}

// T9/T9b — cycles (mutual and self) are hard errors naming the chain.
#[tokio::test]
async fn cycles_are_hard_errors() {
    let dir = test_dir("cycles");
    write_module(&dir, "a.mix", "$b = require(\"b.mix\")\n");
    write_module(&dir, "b.mix", "$a = require(\"a.mix\")\n");
    let err = run_in_dir(&dir, "require(\"a.mix\")\n").await.unwrap_err();
    assert!(err.contains("circular module dependency"), "{err}");
    write_module(&dir, "selfy.mix", "$s = require(\"selfy.mix\")\n");
    let err = run_in_dir(&dir, "require(\"selfy.mix\")\n")
        .await
        .unwrap_err();
    assert!(err.contains("circular module dependency"), "{err}");
}

// T10 — a failed require caches nothing and is retryable.
#[tokio::test]
async fn failed_require_is_retryable() {
    let dir = test_dir("retryable");
    let late = dir.join("late.mix");
    let late_s = late.to_string_lossy().to_string();
    let src = format!(
        "try\n  require(\"{late}\")\ncatch $e\n  print(\"caught\")\nend\n\
         write_file(\"{late}\", \"$ok = true\\n\")\n\
         $m = require(\"{late}\")\nprint($m.ok)\n",
        late = late_s
    );
    let out = run_in_dir(&dir, &src).await.unwrap();
    assert_eq!(out, "caught\ntrue\n");
}

// T11 — mutating the returned map does not poison the cache.
#[tokio::test]
async fn cache_is_isolated_from_caller_mutation() {
    let dir = test_dir("cache_isolated");
    write_module(&dir, "m.mix", "$v = 1\n");
    let out = run_in_dir(
        &dir,
        "$a = require(\"m.mix\")\n$a.v = 999\n$b = require(\"m.mix\")\nprint($b.v)\n",
    )
    .await
    .unwrap();
    assert_eq!(out, "1\n");
}

// T12 — require inside a function: the module's top-level `return`
// must not escape as the caller function's return.
#[tokio::test]
async fn module_return_does_not_escape_enclosing_function() {
    let dir = test_dir("fn_scoped");
    write_module(&dir, "answer.mix", "return 42\n");
    let out = run_in_dir(
        &dir,
        "function load_it()\n  $v = require(\"answer.mix\")\n  return \"got:\" .. $v\nend\n\
         print(load_it())\n",
    )
    .await
    .unwrap();
    assert_eq!(out, "got:42\n");
}

// T14 — nested require resolves relative to the REQUIRING module.
#[tokio::test]
async fn nested_require_is_module_relative() {
    let dir = test_dir("nested");
    let sub = dir.join("lib");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("inner.mix"), "$leaf = \"deep\"\n").unwrap();
    std::fs::write(
        sub.join("outer.mix"),
        "$inner = require(\"inner.mix\")\nfunction leaf()\n  return $inner.leaf\nend\n",
    )
    .unwrap();
    let out = run_in_dir(
        &dir,
        "$m = require(\"lib/outer.mix\")\nprint($m.leaf())\nprint($m.inner.leaf)\n",
    )
    .await
    .unwrap();
    assert_eq!(out, "deep\ndeep\n");
}

// T15 — an FsRead-denying policy refuses require before any fs access.
#[tokio::test]
async fn capability_gate_refuses_require() {
    let dir = test_dir("cap_gate");
    write_module(&dir, "m.mix", "$v = 1\n");
    let err = run_in_dir_with(&dir, "require(\"m.mix\")\n", false, |e| {
        e.set_capability_policy(std::rc::Rc::new(CategoryAllowList::new(&[
            CapabilityClass::Pure,
        ])));
    })
    .await
    .unwrap_err();
    assert!(err.contains("capability"), "{err}");
}

// T16 — module parse and runtime errors are attributed to the module
// file, nothing is cached, and the caller's file context is restored.
#[tokio::test]
async fn module_errors_attribute_and_stay_retryable() {
    let dir = test_dir("attribution");
    write_module(&dir, "bad_parse.mix", "function oops(\n");
    write_module(&dir, "bad_run.mix", "$x = 1 / 0\n");
    let err = run_in_dir(&dir, "require(\"bad_parse.mix\")\n")
        .await
        .unwrap_err();
    assert!(
        !err.contains("main.mix"),
        "parse error blamed the caller: {err}"
    );
    let err = run_in_dir(&dir, "require(\"bad_run.mix\")\n")
        .await
        .unwrap_err();
    assert!(
        err.contains("bad_run.mix"),
        "runtime error must name the module: {err}"
    );
    // Caller context restored: a follow-up caller error names main.mix.
    let err = run_in_dir(
        &dir,
        "try\n  require(\"bad_run.mix\")\ncatch $e\nend\n$y = 1 / 0\n",
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("main.mix"),
        "caller file context not restored: {err}"
    );
}

// A9 — `on` handlers cannot be registered from a module.
#[tokio::test]
async fn on_handlers_refused_in_modules() {
    let dir = test_dir("on_refused");
    write_module(&dir, "m.mix", "on ping\n  reply(\"pong\")\nend\n");
    let err = run_in_dir(&dir, "require(\"m.mix\")\n").await.unwrap_err();
    assert!(err.contains("'on' handlers cannot be registered"), "{err}");
}

// T13 — module top-level unknown names inside a caller `address` block
// error (the address stack is taken for the module's duration).
#[tokio::test]
async fn address_block_does_not_leak_into_module() {
    let dir = test_dir("address_leak");
    write_module(&dir, "m.mix", "totally_unknown_fn()\n");
    // Address-block BODY lines are implicit sends by construction, so
    // the reachable leak vector is a send ARG expression evaluated
    // while the address stack is live. If the caller's address stack
    // leaked into the module, the module's unknown name would become a
    // non-fatal SEND (rc=-3, script succeeds); the taken stack makes it
    // a hard undefined-function error instead.
    let err = run_in_dir(
        &dir,
        "address \"svc\"\n  someverb key=require(\"m.mix\")\nend\n",
    )
    .await
    .unwrap_err();
    assert!(err.contains("totally_unknown_fn"), "{err}");
}

// T17 — MethodCall parity and pinned precedence.
#[tokio::test]
async fn method_call_precedence_pinned() {
    let dir = test_dir("precedence");
    // (a) UFCS still wins over a member of the same name (probe P3/P7).
    let out = run_in_dir(
        &dir,
        "function f($self, $x)\n  return \"global\" .. $x\nend\n\
         $m = { f: function($x) return \"member\" .. $x end }\n\
         print($m.f(1))\n\
         print($m[\"f\"](1))\n",
    )
    .await
    .unwrap();
    assert_eq!(out, "global1\nmember1\n");
    // (b) builtins keep the UFCS desugar even against a same-named member.
    let out = run_in_dir(
        &dir,
        "$m = { keys: function() return \"member-keys\" end }\n\
         print(len($m.keys()))\n",
    )
    .await
    .unwrap();
    // builtin keys($m) → list of the map's keys → len 1.
    assert_eq!(out, "1\n");
    // (c) expression-position member call on a plain map (the former
    // error path) and the enriched error when the member is absent.
    let out = run_in_dir(
        &dir,
        "$m = { hi: function() return \"yo\" end }\nprint($m.hi())\n",
    )
    .await
    .unwrap();
    assert_eq!(out, "yo\n");
    let err = run_in_dir(&dir, "$m = { v: 1 }\nprint($m.nope())\n")
        .await
        .unwrap_err();
    assert!(err.contains("undefined function 'nope'"), "{err}");
}

// A1 — the in-place mutating trio keeps working through method syntax.
#[tokio::test]
async fn method_syntax_push_still_mutates_in_place() {
    let dir = test_dir("push_parity");
    let out = run_in_dir(&dir, "$l = [1]\nprint($l.push(2))\nprint($l)\n")
        .await
        .unwrap();
    assert_eq!(out, "nil\n[1, 2]\n");
}

// T18 — bareword Function-variable dispatch: fires only where dispatch
// previously errored; named functions still win.
#[tokio::test]
async fn bareword_function_variable_dispatch() {
    let dir = test_dir("bareword");
    let out = run_in_dir(
        &dir,
        "$b = function()\n  return 7\nend\nprint(b())\n\
         function c()\n  return \"named\"\nend\n\
         $c = function()\n  return \"var\"\nend\n\
         print(c())\n",
    )
    .await
    .unwrap();
    assert_eq!(out, "7\nnamed\n");
}

// T19 — sync-gate sweep: a self-recursive numeric module function
// reading a module var computes correctly (would nil-read if any sync
// fast path skipped the env injection).
#[tokio::test]
async fn sync_fast_paths_gated_for_module_functions() {
    let dir = test_dir("sync_gate");
    write_module(
        &dir,
        "m.mix",
        "$base = 1\n\
         function fib($n)\n  if $n < 2 then\n    return $n * $base\n  end\n  return fib($n - 1) + fib($n - 2)\nend\n",
    );
    let out = run_in_dir(&dir, "$m = require(\"m.mix\")\nprint($m.fib(10))\n")
        .await
        .unwrap();
    assert_eq!(out, "55\n");
}

// FIX 1 negative half — the with_shared trap is real: the same module
// statements evaluated against a shared-globals scope leave the shared
// handle EMPTY (vars land in the local root), which is exactly why
// exec_require must harvest a Scope::new(). Guarded via the public
// surface: a Class C handler's `require` still exports vars (the
// invocation evaluator swaps in a fresh standalone scope for the
// module), which fails if the harvest ever routes through the shared
// path. Covered indirectly by exports_carry_top_level_vars_and_fns +
// the serve-mode smoke in the deploy gate; the direct scope-level
// assertion lives in scope.rs's gamma tests
// (update_or_set_walks_shared_globals_when_present).
// T20 — serve/γ: require inside an `on` handler (the per-invocation
// evaluator runs on a with_shared scope — exactly where a wrong
// harvest would return nil for module vars). Second dispatch is a
// cache hit (module body runs once); the shared globals survive.
#[tokio::test]
async fn require_inside_handler_and_cache_across_invocations() {
    use cosmix_mix::evaluator::IncomingEvent;
    let dir = test_dir("handler");
    write_module(
        &dir,
        "m.mix",
        "print(\"LOADED\")\n$v = 5\nfunction dbl($x)\n  return $x * 2\nend\n",
    );
    let module_path = dir.join("m.mix").to_string_lossy().to_string();
    let source = format!(
        "$total = 0\n\
         on bump\n\
           $m = require(\"{module_path}\")\n\
           $total = $total + $m.dbl($m.v)\n\
         end\n"
    );
    let mut lexer = Lexer::new(&source);
    let stmts = Parser::new(lexer.tokenize().unwrap(), &source)
        .parse_program()
        .unwrap();
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    eval.execute(&stmts).await.unwrap();
    let mk = || IncomingEvent {
        command: "bump".to_string(),
        headers: std::collections::BTreeMap::new(),
        body: String::new(),
    };
    eval.dispatch_event(mk()).await.unwrap();
    eval.dispatch_event(mk()).await.unwrap();
    let out = stdout.to_string_lossy();
    assert_eq!(
        out.matches("LOADED").count(),
        1,
        "module body must run once across handler invocations: {out:?}"
    );
    assert_eq!(
        eval.get_global("total").unwrap().to_mix_string(),
        "20",
        "module exports must resolve inside handlers (vars AND fns)"
    );
}

// Codex round-1 MAJOR — a module's top level must NOT see the
// enclosing handler's reply handle: module bodies run once per path
// (cache), so a top-level reply() would consume the handle on first
// require and silently no-op on every later cache hit. The handle is
// taken for the module's duration → the standard "only from within an
// `on` handler" error, catchable in the handler.
#[tokio::test]
async fn module_top_level_reply_is_refused_inside_handlers() {
    use cosmix_mix::evaluator::IncomingEvent;
    let dir = test_dir("reply_isolated");
    write_module(&dir, "replier.mix", "reply(\"hijacked\")\n$v = 1\n");
    let module_path = dir.join("replier.mix").to_string_lossy().to_string();
    let source = format!(
        "$caught = \"no\"\n\
         on poke\n\
           try\n\
             require(\"{module_path}\")\n\
           catch $e\n\
             $caught = $e\n\
           end\n\
         end\n"
    );
    let mut lexer = Lexer::new(&source);
    let stmts = Parser::new(lexer.tokenize().unwrap(), &source)
        .parse_program()
        .unwrap();
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    eval.execute(&stmts).await.unwrap();
    eval.dispatch_event(IncomingEvent {
        command: "poke".to_string(),
        headers: std::collections::BTreeMap::new(),
        body: String::new(),
    })
    .await
    .unwrap();
    let caught = eval.get_global("caught").unwrap().to_mix_string();
    assert!(
        caught.contains("on` handler") || caught.contains("on handler"),
        "module top-level reply() must be refused, got: {caught}"
    );
}

#[tokio::test]
async fn require_error_message_for_missing_file() {
    let dir = test_dir("missing");
    let err = run_in_dir(&dir, "require(\"absent.mix\")\n")
        .await
        .unwrap_err();
    assert!(err.contains("require: "), "{err}");
    assert!(err.contains("absent.mix"), "{err}");
}
