//! P1 of the mix tokenizer fuzz/property corpus
//! (_doc/planned/mix-tokenizer-fuzz-corpus.md in the cosmix hub): property-based
//! coverage of the Mix LANGUAGE lexer and parser (distinct from the shell-dispatch
//! tokenizer covered by P0 in `cosmix-mix`).
//!
//! Oracles:
//! - robustness — `tokenize` / `parse_program` never panic on arbitrary input,
//!   only `Ok` or a structured `MixError`;
//! - positive correctness — generated VALID Mix always parses (the round-trip
//!   substitute, since the lexer has no token→source renderer to round-trip
//!   through).

use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;
use proptest::prelude::*;

/// Lex `s`, returning the token result. A fresh `Lexer` per call (it is consumed
/// by `tokenize`).
fn lex(s: &str) -> cosmix_mix::error::MixResult<Vec<cosmix_mix::token::SpannedToken>> {
    let mut lx = Lexer::new(s);
    lx.tokenize()
}

/// A strategy over VALID Mix statements. Every arm is a shape verified to parse
/// against the live binary; idents start with `v` so a random name can never
/// collide with a Mix keyword.
fn valid_stmt() -> impl Strategy<Value = String> {
    let ident = "v[a-z0-9_]{0,5}";
    prop_oneof![
        (ident, 0i64..100_000).prop_map(|(n, v)| format!("${n} = {v}")),
        (0i64..100_000).prop_map(|v| format!("print({v})")),
        (0i64..1_000, 0i64..1_000).prop_map(|(a, b)| format!("print({a} + {b})")),
        ident.prop_map(|n| format!("${n} = nil")),
        Just("print(\"a\" .. \"b\")".to_string()),
    ]
}

/// A whole valid program: 1..6 statements, separated by newline or `;`.
fn valid_program() -> impl Strategy<Value = String> {
    (prop::collection::vec(valid_stmt(), 1..6), any::<bool>())
        .prop_map(|(stmts, use_semicolon)| stmts.join(if use_semicolon { "; " } else { "\n" }))
}

proptest! {
    // Robustness: the lexer never panics; arbitrary input yields Ok or MixError.
    #[test]
    fn lex_never_panics(s in "(?s).{0,300}") {
        let _ = lex(&s);
    }

    // Robustness: the parser never panics. Feed it whatever the lexer produced
    // (only lexable input reaches the parser, matching the real pipeline).
    #[test]
    fn parse_never_panics(s in "(?s).{0,300}") {
        if let Ok(tokens) = lex(&s) {
            let _ = Parser::new(tokens, &s).parse_program();
        }
    }

    // Positive correctness: generated valid Mix always lexes AND parses. Catches
    // a parser/lexer regression that rejects syntax it must accept — the value
    // the absent byte-round-trip would otherwise provide.
    #[test]
    fn valid_programs_parse(src in valid_program()) {
        let tokens = lex(&src).map_err(|e| TestCaseError::fail(format!("lex rejected valid program {src:?}: {e:?}")))?;
        let parsed = Parser::new(tokens, &src).parse_program();
        prop_assert!(parsed.is_ok(), "parser rejected valid program {:?}: {:?}", src, parsed.err());
    }
}
