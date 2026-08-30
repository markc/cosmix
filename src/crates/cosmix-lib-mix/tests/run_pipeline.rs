//! run_pipeline / run_pipeline_must: shell-free argv pipelines with one
//! deadline and per-stage outcomes.

#![cfg(unix)]

use cosmix_mix::error::MixError;
use cosmix_mix::evaluator::{Evaluator, SharedBuf};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;

async fn run(source: &str) -> Result<String, MixError> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize()?;
    let mut parser = Parser::new(tokens, source);
    let statements = parser.parse_program()?;
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut evaluator = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    evaluator.execute(&statements).await?;
    Ok(stdout.to_string_lossy())
}

async fn run_ok(source: &str) -> String {
    run(source)
        .await
        .unwrap_or_else(|error| panic!("script should succeed, got: {error}"))
}

#[tokio::test]
async fn pipeline_two_stage_connects_stdout_to_stdin() {
    let output = run_ok(
        "$r = run_pipeline([[\"printf\", \"alpha\"], [\"tr\", \"a-z\", \"A-Z\"]])\n\
         print($r.ok .. \" \" .. $r.exit_code .. \" \" .. $r.stdout)\n\
         print(length($r.stages) .. \" \" .. $r.stages[0].ok .. \" \" .. $r.stages[1].ok)\n",
    )
    .await;
    assert_eq!(output, "true 0 ALPHA\n2 true true\n");
}

#[tokio::test]
async fn pipeline_three_stage_reports_every_stage() {
    let output = run_ok(
        "$r = run_pipeline([[\"printf\", \"c\\nb\\na\\n\"], [\"sort\"], [\"head\", \"-n\", \"2\"]])\n\
         print($r.stdout)\n\
         print(length($r.stages) .. \" \" .. $r.stages[0].exit_code .. \" \" .. $r.stages[1].exit_code .. \" \" .. $r.stages[2].exit_code)\n",
    )
    .await;
    assert_eq!(output, "a\nb\n\n3 0 0 0\n");
}

#[tokio::test]
async fn pipeline_middle_stage_failure_makes_overall_false() {
    let output = run_ok(
        "$r = run_pipeline([[\"printf\", \"payload\"], [\"sh\", \"-c\", \"cat >/dev/null; exit 7\"], [\"cat\"]])\n\
         print($r.ok .. \" \" .. $r.exit_code)\n\
         print($r.stages[0].ok .. \" \" .. $r.stages[1].ok .. \" \" .. $r.stages[1].exit_code .. \" \" .. $r.stages[2].ok)\n",
    )
    .await;
    assert_eq!(output, "false 0\ntrue false 7 true\n");
}

#[tokio::test]
async fn pipeline_last_stage_failure_is_reported() {
    let output = run_ok(
        "$r = run_pipeline([[\"printf\", \"payload\"], [\"sh\", \"-c\", \"cat >/dev/null; printf final-error >&2; exit 9\"]])\n\
         print($r.ok .. \" \" .. $r.exit_code .. \" \" .. $r.signal)\n\
         print($r.stages[1].ok .. \" \" .. $r.stages[1].exit_code .. \" \" .. $r.stages[1].stderr)\n",
    )
    .await;
    assert_eq!(output, "false 9 nil\nfalse 9 final-error\n");
}

#[tokio::test]
async fn pipeline_accepts_nonfinal_sigpipe_only_when_asked() {
    // allow_signal defaults FALSE: the `yes | head -1` idiom is a signal death
    // like any other unless the caller opts in. Mix cannot tell a benign
    // early-reader SIGPIPE from a stage that killed itself for a fatal reason,
    // so the honest answer is the default. See the opt-out test below for the
    // failure this default refuses to conceal.
    let defaulted = run_ok(
        "$r = run_pipeline([[\"yes\"], [\"head\", \"-n\", \"1\"]])\n\
         print($r.ok .. \" \" .. $r.stdout)\n\
         print($r.stages[0].ok .. \" \" .. $r.stages[0].signal .. \" \" .. $r.stages[0].accepted_signal .. \" \" .. $r.stages[1].ok)\n",
    )
    .await;
    assert_eq!(defaulted, "false y\n\nfalse 13 false true\n");

    // Opting in restores the idiom, and only then.
    let opted_in = run_ok(
        "$r = run_pipeline([[\"yes\"], [\"head\", \"-n\", \"1\"]], {allow_signal: true})\n\
         print($r.ok .. \" \" .. $r.stdout)\n\
         print($r.stages[0].ok .. \" \" .. $r.stages[0].signal .. \" \" .. $r.stages[0].accepted_signal .. \" \" .. $r.stages[1].ok)\n",
    )
    .await;
    assert_eq!(opted_in, "true y\n\ntrue 13 true true\n");
}

/// The case that decided the default: a middle stage announces a fatal
/// condition and kills itself with SIGPIPE while every downstream stage exits
/// 0. Under the old `allow_signal: true` default this returned ok:true and
/// run_pipeline_must returned SUCCESS.
#[tokio::test]
async fn a_stage_that_kills_itself_with_sigpipe_is_not_success_by_default() {
    let output = run_ok(
        "$r = run_pipeline([[\"yes\"], [\"sh\", \"-c\", \"printf fatal >&2; kill -PIPE $$\"], [\"true\"]])\n\
         print($r.ok .. \" \" .. $r.stages[1].ok .. \" \" .. $r.stages[1].signal)\n",
    )
    .await;
    assert_eq!(output, "false false 13\n");

    // ...and the raising twin must raise rather than hand back stdout.
    let must = run_ok(
        "try\n\
         run_pipeline_must([[\"yes\"], [\"sh\", \"-c\", \"printf fatal >&2; kill -PIPE $$\"], [\"true\"]])\n\
         print(\"RETURNED_SUCCESS\")\n\
         catch $m, $e\n\
         print($e.code)\n\
         end\n",
    )
    .await;
    assert_ne!(
        must.trim(),
        "RETURNED_SUCCESS",
        "run_pipeline_must must not report success for a fatal signalled stage"
    );
    assert!(must.starts_with("PIPELINE_"), "got: {must:?}");
}

#[tokio::test]
async fn pipeline_does_not_mask_sigpipe_when_downstream_fails() {
    let output = run_ok(
        "$r = run_pipeline([[\"yes\"], [\"head\", \"-n\", \"1\"], [\"sh\", \"-c\", \"cat >/dev/null; exit 4\"]])\n\
         print($r.ok .. \" \" .. $r.exit_code)\n\
         print($r.stages[0].signal .. \" \" .. $r.stages[0].accepted_signal .. \" \" .. $r.stages[0].ok .. \" \" .. $r.stages[2].ok)\n",
    )
    .await;
    assert_eq!(output, "false 4\n13 false false false\n");
}

#[tokio::test]
async fn pipeline_allow_signal_false_rejects_sigpipe() {
    let output = run_ok(
        "$r = run_pipeline([[\"yes\"], [\"head\", \"-n\", \"1\"]], {allow_signal: false})\n\
         print($r.ok .. \" \" .. $r.stages[0].ok .. \" \" .. $r.stages[0].signal .. \" \" .. $r.stages[0].accepted_signal)\n",
    )
    .await;
    assert_eq!(output, "false false 13 false\n");
}

#[tokio::test]
async fn pipeline_timeout_kills_every_stage_under_one_deadline() {
    let output = run_ok(
        "$r = run_pipeline([[\"sleep\", \"5\"], [\"cat\"]], {timeout: 0.2})\n\
         print($r.ok .. \" \" .. $r.timed_out .. \" \" .. $r.interrupted .. \" \" .. ($r.duration_ms < 1500))\n\
         print($r.stages[0].signal .. \" \" .. $r.stages[1].signal)\n",
    )
    .await;
    assert_eq!(output, "false true false true\n9 9\n");
}

#[tokio::test]
async fn pipeline_one_stage_matches_run_argv_fields() {
    let output = run_ok(
        "$a = run_argv([\"sh\", \"-c\", \"printf out; printf err >&2; exit 3\"], {max_output: 99})\n\
         $p = run_pipeline([{argv: [\"sh\", \"-c\", \"printf out; printf err >&2; exit 3\"]}], {max_output: 99})\n\
         print(($a.ok == $p.ok) .. \" \" .. ($a.exit_code == $p.exit_code) .. \" \" .. ($a.stdout == $p.stdout) .. \" \" .. ($a.stderr == $p.stderr))\n\
         print(($a.timed_out == $p.timed_out) .. \" \" .. ($a.interrupted == $p.interrupted) .. \" \" .. ($a.signal == $p.signal) .. \" \" .. ($a.stdout_truncated == $p.stdout_truncated) .. \" \" .. ($a.stderr_truncated == $p.stderr_truncated))\n",
    )
    .await;
    assert_eq!(output, "true true true true\ntrue true true true true\n");
}

#[tokio::test]
async fn pipeline_stage_cwd_env_and_clear_env_are_independent() {
    let output = run_ok(
        "$r = run_pipeline([{argv: [\"/bin/sh\", \"-c\", \"printf %s:%s \\\"$FIRST\\\" \\\"$PWD\\\"\"], cwd: \"/tmp\", env: {FIRST: \"one\"}}, {argv: [\"/bin/sh\", \"-c\", \"read line; printf %s:%s:%s \\\"$line\\\" \\\"$SECOND\\\" \\\"$PWD\\\"\"], cwd: \"/\", clear_env: true, env: {SECOND: \"two\"}}])\n\
         print($r.ok .. \" \" .. $r.stdout)\n",
    )
    .await;
    assert_eq!(output, "true one:/tmp:two:/\n");
}

#[tokio::test]
async fn pipeline_must_returns_stdout_and_raises_pipeline_details() {
    let output = run_ok(
        "print(run_pipeline_must([[\"printf\", \"ok\"], [\"cat\"]]))\n\
         try\n\
           run_pipeline_must([[\"printf\", \"x\"], [\"sh\", \"-c\", \"cat >/dev/null; exit 6\"]])\n\
         catch $message, $error\n\
           print($error.code .. \" \" .. $error.details.result.exit_code .. \" \" .. $error.details.result.stages[1].exit_code)\n\
         end\n",
    )
    .await;
    assert_eq!(output, "ok\nPIPELINE_EXIT_NONZERO 6 6\n");
}

#[tokio::test]
async fn pipeline_validation_uses_type_and_option_codes() {
    let output = run_ok(
        "try\n\
           run_pipeline([])\n\
         catch $message, $error\n\
           print($error.code)\n\
         end\n\
         try\n\
           run_pipeline([[\"true\"], {argv: [\"cat\"], stdin: \"bad\"}])\n\
         catch $message, $error\n\
           print($error.code)\n\
         end\n\
         try\n\
           run_pipeline([[\"true\"]], {allow_signal: 1})\n\
         catch $message, $error\n\
           print($error.code)\n\
         end\n",
    )
    .await;
    assert_eq!(output, "TYPE_MISMATCH\nOPTION_INVALID\nOPTION_INVALID\n");
}

/// The lifecycle codes split by builtin: `run_pipeline` returns
/// `PIPELINE_STDIO`/`SPAWN`/`IO`/`INTERNAL` in the value and leaves
/// `error_code` nil for ordinary non-zero exits, signals, timeouts and
/// truncation — only `run_pipeline_must` turns those into raises. This is the
/// claim `docs/mix/errors.md` and `docs/mix/system.md` both make; it fails if
/// either the value ever carries `PIPELINE_EXIT_NONZERO`-class codes or the
/// setup failure stops being encoded in the value.
#[tokio::test]
async fn pipeline_value_carries_only_setup_codes_not_the_must_only_codes() {
    let output = run_ok(
        "$a = run_pipeline([[\"printf\", \"x\"], [\"sh\", \"-c\", \"cat >/dev/null; exit 7\"], [\"cat\"]])\n\
         print(\"nonzero \" .. $a.ok .. \" \" .. $a.error_code .. \" \" .. $a.error)\n\
         $b = run_pipeline([[\"sleep\", \"5\"], [\"cat\"]], {timeout: 0.2})\n\
         print(\"timeout \" .. $b.ok .. \" \" .. $b.timed_out .. \" \" .. $b.error_code)\n\
         $c = run_pipeline([[\"yes\"], [\"head\", \"-n\", \"1\"]], {allow_signal: false})\n\
         print(\"signal \" .. $c.ok .. \" \" .. $c.error_code)\n\
         $d = run_pipeline([[\"sh\", \"-c\", \"printf aaaaaaaaaa\"], [\"cat\"]], {max_output: 3})\n\
         print(\"truncated \" .. $d.ok .. \" \" .. $d.stdout_truncated .. \" \" .. $d.error_code)\n",
    )
    .await;
    assert_eq!(
        output,
        "nonzero false nil nil\ntimeout false true nil\nsignal false nil\ntruncated true true nil\n"
    );
}

/// The other half of the split: a setup failure IS encoded in the value with
/// `PIPELINE_STDIO` and a nil `exit_code`, and never raises.
#[tokio::test]
async fn pipeline_stdio_setup_failure_is_returned_not_raised() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_nanos();
    // Parent directory deliberately does not exist, so opening the route fails.
    let missing = std::env::temp_dir()
        .join(format!(
            "mix-pipeline-absent-{}-{nonce}",
            std::process::id()
        ))
        .join("out");
    assert!(
        !missing.parent().expect("parent").exists(),
        "the setup-failure test needs a genuinely absent parent directory"
    );
    let source = format!(
        "$r = run_pipeline([[\"printf\", \"x\"], {{argv: [\"cat\"], stdout: {{file: \"{}\"}}}}])\n\
         print($r.ok .. \" \" .. $r.error_code .. \" \" .. $r.exit_code)\n",
        missing.display()
    );
    let output = run_ok(&source).await;
    assert_eq!(output, "false PIPELINE_STDIO nil\n");
}

/// `run_pipeline_must` is where those same outcomes become raises, with the
/// full pipeline_result under `$err.details.result`.
#[tokio::test]
async fn pipeline_must_raises_the_codes_the_value_never_carries() {
    let output = run_ok(
        "try\n\
           run_pipeline_must([[\"sleep\", \"5\"], [\"cat\"]], {timeout: 0.2})\n\
         catch $message, $error\n\
           print($error.code .. \" \" .. $error.details.result.timed_out)\n\
         end\n\
         try\n\
           run_pipeline_must([[\"yes\"], [\"head\", \"-n\", \"1\"]], {allow_signal: false})\n\
         catch $message, $error\n\
           print($error.code .. \" \" .. $error.details.result.stages[0].signal)\n\
         end\n\
         try\n\
           run_pipeline_must([[\"sh\", \"-c\", \"printf aaaaaaaaaa\"], [\"cat\"]], {max_output: 3})\n\
         catch $message, $error\n\
           print($error.code .. \" \" .. $error.details.result.stdout_truncated)\n\
         end\n",
    )
    .await;
    assert_eq!(
        output,
        "PIPELINE_TIMEOUT true\nPIPELINE_SIGNAL 13\nPIPELINE_OUTPUT_LIMIT true\n"
    );
}

/// A pipeline opens every stage's file routes before it spawns ANY stage, and
/// truncates non-append routes only after the whole set has opened. A bad route
/// on a later stage therefore leaves an earlier stage's route file intact and
/// leaves the pipeline entirely unrun.
#[tokio::test]
async fn pipeline_stdio_setup_failure_runs_nothing_and_truncates_nothing() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_nanos();
    let dir =
        std::env::temp_dir().join(format!("mix-pipeline-setup-{}-{nonce}", std::process::id()));
    std::fs::create_dir(&dir).expect("create test directory");
    let keep = dir.join("keep");
    let witness = dir.join("stage0-ran");
    let bad = dir.join("missing-parent").join("err");
    std::fs::write(&keep, b"PRECIOUS").expect("seed the existing stage route file");

    let source = format!(
        "$r = run_pipeline([\
           {{argv: [\"sh\", \"-c\", \"touch {}; printf x\"], stderr: {{file: \"{}\"}}}},\
           {{argv: [\"cat\"], stderr: {{file: \"{}\"}}}}\
         ])\n\
         print($r.ok .. \" \" .. $r.error_code .. \" \" .. $r.exit_code)\n",
        witness.display(),
        keep.display(),
        bad.display(),
    );
    let output = run_ok(&source).await;

    let kept = std::fs::read(&keep).unwrap();
    let ran = witness.exists();
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(output, "false PIPELINE_STDIO nil\n");
    assert_eq!(
        kept, b"PRECIOUS",
        "an earlier stage's route file was truncated by a later stage's failure"
    );
    assert!(
        !ran,
        "a stage was spawned after pipeline stdio setup failed"
    );
}

/// A later spawn failure is the unavoidable partial-run case: it must remain
/// PIPELINE_SPAWN (never masquerade as PIPELINE_STDIO) and disclose the stages
/// that were actually started.
#[tokio::test]
async fn pipeline_spawn_failure_reports_already_started_stages() {
    let output = run_ok(
        "$r = run_pipeline([[\"true\"], [\"/definitely/no/such/mix-command\"]])\n\
         print($r.error_code .. \" \" .. length($r.stages) .. \" \" .. $r.stages[0].index .. \" \" .. $r.stderr_truncated .. \" \" .. $r.stages[0].stderr_truncated)\n",
    )
    .await;
    assert_eq!(output, "PIPELINE_SPAWN 1 0 true true\n");
}
