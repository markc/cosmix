//! `mix apropos TERM` — one substring search across builtins, keywords, and
//! manual section headings. The "lpad lesson made structural": finding a
//! capability no longer needs the exact name up front. Also pins the
//! `mix what TERM` fall-through (a non-exact name searches instead of the old
//! bare "unknown").

use std::process::Command;

fn mix() -> Command {
    Command::new(env!("CARGO_BIN_EXE_mix"))
}

fn stdout(args: &[&str]) -> String {
    let out = mix().args(args).output().expect("spawn mix");
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn apropos_pad_finds_the_pad_family_by_description() {
    // None of these has "pad" as an exact queryable-without-knowing name; the
    // point is that a description/name substring surfaces all of them.
    let out = stdout(&["apropos", "pad"]);
    assert!(out.contains("BUILTINS"), "{out}");
    for name in ["lpad", "rpad", "lpad_w", "rpad_w"] {
        assert!(out.contains(name), "apropos pad missing {name}:\n{out}");
    }
    assert!(out.contains("match(es)"), "{out}");
}

#[test]
fn apropos_reports_no_match_cleanly() {
    let out = stdout(&["apropos", "zzz_definitely_no_such_thing"]);
    assert!(
        out.contains("no builtins, keywords, or manual sections match"),
        "{out}"
    );
}

#[test]
fn what_falls_through_to_apropos_for_a_non_exact_name() {
    // Exact name → the old one-liner behaviour is unchanged.
    let exact = stdout(&["what", "round"]);
    assert!(exact.starts_with("round:"), "{exact}");
    // Non-exact → searches instead of printing "unknown".
    let fuzzy = stdout(&["what", "pad"]);
    assert!(fuzzy.contains("searching"), "{fuzzy}");
    assert!(fuzzy.contains("lpad"), "{fuzzy}");
}
