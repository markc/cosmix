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
fn compound_address_clones_preserve_later_body_physical_mapping() {
    let src = "address \"target-prefix-\" .. \"1\"\n  command\n  other\nend\n$r = ssh_mix(\n  \"target\",\n  \"missing()\\n\"\n)\n";
    let found = diags_with_source(src);
    let body = found.iter().find(|(code, _, _, message)| {
        code == "MIX-E1102" && message.contains("inside ssh_mix body")
    }).expect("undefined call in remote body");
    assert_eq!(body.2, Some(7), "compound address must not lose later mapping: {found:?}");
}

#[test]
fn unrelated_tree_with_identical_literal_sequence_refuses_physical_origins() {
    let original = "$r = ssh_mix(\"target\", \"missing()\\n\")\n";
    let unrelated = "$other = ssh_mix(\n  \"target\",\n  \"missing()\\n\"\n)\n";
    let tokens = Lexer::new(original).tokenize().expect("lex");
    let stmts = Parser::new(tokens, original).parse_program().expect("parse");
    let cfg = AnalyzerConfig { source: Some(unrelated.to_string()), ..Default::default() };
    let result = analyze(&stmts, None, &cfg);
    let hit = result.diagnostics.iter().find(|d| {
        d.code == "MIX-E1102" && d.message.contains("inside ssh_mix body")
    }).expect("remote undefined call");
    assert_eq!(hit.line, Some(1), "source/tree mismatch must retain statement-line estimate");
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
fn names_injected_by_bindings_are_bound_inside_the_body() {
    // A remote body's free names come from `ssh_mix`'s `bindings` option,
    // which prepends a real `$name = value` assignment to the shipped
    // source. Those names are bound in the program that runs, so they must
    // never be reported. The OUTER file keeps its own name checks.
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
    // A remote body may itself call ssh_mix. The nested analysis does not
    // descend into its own ssh_mix bodies — so this terminates instead of
    // looping.
    let src =
        "$h = \"a\"\n$r = ssh_mix($h, '\n$q = ssh_mix(\"b\", \\'\nprint(regex_match(\"^a\", \"b\"))\n\\')\n')\n";
    // It must return, and the INNER body (the only place `regex_match`
    // appears) must stay unanalysed: one level deep only.
    let c = codes(src);
    assert!(!c.iter().any(|x| x == "MIX-D3001"), "inner body was analysed: {c:?}");
}

// ── heredoc bound once + `bindings` (TODO-mix, filed 2026-09-18) ──────

/// The manual's own fleet idiom (probe h1): the remote program bound ONCE to
/// a heredoc, shipped from a loop over hosts, with local values passed
/// through `bindings` and written bare in the body.
const H1: &str = "\
$base = \"/srv\"
$probe = <<END
$n = length($base)
print($n)
END
for $h in [\"alpha\", \"beta\"]
  $r = ssh_mix($h, $probe, {bindings: {base: $base}})
  print($r.ok)
end
";

#[test]
fn h1_the_bound_heredoc_idiom_lints_clean() {
    // It used to cost two false findings every time: MIX-W2402 on `$base`
    // (bare is exactly right: it is the remote binding) and MIX-D3012 (it
    // IS a literal, merely bound to a name first).
    let c = codes(H1);
    assert!(c.is_empty(), "h1 must lint clean, got {c:?}");
}

#[test]
fn a_heredoc_bound_once_is_analysed_at_its_own_lines() {
    // Resolved through the variable, the body is linted — and a finding
    // points at the heredoc line that holds it (line 4 here), not at the
    // call.
    let src = "$h = \"alpha\"\n$probe = <<END\n$m = {a: []}\npush($m[\"a\"], 1)\nprint($m)\nEND\n$r = ssh_mix($h, $probe)\n";
    assert_eq!(src.lines().nth(3).unwrap(), "push($m[\"a\"], 1)");
    let d = diags(src);
    let hit = d
        .iter()
        .find(|(c, ..)| c == "MIX-E1501")
        .expect("the resolved body must be analysed");
    assert_eq!(hit.2, Some(4), "{d:?}");
    assert!(hit.3.contains("inside ssh_mix body"), "{}", hit.3);
    assert!(!d.iter().any(|(c, ..)| c == "MIX-D3012"), "{d:?}");
}

#[test]
fn a_shared_heredoc_reports_each_finding_once() {
    let src = "$probe = <<END\n$m = {a: []}\npush($m[\"a\"], 1)\nprint($m)\nEND\nfor $h in [\"a\", \"b\"]\n  $r = ssh_mix($h, $probe)\nend\n$s = ssh_mix(\"c\", $probe)\n";
    let n = codes(src).iter().filter(|c| *c == "MIX-E1501").count();
    assert_eq!(n, 1, "{:?}", codes(src));
}

#[test]
fn a_shared_heredoc_with_different_bindings_still_reports_each_finding_once() {
    // Three calls, three different bindings sets: each is analysed (they
    // could disagree about undefined names), but the finding they share is
    // one finding.
    let src = "$probe = <<END\n$m = {a: []}\npush($m[\"a\"], 1)\nprint($m)\nEND\n$r1 = ssh_mix(\"a\", $probe, {bindings: {x: 1}})\n$r2 = ssh_mix(\"b\", $probe, {bindings: {y: 2}})\n$r3 = ssh_mix(\"c\", $probe)\n";
    let n = codes(src).iter().filter(|c| *c == "MIX-E1501").count();
    assert_eq!(n, 1, "{:?}", codes(src));
}

#[test]
fn two_different_bodies_on_one_line_keep_a_finding_each() {
    // Distinct bodies whose findings land on the same line with the same
    // text are still two findings: the dedupe is per body.
    for src in [
        "$r = [ssh_mix(\"a\", 'missing()'), ssh_mix(\"b\", 'missing()')]\nprint($r)\n",
        "$r = ssh_mix(\"a\", 'missing()'); $s = ssh_mix(\"b\", 'missing()')\nprint($r .. $s)\n",
    ] {
        let n = codes(src).iter().filter(|c| *c == "MIX-E1102").count();
        assert_eq!(n, 2, "{src:?}: {:?}", codes(src));
    }
}

#[test]
fn a_variable_bound_more_than_once_is_not_resolved() {
    // "Sole definition" is the guarantee: with two binders the value at
    // the call is not knowable, so the body stays unanalysable.
    let src = "$p = 'print(1)'\nif 1 == 1 then\n  $p = 'print(2)'\nend\n$r = ssh_mix(\"a\", $p)\n";
    assert!(codes(src).iter().any(|c| c == "MIX-D3012"), "{:?}", codes(src));
    // A parameter is a binder too.
    let src = "$p = 'print(1)'\nfn go($p)\n  return ssh_mix(\"a\", $p)\nend\nprint(go($p))\n";
    assert!(codes(src).iter().any(|c| c == "MIX-D3012"), "{:?}", codes(src));
}

#[test]
fn a_braced_local_splice_in_the_body_still_warns() {
    // `${base}` interpolates the LOCAL value into the remote source — the
    // classic bug — so the body is not a literal and says so, naming it.
    let src = "$base = \"/srv\"\n$probe = <<END\nprint(\"${base}\")\nEND\n$r = ssh_mix(\"a\", $probe, {bindings: {base: $base}})\n";
    let d = diags(src);
    let hit = d
        .iter()
        .find(|(c, ..)| c == "MIX-D3012")
        .expect("an interpolated body must still be reported");
    assert!(hit.3.contains("${base}"), "must name the splice: {}", hit.3);
}

#[test]
fn an_unbound_name_in_the_body_is_flagged() {
    // With the bindings map readable, the body's universe is known: its
    // own binders, the builtins, and the bindings keys. Anything else is
    // undefined on the remote.
    let src = "$base = \"/srv\"\n$probe = <<END\nprint($base .. $notbound)\nEND\n$r = ssh_mix(\"a\", $probe, {bindings: {base: $base}})\n";
    let d = diags(src);
    let e1101: Vec<_> = d.iter().filter(|(c, ..)| c == "MIX-E1101").collect();
    assert_eq!(e1101.len(), 1, "{d:?}");
    assert!(e1101[0].3.contains("$notbound"), "{}", e1101[0].3);
    assert!(e1101[0].3.contains("inside ssh_mix body"), "{}", e1101[0].3);
}

#[test]
fn w2402_is_silenced_for_bindings_names_only() {
    // `$target` is bound locally but NOT passed as a binding: bare in the
    // body it is undefined remotely, so W2402 keeps firing for it (and the
    // body's own E1101 names it) while `$base` stays silent.
    let src = "$base = \"/srv\"\n$target = \"x\"\n$probe = <<END\nprint($base .. $target)\nEND\n$r = ssh_mix(\"a\", $probe, {bindings: {base: $base}})\n";
    let d = diags(src);
    let w: Vec<_> = d.iter().filter(|(c, ..)| c == "MIX-W2402").collect();
    assert_eq!(w.len(), 1, "{d:?}");
    assert!(w[0].3.contains("$target"), "{}", w[0].3);
    // An ordinary text heredoc is untouched.
    let src = "$base = \"/srv\"\n$conf = <<END\nroot $base\nEND\nprint($conf)\n";
    assert!(codes(src).iter().any(|c| c == "MIX-W2402"));
}

#[test]
fn w2402_is_silent_for_a_remote_functions_params_and_locals() {
    // `$x` and `$y` are bound locally too, but inside the body they are a
    // remote fn's parameter and local. Advising `${x}` would splice the
    // local value into the remote function.
    let src = "$x = 1\n$y = 2\n$probe = <<END\nfn identity($x)\n  $y = $x\n  return $y\nend\nprint(identity(3))\nEND\n$r = ssh_mix(\"a\", $probe)\n";
    let d = diags(src);
    assert!(!d.iter().any(|(c, ..)| c == "MIX-W2402"), "{d:?}");
}

#[test]
fn a_body_e1101_hint_points_at_bindings_not_allow_global() {
    // Inside a body, env() reads the REMOTE environment and --allow-global
    // only silences lint; the fix that ships a value is `bindings`.
    let src = "$z = 1\n$r = ssh_mix(\"a\", 'print($z)')\n";
    let stmts = Parser::new(Lexer::new(src).tokenize().unwrap(), src)
        .parse_program()
        .unwrap();
    let hint = analyze(&stmts, None, &AnalyzerConfig::default())
        .diagnostics
        .into_iter()
        .find(|d| d.code == "MIX-E1101")
        .and_then(|d| d.hint)
        .expect("E1101 with a hint");
    assert!(hint.contains("{bindings: {z: …}}"), "{hint}");
    assert!(hint.contains("REMOTE environment"), "{hint}");
    assert!(!hint.contains("--allow-global"), "{hint}");
}

#[test]
fn env_keys_are_bound_inside_the_body_too() {
    // `env` ships as prepended `export KEY = "value"` lines.
    let src = "$r = ssh_mix(\"a\", '\nprint($FOO)\n', {env: {FOO: \"1\"}})\n";
    assert!(codes(src).is_empty(), "{:?}", codes(src));
}

#[test]
fn an_opts_variable_bound_once_to_a_map_literal_resolves() {
    // The opts twin of the body's sole-string resolution: `$o` assigned
    // ONCE to a map literal supplies its bindings keys statically, so the
    // body gets full name checks — no uncertainty note (MIX-D3018), and a
    // genuinely undefined name is still caught.
    let src = "$o = {bindings: {x: 1}}\n$r = ssh_mix(\"a\", '\nprint($x)\n', $o)\n";
    assert!(codes(src).is_empty(), "{:?}", codes(src));
    let src = "$o = {bindings: {x: 1}}\n$r = ssh_mix(\"a\", '\nprint($x .. $y)\n', $o)\n";
    let c = codes(src);
    assert!(c.iter().any(|x| x == "MIX-E1101"), "full checks must run: {c:?}");
}

#[test]
fn unreadable_opts_skip_variable_checks_but_keep_callable_checks() {
    // The boundary: opts is a CALL, so the injected names are unknown.
    // `$x` could be supplied by dynamic bindings (no E1101), but
    // `missing_fn()` cannot ride in through strict-data bindings (E1102
    // fires) and the body's own arity mistake fires too (E1202). The skip
    // is reported, not silent (MIX-D3018).
    let src = "$o = read_file(\"opts.mix\")\n$r = ssh_mix(\"a\", '\nprint($x)\nmissing_fn()\nfn helper($n)\n  return $n\nend\nprint(helper())\n', $o)\n";
    let c = codes(src);
    assert!(!c.iter().any(|x| x == "MIX-E1101"), "{c:?}");
    assert!(c.iter().any(|x| x == "MIX-E1102"), "{c:?}");
    assert!(c.iter().any(|x| x == "MIX-E1202"), "{c:?}");
    assert!(c.iter().any(|x| x == "MIX-D3018"), "{c:?}");
}

#[test]
fn a_typo_suspect_in_an_unknown_opts_body_gets_a_note() {
    // MIX-D3017: `$pott` cannot be proved undefined (dynamic bindings may
    // supply it), but it is one edit from the body's own parameter
    // `$port` — the typo is the likelier explanation, so lint records it
    // instead of passing silently. One edit is one edit: an adjacent
    // TRANSPOSITION (`$prot`) is TWO edits under plain Levenshtein, and
    // the short-name threshold is deliberately one, so it stays silent —
    // broadening it would flood every 4-character dynamic binding with
    // invented neighbours. A name with no near neighbour (a plain
    // dynamic binding) stays silent.
    let src = "$o = read_file(\"opts.mix\")\n$r = ssh_mix(\"a\", '\nfn connect($port)\n  return $pott\nend\nprint(connect(22))\n', $o)\n";
    let d = diags(src);
    let note = d
        .iter()
        .find(|(c, ..)| c == "MIX-D3017")
        .expect("typo-suspect note");
    assert!(note.3.contains("$pott"), "{}", note.3);
    assert!(note.3.contains("did you mean '$port'"), "{}", note.3);
    assert!(!d.iter().any(|(c, ..)| c == "MIX-E1101"), "{d:?}");
    // `$x` alone has nothing near it in the body: silent, no invented
    // finding.
    let src = "$o = read_file(\"opts.mix\")\n$r = ssh_mix(\"a\", '\nprint($x)\n', $o)\n";
    assert!(!codes(src).iter().any(|c| c == "MIX-D3017"), "{:?}", codes(src));
}

#[test]
fn w2402_stands_down_when_the_bodys_opts_are_unreadable() {
    // "did you mean `${target}`?" splices the LOCAL value into remote
    // source — only safe when the name universe is known. With unreadable
    // opts any free name could be a remote binding, so the whole heredoc
    // stands down; the unreadable opts are reported as MIX-D3018 instead.
    let src = "$target = \"x\"\n$probe = <<END\nprint($target)\nEND\n$o = read_file(\"o.mix\")\n$r = ssh_mix(\"a\", $probe, $o)\n";
    let c = codes(src);
    assert!(!c.iter().any(|x| x == "MIX-W2402"), "{c:?}");
    assert!(!c.iter().any(|x| x == "MIX-E1101"), "{c:?}");
    assert!(c.iter().any(|x| x == "MIX-D3018"), "{c:?}");
    // The control: readable opts keep the warning for a name that is NOT
    // injected.
    let src = "$target = \"x\"\n$probe = <<END\nprint($target)\nEND\n$r = ssh_mix(\"a\", $probe, {bindings: {other: 1}})\n";
    assert!(codes(src).iter().any(|c| c == "MIX-W2402"), "{:?}", codes(src));
}

#[test]
fn an_undefined_function_in_the_body_is_reported_with_its_suggestion() {
    // Probe h4: the remote half failing on a typo is the failure this pass
    // exists to catch. A body cannot see the outer file's functions.
    let src = "fn helper()\n  return 1\nend\n$r = ssh_mix(\"a\", '\nprint(json_decode(\"{}\"))\nprint(helper())\n')\nprint(helper())\n";
    let d = diags(src);
    let e1102: Vec<_> = d.iter().filter(|(c, ..)| c == "MIX-E1102").collect();
    assert_eq!(e1102.len(), 2, "{d:?}");
    assert!(e1102.iter().all(|x| x.3.contains("inside ssh_mix body")));
    assert!(e1102.iter().any(|x| x.3.contains("json_decode")));
    assert!(e1102.iter().any(|x| x.3.contains("helper")));
    // "defined nowhere in this file" would be false: helper IS defined in
    // this file. The message names the real scope.
    assert!(
        e1102.iter().all(|x| x.3.contains("outer-file functions do not ship")),
        "{e1102:?}"
    );
    let e1101 = diags("$z = 1\n$r = ssh_mix(\"a\", 'print($z)')\n");
    assert!(
        e1101.iter().any(|x| x.0 == "MIX-E1101" && x.3.contains("outer-file variables do not ship")),
        "{e1101:?}"
    );
}

#[test]
fn a_call_inside_a_loop_is_linted() {
    // The loop-over-hosts shape. The 0.69.0 pass searched top-level
    // statements only, so exactly this was invisible.
    let src = "for $h in [\"a\"]\n  $r = ssh_mix($h, '\n$m = {a: []}\npush($m[\"a\"], 1)\nprint($m)\n')\nend\n";
    assert!(codes(src).iter().any(|c| c == "MIX-E1501"), "{:?}", codes(src));
}

/// A body with one finding (E1501) that needs no names from outside.
const LOST_PUSH: &str = "'\n$m = {a: []}\npush($m[\"a\"], 1)\nprint($m)\n'";

#[test]
fn calls_are_found_in_every_expression_position() {
    // The general walkers skip these positions on purpose (their callers
    // handle scope themselves); the body pass must not.
    for (what, src) in [
        (
            "if-expression condition",
            format!("$v = if ssh_mix(\"a\", {LOST_PUSH}).ok then 1 else 0 end\nprint($v)\n"),
        ),
        (
            "lambda parameter default",
            format!("$f = function($r = ssh_mix(\"a\", {LOST_PUSH})) = $r\nprint($f())\n"),
        ),
        (
            "expression lambda body",
            format!("$f = function($h) = ssh_mix($h, {LOST_PUSH})\nprint($f(\"a\"))\n"),
        ),
        (
            "named fn = expr body",
            format!("fn go($h) = ssh_mix($h, {LOST_PUSH})\nprint(go(\"a\"))\n"),
        ),
        (
            "named fn parameter default",
            format!("fn go($r = ssh_mix(\"a\", {LOST_PUSH}))\n  return $r\nend\nprint(go())\n"),
        ),
    ] {
        let c = codes(&src);
        assert!(c.iter().any(|x| x == "MIX-E1501"), "{what}: body not analysed: {c:?}");
    }
}

#[test]
fn a_named_fn_expression_body_resolves_names_against_its_bindings() {
    let src = "fn go($x) = ssh_mix(\"a\", 'print($x .. $missing)', {bindings: {x: $x}})\nprint(go(1))\n";
    let d = diags(src);
    let e1101: Vec<_> = d.iter().filter(|(c, ..)| c == "MIX-E1101").collect();
    assert_eq!(e1101.len(), 1, "{d:?}");
    assert!(e1101[0].3.contains("$missing"), "{}", e1101[0].3);
}

#[test]
fn a_lambda_parameter_is_a_binder() {
    // `$p` has a heredoc assignment AND a lambda parameter of that name, so
    // it is bound twice and must not resolve.
    let src = "$p = 'print(1)'\n$f = function($p) = ssh_mix(\"a\", $p)\nprint($f(\"x\"))\n";
    assert!(codes(src).iter().any(|c| c == "MIX-D3012"), "{:?}", codes(src));
}

#[test]
fn another_functions_local_is_not_resolved() {
    // `$q` is bound once, but as a local of build(); inside ship() it is
    // undefined at runtime, so its heredoc never ships from there.
    let src = format!(
        "fn build()\n  $q = {LOST_PUSH}\n  return $q\nend\nfn ship($h)\n  return ssh_mix($h, $q)\nend\nprint(build())\nprint(ship(\"a\"))\n"
    );
    let c = codes(&src);
    assert!(!c.iter().any(|x| x == "MIX-E1501"), "resolved across frames: {c:?}");
    assert!(c.iter().any(|x| x == "MIX-D3012"), "{c:?}");
    // The same local used in its OWN frame does resolve, and so does a
    // top-level binding read from inside a function.
    let own = format!("fn ship($h)\n  $q = {LOST_PUSH}\n  return ssh_mix($h, $q)\nend\nprint(ship(\"a\"))\n");
    assert!(codes(&own).iter().any(|x| x == "MIX-E1501"), "{:?}", codes(&own));
    let top = format!("$q = {LOST_PUSH}\nfn ship($h)\n  return ssh_mix($h, $q)\nend\nprint(ship(\"a\"))\n");
    assert!(codes(&top).iter().any(|x| x == "MIX-E1501"), "{:?}", codes(&top));
}

#[test]
fn a_heredoc_shipped_from_a_lambda_in_its_own_fn_is_resolved() {
    // Lambdas are closures: `$probe` bound in uptimes() is visible inside
    // the `map` lambda. The body must be analysed, and W2402 must not fire
    // on the body's own `$out`.
    let src = "$out = []\nfn uptimes($hosts)\n  $probe = <<END\n$out = run(\"uptime\")\n$m = {a: []}\npush($m[\"a\"], 1)\nprint($out .. $m)\nEND\n  return map($hosts, function($h) = ssh_mix($h, $probe))\nend\nprint(uptimes([\"a\"]))\n";
    let d = diags(src);
    assert!(d.iter().any(|(c, ..)| c == "MIX-E1501"), "body not analysed: {d:?}");
    assert!(!d.iter().any(|(c, ..)| c == "MIX-D3012"), "{d:?}");
    assert!(!d.iter().any(|(c, ..)| c == "MIX-W2402"), "{d:?}");
}

#[test]
fn a_named_nested_fn_does_not_see_its_parents_local() {
    // A named fn is NOT a closure (probed: NAME_UNDEFINED), so its call
    // must not resolve to the enclosing fn's heredoc.
    let src = format!(
        "fn outer()\n  $q = {LOST_PUSH}\n  fn inner($h)\n    return ssh_mix($h, $q)\n  end\n  return inner(\"a\")\nend\nprint(outer())\n"
    );
    let c = codes(&src);
    assert!(!c.iter().any(|x| x == "MIX-E1501"), "{c:?}");
    assert!(c.iter().any(|x| x == "MIX-D3012"), "{c:?}");
}

#[test]
fn a_file_with_source_or_include_resolves_nothing() {
    // The loaded file can rebind anything, so "sole binder" is unknowable.
    let src = "source(\"other.mix\")\n$p = 'print(1)'\n$r = ssh_mix(\"a\", $p)\n";
    assert!(codes(src).iter().any(|c| c == "MIX-D3012"), "{:?}", codes(src));
}

/// As `diags`, with the source text supplied the way `mix lint` does.
fn diags_with_source(source: &str) -> Vec<(String, Severity, Option<usize>, String)> {
    let tokens = Lexer::new(source).tokenize().expect("lex");
    let stmts = Parser::new(tokens, source).parse_program().expect("parse");
    let cfg = AnalyzerConfig {
        source: Some(source.to_string()),
        ..AnalyzerConfig::default()
    };
    analyze(&stmts, None, &cfg)
        .diagnostics
        .into_iter()
        .map(|d| (d.code.to_string(), d.severity, d.line, d.message))
        .collect()
}

#[test]
fn a_body_opened_below_the_statement_line_maps_to_its_real_lines() {
    // The opener sits on line 3, so `push` is on file line 5 — not the
    // line 3 (heredoc) or 3 (string) the statement line alone would give.
    for src in [
        "$r = ssh_mix(\n  \"alpha\",\n  <<EOF\n$m = {a: []}\npush($m[\"a\"], 1)\nprint($m)\nEOF\n)\n",
        "$r = ssh_mix(\n  \"alpha\",\n  '\n$m = {a: []}\npush($m[\"a\"], 1)\nprint($m)\n')\n",
    ] {
        assert_eq!(src.lines().nth(4).unwrap(), "push($m[\"a\"], 1)");
        let d = diags_with_source(src);
        let hit = d.iter().find(|(c, ..)| c == "MIX-E1501").expect("analysed");
        assert_eq!(hit.2, Some(5), "{src:?}: {d:?}");
    }
}

#[test]
fn identical_bodies_in_one_statement_map_to_their_own_lines() {
    // Two identical literals in one statement: the second must not borrow
    // the first one's opener. `missing()` sits on lines 2 and 4.
    let src = "$r = [ssh_mix(\"a\", '\nmissing()\n'), ssh_mix(\"b\", '\nmissing()\n')]\nprint($r)\n";
    assert_eq!(src.lines().nth(1).unwrap(), "missing()");
    assert_eq!(src.lines().nth(3).unwrap(), "missing()");
    let d = diags_with_source(src);
    let mut lines: Vec<_> = d
        .iter()
        .filter(|(c, ..)| c == "MIX-E1102")
        .map(|x| x.2)
        .collect();
    lines.sort();
    assert_eq!(lines, vec![Some(2), Some(4)], "{d:?}");
}

#[test]
fn an_inline_heredoc_body_is_analysed() {
    // The manual's headline idiom writes the heredoc inline; it used to
    // count as "not a literal".
    let src = "$r = ssh_mix(\"alpha\", <<EOF\n$m = {a: []}\npush($m[\"a\"], 1)\nprint($m)\nEOF\n)\n";
    let d = diags(src);
    let hit = d.iter().find(|(c, ..)| c == "MIX-E1501").expect("analysed");
    assert_eq!(hit.2, Some(3), "{d:?}");
    assert!(!d.iter().any(|(c, ..)| c == "MIX-D3012"), "{d:?}");
}

#[test]
fn an_ssh_mix_many_body_is_analysed_like_an_ssh_mix_body() {
    // `ssh_mix_many(hosts, source[, opts])` ships its second argument to
    // every host. Unlinted, a fan-out's whole remote half would be the blind
    // spot this file exists to close — for N hosts at once.
    let src = "$r = ssh_mix_many([\"alpha\", \"beta\"], '\nprint(regex_match(\"^a\", \"abc\"))\n')\n";
    let d = diags(src);
    let hit = d
        .iter()
        .find(|(c, ..)| c == "MIX-D3001")
        .expect("legacy regex name inside an ssh_mix_many body must be reported");
    assert!(hit.3.contains("inside ssh_mix body"), "{}", hit.3);
    assert_eq!(hit.2, Some(2), "{d:?}");

    // Its bindings are the remote program's names exactly as for ssh_mix:
    // `$base` is bound, `$target` is not.
    let src = "$base = \"/srv\"\n$target = \"x\"\n$probe = <<END\nprint($base .. $target)\nEND\n$r = ssh_mix_many([\"a\", \"b\"], $probe, {bindings: {base: $base}, max: 2})\n";
    let d = diags(src);
    let w: Vec<_> = d.iter().filter(|(c, ..)| c == "MIX-W2402").collect();
    assert_eq!(w.len(), 1, "{d:?}");
    assert!(w[0].3.contains("$target"), "{}", w[0].3);

    // And an unreadable body is reported, not silently clean.
    let c = codes("$r = ssh_mix_many([\"a\"], $a .. $b)\n");
    assert!(c.iter().any(|x| x == "MIX-D3012"), "{c:?}");
}

// ── decoded-line → physical-line mapping (TODO-mix 2026-09-25 residual) ─

#[test]
fn a_double_quoted_escaped_body_reports_on_its_one_physical_line() {
    // THE bug this closes: `\n` escapes decode to inner lines that have
    // no physical line of their own — every diagnostic must report on the
    // literal's single physical line (1 here), not spread across
    // invented lines.
    let src = "$r = ssh_mix(\"a\", \"print(1)\\nprint($typo)\")\n";
    assert_eq!(src.lines().count(), 1);
    let d = diags_with_source(src);
    let e1101: Vec<_> = d.iter().filter(|(c, ..)| c == "MIX-E1101").collect();
    assert_eq!(e1101.len(), 1, "{d:?}");
    assert_eq!(e1101[0].2, Some(1), "{d:?}");
}

#[test]
fn a_physically_multiline_double_quoted_body_still_maps_one_to_one() {
    // Real newlines are real lines — the map must not collapse them.
    let src = "$r = ssh_mix(\"a\", \"print(1)\nprint($typo)\")\n";
    assert_eq!(src.lines().count(), 2);
    let d = diags_with_source(src);
    let hit = d.iter().find(|(c, ..)| c == "MIX-E1101").expect("reported");
    assert_eq!(hit.2, Some(2), "{d:?}");
}

#[test]
fn a_heredoc_escape_reports_on_the_escapes_physical_line() {
    // `\n` inside a heredoc body adds a decoded line with no physical
    // one: the decoded line-2 finding lands on the heredoc line the
    // escape was written on (line 2), not on an invented line 3.
    let src = "$probe = <<END\nprint(1)\\nprint($typo)\nEND\n$r = ssh_mix(\"a\", $probe)\n";
    assert_eq!(src.lines().nth(1).unwrap(), "print(1)\\nprint($typo)");
    let d = diags_with_source(src);
    let hit = d.iter().find(|(c, ..)| c == "MIX-E1101").expect("reported");
    assert_eq!(hit.2, Some(2), "{d:?}");
}

#[test]
fn a_unicode_escape_newline_stays_on_its_physical_line() {
    // `\u{000A}` decodes to a newline with no physical line of its own —
    // the same rule as `\n`, through a different escape arm.
    let src = "$r = ssh_mix(\"a\", \"print(1)\\u{000A}print($typo)\")\n";
    let d = diags_with_source(src);
    let hit = d.iter().find(|(c, ..)| c == "MIX-E1101").expect("reported");
    assert_eq!(hit.2, Some(1), "{d:?}");
}

#[test]
fn a_parse_error_in_an_escaped_body_maps_to_the_physical_line() {
    // The unparsable-body finding carries the error's OWN inner line,
    // mapped like any other diagnostic — not the body's first line.
    let src = "$r = ssh_mix(\"a\", \"if 1 == 1 or\\n  2 == 2 then\\nend\")\n";
    let d = diags_with_source(src);
    let hit = d
        .iter()
        .find(|(c, ..)| c == "MIX-D3012")
        .expect("an unparsable body must be reported");
    assert!(hit.3.contains("did not parse"), "{}", hit.3);
    assert_eq!(hit.2, Some(1), "{d:?}");
}

#[test]
fn a_lexer_error_in_an_escaped_body_maps_to_the_physical_line() {
    // An unterminated single-quoted string on the body's second decoded
    // line: the finding lands on the ONE physical line the body occupies.
    let src = "$r = ssh_mix(\"a\", \"print(1)\\n$s = 'oops\")\n";
    let d = diags_with_source(src);
    let hit = d
        .iter()
        .find(|(c, ..)| c == "MIX-D3012")
        .expect("an unlexable body must be reported");
    assert!(hit.3.contains("did not parse"), "{}", hit.3);
    assert_eq!(hit.2, Some(1), "{d:?}");
}

#[test]
fn identical_escaped_bodies_in_one_statement_keep_a_finding_each() {
    // Two identical escaped literals in one statement: one finding per
    // body, both on the single physical line they share.
    let src = "$r = [ssh_mix(\"a\", \"missing()\\n\"), ssh_mix(\"b\", \"missing()\\n\")]\nprint($r)\n";
    let d = diags_with_source(src);
    let lines: Vec<_> = d
        .iter()
        .filter(|(c, ..)| c == "MIX-E1102")
        .map(|x| x.2)
        .collect();
    assert_eq!(lines, vec![Some(1), Some(1)], "{d:?}");
}

#[test]
fn identical_escaped_bodies_on_distinct_lines_map_to_their_own_lines() {
    // Three identical escaped bodies on three distinct physical lines,
    // plus an UNRELATED ordinary string of the same content on the line
    // above them: each body must claim its own opener. A per-text
    // ordinal over the pre-filtered opener list used to overshoot once
    // the statement-line filter thinned it (the second body borrowed the
    // third's opener, the third fell back to the estimate), and the
    // ordinary string must not be claimed by any of them.
    let src = "$plain = \"missing()\\n\"\n$r = ssh_mix(\"a\", \"missing()\\n\")\n$s = ssh_mix(\"b\", \"missing()\\n\")\n$t = ssh_mix(\"c\", \"missing()\\n\")\nprint($r)\n";
    assert_eq!(src.lines().count(), 5);
    let d = diags_with_source(src);
    let lines: Vec<_> = d
        .iter()
        .filter(|(c, ..)| c == "MIX-E1102")
        .map(|x| x.2)
        .collect();
    assert_eq!(lines, vec![Some(2), Some(3), Some(4)], "{d:?}");
}

#[test]
fn a_once_bound_body_shipped_with_different_opts_keeps_one_source_map() {
    // The same variable-bound literal shipped twice with DIFFERENT
    // bindings is ONE body, analysed twice (the findings differ: each
    // binding covers one of the two reads) — but it has ONE physical
    // source, the assignment's literal on line 1, so both findings
    // report there. The second call must not claim another opener (there
    // is none) and must not fall back to its own call line.
    let src = "$body = \"print($x .. $y)\\n\"\n$r = ssh_mix(\"a\", $body, {bindings: {x: 1}})\n$s = ssh_mix(\"b\", $body, {bindings: {y: 2}})\n";
    let d = diags_with_source(src);
    let e1101: Vec<_> = d.iter().filter(|(c, ..)| c == "MIX-E1101").collect();
    assert_eq!(e1101.len(), 2, "{d:?}");
    assert_eq!(e1101[0].2, Some(1), "{d:?}");
    assert_eq!(e1101[1].2, Some(1), "{d:?}");
    assert!(e1101.iter().any(|x| x.3.contains("$x")), "{e1101:?}");
    assert!(e1101.iter().any(|x| x.3.contains("$y")), "{e1101:?}");
}

#[test]
fn without_source_text_the_linear_estimate_is_pinned() {
    // `analyze()` without `AnalyzerConfig::source` cannot tell an escape
    // from a physical newline. The documented estimate
    // `first_line + inner - 1` stands — exact for a physical multi-line
    // literal, best effort for an escaped one. Pinned so changing the
    // convention is a conscious diff, not drift.
    let src = "$h = \"a\"\n$r = ssh_mix($h, \"print(1)\\nprint($typo)\")\n";
    let d = diags(src); // default config: no source text
    let hit = d.iter().find(|(c, ..)| c == "MIX-E1101").expect("reported");
    assert_eq!(hit.2, Some(3), "{d:?}"); // stmt line 2 + inner line 2 - 1
}

#[test]
fn ordinary_literal_before_identical_remote_body_in_one_statement_does_not_steal_map() {
    let src = "$r = [\"missing()\n\", ssh_mix(\"a\", \"missing()\\n\")]\n";
    let d = diags_with_source(src);
    let lines: Vec<_> = d.iter().filter(|(c, ..)| c == "MIX-E1102").map(|x| x.2).collect();
    assert_eq!(lines, vec![Some(2)], "{d:?}");
}

#[test]
fn identical_quoted_map_key_before_remote_value_does_not_steal_map() {
    let src = "$r = {\"missing()\n\": ssh_mix(\"a\", \"missing()\\n\")}\n";
    let d = diags_with_source(src);
    let lines: Vec<_> = d.iter().filter(|(c, ..)| c == "MIX-E1102").map(|x| x.2).collect();
    assert_eq!(lines, vec![Some(2)], "{d:?}");
}

#[test]
fn branch_body_literals_claim_maps_before_later_branch_conditions() {
    let src = "if true then\n  $plain = \"missing()\n\"\nelse if ssh_mix(\"a\", \"missing()\\n\") then\n  print(1)\nend\n";
    let d = diags_with_source(src);
    let lines: Vec<_> = d.iter().filter(|(c, ..)| c == "MIX-E1102").map(|x| x.2).collect();
    assert_eq!(lines, vec![Some(4)], "{d:?}");
}

// ── literal-origin pairing: non-expression strings cannot steal a map ──

#[test]
fn a_bare_keyword_map_key_does_not_steal_an_identical_bodys_map() {
    // The blocker this pairing closes: a bare map key (`if` — keywords
    // are legal bare keys) is a KEY, not an expression literal. It must
    // not consume the opener of the identical remote body written later
    // in the same statement, or the body's parse error would fall back
    // to the enclosing statement line (1) instead of the quoted body's
    // own physical line (2).
    let src = "$r = {\n  if: ssh_mix(\"a\", \"if\")\n}\nprint($r)\n";
    assert_eq!(src.lines().nth(1).unwrap(), "  if: ssh_mix(\"a\", \"if\")");
    let d = diags_with_source(src);
    let hit = d.iter().find(|(c, ..)| c == "MIX-D3012").expect("`if` alone is not Mix");
    assert!(hit.3.contains("did not parse"), "{}", hit.3);
    assert_eq!(hit.2, Some(2), "{d:?}");
}

#[test]
fn identical_text_in_condition_and_body_map_to_their_own_lines() {
    // The same decoded text in a branch CONDITION and inside the branch's
    // BODY: each literal keeps its own opener in source order —
    // conditions are walked before their bodies, bodies before later
    // conditions (the general scope walker would swap these).
    let src = "if ssh_mix(\"a\", \"missing()\\n\") then\n  ssh_mix(\"b\", \"missing()\\n\")\nend\nprint(1)\n";
    let d = diags_with_source(src);
    let lines: Vec<_> = d.iter().filter(|(c, ..)| c == "MIX-E1102").map(|x| x.2).collect();
    assert_eq!(lines, vec![Some(1), Some(2)], "{d:?}");
}

#[test]
fn a_quoted_parse_delimiter_does_not_claim_a_body_map() {
    // A quoted `parse … with "<delim>"` text is a parse PART, not an
    // expression literal: identical decoded text must not steal the
    // opener of the remote body on the next line.
    let src = "parse $s with \"missing()\\n\" to $y\n$r = ssh_mix(\"a\", \"missing()\\n\")\nprint($r)\n";
    let d = diags_with_source(src);
    let lines: Vec<_> = d.iter().filter(|(c, ..)| c == "MIX-E1102").map(|x| x.2).collect();
    assert_eq!(lines, vec![Some(2)], "{d:?}");
}

#[test]
fn an_on_doc_string_does_not_claim_a_body_map() {
    // The `on … desc "<doc>"` string is handler metadata, not an
    // expression literal: identical text must not steal the opener of
    // the remote body below it.
    let src = "on foo desc \"missing()\\n\"\n  print(1)\nend\n$r = ssh_mix(\"a\", \"missing()\\n\")\nprint($r)\n";
    let d = diags_with_source(src);
    let lines: Vec<_> = d.iter().filter(|(c, ..)| c == "MIX-E1102").map(|x| x.2).collect();
    assert_eq!(lines, vec![Some(4)], "{d:?}");
}
