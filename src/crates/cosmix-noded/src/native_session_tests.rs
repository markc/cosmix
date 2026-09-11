//! Native-session fixtures: real Axum listeners, routing and response ownership.
use super::*;
use cosmix_bus::native_session::{PRINCIPAL_HEADER, read_principal};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message as WsMessage};

struct Broker {
    sessions: Arc<tokio::sync::Mutex<session::Sessions>>,
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
        Self::start_with_grant_limit(unix, unavailable, node, 32).await
    }
    async fn start_with_grant_limit(
        unix: bool,
        unavailable: bool,
        node: String,
        pending_grants_per_parent: usize,
    ) -> Self {
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
        let (probe_tx, probe_rx) = oneshot::channel();
        let task = tokio::spawn(run(
            RunConfig {
                session_probe: Some(probe_tx),
                listen: listen.clone(),
                node,
                wg_ip: "127.0.0.1".into(),
                mesh_config_path: None,
                spec_dir: None,
                admission_mode: AdmissionMode::Off,
                observe_allowed_services: vec!["audit-observer".into()],
                unix_socket: unix.then(|| root.join("bus.sock")),
                pending_grants_per_parent,
            },
            ready_tx,
        ));
        tokio::time::timeout(std::time::Duration::from_secs(5), ready_rx)
            .await
            .unwrap()
            .unwrap();
        Self {
            sessions: probe_rx.await.unwrap(),
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
    for scope_error in ["role", "parent", "uid"] {
        let reply = session_call(&mut socket, "challenge", "wrong-scope", selector.clone()).await;
        let mut proof: ProofTranscript = serde_json::from_str(&reply.body).unwrap();
        match scope_error {
            "role" => {
                proof.role = Role::Term;
                proof.parent_instance = None;
                proof.parent_incarnation = None;
                proof.parent_key_hash = None;
                proof.pane_id = None;
                proof.pane_generation = None;
            }
            "parent" => proof.parent_instance = Some(HexBytes([0; 16])),
            _ => proof.unix_uid = proof.unix_uid.wrapping_add(1),
        }
        let signature = HexBytes(key.sign(&encode_proof(&proof).unwrap()).to_bytes());
        assert_eq!(
            session_call(
                &mut socket,
                "prove",
                "wrong-scope-proof",
                serde_json::json!({"challenge_id":proof.challenge_id,"signature":signature})
            )
            .await
            .get("rc"),
            Some("10")
        );
    }
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

#[tokio::test]
async fn parent_revocation_is_ordered_against_inflight_child_prove() {
    use cosmix_bus::native_session::*;
    use ed25519_dalek::SigningKey;
    use sha2::{Digest, Sha256};
    let broker = Broker::start().await;
    let observer = verified(&broker).await;
    for _ in 0..4 {
        let parent = verified(&broker).await;
        let parent_key = SigningKey::from_bytes(&rand::random());
        let record = parent
            .session_allocate(&parent_key, Policy::Restricted)
            .await
            .unwrap()
            .record;
        let key = SigningKey::from_bytes(&rand::random());
        let public_key = HexBytes(key.verifying_key().to_bytes());
        let grant = parent
            .session_grant_create(&GrantCreateArgs {
                parent: record.reference(),
                pane_id: DecimalU64(1),
                pane_generation: DecimalU64(1),
                public_key,
                role: Role::PaneShell,
                capabilities: vec![Capability::Input],
            })
            .await
            .unwrap();
        let child = verified(&broker).await;
        let challenge = child
            .session_challenge(&ChallengeArgs::Key(KeyChallenge {
                public_key,
                purpose: Purpose::Enrol,
            }))
            .await
            .unwrap();
        let scope = cosmix_client::session::ExpectedScope {
            broker_epoch: child.session_hello().await.unwrap().broker_epoch,
            purpose: Purpose::Enrol,
            unix_uid: record.owner_uid,
            parent_key_hash: Some(grant.grant.parent_key_hash),
            pane_id: Some(DecimalU64(1)),
            pane_high_water: None,
            role: Role::PaneShell,
            public_key_hash: HexBytes(Sha256::digest(public_key.0).into()),
            capabilities_hash: HexBytes(
                Sha256::digest(encode_capabilities(&[Capability::Input]).unwrap()).into(),
            ),
        };
        let proof = challenge.sign(&key, &scope).unwrap();
        let (_, _) = tokio::join!(
            parent.session_revoke(record.reference()),
            child.session_prove(&proof)
        );
        let list = observer.session_list().await.unwrap();
        for id in [record.record_id, grant.record.record_id] {
            assert_eq!(
                list.records
                    .iter()
                    .find(|r| r.record_id == id)
                    .unwrap()
                    .state,
                BindingState::Revoked
            );
        }
        assert!(observer.session_prove(&proof).await.is_err());
        assert!(
            observer
                .session_challenge(&ChallengeArgs::Key(KeyChallenge {
                    public_key,
                    purpose: Purpose::Enrol
                }))
                .await
                .is_err()
        );
        parent.client().close().await;
        child.client().close().await;
    }
    observer.client().close().await;
}

#[tokio::test]
async fn recipient_dependency_cap_refuses_the_bound_delivery() {
    use cosmix_bus::native_session::*;
    use ed25519_dalek::{Signer, SigningKey};
    let broker = Broker::start().await;
    let mut parent = broker.unix().await;
    let term = allocate_term(&mut parent).await;
    let mut recipient = broker.unix().await;
    register(&mut recipient, "dependency-recipient").await;
    let mut children = Vec::new();
    for pane in 0..257 {
        let key = SigningKey::from_bytes(&rand::random());
        let public_key = HexBytes(key.verifying_key().to_bytes());
        let grant = session_call(&mut parent,"grant.create",&(pane+2).to_string(),serde_json::json!({"parent":term.reference(),"pane_id":pane.to_string(),"pane_generation":"1","public_key":public_key,"role":"pane-shell","capabilities":["input"]})).await;
        assert_eq!(grant.get("rc"), Some("0"), "{}", grant.body);
        let descriptor: serde_json::Value = serde_json::from_str(&grant.body).unwrap();
        let mut child = broker.unix().await;
        let challenge = session_call(
            &mut child,
            "challenge",
            "challenge",
            serde_json::json!({"record_id":descriptor["grant"]["record_id"],"incarnation":descriptor["grant"]["incarnation"],"grant_id":descriptor["grant"]["grant_id"],"purpose":"enrol"}),
        )
        .await;
        let proof: ProofTranscript = serde_json::from_str(&challenge.body).unwrap();
        let signature = HexBytes(key.sign(&encode_proof(&proof).unwrap()).to_bytes());
        let attached = session_call(
            &mut child,
            "prove",
            "prove",
            serde_json::json!({"challenge_id":proof.challenge_id,"signature":signature}),
        )
        .await;
        assert_eq!(attached.get("rc"), Some("0"), "{}", attached.body);
        send(
            &mut child,
            &request("probe.dependency", "dependency-recipient", "delivery"),
        )
        .await;
        if pane < 256 {
            let received = receive(&mut recipient).await;
            assert_eq!(received.command_name(), Some("probe.dependency"));
            send(
                &mut recipient,
                &request("probe.dependency", "noded", received.get("id").unwrap())
                    .with_header("type", "response")
                    .with_header("rc", "0"),
            )
            .await;
        }
        let response = loop {
            let response = receive(&mut child).await;
            if response.message_type() == Some("response") && response.get("id") == Some("delivery")
            {
                break response;
            }
        };
        if pane < 256 {
            assert_eq!(response.get("rc"), Some("0"));
        } else {
            assert_eq!(response.get("rc"), Some("10"));
            assert!(response.body.contains("recipient_dependency_limit"));
        }
        children.push(child);
    }
    // No refused envelope was queued. A subsequent unbound marker is next.
    let mut ambient = broker.unix().await;
    send(
        &mut ambient,
        &request("probe.marker", "dependency-recipient", "marker").with_header("type", "event"),
    )
    .await;
    assert_eq!(
        receive(&mut recipient).await.command_name(),
        Some("probe.marker")
    );
}

#[tokio::test]
async fn notice_overflow_delivers_gap_before_notices_and_key_resync_succeeds() {
    use cosmix_bus::native_session::*;
    use ed25519_dalek::SigningKey;
    let broker = Broker::start().await;
    let parent = verified(&broker).await;
    let key = SigningKey::from_bytes(&rand::random());
    let term = parent
        .session_allocate(&key, Policy::Restricted)
        .await
        .unwrap()
        .record;
    let child_key = SigningKey::from_bytes(&rand::random());
    let public_key = HexBytes(child_key.verifying_key().to_bytes());
    let grant = parent
        .session_grant_create(&GrantCreateArgs {
            parent: term.reference(),
            pane_id: DecimalU64(1),
            pane_generation: DecimalU64(1),
            public_key,
            role: Role::PaneShell,
            capabilities: vec![Capability::Input],
        })
        .await
        .unwrap();
    let mut waiting = verified(&broker).await;
    let selector = ChallengeArgs::Key(KeyChallenge {
        public_key,
        purpose: Purpose::Enrol,
    });
    waiting.session_challenge(&selector).await.unwrap();
    // Real broker queues, atomically filled while their writer cannot drain.
    // Duplicate notices are legal and must be idempotent at the recipient.
    broker
        .sessions
        .lock()
        .await
        .test_notice_burst(grant.record.record_id, 257);
    let event = tokio::time::timeout(std::time::Duration::from_secs(3), waiting.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.command().command, "noded.session.lifecycle.gap");
    assert!(event.trusted_context().is_none());
    let challenge = waiting.session_challenge(&selector).await.unwrap();
    assert_eq!(challenge.transcript.record_id, grant.record.record_id);
    assert_eq!(challenge.transcript.binding_generation, DecimalU64(1));
    parent.session_renew(term.reference()).await.unwrap();
    parent.client().close().await;
    waiting.client().close().await;
}

#[tokio::test]
async fn session_interest_and_challenge_quotas_are_independent() {
    use cosmix_bus::native_session::*;
    use ed25519_dalek::SigningKey;
    let broker = Broker::start().await;
    let mut parent = broker.unix().await;
    let term = allocate_term(&mut parent).await;
    let key = SigningKey::from_bytes(&rand::random());
    let public_key = HexBytes(key.verifying_key().to_bytes());
    let selector = serde_json::json!({"public_key":public_key,"purpose":"enrol"});
    let mut sockets = Vec::new();
    for n in 0..257 {
        let mut socket = broker.unix().await;
        let reply = session_call(&mut socket, "challenge", "interest", selector.clone()).await;
        let body: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
        assert_eq!(body["error_code"], "FORBIDDEN");
        assert_eq!(body.get("wake_error").is_some(), n == 256);
        sockets.push(socket);
    }
    session_call(
        &mut parent,
        "renew",
        "renew",
        serde_json::json!({"target":term.reference()}),
    )
    .await;
    let grant = session_call(&mut parent,"grant.create","2",serde_json::json!({"parent":term.reference(),"pane_id":"1","pane_generation":"1","public_key":public_key,"role":"pane-shell","capabilities":["input"]})).await;
    assert_eq!(grant.get("rc"), Some("0"), "{}", grant.body);
    for socket in sockets.iter_mut().take(128) {
        assert_eq!(
            session_call(socket, "challenge", "challenge", selector.clone())
                .await
                .get("rc"),
            Some("0")
        );
    }
    let reply = session_call(&mut sockets[128], "challenge", "full", selector.clone()).await;
    let body: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(body["error_code"], "RESOURCE_LIMIT");
    assert!(body.get("wake_error").is_none());
    // A malformed identifiable prove releases only its own outstanding slot.
    assert_eq!(
        session_call(
            &mut sockets[0],
            "prove",
            "malformed",
            serde_json::json!({"signature":"bad"})
        )
        .await
        .get("rc"),
        Some("10")
    );
    let reply = session_call(&mut sockets[256], "challenge", "degraded-success", selector).await;
    assert_eq!(reply.get("rc"), Some("0"), "{}", reply.body);
    let body: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(body["wake_error"]["error_code"], "RESOURCE_LIMIT");
    let fetched = session_call(
        &mut parent,
        "grant.fetch",
        "fetch",
        serde_json::json!({"public_key":public_key}),
    )
    .await;
    let body: serde_json::Value = serde_json::from_str(&fetched.body).unwrap();
    assert_eq!(body["grant"]["state"], "pending");
}

// This helper is invoked in a separate OS process by the fixtures below.
// It is ignored in ordinary runs, never counted as a skipped privileged pass.
#[tokio::test]
#[ignore = "fixture subprocess only; requires COSMIX_SESSION_FIXTURE_ENDPOINT"]
async fn session_process_fixture() {
    let endpoint = std::env::var("COSMIX_SESSION_FIXTURE_ENDPOINT")
        .expect("SKIPPED: fixture endpoint not supplied; run the parent fixture");
    let stream = tokio::net::UnixStream::connect(endpoint).await.unwrap();
    let mut socket = tokio_tungstenite::client_async("ws://localhost/ws", stream)
        .await
        .unwrap()
        .0;
    let name = std::env::var("COSMIX_SESSION_FIXTURE_NAME").unwrap();
    send(
        &mut socket,
        &request("noded.register", "noded", "preclaim").with_header("from", &name),
    )
    .await;
    assert_eq!(receive(&mut socket).await.get("rc"), Some("10"));
    if let Ok(selector) = std::env::var("COSMIX_SESSION_FIXTURE_SELECTOR") {
        let reply = session_call(
            &mut socket,
            "challenge",
            "unowned",
            serde_json::from_str(&selector).unwrap(),
        )
        .await;
        let missing = session_call(&mut socket,"challenge","missing",serde_json::json!({"public_key":"0000000000000000000000000000000000000000000000000000000000000000","purpose":"enrol"})).await;
        // Changing the key on this connection adds wake_error. Compare only
        // the UID-independent lookup error, never regard a signature as proof.
        let mut a: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
        let mut b: serde_json::Value = serde_json::from_str(&missing.body).unwrap();
        a.as_object_mut().unwrap().remove("wake_error");
        b.as_object_mut().unwrap().remove("wake_error");
        assert_eq!(a, b);
        assert_eq!(a["error_code"], "FORBIDDEN");
        send(&mut socket, &request("noded.list", "noded", "discovery")).await;
        let list: serde_json::Value =
            serde_json::from_str(&receive(&mut socket).await.body).unwrap();
        assert!(list.as_array().unwrap().contains(&serde_json::json!(name)));
        if let Ok(proof) = std::env::var("COSMIX_SESSION_FIXTURE_PROOF") {
            let reply = session_call(
                &mut socket,
                "prove",
                "stolen-proof",
                serde_json::from_str(&proof).unwrap(),
            )
            .await;
            assert_eq!(reply.get("rc"), Some("10"));
        }
    }
}

fn fixture_process(broker: &Broker, name: &str) -> std::process::Command {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "noded::native_session_tests::session_process_fixture",
            "--ignored",
            "--nocapture",
        ])
        .env(
            "COSMIX_SESSION_FIXTURE_ENDPOINT",
            broker.root.join("bus.sock"),
        )
        .env("COSMIX_SESSION_FIXTURE_NAME", name);
    command
}

#[tokio::test]
async fn p0i_02_competing_process_preclaim_preserves_allocated_route() {
    let broker = Broker::start().await;
    let mut process = fixture_process(&broker, "t0-aaaaaaaaaaaaaaaaaaaaaa")
        .spawn()
        .unwrap();
    // Wait without blocking the current-thread broker's event loop.
    loop {
        if let Some(status) = process.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let parent = verified(&broker).await;
    let key = ed25519_dalek::SigningKey::from_bytes(&rand::random());
    let record = parent
        .session_allocate(&key, cosmix_bus::native_session::Policy::Restricted)
        .await
        .unwrap()
        .record;
    let mut process = fixture_process(&broker, &record.name).spawn().unwrap();
    loop {
        if let Some(status) = process.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    parent.session_renew(record.reference()).await.unwrap();
    parent.client().close().await;
}

#[tokio::test]
#[ignore = "SKIPPED privileged multi-UID fixture: requires root and COSMIX_SESSION_TEST_UID; run explicitly with --ignored"]
async fn p0i_03_privileged_other_uid_cannot_lookup_or_consume_grant() {
    use cosmix_bus::native_session::*;
    use ed25519_dalek::Signer;
    use std::os::unix::process::CommandExt;
    // No missing-prerequisite return path may be reported as a pass.
    let uid: u32 = std::env::var("COSMIX_SESSION_TEST_UID")
        .expect("SKIPPED: COSMIX_SESSION_TEST_UID is required")
        .parse()
        .unwrap();
    assert_eq!(unsafe { libc::geteuid() }, 0, "SKIPPED: requires root");
    assert_ne!(uid, 0, "fixture UID must differ from owner");
    let broker = Broker::start().await;
    let parent = verified(&broker).await;
    let parent_key = ed25519_dalek::SigningKey::from_bytes(&rand::random());
    let record = parent
        .session_allocate(&parent_key, Policy::Restricted)
        .await
        .unwrap()
        .record;
    let key = ed25519_dalek::SigningKey::from_bytes(&rand::random());
    let public_key = HexBytes(key.verifying_key().to_bytes());
    parent
        .session_grant_create(&GrantCreateArgs {
            parent: record.reference(),
            pane_id: DecimalU64(1),
            pane_generation: DecimalU64(1),
            public_key,
            role: Role::PaneShell,
            capabilities: vec![Capability::Input],
        })
        .await
        .unwrap();
    let mut command = fixture_process(&broker, &record.name);
    let rightful = verified(&broker).await;
    let challenge = rightful
        .session_challenge(&ChallengeArgs::Key(KeyChallenge {
            public_key,
            purpose: Purpose::Enrol,
        }))
        .await
        .unwrap();
    let proof = ProveArgs {
        challenge_id: challenge.transcript.challenge_id,
        signature: HexBytes(
            key.sign(&encode_proof(&challenge.transcript).unwrap())
                .to_bytes(),
        ),
    };
    command.env(
        "COSMIX_SESSION_FIXTURE_PROOF",
        serde_json::to_string(&proof).unwrap(),
    );
    command.uid(uid).gid(uid).env(
        "COSMIX_SESSION_FIXTURE_SELECTOR",
        serde_json::json!({"public_key":public_key,"purpose":"enrol"}).to_string(),
    );
    let mut process = command.spawn().unwrap();
    loop {
        if let Some(status) = process.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        parent
            .session_grant_fetch(public_key)
            .await
            .unwrap()
            .grant
            .state,
        GrantState::Pending
    );
    assert_eq!(
        rightful
            .session_prove(&proof)
            .await
            .unwrap()
            .record
            .binding_generation,
        DecimalU64(1)
    );
    rightful.client().close().await;
    parent.client().close().await;
}

#[tokio::test]
async fn configured_grant_limit_is_advertised_isolated_and_released_on_revoke() {
    use cosmix_bus::native_session::*;
    use cosmix_client::session::SessionFailure;
    use ed25519_dalek::SigningKey;
    let broker = Broker::start_with_grant_limit(true, false, "test-node".into(), 1).await;
    let mut raw = broker.unix().await;
    send(&mut raw, &request("noded.ping", "noded", "limits")).await;
    let ping: serde_json::Value = serde_json::from_str(&receive(&mut raw).await.body).unwrap();
    assert_eq!(
        ping["native_session_limits"]["pending_grants_per_parent"],
        "1"
    );
    let first = verified(&broker).await;
    let second = verified(&broker).await;
    let first_record = first
        .session_allocate(&SigningKey::from_bytes(&rand::random()), Policy::Restricted)
        .await
        .unwrap()
        .record;
    let second_record = second
        .session_allocate(&SigningKey::from_bytes(&rand::random()), Policy::Restricted)
        .await
        .unwrap()
        .record;
    let key = SigningKey::from_bytes(&rand::random());
    let mut args = GrantCreateArgs {
        parent: first_record.reference(),
        pane_id: DecimalU64(1),
        pane_generation: DecimalU64(1),
        public_key: HexBytes(key.verifying_key().to_bytes()),
        role: Role::PaneShell,
        capabilities: vec![Capability::Input],
    };
    let granted = first.session_grant_create(&args).await.unwrap();
    let mut excess = args.clone();
    excess.pane_id = DecimalU64(2);
    excess.public_key = HexBytes(
        SigningKey::from_bytes(&rand::random())
            .verifying_key()
            .to_bytes(),
    );
    assert!(matches!(
        first.session_grant_create(&excess).await,
        Err(SessionFailure::Refused {
            error: SessionError {
                error_code: ErrorCode::ResourceLimit,
                ..
            },
            ..
        })
    ));
    excess.parent = second_record.reference();
    second.session_grant_create(&excess).await.unwrap();
    assert!(
        first
            .session_revoke(granted.record.reference())
            .await
            .unwrap()
            .revoked
    );
    assert!(
        !first
            .session_revoke(granted.record.reference())
            .await
            .unwrap()
            .revoked
    );
    assert_eq!(
        first
            .session_grant_fetch(args.public_key)
            .await
            .unwrap()
            .grant
            .state,
        GrantState::Revoked
    );
    assert!(matches!(
        first.session_grant_create(&args).await,
        Err(SessionFailure::Refused {
            error: SessionError {
                error_code: ErrorCode::StaleGeneration,
                ..
            },
            ..
        })
    ));
    args.pane_generation = DecimalU64(2);
    let replacement = first.session_grant_create(&args).await.unwrap();
    assert_ne!(replacement.record.name, granted.record.name);
    assert_ne!(replacement.grant.grant_id, granted.grant.grant_id);
    assert_eq!(replacement.grant.state, GrantState::Pending);
    first.client().close().await;
    second.client().close().await;
}

#[tokio::test]
async fn session_term_and_grant_quotas_and_retention_high_water() {
    use cosmix_bus::native_session::*;
    use ed25519_dalek::SigningKey;
    let broker = Broker::start().await;
    let mut parents = Vec::new();
    for _ in 0..64 {
        let c = verified(&broker).await;
        let key = SigningKey::from_bytes(&rand::random());
        let record = c
            .session_allocate(&key, Policy::Restricted)
            .await
            .unwrap()
            .record;
        parents.push((c, record));
    }
    let excess = verified(&broker).await;
    let key = SigningKey::from_bytes(&rand::random());
    assert!(matches!(
        excess.session_allocate(&key, Policy::Restricted).await,
        Err(cosmix_client::session::SessionFailure::Refused {
            error: SessionError {
                error_code: ErrorCode::ResourceLimit,
                ..
            },
            ..
        })
    ));
    for (c, record) in parents.iter().take(32) {
        c.session_renew(record.reference()).await.unwrap();
        for pane in 1..=32 {
            let key = SigningKey::from_bytes(&rand::random());
            c.session_grant_create(&GrantCreateArgs {
                parent: record.reference(),
                pane_id: DecimalU64(pane),
                pane_generation: DecimalU64(1),
                public_key: HexBytes(key.verifying_key().to_bytes()),
                role: Role::PaneShell,
                capabilities: vec![Capability::Input],
            })
            .await
            .unwrap();
        }
        // Check the parent bound before the global grant pool is exhausted.
        assert!(matches!(
            c.session_grant_create(&GrantCreateArgs {
                parent: record.reference(),
                pane_id: DecimalU64(99),
                pane_generation: DecimalU64(1),
                public_key: HexBytes(key.verifying_key().to_bytes()),
                role: Role::PaneShell,
                capabilities: vec![Capability::Input],
            })
            .await,
            Err(cosmix_client::session::SessionFailure::Refused {
                error: SessionError {
                    error_code: ErrorCode::ResourceLimit,
                    ..
                },
                ..
            })
        ));
    }
    for index in [0, 32] {
        let (c, record) = &parents[index];
        let result = c
            .session_grant_create(&GrantCreateArgs {
                parent: record.reference(),
                pane_id: DecimalU64(99),
                pane_generation: DecimalU64(1),
                public_key: HexBytes(key.verifying_key().to_bytes()),
                role: Role::PaneShell,
                capabilities: vec![Capability::Input],
            })
            .await;
        assert!(matches!(
            result,
            Err(cosmix_client::session::SessionFailure::Refused {
                error: SessionError {
                    error_code: ErrorCode::ResourceLimit,
                    ..
                },
                ..
            })
        ));
    }
    // Retained refusal results are bounded too; old IDs cannot re-execute.
    let mut raw = broker.unix().await;
    let impossible = serde_json::json!({"target":{"record_id":HexBytes([0;16]),"incarnation":HexBytes([0;16]),"binding_generation":"1"}});
    for id in 1..=1025 {
        session_call(&mut raw, "revoke", &id.to_string(), impossible.clone()).await;
    }
    let reply = session_call(&mut raw, "revoke", "1", impossible).await;
    assert!(reply.body.contains("unknown_outcome"), "{}", reply.body);
    for (c, _) in parents {
        c.client().close().await;
    }
    excess.client().close().await;
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

#[derive(Clone)]
struct CaptureLog(Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for CaptureLog {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureLog {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}

#[tokio::test]
async fn p0i_09_binding_bootstrap_omits_keys_and_proofs_from_observe_tap_and_logs() {
    use cosmix_bus::native_session::*;
    let logs = CaptureLog(Default::default());
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(logs.clone())
        .without_time()
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let broker = Broker::start().await;
    let mut observer = broker.tcp().await;
    register(&mut observer, "audit-observer").await;
    send(
        &mut observer,
        &request("noded.observe.start", "noded", "observe")
            .with_body(r#"{"filter":{"verbs":["noded.session.*"]},"body":"redacted"}"#),
    )
    .await;
    assert_eq!(receive(&mut observer).await.get("rc"), Some("0"));
    let mut tap = broker.tcp().await;
    send(&mut tap, &request("noded.tap", "noded", "tap")).await;
    assert_eq!(receive(&mut tap).await.get("rc"), Some("0"));
    let parent = verified(&broker).await;
    let key = ed25519_dalek::SigningKey::from_bytes(&rand::random());
    let record = parent
        .session_allocate(&key, Policy::Restricted)
        .await
        .unwrap()
        .record;
    parent.session_renew(record.reference()).await.unwrap();
    let private = serde_json::to_value(HexBytes(key.to_bytes()))
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned();
    let public = serde_json::to_value(HexBytes(key.verifying_key().to_bytes()))
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned();
    for _ in 0..6 {
        let event = receive(&mut observer).await;
        let wire = event.to_wire();
        assert!(!wire.contains(&private));
        assert!(!wire.contains(&public));
        assert!(!wire.contains("signature"));
        assert!(wire.contains("native_session_protected"));
    }
    // Ordered public marker drains all tap traffic produced during bootstrap.
    let mut marker = broker.tcp().await;
    register(&mut marker, "tap-marker").await;
    send(
        &mut marker,
        &request("probe.marker", "tap-marker", "tap-end-marker")
            .with_header("type", "event")
            .with_body("tap-end-marker"),
    )
    .await;
    receive(&mut marker).await;
    loop {
        let frame = receive(&mut tap).await;
        assert!(!frame.to_wire().contains("noded.session."));
        assert!(!frame.to_wire().contains(&public));
        assert!(!frame.to_wire().contains(&private));
        if frame.body.trim() == "tap-end-marker" {
            break;
        }
    }
    let output = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(!output.contains(&private));
    assert!(!output.contains(&public));
    parent.client().close().await;
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

async fn verified(broker: &Broker) -> cosmix_client::VerifiedConnection {
    let cosmix_client::UnixConnectOutcome::VerifiedUnix(c) =
        cosmix_client::NodedClient::connect_unix("", &broker.url, &client_options(broker), None)
            .await
            .unwrap()
    else {
        panic!("Unix required")
    };
    c
}

#[tokio::test]
async fn typed_session_parent_resume_wakes_child_and_discovery_is_uid_gated() {
    use cosmix_bus::native_session::*;
    use ed25519_dalek::SigningKey;
    use sha2::{Digest, Sha256};
    let broker = Broker::start().await;
    let parent = verified(&broker).await;
    let parent_key = SigningKey::from_bytes(&rand::random());
    let parent_record = parent
        .session_allocate(&parent_key, Policy::Restricted)
        .await
        .unwrap()
        .record;
    let key = SigningKey::from_bytes(&rand::random());
    let public_key = HexBytes(key.verifying_key().to_bytes());
    let granted = parent
        .session_grant_create(&GrantCreateArgs {
            parent: parent_record.reference(),
            pane_id: DecimalU64(7),
            pane_generation: DecimalU64(1),
            public_key,
            role: Role::PaneShell,
            capabilities: vec![Capability::Input],
        })
        .await
        .unwrap();
    assert_eq!(
        parent.session_grant_fetch(public_key).await.unwrap().grant,
        granted.grant
    );
    let child = verified(&broker).await;
    let selector = ChallengeArgs::Key(KeyChallenge {
        public_key,
        purpose: Purpose::Enrol,
    });
    let mut scope = cosmix_client::session::ExpectedScope {
        broker_epoch: child.session_hello().await.unwrap().broker_epoch,
        purpose: Purpose::Enrol,
        unix_uid: parent_record.owner_uid,
        parent_key_hash: Some(HexBytes(
            Sha256::digest(parent_key.verifying_key().to_bytes()).into(),
        )),
        pane_id: Some(DecimalU64(7)),
        pane_high_water: None,
        role: Role::PaneShell,
        public_key_hash: HexBytes(Sha256::digest(public_key.0).into()),
        capabilities_hash: HexBytes(
            Sha256::digest(encode_capabilities(&[Capability::Input]).unwrap()).into(),
        ),
    };
    let challenge = child.session_challenge(&selector).await.unwrap();
    let mut wrong_context = scope.clone();
    wrong_context.purpose = Purpose::Resume;
    assert!(matches!(
        challenge.sign(&key, &wrong_context),
        Err(cosmix_client::session::SessionFailure::ScopeMismatch)
    ));
    wrong_context = scope.clone();
    wrong_context.broker_epoch.0[0] ^= 1;
    assert!(matches!(
        challenge.sign(&key, &wrong_context),
        Err(cosmix_client::session::SessionFailure::ScopeMismatch)
    ));
    let mut higher_scope = scope.clone();
    higher_scope.pane_high_water = Some(DecimalU64(
        challenge.transcript.pane_generation.unwrap().0 + 1,
    ));
    assert!(matches!(
        challenge.sign(&key, &higher_scope),
        Err(cosmix_client::session::SessionFailure::ScopeMismatch)
    ));
    higher_scope.pane_high_water = challenge.transcript.pane_generation;
    assert!(challenge.sign(&key, &higher_scope).is_ok());
    let proof = challenge.sign(&key, &scope).unwrap();
    let record = child.session_prove(&proof).await.unwrap().record;
    scope.purpose = Purpose::Resume;
    child.session_renew(record.reference()).await.unwrap();
    let mut legacy = broker.tcp().await;
    send(&mut legacy, &request("noded.list", "noded", "list")).await;
    let list: serde_json::Value = serde_json::from_str(&receive(&mut legacy).await.body).unwrap();
    assert!(
        list.as_array()
            .unwrap()
            .contains(&serde_json::json!(record.name))
    );
    let discovery = parent.client().service_inventory().await.unwrap();
    assert!(discovery.iter().any(|s| {
        s.native_session
            .as_ref()
            .is_some_and(|r| r.record_id == record.record_id)
    }));
    let mut waiting = verified(&broker).await;
    // Register interest before parent loss; this first challenge is consumed
    // by an invalid proof so it cannot mask the post-resume fresh transcript.
    let wake_challenge = waiting.session_challenge(&selector).await.unwrap();
    assert!(
        waiting
            .session_prove(&ProveArgs {
                challenge_id: wake_challenge.transcript.challenge_id,
                signature: HexBytes([0; 64])
            })
            .await
            .is_err()
    );
    parent.client().close().await;
    // Wait for the affected child's suspension notice on its key-interest lane.
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(3), waiting.recv())
            .await
            .unwrap()
            .unwrap();
        if event.command().command == "noded.session.lifecycle" {
            break;
        }
    }
    // A refused resume consumes the proof slot but must preserve the child's
    // key interest, so the later parent resume can wake it without re-granting.
    let suspended_challenge = waiting.session_challenge(&selector).await.unwrap();
    assert!(matches!(
        waiting
            .session_prove(&suspended_challenge.sign(&key, &scope).unwrap())
            .await,
        Err(cosmix_client::session::SessionFailure::Refused {
            error: SessionError {
                error_code: ErrorCode::Expired,
                ..
            },
            ..
        })
    ));
    let replacement = verified(&broker).await;
    let challenge = replacement
        .session_challenge(&ChallengeArgs::Key(KeyChallenge {
            public_key: HexBytes(parent_key.verifying_key().to_bytes()),
            purpose: Purpose::Enrol,
        }))
        .await
        .unwrap();
    let parent_scope = cosmix_client::session::ExpectedScope {
        broker_epoch: replacement.session_hello().await.unwrap().broker_epoch,
        purpose: Purpose::Resume,
        unix_uid: parent_record.owner_uid,
        parent_key_hash: None,
        pane_id: None,
        pane_high_water: None,
        role: Role::Term,
        public_key_hash: HexBytes(Sha256::digest(parent_key.verifying_key().to_bytes()).into()),
        capabilities_hash: HexBytes(
            Sha256::digest(encode_capabilities(&parent_record.capabilities).unwrap()).into(),
        ),
    };
    replacement
        .session_prove(&challenge.sign(&parent_key, &parent_scope).unwrap())
        .await
        .unwrap();
    let event = tokio::time::timeout(std::time::Duration::from_secs(3), waiting.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.command().command, "noded.session.lifecycle");
    let challenge = waiting.session_challenge(&selector).await.unwrap();
    let resumed = waiting
        .session_prove(&challenge.sign(&key, &scope).unwrap())
        .await
        .unwrap();
    assert_eq!(resumed.record.binding_generation, DecimalU64(2));
    assert_eq!(resumed.record.pane_id, Some(DecimalU64(7)));
    replacement.client().close().await;
    waiting.client().close().await;
    child.client().close().await;
}

#[tokio::test]
async fn p0i_04_captured_child_proof_fails_after_revoke_and_restart_fresh_enrol_succeeds() {
    use cosmix_bus::native_session::*;
    use ed25519_dalek::SigningKey;
    use sha2::{Digest, Sha256};
    let parent_key = SigningKey::from_bytes(&rand::random());
    let child_key = SigningKey::from_bytes(&rand::random());
    let public_key = HexBytes(child_key.verifying_key().to_bytes());
    let mut captured = None;
    let mut epoch = None;
    for _ in 0..2 {
        let broker = Broker::start().await;
        let parent = verified(&broker).await;
        let record = parent
            .session_allocate(&parent_key, Policy::Restricted)
            .await
            .unwrap()
            .record;
        if let Some(old) = epoch {
            assert_ne!(record.broker_epoch, old);
        }
        epoch = Some(record.broker_epoch);
        let granted = parent
            .session_grant_create(&GrantCreateArgs {
                parent: record.reference(),
                pane_id: DecimalU64(9),
                pane_generation: DecimalU64(1),
                public_key,
                role: Role::PaneShell,
                capabilities: vec![Capability::Input],
            })
            .await
            .unwrap();
        let child = verified(&broker).await;
        if let Some(proof) = &captured {
            assert!(child.session_prove(proof).await.is_err());
        }
        let mut scope = cosmix_client::session::ExpectedScope {
            broker_epoch: child.session_hello().await.unwrap().broker_epoch,
            purpose: Purpose::Enrol,
            unix_uid: record.owner_uid,
            parent_key_hash: Some(granted.grant.parent_key_hash),
            pane_id: Some(DecimalU64(9)),
            pane_high_water: None,
            role: Role::PaneShell,
            public_key_hash: HexBytes(Sha256::digest(public_key.0).into()),
            capabilities_hash: HexBytes(
                Sha256::digest(encode_capabilities(&[Capability::Input]).unwrap()).into(),
            ),
        };
        let challenge = child
            .session_challenge(&ChallengeArgs::Key(KeyChallenge {
                public_key,
                purpose: Purpose::Enrol,
            }))
            .await
            .unwrap();
        let proof = challenge.sign(&child_key, &scope).unwrap();
        let attached = child.session_prove(&proof).await.unwrap();
        scope.purpose = Purpose::Resume;
        assert_eq!(attached.record.binding_generation, DecimalU64(1));
        assert!(child.session_prove(&proof).await.is_err());
        child.client().close().await;
        let resumed = verified(&broker).await;
        let selector = ChallengeArgs::Key(KeyChallenge {
            public_key,
            purpose: Purpose::Enrol,
        });
        let before_replay = resumed.session_challenge(&selector).await.unwrap();
        assert!(resumed.session_prove(&proof).await.is_err());
        // Even a replay naming an old challenge consumes this connection's
        // outstanding slot. Neither captured bytes nor that slot can bind it.
        assert!(
            resumed
                .session_prove(&before_replay.sign(&child_key, &scope).unwrap())
                .await
                .is_err()
        );
        let fresh = resumed.session_challenge(&selector).await.unwrap();
        let rival = verified(&broker).await;
        let competing = rival.session_challenge(&selector).await.unwrap();
        assert_eq!(
            competing.transcript.binding_generation,
            fresh.transcript.binding_generation
        );
        let attached = resumed
            .session_prove(&fresh.sign(&child_key, &scope).unwrap())
            .await
            .unwrap();
        assert_eq!(attached.record.binding_generation, DecimalU64(2));
        assert!(matches!(
            rival
                .session_prove(&competing.sign(&child_key, &scope).unwrap())
                .await,
            Err(cosmix_client::session::SessionFailure::Refused {
                error: SessionError {
                    error_code: ErrorCode::StaleGeneration,
                    ..
                },
                ..
            })
        ));
        // Losing proofs and delayed cleanup cannot remove the winning channel.
        resumed
            .session_renew(attached.record.reference())
            .await
            .unwrap();
        rival.client().close().await;
        assert!(
            parent
                .session_revoke(attached.record.reference())
                .await
                .unwrap()
                .revoked
        );
        let fresh_connection = verified(&broker).await;
        assert!(fresh_connection.session_prove(&proof).await.is_err());
        assert!(
            fresh_connection
                .session_challenge(&ChallengeArgs::Key(KeyChallenge {
                    public_key,
                    purpose: Purpose::Enrol
                }))
                .await
                .is_err()
        );
        captured = Some(proof);
        parent.client().close().await;
        child.client().close().await;
        resumed.client().close().await;
        fresh_connection.client().close().await;
    }
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
