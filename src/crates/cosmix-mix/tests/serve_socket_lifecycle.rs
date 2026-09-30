#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::fs::File;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use cosmix_lib_client::{NodedClient, UnixConnectOutcome, VerifiedConnection};
use serde_json::{Value, json};
use term_native_test_broker::Broker;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

const SVC: &str = "sock-test";
const HARD: Duration = Duration::from_secs(15);
const PING_DURATION: Duration = Duration::from_millis(1500);
const IDLE_DURATION: Duration = Duration::from_secs(20);

static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn lock() -> tokio::sync::MutexGuard<'static, ()> {
    LOCK.lock().await
}

struct Dir(PathBuf);

impl Dir {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d =
            std::env::temp_dir().join(format!("mix-sock-{tag}-{}-{nanos:x}", std::process::id()));
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

struct Citizen {
    pgid: libc::pid_t,
    child: Child,
    exited: bool,
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
        .env("REL_TRACE", dir.0.join("trace"))
        .env("REL_STATE", dir.0.join("state"))
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
            exited: false,
        }
    }
}

impl Drop for Citizen {
    fn drop(&mut self) {
        if !self.exited {
            unsafe { libc::kill(-self.pgid, libc::SIGKILL) };
            let _ = self.child.wait();
        }
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

#[derive(Debug)]
struct TraceLine {
    fields: Value,
}

fn parse_trace(dir: &Dir, event: &str) -> Vec<TraceLine> {
    dir.read("trace")
        .lines()
        .filter_map(|l| {
            let mut parts = l.splitn(3, '|');
            if parts.next()? != event {
                return None;
            }
            let fields: Value = serde_json::from_str(parts.next()?).ok()?;
            Some(TraceLine { fields })
        })
        .collect()
}

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

async fn wait_lines(dir: &Dir, event: &str, count: usize, timeout: Duration) -> Vec<TraceLine> {
    let deadline = Instant::now() + timeout;
    loop {
        let v = parse_trace(dir, event);
        if v.len() >= count {
            return v;
        }
        assert!(
            Instant::now() < deadline,
            "expected {count} {event} lines within {timeout:?}; trace:\n{}",
            trace(dir)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

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
    tokio::time::timeout(HARD, c.client().call(SVC, cmd, args))
        .await
        .map_err(|_| format!("ABP {cmd} deadline expired"))?
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

fn count_switches(pid: u32) -> BTreeMap<String, u64> {
    let mut totals = BTreeMap::new();
    let task_dir = format!("/proc/{}/task", pid);
    for entry in std::fs::read_dir(&task_dir).expect("citizen task directory") {
        let entry = entry.expect("citizen thread entry");
        let status_path = entry.path().join("status");
        let status = std::fs::read_to_string(&status_path).expect("citizen thread status");
        let mut total = 0;
        let mut found = 0;
        for line in status.lines() {
            if (line.starts_with("voluntary_ctxt_switches:")
                || line.starts_with("nonvoluntary_ctxt_switches:"))
                && let Some(num_str) = line.split_whitespace().last()
            {
                total += num_str.parse::<u64>().expect("context switch count");
                found += 1;
            }
        }
        assert_eq!(found, 2, "both switch counters must exist");
        totals.insert(entry.file_name().to_string_lossy().into_owned(), total);
    }
    assert!(!totals.is_empty(), "citizen must have live threads");
    totals
}

const MIX_SCRIPT: &str = r#"
$s = {}
$TRACE = env("REL_TRACE")
fn tlog($what, $fields)
  append_file($TRACE, $what .. "|" .. json_encode($fields) .. "|t=" .. monotonic() .. chr(10))
end
on rel.state
  reply(0, "{}")
end
on lifecycle.commit
  tlog("commit", {})
end

on setup_park
  $s.h = tcp_connect("127.0.0.1", $event.args.port, {timeout: 5})
  reply(0, "{}")
end
on park async
  tlog("park-ready", {})
  $r = tcp_recv($s.h, {timeout: 8})
  tlog("park-done", {len: len($r ?? "")})
  reply(0, "{}")
end
on busy async
  try
    $r = tcp_recv($s.h, {timeout: 0.1})
    reply(0, json_encode({ok: true}))
  catch $m,$e
    reply(0, json_encode({ok: false, err: $e.code}))
  end
end
on ping async
  reply(0, "{}")
end

on setup_sub
  $s.sub_h = tcp_connect("127.0.0.1", $event.args.port, {timeout: 5})
  $s.sub = tcp_on($s.sub_h, "my.frame", {frame: "line"})
  reply(0, "{}")
end
on my.frame async
  if type($event.args.closed) == "map" then
     tlog("eof", {})
  else
     $d = $event.args.frame.data
     tcp_send($s.sub_h, "ack:" .. $d .. "\n")
     tlog("frame", {d: $d})
  end
end
"#;

#[tokio::test]
async fn test_serve_socket_lifecycle() {
    let _g = lock().await;
    let dir = Dir::new("sock-life");
    let mut broker = Broker::start();

    let bin_path = std::env::var("MIX_SOCKET_TEST_BIN")
        .unwrap_or_else(|_| env!("CARGO_BIN_EXE_mix").to_string());

    let node_conf = dir.write("node.conf.mix", &node_conf_text(broker_tcp_port(&broker)));
    let script = dir.write("svc.mix", MIX_SCRIPT);

    let mut citizen = Citizen::spawn(Path::new(&bin_path), &dir, &node_conf, &script);
    let c1 = connect(&broker).await;
    let c2 = connect(&broker).await;
    wait_service(&c1, HARD).await;

    let park_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let park_port = park_listener.local_addr().unwrap().port();
    let sub_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let sub_port = sub_listener.local_addr().unwrap().port();

    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = tokio::time::timeout(HARD, park_listener.accept())
            .await
            .expect("park accept deadline")
            .unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        tokio::time::timeout(HARD, release_rx)
            .await
            .expect("park release deadline")
            .expect("park release");
        stream.write_all(b"done").await.unwrap();
    });

    // ── Phase 1: Async tcp_recv blocking, SOCKET_BUSY, and latency ──
    call(&c1, "setup_park", json!({"port": park_port}))
        .await
        .unwrap();

    let park_c = connect(&broker).await;
    let park_handle = tokio::spawn(async move { call(&park_c, "park", json!({})).await });

    wait_trace(&dir, "park-ready", HARD).await;

    let mut latencies = Vec::new();
    let ping_end = Instant::now() + PING_DURATION;
    while Instant::now() < ping_end {
        let t0 = Instant::now();
        call(&c2, "ping", json!({})).await.unwrap();
        latencies.push(t0.elapsed().as_micros());
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let busy_res = call(&c2, "busy", json!({})).await.unwrap();
    assert_eq!(busy_res["ok"], false);
    assert!(
        busy_res["err"].as_str().unwrap().contains("SOCKET_BUSY"),
        "expected deterministically refused second recv, got: {}",
        busy_res["err"]
    );

    release_tx.send(()).expect("release parked receive");
    park_handle.await.unwrap().unwrap();
    peer.await.unwrap();

    latencies.sort_unstable();
    let min = latencies[0];
    let max = latencies.last().unwrap();
    let median = latencies[latencies.len() / 2];
    let p90 = latencies[(latencies.len() as f64 * 0.9) as usize];

    let mode = std::env::var("MIX_SOCKET_TEST_PROFILE").unwrap_or_else(|_| {
        assert!(
            std::env::var_os("MIX_SOCKET_TEST_BIN").is_none(),
            "binary override requires explicit MIX_SOCKET_TEST_PROFILE"
        );
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
        .to_string()
    });
    assert!(
        matches!(mode.as_str(), "debug" | "release"),
        "profile must be debug or release"
    );
    let is_release = mode == "release";
    println!("Phase 1: park & ping latency test");
    println!("mode: {}", if is_release { "release" } else { "debug" });
    println!("samples: {}", latencies.len());
    println!(
        "min: {} us, median: {} us, p90: {} us, max: {} us",
        min, median, p90, max
    );

    assert!(
        latencies.len() >= 50,
        "expected >= 50 roundtrips while parked, got {}",
        latencies.len()
    );

    let limit = if is_release { 1000 } else { 100000 };
    assert!(
        p90 < limit,
        "p90 latency {} us exceeds limit {} us",
        p90,
        limit
    );

    // ── Phase 2: tcp_on subscription, context switches, FIFO, and EOF ──
    call(&c1, "setup_sub", json!({"port": sub_port}))
        .await
        .unwrap();
    let (mut sub_stream, _) = sub_listener.accept().await.unwrap();

    tokio::time::sleep(Duration::from_millis(500)).await;

    let pid = citizen.child.id();
    let sw1 = count_switches(pid);

    println!(
        "\nPhase 2: measuring idle context switches for {}s...",
        IDLE_DURATION.as_secs()
    );
    tokio::time::sleep(IDLE_DURATION).await;

    let sw2 = count_switches(pid);
    assert_eq!(
        sw1.keys().collect::<Vec<_>>(),
        sw2.keys().collect::<Vec<_>>(),
        "idle thread set must remain stable"
    );
    let before: u64 = sw1.values().sum();
    let after: u64 = sw2.values().sum();
    let delta = after
        .checked_sub(before)
        .expect("stable thread counters are monotonic");
    println!(
        "Context switches over 20s idle: {} -> {} (delta: {}, rate: {:.2}/s)",
        before,
        after,
        delta,
        delta as f64 / IDLE_DURATION.as_secs_f64()
    );

    assert!(
        delta <= 15,
        "expected bounded background native bookkeeping <= 15, got delta {delta}"
    );

    for i in 0..20 {
        sub_stream
            .write_all(format!("idx:{i}\n").as_bytes())
            .await
            .unwrap();
    }

    let mut reader = tokio::io::BufReader::new(&mut sub_stream);
    for i in 0..20 {
        let mut line = String::new();
        tokio::time::timeout(HARD, reader.read_line(&mut line))
            .await
            .expect("ack read timeout")
            .unwrap();
        assert_eq!(line.trim(), format!("ack:idx:{i}"));
    }

    let traces = wait_lines(&dir, "frame", 20, HARD).await;
    for (i, trace) in traces.iter().enumerate().take(20) {
        assert_eq!(trace.fields["d"].as_str().unwrap(), format!("idx:{i}"));
    }

    call(&c1, "RELOAD", json!({})).await.unwrap();

    let mut eof_buf = [0u8; 10];
    let n = tokio::time::timeout(HARD, reader.read(&mut eof_buf))
        .await
        .expect("bounded EOF wait timeout")
        .unwrap();
    assert_eq!(
        n, 0,
        "expected generation retirement EOF bounded on socket, got data"
    );
    assert!(
        citizen.child.try_wait().unwrap().is_none(),
        "reload must keep the citizen alive: {}",
        dir.read("citizen.stderr")
    );
    wait_service(&c1, HARD).await;
    let props = call(&c1, "sock-test.props.get", json!({}))
        .await
        .expect("post-reload lifecycle");
    assert_eq!(
        props["lifecycle"]["generation"].as_u64(),
        Some(1),
        "EOF must belong to a committed new generation"
    );

    broker.stop();
}
