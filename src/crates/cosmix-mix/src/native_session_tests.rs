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
            c"cosmix-session-test".as_ptr(),
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
    let parsed = parse(&file).expect("valid bootstrap");
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
    let mut extended = descriptor();
    extended["future_field"] = serde_json::json!(true);
    assert!(parse(&memfd(extended, 1, SEALS, false)).is_err());
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
    for fd in 0..3 {
        assert!(unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0);
    }
}

#[test]
fn consume_closes_fd_and_scrubs_marker_on_success_and_failure() {
    for (valid, marker) in [
        (true, "64"),
        (false, "64"),
        (true, "65"),
        (true, "bogus"),
        (true, "0"),
    ] {
        let file = memfd(descriptor(), 1, if valid { SEALS } else { 0 }, false);
        let raw = file.as_raw_fd();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "native_session::tests::consume_scrub_helper",
                "--nocapture",
            ])
            .env(MARKER, marker)
            .env(
                "MIX_BOOTSTRAP_SCRUB_TEST",
                if valid && marker == "64" {
                    "ok"
                } else {
                    "error"
                },
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
    assert!(
        !relevant_notice(
            "noded.session.lifecycle",
            &notice.to_string(),
            &hello,
            Some(&record)
        )
        .unwrap()
    );
    notice["state"] = serde_json::json!("suspended");
    notice["future_broker_field"] = serde_json::json!({"ignored": true});
    assert!(
        relevant_notice(
            "noded.session.lifecycle",
            &notice.to_string(),
            &hello,
            Some(&record)
        )
        .unwrap()
    );
    notice["target"]["binding_generation"] = serde_json::json!("0");
    assert!(
        !relevant_notice(
            "noded.session.lifecycle",
            &notice.to_string(),
            &hello,
            Some(&record)
        )
        .unwrap()
    );
    assert!(
        relevant_notice(
            "noded.session.lifecycle.gap",
            &serde_json::json!({"broker_epoch":hello.broker_epoch, "future_broker_field":42})
                .to_string(),
            &hello,
            Some(&record)
        )
        .unwrap()
    );
    assert!(relevant_notice("noded.session.lifecycle", "{", &hello, Some(&record)).is_err());
}

#[test]
fn fresh_proof_retry_classification_and_budget_are_bounded() {
    for (code, reason, retry) in [
        (ErrorCode::Expired, "challenge_expired", true),
        (ErrorCode::Conflict, "challenge_consumed", true),
        (ErrorCode::Forbidden, "", false),
        (ErrorCode::Expired, "grant_expired", false),
        (ErrorCode::Conflict, "other", false),
    ] {
        let error = SessionError {
            error_code: code,
            message: String::new(),
            details: serde_json::from_value(serde_json::json!({"reason":reason})).unwrap(),
        };
        assert_eq!(
            matches!(
                refusal_recovery("prove: refused", &error),
                Recovery::FreshChallenge
            ),
            retry
        );
        assert!(matches!(
            refusal_recovery("challenge: refused", &error),
            Recovery::Wait
        ));
    }
    let mut budget = ProofRetries::default();
    let now = Instant::now();
    for _ in 0..PROOF_RETRY_CAP {
        assert!(budget.next(now).unwrap() >= now + PROOF_RETRY_FLOOR);
    }
    for _ in 0..100 {
        assert!(budget.next(now).is_none());
    }
}

#[test]
fn resident_uses_configuration_captured_before_thread_start() {
    let source = include_str!("native_session.rs");
    let start = source.split("pub(super) fn start()").nth(1).unwrap();
    let spawn = start.find(".spawn(move ||").unwrap();
    assert!(start.find("NativeEnvironment::capture()").unwrap() < spawn);
    assert!(start.find("environment.resolve()").unwrap() > spawn);
    assert!(start.find("options(account,").unwrap() > spawn);
    let worker = source.split("impl Bootstrap {").nth(1).unwrap();
    for forbidden in [
        "std::env::",
        "use std::env",
        "resolve_noded_url()",
        "native_endpoint()",
    ] {
        assert!(
            !worker.contains(forbidden),
            "resident must not read mutable environ"
        );
    }
    // Imports above the worker must not make an unqualified env read invisible.
    assert!(!source.contains("use std::env"));
    let paths = include_str!("cosmix_paths.rs");
    let resolution = paths
        .split("pub(crate) fn resolve(self)")
        .nth(1)
        .unwrap()
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    for forbidden in ["std::env::var", "use std::env", "dirs::"] {
        assert!(!resolution.contains(forbidden));
    }
}

#[tokio::test]
async fn restart_during_reconnect_backoff_revokes_over_independent_uds() {
    use term_native_test_broker::Broker;
    let mut broker = Broker::start();
    let options = broker.options();
    let UnixConnectOutcome::VerifiedUnix(parent) =
        NodedClient::connect_unix("", &broker.url, &options, None)
            .await
            .unwrap()
    else {
        panic!("verified parent")
    };
    let parent_key = SigningKey::from_bytes(&[19; 32]);
    let parent_record = parent
        .session_allocate(&parent_key, Policy::DefaultOpen)
        .await
        .unwrap()
        .record;
    let child_key = SigningKey::from_bytes(&[42; 32]);
    let grant = parent
        .session_grant_create(&GrantCreateArgs {
            parent: parent_record.reference(),
            pane_id: DecimalU64(1),
            pane_generation: DecimalU64(1),
            public_key: HexBytes(child_key.verifying_key().to_bytes()),
            role: Role::PaneShell,
            capabilities: vec![Capability::ReadState],
        })
        .await
        .unwrap();
    let file = memfd(
        serde_json::json!({"grant": grant.grant, "record": grant.record}),
        1,
        SEALS,
        false,
    );
    let mut bootstrap = parse(&file).ok().unwrap();
    let UnixConnectOutcome::VerifiedUnix(child) =
        NodedClient::connect_unix("", &broker.url, &options, None)
            .await
            .unwrap()
    else {
        panic!("verified child")
    };
    let hello = child.session_hello().await.unwrap();
    let bound = bootstrap
        .attach(&child, &hello, &mut Reporter::default())
        .await
        .ok()
        .unwrap();
    assert_eq!(bound.state, BindingState::Attached);
    close(&child).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while parent
            .session_self(bound.record_id)
            .await
            .unwrap()
            .record
            .state
            != BindingState::Suspended
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    // Exercise the production 40s backoff arm with a retained record but NO
    // usable old connection. An unconditional ack(false) fails this test.
    let (sender, mut restart) = tokio::sync::mpsc::unbounded_channel();
    let (ack, received) = std::sync::mpsc::sync_channel(1);
    sender.send(ack).unwrap();
    assert!(
        !tokio::time::timeout(
            Duration::from_secs(14),
            reconnect_backoff(
                4,
                &mut restart,
                &mut bootstrap,
                Some(&bound),
                &broker.url,
                &options
            )
        )
        .await
        .unwrap()
    );
    assert!(received.try_recv().unwrap());
    assert_eq!(
        parent
            .session_self(bound.record_id)
            .await
            .unwrap()
            .record
            .state,
        BindingState::Revoked
    );

    broker.stop();
    let (ack, received) = std::sync::mpsc::sync_channel(1);
    sender.send(ack).unwrap();
    assert!(
        !tokio::time::timeout(
            Duration::from_secs(14),
            reconnect_backoff(
                4,
                &mut restart,
                &mut bootstrap,
                Some(&bound),
                &broker.url,
                &options
            )
        )
        .await
        .unwrap()
    );
    assert!(
        !received.try_recv().unwrap(),
        "unreachable broker cannot confirm revocation"
    );
}

#[tokio::test]
async fn cached_status_delivery_rechecks_broker_suspend_without_notices() {
    use term_native_test_broker::Broker;
    let broker = Broker::start();
    async fn connect(broker: &Broker) -> VerifiedConnection {
        let UnixConnectOutcome::VerifiedUnix(connection) =
            NodedClient::connect_unix("", &broker.url, &broker.options(), None)
                .await
                .unwrap()
        else {
            panic!("verified connection required")
        };
        connection
    }
    let parent = connect(&broker).await;
    let parent_record = parent
        .session_allocate(&SigningKey::from_bytes(&[21; 32]), Policy::DefaultOpen)
        .await
        .unwrap()
        .record;
    // The memfd fixture seeds the child half with [42; 32]; the grant must
    // name that exact key or parse() rejects the bootstrap.
    let child_key = SigningKey::from_bytes(&[42; 32]);
    let grant = parent
        .session_grant_create(&GrantCreateArgs {
            parent: parent_record.reference(),
            pane_id: DecimalU64(1),
            pane_generation: DecimalU64(1),
            public_key: HexBytes(child_key.verifying_key().to_bytes()),
            role: Role::PaneShell,
            capabilities: vec![Capability::ReadState],
        })
        .await
        .unwrap();
    let file = memfd(
        serde_json::json!({"grant":grant.grant,"record":grant.record}),
        1,
        SEALS,
        false,
    );
    let mut bootstrap = parse(&file).ok().unwrap();
    let child = connect(&broker).await;
    let hello = child.session_hello().await.unwrap();
    let bound = bootstrap
        .attach(&child, &hello, &mut Reporter::default())
        .await
        .ok()
        .unwrap();
    let observer = std::sync::Arc::new(connect(&broker).await);
    let observer_hello = observer.session_hello().await.unwrap();
    let sending = observer.clone();
    let name = bound.name.clone();
    let request = tokio::spawn(async move {
        sending
            .client()
            .call(&name, "shell.status", serde_json::json!({}))
            .await
    });
    let delivery = loop {
        let event = child.recv_shared().await.unwrap();
        if event.command().command == "shell.status" {
            break event;
        }
    };
    let principal = delivery.trusted_context().unwrap();
    assert!(crate::session_status::admitted(&observer, &observer_hello, principal, &bound, Capability::ReadState).await);
    parent.client().close().await; // real broker recursively suspends the child
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let current = observer.session_self(bound.record_id).await.unwrap().record;
        if current.state == BindingState::Suspended {
            break;
        }
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Never consume a lifecycle hint. The saved delivery + old Attached record
    // alone would pass policy; a fresh verified reader isolates the Attached
    // re-read gate from the original connection's compulsory broker close.
    assert!(!crate::session_status::admitted(&observer, &observer_hello, principal, &bound, Capability::ReadState).await);
    request.abort();
    let _ = request.await;
}

#[tokio::test]
async fn real_broker_challenge_expiry_recovers_without_a_lifecycle_notice() {
    use term_native_test_broker::{Broker, session_fd::LaunchFd};
    let broker = Broker::start();
    let options = broker.options();
    let UnixConnectOutcome::VerifiedUnix(parent) =
        NodedClient::connect_unix("", &broker.url, &options, None)
            .await
            .unwrap()
    else {
        panic!("verified parent required")
    };
    let parent_key = SigningKey::from_bytes(&[19; 32]);
    let mut parent_record = parent
        .session_allocate(&parent_key, Policy::DefaultOpen)
        .await
        .unwrap()
        .record;
    let child_key = SigningKey::from_bytes(&[42; 32]);
    let grant = parent
        .session_grant_create(&GrantCreateArgs {
            parent: parent_record.reference(),
            pane_id: DecimalU64(1),
            pane_generation: DecimalU64(1),
            public_key: HexBytes(child_key.verifying_key().to_bytes()),
            role: Role::PaneShell,
            capabilities: vec![Capability::ReadState],
        })
        .await
        .unwrap();
    let launch = LaunchFd::new(&grant, &child_key).unwrap();
    let raw = unsafe { libc::fcntl(launch.mapping().0, libc::F_DUPFD_CLOEXEC, 3) };
    assert!(raw >= 3);
    let file = unsafe { File::from_raw_fd(raw) };
    let mut bootstrap = parse(&file).ok().unwrap();
    drop(file);
    drop(launch);
    // Delay only the first proof beyond the broker's actual 5s challenge life.
    // No broker bounce, gap or parent mutation can rescue a Wait-only owner.
    bootstrap.proof_delay_once = Duration::from_secs(6);
    let attempts = bootstrap.proof_attempts.clone();
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let url = broker.url.clone();
    let task = tokio::spawn(async move {
        let mut reporter = Reporter::default();
        own(bootstrap, options, url, &mut reporter, receiver).await;
        reporter.reported
    });
    let started = Instant::now();
    let mut renewed = Instant::now();
    loop {
        if renewed.elapsed() >= Duration::from_secs(3) {
            parent_record = parent
                .session_renew(parent_record.reference())
                .await
                .unwrap()
                .record;
            renewed = Instant::now();
        }
        let record = parent
            .session_self(grant.record.record_id)
            .await
            .unwrap()
            .record;
        if record.state == BindingState::Attached {
            assert_eq!(record.binding_generation, DecimalU64(1));
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(25),
            "expired proof stranded child"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(attempts.load(std::sync::atomic::Ordering::Relaxed), 2);
    assert!(started.elapsed() >= Duration::from_secs(6) + PROOF_RETRY_FLOOR);
    let (ack, received) = std::sync::mpsc::sync_channel(1);
    sender.send(ack).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap()
    );
    assert!(received.try_recv().unwrap());
}

/// A byte relay that adds an adjustable one-way delay to everything it
/// forwards, so a resident on one connection pays a realistic per-RPC cost
/// against an embedded broker whose real round trip is microseconds. It runs in
/// the same process and under the same uid as the broker, so BUS-013 endpoint
/// ownership and peer-credential verification are unchanged by it.
struct SlowEndpoint {
    path: std::path::PathBuf,
    root: std::path::PathBuf,
    delay_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl SlowEndpoint {
    fn start(target: &std::path::Path, delay: Duration) -> Self {
        use std::os::unix::fs::PermissionsExt;
        // The broker's own root is 0755 for the same reason: a 0700 ancestor
        // fails endpoint verification outright, which would look like a
        // profile problem rather than a fixture one.
        let unique = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let root = std::env::temp_dir().join(format!("mix-slow-session-{unique}"));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = root.join("bus.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::UnixListener::from_std(listener).unwrap();
        let delay_ms =
            std::sync::Arc::new(std::sync::atomic::AtomicU64::new(delay.as_millis() as u64));
        let target = target.to_path_buf();
        let forwarded = delay_ms.clone();
        tokio::spawn(async move {
            while let Ok((inbound, _)) = listener.accept().await {
                let target = target.clone();
                let forwarded = forwarded.clone();
                tokio::spawn(async move {
                    let Ok(outbound) = tokio::net::UnixStream::connect(&target).await else {
                        return;
                    };
                    let (from_client, to_client) = inbound.into_split();
                    let (from_broker, to_broker) = outbound.into_split();
                    tokio::join!(
                        pump(from_client, to_broker, forwarded.clone()),
                        pump(from_broker, to_client, forwarded),
                    );
                });
            }
        });
        Self {
            path,
            root,
            delay_ms,
        }
    }

    /// Every later forwarded chunk waits this long. Read per chunk, so a window
    /// opened here applies to whatever crosses the relay while it is open.
    fn set_delay(&self, delay: Duration) {
        self.delay_ms.store(
            delay.as_millis() as u64,
            std::sync::atomic::Ordering::Release,
        );
    }
}

impl Drop for SlowEndpoint {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn pump(
    mut read: tokio::net::unix::OwnedReadHalf,
    mut write: tokio::net::unix::OwnedWriteHalf,
    delay_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut bytes = vec![0u8; 65536];
    while let Ok(forwarded @ 1..) = read.read(&mut bytes).await {
        tokio::time::sleep(Duration::from_millis(
            delay_ms.load(std::sync::atomic::Ordering::Acquire),
        ))
        .await;
        if write.write_all(&bytes[..forwarded]).await.is_err() {
            break;
        }
    }
    let _ = write.shutdown().await;
}

/// Reports when the resident's next renewal lands, read from the lease this
/// authenticated observation sees jump back up. Nothing in the resident is
/// instrumented: the broker's own committed state is the signal.
async fn next_renewal(observer: &VerifiedConnection, record: HexBytes<16>) {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut previous = u64::MAX;
    loop {
        let remaining = observer
            .session_self(record)
            .await
            .unwrap()
            .record
            .lease_remaining_ms
            .unwrap()
            .0;
        if remaining > previous {
            return;
        }
        previous = remaining;
        assert!(Instant::now() < deadline, "no renewal observed");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn admission_contention_delays_a_renewal_instead_of_dropping_the_attachment() {
    use term_native_test_broker::{Broker, session_fd::LaunchFd};
    let broker = Broker::start();
    // Every session RPC on the resident's connection now costs a real round
    // trip. The observer half connects straight to the broker: the fixture must
    // be able to read committed state without paying the resident's latency.
    let relay = SlowEndpoint::start(&broker.endpoint, Duration::from_millis(100));
    let UnixConnectOutcome::VerifiedUnix(parent) =
        NodedClient::connect_unix("", &broker.url, &broker.options(), None)
            .await
            .unwrap()
    else {
        panic!("verified parent required")
    };
    let parent_key = SigningKey::from_bytes(&[23; 32]);
    let parent_record = parent
        .session_allocate(&parent_key, Policy::DefaultOpen)
        .await
        .unwrap()
        .record;
    let child_key = SigningKey::from_bytes(&[42; 32]);
    let grant = parent
        .session_grant_create(&GrantCreateArgs {
            parent: parent_record.reference(),
            pane_id: DecimalU64(1),
            pane_generation: DecimalU64(1),
            public_key: HexBytes(child_key.verifying_key().to_bytes()),
            role: Role::PaneShell,
            capabilities: vec![Capability::ReadState],
        })
        .await
        .unwrap();
    let launch = LaunchFd::new(&grant, &child_key).unwrap();
    let raw = unsafe { libc::fcntl(launch.mapping().0, libc::F_DUPFD_CLOEXEC, 3) };
    assert!(raw >= 3);
    let file = unsafe { File::from_raw_fd(raw) };
    let bootstrap = parse(&file).ok().unwrap();
    drop(file);
    drop(launch);

    let mut options = broker.options();
    options.endpoint = Some(relay.path.clone());
    options.require_native_session = true;
    options.incoming_capacity = Some(64);
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let url = broker.url.clone();
    let resident = tokio::spawn(async move {
        let mut reporter = Reporter::default();
        own(bootstrap, options, url, &mut reporter, receiver).await;
        reporter.reported
    });

    let parent = std::sync::Arc::new(parent);
    // The Term half of the fixture owes its own lease; its reference is stable
    // across renewals, so one retained reference keeps it alive throughout.
    let keepalive = tokio::spawn({
        let renewing = parent.clone();
        let reference = parent_record.reference();
        async move {
            loop {
                tokio::time::sleep(Duration::from_secs(3)).await;
                if renewing.session_renew(reference.clone()).await.is_err() {
                    return;
                }
            }
        }
    });

    let started = Instant::now();
    let bound = loop {
        let record = parent
            .session_self(grant.record.record_id)
            .await
            .unwrap()
            .record;
        if record.state == BindingState::Attached {
            break record;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "child did not attach through the relay"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(bound.binding_generation, DecimalU64(1));
    let request = serde_json::json!({"version":1,"target":{
        "broker_epoch":bound.broker_epoch,"record":bound.reference(),
        "instance_id":bound.instance_id,"pane_id":bound.pane_id,
        "pane_generation":bound.pane_generation
    }});

    // Four concurrent admissions, continuously. This caller is session-bound,
    // so each one costs the resident session RPCs on the very connection its
    // renewal uses; the resident must stay attached through them.
    let mut flood = tokio::task::JoinSet::new();
    for _ in 0..4 {
        let caller = parent.clone();
        let name = bound.name.clone();
        let request = request.clone();
        flood.spawn(async move {
            loop {
                let _ = tokio::time::timeout(
                    Duration::from_secs(5),
                    caller.client().call(&name, "shell.status", request.clone()),
                )
                .await;
                tokio::task::yield_now().await;
            }
        });
    }
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        let current = parent.session_self(bound.record_id).await.unwrap().record;
        assert_eq!(current.state, BindingState::Attached);
        assert_eq!(
            current.reference(),
            bound.reference(),
            "admission load must not reconnect the attachment"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    flood.abort_all();
    while flood.join_next().await.is_some() {}

    // With the lane quiet again the renewal is the only traffic left on the
    // relay, so a stall opened just before one is due is a stall that renewal
    // meets. It is longer than a single RPC deadline and shorter than the
    // retry's: one attempt cannot survive it, an attempt plus a retry can.
    tokio::time::sleep(Duration::from_secs(1)).await;
    next_renewal(&parent, bound.record_id).await;
    tokio::time::sleep(RENEW_CADENCE - Duration::from_millis(700)).await;
    relay.set_delay(RPC * 2 / 3);
    tokio::time::sleep(Duration::from_millis(3200)).await;
    relay.set_delay(Duration::from_millis(100));
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        let current = parent.session_self(bound.record_id).await.unwrap().record;
        assert_eq!(current.state, BindingState::Attached);
        assert_eq!(
            current.reference(),
            bound.reference(),
            "a stalled renewal must be retried, not abandoned"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let (ack, received) = std::sync::mpsc::sync_channel(1);
    sender.send(ack).unwrap();
    assert!(
        !tokio::time::timeout(Duration::from_secs(20), resident)
            .await
            .unwrap()
            .unwrap(),
        "the resident reported a failure it should have ridden out"
    );
    assert!(received.try_recv().unwrap());
    keepalive.abort();
}

#[tokio::test]
async fn a_session_bound_admission_costs_two_round_trips() {
    use term_native_test_broker::Broker;
    let broker = Broker::start();
    let delay = Duration::from_millis(150);
    let relay = SlowEndpoint::start(&broker.endpoint, delay);
    async fn connect(broker: &Broker, endpoint: Option<&std::path::Path>) -> VerifiedConnection {
        let mut options = broker.options();
        if let Some(endpoint) = endpoint {
            options.endpoint = Some(endpoint.to_path_buf());
        }
        let UnixConnectOutcome::VerifiedUnix(connection) =
            NodedClient::connect_unix("", &broker.url, &options, None)
                .await
                .unwrap()
        else {
            panic!("verified connection required")
        };
        connection
    }
    let parent = connect(&broker, None).await;
    let parent_record = parent
        .session_allocate(&SigningKey::from_bytes(&[24; 32]), Policy::DefaultOpen)
        .await
        .unwrap()
        .record;
    // The memfd fixture seeds the child half with [42; 32].
    let child_key = SigningKey::from_bytes(&[42; 32]);
    let grant = parent
        .session_grant_create(&GrantCreateArgs {
            parent: parent_record.reference(),
            pane_id: DecimalU64(1),
            pane_generation: DecimalU64(1),
            public_key: HexBytes(child_key.verifying_key().to_bytes()),
            role: Role::PaneShell,
            capabilities: vec![Capability::ReadState],
        })
        .await
        .unwrap();
    let file = memfd(
        serde_json::json!({"grant":grant.grant,"record":grant.record}),
        1,
        SEALS,
        false,
    );
    let mut bootstrap = parse(&file).ok().unwrap();
    let child = connect(&broker, Some(&relay.path)).await;
    // One hello for the connection, exactly as the resident takes it.
    let hello = child.session_context().await.unwrap();
    let bound = bootstrap
        .attach(&child, &hello, &mut Reporter::default())
        .await
        .ok()
        .unwrap();
    let caller = std::sync::Arc::new(parent);
    let sending = caller.clone();
    let name = bound.name.clone();
    let request = tokio::spawn(async move {
        sending
            .client()
            .call(&name, "shell.status", serde_json::json!({}))
            .await
    });
    let delivery = loop {
        let event = child.recv_shared().await.unwrap();
        if event.command().command == "shell.status" {
            break event;
        }
    };
    let principal = delivery.trusted_context().unwrap();
    assert_eq!(principal.assurance, Assurance::SessionBound);

    // A session-bound caller's admission is one lease check plus one re-read of
    // our own attachment. The hello that used to precede the lease check made it
    // three, and every one of them serialises on the connection the resident
    // renews over.
    let started = Instant::now();
    assert!(crate::session_status::admitted(&child, &hello, principal, &bound, Capability::ReadState).await);
    let elapsed = started.elapsed();
    assert!(
        elapsed < delay * 5,
        "admission took {elapsed:?}: two round trips cost about {:?}, three about {:?}",
        delay * 4,
        delay * 6
    );
    request.abort();
    let _ = request.await;
}
