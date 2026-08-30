//! Maild-owned token-cap policies.

/// Apply the current priority-aware post-tokenisation cap.
///
/// Header, flag, and other non-body/non-URL evidence is retained first, then
/// body evidence, then URL evidence. Within each group the tokenizer's sorted
/// order is preserved. A cap only ever drops URL then body evidence, never
/// sender/header evidence.
pub(crate) fn apply_token_cap(tokens: &mut Vec<String>, cap: u32) {
    let cap = cap as usize;
    if cap == 0 || tokens.len() <= cap {
        return;
    }

    tokens.sort_by_key(|token| {
        if token.starts_with("u:") {
            2
        } else if token.starts_with("b:") {
            1
        } else {
            0
        }
    });
    tokens.truncate(cap);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_cap_bounds_url_bomb_without_dropping_headers() {
        let mut tokens = Vec::with_capacity(120_005);
        for i in 0..60_000 {
            tokens.push(format!("b:{i:05}"));
        }
        for i in 0..5 {
            tokens.push(format!("h:test:{i}"));
        }
        for i in 0..60_000 {
            tokens.push(format!("u:https://example.test/{i:05}"));
        }
        tokens.sort();

        apply_token_cap(&mut tokens, 50_000);

        assert_eq!(tokens.len(), 50_000);
        assert_eq!(
            tokens
                .iter()
                .filter(|token| token.starts_with("h:"))
                .count(),
            5
        );
        assert_eq!(
            tokens
                .iter()
                .filter(|token| token.starts_with("b:"))
                .count(),
            49_995
        );
        assert_eq!(
            tokens
                .iter()
                .filter(|token| token.starts_with("u:"))
                .count(),
            0
        );
    }
}
