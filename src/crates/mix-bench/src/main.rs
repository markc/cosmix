//! mix-bench — autoresearch metric harness for the standalone Mix arena.
//!
//! Two corpora:
//!   1. correctness corpus — every `*.mix` script under
//!      `cosmix-lib-mix/tests/scripts/`. Each script declares its expected
//!      output as a `-- Expected output:` comment block; we run the script,
//!      compare stdout, and count failures.
//!   2. bench corpus — a curated subset of CPU-bound scripts plus a small
//!      set of synthetic stressers, run repeatedly. Wall-clock of this run
//!      is the timing component of the metric.
//!
//! Score:  bench_ms + failed_tests * 60_000  (lower is better)
//!
//! Final stdout line:  `score: <float>`
//!
//! Exit code: 0 if the harness ran to completion (even with test failures).
//! Non-zero only on harness panic / unhandled error (treated as a crash by
//! the autoresearch loop).

use std::env;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Instant;

use cosmix_mix::error::MixResult;
use cosmix_mix::evaluator::{BusHandler, Evaluator, IncomingEvent, SharedBuf};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;
use cosmix_mix::value::Value;

// ── Mock Bus handler used by send.mix and any other script that exercises
//    the `send`/`address`/`emit` keywords. Echoes command + first arg.

struct MockBusHandler;

impl BusHandler for MockBusHandler {
    fn send<'a>(
        &'a self,
        _target: &'a str,
        command: &'a str,
        args: &'a Value,
    ) -> Pin<Box<dyn Future<Output = MixResult<(i32, Value)>> + 'a>> {
        Box::pin(async move {
            let result = match args {
                Value::Map(m) => {
                    if command == "greet" {
                        let name = m
                            .get("name")
                            .map(|v| v.to_mix_string())
                            .unwrap_or_else(|| "unknown".to_string());
                        format!("{} from mock", name)
                    } else {
                        let first_val = m
                            .values()
                            .next()
                            .map(|v| v.to_mix_string())
                            .unwrap_or_default();
                        format!("{}:{} from mock", command, first_val)
                    }
                }
                _ => format!("{}: from mock", command),
            };
            Ok((0, Value::String(result)))
        })
    }

    fn emit<'a>(
        &'a self,
        _target: &'a str,
        _command: &'a str,
        _args: &'a Value,
    ) -> Pin<Box<dyn Future<Output = MixResult<()>> + 'a>> {
        Box::pin(async move { Ok(()) })
    }

    fn port_exists<'a>(
        &'a self,
        _target: &'a str,
    ) -> Pin<Box<dyn Future<Output = MixResult<bool>> + 'a>> {
        Box::pin(async move { Ok(true) })
    }

    fn next_incoming<'a>(&'a self) -> Pin<Box<dyn Future<Output = Option<IncomingEvent>> + 'a>> {
        Box::pin(async move { None })
    }
}

/// Per-script policy: which extras (extension fns, Bus handler) to install
/// before running. The default is `Plain`; only a couple of scripts need
/// anything else, hard-coded by filename below.
#[derive(Copy, Clone)]
enum Policy {
    Plain,
    Extensions,
    Bus,
}

fn policy_for(name: &str) -> Policy {
    match name {
        "extensions.mix" => Policy::Extensions,
        "send.mix" => Policy::Bus,
        _ => Policy::Plain,
    }
}

/// Configure an Evaluator for the given script's policy. Stdout/stderr are
/// always captured into the supplied buffers — the bench is non-interactive.
fn configure(eval: &mut Evaluator, policy: Policy) {
    // The bench runs fib(22) (recursion depth 22) on the ~8 MB main
    // thread — raise the cap above the conservative library default
    // (sized for ~2 MB embedder stacks), which would otherwise trip.
    eval.set_limits(cosmix_mix::EvalLimits {
        recursion_limit: 512,
        ..Default::default()
    });
    match policy {
        Policy::Plain => {}
        Policy::Extensions => {
            eval.register(
                "add_numbers",
                cosmix_mix::sync_ext(|args| {
                    let a = args.first().and_then(|v| v.to_number()).unwrap_or(0.0);
                    let b = args.get(1).and_then(|v| v.to_number()).unwrap_or(0.0);
                    Ok(Value::Number(a + b))
                }),
            );
            eval.register(
                "greet",
                cosmix_mix::sync_ext(|args| {
                    let name = args.first().map(|v| v.to_mix_string()).unwrap_or_default();
                    Ok(Value::String(format!("hello from {}", name)))
                }),
            );
            eval.register(
                "get_list",
                cosmix_mix::sync_ext(|_args| {
                    Ok(Value::list(vec![
                        Value::String("alpha".to_string()),
                        Value::String("bravo".to_string()),
                        Value::String("charlie".to_string()),
                    ]))
                }),
            );
        }
        Policy::Bus => {
            eval.set_bus_handler(std::rc::Rc::new(MockBusHandler));
        }
    }
}

fn extract_expected(source: &str) -> Vec<String> {
    let mut expected = Vec::new();
    let mut in_expected = false;
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("-- Expected output:") {
            in_expected = true;
            continue;
        }
        if in_expected {
            if let Some(rest) = trimmed.strip_prefix("-- ") {
                expected.push(rest.to_string());
            } else if trimmed == "--" {
                expected.push(String::new());
            } else {
                break;
            }
        }
    }
    expected
}

/// Run a script source through a freshly configured Evaluator and return
/// captured stdout. `policy` selects per-script setup; `name` is just for
/// nicer error messages.
async fn run_source_capturing(source: &str, policy: Policy, name: &str) -> Result<String, String> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer
        .tokenize()
        .map_err(|e| format!("{}: lex: {}", name, e))?;
    let mut parser = Parser::new(tokens, source);
    let stmts = parser
        .parse_program()
        .map_err(|e| format!("{}: parse: {}", name, e))?;

    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    configure(&mut eval, policy);

    eval.execute(&stmts)
        .await
        .map_err(|e| format!("{}: exec: {}", name, e))?;

    Ok(stdout.to_string_lossy())
}

#[derive(Debug)]
struct TestOutcome {
    name: String,
    passed: bool,
    note: Option<String>,
}

async fn run_test_script(scripts_dir: &Path, name: &str) -> TestOutcome {
    let path = scripts_dir.join(name);
    let source = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            return TestOutcome {
                name: name.to_string(),
                passed: false,
                note: Some(format!("read: {}", e)),
            };
        }
    };
    let expected = extract_expected(&source);
    if expected.is_empty() {
        return TestOutcome {
            name: name.to_string(),
            passed: false,
            note: Some("no expected output block".into()),
        };
    }

    match run_source_capturing(&source, policy_for(name), name).await {
        Ok(output) => {
            let actual: Vec<&str> = output.trim_end_matches('\n').lines().collect();
            if actual.len() != expected.len() {
                return TestOutcome {
                    name: name.to_string(),
                    passed: false,
                    note: Some(format!(
                        "line count: expected {}, got {}",
                        expected.len(),
                        actual.len()
                    )),
                };
            }
            for (i, (a, e)) in actual.iter().zip(expected.iter()).enumerate() {
                if a != e {
                    return TestOutcome {
                        name: name.to_string(),
                        passed: false,
                        note: Some(format!("line {}: expected `{}`, got `{}`", i + 1, e, a)),
                    };
                }
            }
            TestOutcome {
                name: name.to_string(),
                passed: true,
                note: None,
            }
        }
        Err(e) => TestOutcome {
            name: name.to_string(),
            passed: false,
            note: Some(e),
        },
    }
}

// ── Synthetic bench programs. These stress hot paths in the interpreter
//    that the test corpus only touches lightly. Each is sized to take
//    ~50–500ms in release builds; the harness loops over them once per
//    iteration and totals the wall time.

const BENCH_FIB: &str = r#"
function fib($n)
    if $n < 2 then
        return $n
    end
    return fib($n - 1) + fib($n - 2)
end
$_ = fib(22)
"#;

const BENCH_LOOP_ARITH: &str = r#"
$sum = 0
for $i = 1 to 1000000
    $sum = $sum + $i * 2 - 1
end
"#;

const BENCH_LIST_BUILD: &str = r#"
$xs = []
for $i = 1 to 20000
    push($xs, $i)
end
$total = 0
for each $x in $xs
    $total = $total + $x
end
"#;

const BENCH_HOFS: &str = r#"
$xs = []
for $i = 1 to 8000
    push($xs, $i)
end
$ys = map($xs, function($x) = $x * 3 + 1)
$zs = filter($ys, function($x) = $x % 2 == 0)
$_ = reduce($zs, 0, function($a, $b) = $a + $b)
"#;

const BENCH_STRINGS: &str = r#"
$s = ""
for $i = 1 to 3000
    $s = $s .. "ab"
end
$n = 0
for $i = 1 to 3000
    if contains($s, "ab") then
        $n = $n + 1
    end
end
"#;

const BENCH_TABLE: &str = r#"
$t = {}
for $i = 1 to 6000
    $t["k${i}"] = $i * 2
end
$total = 0
for each $k in keys($t)
    $total = $total + $t[$k]
end
"#;

/// (label, source, iterations) — iterations multiplies the body so a single
/// label reaches a measurable wall-time without one program dominating.
fn bench_corpus() -> Vec<(&'static str, &'static str, u32)> {
    vec![
        ("fib", BENCH_FIB, 8),
        ("loop_arith", BENCH_LOOP_ARITH, 1),
        ("list_build", BENCH_LIST_BUILD, 1),
        ("hofs", BENCH_HOFS, 2),
        ("strings", BENCH_STRINGS, 1),
        ("table", BENCH_TABLE, 1),
    ]
}

/// Test scripts that are also good interpreter stressers and don't touch
/// disk or spawn subprocesses — safe to include in timed benches.
const BENCH_REPLAY_SCRIPTS: &[&str] = &[
    "variables.mix",
    "strings.mix",
    "control.mix",
    "loops.mix",
    "functions.mix",
    "lists.mix",
    "list_hofs.mix",
    "lambdas.mix",
    "slicing.mix",
    "printf.mix",
    "tables.mix",
    "terminators.mix",
    "parse.mix",
    "json.mix",
    "jsonl.mix",
    "labels.mix",
    "continue_for_each.mix",
];

const BENCH_REPLAY_ITERATIONS: u32 = 50;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // `--json` is accepted for compatibility with program.md's invocation
    // shape; we don't actually emit JSON, but the loop driver may pass it.
    let _args: Vec<String> = env::args().skip(1).collect();

    let workspace_root = workspace_root();
    // Test scripts use relative paths like `tests/fs_fixtures/...` —
    // chdir into the cosmix-lib-mix crate dir so those resolve, matching
    // how `cargo test` runs the integration suite.
    let crate_dir = workspace_root.join("cosmix-lib-mix");
    if let Err(e) = env::set_current_dir(&crate_dir) {
        eprintln!("mix-bench: chdir {}: {}", crate_dir.display(), e);
        std::process::exit(2);
    }
    let scripts_dir = PathBuf::from("tests/scripts");

    if !scripts_dir.exists() {
        eprintln!(
            "mix-bench: scripts dir not found: {}",
            scripts_dir.display()
        );
        std::process::exit(2);
    }

    // ── Phase 1: correctness corpus
    let mut script_names: Vec<String> = fs::read_dir(&scripts_dir)
        .expect("read_dir scripts")
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(".mix") {
                Some(name)
            } else {
                None
            }
        })
        .collect();
    script_names.sort();

    eprintln!("== test corpus ({} scripts)", script_names.len());
    let test_start = Instant::now();
    let mut failed_tests = 0u32;
    let mut outcomes: Vec<TestOutcome> = Vec::with_capacity(script_names.len());
    for name in &script_names {
        let outcome = run_test_script(&scripts_dir, name).await;
        if !outcome.passed {
            failed_tests += 1;
            eprintln!(
                "  FAIL {}{}",
                outcome.name,
                outcome
                    .note
                    .as_deref()
                    .map(|n| format!(" — {}", n))
                    .unwrap_or_default()
            );
        }
        outcomes.push(outcome);
    }
    let test_ms = test_start.elapsed().as_secs_f64() * 1000.0;
    let passed = outcomes.iter().filter(|o| o.passed).count();
    eprintln!(
        "  passed={} failed={} ({:.1} ms)",
        passed, failed_tests, test_ms
    );

    // ── Phase 2: bench corpus (synthetic stressers + replayed scripts)
    eprintln!("== bench corpus");
    let bench_start = Instant::now();

    let synthetic = bench_corpus();
    for (label, src, iters) in &synthetic {
        let label_start = Instant::now();
        for _ in 0..*iters {
            if let Err(e) = run_source_capturing(src, Policy::Plain, label).await {
                eprintln!("  CRASH bench {} — {}", label, e);
                std::process::exit(3);
            }
        }
        eprintln!(
            "  bench {:14} x{} {:>7.1} ms",
            label,
            iters,
            label_start.elapsed().as_secs_f64() * 1000.0
        );
    }

    for name in BENCH_REPLAY_SCRIPTS {
        let path = scripts_dir.join(name);
        let source = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let label_start = Instant::now();
        for _ in 0..BENCH_REPLAY_ITERATIONS {
            if let Err(e) = run_source_capturing(&source, policy_for(name), name).await {
                eprintln!("  CRASH replay {} — {}", name, e);
                std::process::exit(3);
            }
        }
        eprintln!(
            "  replay {:14} x{} {:>7.1} ms",
            name,
            BENCH_REPLAY_ITERATIONS,
            label_start.elapsed().as_secs_f64() * 1000.0
        );
    }

    let bench_ms = bench_start.elapsed().as_secs_f64() * 1000.0;
    eprintln!("  bench total: {:.1} ms", bench_ms);

    // ── Score (lower is better)
    let score = bench_ms + (failed_tests as f64) * 60_000.0;
    eprintln!("failed_tests: {}", failed_tests);
    eprintln!("bench_ms: {:.3}", bench_ms);
    println!("score: {:.3}", score);
}

/// Best-effort workspace-root detection: return the directory that contains
/// `cosmix-lib-mix/tests/scripts/`. mix-bench is a workspace sibling of
/// `cosmix-lib-mix`, so the compile-time `CARGO_MANIFEST_DIR` is
/// `.../src/crates/mix-bench` and `.parent()` is the workspace's
/// `crates/` directory — `cosmix-lib-mix/tests/scripts/` is one join
/// away. Falls back to a CWD walk for cargo invocations from
/// oddly-cwd'd environments.
fn workspace_root() -> PathBuf {
    let from_manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    // Sibling crate inside the same `crates/` directory.
    if let Some(p) = from_manifest.parent()
        && p.join("cosmix-lib-mix/tests/scripts").exists()
    {
        return p.to_path_buf();
    }

    // CWD walk fallback (legacy).
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut cur = cwd.as_path();
    loop {
        if cur.join("cosmix-lib-mix/tests/scripts").exists() {
            return cur.to_path_buf();
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => return cwd,
        }
    }
}
