#![no_main]
//! Coverage-guided robustness target for the Mix lexer: `Lexer::tokenize` must
//! never panic, hang, or trip a sanitizer on ANY input — only return `Ok` or a
//! structured `MixError`. Non-UTF-8 byte sequences are skipped (Mix source is
//! `&str`); libFuzzer mutates toward valid-UTF-8 paths via the seed corpus.
use cosmix_mix::lexer::Lexer;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let mut lx = Lexer::new(s);
        let _ = lx.tokenize();
    }
});
