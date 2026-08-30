//! Shared fixtures + helpers for the `cosmix-lib-dns` §9 test matrix.
//!
//! Not a test file itself (Cargo treats `tests/common/mod.rs` as a
//! plain module included via `mod common;`). Each integration test
//! crate links only the subset of helpers it uses, so the unused-in-
//! this-crate ones would warn — silence at the module root.
#![allow(dead_code)]

use cosmix_dns::{InMemoryPersistence, Persistence};
use cosmix_dns::{
    ParsedZones, RejectReason, ZoneSnapshot, adopt_candidate, build_candidate, parse_zones,
    validate_candidate,
};
use hickory_proto::op::{Edns, Message, MessageType, OpCode, Query};
use hickory_proto::rr::{DNSClass, Name as HName, RecordType as HRecordType};
use std::sync::Arc;

/// A tailored zones.mix exercising every §9 resolver/candidate path:
/// multi-MX one name, in-zone + out-of-zone MX glue, SRV, TXT, AAAA,
/// a withdrawn (tombstone) record, two owners (`alpha` serial 5,
/// `gamma` serial 3), `example.com`, and the `2.0.192.in-addr.arpa`
/// reverse zone with the five mesh PTRs. Same map/list strict-data
/// schema as the committed `cosmix-dnsd/zones.mix`.
pub const ZONES_SRC: &str = r#"zones: {
  "bus": {
    soa: { primary: "gw.bus", mbox: "hostmaster.bus", ttl: 300, minimum: 60 },
    ns: [ "gw.bus" ],
    serial_floor: 1,
    bundles: [
      { owner: "alpha", serial: 5, records: [
          { name: "alpha.bus", type: "A", ttl: 300, data: "192.0.2.5" },
          { name: "alpha.bus", type: "AAAA", ttl: 300, data: "fd00::5" },
          { name: "alpha.bus", type: "MX", ttl: 300, data: "10 alpha.bus" },
          { name: "alpha.bus", type: "MX", ttl: 300, data: "20 gamma.bus" },
          { name: "alpha.bus", type: "TXT", ttl: 300, data: "v=spf1 -all" },
          { name: "_imaps._tcp.alpha.bus", type: "SRV", ttl: 300, data: "0 1 993 alpha.bus" },
          { name: "ext.bus", type: "MX", ttl: 300, data: "10 mail.example.net" },
          { name: "old.bus", type: "A", ttl: 300, withdrawn: true, data: "1.2.3.4" },
          { name: "*.wild.bus", type: "A", ttl: 300, data: "192.0.2.99" },
          { name: "host.wild.bus", type: "A", ttl: 300, data: "192.0.2.50" },
          { name: "deep.sub.wild.bus", type: "A", ttl: 300, data: "192.0.2.77" }
      ] },
      { owner: "gamma", serial: 3, records: [
          { name: "gamma.bus", type: "A", ttl: 300, data: "192.0.2.4" }
      ] }
    ]
  },
  "example.com": {
    soa: { primary: "gw.bus", mbox: "hostmaster.bus", ttl: 300, minimum: 60 },
    ns: [ "gw.bus" ],
    serial_floor: 1,
    bundles: [
      { owner: "alpha", serial: 5, records: [
          { name: "alpha.example.com", type: "A", ttl: 300, data: "192.0.2.5" }
      ] }
    ]
  },
  "2.0.192.in-addr.arpa": {
    soa: { primary: "gw.bus", mbox: "hostmaster.bus", ttl: 300, minimum: 60 },
    ns: [ "gw.bus" ],
    serial_floor: 1,
    bundles: [
      { owner: "alpha", serial: 5, records: [
          { name: "5.2.0.192.in-addr.arpa", type: "PTR", ttl: 300, data: "alpha.bus" }
      ] },
      { owner: "gamma", serial: 3, records: [
          { name: "4.2.0.192.in-addr.arpa", type: "PTR", ttl: 300, data: "gamma.bus" }
      ] },
      { owner: "delta", serial: 1, records: [
          { name: "210.2.0.192.in-addr.arpa", type: "PTR", ttl: 300, data: "delta.bus" }
      ] },
      { owner: "epsilon", serial: 1, records: [
          { name: "9.2.0.192.in-addr.arpa", type: "PTR", ttl: 300, data: "epsilon.bus" }
      ] },
      { owner: "gw", serial: 1, records: [
          { name: "1.2.0.192.in-addr.arpa", type: "PTR", ttl: 300, data: "gw.bus" }
      ] }
    ]
  }
}
"#;

pub fn parse(src: &str) -> ParsedZones {
    parse_zones(src).expect("fixture parses")
}

/// Run the full candidate path once (build → validate → adopt) against
/// `p`, threading `current` for the serial-discipline WARN path.
pub fn adopt(
    src: &str,
    p: &mut dyn Persistence,
    current: Option<Arc<ZoneSnapshot>>,
) -> Result<Arc<ZoneSnapshot>, RejectReason> {
    let parsed = parse_zones(src).map_err(|e| RejectReason::Malformed(e.to_string()))?;
    let cand = build_candidate(parsed, &*p)?;
    validate_candidate(&cand)?;
    adopt_candidate(current, cand, p)
}

/// First adopt against a fresh in-memory store; panics on reject.
pub fn fresh_snapshot(src: &str) -> (Arc<ZoneSnapshot>, InMemoryPersistence) {
    let mut p = InMemoryPersistence::new();
    let snap = adopt(src, &mut p, None).expect("fresh adopt");
    (snap, p)
}

/// Build a query `Message` (id 0x2a2a). `edns` = advertised UDP payload
/// when `Some` (adds an OPT). Class defaults to IN.
pub fn query_msg(name: &str, qtype: HRecordType, edns: Option<u16>) -> Message {
    query_msg_class(name, qtype, DNSClass::IN, edns)
}

pub fn query_msg_class(
    name: &str,
    qtype: HRecordType,
    class: DNSClass,
    edns: Option<u16>,
) -> Message {
    let mut q = Query::query(HName::from_ascii(name).expect("test qname"), qtype);
    q.set_query_class(class);
    let mut m = Message::new();
    m.set_id(0x2a2a);
    m.set_message_type(MessageType::Query);
    m.set_op_code(OpCode::Query);
    m.set_recursion_desired(true);
    m.add_query(q);
    if let Some(payload) = edns {
        let mut e = Edns::new();
        e.set_version(0);
        e.set_max_payload(payload);
        m.set_edns(e);
    }
    m
}

/// Pull the SOA `serial` out of the first SOA record in a slice
/// (answer or authority section).
pub fn soa_serial(records: &[hickory_proto::rr::Record]) -> Option<u32> {
    use hickory_proto::rr::RData as HRData;
    records.iter().find_map(|r| match r.data() {
        HRData::SOA(soa) => Some(soa.serial()),
        _ => None,
    })
}
