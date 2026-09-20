//! `mix edit` at the interactive Mix prompt.
//!
//! The subcommand exists for a one-line edit **from a prompt**, and Mix
//! is a login shell here — so the REPL is the prompt it has to work at.
//! The one-shot CLI intercepts `edit` before `meta::dispatch`, which the
//! REPL never reaches; without its own REPL arm the command comes back
//! as `unknown meta-command 'edit'` and the file is untouched, which is
//! the silent no-op this whole subcommand exists to refuse.
#![cfg(target_os = "linux")]

use std::fs;
use std::io::Write;
use std::process::{Command, Output, Stdio};

fn repl(script: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mix"))
        .arg("-i")
        .env("MIX_STATS", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mix -i");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(script.as_bytes())
        .expect("write script");
    child.wait_with_output().expect("wait")
}

fn workdir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("mix-edit-repl-{}-{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create workdir");
    dir
}

#[test]
fn the_repl_dispatches_edit_and_actually_writes() {
    let dir = workdir("ok");
    let p = dir.join("f.txt");
    fs::write(&p, "alpha\n").unwrap();

    let out = repl(&format!(
        "mix edit {} alpha BETA\nprint($status)\nexit\n",
        p.display()
    ));
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        !stderr.contains("unknown meta-command"),
        "the REPL did not route `mix edit`: {stderr}"
    );
    assert_eq!(
        fs::read_to_string(&p).unwrap(),
        "BETA\n",
        "the REPL reported no error but wrote nothing"
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains('0'),
        "$status should be 0 after a successful edit"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// A refusal at the prompt must land in `$status`, not vanish — the
/// exit code is the whole reason `edit` bypasses `meta::dispatch`.
#[test]
fn a_refusal_in_the_repl_sets_status_and_leaves_the_file_alone() {
    let dir = workdir("refuse");
    let p = dir.join("two.txt");
    fs::write(&p, "x = 1\ny = 0\nx = 1\n").unwrap();

    let out = repl(&format!(
        "mix edit {} 'x = 1' 'x = 2'\nprint($status)\nexit\n",
        p.display()
    ));
    assert_eq!(
        fs::read_to_string(&p).unwrap(),
        "x = 1\ny = 0\nx = 1\n",
        "an ambiguous needle must not write"
    );
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        stdout.contains('2'),
        "$status should be 2 after an ambiguous refusal; stdout: {stdout:?}"
    );
    let _ = fs::remove_dir_all(&dir);
}
