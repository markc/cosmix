//! Acceptance exercises production noded, Term's allocated route and actual
//! VerifiedConnection deliveries. No test calls the pure policy evaluator.
use super::*;
use crate::control::Target;
use crate::tabs::{Cleanup, TabSet};
use serde_json::{Value, json};
use std::sync::Mutex;
use term_native_test_broker::Broker;

const REQUIRE_MIX: &str = "requires clean current-HEAD COSMIX_E2E_MIX_BIN; explicit S4 gate only";

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}
async fn verified(broker: &Broker) -> VerifiedConnection {
    let UnixConnectOutcome::VerifiedUnix(c) =
        NodedClient::connect_unix("", &broker.url, &broker.options(), None)
            .await
            .unwrap()
    else {
        panic!("verified Unix required");
    };
    c
}
async fn call(client: &NodedClient, name: &str, verb: &str, mut body: Value) -> (u8, Value) {
    // Caller-side cache preserves the operation epoch after pane close. The
    // recipient still verifies this value against its broker-stamped actor.
    thread_local! {
        static EPOCHS: std::cell::RefCell<std::collections::HashMap<(usize, String), Value>> = std::cell::RefCell::new(std::collections::HashMap::new());
    }
    if body.get("request_id").is_some() && body.get("request_epoch").is_none() {
        // Keyed on the client ADDRESS, which is only unique while every
        // connection is held: a loop that builds one in the same stack slot
        // each iteration aliases the first entry. Rather than make callers
        // remember that, a cached epoch that turns out to be wrong is detected
        // and healed below, because the recipient refuses a mismatched epoch
        // before it executes anything.
        let key = (std::ptr::from_ref(client) as usize, name.to_string());
        let cached = EPOCHS.with(|epochs| epochs.borrow().get(&key).cloned());
        let from_cache = cached.is_some();
        let epoch = if let Some(epoch) = cached {
            epoch
        } else {
            let request = json!({"target":body["target"]}).to_string();
            let response = tokio::time::timeout(
                Duration::from_secs(6),
                client.call_with_headers_raw(name, "term.session", &Default::default(), &request),
            )
            .await;
            let epoch = match response {
                Ok(Ok((0, body, _))) => {
                    serde_json::from_str::<Value>(&body).unwrap()["request_epoch"].clone()
                }
                _ => json!(HexBytes([0u8; 16])),
            };
            EPOCHS.with(|epochs| epochs.borrow_mut().insert(key.clone(), epoch.clone()));
            epoch
        };
        body["request_epoch"] = epoch;
        if from_cache {
            // A stale cached epoch is refused BEFORE the recipient executes
            // anything, so re-probing and retrying once cannot double-apply.
            // A genuine unknown outcome answers the same way the second time.
            let first = raw(client, name, verb, &body).await;
            if first.1["error_code"] != "UNKNOWN_OUTCOME" {
                return first;
            }
            EPOCHS.with(|epochs| epochs.borrow_mut().remove(&key));
            let mut retry = body.clone();
            retry.as_object_mut().unwrap().remove("request_epoch");
            return Box::pin(call(client, name, verb, retry)).await;
        }
    }
    raw(client, name, verb, &body).await
}
async fn raw(client: &NodedClient, name: &str, verb: &str, body: &Value) -> (u8, Value) {
    let (rc, body, _) = tokio::time::timeout(
        Duration::from_secs(6),
        client.call_with_headers_raw(name, verb, &Default::default(), &body.to_string()),
    )
    .await
    .expect("real Term reply deadline")
    .expect("real Bus transport");
    (
        rc,
        serde_json::from_str(&body).expect("structured Term response"),
    )
}
/// libtest exits 0 when its filter matches nothing, so a renamed module or a
/// mistyped --exact path would leave a self-exec fixture passing while asserting
/// nothing at all. Require the child to report that it ran the one test.
fn ran_one_test(stdout: &[u8]) -> bool {
    String::from_utf8_lossy(stdout).contains("test result: ok. 1 passed")
}
fn forbidden(reply: (u8, Value)) {
    assert_eq!(reply, (10, json!({"error_code":"FORBIDDEN"})));
}

struct Fixture {
    broker: Broker,
    supervisor: Option<Supervisor>,
    tabs: Arc<Mutex<TabSet>>,
    _control: Arc<crate::control::Control>,
    cleanup: Cleanup,
    key: SigningKey,
    program: std::path::PathBuf,
    home: String,
    environment: Vec<(String, String)>,
}
impl Fixture {
    fn new(policy: Policy) -> Self {
        Self::with_fault(policy, "none")
    }
    fn with_fault(policy: Policy, fault: &str) -> Self {
        let program = super::production_e2e::current_mix();
        let broker = if fault == "quota" {
            Broker::with_grant_limit(1)
        } else {
            Broker::start()
        };
        let root = broker.endpoint.parent().unwrap();
        let home = root.join("s4-home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join(".mixrc"), "fn prompt()\nreturn \"S4> \"\nend\n").unwrap();
        let config = root.join("node.mix");
        std::fs::write(
            &config,
            format!(
                "noded: {{ unix_socket: {} }}\n",
                serde_json::to_string(&broker.endpoint).unwrap()
            ),
        )
        .unwrap();
        let home = home.display().to_string();
        let environment = vec![
            ("HOME".into(), home.clone()),
            ("COSMIX_SRC".into(), home.clone()),
            ("COSMIX_NODE_CONFIG".into(), config.display().to_string()),
            (
                "COSMIX_BROKER_ACCOUNT".into(),
                super::production_e2e::account_name(),
            ),
            ("MIX_STATS".into(), "off".into()),
            ("MIX_EDITOR".into(), "owned".into()),
        ];
        let supervisor = if fault == "read_state_only" {
            Supervisor::with_capabilities(
                broker.options(),
                broker.url.clone(),
                policy,
                vec![Capability::ReadState],
            )
            .unwrap()
        } else {
            Supervisor::with_policy(broker.options(), broker.url.clone(), policy).unwrap()
        };
        super::tests::wait_ready(&supervisor.handle, 1);
        // Retain a fixture-only key copy to exercise a legitimate resumption
        // after the production Mix has proved its original launch attachment.
        // Production still drops its only copy on prepare().
        let key = SigningKey::from_bytes(
            &supervisor
                .handle
                .1
                .lock()
                .unwrap()
                .ready
                .as_ref()
                .unwrap()
                .key
                .to_bytes(),
        );
        {
            let mut shared = supervisor.handle.1.lock().unwrap();
            match fault {
                "none" | "read_state_only" => {}
                "missing" => {
                    shared.ready = None;
                }
                "cancelled" => shared
                    .ready
                    .as_ref()
                    .unwrap()
                    .pane
                    .live
                    .store(false, Ordering::Release),
                "expired" => shared.ready.as_mut().unwrap().deadline = 0,
                "broken_fd" | "quota" => {
                    shared.ready.as_mut().unwrap().fd =
                        crate::session_fd::LaunchFd::invalid_fixture().unwrap()
                }
                "wrong_grant" => {
                    use std::os::{fd::BorrowedFd, unix::fs::FileExt};
                    let ready = shared.ready.as_mut().unwrap();
                    let fd = unsafe { BorrowedFd::borrow_raw(ready.fd.mapping().0) }
                        .try_clone_to_owned()
                        .unwrap();
                    let file = std::fs::File::from(fd);
                    let mut header = [0u8; 5];
                    file.read_exact_at(&mut header, 0).unwrap();
                    let len = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
                    assert!(len <= 16384);
                    let mut public = vec![0u8; len];
                    file.read_exact_at(&mut public, 5).unwrap();
                    let mut descriptor: GrantResult = serde_json::from_slice(&public).unwrap();
                    descriptor.record.pane_id = Some(DecimalU64(777));
                    ready.fd = crate::session_fd::LaunchFd::new(&descriptor, &ready.key).unwrap();
                }
                _ => panic!("unknown launch fault"),
            }
        }
        let settings = crate::config::Settings {
            config: crate::config::Config::default(),
            term: "xterm-256color",
        };
        let tabs = Arc::new(Mutex::new(
            TabSet::with_initial(settings, Some(supervisor.handle.clone()), || {
                crate::terminal::Terminal::start_session_e2e(
                    settings,
                    &supervisor.handle,
                    program.to_str().unwrap(),
                    home.clone(),
                    environment.clone(),
                )
            })
            .unwrap(),
        ));
        let (cleanup, _worker) = Cleanup::start().unwrap();
        let control = supervisor
            .handle
            .install_control(tabs.clone(), cleanup.clone());
        Self {
            broker,
            supervisor: Some(supervisor),
            tabs,
            _control: control,
            cleanup,
            key,
            program,
            home,
            environment,
        }
    }
    fn native(&self) -> &NativeSession {
        &self.supervisor.as_ref().unwrap().handle
    }
    async fn records(
        &self,
        client: &VerifiedConnection,
        pane: u64,
    ) -> (SessionRecord, SessionRecord) {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let records = client.session_list().await.unwrap().records;
                if let Some(child) = records.iter().find(|r| {
                    r.pane_id == Some(DecimalU64(pane)) && r.state == BindingState::Attached
                }) && self.native().pane_generation(pane).is_some()
                    && let Some(parent) = records
                        .iter()
                        .find(|r| r.instance_id == child.parent_instance.unwrap())
                {
                    return (parent.clone(), child.clone());
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("production Mix attachment and recipient lifecycle notice")
    }
    async fn bound(&self, client: &VerifiedConnection) -> (SessionRecord, Target) {
        self.rebind(client, true).await
    }
    async fn rebind(&self, client: &VerifiedConnection, initial: bool) -> (SessionRecord, Target) {
        let (parent, child) = self.records(client, 1).await;
        if initial {
            assert_eq!(
                child.binding_generation,
                DecimalU64(1),
                "production launch must enrol first"
            );
        }
        // Stop the real child so its private renewal owner cannot race this
        // deliberate key-resumption fixture. PID is never used for policy.
        let pid = self
            .tabs
            .lock()
            .unwrap()
            .pane_by_id(1)
            .unwrap()
            .lock()
            .unwrap()
            .pid;
        assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
        let challenge = client
            .session_challenge_key(HexBytes(self.key.verifying_key().to_bytes()))
            .await
            .unwrap();
        let expected = ExpectedScope {
            broker_epoch: parent.broker_epoch,
            purpose: Purpose::Resume,
            unix_uid: parent.owner_uid,
            parent_key_hash: challenge.transcript.parent_key_hash,
            pane_id: child.pane_id,
            pane_high_water: child.pane_generation,
            role: Role::PaneShell,
            public_key_hash: HexBytes(Sha256::digest(self.key.verifying_key().to_bytes()).into()),
            capabilities_hash: HexBytes(
                Sha256::digest(encode_capabilities(&child.capabilities).unwrap()).into(),
            ),
        };
        let proved = client
            .session_prove(&challenge.sign(&self.key, &expected).unwrap())
            .await
            .unwrap()
            .record;
        assert!(proved.binding_generation.0 > child.binding_generation.0);
        let (_, child) = self.records(client, 1).await;
        (parent.clone(), target(&parent, &child))
    }
    fn open_second(&self) {
        super::tests::wait_ready(self.native(), 2);
        let settings = crate::config::Settings {
            config: crate::config::Config::default(),
            term: "xterm-256color",
        };
        let native = self.native().clone();
        let program = self.program.clone();
        let home = self.home.clone();
        let environment = self.environment.clone();
        self.tabs
            .lock()
            .unwrap()
            .open_with(move || {
                crate::terminal::Terminal::start_session_scoped_e2e(
                    settings,
                    &native,
                    2,
                    program.to_str().unwrap(),
                    home,
                    environment,
                )
            })
            .unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let mut tabs = self.tabs.lock().unwrap();
        for pane in tabs.control_panes() {
            unsafe {
                libc::kill(pane.child_pid, libc::SIGCONT);
            }
        }
        self.cleanup.submit(tabs.shutdown());
        drop(tabs);
        drop(self.supervisor.take());
    }
}
fn target(parent: &SessionRecord, child: &SessionRecord) -> Target {
    Target {
        instance_id: parent.instance_id,
        incarnation: parent.incarnation,
        pane_id: child.pane_id.unwrap(),
        pane_generation: child.pane_generation.unwrap(),
    }
}

#[test]
#[ignore = "requires clean current-HEAD COSMIX_E2E_MIX_BIN; explicit S4 gate only"]
fn p0i_07_real_recipient_both_policies() {
    eprintln!("{REQUIRE_MIX}");
    for policy in [Policy::DefaultOpen, Policy::Restricted] {
        let fixture = Fixture::new(policy);
        runtime().block_on(async {
            let owner = verified(&fixture.broker).await;
            let (parent, child) = fixture.records(&owner, 1).await;
            let target = target(&parent, &child);
            for verb in ["term.session", "term.list", "term.tabs", "term.panes", "term.snapshot"] {
                let reply = call(owner.client(), &parent.name, verb, json!({"target":target})).await;
                if policy == Policy::DefaultOpen { assert_eq!(reply.0, 0, "{verb}: {reply:?}"); }
                else { forbidden(reply); }
            }
            for property in ["state", "contents"] {
                let reply = call(owner.client(), &parent.name, "term.props.get", json!({"target":target,"property":property})).await;
                if policy == Policy::DefaultOpen { assert_eq!(reply.0, 0); } else { forbidden(reply); }
            }
            let tcp = NodedClient::connect_anonymous(&fixture.broker.url).await.unwrap();
            for verb in ["term.session", "term.snapshot", "term.type", "term.pane.close", "term.props.set", "term.execute"] {
                forbidden(call(&tcp, &parent.name, verb, json!({"target":target})).await);
            }
            let bound = verified(&fixture.broker).await;
            let (parent, target) = fixture.bound(&bound).await;
            for (verb, extra) in [
                ("term.session", json!({})), ("term.snapshot", json!({"contents":true})),
                ("term.pane.select", json!({"request_id":"1"})),
                ("term.props.get", json!({"property":"contents"})),
                ("term.props.set", json!({"request_id":"2","property":"selected","value":true})),
            ] {
                let mut body = json!({"target":target});
                body.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
                assert_eq!(call(bound.client(), &parent.name, verb, body).await.0, 0, "{policy:?} {verb}");
            }
            // Stage D made `execute` real, so an authorised caller no longer
            // gets the "unimplemented" answer — it gets the schema refusal for
            // a submission that names no source and no generation. What has NOT
            // changed is that an unauthorised caller is refused above, before
            // any of this is visible. The full forwarding path is
            // p0j_d_execute_forwards_to_the_pane_shell_and_refuses_at_the_edges.
            assert_eq!(call(bound.client(), &parent.name, "term.execute", json!({"target":target})).await, (10,json!({"error_code":"INVALID_ARGUMENT"})));
            let state = call(bound.client(), &parent.name, "term.session", json!({"target":target})).await.1;
            let request = json!({"target":target,"request_id":"3","foreground_generation":state["foreground_generation"].as_u64().unwrap().to_string(),"text":"S4_INPUT"});
            let first = call(bound.client(), &parent.name, "term.type", request.clone()).await;
            assert_eq!(first.0, 0);
            assert_eq!(call(bound.client(), &parent.name, "term.type", request.clone()).await, first);
            let mut conflict = request.clone(); conflict["text"] = json!("DIFFERENT");
            assert_eq!(call(bound.client(), &parent.name, "term.type", conflict).await.1["error_code"], "CONFLICT");
            let independent = verified(&fixture.broker).await;
            let reply = call(independent.client(), &parent.name, "term.snapshot", json!({"target":target,"contents":true})).await;
            if policy == Policy::DefaultOpen { assert_eq!(reply.0, 0); } else { forbidden(reply); }
            // Real second pane, same Term; a bound child never gains sibling
            // access, whereas default-open independent connections are owners.
            fixture.open_second();
            let observer = verified(&fixture.broker).await;
            let (_, sibling) = fixture.records(&observer, 2).await;
            let sibling_target = super::enforcement_tests::target(&parent, &sibling);
            forbidden(call(bound.client(), &parent.name, "term.snapshot", json!({"target":sibling_target,"contents":true})).await);
            forbidden(call(bound.client(), &parent.name, "term.props.set", json!({"target":sibling_target,"request_id":"4","property":"selected","value":true})).await);
            forbidden(call(bound.client(), &parent.name, "term.tab.new", json!({"target":target,"affected":[sibling_target],"request_id":"5"})).await);
            // Body assertions cannot manufacture a principal or change policy.
            // Sent from the VERIFIED owner as well as the anonymous peer: an
            // anonymous caller is refused before its body is even parsed, so
            // that arm alone would prove nothing about the body. From a caller
            // who IS otherwise authorised, the request schema simply has no
            // field through which to assert either one.
            let forged = json!({"target":target,"principal":{"unix_uid":parent.owner_uid},"policy":"default-open"});
            let reply = call(owner.client(), &parent.name, "term.session", forged.clone()).await;
            assert_ne!(reply.0, 0, "forged body accepted from a verified owner: {reply:?}");
            if policy == Policy::DefaultOpen {
                assert_eq!(reply.1["error_code"], "INVALID_ARGUMENT", "{reply:?}");
            } else {
                forbidden(reply);
            }
            forbidden(call(&tcp, &parent.name, "term.session", forged).await);
        });
    }
}

#[test]
#[ignore = "requires clean current-HEAD COSMIX_E2E_MIX_BIN; explicit S4 gate only"]
fn p0i_08_queued_input_human_revoke_and_deadline() {
    let fixture = Fixture::new(Policy::DefaultOpen);
    runtime().block_on(async {
        let owner = verified(&fixture.broker).await;
        let (parent, child) = fixture.records(&owner, 1).await;
        let target = target(&parent, &child);
        let pane = fixture.tabs.lock().unwrap().pane_by_id(1).unwrap();
        let listener = pane.lock().unwrap().listener.clone();
        listener.block_control_writes(true);
        let before = pane.lock().unwrap().stats.lock().unwrap().input_written;
        let body = json!({"target":target,"request_id":"1","foreground_generation":listener.foreground_generation().to_string(),"text":"MUST_NOT_REACH_PTY"});
        assert_eq!(call(owner.client(), &parent.name, "term.type", body.clone()).await.0, 0);
        let other = verified(&fixture.broker).await;
        assert_eq!(call(other.client(), &parent.name, "term.type", body).await.1["error_code"], "BUSY");
        // Real keyboard entry point invalidates before admitting the human key.
        listener.key(crate::terminal::Key::Char(' '), Instant::now()).unwrap();
        listener.block_control_writes(false);
        listener.key(crate::terminal::Key::Interrupt, Instant::now()).unwrap();
        let event = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let event = owner.recv_shared().await.expect("owner connection remains live");
                if event.command().command == "term.input.revoked" { break event; }
            }
        }).await.expect("private revocation event reaches unnamed verified owner");
        assert_eq!(event.trusted_context().unwrap().session.as_ref().unwrap().record_id, parent.record_id);
        assert_eq!(serde_json::from_str::<Value>(&event.command().body).unwrap()["outcome"], "partial_or_unknown");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(pane.lock().unwrap().stats.lock().unwrap().input_written - before, 2);
        listener.block_control_writes(true);
        let body = json!({"target":target,"request_id":"2","foreground_generation":listener.foreground_generation().to_string(),"text":"EXPIRED_INPUT"});
        assert_eq!(call(owner.client(), &parent.name, "term.type", body).await.0, 0);
        let paused = fixture.broker.pause();
        tokio::time::sleep(Duration::from_millis(2200)).await;
        drop(paused);
        // Prove the permit aged out on its own deadline BEFORE any human key.
        // A keypress revokes it regardless, so asserting only after one would
        // pass whether or not the deadline did anything. Another actor being
        // admitted is only possible once the previous permit is invalid.
        let expiry = verified(&fixture.broker).await;
        // Term's own renew failed while the broker was paused, so it has to
        // re-earn its deadline first. Asking it anything protected before then
        // races that recovery instead of testing the permit.
        let epoch = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let session = call(expiry.client(), &parent.name, "term.session", json!({"target":target})).await;
                if session.0 == 0 {
                    return session.1["request_epoch"].clone();
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("Term re-establishes its own attachment after the broker pause");
        // Empty text on purpose. The one-writer check runs before anything is
        // queued, so this still discriminates BUSY from admitted, while adding
        // no bytes whose flush would race the revocation below and make the
        // byte-count assertion nondeterministic.
        assert_eq!(
            call(expiry.client(), &parent.name, "term.type", json!({"target":target,"request_id":"1","request_epoch":epoch,"foreground_generation":listener.foreground_generation().to_string(),"text":""})).await.0,
            0,
            "an expired permit must not still hold the one-writer lease"
        );
        listener.block_control_writes(false);
        listener.key(crate::terminal::Key::Interrupt, Instant::now()).unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(pane.lock().unwrap().stats.lock().unwrap().input_written - before, 3);
        // Removing the pane invalidates locally before asynchronous cleanup.
        fixture.native().revoke_pane(1);
        forbidden(call(owner.client(), &parent.name, "term.snapshot", json!({"target":target})).await);
        forbidden(call(owner.client(), &parent.name, "term.props.set", json!({"target":target,"request_id":"3","property":"input","value":"STALE"})).await);
    });
}

#[test]
#[ignore = "requires clean current-HEAD COSMIX_E2E_MIX_BIN; explicit S4 gate only"]
fn p0i_10_unbound_pane_has_no_control() {
    let fixture = Fixture::new(Policy::DefaultOpen);
    runtime().block_on(async {
        let owner = verified(&fixture.broker).await;
        let (parent, child) = fixture.records(&owner, 1).await;
        let mut target = target(&parent, &child);
        // A real TabSet pane with no native launch handle is graphics-only.
        let settings = crate::config::Settings {
            config: crate::config::Config::default(),
            term: "xterm-256color",
        };
        fixture
            .tabs
            .lock()
            .unwrap()
            .open_with(|| crate::terminal::Terminal::start_session(settings, None, 2))
            .unwrap();
        target.pane_id = DecimalU64(2);
        for verb in [
            "term.session",
            "term.snapshot",
            "term.type",
            "term.tab.select",
            "term.pane.close",
            "term.props.get",
            "term.props.set",
        ] {
            let mut body = json!({"target":target,"request_id":"1"});
            if verb == "term.props.get" {
                body["property"] = json!("state");
            }
            if verb == "term.props.set" {
                body["property"] = json!("input");
                body["value"] = json!("blocked");
            }
            forbidden(call(owner.client(), &parent.name, verb, body).await);
        }
    });
}

#[test]
#[ignore = "requires clean current-HEAD COSMIX_E2E_MIX_BIN; explicit S4 gate only"]
fn p0i_07_owner_mutations_and_retired_retries() {
    for verb in [
        "term.tab.new",
        "term.pane.split",
        "term.tab.select",
        "term.pane.select",
        "term.tab.close",
        "term.pane.close",
        "term.type",
        "term.props.set",
    ] {
        let fixture = Fixture::new(Policy::DefaultOpen);
        runtime().block_on(async {
            let owner = verified(&fixture.broker).await;
            let (parent, child) = fixture.records(&owner, 1).await;
            let target = target(&parent, &child);
            let mut request = json!({"target":target,"request_id":"1"});
            if verb == "term.pane.split" {
                request["dir"] = json!("h");
            }
            if verb == "term.type" {
                request["foreground_generation"] = json!(
                    fixture
                        .tabs
                        .lock()
                        .unwrap()
                        .pane_by_id(1)
                        .unwrap()
                        .lock()
                        .unwrap()
                        .listener
                        .foreground_generation()
                        .to_string()
                );
                request["text"] = json!("print(123)\n");
            }
            if verb == "term.props.set" {
                request["property"] = json!("selected");
                request["value"] = json!(true);
            }
            let first = call(owner.client(), &parent.name, verb, request.clone()).await;
            assert_eq!(first.0, 0, "{verb}: {first:?}");
            assert_eq!(
                call(owner.client(), &parent.name, verb, request.clone()).await,
                first,
                "retry duplicated {verb}"
            );
            assert_eq!(
                call(
                    owner.client(),
                    &parent.name,
                    "term.operation",
                    json!({"target":target,"operation_id":"1"})
                )
                .await
                .0,
                0
            );
            fixture._control.expire_retries();
            assert_eq!(
                call(owner.client(), &parent.name, verb, request).await.1["error_code"],
                "UNKNOWN_OUTCOME"
            );
            assert_eq!(
                call(
                    owner.client(),
                    &parent.name,
                    "term.operation",
                    json!({"target":target,"operation_id":"1"})
                )
                .await
                .1["error_code"],
                "UNKNOWN_OUTCOME"
            );
        });
    }
}

#[test]
#[ignore = "requires clean current-HEAD COSMIX_E2E_MIX_BIN; explicit S4 gate only"]
fn p0i_08_child_pane_term_and_broker_lifetime() {
    for cause in ["child_exit", "pane_close", "term_exit", "broker_bounce"] {
        let mut fixture = Fixture::new(Policy::DefaultOpen);
        runtime().block_on(async {
            let owner = verified(&fixture.broker).await;
            let (parent, child) = fixture.records(&owner, 1).await;
            let target = target(&parent, &child);
            let pane = fixture.tabs.lock().unwrap().pane_by_id(1).unwrap();
            let listener = pane.lock().unwrap().listener.clone();
            listener.block_control_writes(true);
            let before = pane.lock().unwrap().stats.lock().unwrap().input_written;
            assert_eq!(call(owner.client(), &parent.name, "term.type", json!({"target":target,"request_id":"1","foreground_generation":listener.foreground_generation().to_string(),"text":"NEVER_WRITE_AFTER_REVOKE"})).await.0, 0);
            match cause {
                "child_exit" => {
                    assert_eq!(unsafe { libc::kill(pane.lock().unwrap().pid, libc::SIGKILL) }, 0);
                    tokio::time::timeout(Duration::from_secs(5), async {
                        while !listener.quit.load(Ordering::Acquire) { tokio::time::sleep(Duration::from_millis(10)).await; }
                    }).await.unwrap();
                    forbidden(call(owner.client(), &parent.name, "term.snapshot", json!({"target":target})).await);
                }
                "pane_close" => {
                    assert_eq!(call(owner.client(), &parent.name, "term.pane.close", json!({"target":target,"request_id":"2"})).await.0, 0);
                    forbidden(call(owner.client(), &parent.name, "term.snapshot", json!({"target":target})).await);
                }
                "term_exit" => {
                    drop(fixture.supervisor.take());
                    let records = owner.session_list().await.unwrap().records;
                    assert!(records.iter().all(|r| r.state == BindingState::Revoked));
                    let response = tokio::time::timeout(Duration::from_secs(5), owner.client().call_with_headers_raw(&parent.name, "term.type", &Default::default(), &json!({"target":target,"request_id":"3","text":"NO_OWNER"}).to_string())).await.unwrap();
                    assert!(response.is_err() || response.unwrap().0 >= 10);
                }
                "broker_bounce" => {
                    fixture.broker.bounce();
                    let fresh = verified(&fixture.broker).await;
                    let new_parent = tokio::time::timeout(Duration::from_secs(20), async {
                        loop {
                            if let Some(p) = fresh.session_list().await.unwrap().records.into_iter().find(|r| r.role == Role::Term && r.state == BindingState::Attached) { break p; }
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                    }).await.unwrap();
                    assert_ne!(parent.broker_epoch, new_parent.broker_epoch);
                    assert_ne!(parent.incarnation, new_parent.incarnation);
                    assert_eq!(call(fresh.client(), &new_parent.name, "term.type", json!({"target":target,"request_id":"1","text":"STALE_EPOCH"})).await.1["error_code"], "UNKNOWN_OUTCOME");
                }
                _ => unreachable!(),
            }
            listener.block_control_writes(false);
            if !listener.quit.load(Ordering::Acquire) {
                listener.key(crate::terminal::Key::Interrupt, Instant::now()).unwrap();
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert!(pane.lock().unwrap().stats.lock().unwrap().input_written - before <= 1, "{cause}: uncommitted agent bytes escaped");
        });
    }
}

#[test]
#[ignore = "requires clean current-HEAD COSMIX_E2E_MIX_BIN; explicit S4 gate only"]
fn p0i_09_real_payload_tap_observe_and_logs() {
    if std::env::var_os("COSMIX_S4_OBSERVE_WORKER").is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "native_session::enforcement_tests::p0i_09_real_payload_tap_observe_and_logs",
                "--ignored",
                "--nocapture",
            ])
            .env("COSMIX_S4_OBSERVE_WORKER", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "observation subprocess failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            ran_one_test(&output.stdout),
            "observation worker never ran its assertions: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        // Scanning an empty stream for the sentinel would pass for free. The
        // worker enables broker/client tracing precisely so there is real
        // diagnostic output here to scan.
        assert!(!output.stderr.is_empty(), "no captured diagnostic output to scan");
        for stream in [&output.stdout, &output.stderr] {
            assert!(
                !String::from_utf8_lossy(stream).contains("PRIVATE_S4_SENTINEL"),
                "protected payload in general logs"
            );
        }
        return;
    }
    term_native_test_broker::trace_to_stderr();
    let fixture = Fixture::new(Policy::DefaultOpen);
    runtime().block_on(async {
        let audit = NodedClient::connect("term-policy-audit", &fixture.broker.url).await.unwrap();
        let mut observed = audit.incoming_async().await.unwrap();
        audit.call("noded", "noded.observe.start", json!({"filter":{"verbs":["term.*"]},"body":"redacted"})).await.unwrap();
        let tap = NodedClient::connect_anonymous(&fixture.broker.url).await.unwrap();
        let mut tapped = tap.incoming_async().await.unwrap();
        tap.call("noded", "noded.tap", json!({})).await.unwrap();
        let owner = verified(&fixture.broker).await;
        let (parent, child) = fixture.records(&owner, 1).await;
        let target = target(&parent, &child);
        let listener = fixture.tabs.lock().unwrap().pane_by_id(1).unwrap().lock().unwrap().listener.clone();
        let input = json!({"target":target,"request_id":"1","foreground_generation":listener.foreground_generation().to_string(),"text":"print(\"PRIVATE_S4_SENTINEL\")\n"});
        assert_eq!(call(owner.client(), &parent.name, "term.type", input).await.0, 0);
        // A revocation while both subscriptions are live. term.input.revoked is
        // itself a protected private event carrying the target and a delivered
        // byte count, so it has to be excluded from tap and observe exactly as
        // the request that created it was. Nothing else in the suite watches
        // that event with real subscribers attached.
        listener.block_control_writes(true);
        let revoked_input = json!({"target":target,"request_id":"2","foreground_generation":listener.foreground_generation().to_string(),"text":"PRIVATE_S4_SENTINEL_REVOKED"});
        assert_eq!(call(owner.client(), &parent.name, "term.type", revoked_input).await.0, 0);
        listener.block_control_writes(false);
        listener.key(crate::terminal::Key::Interrupt, Instant::now()).unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let reply = call(owner.client(), &parent.name, "term.props.get", json!({"target":target,"property":"contents"})).await;
                if reply.1["text"].as_str().is_some_and(|text| text.contains("PRIVATE_S4_SENTINEL")) { break; }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }).await.expect("real PTY contents via protected property handler");
        let mut count = 0;
        while let Ok(Some(event)) = tokio::time::timeout(Duration::from_millis(200), observed.recv()).await {
            assert!(!event.body.contains("PRIVATE_S4_SENTINEL"));
            assert!(!event.body.contains("broker_principal"));
            let body: Value = serde_json::from_str(&event.body).unwrap();
            assert_eq!(body["payload_omitted"], "native_session_protected");
            assert!(body["payload"].is_null());
            count += 1;
        }
        assert!(count >= 4, "must observe actual protected request/response metadata");
        let marker = NodedClient::connect("s4-public-marker", &fixture.broker.url).await.unwrap();
        let sender = NodedClient::connect_anonymous(&fixture.broker.url).await.unwrap();
        sender.send("s4-public-marker", "probe.public", json!({"marker":"public"})).await.unwrap();
        let first = tokio::time::timeout(Duration::from_secs(2), tapped.recv()).await.unwrap().unwrap();
        assert_eq!(first.command, "probe.public", "tap received protected envelope before marker");
        marker.close().await;
    });
}

#[test]
#[ignore = "SKIPPED privileged S4 multi-UID fixture: requires root, COSMIX_SESSION_TEST_UID and current-HEAD COSMIX_E2E_MIX_BIN; run explicitly"]
fn p0i_07_other_uid_both_policies() {
    if let Ok(configuration) = std::env::var("COSMIX_S4_OTHER_UID_WORKER") {
        let configuration: Value = serde_json::from_str(&configuration).unwrap();
        runtime().block_on(async {
            let mut options = UnixConnectOptions::new(BrokerAccount {
                uid:configuration["broker_uid"].as_u64().unwrap() as u32,
                gid:configuration["broker_gid"].as_u64().unwrap() as u32,
            });
            options.endpoint = Some(configuration["endpoint"].as_str().unwrap().into());
            options.require_native_session = true;
            let UnixConnectOutcome::VerifiedUnix(client) = NodedClient::connect_unix("", configuration["url"].as_str().unwrap(), &options, None).await.unwrap()
            else { panic!("real other-UID kernel verified connection required"); };
            assert_ne!(unsafe { libc::geteuid() }, options.broker_account.uid);
            for verb in ["term.session", "term.list", "term.tabs", "term.panes", "term.snapshot", "term.type", "term.execute", "term.tab.new", "term.tab.select", "term.tab.close", "term.pane.split", "term.pane.select", "term.pane.close", "term.props.get", "term.props.set", "term.props.watch"] {
                for target in [configuration["target"].clone(), json!({"nonexistent":true})] {
                    forbidden(call(client.client(), configuration["name"].as_str().unwrap(), verb, json!({"target":target,"request_id":"1","property":"input","value":"BLOCKED"})).await);
                }
            }
        });
        return;
    }
    use std::os::unix::process::CommandExt;
    let uid: u32 = std::env::var("COSMIX_SESSION_TEST_UID")
        .expect("SKIPPED: COSMIX_SESSION_TEST_UID is required; no silent pass")
        .parse()
        .unwrap();
    assert_eq!(unsafe { libc::geteuid() }, 0, "SKIPPED: requires root");
    assert_ne!(uid, 0, "fixture UID must differ from owner");
    for policy in [Policy::DefaultOpen, Policy::Restricted] {
        let fixture = Fixture::new(policy);
        runtime().block_on(async {
            let owner = verified(&fixture.broker).await;
            let (parent, child) = fixture.records(&owner, 1).await;
            let configuration = json!({"broker_uid":unsafe { libc::geteuid() },"broker_gid":unsafe { libc::getegid() },"endpoint":fixture.broker.endpoint,"url":fixture.broker.url,"name":parent.name,"target":target(&parent,&child)});
            let mut process = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "native_session::enforcement_tests::p0i_07_other_uid_both_policies", "--ignored", "--nocapture"])
                .uid(uid).gid(uid).env("COSMIX_S4_OTHER_UID_WORKER", configuration.to_string())
                .stdout(std::process::Stdio::piped()).spawn().unwrap();
            let status = tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    if let Some(status) = process.try_wait().unwrap() { return status; }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }).await.expect("other-UID real recipient fixture deadline");
            assert!(status.success());
            let mut stdout = Vec::new();
            std::io::Read::read_to_end(&mut process.stdout.take().unwrap(), &mut stdout).unwrap();
            assert!(
                ran_one_test(&stdout),
                "other-UID worker never ran its denial assertions: {}",
                String::from_utf8_lossy(&stdout)
            );
        });
    }
}

#[test]
#[ignore = "requires clean current-HEAD COSMIX_E2E_MIX_BIN; explicit S4 gate only"]
fn p0i_10_real_tcp_fallback_is_control_free() {
    let fixture = Fixture::new(Policy::DefaultOpen);
    let (notify, receiver) = tokio::sync::mpsc::unbounded_channel();
    drop(notify);
    let service = crate::bus::start_at(fixture.tabs.clone(), receiver, fixture.broker.url.clone());
    runtime().block_on(async {
        let client = NodedClient::connect_anonymous(&fixture.broker.url)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if client.call("term", "HELP", json!({})).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("real legacy diagnostic registration");
        for verb in [
            "term.session",
            "term.tabs",
            "term.panes",
            "term.snapshot",
            "term.type",
            "term.tab.new",
            "term.tab.select",
            "term.tab.close",
            "term.pane.split",
            "term.pane.select",
            "term.pane.close",
            "term.execute",
            "term.props.get",
            "term.props.set",
            "term.props.watch",
        ] {
            forbidden(call(&client, "term", verb, json!({"text":"NO_LEGACY_CONTROL"})).await);
        }
    });
    fixture
        .cleanup
        .submit(fixture.tabs.lock().unwrap().shutdown());
    service.join().unwrap();
}

#[test]
#[ignore = "requires clean current-HEAD COSMIX_E2E_MIX_BIN; explicit S4 gate only"]
fn p0i_10_launch_failures_reach_real_recipient_denial() {
    for fault in [
        "missing",
        "wrong_grant",
        "expired",
        "broken_fd",
        "cancelled",
        "quota",
    ] {
        let fixture = Fixture::with_fault(Policy::DefaultOpen, fault);
        runtime().block_on(async {
            let owner = verified(&fixture.broker).await;
            let records = owner.session_list().await.unwrap().records;
            let parent = records.iter().find(|r| r.role == Role::Term).unwrap();
            let child = records.iter().find(|r| r.pane_id == Some(DecimalU64(1))).unwrap();
            let target = target(parent, child);
            // The actual Mix must get past startup, with a usable ordinary
            // prompt; a dead/missing shell is not no-control acceptance.
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if fixture.tabs.lock().unwrap().pane_by_id(1).unwrap().lock().unwrap().snapshot().contains("S4>") { break; }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }).await.unwrap_or_else(|_| panic!("{fault}: missing graphics-only Mix prompt"));
            assert!(fixture.native().pane_generation(1).is_none());
            for (verb, extra) in [
                ("term.session", json!({})), ("term.snapshot", json!({"contents":true})),
                ("term.type", json!({"text":"MUST_NOT_EXECUTE","foreground_generation":"1"})),
                ("term.tab.new", json!({})), ("term.pane.split", json!({"dir":"h"})),
                ("term.tab.select", json!({})), ("term.pane.select", json!({})),
                ("term.tab.close", json!({})), ("term.pane.close", json!({})),
                ("term.props.get", json!({"property":"contents"})),
                ("term.props.set", json!({"property":"input","value":"MUST_NOT_EXECUTE","foreground_generation":"1"})),
            ] {
                let mut body = json!({"target":target,"request_id":"1"});
                body.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
                forbidden(call(owner.client(), &parent.name, verb, body).await);
            }
            // A names-only collision cannot install an alternative native
            // endpoint, even while the real child is graphics-only.
            assert!(NodedClient::connect(&parent.name, &fixture.broker.url).await.is_err());
            if fault == "quota" {
                tokio::time::timeout(Duration::from_secs(5), async {
                    while !fixture.native().status().to_string().contains("quota") { tokio::time::sleep(Duration::from_millis(25)).await; }
                }).await.expect("real broker pending-grant quota refusal");
            }
        });
    }
}

#[test]
#[ignore = "requires clean current-HEAD COSMIX_E2E_MIX_BIN; explicit S4 gate only"]
fn p0i_10_parent_bootstrap_outage_has_only_diagnostic_lane() {
    let program = super::production_e2e::current_mix();
    let broker = Broker::start();
    let mut options = broker.options();
    options.endpoint = Some(broker.endpoint.with_file_name("absent.sock"));
    let mut supervisor = Supervisor::with_options(options, broker.url.clone()).unwrap();
    supervisor.wait_startup();
    let settings = crate::config::Settings {
        config: crate::config::Config::default(),
        term: "xterm-256color",
    };
    let home = broker.endpoint.parent().unwrap().display().to_string();
    let tabs = Arc::new(Mutex::new(
        TabSet::with_initial(settings, Some(supervisor.handle.clone()), || {
            crate::terminal::Terminal::start_session_e2e(
                settings,
                &supervisor.handle,
                program.to_str().unwrap(),
                home.clone(),
                vec![("HOME".into(), home.clone())],
            )
        })
        .unwrap(),
    ));
    let (cleanup, _worker) = Cleanup::start().unwrap();
    let _control = supervisor
        .handle
        .install_control(tabs.clone(), cleanup.clone());
    let (notify, receiver) = tokio::sync::mpsc::unbounded_channel();
    drop(notify);
    let diagnostic = crate::bus::start_at(tabs.clone(), receiver, broker.url.clone());
    runtime().block_on(async {
        let owner = verified(&broker).await;
        assert!(
            owner.session_list().await.unwrap().records.is_empty(),
            "failed bootstrap allocated a control route"
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if owner.client().call("term", "HELP", json!({})).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();
        for verb in [
            "term.session",
            "term.snapshot",
            "term.type",
            "term.tab.new",
            "term.pane.split",
            "term.pane.close",
            "term.props.get",
            "term.props.set",
        ] {
            forbidden(call(owner.client(), "term", verb, json!({})).await);
        }
    });
    cleanup.submit(tabs.lock().unwrap().shutdown());
    diagnostic.join().unwrap();
}

#[test]
#[ignore = "requires clean current-HEAD COSMIX_E2E_MIX_BIN; explicit S4 gate only"]
fn p0i_08_stale_cleanup_and_private_event_connection_guard() {
    let fixture = Fixture::new(Policy::Restricted);
    runtime().block_on(async {
        let first = verified(&fixture.broker).await;
        let (parent, target) = fixture.bound(&first).await;
        let listener = fixture.tabs.lock().unwrap().pane_by_id(1).unwrap().lock().unwrap().listener.clone();
        listener.block_control_writes(true);
        assert_eq!(call(first.client(), &parent.name, "term.type", json!({"target":target,"request_id":"1","foreground_generation":listener.foreground_generation().to_string(),"text":"OLD_ATTACHMENT"})).await.0, 0);
        let successor = verified(&fixture.broker).await;
        fixture.rebind(&successor, false).await;
        first.client().close().await;
        assert_eq!(call(successor.client(), &parent.name, "term.props.get", json!({"target":target,"property":"state"})).await.0, 0);
        // Fire the revocation the CLOSED connection was owed. Previously the
        // negative below ran before any revocation existed, so it held against
        // a completely dead notice pipeline.
        listener.block_control_writes(false);
        listener.key(crate::terminal::Key::Interrupt, Instant::now()).unwrap();
        let crossed = tokio::time::timeout(Duration::from_millis(350), async {
            loop {
                let event = successor.recv_shared().await.unwrap();
                if event.command().command == "term.input.revoked" { break event; }
            }
        }).await;
        assert!(crossed.is_err(), "private event crossed from old connection to successor");
        // Positive control, same fixture: the pipeline that just stayed silent
        // has to be able to deliver at all, or the assertion above proves
        // nothing. The successor claims input itself and must receive its own
        // revocation, addressed to its own connection.
        listener.block_control_writes(true);
        assert_eq!(call(successor.client(), &parent.name, "term.type", json!({"target":target,"request_id":"2","foreground_generation":listener.foreground_generation().to_string(),"text":"NEW_ATTACHMENT"})).await.0, 0);
        listener.block_control_writes(false);
        listener.key(crate::terminal::Key::Interrupt, Instant::now()).unwrap();
        let delivered = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let event = successor.recv_shared().await.unwrap();
                if event.command().command == "term.input.revoked" { break event; }
            }
        }).await.expect("the successor must receive its OWN revocation");
        assert_eq!(delivered.trusted_context().unwrap().session.as_ref().unwrap().record_id, parent.record_id);
        assert_eq!(call(successor.client(), &parent.name, "term.snapshot", json!({"target":target})).await.0, 0, "stale close revoked successor");
    });
}

#[test]
#[ignore = "requires clean current-HEAD COSMIX_E2E_MIX_BIN; explicit S4 gate only"]
fn p0i_07_capability_separation_and_bound_termination() {
    for policy in [Policy::DefaultOpen, Policy::Restricted] {
        let fixture = Fixture::with_fault(policy, "read_state_only");
        runtime().block_on(async {
            let bound = verified(&fixture.broker).await;
            let (parent, target) = fixture.bound(&bound).await;
            assert_eq!(
                call(
                    bound.client(),
                    &parent.name,
                    "term.session",
                    json!({"target":target})
                )
                .await
                .0,
                0
            );
            assert_eq!(
                call(
                    bound.client(),
                    &parent.name,
                    "term.props.get",
                    json!({"target":target,"property":"state"})
                )
                .await
                .0,
                0
            );
            for (verb, extra) in [
                ("term.snapshot", json!({"contents":true})),
                (
                    "term.type",
                    json!({"text":"DENIED","foreground_generation":"1"}),
                ),
                ("term.pane.select", json!({})),
                ("term.pane.close", json!({})),
                ("term.execute", json!({})),
                ("term.props.get", json!({"property":"contents"})),
                (
                    "term.props.set",
                    json!({"property":"selected","value":true}),
                ),
            ] {
                let mut body = json!({"target":target,"request_id":"1"});
                body.as_object_mut()
                    .unwrap()
                    .extend(extra.as_object().unwrap().clone());
                forbidden(call(bound.client(), &parent.name, verb, body).await);
            }
        });
        drop(fixture);
        let fixture = Fixture::new(policy);
        runtime().block_on(async {
            let bound = verified(&fixture.broker).await;
            let (parent, target) = fixture.bound(&bound).await;
            assert_eq!(
                call(
                    bound.client(),
                    &parent.name,
                    "term.pane.close",
                    json!({"target":target,"request_id":"1"})
                )
                .await
                .0,
                0
            );
            assert!(fixture.tabs.lock().unwrap().is_empty());
        });
    }
}

#[test]
#[ignore = "requires clean current-HEAD COSMIX_E2E_MIX_BIN; explicit S4 gate only"]
fn p0i_07_affected_set_and_live_generation_authority() {
    eprintln!("{REQUIRE_MIX}");
    let fixture = Fixture::new(Policy::DefaultOpen);
    runtime().block_on(async {
        let owner = verified(&fixture.broker).await;
        let (parent, first) = fixture.records(&owner, 1).await;
        let first = target(&parent, &first);
        fixture.open_second();
        let (_, second) = fixture.records(&owner, 2).await;
        let second = target(&parent, &second);
        assert_ne!(first.pane_id, second.pane_id);

        // A LIVE pane is still not addressable by a generation it is not on.
        // This is the permit's pane guard rather than the pure policy decision,
        // so it only means anything against a pane that really is alive: the
        // same request with the current generation succeeds immediately after.
        let mut ahead = first.clone();
        ahead.pane_generation.0 += 1;
        forbidden(
            call(
                owner.client(),
                &parent.name,
                "term.snapshot",
                json!({"target":ahead,"contents":true}),
            )
            .await,
        );
        let mut zero = first.clone();
        zero.pane_generation.0 = 0;
        forbidden(
            call(
                owner.client(),
                &parent.name,
                "term.snapshot",
                json!({"target":zero,"contents":true}),
            )
            .await,
        );
        assert_eq!(
            call(
                owner.client(),
                &parent.name,
                "term.snapshot",
                json!({"target":first,"contents":true})
            )
            .await
            .0,
            0,
            "the generation gate must be the only thing refusing those"
        );
        // Selecting the first pane while the second tab is active moves focus
        // in both tabs, so the request must carry authority over the whole
        // affected set. This ambient owner WOULD be allowed that sibling on its
        // own, which is what makes this the affected-set gate rather than a
        // policy one: omitting the sibling is refused anyway.
        forbidden(
            call(
                owner.client(),
                &parent.name,
                "term.pane.select",
                json!({"target":first,"request_id":"2"}),
            )
            .await,
        );
        let mut stale = second.clone();
        stale.pane_generation.0 += 1;
        forbidden(
            call(
                owner.client(),
                &parent.name,
                "term.pane.select",
                json!({"target":first,"affected":[stale],"request_id":"3"}),
            )
            .await,
        );
        assert_eq!(
            call(
                owner.client(),
                &parent.name,
                "term.pane.select",
                json!({"target":first,"affected":[second],"request_id":"4"})
            )
            .await
            .0,
            0
        );

        // Input is refused on the same stale generation, against the same live
        // pane. This comes after a valid mutation deliberately: the harness
        // caches one request epoch per (connection, service) from a term.session
        // probe using whatever target the first request-ID call carries, so
        // leading with a bad target would poison every later mutation with a
        // zero epoch and mask what is actually being tested.
        forbidden(
            call(
                owner.client(),
                &parent.name,
                "term.type",
                json!({"target":ahead,"request_id":"5","foreground_generation":"1","text":"NO"}),
            )
            .await,
        );

        // Closing the now-active first tab promotes the second one, so the
        // replacement's panes join the affected set even though the request
        // never names that tab.
        forbidden(
            call(
                owner.client(),
                &parent.name,
                "term.tab.close",
                json!({"target":first,"request_id":"6"}),
            )
            .await,
        );
        assert_eq!(
            call(
                owner.client(),
                &parent.name,
                "term.tab.close",
                json!({"target":first,"affected":[second],"request_id":"7"})
            )
            .await
            .0,
            0
        );
    });
}

#[test]
#[ignore = "requires clean current-HEAD COSMIX_E2E_MIX_BIN; explicit S4 gate only"]
fn p0i_08_lease_window_refresh_and_successor_binding_invalidation() {
    eprintln!("{REQUIRE_MIX}");
    let fixture = Fixture::new(Policy::DefaultOpen);
    runtime().block_on(async {
        let bound = verified(&fixture.broker).await;
        let (parent, target) = fixture.bound(&bound).await;
        assert_eq!(
            call(
                bound.client(),
                &parent.name,
                "term.session",
                json!({"target":target})
            )
            .await
            .0,
            0
        );

        // A bound caller's lease check is reused for one five-second window.
        // Every other fixture finishes well inside it, so the branch that lets
        // the cache expire and takes a fresh check has never run.
        tokio::time::sleep(Duration::from_millis(5500)).await;
        assert_eq!(
            call(
                bound.client(),
                &parent.name,
                "term.session",
                json!({"target":target})
            )
            .await
            .0,
            0,
            "a bound caller must survive its cached lease check expiring"
        );

        // A successor binding retires what the previous one authorised. Proved
        // without a keypress, which would revoke the permit regardless: another
        // actor is admitted to the one-writer lease only once the queued permit
        // has actually been invalidated.
        let pane = fixture.tabs.lock().unwrap().pane_by_id(1).unwrap();
        let listener = pane.lock().unwrap().listener.clone();
        listener.block_control_writes(true);
        let generation = listener.foreground_generation().to_string();
        assert_eq!(
            call(
                bound.client(),
                &parent.name,
                "term.type",
                json!({"target":target,"request_id":"1","foreground_generation":generation,"text":"NEVER_AFTER_REBIND"})
            )
            .await
            .0,
            0
        );
        let claimed = Instant::now();
        let owner = verified(&fixture.broker).await;
        assert_eq!(
            call(owner.client(), &parent.name, "term.type", json!({"target":target,"request_id":"1","foreground_generation":listener.foreground_generation().to_string(),"text":"BLOCKED"})).await.1["error_code"],
            "BUSY",
            "the queued permit must hold the lease before the successor lands"
        );
        let (parent, target) = fixture.rebind(&bound, false).await;
        assert_eq!(
            call(owner.client(), &parent.name, "term.type", json!({"target":target,"request_id":"2","foreground_generation":listener.foreground_generation().to_string(),"text":"ADMITTED"})).await.0,
            0,
            "a successor binding must invalidate the previous permit"
        );
        // Without this the permit's own two-second deadline would explain the
        // admission just as well, and the assertion above would prove nothing.
        assert!(
            claimed.elapsed() < Duration::from_secs(2),
            "permit aged out on its own deadline; this says nothing about the successor binding"
        );
        listener.block_control_writes(false);
    });
}

/// The plan requires a revoked paste to finish any bracketed-paste terminator
/// before discarding the remainder. This encoder never opens one — it emits
/// per-key sequences only — so that obligation is inapplicable here rather than
/// quietly skipped. If bracketed paste is ever added to the control input path
/// this fails, and the terminator handling has to be written alongside it.
#[test]
fn control_input_never_opens_a_bracketed_paste() {
    const OPEN: &str = "\u{1b}[200~";
    const CLOSE: &str = "\u{1b}[201~";
    let encoded = crate::terminal::encode_text("plain text\nwith\ttabs\u{3}").unwrap();
    let encoded = String::from_utf8_lossy(&encoded).into_owned();
    assert!(!encoded.contains(OPEN), "bracketed paste opened: {encoded:?}");
    assert!(!encoded.contains(CLOSE), "bracketed paste closed: {encoded:?}");
    // The needles have to be able to match something, or the two assertions
    // above would hold for any encoder at all.
    assert!(format!("x{OPEN}y{CLOSE}z").contains(OPEN));
    assert!(format!("x{OPEN}y{CLOSE}z").contains(CLOSE));
}

#[test]
#[ignore = "requires clean current-HEAD COSMIX_E2E_MIX_BIN; explicit S4 gate only"]
fn p0i_07_actor_table_survives_reconnect_churn() {
    eprintln!("{REQUIRE_MIX}");
    let fixture = Fixture::new(Policy::DefaultOpen);
    runtime().block_on(async {
        let owner = verified(&fixture.broker).await;
        let (parent, child) = fixture.records(&owner, 1).await;
        let target = target(&parent, &child);
        // One ambient actor key is minted per connection, so a client that
        // reconnects mints them without end. Well past the 256-key cap the
        // instance must still accept mutations: refusing at the cap would have
        // bricked every mutation on this Term for the rest of its life, and no
        // amount of waiting would have cleared it. The connections are kept
        // alive so the table really is full of live keys, which is the case
        // that has no expired entry to reclaim.
        let mut actors = Vec::new();
        for cycle in 0..320u32 {
            let actor = verified(&fixture.broker).await;
            // Read this connection's epoch and pass it explicitly. The helper
            // would otherwise cache one per client ADDRESS, and every iteration
            // builds its connection in the same stack slot, so cycle 1 would
            // reuse cycle 0's epoch and every mutation would come back
            // UNKNOWN_OUTCOME for a reason that has nothing to do with the cap.
            let session = call(
                actor.client(),
                &parent.name,
                "term.session",
                json!({"target":target}),
            )
            .await;
            assert_eq!(session.0, 0, "cycle {cycle} session refused: {session:?}");
            let reply = call(
                actor.client(),
                &parent.name,
                "term.pane.select",
                json!({"target":target,"request_id":"1","request_epoch":session.1["request_epoch"]}),
            )
            .await;
            assert_eq!(reply.0, 0, "cycle {cycle} refused: {reply:?}");
            actors.push(actor);
        }
        // The original owner is unaffected by the churn.
        assert_eq!(
            call(
                owner.client(),
                &parent.name,
                "term.session",
                json!({"target":target})
            )
            .await
            .0,
            0
        );
    });
}

/// A grant whose capability set misses a verb the dispatch table can route
/// leaves a bound child permanently unable to use it, and nothing else would
/// say so: the child would just see FORBIDDEN and have no way to tell a policy
/// decision from a provisioning mistake. Pin the two tables against each other.
#[test]
fn default_child_capabilities_cover_every_dispatchable_verb() {
    let granted = super::capabilities();
    // Mirrors dispatch's verb-to-capability table. Execute is listed because
    // the grant must still carry it even while the verb answers UNSUPPORTED;
    // stage D turns the verb on without reissuing anyone's grant.
    for (verb, capability) in [
        ("term.session", Capability::ReadState),
        ("term.list", Capability::ReadState),
        ("term.tabs", Capability::ReadState),
        ("term.panes", Capability::ReadState),
        ("term.operation", Capability::ReadState),
        ("term.snapshot", Capability::ReadContents),
        ("term.type", Capability::Input),
        ("term.tab.new", Capability::ManageLayout),
        ("term.tab.select", Capability::ManageLayout),
        ("term.pane.split", Capability::ManageLayout),
        ("term.pane.select", Capability::ManageLayout),
        ("term.tab.close", Capability::Terminate),
        ("term.pane.close", Capability::Terminate),
        ("term.execute", Capability::Execute),
    ] {
        assert!(
            granted.contains(&capability),
            "{verb} routes to {capability:?}, which the default grant omits"
        );
    }
    // And the reverse: a capability nobody routes to is dead weight in every
    // grant, so the two tables have to stay the same size.
    assert_eq!(granted.len(), 6, "granted capabilities: {granted:?}");
}

// ---------------------------------------------------------------------------
// P0-J stage D: BROKER-023 `execute`, end to end through the real Term.

/// Ask the CHILD directly. Term forwards executions; it does not observe the
/// child's prompt, and a test that read the generation from Term would be
/// asserting against a number nobody publishes.
async fn shell_status(client: &NodedClient, child: &SessionRecord) -> Value {
    let body = json!({"version":1,"target":{
        "broker_epoch":child.broker_epoch,
        "record":child.reference(),
        "instance_id":child.instance_id,
        "pane_id":child.pane_id,
        "pane_generation":child.pane_generation,
    }});
    raw(client, &child.name, "shell.status", &body).await.1
}

async fn idle_generation(client: &NodedClient, child: &SessionRecord) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let status = shell_status(client, child).await;
        if status["status"]["snapshot"]["phase"] == "prompt-ready"
            && status["status"]["snapshot"]["continuation"] == false
        {
            return status["status"]["snapshot"]["prompt_generation"]
                .as_str()
                .expect("decimal-string generation")
                .to_owned();
        }
        assert!(Instant::now() < deadline, "child never idled: {status}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[test]
#[ignore = "requires clean current-HEAD COSMIX_E2E_MIX_BIN; explicit stage-D gate only"]
fn p0j_d_execute_forwards_to_the_pane_shell_and_refuses_at_the_edges() {
    eprintln!("{REQUIRE_MIX}");
    let fixture = Fixture::new(Policy::DefaultOpen);
    runtime().block_on(async {
        let owner = verified(&fixture.broker).await;
        let (parent, child) = fixture.records(&owner, 1).await;
        let target = target(&parent, &child);
        let generation = idle_generation(owner.client(), &child).await;

        // A submission that names no source cannot be a submission. This is the
        // schema refusal, and it happens before anything is forwarded.
        assert_eq!(
            call(
                owner.client(),
                &parent.name,
                "term.execute",
                json!({"target":target,"request_id":"1","prompt_generation":generation})
            )
            .await,
            (10, json!({"error_code":"INVALID_ARGUMENT"}))
        );

        // The real thing: Term forwards, the child admits, and the answer that
        // comes back is the CHILD's, carrying the operation id the child minted.
        let submission = json!({
            "target": target, "request_id": "2",
            "prompt_generation": generation,
            "source": "print(\"TERM_EXEC_OK\")",
        });
        let accepted = call(owner.client(), &parent.name, "term.execute", submission.clone()).await;
        assert_eq!(accepted.0, 0, "{accepted:?}");
        assert_eq!(accepted.1["status"], "accepted");
        let operation = accepted.1["operation_id"].clone();
        assert!(operation.is_string(), "{accepted:?}");
        // Term stamps the caller's own target back on, so a reply is
        // recognisable as belonging to the request that was sent to Term.
        assert_eq!(accepted.1["target"], json!(target));

        // The result family resolves the same operation through Term.
        let deadline = Instant::now() + Duration::from_secs(20);
        let result = loop {
            let reply = call(
                owner.client(),
                &parent.name,
                "term.exec.result",
                json!({"target":target,"operation_id":operation}),
            )
            .await;
            assert_eq!(reply.0, 0, "{reply:?}");
            if reply.1["state"] == "finished" {
                break reply.1;
            }
            assert!(Instant::now() < deadline, "never finished: {reply:?}");
            tokio::time::sleep(Duration::from_millis(25)).await;
        };
        assert_eq!(result["result"]["outcome"], "completed", "{result}");

        // BROKER-018 across BOTH hops: the identical retry answers from Term's
        // own record and never reaches the child a second time.
        let replay = call(owner.client(), &parent.name, "term.execute", submission).await;
        assert_eq!(replay, accepted, "a retry must replay, not re-execute");
        let snapshot = call(
            owner.client(),
            &parent.name,
            "term.snapshot",
            json!({"target":target,"contents":true}),
        )
        .await;
        assert_eq!(
            snapshot.1["text"].as_str().unwrap().matches("TERM_EXEC_OK").count(),
            // Once as the admission echo, once as the output. A third would be
            // a second execution.
            2,
            "exactly one execution reached the pane: {}",
            snapshot.1["text"]
        );

        // The child's refusals are RELAYED, not replaced: BUSY and
        // STALE_GENERATION tell a caller two different things to do next.
        let stale = call(
            owner.client(),
            &parent.name,
            "term.execute",
            json!({"target":target,"request_id":"3","prompt_generation":generation,
                   "source":"print(\"NEVER\")"}),
        )
        .await;
        assert_eq!(stale.1["error_code"], "STALE_GENERATION", "{stale:?}");

        // Cancellation resolves through the same family, and an id this shell
        // never admitted addresses nothing.
        let unknown = call(
            owner.client(),
            &parent.name,
            "term.exec.cancel",
            json!({"target":target,"operation_id":"9999"}),
        )
        .await;
        assert_eq!(unknown.1["error_code"], "UNKNOWN_OUTCOME", "{unknown:?}");
        let finished = call(
            owner.client(),
            &parent.name,
            "term.exec.cancel",
            json!({"target":target,"operation_id":operation}),
        )
        .await;
        assert_eq!(finished.0, 0, "{finished:?}");
        assert_eq!(finished.1["outcome"], "already_finished");

        // An unverified peer never reaches any of it.
        let tcp = NodedClient::connect_anonymous(&fixture.broker.url).await.unwrap();
        for verb in ["term.execute", "term.exec.result", "term.exec.cancel"] {
            forbidden(call(&tcp, &parent.name, verb, json!({"target":target})).await);
        }

        // A pane generation that is not this child's is not this child. Term
        // must not forward to whatever happens to be bound now.
        let mut wrong = target;
        wrong.pane_generation = DecimalU64(wrong.pane_generation.0 + 1);
        let reply = call(
            owner.client(),
            &parent.name,
            "term.execute",
            json!({"target":wrong,"request_id":"4","prompt_generation":generation,
                   "source":"print(\"NEVER\")"}),
        )
        .await;
        assert_eq!(reply, (10, json!({"error_code":"FORBIDDEN"})), "{reply:?}");
    });
}
