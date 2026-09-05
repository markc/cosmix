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
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
// INVARIANT: every value originates from `new` (FromStr and Deserialize
// both route through it); Serialize stays derived-transparent on that
// basis. No other constructor may set this field.
pub struct Capability(String);

// Manual impl so wire input is validated by `new` — the derived
// transparent form would admit any string.
impl<'de> Deserialize<'de> for Capability {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(de)?;
        Self::new(s).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CapabilityError {
    Empty,
}

impl fmt::Display for CapabilityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "capability is empty"),
        }
    }
}

impl std::error::Error for CapabilityError {}

impl Capability {
    pub fn new(s: impl Into<String>) -> Result<Self, CapabilityError> {
        let s = s.into();
        if s.is_empty() {
            return Err(CapabilityError::Empty);
        }
        Ok(Self(s))
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

impl std::str::FromStr for Capability {
    type Err = CapabilityError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl TryFrom<&str> for Capability {
    type Error = CapabilityError;
    fn try_from(s: &str) -> Result<Self, Self::Error> {
        Self::new(s)
    }
}

impl TryFrom<String> for Capability {
    type Error = CapabilityError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::new(s)
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
    fn rejects_empty_at_every_entry_point() {
        assert_eq!(Capability::new("").unwrap_err(), CapabilityError::Empty);
        assert_eq!(
            "".parse::<Capability>().unwrap_err(),
            CapabilityError::Empty
        );
        assert_eq!(
            Capability::try_from("").unwrap_err(),
            CapabilityError::Empty
        );
        assert_eq!(
            Capability::try_from(String::new()).unwrap_err(),
            CapabilityError::Empty
        );
        assert!(
            serde_json::from_value::<Capability>(serde_json::json!(""))
                .unwrap_err()
                .to_string()
                .contains("capability is empty")
        );
        for wire in [
            serde_json::json!([""]),
            serde_json::json!(["props.read:maild.accounts", ""]),
        ] {
            assert!(serde_json::from_value::<CapabilitySet>(wire).is_err());
        }
        assert!(
            serde_json::from_value::<CapabilitySet>(serde_json::json!([]))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn opaque_non_empty_capabilities_round_trip() {
        for s in [
            "props.audit:*",
            "webd.acme.renew:webd.vhosts",
            "custom vocabulary:部署",
            " ",
            "\n",
        ] {
            let cap = Capability::new(s).unwrap();
            assert_eq!(s.parse::<Capability>().unwrap(), cap);
            assert_eq!(Capability::try_from(s).unwrap(), cap);
            assert_eq!(Capability::try_from(s.to_owned()).unwrap(), cap);
            let wire = serde_json::json!(s);
            assert_eq!(serde_json::to_value(&cap).unwrap(), wire);
            assert_eq!(serde_json::from_value::<Capability>(wire).unwrap(), cap);
        }
    }

    #[test]
    fn capability_round_trip() {
        let c = Capability::new("props.write:maild.accounts").expect("non-empty capability");
        assert_eq!(c.as_str(), "props.write:maild.accounts");
        let j = serde_json::to_string(&c).unwrap();
        assert_eq!(j, "\"props.write:maild.accounts\"");
        let back: Capability = serde_json::from_str(&j).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn capability_set_membership() {
        let mut set = CapabilitySet::empty();
        assert!(!set.contains(
            &Capability::new("props.write:maild.accounts").expect("non-empty capability")
        ));
        assert!(
            set.insert(
                Capability::new("props.write:maild.accounts").expect("non-empty capability")
            )
        );
        assert!(
            !set.insert(
                Capability::new("props.write:maild.accounts").expect("non-empty capability")
            )
        );
        assert!(set.contains(
            &Capability::new("props.write:maild.accounts").expect("non-empty capability")
        ));
        assert!(!set.contains(
            &Capability::new("props.read:maild.accounts").expect("non-empty capability")
        ));
    }

    #[test]
    fn capability_set_serde() {
        let set: CapabilitySet = [
            Capability::new("props.write:maild.accounts").expect("non-empty capability"),
            Capability::new("props.read:maild.accounts").expect("non-empty capability"),
        ]
        .into_iter()
        .collect();
        let j = serde_json::to_value(&set).unwrap();
        let arr = j.as_array().unwrap();
        // BTreeSet → lexicographic sort: read < write.
        assert_eq!(arr[0], "props.read:maild.accounts");
        assert_eq!(arr[1], "props.write:maild.accounts");
        assert_eq!(serde_json::from_value::<CapabilitySet>(j).unwrap(), set);
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
