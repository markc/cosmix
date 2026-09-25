//! `mix --serve` fatal outcomes must be visible on stderr. Mix installs no
//! tracing subscriber, so before this a syntax error, a missing script or a
//! refused registration exited 1 with no output anywhere. The cases here fail
//! before any broker connection, so the test is hermetic.

use std::process::Command;

fn serve(script: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_mix"))
        .args(["--serve", script, "--name", "serve-stderr-probe"])
        .output()
        .expect("spawn mix --serve")
}

#[test]
fn a_parse_error_is_reported_on_stderr() {
    let dir = std::env::temp_dir().join(format!("serve-stderr-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("bad.mix");
    std::fs::write(&script, "if then\n").unwrap();
    let out = serve(script.to_str().unwrap());
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("mix --serve serve-stderr-probe:") && err.contains("error"),
        "stderr must name the service and the failure, got: {err:?}"
    );
}

#[test]
fn a_missing_script_is_reported_on_stderr() {
    let out = serve("/nonexistent/serve-stderr-probe.mix");
    assert_ne!(out.status.code(), Some(0));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("serve-stderr-probe"),
        "stderr must name the service, got: {err:?}"
    );
}
