//! §9 tier-3: resolver / rcodes. **NODATA-vs-NXDOMAIN+SOA first** —
//! the exact 2026-05-17 live-mesh failure class this daemon exists to
//! fix — then positive answers, apex SOA/NS, glue, 0x20, PTR, header
//! hygiene, the single mandatory ANY outcome, and snapshot purity.

mod common;

use common::{ZONES_SRC, fresh_snapshot, query_msg, query_msg_class, soa_serial};
use cosmix_dns::{Name, RecordType, ZoneName, ZoneSnapshot, resolve};
use hickory_proto::op::{Message, OpCode, ResponseCode};
use hickory_proto::rr::{DNSClass, RData as HRData, RecordType as HRecordType};

fn emitted(snap: &ZoneSnapshot, zone: &str) -> u32 {
    snap.zones[&ZoneName(Name::parse(zone).unwrap())]
        .emitted_serial
        .0
}

// ── NODATA vs NXDOMAIN (+SOA AUTHORITY carrying emitted_serial) ───────

#[test]
fn nodata_vs_nxdomain_each_with_soa_authority_serial() {
    let (snap, _p) = fresh_snapshot(ZONES_SRC);
    let want = emitted(&snap, "bus");

    // NODATA: existing name (alpha.bus has A/AAAA/MX/TXT), absent
    // type (no SRV at the apex name) → NOERROR, empty ANSWER, SOA in
    // AUTHORITY, AA=1, SOA SERIAL == emitted_serial.
    let r = resolve(&snap, &query_msg("alpha.bus", HRecordType::SRV, None));
    assert_eq!(
        r.response_code(),
        ResponseCode::NoError,
        "NODATA is NOERROR"
    );
    assert!(r.answers().is_empty(), "NODATA has empty ANSWER");
    assert!(r.authoritative(), "AA=1");
    assert_eq!(
        soa_serial(r.name_servers()),
        Some(want),
        "NODATA SOA == emitted"
    );

    // NXDOMAIN: absent name → RCODE 3, SOA in AUTHORITY, AA=1, SERIAL
    // == emitted_serial.
    let r = resolve(&snap, &query_msg("nope.bus", HRecordType::A, None));
    assert_eq!(r.response_code(), ResponseCode::NXDomain);
    assert!(r.answers().is_empty());
    assert!(r.authoritative(), "AA=1");
    assert_eq!(
        soa_serial(r.name_servers()),
        Some(want),
        "NXDOMAIN SOA == emitted"
    );
}

// ── RFC 4592 single-label wildcard synthesis ─────────────────────────

#[test]
fn wildcard_synthesizes_for_absent_name_with_owner_qname() {
    let (snap, _p) = fresh_snapshot(ZONES_SRC);
    // `*.wild.bus` A exists; `foo.wild.bus` is not a node → synthesize.
    let r = resolve(&snap, &query_msg("foo.wild.bus", HRecordType::A, None));
    assert_eq!(
        r.response_code(),
        ResponseCode::NoError,
        "wildcard hit is NOERROR"
    );
    assert!(r.authoritative(), "AA=1");
    assert_eq!(r.answers().len(), 1, "one synthesized A");
    // Owner is the QUERIED name, never the `*` owner (RFC 4592 §3.3.1).
    assert_eq!(r.answers()[0].name().to_string(), "foo.wild.bus.");
    assert!(matches!(r.answers()[0].data(), HRData::A(a) if a.0.to_string() == "192.0.2.99"));
}

#[test]
fn wildcard_does_not_cover_an_existing_node_other_type_is_nodata() {
    let (snap, _p) = fresh_snapshot(ZONES_SRC);
    // `host.wild.bus` exists (A). A TXT query is NODATA — an existing
    // node never falls through to the wildcard (RFC 4592 §2.2.2).
    let r = resolve(&snap, &query_msg("host.wild.bus", HRecordType::TXT, None));
    assert_eq!(
        r.response_code(),
        ResponseCode::NoError,
        "NODATA, not wildcard"
    );
    assert!(
        r.answers().is_empty(),
        "no synthesized answer for an existing node"
    );
    assert!(r.authoritative(), "AA=1");
    assert_eq!(soa_serial(r.name_servers()), Some(emitted(&snap, "bus")));
}

#[test]
fn wildcard_match_without_qtype_is_nodata_not_nxdomain() {
    let (snap, _p) = fresh_snapshot(ZONES_SRC);
    // `*.wild.bus` has only A. An AAAA query for an absent name matches
    // the wildcard NODE → the name exists via synthesis, so missing
    // AAAA is NODATA (NOERROR + SOA), NOT NXDOMAIN — else a resolver
    // would negatively cache the whole name and break its A (RFC 8020).
    let r = resolve(&snap, &query_msg("foo.wild.bus", HRecordType::AAAA, None));
    assert_eq!(
        r.response_code(),
        ResponseCode::NoError,
        "wildcard NODATA is NOERROR"
    );
    assert!(r.answers().is_empty());
    assert!(r.authoritative(), "AA=1");
    assert_eq!(soa_serial(r.name_servers()), Some(emitted(&snap, "bus")));
}

#[test]
fn wildcard_does_not_match_its_own_parent() {
    let (snap, _p) = fresh_snapshot(ZONES_SRC);
    // `wild.bus` is an empty non-terminal (it has children `host`,
    // `deep.sub`, `*`) → it EXISTS, so a missing A is NODATA, and it is
    // never synthesized from its own `*.wild.bus` child.
    let r = resolve(&snap, &query_msg("wild.bus", HRecordType::A, None));
    assert_eq!(
        r.response_code(),
        ResponseCode::NoError,
        "ENT is NODATA, not NXDOMAIN"
    );
    assert!(
        r.answers().is_empty(),
        "no synthesis for the wildcard's own parent"
    );
    assert!(r.authoritative(), "AA=1");
}

#[test]
fn empty_non_terminal_exists_and_blocks_the_wildcard() {
    let (snap, _p) = fresh_snapshot(ZONES_SRC);
    // `sub.wild.bus` is an empty non-terminal (parent of `deep.sub`).
    // It EXISTS → NODATA for an absent type, and crucially it is NOT
    // synthesized from `*.wild.bus` (an ENT is closer than the wildcard).
    let r = resolve(&snap, &query_msg("sub.wild.bus", HRecordType::A, None));
    assert_eq!(r.response_code(), ResponseCode::NoError, "ENT is NODATA");
    assert!(r.answers().is_empty(), "ENT not wildcard-synthesized");
    assert!(r.authoritative(), "AA=1");
}

#[test]
fn wildcard_synthesizes_for_a_multi_label_absent_name() {
    let (snap, _p) = fresh_snapshot(ZONES_SRC);
    // `a.b.wild.bus` — `b.wild.bus` does not exist, so the closest
    // encloser is `wild.bus` and `*.wild.bus` is the source of
    // synthesis (RFC 4592 closest-encloser, deeper than one label).
    let r = resolve(&snap, &query_msg("a.b.wild.bus", HRecordType::A, None));
    assert_eq!(r.response_code(), ResponseCode::NoError);
    assert!(r.authoritative(), "AA=1");
    assert_eq!(r.answers().len(), 1);
    assert_eq!(r.answers()[0].name().to_string(), "a.b.wild.bus.");
    assert!(matches!(r.answers()[0].data(), HRData::A(a) if a.0.to_string() == "192.0.2.99"));
}

#[test]
fn closest_encloser_without_wildcard_is_nxdomain() {
    let (snap, _p) = fresh_snapshot(ZONES_SRC);
    // `nope.bus`: closest encloser is the apex `bus`, which has NO
    // `*.bus` wildcard node → NXDOMAIN (wildcards do not cascade up).
    let r = resolve(&snap, &query_msg("nope.bus", HRecordType::A, None));
    assert_eq!(r.response_code(), ResponseCode::NXDomain);
    assert!(r.answers().is_empty());
}

#[test]
fn any_on_wildcard_covered_name_is_noerror_not_nxdomain() {
    let (snap, _p) = fresh_snapshot(ZONES_SRC);
    // `foo.wild.bus` is covered by `*.wild.bus`. ANY must give the
    // minimal NOERROR outcome (empty, AA=1) — NOT NXDOMAIN, which the
    // pre-wildcard ANY branch would have returned for an absent node.
    let r = resolve(&snap, &query_msg("foo.wild.bus", HRecordType::ANY, None));
    assert_eq!(
        r.response_code(),
        ResponseCode::NoError,
        "wildcard-covered ANY is NOERROR"
    );
    assert!(
        r.answers().is_empty(),
        "minimal ANY: no enumeration even via wildcard"
    );
    assert!(r.authoritative(), "AA=1");
}

#[test]
fn out_of_vocab_qtype_on_wildcard_covered_name_is_nodata() {
    let (snap, _p) = fresh_snapshot(ZONES_SRC);
    // CAA is not in the model vocab, so it skips the rrset block. A
    // wildcard-covered name must still be NODATA (NOERROR + SOA), never
    // NXDOMAIN — the name exists via `*.wild.bus` (RFC 8020 safety).
    let r = resolve(&snap, &query_msg("foo.wild.bus", HRecordType::CAA, None));
    assert_eq!(
        r.response_code(),
        ResponseCode::NoError,
        "covered out-of-vocab is NODATA"
    );
    assert!(r.answers().is_empty());
    assert!(r.authoritative(), "AA=1");
    assert_eq!(soa_serial(r.name_servers()), Some(emitted(&snap, "bus")));
}

// ── positive answers + multi-RDATA single RRset ──────────────────────

#[test]
fn positive_a_aaaa_txt() {
    let (snap, _p) = fresh_snapshot(ZONES_SRC);

    let r = resolve(&snap, &query_msg("alpha.bus", HRecordType::A, None));
    assert_eq!(r.response_code(), ResponseCode::NoError);
    assert!(r.authoritative());
    assert!(matches!(r.answers()[0].data(), HRData::A(a) if a.0.to_string() == "192.0.2.5"));

    let r = resolve(&snap, &query_msg("alpha.bus", HRecordType::AAAA, None));
    assert!(matches!(r.answers()[0].data(), HRData::AAAA(a) if a.0.to_string() == "fd00::5"));

    let r = resolve(&snap, &query_msg("alpha.bus", HRecordType::TXT, None));
    let HRData::TXT(txt) = r.answers()[0].data() else {
        panic!("expected TXT");
    };
    assert_eq!(String::from_utf8_lossy(&txt.txt_data()[0]), "v=spf1 -all");
}

#[test]
fn multi_mx_same_name_is_one_rrset_two_rdata() {
    let (snap, _p) = fresh_snapshot(ZONES_SRC);
    let r = resolve(&snap, &query_msg("alpha.bus", HRecordType::MX, None));
    assert_eq!(r.response_code(), ResponseCode::NoError);
    let mut prefs: Vec<u16> = r
        .answers()
        .iter()
        .filter_map(|rec| match rec.data() {
            HRData::MX(mx) => Some(mx.preference()),
            _ => None,
        })
        .collect();
    prefs.sort_unstable();
    assert_eq!(prefs, vec![10, 20], "both MX rdata in one RRset");
}

#[test]
fn positive_srv_and_ptr() {
    let (snap, _p) = fresh_snapshot(ZONES_SRC);

    let r = resolve(
        &snap,
        &query_msg("_imaps._tcp.alpha.bus", HRecordType::SRV, None),
    );
    let HRData::SRV(srv) = r.answers()[0].data() else {
        panic!("expected SRV");
    };
    assert_eq!((srv.priority(), srv.weight(), srv.port()), (0, 1, 993));
    assert_eq!(srv.target().to_ascii(), "alpha.bus.");

    // PTR sweep — all five mesh reverse names.
    for (rev, host) in [
        ("1.2.0.192.in-addr.arpa", "gw.bus."),
        ("5.2.0.192.in-addr.arpa", "alpha.bus."),
        ("4.2.0.192.in-addr.arpa", "gamma.bus."),
        ("210.2.0.192.in-addr.arpa", "delta.bus."),
        ("9.2.0.192.in-addr.arpa", "epsilon.bus."),
    ] {
        let r = resolve(&snap, &query_msg(rev, HRecordType::PTR, None));
        assert_eq!(r.response_code(), ResponseCode::NoError, "{rev}");
        let HRData::PTR(p) = r.answers()[0].data() else {
            panic!("{rev}: expected PTR");
        };
        assert_eq!(p.0.to_ascii(), host, "{rev}");
    }
}

// ── positive apex SOA / NS; apex never in rrsets; non-apex → NODATA ───

#[test]
fn positive_apex_soa_and_ns_for_every_zone() {
    let (snap, _p) = fresh_snapshot(ZONES_SRC);
    for zone in ["bus", "example.com", "2.0.192.in-addr.arpa"] {
        let want = emitted(&snap, zone);

        let r = resolve(&snap, &query_msg(zone, HRecordType::SOA, None));
        assert_eq!(r.response_code(), ResponseCode::NoError, "{zone} SOA");
        assert!(r.authoritative(), "{zone} SOA AA=1");
        assert_eq!(
            soa_serial(r.answers()),
            Some(want),
            "{zone} apex SOA == emitted"
        );

        let r = resolve(&snap, &query_msg(zone, HRecordType::NS, None));
        assert_eq!(r.response_code(), ResponseCode::NoError, "{zone} NS");
        assert!(
            r.answers().iter().any(|rec| matches!(
                rec.data(),
                HRData::NS(ns) if ns.0.to_ascii() == "gw.bus."
            )),
            "{zone} apex NS"
        );

        // Apex SOA/NS are answered from soa/ns fields, NEVER duplicated
        // into the flattened rrsets.
        let zs = &snap.zones[&ZoneName(Name::parse(zone).unwrap())];
        assert!(
            !zs.rrsets
                .keys()
                .any(|k| matches!(k.rr_type, RecordType::Soa | RecordType::Ns)),
            "{zone}: apex SOA/NS must not appear as rrsets entries"
        );
    }
}

#[test]
fn non_apex_soa_or_ns_is_ordinary_nodata_or_nxdomain() {
    let (snap, _p) = fresh_snapshot(ZONES_SRC);

    // Existing non-apex name, SOA type → NODATA (not a positive apex).
    let r = resolve(&snap, &query_msg("alpha.bus", HRecordType::SOA, None));
    assert_eq!(r.response_code(), ResponseCode::NoError);
    assert!(
        r.answers().is_empty(),
        "non-apex SOA is NODATA, not an answer"
    );
    assert!(soa_serial(r.name_servers()).is_some(), "SOA in AUTHORITY");

    // Absent non-apex name, NS type → NXDOMAIN.
    let r = resolve(&snap, &query_msg("nope.bus", HRecordType::NS, None));
    assert_eq!(r.response_code(), ResponseCode::NXDomain);
}

// ── REFUSED out-of-zone; in-zone never REFUSED after a bad reload ─────

#[test]
fn out_of_zone_is_refused_aa0() {
    let (snap, _p) = fresh_snapshot(ZONES_SRC);
    // Use `notazone.invalid` (RFC 2606 reserved test TLD) so this stays
    // out-of-zone regardless of which placeholder zones the fixture happens
    // to publish.
    let r = resolve(&snap, &query_msg("notazone.invalid", HRecordType::A, None));
    assert_eq!(r.response_code(), ResponseCode::Refused);
    assert!(!r.authoritative(), "REFUSED carries AA=0");
}

#[test]
fn in_zone_never_refused_even_after_a_bad_reload() {
    use cosmix_dns::{FilePersistence, StateLoad, StaticZoneStore, ZoneStore};
    let dir = tempfile::tempdir().unwrap();
    let zpath = dir.path().join("zones.mix");
    std::fs::write(&zpath, ZONES_SRC).unwrap();
    let StateLoad::Ok(p) = FilePersistence::open(&dir.path().join("state")) else {
        panic!("fresh state Ok");
    };
    let store = StaticZoneStore::load_initial(zpath.clone(), Box::new(p)).expect("load_initial");

    // Cross-owner collision on reload → rejected, last-good retained.
    std::fs::write(
        &zpath,
        r#"zones: {
  "bus": {
    soa: { primary: "gw.bus", mbox: "hostmaster.bus", ttl: 300, minimum: 60 },
    ns: [ "gw.bus" ], serial_floor: 1,
    bundles: [
      { owner: "a", serial: 9, records: [ { name: "c.bus", type: "A", ttl: 300, data: "1.1.1.1" } ] },
      { owner: "b", serial: 9, records: [ { name: "c.bus", type: "A", ttl: 300, data: "2.2.2.2" } ] }
    ] } }
"#,
    )
    .unwrap();
    assert!(store.reload().is_err(), "colliding reload must be rejected");

    // In-zone query still resolves against last-known-good (NOT REFUSED).
    let r = resolve(
        &store.snapshot(),
        &query_msg("alpha.bus", HRecordType::A, None),
    );
    assert_eq!(r.response_code(), ResponseCode::NoError);
    assert!(
        !r.answers().is_empty(),
        "in-zone still answered post-bad-reload"
    );
}

// ── glue: in-zone MX/SRV target → ADDITIONAL; out-of-zone → none ──────

#[test]
fn in_zone_mx_glue_present_out_of_zone_absent() {
    let (snap, _p) = fresh_snapshot(ZONES_SRC);

    // alpha.bus MX {10 alpha.bus, 20 gamma.bus}: both targets in
    // zone → A/AAAA glue in ADDITIONAL.
    let r = resolve(&snap, &query_msg("alpha.bus", HRecordType::MX, None));
    let glue_names: Vec<String> = r
        .additionals()
        .iter()
        .map(|rec| rec.name().to_ascii())
        .collect();
    assert!(glue_names.iter().any(|n| n == "alpha.bus."), "self glue");
    assert!(glue_names.iter().any(|n| n == "gamma.bus."), "gamma glue");

    // ext.bus MX 10 mail.example.net: target out of zone → no glue.
    let r = resolve(&snap, &query_msg("ext.bus", HRecordType::MX, None));
    assert_eq!(r.response_code(), ResponseCode::NoError);
    assert!(r.additionals().is_empty(), "out-of-zone target → no glue");
}

// ── 0x20: QUESTION echoed verbatim, answer canonical ─────────────────

#[test]
fn zero_x_twenty_question_verbatim_answer_canonical() {
    let (snap, _p) = fresh_snapshot(ZONES_SRC);
    let req = query_msg("Alpha.BUS", HRecordType::A, None);
    let r = resolve(&snap, &req);

    // QUESTION echoed verbatim — byte-for-byte identical to what the
    // client sent (case preserved; no normalisation, incl. no forced
    // trailing dot).
    assert_eq!(
        r.queries()[0].name().to_ascii(),
        req.queries()[0].name().to_ascii(),
        "0x20: question is echoed verbatim"
    );
    assert_eq!(r.queries()[0].name().to_ascii(), "Alpha.BUS");
    // Answer name is canonical lower-case (built from the snapshot, not
    // echoed) → absolute, dotted.
    assert_eq!(r.answers()[0].name().to_ascii(), "alpha.bus.");
    // Alpha.Bus ≡ alpha.bus. — identical rdata.
    let lo = resolve(&snap, &query_msg("alpha.bus", HRecordType::A, None));
    assert_eq!(
        format!("{:?}", r.answers()[0].data()),
        format!("{:?}", lo.answers()[0].data())
    );
}

// ── header hygiene + the single mandatory ANY outcome ────────────────

#[test]
fn opcode_not_query_is_notimp() {
    let (snap, _p) = fresh_snapshot(ZONES_SRC);
    let mut req = query_msg("alpha.bus", HRecordType::A, None);
    req.set_op_code(OpCode::Status);
    assert_eq!(resolve(&snap, &req).response_code(), ResponseCode::NotImp);
}

#[test]
fn qdcount_not_one_is_formerr() {
    let (snap, _p) = fresh_snapshot(ZONES_SRC);
    let mut zero = Message::new();
    zero.set_message_type(hickory_proto::op::MessageType::Query);
    assert_eq!(resolve(&snap, &zero).response_code(), ResponseCode::FormErr);

    let mut two = query_msg("alpha.bus", HRecordType::A, None);
    two.add_query(hickory_proto::op::Query::query(
        hickory_proto::rr::Name::from_ascii("gamma.bus.").unwrap(),
        HRecordType::A,
    ));
    assert_eq!(resolve(&snap, &two).response_code(), ResponseCode::FormErr);
}

#[test]
fn bad_class_is_refused() {
    let (snap, _p) = fresh_snapshot(ZONES_SRC);
    let req = query_msg_class("alpha.bus", HRecordType::A, DNSClass::CH, None);
    assert_eq!(resolve(&snap, &req).response_code(), ResponseCode::Refused);
}

#[test]
fn any_is_the_single_mandatory_minimal_outcome() {
    let (snap, _p) = fresh_snapshot(ZONES_SRC);

    // Existing name: NOERROR, AA=1, ANCOUNT=0, NO SOA-as-answer, NO
    // synthetic RR, NO multi-RRset enumeration (alpha.bus has
    // A/AAAA/MX/TXT — none may appear).
    let r = resolve(&snap, &query_msg("alpha.bus", HRecordType::ANY, None));
    assert_eq!(r.response_code(), ResponseCode::NoError);
    assert!(r.authoritative(), "AA=1");
    assert!(r.answers().is_empty(), "ANCOUNT=0 — no RRset enumeration");
    assert!(
        r.name_servers().is_empty(),
        "no SOA-as-answer/authority for existing-name ANY"
    );
    assert!(
        r.additionals()
            .iter()
            .all(|rec| rec.record_type() == HRecordType::OPT),
        "no synthetic RR for ANY"
    );

    // Absent name still NXDOMAIN (+SOA AUTHORITY).
    let r = resolve(&snap, &query_msg("nope.bus", HRecordType::ANY, None));
    assert_eq!(r.response_code(), ResponseCode::NXDomain);
    assert!(soa_serial(r.name_servers()).is_some());
}

#[test]
fn hinfo_is_never_emitted_for_any_query_type() {
    let (snap, _p) = fresh_snapshot(ZONES_SRC);
    for qt in [HRecordType::HINFO, HRecordType::ANY, HRecordType::A] {
        let r = resolve(&snap, &query_msg("alpha.bus", qt, None));
        for sec in [r.answers(), r.name_servers(), r.additionals()] {
            assert!(
                sec.iter()
                    .all(|rec| rec.record_type() != HRecordType::HINFO),
                "HINFO must never appear (qtype {qt:?})"
            );
        }
    }
}

// ── snapshot purity (compile-enforced signature + determinism) ───────

#[test]
fn resolver_signature_is_exactly_snapshot_and_message() {
    // Compile-time guard: no owner/store/persistence/Transport/size in
    // the resolver path. If the signature ever widens this stops
    // compiling (anti-pattern (b) tripwire).
    let _f: fn(&ZoneSnapshot, &Message) -> Message = resolve;
}

#[test]
fn resolve_is_deterministic_given_snap_and_req() {
    let (snap, _p) = fresh_snapshot(ZONES_SRC);
    let req = query_msg("alpha.bus", HRecordType::MX, None);
    let a = resolve(&snap, &req);
    let b = resolve(&snap, &req);
    assert_eq!(
        cosmix_dns::encode_tcp(&a),
        cosmix_dns::encode_tcp(&b),
        "pure: same (snap,req) → byte-identical response"
    );
}
