//! Mesh-aware naming: derive a kernel-legal WireGuard interface name from
//! a mesh FQDN. See `_doc/planned/cosmix-wgd.md` §12 *Layer 1* — the
//! mesh's full FQDN (`mesh_fqdn = "example.com"`) is the substrate's
//! identity anchor; the kernel interface name is the first DNS label of
//! that FQDN, validated against kernel constraints.
//!
//! Validation rules (matching §12 *Layer 1*):
//! - Non-empty after taking the first label.
//! - Max 15 characters (`IFNAMSIZ = 16`, NUL-terminated → 15 usable).
//! - DNS label charset (`[a-z0-9-]`), lowercase only.
//! - No leading hyphen (also a DNS label rule).
//!
//! Pure: no I/O, no FQDN-resolution side trip. The caller passes the
//! FQDN as a string; we strip everything past the first dot and
//! validate.

/// Maximum length of a Linux network interface name. `IFNAMSIZ` is 16
/// in `linux/if.h`, but the final byte is reserved for NUL termination.
pub const IFNAMSIZ_MINUS_NUL: usize = 15;

/// Errors from [`iface_name_for_mesh`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IfaceNameError {
    /// The FQDN was empty, or the first label (before the first `.`)
    /// was empty (e.g. `".example.org"`).
    #[error("mesh fqdn is empty or has an empty first label")]
    Empty,
    /// The first label exceeds `IFNAMSIZ - 1` characters. Per §12,
    /// operators with a long first label must either pick a shorter
    /// mesh name or accept that we refuse to derive an interface name
    /// rather than silently truncate.
    #[error("interface name `{label}` is too long: {got} chars, max {max}")]
    TooLong {
        /// The offending label, lowercased.
        label: String,
        got: usize,
        max: usize,
    },
    /// A character outside `[a-z0-9-]` survived lowercasing. Most
    /// commonly an underscore, a non-ASCII letter, or a Punycode-
    /// expanded IDN label.
    #[error("interface name `{label}` contains invalid character `{ch}`")]
    InvalidChar {
        /// The label as it was after lowercasing — useful for the
        /// operator who needs to see why a Unicode FQDN was rejected.
        label: String,
        /// The first offending character.
        ch: char,
    },
    /// First character is a hyphen. DNS labels and kernel ifnames both
    /// forbid this, so we reject pre-lowercase.
    #[error("interface name `{label}` must not start with a hyphen")]
    LeadingHyphen { label: String },
    /// Last character is a hyphen. RFC 1035 forbids trailing hyphens on
    /// DNS labels; the kernel doesn't care, but if the mesh FQDN's
    /// first label can't legally be a DNS label then the §12 identity-
    /// alignment story breaks (the same string would not be servable
    /// as a hostname). We reject for consistency with §12 rather than
    /// for any kernel-side reason.
    #[error("interface name `{label}` must not end with a hyphen")]
    TrailingHyphen { label: String },
}

/// Derive a WireGuard kernel interface name from a mesh FQDN per
/// `_doc/planned/cosmix-wgd.md` §12. The first DNS label is lowercased
/// and validated; the result is the string the reconciler hands to
/// netlink's `IFLA_IFNAME` attribute (C7).
///
/// Examples:
/// - `iface_name_for_mesh("example.com")` → `Ok("example")`
/// - `iface_name_for_mesh("EXAMPLE.org")` → `Ok("example")` (lowercased)
/// - `iface_name_for_mesh("very-long-mesh-name.example")` →
///   `Err(TooLong { ... })`
/// - `iface_name_for_mesh("under_score.example")` →
///   `Err(InvalidChar { ch: '_' })`
pub fn iface_name_for_mesh(fqdn: &str) -> Result<String, IfaceNameError> {
    let first_label = fqdn.split('.').next().unwrap_or("");
    if first_label.is_empty() {
        return Err(IfaceNameError::Empty);
    }

    let label = first_label.to_ascii_lowercase();

    if label.len() > IFNAMSIZ_MINUS_NUL {
        return Err(IfaceNameError::TooLong {
            got: label.len(),
            max: IFNAMSIZ_MINUS_NUL,
            label,
        });
    }

    // Leading/trailing-hyphen checks before per-char loop so the error
    // variant is specific (operators reading logs prefer "must not start
    // with a hyphen" over "contains invalid character `-`").
    if label.starts_with('-') {
        return Err(IfaceNameError::LeadingHyphen { label });
    }
    if label.ends_with('-') {
        return Err(IfaceNameError::TrailingHyphen { label });
    }

    for ch in label.chars() {
        let ok = matches!(ch, 'a'..='z' | '0'..='9' | '-');
        if !ok {
            return Err(IfaceNameError::InvalidChar { label, ch });
        }
    }

    Ok(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_short_fqdn() {
        assert_eq!(iface_name_for_mesh("example.com").unwrap(), "example");
    }

    #[test]
    fn happy_path_single_label_no_dot() {
        // A bare label (no dot) is still valid input — operator may pass
        // just the first label when the mesh FQDN's TLD is irrelevant
        // (configuration-time mistake we still want to handle).
        assert_eq!(iface_name_for_mesh("example").unwrap(), "example");
    }

    #[test]
    fn lowercases_uppercase_input() {
        assert_eq!(iface_name_for_mesh("Example.Org").unwrap(), "example");
        assert_eq!(iface_name_for_mesh("EXAMPLE.ORG").unwrap(), "example");
    }

    #[test]
    fn allows_digits_and_hyphens() {
        assert_eq!(iface_name_for_mesh("k4n4ry.org").unwrap(), "k4n4ry");
        assert_eq!(iface_name_for_mesh("ka-na-ry.org").unwrap(), "ka-na-ry");
        assert_eq!(
            iface_name_for_mesh("123abc-456.example").unwrap(),
            "123abc-456"
        );
    }

    #[test]
    fn rejects_empty_input() {
        assert_eq!(iface_name_for_mesh(""), Err(IfaceNameError::Empty));
    }

    #[test]
    fn rejects_empty_first_label() {
        assert_eq!(
            iface_name_for_mesh(".example.org"),
            Err(IfaceNameError::Empty)
        );
    }

    #[test]
    fn rejects_label_at_15_plus_one() {
        // Exactly 15 chars is the boundary; 16 is the first too-long.
        let len_15 = "a".repeat(15);
        assert_eq!(iface_name_for_mesh(&len_15).unwrap(), len_15);

        let len_16 = "a".repeat(16);
        match iface_name_for_mesh(&format!("{len_16}.example")) {
            Err(IfaceNameError::TooLong { got, max, label }) => {
                assert_eq!(got, 16);
                assert_eq!(max, IFNAMSIZ_MINUS_NUL);
                assert_eq!(label, len_16);
            }
            other => panic!("expected TooLong, got {other:?}"),
        }
    }

    #[test]
    fn rejects_underscore() {
        match iface_name_for_mesh("kan_ary.org") {
            Err(IfaceNameError::InvalidChar { label, ch }) => {
                assert_eq!(label, "kan_ary");
                assert_eq!(ch, '_');
            }
            other => panic!("expected InvalidChar, got {other:?}"),
        }
    }

    #[test]
    fn rejects_non_ascii() {
        // Unicode mesh names round-trip via IDNA in DNS but the kernel
        // ifname is bytes; we refuse to silently encode.
        match iface_name_for_mesh("example♥.org") {
            Err(IfaceNameError::InvalidChar { ch, .. }) => {
                assert_eq!(ch, '♥');
            }
            other => panic!("expected InvalidChar, got {other:?}"),
        }
    }

    #[test]
    fn rejects_leading_hyphen() {
        match iface_name_for_mesh("-example.com") {
            Err(IfaceNameError::LeadingHyphen { label }) => {
                assert_eq!(label, "-example");
            }
            other => panic!("expected LeadingHyphen, got {other:?}"),
        }
    }

    #[test]
    fn rejects_trailing_hyphen() {
        match iface_name_for_mesh("example-.example") {
            Err(IfaceNameError::TrailingHyphen { label }) => {
                assert_eq!(label, "example-");
            }
            other => panic!("expected TrailingHyphen, got {other:?}"),
        }
    }

    #[test]
    fn lone_hyphen_label_is_leading_hyphen_not_trailing() {
        // "-" is both a leading and trailing hyphen; the leading check
        // runs first so callers get the more-specific error.
        match iface_name_for_mesh("-") {
            Err(IfaceNameError::LeadingHyphen { label }) => assert_eq!(label, "-"),
            other => panic!("expected LeadingHyphen, got {other:?}"),
        }
    }

    #[test]
    fn rejects_dot_only_input() {
        // ".".split('.').next() == "" → Empty, not LeadingHyphen.
        assert_eq!(iface_name_for_mesh("."), Err(IfaceNameError::Empty));
    }

    #[test]
    fn boundary_15_chars_with_hyphens_and_digits() {
        // Realistic shape: "wg-mesh-a12345" is exactly 14 chars; nudge
        // to 15 with a trailing digit.
        let label = "wg-mesh-a12345x";
        assert_eq!(label.len(), 15);
        assert_eq!(iface_name_for_mesh(label).unwrap(), label);
    }
}
