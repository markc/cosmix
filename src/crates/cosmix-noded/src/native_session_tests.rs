//! Native-session fixtures: real Axum listeners, routing and response ownership.
use super::*;
use cosmix_bus::native_session::{PRINCIPAL_HEADER, read_principal};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message as WsMessage};

struct Broker {
    task: tokio::task::JoinHandle<Result<()>>,
    root: PathBuf,
    url: String,
}
impl Drop for Broker {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
impl Broker {
    async fn start() -> Self {
        Self::start_with_unix(true).await
    }
    async fn start_with_unix(unix: bool) -> Self {
        Self::start_mode(unix, false).await
    }
    async fn start_mode(unix: bool, unavailable: bool) -> Self {
        Self::start_named(unix, unavailable, "test-node".into()).await
    }
    async fn start_named(unix: bool, unavailable: bool, node: String) -> Self {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let listen = probe.local_addr().unwrap().to_string();
        drop(probe);
        let root =
            std::env::temp_dir().join(format!("cosmix-native-{:032x}", rand::random::<u128>()));
        if unavailable {
            use std::os::unix::fs::PermissionsExt;
            std::fs::create_dir(&root).unwrap();
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o750)).unwrap();
        }
        let (ready_tx, ready_rx) = oneshot::channel();
        let task = tokio::spawn(run(
            RunConfig {
                listen: listen.clone(),
                node,
                wg_ip: "127.0.0.1".into(),
                mesh_config_path: None,
                spec_dir: None,
                admission_mode: AdmissionMode::Off,
                observe_allowed_services: vec!["audit-observer".into()],
                unix_socket: unix.then(|| root.join("bus.sock")),
            },
            ready_tx,
        ));
        tokio::time::timeout(std::time::Duration::from_secs(5), ready_rx)
            .await
            .unwrap()
            .unwrap();
        Self {
            task,
            root,
            url: format!("ws://{listen}/ws"),
        }
    }
    async fn unix(&self) -> WebSocketStream<tokio::net::UnixStream> {
        let socket = tokio::net::UnixStream::connect(self.root.join("bus.sock"))
            .await
            .unwrap();
        tokio_tungstenite::client_async("ws://localhost/ws", socket)
            .await
            .unwrap()
            .0
    }
    async fn tcp(
        &self,
    ) -> WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
        tokio_tungstenite::connect_async(&self.url).await.unwrap().0
    }
}
fn request(command: &str, to: &str, id: &str) -> BusMessage {
    BusMessage::new()
        .with_header("bus", "1")
        .with_header("type", "request")
        .with_header("command", command)
        .with_header("to", to)
        .with_header("id", id)
}

async fn session_call<S: AsyncRead + AsyncWrite + Unpin>(
    socket: &mut WebSocketStream<S>,
    command: &str,
    id: &str,
    body: serde_json::Value,
) -> BusMessage {
    send(
        socket,
        &request(&format!("noded.session.{command}"), "noded", id)
            .with_header("native-session", "1")
            .with_body(&body.to_string()),
    )
    .await;
    loop {
        let reply = receive(socket).await;
        if reply.message_type() == Some("response") && reply.get("id") == Some(id) {
            return reply;
        }
    }
}

#[tokio::test]
async fn session_allocation_proof_connection_binding_retention_and_renew() {
    use cosmix_bus::native_session::*;
    use ed25519_dalek::{Signer, SigningKey};
    let broker = Broker::start().await;
    let mut socket = broker.unix().await;
    let hello = session_call(&mut socket, "hello", "hello", serde_json::json!({})).await;
    let context: serde_json::Value = serde_json::from_str(&hello.body).unwrap();
    let key = SigningKey::from_bytes(&rand::random());
    let public_key = HexBytes(key.verifying_key().to_bytes());
    let signature = HexBytes(
        key.sign(&encode_allocate(
            serde_json::from_value(context["broker_epoch"].clone()).unwrap(),
            serde_json::from_value(context["connection_id"].clone()).unwrap(),
            public_key,
            Policy::Restricted,
        ))
        .to_bytes(),
    );
    let args =
        serde_json::json!({"public_key":public_key, "signature":signature, "policy":"restricted"});
    let mut thief = broker.unix().await;
    assert_eq!(
        session_call(&mut thief, "allocate", "1", args.clone())
            .await
            .get("rc"),
        Some("10")
    );
    let allocated = session_call(&mut socket, "allocate", "1", args.clone()).await;
    assert_eq!(allocated.get("rc"), Some("0"), "{}", allocated.body);
    let result: serde_json::Value = serde_json::from_str(&allocated.body).unwrap();
    let record: SessionRecord = serde_json::from_value(result["record"].clone()).unwrap();
    assert!(reserved_session_name(&record.name));
    assert_eq!(record.capabilities.len(), 6);
    assert_eq!(record.binding_generation, DecimalU64(1));
    assert_eq!(
        session_call(&mut socket, "allocate", "1", args).await.body,
        allocated.body
    );
    assert_eq!(
        session_call(
            &mut socket,
            "revoke",
            "1",
            serde_json::json!({"target":record.reference()})
        )
        .await
        .get("rc"),
        Some("10")
    );
    for _ in 0..2 {
        let reply = session_call(
            &mut socket,
            "renew",
            "repeat",
            serde_json::json!({"target":record.reference()}),
        )
        .await;
        assert_eq!(reply.get("rc"), Some("0"));
        let body: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
        assert_eq!(body["record"]["lease_remaining_ms"], "15000");
    }
    let listed = session_call(&mut socket, "list", "list", serde_json::json!({})).await;
    let body: serde_json::Value = serde_json::from_str(&listed.body).unwrap();
    assert_eq!(body["records"].as_array().unwrap().len(), 1);
}

#[test]
fn reserved_session_namespace_matches_shape_not_canonical_uid() {
    let suffix = "abcdefghijklmnopqrstuvwxyz234567";
    for kind in ['t', 'c'] {
        for uid in ["0", "1", "rs", "1z141z3", "00", "0000000", "zzzzzzz"] {
            // Exercise every allowed suffix symbol, not just 'a'.
            for symbol in suffix.chars() {
                let name = format!("{kind}{uid}-{}", symbol.to_string().repeat(22));
                assert!(valid_service_name(&name));
                assert!(reserved_session_name(&name), "{name}");
            }
        }
    }
    for name in [
        "",
        "t",
        "t-aaaaaaaaaaaaaaaaaaaaaa",
        "t12345678-aaaaaaaaaaaaaaaaaaaaaa",
        "x0-aaaaaaaaaaaaaaaaaaaaaa",
        "t0-aaaaaaaaaaaaaaaaaaaaa",
        "t0-aaaaaaaaaaaaaaaaaaaaaaa",
        "t0-aaaaaaaaaaaaaaaaaaaaa0",
        "t0-aaaaaaaaaaaaaaaaaaaaa1",
        "t0-aaaaaaaaaaaaaaaaaaaaa8",
        "t0-aaaaaaaaaaaaaaaaaaaaa9",
        "t0-aaaaaaaaaaaaaaaaaaaaaA",
        "t0-aaaaaaaaaaaaaaaaaaaaa-",
        "tA-aaaaaaaaaaaaaaaaaaaaaa",
        "té-aaaaaaaaaaaaaaaaaaaaaa",
        "t0-aaaaaaaaaaaaaaaaaaaaé",
        "t0-aaaaaaaaaaaaaaaaaaaaaa\n",
    ] {
        assert!(!reserved_session_name(name), "{name:?}");
    }
}

async fn allocate_term(
    socket: &mut WebSocketStream<tokio::net::UnixStream>,
) -> cosmix_bus::native_session::SessionRecord {
    use cosmix_bus::native_session::*;
    use ed25519_dalek::{Signer, SigningKey};
    let hello = session_call(socket, "hello", "hello", serde_json::json!({})).await;
    let h: serde_json::Value = serde_json::from_str(&hello.body).unwrap();
    let key = SigningKey::from_bytes(&rand::random());
    let public_key = HexBytes(key.verifying_key().to_bytes());
    let signature = HexBytes(
        key.sign(&encode_allocate(
            serde_json::from_value(h["broker_epoch"].clone()).unwrap(),
            serde_json::from_value(h["connection_id"].clone()).unwrap(),
            public_key,
            Policy::Restricted,
        ))
        .to_bytes(),
    );
    let reply = session_call(
        socket,
        "allocate",
        "1",
        serde_json::json!({"public_key":public_key,"signature":signature,"policy":"restricted"}),
    )
    .await;
    assert_eq!(reply.get("rc"), Some("0"), "{}", reply.body);
    let body: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
    serde_json::from_value(body["record"].clone()).unwrap()
}

#[tokio::test]
async fn p0i_03_child_proof_scope_and_challenge_consumption() {
    use cosmix_bus::native_session::*;
    use ed25519_dalek::{Signer, SigningKey};
    let broker = Broker::start().await;
    let mut parent = broker.unix().await;
    let term = allocate_term(&mut parent).await;
    let key = SigningKey::from_bytes(&rand::random());
    let public_key = HexBytes(key.verifying_key().to_bytes());
    let args = serde_json::json!({"parent":term.reference(),"pane_id":"7","pane_generation":"1","public_key":public_key,"role":"pane-shell","capabilities":["input"]});
    let reply = session_call(&mut parent, "grant.create", "2", args).await;
    assert_eq!(reply.get("rc"), Some("0"), "{}", reply.body);
    let created: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
    let child: SessionRecord = serde_json::from_value(created["record"].clone()).unwrap();
    assert_eq!(child.binding_generation, DecimalU64(0));
    let grant: SessionGrant = serde_json::from_value(created["grant"].clone()).unwrap();
    let mut socket = broker.unix().await;
    let selector = serde_json::json!({"record_id":child.record_id,"incarnation":child.incarnation,"purpose":"enrol","grant_id":grant.grant_id});
    let reply = session_call(&mut socket, "challenge", "first", selector.clone()).await;
    assert_eq!(reply.get("rc"), Some("0"), "{}", reply.body);
    assert_eq!(
        session_call(&mut socket, "challenge", "repeat", selector.clone())
            .await
            .body,
        reply.body
    );
    let proof: ProofTranscript = serde_json::from_str(&reply.body).unwrap();
    let mut wrong = proof.clone();
    wrong.pane_id = Some(DecimalU64(8));
    let signature = HexBytes(key.sign(&encode_proof(&wrong).unwrap()).to_bytes());
    let attempt = serde_json::json!({"challenge_id":proof.challenge_id,"signature":signature});
    assert_eq!(
        session_call(&mut socket, "prove", "bad", attempt.clone())
            .await
            .get("rc"),
        Some("10")
    );
    assert!(
        session_call(&mut socket, "prove", "bad-again", attempt)
            .await
            .body
            .contains("challenge_consumed")
    );
    let fetched = session_call(
        &mut parent,
        "grant.fetch",
        "fetch",
        serde_json::json!({"public_key":public_key}),
    )
    .await;
    let fetched: serde_json::Value = serde_json::from_str(&fetched.body).unwrap();
    assert_eq!(fetched["grant"]["state"], "pending");
    for generation in [1, 2] {
        let reply = session_call(
            &mut socket,
            "challenge",
            "fresh",
            serde_json::json!({"public_key":public_key,"purpose":"enrol"}),
        )
        .await;
        let proof: ProofTranscript = serde_json::from_str(&reply.body).unwrap();
        assert_eq!(proof.binding_generation, DecimalU64(generation));
        if generation == 2 {
            assert_eq!(proof.purpose, Purpose::Resume);
            assert_eq!(proof.grant_id, None);
        }
        let signature = HexBytes(key.sign(&encode_proof(&proof).unwrap()).to_bytes());
        let attempt = serde_json::json!({"challenge_id":proof.challenge_id,"signature":signature});
        let mut impostor = broker.unix().await;
        assert_eq!(
            session_call(&mut impostor, "prove", "stolen", attempt.clone())
                .await
                .get("rc"),
            Some("10")
        );
        let attached = session_call(&mut socket, "prove", "good", attempt).await;
        assert_eq!(attached.get("rc"), Some("0"), "{}", attached.body);
        let attached: serde_json::Value = serde_json::from_str(&attached.body).unwrap();
        assert_eq!(
            attached["record"]["binding_generation"],
            generation.to_string()
        );
    }
}

#[tokio::test]
async fn bound_delivery_registers_lease_dependency_and_disconnect_notifies() {
    let broker = Broker::start().await;
    let mut parent = broker.unix().await;
    let record = allocate_term(&mut parent).await;
    let mut recipient = broker.unix().await;
    register(&mut recipient, "lease-recipient").await;
    let target = serde_json::json!({"target":record.reference()});
    assert!(
        session_call(&mut recipient, "lease.check", "before", target.clone())
            .await
            .body
            .contains("dependency_missing")
    );
    send(
        &mut parent,
        &request("probe.event", "lease-recipient", "delivery").with_header("type", "event"),
    )
    .await;
    let delivery = receive(&mut recipient).await;
    let principal = read_principal(&delivery).unwrap().unwrap();
    assert_eq!(principal.assurance, Assurance::SessionBound);
    assert_eq!(principal.session.unwrap().record_id, record.record_id);
    let lease = session_call(&mut recipient, "lease.check", "after", target).await;
    assert_eq!(lease.get("rc"), Some("0"), "{}", lease.body);
    parent.close(None).await.unwrap();
    let notice = receive(&mut recipient).await;
    assert_eq!(notice.command_name(), Some("noded.session.lifecycle"));
    let notice: serde_json::Value = serde_json::from_str(&notice.body).unwrap();
    assert_eq!(notice["state"], "suspended");
}

async fn assert_preclaim_refused<S: AsyncRead + AsyncWrite + Unpin>(
    caller: &mut WebSocketStream<S>,
    recipient: &mut WebSocketStream<tokio::net::UnixStream>,
) {
    // No allocations exist. Refusal must not depend on an issued-name lookup.
    for name in [
        "t0-aaaaaaaaaaaaaaaaaaaaaa",
        "c1-234567aaaaaaaaaaaaaaaa",
        "t00-aaaaaaaaaaaaaaaaaaaaaa",
        "czzzzzzz-aaaaaaaaaaaaaaaaaaaaaa",
    ] {
        send(
            caller,
            &request("noded.register", "noded.test-node.bus", "preclaim").with_header("from", name),
        )
        .await;
        let reply = receive(caller).await;
        assert_eq!(reply.get("id"), Some("preclaim"));
        assert_eq!(reply.get("rc"), Some("10"));
        assert_eq!(reply.get("error"), Some("reserved_name"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&reply.body).unwrap(),
            serde_json::json!({"error": "reserved_name"})
        );

        // Refusal must preserve the connection's previous registration and
        // canonicalisation authority. Also test that a forged reserved `from`
        // on ordinary routed traffic cannot establish that identity.
        send(
            caller,
            &request("probe.event", "namespace-recipient", "after-refusal")
                .with_header("type", "event")
                .with_header("from", name),
        )
        .await;
        assert_eq!(receive(recipient).await.from_addr(), Some("legacy-owner"));
    }
}

#[tokio::test]
async fn p0i_02_reserved_preclaims_refused_on_tcp_and_unix() {
    let broker = Broker::start().await;
    let mut recipient = broker.unix().await;
    register(&mut recipient, "namespace-recipient").await;
    let mut tcp = broker.tcp().await;
    register(&mut tcp, "legacy-owner").await;
    assert_preclaim_refused(&mut tcp, &mut recipient).await;
    // Release the ordinary alias before testing the other ingress. Awaiting
    // deregistration makes this independent of connection-cleanup scheduling.
    send(&mut tcp, &request("noded.deregister", "noded", "release")).await;
    assert_eq!(receive(&mut tcp).await.get("rc"), Some("0"));
    let mut unix = broker.unix().await;
    register(&mut unix, "legacy-owner").await;
    assert_preclaim_refused(&mut unix, &mut recipient).await;
    // A neighbouring legacy name outside the reserved shape still registers.
    register(&mut unix, "t0-aaaaaaaaaaaaaaaaaaaaa1").await;
}

#[tokio::test]
async fn p0i_02_reserved_preclaim_refused_without_unix_listener() {
    let broker = Broker::start_with_unix(false).await;
    let mut tcp = broker.tcp().await;
    send(
        &mut tcp,
        &request("noded.register", "noded", "preclaim")
            .with_header("from", "t0-aaaaaaaaaaaaaaaaaaaaaa"),
    )
    .await;
    assert_eq!(receive(&mut tcp).await.get("error"), Some("reserved_name"));
    register(&mut tcp, "legacy-after-refusal").await;
}

#[tokio::test]
async fn oversized_node_principal_refuses_unix_upgrade_without_panicking() {
    let broker = Broker::start_named(true, false, "n".repeat(4096)).await;
    for _ in 0..2 {
        let socket = tokio::net::UnixStream::connect(broker.root.join("bus.sock"))
            .await
            .unwrap();
        let error = tokio_tungstenite::client_async("ws://localhost/ws", socket)
            .await
            .unwrap_err();
        assert!(
            matches!(error, tokio_tungstenite::tungstenite::Error::Http(response)
            if response.status() == 403)
        );
    }
    let mut tcp = broker.tcp().await;
    send(&mut tcp, &request("noded.ping", "noded", "alive")).await;
    assert_eq!(receive(&mut tcp).await.get("rc"), Some("0"));
}
async fn send<S: AsyncRead + AsyncWrite + Unpin>(
    socket: &mut WebSocketStream<S>,
    msg: &BusMessage,
) {
    socket
        .send(WsMessage::Text(msg.to_wire().into()))
        .await
        .unwrap();
}
async fn receive<S: AsyncRead + AsyncWrite + Unpin>(socket: &mut WebSocketStream<S>) -> BusMessage {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            match socket
                .next()
                .await
                .expect("connection closed")
                .expect("WebSocket error")
            {
                WsMessage::Text(text) => return bus::parse(&text).unwrap(),
                WsMessage::Ping(_) | WsMessage::Pong(_) => continue,
                other => panic!("unexpected message {other:?}"),
            }
        }
    })
    .await
    .expect("broker delivery deadline")
}
async fn register<S: AsyncRead + AsyncWrite + Unpin>(socket: &mut WebSocketStream<S>, name: &str) {
    send(
        socket,
        &request("noded.register", "noded", "register").with_header("from", name),
    )
    .await;
    let reply = receive(socket).await;
    assert_eq!(reply.get("rc"), Some("0"));
    assert_eq!(
        reply.get(PRINCIPAL_HEADER),
        None,
        "broker control reply is not a caller stamp"
    );
}

#[tokio::test]
async fn p0i_06_native_ingress_principal_and_responder_channel_binding() {
    let broker = Broker::start().await;
    let mut service = broker.unix().await;
    register(&mut service, "service-a").await;
    let mut attacker = broker.tcp().await;
    register(&mut attacker, "attacker-a").await;
    let mut caller = broker.unix().await;
    let message = request("probe.echo", "service-a", "original-id")
        .with_header("from", "service-a")
        .with_header("BROKER_PRINCIPAL", "forged")
        .with_header("bRoKeR_pRiNcIpAl", "another forgery")
        .with_body("private request");
    send(&mut caller, &message).await;
    let routed = receive(&mut service).await;
    assert_eq!(
        routed.from_addr(),
        None,
        "anonymous from must not impersonate a service"
    );
    let principal = read_principal(&routed).unwrap().unwrap();
    assert_eq!(principal.assurance, Assurance::LocalUnix);
    // SAFETY: these process-credential reads have no preconditions.
    assert_eq!(principal.unix_uid, unsafe { libc::geteuid() });
    assert_eq!(principal.unix_gid, unsafe { libc::getegid() });
    assert_eq!(principal.peer_pid, std::process::id());
    assert!(principal.session.is_none());
    let broker_id = routed.get("id").unwrap();
    assert_ne!(broker_id, "original-id");
    let forged = request("probe.echo", "caller", broker_id)
        .with_header("type", "response")
        .with_header("from", "service-a")
        .with_header("rc", "0")
        .with_header(
            "BROKER_PRINCIPAL",
            &serde_json::to_string(&principal).unwrap(),
        )
        .with_body("forged response");
    send(&mut attacker, &forged).await;
    // An ordered barrier proves the forged response was processed before the
    // legitimate responder sends. No timing-based negative assertion is needed.
    send(&mut attacker, &request("noded.ping", "noded", "barrier")).await;
    let ping = receive(&mut attacker).await;
    assert_eq!(ping.get("id"), Some("barrier"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&ping.body).unwrap()["extensions"]["native-session"],
        "1"
    );
    send(
        &mut service,
        &forged.clone().with_body("legitimate response"),
    )
    .await;
    let reply = receive(&mut caller).await;
    assert_eq!(reply.get("id"), Some("original-id"));
    assert_eq!(reply.from_addr(), Some("service-a"));
    assert_eq!(reply.body.trim(), "legitimate response");
    assert!(read_principal(&reply).unwrap().is_some());

    send(&mut attacker, &message).await;
    let tcp_request = receive(&mut service).await;
    assert_eq!(tcp_request.from_addr(), Some("attacker-a"));
    assert_eq!(read_principal(&tcp_request).unwrap(), None);

    // Events and replies carry the sender's verified principal too.
    register(&mut caller, "native-app").await;
    send(
        &mut caller,
        &request("probe.event", "service-a", "event-id").with_header("type", "event"),
    )
    .await;
    let event = receive(&mut service).await;
    assert_eq!(
        read_principal(&event).unwrap().unwrap().connection_id,
        principal.connection_id
    );
    send(
        &mut service,
        &request("probe.reverse", "native-app", "reverse"),
    )
    .await;
    let reverse = receive(&mut caller).await;
    assert!(read_principal(&reverse).unwrap().is_some());
    send(
        &mut caller,
        &request("probe.reverse", "service-a", reverse.get("id").unwrap())
            .with_header("type", "response")
            .with_header("from", "attacker-a")
            .with_header("rc", "0"),
    )
    .await;
    let reply = receive(&mut service).await;
    assert_eq!(reply.from_addr(), Some("native-app"));
    assert_eq!(reply.get("id"), Some("reverse"));
    assert_eq!(
        read_principal(&reply).unwrap().unwrap().connection_id,
        principal.connection_id
    );
}

#[tokio::test]
async fn protected_requests_responses_and_recipient_events_never_reach_tap_payloads() {
    let broker = Broker::start().await;
    let mut observer = broker.tcp().await;
    register(&mut observer, "audit-observer").await;
    send(
        &mut observer,
        &request("noded.observe.start", "noded", "observe")
            .with_body(r#"{"filter":{"verbs":["probe.*"]},"body":"redacted"}"#),
    )
    .await;
    assert_eq!(receive(&mut observer).await.get("rc"), Some("0"));
    let mut tap = broker.tcp().await;
    send(&mut tap, &request("noded.tap", "noded", "tap")).await;
    assert_eq!(receive(&mut tap).await.get("rc"), Some("0"));
    let mut service = broker.tcp().await;
    register(&mut service, "service-a").await;
    let mut caller = broker.unix().await;
    register(&mut caller, "native-app").await;
    let secret = r#"{"secret":"PRIVATE-SENTINEL"}"#;
    send(
        &mut caller,
        &request("probe.echo", "service-a", "private").with_body(secret),
    )
    .await;
    let routed = receive(&mut service).await;
    send(
        &mut service,
        &request("probe.echo", "native-app", routed.get("id").unwrap())
            .with_header("type", "response")
            .with_header("rc", "0")
            .with_body(secret),
    )
    .await;
    receive(&mut caller).await;
    send(
        &mut service,
        &request("probe.event", "native-app", "private-event")
            .with_header("type", "event")
            .with_body(secret),
    )
    .await;
    receive(&mut caller).await;
    for _ in 0..3 {
        let observed = receive(&mut observer).await;
        assert!(!observed.to_wire().contains("PRIVATE-SENTINEL"));
        assert!(!observed.to_wire().contains("broker_principal"));
        let body: serde_json::Value = serde_json::from_str(&observed.body).unwrap();
        assert_eq!(body["payload_omitted"], "native_session_protected");
        assert!(body["payload"].is_null());
    }
    // A legacy marker must be the tap's first routed frame; any earlier native
    // enqueue fails this assertion. Legacy payload capture still works.
    let mut legacy = broker.tcp().await;
    send(
        &mut legacy,
        &request("probe.public", "service-a", "public-marker").with_body("public body"),
    )
    .await;
    receive(&mut service).await;
    let tapped = receive(&mut tap).await;
    assert_eq!(tapped.command_name(), Some("probe.public"));
    assert_eq!(tapped.body.trim(), "public body");
}

#[tokio::test]
async fn unix_binary_frames_refused_and_bootstrap_is_strict() {
    let broker = Broker::start().await;
    let mut legacy = broker.tcp().await;
    for command in ["noded.session.lifecycle", "noded.session.lifecycle.gap"] {
        send(&mut legacy, &request(command, "noded", "reserved")).await;
        let response = receive(&mut legacy).await;
        assert_eq!(response.get("rc"), Some("10"));
        assert_eq!(response.get("id"), Some("reserved"));
        assert_eq!(response.body, r#"{"error":"reserved_name"}"#);
    }
    let mut socket = broker.unix().await;
    send(
        &mut socket,
        &request("noded.session.hello", "noded", "hello")
            .with_header("native-session", "1")
            .with_body("{}"),
    )
    .await;
    let reply = receive(&mut socket).await;
    assert_eq!(reply.get("native-session"), Some("1"));
    assert_eq!(reply.get("rc"), Some("0"));
    let hello: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(hello["broker_epoch"].as_str().unwrap().len(), 32);
    assert_eq!(hello["connection_id"].as_str().unwrap().len(), 32);
    let malformed = request("noded.session.hello", "noded", "bad")
        .with_header("native-session", "1")
        .with_body(r#"{"x":1,"x":2}"#);
    send(&mut socket, &malformed).await;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&receive(&mut socket).await.body).unwrap()["error_code"],
        "INVALID_ARGUMENT"
    );
    send(
        &mut socket,
        &request("noded.session.hello", "noded", "oversize")
            .with_header("native-session", "1")
            .with_body(&"p".repeat(20_000)),
    )
    .await;
    let oversize = receive(&mut socket).await;
    assert_eq!(oversize.get("id"), Some("oversize"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&oversize.body).unwrap()["error_code"],
        "INVALID_ARGUMENT"
    );
    socket
        .send(WsMessage::Binary(b"binary is refused".to_vec().into()))
        .await
        .unwrap();
    let next = tokio::time::timeout(std::time::Duration::from_secs(3), socket.next())
        .await
        .unwrap();
    assert!(matches!(
        next,
        None | Some(Err(_)) | Some(Ok(WsMessage::Close(_)))
    ));
}

#[test]
fn malformed_bootstrap_prescan_keeps_only_bounded_correlation() {
    let raw = request("noded.session.prove", "noded", "proof")
        .with_body(&"private".repeat(10_000))
        .to_wire();
    let command = raw_session_command(&raw).unwrap();
    let minimal = invalid_bootstrap_envelope(&raw, command);
    assert!(minimal.body.is_empty());
    assert_eq!(minimal.headers.len(), 2);
    assert_eq!(minimal.get("id"), Some("proof"));
    let duplicate = raw.replace("id: proof", "id: proof\nID: duplicate");
    assert_eq!(
        invalid_bootstrap_envelope(&duplicate, command).get("id"),
        None
    );
    let padded = raw.replace("command:", " command :");
    assert_eq!(raw_session_command(&padded), Some("noded.session.prove"));
    assert!(cosmix_bus::native_session::parse_bootstrap(padded.as_bytes()).is_err());
}

#[tokio::test]
async fn protected_late_response_after_disconnect_and_duplicate_after_completion() {
    for disconnected in [true, false] {
        let broker = Broker::start().await;
        let mut observer = broker.tcp().await;
        register(&mut observer, "audit-observer").await;
        send(
            &mut observer,
            &request("noded.observe.start", "noded", "observe")
                .with_body(r#"{"filter":{"verbs":["probe.*"]},"body":"redacted"}"#),
        )
        .await;
        assert_eq!(receive(&mut observer).await.get("rc"), Some("0"));
        let mut service = broker.tcp().await;
        register(&mut service, "late-service").await;
        let mut caller = broker.unix().await;
        register(&mut caller, "late-caller").await;
        send(
            &mut caller,
            &request("probe.echo", "late-service", "original").with_body("PRIVATE-LATE-SENTINEL"),
        )
        .await;
        let routed = receive(&mut service).await;
        assert!(
            read_principal(&routed).unwrap().is_none(),
            "TCP egress strips Unix metadata"
        );
        let response = request("probe.echo", "late-caller", routed.get("id").unwrap())
            .with_header("type", "response")
            .with_header("rc", "0")
            .with_body("PRIVATE-LATE-SENTINEL");
        if disconnected {
            caller.close(None).await.unwrap();
            // Test-only synchronisation: absence is published only after the
            // caller's pending entries have been drained into tombstones.
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                loop {
                    send(
                        &mut service,
                        &request("noded.list", "noded", "cleanup-barrier"),
                    )
                    .await;
                    if !receive(&mut service).await.body.contains("late-caller") {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        } else {
            send(&mut service, &response).await;
            assert_eq!(
                receive(&mut caller).await.body.trim(),
                "PRIVATE-LATE-SENTINEL"
            );
        }
        send(&mut service, &response).await;
        for _ in 0..if disconnected { 2 } else { 3 } {
            let event = receive(&mut observer).await;
            assert!(!event.to_wire().contains("PRIVATE-LATE-SENTINEL"));
            let body: serde_json::Value = serde_json::from_str(&event.body).unwrap();
            assert_eq!(body["payload_omitted"], "native_session_protected");
        }
    }
}

#[tokio::test]
async fn unavailable_unix_keeps_tcp_ready_and_dev_endpoint_is_discoverable() {
    use cosmix_client::{NodedClient, UnixConnectOptions, UnixConnectOutcome};
    let unavailable = Broker::start_mode(true, true).await;
    let tcp = NodedClient::connect("fallback-check", &unavailable.url)
        .await
        .unwrap();
    let ping = tcp
        .call("noded", "noded.ping", serde_json::Value::Null)
        .await
        .unwrap();
    assert!(ping["extensions"]["native-session"].is_null());
    assert!(ping["extensions"]["native-session-endpoint"].is_null());
    assert!(
        NodedClient::connect_unix(
            "required",
            &unavailable.url,
            &client_options(&unavailable),
            None
        )
        .await
        .is_err()
    );
    tcp.close().await;
    let broker = Broker::start().await;
    let mut options = UnixConnectOptions::new(client_options(&broker).broker_account);
    options.require_native_session = true;
    // Neither explicit nor configured path: ping discovers the real dev-root
    // listener, then endpoint/peer verification authenticates it.
    let UnixConnectOutcome::VerifiedUnix(connection) =
        NodedClient::connect_unix("discovered", &broker.url, &options, None)
            .await
            .unwrap()
    else {
        panic!("required profile downgraded")
    };
    let ping = connection
        .client()
        .call("noded", "noded.ping", serde_json::Value::Null)
        .await
        .unwrap();
    assert_eq!(
        ping["extensions"]["native-session-endpoint"],
        broker.root.join("bus.sock").to_str().unwrap()
    );
    connection.client().close().await;
}

#[tokio::test]
async fn tcp_only_reserved_topic_keeps_legacy_observation_without_fanout_events() {
    let broker = Broker::start().await;
    let mut owner = broker.tcp().await;
    register(&mut owner, "maild").await;
    let mut subscriber = broker.tcp().await;
    register(&mut subscriber, "legacy-reader").await;
    let mut observer = broker.tcp().await;
    register(&mut observer, "audit-observer").await;
    send(
        &mut observer,
        &request("noded.observe.start", "noded", "observe")
            .with_body(r#"{"filter":{"verbs":["maild.props.*","noded.ping"]},"body":"redacted"}"#),
    )
    .await;
    assert_eq!(receive(&mut observer).await.get("rc"), Some("0"));
    let grant = request("noded.props.subscribe_grant", "noded", "grant")
        .with_header("topic", "maild.props.records.changed")
        .with_header("target_peer", "legacy-reader")
        .with_header("namespace", "maild.accounts");
    send(&mut owner, &grant).await;
    assert_eq!(receive(&mut owner).await.get("rc"), Some("0"));
    let inner = BusMessage::new()
        .with_header("command", "maild.props.records.changed")
        .with_header("type", "event")
        .with_body(r#"{"namespace":"maild.accounts","value":"legacy"}"#);
    send(
        &mut owner,
        &request("topic.publish", "noded", "publish")
            .with_header("name", "maild.props.records.changed")
            .with_body(&inner.to_wire()),
    )
    .await;
    assert_eq!(receive(&mut owner).await.get("rc"), Some("0"));
    assert!(receive(&mut subscriber).await.body.contains("legacy"));
    let mut replay = broker.tcp().await;
    register(&mut replay, "legacy-replay").await;
    send(
        &mut owner,
        &grant.with_header("target_peer", "legacy-replay"),
    )
    .await;
    assert_eq!(receive(&mut owner).await.get("rc"), Some("0"));
    assert!(receive(&mut replay).await.body.contains("legacy"));
    send(&mut owner, &request("noded.ping", "noded", "barrier")).await;
    receive(&mut owner).await;
    // At the pre-S1 baseline, inner fan-out/replay had no observe events.
    // The first matching observation must still be this ordered ping barrier.
    let event = receive(&mut observer).await;
    let body: serde_json::Value = serde_json::from_str(&event.body).unwrap();
    assert_eq!(body["verb"], "noded.ping");
    assert_ne!(body["payload_omitted"], "native_session_protected");
}

fn client_options(broker: &Broker) -> cosmix_client::UnixConnectOptions {
    // SAFETY: process credential reads have no preconditions.
    let mut options = cosmix_client::UnixConnectOptions::new(cosmix_client::BrokerAccount {
        uid: unsafe { libc::geteuid() },
        gid: unsafe { libc::getegid() },
    });
    options.configured_endpoint = Some(broker.root.join("bus.sock"));
    options.require_native_session = true;
    options
}

#[tokio::test]
async fn p0i_06_client_verified_delivery_and_tcp_has_no_trusted_context() {
    use cosmix_client::{NodedClient, UnixConnectOutcome};
    let broker = Broker::start().await;
    let options = client_options(&broker);
    let UnixConnectOutcome::VerifiedUnix(mut service) =
        NodedClient::connect_unix("verified-service", &broker.url, &options, None)
            .await
            .unwrap()
    else {
        panic!("required connection downgraded")
    };
    let mut caller = broker.unix().await;
    register(&mut caller, "verified-caller").await;
    send(
        &mut caller,
        &request("probe.echo", "verified-service", "context")
            .with_header("BROKER_PRINCIPAL", "forged")
            .with_body("request"),
    )
    .await;
    let delivery = tokio::time::timeout(std::time::Duration::from_secs(3), service.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(delivery.command().from, "verified-caller");
    assert_eq!(
        delivery.trusted_context().unwrap().unix_uid,
        options.broker_account.uid
    );
    service
        .client()
        .respond(delivery.command(), 0, "legitimate reply")
        .await
        .unwrap();
    assert_eq!(receive(&mut caller).await.body.trim(), "legitimate reply");

    let tcp = NodedClient::connect("ordinary-tcp", &broker.url)
        .await
        .unwrap();
    tcp.send_raw(
        &request("probe.event", "verified-service", "tcp")
            .with_header("type", "event")
            .with_header("broker_principal", "forged"),
    )
    .await
    .unwrap();
    let delivery = tokio::time::timeout(std::time::Duration::from_secs(3), service.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(delivery.command().from, "ordinary-tcp");
    assert!(delivery.trusted_context().is_none());
    // Unix caller metadata must not leave over the TCP recipient transport.
    // IncomingCommand also provides no trusted-context accessor.
    let mut raw = tcp.incoming_async().await.unwrap();
    send(
        &mut caller,
        &request("probe.event", "ordinary-tcp", "raw").with_header("type", "event"),
    )
    .await;
    let raw = tokio::time::timeout(std::time::Duration::from_secs(3), raw.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(raw.header("broker_principal").is_none());
    tcp.close().await;
    service.client().close().await;
}

#[tokio::test]
async fn client_required_unix_never_downgrades_and_fallback_is_typed_unverified() {
    use cosmix_client::{ConnectError, NodedClient, UnixConnectOutcome};
    let broker = Broker::start_with_unix(false).await;
    let mut options = client_options(&broker);
    options.allow_unverified_tcp_fallback = true;
    assert!(matches!(
        NodedClient::connect_unix("required", &broker.url, &options, None).await,
        Err(ConnectError::Io(_))
    ));
    // A TCP-only broker is alive and useful, but it cannot satisfy the profile.
    options.require_native_session = false;
    let UnixConnectOutcome::UnverifiedTcp { client, unix_error } =
        NodedClient::connect_unix("fallback", &broker.url, &options, None)
            .await
            .unwrap()
    else {
        panic!("TCP fallback must be explicitly unverified")
    };
    assert!(matches!(unix_error, ConnectError::Io(_)));
    assert!(
        client
            .call("noded", "noded.ping", serde_json::Value::Null)
            .await
            .unwrap()["pong"]
            == true
    );
    client.close().await;
}

#[tokio::test]
async fn client_rejects_wrong_endpoint_owner_and_server_credentials_without_fallback() {
    use cosmix_client::{ConnectError, NodedClient};
    let broker = Broker::start().await;
    let mut options = client_options(&broker);
    options.allow_unverified_tcp_fallback = true;
    options.broker_account.uid = options.broker_account.uid.wrapping_add(1);
    assert!(matches!(
        NodedClient::connect_unix("wrong-owner", &broker.url, &options, None).await,
        Err(ConnectError::EndpointOwnership)
    ));
    options = client_options(&broker);
    options.broker_account.gid = options.broker_account.gid.wrapping_add(1);
    assert!(matches!(
        NodedClient::connect_unix("wrong-peer", &broker.url, &options, None).await,
        Err(ConnectError::PeerCredentials)
    ));
}

#[tokio::test]
async fn client_explicit_development_endpoint_requires_protected_path() {
    use cosmix_client::{ConnectError, NodedClient};
    use std::os::unix::fs::{PermissionsExt, symlink};
    let broker = Broker::start().await;
    let mut options = client_options(&broker);
    options.endpoint = Some(broker.root.join("alias.sock"));
    symlink(
        broker.root.join("bus.sock"),
        options.endpoint.as_ref().unwrap(),
    )
    .unwrap();
    assert!(matches!(
        NodedClient::connect_unix("symlink", &broker.url, &options, None).await,
        Err(ConnectError::EndpointOwnership)
    ));
    options.endpoint = Some(broker.root.join("bus.sock"));
    std::fs::set_permissions(&broker.root, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(matches!(
        NodedClient::connect_unix("writable", &broker.url, &options, None).await,
        Err(ConnectError::EndpointOwnership)
    ));
    std::fs::set_permissions(&broker.root, std::fs::Permissions::from_mode(0o755)).unwrap();
    options.endpoint = Some("relative/bus.sock".into());
    assert!(matches!(
        NodedClient::connect_unix("relative", &broker.url, &options, None).await,
        Err(ConnectError::InvalidEndpoint)
    ));
}
