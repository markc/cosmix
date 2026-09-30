//! Serve-mode Bus error-honesty acceptance: the built `mix --serve`
//! subprocess (a real `MixServeHandler` over a real `SupervisedClient`)
//! booted against a real embedded `cosmix-noded` broker
//! (term-native-test-broker). No source mocks, no seam assertions:
//!
//! - connected: `port_exists()` on a successfully retrieved service list
//!   missing the target answers `false`, and both `emit` shapes (Map →
//!   header routing, scalar → JSON body) succeed on the live link;
//! - after the owned broker stops, port_exists raises a catchable transport
//!   error while language-level fire-and-forget emit remains nonfatal.
//!
//! The citizen probes in three looping tasks (one per operation) and
//! appends results to a test-owned trace file. The lookup handler catches and
//! records its error; both emit handlers keep recording progress. Both public
//! emit forms use the Map branch internally. The pre-cut successes establish
//! that the later lookup failure comes from the cut.
//!
//! Run: `cargo test -p cosmix-mix --test serve_bus_honesty`
//!      (the workspace gate runs it: `cargo test --workspace`)
#![cfg(target_os = "linux")]

use std::fs::File;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use cosmix_lib_client::{NodedClient, UnixConnectOutcome, VerifiedConnection};
use serde_json::{Value, json};
use term_native_test_broker::Broker;

const SVC: &str = "honest-probe";
const HARD: Duration = Duration::from_secs(30);

/// One whole fixture at a time: the fixed TCP port reservation and the
/// embedded broker must not race sibling tests in this binary.
static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn lock() -> tokio::sync::MutexGuard<'static, ()> {
    LOCK.lock().await
}

// ── fixture scaffolding ──────────────────────────────────────────────────

/// Test-owned temp dir: node.conf, scripts, trace, citizen stderr.
struct Dir(PathBuf);

impl Dir {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = std::env::temp_dir().join(format!(
            "mix-honesty-{tag}-{}-{nanos:x}",
            std::process::id()
        ));
        std::fs::create_dir_all(&d).unwrap();
        Dir(d)
    }
    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let p = self.0.join(name);
        std::fs::write(&p, contents).unwrap();
        p
    }
    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.0.join(name)).unwrap_or_default()
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The `mix --serve` subprocess, in its own process group. Drop SIGKILLs
/// the whole group.
struct Citizen {
    pgid: libc::pid_t,
    child: Child,
}

impl Citizen {
    fn spawn(bin: &Path, dir: &Dir, node_conf: &Path, script: &Path) -> Citizen {
        let stderr = File::create(dir.0.join("citizen.stderr")).unwrap();
        let mut cmd = Command::new(bin);
        cmd.args([
            "--no-prelude",
            "--serve",
            script.to_str().unwrap(),
            "--name",
            SVC,
        ])
        .env("COSMIX_NODE_CONFIG", node_conf)
        .env("COSMIX_ETC", &dir.0)
        .env("HONEST_TRACE", dir.0.join("trace"))
        .env("MIX_STATS", "off")
        .env_remove("COSMIX_SESSION_FD")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr));
        unsafe {
            cmd.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().unwrap();
        Citizen {
            pgid: child.id() as libc::pid_t,
            child,
        }
    }
}

impl Drop for Citizen {
    fn drop(&mut self) {
        unsafe { libc::kill(-self.pgid, libc::SIGKILL) };
        let _ = self.child.wait();
    }
}

fn node_conf_text(port: u16) -> String {
    format!("wg_ip: \"127.0.0.1\"\nnoded: {{ port: {port} }}\n")
}

fn broker_tcp_port(broker: &Broker) -> u16 {
    broker
        .url
        .trim_start_matches("ws://127.0.0.1:")
        .trim_end_matches("/ws")
        .parse()
        .unwrap()
}

// ── trace / stderr (test-owned files, appended by the real citizen) ──────

fn trace(dir: &Dir) -> String {
    dir.read("trace")
}

async fn wait_trace(dir: &Dir, needle: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if trace(dir).contains(needle) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "trace marker {needle:?} not seen within {timeout:?}; trace:\n{}",
        trace(dir)
    );
}

async fn wait_trace_count(dir: &Dir, needle: &str, count: usize, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if trace(dir).matches(needle).count() >= count {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "trace count {count} for {needle:?} not seen within {timeout:?}; trace:\n{}",
        trace(dir)
    );
}

// ── native ABP control ───────────────────────────────────────────────────

async fn connect(broker: &Broker) -> VerifiedConnection {
    let UnixConnectOutcome::VerifiedUnix(c) =
        NodedClient::connect_unix("", &broker.url, &broker.options(), None)
            .await
            .expect("native control connect")
    else {
        panic!("control must be a verified Unix connection");
    };
    c
}

async fn call(c: &VerifiedConnection, cmd: &str, args: Value) -> Result<Value, String> {
    c.client()
        .call(SVC, cmd, args)
        .await
        .map_err(|e| format!("{e:#}"))
}

async fn wait_service(c: &VerifiedConnection, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if call(c, "rel.state", json!({})).await.is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "service {SVC} never answered within {timeout:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

// ── the citizen under test ───────────────────────────────────────────────

/// Three looping probe tasks: `pe` logs `port_exists` results, `emit` /
/// `emitm` log after a successful header-routed (Map) / scalar-body emit.
/// Lookup records its caught transport error; emits retain their established
/// nonfatal language contract.
const SCRIPT: &str = r#"-- version: 0.1.0
$TRACE = env("HONEST_TRACE")
fn tlog($what, $fields)
  append_file($TRACE, $what .. "|" .. json_encode($fields) .. "|t=" .. monotonic() .. chr(10))
end
on rel.state
  reply(0, json_encode({state: "ok"}))
end
on probe_pe async
  sleep(1)
  while true
    try
      tlog("pe", {v: port_exists("no-such-svc")})
    catch $message, $error
      tlog("pe_error", {message:$message})
      return
    end
    sleep(0.15)
  end
end
on probe_emit async
  sleep(1)
  while true
    emit "no-such-svc" ping k="v"
    tlog("emit", {v: true})
    sleep(0.15)
  end
end
on probe_emits async
  sleep(1)
  while true
    emit "no-such-svc" ping body="scalar-body"
    tlog("emitm", {v: true})
    sleep(0.15)
  end
end
task_start("probe_pe", json_encode({}))
task_start("probe_emit", json_encode({}))
task_start("probe_emits", json_encode({}))
"#;

#[tokio::test]
async fn lookup_reports_transport_failure_and_emit_remains_nonfatal() {
    let _g = lock().await;
    let dir = Dir::new("honesty");
    let mut broker = Broker::start();
    let node_conf = dir.write("node.conf.mix", &node_conf_text(broker_tcp_port(&broker)));
    let script = dir.write("svc.mix", SCRIPT);
    let _citizen = Citizen::spawn(
        Path::new(env!("CARGO_BIN_EXE_mix")),
        &dir,
        &node_conf,
        &script,
    );
    let c = connect(&broker).await;
    wait_service(&c, HARD).await;

    // Connected: a successfully retrieved list missing the target is
    // `false`, and both emit shapes succeed on the live link. These
    // successes prove the post-cut raises below are caused by the cut.
    wait_trace(&dir, "pe|{\"v\":false}", HARD).await;
    wait_trace(&dir, "emit|{\"v\":true}", HARD).await;
    wait_trace(&dir, "emitm|{\"v\":true}", HARD).await;

    // Lookup must distinguish absent from unreachable. Language-level
    // fire-and-forget emit remains nonfatal, including during an outage.
    let emits = trace(&dir).matches("emit|").count();
    let emitms = trace(&dir).matches("emitm|").count();
    broker.stop();
    wait_trace(
        &dir,
        "pe_error|{\"message\":\"mesh unavailable: serve port_exists(no-such-svc)",
        HARD,
    )
    .await;
    wait_trace_count(&dir, "emit|", emits + 2, HARD).await;
    wait_trace_count(&dir, "emitm|", emitms + 2, HARD).await;
}
