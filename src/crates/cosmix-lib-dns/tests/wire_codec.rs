//! §9 tier-4: codec round-trip for every `RecordType`, wire/TC/EDNS0,
//! header hygiene, and the single mandatory ANY assertion at the codec
//! edge (no synthetic RR appears for ANY).

mod common;

use common::{ZONES_SRC, fresh_snapshot, query_msg};
use cosmix_dns::{Name, RData, decode, encode_tcp, encode_udp, resolve};
use hickory_proto::op::{Message, MessageType, ResponseCode};
use hickory_proto::rr::{RData as HRData, RecordType as HRecordType};
use std::net::{Ipv4Addr, Ipv6Addr};

fn n(s: &str) -> Name {
    Name::parse(s).unwrap()
}

/// One record of every modelled `RData` variant (SOA excluded — it is
/// config, emitted only via the resolver's apex/authority path).
fn one_of_each() -> Vec<RData> {
    vec![
        RData::A(Ipv4Addr::new(192, 0, 2, 5)),
        RData::Aaaa("fd00::5".parse::<Ipv6Addr>().unwrap()),
        RData::Ns(n("gw.bus")),
        RData::Mx {
            pref: 10,
            exch: n("alpha.bus"),
        },
        RData::Srv {
            prio: 0,
            weight: 1,
            port: 993,
            target: n("alpha.bus"),
        },
        RData::Txt(vec!["v=spf1 -all".to_string()]),
        RData::Ptr(n("alpha.bus")),
    ]
}

fn response_with(records: &[RData]) -> Message {
    let mut m = Message::new();
    m.set_id(0x2a2a);
    m.set_message_type(MessageType::Response);
    for (i, rd) in records.iter().enumerate() {
        let name = n(&format!("r{i}.bus"));
        m.add_answer(rd.to_hickory_record(&name, 300, 7));
    }
    m
}

// ── codec round-trip for every RecordType ────────────────────────────

#[test]
fn tcp_round_trip_preserves_every_record_type_and_prefix() {
    let msg = response_with(&one_of_each());
    let out = encode_tcp(&msg);

    // The 2-byte big-endian length prefix is produced exactly once
    // here; `decode` parses the bare (prefix-stripped) body.
    let prefix = u16::from_be_bytes([out[0], out[1]]) as usize;
    let body = &out[2..];
    assert_eq!(prefix, body.len(), "TCP length prefix == body length");

    let back = decode(body).expect("decode");
    let got: Vec<HRecordType> = back.answers().iter().map(|r| r.record_type()).collect();
    assert_eq!(
        got,
        vec![
            HRecordType::A,
            HRecordType::AAAA,
            HRecordType::NS,
            HRecordType::MX,
            HRecordType::SRV,
            HRecordType::TXT,
            HRecordType::PTR,
        ]
    );
}

#[test]
fn udp_round_trip_within_budget_is_lossless() {
    let msg = response_with(&one_of_each());
    let back = decode(&encode_udp(&msg, 1232)).expect("decode");
    assert!(!back.truncated(), "fits in 1232 → not truncated");
    assert_eq!(back.answers().len(), 7);
}

// ── long TXT (RSA DKIM) splits into <=255-byte character-strings ──────

#[test]
fn long_txt_chunks_into_255_byte_strings_and_reassembles() {
    // One operator-written string longer than a single 255-byte DNS
    // character-string — an RSA-2048 DKIM public key is ~390 bytes.
    // hickory's encoder rejects a >255-byte character-string (it
    // surfaced as SERVFAIL on the live mesh), so `to_hickory_record`
    // splits it; the split must round-trip and reassemble byte-exact.
    let long = format!("v=DKIM1; k=rsa; p={}", "A".repeat(370));
    assert!(long.len() > 255 && long.len() < 512);

    let mut m = Message::new();
    m.set_id(0x2a2a);
    m.set_message_type(MessageType::Response);
    m.add_answer(RData::Txt(vec![long.clone()]).to_hickory_record(&n("dkim.bus"), 300, 0));

    let out = encode_tcp(&m);
    let back = decode(&out[2..]).expect("decode");
    let HRData::TXT(txt) = back.answers()[0].data() else {
        panic!("expected TXT");
    };
    // On the wire it is >=2 character-strings, each <=255 bytes...
    assert!(
        txt.txt_data().len() >= 2,
        "long TXT split into >=2 character-strings"
    );
    assert!(
        txt.txt_data().iter().all(|cs| cs.len() <= 255),
        "every character-string is <=255 bytes"
    );
    // ...that reassemble to the exact original operator string.
    let joined: Vec<u8> = txt
        .txt_data()
        .iter()
        .flat_map(|c| c.iter().copied())
        .collect();
    assert_eq!(String::from_utf8_lossy(&joined), long);
}

// ── TC / EDNS0 negotiation ───────────────────────────────────────────

fn big_response() -> Message {
    // ~10 long TXT records → well over 512 and over 1232.
    let recs: Vec<RData> = (0..10).map(|_| RData::Txt(vec!["x".repeat(200)])).collect();
    response_with(&recs)
}

#[test]
fn oversized_without_opt_sets_tc_and_drops_answers() {
    let big = big_response();
    let bytes = encode_udp(&big, 512); // bare-DNS 512, no OPT
    let back = decode(&bytes).expect("decode");
    assert!(back.truncated(), "TC=1 when it cannot fit");
    assert!(back.answers().is_empty(), "ANSWER dropped under truncation");
    assert_eq!(back.id(), 0x2a2a, "header stays well-formed (id preserved)");
}

#[test]
fn with_opt_the_opt_is_echoed_and_negotiated_size_honoured() {
    let (snap, _p) = fresh_snapshot(ZONES_SRC);
    // Query carrying an OPT advertising 1232.
    let req = query_msg("alpha.bus", HRecordType::A, Some(1232));
    let resp = resolve(&snap, &req);
    assert!(resp.extensions().is_some(), "resolver echoes an OPT");
    assert_eq!(
        resp.extensions().as_ref().unwrap().max_payload(),
        1232,
        "advertised payload negotiated"
    );
    // Small response within the negotiated size → not truncated, OPT
    // survives the codec.
    let back = decode(&encode_udp(&resp, 1232)).expect("decode");
    assert!(!back.truncated());
    assert!(
        back.extensions().is_some(),
        "OPT preserved through encode_udp"
    );
    assert!(!back.answers().is_empty());
}

#[test]
fn tcp_never_truncates_even_when_huge() {
    let big = big_response();
    let out = encode_tcp(&big);
    let back = decode(&out[2..]).expect("decode");
    assert!(!back.truncated(), "TCP path never sets TC");
    assert_eq!(back.answers().len(), 10, "all records survive on TCP");
}

#[test]
fn tcp_frame_prefix_always_matches_payload_even_for_a_huge_message() {
    // The anti-desync invariant (Codex MAJOR): the 2-byte length
    // prefix MUST always equal the byte count that follows, so a
    // client's stream framer never reads into the next message. For a
    // logically huge response hickory's own serializer self-caps at
    // ≤65535 (sets TC, drops records), so the explicit >65535
    // SERVFAIL fallback in encode_tcp is unreachable through
    // `serialize` and purely defensive — but the prefix==payload
    // guarantee it protects is exactly what this asserts, and it would
    // fail under the prior `unwrap_or(u16::MAX)` + append-full-body.
    let recs: Vec<RData> = (0..500)
        .map(|_| RData::Txt(vec!["z".repeat(250)]))
        .collect();
    let huge = response_with(&recs);
    let out = encode_tcp(&huge);

    let prefix = u16::from_be_bytes([out[0], out[1]]) as usize;
    assert_eq!(prefix, out.len() - 2, "TCP prefix == actual body length");
    let back = decode(&out[2..]).expect("framed body decodes cleanly");
    assert_eq!(
        back.id(),
        0x2a2a,
        "id preserved so the client can correlate"
    );
    assert_ne!(
        back.response_code(),
        ResponseCode::FormErr,
        "frame is a well-formed message, never a torn one"
    );
}

// ── single mandatory ANY assertion at the codec edge ─────────────────

#[test]
fn any_response_carries_no_synthetic_rr_through_the_codec() {
    let (snap, _p) = fresh_snapshot(ZONES_SRC);
    let resp = resolve(&snap, &query_msg("alpha.bus", HRecordType::ANY, None));

    for bytes in [encode_udp(&resp, 1232), {
        let t = encode_tcp(&resp);
        t[2..].to_vec()
    }] {
        let back = decode(&bytes).expect("decode");
        assert!(
            back.answers().is_empty(),
            "ANY: no answer survives the codec"
        );
        assert!(back.name_servers().is_empty(), "ANY: no synthetic SOA");
        assert!(
            back.additionals()
                .iter()
                .all(|r| r.record_type() == HRecordType::OPT),
            "ANY: no synthetic RR (incl. no HINFO) at the codec edge"
        );
        let all_recs = back
            .answers()
            .iter()
            .chain(back.name_servers())
            .chain(back.additionals());
        assert!(
            all_recs
                .into_iter()
                .all(|r| r.record_type() != HRecordType::HINFO),
            "HINFO never constructed in any section"
        );
    }
}
