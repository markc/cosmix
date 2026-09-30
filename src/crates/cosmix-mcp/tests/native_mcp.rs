//! Real stdio MCP -> native ABP -> isolated noded acceptance.
//! The named services below are protocol fixtures, not GUI coverage.
//! See docs/cos/cosmix-mcp/native-acceptance.md for the acceptance command.

use cosmix_client::NodedClient;
use rmcp::{
    ClientHandler, RoleClient, ServiceExt,
    model::{
        CallToolRequest, CallToolRequestParams, CallToolResult, ClientRequest,
        ProgressNotificationParam, Tool,
    },
    service::{NotificationContext, PeerRequestOptions, RunningService},
};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
    task::JoinHandle,
};

#[derive(Clone, Default)]
struct Observer(Arc<Mutex<Vec<ProgressNotificationParam>>>);

impl ClientHandler for Observer {
    async fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _: NotificationContext<RoleClient>,
    ) {
        self.0.lock().unwrap().push(params);
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    config: PathBuf,
    address: String,
    broker: Child,
    mcp: Child,
    client: RunningService<RoleClient, Observer>,
    observer: Observer,
    tasks: Vec<JoinHandle<()>>,
    owned: Vec<Child>,
    stderr: Arc<Mutex<Vec<u8>>>,
}

fn broker_command(config: &PathBuf, dir: &std::path::Path, address: &str) -> Command {
    let mut cmd = Command::new(
        std::env::var_os("COSMIX_MCP_TEST_NODED")
            .expect("set COSMIX_MCP_TEST_NODED to the freshly built cosmix-noded"),
    );
    cmd.args([
        "serve",
        "--listen",
        address,
        "--node",
        "mcp-test",
        "--no-monitor",
        "--no-log",
    ])
    .env("COSMIX_NODE_CONFIG", config)
    .env("COSMIX_ETC", dir)
    .env("COSMIX_RUN", dir)
    .env("COSMIX_LOG", dir)
    .env("HOME", dir)
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
    cmd
}

async fn drain(stream: impl tokio::io::AsyncRead + Unpin, evidence: Arc<Mutex<Vec<u8>>>) {
    let mut reader = stream.take(256 * 1024);
    let mut buffer = [0; 4096];
    while let Ok(size) = reader.read(&mut buffer).await {
        if size == 0 {
            break;
        }
        evidence.lock().unwrap().extend_from_slice(&buffer[..size]);
    }
}

async fn ready(url: &str, broker: &mut Child) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            broker.try_wait().unwrap().is_none(),
            "isolated noded exited before readiness"
        );
        if let Ok(Ok(client)) = tokio::time::timeout(
            Duration::from_millis(250),
            NodedClient::connect_anonymous(url),
        )
        .await
        {
            client.close().await;
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "isolated noded readiness exceeded 5s"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

impl Fixture {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        drop(listener);
        let config = dir.path().join("node.conf.mix");
        // Explicit config and socket paths prevent falling into a developer's
        // live broker. Strict-data Mix maps are also valid JSON.
        std::fs::write(
            &config,
            json!({"node":"mcp-test", "wg_ip":"127.0.0.1",
            "noded":{"port":address.rsplit(':').next().unwrap().parse::<u16>().unwrap(),
                "unix_socket":dir.path().join("bus.sock")}})
            .to_string(),
        )
        .unwrap();
        let mut broker = broker_command(&config, dir.path(), &address)
            .spawn()
            .unwrap();
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let broker_log = tokio::spawn(drain(broker.stderr.take().unwrap(), stderr.clone()));
        ready(&format!("ws://{address}/ws"), &mut broker).await;
        let mut mcp = Command::new(env!("CARGO_BIN_EXE_cosmix-mcp"))
            .current_dir(dir.path())
            .env("COSMIX_NODE_CONFIG", &config)
            .env("COSMIX_ETC", dir.path())
            .env("COSMIX_RUN", dir.path())
            .env("COSMIX_LOG", dir.path())
            .env("HOME", dir.path())
            .env("RUST_LOG", "cosmix_mcp=debug")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mcp_log = tokio::spawn(drain(mcp.stderr.take().unwrap(), stderr.clone()));
        let observer = Observer::default();
        // ServiceExt performs the real initialize/initialized handshake.
        let client = tokio::time::timeout(
            Duration::from_secs(5),
            observer
                .clone()
                .serve((mcp.stdout.take().unwrap(), mcp.stdin.take().unwrap())),
        )
        .await
        .expect("MCP handshake timeout")
        .expect("MCP handshake failed");
        Self {
            dir,
            config,
            address,
            broker,
            mcp,
            client,
            observer,
            tasks: vec![broker_log, mcp_log],
            owned: Vec::new(),
            stderr,
        }
    }

    fn url(&self) -> String {
        format!("ws://{}/ws", self.address)
    }

    async fn call(&self, tool: &str, args: Value) -> CallToolResult {
        tokio::time::timeout(
            Duration::from_secs(8),
            self.client.call_tool(
                CallToolRequestParams::new(tool.to_owned())
                    .with_arguments(args.as_object().unwrap().clone()),
            ),
        )
        .await
        .expect("MCP call exceeded harness bound")
        .expect("MCP request failed")
    }

    async fn shutdown(mut self) {
        self.client.cancellation_token().cancel();
        for child in &mut self.owned {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        }
        let _ = self.mcp.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(2), self.mcp.wait()).await;
        let _ = self.broker.start_kill();
        for child in &mut self.owned {
            let _ = child.start_kill();
        }
        let _ = tokio::time::timeout(Duration::from_secs(2), self.broker.wait()).await;
        for task in &mut self.tasks {
            if tokio::time::timeout(Duration::from_secs(1), &mut *task)
                .await
                .is_err()
            {
                task.abort();
            }
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.mcp.start_kill();
        let _ = self.broker.start_kill();
        if std::thread::panicking() {
            eprintln!(
                "native MCP fixture evidence: {}",
                String::from_utf8_lossy(&self.stderr.lock().unwrap())
            );
            if let Ok(log) =
                std::fs::read_to_string(self.dir.path().join(".local/log/cosmix/cosmix-mcp.log"))
            {
                eprintln!("MCP log: {log}");
            }
        }
        for task in &self.tasks {
            task.abort();
        }
    }
}

fn text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn success<'a>(result: &'a CallToolResult, tools: &[Tool], name: &str) -> &'a Value {
    assert!(
        !result.is_error.unwrap_or(false),
        "{name}: {}",
        text(result)
    );
    assert!(!text(result).is_empty(), "{name} omitted readable text");
    let value = result
        .structured_content
        .as_ref()
        .expect("successful result omitted structuredContent");
    let schema = tools
        .iter()
        .find(|t| t.name == name)
        .unwrap()
        .output_schema
        .as_ref()
        .unwrap();
    jsonschema::validator_for(&Value::Object((**schema).clone()))
        .unwrap()
        .validate(value)
        .unwrap_or_else(|e| panic!("{name} result violates advertised outputSchema: {e}"));
    value
}

#[derive(Default)]
struct ServiceState {
    deliveries: AtomicUsize,
    mutations: AtomicUsize,
    instance: AtomicU64,
    value: Mutex<Value>,
    last: Mutex<Option<(String, Value, String)>>,
}

async fn protocol_service(
    url: &str,
    name: &str,
    state: Arc<ServiceState>,
) -> (Arc<NodedClient>, JoinHandle<()>) {
    let client = Arc::new(NodedClient::connect(name, url).await.unwrap());
    let mut incoming = client.incoming_async().await.unwrap();
    let owner = client.clone();
    let task = tokio::spawn(async move {
        while let Some(cmd) = incoming.recv().await {
            state.deliveries.fetch_add(1, Ordering::SeqCst);
            *state.last.lock().unwrap() =
                Some((cmd.command.clone(), cmd.args.clone(), cmd.from.clone()));
            let instance = state.instance.load(Ordering::SeqCst);
            let (rc, body) = match cmd.command.as_str() {
                "INFO" => (0, json!({"verbs":[]})),
                "bterm.tabs" => (
                    0,
                    json!(format!(
                        "id=1 active=true title=Shell with spaces cols=80 rows=24 instance={instance}"
                    )),
                ),
                "bterm.panes" => (
                    0,
                    json!(format!(
                        "id=1 active=true cols=80 rows=24 instance={instance}"
                    )),
                ),
                "bterm.type" if cmd.args["instance"].as_u64() != Some(instance) => (
                    10,
                    json!({"error_code":"INVALID_ARGUMENT","message":"instance is not this term process"}),
                ),
                "bterm.type" => {
                    state.mutations.fetch_add(1, Ordering::SeqCst);
                    (0, json!("keys queued"))
                }
                "app.describe" => (0, json!({"app":"fixture", "engine":"ctk"})),
                "app.controls.list" => (
                    0,
                    json!({"controls":[{"id":"volume", "value_type":"number"}]}),
                ),
                "app.controls.get" => (
                    0,
                    json!({"id":"volume","value":state.value.lock().unwrap().clone()}),
                ),
                "app.controls.set" => {
                    *state.value.lock().unwrap() = cmd.args["value"].clone();
                    state.mutations.fetch_add(1, Ordering::SeqCst);
                    (0, json!({"applied":true}))
                }
                "actions.list" => (0, json!({"actions":[{"id":"mute"}]})),
                "action.invoke" => {
                    state.mutations.fetch_add(1, Ordering::SeqCst);
                    (0, json!({"accepted":true}))
                }
                "comp.windows.list" => (0, json!({"windows":[{"id":1,"generation":3}]})),
                "comp.window.wait" => (0, json!({"matched":true,"id":1,"generation":3})),
                "fixture.slow_mutation" => {
                    state.mutations.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(700)).await;
                    (0, json!({"applied":true}))
                }
                _ => (10, json!({"message":"unknown fixture command"})),
            };
            let _ = owner.respond(&cmd, rc, &body.to_string()).await;
        }
    });
    (client, task)
}

#[tokio::test]
#[ignore = "requires freshly built noded; isolated acceptance command is documented"]
async fn native_mcp_acceptance() {
    // Derived from Gemini's proposed cases; repaired against the pinned SDK,
    // strengthened to require structured schemas and observable effects.
    let mut fixture = Fixture::start().await;
    let tools = fixture.client.list_tools(None).await.unwrap().tools;
    assert!(tools.len() > 20);
    for tool in &tools {
        assert!(
            tool.output_schema.is_some(),
            "{} lacks outputSchema",
            tool.name
        );
        assert!(
            tool.annotations.is_some(),
            "{} lacks annotations",
            tool.name
        );
    }
    for (tool_name, fields) in [
        ("bus_call", vec!["result", "rc"]),
        (
            "mix_execute",
            vec!["result", "stdout", "stderr", "exit_code"],
        ),
        (
            "mcp_status",
            vec!["broker_connected", "total_calls", "recent"],
        ),
    ] {
        let schema = tools
            .iter()
            .find(|t| t.name == tool_name)
            .unwrap()
            .output_schema
            .as_ref()
            .unwrap();
        for field in fields {
            assert!(
                schema["properties"].get(field).is_some(),
                "{tool_name} schema omits {field}"
            );
        }
    }
    for name in [
        "bus_call",
        "mix_execute",
        "app_action_invoke",
        "app_control_set",
    ] {
        let ann = tools
            .iter()
            .find(|t| t.name == name)
            .unwrap()
            .annotations
            .as_ref()
            .unwrap();
        assert_eq!(ann.read_only_hint, Some(false));
        assert_eq!(ann.idempotent_hint, Some(false));
    }
    let status = fixture.call("mcp_status", json!({})).await;
    assert_eq!(
        success(&status, &tools, "mcp_status")["broker_connected"],
        false,
        "initialise/tools-list must not eagerly connect"
    );
    let log_path = fixture
        .dir
        .path()
        .join(".local/log/cosmix/observed-error.log");
    std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
    std::fs::write(log_path, "ERROR: this is an observed log line\n").unwrap();
    let observed = fixture
        .call("log_tail", json!({"file":"observed-error.log"}))
        .await;
    assert_eq!(
        success(&observed, &tools, "log_tail")["result"],
        "ERROR: this is an observed log line"
    );
    let missing = fixture.call("log_tail", json!({"file":"absent.log"})).await;
    assert!(missing.is_error.unwrap_or(false));

    let terminal = Arc::new(ServiceState::default());
    terminal.instance.store(55, Ordering::SeqCst);
    let (term_client, term_task) =
        protocol_service(&fixture.url(), "bterm", terminal.clone()).await;
    fixture.tasks.push(term_task);
    let rejected = fixture
        .call("term_type", json!({"text":"hello", "pane":1}))
        .await;
    assert!(rejected.is_error.unwrap_or(false));
    assert!(text(&rejected).contains("term_list"));
    assert_eq!(terminal.deliveries.load(Ordering::SeqCst), 0);
    let listing = fixture.call("term_list", json!({})).await;
    let listed = success(&listing, &tools, "term_list");
    assert_eq!(listed["service"], "bterm");
    assert_eq!(listed["instance"], 55);
    assert_eq!(listed["tabs"][0]["title"], "Shell with spaces");
    let typed = fixture
        .call("term_type", json!({"text":"hello", "pane":1}))
        .await;
    success(&typed, &tools, "term_type");
    assert_eq!(terminal.mutations.load(Ordering::SeqCst), 1);
    terminal.instance.store(99, Ordering::SeqCst);
    let stale = fixture
        .call("term_type", json!({"text":"hello", "pane":1}))
        .await;
    assert!(stale.is_error.unwrap_or(false));
    assert!(text(&stale).contains("restarted since term_list"));
    assert_eq!(
        terminal.mutations.load(Ordering::SeqCst),
        1,
        "stale target caused a mutation"
    );

    let deliveries = terminal.deliveries.load(Ordering::SeqCst);
    for args in [
        json!({"to":"bterm","command":"bterm.type","args":"{"}),
        json!({"to":"bterm","command":"bterm.type","typo":true}),
    ] {
        let rejected = fixture.call("bus_call", args).await;
        assert!(rejected.is_error.unwrap_or(false));
    }
    assert_eq!(terminal.deliveries.load(Ordering::SeqCst), deliveries);
    let app_error = fixture
        .call("bus_call", json!({"to":"bterm","command":"unknown"}))
        .await;
    assert!(app_error.is_error.unwrap_or(false));
    assert_eq!(
        app_error.structured_content.as_ref().unwrap()["result"]["rc"],
        10
    );
    assert!(text(&app_error).contains("unknown fixture command"));

    let app = Arc::new(ServiceState::default());
    let (app_client, app_task) = protocol_service(&fixture.url(), "fixture-app", app.clone()).await;
    fixture.tasks.push(app_task);
    for name in [
        "app_describe",
        "app_controls_list",
        "app_actions_list",
        "desktop_windows",
    ] {
        let result = fixture.call(name, json!({"service":"fixture-app"})).await;
        success(&result, &tools, name);
    }
    let result = fixture
        .call(
            "app_control_set",
            json!({"service":"fixture-app", "target":"volume", "value":0.75}),
        )
        .await;
    success(&result, &tools, "app_control_set");
    assert_eq!(*app.value.lock().unwrap(), json!(0.75));
    assert!(
        app.last
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .2
            .starts_with("mcp-"),
        "native caller must be registered"
    );
    let observed = fixture
        .call(
            "app_control_wait",
            json!({"service":"fixture-app", "target":"volume", "value":0.75, "timeout_ms":1000}),
        )
        .await;
    assert_eq!(
        success(&observed, &tools, "app_control_wait")["result"]["value"],
        0.75
    );
    let mismatch = fixture
        .call(
            "app_control_wait",
            json!({"service":"fixture-app", "target":"volume", "value":0.1, "timeout_ms":80}),
        )
        .await;
    assert!(mismatch.is_error.unwrap_or(false));
    let wait = fixture.call("desktop_window_wait", json!({"service":"fixture-app", "window":{"id":1,"generation":3}, "until":"presented", "timeout_ms":1000})).await;
    success(&wait, &tools, "desktop_window_wait");
    assert_eq!(
        app.last.lock().unwrap().as_ref().unwrap().1["match"]["generation"],
        3
    );

    // Cancellation stops the local request, not an already delivered mutation.
    let before = app.mutations.load(Ordering::SeqCst);
    let params = CallToolRequestParams::new("bus_call").with_arguments(
        json!({"to":"fixture-app", "command":"fixture.slow_mutation"})
            .as_object()
            .unwrap()
            .clone(),
    );
    let timeout = fixture
        .client
        .send_request_with_option(
            ClientRequest::CallToolRequest(CallToolRequest::new(params)),
            PeerRequestOptions::with_timeout(Duration::from_millis(250)),
        )
        .await
        .unwrap()
        .await_response()
        .await;
    assert!(
        timeout.is_err(),
        "slow mutation did not time out: {timeout:?}; native delivery: {:?}",
        app.last.lock().unwrap()
    );
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(
        app.mutations.load(Ordering::SeqCst),
        before + 1,
        "delivered work was replayed or never delivered"
    );
    success(
        &fixture
            .call(
                "app_control_get",
                json!({"service":"fixture-app", "target":"volume"}),
            )
            .await,
        &tools,
        "app_control_get",
    );

    let isolated = fixture.dir.path().join("script-cwd");
    std::fs::create_dir(&isolated).unwrap();
    let script = "print(cwd())\nrun_stream([\"/usr/bin/printf\",\"inherited-child-stdout\\n\"])";
    let output = fixture
        .call("mix_execute", json!({"script":script, "cwd":isolated}))
        .await;
    let output = success(&output, &tools, "mix_execute");
    assert!(
        output["stdout"]
            .as_str()
            .unwrap()
            .contains("inherited-child-stdout")
    );
    assert!(
        output["stdout"]
            .as_str()
            .unwrap()
            .contains(isolated.to_str().unwrap())
    );
    let other = fixture
        .call("mix_execute", json!({"script":"print(cwd())"}))
        .await;
    assert_eq!(
        success(&other, &tools, "mix_execute")["stdout"]
            .as_str()
            .unwrap()
            .trim(),
        fixture.dir.path().to_str().unwrap()
    );
    let failed = fixture
        .call("mix_execute", json!({"script":"no_such_builtin()"}))
        .await;
    assert!(failed.is_error.unwrap_or(false));
    let overflowing = format!(
        "$s={}\nfor $i = 1 to 6\n print($s)\nend",
        serde_json::to_string(&"x".repeat(200_000)).unwrap()
    );
    let overflow = fixture
        .call("mix_execute", json!({"script":overflowing}))
        .await;
    assert!(overflow.is_error.unwrap_or(false));
    assert!(
        text(&overflow).contains("exceeded"),
        "bounded capture did not report its limit"
    );
    let native = fixture.call("mix_execute", json!({"script":"send \"fixture-app\" \"app.controls.get\" target=\"volume\"\nprint($reply)"})).await;
    success(&native, &tools, "mix_execute");
    assert!(
        text(&native).contains("0.75"),
        "worker did not use native Bus"
    );

    // Cancellation must kill the evaluator AND its inherited process group.
    // run_argv starts a separate child group: its native parent-death
    // backstop must also work when the evaluator is terminated.
    for runner in ["run_stream", "run_argv"] {
        let started = fixture.dir.path().join(format!("{runner}-started"));
        let escaped = fixture.dir.path().join(format!("{runner}-escaped"));
        let nested = format!(
            "write_file({},\"started\"); sleep(3); write_file({},\"escaped\")",
            serde_json::to_string(&started).unwrap(),
            serde_json::to_string(&escaped).unwrap()
        );
        let script = format!(
            "{runner}([\"/opt/cosmix/bin/mix\",\"-c\",{}])",
            serde_json::to_string(&nested).unwrap()
        );
        let params = CallToolRequestParams::new("mix_execute")
            .with_arguments(json!({"script":script}).as_object().unwrap().clone());
        let cancelled = fixture
            .client
            .send_request_with_option(
                ClientRequest::CallToolRequest(CallToolRequest::new(params)),
                PeerRequestOptions::with_timeout(Duration::from_secs(1)),
            )
            .await
            .unwrap()
            .await_response()
            .await;
        assert!(cancelled.is_err());
        assert!(
            started.is_file(),
            "worker never started; cancellation assertion would be vacuous"
        );
        tokio::time::sleep(Duration::from_millis(3200)).await;
        assert!(
            !escaped.exists(),
            "cancelled worker descendant continued executing"
        );
    }

    fixture.observer.0.lock().unwrap().clear();
    let handle = fixture
        .client
        .send_request_with_option(
            ClientRequest::CallToolRequest(CallToolRequest::new(CallToolRequestParams::new(
                "mcp_status",
            ))),
            PeerRequestOptions::with_timeout(Duration::from_secs(2)),
        )
        .await
        .unwrap();
    let token = handle.progress_token.clone();
    handle.await_response().await.unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    let progress = fixture.observer.0.lock().unwrap().clone();
    assert_eq!(
        progress.len(),
        2,
        "missing requested progress notifications"
    );
    assert_eq!(
        progress.iter().map(|p| p.progress).collect::<Vec<_>>(),
        vec![1.0, 2.0]
    );
    assert!(progress.iter().all(|p| p.progress_token == token));

    // Actual connection liveness, then lazy reconnection on a new request.
    fixture.broker.kill().await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let status = fixture.call("mcp_status", json!({})).await;
        if success(&status, &tools, "mcp_status")["broker_connected"] == false {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "stale broker liveness"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    fixture.broker = broker_command(&fixture.config, fixture.dir.path(), &fixture.address)
        .spawn()
        .unwrap();
    fixture.tasks.push(tokio::spawn(drain(
        fixture.broker.stderr.take().unwrap(),
        fixture.stderr.clone(),
    )));
    ready(&fixture.url(), &mut fixture.broker).await;
    success(
        &fixture.call("bus_list_services", json!({})).await,
        &tools,
        "bus_list_services",
    );
    let status = fixture.call("mcp_status", json!({})).await;
    assert_eq!(
        success(&status, &tools, "mcp_status")["broker_connected"],
        true
    );
    term_client.close().await;
    app_client.close().await;
    fixture.shutdown().await;
}

#[tokio::test]
#[ignore = "requires freshly built noded and CTK mcp_control_probe"]
async fn owning_ctk_mcp_acceptance() {
    let mut fixture = Fixture::start().await;
    let tools = fixture.client.list_tools(None).await.unwrap().tools;
    let mut probe = Command::new(
        std::env::var_os("COSMIX_MCP_TEST_CTK_PROBE")
            .expect("set COSMIX_MCP_TEST_CTK_PROBE to the freshly built CTK example"),
    )
    .env("COSMIX_MCP_TEST_URL", fixture.url())
    .env("COSMIX_NODE_CONFIG", &fixture.config)
    .env("COSMIX_ETC", fixture.dir.path())
    .env("COSMIX_RUN", fixture.dir.path())
    .env("COSMIX_LOG", fixture.dir.path())
    .env("HOME", fixture.dir.path())
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .kill_on_drop(true)
    .spawn()
    .unwrap();
    fixture.tasks.push(tokio::spawn(drain(
        probe.stderr.take().unwrap(),
        fixture.stderr.clone(),
    )));
    fixture.owned.push(probe);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let result = fixture
            .call("app_describe", json!({"service":"mcp-control-probe"}))
            .await;
        if !result.is_error.unwrap_or(false) {
            success(&result, &tools, "app_describe");
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "CTK native port never became ready: {}",
            text(&result)
        );
        assert!(
            fixture.owned[0].try_wait().unwrap().is_none(),
            "CTK probe exited"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let listed = fixture
        .call("app_controls_list", json!({"service":"mcp-control-probe"}))
        .await;
    assert_eq!(
        success(&listed, &tools, "app_controls_list")["result"]["controls"][0]["id"],
        "trim"
    );
    let set = fixture
        .call(
            "app_control_set",
            json!({"service":"mcp-control-probe", "target":"trim", "value":6.0}),
        )
        .await;
    success(&set, &tools, "app_control_set");
    let value = fixture
        .call(
            "app_control_wait",
            json!({"service":"mcp-control-probe", "target":"trim", "value":6.0, "timeout_ms":1000}),
        )
        .await;
    assert_eq!(
        success(&value, &tools, "app_control_wait")["result"]["value"],
        6.0
    );
    let changes = fixture
        .call(
            "bus_call",
            json!({"to":"mcp-control-probe", "command":"app.probe"}),
        )
        .await;
    assert_eq!(
        success(&changes, &tools, "bus_call")["result"]["final_changes"],
        1
    );
    let bad = fixture
        .call(
            "app_control_set",
            json!({"service":"mcp-control-probe", "target":"trim", "value":"wrong type"}),
        )
        .await;
    assert!(bad.is_error.unwrap_or(false), "CTK admitted wrong type");
    let unknown = fixture
        .call(
            "app_control_set",
            json!({"service":"mcp-control-probe", "target":"missing", "value":0.0}),
        )
        .await;
    assert!(
        unknown.is_error.unwrap_or(false),
        "CTK admitted unknown control"
    );
    let value = fixture
        .call(
            "app_control_get",
            json!({"service":"mcp-control-probe", "target":"trim"}),
        )
        .await;
    assert_eq!(
        success(&value, &tools, "app_control_get")["result"]["value"],
        6.0
    );
    let changes = fixture
        .call(
            "bus_call",
            json!({"to":"mcp-control-probe", "command":"app.probe"}),
        )
        .await;
    assert_eq!(
        success(&changes, &tools, "bus_call")["result"]["final_changes"],
        1,
        "rejected writes entered CTK change pipeline"
    );
    fixture.shutdown().await;
}
