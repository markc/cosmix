//! Canonical user-keyword (IMAP "Tags" / JMAP custom `keywords`) parsing,
//! validation, and normalisation — the single source of truth shared by the
//! IMAP (`imap::op`) and JMAP (`jmap::email`) wire layers.
//!
//! ## System keywords vs user keywords
//!
//! The five RFC 9051 §2.3.2 system flags (`\Seen \Flagged \Answered \Draft
//! \Deleted`) live in the [`Flags`] bitmap; JMAP projects the first four as the
//! reserved `$`-keywords `$seen / $flagged / $answered / $draft` (`\Deleted`
//! has no JMAP keyword — deletion is `Email/set destroy`). Everything else is a
//! *user keyword*: an arbitrary client-chosen string stored in `Tags`.
//!
//! ## Normalisation policy (decided + pinned)
//!
//! User keywords are normalised to **NFC + lowercase** on store and matched
//! case-insensitively. The `$` prefix is **reserved only for the four system
//! names** (`$seen/$flagged/$answered/$draft`): those route to [`Flags`] and
//! are rejected as user keywords. Every OTHER `$`-keyword is a normal user
//! keyword and IS stored — the IMAP/JMAP keyword registry (RFC 5788 / RFC 8621
//! §4.1.4) is the client's namespace (Thunderbird's tags are `$label1..N`;
//! `$Forwarded` / `$Junk` etc. are registered), and `PERMANENTFLAGS \*`
//! obliges us to persist them (RFC 9051 §2.3.2). The accepted
//! charset is the RFC 9051 IMAP `atom` minus `atom-specials` (and minus control
//! characters, space, and `\`), which is a conservative subset of the RFC 8621
//! JMAP keyword grammar — so one validated form round-trips through both
//! protocols. Bounds ([`MAX_KEYWORD_LEN`], [`MAX_KEYWORDS_PER_MESSAGE`]) cap an
//! abusive client from bloating a membership row.
//!
//! Changing this policy later re-buckets already-stored keywords, so it is
//! intentionally centralised here rather than duplicated per wire layer.

use cosmix_mds::Flags;
use unicode_normalization::UnicodeNormalization;

/// Maximum length, in bytes of the normalised form, of one user keyword.
pub const MAX_KEYWORD_LEN: usize = 64;

/// Maximum number of user keywords stored on a single message.
pub const MAX_KEYWORDS_PER_MESSAGE: usize = 64;

/// Why a client keyword name was rejected. The wire layers map these to the
/// protocol-appropriate error (`BAD`/`NO` for IMAP, `invalidProperties` for
/// JMAP).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeywordError {
    /// Empty (before or after NFC normalisation).
    Empty,
    /// One of the four allocated system `$`-names ($seen/$flagged/$answered/
    /// $draft) — those map to the system flags and cannot also be a user
    /// keyword. Every OTHER `$`-keyword (`$label2`, `$forwarded`, …) is allowed.
    Reserved,
    /// Longer than [`MAX_KEYWORD_LEN`] bytes (normalised form).
    TooLong,
    /// Contains a control character, space, or an IMAP `atom-special`
    /// (`( ) { } % * " \ ]`).
    BadChar(char),
    /// More than [`MAX_KEYWORDS_PER_MESSAGE`] user keywords on one message.
    TooMany,
}

impl std::fmt::Display for KeywordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeywordError::Empty => write!(f, "keyword must not be empty"),
            KeywordError::Reserved => write!(
                f,
                "keyword is a reserved system name ($seen/$flagged/$answered/$draft)"
            ),
            KeywordError::TooLong => {
                write!(f, "keyword exceeds {MAX_KEYWORD_LEN} bytes")
            }
            KeywordError::BadChar(c) => {
                write!(f, "keyword contains an invalid character {c:?}")
            }
            KeywordError::TooMany => write!(
                f,
                "message would carry more than {MAX_KEYWORDS_PER_MESSAGE} user keywords"
            ),
        }
    }
}

impl std::error::Error for KeywordError {}

/// A character that may not appear in a user keyword: the IMAP `atom-special`
/// set (RFC 9051) plus the always-illegal `\` and control characters. Space is
/// rejected too (it is the atom separator).
fn is_forbidden_keyword_char(c: char) -> bool {
    c.is_control() || c == ' ' || matches!(c, '(' | ')' | '{' | '}' | '%' | '*' | '"' | '\\' | ']')
}

/// Normalise + validate one client-supplied **user** keyword name to its
/// canonical storage form (**NFC + lowercase**), rejecting the four reserved
/// system `$`-names and enforcing the charset + bounds rules.
///
/// Both wire layers call this: IMAP STORE/APPEND atoms (after the caller has
/// confirmed the token has no leading `\`) and JMAP custom `keywords` (after
/// the caller has confirmed it is not one of the four system `$`-names). The
/// returned `String` is exactly what gets inserted into `Tags`. A `$`-prefixed
/// name is rejected here ([`KeywordError::Reserved`]) — system keywords must be
/// routed to [`Flags`] by the caller *before* this is reached.
pub fn normalise_user_keyword(raw: &str) -> Result<String, KeywordError> {
    if raw.is_empty() {
        return Err(KeywordError::Empty);
    }
    // No trimming: a keyword is an atom, not free text, so leading/trailing
    // whitespace is a charset violation (rejected below as `BadChar`), never
    // silently canonicalised away. NFC composes a decomposed form so `é`
    // (U+00E9) and `e`+◌́ fold together, then lowercase for case-insensitive
    // matching. Validation runs on the normalised form so the stored bytes are
    // exactly what later queries normalise to.
    let normalised: String = raw.nfc().collect::<String>().to_lowercase();
    if normalised.is_empty() {
        return Err(KeywordError::Empty);
    }
    // The `$` prefix is RESERVED — but only the four allocated SYSTEM names
    // ($seen/$flagged/$answered/$draft) are rejected here: those map to the
    // system `Flags` and must never become a user `Tags` entry, else a user
    // keyword would collide with a system keyword in the unified JMAP
    // `keywords` projection. EVERY OTHER `$`-keyword is a legitimate user
    // keyword: the IMAP/JMAP keyword registry (RFC 5788 / RFC 8621 §4.1.4) is
    // the client's namespace and real clients depend on it — Thunderbird's
    // built-in tags are `$label1`..`$label5`, and `$Forwarded` / `$MDNSent` /
    // `$Junk` / `$NotJunk` / `$Phishing` are registered. Since SELECT
    // advertises `\*` in PERMANENTFLAGS, RFC 9051 §2.3.2 obliges us to persist
    // them; rejecting `$label2` is both a client-compat break and a spec
    // violation.
    if jmap_system_keyword_bit(&normalised).is_some() {
        return Err(KeywordError::Reserved);
    }
    if normalised.len() > MAX_KEYWORD_LEN {
        return Err(KeywordError::TooLong);
    }
    if let Some(c) = normalised.chars().find(|&c| is_forbidden_keyword_char(c)) {
        return Err(KeywordError::BadChar(c));
    }
    Ok(normalised)
}

/// The JMAP `$`-keyword name for a system [`Flags`] bit, or `None` for a bit
/// with no JMAP keyword (`\Deleted`). Single source of truth for the bit→name
/// projection — collapses the formerly hand-duplicated match arms.
pub fn jmap_keyword_for_bit(bit: u32) -> Option<&'static str> {
    match bit {
        Flags::SEEN => Some("$seen"),
        Flags::FLAGGED => Some("$flagged"),
        Flags::ANSWERED => Some("$answered"),
        Flags::DRAFT => Some("$draft"),
        _ => None,
    }
}

/// The system [`Flags`] bit for a JMAP `$`-keyword name, or `None` if `name`
/// is not one of the four allocated system keywords. (`$deleted` is
/// deliberately absent — `\Deleted` has no JMAP keyword.)
pub fn jmap_system_keyword_bit(name: &str) -> Option<u32> {
    match name {
        "$seen" => Some(Flags::SEEN),
        "$flagged" => Some(Flags::FLAGGED),
        "$answered" => Some(Flags::ANSWERED),
        "$draft" => Some(Flags::DRAFT),
        _ => None,
    }
}

/// What a client-supplied keyword name resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeywordClass {
    /// A known system keyword → OR this [`Flags`] bit into the flag set.
    System(u32),
    /// A user keyword → insert this normalised name into `Tags`.
    User(String),
}

/// Classify a JMAP keyword-object key (or any `$`-spelled keyword name): a
/// known system `$`-name → [`KeywordClass::System`]; any other name (non-`$`,
/// OR a registry `$`-keyword like `$forwarded`/`$label2`) → normalise to
/// [`KeywordClass::User`]. Only a mixed-case variant of a system name (e.g.
/// `$Seen`, which normalises onto `$seen`) is rejected as
/// [`KeywordError::Reserved`]. The system check wins first, so the four
/// allocated lowercase `$`-names route to `Flags`, never a tag.
pub fn classify_jmap_keyword(name: &str) -> Result<KeywordClass, KeywordError> {
    if let Some(bit) = jmap_system_keyword_bit(name) {
        return Ok(KeywordClass::System(bit));
    }
    Ok(KeywordClass::User(normalise_user_keyword(name)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_case_and_rejects_whitespace() {
        assert_eq!(normalise_user_keyword("Public").unwrap(), "public");
        assert_eq!(normalise_user_keyword("WORK").unwrap(), "work");
        assert_eq!(normalise_user_keyword("important").unwrap(), "important");
        // Leading/trailing/interior whitespace is a charset violation, not
        // trimmed away.
        assert!(matches!(
            normalise_user_keyword(" Public "),
            Err(KeywordError::BadChar(' '))
        ));
        assert!(matches!(
            normalise_user_keyword("a\tb"),
            Err(KeywordError::BadChar(_))
        ));
    }

    #[test]
    fn slash_and_tilde_are_valid_keyword_chars() {
        // `/` and `~` are not IMAP atom-specials — a keyword may contain them
        // (JMAP addresses them via RFC 6901 pointer escaping `~1`/`~0`).
        assert_eq!(normalise_user_keyword("a/b").unwrap(), "a/b");
        assert_eq!(normalise_user_keyword("a~b").unwrap(), "a~b");
    }

    #[test]
    fn case_insensitive_round_trip() {
        // Store and query both normalise; different casings collapse to one.
        let a = normalise_user_keyword("ProjectX").unwrap();
        let b = normalise_user_keyword("projectx").unwrap();
        let c = normalise_user_keyword("PROJECTX").unwrap();
        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    #[test]
    fn nfc_folds_decomposed_and_precomposed() {
        // Precomposed "café" (U+00E9) and decomposed "cafe\u{301}" must
        // normalise to the same stored keyword.
        let pre = normalise_user_keyword("caf\u{00e9}").unwrap();
        let dec = normalise_user_keyword("cafe\u{0301}").unwrap();
        assert_eq!(pre, dec);
    }

    #[test]
    fn reserves_only_the_four_system_dollar_names() {
        // The four system $-names are reserved (they map to Flags), even
        // reaching this fn directly / in any case.
        for sys in ["$seen", "$flagged", "$answered", "$draft", "$SEEN"] {
            assert_eq!(
                normalise_user_keyword(sys),
                Err(KeywordError::Reserved),
                "{sys}"
            );
        }
        // Every OTHER $-keyword is a valid user keyword — Thunderbird's built-in
        // tags ($label1..N) and registry names ($Forwarded/$Junk/...) — stored
        // normalised to lowercase. Rejecting these broke real IMAP clients.
        assert_eq!(normalise_user_keyword("$label2").unwrap(), "$label2");
        assert_eq!(normalise_user_keyword("$Forwarded").unwrap(), "$forwarded");
        assert_eq!(normalise_user_keyword("$Junk").unwrap(), "$junk");
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(normalise_user_keyword(""), Err(KeywordError::Empty));
        // All-whitespace is a charset violation now (no trim).
        assert!(matches!(
            normalise_user_keyword("   "),
            Err(KeywordError::BadChar(' '))
        ));
    }

    #[test]
    fn rejects_atom_specials_and_controls() {
        for bad in ["a(b", "a)b", "a b", "a\"b", "a\\b", "a*b", "a%b", "a]b"] {
            assert!(
                matches!(normalise_user_keyword(bad), Err(KeywordError::BadChar(_))),
                "expected BadChar for {bad:?}"
            );
        }
        assert!(matches!(
            normalise_user_keyword("a\u{0007}b"),
            Err(KeywordError::BadChar(_))
        ));
    }

    #[test]
    fn rejects_over_length() {
        let long = "a".repeat(MAX_KEYWORD_LEN + 1);
        assert_eq!(normalise_user_keyword(&long), Err(KeywordError::TooLong));
        let ok = "a".repeat(MAX_KEYWORD_LEN);
        assert!(normalise_user_keyword(&ok).is_ok());
    }

    #[test]
    fn classify_routes_system_vs_user() {
        assert_eq!(
            classify_jmap_keyword("$seen"),
            Ok(KeywordClass::System(Flags::SEEN))
        );
        assert_eq!(
            classify_jmap_keyword("$flagged"),
            Ok(KeywordClass::System(Flags::FLAGGED))
        );
        assert_eq!(
            classify_jmap_keyword("Public"),
            Ok(KeywordClass::User("public".to_string()))
        );
        // A registered (non-system) $-name routes to a user tag, stored as-is.
        assert_eq!(
            classify_jmap_keyword("$Forwarded"),
            Ok(KeywordClass::User("$forwarded".to_string()))
        );
        // Only the four system $-names are reserved.
        assert_eq!(
            classify_jmap_keyword("$seen"),
            Ok(KeywordClass::System(Flags::SEEN))
        );
    }

    #[test]
    fn bit_name_round_trip() {
        for bit in [Flags::SEEN, Flags::FLAGGED, Flags::ANSWERED, Flags::DRAFT] {
            let name = jmap_keyword_for_bit(bit).unwrap();
            assert_eq!(jmap_system_keyword_bit(name), Some(bit));
        }
        // \Deleted has no JMAP keyword.
        assert_eq!(jmap_keyword_for_bit(Flags::DELETED), None);
    }
}
