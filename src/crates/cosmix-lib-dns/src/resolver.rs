//! The authoritative resolver (`resolver.rs`) — plan §4 / prompt §5.
//!
//! `resolve(&ZoneSnapshot, &Message) -> Message` — **exactly two
//! arguments**. No owner, no store, no persistence, **and no
//! `Transport`/size**. That signature is the structural guard against
//! anti-pattern (b) *and* a transport-blindness guarantee: the resolver
//! is pure, pre-encode, and cannot truncate because the negotiated size
//! is never in scope (truncation is wholly the `wire.rs` edge, §6).
//!
//! Pure and deterministic given `(snap, req)` — no I/O, no clock, no
//! network. There is **no upstream path in the code at all**: a
//! query never recurses or forwards (structural guarantee, plan §5).

use crate::canonical::Name;
use crate::model::{RrsetKey, ZoneAnswerState, ZoneName, ZoneSnapshot};
use crate::rr::{RData, RecordType};
use hickory_proto::op::{Edns, Message, MessageType, OpCode, ResponseCode};
use hickory_proto::rr::{DNSClass, RecordType as HRecordType};
use std::collections::HashSet;

/// Response EDNS payload we advertise when the query carried an OPT
/// (prompt §5 negotiation rule). Truncation itself is the wire edge.
const RESPONSE_EDNS_PAYLOAD: u16 = 1232;

pub fn resolve(snap: &ZoneSnapshot, req: &Message) -> Message {
    let mut resp = Message::new();
    resp.set_id(req.id());
    resp.set_message_type(MessageType::Response);
    resp.set_op_code(req.op_code());
    resp.set_recursion_desired(req.recursion_desired());
    resp.set_recursion_available(false);

    // Echo an OPT advertising our payload when the query had one. The
    // resolver never *uses* the size (it cannot truncate); the serve
    // loop computes `max_payload` independently for `wire::encode_udp`.
    if req.extensions().is_some() {
        let mut edns = Edns::new();
        edns.set_version(0);
        edns.set_max_payload(RESPONSE_EDNS_PAYLOAD);
        resp.set_edns(edns);
    }

    // opcode ≠ QUERY → NOTIMP.
    if req.op_code() != OpCode::Query {
        resp.set_authoritative(false);
        resp.set_response_code(ResponseCode::NotImp);
        return resp;
    }

    // QDCOUNT ≠ 1 → FORMERR.
    if req.queries().len() != 1 {
        resp.set_authoritative(false);
        resp.set_response_code(ResponseCode::FormErr);
        return resp;
    }

    // QUESTION echoed verbatim (original case — 0x20).
    let query = req.queries()[0].clone();
    let qclass = query.query_class();
    let qtype_h = query.query_type();
    let qname = Name::from_hickory(query.name());
    resp.add_query(query);

    // QCLASS ∉ {IN, ANY} → REFUSED.
    if qclass != DNSClass::IN && qclass != DNSClass::ANY {
        resp.set_authoritative(false);
        resp.set_response_code(ResponseCode::Refused);
        return resp;
    }

    // Longest-suffix zone match.
    let Some((zname, zstate)) = match_zone(snap, &qname) else {
        // Out-of-zone → REFUSED, AA=0. Never recurse/forward.
        resp.set_authoritative(false);
        resp.set_response_code(ResponseCode::Refused);
        return resp;
    };

    let is_apex = qname == *zname.name();
    let name_exists = is_apex || zstate.existing_names.contains(&qname);

    // A name absent as a node may still be COVERED by a wildcard: the
    // `*.<closest-encloser>` source of synthesis (RFC 4592). Resolve it
    // once — it drives BOTH the positive synthesis below AND every
    // negative-answer decision (ANY, NODATA-vs-NXDOMAIN, out-of-vocab
    // QTYPEs). A covered name MUST read as existing everywhere, or a
    // resolver negatively caches a name the wildcard actually serves
    // (RFC 8020). `wildcard_node` is the existing `*` owner, if any.
    let wildcard_node = if name_exists {
        None
    } else {
        wildcard_source(&qname, &zstate.existing_names)
            .filter(|w| zstate.existing_names.contains(w))
    };
    let name_covered = name_exists || wildcard_node.is_some();

    // QTYPE=ANY → the single mandatory minimal outcome: NOERROR, AA=1,
    // empty ANSWER, no SOA-as-answer, no synthetic RR, NO RRset
    // enumeration. NXDOMAIN still applies when the name is neither a node
    // nor wildcard-covered. (There is NO HINFO/RFC-8482 variant anywhere —
    // HINFO is not a model type and cannot be constructed.)
    if qtype_h == HRecordType::ANY {
        resp.set_authoritative(true);
        if name_covered {
            resp.set_response_code(ResponseCode::NoError);
        } else {
            resp.set_response_code(ResponseCode::NXDomain);
            add_soa_authority(&mut resp, zname, zstate);
        }
        return resp;
    }

    // Positive apex SOA / NS — answered ONLY from the soa/ns fields
    // (assembly contract: never duplicated into rrsets).
    if is_apex && qtype_h == HRecordType::SOA {
        resp.set_authoritative(true);
        resp.set_response_code(ResponseCode::NoError);
        resp.add_answer(soa_record(zname, zstate));
        return resp;
    }
    if is_apex && qtype_h == HRecordType::NS {
        resp.set_authoritative(true);
        resp.set_response_code(ResponseCode::NoError);
        for ns in &zstate.ns {
            resp.add_answer(RData::Ns(ns.clone()).to_hickory_record(
                zname.name(),
                zstate.soa.ttl,
                0,
            ));
        }
        return resp;
    }

    // Ordinary path. A type we do not model (incl. non-apex SOA/NS with
    // no stored rrset, HINFO, CAA, …) falls through to NODATA/NXDOMAIN.
    let model_qt = RecordType::from_hickory(qtype_h);
    if let Some(rt) = model_qt {
        let key = RrsetKey {
            zone: zname.clone(),
            name: qname.clone(),
            rr_type: rt,
        };
        if let Some(rrset) = zstate.rrsets.get(&key) {
            resp.set_authoritative(true);
            resp.set_response_code(ResponseCode::NoError);
            for rd in &rrset.rdatas {
                resp.add_answer(rd.to_hickory_record(&qname, rrset.ttl, zstate.emitted_serial.0));
                // Additional-section glue for in-zone MX/SRV targets.
                match rd {
                    RData::Mx { exch, .. } => add_glue(&mut resp, snap, exch),
                    RData::Srv { target, .. } => add_glue(&mut resp, snap, target),
                    _ => {}
                }
            }
            return resp;
        }

        // RFC 4592 wildcard synthesis. The exact name had no rrset of this
        // type. If the name is wildcard-covered, emit the QTYPE rrset from
        // the `*.<closest-encloser>` source with owner = qname. A covered
        // name with no rrset of this type falls through to the unified
        // negative branch below, which renders it NODATA (never NXDOMAIN)
        // via `name_covered`.
        if let Some(wname) = &wildcard_node {
            let wkey = RrsetKey {
                zone: zname.clone(),
                name: wname.clone(),
                rr_type: rt,
            };
            if let Some(rrset) = zstate.rrsets.get(&wkey) {
                resp.set_authoritative(true);
                resp.set_response_code(ResponseCode::NoError);
                for rd in &rrset.rdatas {
                    // Owner is the QUERIED name, not the `*` owner.
                    resp.add_answer(rd.to_hickory_record(
                        &qname,
                        rrset.ttl,
                        zstate.emitted_serial.0,
                    ));
                    match rd {
                        RData::Mx { exch, .. } => add_glue(&mut resp, snap, exch),
                        RData::Srv { target, .. } => add_glue(&mut resp, snap, target),
                        _ => {}
                    }
                }
                return resp;
            }
        }
    }

    // No matching RRset (incl. out-of-vocab QTYPEs that skip the block
    // above): NODATA if the name exists as a node OR is wildcard-covered,
    // else NXDOMAIN. Both carry the SOA in AUTHORITY with `emitted_serial`,
    // AA=1.
    resp.set_authoritative(true);
    if name_covered {
        resp.set_response_code(ResponseCode::NoError);
    } else {
        resp.set_response_code(ResponseCode::NXDomain);
    }
    add_soa_authority(&mut resp, zname, zstate);
    resp
}

/// RFC 4592 source of synthesis for `qname`: `*.<closest-encloser>`,
/// where the closest encloser is the longest existing ancestor of
/// `qname` (an owner, an empty non-terminal, or the apex — all carried
/// in `existing`). Walks qname's ancestor chain from nearest to apex and
/// stops at the first that exists. Returns `None` only for a name with
/// no parent (the root); the apex always exists, so an in-zone qname
/// always resolves to some encloser. The caller still checks that the
/// returned `*.<encloser>` is an actual wildcard node before synthesizing
/// — a closest encloser without a `*` child means NXDOMAIN, not a match.
fn wildcard_source(qname: &Name, existing: &HashSet<Name>) -> Option<Name> {
    let mut ancestor = qname.parent()?;
    loop {
        if existing.contains(&ancestor) {
            // Build the `*` owner — `parse_owner` since strict `parse`
            // now rejects a leftmost `*`.
            return Name::parse_owner(&format!("*.{}", ancestor.as_str())).ok();
        }
        ancestor = ancestor.parent()?;
    }
}

/// Longest-suffix zone match.
fn match_zone<'a>(
    snap: &'a ZoneSnapshot,
    qname: &Name,
) -> Option<(&'a ZoneName, &'a ZoneAnswerState)> {
    snap.zones
        .iter()
        .filter(|(z, _)| qname.in_zone(z.name()))
        .max_by_key(|(z, _)| z.name().label_count())
}

fn soa_record(zname: &ZoneName, zstate: &ZoneAnswerState) -> hickory_proto::rr::Record {
    RData::Soa(zstate.soa.clone()).to_hickory_record(
        zname.name(),
        zstate.soa.ttl,
        zstate.emitted_serial.0,
    )
}

fn add_soa_authority(resp: &mut Message, zname: &ZoneName, zstate: &ZoneAnswerState) {
    resp.add_name_server(soa_record(zname, zstate));
}

/// Add A/AAAA glue for an in-zone MX exchange / SRV target.
fn add_glue(resp: &mut Message, snap: &ZoneSnapshot, target: &Name) {
    let Some((zname, zstate)) = match_zone(snap, target) else {
        return; // out-of-zone target → no glue
    };
    for rt in [RecordType::A, RecordType::Aaaa] {
        let key = RrsetKey {
            zone: zname.clone(),
            name: target.clone(),
            rr_type: rt,
        };
        if let Some(rrset) = zstate.rrsets.get(&key) {
            for rd in &rrset.rdatas {
                resp.add_additional(rd.to_hickory_record(target, rrset.ttl, 0));
            }
        }
    }
}
