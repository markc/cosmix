//! RR vocabulary (`rr.rs`) — the **closed** record-type/rdata set.
//!
//! `RecordType` is a closed enum: `Soa, Ns, A, Aaaa, Mx, Srv, Txt,
//! Ptr`. **No `Cname`. No `Hinfo`.** Those are not members, cannot be
//! parsed from `zones.mix`, cannot be stored, and cannot be
//! synthesized — the resolver works in these model types only, so it is
//! structurally incapable of emitting a CNAME or a HINFO/RFC-8482
//! variant (prompt §5/§6, hard rules 2/8).
//!
//! Conversion to hickory `Record` happens **only** here (`to_hickory`),
//! invoked at the `wire.rs` edge; the rest of the crate is in model
//! types.

use crate::canonical::Name;
use hickory_proto::rr::rdata::{A, AAAA, MX, NS, PTR, SOA, SRV, TXT};
use hickory_proto::rr::{RData as HRData, Record, RecordType as HRecordType};
use std::net::{Ipv4Addr, Ipv6Addr};

/// Closed authoritative RR type vocabulary (prompt §3). Ordering is
/// stable and used by `config_hash` canonical serialization.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RecordType {
    Soa,
    Ns,
    A,
    Aaaa,
    Mx,
    Srv,
    Txt,
    Ptr,
}

impl RecordType {
    /// Parse the `type` string from `zones.mix`. The closed vocab is the
    /// validation: `SOA` (apex-only, comes from the `soa` block, never a
    /// `records` entry), `CNAME`, `HINFO` and anything else are `None`
    /// → the loader turns that into a reject.
    pub fn parse(s: &str) -> Option<RecordType> {
        match s.to_ascii_uppercase().as_str() {
            "NS" => Some(RecordType::Ns),
            "A" => Some(RecordType::A),
            "AAAA" => Some(RecordType::Aaaa),
            "MX" => Some(RecordType::Mx),
            "SRV" => Some(RecordType::Srv),
            "TXT" => Some(RecordType::Txt),
            "PTR" => Some(RecordType::Ptr),
            // "SOA" is intentionally not accepted here; "CNAME"/"HINFO"
            // are not in the vocab at all.
            _ => None,
        }
    }

    pub fn to_hickory(self) -> HRecordType {
        match self {
            RecordType::Soa => HRecordType::SOA,
            RecordType::Ns => HRecordType::NS,
            RecordType::A => HRecordType::A,
            RecordType::Aaaa => HRecordType::AAAA,
            RecordType::Mx => HRecordType::MX,
            RecordType::Srv => HRecordType::SRV,
            RecordType::Txt => HRecordType::TXT,
            RecordType::Ptr => HRecordType::PTR,
        }
    }

    /// Map an inbound hickory query type to the model vocab. `ANY` and
    /// any out-of-vocab type return `None` — `ANY` is handled
    /// specially by the resolver (no expansion), the rest are NODATA.
    pub fn from_hickory(h: HRecordType) -> Option<RecordType> {
        match h {
            HRecordType::SOA => Some(RecordType::Soa),
            HRecordType::NS => Some(RecordType::Ns),
            HRecordType::A => Some(RecordType::A),
            HRecordType::AAAA => Some(RecordType::Aaaa),
            HRecordType::MX => Some(RecordType::Mx),
            HRecordType::SRV => Some(RecordType::Srv),
            HRecordType::TXT => Some(RecordType::Txt),
            HRecordType::PTR => Some(RecordType::Ptr),
            _ => None,
        }
    }
}

/// SOA configuration (apex only). The emitted `SERIAL` is **not** here —
/// it is the per-node `emitted_serial` on `ZoneAnswerState`, applied at
/// resolve time. `refresh`/`retry`/`expire` are fixed P1 constants
/// (v0 has no zone-transfer secondaries to consume them — prompt §12).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SoaConfig {
    pub primary: Name,
    pub mbox: Name,
    pub ttl: u32,
    pub minimum: u32,
}

/// Fixed SOA timer constants (no AXFR/IXFR secondaries in v0).
pub const SOA_REFRESH: i32 = 3600;
pub const SOA_RETRY: i32 = 600;
pub const SOA_EXPIRE: i32 = 604_800;

/// Closed RDATA vocabulary. Derives `Ord`/`Hash` so an RRset's rdatas
/// live in a `BTreeSet` (order-stable for `config_hash`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RData {
    A(Ipv4Addr),
    Aaaa(Ipv6Addr),
    Ns(Name),
    Mx {
        pref: u16,
        exch: Name,
    },
    Srv {
        prio: u16,
        weight: u16,
        port: u16,
        target: Name,
    },
    Txt(Vec<String>),
    Ptr(Name),
    Soa(SoaConfig),
}

impl RData {
    pub fn record_type(&self) -> RecordType {
        match self {
            RData::A(_) => RecordType::A,
            RData::Aaaa(_) => RecordType::Aaaa,
            RData::Ns(_) => RecordType::Ns,
            RData::Mx { .. } => RecordType::Mx,
            RData::Srv { .. } => RecordType::Srv,
            RData::Txt(_) => RecordType::Txt,
            RData::Ptr(_) => RecordType::Ptr,
            RData::Soa(_) => RecordType::Soa,
        }
    }

    /// Build a hickory `Record`. `serial` is threaded only for the SOA
    /// path (the resolver passes `emitted_serial`); other variants
    /// ignore it. Wire-edge only.
    pub fn to_hickory_record(&self, name: &Name, ttl: u32, serial: u32) -> Record {
        let hname = name.to_hickory();
        let hrdata = match self {
            RData::A(ip) => HRData::A(A(*ip)),
            RData::Aaaa(ip) => HRData::AAAA(AAAA(*ip)),
            RData::Ns(n) => HRData::NS(NS(n.to_hickory())),
            RData::Mx { pref, exch } => HRData::MX(MX::new(*pref, exch.to_hickory())),
            RData::Srv {
                prio,
                weight,
                port,
                target,
            } => HRData::SRV(SRV::new(*prio, *weight, *port, target.to_hickory())),
            RData::Txt(parts) => {
                // DNS TXT RDATA is a sequence of <=255-byte
                // character-strings (RFC 1035 §3.3.14). hickory's encoder
                // rejects a single string longer than 255 bytes, which
                // surfaced as SERVFAIL for an RSA-2048 DKIM key (~390 B).
                // Split each operator-supplied string into <=255-byte
                // character-strings on UTF-8 codepoint boundaries — a
                // `String` is always valid UTF-8, so this never splits a
                // codepoint (no lossy re-encoding) and never overshoots
                // 255 bytes. The wire result re-assembles transparently at
                // the client, which concatenates the character-strings.
                let mut chunks: Vec<String> = Vec::new();
                for part in parts {
                    if part.is_empty() {
                        chunks.push(String::new());
                        continue;
                    }
                    let mut cur = String::new();
                    for ch in part.chars() {
                        if cur.len() + ch.len_utf8() > 255 {
                            chunks.push(std::mem::take(&mut cur));
                        }
                        cur.push(ch);
                    }
                    chunks.push(cur);
                }
                HRData::TXT(TXT::new(chunks))
            }
            RData::Ptr(n) => HRData::PTR(PTR(n.to_hickory())),
            RData::Soa(soa) => HRData::SOA(SOA::new(
                soa.primary.to_hickory(),
                soa.mbox.to_hickory(),
                serial,
                SOA_REFRESH,
                SOA_RETRY,
                SOA_EXPIRE,
                soa.minimum,
            )),
        };
        Record::from_rdata(hname, ttl, hrdata)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_vocab_rejects_soa_cname_hinfo_in_records() {
        assert_eq!(RecordType::parse("A"), Some(RecordType::A));
        assert_eq!(RecordType::parse("srv"), Some(RecordType::Srv));
        assert_eq!(RecordType::parse("SOA"), None);
        assert_eq!(RecordType::parse("CNAME"), None);
        assert_eq!(RecordType::parse("HINFO"), None);
        assert_eq!(RecordType::parse("WqQ"), None);
    }

    #[test]
    fn any_and_hinfo_have_no_model_type() {
        assert_eq!(RecordType::from_hickory(HRecordType::ANY), None);
        assert_eq!(RecordType::from_hickory(HRecordType::HINFO), None);
        assert_eq!(RecordType::from_hickory(HRecordType::CNAME), None);
    }
}
