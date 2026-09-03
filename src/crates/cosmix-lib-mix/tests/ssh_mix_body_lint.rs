//! `mix lint` sees inside `ssh_mix` remote bodies (v0.69.0).
//!
//! `ssh_mix(host, source[, opts])` ships its SECOND argument to a remote
//! `mix -`, so that argument IS Mix source. Every earlier analyzer treated it
//! as an opaque string, which made a deploy script's entire remote half
//! invisible to lint AND to every inventory built from lint.
//!
//! That blind spot is not hypothetical: it is why the MIX-D3006 inventory
//! that gated the 0.68.0 map-binding flip reported ZERO sites for
//! `deploy_vhost.mix` — locally and on 27/27 fleet nodes — while line 283 of
//! that file is a two-variable loop over a MAP living inside such a body.

use cosmix_mix::analyzer::{AnalyzerConfig, Severity, analyze};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;

fn diags(source: &str) -> Vec<(String, Severity, Option<usize>, String)> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().expect("lex");
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program().expect("parse");
    analyze(&stmts, None, &AnalyzerConfig::default())
        .diagnostics
        .into_iter()
        .map(|d| (d.code.to_string(), d.severity, d.line, d.message))
        .collect()
}

fn codes(source: &str) -> Vec<String> {
    diags(source).into_iter().map(|(c, _, _, _)| c).collect()
}

#[test]
fn a_legacy_call_inside_a_remote_body_is_reported() {
    // Without this pass the file is clean: the whole program is one string.
    let src = "$h = \"alpha\"\n$r = ssh_mix($h, '\nprint(regex_match(\"^a\", \"abc\"))\n')\n";
    let found = diags(src);
    let d = found
        .iter()
        .find(|(c, ..)| c == "MIX-D3001")
        .expect("legacy regex name inside the body must be reported");
    assert!(
        d.3.contains("inside ssh_mix body"),
        "must say where it came from: {}",
        d.3
    );
}

#[test]
fn inner_line_numbers_map_into_the_enclosing_file() {
    // The mapping is `stmt_line + inner_line - 1`, which is exact for the
    // universal `$x = ssh_mix($HOST, '` shape: the literal's line 1 is the
    // statement's own line. Here the call is on line 2 and the offending
    // call is the body's 3rd line, so it must report as line 4 — the real
    // line of `regex_match` in this source.
    let src = "$h = \"alpha\"\n$r = ssh_mix($h, '\nprint(\"filler\")\nprint(regex_match(\"^a\", \"b\"))\n')\n";
    assert_eq!(src.lines().nth(3).unwrap().trim(), "print(regex_match(\"^a\", \"b\"))");
    let d = diags(src);
    let hit = d
        .iter()
        .find(|(c, ..)| c == "MIX-D3001")
        .expect("reported");
    assert_eq!(hit.2, Some(4), "must map onto the real source line");
}

#[test]
fn a_non_literal_body_is_reported_as_unanalysable_not_silently_clean() {
    // THE rule that matters. An inventory that silently counts an
    // unreadable body as clean is what produced the 0.68.0 near-miss; a
    // visible gap is worth more than the analysis it replaces.
    for body in ["$prog", "read_file(\"remote.mix\")", "$a .. $b"] {
        let src = format!("$h = \"alpha\"\n$r = ssh_mix($h, {body})\n");
        let c = codes(&src);
        assert!(
            c.iter().any(|x| x == "MIX-D3012"),
            "non-literal body {body:?} must raise the unanalysable note, got {c:?}"
        );
    }
}

#[test]
fn an_interpolated_body_is_unanalysable_too() {
    // Partly knowable is not knowable: the literal segments are Mix source
    // but the substitutions are holes, so parsing would report errors that
    // are artefacts of the holes rather than of the program.
    let src = "$h = \"a\"\n$x = 1\n$r = ssh_mix($h, \"print(${x})\")\n";
    assert!(codes(src).iter().any(|c| c == "MIX-D3012"));
}

#[test]
fn a_body_that_does_not_parse_is_reported_rather_than_swallowed() {
    // Five hub scripts were found in exactly this state — a boolean
    // condition split after a trailing `or` with no `\` continuation. The
    // remote would fail the same way, so silence here is the failure mode
    // this pass exists to remove.
    let src = "$h = \"a\"\n$r = ssh_mix($h, '\nif 1 == 1 or\n    2 == 2 then\nend\n')\n";
    let d = diags(src);
    let hit = d
        .iter()
        .find(|(c, ..)| c == "MIX-D3012")
        .expect("an unparsable body must be reported");
    assert!(hit.3.contains("did not parse"), "{}", hit.3);
}

#[test]
fn name_resolution_is_suppressed_inside_the_body() {
    // A remote body's free names come from `ssh_mix`'s `bindings` option and
    // from the remote's own environment — neither visible locally. Reporting
    // them would be pure noise, and a linter that cries wolf about remote
    // bodies gets switched off. The OUTER file keeps its own name checks.
    let src = "$h = \"alpha\"\n$r = ssh_mix($h, '\nprint($injected_by_bindings)\n', {bindings: {injected_by_bindings: 1}})\n";
    let c = codes(src);
    assert!(
        !c.iter().any(|x| x == "MIX-E1101"),
        "must not flag names the bindings option injects: {c:?}"
    );
}

#[test]
fn the_outer_files_own_name_checks_still_fire() {
    // The suppression must be scoped to the nested analysis only — a real
    // undefined name in the ENCLOSING file is still an error.
    let src = "$r = ssh_mix($undefined_host, '\nprint(1)\n')\n";
    let c = codes(src);
    assert!(
        c.iter().any(|x| x == "MIX-E1101"),
        "outer undefined name must still be reported: {c:?}"
    );
}

#[test]
fn a_clean_body_adds_nothing() {
    // False positives near zero is this analyzer's stated bias, and a pass
    // that fires on healthy remote code would be turned off within a week.
    let src = "$h = \"alpha\"\n$r = ssh_mix($h, '\n$n = 1\nprint($n)\n')\n";
    assert!(
        codes(src).is_empty(),
        "a clean remote body must be silent: {:?}",
        codes(src)
    );
}

#[test]
fn nested_ssh_mix_does_not_recurse_unboundedly() {
    // A remote body may itself call ssh_mix. The nested analysis runs with
    // name checks suppressed, and that same flag stops it descending again —
    // so this terminates instead of looping.
    let src =
        "$h = \"a\"\n$r = ssh_mix($h, '\n$q = ssh_mix(\"b\", \\'\nprint(regex_match(\"^a\", \"b\"))\n\\')\n')\n";
    let _ = codes(src); // must simply return
}
