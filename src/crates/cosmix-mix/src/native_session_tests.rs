use super::*;
use std::io::{Seek, Write};
use std::os::unix::process::CommandExt;

fn descriptor() -> serde_json::Value {
    let seed = Zeroizing::new([42u8; 32]);
    let key = SigningKey::from_bytes(&seed);
    serde_json::json!({
        "grant": {"grant_id":"11".repeat(16), "record_id":"22".repeat(16),
            "incarnation":"33".repeat(16), "public_key":HexBytes(key.verifying_key().to_bytes()),
            "parent_key_hash":"44".repeat(32), "expires_ms":"30000", "state":"pending"},
        "record": {"name":"test-child", "record_assurance":"reserved", "owner_node":"test-node",
            "owner_uid":unsafe { libc::geteuid() }, "broker_epoch":"55".repeat(16), "record_id":"22".repeat(16),
            "instance_id":"66".repeat(16), "incarnation":"33".repeat(16), "role":"pane-shell",
            "parent_instance":"77".repeat(16), "parent_incarnation":"88".repeat(16),
            "pane_id":"1", "pane_generation":"2", "binding_generation":"0", "state":"pending",
            "capabilities":["read_state"], "policy":"default-open", "lease_remaining_ms":"999999"}
    })
}

fn memfd(descriptor: serde_json::Value, version: u8, seals: i32, extra: bool) -> File {
    let raw = unsafe {
        libc::memfd_create(
            c"mix-bootstrap-test".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    assert!(raw >= 3);
    let mut file = unsafe { File::from_raw_fd(raw) };
    let public = serde_json::to_vec(&descriptor).unwrap();
    file.write_all(&[version]).unwrap();
    file.write_all(&(public.len() as u32).to_be_bytes())
        .unwrap();
    file.write_all(&public).unwrap();
    let seed = Zeroizing::new([42u8; 32]);
    file.write_all(seed.as_ref()).unwrap();
    if extra {
        file.write_all(&[0]).unwrap();
    }
    if seals != 0 {
        assert_eq!(unsafe { libc::fcntl(raw, libc::F_ADD_SEALS, seals) }, 0);
    }
    file
}

#[test]
fn parse_v1_uses_pread_checks_scope_and_ignores_stale_lease() {
    let mut file = memfd(descriptor(), 1, SEALS, false);
    assert!(file.stream_position().unwrap() > 0); // deliberately at EOF
    let parsed = parse(&file).ok().expect("valid bootstrap");
    assert_eq!(parsed.scope.pane_high_water, Some(DecimalU64(2)));
    assert_eq!(parsed.scope.unix_uid, unsafe { libc::geteuid() });
    assert_eq!(parsed.scope.purpose, Purpose::Enrol);
    assert_eq!(parsed.scope.parent_key_hash, Some(HexBytes([0x44; 32])));
    assert_eq!(
        parsed.public_key,
        HexBytes(
            SigningKey::from_bytes(&parsed.seed)
                .verifying_key()
                .to_bytes()
        )
    );
}

#[test]
fn rejects_unsealed_partial_seals_layout_and_scope_substitution() {
    for seals in [
        0,
        libc::F_SEAL_SEAL,
        SEALS & !libc::F_SEAL_WRITE,
        SEALS & !libc::F_SEAL_GROW,
        SEALS & !libc::F_SEAL_SHRINK,
        SEALS & !libc::F_SEAL_SEAL,
    ] {
        assert!(parse(&memfd(descriptor(), 1, seals, false)).is_err());
    }
    assert!(parse(&memfd(descriptor(), 2, SEALS, false)).is_err());
    assert!(parse(&memfd(descriptor(), 1, SEALS, true)).is_err());
    for (field, value) in [
        ("role", serde_json::json!("term")),
        ("pane_id", serde_json::Value::Null),
        (
            "owner_uid",
            serde_json::json!(unsafe { libc::geteuid() }.wrapping_add(1)),
        ),
        ("pane_generation", serde_json::json!("0")),
    ] {
        let mut public = descriptor();
        public["record"][field] = value;
        assert!(parse(&memfd(public, 1, SEALS, false)).is_err());
    }
    let mut public = descriptor();
    public["grant"]["public_key"] = serde_json::json!("00".repeat(32));
    assert!(parse(&memfd(public, 1, SEALS, false)).is_err());
}

#[test]
fn marker_never_owns_stdio() {
    for value in ["-1", "0", "1", "2", "bogus", "2147483648"] {
        assert!(marker_fd(std::ffi::OsStr::new(value)).is_err());
    }
    assert_eq!(marker_fd(std::ffi::OsStr::new("64")), Ok(64));
}

#[test]
fn consume_scrub_helper() {
    let Ok(expected) = std::env::var("MIX_BOOTSTRAP_SCRUB_TEST") else {
        return;
    };
    let result = consume();
    assert_eq!(result.is_ok(), expected == "ok");
    assert!(std::env::var_os(MARKER).is_none());
    assert_eq!(unsafe { libc::fcntl(64, libc::F_GETFD) }, -1);
}

#[test]
fn consume_closes_fd_and_scrubs_marker_on_success_and_failure() {
    for valid in [true, false] {
        let file = memfd(descriptor(), 1, if valid { SEALS } else { 0 }, false);
        let raw = file.as_raw_fd();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "native_session::tests::consume_scrub_helper",
                "--nocapture",
            ])
            .env(MARKER, "64")
            .env(
                "MIX_BOOTSTRAP_SCRUB_TEST",
                if valid { "ok" } else { "error" },
            );
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(raw, 64) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::fcntl(64, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn expected_scope_retains_high_water_and_resets_only_parent_domain() {
    let file = memfd(descriptor(), 1, SEALS, false);
    let bootstrap = parse(&file).ok().unwrap();
    let mut record: SessionRecord = serde_json::from_value(descriptor()["record"].clone()).unwrap();
    let hello = Hello {
        broker_epoch: record.broker_epoch,
        connection_id: HexBytes([9; 16]),
    };
    assert_eq!(
        bootstrap
            .expected(&hello, &record)
            .ok()
            .unwrap()
            .pane_high_water,
        Some(DecimalU64(2))
    );
    record.state = BindingState::Suspended;
    assert_eq!(
        bootstrap.expected(&hello, &record).ok().unwrap().purpose,
        Purpose::Resume
    );
    record.parent_instance = Some(HexBytes([10; 16]));
    let replaced = bootstrap.expected(&hello, &record).ok().unwrap();
    assert_eq!(replaced.pane_high_water, Some(DecimalU64(1)));
    assert_eq!(replaced.parent_key_hash, bootstrap.scope.parent_key_hash);
    record.capabilities.push(Capability::Execute);
    assert!(bootstrap.expected(&hello, &record).is_err());
}

#[test]
fn attached_notice_does_not_create_a_self_resume_loop() {
    let mut record: SessionRecord = serde_json::from_value(descriptor()["record"].clone()).unwrap();
    record.state = BindingState::Attached;
    record.binding_generation = DecimalU64(1);
    let hello = Hello {
        broker_epoch: record.broker_epoch,
        connection_id: HexBytes([9; 16]),
    };
    let mut notice = serde_json::json!({"broker_epoch":hello.broker_epoch, "target":record.reference(), "state":"attached"});
    assert!(!relevant_notice(
        "noded.session.lifecycle",
        &notice.to_string(),
        &hello,
        Some(&record)
    ));
    notice["state"] = serde_json::json!("suspended");
    assert!(relevant_notice(
        "noded.session.lifecycle",
        &notice.to_string(),
        &hello,
        Some(&record)
    ));
    notice["target"]["binding_generation"] = serde_json::json!("0");
    assert!(!relevant_notice(
        "noded.session.lifecycle",
        &notice.to_string(),
        &hello,
        Some(&record)
    ));
    assert!(relevant_notice(
        "noded.session.lifecycle.gap",
        &serde_json::json!({"broker_epoch":hello.broker_epoch}).to_string(),
        &hello,
        Some(&record)
    ));
}
