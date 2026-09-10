use super::*;
use crate::bus::BusMessage;

pub const PRINCIPAL_HEADER: &str = "broker_principal";
pub const MAX_PRINCIPAL_BYTES: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub enum PrincipalVersion {
    V1,
}

impl TryFrom<u8> for PrincipalVersion {
    type Error = WireError;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        if value == 1 {
            Ok(Self::V1)
        } else {
            Err(WireError("unsupported principal version"))
        }
    }
}
impl From<PrincipalVersion> for u8 {
    fn from(_: PrincipalVersion) -> Self {
        1
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Assurance {
    LocalUnix,
    SessionBound,
}

/// Unknown fields are deliberately allowed for forward compatibility. Known
/// fields and their encodings remain strict; use `read_principal` at a boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokerPrincipal {
    pub version: PrincipalVersion,
    pub assurance: Assurance,
    pub owner_node: String,
    pub unix_uid: u32,
    pub unix_gid: u32,
    pub peer_pid: u32,
    pub broker_epoch: HexBytes<16>,
    pub connection_id: HexBytes<16>,
    #[serde(deserialize_with = "present_nullable")]
    pub session: Option<SessionIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionIdentity {
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
    pub capabilities: Vec<Capability>,
    pub lease_remaining_ms: DecimalU64,
}

impl BrokerPrincipal {
    pub fn validate(&self) -> Result<(), WireError> {
        if self.owner_node.is_empty() {
            return Err(WireError("missing owner node"));
        }
        match (self.assurance, &self.session) {
            (Assurance::LocalUnix, None) => Ok(()),
            (Assurance::SessionBound, Some(s)) => {
                let child = s.role == Role::PaneShell;
                if s.parent_instance.is_some() != child
                    || s.parent_incarnation.is_some() != child
                    || s.pane_id.is_some() != child
                    || s.pane_generation.is_some() != child
                    || s.binding_generation.0 == 0
                    || s.pane_generation.is_some_and(|g| g.0 == 0)
                {
                    return Err(WireError("invalid session scope"));
                }
                encode_capabilities(&s.capabilities)?;
                if s.capabilities
                    .windows(2)
                    .any(|w| w[0].as_str() >= w[1].as_str())
                {
                    return Err(WireError("unsorted capabilities"));
                }
                Ok(())
            }
            _ => Err(WireError("assurance/session mismatch")),
        }
    }
}

/// Remove every ASCII-case variant. Never use the removed value as authority.
pub fn strip_principal(message: &mut BusMessage) {
    message
        .headers
        .retain(|key, _| !key.eq_ignore_ascii_case(PRINCIPAL_HEADER));
}

/// Strip first even when serialisation/validation fails. None means unverified
/// delivery (including TCP/mesh), so the output carries no caller assertion.
pub fn stamp_principal(
    message: &mut BusMessage,
    principal: Option<&BrokerPrincipal>,
) -> Result<(), WireError> {
    strip_principal(message);
    if let Some(principal) = principal {
        principal.validate()?;
        let value = serde_json::to_string(principal).map_err(|_| WireError("invalid principal"))?;
        if value.len() > MAX_PRINCIPAL_BYTES {
            return Err(WireError("principal byte limit"));
        }
        message.set(PRINCIPAL_HEADER, &value);
    }
    Ok(())
}

/// Caller must separately authenticate the broker transport. A compatibility
/// parser cannot detect identical duplicate headers: parse raw input strictly
/// before this helper when receiving untrusted native-profile wire bytes.
pub fn read_principal(message: &BusMessage) -> Result<Option<BrokerPrincipal>, WireError> {
    let mut fields = message
        .headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case(PRINCIPAL_HEADER));
    let Some((key, value)) = fields.next() else {
        return Ok(None);
    };
    if key != PRINCIPAL_HEADER || fields.next().is_some() || value.len() > MAX_PRINCIPAL_BYTES {
        return Err(WireError("invalid principal header"));
    }
    validate_json(value.as_bytes())?;
    let principal: BrokerPrincipal =
        serde_json::from_str(value).map_err(|_| WireError("invalid principal fields"))?;
    principal.validate()?;
    Ok(Some(principal))
}
