//! Optional `each` (mix 0.63.0): `for $x in $xs` and `for $i, $x in $xs`
//! parse to the same `StmtKind::ForEach` as the `for each …` spellings,
//! which stay accepted indefinitely. The counted C-style loop
//! (`for $i = 1 to N [step S]`) is disambiguated by one token of
//! lookahead after the variable (`=` vs `in`/`,`).

use cosmix_mix::ast::StmtKind;
use cosmix_mix::error::MixError;
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;
use cosmix_mix::run_capturing;

fn parse(source: &str) -> Result<Vec<cosmix_mix::ast::Stmt>, MixError> {
    let tokens = Lexer::new(source).tokenize()?;
    Parser::new(tokens, source).parse_program()
}

async fn output(source: &str) -> String {
    let (_, stdout, stderr) = run_capturing(source).await.expect("source should run");
    assert!(stderr.is_empty(), "unexpected stderr: {stderr}");
    stdout
}

// ---------- the two new spellings ----------

#[tokio::test]
async fn bare_for_in_iterates() {
    let out = output("for $x in [1, 2, 3]\n  print($x)\nend\n").await;
    assert_eq!(out, "1\n2\n3\n");
}

#[tokio::test]
async fn bare_for_with_index_variable() {
    let out = output("for $i, $x in [\"a\", \"b\"]\n  print($i .. \":\" .. $x)\nend\n").await;
    assert_eq!(out, "0:a\n1:b\n");
}

#[tokio::test]
async fn bare_form_parses_to_the_same_ast_as_for_each() {
    let bare = parse("for $x in $xs\n  print($x)\nend\n").expect("bare form parses");
    let each = parse("for each $x in $xs\n  print($x)\nend\n").expect("each form parses");
    assert_eq!(bare.len(), 1);
    assert!(
        matches!(&bare[0].kind, StmtKind::ForEach { .. }),
        "bare form must be ForEach"
    );
    // Same statement kind, same variables, same shape.
    assert_eq!(format!("{:?}", bare[0].kind), format!("{:?}", each[0].kind));

    let bare2 = parse("for $i, $x in $xs\n  print($x)\nend\n").expect("bare indexed parses");
    let each2 = parse("for each $i, $x in $xs\n  print($x)\nend\n").expect("each indexed parses");
    assert_eq!(format!("{:?}", bare2[0].kind), format!("{:?}", each2[0].kind));
}

#[tokio::test]
async fn bare_form_iterates_map_keys_like_each() {
    // One-variable map iteration yields keys (unchanged semantics).
    let out = output("$m = {a: 1, b: 2}\nfor $k in $m\n  print($k .. \"=\" .. $m[$k])\nend\n").await;
    assert_eq!(out, "a=1\nb=2\n");
}

#[tokio::test]
async fn break_and_continue_work_in_bare_form() {
    let out = output(
        "for $x in [1, 2, 3, 4]\n  continue if $x == 2\n  break if $x == 4\n  print($x)\nend\n",
    )
    .await;
    assert_eq!(out, "1\n3\n");
}

#[tokio::test]
async fn nested_bare_loops() {
    let out = output("for $a in [1, 2]\n  for $b in [\"x\"]\n    print($a .. $b)\n  end\nend\n").await;
    assert_eq!(out, "1x\n2x\n");
}

// ---------- the existing spellings are unchanged ----------

#[tokio::test]
async fn for_each_forms_still_accepted() {
    let out = output("for each $x in [7]\n  print($x)\nend\n").await;
    assert_eq!(out, "7\n");
    let out = output("for each $i, $x in [\"z\"]\n  print($i .. $x)\nend\n").await;
    assert_eq!(out, "0z\n");
}

#[tokio::test]
async fn counted_loop_still_counted() {
    let out = output("for $i = 1 to 3\n  print($i)\nend\n").await;
    assert_eq!(out, "1\n2\n3\n");
    let out = output("for $i = 1 to 5 step 2\n  print($i)\nend\n").await;
    assert_eq!(out, "1\n3\n5\n");
}

// ---------- still errors ----------

#[test]
fn for_without_variable_is_a_parse_error() {
    assert!(parse("for in $xs\n  print(1)\nend\n").is_err());
}

#[test]
fn for_variable_then_garbage_is_a_parse_error() {
    // Neither `=` (counted) nor `in`/`,` (iteration) after the variable.
    assert!(parse("for $x $y\n  print(1)\nend\n").is_err());
}
