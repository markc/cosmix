//! `mix --version` answers from a cold process.
//!
//! Mark's contract (2026-09-21): a version query does **nothing** except
//! report the version and the build hash, even while another mix is already
//! running. Before this, the `--version` arm lived inside `real_main`, so
//! `session_task::capture_base_env()`, `native_session::start()` (which
//! begins Bus dispatch) and the evaluation thread had all already run by the
//! time the version was printed.
//!
//! The instrument is the native-session lane's own diagnostic: given a
//! malformed `COSMIX_SESSION_FD`, a mix that starts the lane writes
//! `mix native-session FAILED at …` to stderr. A version query must stay
//! silent on the same input — and `control_proves_the_probe_can_fail` runs
//! that same input through `-c` to show the probe is live, per the
//! prove-the-gate-can-fail rule. Without the control, an unconditionally
//! empty stderr would read as a pass.
#![cfg(target_os = "linux")]

use std::process::{Command, Output};

const MARKER: &str = "COSMIX_SESSION_FD";

/// A value that is not a usable inherited descriptor, so the lane — if it
/// starts at all — must complain.
const BOGUS: &str = "99999";

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mix"))
        .args(args)
        .env(MARKER, BOGUS)
        .env("MIX_STATS", "off")
        .output()
        .expect("run mix")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn control_proves_the_probe_can_fail() {
    let out = run(&["--no-prelude", "-c", "print(1)"]);
    assert!(
        stderr(&out).contains("mix native-session FAILED at"),
        "the probe is dead — a malformed {MARKER} no longer makes the session \
         lane complain, so the silence asserted below proves nothing. \
         stderr: {:?}",
        stderr(&out)
    );
}

#[test]
fn version_starts_no_session_lane() {
    for args in [&["--version"][..], &["-V"][..], &["--version", "--json"][..]] {
        let out = run(args);
        assert!(out.status.success(), "mix {args:?} exited {:?}", out.status);
        assert_eq!(
            stderr(&out),
            "",
            "mix {args:?} started the session lane — it must report the \
             version and nothing else"
        );
    }
}

#[test]
fn version_reports_the_version_and_the_build_hash() {
    let out = run(&["--version"]);
    let line = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
    let (version, rest) = line
        .strip_prefix("mix ")
        .and_then(|s| s.split_once(" ("))
        .unwrap_or_else(|| panic!("expected `mix X.Y.Z (sha)`, got {line:?}"));
    assert_eq!(version, env!("CARGO_PKG_VERSION"));
    let sha = rest
        .strip_suffix(')')
        .unwrap_or_else(|| panic!("unterminated build hash in {line:?}"))
        .trim_end_matches("-dirty");
    assert!(!sha.is_empty(), "blank build hash in {line:?}");
    assert!(
        sha == "unknown" || sha.chars().all(|c| c.is_ascii_hexdigit()),
        "build hash must be hex or the explicit \"unknown\", got {sha:?}"
    );
}
