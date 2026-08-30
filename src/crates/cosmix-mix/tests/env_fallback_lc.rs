//! Phase A lowercase-head env-fallback contract — out-of-process variant.
//!
//! This test lives in `cosmix-mix` (not `cosmix-lib-mix`) because pinning
//! the "all heads, not just uppercase" rule as an executable contract
//! requires actually exporting an env var the evaluator can read. Doing
//! that in-process with `std::env::set_var` is unsound under Rust 2024 +
//! tokio's parallel test runner: concurrent in-process `getenv` from
//! other tests (tilde expansion, `env()` builtin, etc.) races with the
//! `setenv` mutation. Codex flagged this as a MAJOR issue on the first
//! Phase A review (thread 019e691a-…) and recommended subprocess
//! isolation.
//!
//! We invoke the freshly-built `mix` binary via `env!("CARGO_BIN_EXE_mix")`
//! with `Command::env(…)`. The child process sees the probe env var from
//! the start; no in-process env mutation occurs in the parent.

use std::process::Command;

#[test]
fn interp_brace_env_fallback_lowercase_head() {
    let mix_bin = env!("CARGO_BIN_EXE_mix");
    let script = format!(
        "{}/tests/scripts/interp_brace_env_fallback_lc.mix",
        env!("CARGO_MANIFEST_DIR")
    );

    let output = Command::new(mix_bin)
        .arg(&script)
        .env("MIX_STATS", "off")
        .env("mix_interp_lowercase_probe", "lowercase_ok")
        .output()
        .expect("failed to spawn mix binary");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "mix exited non-zero ({:?})\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        stdout,
        stderr
    );

    let actual = stdout.trim();
    let expected = "lowercase env-fallback: true";
    assert_eq!(
        actual, expected,
        "lowercase env-fallback contract failed\nexpected: {}\nactual:   {}\nstderr:\n{}",
        expected, actual, stderr
    );
}
