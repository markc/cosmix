//! `finally` and exit semantics that need the real binary: the evaluator
//! unwinds `exit()` through cleanup, then each binary entrypoint must consume
//! the control signal as the exact requested process status.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

fn run_args(args: &[&str], stdin: Option<&str>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mix"))
        .args(args)
        .env("MIX_STATS", "off")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mix");
    if let Some(input) = stdin {
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(input.as_bytes())
            .expect("write stdin");
    }
    child.wait_with_output().expect("wait for mix")
}

fn run(body: &str) -> (Option<i32>, String, String) {
    let out = run_args(&["-c", body], None);
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn temp_path(label: &str) -> PathBuf {
    let serial = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "mix-finally-{label}-{}-{serial}.mix",
        std::process::id()
    ))
}

#[test]
fn exit_runs_finally_and_preserves_status() {
    let (code, stdout, stderr) =
        run("try\n  print(\"body\")\n  exit(3)\nfinally\n  print(\"FINALLY RAN\")\nend\n");
    assert_eq!(code, Some(3));
    assert_eq!(stdout, "body\nFINALLY RAN\n");
    assert!(stderr.is_empty(), "stderr:\n{stderr}");
}

#[test]
fn exit_zero_is_clean_from_c_file_and_stdin() {
    let (code, stdout, stderr) = run("exit(0)\n");
    assert_eq!(code, Some(0));
    assert!(stdout.is_empty(), "stdout:\n{stdout}");
    assert!(stderr.is_empty(), "stderr:\n{stderr}");

    let path = temp_path("zero");
    fs::write(&path, "exit(0)\n").expect("write script");
    let path_arg = path.to_string_lossy().into_owned();
    let file_out = run_args(&[&path_arg], None);
    let _ = fs::remove_file(&path);
    assert_eq!(file_out.status.code(), Some(0));
    assert!(file_out.stdout.is_empty());
    assert!(file_out.stderr.is_empty());

    let stdin_out = run_args(&["-"], Some("exit(0)\n"));
    assert_eq!(stdin_out.status.code(), Some(0));
    assert!(stdin_out.stdout.is_empty());
    assert!(stdin_out.stderr.is_empty());
}

#[test]
fn exit_request_is_consumed_by_repl_boundary() {
    let out = run_args(&["-i"], Some("exit(4)\n"));
    assert_eq!(out.status.code(), Some(4));
    assert!(out.stderr.is_empty());
}

#[test]
fn panic_skips_finally() {
    let (_code, stdout, _stderr) =
        run("try\n  print(\"before\")\n  panic(\"boom\")\nfinally\n  print(\"FIN\")\nend\n");
    assert!(stdout.contains("before"), "body ran: {stdout}");
    assert!(
        !stdout.contains("FIN"),
        "finally must NOT run after panic(): {stdout}"
    );
}

#[test]
fn finally_runs_on_ordinary_error_through_binary() {
    let (code, stdout, _stderr) =
        run("try\n  print(\"before\")\n  die(\"x\")\nfinally\n  print(\"FIN\")\nend\n");
    assert_eq!(code, Some(1));
    assert!(
        stdout.contains("before") && stdout.contains("FIN"),
        "{stdout}"
    );
}
