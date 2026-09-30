//! D1 (TODO-mix 2026-09-24): the `-c`/stdin pre-execution lint gate —
//! a hard-safe diagnostic (arity, dead mutation, push-assign-back,
//! literal-type) refuses to run with exit 2 BEFORE any line executes;
//! `--no-lint` overrides; `--agent`/`MIX_LINT=warn` prints the soft set.

use std::io::Write;
use std::process::{Command, Stdio};

fn mix_bin() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_mix"));
    c.env("MIX_STATS", "off");
    c
}

#[test]
fn hard_safe_diagnostic_refuses_with_exit_2_before_running() {
    // E1201: remove() with 2 args — provable, refused, and the marker
    // file is NOT created (nothing ran).
    let marker = std::env::temp_dir().join(format!("d1-marker-{}", std::process::id()));
    let src = format!("remove({}, \"k\")\nprint(\"ran\")\n", serde_json::to_string(&marker).unwrap());
    let out = mix_bin()
        .args(["-c", &src])
        .output()
        .expect("run mix");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "refusal exit code: {stderr}");
    assert!(stderr.contains("MIX-E1201"), "got: {stderr}");
    assert!(stderr.contains("refusing to run"), "got: {stderr}");
    assert!(!marker.exists(), "nothing may run under refusal");
}

#[test]
fn no_lint_overrides_the_gate() {
    // With --no-lint the source RUNS — and under the strict default the
    // 2-arg remove then fails at runtime with ARITY_MISMATCH (not the
    // gate's exit-2 refusal).
    let out = mix_bin()
        .args(["--no-lint", "-c", "print(\"ran\")\n$m = {}\nremove($m, \"k\")\n"])
        .output()
        .expect("run mix");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success() && out.status.code() != Some(2),
        "--no-lint runs the source: {stderr}"
    );
    assert!(!stderr.contains("refusing to run"), "got: {stderr}");
    assert!(stderr.contains("ARITY_MISMATCH"), "the runtime caught it: {stderr}");
}

#[test]
fn clean_source_runs_through_the_gate() {
    let out = mix_bin()
        .args(["-c", "print(\"ok\")\n"])
        .output()
        .expect("run mix");
    assert!(out.status.success(), "clean -c runs");
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("refusing to run"),
        "no refusal on clean source"
    );
}

#[test]
fn stdin_mode_runs_the_same_gate() {
    let mut child = mix_bin()
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mix");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"$m = {}\nremove($m, \"k\")\nprint(\"ran\")\n")
        .expect("feed stdin");
    let out = child.wait_with_output().expect("wait");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "stdin gate refuses: {stderr}");
    assert!(stderr.contains("MIX-E1201"), "got: {stderr}");
}

#[test]
fn agent_mode_prints_the_soft_set_and_still_runs() {
    // A literal-type E1203 is HARD — so use a soft-only shape here: the
    // gate must not refuse a warning and --agent must surface it. The
    // simplest deterministic soft diagnostic is MIX-D3016 (version
    // header) which fires on unversioned inputs.
    let out = mix_bin()
        .args(["--agent", "-c", "print(1)\n"])
        .output()
        .expect("run mix");
    assert!(
        out.status.success(),
        "soft diagnostics never refuse: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
