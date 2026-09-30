//! Real `run_serve` hot-reload lifecycle acceptance (SPEC 18 RELOAD /
//! post-commit handover): the built `mix --serve` subprocess booted
//! against a real embedded `cosmix-noded` broker (term-native-test-broker),
//! controlled ONLY over native ABP (a verified Unix `NodedClient`), and
//! observed through a test-owned trace file the citizen appends to. No
//! source mocks, no seam assertions:
//!
//! - an invalid-parse RELOAD and a parsed-but-raising candidate both leave
//!   the boot generation's children running, launch no successor, and leave
//!   `lifecycle.generation` unchanged;
//! - a committed swap retires the old generation's children BEFORE the
//!   `lifecycle.commit` hook launches its successor (the hook itself
//!   records the observation in the trace), launches exactly one successor,
//!   advances the generation by exactly one, and keeps the candidate's own
//!   legacy child;
//! - a wire-spoofed `lifecycle.commit` is refused (rc:10) with no side
//!   effects and is not advertised in HELP;
//! - the local commit event dispatches with the broker CUT during candidate
//!   preparation and the citizen reconciles (reconnect/re-register) when
//!   the broker returns at the same address — no stranded child;
//! - SIGTERM shuts down the whole child group from inside candidate passive
//!   prep and from inside the dispatching commit hook;
//! - the offloaded retirement grace keeps the reactor responsive while a
//!   TERM-ignoring child is being swept (the candidate's heartbeat task
//!   keeps appending through the whole ~2 s grace), and the grace really
//!   waited rather than retiring an empty registry.
//!
//! Every phase waits on trace-file events or ABP replies — no sleeps-and-
//! hopes. The citizen subprocess runs in its own process group and the
//! test guard SIGKILLs the whole group on panic/drop, so a failed
//! assertion cannot leak infinite sleep-producer children.
//!
//! Run: `cargo test -p cosmix-mix --test serve_reload_lifecycle`
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

const SVC: &str = "rel-probe";
const HARD: Duration = Duration::from_secs(15);
const RECONCILE: Duration = Duration::from_secs(90);

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
        let d = std::env::temp_dir().join(format!("mix-reload-{tag}-{}-{nanos:x}", std::process::id()));
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
/// the whole group unless the test observed a clean exit.
struct Citizen {
    pgid: libc::pid_t,
    child: Child,
    exited: bool,
}

impl Citizen {
    fn spawn(bin: &Path, dir: &Dir, node_conf: &Path, script: &Path) -> Citizen {
        let stderr = File::create(dir.0.join("citizen.stderr")).unwrap();
        let mut cmd = Command::new(bin);
        cmd.args(["--no-prelude", "--serve", script.to_str().unwrap(), "--name", SVC])
            .env("COSMIX_NODE_CONFIG", node_conf)
            .env("COSMIX_ETC", &dir.0)
            .env("REL_TRACE", dir.0.join("trace"))
            .env("REL_STATE", dir.0.join("state"))
            .env("MIX_STATS", "off")
            .env_remove("COSMIX_SESSION_FD")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr));
        // Own process group: the test (and the guard) signals the citizen's
        // whole tree, mirroring a supervisor stop.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().unwrap();
        Citizen { pgid: child.id() as libc::pid_t, child, exited: false }
    }

    fn sigterm(&self) {
        unsafe { libc::kill(-self.pgid, libc::SIGTERM) };
    }

    fn wait_exit(&mut self, timeout: Duration) -> std::process::ExitStatus {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().unwrap() {
                self.exited = true;
                return status;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("citizen did not exit within {timeout:?}");
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

/// Reserve a loopback port for a bounce-stable broker (bind `:0`, note,
/// drop — the caller holds the fixture lock until the broker binds it).
fn reserve_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

// ── trace (test-owned file, appended by the real citizen) ────────────────

#[derive(Debug)]
struct TraceLine {
    fields: Value,
    t: f64,
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
            let t = parts.next()?.trim_start_matches("t=").parse::<f64>().ok()?;
            Some(TraceLine { fields, t })
        })
        .collect()
}

fn trace(dir: &Dir) -> String {
    dir.read("trace")
}

fn wait_trace(dir: &Dir, needle: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if trace(dir).contains(needle) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("trace marker {needle:?} not seen within {timeout:?}; trace:\n{}", trace(dir));
}

fn wait_lines(dir: &Dir, event: &str, count: usize, timeout: Duration) -> Vec<TraceLine> {
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
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_stderr(dir: &Dir, needle: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if dir.read("citizen.stderr").contains(needle) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!(
        "stderr marker {needle:?} not seen within {timeout:?}; stderr:\n{}",
        dir.read("citizen.stderr")
    );
}

fn alive(pid: i64) -> bool {
    pid > 0 && unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

fn wait_dead(pid: i64, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!alive(pid), "pid {pid} still alive after {timeout:?}");
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

async fn state(c: &VerifiedConnection) -> Value {
    call(c, "rel.state", json!({})).await.expect("rel.state")
}

async fn props_generation(c: &VerifiedConnection) -> i64 {
    let props = call(c, "rel-probe.props.get", json!({}))
        .await
        .expect("props.get");
    props["lifecycle"]["generation"]
        .as_i64()
        .expect("numeric lifecycle.generation")
}

// ── the citizen under test ───────────────────────────────────────────────

const NORMAL_LEGACY: &str = r#"["sleep", "600"]"#;
const TERM_IGNORING_LEGACY: &str = "rust-term-ignoring-fixture";

/// Native subprocess fixture; invoked only by the grace acceptance test.
#[test]
#[ignore = "subprocess helper, launched explicitly by the grace test"]
fn term_ignoring_child_fixture() {
    use std::io::Write;
    unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN); }
    let mut trace = std::fs::OpenOptions::new().append(true)
        .open(std::env::var_os("REL_TRACE").expect("fixture trace")).unwrap();
    writeln!(trace, "legacy-ready|{{\"pid\":{}}}|t=0", std::process::id()).unwrap();
    loop { std::thread::park(); }
}

#[derive(Clone, Copy)]
struct Variant {
    legacy_cmd: &'static str,
    candidate_sleep: f64,
    candidate_spawns_legacy: bool,
    candidate_raises: bool,
    candidate_heartbeat: bool,
    hook_slow: bool,
}

/// The citizen: boot generation spawns one legacy (`die_with_parent`) and
/// one managed (`die_with_parent` + `exit_event`) child and writes their
/// pids to durable state; a reload candidate rehydrates that state
/// (passive), optionally spawns its own legacy child, optionally raises,
/// and — when committed — its `lifecycle.commit` hook spawns ONE successor
/// while recording whether the old children were already gone.
fn script(v: &Variant) -> String {
    let legacy = if v.legacy_cmd == TERM_IGNORING_LEGACY {
        serde_json::to_string(&vec![
            std::env::current_exe().unwrap().to_str().unwrap().to_string(),
            "--exact".to_string(), "term_ignoring_child_fixture".to_string(),
            "--ignored".to_string(), "--nocapture".to_string(),
        ]).unwrap()
    } else {
        v.legacy_cmd.to_string()
    };
    let cand_spawn = if v.candidate_spawns_legacy {
        "$s.cand_legacy = spawn([\"sleep\", \"600\"], {die_with_parent: true})\n  tlog(\"cand\", {step: \"spawned\", pid: $s.cand_legacy})\n"
    } else {
        ""
    };
    let cand_raise = if v.candidate_raises {
        "replace_must(\"abc\", \"xyz\", \"abc\")\n"
    } else {
        ""
    };
    let cand_hb = if v.candidate_heartbeat {
        "task_start(\"rel.heartbeat\", json_encode({till: monotonic() + 5}))\n"
    } else {
        ""
    };
    let hook_slow = if v.hook_slow {
        "tlog(\"hookstep\", {step: \"start\"})\n  $end = monotonic() + 2\n  while monotonic() < $end\n    $tick = monotonic()\n  end\n  tlog(\"hookstep\", {step: \"end\"})\n"
    } else {
        ""
    };
    format!(
        r#"$s = {{}}
$TRACE = env("REL_TRACE")
$STATE = env("REL_STATE")
fn tlog($what, $fields)
  append_file($TRACE, $what .. "|" .. json_encode($fields) .. "|t=" .. monotonic() .. chr(10))
end
on rel.state
  reply(0, json_encode({{generation: $s.gen ?? 0, legacy: $s.gen0_legacy, managed: $s.gen0_managed, successor: $s.successor ?? 0, cand_legacy: $s.cand_legacy ?? 0}}))
end
on lifecycle.commit
  if len(keys($event.headers)) != 0 then return end
  $s.gen = $event.args.generation
  $s.successor = spawn(["sleep", "600"], {{die_with_parent: true}})
  tlog("commit", {{gen: $s.gen, successor: $s.successor, old_managed: exists("/proc/" .. json_encode($s.gen0_managed)), old_legacy: exists("/proc/" .. json_encode($s.gen0_legacy)), cand_legacy_alive: exists("/proc/" .. json_encode($s.cand_legacy ?? 0))}})
  {hook_slow}end
on rel.heartbeat async
  while monotonic() < $event.args.till
    append_file($TRACE, "hb|" .. monotonic() .. chr(10))
    sleep(0.05)
  end
end
if not is_reload_candidate() then
  $s.gen0_legacy = spawn({legacy}, {{die_with_parent: true}})
  $s.gen0_managed = spawn(["sleep", "600"], {{die_with_parent: true, exit_event: true, tag: "gen0-managed"}})
  tlog("boot", {{legacy: $s.gen0_legacy, managed: $s.gen0_managed}})
  write_file($STATE, json_encode({{legacy: $s.gen0_legacy, managed: $s.gen0_managed}}))
else
  $pids = json_parse(read_file($STATE))
  $s.gen0_legacy = $pids.legacy
  $s.gen0_managed = $pids.managed
  tlog("cand", {{step: "start"}})
  sleep({cand_sleep})
  {cand_spawn}{cand_raise}{cand_hb}tlog("cand", {{step: "done"}})
end
"#,
        legacy = legacy,
        cand_sleep = v.candidate_sleep,
        cand_spawn = cand_spawn,
        cand_raise = cand_raise,
        cand_hb = cand_hb,
        hook_slow = hook_slow,
    )
}

fn boot(citizen_script: &Variant, dir: &Dir, broker: &Broker) -> (PathBuf, Citizen) {
    let node_conf = dir.write("node.conf.mix", &node_conf_text(broker_tcp_port(broker)));
    let script = dir.write("svc.mix", &script(citizen_script));
    let citizen = Citizen::spawn(Path::new(env!("CARGO_BIN_EXE_mix")), dir, &node_conf, &script);
    (node_conf, citizen)
}

// ── acceptance 1: refusals leave the boot generation untouched ───────────

#[tokio::test]
async fn invalid_parse_and_raising_candidate_leave_the_boot_generation_running() {
    let _g = lock().await;
    let dir = Dir::new("refuse");
    let mut broker = Broker::start();
    let variant = Variant {
        legacy_cmd: NORMAL_LEGACY,
        candidate_sleep: 0.2,
        candidate_spawns_legacy: true,
        candidate_raises: true,
        candidate_heartbeat: false,
        hook_slow: false,
    };
    let (_node_conf, _citizen) = boot(&variant, &dir, &broker);
    let c = connect(&broker).await;
    wait_service(&c, HARD).await;

    let boot = parse_trace(&dir, "boot");
    assert_eq!(boot.len(), 1, "boot generation launches exactly once");
    let legacy = boot[0].fields["legacy"].as_i64().unwrap();
    let managed = boot[0].fields["managed"].as_i64().unwrap();
    assert!(alive(legacy) && alive(managed), "boot children must be running");
    assert_eq!(props_generation(&c).await, 0);

    // Phase A — invalid parse: rc:10, the pump never breaks, nothing changes.
    dir.write("svc.mix", "on rel.state\n  reply(0, \"{\"\n");
    let err = call(&c, "RELOAD", json!({}))
        .await
        .expect_err("parse failure must answer rc:10");
    assert!(err.contains("does not parse"), "{err}");
    let st = state(&c).await;
    assert_eq!(st["generation"].as_i64().unwrap(), 0, "generation stable");
    assert_eq!(st["successor"].as_i64().unwrap(), 0, "no successor launched");
    assert!(alive(legacy) && alive(managed), "old children untouched");
    assert!(parse_trace(&dir, "commit").is_empty());

    // Phase B — parses, but the candidate init raises: the swap reverts,
    // the candidate's OWN legacy child is cleaned, the old children live.
    dir.write("svc.mix", &script(&variant));
    let ok = call(&c, "RELOAD", json!({}))
        .await
        .expect("valid source must be accepted with rc:0");
    assert_eq!(ok["reloading"], true);
    let deadline = Instant::now() + HARD;
    while !parse_trace(&dir, "cand").iter().any(|l| l.fields["step"] == "spawned") {
        assert!(Instant::now() < deadline, "candidate did not spawn: {}", trace(&dir));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    wait_stderr(&dir, "reload reverted", HARD);
    let cand_legacy = parse_trace(&dir, "cand")
        .iter()
        .find(|l| l.fields["step"] == "spawned")
        .unwrap()
        .fields["pid"]
        .as_i64()
        .unwrap();
    wait_dead(cand_legacy, HARD);
    let st = state(&c).await; // serviced AFTER the revert (pump break serialises)
    assert_eq!(st["generation"].as_i64().unwrap(), 0, "revert never advances");
    assert_eq!(st["successor"].as_i64().unwrap(), 0);
    assert!(alive(legacy) && alive(managed), "old generation resumes with children intact");
    assert!(parse_trace(&dir, "commit").is_empty());
    broker.stop();
}

// ── acceptance 1: committed swap retires the old generation, then ────────
// launches exactly one successor; ownership stays generation-scoped ───────

#[tokio::test]
async fn committed_reload_retires_old_children_then_launches_one_successor() {
    let _g = lock().await;
    let dir = Dir::new("commit");
    let mut broker = Broker::start();
    let variant = Variant {
        legacy_cmd: NORMAL_LEGACY,
        candidate_sleep: 0.1,
        candidate_spawns_legacy: true,
        candidate_raises: false,
        candidate_heartbeat: false,
        hook_slow: false,
    };
    let (_node_conf, _citizen) = boot(&variant, &dir, &broker);
    let c = connect(&broker).await;
    wait_service(&c, HARD).await;

    let boot = parse_trace(&dir, "boot");
    let legacy = boot[0].fields["legacy"].as_i64().unwrap();
    let managed = boot[0].fields["managed"].as_i64().unwrap();

    // Commit 1.
    let ok = call(&c, "RELOAD", json!({})).await.expect("rc:0");
    assert_eq!(ok["reloading"], true);
    let commits = wait_lines(&dir, "commit", 1, HARD);
    assert_eq!(commits.len(), 1, "exactly one launch per commit");
    let c0 = &commits[0];
    assert_eq!(c0.fields["gen"].as_i64().unwrap(), 1);
    assert_eq!(
        c0.fields["old_managed"], false,
        "the hook must observe the old managed child already reaped — no overlap"
    );
    assert_eq!(
        c0.fields["old_legacy"], false,
        "the old legacy child must be retired BEFORE the hook runs"
    );
    assert_eq!(c0.fields["cand_legacy_alive"], true, "candidate-owned legacy survives");
    let successor1 = c0.fields["successor"].as_i64().unwrap();
    assert_ne!(successor1, 0);
    assert!(alive(successor1));
    wait_dead(legacy, HARD);
    wait_dead(managed, HARD);
    assert_eq!(props_generation(&c).await, 1, "advanced by exactly one");

    // Commit 2 retires generation 1's children (successor1 + its
    // candidate-owned legacy) while launching successor2.
    let cand1 = parse_trace(&dir, "cand")
        .iter()
        .rev()
        .find(|l| l.fields["step"] == "spawned")
        .unwrap()
        .fields["pid"]
        .as_i64()
        .unwrap();
    let ok = call(&c, "RELOAD", json!({})).await.expect("rc:0");
    assert_eq!(ok["reloading"], true);
    let commits = wait_lines(&dir, "commit", 2, HARD);
    assert_eq!(commits.len(), 2);
    let c1 = &commits[1];
    assert_eq!(c1.fields["gen"].as_i64().unwrap(), 2);
    let successor2 = c1.fields["successor"].as_i64().unwrap();
    assert_ne!(successor2, successor1);
    assert!(alive(successor2));
    wait_dead(successor1, HARD);
    wait_dead(cand1, HARD);
    assert_eq!(props_generation(&c).await, 2, "one advance per live swap");
    broker.stop();
}

// ── acceptance 2: wire-spoofed lifecycle.commit is refused, no side ──────
// effects, and HELP never advertises the native-only hook ─────────────────

#[tokio::test]
async fn wire_spoofed_lifecycle_commit_is_refused_without_side_effects() {
    let _g = lock().await;
    let dir = Dir::new("spoof");
    let mut broker = Broker::start();
    let variant = Variant {
        legacy_cmd: NORMAL_LEGACY,
        candidate_sleep: 0.1,
        candidate_spawns_legacy: false,
        candidate_raises: false,
        candidate_heartbeat: false,
        hook_slow: false,
    };
    let (_node_conf, _citizen) = boot(&variant, &dir, &broker);
    let c = connect(&broker).await;
    wait_service(&c, HARD).await;

    let err = call(&c, "lifecycle.commit", json!({"generation": 0}))
        .await
        .expect_err("wire copies must answer rc:10");
    assert!(err.contains("not a callable verb"), "{err}");
    assert!(parse_trace(&dir, "commit").is_empty(), "no commit hook ran");
    let st = state(&c).await;
    assert_eq!(st["generation"].as_i64().unwrap(), 0);
    assert_eq!(st["successor"].as_i64().unwrap(), 0);
    assert_eq!(trace(&dir).lines().count(), 1, "no side-effect extra spawn: only boot logged");
    let help = call(&c, "HELP", json!({})).await.expect("HELP");
    let names: Vec<&str> = help
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["name"].as_str())
        .collect();
    assert!(names.contains(&"rel.state"));
    assert!(!names.contains(&"lifecycle.commit"), "{names:?}");
    broker.stop();
}

// ── acceptance 2: broker cut during candidate prep — the local commit ────
// still dispatches, and the citizen reconciles when the broker returns ────

#[tokio::test]
async fn broker_cut_during_candidate_prep_commit_dispatches_and_reconciles() {
    let _g = lock().await;
    let dir = Dir::new("cut");
    let port = reserve_port();
    let mut broker = Broker::with_tcp_port(port);
    let variant = Variant {
        legacy_cmd: NORMAL_LEGACY,
        candidate_sleep: 0.6,
        candidate_spawns_legacy: true,
        candidate_raises: false,
        candidate_heartbeat: false,
        hook_slow: false,
    };
    let (_node_conf, mut citizen) = boot(&variant, &dir, &broker);
    let c = connect(&broker).await;
    wait_service(&c, HARD).await;
    let boot = parse_trace(&dir, "boot");
    let legacy = boot[0].fields["legacy"].as_i64().unwrap();
    let managed = boot[0].fields["managed"].as_i64().unwrap();

    let ok = call(&c, "RELOAD", json!({})).await.expect("rc:0");
    assert_eq!(ok["reloading"], true);
    // The rc:0 reply precedes the swap, so the candidate is in its prep
    // sleep when this barrier is observed — cut the broker then.
    wait_trace(&dir, "cand|{\"step\":\"start\"}", HARD);
    broker.stop();

    // The commit hook is a LOCAL queue injection: it must dispatch with the
    // broker absent.
    let commits = wait_lines(&dir, "commit", 1, HARD);
    assert_eq!(commits[0].fields["gen"].as_i64().unwrap(), 1);
    assert_eq!(commits[0].fields["old_managed"], false);
    assert_eq!(commits[0].fields["old_legacy"], false);
    let successor = commits[0].fields["successor"].as_i64().unwrap();
    assert!(alive(successor), "no stranded or missing child");
    assert!(
        citizen.child.try_wait().unwrap().is_none(),
        "the supervised citizen must survive a transient broker cut"
    );
    wait_dead(legacy, HARD);
    wait_dead(managed, HARD);

    // Reconcile: broker back at the SAME address (bounce-stable port) —
    // the supervisor reconnects and re-registers on the native path.
    broker.bounce();
    let c2 = connect(&broker).await;
    wait_service(&c2, RECONCILE).await;
    let st = state(&c2).await;
    assert_eq!(st["generation"].as_i64().unwrap(), 1, "durable commit survived the cut");
    assert_eq!(st["successor"].as_i64().unwrap(), successor);
    assert!(alive(successor));
    let cand_legacy = parse_trace(&dir, "cand")
        .iter()
        .find(|l| l.fields["step"] == "spawned")
        .unwrap()
        .fields["pid"]
        .as_i64()
        .unwrap();
    assert!(alive(cand_legacy), "candidate-owned child survives the commit");
    broker.stop();
}

// ── acceptance 2/3: SIGTERM from candidate prep and from inside the ──────
// dispatching commit hook shuts down the whole child group ────────────────

#[tokio::test]
async fn sigterm_shuts_down_the_child_group_from_prep_and_from_the_dispatching_hook() {
    let _g = lock().await;
    let dir = Dir::new("sigterm");
    let mut broker = Broker::start();

    // Phase A — candidate passive prep waiting (long prep sleep).
    let prep = Variant {
        legacy_cmd: NORMAL_LEGACY,
        candidate_sleep: 3.0,
        candidate_spawns_legacy: false,
        candidate_raises: false,
        candidate_heartbeat: false,
        hook_slow: false,
    };
    let (_node_conf, mut citizen_a) = boot(&prep, &dir, &broker);
    let c = connect(&broker).await;
    wait_service(&c, HARD).await;
    let boot_a = parse_trace(&dir, "boot");
    let legacy_a = boot_a[0].fields["legacy"].as_i64().unwrap();
    let managed_a = boot_a[0].fields["managed"].as_i64().unwrap();
    assert!(call(&c, "RELOAD", json!({})).await.is_ok());
    wait_trace(&dir, "cand|{\"step\":\"start\"}", HARD);
    citizen_a.sigterm();
    let status = citizen_a.wait_exit(HARD);
    assert_eq!(status.code(), Some(0), "clean deregister-before-exit shutdown");
    assert!(parse_trace(&dir, "commit").is_empty(), "no commit, no successor");
    wait_dead(legacy_a, HARD);
    wait_dead(managed_a, HARD);

    // Phase B — SIGTERM lands while the queued commit hook is dispatching
    // its cleanup (successor already spawned, hook busy).
    let slow = Variant {
        legacy_cmd: NORMAL_LEGACY,
        candidate_sleep: 0.1,
        candidate_spawns_legacy: false,
        candidate_raises: false,
        candidate_heartbeat: false,
        hook_slow: true,
    };
    dir.write("svc-b.mix", &script(&slow));
    let node_conf_b = dir.write("node-b.conf.mix", &node_conf_text(broker_tcp_port(&broker)));
    let script_b = dir.0.join("svc-b.mix");
    let mut citizen_b =
        Citizen::spawn(Path::new(env!("CARGO_BIN_EXE_mix")), &dir, &node_conf_b, &script_b);
    let cb = connect(&broker).await;
    wait_service(&cb, HARD).await;
    let boot_b = parse_trace(&dir, "boot");
    assert_eq!(boot_b.len(), 2, "the second citizen appends to the same trace");
    let legacy_b = boot_b[1].fields["legacy"].as_i64().unwrap();
    let managed_b = boot_b[1].fields["managed"].as_i64().unwrap();
    assert!(call(&cb, "RELOAD", json!({})).await.is_ok());
    wait_trace(&dir, "commit|", HARD);
    wait_trace(&dir, "hookstep|{\"step\":\"start\"}", HARD);
    let successor_b = parse_trace(&dir, "commit")[0].fields["successor"].as_i64().unwrap();
    assert!(alive(successor_b), "hook spawned its successor before the signal");
    citizen_b.sigterm();
    let status_b = citizen_b.wait_exit(HARD);
    assert_eq!(status_b.code(), Some(0));
    wait_dead(successor_b, HARD);
    wait_dead(legacy_b, HARD);
    wait_dead(managed_b, HARD);
    assert!(parse_trace(&dir, "hookstep").len() <= 2, "hook did not continue past shutdown");
    broker.stop();
}

// ── acceptance 3: offloaded grace keeps the reactor responsive while ─────
// sweeping a TERM-ignoring child (not only an empty registry) ─────────────

#[tokio::test]
async fn offloaded_grace_keeps_reactor_responsive_while_sweeping_a_term_ignoring_child() {
    let _g = lock().await;
    let dir = Dir::new("grace");
    let mut broker = Broker::start();
    let variant = Variant {
        legacy_cmd: TERM_IGNORING_LEGACY,
        candidate_sleep: 0.2,
        candidate_spawns_legacy: false,
        candidate_raises: false,
        candidate_heartbeat: true,
        hook_slow: false,
    };
    let (_node_conf, _citizen) = boot(&variant, &dir, &broker);
    let c = connect(&broker).await;
    wait_service(&c, HARD).await;
    wait_trace(&dir, "legacy-ready|", HARD);
    let boot = parse_trace(&dir, "boot");
    let legacy = boot[0].fields["legacy"].as_i64().unwrap();
    let managed = boot[0].fields["managed"].as_i64().unwrap();

    assert!(call(&c, "RELOAD", json!({})).await.is_ok());
    let commits = wait_lines(&dir, "commit", 1, HARD);
    let c0 = &commits[0];
    assert_eq!(c0.fields["gen"].as_i64().unwrap(), 1);
    assert_eq!(c0.fields["old_legacy"], false, "TERM-ignoring child was SIGKILLed after grace");
    let successor = c0.fields["successor"].as_i64().unwrap();
    assert!(alive(successor));
    wait_dead(legacy, HARD);
    wait_dead(managed, HARD);

    // The sweep really waited the ~2 s grace (a TERM-ignoring group keeps
    // the registry non-empty), not an instant empty-registry retire.
    let cand_done = parse_trace(&dir, "cand")
        .iter()
        .find(|l| l.fields["step"] == "done")
        .unwrap()
        .t;
    let waited = c0.t - cand_done;
    assert!(waited >= 1.5, "sweep did not wait the grace: {waited:.2}s");

    // The reactor stayed live DURING that grace: the candidate's heartbeat
    // task kept appending. An inline (runtime-blocking) sweep would yield
    // zero stamps between candidate-done and commit.
    let hbs: Vec<f64> = trace(&dir)
        .lines()
        .filter_map(|l| l.strip_prefix("hb|").and_then(|t| t.parse::<f64>().ok()))
        .collect();
    let during = hbs.iter().filter(|t| **t > cand_done && **t < c0.t).count();
    assert!(
        during >= 5,
        "only {during} heartbeats during the {waited:.2}s sweep window — the reactor was blocked"
    );
    broker.stop();
}
