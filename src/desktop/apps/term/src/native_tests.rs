//! Exercise iced's shared startup path against production noded's fixture.
use super::*;
use cosmix_bus::native_session::{BindingState, Role};
use cosmix_client::{BrokerAccount, NodedClient, UnixConnectOptions, UnixConnectOutcome};
use serde_json::{Value, json};
use std::time::Duration;

#[test]
fn native_lane_round_trip() {
    // Isolate shell startup configuration from other tests and the operator's
    // Mix rc. No process-wide environment mutation alongside PTY threads.
    if std::env::var_os("TERM_C7_FIXTURE").is_none() {
        let broker = term_native_test_broker::Broker::start();
        let root = broker.endpoint.parent().unwrap();
        std::fs::write(root.join(".mixrc"), "fn prompt()\nreturn \"C7> \"\nend\n").unwrap();
        let config = root.join("node.mix");
        std::fs::write(
            &config,
            format!(
                "noded: {{ unix_socket: {} }}\n",
                serde_json::to_string(&broker.endpoint).unwrap()
            ),
        )
        .unwrap();
        // SAFETY: caller-owned passwd storage and buffer; copy the name before
        // they go out of scope. Other parallel tests may also query accounts.
        let account = unsafe {
            let mut storage = std::mem::MaybeUninit::<libc::passwd>::uninit();
            let mut buffer = vec![0u8; 65536];
            let mut entry = std::ptr::null_mut();
            assert_eq!(
                libc::getpwuid_r(
                    libc::geteuid(),
                    storage.as_mut_ptr(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    &mut entry
                ),
                0
            );
            assert!(!entry.is_null());
            std::ffi::CStr::from_ptr((*entry).pw_name)
                .to_string_lossy()
                .into_owned()
        };
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "native_tests::native_lane_round_trip",
                "--nocapture",
            ])
            .env("TERM_C7_FIXTURE", &broker.endpoint)
            .env("TERM_C7_URL", &broker.url)
            .env("HOME", root)
            .env("COSMIX_SRC", root)
            .env("COSMIX_NODE_CONFIG", config)
            .env("COSMIX_BROKER_ACCOUNT", account)
            .env("COSMIX_MESH_OPEN", "1")
            .env("COSMIX_TERM_POLICY", "default-open")
            .env("MIX_STATS", "off")
            .env("MIX_EDITOR", "owned")
            .output()
            .unwrap();
        assert!(
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let mut lane = NativeLane::start();
    let tabs = Arc::new(Mutex::new(
        lane.open_tabs(config::Settings {
            config: config::Config::default(),
            term: "xterm-256color",
        })
        .unwrap(),
    ));
    let (cleanup, reaper) = tabs::Cleanup::start().unwrap();
    lane.install_control(tabs.clone(), cleanup.clone());
    let wake = WakeFd::new().unwrap();
    tabs.lock().unwrap().set_wake(wake.waker());
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let mut options = UnixConnectOptions::new(BrokerAccount {
            // SAFETY: process credentials, shared with the fixture broker.
            uid: unsafe { libc::geteuid() }, gid: unsafe { libc::getegid() },
        });
        options.endpoint = Some(std::env::var_os("TERM_C7_FIXTURE").unwrap().into());
        options.require_native_session = true;
        let UnixConnectOutcome::VerifiedUnix(client) = NodedClient::connect_unix(
            "", &std::env::var("TERM_C7_URL").unwrap(), &options, None,
        ).await.unwrap() else { panic!("native ingress required") };
        let (parent, child) = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let records = client.session_list().await.unwrap().records;
                let parent = records.iter().find(|r| r.role == Role::Term && r.state == BindingState::Attached);
                let child = records.iter().find(|r| r.role == Role::PaneShell && r.state == BindingState::Attached);
                if let (Some(parent), Some(child)) = (parent, child) {
                    break (parent.clone(), child.clone());
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }).await.expect("frontend supervisor and first pane must attach");
        let target = json!({"instance_id":parent.instance_id,"incarnation":parent.incarnation,
            "pane_id":child.pane_id,"pane_generation":child.pane_generation});
        let list = call(client.client(), &parent.name, "term.list", json!({"target":target})).await;
        assert_eq!(list["panes"][0]["target"], target);
        let shell_target = json!({"version":1,"target":{
            "broker_epoch":child.broker_epoch,"record":child.reference(),
            "instance_id":child.instance_id,"pane_id":child.pane_id,"pane_generation":child.pane_generation}});
        let generation = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let status = call(client.client(), &child.name, "shell.status", shell_target.clone()).await;
                if status["status"]["snapshot"]["phase"] == "prompt-ready" {
                    break status["status"]["snapshot"]["prompt_generation"].clone();
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }).await.unwrap();
        // Iced consumes its first frame before the next output wake. Rio
        // coalesces RenderRoute until that consuming capture rearms damage.
        tabs.lock().unwrap().pane_by_id(child.pane_id.unwrap().0).unwrap().lock().unwrap().grid_snapshot();
        wake.drain();
        let accepted = call(client.client(), &parent.name, "term.execute", json!({
            "target":target,"request_id":"1","request_epoch":list["request_epoch"],
            "prompt_generation":generation,"source":"print(\"C7_EXEC_OK\")"
        })).await;
        assert_eq!(accepted["status"], "accepted");
        let result = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let result = call(client.client(), &parent.name, "term.exec.result", json!({
                    "target":target,"operation_id":accepted["operation_id"]})).await;
                if result["state"] == "finished" { break result; }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }).await.unwrap();
        assert_eq!(result["result"]["outcome"], "completed");
        // The same eventfd consumed by iced's subscription sees native work.
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let snapshot = call(client.client(), &parent.name, "term.snapshot",
                    json!({"target":target,"contents":true})).await;
                if snapshot["text"].as_str().unwrap().contains("C7_EXEC_OK") { break; }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while !wake.drain() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }).await.expect("native execution must wake iced");
        client.client().close().await;
    });
    cleanup.submit(tabs.lock().unwrap().shutdown());
    drop(lane);
    drop(cleanup);
    // Bounded assertion catches retaining Control's cleanup sender on exit.
    let (done, completion) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        reaper.join().unwrap();
        done.send(()).unwrap();
    });
    completion
        .recv_timeout(Duration::from_secs(10))
        .expect("cleanup worker stopped");
}

async fn call(client: &NodedClient, name: &str, verb: &str, body: Value) -> Value {
    let (rc, body, _) = tokio::time::timeout(
        Duration::from_secs(6),
        client.call_with_headers_raw(name, verb, &Default::default(), &body.to_string()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(rc, 0, "{verb}: {body}");
    serde_json::from_str(&body).unwrap()
}

#[test]
fn native_start_failure_keeps_graphics_tabs() {
    let mut lane = NativeLane::from_startup(Err("fixture ingress unavailable".into()));
    let mut tabs = lane
        .open_tabs(config::Settings {
            config: config::Config::default(),
            term: "xterm-256color",
        })
        .unwrap();
    assert_eq!(tabs.list().len(), 1);
    assert!(
        tabs.session_status()["diagnostic"]
            .as_str()
            .unwrap()
            .contains("graphics-only")
    );
    drop(tabs.shutdown());
}

#[test]
fn unavailable_ingress_logs_once_across_retries() {
    if std::env::var_os("TERM_C7_OFFLINE").is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "native_tests::unavailable_ingress_logs_once_across_retries",
                "--nocapture",
            ])
            .env("TERM_C7_OFFLINE", "1")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        assert_eq!(
            stderr.matches("broker/profile unavailable").count(),
            1,
            "{stderr}"
        );
        return;
    }
    let mut options = UnixConnectOptions::new(BrokerAccount {
        // SAFETY: current process credentials, no wire-supplied identity.
        uid: unsafe { libc::geteuid() },
        gid: unsafe { libc::getegid() },
    });
    options.endpoint = Some(
        std::env::temp_dir()
            .join(format!("term-c7-missing-{}", std::process::id()))
            .join("bus.sock"),
    );
    options.require_native_session = true;
    let mut lane =
        NativeLane::from_startup(cosmix_term_core::native_session::Supervisor::with_options(
            options,
            "ws://127.0.0.1:1/ws".into(),
        ));
    let mut tabs = lane
        .open_tabs(config::Settings {
            config: config::Config::default(),
            term: "xterm-256color",
        })
        .unwrap();
    assert_eq!(tabs.list().len(), 1);
    // Span the actor's five-second reconnect cadence. Failed native ingress
    // never delays creation of the graphics-only pane beyond startup's budget.
    std::thread::sleep(Duration::from_secs(6));
    assert!(
        tabs.session_status()["diagnostic"]
            .as_str()
            .unwrap()
            .contains("graphics-only")
    );
    drop(tabs.shutdown());
    drop(lane);
}
