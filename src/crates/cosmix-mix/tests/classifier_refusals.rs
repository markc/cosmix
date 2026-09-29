//! The shell classifier's refusal layer (B1, B3, 09-29 #1): a bash
//! keyword written as a shell command fails ONCE and legibly instead of
//! piecemeal at exit 0, an unquoted newline is a `;`, and a misrouted
//! Mix one-liner names itself.

use std::process::Command;

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_mix"))
        .args(args)
        .env("MIX_STATS", "off")
        .env_remove("MIXRC")
        .output()
        .expect("run mix");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn unquoted_newline_is_a_separator() {
    // B1: `mix -c $'true\nfalse'` must run BOTH commands — the second
    // one's exit code wins (1), never a single command with a newline in
    // its filename at exit 0.
    let (code, _, _) = run(&["-c", "true\nfalse"]);
    assert_eq!(code, 1, "a bare newline must act like ';'");
}

#[test]
fn bash_keyword_refuses_the_whole_list() {
    let (code, out, err) = run(&["-c", "for x in 1; do echo hi; done; echo END"]);
    assert_eq!(code, 2, "a bash keyword head must refuse the whole list");
    assert!(!out.contains("END"), "the chain must not carry on: {out}");
    assert!(
        err.contains("`for` is bash"),
        "the refusal must name the Mix form: {err}"
    );
}

#[test]
fn dollar_question_marks_are_refused_with_the_rc_pointer() {
    let (code, _, err) = run(&["-c", "echo $?"]);
    assert_eq!(code, 2, "a bare $? must be refused");
    assert!(err.contains("$rc"), "the refusal must point at $rc: {err}");
}

#[test]
fn single_quoted_dollar_question_stays_literal() {
    // `'$?'` is literal text in sh semantics — it must keep working.
    let (code, out, _) = run(&["-c", "echo '$?'"]);
    assert_eq!(code, 0, "a single-quoted $? is literal, not a refusal");
    assert!(out.contains("$?"), "the literal must reach echo: {out}");
}

#[test]
fn keyword_word_as_an_argument_is_not_a_head() {
    // `echo for` — 'for' is an ARGUMENT, the head is echo.
    let (code, out, _) = run(&["-c", "echo for"]);
    assert_eq!(code, 0);
    assert!(out.contains("for"));
}

#[test]
fn misrouted_mix_line_names_itself() {
    let (code, _, err) = run(&["-c", "nosuchone\nnosuchtwo"]);
    assert_ne!(code, 0);
    assert!(
        err.contains("look like Mix keywords"),
        "all-ENOENT lines must say so: {err}"
    );
}

#[test]
fn a_single_typo_stays_a_plain_enoent() {
    // One ENOENT piece is a typo, not a misrouted program — no heuristic.
    let (code, _, err) = run(&["-c", "typo-command-xyz"]);
    assert_ne!(code, 0);
    assert!(
        !err.contains("look like Mix keywords"),
        "a single typo must not trigger the heuristic: {err}"
    );
}
