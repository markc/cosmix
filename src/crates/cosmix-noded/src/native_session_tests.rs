//! p0i-06 S1: exercise real Axum listeners, routing and response ownership.
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
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let listen = probe.local_addr().unwrap().to_string();
        drop(probe);
        let root =
            std::env::temp_dir().join(format!("cosmix-native-{:032x}", rand::random::<u128>()));
        let (ready_tx, ready_rx) = oneshot::channel();
        let task = tokio::spawn(run(
            RunConfig {
                listen: listen.clone(),
                node: "test-node".into(),
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
    let mut service = broker.tcp().await;
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
    assert_eq!(
        read_principal(&reply).unwrap(),
        None,
        "TCP responder never receives Unix authority"
    );

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
    assert_eq!(read_principal(&reverse).unwrap(), None);
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
async fn unix_binary_frames_refused_and_bootstrap_is_strict_but_not_enabled_as_state_machine() {
    let broker = Broker::start().await;
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
    assert_eq!(reply.get("rc"), Some("10"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reply.body).unwrap()["error_code"],
        "UNSUPPORTED"
    );
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
    // Even a real stamp received by an ordinary TCP service remains raw data:
    // NodedClient/IncomingCommand provide no trusted-context accessor.
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
    assert!(raw.header("broker_principal").is_some());
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
