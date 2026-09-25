//! Origins (who made an edit) and the attested transport provenance (`Via`).
//!
//! An origin is a LABEL, not an authorization. Its *kind* is decided by editd
//! from broker-attested headers (plan §4.3, D12): `human:` only for a local
//! registered caller, `tool:` only for editd itself, everything else `agent:`.
//! The core stores origins; it never derives them.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OriginKind {
    Human,
    Agent,
    Tool,
}

/// `kind:label`; label matches `^[A-Za-z0-9._@/+-]{1,64}$`; split on the first `:`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Origin {
    pub kind: OriginKind,
    pub label: String,
}

/// Reserved: external reload.
pub const TOOL_DISK: &str = "tool:disk";
/// Reserved: editd-internal.
pub const TOOL_EDITD: &str = "tool:editd";

impl Origin {
    pub fn new(kind: OriginKind, label: impl Into<String>) -> Self {
        Self { kind, label: label.into() }
    }

    /// Whether `label` matches the label grammar.
    pub fn is_valid_label(label: &str) -> bool {
        !label.is_empty()
            && label.len() <= crate::limits::LABEL_MAX
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'@' | b'/' | b'+' | b'-'))
    }

    /// A derived label longer than 64 chars becomes its first 55 chars + `+` +
    /// 8 lowercase hex chars of a hash of the full label (editd supplies the hash).
    pub fn truncate_label(label: &str, hash8: &str) -> String {
        if label.len() <= crate::limits::LABEL_MAX {
            return label.to_string();
        }
        let mut cut = 55;
        while !label.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}+{}", &label[..cut], hash8)
    }
}

impl std::fmt::Display for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self.kind {
            OriginKind::Human => "human",
            OriginKind::Agent => "agent",
            OriginKind::Tool => "tool",
        };
        write!(f, "{kind}:{}", self.label)
    }
}

impl std::str::FromStr for Origin {
    type Err = crate::error::CoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = || {
            crate::error::CoreError::new(
                crate::error::ErrorCode::InvalidArgument,
                crate::error::reason::BAD_ORIGIN,
                format!("origin {s:?} is not kind:label"),
            )
        };
        let (kind, label) = s.split_once(':').ok_or_else(bad)?;
        let kind = match kind {
            "human" => OriginKind::Human,
            "agent" => OriginKind::Agent,
            "tool" => OriginKind::Tool,
            _ => return Err(bad()),
        };
        if !Self::is_valid_label(label) {
            return Err(bad());
        }
        Ok(Self::new(kind, label))
    }
}

impl Serialize for Origin {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Origin {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Broker-attested headers of the request that produced a log entry
/// (`broker_origin` is `local` or `mesh`; the others are absent for anonymous
/// or local callers as noded stamps them).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Via {
    pub from: Option<String>,
    pub broker_origin: String,
    pub broker_peer: Option<String>,
    pub broker_service: Option<String>,
}
