//! spawn() takes strings and coerces nothing (0.52.0).
//!
//! The bug this pins: `spawn(["touch", $p])` stringified the list to its
//! display form and handed `[touch, /path]` to `sh -c`, which died with
//! "command not found" — while spawn returned a healthy-looking PID, because
//! it does not wait and has no result map to carry the failure. The caller had
//! no signal whatsoever. So the assertions below are paired: the call must
//! RAISE, *and* the side effect the caller asked for must not have happened.
//! Asserting only the raise would still pass if a future spawn silently
//! swallowed the argv and did nothing.

#![cfg(unix)]

use cosmix_mix::error::MixError;
use cosmix_mix::evaluator::{Evaluator, SharedBuf};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;
use std::path::PathBuf;

async fn run(source: &str) -> Result<String, MixError> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize()?;
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program()?;
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    eval.execute(&stmts).await?;
    Ok(stdout.to_string_lossy())
}

async fn run_err(source: &str) -> MixError {
    match run(source).await {
        Ok(out) => panic!("script should have raised, got stdout: {out:?}"),
        Err(e) => e,
    }
}

/// A unique path under the crate's target dir, distinct per test, that the
/// test asserts against and then removes.
fn witness(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "mix-spawn-strict-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

/// Wait for a spawned child's side effect, bounded — spawn does not wait, so
/// the happy-path test cannot assume the touch has landed on return.
fn wait_for(path: &std::path::Path) -> bool {
    for _ in 0..100 {
        if path.exists() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    false
}

#[tokio::test]
async fn argv_list_as_cmd_raises_and_runs_nothing() {
    let w = witness("list");
    let src = format!("spawn([\"touch\", \"{}\"])\n", w.display());

    let err = run_err(&src).await;
    let msg = err.to_string();
    assert!(
        msg.contains("cmd must be a shell command string"),
        "error must say what is wrong with the argument, got: {msg}"
    );
    assert!(
        msg.contains("run_argv"),
        "error must name the argv-capable runner, got: {msg}"
    );

    // The load-bearing half: the child must never have run. Give a would-be
    // child the same grace the happy path gets, so this cannot pass merely by
    // racing it.
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(
        !w.exists(),
        "spawn raised but still ran something: {} exists",
        w.display()
    );
}

#[tokio::test]
async fn string_cmd_still_spawns() {
    let w = witness("string");
    let src = format!("$p = spawn(\"touch '{}'\")\nprint($p > 0)\n", w.display());

    let out = run(&src).await.expect("string form must still work");
    assert_eq!(out.trim(), "true", "spawn must return a positive PID");
    assert!(
        wait_for(&w),
        "string form must actually run the command: {} missing",
        w.display()
    );
    let _ = std::fs::remove_file(&w);
}

#[tokio::test]
async fn non_string_stdio_paths_raise_rather_than_creating_stringified_files() {
    // The same coercion hole existed on the path arguments: a list would have
    // been stringified into a file literally named "[a]".
    let err = run_err("spawn(\"true\", [\"a\"])\n").await;
    assert!(
        err.to_string().contains("stdout_path must be a string"),
        "got: {err}"
    );

    let err = run_err("spawn(\"true\", \"/dev/null\", 7)\n").await;
    assert!(
        err.to_string().contains("stderr_path must be a string"),
        "got: {err}"
    );
}

#[tokio::test]
async fn every_non_string_cmd_type_raises() {
    for literal in ["7", "true", "nil", "{a: 1}"] {
        let err = run_err(&format!("spawn({literal})\n")).await;
        let msg = err.to_string();
        assert!(
            msg.contains("spawn: cmd must be"),
            "spawn({literal}) must raise a cmd type error, got: {msg}"
        );
        assert!(
            !msg.is_empty() && msg.contains("string"),
            "spawn({literal}) error must name the expected type, got: {msg}"
        );
    }
}

#[tokio::test]
async fn nul_byte_in_cmd_raises() {
    // Not a new rejection — std's Command::spawn already refused an interior
    // NUL ("nul byte found in provided data"). What is new is that it is
    // refused as TYPE_MISMATCH during argument validation. See the next test
    // for the behavioural difference that buys.
    let err = run_err("spawn(\"true\\u{0}rm -rf /\")\n").await;
    assert!(err.to_string().contains("NUL"), "got: {err}");
}

#[tokio::test]
async fn a_bad_stderr_path_no_longer_truncates_the_good_stdout_file() {
    // The reason validating before opening matters. spawn opened stdout first,
    // so a NUL in stderr_path destroyed the contents of a perfectly valid
    // stdout file on its way to failing. Verified against 0.51.0: the witness
    // came back empty.
    let w = witness("nultrunc");
    std::fs::write(&w, b"PRECIOUS CONTENT").unwrap();

    let err = run_err(&format!(
        "spawn(\"true\", \"{}\", \"/tmp/bad\\u{{0}}path\")\n",
        w.display()
    ))
    .await;
    assert!(err.to_string().contains("NUL"), "got: {err}");

    assert_eq!(
        std::fs::read_to_string(&w).unwrap(),
        "PRECIOUS CONTENT",
        "the stdout file must not have been opened, let alone truncated"
    );
    let _ = std::fs::remove_file(&w);
}

// ── kill(): the same coercion hole, with a worse blast radius ──────────

// NOTE: the `kill(false)` / `process_alive(false)` cases deliberately live in
// cosmix-mix/tests/kill_pid_not_coerced.rs, out of process. Reverting the fix
// makes them signal the caller's own process group, which kills an in-process
// test runner outright instead of failing an assertion. See that file.

#[tokio::test]
async fn string_pid_is_not_coerced() {
    let err = run_err("kill(\"12345\")\n").await;
    assert!(
        err.to_string().contains("pid must be a number"),
        "got: {err}"
    );
}

#[tokio::test]
async fn unrecognised_signal_raises_rather_than_silently_sending_sigterm() {
    // The old code was `.and_then(to_number).unwrap_or(15.0)`, so a caller
    // who wrote kill($p, "SIGKILL") sent SIGTERM and was told it worked.
    //
    // The pid here is 999999 rather than 1 deliberately: if this fix is ever
    // reverted, the assertion fails but the call underneath it still RUNS, and
    // `kill(1, 15)` from a root test runner in a container signals init. A pid
    // that reliably does not exist keeps the revert harmless.
    let err = run_err("kill(999999, \"SIGKILL\")\n").await;
    assert!(
        err.to_string().contains("signal must be a number"),
        "got: {err}"
    );
    assert!(
        !err.to_string().contains("entire group"),
        "the pid-specific warning must not be quoted at a bad SIGNAL: {err}"
    );
}

// ── process_alive(): the last coercion machine in the family ───────────

#[tokio::test]
async fn process_alive_does_not_coerce_its_pid() {
    // process_alive(false) returned TRUE: to_number(false) is 0, waitpid(0,
    // WNOHANG) reaps an arbitrary child of this process group — a side effect,
    // not merely a wrong answer — and kill(0, 0) then succeeds.
    for literal in ["false", "true", "\"123\"", "1.9"] {
        let err = run_err(&format!("process_alive({literal})\n")).await;
        assert!(
            err.to_string().contains("process_alive: pid must be"),
            "process_alive({literal}) must raise, got: {err}"
        );
    }
}

#[tokio::test]
async fn process_alive_still_answers_for_real_pids() {
    let out = run("$p = spawn(\"sleep 30\")\nprint(process_alive($p))\nprint(process_alive(999999))\nkill($p)\n")
        .await
        .expect("numeric process_alive must keep working");
    let lines: Vec<&str> = out.trim().lines().collect();
    assert_eq!(lines, vec!["true", "false"], "got: {out:?}");
}

#[tokio::test]
async fn fractional_pid_or_signal_raises_rather_than_truncating() {
    for src in ["kill(1.5)", "kill(1, 9.5)"] {
        let err = run_err(&format!("{src}\n")).await;
        assert!(
            err.to_string().contains("must be a whole number"),
            "{src} must refuse truncation, got: {err}"
        );
    }
}

#[tokio::test]
async fn numeric_kill_still_works_and_reports_honestly() {
    // Behaviour that must NOT change: a real signal to a real child, and an
    // honest false for a pid that isn't there.
    let out = run("$p = spawn(\"sleep 30\")\nprint(kill($p))\nprint(kill(999999))\n")
        .await
        .expect("numeric kill must keep working");
    let lines: Vec<&str> = out.trim().lines().collect();
    assert_eq!(lines, vec!["true", "false"], "got: {out:?}");
}

// ── access(): the shared kernel path must not drift it ─────────────────

#[tokio::test]
async fn access_messages_are_unchanged_by_the_shared_helper() {
    // which() reuses access()'s faccessat2 path via access_ok(), which took a
    // `caller` parameter so a which() failure stops reporting itself as
    // "access '...'". Threading that parameter through silently rewrote
    // access()'s own NUL message from "access():" to "access:" once already.
    // These strings are a contract; pin them.
    // ends_with, not contains: anchored at the end so a drift like
    // "access: ..." (the exact regression this pins) cannot satisfy it, while
    // the evaluator's "Runtime error at line N: " prefix stays out of the way.
    let err = run_err("access(\"/tmp/a\\u{0}b\", \"x\")\n").await;
    assert!(
        err.to_string()
            .ends_with("access(): path contains an interior NUL byte"),
        "access()'s NUL message must match 0.51.0 exactly, got: {err}"
    );

    // And the ordinary answers stay answers, not raises.
    let out =
        run("print(access(\"/bin/sh\", \"x\"))\nprint(access(\"/nonexistent-xyz\", \"f\"))\n")
            .await
            .expect("access must not raise for ordinary yes/no");
    let lines: Vec<&str> = out.trim().lines().collect();
    assert_eq!(lines, vec!["true", "false"], "got: {out:?}");
}
