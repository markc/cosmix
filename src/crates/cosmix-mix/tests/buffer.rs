//! Executable contract for the reference-semantic `Value::Buffer` type
//! (mix 0.26.0). Runs `tests/scripts/buffer.mix` — which exits non-zero on
//! the first failed check — and asserts a clean exit plus the final marker.
//!
//! Buffer reference semantics (`$b = $a` shares one backing store) require
//! the evaluator + scope, so an out-of-process Mix-source run is the right
//! test level (a lib-only unit test can't exercise assignment aliasing).

use std::process::Command;

#[test]
fn buffer_reference_semantics_contract() {
    let mix_bin = env!("CARGO_BIN_EXE_mix");
    let script = format!("{}/tests/scripts/buffer.mix", env!("CARGO_MANIFEST_DIR"));

    let output = Command::new(mix_bin)
        .arg(&script)
        .env("MIX_STATS", "off")
        .output()
        .expect("failed to spawn mix binary");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "buffer.mix exited non-zero.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("ALL BUFFER CHECKS PASSED"),
        "buffer.mix did not reach the PASS marker.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
