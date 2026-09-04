//! Anti-drift for `mix explain`, binary side. The lib's `lint_docs` test greps
//! analyzer.rs; the lexer/parser codes E1001–E1003 are assigned in THIS crate's
//! lint.rs, so a new code added there would slip past the lib test. This greps
//! lint.rs and asserts every `MIX-####` it names has a `lint_docs` record — so
//! neither the analyzer nor the lint driver can ship a code with no explanation.

/// Extract every `MIX-<Letter><4 digits>` code mentioned in `src`.
fn codes_in(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = src.as_bytes();
    let mut i = 0;
    while let Some(pos) = src[i..].find("MIX-") {
        let start = i + pos;
        i = start + 4;
        let c = &bytes[start..];
        if c.len() >= 9
            && c[4].is_ascii_uppercase()
            && c[5..9].iter().all(u8::is_ascii_digit)
        {
            out.push(String::from_utf8_lossy(&c[..9]).into_owned());
        }
    }
    out.sort();
    out.dedup();
    out
}

#[test]
fn every_lint_driver_code_has_an_explanation() {
    let src = include_str!("../src/lint.rs");
    let missing: Vec<String> = codes_in(src)
        .into_iter()
        .filter(|code| cosmix_mix::lint_docs::explain(code).is_none())
        .collect();
    assert!(
        missing.is_empty(),
        "lint.rs names codes with no lint_docs record (add them to lint_docs.rs): {missing:?}",
    );
}
