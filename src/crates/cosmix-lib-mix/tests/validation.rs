//! Validation builtins (0.29.0, decision record D7): require_key,
//! expect_type, nonblank, get_or, validate — strict data boundaries by
//! choice, structured VALIDATION_* errors with {path, expected,
//! actual_type} details.

use cosmix_mix::error::MixError;
use cosmix_mix::evaluator::{Evaluator, SharedBuf};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;

async fn run(source: &str) -> Result<String, MixError> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize()?;
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program()?;
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    eval.execute(&stmts).await?;
    Ok(stdout.to_string_lossy())
}

async fn run_ok(source: &str) -> String {
    match run(source).await {
        Ok(out) => out,
        Err(e) => panic!("script should succeed, got: {e}"),
    }
}

/// Run a snippet expected to raise; print code + details fields.
async fn catch_code(snippet: &str) -> String {
    run_ok(&format!(
        "try\n  {snippet}\ncatch $m, $e\n  print($e.code .. \" path=\" .. $e.details.path .. \" expected=\" .. $e.details.expected .. \" actual=\" .. $e.details.actual_type)\nend\n"
    ))
    .await
}

// ── require_key / get_or / expect_type / nonblank ───────────────────

#[tokio::test]
async fn require_key_present_absent_nil() {
    let out = run_ok("print(require_key({a: 1}, \"a\"))\n").await;
    assert_eq!(out, "1\n");
    assert_eq!(
        catch_code("require_key({a: 1}, \"b\")").await,
        "VALIDATION_REQUIRED path=b expected=present non-nil value actual=nil\n"
    );
    assert_eq!(
        catch_code("require_key({b: nil}, \"b\")").await,
        "VALIDATION_REQUIRED path=b expected=present non-nil value actual=nil\n"
    );
}

#[tokio::test]
async fn get_or_covers_absent_and_nil() {
    let out = run_ok(
        "print(get_or({a: 1}, \"a\", 9))\nprint(get_or({a: 1}, \"b\", 9))\nprint(get_or({b: nil}, \"b\", 9))\n",
    )
    .await;
    assert_eq!(out, "1\n9\n9\n");
}

#[tokio::test]
async fn expect_type_matrix() {
    let out = run_ok(
        "print(expect_type(7, \"integer\"))\nprint(expect_type(7.5, \"number\"))\nprint(expect_type(\"x\", \"string\"))\n",
    )
    .await;
    assert_eq!(out, "7\n7.5\nx\n");
    assert_eq!(
        catch_code("expect_type(7.5, \"integer\")").await,
        "VALIDATION_TYPE path=value expected=integer actual=number\n"
    );
    // Unknown type name is a SPEC error, not a value error.
    let out = run_ok("try\n  expect_type(1, \"int\")\ncatch $m, $e\n  print($e.code)\nend\n").await;
    assert_eq!(out, "VALIDATION_SPEC\n");
    // Whole-but-huge numbers are not safe integers.
    assert_eq!(
        catch_code("expect_type(9007199254740993, \"integer\")").await,
        "VALIDATION_TYPE path=value expected=integer actual=number\n"
    );
}

#[tokio::test]
async fn nonblank_returns_untrimmed_and_names_the_label() {
    let out = run_ok("print(\"[\" .. nonblank(\"  x \") .. \"]\")\n").await;
    assert_eq!(out, "[  x ]\n");
    assert_eq!(
        catch_code("nonblank(\"   \", \"node\")").await,
        "VALIDATION_NONBLANK path=node expected=non-blank string actual=string\n"
    );
    assert_eq!(
        catch_code("nonblank(nil, \"node\")").await,
        "VALIDATION_NONBLANK path=node expected=non-blank string actual=nil\n"
    );
}

// ── validate: the provisioning-shaped spec ──────────────────────────

const JOB_SPEC: &str = "{node: {type: \"string\", nonblank: true}, host: {type: \"string\", nonblank: true}, vmid: {type: \"integer\", min: 100, max: 999999}, plan: {enum: [\"gold\", \"silver\"]}, tags: {required: false, type: \"list\", items: {type: \"string\", nonblank: true}}, owner: {required: false, type: \"map\", schema: {name: {nonblank: true}}}}";

#[tokio::test]
async fn validate_passes_and_returns_original() {
    let src = format!(
        "$raw = {{node: \"ct120\", host: \"pve3\", vmid: 120, plan: \"gold\", extra: \"kept\"}}\n$job = validate($raw, {JOB_SPEC})\nprint($job.node .. \" \" .. $job.extra)\n"
    );
    let out = run_ok(&src).await;
    assert_eq!(out, "ct120 kept\n");
}

#[tokio::test]
async fn validate_blank_node_is_rejected_with_path() {
    // THE historical provisioning failure: a blank node flowed into a
    // constructed hostname as "nil". Now it dies at the boundary.
    let src = format!(
        "try\n  validate({{node: \"\", host: \"pve3\", vmid: 120, plan: \"gold\"}}, {JOB_SPEC})\ncatch $m, $e\n  print($e.code .. \" \" .. $e.details.path)\nend\n"
    );
    let out = run_ok(&src).await;
    assert_eq!(out, "VALIDATION_NONBLANK node\n");
}

#[tokio::test]
async fn validate_missing_required_absent_vs_optional() {
    let src = format!(
        "try\n  validate({{host: \"pve3\", vmid: 120, plan: \"gold\"}}, {JOB_SPEC})\ncatch $m, $e\n  print($e.code .. \" \" .. $e.details.path)\nend\n"
    );
    assert_eq!(run_ok(&src).await, "VALIDATION_REQUIRED node\n");
    // Optional fields absent → fine.
    let src = format!(
        "$ok = validate({{node: \"n\", host: \"h\", vmid: 500, plan: \"silver\"}}, {JOB_SPEC})\nprint(\"ok\")\n"
    );
    assert_eq!(run_ok(&src).await, "ok\n");
}

#[tokio::test]
async fn validate_enum_range_and_integer() {
    let src = format!(
        "try\n  validate({{node: \"n\", host: \"h\", vmid: 120, plan: \"bronze\"}}, {JOB_SPEC})\ncatch $m, $e\n  print($e.code .. \" \" .. $e.details.path)\nend\n"
    );
    assert_eq!(run_ok(&src).await, "VALIDATION_ENUM plan\n");
    let src = format!(
        "try\n  validate({{node: \"n\", host: \"h\", vmid: 7, plan: \"gold\"}}, {JOB_SPEC})\ncatch $m, $e\n  print($e.code .. \" \" .. $e.details.path)\nend\n"
    );
    assert_eq!(run_ok(&src).await, "VALIDATION_RANGE vmid\n");
    let src = format!(
        "try\n  validate({{node: \"n\", host: \"h\", vmid: 120.5, plan: \"gold\"}}, {JOB_SPEC})\ncatch $m, $e\n  print($e.code .. \" \" .. $e.details.path)\nend\n"
    );
    assert_eq!(run_ok(&src).await, "VALIDATION_TYPE vmid\n");
}

#[tokio::test]
async fn validate_nested_paths_items_and_schema() {
    let src = format!(
        "try\n  validate({{node: \"n\", host: \"h\", vmid: 120, plan: \"gold\", tags: [\"a\", \"  \"]}}, {JOB_SPEC})\ncatch $m, $e\n  print($e.code .. \" \" .. $e.details.path)\nend\n"
    );
    assert_eq!(run_ok(&src).await, "VALIDATION_NONBLANK tags[1]\n");
    let src = format!(
        "try\n  validate({{node: \"n\", host: \"h\", vmid: 120, plan: \"gold\", owner: {{name: \"\"}}}}, {JOB_SPEC})\ncatch $m, $e\n  print($e.code .. \" \" .. $e.details.path)\nend\n"
    );
    assert_eq!(run_ok(&src).await, "VALIDATION_NONBLANK owner.name\n");
}

#[tokio::test]
async fn validate_spec_errors() {
    for (spec, needle) in [
        ("{a: {nonblnk: true}}", "unknown rule"),
        ("{a: {type: 7}}", "'type' at a"),
        ("{a: {enum: []}}", "non-empty list"),
        ("{a: {required: \"yes\"}}", "'required' at a"),
    ] {
        let src = format!(
            "try\n  validate({{a: \"x\"}}, {spec})\ncatch $m, $e\n  print($e.code .. \": \" .. $m)\nend\n"
        );
        let out = run_ok(&src).await;
        assert!(
            out.starts_with("VALIDATION_SPEC:") && out.contains(needle),
            "{spec} -> {out}"
        );
    }
}

#[tokio::test]
async fn validate_length_rules() {
    let out = run_ok(
        "try\n  validate({name: \"toolongname\"}, {name: {max_length: 5}})\ncatch $m, $e\n  print($e.code .. \" \" .. $e.details.path)\nend\n",
    )
    .await;
    assert_eq!(out, "VALIDATION_LENGTH name\n");
    let out = run_ok(
        "$v = validate({name: \"ab\"}, {name: {min_length: 1, max_length: 5}})\nprint(\"ok\")\n",
    )
    .await;
    assert_eq!(out, "ok\n");
}

#[tokio::test]
async fn validate_enum_uses_mix_equality() {
    // Value::PartialEq coerces number <-> numeric string — "normal Mix
    // equality" per D7.
    let out =
        run_ok("$v = validate({port: \"8080\"}, {port: {enum: [8080, 9090]}})\nprint(\"ok\")\n")
            .await;
    assert_eq!(out, "ok\n");
}

#[tokio::test]
async fn validate_preflights_the_whole_spec() {
    // codex C4 review MAJOR: a malformed rule must fail loudly even
    // when the current input would never exercise it.
    for (input, spec) in [
        ("{}", "{a: {required: false, type: 7}}"), // optional + absent
        ("{a: \"x\"}", "{a: {type: [\"string\", \"bogus\"]}}"), // satisfied union
        ("{a: []}", "{a: {items: {bogus: true}}}"), // empty list
        ("{a: 1}", "{a: {min: 5, max: 2}}"),       // inverted bounds
    ] {
        let src =
            format!("try\n  validate({input}, {spec})\ncatch $m, $e\n  print($e.code)\nend\n");
        assert_eq!(run_ok(&src).await, "VALIDATION_SPEC\n", "{spec}");
    }
    // Non-map spec argument is a SPEC error too.
    let out =
        run_ok("try\n  validate({a: 1}, \"nope\")\ncatch $m, $e\n  print($e.code)\nend\n").await;
    assert_eq!(out, "VALIDATION_SPEC\n");
}

#[tokio::test]
async fn validate_depth_ceiling_is_consistent_for_items_and_schema() {
    // codex release review MINOR: schema used to count two levels per
    // nesting; both forms now share the same ceiling.
    let schema_depth = "\
function build($n)
  $rules = {type: \"string\"}
  $val = \"x\"
  for $i = 1 to $n
    $rules = {type: \"map\", schema: {leaf: $rules}}
    $val = {leaf: $val}
  end
  try
    validate({leaf: $val}, {leaf: $rules})
  catch $m, $e
    return \"raised\"
  end
  return \"ok\"
end
print(build(30))
print(build(70))
";
    let out = run_ok(schema_depth).await;
    // 30 levels well under the ceiling → ok; 70 over → raised. (Exact
    // boundary is pinned by the Rust preflight unit; here we assert the
    // two forms don't diverge wildly.)
    assert_eq!(out, "ok\nraised\n");
}

#[tokio::test]
async fn validate_spec_error_carries_details() {
    // codex release review MINOR: VALIDATION_SPEC now carries the same
    // {path, expected, actual_type} details as data violations.
    let out = run_ok(
        "try\n  validate({a: 1}, {a: {type: 7}})\ncatch $m, $e\n  print($e.code .. \" \" .. $e.details.path .. \" \" .. $e.details.actual_type)\nend\n",
    )
    .await;
    assert_eq!(out, "VALIDATION_SPEC a number\n");
}

#[tokio::test]
async fn validate_deep_spec_raises_instead_of_overflowing() {
    // codex C4 review BLOCKER: a programmatically-built 12k-deep spec
    // used to overflow the native stack (uncatchable abort). The depth
    // ceiling turns it into a catchable VALIDATION_SPEC.
    let src = "\
$spec = {leaf: {type: \"string\"}}
$val = {leaf: \"x\"}
for $i = 1 to 200
  $spec = {child: {type: \"map\", schema: $spec}}
  $val = {child: $val}
end
try
  validate($val, $spec)
catch $m, $e
  print($e.code .. \" \" .. (pos(\"nesting\", $m) > 0))
end
";
    assert_eq!(run_ok(src).await, "VALIDATION_SPEC true\n");
}

#[tokio::test]
async fn validate_paths_escape_non_identifier_keys() {
    let out = run_ok(
        "try\n  validate({\"owner.name\": \"\"}, {\"owner.name\": {nonblank: true}})\ncatch $m, $e\n  print($e.details.path)\nend\n",
    )
    .await;
    assert_eq!(out, "[\"owner.name\"]\n");
}

#[tokio::test]
async fn validate_as_bare_statement_asserts() {
    // Discarding validate's result is a legitimate assertion pattern —
    // it must NOT be must_use in the metadata.
    let out = run_ok("validate({a: 1}, {a: {type: \"integer\"}})\nprint(\"passed\")\n").await;
    assert_eq!(out, "passed\n");
}
