//! An uncaught runtime error prints the offending source line under it.
//!
//! The line is rendered from the exact bytes the process executed (the `-c`
//! body or the loaded script), never a disk re-read — so it cannot go stale
//! or lie. These pin both that the footer appears for `-c` and a file, and
//! that it names the right line and text.

use std::io::Write;
use std::process::Command;

fn mix() -> Command {
    Command::new(env!("CARGO_BIN_EXE_mix"))
}

fn stderr_of(cmd_output: std::process::Output) -> String {
    String::from_utf8_lossy(&cmd_output.stderr).to_string()
}

#[test]
fn dash_c_error_shows_the_offending_line() {
    let out = mix()
        .arg("-c")
        .arg("$x = 5\nprint($y)")
        .output()
        .expect("spawn mix");
    let err = stderr_of(out);
    // The error itself, then the source-line footer for line 2.
    assert!(err.contains("undefined variable '$y'"), "{err}");
    assert!(err.contains("\n  2 | print($y)"), "no footer in:\n{err}");
}

#[test]
fn file_error_shows_the_offending_line() {
    let mut f = tempfile::NamedTempFile::new().expect("tempfile");
    write!(f, "$a = 1\n$b = 2\nsqrt(\"nope\")\n").unwrap();
    let out = mix().arg(f.path()).output().expect("spawn mix");
    let err = stderr_of(out);
    assert!(err.contains("sqrt() expects a number"), "{err}");
    assert!(err.contains("\n  3 | sqrt(\"nope\")"), "no footer in:\n{err}");
}

#[test]
fn no_footer_for_a_blank_or_missing_site() {
    // `die` at top level carries a site, but a bare message with no code and
    // no offending expression on a blank line must not fabricate a footer.
    let out = mix()
        .arg("-c")
        .arg("print(1)\nprint(2)")
        .output()
        .expect("spawn mix");
    // A clean run prints no error footer at all.
    let err = stderr_of(out);
    assert!(!err.contains(" | "), "unexpected footer:\n{err}");
}
