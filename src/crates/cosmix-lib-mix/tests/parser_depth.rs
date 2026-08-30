//! Parser/lexer stack-safety: deeply nested input must surface a clean
//! `MixError` instead of overflowing the native stack (SIGABRT,
//! uncatchable). The deep-input tests here PROVE the depth cap — before
//! it landed, each one killed the test process.
//!
//! Covered cycles:
//! - parse_expression → parse_unary → parse_primary → `(` / `[` / `{` /
//!   call args → parse_expression
//! - parse_unary self-recursion (`!!!…`, `---…`)
//! - parse_binary_rhs right-recursion (right-nested `**` via parens;
//!   FLAT `**` chains are iterative left-assoc and never recurse)
//! - parse_statement → block constructs → parse_block → parse_statement
//! - strict-data: parse_data_value → parse_data_list / parse_data_map
//! - lexer next_token comment-skip / suppressed-newline (loop, not
//!   tail recursion — a debug build has no TCO)

use cosmix_mix::ast::Stmt;
use cosmix_mix::error::{MixError, MixResult};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;

const DEEP: usize = 10_000;

fn parse(src: &str) -> MixResult<Vec<Stmt>> {
    let mut lexer = Lexer::new(src);
    let tokens = lexer.tokenize()?;
    Parser::new(tokens, src).parse_program()
}

/// Assert `src` fails with the depth-cap parse error (and that getting
/// here at all means the process did not abort).
fn assert_too_deep(src: &str) {
    match parse(src) {
        Err(MixError::ParseError { msg, .. }) => {
            assert!(msg.contains("nesting too deep"), "unexpected msg: {msg}");
        }
        Err(other) => panic!("expected ParseError, got {other:?}"),
        Ok(_) => panic!("deeply nested input must not parse"),
    }
}

// --- deep input → clean error, not abort ---

#[test]
fn deep_nested_parens_error_cleanly() {
    let src = format!("print({}1{})", "(".repeat(DEEP), ")".repeat(DEEP));
    assert_too_deep(&src);
}

#[test]
fn deep_nested_brackets_error_cleanly() {
    let src = format!("$x = {}1{}", "[".repeat(DEEP), "]".repeat(DEEP));
    assert_too_deep(&src);
}

#[test]
fn deep_nested_maps_error_cleanly() {
    let src = format!("$x = {}1{}", "{a:".repeat(DEEP), "}".repeat(DEEP));
    assert_too_deep(&src);
}

#[test]
fn deep_unary_bang_chain_errors_cleanly() {
    let src = format!("print({}1)", "!".repeat(DEEP));
    assert_too_deep(&src);
}

#[test]
fn deep_unary_minus_chain_errors_cleanly() {
    // Space-separated: a bare `--` run would lex as a line comment.
    let src = format!("$x = {}1", "- ".repeat(DEEP));
    assert_too_deep(&src);
}

#[test]
fn deep_right_nested_power_errors_cleanly() {
    // `1**(1**(1**(…)))` — explicit right-nesting drives the recursion.
    let src = format!("$x = {}1{}", "1**(".repeat(DEEP), ")".repeat(DEEP));
    assert_too_deep(&src);
}

#[test]
fn flat_power_chain_is_iterative() {
    // A FLAT `**` chain is left-assoc (`prec + 1`) and parses with
    // constant stack — it must NOT hit the nesting cap. Kept small so
    // dropping the left-deep result tree stays shallow-ish.
    let src = format!("$x = {}", vec!["1"; 300].join("**"));
    parse(&src).expect("flat ** chain must parse");
}

#[test]
fn deep_nested_if_blocks_error_cleanly() {
    let mut src = "if 1 then\n".repeat(DEEP);
    src.push_str("print(1)\n");
    src.push_str(&"end\n".repeat(DEEP));
    assert_too_deep(&src);
}

// --- reasonable nesting still parses ---

#[test]
fn reasonable_nesting_still_parses() {
    let parens = format!("print({}1{})", "(".repeat(100), ")".repeat(100));
    parse(&parens).expect("100 nested parens must parse");

    let brackets = format!("$x = {}1{}", "[".repeat(100), "]".repeat(100));
    parse(&brackets).expect("100 nested brackets must parse");

    let maps = format!("$x = {}1{}", "{a:".repeat(100), "}".repeat(100));
    parse(&maps).expect("100 nested maps must parse");

    let unary = format!("$x = {}1", "!".repeat(100));
    parse(&unary).expect("100 chained unary ops must parse");

    // Statements cost more of the shared budget than expressions
    // (their parse frames are several times larger), so the block
    // ceiling sits near 50 — 40 leaves headroom for the inner print.
    let mut blocks = "if 1 then\n".repeat(40);
    blocks.push_str("print(1)\n");
    blocks.push_str(&"end\n".repeat(40));
    parse(&blocks).expect("40 nested if blocks must parse");
}

// --- the cap fits a 2 MB stack (tokio worker size) even in debug ---

#[test]
fn worst_case_depth_fits_2mb_stack() {
    // Drive every guarded cycle to its cap inside a 2 MB thread — the
    // stack a daemon-embedded parser actually gets. Statements have the
    // largest per-level frames, so the nested-if case is the budget's
    // worst case.
    let cases: Vec<String> = vec![
        format!("print({}1{})", "(".repeat(DEEP), ")".repeat(DEEP)),
        format!("$x = {}1{}", "[".repeat(DEEP), "]".repeat(DEEP)),
        format!("$x = {}1{}", "{a:".repeat(DEEP), "}".repeat(DEEP)),
        format!("print({}1)", "!".repeat(DEEP)),
        {
            let mut s = "if 1 then\n".repeat(DEEP);
            s.push_str("print(1)\n");
            s.push_str(&"end\n".repeat(DEEP));
            s
        },
    ];
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(move || {
            for src in &cases {
                assert_too_deep(src);
            }
        })
        .expect("spawn 2 MB thread")
        .join()
        .expect("worst-case parses must error cleanly on a 2 MB stack");
}

// --- strict-data path (from_conf_mix_str / parse_data_file feed it) ---

#[test]
fn deep_data_lists_error_cleanly() {
    let src = format!("{}1{}", "[".repeat(DEEP), "]".repeat(DEEP));
    match cosmix_mix::parse_data(&src) {
        Err(MixError::ParseError { msg, .. }) => {
            assert!(msg.contains("nesting too deep"), "unexpected msg: {msg}");
        }
        Err(other) => panic!("expected ParseError, got {other:?}"),
        Ok(_) => panic!("deeply nested data must not parse"),
    }
}

#[test]
fn deep_data_maps_error_cleanly() {
    let src = format!("{}1{}", "{a:".repeat(DEEP), "}".repeat(DEEP));
    match cosmix_mix::parse_data(&src) {
        Err(MixError::ParseError { msg, .. }) => {
            assert!(msg.contains("nesting too deep"), "unexpected msg: {msg}");
        }
        Err(other) => panic!("expected ParseError, got {other:?}"),
        Ok(_) => panic!("deeply nested data must not parse"),
    }
}

#[test]
fn reasonable_data_nesting_still_parses() {
    let src = format!("{}1{}", "{a:".repeat(100), "}".repeat(100));
    cosmix_mix::parse_data(&src).expect("100 nested data maps must parse");
}

// --- lexer: comment / suppressed-newline skip must be a loop ---

#[test]
fn comment_flood_inside_group_lexes_without_overflow() {
    // 100k comment lines inside an open paren: every one used to be a
    // tail self-call in next_token. A debug test binary has no TCO, so
    // this passing proves the loop rewrite.
    let mut src = String::from("$x = (1 +\n");
    src.push_str(&"-- comment line\n".repeat(100_000));
    src.push_str("1)\n");
    let mut lexer = Lexer::new(&src);
    let tokens = lexer.tokenize().expect("comment flood must lex");
    Parser::new(tokens, &src)
        .parse_program()
        .expect("comment flood must parse");
}

#[test]
fn hash_comment_flood_at_top_level_lexes_without_overflow() {
    // Top-level `#` comments: each comment+newline pair is one skip
    // iteration (the newline run collapses, but every comment line
    // re-enters the skip path).
    let mut src = "# comment\n".repeat(100_000);
    src.push_str("print(1)\n");
    let mut lexer = Lexer::new(&src);
    let tokens = lexer.tokenize().expect("hash comment flood must lex");
    Parser::new(tokens, &src)
        .parse_program()
        .expect("hash comment flood must parse");
}
