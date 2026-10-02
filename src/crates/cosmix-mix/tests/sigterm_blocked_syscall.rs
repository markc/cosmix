//! SIGTERM must end a non-interactive mix even when evaluation is stuck in a
//! synchronous syscall. tokio's handler replaces the default disposition and
//! only wakes a `select!` arm, which a builtin blocked on the single runtime
//! thread never lets run: `append_file` to a FIFO with no reader survived
//! SIGTERM for a week (2026-10-02). The backstop thread must force the exit.

#![cfg(target_os = "linux")]

use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn sigterm_ends_a_script_blocked_opening_a_fifo() {
    let dir = std::env::temp_dir().join(format!("sigterm-blocked-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fifo = dir.join("f");
    let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0, "mkfifo");
    let script = dir.join("t.mix");
    std::fs::write(
        &script,
        format!("append_file(\"{}\", \"hi\\n\")\nprint(\"unreachable\")\n", fifo.display()),
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_mix"))
        .arg(&script)
        .env("MIX_SIGTERM_BACKSTOP_SECS", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mix");

    // Let it reach the blocking open.
    std::thread::sleep(Duration::from_millis(800));
    assert!(child.try_wait().unwrap().is_none(), "script should be blocked on the FIFO");
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };

    let started = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if started.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            let _ = std::fs::remove_dir_all(&dir);
            panic!("mix ignored SIGTERM while blocked in a syscall");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let out = child.wait_with_output().unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(status.code(), Some(128 + libc::SIGTERM), "status {status:?} signal {:?}", status.signal());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("SIGTERM not honoured"), "stderr: {err:?}");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("unreachable"));
}
