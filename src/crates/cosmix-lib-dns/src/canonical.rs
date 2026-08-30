//! Name canonicalization (`canonical.rs`).
//!
//! `Name` is **canonical by construction**: lower-case, absolute
//! (trailing dot), validated labels. The pipeline step the prompt calls
//! "canonicalize" is discharged here at parse/decode time, so every
//! `Name` value elsewhere in the crate is already canonical — lookup,
//! conflict detection and `config_hash` never re-canonicalize.
//!
//! 0x20 / case handling: lookup and comparison are on this canonical
//! lower-case form; the *resolver* echoes the query QUESTION verbatim
//! (original case) by cloning the inbound `Query` — see `resolver.rs`.
//! That split (compare canonical, echo verbatim) is the whole of 0x20
//! for an authoritative server, so there is no separate 0x20 helper.

use hickory_proto::rr::Name as HName;
use std::fmt;

/// A canonical, lower-case, absolute DNS name (always ends in `.`).
///
/// The single constructor (`parse`) enforces the invariant; there is no
/// way to build a non-canonical `Name`. `ZoneName` wraps this same type
/// (see `model`) so zone-apex comparison is plain string equality.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Name(String);

/// Why a candidate name string was not a valid DNS name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NameError {
    Empty,
    LabelTooLong(String),
    NameTooLong,
    EmptyLabel,
    BadChar { label: String, ch: char },
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NameError::Empty => write!(f, "empty name"),
            NameError::LabelTooLong(l) => write!(f, "label exceeds 63 octets: {l:?}"),
            NameError::NameTooLong => write!(f, "name exceeds 255 octets"),
            NameError::EmptyLabel => write!(f, "empty label (consecutive dots)"),
            NameError::BadChar { label, ch } => {
                write!(f, "illegal character {ch:?} in label {label:?}")
            }
        }
    }
}

impl std::error::Error for NameError {}

impl Name {
    /// Parse and canonicalize a **strict** domain name.
    ///
    /// Accepts an optional trailing dot; the result always has one. Labels
    /// are lower-cased; the permitted character set is `[a-z0-9-_]`
    /// (underscore is required for `_service._proto` SRV owner names).
    /// Empty input, empty interior labels, over-long labels/names, and a
    /// `*` wildcard label are rejected. Use this for every name EXCEPT a
    /// record owner — rdata targets (MX/SRV/NS/PTR), SOA primary/mbox, NS
    /// items and the zone apex must never be a wildcard.
    pub fn parse(input: &str) -> Result<Name, NameError> {
        Self::parse_inner(input, false)
    }

    /// Parse a record-**owner** name, additionally allowing a single
    /// leftmost `*` label (RFC 4592 wildcard owner, e.g. `*.example.org`).
    /// ONLY the record `name` field uses this; everything else uses
    /// `parse`, so a `*` can never reach storage in a non-owner position.
    pub fn parse_owner(input: &str) -> Result<Name, NameError> {
        Self::parse_inner(input, true)
    }

    fn parse_inner(input: &str, allow_wildcard: bool) -> Result<Name, NameError> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(NameError::Empty);
        }
        // Root.
        if trimmed == "." {
            return Ok(Name(".".to_string()));
        }
        let body = trimmed.strip_suffix('.').unwrap_or(trimmed);
        if body.is_empty() {
            return Err(NameError::Empty);
        }
        let mut canonical = String::with_capacity(body.len() + 1);
        for (i, label) in body.split('.').enumerate() {
            if label.is_empty() {
                return Err(NameError::EmptyLabel);
            }
            if label.len() > 63 {
                return Err(NameError::LabelTooLong(label.to_string()));
            }
            // RFC 4592 wildcard owner: a bare `*` label, accepted ONLY when
            // parsing a record owner (`allow_wildcard`) AND at the leftmost
            // position. Any other `*` — non-owner context, non-leftmost
            // position, or embedded ("*foo"/"fo*o") — falls through to the
            // per-char rule below and is rejected as an illegal character.
            if label == "*" && allow_wildcard && i == 0 {
                canonical.push('*');
                canonical.push('.');
                continue;
            }
            for ch in label.chars() {
                let lc = ch.to_ascii_lowercase();
                let ok = lc.is_ascii_lowercase() || lc.is_ascii_digit() || lc == '-' || lc == '_';
                if !ok {
                    return Err(NameError::BadChar {
                        label: label.to_string(),
                        ch,
                    });
                }
                canonical.push(lc);
            }
            canonical.push('.');
        }
        // Wire length: labels + length octets + root ≈ body.len()+2.
        if canonical.len() > 255 {
            return Err(NameError::NameTooLong);
        }
        Ok(Name(canonical))
    }

    /// Build the canonical form from a hickory `Name` (decode path).
    pub fn from_hickory(h: &HName) -> Name {
        if h.is_root() {
            return Name(".".to_string());
        }
        let mut s = String::new();
        for label in h.iter() {
            let lossy = String::from_utf8_lossy(label);
            for ch in lossy.chars() {
                s.push(ch.to_ascii_lowercase());
            }
            s.push('.');
        }
        Name(s)
    }

    /// Convert to a hickory `Name` (wire-edge only — `wire.rs`).
    pub fn to_hickory(&self) -> HName {
        // Canonical, lower-case, absolute — `from_ascii` parses it as an
        // FQDN. The label set was validated in `parse`/`from_hickory`,
        // so this does not fail in practice; fall back to root rather
        // than panicking on the impossible case.
        HName::from_ascii(&self.0).unwrap_or_else(|_| HName::root())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0 == "."
    }

    /// `self` is in zone `zone` iff it equals the apex or is a strict
    /// subdomain of it. Both are canonical absolute strings, so this is
    /// pure string work with a label-boundary guard.
    pub fn in_zone(&self, zone: &Name) -> bool {
        if self.0 == zone.0 {
            return true;
        }
        if zone.is_root() {
            return true;
        }
        // ".bus." is the boundary-safe suffix for zone "bus.".
        self.0.ends_with(&format!(".{}", zone.0))
    }

    /// Label count (root = 0). Used to pick the longest-suffix zone.
    pub fn label_count(&self) -> usize {
        if self.is_root() {
            0
        } else {
            self.0.matches('.').count()
        }
    }

    /// The parent name (drop the leftmost label). `alpha.bus.` →
    /// `bus.`; a single-label name → root `.`; root → `None`. Used to
    /// walk the ancestor chain for empty-non-terminal population and the
    /// RFC 4592 closest-encloser search.
    pub fn parent(&self) -> Option<Name> {
        if self.is_root() {
            return None;
        }
        match self.0.split_once('.') {
            // `rest` keeps its own trailing dot, so it stays canonical.
            Some((_, rest)) if !rest.is_empty() => Some(Name(rest.to_string())),
            // Single label (`bus.`) → its parent is the root.
            _ => Some(Name(".".to_string())),
        }
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_case_and_absoluteness() {
        let a = Name::parse("Alpha.BUS").unwrap();
        let b = Name::parse("alpha.bus.").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.as_str(), "alpha.bus.");
    }

    #[test]
    fn underscore_labels_allowed_for_srv() {
        let n = Name::parse("_imaps._tcp.alpha.bus").unwrap();
        assert_eq!(n.as_str(), "_imaps._tcp.alpha.bus.");
    }

    #[test]
    fn wildcard_owner_only_leftmost_strict_parse_never() {
        // `parse_owner` accepts a bare leftmost `*` (RFC 4592 owner)...
        let w = Name::parse_owner("*.example.org").unwrap();
        assert_eq!(w.as_str(), "*.example.org.");
        // ...but strict `parse` rejects `*` entirely (non-owner fields:
        // MX/SRV/NS/PTR targets, SOA, apex).
        assert!(matches!(
            Name::parse("*.example.org"),
            Err(NameError::BadChar { .. })
        ));
        // `*` embedded in a label stays illegal even for an owner.
        assert!(matches!(
            Name::parse_owner("foo*.example.org"),
            Err(NameError::BadChar { .. })
        ));
        assert!(matches!(
            Name::parse_owner("*foo.example.org"),
            Err(NameError::BadChar { .. })
        ));
        // `*` in any non-leftmost position is rejected, even for an owner.
        assert!(matches!(
            Name::parse_owner("foo.*.example.org"),
            Err(NameError::BadChar { .. })
        ));
        assert!(matches!(
            Name::parse_owner("example.*"),
            Err(NameError::BadChar { .. })
        ));
    }

    #[test]
    fn parent_walks_to_root() {
        let n = Name::parse("a.b.bus").unwrap();
        let p = n.parent().unwrap();
        assert_eq!(p.as_str(), "b.bus.");
        let pp = p.parent().unwrap();
        assert_eq!(pp.as_str(), "bus.");
        let root = pp.parent().unwrap();
        assert!(root.is_root());
        assert_eq!(Name::parse(".").unwrap().parent(), None);
    }

    #[test]
    fn rejects_empty_and_bad_chars() {
        assert_eq!(Name::parse(""), Err(NameError::Empty));
        assert_eq!(Name::parse("a..b"), Err(NameError::EmptyLabel));
        assert!(matches!(
            Name::parse("a/b.bus"),
            Err(NameError::BadChar { .. })
        ));
    }

    #[test]
    fn zone_membership_is_label_safe() {
        let zone = Name::parse("bus").unwrap();
        assert!(Name::parse("bus").unwrap().in_zone(&zone));
        assert!(Name::parse("alpha.bus").unwrap().in_zone(&zone));
        // "xamp." must NOT be in zone "bus." (no false suffix match).
        assert!(!Name::parse("xamp").unwrap().in_zone(&zone));
        assert!(!Name::parse("alpha.org").unwrap().in_zone(&zone));
    }

    #[test]
    fn hickory_round_trips_canonical() {
        let n = Name::parse("_submissions._tcp.gamma.bus").unwrap();
        let h = n.to_hickory();
        assert_eq!(Name::from_hickory(&h), n);
    }
}
