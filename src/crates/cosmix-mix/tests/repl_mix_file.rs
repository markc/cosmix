//! The REPL's `mix` dispatcher shares meta names with the one-shot CLI, then
//! falls back to a script file. Exercise the real process boundary: this is
//! where clean scope, positional argv, stdout/stderr routing, and meta-name
//! precedence meet.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let serial = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("mix-repl-{label}-{}-{serial}", std::process::id()));
        fs::create_dir(&path).expect("create temporary test directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run_repl(cwd: &Path, input: &str) -> Output {
    let home = cwd.join("home");
    fs::create_dir(&home).expect("create isolated HOME");

    let mut child = Command::new(env!("CARGO_BIN_EXE_mix"))
        .arg("-i")
        .current_dir(cwd)
        .env("HOME", &home)
        .env("MIX_STATS", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn Mix REPL");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(input.as_bytes())
        .expect("write REPL input");
    child.wait_with_output().expect("wait for Mix REPL")
}

fn output_text(output: &Output) -> (String, String) {
    assert!(
        output.status.success(),
        "REPL exited {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn trailing_concat_drives_the_real_repl_accumulator() {
    let dir = TempDir::new("concat-continuation");
    let input = "$s = \"left\" ..\n\"right\"\nprint($s)\n";

    let (stdout, stderr) = output_text(&run_repl(dir.path(), input));
    assert!(stdout.contains("leftright"), "stdout:\n{stdout}");
    assert!(
        !stderr.contains("Parse error"),
        "the first line must retain the REPL buffer, not error:\n{stderr}"
    );
}

#[test]
fn mix_file_runs_with_positionals_and_returns_to_repl() {
    let dir = TempDir::new("file");
    let script = dir.path().join("hello.mix");
    fs::write(&script, "print(\"ran: \" .. $1)\n").expect("write script");
    let input = format!(
        "mix {} alpha\nprint(\"repl survived\")\n",
        script.to_string_lossy()
    );

    let (stdout, stderr) = output_text(&run_repl(dir.path(), &input));
    assert!(stdout.contains("ran: alpha"), "stdout:\n{stdout}");
    assert!(stdout.contains("repl survived"), "stdout:\n{stdout}");
    assert!(!stdout.contains("mix meta-commands:"), "stdout:\n{stdout}");
    assert!(stderr.is_empty(), "stderr:\n{stderr}");
}

#[test]
fn meta_name_wins_over_same_named_file_in_cwd() {
    let dir = TempDir::new("precedence");
    fs::write(dir.path().join("status"), "print(\"wrong script\")\n")
        .expect("write shadowed script");

    let (stdout, stderr) = output_text(&run_repl(dir.path(), "mix status\n"));
    assert!(
        stdout.contains(&format!("mix {}", env!("CARGO_PKG_VERSION"))),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("pid:"), "stdout:\n{stdout}");
    assert!(!stdout.contains("wrong script"), "stdout:\n{stdout}");
    assert!(stderr.is_empty(), "stderr:\n{stderr}");
}

#[test]
fn unknown_non_file_names_token_then_shows_help() {
    let dir = TempDir::new("unknown");

    let (stdout, stderr) = output_text(&run_repl(dir.path(), "mix not-a-command\n"));
    assert!(stdout.contains("mix meta-commands:"), "stdout:\n{stdout}");
    assert!(
        stderr.contains("mix: unknown meta-command 'not-a-command' (and no such file)"),
        "stderr:\n{stderr}"
    );
}

#[test]
fn non_zero_script_status_is_reported_and_repl_survives() {
    let dir = TempDir::new("non-zero");
    let script = dir.path().join("fail.mix");
    fs::write(&script, "exit(7)\n").expect("write failing script");
    let input = format!(
        "mix {}\nprint(\"after failure\")\n",
        script.to_string_lossy()
    );

    let (stdout, stderr) = output_text(&run_repl(dir.path(), &input));
    assert!(stdout.contains("after failure"), "stdout:\n{stdout}");
    assert!(
        stderr.contains("exited with exit status: 7"),
        "stderr:\n{stderr}"
    );
}
