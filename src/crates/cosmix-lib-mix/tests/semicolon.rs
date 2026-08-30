//! Executable-Mix `;` statement-separator semantics.
//!
//! Semicolons survive lexing inside grouping but are accepted only at actual
//! statement boundaries. Strict-data deliberately remains newline/comma-only.

use cosmix_mix::ast::StmtKind;
use cosmix_mix::evaluator::{Evaluator, SharedBuf};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;
use cosmix_mix::{MixError, parse_data};

async fn run_capturing(source: &str) -> Result<String, String> {
    let tokens = Lexer::new(source).tokenize().map_err(|e| e.to_string())?;
    let stmts = Parser::new(tokens, source)
        .parse_program()
        .map_err(|e| e.to_string())?;
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr));
    eval.execute(&stmts).await.map_err(|e| e.to_string())?;
    Ok(stdout.to_string_lossy())
}

fn parse_error(source: &str) -> String {
    let tokens = Lexer::new(source).tokenize().expect("semicolon must lex");
    Parser::new(tokens, source)
        .parse_program()
        .expect_err("source must be rejected")
        .to_string()
}

#[tokio::test]
async fn top_level_leading_trailing_and_repeated_separators() {
    let out = run_capturing("; $a = 1;; $b = 2; print($a + $b);;")
        .await
        .unwrap();
    assert_eq!(out, "3\n");
}

#[tokio::test]
async fn separators_work_across_block_statement_forms() {
    let source = r#"
function twice($x); $y = $x * 2; return $y; end;
if twice(3) == 6 then; print("if"); print("body"); end;
for $i = 1 to 2; print($i); end;
select 2; when 1 then; print("bad"); when 2 then; print("select"); otherwise; print("bad"); end;
try; die("boom"); catch $msg, $err; print("caught:" .. $err.code); finally; print("finally"); end;
"#;
    let out = run_capturing(source).await.unwrap();
    assert_eq!(out, "if\nbody\n1\n2\nselect\ncaught:USER_DIE\nfinally\n");
}

#[tokio::test]
async fn expression_position_blocks_can_use_semicolon_bodies_inside_grouping() {
    let source = r#"
print(if true then; $x = 1; $x + 1; else; 0; end);
print((function($n); $v = $n + 1; return $v; end)(4));
"#;
    let out = run_capturing(source).await.unwrap();
    assert_eq!(out, "2\n5\n");
}

#[test]
fn semicolon_is_rejected_in_ordinary_expression_containers() {
    for source in [
        "print(1; 2)",
        "$x = (1; 2)",
        "$x = [1; 2]",
        "$x = {a: 1; b: 2}",
    ] {
        let err = parse_error(source);
        assert!(
            err.contains("unexpected ';' inside expression"),
            "{source:?}: {err}"
        );
    }
}

#[test]
fn send_keyword_arguments_keep_historical_block_keyword_names() {
    let source = "send t cmd when=1 done=2 next=3 otherwise=4; print(2)";
    let tokens = Lexer::new(source).tokenize().unwrap();
    let stmts = Parser::new(tokens, source).parse_program().unwrap();
    assert_eq!(
        stmts.len(),
        2,
        "semicolon must terminate the send statement"
    );
}

#[test]
fn semicolon_binds_looser_than_statement_chains() {
    let source = "print(\"a\") && print(\"b\"); print(\"c\")";
    let tokens = Lexer::new(source).tokenize().unwrap();
    let stmts = Parser::new(tokens, source).parse_program().unwrap();
    assert_eq!(stmts.len(), 2);
    assert!(matches!(stmts[0].kind, StmtKind::Chain { .. }));

    let err = parse_error("print(\"a\"); && print(\"b\")");
    assert!(err.contains("AndAnd"), "{err}");
    let err = parse_error("print(\"a\") && ; print(\"b\")");
    assert!(err.contains("unexpected ';' inside expression"), "{err}");
}

#[tokio::test]
async fn strings_and_physical_line_comments_keep_semicolons_literal() {
    let source = "print(\"a;b\"); print('c;d'); -- comment; print(\"hidden\")\nprint(\"after\")";
    let out = run_capturing(source).await.unwrap();
    assert_eq!(out, "a;b\nc;d\nafter\n");
}

#[tokio::test]
async fn heredoc_body_keeps_semicolon_literal() {
    let source = "$body = <<END\na;b\nEND\nprint($body)";
    let out = run_capturing(source).await.unwrap();
    assert_eq!(out, "a;b\n");
}

#[test]
fn bareword_source_path_stops_before_semicolon() {
    let source = "source ./fixture.mix; print(1)";
    let tokens = Lexer::new(source).tokenize().unwrap();
    let stmts = Parser::new(tokens, source).parse_program().unwrap();
    assert_eq!(stmts.len(), 2, "source path must not swallow `; print(1)`");
}

#[test]
fn strict_data_rejects_semicolons_everywhere() {
    for source in ["a: 1; b: 2", "[1; 2]", "{a: 1; b: 2}", "; a: 1", "a: 1;"] {
        let err = parse_data(source).expect_err("strict-data must reject semicolon");
        assert!(
            matches!(err, MixError::StrictDataViolation { .. }),
            "{source:?}: expected StrictDataViolation, got {err:?}"
        );
        assert!(err.to_string().contains("semicolon"), "{source:?}: {err}");
    }
}
