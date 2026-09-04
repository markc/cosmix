//! Regex escape classes — behaviour pins (2026-09-03; migrated to the
//! subject-first re_* family when release B deleted the legacy names,
//! 0.73.0).
//!
//! Every test asserts WHAT matches, not merely that the call ran, per
//! escape class across the pattern-taking builtins. History: a TODO
//! entry ("\xNN hex escapes match the ENTIRE subject, silent data
//! loss") turned out to be a MISDIAGNOSIS — its probes passed the
//! replacement where the legacy regex_replace(pattern, text,
//! replacement) took the TEXT, so the "destroyed subject" was the
//! untouched second argument echoed back. Hex escapes have worked all
//! along; these pins keep that provable. The subject-first argument
//! order that ended the trap for good is pinned here too, as is the
//! loud death of the deleted legacy names.

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
    assert_eq!(out("print(re_replace(\"AB\", \"\\\\x41\", \"-\"))").await, "-B\n");
    assert_eq!(out("print(re_match(\"AB\", \"\\\\x41\"))").await, "true\n");
    assert_eq!(out("print(re_match(\"b\", \"\\\\x41\"))").await, "false\n");
    assert_eq!(out("print(len(re_find(\"ABCA\", \"\\\\x41\")))").await, "2\n");
}

#[tokio::test]
async fn hex_escape_absent_character_leaves_subject_alone() {
    // THE assertion that would have caught the misdiagnosis: \x1b (ESC)
    // absent from the subject — the subject must come back untouched.
    assert_eq!(out("print(re_replace(\"AB\", \"\\\\x1b\", \"-\"))").await, "AB\n");
    assert_eq!(out("print(re_match(\"AB\", \"\\\\x1b\"))").await, "false\n");
    assert_eq!(out("print(len(re_find(\"AB\", \"\\\\x1b\")))").await, "0\n");
}

#[tokio::test]
async fn hex_escape_strips_real_ansi_sequences() {
    // The original incident's job, in the subject-first spelling:
    // ANSI colour codes stripped, payload intact, byte count sane.
    let src = "$t = \"\\u{1b}[31mred\\u{1b}[0m plain\"\n$s = re_replace($t, \"\\\\x1b\\\\[[0-9;]*m\", \"\")\nprint($s)\nprint(len($s))\n";
    assert_eq!(out(src).await, "red plain\n9\n");
}

#[tokio::test]
async fn brace_and_unicode_hex_forms() {
    assert_eq!(out("print(re_match(\"A\", \"\\\\x{41}\"))").await, "true\n");
    assert_eq!(out("print(re_match(\"A\", \"\\\\u0041\"))").await, "true\n");
    assert_eq!(out("print(re_match(\"A\", \"\\\\u{41}\"))").await, "true\n");
    assert_eq!(out("print(re_match(\"caf\\u{e9}\", \"\\\\u{e9}\"))").await, "true\n");
}

// ---------- class escapes ----------

#[tokio::test]
async fn class_escapes_match_their_classes() {
    // \d digits only
    assert_eq!(out("print(re_replace(\"a7b42c\", \"\\\\d+\", \"#\"))").await, "a#b#c\n");
    // \w word chars
    assert_eq!(out("print(re_match(\"ab_1\", \"^\\\\w+$\"))").await, "true\n");
    assert_eq!(out("print(re_match(\"a b\", \"^\\\\w+$\"))").await, "false\n");
    // \s whitespace runs
    assert_eq!(out("print(re_replace(\"a  b\\tc\", \"\\\\s+\", \"_\"))").await, "a_b_c\n");
    // \b word boundary
    assert_eq!(out("print(re_match(\"a word here\", \"\\\\bword\\\\b\"))").await, "true\n");
    assert_eq!(out("print(re_match(\"swordfish\", \"\\\\bword\\\\b\"))").await, "false\n");
}

#[tokio::test]
async fn literal_backslash_escape() {
    // Pattern \\ (one escaped backslash) matches one literal backslash.
    assert_eq!(out("print(re_replace(\"a\\\\b\", \"\\\\\\\\\", \"/\"))").await, "a/b\n");
}

// ---------- unsupported escapes fail loudly, never silently ----------

#[tokio::test]
async fn unsupported_escape_is_a_loud_error() {
    let e = err("print(re_match(\"x\", \"\\\\e\"))").await;
    assert!(e.contains("invalid regex"), "got: {e}");
    assert!(e.contains("escape sequence"), "got: {e}");
}

// ---------- the same escapes through split and grep_lines ----------

#[tokio::test]
async fn split_and_grep_share_the_escape_handling() {
    assert_eq!(out("print(len(re_split(\"a1b2c\", \"\\\\d\")))").await, "3\n");
    assert_eq!(
        out("print(len(grep_lines(\"Alpha\\nbeta\\nApex\", \"\\\\x41\")))").await,
        "2\n"
    );
}

// ---------- argument order: subject FIRST, pinned both ways ----------

#[tokio::test]
async fn re_replace_argument_order_is_subject_pattern_replacement() {
    assert_eq!(out("print(re_replace(\"AB\", \"B\", \"-\"))").await, "A-\n");
    // Swapped (the legacy trap's shape): "B" becomes the subject, "AB"
    // the pattern, no match — "B" comes back. Still silent for a caller
    // who swaps re_* args, but the subject-first order matches every
    // literal-string builtin, which is what retired the trap in practice.
    assert_eq!(out("print(re_replace(\"B\", \"AB\", \"-\"))").await, "B\n");
}

// ---------- the deleted legacy names die loudly (release B, 0.73.0) ----------

#[tokio::test]
async fn deleted_legacy_names_are_undefined_functions() {
    for call in [
        "regex_match(\"a\", \"a\")",
        "regex_find(\"a\", \"a\")",
        "regex_replace(\"a\", \"a\", \"b\")",
        "regex_split(\"a\", \"a\")",
        "grep(\"a\", \"a\")",
    ] {
        let e = err(&format!("print({call})")).await;
        assert!(
            e.contains("undefined function") || e.contains("FUNCTION_UNDEFINED"),
            "{call} must be gone, got: {e}"
        );
    }
}

// ---------- long-pattern compile diagnostic (swapped-call incident class) ----------

#[tokio::test]
async fn long_invalid_pattern_error_is_truncated_and_hints_arg_order() {
    // A whole-document "pattern" (the swapped-call shape) with an
    // invalid construct: the error must stay short, keep the real
    // complaint, and name the usual cause.
    let src = "$doc = repeat(\"line of roster text \", 400) .. \"x{bad}\"\nre_match(\"y\", $doc)\n";
    let e = err(src).await;
    assert!(e.len() < 600, "diagnostic not truncated ({} chars): {e}", &e[..200]);
    assert!(e.contains("truncated"), "got: {e}");
    assert!(e.contains("SUBJECT comes first"), "got: {e}");
    assert!(e.contains("error:"), "the regex crate's own complaint must survive: {e}");
}

#[tokio::test]
async fn short_invalid_pattern_error_keeps_the_full_report() {
    let e = err("re_match(\"x\", \"[unterminated\")").await;
    assert!(e.contains("invalid regex '[unterminated'"), "got: {e}");
    assert!(!e.contains("SUBJECT comes first"), "short patterns keep the old format: {e}");
}
