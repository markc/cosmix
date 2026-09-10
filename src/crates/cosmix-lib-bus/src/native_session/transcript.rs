use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Purpose {
    Enrol,
    Resume,
}

/// Signed fields only. Unsigned `wake_error` belongs to the result envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProofTranscript {
    pub purpose: Purpose,
    pub broker_epoch: HexBytes<16>,
    pub connection_id: HexBytes<16>,
    pub challenge_id: HexBytes<16>,
    pub nonce: HexBytes<32>,
    #[serde(deserialize_with = "present_nullable")]
    pub grant_id: Option<HexBytes<16>>,
    pub record_id: HexBytes<16>,
    pub instance_id: HexBytes<16>,
    pub incarnation: HexBytes<16>,
    pub unix_uid: u32,
    #[serde(deserialize_with = "present_nullable")]
    pub parent_instance: Option<HexBytes<16>>,
    #[serde(deserialize_with = "present_nullable")]
    pub parent_incarnation: Option<HexBytes<16>>,
    #[serde(deserialize_with = "present_nullable")]
    pub parent_key_hash: Option<HexBytes<32>>,
    #[serde(deserialize_with = "present_nullable")]
    pub pane_id: Option<DecimalU64>,
    #[serde(deserialize_with = "present_nullable")]
    pub pane_generation: Option<DecimalU64>,
    pub role: Role,
    pub public_key_hash: HexBytes<32>,
    pub capabilities_hash: HexBytes<32>,
    pub binding_generation: DecimalU64,
    #[serde(deserialize_with = "present_nullable")]
    pub grant_expires_ms: Option<DecimalU64>,
    pub challenge_expires_ms: DecimalU64,
}

fn lp(out: &mut Vec<u8>, text: &str) {
    // All callers use closed enums whose tokens are shorter than u16::MAX.
    out.extend_from_slice(&(text.len() as u16).to_be_bytes());
    out.extend_from_slice(text.as_bytes());
}
fn opt<const N: usize>(out: &mut Vec<u8>, value: Option<[u8; N]>) {
    out.push(u8::from(value.is_some()));
    if let Some(bytes) = value {
        out.extend_from_slice(&bytes);
    }
}

/// Exact BUS-016 bytes. Hashes are already SHA-256 digests from challenge state;
/// this encoder does not perform signature verification or expiry decisions.
pub fn encode_proof(p: &ProofTranscript) -> Result<Vec<u8>, WireError> {
    let enrol = p.purpose == Purpose::Enrol;
    let child = p.role == Role::PaneShell;
    if p.grant_id.is_some() != enrol
        || p.grant_expires_ms.is_some() != enrol
        || p.binding_generation.0 == 0
        || (enrol && p.binding_generation.0 != 1)
        || (!enrol && p.binding_generation.0 < 2)
        || p.parent_instance.is_some() != child
        || p.parent_incarnation.is_some() != child
        || p.parent_key_hash.is_some() != child
        || p.pane_id.is_some() != child
        || p.pane_generation.is_some() != child
        || p.pane_generation.is_some_and(|g| g.0 == 0)
    {
        return Err(WireError("invalid proof scope"));
    }
    let mut out = b"cosmix.native-session.proof\0\0\x01".to_vec();
    out.push(if enrol { 1 } else { 2 });
    out.extend_from_slice(&p.broker_epoch.0);
    out.extend_from_slice(&p.connection_id.0);
    out.extend_from_slice(&p.challenge_id.0);
    out.extend_from_slice(&p.nonce.0);
    opt(&mut out, p.grant_id.map(|v| v.0));
    out.extend_from_slice(&p.record_id.0);
    out.extend_from_slice(&p.instance_id.0);
    out.extend_from_slice(&p.incarnation.0);
    out.extend_from_slice(&p.unix_uid.to_be_bytes());
    opt(&mut out, p.parent_instance.map(|v| v.0));
    opt(&mut out, p.parent_incarnation.map(|v| v.0));
    opt(&mut out, p.parent_key_hash.map(|v| v.0));
    opt(&mut out, p.pane_id.map(|v| v.0.to_be_bytes()));
    opt(&mut out, p.pane_generation.map(|v| v.0.to_be_bytes()));
    lp(&mut out, p.role.as_str());
    out.extend_from_slice(&p.public_key_hash.0);
    out.extend_from_slice(&p.capabilities_hash.0);
    out.extend_from_slice(&p.binding_generation.0.to_be_bytes());
    opt(&mut out, p.grant_expires_ms.map(|v| v.0.to_be_bytes()));
    out.extend_from_slice(&p.challenge_expires_ms.0.to_be_bytes());
    Ok(out)
}

pub fn encode_allocate(
    epoch: HexBytes<16>,
    connection: HexBytes<16>,
    public_key: HexBytes<32>,
    policy: Policy,
) -> Vec<u8> {
    let mut out = b"cosmix.native-session.allocate\0\0\x01".to_vec();
    out.extend_from_slice(&epoch.0);
    out.extend_from_slice(&connection.0);
    out.extend_from_slice(&public_key.0);
    lp(&mut out, policy.as_str());
    out
}

/// Canonical set encoding, independent of input order. Duplicate capabilities
/// are errors, not silently deduplicated grants.
pub fn encode_capabilities(capabilities: &[Capability]) -> Result<Vec<u8>, WireError> {
    if capabilities.is_empty() || capabilities.len() > 6 {
        return Err(WireError("capability count"));
    }
    let mut names: Vec<_> = capabilities.iter().map(|c| c.as_str()).collect();
    names.sort_unstable();
    if names.windows(2).any(|w| w[0] == w[1]) {
        return Err(WireError("duplicate capability"));
    }
    let mut out = (names.len() as u16).to_be_bytes().to_vec();
    for name in names {
        lp(&mut out, name);
    }
    Ok(out)
}
