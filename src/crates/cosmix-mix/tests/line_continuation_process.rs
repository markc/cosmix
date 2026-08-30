use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let serial = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "mix-line-continuation-{}-{serial}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create temporary test directory");
        Self(path)
    }

    fn write(&self, name: &str, source: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, source).expect("write fixture");
        path
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

fn mix(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mix"))
        .args(args)
        .env("MIX_STATS", "off")
        .output()
        .expect("run mix")
}

fn mix_in(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mix"))
        .args(args)
        .current_dir(cwd)
        .env("MIX_STATS", "off")
        .output()
        .expect("run mix in fixture directory")
}

fn repl(cwd: &Path, input: &str) -> Output {
    let home = cwd.join("home");
    fs::create_dir_all(&home).expect("create isolated HOME");
    let mut child = Command::new(env!("CARGO_BIN_EXE_mix"))
        .arg("-i")
        .current_dir(cwd)
        .env("HOME", home)
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
        .expect("drive Mix REPL accumulator");
    child.wait_with_output().expect("wait for Mix REPL")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "status={:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn dash_c_executes_an_embedded_newline_after_concat() {
    let output = mix(&["-c", "$s = \"a\" ..\n  \"b\"; print($s)"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "ab\n");
}

#[test]
fn dash_c_reports_eof_after_concat_as_incomplete() {
    let output = mix(&["-c", "$s = \"a\" .."]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("incomplete input"), "stderr: {stderr}");
    assert!(stderr.contains("expression"), "stderr: {stderr}");
}

#[test]
fn script_check_and_lint_accept_trailing_concat_continuation() {
    let dir = TempDir::new();
    let path = dir.write(
        "continued.mix",
        "$s = \"a\" .. -- comment\n\n  \"b\"\nprint($s)\n",
    );
    let path = path.to_str().expect("UTF-8 path");

    let run = mix(&[path]);
    assert!(
        run.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&run.stdout), "ab\n");

    let check = mix(&["--check", path]);
    assert!(
        check.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&check.stderr)
    );
    assert!(String::from_utf8_lossy(&check.stdout).contains(": OK"));

    let lint = mix(&["lint", path]);
    assert!(
        lint.status.success(),
        "stdout: {}",
        String::from_utf8_lossy(&lint.stdout)
    );
    assert!(String::from_utf8_lossy(&lint.stdout).contains("0 error(s)"));
}

#[test]
fn command_continuation_matches_across_file_dash_c_and_real_repl() {
    let dir = TempDir::new();
    let command = "/usr/bin/echo one \\\ntwo";
    let path = dir.write("command.mix", &format!("{command}\n"));
    let path = path.to_str().expect("UTF-8 path");

    let file = mix(&[path]);
    assert_success(&file);
    assert_eq!(String::from_utf8_lossy(&file.stdout), "one two\n");

    let dash_c = mix(&["-c", command]);
    assert_success(&dash_c);
    assert_eq!(String::from_utf8_lossy(&dash_c.stdout), "one two\n");

    let interactive = repl(dir.path(), &format!("{command}\nexit\n"));
    assert_success(&interactive);
    let stdout = String::from_utf8_lossy(&interactive.stdout);
    assert!(stdout.contains("one two\n"), "stdout:\n{stdout}");
    assert!(
        !stdout.contains("one \n"),
        "first half ran early:\n{stdout}"
    );
}

#[test]
fn joined_tight_hyphen_command_is_classified_before_mix_arithmetic() {
    let dir = TempDir::new();
    let path = dir.write("ls.mix", "ls -d \\\n/tmp\n");
    let output = mix(&[path.to_str().unwrap()]);
    assert_success(&output);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "/tmp\n");
}

#[test]
fn even_backslashes_and_quoted_backslashes_are_literal() {
    let dir = TempDir::new();
    let even = concat!("/usr/bin/printf '<%s>\\n' ", "\\\\");
    let output = mix(&["-c", even]);
    assert_success(&output);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "<\\>\n");
    let interactive = repl(dir.path(), &format!("{even}\nexit\n"));
    assert_success(&interactive);
    assert!(
        String::from_utf8_lossy(&interactive.stdout).contains("<\\>\n"),
        "stdout: {}",
        String::from_utf8_lossy(&interactive.stdout)
    );

    let output = mix(&["-c", "$p = \"C:\\\\path\"; print($p)"]);
    assert_success(&output);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "C:\\path\n");
    let output = mix(&["-c", "/usr/bin/printf '<%s>\\n' 'C:\\path'"]);
    assert_success(&output);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "<C:\\path>\n");
}

#[test]
fn eof_marker_is_incomplete_and_never_executes() {
    let dir = TempDir::new();
    let path = dir.write("eof.mix", "echo SHOULD_NOT_RUN \\");
    let file = mix(&[path.to_str().unwrap()]);
    assert_eq!(file.status.code(), Some(1));
    assert!(file.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&file.stderr).contains("another physical line"),
        "stderr: {}",
        String::from_utf8_lossy(&file.stderr)
    );

    let dash_c = mix(&["-c", "echo SHOULD_NOT_RUN \\"]);
    assert_eq!(dash_c.status.code(), Some(1));
    assert!(dash_c.stdout.is_empty());
    assert!(String::from_utf8_lossy(&dash_c.stderr).contains("incomplete input"));

    let interactive = repl(dir.path(), "echo SHOULD_NOT_RUN \\\n");
    assert_success(&interactive);
    assert!(!String::from_utf8_lossy(&interactive.stdout).contains("SHOULD_NOT_RUN"));
    assert!(
        String::from_utf8_lossy(&interactive.stderr).contains("another physical line"),
        "stderr: {}",
        String::from_utf8_lossy(&interactive.stderr)
    );
}

#[test]
fn heredoc_body_and_shell_argument_strings_keep_backslash_newlines() {
    let dir = TempDir::new();
    let heredoc = dir.write(
        "heredoc.mix",
        "$body = <<END\nalpha\\\nbeta\nEND\nprint($body)\n",
    );
    let output = mix(&[heredoc.to_str().unwrap()]);
    assert_success(&output);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "alpha\\\nbeta\n");

    let sh_string = dir.write("sh-string.mix", "sh \"printf '%s' 'a\\\nb'\"\n");
    let output = mix(&[sh_string.to_str().unwrap()]);
    assert_success(&output);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "a\\\nb");
}

#[test]
fn dotdot_paths_and_strict_data_keep_their_existing_meanings() {
    let dir = TempDir::new();
    let base = dir.path().join("base");
    let sub = base.join("sub");
    fs::create_dir_all(&sub).unwrap();
    let relscript = base.join("relscript.sh");
    fs::write(&relscript, "#!/bin/sh\nprintf 'RELSCRIPT_OK\\n'\n").unwrap();
    let mut permissions = fs::metadata(&relscript).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&relscript, permissions).unwrap();
    fs::write(base.join("f.mix"), "print(\"SOURCE_FILE_OK\")\n").unwrap();
    fs::write(
        base.join("continued.mix"),
        "/usr/bin/echo SOURCE_CONTINUED \\\nOK\n",
    )
    .unwrap();

    let ls = mix_in(&sub, &["-c", "ls .."]);
    assert_success(&ls);
    assert!(String::from_utf8_lossy(&ls.stdout).contains("relscript.sh"));

    let cd = mix_in(&sub, &["-c", "cd .. && pwd"]);
    assert_success(&cd);
    assert_eq!(
        String::from_utf8_lossy(&cd.stdout).trim(),
        base.display().to_string()
    );

    let rel = mix_in(&sub, &["-c", "../relscript.sh"]);
    assert_success(&rel);
    assert_eq!(String::from_utf8_lossy(&rel.stdout), "RELSCRIPT_OK\n");

    let source_dir = mix_in(&sub, &["-c", "source .."]);
    assert_eq!(source_dir.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&source_dir.stderr).contains("Is a directory"));

    let source_file = mix_in(&sub, &["-c", "source ../f.mix"]);
    assert_success(&source_file);
    assert_eq!(
        String::from_utf8_lossy(&source_file.stdout),
        "SOURCE_FILE_OK\n"
    );

    let source_continued = mix_in(&sub, &["-c", "source ../continued.mix"]);
    assert_success(&source_continued);
    assert_eq!(
        String::from_utf8_lossy(&source_continued.stdout),
        "SOURCE_CONTINUED OK\n"
    );

    let interactive = repl(&sub, "ls ..\ncd ..\npwd\nexit\n");
    assert_success(&interactive);
    let stdout = String::from_utf8_lossy(&interactive.stdout);
    assert!(stdout.contains("relscript.sh"), "stdout:\n{stdout}");
    assert!(
        stdout.contains(&base.display().to_string()),
        "stdout:\n{stdout}"
    );

    let data = dir.write("sample.conf.mix", "{ answer: 42, path: \"C:\\\\path\" }\n");
    let loader = dir.write(
        "load.mix",
        &format!(
            "$cfg = load_data(\"{}\")\nprint($cfg.answer)\nprint($cfg.path)\n",
            data.display()
        ),
    );
    let loaded = mix(&[loader.to_str().unwrap()]);
    assert_success(&loaded);
    assert_eq!(String::from_utf8_lossy(&loaded.stdout), "42\nC:\\path\n");
}
