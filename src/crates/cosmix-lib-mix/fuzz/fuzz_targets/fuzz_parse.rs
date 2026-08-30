#![no_main]
//! Coverage-guided robustness target for the Mix parser: lex, then (only on a
//! clean lex, matching the real pipeline) `Parser::parse_program` must never
//! panic, hang, or trip a sanitizer — only `Ok` or a structured `MixError`.
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let mut lx = Lexer::new(s);
        if let Ok(tokens) = lx.tokenize() {
            let mut p = Parser::new(tokens, s);
            let _ = p.parse_program();
        }
    }
});
