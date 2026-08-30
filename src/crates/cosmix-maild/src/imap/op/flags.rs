//! Shared IMAP flag-atom parsing + rendering for STORE / APPEND / FETCH — the
//! single implementation that replaces the hand-synced copies these ops
//! previously each carried.
//!
//! Two halves:
//! - **parse** (write path): an atom is either a `\`-prefixed RFC 9051 §2.3.2
//!   system flag (`\Answered \Flagged \Deleted \Seen \Draft`) or a bare-atom
//!   *user keyword*, normalised + validated via [`crate::keyword`] (NFC +
//!   lowercase, charset, bounds). System keywords go to the [`Flags`] bitmap,
//!   user keywords to the [`Tags`] set.
//! - **render** (read path): `FLAGS (\Seen Important $work)` — the system
//!   `\`-atoms in canonical order, then the user keywords bare (no `\`), sorted
//!   (`Tags` is a `BTreeSet`). STORE's FETCH echo and FETCH itself share this
//!   so the two never disagree.

use crate::keyword;
use cosmix_mds::{Flags, Tags};

/// A parsed IMAP flag atom: a system-flag bit, or a normalised user keyword.
pub enum FlagToken {
    /// A `\`-system flag bit (`Flags::SEEN` etc.).
    System(u32),
    /// A user keyword, already normalised (NFC + lowercase).
    User(String),
}

/// The IMAP system-flag bit for a `\`-atom name (already lowercased), or
/// `None` if it is not one of the five RFC 9051 §2.3.2 system flags. Unlike
/// the JMAP `$`-keyword set this includes `\Deleted` (no JMAP equivalent).
fn imap_system_flag_bit(name_lower: &str) -> Option<u32> {
    match name_lower {
        "answered" => Some(Flags::ANSWERED),
        "flagged" => Some(Flags::FLAGGED),
        "deleted" => Some(Flags::DELETED),
        "seen" => Some(Flags::SEEN),
        "draft" => Some(Flags::DRAFT),
        _ => None,
    }
}

/// Parse one flag atom (RFC 9051 §2.3.2). A `\`-prefixed atom is a system flag
/// (case-insensitive on the name); an unknown `\Xxx` is rejected. A bare atom
/// is a user keyword, normalised + validated via [`crate::keyword`]. The `Err`
/// string is the human-readable reason for a `BAD`/`NO` tagged response.
pub fn parse_flag_token(tok: &str) -> Result<FlagToken, String> {
    if let Some(name) = tok.strip_prefix('\\') {
        let lower = name.to_ascii_lowercase();
        match imap_system_flag_bit(&lower) {
            Some(bit) => Ok(FlagToken::System(bit)),
            None => Err(format!(
                "unknown system flag \\{name} (only \\Answered \\Flagged \\Deleted \\Seen \\Draft supported)"
            )),
        }
    } else {
        keyword::normalise_user_keyword(tok)
            .map(FlagToken::User)
            .map_err(|e| format!("invalid keyword {tok:?}: {e}"))
    }
}

/// Parse the *inner* of an IMAP `<flag-list>` (the space-separated atoms,
/// parentheses already stripped by the caller) into the system [`Flags`]
/// bitmap + the user-keyword [`Tags`] set. Enforces the per-message keyword
/// cap. An empty input is `(Flags(0), Tags::new())`.
pub fn parse_flag_list_inner(inner: &str) -> Result<(Flags, Tags), String> {
    let mut bits: u32 = 0;
    let mut tags = Tags::new();
    for tok in inner.split_ascii_whitespace() {
        match parse_flag_token(tok)? {
            FlagToken::System(bit) => bits |= bit,
            FlagToken::User(name) => {
                tags.insert(name);
            }
        }
    }
    if tags.len() > keyword::MAX_KEYWORDS_PER_MESSAGE {
        return Err(format!("{}", keyword::KeywordError::TooMany));
    }
    Ok((Flags(bits), tags))
}

/// Render `(\Answered \Flagged \Deleted \Seen \Draft <keyword>…)` for a FETCH
/// `FLAGS` item (or a STORE FETCH echo): the set system `\`-atoms in the
/// canonical RFC 9051 §2.3.2 order, then each user keyword **bare** (no leading
/// `\` — the `\` sigil is reserved for system flags), in `Tags` sort order.
/// Unset system bits and unallocated bits are dropped.
pub fn render_keywords(flags: Flags, tags: &Tags) -> String {
    let mut parts: Vec<String> = Vec::new();
    if flags.0 & Flags::ANSWERED != 0 {
        parts.push("\\Answered".into());
    }
    if flags.0 & Flags::FLAGGED != 0 {
        parts.push("\\Flagged".into());
    }
    if flags.0 & Flags::DELETED != 0 {
        parts.push("\\Deleted".into());
    }
    if flags.0 & Flags::SEEN != 0 {
        parts.push("\\Seen".into());
    }
    if flags.0 & Flags::DRAFT != 0 {
        parts.push("\\Draft".into());
    }
    for t in tags.iter() {
        parts.push(t.clone());
    }
    format!("({})", parts.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sys(tok: &str) -> u32 {
        match parse_flag_token(tok).unwrap() {
            FlagToken::System(b) => b,
            FlagToken::User(_) => panic!("expected system flag for {tok}"),
        }
    }

    fn usr(tok: &str) -> String {
        match parse_flag_token(tok).unwrap() {
            FlagToken::User(s) => s,
            FlagToken::System(_) => panic!("expected user keyword for {tok}"),
        }
    }

    #[test]
    fn parses_system_flags_case_insensitively() {
        assert_eq!(sys("\\Seen"), Flags::SEEN);
        assert_eq!(sys("\\seen"), Flags::SEEN);
        assert_eq!(sys("\\DELETED"), Flags::DELETED);
        assert_eq!(sys("\\Draft"), Flags::DRAFT);
    }

    #[test]
    fn parses_bare_atom_as_normalised_user_keyword() {
        assert_eq!(usr("Important"), "important");
        assert_eq!(usr("work"), "work");
    }

    #[test]
    fn rejects_unknown_backslash_flag() {
        assert!(parse_flag_token("\\Bogus").is_err());
    }

    #[test]
    fn accepts_registry_dollar_keyword_rejects_malformed() {
        // A registry `$`-keyword (Thunderbird `$label2`, `$Forwarded`) is a
        // valid user keyword — accepted, normalised.
        assert_eq!(usr("$label2"), "$label2");
        assert_eq!(usr("$Forwarded"), "$forwarded");
        // A bare system `$`-name and an atom-special are rejected.
        assert!(parse_flag_token("$seen").is_err());
        assert!(parse_flag_token("a(b").is_err());
    }

    #[test]
    fn parse_flag_list_splits_flags_and_tags() {
        let (flags, tags) = parse_flag_list_inner("\\Seen Important \\Flagged work").unwrap();
        assert_eq!(flags, Flags(Flags::SEEN | Flags::FLAGGED));
        assert_eq!(
            tags,
            Tags::from(vec!["important".to_string(), "work".to_string()])
        );
        // Empty input.
        let (f0, t0) = parse_flag_list_inner("").unwrap();
        assert_eq!(f0, Flags(0));
        assert!(t0.is_empty());
    }

    #[test]
    fn renders_system_then_user_bare() {
        let tags = Tags::from(vec!["important".to_string(), "work".to_string()]);
        assert_eq!(
            render_keywords(Flags(Flags::SEEN | Flags::FLAGGED), &tags),
            "(\\Flagged \\Seen important work)"
        );
        assert_eq!(render_keywords(Flags(0), &Tags::new()), "()");
        // System-only and user-only.
        assert_eq!(
            render_keywords(Flags(Flags::SEEN), &Tags::new()),
            "(\\Seen)"
        );
    }
}
