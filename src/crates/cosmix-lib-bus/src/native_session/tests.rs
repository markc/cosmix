use super::*;
use crate::bus::{self, BusMessage};

fn principal() -> BrokerPrincipal {
    BrokerPrincipal {
        version: PrincipalVersion::V1,
        assurance: Assurance::LocalUnix,
        owner_node: "node-a".into(),
        unix_uid: 1000,
        unix_gid: 1000,
        peer_pid: 42,
        broker_epoch: HexBytes([1; 16]),
        connection_id: HexBytes([2; 16]),
        session: None,
    }
}

#[test]
fn principal_strip_then_stamp_all_ascii_case_forgery_vectors() {
    for key in [
        "broker_principal",
        "BROKER_PRINCIPAL",
        "Broker_Principal",
        "bRoKeR_pRiNcIpAl",
    ] {
        let mut msg = BusMessage::new()
            .with_header(key, "forged")
            .with_header("from", "legacy");
        stamp_principal(&mut msg, Some(&principal())).unwrap();
        assert_eq!(read_principal(&msg).unwrap(), Some(principal()));
        assert_eq!(msg.headers.len(), 2);
        assert_eq!(msg.from_addr(), Some("legacy"));
        msg.set("BROKER_PRINCIPAL", "forged second copy");
        assert!(read_principal(&msg).is_err());
        stamp_principal(&mut msg, None).unwrap();
        assert_eq!(read_principal(&msg).unwrap(), None);
        assert_eq!(msg.headers.len(), 1);
    }
}

#[test]
fn principal_failed_stamp_removes_forgery() {
    let mut p = principal();
    p.assurance = Assurance::SessionBound;
    let mut msg = BusMessage::new().with_header("BROKER_PRINCIPAL", "forged");
    assert!(stamp_principal(&mut msg, Some(&p)).is_err());
    assert_eq!(read_principal(&msg).unwrap(), None);
    p.assurance = Assurance::LocalUnix;
    p.owner_node = "x".repeat(MAX_PRINCIPAL_BYTES);
    assert!(stamp_principal(&mut msg, Some(&p)).is_err());
    assert!(msg.headers.is_empty());
}

#[test]
fn principal_known_fields_strict_unknown_fields_allowed_null_required() {
    let good = serde_json::to_value(principal()).unwrap();
    let mut extended = good.clone();
    extended["future"] = serde_json::json!({"anything": true});
    let msg = BusMessage::new().with_header(PRINCIPAL_HEADER, &extended.to_string());
    assert_eq!(read_principal(&msg).unwrap(), Some(principal()));
    for (field, bad) in [
        ("version", serde_json::json!(2)),
        ("version", serde_json::json!("1")),
        ("unix_uid", serde_json::json!(-1)),
        ("unix_gid", serde_json::json!(4294967296u64)),
        ("peer_pid", serde_json::json!(1.0)),
        ("unix_uid", serde_json::json!("1000")),
        ("broker_epoch", serde_json::json!("AB".repeat(16))),
        ("assurance", serde_json::json!("reserved")),
    ] {
        let mut value = good.clone();
        value[field] = bad;
        assert!(
            serde_json::from_value::<BrokerPrincipal>(value).is_err(),
            "{field}"
        );
    }
    let mut missing = good;
    missing.as_object_mut().unwrap().remove("session");
    assert!(serde_json::from_value::<BrokerPrincipal>(missing).is_err());
    let raw = serde_json::to_string(&principal()).unwrap().replacen(
        "\"version\":1",
        "\"version\":1,\"version\":1",
        1,
    );
    assert!(read_principal(&BusMessage::new().with_header(PRINCIPAL_HEADER, &raw)).is_err());
}

#[test]
fn bound_principal_scope_and_decimal_types() {
    let mut p = principal();
    p.assurance = Assurance::SessionBound;
    p.session = Some(SessionIdentity {
        record_id: HexBytes([3; 16]),
        instance_id: HexBytes([4; 16]),
        incarnation: HexBytes([5; 16]),
        role: Role::PaneShell,
        parent_instance: Some(HexBytes([6; 16])),
        parent_incarnation: Some(HexBytes([7; 16])),
        pane_id: Some(DecimalU64(0)),
        pane_generation: Some(DecimalU64(1)),
        binding_generation: DecimalU64(1),
        capabilities: vec![Capability::Execute, Capability::Input],
        lease_remaining_ms: DecimalU64(10000),
    });
    let mut msg = BusMessage::new();
    stamp_principal(&mut msg, Some(&p)).unwrap();
    assert_eq!(read_principal(&msg).unwrap(), Some(p.clone()));
    let mut value = serde_json::to_value(&p).unwrap();
    value["session"]
        .as_object_mut()
        .unwrap()
        .remove("parent_instance");
    assert!(serde_json::from_value::<BrokerPrincipal>(value).is_err());
    for bad in [
        serde_json::json!(1),
        serde_json::json!("01"),
        serde_json::json!("+1"),
        serde_json::json!("18446744073709551616"),
    ] {
        let mut value = serde_json::to_value(&p).unwrap();
        value["session"]["binding_generation"] = bad;
        assert!(serde_json::from_value::<BrokerPrincipal>(value).is_err());
    }
    p.session.as_mut().unwrap().role = Role::Term;
    assert!(p.validate().is_err());
    assert!(serde_json::from_str::<HexBytes<16>>("\"\"").is_err());
    assert_eq!(
        serde_json::from_str::<DecimalU64>("\"18446744073709551615\"")
            .unwrap()
            .0,
        u64::MAX
    );
}

fn request(command: &str, body: &str) -> String {
    format!(
        "---\nbus: 1\ntype: request\nto: noded\nid: 1\nnative-session: 1\ncommand: noded.session.{command}\n---\n{body}"
    )
}

#[test]
fn bootstrap_duplicate_evidence_preserved_at_all_depths() {
    let good = request("hello", "{}");
    assert!(parse_bootstrap(good.as_bytes()).is_ok());
    for extra in ["id: 2\n", "ID: 1\n", "Native-Session: 1\n"] {
        let raw = good.replacen("bus: 1\n", &format!("bus: 1\n{extra}"), 1);
        assert!(parse_bootstrap(raw.as_bytes()).is_err());
        assert!(bus::parse(&raw).is_ok(), "legacy parser remains permissive");
    }
    for json in [
        r#"{"a":1,"a":2}"#,
        r#"{"a":[{"b":1,"\u0062":2}]}"#,
        r#"{"a":{"b":{"c":1,"c":2}}}"#,
    ] {
        assert!(validate_json(json.as_bytes()).is_err());
        assert!(serde_json::from_str::<serde_json::Value>(json).is_ok());
    }
    assert!(validate_json(br#"{"a":{"x":1},"b":{"x":2}}"#).is_ok());
}

#[test]
fn bootstrap_envelope_and_resource_bounds() {
    let good = request("hello", "{}");
    for raw in [
        good.replacen("---\n", "", 1),
        good.replace("\n---\n", "\n"),
        good.replace("bus: 1", "bus: 2"),
        good.replace("type: request", "type: response"),
        good.replace("to: noded", "to: term"),
        good.replace("id: 1", "id: a b"),
        good.replace("id: 1", &format!("id: {}", "a".repeat(129))),
        good.replace("id: 1", "id: "),
        good.replace("id: 1", "id: é"),
        good.replace("bus: 1", "bus 1"),
        good.replace("bus: 1", "bus: 1\nunknown: yes"),
        request("hello", "[]"),
        request("hello", "{} {}"),
        request("hello", "{\"extra\":1}"),
        request("notice.ack", "{}"),
        request("lifecycle", "{}"),
        request("lifecycle.gap", "{}"),
    ] {
        assert!(parse_bootstrap(raw.as_bytes()).is_err(), "{raw}");
    }
    let mut bytes = good.as_bytes().to_vec();
    bytes.push(0xff);
    assert!(parse_bootstrap(&bytes).is_err());
    let exact = format!("{good}{}", " ".repeat(MAX_BOOTSTRAP_BYTES - good.len()));
    assert!(parse_bootstrap(exact.as_bytes()).is_ok());
    assert!(parse_bootstrap(format!("{exact} ").as_bytes()).is_err());
    let depth16 = format!("{}0{}", "[".repeat(16), "]".repeat(16));
    assert!(validate_json(depth16.as_bytes()).is_ok());
    assert!(validate_json(format!("[{depth16}]").as_bytes()).is_err());
    // Excess headers cannot hide behind case-variant principal stripping.
    let mut headers = String::new();
    for mask in 0..27 {
        let key: String = PRINCIPAL_HEADER
            .chars()
            .enumerate()
            .map(|(i, c)| {
                if mask & (1 << i) != 0 {
                    c.to_ascii_uppercase()
                } else {
                    c
                }
            })
            .collect();
        headers.push_str(&format!("{key}: forged\n"));
    }
    assert!(parse_bootstrap(good.replace("bus: 1\n", &headers).as_bytes()).is_err());
    let forged = good.replace("bus: 1\n", "bus: 1\nBrOkEr_PrInCiPaL: forged\n");
    assert_eq!(
        read_principal(&parse_bootstrap(forged.as_bytes()).unwrap().message).unwrap(),
        None
    );
}

#[test]
fn bootstrap_command_schemas_and_required_nulls() {
    let key = "12".repeat(32);
    let sig = "34".repeat(64);
    let id = "56".repeat(16);
    let allocation = format!(r#"{{"public_key":"{key}","signature":"{sig}"}}"#);
    assert!(matches!(
        parse_bootstrap(request("allocate", &allocation).as_bytes())
            .unwrap()
            .command,
        SessionCommand::Allocate(AllocateArgs {
            policy: Policy::DefaultOpen,
            ..
        })
    ));
    for invalid_id in ["0", "01", "+1", "abc", "18446744073709551616"] {
        assert!(
            parse_bootstrap(
                request("allocate", &allocation)
                    .replace("id: 1\n", &format!("id: {invalid_id}\n"))
                    .as_bytes()
            )
            .is_err()
        );
    }
    for body in [
        format!(r#"{{"public_key":"{key}","purpose":"enrol"}}"#),
        format!(
            r#"{{"record_id":"{id}","incarnation":"{id}","purpose":"resume","grant_id":null}}"#
        ),
        format!(
            r#"{{"record_id":"{id}","incarnation":"{id}","purpose":"enrol","grant_id":"{id}"}}"#
        ),
    ] {
        assert!(parse_bootstrap(request("challenge", &body).as_bytes()).is_ok());
    }
    for body in [
        format!(r#"{{"public_key":"{key}","purpose":"resume"}}"#),
        format!(r#"{{"record_id":"{id}","incarnation":"{id}","purpose":"resume"}}"#),
        format!(r#"{{"record_id":"{id}","incarnation":"{id}","purpose":"enrol","grant_id":null}}"#),
        format!(r#"{{"public_key":"{key}","purpose":"enrol","grant_id":null}}"#),
    ] {
        assert!(parse_bootstrap(request("challenge", &body).as_bytes()).is_err());
    }
    let reference =
        format!(r#"{{"record_id":"{id}","incarnation":"{id}","binding_generation":"1"}}"#);
    for command in ["renew", "revoke", "lease.check"] {
        let valid = request(command, &format!(r#"{{"target":{reference}}}"#));
        assert!(parse_bootstrap(valid.as_bytes()).is_ok());
        assert!(
            parse_bootstrap(
                valid
                    .replace("\"binding_generation\":\"1\"", "\"binding_generation\":1")
                    .as_bytes()
            )
            .is_err()
        );
    }
    assert!(
        parse_bootstrap(
            request(
                "prove",
                &format!(r#"{{"challenge_id":"{id}","signature":"{sig}"}}"#)
            )
            .as_bytes()
        )
        .is_ok()
    );
    assert!(
        parse_bootstrap(request("grant.fetch", &format!(r#"{{"public_key":"{key}"}}"#)).as_bytes())
            .is_ok()
    );
    assert!(parse_bootstrap(request("list", "{}").as_bytes()).is_ok());
    let grant = format!(
        r#"{{"parent":{reference},"pane_id":"0","pane_generation":"1","public_key":"{key}","role":"pane-shell","capabilities":["input"]}}"#
    );
    assert!(parse_bootstrap(request("grant.create", &grant).as_bytes()).is_ok());
    for bad in [
        grant.replace("pane-shell", "term"),
        grant.replace("[\"input\"]", "[]"),
        grant.replace("[\"input\"]", "[\"input\",\"input\"]"),
        grant.replace("\"pane_generation\":\"1\"", "\"pane_generation\":\"0\""),
    ] {
        assert!(parse_bootstrap(request("grant.create", &bad).as_bytes()).is_err());
    }
}

fn proof(child: bool, purpose: Purpose) -> ProofTranscript {
    ProofTranscript {
        purpose,
        broker_epoch: HexBytes([1; 16]),
        connection_id: HexBytes([2; 16]),
        challenge_id: HexBytes([3; 16]),
        nonce: HexBytes([4; 32]),
        grant_id: (purpose == Purpose::Enrol).then_some(HexBytes([5; 16])),
        record_id: HexBytes([6; 16]),
        instance_id: HexBytes([7; 16]),
        incarnation: HexBytes([8; 16]),
        unix_uid: 0x01020304,
        parent_instance: child.then_some(HexBytes([9; 16])),
        parent_incarnation: child.then_some(HexBytes([10; 16])),
        parent_key_hash: child.then_some(HexBytes([11; 32])),
        pane_id: child.then_some(DecimalU64(0x0102030405060708)),
        pane_generation: child.then_some(DecimalU64(9)),
        role: if child { Role::PaneShell } else { Role::Term },
        public_key_hash: HexBytes([12; 32]),
        capabilities_hash: HexBytes([13; 32]),
        binding_generation: DecimalU64(if purpose == Purpose::Enrol { 1 } else { 2 }),
        grant_expires_ms: (purpose == Purpose::Enrol).then_some(DecimalU64(0x1112131415161718)),
        challenge_expires_ms: DecimalU64(0x2122232425262728),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn proof_enrol_present_fields_golden_vector() {
    let expected = concat!(
        "636f736d69782e6e61746976652d73657373696f6e2e70726f6f6600000101",
        "01010101010101010101010101010101",
        "02020202020202020202020202020202",
        "03030303030303030303030303030303",
        "0404040404040404040404040404040404040404040404040404040404040404",
        "0105050505050505050505050505050505",
        "06060606060606060606060606060606",
        "07070707070707070707070707070707",
        "08080808080808080808080808080808",
        "01020304",
        "0109090909090909090909090909090909",
        "010a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a",
        "010b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b",
        "010102030405060708",
        "010000000000000009",
        "000a70616e652d7368656c6c",
        "0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c",
        "0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d",
        "0000000000000001",
        "011112131415161718",
        "2122232425262728"
    );
    assert_eq!(
        hex(&encode_proof(&proof(true, Purpose::Enrol)).unwrap()),
        expected
    );
}

#[test]
fn proof_resume_null_fields_golden_vector() {
    let expected = concat!(
        "636f736d69782e6e61746976652d73657373696f6e2e70726f6f6600000102",
        "01010101010101010101010101010101",
        "02020202020202020202020202020202",
        "03030303030303030303030303030303",
        "0404040404040404040404040404040404040404040404040404040404040404",
        "00",
        "06060606060606060606060606060606",
        "07070707070707070707070707070707",
        "08080808080808080808080808080808",
        "01020304",
        "0000000000",
        "00047465726d",
        "0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c",
        "0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d",
        "0000000000000002",
        "00",
        "2122232425262728"
    );
    let p = proof(false, Purpose::Resume);
    assert_eq!(hex(&encode_proof(&p).unwrap()), expected);
    assert_eq!(
        serde_json::from_str::<ProofTranscript>(&serde_json::to_string(&p).unwrap()).unwrap(),
        p
    );
    for field in [
        "grant_id",
        "parent_instance",
        "parent_incarnation",
        "parent_key_hash",
        "pane_id",
        "pane_generation",
        "grant_expires_ms",
    ] {
        let mut value = serde_json::to_value(&p).unwrap();
        value.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<ProofTranscript>(value).is_err(),
            "{field}"
        );
    }
    let mut invalid = p;
    invalid.grant_id = Some(HexBytes([0; 16]));
    assert!(encode_proof(&invalid).is_err());
}

#[test]
fn allocate_domains_and_policies_golden_vectors() {
    let prefix = concat!(
        "636f736d69782e6e61746976652d73657373696f6e2e616c6c6f63617465000001",
        "01010101010101010101010101010101",
        "02020202020202020202020202020202",
        "0303030303030303030303030303030303030303030303030303030303030303"
    );
    for (policy, suffix) in [
        (Policy::DefaultOpen, "000c64656661756c742d6f70656e"),
        (Policy::Restricted, "000a72657374726963746564"),
    ] {
        assert_eq!(
            hex(&encode_allocate(
                HexBytes([1; 16]),
                HexBytes([2; 16]),
                HexBytes([3; 32]),
                policy
            )),
            format!("{prefix}{suffix}")
        );
    }
}

#[test]
fn capability_encoding_golden_set_order_and_limits() {
    use Capability::*;
    let expected = b"\0\x06\0\x07execute\0\x05input\0\x0dmanage_layout\0\x0dread_contents\0\x0aread_state\0\x09terminate";
    assert_eq!(
        encode_capabilities(&[
            ReadState,
            Terminate,
            Input,
            ManageLayout,
            Execute,
            ReadContents
        ])
        .unwrap(),
        expected
    );
    assert_eq!(encode_capabilities(&[Input]).unwrap(), b"\0\x01\0\x05input");
    assert!(encode_capabilities(&[]).is_err());
    assert!(encode_capabilities(&[Input, Input]).is_err());
    assert!(encode_capabilities(&[Input; 7]).is_err());
}

#[test]
fn forbidden_has_uniform_rc_and_body() {
    let error = SessionError::forbidden();
    assert_eq!(error.rc(), 10);
    assert_eq!(
        serde_json::to_string(&error).unwrap(),
        r#"{"error_code":"FORBIDDEN","message":"forbidden","details":{}}"#
    );
}
