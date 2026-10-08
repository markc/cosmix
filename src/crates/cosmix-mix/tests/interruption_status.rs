//! Error prose never grants interruption status; real signals retain CLI exits.
#![cfg(unix)]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn command(home: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mix"));
    cmd.env("HOME", home)
        .env("MIX_STATS", "off")
        .env("MIX_SIGTERM_BACKSTOP_SECS", "1")
        .env_remove("COSMIX_SESSION_FD")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

#[test]
fn error_messages_and_codes_cannot_impersonate_a_signal() {
    let dir = tempfile::tempdir().unwrap();
    for (body, diagnostic) in [
        (
            r#"raise("PROBE_REFUSAL", json_encode({interrupted:false,error:"not a signal"}))"#,
            "PROBE_REFUSAL",
        ),
        (
            r#"die("ordinary interrupted operation")"#,
            "ordinary interrupted operation",
        ),
        (r#"run_argv_must(["false"])"#, "PROCESS_EXIT_NONZERO"),
        (r#"raise("SIGNAL_INTERRUPT", "2")"#, "SIGNAL_INTERRUPT"),
    ] {
        for mode in ["command", "file", "function"] {
            let mut cmd = command(dir.path());
            match mode {
                "command" => {
                    cmd.args(["-c", body]);
                }
                "file" => {
                    let script = dir.path().join("probe.mix");
                    std::fs::write(&script, body).unwrap();
                    cmd.arg(script);
                }
                _ => {
                    std::fs::write(
                        dir.path().join(".mixrc"),
                        format!("fn probe_fail()\n{body}\nend\n"),
                    )
                    .unwrap();
                    cmd.args(["-i", "-c", "probe_fail"]);
                }
            }
            let out = cmd.output().unwrap();
            let err = String::from_utf8_lossy(&out.stderr);
            assert_eq!(out.status.code(), Some(1), "{mode}: {err}");
            assert!(
                err.contains(diagnostic),
                "{mode}: missing {diagnostic}: {err}"
            );
        }
    }
}

struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn real_signals_exit_nonzero_for_waiting_and_cooperative_evaluations() {
    for mode in ["command", "file", "function"] {
        for work in ["sleep(30)", "$n=0\nwhile true\n$n=$n+1\nend"] {
            for signal in [libc::SIGINT, libc::SIGTERM] {
                let dir = tempfile::tempdir().unwrap();
                let marker = dir.path().join("ready");
                let body = format!(
                    "write_file({}, \"ready\")\n{work}\nprint(\"UNREACHABLE\")\n",
                    serde_json::to_string(marker.to_str().unwrap()).unwrap()
                );
                let mut cmd = command(dir.path());
                match mode {
                    "command" => {
                        cmd.args(["-c", &body]);
                    }
                    "file" => {
                        let script = dir.path().join("signal.mix");
                        std::fs::write(&script, &body).unwrap();
                        cmd.arg(script);
                    }
                    _ => {
                        std::fs::write(
                            dir.path().join(".mixrc"),
                            format!("fn probe_run()\n{body}\nend\n"),
                        )
                        .unwrap();
                        cmd.args(["-i", "-c", "probe_run"]);
                    }
                }
                let mut child = OwnedChild(cmd.spawn().unwrap());
                let ready_deadline = Instant::now() + Duration::from_secs(5);
                while !marker.is_file() {
                    assert!(
                        child.0.try_wait().unwrap().is_none(),
                        "{mode} exited before ready"
                    );
                    assert!(
                        Instant::now() < ready_deadline,
                        "{mode} did not reach evaluation"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert!(child.0.try_wait().unwrap().is_none());
                assert_eq!(unsafe { libc::kill(child.0.id() as i32, signal) }, 0);
                let deadline = Instant::now() + Duration::from_secs(20);
                let status = loop {
                    if let Some(status) = child.0.try_wait().unwrap() {
                        break status;
                    }
                    assert!(Instant::now() < deadline, "{mode} ignored signal {signal}");
                    std::thread::sleep(Duration::from_millis(10));
                };
                assert_eq!(
                    status.code(),
                    Some(128 + signal),
                    "{mode} {work} signal {signal}: {status}"
                );
            }
        }
    }
}
