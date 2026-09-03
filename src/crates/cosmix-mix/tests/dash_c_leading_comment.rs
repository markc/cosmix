//! `mix -c` must run a program whose FIRST line is a comment.
//!
//! It did not: both the no-op guard in `main.rs` and the classifier in
//! `shell.rs` tested `trimmed.starts_with("--")`, which is correct for a REPL
//! line (the input IS one line) but wrong for `-c`, which carries whole
//! programs. A program opening with a comment was classified Empty, silently
//! discarded, and reported exit 0.
//!
//! Silent discard reporting success is the worst available failure shape: a
//! script that never ran is indistinguishable from one that ran and did
//! nothing. Comments are the normal way to open a generated script, so this hit
//! machine-authored code squarely.
//!
//! Both directions are pinned here — the bug case must RUN, and a genuinely
//! all-comment input must still be a clean no-op. Fixing only the first would
//! turn every stray comment line into a parse error.

use std::process::Command;

fn mix() -> Command {
    Command::new(env!("CARGO_BIN_EXE_mix"))
}

fn run(code: &str) -> (String, i32) {
    let out = mix().arg("-c").arg(code).output().expect("spawn mix");
    (
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn leading_comment_does_not_discard_the_program() {
    let (stdout, code) = run("-- set up\nprint(\"RAN\")");
    assert_eq!(stdout, "RAN", "a program whose first line is a comment must run");
    assert_eq!(code, 0);
}

#[test]
fn leading_hash_comment_does_not_discard_the_program() {
    let (stdout, code) = run("# set up\nprint(\"RAN\")");
    assert_eq!(stdout, "RAN");
    assert_eq!(code, 0);
}

#[test]
fn comment_then_blank_then_code_runs() {
    let (stdout, code) = run("-- c\n\nprint(\"RAN\")");
    assert_eq!(stdout, "RAN");
    assert_eq!(code, 0);
}

#[test]
fn wholly_comment_input_is_still_a_clean_noop() {
    for code_str in ["-- just a comment", "# hash only", "-- one\n-- two", "", "   "] {
        let (stdout, code) = run(code_str);
        assert_eq!(stdout, "", "all-comment input must produce no output: {code_str:?}");
        assert_eq!(code, 0, "all-comment input must exit 0: {code_str:?}");
    }
}
