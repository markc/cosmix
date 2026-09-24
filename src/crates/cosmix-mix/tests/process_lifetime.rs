//! TODO-mix P2 out of process: a `spawn(argv, {die_with_parent: true})`
//! child must end with the `mix` process that started it — gracefully (SIGTERM
//! to its whole group first) when mix exits normally, and by the kernel's
//! PDEATHSIG when mix is killed outright. Only a real mix process can exit or
//! be SIGKILLed, so this lives here rather than in the in-process suite.
//!
//! The regression it pins (goose ACP spike, 2026-09-23): a `mix --serve`
//! citizen that spawned a helper server left it running after QUIT/SIGTERM,
//! and every restart orphaned one more server holding its port.

#![cfg(target_os = "linux")]

use std::io::Write;
use std::os::fd::FromRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn scratch_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "mix-process-lifetime-{label}-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn pid_is_gone(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit_once(')')
            .map(|(_, rest)| rest.trim_start().starts_with('Z'))
            .unwrap_or(false),
    }
}

fn wait_gone(pid: u32, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    loop {
        if pid_is_gone(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn read_pid(path: &Path, within: Duration) -> u32 {
    let deadline = Instant::now() + within;
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && let Ok(pid) = text.trim().parse()
        {
            return pid;
        }
        assert!(
            Instant::now() < deadline,
            "{} was never written",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn kill_leftover(pid: u32) {
    // SAFETY: kill(2) on a pid this test's tree created; stale is ESRCH.
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

/// The helper: a shell LEADER that records its pid, traps SIGTERM to leave a
/// marker (so the test can tell a graceful TERM from a PDEATHSIG KILL), and a
/// DESCENDANT `sleep` in the same group.
fn helper_argv(dir: &Path) -> String {
    let d = dir.display();
    format!(
        "[\"sh\", \"-c\", \"trap 'echo term > {d}/term; exit 0' TERM; echo $$ > {d}/leader; sleep 60 & echo $! > {d}/desc; wait\"]"
    )
}

fn write_script(dir: &Path, source: &str) -> PathBuf {
    let path = dir.join("script.mix");
    let mut file = std::fs::File::create(&path).expect("write script");
    file.write_all(source.as_bytes()).expect("write script");
    path
}

/// Graceful exit: the script spawns the helper, waits until both pids exist,
/// and ends. Mix's exit sweep must SIGTERM the helper's group (the trap leaves
/// its marker) and leave neither the leader nor the descendant running.
#[test]
fn owned_spawn_group_is_terminated_when_mix_exits_normally() {
    let dir = scratch_dir("graceful");
    let d = dir.display().to_string();
    let source = format!(
        "$pid = spawn({}, {{die_with_parent: true}})\n\
         $n = 0\n\
         while !exists(\"{d}/desc\") and $n < 250 do\n  sleep(0.02)\n  $n = $n + 1\nend\n\
         print($pid)\n",
        helper_argv(&dir)
    );
    let script = write_script(&dir, &source);
    let status = Command::new(env!("CARGO_BIN_EXE_mix"))
        .arg(&script)
        .env("MIX_STATS", "off")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .expect("mix binary must run");
    assert!(status.success(), "script failed: {status:?}");

    let leader = read_pid(&dir.join("leader"), Duration::from_secs(1));
    let desc = read_pid(&dir.join("desc"), Duration::from_secs(1));
    let leader_gone = wait_gone(leader, Duration::from_secs(5));
    let desc_gone = wait_gone(desc, Duration::from_secs(5));
    if !leader_gone {
        kill_leftover(leader);
    }
    if !desc_gone {
        kill_leftover(desc);
    }
    let termed = dir.join("term").exists();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(leader_gone, "owned leader {leader} outlived mix");
    assert!(desc_gone, "owned descendant {desc} outlived mix");
    assert!(termed, "the leader got no SIGTERM: the graceful sweep did not run first");
}

/// Crash: mix is SIGKILLed, so no sweep can run. PR_SET_PDEATHSIG must still
/// take the leader down. (Its descendant is out of PDEATHSIG's reach — the
/// documented limit — and is cleaned up by the test, not asserted.)
#[test]
fn owned_spawn_leader_dies_when_mix_is_killed() {
    let dir = scratch_dir("crash");
    let d = dir.display().to_string();
    let source = format!(
        "spawn({}, {{die_with_parent: true}})\n\
         $n = 0\n\
         while !exists(\"{d}/desc\") and $n < 250 do\n  sleep(0.02)\n  $n = $n + 1\nend\n\
         print(\"ready\")\n\
         sleep(60)\n",
        helper_argv(&dir)
    );
    let script = write_script(&dir, &source);
    let mut child = Command::new(env!("CARGO_BIN_EXE_mix"))
        .arg(&script)
        .env("MIX_STATS", "off")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("mix binary must run");
    let leader = read_pid(&dir.join("leader"), Duration::from_secs(5));
    let desc = read_pid(&dir.join("desc"), Duration::from_secs(5));
    child.kill().expect("SIGKILL mix");
    let _ = child.wait();
    let leader_gone = wait_gone(leader, Duration::from_secs(5));
    if !leader_gone {
        kill_leftover(leader);
    }
    kill_leftover(desc);
    let termed = dir.join("term").exists();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(leader_gone, "PDEATHSIG did not end owned leader {leader} after mix was killed");
    assert!(!termed, "a SIGKILLed mix cannot have run the graceful sweep");
}

/// Without the option a spawned child is untouched by mix's exit — the
/// default spawn contract ("fire-and-forget, owns nothing") is unchanged.
#[test]
fn plain_spawn_child_survives_mix_exit() {
    let dir = scratch_dir("plain");
    let d = dir.display().to_string();
    let source = format!(
        "spawn([\"sh\", \"-c\", \"echo $$ > {d}/leader; exec sleep 60\"])\n\
         $n = 0\n\
         while !exists(\"{d}/leader\") and $n < 250 do\n  sleep(0.02)\n  $n = $n + 1\nend\n"
    );
    let script = write_script(&dir, &source);
    let status = Command::new(env!("CARGO_BIN_EXE_mix"))
        .arg(&script)
        .env("MIX_STATS", "off")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .expect("mix binary must run");
    assert!(status.success(), "script failed: {status:?}");
    let leader = read_pid(&dir.join("leader"), Duration::from_secs(1));
    std::thread::sleep(Duration::from_millis(300));
    let alive = !pid_is_gone(leader);
    kill_leftover(leader);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(alive, "a plain spawn child must not be ended by mix's exit");
}

fn run_with_stdin(label: &str, source: &str, stdin: Stdio, feed: Option<&[u8]>) -> (bool, String) {
    let dir = scratch_dir(label);
    let script = write_script(&dir, source);
    let mut child = Command::new(env!("CARGO_BIN_EXE_mix"))
        .arg(&script)
        .env("MIX_STATS", "off")
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("mix binary must run");
    if let Some(bytes) = feed {
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(bytes)
            .expect("feed stdin");
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().expect("try_wait").is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_dir_all(&dir);
            panic!("`{label}` wedged — a child blocked on inherited stdin?");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = child.wait_with_output().expect("collect output");
    let _ = std::fs::remove_dir_all(&dir);
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// stdin {inherit: true} with a PIPE as mix's stdin: the child reads it.
/// The default (nil) still gives the child nothing.
#[test]
fn stdin_inherit_reads_a_piped_stdin() {
    let (ok, out) = run_with_stdin(
        "stdin-pipe",
        "print(run_argv([\"cat\"], {stdin: {inherit: true}}).stdout)\n\
         print(\"[\" .. run_argv([\"cat\"]).stdout .. \"]\")\n",
        Stdio::piped(),
        Some(b"from-the-caller"),
    );
    assert!(ok, "script failed: {out}");
    assert_eq!(out, "from-the-caller\n[]\n");

    let (ok, out) = run_with_stdin(
        "stdin-pipe-pipeline",
        "print(run_pipeline([{argv: [\"cat\"], stdin: {inherit: true}}, [\"tr\", \"a-z\", \"A-Z\"]]).stdout)\n",
        Stdio::piped(),
        Some(b"piped"),
    );
    assert!(ok, "script failed: {out}");
    assert_eq!(out, "PIPED\n");
}

/// stdin {inherit: true} with a TERMINAL as mix's stdin is refused before
/// anything spawns: the child would lead a background process group and a
/// terminal read would stop it with SIGTTIN. The error names run_stream.
#[test]
fn stdin_inherit_refuses_a_terminal_stdin() {
    let mut master = -1;
    let mut slave = -1;
    // SAFETY: openpty writes two fresh descriptors into the locals.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(rc, 0, "openpty");
    // SAFETY: `slave` is a descriptor we own; Stdio takes ownership of it.
    let stdin = unsafe { Stdio::from(std::os::fd::OwnedFd::from_raw_fd(slave)) };
    let (ok, out) = run_with_stdin(
        "stdin-tty",
        "try\n  run_argv([\"cat\"], {stdin: {inherit: true}})\ncatch $m, $e\n  print($e.code .. \" \" .. contains($m, \"run_stream\"))\nend\n\
         try\n  run_pipeline([{argv: [\"cat\"], stdin: {inherit: true}}])\ncatch $m, $e\n  print($e.code)\nend\n\
         print(run_argv([\"true\"]).ok)\n",
        stdin,
        None,
    );
    // SAFETY: closing the master we opened.
    unsafe {
        libc::close(master);
    }
    assert!(ok, "script failed: {out}");
    assert_eq!(out, "STDIN_TERMINAL true\nSTDIN_TERMINAL\ntrue\n");
}
