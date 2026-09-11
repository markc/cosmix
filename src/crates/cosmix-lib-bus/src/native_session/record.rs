use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecordAssurance {
    Reserved,
    SessionBound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BindingState {
    Pending,
    Attached,
    Suspended,
    Revoked,
}

/// BROKER-021 discovery snapshot. Historical assurance is not live authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub name: String,
    pub record_assurance: RecordAssurance,
    pub owner_node: String,
    pub owner_uid: u32,
    pub broker_epoch: HexBytes<16>,
    pub record_id: HexBytes<16>,
    pub instance_id: HexBytes<16>,
    pub incarnation: HexBytes<16>,
    pub role: Role,
    #[serde(deserialize_with = "present_nullable")]
    pub parent_instance: Option<HexBytes<16>>,
    #[serde(deserialize_with = "present_nullable")]
    pub parent_incarnation: Option<HexBytes<16>>,
    #[serde(deserialize_with = "present_nullable")]
    pub pane_id: Option<DecimalU64>,
    #[serde(deserialize_with = "present_nullable")]
    pub pane_generation: Option<DecimalU64>,
    pub binding_generation: DecimalU64,
    pub state: BindingState,
    pub capabilities: Vec<Capability>,
    pub policy: Policy,
    #[serde(deserialize_with = "present_nullable")]
    pub lease_remaining_ms: Option<DecimalU64>,
}

impl SessionRecord {
    pub fn reference(&self) -> RecordRef {
        RecordRef {
            record_id: self.record_id,
            incarnation: self.incarnation,
            binding_generation: self.binding_generation,
        }
    }
}
