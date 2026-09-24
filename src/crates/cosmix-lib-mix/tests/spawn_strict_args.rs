//! spawn() argument discipline: the shell form coerces nothing (0.52.0), and
//! the argv form (0.89.0) runs a list directly with no shell.
//!
//! History this pins: originally `spawn(["touch", $p])` stringified the list
//! and handed `[touch, /path]` to `sh -c`, which died "command not found"
//! while spawn returned a healthy-looking PID (it does not wait). 0.52.0 made
//! that a hard TYPE_MISMATCH. 0.89.0 gave the list a real meaning — the argv
//! form, `spawn(argv[, opts])`, running the vector directly (no shell, so no
//! word-splitting or glob surprises) with optional detach/cwd/env/stdio. The
//! string form is unchanged. The argv assertions are paired the same way the
//! old raise-assertions were: the call must SUCCEED *and* its side effect must
//! actually land, so a future spawn that swallowed the argv silently would
//! still fail the test.

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

/// Wait for a file to contain `needle` (trimmed) — for children whose output
/// lands after the file is created (a redirect opens the file before the
/// command writes it), so an existence-only wait can race.
fn wait_for_contents(path: &std::path::Path, needle: &str) -> bool {
    for _ in 0..100 {
        if let Ok(s) = std::fs::read_to_string(path)
            && s.trim() == needle
        {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    false
}

/// Like `wait_for_contents` but a substring match (the file may hold more
/// than the needle, e.g. appended content).
fn wait_for_contents_containing(path: &std::path::Path, needle: &str) -> bool {
    for _ in 0..100 {
        if let Ok(s) = std::fs::read_to_string(path)
            && s.contains(needle)
        {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    false
}

#[tokio::test]
async fn argv_list_runs_without_a_shell() {
    // The behaviour 0.89.0 introduced (and the inverse of what this test
    // pinned before): a list first arg is the argv form and RUNS directly.
    let w = witness("argv");
    let src = format!(
        "$p = spawn([\"touch\", \"{}\"])\nprint($p > 0)\n",
        w.display()
    );
    let out = run(&src).await.expect("argv form must run");
    assert_eq!(out.trim(), "true", "argv spawn returns a positive PID");
    assert!(
        wait_for(&w),
        "argv form must actually run the command: {} missing",
        w.display()
    );
    let _ = std::fs::remove_file(&w);
}

#[tokio::test]
async fn argv_no_shell_means_no_word_splitting() {
    // The point of an argv form: an argument with spaces is ONE argument, not
    // split by a shell. `touch "a b"` under sh -c would make two files; the
    // argv form makes exactly one, literally named "a b".
    let dir = witness("nosplit");
    std::fs::create_dir(&dir).unwrap();
    let target = dir.join("a b");
    let src = format!(
        "spawn([\"touch\", \"{}\"])\n",
        target.display()
    );
    run(&src).await.expect("argv spawn");
    assert!(wait_for(&target), "the single spaced-name file must exist");
    // Exactly one entry, and it is the spaced name — no "a" and "b" split.
    let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).collect();
    assert_eq!(entries.len(), 1, "no word-splitting: exactly one file");
    assert_eq!(entries[0].file_name().to_string_lossy(), "a b");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn argv_detach_puts_the_child_in_a_new_session() {
    // detach:true → setsid → the child leads its own session (SID == PID),
    // detached from the caller's controlling terminal. Read it back via
    // /proc so the assertion is about the real kernel state.
    let out = run("$p = spawn([\"sleep\", \"30\"], {detach: true})\nprint($p)\n")
        .await
        .expect("detached argv spawn");
    let pid: i32 = out.trim().parse().expect("a numeric pid");
    // getsid(pid) via /proc/<pid>/stat field 6 (sid). Give the child a beat.
    std::thread::sleep(std::time::Duration::from_millis(100));
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("child alive");
    // stat fields after the (comm) paren group: state ppid pgrp session ...
    let after = stat.rsplit(')').next().unwrap();
    let fields: Vec<&str> = after.split_whitespace().collect();
    // fields[0]=state, [1]=ppid, [2]=pgrp, [3]=session
    let sid: i32 = fields[3].parse().unwrap();
    assert_eq!(sid, pid, "detach:true must make the child a session leader");
    unsafe { libc::kill(pid, libc::SIGKILL); }
}

/// `/proc/<pid>/stat` fields after the `(comm)` group: [0]=state, [1]=ppid.
fn proc_stat_fields(pid: i32) -> Option<Vec<String>> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = stat.rsplit(')').next()?;
    Some(after.split_whitespace().map(str::to_string).collect())
}

/// Wait (bounded) until the pid has exec'd `sleep`, so the fields read are
/// the real child's, not a pre-exec fork of the test binary.
fn wait_for_exec(pid: i32, comm: &str) -> bool {
    for _ in 0..250 {
        if std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .is_ok_and(|c| c.trim() == comm)
        {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    false
}

#[tokio::test]
async fn argv_detach_double_forks_so_the_caller_never_holds_a_zombie() {
    // TODO-mix P8: a setsid-only child stays the caller's child, so when it
    // exits it is a zombie until the caller reaps — and a long-lived caller
    // (a serve citizen launcher) never does. This test process IS such a
    // long-lived caller: it never waits on the pid. With the double fork the
    // child's parent is not us, and once killed it is reaped by init (or a
    // subreaper) and vanishes from /proc. Against a setsid-only spawn both
    // assertions fail: ppid == our pid, and the killed child lingers as Z.
    let out = run("$p = spawn([\"sleep\", \"30\"], {detach: true})\nprint($p)\n")
        .await
        .expect("detached argv spawn");
    let pid: i32 = out.trim().parse().expect("a numeric pid");
    assert!(wait_for_exec(pid, "sleep"), "the returned pid must be the exec'd child");
    let fields = proc_stat_fields(pid).expect("child alive");
    let ppid: u32 = fields[1].parse().unwrap();
    assert_ne!(ppid, std::process::id(), "detach:true must reparent the child away from the caller");
    assert_eq!(fields[3], pid.to_string(), "the returned pid must still lead its own session");

    unsafe { libc::kill(pid, libc::SIGKILL); }
    let mut last_state = String::new();
    for _ in 0..250 {
        match proc_stat_fields(pid) {
            None => return,
            Some(f) => last_state = f[0].clone(),
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("killed detached child {pid} was never reaped (state {last_state}) — the caller still owns it");
}

#[tokio::test]
async fn argv_without_detach_stays_the_callers_child() {
    // Non-detached spawn keeps its semantics: a plain child of the caller.
    let out = run("$p = spawn([\"sleep\", \"30\"])\nprint($p)\n")
        .await
        .expect("argv spawn");
    let pid: i32 = out.trim().parse().expect("a numeric pid");
    let fields = proc_stat_fields(pid).expect("child alive");
    let ppid: u32 = fields[1].parse().unwrap();
    assert_eq!(ppid, std::process::id(), "a non-detached child is the caller's");
    unsafe {
        libc::kill(pid, libc::SIGKILL);
        libc::waitpid(pid, std::ptr::null_mut(), 0);
    }
}

#[tokio::test]
async fn argv_detach_still_reports_a_missing_binary() {
    // The exec-error pipe must survive the double fork: a missing program is
    // still a raise, not a pid of a grandchild that died on exec.
    let e = run_err("spawn([\"/nonexistent/mix-p8-no-such-binary\"], {detach: true})\n").await;
    assert!(format!("{e}").contains("spawn failed"), "got: {e}");
}

#[tokio::test]
async fn argv_env_reaches_the_child() {
    let w = witness("env");
    let src = format!(
        "spawn([\"sh\", \"-c\", \"echo $MARKER > {}\"], {{env: {{MARKER: \"reached\"}}}})\n",
        w.display()
    );
    run(&src).await.expect("argv spawn with env");
    // Wait for the CONTENT, not just the file: the redirect creates the file
    // before `echo` writes it, so an existence-only check can race.
    assert!(
        wait_for_contents(&w, "reached"),
        "child must run and write the env value: {:?}",
        std::fs::read_to_string(&w)
    );
    let _ = std::fs::remove_file(&w);
}

#[tokio::test]
async fn argv_cwd_nul_is_refused_before_opening_the_log() {
    // A NUL in cwd must be OPTION_INVALID at validation, before the stdout
    // log is opened — so a good log is never truncated on the way to a late
    // failure (the run_argv parity codex flagged).
    let w = witness("cwdnul");
    std::fs::write(&w, b"PRECIOUS").unwrap();
    let src = format!(
        "spawn([\"true\"], {{cwd: \"/tmp/x\\u{{0}}y\", stdout: {{file: \"{}\"}}}})\n",
        w.display()
    );
    let err = run_err(&src).await;
    assert!(err.to_string().contains("cwd contains a NUL"), "got: {err}");
    assert_eq!(
        std::fs::read_to_string(&w).unwrap(),
        "PRECIOUS",
        "the stdout log must not have been opened, let alone truncated"
    );
    let _ = std::fs::remove_file(&w);
}

#[tokio::test]
async fn argv_stderr_stdout_merges_into_the_file() {
    // The load-bearing MAJOR the codex arm took two rounds to get right:
    // stderr:"stdout" must actually merge. To a file route it clones the
    // stdout handle; both the child's stdout AND stderr must land in the
    // one file. (The inherit-merge path is the fd-dup case, exercised live;
    // this pins the file-clone path in CI.)
    let w = witness("merge");
    let src = format!(
        "spawn([\"sh\", \"-c\", \"echo OUT; echo ERR 1>&2\"], \
         {{stdout: {{file: \"{}\"}}, stderr: \"stdout\"}})\n",
        w.display()
    );
    run(&src).await.expect("argv spawn with merge");
    // Wait until both lines have landed.
    let mut got = String::new();
    for _ in 0..100 {
        got = std::fs::read_to_string(&w).unwrap_or_default();
        if got.contains("OUT") && got.contains("ERR") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(got.contains("OUT"), "stdout must land in the file: {got:?}");
    assert!(got.contains("ERR"), "merged stderr must land in the SAME file: {got:?}");
    let _ = std::fs::remove_file(&w);
}

#[tokio::test]
async fn argv_stdout_append_preserves_existing_content() {
    let w = witness("append");
    std::fs::write(&w, "PRIOR\n").unwrap();
    let src = format!(
        "spawn([\"sh\", \"-c\", \"echo ADDED\"], \
         {{stdout: {{file: \"{}\", append: true}}}})\n",
        w.display()
    );
    run(&src).await.expect("argv spawn append");
    assert!(
        wait_for_contents_containing(&w, "ADDED"),
        "append must have added the new line"
    );
    let final_content = std::fs::read_to_string(&w).unwrap();
    assert!(
        final_content.contains("PRIOR") && final_content.contains("ADDED"),
        "append must PRESERVE the prior content: {final_content:?}"
    );
    let _ = std::fs::remove_file(&w);
}

#[tokio::test]
async fn argv_clear_env_starts_from_empty() {
    // clear_env:true drops the inherited environment before layering `env`.
    // With clear_env and no PATH, a bare `env` builtin lookup via sh would
    // fail — so use an absolute interpreter and print the whole environment,
    // asserting only our layered var is present and an inherited one is not.
    let w = witness("clearenv");
    let src = format!(
        "spawn([\"/bin/sh\", \"-c\", \"echo m=$MARKER h=$HOME > {}\"], \
         {{clear_env: true, env: {{MARKER: \"only\"}}}})\n",
        w.display()
    );
    run(&src).await.expect("argv spawn clear_env");
    assert!(wait_for_contents_containing(&w, "m=only"), "layered var must be set");
    let content = std::fs::read_to_string(&w).unwrap();
    // With HOME cleared, `h=$HOME` expands to `h=` at the end of the line.
    assert_eq!(
        content.trim(),
        "m=only h=",
        "clear_env must drop the inherited HOME and keep only the layered MARKER"
    );
    let _ = std::fs::remove_file(&w);
}

#[tokio::test]
async fn argv_capture_is_refused() {
    // Capturing means waiting; spawn is fire-and-forget. Both streams refuse
    // "capture" with OPTION_INVALID before spawning.
    for stream in ["stdout", "stderr"] {
        let err = run_err(&format!("spawn([\"true\"], {{{stream}: \"capture\"}})\n")).await;
        let msg = err.to_string();
        assert!(msg.contains("cannot be \"capture\""), "got: {msg}");
        assert!(msg.contains("run_argv"), "should point at run_argv: {msg}");
    }
}

#[tokio::test]
async fn argv_unknown_option_and_empty_and_nonstring_element_are_refused() {
    let err = run_err("spawn([\"true\"], {bogus: 1})\n").await;
    assert!(err.to_string().contains("unknown option 'bogus'"), "got: {err}");

    let err = run_err("spawn([])\n").await;
    assert!(err.to_string().contains("must not be empty"), "got: {err}");

    let err = run_err("spawn([\"echo\", 42])\n").await;
    let msg = err.to_string();
    assert!(msg.contains("argv[1] must be a string"), "got: {msg}");
    assert!(msg.contains("never coerced"), "got: {msg}");
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
