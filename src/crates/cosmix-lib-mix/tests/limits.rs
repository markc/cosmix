//! Native evaluator self-protection limits (the depth / capability /
//! time / collection knobs added for in-daemon embedding). Each test
//! drives a Mix program that would otherwise crash or exhaust the host
//! and asserts it returns a clean, catchable Mix error instead.

use cosmix_mix::evaluator::{Evaluator, SharedBuf};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;
use cosmix_mix::{CapabilityClass, CategoryAllowList, DEFAULT_RECURSION_LIMIT, EvalLimits};
use std::rc::Rc;
use std::time::Duration;

/// Parse + run `source`, applying `configure` to the evaluator first.
/// Returns Ok(stdout) or Err(error message).
async fn run(source: &str, configure: impl FnOnce(&mut Evaluator)) -> Result<String, String> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().map_err(|e| e.to_string())?;
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program().map_err(|e| e.to_string())?;
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    configure(&mut eval);
    eval.execute(&stmts).await.map_err(|e| e.to_string())?;
    Ok(stdout.to_string_lossy())
}

fn limits(recursion_limit: usize) -> EvalLimits {
    EvalLimits {
        recursion_limit,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Knob B — recursion-depth cap (every native path)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn recursion_cap_generic_path() {
    let src = "function f($n)\n  return f($n + 1)\nend\nprint(f(0))\n";
    // A low cap so it fires well before even a small (libtest ~1.4 MB)
    // thread's native stack overflows — this tests the mechanism, not a
    // specific safe depth (that is `default_limit_is_safe_on_2mb_stack`).
    let err = run(src, |e| e.set_limits(limits(16)))
        .await
        .expect_err("infinite recursion must error, not overflow");
    assert!(err.contains("recursion depth exceeded"), "got: {err}");
}

#[tokio::test]
async fn recursion_cap_numeric_fib_path() {
    // fib drives the numeric / fib-bytecode fast path (eval_fib_program /
    // try_call_function_sync_block_f64) — the path that bypasses the
    // function_depth increments on the AST path. A deep fib must still
    // hit the cap cleanly.
    let src = "function fib($n)\n  if $n < 2 then\n    return $n\n  end\n  return fib($n - 1) + fib($n - 2)\nend\nprint(fib(500))\n";
    let err = run(src, |e| e.set_limits(limits(16)))
        .await
        .expect_err("deep fib must error, not overflow");
    assert!(err.contains("recursion depth exceeded"), "got: {err}");
}

#[tokio::test]
async fn recursion_cap_default_param_path() {
    // A recursive DEFAULT parameter recurses via `eval_expr(default)`
    // *before* the body runs; the depth must already be incremented for
    // this frame or that path bypasses the cap and overflows the stack.
    let src = "function f($x = f())\n  return $x\nend\nprint(f())\n";
    let err = run(src, |e| e.set_limits(limits(16)))
        .await
        .expect_err("recursive default param must error, not overflow");
    assert!(err.contains("recursion depth exceeded"), "got: {err}");
}

#[tokio::test]
async fn recursion_within_limit_still_computes() {
    let src = "function fib($n)\n  if $n < 2 then\n    return $n\n  end\n  return fib($n - 1) + fib($n - 2)\nend\nprint(fib(10))\n";
    let out = run(src, |e| e.set_limits(limits(64)))
        .await
        .expect("shallow fib computes");
    assert_eq!(out.trim(), "55");
}

// ---------------------------------------------------------------------------
// Knob A — capability gate over the builtin table
// ---------------------------------------------------------------------------

#[tokio::test]
async fn capability_denies_unallowed_class() {
    // exists() is classified FsRead; an empty allow-list denies it.
    let err = run("print(exists(\"/\"))\n", |e| {
        e.set_capability_policy(Rc::new(CategoryAllowList::new(&[])))
    })
    .await
    .expect_err("FsRead must be denied");
    assert!(err.contains("capability denied"), "got: {err}");
}

#[tokio::test]
async fn capability_allows_listed_class() {
    let out = run("print(exists(\"/\"))\n", |e| {
        e.set_capability_policy(Rc::new(CategoryAllowList::new(&[CapabilityClass::FsRead])))
    })
    .await
    .expect("FsRead allowed by policy");
    assert_eq!(out.trim(), "true");
}

#[tokio::test]
async fn capability_pure_builtins_always_allowed() {
    let out = run("print(upper(\"hi\"))\n", |e| {
        e.set_capability_policy(Rc::new(CategoryAllowList::new(&[])))
    })
    .await
    .expect("pure builtins are never gated");
    assert_eq!(out.trim(), "HI");
}

// Shell-syntax execution (`sh "…"`, `$(…)`, `… | cmd`) spawns /bin/sh
// WITHOUT resolving to a named builtin, so the builtin gate alone would
// let it escape (the slice #3 webd-embed escape). The Process class must
// gate these paths too.

#[tokio::test]
async fn capability_denies_sh_keyword() {
    let err = run("sh \"true\"\n", |e| {
        e.set_capability_policy(Rc::new(CategoryAllowList::new(&[CapabilityClass::FsRead])))
    })
    .await
    .expect_err("`sh` must be denied without the Process capability");
    assert!(err.contains("capability denied"), "got: {err}");
}

#[tokio::test]
async fn capability_denies_command_substitution() {
    let err = run("$x = $(true)\n", |e| {
        e.set_capability_policy(Rc::new(CategoryAllowList::new(&[CapabilityClass::FsRead])))
    })
    .await
    .expect_err("`$()` must be denied without the Process capability");
    assert!(err.contains("capability denied"), "got: {err}");
}

#[tokio::test]
async fn capability_denies_pipe_to_external() {
    let err = run("\"x\" | cat\n", |e| {
        e.set_capability_policy(Rc::new(CategoryAllowList::new(&[CapabilityClass::FsRead])))
    })
    .await
    .expect_err("pipe-to-external must be denied without the Process capability");
    assert!(err.contains("capability denied"), "got: {err}");
}

#[tokio::test]
async fn capability_allows_sh_when_process_listed() {
    // Process in the allow-set → `sh` runs (exit 0, no Mix error).
    run("sh \"true\"\n", |e| {
        e.set_capability_policy(Rc::new(CategoryAllowList::new(&[CapabilityClass::Process])))
    })
    .await
    .expect("Process allowed → sh runs");
}

#[tokio::test]
async fn default_check_class_gates_shell_for_builtin_only_policy() {
    // `DenyByName` implements only `check_builtin`; the trait's default
    // `check_class` delegates Process -> check_builtin("run"), so even a
    // builtin-only policy gates shell syntax.
    let err = run("sh \"true\"\n", |e| {
        e.set_capability_policy(Rc::new(DenyByName("run")))
    })
    .await
    .expect_err("default check_class must gate `sh` via the run representative");
    assert!(err.contains("capability denied"), "got: {err}");
}

// `include`/`source` read a file from disk — FsRead-gated so a Pure-only
// sandbox can't load + run arbitrary on-disk code.

#[tokio::test]
async fn include_denied_without_fsread() {
    let err = run("include \"/no/such/file.mix\"\n", |e| {
        e.set_capability_policy(Rc::new(CategoryAllowList::new(&[])))
    })
    .await
    .expect_err("include must be FsRead-gated");
    assert!(err.contains("capability denied"), "got: {err}");
}

#[tokio::test]
async fn include_allowed_with_fsread_passes_gate() {
    // FsRead granted → the gate passes; the only error is the missing
    // file, NOT a capability denial (proves the gate isn't over-blocking).
    let err = run("include \"/no/such/file.mix\"\n", |e| {
        e.set_capability_policy(Rc::new(CategoryAllowList::new(&[CapabilityClass::FsRead])))
    })
    .await
    .expect_err("missing include file errors");
    assert!(
        !err.contains("capability denied"),
        "FsRead allows include; got: {err}"
    );
    assert!(err.contains("include"), "got: {err}");
}

#[test]
fn sensitive_builtins_stay_categorized() {
    // The capability class now lives in the builtin_table! entry (so a new
    // builtin can't be unclassified). This pins the SPECIFIC class of every
    // sensitive builtin so a table edit that flips one (e.g. http_get from
    // Network to Pure) is caught — the full set, not a sample.
    use CapabilityClass::*;
    use cosmix_mix::capability_category;
    let cases: &[(&str, CapabilityClass)] = &[
        // FsRead
        ("read_file", FsRead),
        ("read_file_bytes", FsRead),
        ("read_lines", FsRead),
        ("load_data", FsRead),
        ("read_json", FsRead),
        ("read_jsonl", FsRead),
        ("exists", FsRead),
        ("is_dir", FsRead),
        ("is_file", FsRead),
        ("glob", FsRead),
        // grep is deliberately absent: it filters in-memory text (Pure
        // since 0.29.0 — the old FsRead classification was a
        // misclassification the codex C1 review caught).
        ("ls", FsRead),
        ("walk", FsRead),
        ("line_count", FsRead),
        ("head", FsRead),
        ("tail", FsRead),
        ("stat", FsRead),
        ("hash_file", FsRead),
        // FsWrite
        ("write_file", FsWrite),
        ("fcntl_lock", FsWrite),
        ("fcntl_unlock", FsWrite),
        ("write_new", FsWrite),
        ("append_file", FsWrite),
        ("mkdir", FsWrite),
        ("chmod", FsWrite),
        ("chown", FsWrite),
        ("sqlopen", FsWrite),
        ("sqlexec", FsWrite),
        ("sqlclose", FsWrite),
        // Network
        ("http_get", Network),
        ("http_post", Network),
        ("http_request", Network),
        ("ssh_run", Network),
        ("ssh_must", Network),
        ("ssh_mix", Network),
        ("ssh_exec", Network),
        ("dns_lookup", Network),
        ("udp_send", Network),
        ("udp_recv", Network),
        // Process
        ("run", Process),
        ("run_rc", Process),
        ("run_stream", Process),
        ("run_argv", Process),
        ("run_argv_must", Process),
        ("run_pipeline", Process),
        ("run_pipeline_must", Process),
        ("spawn", Process),
        ("kill", Process),
        ("process_alive", Process),
        ("exit", Process),
        ("chdir", Process),
        ("panic", Process),
        // Env
        ("env", Env),
        ("pid", Env),
        ("hostname", Env),
        ("cwd", Env),
        ("platform", Env),
        ("which", Env),
        ("readline", Env),
        ("read_stdin", Env),
        // Db / Jmap (mediated seams)
        ("db_query", Db),
        ("db_exec", Db),
        ("jmap", Jmap),
        // A few Pure anchors.
        ("upper", Pure),
        ("length", Pure),
        ("json_parse", Pure),
        ("map", Pure),
    ];
    for (name, expect) in cases {
        assert_eq!(
            capability_category(name),
            *expect,
            "builtin `{name}` is miscategorised"
        );
    }
}

struct DenyByName(&'static str);
impl cosmix_mix::CapabilityPolicy for DenyByName {
    fn check_builtin(&self, name: &str) -> Result<(), String> {
        if name == self.0 {
            Err(format!("{name} denied"))
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
async fn capability_gates_mutating_builtin() {
    // push/pop/shift route through call_mutating_builtin (not the normal
    // builtin dispatch); an exact-name policy must still be able to deny
    // them.
    let src = "$l = [1, 2]\npush($l, 3)\nprint(length($l))\n";
    let err = run(src, |e| {
        e.set_capability_policy(Rc::new(DenyByName("push")))
    })
    .await
    .expect_err("push must be deniable by exact name");
    assert!(err.contains("capability denied"), "got: {err}");
}

#[tokio::test]
async fn no_policy_is_permissive() {
    let out = run("print(exists(\"/\"))\n", |_| {})
        .await
        .expect("no policy installed -> fully permissive");
    assert_eq!(out.trim(), "true");
}

// ---------------------------------------------------------------------------
// Knob C — wall-clock deadline
// ---------------------------------------------------------------------------

#[tokio::test]
async fn deadline_bounds_infinite_loop() {
    // A loop re-enters execute() each iteration -> caught at the poll.
    let src = "$keep = 1\n$i = 0\nwhile $keep == 1\n  $i = $i + 1\nend\n";
    let err = run(src, |e| {
        e.set_limits(EvalLimits {
            time_limit: Some(Duration::from_millis(50)),
            recursion_limit: 1000,
            ..Default::default()
        })
    })
    .await
    .expect_err("infinite loop must hit the deadline");
    assert!(err.contains("deadline exceeded"), "got: {err}");
}

#[tokio::test]
async fn deadline_bounds_wide_numeric_recursion() {
    // fib(40) is shallow (depth 40) but ~1e9 calls; it never re-enters the
    // statement poll, so the throttled in-fast-path deadline check must
    // catch it. recursion_limit is set high so the depth cap can't fire.
    let src = "function fib($n)\n  if $n < 2 then\n    return $n\n  end\n  return fib($n - 1) + fib($n - 2)\nend\nprint(fib(40))\n";
    let err = run(src, |e| {
        e.set_limits(EvalLimits {
            time_limit: Some(Duration::from_millis(50)),
            recursion_limit: 100_000,
            ..Default::default()
        })
    })
    .await
    .expect_err("wide numeric recursion must hit the deadline");
    assert!(err.contains("deadline exceeded"), "got: {err}");
}

// ---------------------------------------------------------------------------
// Knob D — collection-size caps
// ---------------------------------------------------------------------------

#[tokio::test]
async fn collection_cap_string_concat() {
    let src = "$s = \"\"\n$i = 0\nwhile $i < 1000\n  $s = $s .. \"abcdefghij\"\n  $i = $i + 1\nend\nprint(length($s))\n";
    let err = run(src, |e| {
        e.set_limits(EvalLimits {
            max_string_len: Some(500),
            recursion_limit: 1000,
            ..Default::default()
        })
    })
    .await
    .expect_err("string growth must hit the cap");
    assert!(
        err.contains("string length") && err.contains("exceeds limit"),
        "got: {err}"
    );
}

#[tokio::test]
async fn collection_cap_list_push() {
    let src =
        "$l = []\n$i = 0\nwhile $i < 1000\n  push($l, $i)\n  $i = $i + 1\nend\nprint(length($l))\n";
    let err = run(src, |e| {
        e.set_limits(EvalLimits {
            max_list_len: Some(100),
            recursion_limit: 1000,
            ..Default::default()
        })
    })
    .await
    .expect_err("list growth must hit the cap");
    assert!(
        err.contains("list length") && err.contains("exceeds limit"),
        "got: {err}"
    );
}

#[tokio::test]
async fn collection_cap_map_insert() {
    let src =
        "$m = {}\n$i = 0\nwhile $i < 1000\n  $m[$i] = $i\n  $i = $i + 1\nend\nprint(length($m))\n";
    let err = run(src, |e| {
        e.set_limits(EvalLimits {
            max_map_len: Some(100),
            recursion_limit: 1000,
            ..Default::default()
        })
    })
    .await
    .expect_err("map growth must hit the cap");
    assert!(
        err.contains("map size") && err.contains("exceeds limit"),
        "got: {err}"
    );
}

#[tokio::test]
async fn collection_cap_builtin_result() {
    // range() produces a whole list in one builtin call -> caught at the
    // dispatch-site result check.
    let err = run("print(length(range(0, 100000)))\n", |e| {
        e.set_limits(EvalLimits {
            max_list_len: Some(1000),
            ..Default::default()
        })
    })
    .await
    .expect_err("oversized builtin result must hit the cap");
    assert!(
        err.contains("list length") && err.contains("exceeds limit"),
        "got: {err}"
    );
}

#[tokio::test]
async fn collection_cap_string_interpolation() {
    // Interpolation (`"${s}..."`) grows a string without going through
    // the `..` operator — its own result check must catch it.
    let src = "$s = \"x\"\n$i = 0\nwhile $i < 1000\n  $s = \"${s}abcdefghij\"\n  $i = $i + 1\nend\nprint(length($s))\n";
    let err = run(src, |e| {
        e.set_limits(EvalLimits {
            max_string_len: Some(500),
            recursion_limit: 1000,
            ..Default::default()
        })
    })
    .await
    .expect_err("interpolation growth must hit the cap");
    assert!(
        err.contains("string length") && err.contains("exceeds limit"),
        "got: {err}"
    );
}

#[tokio::test]
async fn collection_cap_nested_builtin_result() {
    // A builtin can return a nested structure whose top level is small
    // but an inner collection is oversized; the recursive result check
    // must catch it. json_parse builds the whole tree in one call, so no
    // intermediate construction check fires first.
    let inner = "0,".repeat(199) + "0"; // 200-element array
    let src = format!("$j = json_parse('{{\"a\": [{inner}]}}')\nprint(\"ok\")\n");
    let err = run(&src, |e| {
        e.set_limits(EvalLimits {
            max_list_len: Some(100),
            ..Default::default()
        })
    })
    .await
    .expect_err("oversized nested array must hit the cap");
    assert!(
        err.contains("list length") && err.contains("exceeds limit"),
        "got: {err}"
    );
}

#[tokio::test]
async fn no_caps_is_unlimited() {
    let out = run("print(length(range(0, 5000)))\n", |_| {})
        .await
        .expect("no caps -> unlimited");
    let n: usize = out.trim().parse().expect("numeric length");
    assert!(n >= 5000, "got {n}");
}

// ---------------------------------------------------------------------------
// Safety invariant — the lib default must not overflow a small stack
// ---------------------------------------------------------------------------

/// The library default recursion limit must be safe on the *smallest*
/// realistic stack (~2 MB: tokio worker / `spawn_blocking` / test
/// threads — e.g. maild's per-message inbound filter). Run an infinite
/// recursion on a 2 MB thread with the DEFAULT limit and assert it
/// returns a clean error. If the default were too high, the thread —
/// and the whole test process — would abort on a native stack overflow
/// instead of failing this assertion. If that happens, lower
/// `DEFAULT_RECURSION_LIMIT`.
/// Compile-time guard: the default must stay conservative for ~2 MB
/// stacks (a too-high default is the bug `default_limit_is_safe_on_2mb_stack`
/// guards against at runtime; this keeps it from silently creeping up).
const _: () = assert!(DEFAULT_RECURSION_LIMIT <= 64);

#[test]
fn default_limit_is_safe_on_2mb_stack() {
    let handle = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("runtime");
            rt.block_on(async {
                let src = "function f($n)\n  return f($n + 1)\nend\nprint(f(0))\n";
                let mut lexer = Lexer::new(src);
                let tokens = lexer.tokenize().expect("lex");
                let mut parser = Parser::new(tokens, src);
                let stmts = parser.parse_program().expect("parse");
                // Evaluator::new() carries the default limits.
                let mut eval = Evaluator::new();
                let err = eval
                    .execute(&stmts)
                    .await
                    .expect_err("must hit the cap, not overflow the stack");
                assert!(
                    err.to_string().contains("recursion depth exceeded"),
                    "got: {err}"
                );
            });
        })
        .expect("spawn 2MB thread");
    handle
        .join()
        .expect("2 MB-stack thread overflowed — lower DEFAULT_RECURSION_LIMIT");
}

/// The `failure[...]` annotation in the builtin contract table classifies
/// **operational** failure only — I/O, network, child exit — and is explicitly
/// NOT about argument validation, which always raises
/// (`builtin_info.rs`, `OperationalFailure`). `mix builtins --json` publishes
/// it, so an agent reads it as "will this raise when the remote is down?".
///
/// The process/remote family all answer "no": they hand back a result map with
/// `ok: false` and an `SSH_*`/`PROCESS_*`/`PIPELINE_*` code. Only the `*_must`
/// twins raise. This pins that, because the tag was flipped to `raises` for
/// `ssh_exec`/`ssh_mix` once already on the strength of their validation
/// raises, which would tell every reader the opposite of what the builtins do.
#[test]
fn operational_failure_annotations_match_what_the_builtins_do() {
    use cosmix_mix::builtin_info::OperationalFailure::{Raises, ReturnsResult};

    for (name, expected) in [
        ("ssh_exec", ReturnsResult),
        ("ssh_mix", ReturnsResult),
        ("ssh_run", ReturnsResult),
        ("run_argv", ReturnsResult),
        ("run_pipeline", ReturnsResult),
        ("run_stream", ReturnsResult),
        ("run_argv_must", Raises),
        ("run_pipeline_must", Raises),
    ] {
        let info = cosmix_mix::builtins::builtin_info_of(name)
            .unwrap_or_else(|| panic!("builtin `{name}` is missing from the contract table"));
        assert_eq!(
            info.contract.failure, expected,
            "builtin `{name}` reports the wrong operational-failure mode"
        );
    }
}
