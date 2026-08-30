//! Regression tests for two evaluator bugs surfaced by embedding Mix in
//! cosmix-webd (server-side `.mix` HTTP handlers run with injected
//! globals + `EvalLimits` inside a `block_on` worker). Both were latent
//! and only bit the embedded surface; both fixed in 0.15.5.
//!
//! 1. **Raw-slot UB in the For/ForEach fast paths.** A paired
//!    `global_slot_ptr` (loop var + accumulator/target) could rehash
//!    `frames[0]` on the second insert, invalidating the first raw
//!    pointer. Fixed via `Scope::global_slot_ptr_pair` (pre-create both,
//!    then non-inserting re-fetch). `slotbug_*` below.
//!
//! 2. **`return` inside a top-level control-flow block was swallowed.**
//!    Nested block bodies were run via the public `execute`, which
//!    unwraps `Return` at `function_depth == 0` — so a `return` inside a
//!    top-level `if`/loop produced `Ok(value)` instead of propagating,
//!    and execution wrongly continued (the webd handler
//!    `if $cond then return {..} end` fall-through). Fixed by running
//!    nested blocks via `execute_block` (propagates `Return`); only the
//!    invocation root unwraps. `noloop_*` below.
use cosmix_mix::evaluator::{Evaluator, SharedBuf};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;
use cosmix_mix::value::Value;
use cosmix_mix::{DEFAULT_RECURSION_LIMIT, EvalLimits};
use std::time::Duration;

async fn run(source: &str) -> Result<String, String> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().map_err(|e| e.to_string())?;
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program().map_err(|e| e.to_string())?;
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    // webd-like injected globals (frames[0] near a growth threshold)
    for k in ["METHOD", "PATH", "QUERY", "HOST", "BODY"] {
        eval.set_global(k, Value::String(k.to_string()));
    }
    eval.set_global("METHOD", Value::String("POST".into()));
    eval.set_global("HEADERS", Value::Map(Default::default()));
    eval.set_limits(EvalLimits {
        recursion_limit: DEFAULT_RECURSION_LIMIT,
        time_limit: Some(Duration::from_secs(5)),
        max_list_len: Some(100_000),
        max_map_len: Some(100_000),
        max_string_len: Some(8_000_000),
    });
    eval.execute(&stmts).await.map_err(|e| e.to_string())?;
    Ok(stdout.to_string_lossy())
}

const SRC_FOR: &str = r#"$total = 0
for $i = 0 to 30
  $total = $total + $i
end
if $METHOD == "POST" then
  print("POST total=" .. ("" .. $total))
else
  print("MISREAD-METHOD=[" .. $METHOD .. "]")
end
"#;

const SRC_FOREACH: &str = r#"$total = 0
for each $x in [1,2,3,4,5,6,7,8,9,10]
  $total = $total + $x
end
if $METHOD == "POST" then
  print("POST total=" .. ("" .. $total))
else
  print("MISREAD-METHOD=[" .. $METHOD .. "]")
end
"#;

#[tokio::test]
async fn slotbug_for_then_global_read() {
    let out = run(SRC_FOR).await.expect("runs");
    assert_eq!(out.trim(), "POST total=465", "got {out:?}");
}

#[tokio::test]
async fn slotbug_foreach_then_global_read() {
    let out = run(SRC_FOREACH).await.expect("runs");
    assert_eq!(out.trim(), "POST total=55", "got {out:?}");
}

// ── No-loop repro: `$METHOD == "POST"` mis-reads FALSE despite METHOD=="POST"
const SRC_NOLOOP: &str = r#"$out = "start"
if $METHOD == "POST" then
  $ok = true
  if $ok then
    $t = trim("  x  ")
    print("BRANCH-A")
  else
    print("BRANCH-B")
  end
else
  print("NO-POST method=[" .. $METHOD .. "]")
end
"#;

async fn run_noloop(configure: impl FnOnce(&mut Evaluator)) -> String {
    let mut lexer = Lexer::new(SRC_NOLOOP);
    let tokens = lexer.tokenize().unwrap();
    let mut parser = Parser::new(tokens, SRC_NOLOOP);
    let stmts = parser.parse_program().unwrap();
    let stdout = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(SharedBuf::new()));
    for k in ["PATH", "QUERY", "HOST", "BODY"] {
        eval.set_global(k, Value::String(k.to_string()));
    }
    eval.set_global("METHOD", Value::String("POST".into()));
    eval.set_global("HEADERS", Value::Map(Default::default()));
    eval.set_capability_policy(std::rc::Rc::new(cosmix_mix::CategoryAllowList::new(&[
        cosmix_mix::CapabilityClass::Db,
    ])));
    configure(&mut eval);
    eval.execute(&stmts).await.unwrap();
    stdout.to_string_lossy().trim().to_string()
}

#[tokio::test]
async fn noloop_method_read_under_limits() {
    let out = run_noloop(|e| {
        e.set_limits(EvalLimits {
            recursion_limit: DEFAULT_RECURSION_LIMIT,
            time_limit: Some(Duration::from_secs(5)),
            max_list_len: Some(100_000),
            max_map_len: Some(100_000),
            max_string_len: Some(8_000_000),
        });
    })
    .await;
    assert_eq!(out, "BRANCH-A", "METHOD misread under limits: {out:?}");
}

#[tokio::test]
async fn noloop_method_read_no_limits() {
    let out = run_noloop(|_| {}).await;
    assert_eq!(out, "BRANCH-A", "METHOD misread (no limits): {out:?}");
}

// ── No-loop repro v2: mirror webd exactly — map-literal returns in
// branches + execute() return value captured + run via a fresh
// current-thread runtime's block_on on a spawn_blocking-style thread.
const SRC_NOLOOP2: &str = r#"$out = "start"
if $METHOD == "POST" then
  $ok = true
  if $ok then
    $t = trim("  x  ")
    return { status: 200, body: "BRANCH-A" }
  end
  return { status: 200, body: "BRANCH-B" }
end
return { status: 200, body: "NO-POST" }
"#;

fn run_noloop2_blocking() -> String {
    // Mirror mix_handler.rs: build a fresh current-thread runtime and
    // block_on inside it (the spawn_blocking worker pattern).
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let mut lexer = Lexer::new(SRC_NOLOOP2);
        let tokens = lexer.tokenize().unwrap();
        let mut parser = Parser::new(tokens, SRC_NOLOOP2);
        let stmts = parser.parse_program().unwrap();
        let mut eval =
            Evaluator::with_output(Box::new(SharedBuf::new()), Box::new(SharedBuf::new()));
        for k in ["PATH", "QUERY", "HOST", "BODY"] {
            eval.set_global(k, Value::String(k.to_string()));
        }
        eval.set_global("METHOD", Value::String("POST".into()));
        eval.set_global("HEADERS", Value::Map(Default::default()));
        eval.set_capability_policy(std::rc::Rc::new(cosmix_mix::CategoryAllowList::new(&[
            cosmix_mix::CapabilityClass::Db,
        ])));
        eval.set_limits(EvalLimits {
            recursion_limit: DEFAULT_RECURSION_LIMIT,
            time_limit: Some(Duration::from_secs(5)),
            max_list_len: Some(100_000),
            max_map_len: Some(100_000),
            max_string_len: Some(8_000_000),
        });
        let v = eval.execute(&stmts).await.unwrap();
        match &v {
            Value::Map(m) => m
                .get("body")
                .map(|b| b.to_mix_string())
                .unwrap_or_else(|| "no-body".into()),
            other => format!("non-map: {}", other.to_mix_string()),
        }
    })
}

#[test]
fn noloop_map_return_blocking() {
    let out = run_noloop2_blocking();
    assert_eq!(
        out, "BRANCH-A",
        "METHOD misread (map-return/block_on): {out:?}"
    );
}
