//! The two-layer data model (`model.rs`) — the load-bearing part.
//!
//! ```text
//! ⛔ Anti-pattern box (prompt §3) — both forbidden:
//! (a) Flatten before the candidate path (discard owner/serial/
//!     withdrawn) — forces a v1 rewrite.
//! (b) Hoist `bundles` into the query-time `ZoneSnapshot` — owner data
//!     at resolve time. The resolver signature is the structural guard.
//! ```
//!
//! **Layer 1** (owner-aware, consumed never served): `OwnerId`,
//! `ZoneName`, `RecordAssertion`, `Bundle`, `ZoneSource`,
//! `ParsedZones`.
//!
//! **Layer 2** (flattened, served, NO owners): `RrsetKey`,
//! `RrsetValue`, `ZoneAnswerState`, `ZoneSnapshot`.

use crate::canonical::Name;
use crate::rr::{RData, RecordType};
use crate::snapshot::SnapshotHash;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

pub use crate::rr::SoaConfig;

// ---------------------------------------------------------------------
// RFC 1982 serial-number arithmetic
// ---------------------------------------------------------------------

/// A DNS serial number. RFC 1982 comparison is a **partial** order:
/// serials exactly 2³¹ apart are *undefined*. Therefore `Serial`
/// deliberately does **not** implement `Ord` and must never be compared
/// with bare `<`/`>` — use [`Serial::compare`], handling `Undefined`
/// fail-closed at every call site.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Serial(pub u32);

/// Result of an RFC 1982 §3.2 comparison. `Undefined` is the 2³¹
/// half-range case — every caller treats it as fail-closed (reject).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SerialCmp {
    Less,
    Equal,
    Greater,
    Undefined,
}

impl Serial {
    /// Compare `self` against `other` per RFC 1982 §3.2.
    ///
    /// `Less` means `self` precedes `other` (e.g. an incoming bundle
    /// older than the stored floor → stale replay). `Undefined` is the
    /// exactly-2³¹ gap — a serial-discipline violation, never silently
    /// treated as newer or older.
    pub fn compare(self, other: Serial) -> SerialCmp {
        let d = other.0.wrapping_sub(self.0);
        if d == 0 {
            SerialCmp::Equal
        } else if d < 0x8000_0000 {
            SerialCmp::Less
        } else if d == 0x8000_0000 {
            SerialCmp::Undefined
        } else {
            SerialCmp::Greater
        }
    }

    /// The RFC 1982 monotone successor (`+1 mod 2³²`). Used for the
    /// emitted-SOA tick; `compare(self, self.successor())` is always
    /// `Less` (never `Undefined`, since the gap is 1).
    pub fn successor(self) -> Serial {
        Serial(self.0.wrapping_add(1))
    }

    /// The RFC-1982 maximum of two serials, fail-closed: `Undefined`
    /// (the 2³¹ gap) yields `None`. Used to seat the emitted-SOA base
    /// at `max(persisted_floor, configured_floor)` before the tick.
    pub fn rfc1982_max(self, other: Serial) -> Option<Serial> {
        match self.compare(other) {
            SerialCmp::Less => Some(other),
            SerialCmp::Equal | SerialCmp::Greater => Some(self),
            SerialCmp::Undefined => None,
        }
    }
}

// ---------------------------------------------------------------------
// Layer 1 — owner-aware source/candidate (consumed, never served)
// ---------------------------------------------------------------------

/// An owner identity. In v0 this is just the node name; v1 binds it to
/// SPEC-10 identity (gated, not P1).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OwnerId(pub String);

/// A zone apex. Wraps the canonical `Name`, so apex comparison is plain
/// string equality.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ZoneName(pub Name);

impl ZoneName {
    pub fn name(&self) -> &Name {
        &self.0
    }
}

/// One asserted record. `withdrawn` is the tombstone bit; v0
/// round-trips it but never emits one (it deletes the line instead).
/// Withdrawal granularity / `data`-required are v1-OPEN (plan §3 b3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordAssertion {
    pub owner: OwnerId,
    pub name: Name,
    pub rr_type: RecordType,
    pub ttl: u32,
    pub rdata: RData,
    pub withdrawn: bool,
}

/// A per-owner bundle carrying that owner's serial.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bundle {
    pub owner: OwnerId,
    pub serial: Serial,
    pub records: Vec<RecordAssertion>,
}

/// One zone's owner-aware source. `configured_serial_floor` comes from
/// `zones.mix` and is **never written back** (the daemon is read-only
/// w.r.t. `zones.mix`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZoneSource {
    pub name: ZoneName,
    pub soa: SoaConfig,
    pub ns: Vec<Name>,
    pub configured_serial_floor: Serial,
    pub bundles: BTreeMap<OwnerId, Bundle>,
}

/// All zones, owner-aware. The output of `strict_data` and the input to
/// the candidate path. Never served.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedZones {
    pub zones: BTreeMap<ZoneName, ZoneSource>,
}

// ---------------------------------------------------------------------
// Layer 2 — flattened answerable snapshot (served; NO owners)
// ---------------------------------------------------------------------

/// RRset index key. Apex SOA/NS are **never** keyed here (assembly
/// contract — they live only in `ZoneAnswerState.soa`/`.ns`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RrsetKey {
    pub zone: ZoneName,
    pub name: Name,
    pub rr_type: RecordType,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RrsetValue {
    pub ttl: u32,
    pub rdatas: BTreeSet<RData>,
}

/// One zone's flattened answerable state. **No owner anywhere.**
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZoneAnswerState {
    pub soa: SoaConfig,
    pub ns: Vec<Name>,
    /// The one emitted SOA serial — daemon-owned, monotone-ticked on
    /// every adopted reload (Q2). Excluded from `config_hash`.
    pub emitted_serial: Serial,
    pub rrsets: HashMap<RrsetKey, RrsetValue>,
    /// NXDOMAIN-vs-NODATA discriminator: every name with ≥1 record,
    /// plus the apex (apex always exists — it has SOA/NS).
    pub existing_names: HashSet<Name>,
}

/// The served snapshot. No `owner` anywhere. `config_hash` is the
/// serial-excluded cross-node config-drift parity primitive (see
/// `snapshot.rs`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZoneSnapshot {
    pub zones: BTreeMap<ZoneName, ZoneAnswerState>,
    pub config_hash: SnapshotHash,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc1982_basic_and_wraparound() {
        assert_eq!(Serial(1).compare(Serial(1)), SerialCmp::Equal);
        assert_eq!(Serial(1).compare(Serial(2)), SerialCmp::Less);
        assert_eq!(Serial(2).compare(Serial(1)), SerialCmp::Greater);
        // Wrap-around: 0xFFFF_FFFF precedes 0 (gap 1).
        assert_eq!(
            Serial(0xFFFF_FFFF).compare(Serial(0)),
            SerialCmp::Less,
            "wrap: max precedes 0"
        );
        assert_eq!(Serial(0).compare(Serial(0xFFFF_FFFF)), SerialCmp::Greater);
    }

    #[test]
    fn rfc1982_undefined_half_range() {
        // Exactly 2^31 apart in either direction → Undefined.
        assert_eq!(Serial(0).compare(Serial(0x8000_0000)), SerialCmp::Undefined);
        assert_eq!(Serial(0x8000_0000).compare(Serial(0)), SerialCmp::Undefined);
        assert_eq!(
            Serial(100).compare(Serial(100u32.wrapping_add(0x8000_0000))),
            SerialCmp::Undefined
        );
    }

    #[test]
    fn successor_is_always_less_than_itself_never_undefined() {
        for s in [0u32, 1, 0x7FFF_FFFF, 0x8000_0000, 0xFFFF_FFFF] {
            let a = Serial(s);
            assert_eq!(a.compare(a.successor()), SerialCmp::Less);
        }
    }

    #[test]
    fn rfc1982_max_fail_closed_on_undefined() {
        assert_eq!(Serial(5).rfc1982_max(Serial(9)), Some(Serial(9)));
        assert_eq!(Serial(9).rfc1982_max(Serial(5)), Some(Serial(9)));
        assert_eq!(Serial(0).rfc1982_max(Serial(0x8000_0000)), None);
    }

    /// Compile-enforced: `Serial` must NOT be `Ord` (RFC 1982 partial
    /// order). This test asserts it via a trait-bound trick that only
    /// compiles if the negative holds at the type level is not
    /// expressible directly, so we assert behaviourally instead: a
    /// half-range pair has no total-order answer.
    #[test]
    fn serial_is_partial_not_total() {
        let a = Serial(0);
        let b = Serial(0x8000_0000);
        // If `Serial: Ord` existed and were used, this pair would have
        // to return Less or Greater. The model forces Undefined.
        assert_eq!(a.compare(b), SerialCmp::Undefined);
    }
}
