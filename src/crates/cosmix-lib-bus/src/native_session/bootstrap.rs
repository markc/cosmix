use super::*;
use crate::bus::BusMessage;
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use std::collections::HashSet;
use std::fmt;

pub const MAX_BOOTSTRAP_BYTES: usize = 16 * 1024;
pub const MAX_BOOTSTRAP_HEADERS: usize = 32;
pub const MAX_JSON_DEPTH: usize = 16;

/// BUS-017's structured error body. Details deliberately allow extension keys.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionError {
    pub error_code: ErrorCode,
    pub message: String,
    pub details: serde_json::Map<String, serde_json::Value>,
}

impl SessionError {
    /// Identical result for absent and unowned selectors; do not add diagnostics.
    pub fn forbidden() -> Self {
        Self {
            error_code: ErrorCode::Forbidden,
            message: "forbidden".into(),
            details: Default::default(),
        }
    }

    pub fn rc(&self) -> u8 {
        if self.error_code == ErrorCode::Unavailable {
            crate::RC_FAILURE
        } else {
            crate::RC_ERROR
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    InvalidArgument,
    Forbidden,
    NotFound,
    StaleGeneration,
    Conflict,
    Expired,
    ResourceLimit,
    Unsupported,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordRef {
    pub record_id: HexBytes<16>,
    pub incarnation: HexBytes<16>,
    pub binding_generation: DecimalU64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllocateArgs {
    pub public_key: HexBytes<32>,
    pub signature: HexBytes<64>,
    #[serde(default)]
    pub policy: Policy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantCreateArgs {
    pub parent: RecordRef,
    pub pane_id: DecimalU64,
    pub pane_generation: DecimalU64,
    pub public_key: HexBytes<32>,
    pub role: Role,
    pub capabilities: Vec<Capability>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyArgs {
    pub public_key: HexBytes<32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordChallenge {
    pub record_id: HexBytes<16>,
    pub incarnation: HexBytes<16>,
    pub purpose: Purpose,
    #[serde(deserialize_with = "present_nullable")]
    pub grant_id: Option<HexBytes<16>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyChallenge {
    pub public_key: HexBytes<32>,
    pub purpose: Purpose,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChallengeArgs {
    Record(RecordChallenge),
    Key(KeyChallenge),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProveArgs {
    pub challenge_id: HexBytes<16>,
    pub signature: HexBytes<64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetArgs {
    pub target: RecordRef,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}

/// Parsed arguments only: authentication, strict Ed25519 verification,
/// generation/high-water checks and atomic mutation are the broker's job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionCommand {
    Hello,
    Allocate(AllocateArgs),
    GrantCreate(GrantCreateArgs),
    GrantFetch(KeyArgs),
    Challenge(ChallengeArgs),
    Prove(ProveArgs),
    Renew(TargetArgs),
    Revoke(TargetArgs),
    List,
    LeaseCheck(TargetArgs),
}

impl SessionCommand {
    pub fn retained_mutation(&self) -> bool {
        matches!(
            self,
            Self::Allocate(_) | Self::GrantCreate(_) | Self::Revoke(_)
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BootstrapRequest {
    pub message: BusMessage,
    pub command: SessionCommand,
}

fn args<T: serde::de::DeserializeOwned>(body: &str) -> Result<T, WireError> {
    serde_json::from_str(body).map_err(|_| WireError("invalid command arguments"))
}

/// Strict, bounded additive entry point. It never calls the compatibility
/// parser: header and JSON duplicate evidence is checked before any map loses it.
pub fn parse_bootstrap(input: &[u8]) -> Result<BootstrapRequest, WireError> {
    if input.len() > MAX_BOOTSTRAP_BYTES {
        return Err(WireError("bootstrap byte limit"));
    }
    let text = std::str::from_utf8(input).map_err(|_| WireError("invalid UTF-8"))?;
    let mut lines = text.split_inclusive('\n');
    if lines.next() != Some("---\n") {
        return Err(WireError("missing opening delimiter"));
    }
    let mut offset = 4;
    let mut message = BusMessage::new();
    let mut keys = HashSet::new();
    let mut closed = false;
    for (count, line) in lines.enumerate() {
        offset += line.len();
        if line == "---\n" || line == "---" {
            closed = true;
            break;
        }
        if count >= MAX_BOOTSTRAP_HEADERS {
            return Err(WireError("bootstrap header limit"));
        }
        let line = line.strip_suffix('\n').unwrap_or(line);
        let (key, value) = line.split_once(':').ok_or(WireError("malformed header"))?;
        if key.is_empty()
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            || value.bytes().any(|b| b < 0x20 && b != b'\t' || b == 0x7f)
        {
            return Err(WireError("malformed header"));
        }
        if !keys.insert(key.to_ascii_lowercase()) {
            return Err(WireError("duplicate header"));
        }
        if key.eq_ignore_ascii_case(PRINCIPAL_HEADER) {
            continue;
        }
        if !matches!(
            key,
            "bus" | "type" | "to" | "id" | "native-session" | "command" | "from"
        ) {
            return Err(WireError("unknown header"));
        }
        message.set(key, value.trim());
    }
    if !closed {
        return Err(WireError("missing closing delimiter"));
    }
    for (key, value) in [
        ("bus", "1"),
        ("type", "request"),
        ("to", "noded"),
        ("native-session", "1"),
    ] {
        if message.get(key) != Some(value) {
            return Err(WireError("invalid envelope"));
        }
    }
    let id = message
        .get("id")
        .ok_or(WireError("missing correlation id"))?;
    if id.is_empty() || id.len() > 128 || !id.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        return Err(WireError("invalid correlation id"));
    }
    let body = &text[offset..];
    validate_json(body.as_bytes())?;
    // Top-level objects only, including for commands with no arguments.
    if !body.trim_start().starts_with('{') {
        return Err(WireError("arguments must be an object"));
    }
    let command = match message.command_name() {
        Some("noded.session.hello") => {
            args::<EmptyArgs>(body)?;
            SessionCommand::Hello
        }
        Some("noded.session.list") => {
            args::<EmptyArgs>(body)?;
            SessionCommand::List
        }
        Some("noded.session.allocate") => SessionCommand::Allocate(args(body)?),
        Some("noded.session.grant.create") => {
            let g: GrantCreateArgs = args(body)?;
            if g.role != Role::PaneShell
                || g.pane_generation.0 == 0
                || g.parent.binding_generation.0 == 0
            {
                return Err(WireError("invalid grant scope"));
            }
            encode_capabilities(&g.capabilities)?;
            SessionCommand::GrantCreate(g)
        }
        Some("noded.session.grant.fetch") => SessionCommand::GrantFetch(args(body)?),
        Some("noded.session.challenge") => {
            if let Ok(key) = args::<KeyChallenge>(body) {
                if key.purpose != Purpose::Enrol {
                    return Err(WireError("invalid key challenge purpose"));
                }
                SessionCommand::Challenge(ChallengeArgs::Key(key))
            } else {
                let record: RecordChallenge = args(body)?;
                if record.grant_id.is_some() != (record.purpose == Purpose::Enrol) {
                    return Err(WireError("invalid grant selector"));
                }
                SessionCommand::Challenge(ChallengeArgs::Record(record))
            }
        }
        Some("noded.session.prove") => SessionCommand::Prove(args(body)?),
        Some("noded.session.renew") => SessionCommand::Renew(args(body)?),
        Some("noded.session.revoke") => SessionCommand::Revoke(args(body)?),
        Some("noded.session.lease.check") => SessionCommand::LeaseCheck(args(body)?),
        _ => return Err(WireError("unknown session command")),
    };
    if command.retained_mutation() {
        if id.starts_with('0')
            || !id.bytes().all(|b| b.is_ascii_digit())
            || id.parse::<u64>().is_err()
        {
            return Err(WireError("invalid mutation id"));
        }
    }
    message.body = body.to_owned();
    Ok(BootstrapRequest { message, command })
}

/// A bounded duplicate-preserving pre-scan. Strings are decoded before comparing
/// keys, so `"a"` and `"\u0061"` collide. No JSON object is collected into a map.
pub(crate) fn validate_json(bytes: &[u8]) -> Result<(), WireError> {
    if bytes.len() > MAX_BOOTSTRAP_BYTES {
        return Err(WireError("JSON byte limit"));
    }
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    JsonScan(0)
        .deserialize(&mut deserializer)
        .map_err(|_| WireError("invalid or duplicate JSON"))?;
    deserializer.end().map_err(|_| WireError("trailing JSON"))
}

struct JsonScan(usize);
impl<'de> DeserializeSeed<'de> for JsonScan {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for JsonScan {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("bounded JSON without duplicate members")
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<(), E> {
        Ok(())
    }
    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        if self.0 >= MAX_JSON_DEPTH {
            return Err(de::Error::custom("JSON depth limit"));
        }
        while seq.next_element_seed(JsonScan(self.0 + 1))?.is_some() {}
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        if self.0 >= MAX_JSON_DEPTH {
            return Err(de::Error::custom("JSON depth limit"));
        }
        let mut keys = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key) {
                return Err(de::Error::custom("duplicate JSON member"));
            }
            map.next_value_seed(JsonScan(self.0 + 1))?;
        }
        Ok(())
    }
}
