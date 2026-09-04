//! A blocking `http_serve` must stop on SIGINT (Ctrl-C). Driven as a
//! subprocess because the interrupt flag is process-global — arming it
//! inside a unit-test binary would poison every parallel interrupt-aware
//! test. Here the real `mix` binary runs a server with no `duration` and
//! no `requests` cap, so ONLY a signal can end it; we send SIGINT and
//! require a prompt, clean exit.

#[cfg(unix)]
#[test]
fn http_serve_stops_on_sigint() {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let dir = std::env::temp_dir().join(format!("mix-http-sig-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // Port 0 → ephemeral; the script prints its own URL. No duration/
    // requests: unstoppable except by the signal under test.
    let script = format!(
        "print(http_serve(\"{}\", {{port: 0}}))\n",
        dir.to_string_lossy()
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_mix"))
        .args(["-c", &script])
        .env("MIX_STATS", "off")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mix http_serve");

    // Give it time to bind and enter the accept loop, then SIGINT.
    std::thread::sleep(Duration::from_millis(600));
    let pid = child.id() as i32;
    assert_eq!(unsafe { libc::kill(pid, libc::SIGINT) }, 0, "kill(SIGINT)");

    // It must exit within a couple of seconds — a spin loop that ignored
    // the flag would run forever and this poll would time out.
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        match child.try_wait().expect("try_wait") {
            Some(_status) => break, // exited — the guarantee under test
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                panic!("http_serve ignored SIGINT and did not exit");
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    std::fs::remove_dir_all(&dir).ok();
}
