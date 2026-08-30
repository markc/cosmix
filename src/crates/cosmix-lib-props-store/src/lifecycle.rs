//! Per-record lifecycle for Saga namespaces (SPEC 12 §6.5).
//!
//! Saga namespaces carry a substrate-reserved `_lifecycle` field on every
//! record. Daemons MUST NOT write this field directly; the substrate
//! library transitions it from `Provisioning` to `Active` or `Failed` as
//! `after_set` returns. Simple namespaces have no lifecycle field at all
//! and behave as plain CRUD.
//!
//! **In-memory vs wire shape.** The `Serialize`/`Deserialize` derived
//! below uses an internally-tagged form (`{"state": "active"}`) suited
//! for the storage backends (toml/mixdata/sqlite blob). The §5.5
//! `<svc>.props.records.changed` wire body uses a *flat* projection:
//! the body carries a top-level string `lifecycle: "active"|"failed"`
//! and, on failure, a sibling top-level `reason: "..."`. The C5
//! dispatcher constructs that wire body using [`Lifecycle::as_state_str`]
//! and [`Lifecycle::reason`] rather than re-serializing the enum.

use serde::{Deserialize, Serialize};

/// SPEC 12 §6.5 — three-state lifecycle observable to watch subscribers.
///
/// Storage form is `{ "state": "...", "reason"?: "..." }` (internally
/// tagged). The dispatcher flattens to the §5.5 wire shape on emit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Lifecycle {
    /// `after_set` is in flight (or, on startup recovery, has not yet
    /// been re-driven against this record).
    Provisioning,
    /// `after_set` returned `Ok`. Externally usable.
    Active,
    /// `after_set` returned `Err`. The record is durable for operator
    /// inspection but is not externally usable; the operator is expected
    /// to delete and recreate.
    Failed { reason: String },
}

impl Lifecycle {
    /// Convenience: is this the externally-usable state.
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Active)
    }

    /// Convenience: is this the in-flight state.
    pub fn is_provisioning(&self) -> bool {
        matches!(self, Self::Provisioning)
    }

    /// Convenience: is this a terminal failure.
    pub fn is_failed(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }

    /// Storage-state helper: the flat string
    /// `provisioning`/`active`/`failed` used inside the in-record
    /// `_lifecycle` reserved field (§6.5) and in audit projections.
    ///
    /// **Not** the §5.5 records.changed body `lifecycle` field —
    /// that field is emitted only on `completed` events and only
    /// carries `active`|`failed` (§5.5 line 601). Dispatchers
    /// projecting to records.changed MUST filter accordingly.
    pub fn as_state_str(&self) -> &'static str {
        match self {
            Self::Provisioning => "provisioning",
            Self::Active => "active",
            Self::Failed { .. } => "failed",
        }
    }

    /// Wire-projection helper for the §5.5 records.changed body: the
    /// sibling `reason` string, present only on `Failed`.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Failed { reason } => Some(reason.as_str()),
            _ => None,
        }
    }
}

/// The substrate-reserved field name on every Saga-namespace record.
/// Daemons that try to set this field directly through `<svc>.props.set`
/// receive a `validation_error` per §6.5.
pub const LIFECYCLE_FIELD: &str = "_lifecycle";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_predicates() {
        assert!(Lifecycle::Provisioning.is_provisioning());
        assert!(Lifecycle::Active.is_active());
        assert!(Lifecycle::Failed { reason: "x".into() }.is_failed());
        assert!(!Lifecycle::Provisioning.is_active());
    }

    #[test]
    fn serde_round_trip() {
        let cases = [
            Lifecycle::Provisioning,
            Lifecycle::Active,
            Lifecycle::Failed {
                reason: "mds seed failed".into(),
            },
        ];
        for c in cases {
            let j = serde_json::to_string(&c).unwrap();
            let back: Lifecycle = serde_json::from_str(&j).unwrap();
            assert_eq!(back, c);
        }
    }

    #[test]
    fn provisioning_serializes_state_only() {
        let j = serde_json::to_value(Lifecycle::Provisioning).unwrap();
        assert_eq!(j["state"], "provisioning");
        // No reason field on the in-flight or success states.
        assert!(j.get("reason").is_none());
    }

    #[test]
    fn failed_carries_reason() {
        let j = serde_json::to_value(Lifecycle::Failed {
            reason: "mds seed failed: lock timeout".into(),
        })
        .unwrap();
        assert_eq!(j["state"], "failed");
        assert_eq!(j["reason"], "mds seed failed: lock timeout");
    }

    #[test]
    fn wire_projection_helpers() {
        assert_eq!(Lifecycle::Provisioning.as_state_str(), "provisioning");
        assert_eq!(Lifecycle::Active.as_state_str(), "active");
        assert_eq!(
            Lifecycle::Failed {
                reason: "boom".into()
            }
            .as_state_str(),
            "failed"
        );

        assert!(Lifecycle::Provisioning.reason().is_none());
        assert!(Lifecycle::Active.reason().is_none());
        assert_eq!(
            Lifecycle::Failed {
                reason: "boom".into()
            }
            .reason(),
            Some("boom")
        );
    }
}
