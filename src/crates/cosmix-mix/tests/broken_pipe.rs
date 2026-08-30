//! The shell's own output must obey normal Unix pipeline semantics.

#[cfg(unix)]
#[test]
fn builtins_json_closed_pipe_never_panics() {
    use std::io::Read;
    use std::process::{Command, Stdio};

    let mut child = Command::new(env!("CARGO_BIN_EXE_mix"))
        .args(["builtins", "--json"])
        .env("MIX_STATS", "off")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mix builtins --json");

    // Let output begin, then close the read end while the large JSON report is
    // still being written. This is the direct equivalent of `| head` exiting.
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut first_byte = [0u8; 1];
    stdout.read_exact(&mut first_byte).expect("read first byte");
    drop(stdout);

    let output = child.wait_with_output().expect("wait for mix");
    assert!(
        output.status.success(),
        "expected a quiet successful exit, status={:?}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("panicked"), "stderr: {stderr}");
    assert!(!stderr.contains("Broken pipe"), "stderr: {stderr}");
}
