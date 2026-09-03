//! Regex escape classes — behaviour pins (2026-09-03).
//!
//! Every test asserts WHAT matches, not merely that the call ran, per
//! escape class across the pattern-taking builtins. History: a TODO
//! entry ("\xNN hex escapes match the ENTIRE subject, silent data
//! loss") turned out to be a MISDIAGNOSIS — its probes passed the
//! replacement where `regex_replace(pattern, text, replacement)` takes
//! the TEXT, so the "destroyed subject" was the untouched second
//! argument echoed back. Hex escapes have worked all along; these pins
//! keep that provable, and the arg-order tests keep the real trap
//! visible. The long-pattern test covers the truncated compile
//! diagnostic added for the swapped-call incident class.

use cosmix_mix::run_capturing;

async fn out(source: &str) -> String {
    let (_, stdout, stderr) = run_capturing(source).await.expect("source should run");
    assert!(stderr.is_empty(), "unexpected stderr: {stderr}");
    stdout
}

async fn err(source: &str) -> String {
    match run_capturing(source).await {
        Ok((v, o, e)) => panic!("expected an error, got {v:?} (stdout={o:?} stderr={e:?})"),
        Err(e) => e.to_string(),
    }
}

// ---------- \xNN hex escapes: match exactly that byte's character ----------

#[tokio::test]
async fn hex_escape_matches_only_that_character() {
    // \x41 is "A": replace it, leave the rest.
    assert_eq!(out("print(regex_replace(\"\\\\x41\", \"AB\", \"-\"))").await, "-B\n");
    assert_eq!(out("print(regex_match(\"\\\\x41\", \"AB\"))").await, "true\n");
    assert_eq!(out("print(regex_match(\"\\\\x41\", \"b\"))").await, "false\n");
    assert_eq!(out("print(len(regex_find(\"\\\\x41\", \"ABCA\")))").await, "2\n");
}

#[tokio::test]
async fn hex_escape_absent_character_leaves_subject_alone() {
    // THE assertion that would have caught the misdiagnosis: \x1b (ESC)
    // absent from the subject — the subject must come back untouched.
    assert_eq!(out("print(regex_replace(\"\\\\x1b\", \"AB\", \"-\"))").await, "AB\n");
    assert_eq!(out("print(regex_match(\"\\\\x1b\", \"AB\"))").await, "false\n");
    assert_eq!(out("print(len(regex_find(\"\\\\x1b\", \"AB\")))").await, "0\n");
}

#[tokio::test]
async fn hex_escape_strips_real_ansi_sequences() {
    // The original incident's job, done with the CORRECT argument order:
    // ANSI colour codes stripped, payload intact, byte count sane.
    let src = "$t = \"\\u{1b}[31mred\\u{1b}[0m plain\"\n$s = regex_replace(\"\\\\x1b\\\\[[0-9;]*m\", $t, \"\")\nprint($s)\nprint(len($s))\n";
    assert_eq!(out(src).await, "red plain\n9\n");
}

#[tokio::test]
async fn brace_and_unicode_hex_forms() {
    assert_eq!(out("print(regex_match(\"\\\\x{41}\", \"A\"))").await, "true\n");
    assert_eq!(out("print(regex_match(\"\\\\u0041\", \"A\"))").await, "true\n");
    assert_eq!(out("print(regex_match(\"\\\\u{41}\", \"A\"))").await, "true\n");
    assert_eq!(out("print(regex_match(\"\\\\u{e9}\", \"caf\\u{e9}\"))").await, "true\n");
}

// ---------- class escapes ----------

#[tokio::test]
async fn class_escapes_match_their_classes() {
    // \d digits only
    assert_eq!(out("print(regex_replace(\"\\\\d+\", \"a7b42c\", \"#\"))").await, "a#b#c\n");
    // \w word chars
    assert_eq!(out("print(regex_match(\"^\\\\w+$\", \"ab_1\"))").await, "true\n");
    assert_eq!(out("print(regex_match(\"^\\\\w+$\", \"a b\"))").await, "false\n");
    // \s whitespace runs
    assert_eq!(out("print(regex_replace(\"\\\\s+\", \"a  b\\tc\", \"_\"))").await, "a_b_c\n");
    // \b word boundary
    assert_eq!(out("print(regex_match(\"\\\\bword\\\\b\", \"a word here\"))").await, "true\n");
    assert_eq!(out("print(regex_match(\"\\\\bword\\\\b\", \"swordfish\"))").await, "false\n");
}

#[tokio::test]
async fn literal_backslash_escape() {
    // Pattern \\ (one escaped backslash) matches one literal backslash.
    assert_eq!(out("print(regex_replace(\"\\\\\\\\\", \"a\\\\b\", \"/\"))").await, "a/b\n");
}

// ---------- unsupported escapes fail loudly, never silently ----------

#[tokio::test]
async fn unsupported_escape_is_a_loud_error() {
    let e = err("print(regex_match(\"\\\\e\", \"x\"))").await;
    assert!(e.contains("invalid regex"), "got: {e}");
    assert!(e.contains("escape sequence"), "got: {e}");
}

// ---------- the same escapes through split and grep ----------

#[tokio::test]
async fn split_and_grep_share_the_escape_handling() {
    assert_eq!(out("print(len(regex_split(\"\\\\d\", \"a1b2c\")))").await, "3\n");
    assert_eq!(
        out("print(len(grep(\"\\\\x41\", \"Alpha\\nbeta\\nApex\")))").await,
        "2\n"
    );
}

// ---------- argument order: the REAL trap behind the misdiagnosis ----------

#[tokio::test]
async fn regex_replace_argument_order_is_pattern_text_replacement() {
    // regex_replace(pattern, TEXT, replacement) — the incident calls
    // passed (pattern, replacement, text) and got their own second
    // argument back. Pin the true order both ways round.
    assert_eq!(out("print(regex_replace(\"B\", \"AB\", \"-\"))").await, "A-\n");
    // Swapped (the mistake): "AB" is now the pattern, subject is "-",
    // no match — the "-" comes back unchanged. Silent, which is exactly
    // why the re_* subject-first names are planned.
    assert_eq!(out("print(regex_replace(\"AB\", \"-\", \"B\"))").await, "-\n");
}

// ---------- long-pattern compile diagnostic (swapped-call incident class) ----------

#[tokio::test]
async fn long_invalid_pattern_error_is_truncated_and_hints_arg_order() {
    // A whole-document "pattern" (the swapped-call shape) with an
    // invalid construct: the error must stay short, keep the real
    // complaint, and name the usual cause.
    let src = "$doc = repeat(\"line of roster text \", 400) .. \"x{bad}\"\nregex_match($doc, \"y\")\n";
    let e = err(src).await;
    assert!(e.len() < 600, "diagnostic not truncated ({} chars): {e}", &e[..200]);
    assert!(e.contains("truncated"), "got: {e}");
    assert!(e.contains("argument 1 is the PATTERN"), "got: {e}");
    assert!(e.contains("error:"), "the regex crate's own complaint must survive: {e}");
}

#[tokio::test]
async fn short_invalid_pattern_error_keeps_the_full_report() {
    let e = err("regex_match(\"[unterminated\", \"x\")").await;
    assert!(e.contains("invalid regex '[unterminated'"), "got: {e}");
    assert!(!e.contains("argument 1 is the PATTERN"), "short patterns keep the old format: {e}");
}
