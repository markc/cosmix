//! A hyphenated Bus service name as a bare `send`/`emit`/`address` target.
//!
//! `send shell-ctl88 shell.debug.status` used to parse as
//! `shell - ctl88` and die at RUNTIME with "cannot use 'shell' as
//! number", while `mix --check` and `mix lint` both passed the line.
//! Hyphens are ordinary in Bus service names (`comp-nested`,
//! `desktop-vt1`), so the bare-word form failed for a large share of
//! real targets, and only when the script ran.
//!
//! **The instrument is differential**, not a broker.
//! `send a-b v` must behave EXACTLY like `send "a-b" v` — same `$rc`,
//! same `$result` — whatever broker happens to be reachable on the
//! machine running the suite. That comparison is what makes these
//! assertions mean the same thing on a workstation with a live noded
//! and on a build worker with none, and it fails hard against the old
//! parse (one form errors out, the other does not).
#![cfg(target_os = "linux")]

use std::process::{Command, Output};

/// The exact runtime error the old parse produced. Its ABSENCE is the
/// broker-free half of the gate.
const SUBTRACTION_ERROR: &str = "as number";

fn mix(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mix"))
        .args(args)
        .env("MIX_STATS", "off")
        .output()
        .expect("run mix")
}

/// Run a snippet and return the `rc=…/result=…` line it prints, so two
/// spellings of the same send can be compared byte for byte.
fn send_outcome(target_expr: &str) -> String {
    let src = format!(
        "send {target_expr} ping timeout=1\n\
         print(\"rc=\" .. to_string($rc) .. \" result=\" .. to_string($result))"
    );
    let out = mix(&["-c", &src]);
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        !stderr.contains(SUBTRACTION_ERROR),
        "`send {target_expr} …` still parses its target as subtraction: {stderr}"
    );
    assert!(
        out.status.success(),
        "`send {target_expr} …` failed: {stderr}"
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn a_bare_hyphenated_target_behaves_exactly_like_the_quoted_one() {
    let bare = send_outcome("shell-ctl88");
    let quoted = send_outcome("\"shell-ctl88\"");
    assert_eq!(
        bare, quoted,
        "the bare and quoted spellings of the same service name must \
         reach the same target with the same outcome"
    );
    identity_check("shell-ctl88", &bare);
}

/// Assert the RESOLVED NAME when the environment can show it.
///
/// The differential above proves the two spellings agree, but on a host
/// with no broker every target fails the same way, so agreement alone
/// would also be satisfied by an implementation that resolved every
/// name to one constant. Where a reply names the target, pin it. Where
/// it does not, say so out loud rather than reporting a pass on an
/// assertion that did not run — the differential and the "no longer
/// parsed as subtraction" check inside `send_outcome` are still live
/// gates in that environment, and they are what go red if the fix is
/// removed.
fn identity_check(expected: &str, outcome: &str) {
    if outcome.contains(expected) {
        return;
    }
    // A reply that names SOME other target means the name was resolved
    // and resolved wrongly — that is a failure, not an absent broker.
    assert!(
        !outcome.contains("not found") && !outcome.contains("Unknown"),
        "the target resolved to the wrong name (expected {expected:?}): {outcome}"
    );
    eprintln!(
        "note: no broker named the target, so the identity of {expected:?} \
         was not asserted here; the differential and the \
         not-parsed-as-subtraction checks still ran"
    );
}

#[test]
fn a_bare_hyphenated_target_behaves_exactly_like_a_variable() {
    let bare = send_outcome("desktop-vt1");
    let out = mix(&[
        "-c",
        "$t = \"desktop-vt1\"\nsend $t ping timeout=1\n\
         print(\"rc=\" .. to_string($rc) .. \" result=\" .. to_string($result))",
    ]);
    let via_var = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(bare, via_var);
}

/// Several hyphens, digits, and a dotted tail are all ordinary in a
/// service name.
#[test]
fn the_accepted_shape_covers_real_service_names() {
    for name in [
        "comp-nested",
        "shell-scenes-p2",
        "bterm-bevy-3164175",
        "a-b.c",
        "_leading-underscore",
        "X-Y",
        // A digit segment is fine as long as it is a well-formed
        // number, and `vt01` is an identifier (it starts with a letter)
        // rather than the leading-zero number the manual warns about.
        "svc-1",
        "svc-10",
        "desktop-vt01",
        // An all-digit segment that is a MALFORMED number (leading zero,
        // several dots). The lexer refused these before the parser could
        // see them (`ambiguous leading-zero number '007'`, an error that
        // never mentioned send); since 0.92.0 it lexes them as word
        // segments in bare target position only.
        "svc-01",
        "node-007",
        "node-7-x",
        "a-1.2.3",
        // A LEADING SEGMENT THAT IS A MIX KEYWORD. `next`, `print`,
        // `on`, `source`, `select`, `loop`, `end`, `to`, `in`, `and`
        // and friends all lex as keyword tokens, not identifiers, so a
        // `Token::String`-only guard left every one of these unwritable
        // bare while its quoted form worked.
        "next-hop",
        "print-server",
        "on-boot",
        "source-x",
        "select-db",
        "loop-back",
        "end-node",
        "true-b",
        "in-box",
    ] {
        let bare = send_outcome(name);
        let quoted = send_outcome(&format!("\"{name}\""));
        assert_eq!(bare, quoted, "target {name:?} did not round-trip");
        // A DOTTED name is a mesh address, so the broker splits it and
        // names a segment (`Unknown mesh node: 'c'`) rather than the
        // whole string — the quoted form does exactly the same, which
        // the equality above already proves. Identity is only checkable
        // for a plain service name.
        if !name.contains('.') {
            identity_check(name, &bare);
        }
    }
}

/// The shape the LEXER still refuses before the parser can see it: `fn-…`,
/// where `fn` starts a lambda. Pinned so the manual's "quote these" table
/// stays true, and so fixing it later is a deliberate act with a failing
/// test to notice. (The malformed-number segments — `svc-01`, `node-007`,
/// `a-1.2.3` — left this list in 0.92.0; they are in the accepted shapes.)
#[test]
fn fn_prefixed_names_still_need_quoting() {
    let name = "fn-svc";
    let out = mix(&["-c", &format!("send {name} ping timeout=1")]);
    assert!(
        !out.status.success(),
        "{name:?} now works bare — update docs/mix/bus.md's quote-it table"
    );
    // …and the quoted spelling is the documented way through.
    let out = mix(&[
        "-c",
        &format!("send \"{name}\" ping timeout=1\nprint(to_string($rc))"),
    ]);
    assert!(
        out.status.success(),
        "the quoted form of {name:?} must work: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The QUOTED spelling of a once-refused name still works on its own — it
/// was the documented way through before 0.92.0 and scripts use it. (The
/// differential above also runs it, but only as the comparison side.)
#[test]
fn quoted_malformed_number_names_still_work() {
    for name in ["svc-01", "node-007", "a-1.2.3"] {
        let out = mix(&[
            "-c",
            &format!("send \"{name}\" ping timeout=1\nprint(to_string($rc))"),
        ]);
        assert!(
            out.status.success(),
            "the quoted form of {name:?} must work: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// The malformed-number leniency is confined to the bare send target:
/// everywhere else a leading-zero or multi-dot number keeps its refusal
/// verbatim — including a `$var-007` target and a hyphenated COMMAND.
#[test]
fn malformed_numbers_outside_the_bare_target_are_still_refused() {
    for src in [
        "$x = 007",
        "$a = 1\n$x = $a-007",
        "$t = \"s\"\nsend $t-007 ping timeout=1",
        "send svc cmd-007 timeout=1",
        "$x = send-007",
    ] {
        let out = mix(&["-c", src]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{src:?} must still be refused");
        assert!(
            stderr.contains("ambiguous leading-zero number '007'"),
            "{src:?} must keep today's refusal verbatim: {stderr}"
        );
    }
}

/// `send`, `emit` and `address` all route through `parse_send_target`,
/// so one fix covers three keywords — and a regression in any of them
/// must show up here rather than in whichever one happened to be tested.
#[test]
fn emit_and_address_take_the_same_target_path() {
    for src in [
        "emit comp-nested ping",
        "address comp-nested\n  ping\nend",
        "$r = send comp-nested ping timeout=1",
    ] {
        let out = mix(&["-c", src]);
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(
            !stderr.contains(SUBTRACTION_ERROR),
            "{src:?} still parses its target as subtraction: {stderr}"
        );
        assert!(out.status.success(), "{src:?} failed: {stderr}");
    }
}

/// The static gates were the other half of the complaint: both passed a
/// line that could only fail at runtime. The line is simply valid now,
/// so they are still right to pass it — this guards the OTHER
/// direction, that the new parse does not start refusing it. (It
/// therefore stays green with the fix disabled, by design; the four
/// differential tests above are what go red.)
#[test]
fn check_and_lint_accept_a_hyphenated_target() {
    let dir = std::env::temp_dir().join(format!("mix-send-hyphen-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let script = dir.join("s.mix");
    std::fs::write(&script, "send shell-ctl88 shell.debug.status timeout=1\n").unwrap();
    let p = script.to_str().unwrap();

    let out = mix(&["--check", p]);
    assert!(
        out.status.success(),
        "mix --check refused it: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = mix(&["lint", p]);
    assert!(
        out.status.success(),
        "mix lint refused it: {} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// **Nothing that worked before may change.** The bareword path
/// REQUIRES a hyphen precisely so that every target which already
/// resolved keeps taking the expression path.
#[test]
fn every_other_target_shape_is_untouched() {
    // A plain bareword, a dotted bare address, a call, an index, a
    // concat, a parenthesised expression.
    for target in [
        "noded",
        "noded.delta.bus",
        "env(\"HOME\")",
        "(\"pre\" .. \"post\")",
    ] {
        let out = mix(&[
            "-c",
            &format!("send {target} ping timeout=1\nprint(to_string($rc))"),
        ]);
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(out.status.success(), "target {target:?} broke: {stderr}");
    }
}

/// A SPACED `a - b` is still an expression, and still the runtime type
/// error it always was — the fix is scoped to the tight-hyphen shape,
/// and a space is the signal that the author meant an operator.
#[test]
fn a_spaced_hyphen_remains_an_expression() {
    let out = mix(&["-c", "send a - b ping timeout=1"]);
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        stderr.contains(SUBTRACTION_ERROR),
        "spaced `a - b` should still be read as subtraction: {stderr}"
    );
    assert!(!out.status.success());
}

/// The two shapes whose BEHAVIOUR changed, pinned here so a future
/// widening is a deliberate act.
///
/// A differential sweep of every adjacent target shape against the
/// pre-fix binary found exactly these two, and both are error-to-error
/// — no program that produced a *value* changed. `a-b..c` was the
/// runtime type error "cannot use 'a' as number" and is now a rejected
/// Bus target (the `..` is inside the scanned word); `a-b .. "c"` was
/// the same runtime error and is now caught at PARSE time, which is
/// strictly better. Neither could have been a working concat, because
/// the left operand of `..` there is a bareword, which is a string, so
/// the subtraction ahead of it always failed first.
#[test]
fn the_two_shapes_that_changed_are_both_error_to_error() {
    // Tight `..` inside the word: parses now, and fails downstream. The
    // assertion is the RC BAND, not the message, so it means the same
    // with or without a broker.
    let out = mix(&[
        "-c",
        "send a-b..c ping timeout=1\nprint(to_string($rc))\nprint(to_string($result))",
    ]);
    assert!(
        out.status.success(),
        "should parse and fail at send time, not raise: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let got = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        !got.starts_with("0\n"),
        "an unroutable target must not report success: {got}"
    );

    // Spaced `..` after a hyphenated word: now a parse error.
    let out = mix(&["-c", "send a-b .. \"c\" ping timeout=1"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        stderr.contains("Parse error"),
        "expected a parse error, got: {stderr}"
    );
}

/// `--` OPENS A COMMENT. The lexer discards everything after it, so a
/// raw-source scan that ignored the marker would resurrect that text
/// into the name: `address a--b` + body + `end` addressed service `a`
/// and would start addressing `a--b` — a working script silently
/// sending somewhere else.
///
/// This is the one counterexample the cold review found to "nothing
/// valid changes meaning", so it is pinned here rather than described.
#[test]
fn a_comment_marker_is_not_swallowed_into_the_target() {
    let out = mix(&[
        "-c",
        "address a--b\n  ping timeout=1\nend\nprint(to_string($result))",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let got = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        !got.contains("a--b"),
        "the comment `--b` was resurrected into the target: {got}"
    );
}

/// A name with BOTH a dot and a hyphen. The dotted bare-address branch
/// stops at the hyphen and leaves `-c` to be read as the command, so
/// this used to be a parse error while its quoted form worked — the
/// hyphen scan has to run first.
#[test]
fn a_dotted_hyphenated_name_matches_its_quoted_form() {
    assert_eq!(send_outcome("a.b-c"), send_outcome("\"a.b-c\""));
    assert_eq!(send_outcome("a-b.c"), send_outcome("\"a-b.c\""));
}

/// A leading digit is not a service-name shape, so `1-2` keeps its
/// arithmetic reading. This is the boundary of the change, asserted so
/// that widening it later is a deliberate act.
#[test]
fn a_leading_digit_is_still_arithmetic() {
    let out = mix(&["-c", "send 1-2 ping timeout=1\nprint(to_string($rc))"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // It resolved to the NUMBER -1, not the string "1-2".
    let probe = mix(&[
        "-c",
        "send 1-2 ping timeout=1\nprint(to_string($result))",
    ]);
    let got = String::from_utf8_lossy(&probe.stdout).into_owned();
    assert!(
        !got.contains("1-2"),
        "a leading digit must keep the arithmetic reading: {got}"
    );
}
