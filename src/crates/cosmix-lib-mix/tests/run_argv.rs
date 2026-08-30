//! run_argv / run_argv_must (0.29.0, decision record D4): direct-argv
//! captured bounded process execution. Unix-only assumptions (signal
//! numbers, /bin/sh helpers for child behavior) match the rest of the
//! process-builtin test suite.

#![cfg(unix)]

use cosmix_mix::error::MixError;
use cosmix_mix::evaluator::{Evaluator, SharedBuf};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;
use std::path::{Path, PathBuf};

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};

        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "mix-run-argv-{label}-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).expect("create test directory");
        Self(path)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn mix_path(path: &Path) -> String {
    format!("{:?}", path.to_string_lossy())
}

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

async fn run_ok(source: &str) -> String {
    match run(source).await {
        Ok(out) => out,
        Err(e) => panic!("script should succeed, got: {e}"),
    }
}

// ── happy path & result schema ──────────────────────────────────────

#[tokio::test]
async fn captures_stdout_stderr_and_exit_code() {
    let out = run_ok(
        "$r = run_argv([\"sh\", \"-c\", \"echo out; echo err >&2; exit 3\"])\nprint($r.ok)\nprint($r.exit_code)\nprint($r.stdout)\nprint($r.stderr)\nprint($r.timed_out .. \" \" .. $r.interrupted .. \" \" .. $r.signal .. \" \" .. $r.stdout_truncated .. \" \" .. $r.stderr_truncated)\nprint($r.error_code .. \"/\" .. $r.error)\n",
    )
    .await;
    assert_eq!(
        out,
        "false\n3\nout\n\nerr\n\nfalse false nil false false\nnil/nil\n"
    );
}

#[tokio::test]
async fn ok_true_on_clean_exit_and_schema_keys_present() {
    let out = run_ok(
        "$r = run_argv([\"true\"])\nprint($r.ok .. \" \" .. $r.exit_code)\nfor each $k in keys($r)\n  print($k)\nend\n",
    )
    .await;
    assert_eq!(
        out,
        "true 0\nok\nexit_code\nstdout\nstderr\ntimed_out\ninterrupted\nsignal\nduration_ms\nstdout_truncated\nstderr_truncated\nutf8_lossy\nerror_code\nerror\n"
    );
}

#[tokio::test]
async fn stdout_is_not_trimmed() {
    let out =
        run_ok("$r = run_argv([\"printf\", \"a\\n\"])\nprint(byte_length($r.stdout))\n").await;
    // "a\n" = 2 bytes — the trailing newline is preserved (unlike run/run_rc).
    assert_eq!(out, "2\n");
}

// ── argv validation (TYPE_MISMATCH, before spawn) ───────────────────

#[tokio::test]
async fn argv_validation_raises_type_mismatch() {
    for (snippet, needle) in [
        ("run_argv([])", "must not be empty"),
        ("run_argv(\"echo hi\")", "must be a list"),
        ("run_argv([\"echo\", 42])", "argv[1] must be a string"),
    ] {
        let src =
            format!("try\n  {snippet}\ncatch $m, $e\n  print($e.code .. \": \" .. $m)\nend\n");
        let out = run_ok(&src).await;
        assert!(
            out.starts_with("TYPE_MISMATCH:") && out.contains(needle),
            "{snippet} -> {out}"
        );
    }
}

// ── option validation (OPTION_INVALID, before spawn) ────────────────

#[tokio::test]
async fn option_validation_raises_option_invalid() {
    for (opts, needle) in [
        ("{bogus: 1}", "unknown option 'bogus'"),
        ("{timeout: \"x\"}", "timeout must be a number"),
        ("{timeout: -1}", "non-negative"),
        ("{env: {\"BAD-NAME\": \"v\"}}", "not a valid name"),
        ("{env: {GOOD: [1]}}", "env value for 'GOOD'"),
        ("{clear_env: 1}", "clear_env must be a bool"),
        ("{max_output: 1.5}", "whole number"),
        ("{stream: 1}", "stream must be a bool"),
        ("{stdin: {null: false}}", "stdin routing map"),
        ("{stdin: {file: \"x\", null: true}}", "stdin routing map"),
        ("{stdout: \"other\"}", "stdout must be"),
        ("{stdout: {append: true}}", "requires `file`"),
        (
            "{stdout: {file: \"x\", append: 1}}",
            "append must be a bool",
        ),
        (
            "{stdout: {file: \"x\", mode: 1.5}}",
            "mode must be a whole number",
        ),
        (
            "{stdout: {file: \"x\", bogus: 1}}",
            "unknown stdout file option",
        ),
        ("{stderr: \"other\"}", "stderr must be"),
        ("{stream: true, stdout: \"inherit\"}", "cannot be combined"),
    ] {
        let src = format!(
            "try\n  run_argv([\"true\"], {opts})\ncatch $m, $e\n  print($e.code .. \": \" .. $m)\nend\n"
        );
        let out = run_ok(&src).await;
        assert!(
            out.starts_with("OPTION_INVALID:") && out.contains(needle),
            "{opts} -> {out}"
        );
    }
}

// ── env / clear_env / cwd ───────────────────────────────────────────

#[tokio::test]
async fn env_overlay_and_value_coercion() {
    let out = run_ok(
        "$r = run_argv([\"sh\", \"-c\", \"printf %s-%s-%s \\\"$A\\\" \\\"$N\\\" \\\"$B\\\"\"], {env: {A: \"x\", N: 7, B: true}})\nprint($r.stdout)\n",
    )
    .await;
    assert_eq!(out, "x-7-true\n");
}

#[tokio::test]
async fn clear_env_empties_inherited_environment() {
    let out = run_ok(
        "$r = run_argv([\"/usr/bin/env\"], {clear_env: true, env: {ONLY: \"me\"}})\nprint($r.stdout)\n",
    )
    .await;
    assert_eq!(out, "ONLY=me\n\n");
}

#[tokio::test]
async fn cwd_changes_child_working_directory() {
    let out = run_ok("$r = run_argv([\"pwd\"], {cwd: \"/tmp\"})\nprint($r.stdout)\n").await;
    assert_eq!(out, "/tmp\n\n");
}

// ── stdin ───────────────────────────────────────────────────────────

#[tokio::test]
async fn stdin_string_and_default_closed() {
    let out = run_ok(
        "$r = run_argv([\"cat\"], {stdin: \"fed\"})\nprint($r.stdout)\n$c = run_argv([\"cat\"])\nprint($c.ok .. \" \" .. byte_length($c.stdout))\n",
    )
    .await;
    // Default stdin is CLOSED, so bare `cat` exits immediately with 0 output.
    assert_eq!(out, "fed\ntrue 0\n");
}

#[tokio::test]
async fn stdin_file_and_explicit_null_routes() {
    let dir = TestDir::new("stdin-routes");
    let input = dir.path("input.txt");
    std::fs::write(&input, b"from-file").expect("write stdin fixture");
    let source = format!(
        "$f = run_argv([\"cat\"], {{stdin: {{file: {}}}}})\n\
         print($f.stdout)\n\
         $n = run_argv([\"cat\"], {{stdin: {{null: true}}}})\n\
         print($n.ok .. \" \" .. byte_length($n.stdout))\n\
         $literal = run_argv([\"cat\"], {{stdin: \"inherit\"}})\n\
         print($literal.stdout)\n",
        mix_path(&input)
    );
    let out = run_ok(&source).await;
    // stdin strings remain data. In particular, "inherit" is payload;
    // there is deliberately no inherit-stdin route.
    assert_eq!(out, "from-file\ntrue 0\ninherit\n");
}

// ── structured stdout/stderr routing ───────────────────────────────

#[tokio::test]
async fn null_and_inherit_routes_return_empty_uncapped_streams() {
    let out = run_ok(
        "$n = run_argv([\"sh\", \"-c\", \"printf 12345; printf abcde >&2\"], {stdout: \"null\", stderr: \"null\", max_output: 1})\nprint(byte_length($n.stdout) .. \" \" .. byte_length($n.stderr) .. \" \" .. $n.stdout_truncated .. \" \" .. $n.stderr_truncated)\n$i = run_argv([\"sh\", \"-c\", \"echo inherited-out; echo inherited-err >&2\"], {stdout: \"inherit\", stderr: \"inherit\", max_output: 1})\nprint(byte_length($i.stdout) .. \" \" .. byte_length($i.stderr) .. \" \" .. $i.stdout_truncated .. \" \" .. $i.stderr_truncated)\n",
    )
    .await;
    assert_eq!(out, "0 0 false false\n0 0 false false\n");
}

#[tokio::test]
async fn stream_true_with_stdout_inherit_is_rejected_before_spawn() {
    let dir = TestDir::new("stream-inherit-invalid");
    let marker = dir.path("child-ran");
    let source = format!(
        "try\n\
           run_argv([\"touch\", {}], {{stream: true, stdout: \"inherit\"}})\n\
         catch $m, $e\n\
           print($e.code)\n\
         end\n",
        mix_path(&marker),
    );
    assert_eq!(run_ok(&source).await, "OPTION_INVALID\n");
    assert!(
        !marker.exists(),
        "child was spawned after option validation failed"
    );
}

#[tokio::test]
async fn file_routes_truncate_append_and_apply_creation_modes() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TestDir::new("file-routes");
    let stdout = dir.path("stdout.txt");
    let stderr = dir.path("stderr.txt");
    let source = format!(
        "$a = run_argv([\"sh\", \"-c\", \"printf first; printf err1 >&2\"], {{stdout: {{file: {}}}, stderr: {{file: {}, mode: 0o640}}, max_output: 1}})\n\
         print(byte_length($a.stdout) .. \" \" .. byte_length($a.stderr) .. \" \" .. $a.stdout_truncated .. \" \" .. $a.stderr_truncated)\n\
         run_argv([\"printf\", \"second\"], {{stdout: {{file: {}}}}})\n\
         run_argv([\"sh\", \"-c\", \"printf +out; printf +err >&2\"], {{stdout: {{file: {}, append: true}}, stderr: {{file: {}, append: true}}}})\n",
        mix_path(&stdout),
        mix_path(&stderr),
        mix_path(&stdout),
        mix_path(&stdout),
        mix_path(&stderr),
    );
    assert_eq!(run_ok(&source).await, "0 0 false false\n");
    assert_eq!(std::fs::read(&stdout).unwrap(), b"second+out");
    assert_eq!(std::fs::read(&stderr).unwrap(), b"err1+err");
    assert_eq!(
        std::fs::metadata(&stdout).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(&stderr).unwrap().permissions().mode() & 0o777,
        0o640
    );
}

#[tokio::test]
async fn stderr_stdout_merges_into_stdout_capture_and_file_route() {
    let dir = TestDir::new("stderr-merge");
    let merged = dir.path("merged.txt");
    let source = format!(
        "$r = run_argv([\"sh\", \"-c\", \"printf out; printf err >&2\"], {{stderr: \"stdout\"}})\n\
         print($r.stdout)\n\
         print(byte_length($r.stderr) .. \" \" .. $r.stderr_truncated)\n\
         $c = run_argv([\"sh\", \"-c\", \"printf out; printf err >&2\"], {{stderr: \"stdout\", max_output: 3}})\n\
         print($c.stdout .. \" \" .. $c.stdout_truncated .. \" \" .. byte_length($c.stderr) .. \" \" .. $c.stderr_truncated)\n\
         $f = run_argv([\"sh\", \"-c\", \"printf fileout; printf fileerr >&2\"], {{stdout: {{file: {}}}, stderr: \"stdout\", max_output: 1}})\n\
         print(byte_length($f.stdout) .. \" \" .. byte_length($f.stderr) .. \" \" .. $f.stdout_truncated .. \" \" .. $f.stderr_truncated)\n",
        mix_path(&merged)
    );
    assert_eq!(
        run_ok(&source).await,
        "outerr\n0 false\nout true 0 false\n0 0 false false\n"
    );
    assert_eq!(std::fs::read(&merged).unwrap(), b"fileoutfileerr");
}

#[tokio::test]
async fn stdio_open_failures_are_values_and_prevent_spawn() {
    let dir = TestDir::new("stdio-failure");
    let marker = dir.path("child-ran");
    let missing_input = dir.path("missing-input");
    let bad_output = dir.path("missing-parent/output");
    let source = format!(
        "$i = run_argv([\"sh\", \"-c\", \"touch {}\"], {{stdin: {{file: {}}}}})\n\
         print($i.ok .. \" \" .. $i.exit_code .. \" \" .. $i.error_code .. \" \" .. byte_length($i.stdout) .. \" \" .. $i.stdout_truncated)\n\
         $o = run_argv([\"sh\", \"-c\", \"touch {}\"], {{stdout: {{file: {}}}}})\n\
         print($o.ok .. \" \" .. $o.exit_code .. \" \" .. $o.error_code)\n",
        marker.display(),
        mix_path(&missing_input),
        marker.display(),
        mix_path(&bad_output),
    );
    assert_eq!(
        run_ok(&source).await,
        "false nil PROCESS_STDIO 0 false\nfalse nil PROCESS_STDIO\n"
    );
    assert!(
        !marker.exists(),
        "child was spawned after stdio setup failed"
    );
}

// ── timeout / signal ────────────────────────────────────────────────

#[tokio::test]
async fn timeout_kills_and_flags() {
    let out = run_ok(
        "$r = run_argv([\"sleep\", \"5\"], {timeout: 0.2})\nprint($r.ok .. \" \" .. $r.timed_out .. \" \" .. $r.exit_code)\n",
    )
    .await;
    assert_eq!(out, "false true nil\n");
}

#[tokio::test]
async fn external_signal_death_reports_signal() {
    let out = run_ok(
        "$r = run_argv([\"sh\", \"-c\", \"kill -TERM $$\"])\nprint($r.ok .. \" \" .. $r.exit_code .. \" \" .. $r.signal)\n",
    )
    .await;
    assert_eq!(out, "false nil 15\n");
}

// ── max_output caps ─────────────────────────────────────────────────

#[tokio::test]
async fn max_output_truncates_without_killing_child() {
    let out = run_ok(
        "$r = run_argv([\"sh\", \"-c\", \"head -c 100000 /dev/zero | tr '\\\\0' 'x'; echo done >&2\"], {max_output: 1000})\nprint(byte_length($r.stdout) .. \" \" .. $r.stdout_truncated .. \" \" .. $r.stderr_truncated .. \" \" .. $r.ok .. \" \" .. $r.exit_code)\n",
    )
    .await;
    // Retained exactly the cap; child ran to completion (exit 0) but
    // truncation is flagged; stderr under cap is untouched.
    assert_eq!(out, "1000 true false true 0\n");
}

// ── spawn failure = value, not raise ────────────────────────────────

#[tokio::test]
async fn spawn_failure_is_encoded_not_raised() {
    let out = run_ok(
        "$r = run_argv([\"/no/such/binary/exists\"])\nprint($r.ok .. \" \" .. $r.exit_code .. \" \" .. $r.error_code)\nprint(pos(\"spawn\", $r.error) > 0)\n",
    )
    .await;
    assert_eq!(out, "false nil PROCESS_SPAWN\ntrue\n");
}

// ── run_argv_must ───────────────────────────────────────────────────

#[tokio::test]
async fn must_returns_stdout_on_ok() {
    let out = run_ok("print(run_argv_must([\"printf\", \"hi\"]))\n").await;
    assert_eq!(out, "hi\n");
}

#[tokio::test]
async fn must_supports_file_routing_and_raises_stdio_failure() {
    let dir = TestDir::new("must-routing");
    let output = dir.path("output.txt");
    let bad = dir.path("missing/output.txt");
    let source = format!(
        "$v = run_argv_must([\"printf\", \"written\"], {{stdout: {{file: {}}}}})\n\
         print(byte_length($v))\n\
         try\n\
           run_argv_must([\"true\"], {{stdout: {{file: {}}}}})\n\
         catch $m, $e\n\
           print($e.code .. \" \" .. $e.details.result.error_code)\n\
         end\n",
        mix_path(&output),
        mix_path(&bad),
    );
    assert_eq!(run_ok(&source).await, "0\nPROCESS_STDIO PROCESS_STDIO\n");
    assert_eq!(std::fs::read(&output).unwrap(), b"written");
}

#[tokio::test]
async fn must_raises_exit_nonzero_with_result_details() {
    let out = run_ok(
        "try\n  run_argv_must([\"sh\", \"-c\", \"echo bad >&2; exit 7\"])\ncatch $m, $e\n  print($e.code)\n  print($e.details.result.exit_code)\n  print($e.details.result.stderr)\n  print(pos(\"exit_code=7\", $m) > 0)\nend\n",
    )
    .await;
    assert_eq!(out, "PROCESS_EXIT_NONZERO\n7\nbad\n\ntrue\n");
}

#[tokio::test]
async fn must_raises_timeout_and_spawn_codes() {
    let out = run_ok(
        "try\n  run_argv_must([\"sleep\", \"5\"], {timeout: 0.2})\ncatch $m, $e\n  print($e.code)\nend\ntry\n  run_argv_must([\"/no/such/bin\"])\ncatch $m, $e\n  print($e.code)\nend\n",
    )
    .await;
    assert_eq!(out, "PROCESS_TIMEOUT\nPROCESS_SPAWN\n");
}

#[tokio::test]
async fn must_raises_output_limit_even_on_clean_exit() {
    let out = run_ok(
        "try\n  run_argv_must([\"sh\", \"-c\", \"head -c 5000 /dev/zero | tr '\\\\0' 'x'\"], {max_output: 100})\ncatch $m, $e\n  print($e.code)\n  print($e.details.result.stdout_truncated)\nend\n",
    )
    .await;
    assert_eq!(out, "PROCESS_OUTPUT_LIMIT\ntrue\n");
}

#[tokio::test]
async fn deadline_holds_when_descendant_outlives_leader() {
    // codex C3 review MAJOR: `sh -c "sleep 1 &"` exits immediately but
    // the background sleep inherits our pipe write ends; the drain
    // joins used to block ~1s past the deadline. The post-exit
    // deadline loop now SIGKILLs the group at the deadline.
    let out = run_ok(
        "$r = run_argv([\"sh\", \"-c\", \"sleep 1 &\"], {timeout: 0.2})\nprint($r.timed_out .. \" \" .. ($r.duration_ms < 800))\n",
    )
    .await;
    assert_eq!(out, "true true\n");
}

// ── duration ────────────────────────────────────────────────────────

#[tokio::test]
async fn duration_ms_is_populated() {
    let out = run_ok("$r = run_argv([\"sleep\", \"0.1\"])\nprint($r.duration_ms >= 90)\n").await;
    assert_eq!(out, "true\n");
}

/// A stdio setup failure must leave every target file exactly as it found it.
/// Routes are opened without `O_TRUNC` and truncated only after the whole set
/// has opened, so a bad stderr route cannot destroy the stdout route's existing
/// file on its way to reporting `PROCESS_STDIO`.
#[tokio::test]
async fn stdio_setup_failure_does_not_truncate_an_earlier_route() {
    let dir = TestDir::new("stdio-failure-truncate");
    let keep = dir.path("keep");
    std::fs::write(&keep, b"PRECIOUS").expect("seed the existing output file");
    let bad_output = dir.path("missing-parent/err");
    let source = format!(
        "$r = run_argv([\"printf\", \"clobber\"], {{stdout: {{file: {}}}, stderr: {{file: {}}}}})\n\
         print($r.ok .. \" \" .. $r.error_code)\n",
        mix_path(&keep),
        mix_path(&bad_output),
    );
    assert_eq!(run_ok(&source).await, "false PROCESS_STDIO\n");
    assert_eq!(
        std::fs::read(&keep).unwrap(),
        b"PRECIOUS",
        "the stdout route's file was truncated by a later route's failure"
    );
}

#[test]
fn errors_manual_does_not_claim_stdio_truncation_is_transactional() {
    let manual = include_str!("../../../../docs/mix/errors.md");
    assert!(
        manual.contains("truncation itself is not transactional"),
        "errors.md must qualify the route-open-before-truncate guarantee"
    );
    assert!(
        !manual.contains("no file route is truncated — routes"),
        "errors.md still makes the false transactional guarantee"
    );
    assert!(
        !manual.contains("this code means no stage ran and no route file was truncated"),
        "errors.md still overclaims the PIPELINE_STDIO guarantee"
    );
}
