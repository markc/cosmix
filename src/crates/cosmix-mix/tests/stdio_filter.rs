//! Mix as a byte-exact pipeline filter (v0.65.0) — `read_stdin_bytes` in,
//! `write_stdout` out, through the REAL binary's fd 0 and fd 1.
//!
//! The library-level tests (`cosmix-lib-mix/tests/raw_stdout.rs`) capture
//! output through a `SharedBuf`, which proves the value semantics but not
//! that the process's own descriptors carry the bytes unchanged. That is the
//! claim a mail filter depends on, so it is tested here against the built
//! binary with real pipes.

use std::io::Write;
use std::process::{Command, Stdio};

/// The input a filter has to survive: NUL, high bytes, a lone 0xFF/0xFE
/// pair, a TRUNCATED two-byte UTF-8 sequence (0xC3 0x28), an embedded
/// newline, and a final byte with no newline after it. `read_stdin`
/// refuses this input outright — that is the gap this release closes.
const BINARY_INPUT: &[u8] = &[
    0x00, 0x01, 0xFF, 0xFE, b' ', b'm', b'i', b'x', b' ', 0xC3, 0x28, 0x0A, 0x80, 0xEF, 0xBF, 0xBD,
    0x7F,
];

fn run_filter(script: &str, stdin_bytes: &[u8]) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mix"))
        .args(["-c", script])
        .env("MIX_STATS", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mix");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(stdin_bytes)
        .expect("write stdin");
    child.wait_with_output().expect("wait for mix")
}

#[test]
fn binary_stdin_round_trips_byte_for_byte() {
    let out = run_filter("write_stdout(read_stdin_bytes())", BINARY_INPUT);
    assert!(
        out.status.success(),
        "status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    // Byte-for-byte, not line-for-line: identical length AND content, so a
    // silently appended newline or a lossy re-encode both fail here.
    assert_eq!(
        out.stdout.len(),
        BINARY_INPUT.len(),
        "length changed: {} in, {} out — a trailing newline or a lossy \
         re-encode would show up exactly here",
        BINARY_INPUT.len(),
        out.stdout.len()
    );
    assert_eq!(out.stdout, BINARY_INPUT);
    assert!(out.stderr.is_empty(), "unexpected stderr: {:?}", out.stderr);
}

#[test]
fn read_stdin_still_refuses_the_same_input() {
    // The negative half of the pair: without this, the round-trip test above
    // could pass against a build where read_stdin had merely been made lossy
    // — which is the wrong fix, because a lossy decode changes the bytes a
    // digest is taken over.
    let out = run_filter("print(read_stdin())", BINARY_INPUT);
    assert!(!out.status.success(), "read_stdin must reject binary stdin");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("valid UTF-8"),
        "expected a UTF-8 error, got: {stderr}"
    );
}

#[test]
fn read_stdin_bytes_cap_truncates_like_read_file_bytes() {
    let out = run_filter("write_stdout(read_stdin_bytes(4))", BINARY_INPUT);
    assert!(out.status.success(), "status={:?}", out.status);
    assert_eq!(out.stdout, &BINARY_INPUT[..4]);
}

#[test]
fn explicit_nil_cap_means_no_cap() {
    // The one deliberate divergence from `read_file_bytes`, which RAISES on an
    // explicit nil: here nil means "no cap", so `read_stdin_bytes($limit)`
    // works when `$limit` is unset. Pinned because it is a documented
    // difference between two builtins that otherwise share a cap contract —
    // exactly the kind of thing a later "make these consistent" edit would
    // change without noticing the doc.
    let out = run_filter("write_stdout(read_stdin_bytes(nil))", BINARY_INPUT);
    assert!(
        out.status.success(),
        "status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, BINARY_INPUT, "nil must read the whole stream");

    // A cap of 0 is a real cap, not "absent".
    let zero = run_filter("write_stdout(\"n=\", bytes_len(read_stdin_bytes(0)))", BINARY_INPUT);
    assert!(zero.status.success(), "status={:?}", zero.status);
    assert_eq!(zero.stdout, b"n=0");
}

#[test]
fn empty_stdin_is_empty_bytes_not_an_error() {
    let out = run_filter("write_stdout(\"len=\", bytes_len(read_stdin_bytes()))", b"");
    assert!(
        out.status.success(),
        "status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"len=0");
}

#[test]
fn filter_output_is_flushed_without_a_trailing_newline() {
    // The sieve case: `execute :pipe :output` captures stdout verbatim and a
    // trailing newline corrupts the variable. Nothing may be added, and the
    // bytes must actually leave the process — stdout is line-buffered, so an
    // unflushed write with no newline in it would arrive as nothing at all.
    let out = run_filter("write_stdout(\"SPAM\")", b"");
    assert!(out.status.success(), "status={:?}", out.status);
    assert_eq!(out.stdout, b"SPAM");
}

#[cfg(unix)]
#[test]
fn broken_pipe_raises_and_the_documented_idiom_exits_quietly() {
    use std::io::Read;

    // Default: a failed write RAISES rather than being swallowed, so a
    // filter cannot report success having written nothing.
    let loud = "$i = 0\nwhile $i < 200000\n  write_stdout(\"line \", $i, \"\\n\")\n  $i = $i + 1\nend\n";
    let mut child = Command::new(env!("CARGO_BIN_EXE_mix"))
        .args(["-c", loud])
        .env("MIX_STATS", "off")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mix");
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut first = [0u8; 1];
    stdout.read_exact(&mut first).expect("read first byte");
    drop(stdout); // the `| head -1` moment
    let out = child.wait_with_output().expect("wait");
    assert!(!out.status.success(), "a dropped write must not report success");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Broken pipe"), "stderr: {stderr}");

    // ...and the idiom documented on the io page turns that into a quiet
    // exit 0 with nothing on stderr. Both halves are pinned, because the
    // default only makes sense if the opt-out actually works.
    let quiet = concat!(
        "$i = 0\n",
        "while $i < 200000\n",
        "  try\n",
        "    write_stdout(\"line \", $i, \"\\n\")\n",
        "  catch $m, $e\n",
        "    if $e.code == \"IO_BROKEN_PIPE\" then\n",
        "      exit(0)\n",
        "    end\n",
        "    raise($e.code, $m)\n",
        "  end\n",
        "  $i = $i + 1\n",
        "end\n",
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_mix"))
        .args(["-c", quiet])
        .env("MIX_STATS", "off")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mix");
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut first = [0u8; 1];
    stdout.read_exact(&mut first).expect("read first byte");
    drop(stdout);
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "the documented idiom must exit 0, status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stderr.is_empty(), "stderr: {:?}", out.stderr);
}
