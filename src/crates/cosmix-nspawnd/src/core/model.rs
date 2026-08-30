use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::name::InstanceName;

pub const TOMBSTONE_SCHEMA: &str = "cosmix.nspawnd.tombstone.v1";
pub const GRANT_SCHEMA: &str = "cosmix.nspawnd.grant.v1";
pub const INSTANCE_SCHEMA: &str = "cosmix.nspawnd.instance.v1";
pub const OPERATION_SCHEMA: &str = "cosmix.nspawnd.operation.v1";
pub const CARRIED_GRANT_SCHEMA: &str = "cosmix.nspawnd.grant-envelope.v2";

/// Controller authority carried on every v2 executor mutation. The executor
/// stamps `installed_at` locally; no caller-controlled timestamp is accepted.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CarriedGrant {
    pub schema: String,
    pub name: InstanceName,
    pub owner: String,
    pub generation: u64,
    pub record_version: u64,
    pub record_state: String,
    pub record_updated: String,
}

impl CarriedGrant {
    pub fn validate_for(
        &self,
        name: &InstanceName,
        owner: &str,
        generation: u64,
    ) -> Result<(), String> {
        if self.schema != CARRIED_GRANT_SCHEMA {
            return Err(format!(
                "unsupported carried grant schema {:?}",
                self.schema
            ));
        }
        if &self.name != name || self.owner != owner || self.generation != generation {
            return Err("carried grant identity does not match the executor request".into());
        }
        if generation == 0 || self.record_version == 0 {
            return Err("grant generation and record_version must be >= 1".into());
        }
        if self.record_state != "placed" || self.record_updated.is_empty() {
            return Err(
                "grant record_state must be placed and record_updated must be non-empty".into(),
            );
        }
        Ok(())
    }

    pub fn into_grant(self, installed_at: String) -> Grant {
        Grant {
            schema: GRANT_SCHEMA.into(),
            name: self.name,
            owner: self.owner,
            generation: self.generation,
            source: GrantSource {
                kind: "controller-placement".into(),
                record_version: self.record_version,
                record_state: self.record_state,
                record_updated: self.record_updated,
            },
            installed_at,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TombstoneSource {
    pub kind: String,
    pub advisory_source: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Tombstone {
    pub schema: String,
    pub name: InstanceName,
    pub minimum_generation: u64,
    pub moved_to: String,
    pub op_id: String,
    pub recorded_at: String,
    pub enforced: bool,
    pub source: TombstoneSource,
}

impl Tombstone {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != TOMBSTONE_SCHEMA {
            return Err(format!("unsupported tombstone schema {:?}", self.schema));
        }
        if self.minimum_generation == 0 {
            return Err("minimum_generation must be >= 1".into());
        }
        if self.moved_to.is_empty() || self.op_id.is_empty() || self.recorded_at.is_empty() {
            return Err("moved_to, op_id, and recorded_at must be non-empty".into());
        }
        if !self.enforced {
            return Err("canonical tombstone must be enforced".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GrantSource {
    pub kind: String,
    pub record_version: u64,
    pub record_state: String,
    pub record_updated: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub schema: String,
    pub name: InstanceName,
    pub owner: String,
    pub generation: u64,
    pub source: GrantSource,
    pub installed_at: String,
}

impl Grant {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != GRANT_SCHEMA {
            return Err(format!("unsupported grant schema {:?}", self.schema));
        }
        if self.owner.is_empty() || self.generation == 0 || self.installed_at.is_empty() {
            return Err("owner and installed_at must be non-empty; generation must be >= 1".into());
        }
        if self.source.kind.is_empty()
            || self.source.record_version == 0
            || self.source.record_state.is_empty()
            || self.source.record_updated.is_empty()
        {
            return Err(
                "grant source fields must be non-empty; record_version must be >= 1".into(),
            );
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DesiredState {
    Running,
    Stopped,
}

impl DesiredState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InstanceRecord {
    pub schema: String,
    pub name: InstanceName,
    pub desired: DesiredState,
    pub updated_at: String,
    pub last_operation: Option<String>,
}

impl InstanceRecord {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != INSTANCE_SCHEMA {
            return Err(format!("unsupported instance schema {:?}", self.schema));
        }
        if self.updated_at.is_empty() {
            return Err("updated_at must be non-empty".into());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationVerb {
    Start,
    Stop,
    ReconcileStart,
    ReconcileStop,
}

impl OperationVerb {
    pub fn bus_name(self) -> &'static str {
        match self {
            Self::Start | Self::ReconcileStart => "nspawnd.start",
            Self::Stop | Self::ReconcileStop => "nspawnd.stop",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationState {
    Running,
    Succeeded,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedInstance {
    pub name: InstanceName,
    pub running: bool,
    pub image_present: bool,
    pub machine_class: Option<String>,
    pub machine_service: Option<String>,
    pub machine_unit: Option<String>,
    pub unit_load: String,
    pub unit_active: String,
    pub unit_sub: String,
    pub unit_file_state: String,
}

impl ObservedInstance {
    pub fn state_label(&self) -> &'static str {
        if self.running {
            "running"
        } else if self.image_present {
            "stopped"
        } else {
            "absent"
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OperationRecord {
    pub schema: String,
    pub op_id: String,
    pub actor: String,
    pub request_id: String,
    pub request_hash: String,
    pub verb: OperationVerb,
    pub name: InstanceName,
    pub generation: u64,
    pub state: OperationState,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub observed_before: ObservedInstance,
    pub observed_after: Option<ObservedInstance>,
    pub response_rc: Option<u8>,
    pub response_body: Option<Value>,
}

impl OperationRecord {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != OPERATION_SCHEMA {
            return Err(format!("unsupported operation schema {:?}", self.schema));
        }
        if self.op_id.is_empty()
            || self.actor.is_empty()
            || self.request_id.is_empty()
            || self.request_hash.is_empty()
            || self.started_at.is_empty()
        {
            return Err("operation identity fields must be non-empty".into());
        }
        let parsed = ulid::Ulid::from_string(&self.op_id)
            .map_err(|error| format!("op_id must be a ULID: {error}"))?;
        if parsed.to_string() != self.op_id {
            return Err("op_id must be a canonical ULID".into());
        }
        if self.generation == 0 {
            return Err("operation generation must be >= 1".into());
        }
        match (self.state, self.completed_at.as_deref()) {
            (OperationState::Running, Some(_)) => {
                return Err("running operation must not have completed_at".into());
            }
            (OperationState::Succeeded | OperationState::Failed, None) => {
                return Err("completed operation must have completed_at".into());
            }
            (_, Some(completed_at)) => {
                let parsed = DateTime::parse_from_rfc3339(completed_at)
                    .map_err(|error| format!("completed_at must be RFC3339: {error}"))?;
                let canonical = parsed
                    .with_timezone(&Utc)
                    .to_rfc3339_opts(SecondsFormat::Secs, true);
                if canonical != completed_at {
                    return Err(
                        "completed_at must be canonical UTC whole-second RFC3339 ending in Z"
                            .into(),
                    );
                }
            }
            (OperationState::Running, None) => {}
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct C0Tombstone {
    pub name: InstanceName,
    pub op: String,
    pub moved_to: String,
    pub generation_next: u64,
    pub at: String,
    pub advisory: bool,
}

impl TryFrom<C0Tombstone> for Tombstone {
    type Error = String;

    fn try_from(value: C0Tombstone) -> Result<Self, Self::Error> {
        if value.generation_next == 0 {
            return Err("C0 generation_next must be >= 1".into());
        }
        if !value.advisory {
            return Err("C0 source tombstone must be stamped advisory:true".into());
        }
        if value.op.is_empty() || value.moved_to.is_empty() || value.at.is_empty() {
            return Err("C0 op, moved_to, and at must be non-empty".into());
        }
        Ok(Self {
            schema: TOMBSTONE_SCHEMA.into(),
            name: value.name,
            minimum_generation: value.generation_next,
            moved_to: value.moved_to,
            op_id: value.op,
            recorded_at: value.at,
            enforced: true,
            source: TombstoneSource {
                kind: "c0-import".into(),
                advisory_source: true,
            },
        })
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct C0PlacementRecord {
    pub name: InstanceName,
    pub owner: String,
    pub generation: u64,
    pub desired: String,
    pub state: String,
    pub op: Option<String>,
    pub version: u64,
    pub updated: String,
    #[serde(default)]
    pub op_dst: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c0_tombstone_conversion_is_strict() {
        let c0: C0Tombstone = serde_json::from_value(serde_json::json!({
            "name":"labspoke", "op":"c0-op", "moved_to":"beta",
            "generation_next":2, "at":"2026-08-08T00:00:00Z", "advisory":true
        }))
        .unwrap();
        let canonical = Tombstone::try_from(c0).unwrap();
        assert_eq!(canonical.minimum_generation, 2);
        assert!(canonical.enforced);

        assert!(
            serde_json::from_value::<C0Tombstone>(serde_json::json!({
                "name":"labspoke", "op":"x", "moved_to":"beta",
                "generation_next":2, "at":"x", "advisory":true, "extra":1
            }))
            .is_err()
        );
    }

    #[test]
    fn carried_grant_is_strict_and_bound_to_the_request() {
        let name = InstanceName::parse("demo").unwrap();
        let grant: CarriedGrant = serde_json::from_value(serde_json::json!({
            "schema":CARRIED_GRANT_SCHEMA,"name":"demo","owner":"alpha","generation":4,
            "record_version":9,"record_state":"placed","record_updated":"2026-08-09T00:00:00Z"
        }))
        .unwrap();
        assert!(grant.validate_for(&name, "alpha", 4).is_ok());
        assert!(grant.validate_for(&name, "beta", 4).is_err());
        assert!(
            serde_json::from_value::<CarriedGrant>(serde_json::json!({
                "schema":CARRIED_GRANT_SCHEMA,"name":"demo","owner":"alpha","generation":4,
                "record_version":9,"record_state":"placed","record_updated":"x","extra":true
            }))
            .is_err()
        );
    }
}
