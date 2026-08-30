//! Capability tags per SPEC 12 §7.1.
//!
//! A capability is a string of the form
//! `props.<action>:<svc>.<namespace>[:<scope>]` issued to a peer by the
//! deployment's `AuthPolicy` (§7.2). The action slot is closed in v0.1
//! (`read`, `write`, `describe`, `audit`); the `<svc>.<namespace>` slot
//! carries the fully qualified namespace and admits the literal `*` for
//! mesh-wide power capabilities; the optional scope slot is a free
//! string (e.g. `props.read:maild.accounts:secrets`,
//! `props.write:maild.accounts:self`).
//!
//! The substrate library only compares capability strings; it does not
//! parse the vocabulary. The policy that decides who holds which
//! capability is the `AuthPolicy` function pointer carried on
//! `NamespaceSpec` (§7.2), populated at namespace registration.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;

/// A single capability tag. The conventional shape is
/// `props.<action>:<svc>.<namespace>[:<scope>]` (§7.1) but the
/// substrate enforces no syntactic constraint here beyond non-empty —
/// validation that an issued token matches a meaningful action/scope
/// is the deployment's `AuthPolicy` responsibility.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Capability(String);

impl Capability {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for Capability {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl From<String> for Capability {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// Set of capabilities currently presented by a caller.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CapabilitySet(pub BTreeSet<Capability>);

impl CapabilitySet {
    pub fn empty() -> Self {
        Self(BTreeSet::new())
    }

    pub fn contains(&self, cap: &Capability) -> bool {
        self.0.contains(cap)
    }

    pub fn insert(&mut self, cap: Capability) -> bool {
        self.0.insert(cap)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Capability> {
        self.0.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl FromIterator<Capability> for CapabilitySet {
    fn from_iter<I: IntoIterator<Item = Capability>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

/// SPEC 12 §7 verb-level decision result. The substrate library returns
/// this from the registration-supplied policy at every verb entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthDecision {
    Allow,
    Deny,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_round_trip() {
        let c = Capability::new("props.write:maild.accounts");
        assert_eq!(c.as_str(), "props.write:maild.accounts");
        let j = serde_json::to_string(&c).unwrap();
        assert_eq!(j, "\"props.write:maild.accounts\"");
        let back: Capability = serde_json::from_str(&j).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn capability_set_membership() {
        let mut set = CapabilitySet::empty();
        assert!(!set.contains(&Capability::from("props.write:maild.accounts")));
        assert!(set.insert(Capability::from("props.write:maild.accounts")));
        assert!(!set.insert(Capability::from("props.write:maild.accounts")));
        assert!(set.contains(&Capability::from("props.write:maild.accounts")));
        assert!(!set.contains(&Capability::from("props.read:maild.accounts")));
    }

    #[test]
    fn capability_set_serde() {
        let set: CapabilitySet = [
            Capability::from("props.write:maild.accounts"),
            Capability::from("props.read:maild.accounts"),
        ]
        .into_iter()
        .collect();
        let j = serde_json::to_value(&set).unwrap();
        let arr = j.as_array().unwrap();
        // BTreeSet → lexicographic sort: read < write.
        assert_eq!(arr[0], "props.read:maild.accounts");
        assert_eq!(arr[1], "props.write:maild.accounts");
    }

    #[test]
    fn auth_decision_serializes_snake() {
        assert_eq!(
            serde_json::to_string(&AuthDecision::Allow).unwrap(),
            "\"allow\""
        );
        assert_eq!(
            serde_json::to_string(&AuthDecision::Deny).unwrap(),
            "\"deny\""
        );
    }
}
