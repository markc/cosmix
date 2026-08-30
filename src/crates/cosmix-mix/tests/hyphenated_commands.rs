//! Process-boundary coverage for the shell/Mix tight-hyphen classifier rule.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let serial = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "mix-hyphenated-{label}-{}-{serial}",
            std::process::id()
        ));
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

fn install_command(dir: &Path, name: &str) {
    let path = dir.join(name);
    fs::write(
        &path,
        format!("#!/bin/sh\nprintf 'ran {name} %s\\n' \"$1\"\n"),
    )
    .expect("write test command");
    let mut permissions = fs::metadata(&path)
        .expect("stat test command")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("make test command executable");
}

fn run_c(path: &Path, line: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mix"))
        .args(["-c", line])
        .env("MIX_STATS", "off")
        .env("PATH", path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run mix -c")
}

#[test]
fn hyphenated_path_commands_run_as_whole_tokens() {
    let dir = TempDir::new("path");
    for name in ["cosmix-comp", "systemd-nspawn", "weston-simple-dmabuf-egl"] {
        install_command(dir.path(), name);
    }

    for (line, expected) in [
        ("cosmix-comp --nested", "ran cosmix-comp --nested\n"),
        ("systemd-nspawn", "ran systemd-nspawn \n"),
        (
            "weston-simple-dmabuf-egl",
            "ran weston-simple-dmabuf-egl \n",
        ),
    ] {
        let out = run_c(dir.path(), line);
        assert_eq!(out.status.code(), Some(0), "line: {line}");
        assert_eq!(String::from_utf8_lossy(&out.stdout), expected);
        assert!(
            out.stderr.is_empty(),
            "line: {line}\nstderr: {:?}",
            out.stderr
        );
    }
}

#[test]
fn missing_hyphenated_command_names_the_whole_head() {
    let dir = TempDir::new("missing");
    let out = run_c(dir.path(), "alpha-no-such-command --flag");
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert_eq!(out.status.code(), Some(127));
    assert!(
        stderr.contains("mix: alpha-no-such-command: No such file or directory"),
        "stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("cannot use 'alpha' as number"),
        "stderr:\n{stderr}"
    );
}

#[test]
fn live_left_variable_does_not_change_command_dispatch() {
    let dir = TempDir::new("variable");
    let home = dir.path().join("home");
    fs::create_dir(&home).expect("create isolated HOME");
    // Bare `alpha` is a string literal, not a reference to `$alpha`. A live
    // variable must therefore have no effect on the command-head decision.
    install_command(dir.path(), "alpha-beta");

    let mut child = Command::new(env!("CARGO_BIN_EXE_mix"))
        .arg("-i")
        .env("HOME", home)
        .env("MIX_STATS", "off")
        .env("PATH", dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn Mix REPL");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(b"$alpha = 8\nalpha-beta\nprint(\"repl survived\")\n")
        .expect("write REPL input");
    let out = child.wait_with_output().expect("wait for Mix REPL");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert_eq!(out.status.code(), Some(0));
    assert!(stdout.contains("ran alpha-beta"), "stdout:\n{stdout}");
    assert!(stdout.contains("repl survived"), "stdout:\n{stdout}");
    assert!(stderr.is_empty(), "stderr:\n{stderr}");
}
