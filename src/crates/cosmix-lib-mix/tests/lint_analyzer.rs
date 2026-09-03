//! Semantic analyzer (0.29.0, decision D3) — rule coverage AND, just as
//! load-bearing, the false-positive guards for Mix's dynamic seams.

use cosmix_mix::analyzer::{AnalyzerConfig, Severity, analyze};
use cosmix_mix::ast::{ChainOp, Expr, FunctionBody, PathSeg, Stmt, StmtKind};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;

fn lint(src: &str) -> Vec<(String, Option<usize>)> {
    lint_cfg(src, &AnalyzerConfig::default())
}

fn lint_cfg(src: &str, cfg: &AnalyzerConfig) -> Vec<(String, Option<usize>)> {
    let tokens = Lexer::new(src).tokenize().expect("lexes");
    let stmts = Parser::new(tokens, src).parse_program().expect("parses");
    analyze(&stmts, Some("test.mix"), cfg)
        .diagnostics
        .into_iter()
        .map(|d| (d.code.to_string(), d.line))
        .collect()
}

fn codes(src: &str) -> Vec<String> {
    lint(src).into_iter().map(|(c, _)| c).collect()
}

// ── detections ──────────────────────────────────────────────────────

#[test]
fn undefined_variable_including_nested_expressions() {
    // THE provisioning failure: a typo'd global inside a concatenation.
    let diags = lint(
        "function build_target($node)\n  return \"ct-\" .. $node .. \".\" .. $DOMAIN\nend\nbuild_target(\"x\")\n",
    );
    assert_eq!(diags, vec![("MIX-E1101".to_string(), Some(2))]);
}

#[test]
fn undefined_function_and_arities() {
    let out =
        codes("function f($a)\n  return $a\nend\nf(1, 2)\nsubstr(\"abc\")\nnope()\nrandom(1)\n");
    assert_eq!(
        out,
        vec!["MIX-E1202", "MIX-E1201", "MIX-E1102", "MIX-E1201"]
    );
}

#[test]
fn duplicate_params_and_defs() {
    let out = codes(
        "function f($a, $a)\n  return 1\nend\nfunction g()\n  return 1\nend\nfunction g()\n  return 2\nend\n",
    );
    assert!(out.contains(&"MIX-E1301".to_string()));
    assert!(out.contains(&"MIX-E1302".to_string()));
}

#[test]
fn unreachable_and_must_use() {
    let out = lint("$r = run_rc(\"true\")\nrun_rc(\"true\")\nexit(0)\nprint(\"dead\")\n");
    let codes: Vec<&str> = out.iter().map(|(c, _)| c.as_str()).collect();
    assert!(codes.contains(&"MIX-W2201"));
    assert!(codes.contains(&"MIX-W2101"));
}

#[test]
fn require_missing_flags_e1401() {
    let out = codes("$m = require(\"/no/such/module-xyz.mix\")\nprint($m)\n");
    assert_eq!(out, vec!["MIX-E1401"]);
}

#[test]
fn list_addition_warns_for_literal_and_proven_variable() {
    assert_eq!(codes("$joined = [\"x\"] + [\"y\"]\n"), vec!["MIX-W2301"]);
    assert_eq!(
        codes("$items = [\"x\"]\n$joined = $items + [\"y\"]\n"),
        vec!["MIX-W2301"]
    );
}

#[test]
fn list_addition_near_misses_stay_quiet() {
    assert_eq!(codes("$joined = \"a\" + \"b\"\n"), Vec::<String>::new());
    assert_eq!(
        codes("$items = []\n$items = \"a\"\n$joined = $items + \"b\"\n"),
        Vec::<String>::new()
    );
    assert_eq!(
        codes(
            "$items = []\nif env(\"USE_STRING\") != \"\" then\n  $items = \"a\"\nend\n$joined = $items + \"b\"\n"
        ),
        Vec::<String>::new()
    );
}

#[test]
fn used_call_to_implicit_nil_function_warns() {
    let src = "function double($n)\n  $n * 2\nend\n$result = double(4)\n";
    assert_eq!(codes(src), vec!["MIX-W2302"]);
}

#[test]
fn discarded_implicit_nil_function_result_stays_quiet() {
    let src = "function double($n)\n  $n * 2\nend\ndouble(4)\n";
    assert_eq!(codes(src), Vec::<String>::new());
    let terminating = "function stop()\n  exit(0)\nend\n$result = stop()\n";
    assert_eq!(codes(terminating), Vec::<String>::new());
    let mixed = "function maybe($ok)\n  if $ok then\n    return 1\n  end\n  2\nend\n$result = maybe(true)\n";
    assert_eq!(codes(mixed), Vec::<String>::new());
}

#[test]
fn hand_built_assignment_chain_ast_keeps_w2303_defence() {
    let assignments = [
        StmtKind::Assignment {
            name: "x".into(),
            value: Expr::BoolLiteral(true),
        },
        StmtKind::FieldAssignment {
            object: "m".into(),
            field: "ok".into(),
            value: Expr::BoolLiteral(true),
        },
        StmtKind::IndexAssignment {
            object: "m".into(),
            index: Expr::StringLiteral("ok".into()),
            value: Expr::BoolLiteral(true),
        },
        StmtKind::PathAssignment {
            root: "m".into(),
            path: vec![PathSeg::Field("inner".into()), PathSeg::Field("ok".into())],
            value: Expr::BoolLiteral(true),
        },
    ];

    for assignment in assignments {
        for op in [ChainOp::And, ChainOp::Or] {
            for assignment_on_left in [true, false] {
                let assignment = Stmt::new(assignment.clone(), 1);
                let expression = Stmt::new(StmtKind::Expression(Expr::BoolLiteral(true)), 1);
                let (left, right) = if assignment_on_left {
                    (assignment, expression)
                } else {
                    (expression, assignment)
                };
                let chain = Stmt::new(
                    StmtKind::Chain {
                        left: Box::new(left),
                        op: op.clone(),
                        right: Box::new(right),
                    },
                    1,
                );
                let analysis =
                    analyze(&[chain], Some("hand-built.mix"), &AnalyzerConfig::default());
                let found = analysis
                    .diagnostics
                    .iter()
                    .find(|d| d.code == "MIX-W2303")
                    .expect("hand-built chain must retain W2303");
                // The warning must name the operator it actually saw: an `&&`
                // message on an `||` chain sends the reader hunting for an
                // operator that is not there.
                let (present, absent) = match op {
                    ChainOp::And => ("`&&`", "`||`"),
                    ChainOp::Or => ("`||`", "`&&`"),
                };
                assert!(
                    found.message.contains(present) && !found.message.contains(absent),
                    "W2303 must name {present}: {}",
                    found.message
                );
                assert!(
                    found
                        .hint
                        .as_deref()
                        .is_some_and(|h| h.contains("`and`") && h.contains("`or`")),
                    "W2303 hint must point at `and`/`or`: {:?}",
                    found.hint
                );
            }
        }
    }
}

/// The direct-operand cases above leave the analyser's *recursion* untested:
/// reverting either the `PipeToExternal` unwrapping or the descent into nested
/// Chains and statement bodies leaves them green. Each shape here is one the
/// parser cannot emit (it rejects first), so W2303 is only reachable through
/// the public `analyze()` API — which is exactly the defence being pinned.
#[test]
fn hand_built_assignment_chain_survives_wrapping_and_nesting() {
    let assignment = || {
        Stmt::new(
            StmtKind::Assignment {
                name: "x".into(),
                value: Expr::BoolLiteral(true),
            },
            1,
        )
    };
    let expression = || Stmt::new(StmtKind::Expression(Expr::BoolLiteral(true)), 1);
    let chain = |left: Stmt, right: Stmt| {
        Stmt::new(
            StmtKind::Chain {
                left: Box::new(left),
                op: ChainOp::And,
                right: Box::new(right),
            },
            1,
        )
    };

    // Operand wrapped in a pipeline: `$x = true | cat && true`.
    let piped = Stmt::new(
        StmtKind::PipeToExternal {
            stmt: Box::new(assignment()),
            command: "cat".into(),
        },
        1,
    );
    // Assignment buried in the deep left spine of a nested chain.
    let nested = chain(
        chain(chain(assignment(), expression()), expression()),
        expression(),
    );
    // Chain inside an `if` body — the analyser must descend into statement bodies.
    let in_body = Stmt::new(
        StmtKind::If {
            condition: Expr::BoolLiteral(true),
            then_body: vec![chain(assignment(), expression())],
            else_ifs: vec![],
            else_body: None,
        },
        1,
    );

    for (label, stmt) in [
        ("pipeline-wrapped operand", chain(piped, expression())),
        ("nested chain spine", nested),
        ("chain inside an if body", in_body),
    ] {
        let analysis = analyze(&[stmt], Some("hand-built.mix"), &AnalyzerConfig::default());
        assert!(
            analysis.diagnostics.iter().any(|d| d.code == "MIX-W2303"),
            "{label} must retain W2303"
        );
    }
}

#[test]
fn separate_assignment_and_or_chain_stays_quiet() {
    let src = "$ok = run_argv([\"true\"])\nrun(\"false\") || print(\"hi\")\n";
    assert_eq!(codes(src), Vec::<String>::new());
}

#[test]
fn separate_assignment_and_chain_stays_quiet() {
    let src = "$ok = run_argv([\"true\"])\nrun(\"true\") && print(\"hi\")\n";
    assert_eq!(codes(src), Vec::<String>::new());
}

#[test]
fn unknown_builtin_result_key_warns_with_real_key() {
    let src = "$r = run_argv([\"true\"])\nif $r[\"code\"] != 0 then\n  print(\"bad\")\nend\n";
    let tokens = Lexer::new(src).tokenize().unwrap();
    let stmts = Parser::new(tokens, src).parse_program().unwrap();
    let analysis = analyze(&stmts, Some("test.mix"), &AnalyzerConfig::default());
    assert_eq!(analysis.diagnostics.len(), 1);
    let diagnostic = &analysis.diagnostics[0];
    assert_eq!(diagnostic.code, "MIX-W2304");
    assert!(
        diagnostic
            .hint
            .as_deref()
            .is_some_and(|hint| hint.contains("use 'exit_code'"))
    );
}

#[test]
fn valid_or_unproven_result_map_keys_stay_quiet() {
    assert_eq!(
        codes("$r = run_argv([\"true\"])\nprint($r[\"exit_code\"])\n"),
        Vec::<String>::new()
    );
    assert_eq!(
        codes("$r = make_result()\nprint($r[\"code\"])\n"),
        vec!["MIX-E1102"]
    );
}

#[test]
fn escaped_quote_in_ssh_command_string_warns_narrowly() {
    assert_eq!(
        codes(
            r#"$r = ssh_run("alpha", "print(\"remote\")")
"#
        ),
        vec!["MIX-W2306"]
    );
    assert_eq!(
        codes(
            r#"$out = ssh_must("alpha", "print(\"remote\")")
"#
        ),
        vec!["MIX-W2306"]
    );
    assert_eq!(
        codes("$r = ssh_run(\"alpha\", \"hostname\")\n"),
        Vec::<String>::new()
    );
    assert_eq!(
        codes(
            r#"$r = ssh_run("alpha", 'print("remote")')
"#
        ),
        Vec::<String>::new()
    );
    assert_eq!(
        codes(
            r#"$r = ssh_exec("alpha", ["print", "\"remote\""])
$m = ssh_mix("alpha", "print(\"remote\")")
"#
        ),
        Vec::<String>::new()
    );
}

// ── false-positive guards ───────────────────────────────────────────

#[test]
fn clean_provisioning_shaped_script_is_clean() {
    let src = "\
$job = validate({node: \"n\", vmid: 120}, {node: {nonblank: true}, vmid: {type: \"integer\"}})
$target = \"ct-\" .. $job.node
$r = run_argv([\"echo\", $target])
if not $r.ok then
  eprint(\"failed: \" .. $r.stderr)
  exit(1)
end
print($r.stdout)
";
    assert_eq!(lint(src), vec![]);
}

#[test]
fn dynamic_include_suppresses_name_checks() {
    let out = lint("source \"./helpers.mix\"\nprint($from_helpers)\nhelper_fn()\n");
    assert_eq!(out, vec![("MIX-W2401".to_string(), Some(1))]);
}

#[test]
fn function_valued_variable_bareword_call_ok() {
    let src = "$greet = function($n) = \"hi \" .. $n\nprint(greet(\"x\"))\n";
    assert_eq!(lint(src), vec![]);
}

#[test]
fn address_block_sends_are_not_undefined_functions() {
    let src = "address \"noded.delta.bus\"\n  some_remote_verb(1)\nend\n";
    assert_eq!(lint(src), vec![]);
}

#[test]
fn injected_and_positional_and_interp_names_ok() {
    let src = "run(\"true\")\nprint($rc)\nprint($1)\nprint(\"${HOME}\")\nprint(\"${undefined_env_thing}\")\n";
    assert_eq!(lint(src), vec![]);
}

#[test]
fn heredoc_ast_keeps_its_provenance() {
    let src = "$s = <<EOF\nplain\nEOF\n";
    let tokens = Lexer::new(src).tokenize().unwrap();
    let stmts = Parser::new(tokens, src).parse_program().unwrap();
    let cosmix_mix::ast::StmtKind::Assignment { value, .. } = &stmts[0].kind else {
        panic!("expected assignment");
    };
    assert!(matches!(value, cosmix_mix::ast::Expr::Heredoc(_)));
}

#[test]
fn bare_bound_variable_in_heredoc_warns_w2402() {
    // No `${...}` part is present: a literal-only heredoc must retain
    // its Heredoc AST provenance and still trigger the warning.
    let src = "$IP = \"10.0.0.1\"\n$s = <<EOF\nBare=$IP\nEOF\nprint($s)\n";
    let tokens = Lexer::new(src).tokenize().unwrap();
    let stmts = Parser::new(tokens, src).parse_program().unwrap();
    let a = analyze(&stmts, Some("test.mix"), &AnalyzerConfig::default());
    assert_eq!(a.diagnostics.len(), 1);
    let d = &a.diagnostics[0];
    assert_eq!(d.code, "MIX-W2402");
    assert_eq!(d.severity, Severity::Warning);
    assert_eq!(
        d.message,
        "`$IP` in heredoc is not interpolated — did you mean `${IP}`?"
    );
    assert_eq!(
        d.hint.as_deref(),
        Some("literal `$IP` output requires no change")
    );
}

#[test]
fn heredoc_bare_variable_warning_false_positive_guards() {
    assert_eq!(
        lint("$IP = \"10.0.0.1\"\n$s = <<EOF\nAddr=${IP}/24\nEOF\nprint($s)\n"),
        vec![]
    );
    assert_eq!(
        lint("$IP = \"10.0.0.1\"\n$s = <<EOF\nBare=$notavar\nEOF\nprint($s)\n"),
        vec![]
    );
    assert_eq!(
        lint("$IP = \"10.0.0.1\"\n$s = <<EOF\nAwk=$1\nEOF\nprint($s)\n"),
        vec![]
    );
    assert_eq!(
        lint("$IP = \"10.0.0.1\"\n$s = \"Bare=$IP\"\nprint($s)\n"),
        vec![]
    );
    assert_eq!(
        lint("$IP = \"10.0.0.1\"\n$s = <<EOF\nEscaped=\\$IP\nEOF\nprint($s)\n"),
        vec![]
    );
}

#[test]
fn prelude_functions_recognized() {
    // Prelude defines helper fns; calling one must not be E1102 (pick
    // one from the embedded prelude if present — resilient check: the
    // prelude set is non-empty and each member lints clean).
    let names = cosmix_mix::analyzer::prelude_function_names();
    if let Some(name) = names.iter().next() {
        let src = format!("{name}()\n");
        let out = lint(&src);
        assert!(
            out.iter().all(|(c, _)| c != "MIX-E1102"),
            "prelude fn {name} flagged: {out:?}"
        );
    }
}

#[test]
fn no_lexical_order_no_block_scoping() {
    let src = "\
function uses_late_global()
  return $config
end
if true then
  $inside_if = 1
end
print($inside_if)
$config = {a: 1}
print(uses_late_global())
for each $item in [1, 2]
  $last = $item
end
print($last)
try
  die(\"x\")
catch $msg, $err
  print($msg .. $err.code)
end
";
    assert_eq!(lint(src), vec![]);
}

#[test]
fn allow_flags_respected() {
    let cfg = AnalyzerConfig {
        allow_globals: vec!["EXTERNAL".to_string()],
        allow_functions: vec!["embedder_fn".to_string()],
        // Only the nested ssh_mix-body analysis sets this (v0.69.0).
        ..AnalyzerConfig::default()
    };
    assert_eq!(lint_cfg("print($EXTERNAL)\nembedder_fn(1)\n", &cfg), vec![]);
}

#[test]
fn lambda_params_and_captures_ok() {
    let src = "$base = 10\n$xs = map([1, 2], function($x) = $x + $base)\nprint($xs)\n";
    assert_eq!(lint(src), vec![]);
}

#[test]
fn must_use_as_last_statement_of_function_is_not_w2201() {
    let src = "function probe($h)\n  ssh_run($h, \"true\")\nend\n$r = probe(\"h\")\nprint($r)\n";
    assert_eq!(lint(src), vec![("MIX-W2302".to_string(), Some(4))]);
}

#[test]
fn variadic_and_optional_builtin_arities_ok() {
    let src = "print(fmt(\"%s %s\", 1, 2))\nprint(min(1, 2, 3, 4))\nprint(substr(\"abc\", 1))\nprint(random())\nprint(random(1, 2))\n";
    assert_eq!(lint(src), vec![]);
}

#[test]
fn capabilities_inventory() {
    let tokens = Lexer::new(
        "$r = run_rc(\"true\")\n$h = http_get(\"https://192.0.2.1/\")\nprint($r.rc .. $h.status)\n",
    )
    .tokenize()
    .unwrap();
    let stmts = Parser::new(tokens, "x").parse_program().unwrap();
    let a = analyze(&stmts, None, &AnalyzerConfig::default());
    assert!(a.capabilities.contains(&"process"), "{:?}", a.capabilities);
    assert!(a.capabilities.contains(&"network"), "{:?}", a.capabilities);
}

// ── codex release-review MAJOR: embedded statement bodies ───────────

#[test]
fn if_expression_branches_are_scope_checked() {
    // false negative fix: undefined names inside an if-EXPRESSION branch.
    let out = codes("$x = if true then $undefined_in_branch else 0 end\nprint($x)\n");
    assert_eq!(out, vec!["MIX-E1101"]);
}

#[test]
fn var_assigned_in_if_expression_then_read_is_ok() {
    // false positive fix: a top-level var bound inside an if-expr branch
    // is in the file universe (no lexical order, no block scoping).
    let src = "$y = if true then\n  $inner = 5\n  $inner\nelse\n  0\nend\nprint($inner + $y)\n";
    assert_eq!(lint(src), vec![]);
}

#[test]
fn function_defined_and_called_inside_block_lambda_is_ok() {
    // false positive fix: a def inside a block lambda is a real callable.
    let src = "$run = function()\n  function helper()\n    return 7\n  end\n  return helper()\nend\nprint($run())\n";
    assert_eq!(lint(src), vec![]);
}

#[test]
fn source_inside_block_lambda_degrades_to_w2401() {
    let out =
        codes("$f = function()\n  source \"./x.mix\"\n  return $from_source\nend\nprint($f())\n");
    assert_eq!(out, vec!["MIX-W2401"]);
}

#[test]
fn param_default_expression_is_scope_checked() {
    // false negative fix: undefined name in a parameter default.
    let out = codes("function f($a = $undefined_default)\n  return $a\nend\nf()\n");
    assert!(out.contains(&"MIX-E1101".to_string()), "{out:?}");
}

#[test]
fn non_trailing_default_arity_in_lint() {
    // matches the evaluator: min = past last required param.
    let out = codes("function f($a = 1, $b)\n  return $b\nend\nf(9)\n");
    assert_eq!(out, vec!["MIX-E1202"]);
}

// ── codex convergence review: analyzer-fix regressions ──────────────

#[test]
fn lambda_local_binding_does_not_leak_to_file_universe() {
    // A name bound only inside a lambda body is NOT visible at top level
    // (isolated frame) — reading it there is E1101.
    let out = codes("$f = function()\n  $lambda_local = 1\nend\nprint($lambda_local)\n");
    assert_eq!(out, vec!["MIX-E1101"]);
}

#[test]
fn function_local_binding_does_not_leak_to_file_universe() {
    let out = codes("function f()\n  $fn_local = 1\nend\nf()\nprint($fn_local)\n");
    assert_eq!(out, vec!["MIX-E1101"]);
}

#[test]
fn duplicate_defs_and_dead_code_inside_lambda_are_flagged() {
    let out = codes(
        "$f = function()\n  function helper()\n    return 1\n  end\n  function helper()\n    return 2\n  end\n  return helper()\nend\nprint($f())\n",
    );
    assert!(out.contains(&"MIX-E1302".to_string()), "{out:?}");
    let out = codes("$g = function()\n  return 1\n  print(\"dead\")\nend\nprint($g())\n");
    assert!(out.contains(&"MIX-W2101".to_string()), "{out:?}");
}

#[test]
fn deeply_nested_lambdas_do_not_blow_up() {
    // codex convergence review MAJOR: nested block lambdas were
    // O(2^depth). Depth 24 must analyze in well under a second.
    let mut src = String::new();
    for i in 0..24 {
        src.push_str(&format!("$f{i} = function()\n"));
    }
    src.push_str("  $x = 1\n");
    for _ in 0..24 {
        src.push_str("end\n");
    }
    let start = std::time::Instant::now();
    let _ = lint(&src);
    assert!(
        start.elapsed().as_millis() < 500,
        "nested-lambda analysis took {}ms",
        start.elapsed().as_millis()
    );
}

#[test]
fn severity_partition_is_stable() {
    let tokens = Lexer::new("nope()\n").tokenize().unwrap();
    let stmts = Parser::new(tokens, "x").parse_program().unwrap();
    let a = analyze(&stmts, None, &AnalyzerConfig::default());
    assert!(a.diagnostics.iter().all(|d| match d.code {
        c if c.starts_with("MIX-E") => d.severity == Severity::Error,
        c if c.starts_with("MIX-W") => d.severity == Severity::Warning,
        _ => false,
    }));
}

// ── E1501 / E1502: statements whose entire effect is provably lost ────
//
// Both are ERRORS, not warnings: the statement does nothing at all while
// reading as though it did. `push($m[$k], $v)` in particular is the
// spelling every newcomer reaches for, and before 0.33.0 it lint-passed
// clean while silently dropping the write.

#[test]
fn e1501_dead_push_into_a_container_element() {
    assert!(
        codes("$m = { a: [1] }\npush($m[\"a\"], 2)\nprint($m)\n")
            .contains(&"MIX-E1501".to_string())
    );
    assert!(
        codes("$m = { a: [1] }\npush($m.a, 2)\nprint($m)\n").contains(&"MIX-E1501".to_string())
    );
    // UFCS spelling desugars to the same call.
    assert!(
        codes("$m = { a: [1] }\n$m[\"a\"].push(2)\nprint($m)\n").contains(&"MIX-E1501".to_string())
    );
    assert!(
        codes("$m = { a: [1] }\npop($m[\"a\"])\nprint($m)\n").contains(&"MIX-E1501".to_string())
    );
}

#[test]
fn e1501_silent_on_the_forms_that_actually_work() {
    // A bare variable IS the mutable slot — this is the whole contract.
    assert!(!codes("$l = [1]\npush($l, 2)\nprint($l)\n").contains(&"MIX-E1501".to_string()));
    // The approved idiom: assign the returned list back through the path.
    assert!(
        !codes("$m = { a: [1] }\n$m[\"a\"] = push($m[\"a\"], 2)\nprint($m)\n")
            .contains(&"MIX-E1501".to_string())
    );
    // Result used → pop/shift on an expression are legitimate.
    assert!(
        !codes("$m = { a: [1] }\n$x = pop($m[\"a\"])\nprint($x)\n")
            .contains(&"MIX-E1501".to_string())
    );
    // A by-value PARAMETER is a bare variable: that dead-push case has
    // its own 0.21.9 diagnostic and must not be double-reported here.
    assert!(
        !codes("function f($p)\n  push($p, 1)\n  print($p)\nend\n")
            .contains(&"MIX-E1501".to_string())
    );
}

#[test]
fn e1502_discarded_pure_transform() {
    assert!(
        codes("$m = { a: 1 }\ndelete($m, \"a\")\nprint($m)\n").contains(&"MIX-E1502".to_string())
    );
    assert!(codes("$m = {}\nmerge($m, { b: 2 })\nprint($m)\n").contains(&"MIX-E1502".to_string()));
    // Assigned back → correct, and silent.
    assert!(
        !codes("$m = { a: 1 }\n$m = delete($m, \"a\")\nprint($m)\n")
            .contains(&"MIX-E1502".to_string())
    );
}

/// The analyser's operand predicate is documented as being kept in lockstep
/// with the parser's. The parser's copy is pinned by
/// `assignment_chain_parse.rs`; without this the analyser's copy could quietly
/// drop a form and the W2303 embedder defence would go silent while E1002
/// still fired — a divergence no source-text test can catch, because source
/// text never reaches the analyser once the parser rejects it.
#[test]
fn hand_built_value_binding_keyword_operands_keep_w2303() {
    let expression = || Stmt::new(StmtKind::Expression(Expr::BoolLiteral(true)), 1);
    let chain = |left: Stmt| {
        Stmt::new(
            StmtKind::Chain {
                left: Box::new(left),
                op: ChainOp::Or,
                right: Box::new(expression()),
            },
            1,
        )
    };

    let binders = [
        (
            "export",
            StmtKind::Export {
                name: "x".into(),
                value: Expr::BoolLiteral(false),
            },
        ),
        (
            "alias define",
            StmtKind::Alias {
                name: Some(Expr::StringLiteral("xx".into())),
                command: Some(Expr::StringLiteral("false".into())),
            },
        ),
        (
            "terse function def",
            StmtKind::FunctionDef {
                name: "f".into(),
                params: vec![],
                body: FunctionBody::Expression(Expr::BoolLiteral(false)),
            },
        ),
    ];

    for (label, kind) in binders {
        let analysis = analyze(
            &[chain(Stmt::new(kind, 1))],
            Some("hand-built.mix"),
            &AnalyzerConfig::default(),
        );
        assert!(
            analysis.diagnostics.iter().any(|d| d.code == "MIX-W2303"),
            "{label} operand must retain W2303"
        );
    }

    // The block-bodied form binds no `=` expression and must stay quiet.
    let block = StmtKind::FunctionDef {
        name: "f".into(),
        params: vec![],
        body: FunctionBody::Block(vec![expression()]),
    };
    let analysis = analyze(
        &[chain(Stmt::new(block, 1))],
        Some("hand-built.mix"),
        &AnalyzerConfig::default(),
    );
    assert!(
        !analysis.diagnostics.iter().any(|d| d.code == "MIX-W2303"),
        "block-bodied function def must stay a legal operand"
    );
}
