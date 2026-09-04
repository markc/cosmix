//! `Severity::Note` + the MIX-D3xxx advisory namespace (0.63.0,
//! `schema_version: 2`) — CLI-level pins, because the stakes are
//! CLI-level: `--deny-warnings` is a live fleet deploy gate
//! (deploy_shared.mix runs it over every shared-manifest source), so a
//! note that counted as a warning would stop fleet deploys the day the
//! deprecation notes ship.

use std::process::Command;

fn write_temp(name: &str, source: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("mix_lint_notes_{name}.mix"));
    std::fs::write(&path, source).unwrap();
    path
}

fn lint(args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_mix"))
        .arg("lint")
        .args(args)
        .env("MIX_STATS", "off")
        .output()
        .expect("run mix lint");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// A file whose ONLY diagnostics are notes (one of each D-code family).
const NOTES_ONLY: &str = "$t = \"a b\"\n\
    print(lastpos(\"a\", $t))\n\
    print(pos(\"b\", $t))\n\
    for each $i, $x in [1, 2]\n  print($i .. $x)\nend\n\
    $m = {a: 1}\n\
    if $m == {a: 1} then print \"eq\" end\n";

#[test]
fn notes_only_file_exits_zero_under_deny_warnings() {
    // THE pin: fleet deploys ride on this exit code.
    let path = write_temp("deny", NOTES_ONLY);
    let (code, out) = lint(&["--deny-warnings", path.to_str().unwrap()]);
    assert_eq!(code, 0, "notes must never deny; output:\n{out}");
    assert!(out.contains("note:"), "notes still rendered: {out}");
    assert!(!out.contains("[denied]"), "{out}");
}

#[test]
fn notes_render_and_count_separately() {
    let path = write_temp("plain", NOTES_ONLY);
    let (code, out) = lint(&[path.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}");
    // D3009/D3008 (lastpos/pos) carry the note flag now: the D3001-5
    // fixtures left with release B — a deleted name adds an E1102 error,
    // which is exactly what a notes-only fixture must not contain.
    assert!(out.contains("MIX-D3009 note:"), "{out}");
    assert!(out.contains("MIX-D3008 note:"), "{out}");
    // RETIRED in 0.68.0 when their A.1 flips landed. Asserted ABSENT rather
    // than just dropped from the count: a watch note that outlives its flip
    // tells every reader to prepare for a change that already happened, and
    // the fixture still contains both shapes that used to trigger them.
    assert!(!out.contains("MIX-D3006"), "D3006 is retired: {out}");
    assert!(!out.contains("MIX-D3007"), "D3007 is retired: {out}");
    assert!(
        out.contains("0 error(s), 0 warning(s), 2 note(s)"),
        "two-count summary: {out}"
    );
}

#[test]
fn json_schema_v2_carries_notes() {
    let path = write_temp("json", NOTES_ONLY);
    let (code, out) = lint(&["--json", path.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}");
    let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
    assert_eq!(v["schema_version"], 2);
    assert_eq!(v["summary"]["notes"], 2, "{out}");
    assert_eq!(v["summary"]["warnings"], 0, "notes are not warnings: {out}");
    assert!(
        v["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .all(|d| d["severity"] == "note"),
        "{out}"
    );
}

#[test]
fn real_warnings_still_deny() {
    // W2305 (bare index_of in a condition) is a born-warning; the
    // --deny-warnings contract for it is unchanged.
    let src = "if index_of(\"abc\", \"z\") then\n  print \"x\"\nend\n";
    let path = write_temp("warn", src);
    let (code, out) = lint(&["--deny-warnings", path.to_str().unwrap()]);
    assert_eq!(code, 1, "warnings still deny: {out}");
    assert!(out.contains("[denied]"), "{out}");
    let (code_plain, _) = lint(&[path.to_str().unwrap()]);
    assert_eq!(code_plain, 0, "without the flag a warning passes");
}

#[test]
fn output_orders_error_then_warning_then_note() {
    let src =
        "print($undefined)\nif index_of(\"abc\", \"z\") then\n  print(pos(\"a\", \"ab\"))\nend\n";
    let path = write_temp("order", src);
    let (_, out) = lint(&[path.to_str().unwrap()]);
    let e = out.find("MIX-E1101").expect("error present");
    let w = out.find("MIX-W2305").expect("warning present");
    let n = out.find("MIX-D3008").expect("note present");
    assert!(e < w && w < n, "error < warning < note ordering: {out}");
}

#[test]
fn builtin_shadowing_definition_warns() {
    // MIX-W2403 (0.74.0): a function named after a builtin is dead code —
    // the builtin wins at every call site. Warning severity (a compat
    // shim for an older mix is legitimate), so it DOES deny under
    // --deny-warnings; a non-colliding name stays quiet.
    let src = "function upper($s)\n  return \"no\"\nend\nprint(upper(\"a\"))\n";
    let path = write_temp("shadow", src);
    let (code, out) = lint(&["--deny-warnings", path.to_str().unwrap()]);
    assert_ne!(code, 0, "shadow definition denies: {out}");
    assert!(out.contains("MIX-W2403"), "{out}");
    assert!(out.contains("cannot be called BY NAME"), "{out}");
    let clean = "function upper_snake($s)\n  return upper($s)\nend\nprint(upper_snake(\"a\"))\n";
    let path = write_temp("shadow_clean", clean);
    let (code, out) = lint(&["--deny-warnings", path.to_str().unwrap()]);
    assert_eq!(code, 0, "non-colliding name is quiet: {out}");
}

#[test]
fn pad_loop_idiom_gets_a_note_not_a_warning() {
    // MIX-D3013 (0.74.0): the hand-rolled pad loop notes toward
    // lpad/rpad — and stays a NOTE, so it never denies a deploy gate.
    let src = "$o = \"x\"\nwhile len($o) < 8\n  $o = $o .. \" \"\nend\nprint($o)\n";
    let path = write_temp("padloop", src);
    let (code, out) = lint(&["--deny-warnings", path.to_str().unwrap()]);
    assert_eq!(code, 0, "a note never denies: {out}");
    assert!(out.contains("MIX-D3013 note:"), "{out}");
    assert!(out.contains("lpad()/rpad()"), "{out}");
    // A loop that appends a VARIABLE (unknown width) is not the idiom.
    let other = "$o = \"x\"\n$pad = \" \"\nwhile len($o) < 8\n  $o = $o .. $pad\nend\nprint($o)\n";
    let path = write_temp("padloop_var", other);
    let (_, out) = lint(&[path.to_str().unwrap()]);
    assert!(!out.contains("MIX-D3013"), "variable append stays quiet: {out}");
}

#[test]
fn deleted_legacy_name_gets_both_the_error_and_the_pointer() {
    // Release B (0.73.0) deleted the five regex/grep legacy names. A
    // straggler call must co-fire MIX-E1102 (undefined function — the
    // deletion is real) AND the retained MIX-D3001 note (the pointer to
    // the replacement, with the hint saying the deletion happened). This
    // is the pin that stops a future cleanup pruning the D3001-5 rows as
    // dead code — lint.md promises the pointer stays.
    let src = "print(regex_match(\"^a\", \"abc\"))\n";
    let path = write_temp("deleted_cofire", src);
    let (code, out) = lint(&[path.to_str().unwrap()]);
    assert_ne!(code, 0, "a deleted name is an error: {out}");
    assert!(out.contains("MIX-E1102"), "undefined function: {out}");
    assert!(out.contains("MIX-D3001"), "pointer note retained: {out}");
    assert!(
        out.contains("DELETED in release B"),
        "hint says it is gone, not merely deprecated: {out}"
    );
}

#[test]
fn ufcs_spelling_is_noted_too() {
    // `$s.pos(..)` desugars to a FunctionCall at PARSE time
    // (method_desugars_to_ufcs covers every builtin name), so the
    // deprecation notes see member-call spellings as well — there is no
    // UFCS blind spot for builtin-named calls. Pinned so a parser
    // change that widened MethodCall would surface here. (This pin rode
    // on regex_match until release B deleted it — a deleted name no
    // longer desugars, so the surviving pos family carries it.)
    let src = "$s = \"abc\"\nprint($s.pos(\"a\"))\n";
    let path = write_temp("ufcs", src);
    let (_, out) = lint(&[path.to_str().unwrap()]);
    assert!(out.contains("MIX-D3008"), "UFCS spelling noted: {out}");
}

#[test]
fn advisory_pass_sees_wrapped_and_expression_shapes() {
    // The walker-geometry blind spots from the GLM review of d73304a6
    // (finding 2), each pinned: piped two-var loop, chained call,
    // if-expression condition, lambda default + expression body,
    // named-fn `= expr` body — and the nested-substr dedupe (finding 1).
    // NOTE: line 1 used to pin the piped-loop geometry via MIX-D3006 on the
    // two-variable loop itself. D3006 retired in 0.68.0 when the map-binding
    // flip landed, so the same geometry is now pinned with a legacy call
    // INSIDE the piped loop's body — which is the stronger shape anyway: it
    // proves the walker descends into the body, not merely that it saw the
    // loop header.
    // (Lines 4 and 5 rode on regex_match/grep until release B deleted
    // them — a deleted name now adds MIX-E1102, so the surviving
    // pos-family notes carry the same walker-geometry pins.)
    let src = "for each $i, $x in [1] print(pos(\"a\", \"ab\")) end | cat\n\
        send noded noded.ping && print(pos(\"a\", \"ab\"))\n\
        $y = if pos(\"b\", \"abc\") > 0 then 1 else 2 end\n\
        $f = fn($a = pos(\"c\", \"c\")) = lastpos(\"x\", $a)\n\
        fn g() = byte_pos(\"z\", \"z\")\n\
        $r = substr(\"abc\", 1 + substr(\"abc\", pos(\"b\", \"abc\"), 1), 1)\n";
    let path = write_temp("blind_shapes", src);
    let (code, out) = lint(&[path.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("blind_shapes.mix:1: MIX-D3008"), "piped loop body: {out}");
    assert!(out.contains("blind_shapes.mix:2: MIX-D3008"), "chained: {out}");
    assert!(out.contains("blind_shapes.mix:3: MIX-D3008"), "if-expr cond: {out}");
    assert!(out.contains("blind_shapes.mix:4: MIX-D3008"), "lambda default: {out}");
    assert!(out.contains("blind_shapes.mix:4: MIX-D3009"), "lambda expr body: {out}");
    assert!(out.contains("blind_shapes.mix:5: MIX-D3010"), "fn expr body: {out}");
    // Exactly one composed note for the nested substr — not two.
    assert_eq!(
        out.matches("composes a 1-based position").count(),
        1,
        "one site, one note: {out}"
    );
    assert!(out.contains("0 error(s), 0 warning(s), 7 note(s)"), "{out}");
}
