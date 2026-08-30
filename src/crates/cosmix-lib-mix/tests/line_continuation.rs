use cosmix_mix::ast::{Expr, StmtKind};
use cosmix_mix::error::MixError;
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;
use cosmix_mix::{parse_data, run_capturing};

fn parse(source: &str) -> Result<Vec<cosmix_mix::ast::Stmt>, MixError> {
    let tokens = Lexer::new(source).tokenize()?;
    Parser::new(tokens, source).parse_program()
}

async fn output(source: &str) -> String {
    let (_, stdout, stderr) = run_capturing(source).await.expect("source should run");
    assert!(stderr.is_empty(), "unexpected stderr: {stderr}");
    stdout
}

#[tokio::test]
async fn trailing_concat_continues_across_newlines_comments_and_blank_lines() {
    let source = "$s = \"a\" .. -- after operator\n\n# between operator and operand\n  \"b\" ..\n  -- another comment\n  \"c\"\nprint($s)\n";
    assert_eq!(output(source).await, "abc\n");
}

#[tokio::test]
async fn continued_concat_preserves_precedence_and_works_in_nested_expressions() {
    let source = "function joined()\n  return \"x\" ..\n    1 + 2\nend\nprint(joined())\nprint(upper(\"a\" ..\n  \"b\"))\n";
    assert_eq!(output(source).await, "x3\nAB\n");
}

#[tokio::test]
async fn heredoc_can_be_the_continued_right_operand() {
    let source = "$s = \"prefix:\" ..\n<<END\nbody\nEND\nprint($s)\n";
    assert_eq!(output(source).await, "prefix:body\n");
}

#[tokio::test]
async fn backslash_continuation_is_unchanged() {
    let source = "$s = \"a\" .. \\\n  \"b\"\nprint($s)\n";
    assert_eq!(output(source).await, "ab\n");
}

#[tokio::test]
async fn backslash_newline_inside_run_string_stays_for_the_child_shell() {
    let source = "print(run(\"printf '%s' 'a\\\nb'\"))\n";
    assert_eq!(output(source).await, "a\\\nb\n");
}

#[test]
fn eof_after_concat_is_a_typed_incomplete_input() {
    for source in [
        "$s = \"a\" ..",
        "$s = \"a\" ..\n",
        "$s = \"a\" .. -- comment",
    ] {
        let error = parse(source).expect_err("missing right operand must be incomplete");
        assert!(
            matches!(error, MixError::IncompleteInput { .. }),
            "{source:?}: expected typed incomplete input, got {error:?}"
        );
        assert!(error.is_incomplete_input());
        assert!(error.to_string().contains("expected expression after `..`"));
    }
}

#[test]
fn continuation_is_trailing_concat_only() {
    for source in [
        "$s = \"a\"\n  .. \"b\"\n",
        "$s = \"a\" +\n  \"b\"\n",
        "$s = \"a\" ..; \"b\"\n",
    ] {
        let error = parse(source).expect_err("form must remain a parse error");
        assert!(
            matches!(error, MixError::ParseError { .. }),
            "{source:?}: expected ordinary parse error, got {error:?}"
        );
    }
}

#[test]
fn source_and_include_dotdot_paths_keep_the_bareword_path_grammar() {
    for (source, include) in [
        ("source ..\n", false),
        ("source ../file.mix\n", false),
        ("include ..\n", true),
        ("include ../file.mix\n", true),
    ] {
        let mut statements = parse(source).expect("dotdot path should parse");
        assert_eq!(statements.len(), 1);
        let path = match statements.remove(0).kind {
            StmtKind::Source { path } if !include => path,
            StmtKind::Include { path } if include => path,
            other => panic!("unexpected statement for {source:?}: {other:?}"),
        };
        let Expr::StringLiteral(path) = path else {
            panic!("expected literal path for {source:?}")
        };
        assert_eq!(path, source.split_whitespace().nth(1).unwrap());
    }
}

#[test]
fn strict_data_still_rejects_concat_continuation() {
    let error = parse_data("value: \"a\" ..\n  \"b\"\n")
        .expect_err("strict-data must not gain executable concat");
    assert!(!error.is_incomplete_input(), "strict-data error: {error:?}");
    assert!(
        matches!(error, MixError::StrictDataViolation { .. }),
        "expected strict-data violation, got {error:?}"
    );

    let value = parse_data("value: <<END\nbody\nEND\n")
        .expect("literal strict-data heredoc must stay valid");
    assert_eq!(value.to_mix_string(), "{value: body}");
}
