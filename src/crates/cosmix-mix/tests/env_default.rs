//! `env(name, default)` two-arg contract — out-of-process variant.
//!
//! Like env_fallback_lc.rs, this pins env-dependent behaviour as an executable
//! contract by exporting vars to a child `mix` process (`Command::env`) rather
//! than mutating the parent's environment — `std::env::set_var` is unsound under
//! Rust 2024 + tokio's parallel test runner (concurrent in-process getenv).
//!
//! Contract: env(name, default) returns `default` when the var is unset OR
//! set-but-empty; a non-empty value wins; the one-arg env(name) is unchanged
//! ("" for unset); the default is returned verbatim (a number stays numeric).

use std::process::Command;

#[test]
fn env_two_arg_default_contract() {
    let mix_bin = env!("CARGO_BIN_EXE_mix");
    let script = format!(
        "{}/tests/scripts/env_default.mix",
        env!("CARGO_MANIFEST_DIR")
    );

    let output = Command::new(mix_bin)
        .arg(&script)
        .env("MIX_STATS", "off")
        .env("MIX_ENV_SET", "real")
        .env("MIX_ENV_EMPTY", "")
        .env_remove("MIX_ENV_UNSET")
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

    let expected = "unset:fallback\nempty:fallback\nset:real\nonearg:[]\nnum:8080";
    assert_eq!(
        stdout.trim(),
        expected,
        "env(name, default) contract failed\nstderr:\n{}",
        stderr
    );
}
