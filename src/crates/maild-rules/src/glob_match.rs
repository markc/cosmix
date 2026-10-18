//! Sender-glob matching for allowlist / blocklist entries.
//!
//! Supports two forms:
//!
//! - Literal address: `brother@example.com`. Case-insensitive
//!   exact match against `RuleContext::envelope_from`.
//! - Wildcard: `*@vendor.example` or `*@*.example.com`. Translated
//!   to a regex anchored at both ends with `*` mapping to `.*` and
//!   `?` mapping to `.`. Other regex metacharacters are escaped.
//!
//! Patterns containing neither `*` nor `?` use the literal-equality
//! fast path. Compilation happens at load time via
//! `compile_sender_globs`; `matches_any` walks the compiled list.

use regex::Regex;

#[derive(Debug, Clone)]
pub(crate) enum SenderGlobMatcher {
    Literal(String),
    Pattern(Regex),
}

impl SenderGlobMatcher {
    pub(crate) fn compile(pat: &str) -> Result<Self, regex::Error> {
        if !pat.contains('*') && !pat.contains('?') {
            Ok(Self::Literal(pat.to_ascii_lowercase()))
        } else {
            let mut re = String::with_capacity(pat.len() + 4);
            re.push_str("(?i)^");
            for ch in pat.chars() {
                match ch {
                    '*' => re.push_str(".*"),
                    '?' => re.push('.'),
                    c if c.is_ascii_alphanumeric() => re.push(c),
                    c => {
                        re.push('\\');
                        re.push(c);
                    }
                }
            }
            re.push('$');
            Ok(Self::Pattern(Regex::new(&re)?))
        }
    }

    pub(crate) fn matches(&self, addr: &str) -> bool {
        match self {
            Self::Literal(lit) => lit.eq_ignore_ascii_case(addr),
            Self::Pattern(re) => re.is_match(addr),
        }
    }
}

/// Validate a sender-glob pattern without keeping the compiled
/// matcher. Mirrors [`SenderGlobMatcher::compile`] — by design the
/// validator and the runtime matcher share the same compile path so a
/// pattern accepted here cannot fail at delivery time.
///
/// Returns the regex `Display`-formatted error string on failure so
/// callers in crates that do not depend on `regex` can surface it
/// without re-importing the error type.
pub fn validate_sender_glob(pat: &str) -> Result<(), String> {
    SenderGlobMatcher::compile(pat)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Compile a list of sender globs. Patterns that fail to compile are
/// dropped silently — caller should surface compile errors via the
/// `RuleSet` failure list.
pub(crate) fn compile_sender_globs(globs: &[String]) -> Vec<SenderGlobMatcher> {
    globs
        .iter()
        .filter_map(|g| SenderGlobMatcher::compile(g).ok())
        .collect()
}

/// True when any matcher in the list fires against `addr`.
pub(crate) fn matches_any(matchers: &[SenderGlobMatcher], addr: &str) -> bool {
    matchers.iter().any(|m| m.matches(addr))
}
