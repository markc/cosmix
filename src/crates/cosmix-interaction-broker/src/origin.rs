//! Non-spoofable provenance labelling (notify.v1 §4).
//!
//! Origin is **broker-stamped from the caller's registered Bus identity** and is
//! never a request field — a local citizen cannot present as `system`/`cosmix`/
//! `root`. This is the buildable slice of the B-2 mitigation: labelling, not full
//! server-side principal derivation.

use std::collections::HashMap;

/// The label shown for a caller with no durable registered identity.
pub const ANONYMOUS: &str = "anonymous";

/// Reserved origin labels a caller may never claim implicitly. In v1 the
/// allowlist that would grant one is **empty** (notify.v1 §10.2), so these are
/// unassignable — a daemon earns one later, explicitly and by name.
pub const RESERVED_LABELS: &[&str] = &["system", "cosmix", "root"];

/// Maps a caller's registered Bus identity (the Bus `from`) to the label a
/// notification is stamped with. Also guards the reserved-label allowlist.
#[derive(Debug, Clone, Default)]
pub struct OriginPolicy {
    /// identity -> reserved label it is permitted to present as. **Empty in v1.**
    allowlist: HashMap<String, String>,
}

impl OriginPolicy {
    /// The v1 policy: empty reserved-label allowlist (notify.v1 §10.2).
    pub fn v1() -> Self {
        OriginPolicy {
            allowlist: HashMap::new(),
        }
    }

    /// Grant `identity` the right to present as reserved label `label`. Post-v1
    /// path — the v1 policy never calls this, so the allowlist stays empty.
    /// Ignored (returns `false`) unless `label` is actually a reserved label.
    pub fn grant_reserved(
        &mut self,
        identity: impl Into<String>,
        label: impl Into<String>,
    ) -> bool {
        let label = label.into();
        if !is_reserved(&label) {
            return false;
        }
        self.allowlist.insert(identity.into(), label);
        true
    }

    /// Resolve the stamped origin for a caller. `from` is the caller's registered
    /// Bus identity; `None` (or empty/whitespace) yields [`ANONYMOUS`]. If the
    /// registered identity happens to collide with a reserved label, it is
    /// downgraded to [`ANONYMOUS`] unless explicitly allowlisted for it — a
    /// caller's raw identity can never grant a reserved label by coincidence.
    pub fn resolve(&self, from: Option<&str>) -> String {
        let from = match from {
            Some(s) if !s.trim().is_empty() => s.trim(),
            _ => return ANONYMOUS.to_string(),
        };
        if is_reserved(from) && !self.may_present_as(from, from) {
            return ANONYMOUS.to_string();
        }
        from.to_string()
    }

    /// Whether `identity` is permitted to present as the reserved `label`.
    /// Non-reserved labels need no permission (`true`); reserved labels require an
    /// allowlist grant — always absent in v1.
    pub fn may_present_as(&self, identity: &str, label: &str) -> bool {
        if !is_reserved(label) {
            return true;
        }
        self.allowlist.get(identity).map(String::as_str) == Some(label)
    }
}

/// Whether `label` is one of the reserved provenance labels (case-insensitive).
pub fn is_reserved(label: &str) -> bool {
    let label = label.trim();
    RESERVED_LABELS
        .iter()
        .any(|r| r.eq_ignore_ascii_case(label))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_identity_is_the_label() {
        let p = OriginPolicy::v1();
        assert_eq!(p.resolve(Some("musicd")), "musicd");
    }

    #[test]
    fn no_identity_is_anonymous() {
        let p = OriginPolicy::v1();
        assert_eq!(p.resolve(None), ANONYMOUS);
        assert_eq!(p.resolve(Some("   ")), ANONYMOUS);
    }

    #[test]
    fn reserved_label_unassignable_in_v1() {
        let p = OriginPolicy::v1();
        // A caller whose very identity is "system" still cannot present as it.
        assert_eq!(p.resolve(Some("system")), ANONYMOUS);
        assert_eq!(p.resolve(Some("Root")), ANONYMOUS);
        assert!(!p.may_present_as("system", "system"));
    }

    #[test]
    fn reserved_label_grantable_post_v1() {
        let mut p = OriginPolicy::v1();
        assert!(p.grant_reserved("cosmix-noded", "system"));
        assert!(p.may_present_as("cosmix-noded", "system"));
        // but only the granted identity, and only the granted label
        assert!(!p.may_present_as("maild", "system"));
        assert!(!p.may_present_as("cosmix-noded", "root"));
    }

    #[test]
    fn grant_rejects_non_reserved_label() {
        let mut p = OriginPolicy::v1();
        assert!(!p.grant_reserved("maild", "maild"));
    }
}
