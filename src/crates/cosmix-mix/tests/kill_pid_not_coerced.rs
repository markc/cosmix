//! `kill(false)` must raise, tested OUT OF PROCESS (0.52.0).
//!
//! This case lives here rather than beside the other kill tests in
//! cosmix-lib-mix because of what it does when the fix is *absent*. Before
//! 0.52.0, `to_number(false)` was `0.0`, so `kill(false)` became
//! `kill(0, SIGTERM)` — a signal to every process in the caller's process
//! group. Running that in-process means the test runner signals itself: during
//! development the mutation check (revert the fix, confirm the test fails) took
//! the whole harness down with exit 144, killed by its own SIGTERM, instead of
//! reporting a failed assertion.
//!
//! A child process absorbs that blast. The assertion is identical; only the
//! blast radius changes, so a future reverter gets a red test rather than a
//! dead runner.

#![cfg(unix)]

use std::process::Command;

fn mix(expr: &str) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_mix"))
        .arg("-c")
        .arg(expr)
        .env("MIX_STATS", "off")
        .output()
        .expect("mix binary must run");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn bool_pid_raises_instead_of_signalling_the_whole_process_group() {
    for literal in ["false", "true"] {
        let (ok, stderr) = mix(&format!("kill({literal})"));
        assert!(!ok, "kill({literal}) must fail, stderr={stderr}");
        // The A2 contract gate (0.103.1) raises before kill's own check:
        // TYPE_MISMATCH, naming the contract position and shape. The
        // blast-radius explanation lives in the kill registry/manual
        // text ("a bool/string pid would signal this process's whole
        // group") — see TODO-mix A2 residual.
        assert!(
            stderr.contains("argument 1 (pid) must be number"),
            "kill({literal}) stderr={stderr}"
        );
    }
}

#[test]
fn process_alive_bool_pid_raises_rather_than_reaping_a_child() {
    // Same blast-radius reasoning: under the old code this reached
    // waitpid(0, WNOHANG), which reaps an arbitrary child of the runner.
    let (ok, stderr) = mix("process_alive(false)");
    assert!(!ok, "process_alive(false) must fail, stderr={stderr}");
    assert!(
        stderr.contains("argument 1 (pid) must be number"),
        "stderr={stderr}"
    );
}
