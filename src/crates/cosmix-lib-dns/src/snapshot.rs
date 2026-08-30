//! `config_hash` (`snapshot.rs`) — the serial-excluded cross-node
//! config-drift parity **primitive**.
//!
//! **Scope (deliberate, spine-consistent — not a gap, not a smuggled-in
//! Layer-1 fulfilment):** `config_hash` is an order-stable hash of the
//! *flattened hand-maintained configuration* — sorted zones → sorted
//! rrset keys → sorted rdatas, plus `soa`, the `ns` set, **and the
//! hand-set `configured_serial_floor`** (two `zones.mix` files with
//! different `serial_floor` ARE config drift), **excluding only the
//! daemon-owned `emitted_serial`** (and owner-stanza placement /
//! comments, which never reach Layer 2).
//!
//! It answers exactly one question: *"are the hand-maintained
//! `zones.mix` files equivalent?"* — a **config-equivalence** hash, not
//! a resolution-equivalence one. `emitted_serial` is excluded because
//! it monotone-ticks per node independently (Q2): two nodes with
//! byte-identical zones legitimately diverge in `emitted_serial`, and
//! hashing it would false-fail config-drift parity.
//!
//! P1 ships **only the primitive**. The plan §8 *Layer-1* check is an
//! Bus cross-node diff and **P1 forbids Bus** — so P1 does not perform
//! the cross-node comparison; it computes the per-node value the future
//! P2 check will compare. `config_hash` is **not** consumed by the
//! per-owner replay floor (serial-only) **nor** by the emitted-SOA bump
//! (a monotone tick consulting neither content nor any hash). No
//! serial-inclusive hash ships in P1 (no P1 consumer — YAGNI).

use crate::canonical::Name;
use crate::model::{RrsetKey, RrsetValue, Serial, SoaConfig, ZoneName};
use crate::rr::RData;
use std::collections::HashMap;

/// Order-stable config-equivalence hash. Deterministic across processes
/// and builds (hand-rolled FNV-1a over a canonical byte serialization —
/// no `DefaultHasher`, whose output is not stable across releases).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SnapshotHash(pub u64);

/// Per-zone inputs to the config hash (borrowed from the candidate at
/// flatten time — `configured_serial_floor` is **not** on the served
/// `ZoneAnswerState`, so it is threaded explicitly here).
pub struct ZoneConfigView<'a> {
    pub zone: &'a ZoneName,
    pub soa: &'a SoaConfig,
    pub ns: &'a [Name],
    pub configured_serial_floor: Serial,
    pub rrsets: &'a HashMap<RrsetKey, RrsetValue>,
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Fnv(FNV_OFFSET)
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(FNV_PRIME);
        }
    }
    /// Length-delimited field write so `"a" + "bc"` ≠ `"ab" + "c"`.
    fn field(&mut self, bytes: &[u8]) {
        self.write(&(bytes.len() as u64).to_le_bytes());
        self.write(bytes);
    }
}

fn rdata_canonical(rd: &RData) -> Vec<u8> {
    let mut v = Vec::new();
    match rd {
        RData::A(ip) => {
            v.push(1);
            v.extend_from_slice(&ip.octets());
        }
        RData::Aaaa(ip) => {
            v.push(2);
            v.extend_from_slice(&ip.octets());
        }
        RData::Ns(n) => {
            v.push(3);
            v.extend_from_slice(n.as_str().as_bytes());
        }
        RData::Mx { pref, exch } => {
            v.push(4);
            v.extend_from_slice(&pref.to_le_bytes());
            v.extend_from_slice(exch.as_str().as_bytes());
        }
        RData::Srv {
            prio,
            weight,
            port,
            target,
        } => {
            v.push(5);
            v.extend_from_slice(&prio.to_le_bytes());
            v.extend_from_slice(&weight.to_le_bytes());
            v.extend_from_slice(&port.to_le_bytes());
            v.extend_from_slice(target.as_str().as_bytes());
        }
        RData::Txt(parts) => {
            v.push(6);
            for p in parts {
                v.extend_from_slice(&(p.len() as u64).to_le_bytes());
                v.extend_from_slice(p.as_bytes());
            }
        }
        RData::Ptr(n) => {
            v.push(7);
            v.extend_from_slice(n.as_str().as_bytes());
        }
        RData::Soa(s) => {
            v.push(8);
            v.extend_from_slice(s.primary.as_str().as_bytes());
            v.extend_from_slice(s.mbox.as_str().as_bytes());
            v.extend_from_slice(&s.ttl.to_le_bytes());
            v.extend_from_slice(&s.minimum.to_le_bytes());
        }
    }
    v
}

/// Compute the config-equivalence hash. Zones, rrset keys and rdatas
/// are all sorted so the result is independent of map iteration order
/// and `zones.mix` line order. **`emitted_serial` is never fed in.**
pub fn compute_config_hash(views: &[ZoneConfigView]) -> SnapshotHash {
    let mut zones: Vec<&ZoneConfigView> = views.iter().collect();
    zones.sort_by(|a, b| a.zone.name().as_str().cmp(b.zone.name().as_str()));

    let mut h = Fnv::new();
    for zv in zones {
        h.field(zv.zone.name().as_str().as_bytes());
        h.field(zv.soa.primary.as_str().as_bytes());
        h.field(zv.soa.mbox.as_str().as_bytes());
        h.write(&zv.soa.ttl.to_le_bytes());
        h.write(&zv.soa.minimum.to_le_bytes());
        // Hand-set floor IS config (drift surface) — included.
        h.write(&zv.configured_serial_floor.0.to_le_bytes());

        let mut ns: Vec<&str> = zv.ns.iter().map(Name::as_str).collect();
        ns.sort_unstable();
        h.write(&(ns.len() as u64).to_le_bytes());
        for n in ns {
            h.field(n.as_bytes());
        }

        // Sort rrset keys deterministically by (name, type-discriminant).
        let mut keys: Vec<&RrsetKey> = zv.rrsets.keys().collect();
        keys.sort_by(|a, b| {
            a.name
                .as_str()
                .cmp(b.name.as_str())
                .then((a.rr_type as u8).cmp(&(b.rr_type as u8)))
        });
        h.write(&(keys.len() as u64).to_le_bytes());
        for k in keys {
            h.field(k.name.as_str().as_bytes());
            h.write(&[k.rr_type as u8]);
            let val = &zv.rrsets[k];
            h.write(&val.ttl.to_le_bytes());
            // `rdatas` is a BTreeSet → already canonically ordered.
            h.write(&(val.rdatas.len() as u64).to_le_bytes());
            for rd in &val.rdatas {
                let bytes = rdata_canonical(rd);
                h.field(&bytes);
            }
        }
    }
    SnapshotHash(h.0)
}
