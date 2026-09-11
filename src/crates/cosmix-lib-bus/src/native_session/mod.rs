//! Additive native-session wire primitives (BUS-013–017).
//!
//! These types carry broker assertions; decoding them does not authenticate a
//! transport. Only a broker may construct trusted transport context, and only
//! recipients on a verified broker connection may trust a principal header.

mod bootstrap;
mod principal;
mod record;
#[cfg(test)]
mod tests;
mod transcript;

pub use bootstrap::*;
pub use principal::*;
pub use record::*;
pub use transcript::*;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use std::net::IpAddr;

/// Kernel/mesh context, never deserialised from caller-supplied metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportIdentity {
    /// SO_PEERCRED snapshot. PID is diagnostic only, including after setuid.
    LocalUnix {
        uid: u32,
        gid: u32,
        peer_pid: u32,
    },
    LegacyTcp {
        source_ip: IpAddr,
    },
    /// D2 admission remains independent of native-session UID authority.
    Mesh {
        source_ip: IpAddr,
        admission: MeshAdmission,
    },
}

/// Wire-layer representation of the existing broker's D2 admission state.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MeshAdmission {
    pub admitted_node: Option<String>,
    pub last_detail: Option<&'static str>,
    pub response_seen: bool,
}

/// Exactly N bytes encoded as lowercase hexadecimal (BUS-016).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HexBytes<const N: usize>(pub [u8; N]);

impl<const N: usize> Serialize for HexBytes<N> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut s = String::with_capacity(N * 2);
        for byte in self.0 {
            s.push(HEX[(byte >> 4) as usize] as char);
            s.push(HEX[(byte & 15) as usize] as char);
        }
        serializer.serialize_str(&s)
    }
}

impl<'de, const N: usize> Deserialize<'de> for HexBytes<N> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        if s.len() != N * 2
            || !s
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(de::Error::custom("invalid lowercase hexadecimal encoding"));
        }
        let mut bytes = [0; N];
        for (out, pair) in bytes.iter_mut().zip(s.as_bytes().chunks_exact(2)) {
            let nibble = |b: u8| if b <= b'9' { b - b'0' } else { b - b'a' + 10 };
            *out = (nibble(pair[0]) << 4) | nibble(pair[1]);
        }
        Ok(Self(bytes))
    }
}

/// Canonical decimal-string u64, never a JSON number (BUS-016).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecimalU64(pub u64);

impl Serialize for DecimalU64 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for DecimalU64 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        if s.is_empty()
            || (s.len() > 1 && s.starts_with('0'))
            || !s.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(de::Error::custom("invalid canonical decimal encoding"));
        }
        s.parse()
            .map(Self)
            .map_err(|_| de::Error::custom("u64 overflow"))
    }
}

/// `deserialize_with` makes a nullable member required: omission must not take
/// Serde's implicit Option default. JSON null remains valid.
fn present_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    Term,
    PaneShell,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Term => "term",
            Self::PaneShell => "pane-shell",
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Policy {
    #[default]
    DefaultOpen,
    Restricted,
}

impl Policy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DefaultOpen => "default-open",
            Self::Restricted => "restricted",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    ReadState,
    ReadContents,
    Input,
    Execute,
    ManageLayout,
    Terminate,
}

impl Capability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadState => "read_state",
            Self::ReadContents => "read_contents",
            Self::Input => "input",
            Self::Execute => "execute",
            Self::ManageLayout => "manage_layout",
            Self::Terminate => "terminate",
        }
    }
}

/// Static diagnostic only: never includes submitted keys, proofs or payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireError(pub &'static str);

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for WireError {}
