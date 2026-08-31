//! WS1 load-bearing proof (SPEC 18 §3.3): the supervised client
//! survives a broker bounce, **re-registers**, **replays the
//! subscription registry in recorded order**, and the outward incoming
//! stream **survives the drop** (consult BLOCKER 2 — a transient drop is
//! never observed as the sticky terminal close).
//!
//! There is no embeddable `cosmix-noded` (it is a binary crate), so this
//! drives a minimal in-process WebSocket stub broker. The full live
//! `cosmix-noded`-restart acceptance is WS8; this pins the WS1 mechanism
//! deterministically.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use cosmix_bus::bus::{self, BusMessage};
use cosmix_client::{ConnState, NameCollision, NodedClient, SupervisedClient};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Notify};
use tokio_tungstenite::tungstenite::Message;

#[derive(Default)]
struct StubState {
    /// `from` of every `noded.register` request, in order — one per
    /// connection, so length == number of (re-)registrations.
    register_names: Vec<String>,
    /// Body (RegisterProvenance JSON) of every `noded.register`, in order
    /// — proves provenance is re-sent on reconnect, not only initial.
    register_bodies: Vec<String>,
    /// `name` header of every RC-0 `topic.subscribe`, in order — proves
    /// replay order.
    subscribed: Vec<String>,
    /// `name` header of every RC-0 `topic.unsubscribe`, in order —
    /// proves WS2 `unsubscribe_topic` reaches the broker.
    unsubscribed: Vec<String>,
    /// Count of `topic.subscribe` requests seen (incl. rejected) —
    /// proves the supervisor keeps retrying replay.
    subscribe_attempts: usize,
    /// `noded.deregister` seen.
    deregistered: bool,
    /// Connections accepted so far (cumulative — only grows).
    connections: usize,
    /// Connections currently open (incremented on accept, decremented
    /// when the per-connection task observes WS close). A reconnect
    /// socket the supervised client *leaked* (never `close()`d) never
    /// decrements this, so it climbs ~1:1 with `connections` — that is
    /// the signal the leak regression test asserts against.
    open_connections: usize,
    /// Active registered name → owning connection index. Models real
    /// `cosmix-noded`'s per-channel registry (WS0 channel-scoped
    /// remove_peer): a name maps to exactly one open connection, is
    /// removed when that connection's WebSocket closes, and a
    /// duplicate `noded.register` for a name another *open* connection
    /// still holds is rejected as a collision. This is what makes a
    /// leaked (never-closed) reconnect socket observable — the next
    /// `noded.register` would collide instead of silently succeeding.
    active_registrations: HashMap<String, usize>,
}

struct Stub {
    state: Mutex<StubState>,
    /// Fired by the test to make connection #1 drop (simulated bounce).
    drop_conn1: Notify,
    /// When true, `topic.subscribe` on reconnected sockets (conn ≥ 2) is
    /// rejected (rc=10) — exercises the MAJOR fix: a failed replay must
    /// NOT flip the citizen to Connected.
    fail_replay_on_reconnect: bool,
    /// When true, `noded.register` on reconnected sockets (conn ≥ 2) is
    /// rejected (rc=10) but the connection is kept **open** — exactly
    /// real `cosmix-noded` behavior on a name collision. Drives the
    /// supervisor's `connect()`-register-failure path so the leak
    /// regression test can prove failed reconnects don't accumulate
    /// open sockets/reader tasks.
    reject_register_on_reconnect: bool,
    /// If set, `topic.subscribe`/`topic.unsubscribe` for this exact
    /// `name` is answered `rc=10` on **every** connection (not just
    /// reconnects). Models a reserved-topic refusal (SPEC 12 §15.5) so
    /// WS2's "rejected (un)subscribe leaves the registry unchanged"
    /// transactional contract is testable on the first connection.
    reject_topic: Option<String>,
}

impl Stub {
    fn new(fail_replay_on_reconnect: bool, reject_register_on_reconnect: bool) -> Arc<Stub> {
        Arc::new(Stub {
            state: Mutex::new(StubState::default()),
            drop_conn1: Notify::new(),
            fail_replay_on_reconnect,
            reject_register_on_reconnect,
            reject_topic: None,
        })
    }

    /// A stub that refuses one specific topic name with `rc=10` on
    /// every connection (everything else succeeds).
    fn rejecting(topic: &str) -> Arc<Stub> {
        Arc::new(Stub {
            state: Mutex::new(StubState::default()),
            drop_conn1: Notify::new(),
            fail_replay_on_reconnect: false,
            reject_register_on_reconnect: false,
            reject_topic: Some(topic.to_string()),
        })
    }
}

/// Build a `type=response` reply correlated to `req`.
fn reply(req: &BusMessage, rc: &str) -> String {
    let mut m = BusMessage::new()
        .with_header("type", "response")
        .with_header("command", req.get("command").unwrap_or("?"))
        .with_header("from", "noded")
        .with_header("rc", rc);
    if let Some(id) = req.get("id") {
        m = m.with_header("id", id);
    }
    m.to_wire()
}

fn collision_reply(req: &BusMessage) -> String {
    let mut response = bus::parse(&reply(req, "10")).expect("stub reply parses");
    response.set("error", "stub collision diagnostic wording");
    response.to_wire()
}

async fn run_stub(listener: TcpListener, stub: Arc<Stub>) {
    while let Ok((tcp, _)) = listener.accept().await {
        let stub = stub.clone();
        let conn_index = {
            let mut s = stub.state.lock().await;
            s.connections += 1;
            s.open_connections += 1;
            s.connections
        };
        tokio::spawn(async move {
            let ws = match tokio_tungstenite::accept_async(tcp).await {
                Ok(w) => w,
                Err(_) => return,
            };
            let (mut sink, mut stream) = ws.split();

            'conn: loop {
                let text = tokio::select! {
                    // Connection #1 is force-dropped when the test fires
                    // `drop_conn1` — this is the simulated broker bounce.
                    _ = stub.drop_conn1.notified(), if conn_index == 1 => {
                        let _ = sink.close().await;
                        break 'conn;
                    }
                    msg = stream.next() => match msg {
                        Some(Ok(Message::Text(t))) => t.to_string(),
                        Some(Ok(Message::Close(_))) | None => break 'conn,
                        Some(Ok(_)) => continue,
                        Some(Err(_)) => break 'conn,
                    },
                };

                let req = match bus::parse(&text) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let command = req.get("command").unwrap_or("").to_string();
                match command.as_str() {
                    "noded.register" => {
                        let from = req.get("from").unwrap_or("").to_string();
                        // Reject (rc=10) when either: the test forces a
                        // reconnect-register rejection, OR the name is
                        // held by a *different still-open* connection (a
                        // leaked, never-`close()`d reconnect socket
                        // would keep its name here). Real `cosmix-noded`
                        // returns rc=10 and KEEPS the connection open —
                        // it does not hang up — so we must not close the
                        // socket server-side; closing it would mask a
                        // client that leaked the socket after its
                        // register failed.
                        let forced_reject = stub.reject_register_on_reconnect && conn_index >= 2;
                        let collision = {
                            let mut s = stub.state.lock().await;
                            s.register_names.push(from.clone());
                            s.register_bodies.push(req.body.clone());
                            if forced_reject {
                                true
                            } else {
                                match s.active_registrations.get(&from) {
                                    Some(&owner) if owner != conn_index => true,
                                    _ => {
                                        s.active_registrations.insert(from.clone(), conn_index);
                                        false
                                    }
                                }
                            }
                        };
                        if collision {
                            let _ = sink.send(Message::Text(collision_reply(&req).into())).await;
                            // KEEP the connection open (real noded
                            // behavior). The client's `connect()` sees
                            // rc=10, fails register, and must `close()`
                            // the half-built client itself — if it
                            // leaks instead, this socket stays open and
                            // `open_connections` climbs (the leak
                            // regression assertion).
                            continue;
                        }
                        let _ = sink.send(Message::Text(reply(&req, "0").into())).await;
                        // On the *reconnected* socket, push an
                        // unsolicited request AFTER re-register: it must
                        // surface on the SAME incoming receiver the test
                        // took before the drop (BLOCKER 2).
                        if conn_index >= 2 {
                            let ping = BusMessage::new()
                                .with_header("type", "request")
                                .with_header("command", "world.test.ping")
                                .with_header("from", "noded")
                                .with_header("id", "ping-1")
                                .to_wire();
                            let _ = sink.send(Message::Text(ping.into())).await;
                        }
                    }
                    "topic.subscribe" => {
                        let name = req.get("name").unwrap_or("").to_string();
                        let reject = (stub.fail_replay_on_reconnect && conn_index >= 2)
                            || stub.reject_topic.as_deref() == Some(name.as_str());
                        {
                            let mut s = stub.state.lock().await;
                            s.subscribe_attempts += 1;
                            if !reject {
                                s.subscribed.push(name);
                            }
                        }
                        let rc = if reject { "10" } else { "0" };
                        let _ = sink.send(Message::Text(reply(&req, rc).into())).await;
                    }
                    "topic.unsubscribe" => {
                        let name = req.get("name").unwrap_or("").to_string();
                        let reject = stub.reject_topic.as_deref() == Some(name.as_str());
                        if !reject {
                            stub.state.lock().await.unsubscribed.push(name);
                        }
                        let rc = if reject { "10" } else { "0" };
                        let _ = sink.send(Message::Text(reply(&req, rc).into())).await;
                    }
                    "noded.deregister" => {
                        stub.state.lock().await.deregistered = true;
                        let _ = sink.send(Message::Text(reply(&req, "0").into())).await;
                    }
                    _ => {
                        let _ = sink.send(Message::Text(reply(&req, "0").into())).await;
                    }
                }
            }

            // Broker-side unregister on WS close (WS0 channel-scoped):
            // every name this connection held is freed once its socket
            // closes, and the open-connection count drops. A reconnect
            // socket the supervised client *leaked* (never `close()`d)
            // never reaches here, so its name stays in
            // `active_registrations` and `open_connections` is never
            // decremented — that is the signal the leak regression
            // tests assert against.
            {
                let mut s = stub.state.lock().await;
                s.active_registrations
                    .retain(|_, owner| *owner != conn_index);
                s.open_connections = s.open_connections.saturating_sub(1);
            }
        });
    }
}

/// Poll `cond` until true or `secs` elapse.
async fn wait_until<F: Fn() -> bool>(secs: u64, cond: F) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while tokio::time::Instant::now() < deadline {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    cond()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnects_reregisters_replays_and_keeps_incoming_stream() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let stub = Stub::new(false, false);
    tokio::spawn(run_stub(listener, stub.clone()));

    let url = format!("ws://127.0.0.1:{port}/ws");
    let client = SupervisedClient::connect_supervised("cosmix-statecache", &url)
        .await
        .expect("initial connect");
    assert_eq!(client.state(), ConnState::Connected);
    assert_eq!(client.connection_generation(), 1);
    let mut states = client.subscribe_state();

    // Take the outward incoming stream BEFORE the bounce — it must
    // survive the reconnect.
    let mut incoming = client.incoming().expect("incoming taken once");

    // WS2: subscribe through the real chokepoint so reconnect has
    // something to replay (in recorded order). RC-0 → recorded.
    client
        .subscribe_topic("world.statecache.probe")
        .await
        .expect("subscribe while Connected");
    {
        let s = stub.state.lock().await;
        assert_eq!(
            s.subscribed,
            vec!["world.statecache.probe".to_string()],
            "subscribe must reach the broker before the bounce"
        );
    }

    {
        let s = stub.state.lock().await;
        assert_eq!(s.connections, 1);
        assert_eq!(s.register_names, vec!["cosmix-statecache".to_string()]);
    }

    // Induce the bounce.
    // `notify_one` retains a permit if the connection task is between loop
    // iterations. `notify_waiters` would lose this bounce in that window.
    stub.drop_conn1.notify_one();

    // Wait on the supervisor's explicit state edge rather than racing it with
    // wall-clock polling. The generation advances immediately before the
    // Connected publication, so this also proves re-registration and replay
    // completed on connection #2.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            states.changed().await.expect("state sender remains live");
            if *states.borrow_and_update() == ConnState::Connected
                && client.connection_generation() >= 2
            {
                break;
            }
        }
    })
    .await
    .expect("expected a reconnect with re-registration and replay");
    {
        let s = stub.state.lock().await;
        assert_eq!(
            s.register_names,
            vec![
                "cosmix-statecache".to_string(),
                "cosmix-statecache".to_string()
            ],
            "must re-register under the same name"
        );
        assert_eq!(
            s.subscribed,
            vec![
                // conn 1: the explicit WS2 subscribe_topic call.
                "world.statecache.probe".to_string(),
                // conn 2: the supervisor's registry replay.
                "world.statecache.probe".to_string(),
            ],
            "registry must be replayed on reconnect (recorded order)"
        );
    }

    // The SAME receiver, taken before the drop, yields the post-bounce
    // unsolicited request — the stream was replaceable, not terminal.
    let got = tokio::time::timeout(Duration::from_secs(5), incoming.recv())
        .await
        .expect("incoming stream must survive the reconnect (not terminal)")
        .expect("a command, not channel close");
    assert_eq!(got.command, "world.test.ping");

    // Outbound while Connected works.
    let r = client
        .call_with_headers(
            "noded",
            "topic.subscriber_count",
            &BTreeMap::from([("name".to_string(), "x".to_string())]),
            "",
        )
        .await;
    assert!(r.is_ok(), "outbound call while Connected: {r:?}");

    // Graceful deregister: stops the supervisor and issues the RPC.
    client.deregister().await.expect("deregister");
    assert!(
        stub.state.lock().await.deregistered,
        "broker must observe noded.deregister before replying"
    );
    assert_eq!(client.state(), ConnState::ShuttingDown);
}

/// Version-discovery contract: provenance passed to
/// `connect_supervised_with_provenance` is sent on the INITIAL register
/// AND re-sent on every reconnect (built once, cloned from SupervisorCtx).
/// A regression that sent it only on the first connect would leave a
/// reconnected citizen provenance-less in `noded.list`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnect_resends_provenance() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let stub = Stub::new(false, false);
    tokio::spawn(run_stub(listener, stub.clone()));

    let url = format!("ws://127.0.0.1:{port}/ws");
    let prov = cosmix_bus::RegisterProvenance::from_parts(
        "cosmix-mix",
        "9.9.9-test",
        "deadbeefcafe",
        false,
        "2026-01-01T00:00:00Z",
        "2026-01-01T00:00:00Z".to_string(),
    );
    let client =
        SupervisedClient::connect_supervised_with_provenance("cosmix-statecache", &url, Some(prov))
            .await
            .expect("initial connect");
    assert_eq!(client.state(), ConnState::Connected);

    // Initial register carried the provenance body.
    {
        let s = stub.state.lock().await;
        assert_eq!(s.register_bodies.len(), 1);
        assert!(
            s.register_bodies[0].contains("9.9.9-test"),
            "initial register must carry provenance: {:?}",
            s.register_bodies[0]
        );
    }

    // Bounce → reconnect → the SECOND register must carry it too.
    stub.drop_conn1.notify_one();
    let stub2 = stub.clone();
    assert!(
        wait_until(10, || {
            stub2
                .state
                .try_lock()
                .map(|s| s.register_bodies.len() >= 2)
                .unwrap_or(false)
        })
        .await,
        "expected a reconnect re-register"
    );
    {
        let s = stub.state.lock().await;
        assert!(
            s.register_bodies[1].contains("9.9.9-test"),
            "reconnect register must RE-SEND provenance: {:?}",
            s.register_bodies[1]
        );
    }
    // (No deregister: the contract under test — provenance re-sent on
    // reconnect — is already asserted; the client drops at scope end.)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initial_connect_failure_is_typed_fatal() {
    // Nothing listening on this port → bounded budget exhausts → typed
    // fatal (SPEC 18 §3.1: serve mode exits non-zero).
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener); // free the port; connect must fail

    let url = format!("ws://127.0.0.1:{port}/ws");
    let err = match SupervisedClient::connect_supervised("cosmix-statecache", &url).await {
        Ok(_) => panic!("connect to a dead port must fail"),
        Err(e) => e,
    };
    match err {
        cosmix_client::SupervisedError::InitialConnectFailed { attempts, .. } => {
            assert_eq!(attempts, cosmix_client::MAX_INITIAL_ATTEMPTS);
        }
        other => panic!("expected InitialConnectFailed, got {other}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registration_rejection_preserves_rc_and_message_without_claiming_collision() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let stub = Stub::new(false, false);
    tokio::spawn(run_stub(listener, stub));

    let url = format!("ws://127.0.0.1:{port}/ws");
    let owner = NodedClient::connect("taken-service", &url)
        .await
        .expect("first registration owns the name");
    let error = match NodedClient::connect("taken-service", &url).await {
        Ok(_) => panic!("duplicate registration must fail"),
        Err(error) => error,
    };
    let (rc, message) = NodedClient::registration_rejection(&error)
        .expect("registration error keeps a typed rejection cause");
    assert_eq!(rc, 10);
    assert_eq!(message, "stub collision diagnostic wording");
    assert!(
        error.downcast_ref::<NameCollision>().is_none(),
        "rc=10 plus diagnostic text is not a machine-readable collision code"
    );

    owner.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_state_watch_publishes_disconnect_and_reconnect_edges() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let stub = Stub::new(false, false);
    let acceptor = tokio::spawn(run_stub(listener, stub.clone()));

    let url = format!("ws://{address}/ws");
    let client = SupervisedClient::connect_supervised("watched-service", &url)
        .await
        .expect("initial connect");
    let mut states = client.subscribe_state();
    assert_eq!(*states.borrow(), ConnState::Connected);

    acceptor.abort();
    stub.drop_conn1.notify_one();
    tokio::time::timeout(Duration::from_secs(5), states.changed())
        .await
        .expect("disconnect edge timeout")
        .expect("state sender remains live");
    assert_eq!(*states.borrow_and_update(), ConnState::Disconnected);

    let listener = TcpListener::bind(address)
        .await
        .expect("restart stub broker on the same address");
    tokio::spawn(run_stub(listener, stub));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            states.changed().await.expect("state sender remains live");
            if *states.borrow_and_update() == ConnState::Connected {
                break;
            }
        }
    })
    .await
    .expect("reconnect edge timeout");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outbound_while_disconnected_fails_fast_typed() {
    // Bring up, then permanently kill the broker; the next outbound
    // call must fail fast with the typed Disconnected error — no queue.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let stub = Stub::new(false, false);
    let handle = tokio::spawn(run_stub(listener, stub.clone()));

    let url = format!("ws://127.0.0.1:{port}/ws");
    let client = SupervisedClient::connect_supervised("cosmix-statecache", &url)
        .await
        .expect("initial connect");

    // Drop conn1 and stop accepting (abort the listener task) so the
    // supervisor can never reconnect — steady-state Disconnected.
    stub.drop_conn1.notify_one();
    handle.abort();

    assert!(
        wait_until(10, || client.state() == ConnState::Disconnected).await,
        "should settle into Disconnected with no broker"
    );

    let err = client
        .call("noded", "noded.list", serde_json::Value::Null)
        .await
        .expect_err("outbound while disconnected must error, not queue");
    assert!(
        matches!(err, cosmix_client::SupervisedError::Disconnected),
        "expected typed Disconnected, got {err}"
    );
}

// ── WS2: the subscribe/unsubscribe transactional chokepoint ──

/// SPEC 18 §3.3 / plan MAJOR 1: an RC-0 `topic.subscribe` records the
/// topic into the registry in first-seen order; that is exactly what
/// gets replayed on reconnect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscribe_records_only_after_rc0() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let stub = Stub::new(false, false);
    tokio::spawn(run_stub(listener, stub.clone()));

    let url = format!("ws://127.0.0.1:{port}/ws");
    let client = SupervisedClient::connect_supervised("cosmix-statecache", &url)
        .await
        .expect("initial connect");

    client
        .subscribe_topic("world.a")
        .await
        .expect("subscribe a");
    client
        .subscribe_topic("world.b")
        .await
        .expect("subscribe b");
    // Duplicate: harmless no-op, registry dedups, no reorder.
    client
        .subscribe_topic("world.a")
        .await
        .expect("duplicate subscribe a is a no-op");

    assert_eq!(
        client.subscription_registry().snapshot(),
        vec!["world.a".to_string(), "world.b".to_string()],
        "registry records RC-0 subscribes in first-seen order, deduped"
    );
    let s = stub.state.lock().await;
    assert_eq!(
        s.subscribed,
        vec![
            "world.a".to_string(),
            "world.b".to_string(),
            "world.a".to_string()
        ],
        "every subscribe (incl. the duplicate) reaches the broker"
    );
}

/// The load-bearing WS2 invariant (plan MAJOR 1): a **rejected**
/// subscribe (broker `rc=10` — e.g. a reserved/refused topic) returns
/// the typed error and leaves the registry **unchanged**. Recording an
/// unsatisfiable topic would replay it forever on every reconnect — the
/// `feedback_refactor_silent_noop_audit` partial-truth shape.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_subscribe_leaves_registry_unchanged() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let stub = Stub::rejecting("reserved.x");
    tokio::spawn(run_stub(listener, stub.clone()));

    let url = format!("ws://127.0.0.1:{port}/ws");
    let client = SupervisedClient::connect_supervised("cosmix-statecache", &url)
        .await
        .expect("initial connect");

    client
        .subscribe_topic("ok.a")
        .await
        .expect("subscribe ok.a");
    let err = client
        .subscribe_topic("reserved.x")
        .await
        .expect_err("a reserved-topic subscribe must error, not record");
    assert!(
        matches!(err, cosmix_client::SupervisedError::Transport(_)),
        "broker rc=10 surfaces as a typed transport error, got {err}"
    );

    assert_eq!(
        client.subscription_registry().snapshot(),
        vec!["ok.a".to_string()],
        "rejected topic must NOT be recorded (no forever-replay of an \
         unsatisfiable topic)"
    );
    assert_eq!(
        stub.state.lock().await.subscribe_attempts,
        2,
        "both subscribes were attempted on the wire; only the RC-0 one recorded"
    );
}

/// Symmetric to record-on-RC-0: an RC-0 `topic.unsubscribe` removes the
/// topic so a later reconnect does **not** re-subscribe a deliberately
/// dropped topic; a failed unsubscribe leaves it recorded (the safe
/// direction — over-deliver, never go silently deaf).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsubscribe_removes_only_after_rc0() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let stub = Stub::new(false, false);
    tokio::spawn(run_stub(listener, stub.clone()));

    let url = format!("ws://127.0.0.1:{port}/ws");
    let client = SupervisedClient::connect_supervised("cosmix-statecache", &url)
        .await
        .expect("initial connect");

    client
        .subscribe_topic("world.a")
        .await
        .expect("subscribe a");
    client
        .subscribe_topic("world.b")
        .await
        .expect("subscribe b");
    client
        .unsubscribe_topic("world.a")
        .await
        .expect("unsubscribe a");

    assert_eq!(
        client.subscription_registry().snapshot(),
        vec!["world.b".to_string()],
        "RC-0 unsubscribe forgets the topic; replay set shrinks"
    );
    assert_eq!(
        stub.state.lock().await.unsubscribed,
        vec!["world.a".to_string()],
        "unsubscribe reached the broker"
    );
}

/// A **failed** unsubscribe (broker `rc=10`) returns the typed error
/// and leaves the topic recorded — it must still be replayed. Seeded
/// directly into the registry so the failure path is isolated from the
/// subscribe path (the stub refuses the same name for both verbs).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_unsubscribe_leaves_registry_unchanged() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let stub = Stub::rejecting("keep");
    tokio::spawn(run_stub(listener, stub.clone()));

    let url = format!("ws://127.0.0.1:{port}/ws");
    let client = SupervisedClient::connect_supervised("cosmix-statecache", &url)
        .await
        .expect("initial connect");

    // Seed the registry as if a prior RC-0 subscribe had recorded it.
    client.subscription_registry().record("keep");

    let err = client
        .unsubscribe_topic("keep")
        .await
        .expect_err("a rejected unsubscribe must error");
    assert!(
        matches!(err, cosmix_client::SupervisedError::Transport(_)),
        "broker rc=10 surfaces as a typed transport error, got {err}"
    );
    assert_eq!(
        client.subscription_registry().snapshot(),
        vec!["keep".to_string()],
        "a failed unsubscribe must NOT forget the topic (stay subscribed)"
    );
}

/// §3.3 fail-fast / no-queue: a subscribe issued while `Disconnected`
/// errors immediately with the typed error and records nothing — there
/// is no outbound buffer (the forbidden partial-truth queue).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscribe_while_disconnected_fails_fast() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let stub = Stub::new(false, false);
    let handle = tokio::spawn(run_stub(listener, stub.clone()));

    let url = format!("ws://127.0.0.1:{port}/ws");
    let client = SupervisedClient::connect_supervised("cosmix-statecache", &url)
        .await
        .expect("initial connect");

    stub.drop_conn1.notify_one();
    handle.abort();
    assert!(
        wait_until(10, || client.state() == ConnState::Disconnected).await,
        "should settle into Disconnected with no broker"
    );

    let err = client
        .subscribe_topic("world.late")
        .await
        .expect_err("subscribe while disconnected must error, not queue");
    assert!(
        matches!(err, cosmix_client::SupervisedError::Disconnected),
        "expected typed Disconnected, got {err}"
    );
    assert!(
        client.subscription_registry().is_empty(),
        "a gated-off subscribe must record nothing"
    );
}

/// MAJOR regression (Codex): a failed subscription replay must NOT flip
/// the citizen to `Connected`. A live-but-silently-unsubscribed daemon
/// is the `feedback_refactor_silent_noop_audit` partial-truth shape —
/// §3.3's gate is *observable* re-subscribe. The supervisor must fail
/// the whole reconnect attempt, stay `Disconnected`, and keep retrying
/// register+replay.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_failure_keeps_disconnected_no_false_connected() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let stub = Stub::new(true, false); // reject topic.subscribe on conn ≥ 2
    tokio::spawn(run_stub(listener, stub.clone()));

    let url = format!("ws://127.0.0.1:{port}/ws");
    let client = SupervisedClient::connect_supervised("cosmix-statecache", &url)
        .await
        .expect("initial connect");
    let _incoming = client.incoming().expect("incoming taken once");
    client
        .subscription_registry()
        .record("world.statecache.probe");

    stub.drop_conn1.notify_one();

    // The supervisor must reconnect AND attempt replay repeatedly — each
    // rejected — without ever flipping to Connected.
    let stub2 = stub.clone();
    assert!(
        wait_until(10, || {
            stub2
                .state
                .try_lock()
                .map(|s| s.connections >= 3 && s.subscribe_attempts >= 2)
                .unwrap_or(false)
        })
        .await,
        "supervisor must keep retrying register+replay after rejection"
    );

    // Never Connected; no rejected topic recorded as subscribed.
    assert_ne!(
        client.state(),
        ConnState::Connected,
        "a partial/failed replay must NOT present as Connected"
    );
    let s = stub.state.lock().await;
    assert!(
        s.subscribed.is_empty(),
        "no rejected subscribe may count as established: {:?}",
        s.subscribed
    );
    assert!(
        s.register_names.len() >= 2,
        "must keep re-registering across retry attempts"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnect_registration_rejection_retries_by_default_without_leak() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let stub = Stub::new(false, true); // reject noded.register on conn ≥ 2
    tokio::spawn(run_stub(listener, stub.clone()));

    let url = format!("ws://127.0.0.1:{port}/ws");
    let client = SupervisedClient::connect_supervised("cosmix-statecache", &url)
        .await
        .expect("initial connect (conn 1) registers fine");
    let _incoming = client.incoming().expect("incoming taken once");

    stub.drop_conn1.notify_one();

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if stub.state.lock().await.register_names.len() >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("default policy retries rejected registrations");
    assert_ne!(client.state(), ConnState::Fatal);

    tokio::time::sleep(Duration::from_millis(300)).await;
    let s = stub.state.lock().await;
    assert!(s.connections >= 3, "registration rejection must be retried");
    assert!(
        s.register_names
            .iter()
            .all(|name| name == "cosmix-statecache"),
        "retry must not silently rename the service"
    );
    assert!(
        s.open_connections <= 1,
        "failed-register reconnects leaked sockets: {} open of {} total",
        s.open_connections,
        s.connections
    );
    drop(s);
    client.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnect_registration_rejection_is_terminal_when_opted_in() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let stub = Stub::new(false, true);
    tokio::spawn(run_stub(listener, stub.clone()));

    let url = format!("ws://127.0.0.1:{port}/ws");
    let client = SupervisedClient::connect_options("cosmix-statecache", &url)
        .fatal_on_registration_rejection(true)
        .connect()
        .await
        .expect("initial connect registers fine");
    let _incoming = client.incoming().expect("incoming taken once");
    let mut states = client.subscribe_state();

    stub.drop_conn1.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            states.changed().await.expect("state sender remains live");
            if *states.borrow_and_update() == ConnState::Fatal {
                break;
            }
        }
    })
    .await
    .expect("opt-in rejection policy publishes a terminal state");

    tokio::time::sleep(Duration::from_millis(300)).await;
    let s = stub.state.lock().await;
    assert_eq!(s.connections, 2, "opt-in rejection must not be retried");
    assert!(s.open_connections <= 1);
}
