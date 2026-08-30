//! Structured errors (0.29.0, decision record D6): code-carrying
//! `MixError::Structured`, `raise()`, the optional second `catch`
//! binding, and traceback frame snapshots.
//!
//! Pins: `catch $m` alone keeps the pre-0.29 message-string contract;
//! the second binding sees `{code, message, details, cause, frames}`;
//! legacy RuntimeError/DieError surface as `RUNTIME_ERROR`/`USER_DIE`;
//! frames are outermost-to-innermost with the failure site last.

use cosmix_mix::error::MixError;
use cosmix_mix::evaluator::{Evaluator, SharedBuf};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;

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
    run(source).await.expect("script should succeed")
}

// ── raise() + second catch binding ─────────────────────────────────

#[tokio::test]
async fn raise_carries_code_message_details_to_second_binding() {
    let out = run_ok(
        "try\n  raise(\"VALIDATION_REQUIRED\", \"node is required\", {field: \"node\"})\ncatch $m, $e\n  print($m)\n  print($e.code)\n  print($e.details.field)\n  print(type($e.frames))\nend\n",
    )
    .await;
    assert_eq!(out, "node is required\nVALIDATION_REQUIRED\nnode\nlist\n");
}

#[tokio::test]
async fn single_binding_contract_unchanged() {
    let out =
        run_ok("try\n  raise(\"MY_CODE\", \"boom\")\ncatch $m\n  print(\"caught: \" .. $m)\nend\n")
            .await;
    assert_eq!(out, "caught: boom\n");
}

#[tokio::test]
async fn legacy_errors_get_runtime_error_and_user_die_codes() {
    let out = run_ok(
        "try\n  $x = $undefined_thing\ncatch $m, $e\n  print($e.code)\nend\ntry\n  die(\"gone\")\ncatch $m, $e\n  print($e.code)\n  print($e.message)\nend\n",
    )
    .await;
    assert_eq!(out, "NAME_UNDEFINED\nUSER_DIE\ngone\n");
}

#[tokio::test]
async fn raise_rejects_bad_code_shape() {
    let out = run_ok(
        "try\n  raise(\"not_upper\", \"x\")\ncatch $m, $e\n  print($e.code)\n  print(pos(\"invalid error code\", $m) > 0)\nend\n",
    )
    .await;
    assert_eq!(out, "RUNTIME_ERROR\ntrue\n");
}

#[tokio::test]
async fn undefined_function_code() {
    let out = run_ok("try\n  no_such_function_xyz(1)\ncatch $m, $e\n  print($e.code)\nend\n").await;
    assert_eq!(out, "FUNCTION_UNDEFINED\n");
}

// ── traceback frames ────────────────────────────────────────────────

#[tokio::test]
async fn frames_outermost_to_innermost_with_failure_site_last() {
    let src = "\
function inner()
  raise(\"DEEP_FAIL\", \"deep\")
end
function outer()
  inner()
end
try
  outer()
catch $m, $e
  for each $f in $e.frames
    print($f.kind .. \":\" .. $f.function)
  end
end
";
    let out = run_ok(src).await;
    assert_eq!(
        out,
        "script:<main>\nscript:outer\nscript:inner\nbuiltin:raise\n"
    );
}

#[tokio::test]
async fn frame_lines_follow_python_convention() {
    // <main> shows the line of the outermost call; each fn frame shows
    // the line within it where the next call/failure happened.
    let src = "\
function inner()
  die(\"x\")
end
function outer()
  inner()
end
try
  outer()
catch $m, $e
  for each $f in $e.frames
    print($f.function .. \"@\" .. $f.line)
  end
end
";
    let out = run_ok(src).await;
    assert_eq!(out, "<main>@8\nouter@5\ninner@2\n");
}

#[tokio::test]
async fn uncaught_structured_error_renders_traceback() {
    let src = "\
function f()
  raise(\"KABOOM\", \"it broke\")
end
f()
";
    let err = run(src).await.expect_err("should fail");
    let rendered = err.render_traceback();
    assert!(
        rendered.starts_with("Traceback (most recent call last):"),
        "got: {rendered}"
    );
    assert!(rendered.contains("  at <main> (line 4)"), "got: {rendered}");
    assert!(rendered.contains("  at f (line 2)"), "got: {rendered}");
    assert!(rendered.ends_with("KABOOM: it broke"), "got: {rendered}");
    // Display stays legacy single-line for compatibility.
    assert!(err.to_string().contains("Runtime error"), "got: {}", err);
}

#[tokio::test]
async fn caught_error_does_not_poison_later_frames() {
    // After a catch, the frame stack must be back to baseline: a later
    // error must not carry frames from the earlier, already-handled one.
    let src = "\
function boom()
  die(\"first\")
end
try
  boom()
catch $m
end
function calm()
  die(\"second\")
end
try
  calm()
catch $m, $e
  print(length($e.frames))
  for each $f in $e.frames
    print($f.function)
  end
end
";
    let out = run_ok(src).await;
    assert_eq!(out, "2\n<main>\ncalm\n");
}

#[tokio::test]
async fn successful_nested_call_does_not_corrupt_later_frame_lines() {
    // codex C2 review MAJOR 1: after ok() returned, ctx.current_line
    // used to stay at ok()'s internal line, so the boom() frame (and
    // outer's displayed line) pointed inside ok(). call_function now
    // restores the caller line on every exit.
    let src = "\
function ok()
  return 1
end
function boom()
  die(\"x\")
end
function outer()
  return ok() + boom()
end
try
  outer()
catch $m, $e
  for each $f in $e.frames
    print($f.function .. \"@\" .. $f.line)
  end
end
";
    let out = run_ok(src).await;
    assert_eq!(out, "<main>@11\nouter@8\nboom@5\n");
}

#[tokio::test]
async fn module_frames_carry_the_module_file() {
    // codex C2 review MAJOR 2: frames used to stamp every entry with
    // the single current_file. Functions now carry def_file and the
    // frame snapshot records per-frame files.
    let dir = std::env::temp_dir().join(format!("mix-structerr-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let module = dir.join("failmod.mix");
    std::fs::write(&module, "function explode()\n  die(\"module boom\")\nend\n").unwrap();
    let src = format!(
        "$m = require(\"{}\")\ntry\n  $m.explode()\ncatch $msg, $e\n  for each $f in $e.frames\n    print($f.function .. \"|\" .. $f.file)\n  end\nend\n",
        module.display()
    );
    let out = run_ok(&src).await;
    std::fs::remove_dir_all(&dir).ok();
    let lines: Vec<&str> = out.lines().collect();
    // <main> runs with no file in this harness; the module function's
    // frame must carry the module path.
    assert_eq!(lines[0], "<main>|nil", "got: {out}");
    assert!(
        lines[1].starts_with("explode|") && lines[1].contains("failmod.mix"),
        "got: {out}"
    );
}

#[tokio::test]
async fn details_default_nil_and_cause_nil() {
    let out = run_ok(
        "try\n  raise(\"A_B\", \"m\")\ncatch $m, $e\n  print($e.details)\n  print($e.cause)\nend\n",
    )
    .await;
    assert_eq!(out, "nil\nnil\n");
}
