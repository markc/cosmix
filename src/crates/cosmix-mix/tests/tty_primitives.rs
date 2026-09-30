//! 2026-09-26 entry: the keystroke-recorder primitives — `tty_mode` and
//! the incremental `stdin_copy`.

use std::io::Write as _;
use std::process::{Command, Stdio};

fn mix_bin() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_mix"));
    c.env("MIX_STATS", "off");
    c
}

#[test]
fn stdin_copy_writes_incrementally_to_a_file() {
    let dir = std::env::temp_dir().join(format!("mix-stdin-copy-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("tmp dir");
    let out_path = dir.join("captured");
    let mut child = mix_bin()
        .args(["-c", &format!("stdin_copy(\"{}\")", out_path.display())])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mix");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"hello keystrokes")
        .expect("feed stdin");
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "stdin_copy must succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let got = std::fs::read(&out_path).expect("read captured file");
    assert_eq!(got, b"hello keystrokes");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn tty_mode_raises_on_a_non_tty_and_unknown_mode() {
    // stdin is a pipe (not a terminal) in this test — raw mode must be
    // refused, and an unknown mode named.
    let out = mix_bin()
        .args(["-c", "tty_mode(\"raw\")"])
        .stdin(Stdio::piped())
        .output()
        .expect("run mix");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(stderr.contains("not a terminal"), "got: {stderr}");

    let out = mix_bin()
        .args(["-c", "tty_mode(\"bogus\")"])
        .stdin(Stdio::piped())
        .output()
        .expect("run mix");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(stderr.contains("unknown mode"), "got: {stderr}");
}
