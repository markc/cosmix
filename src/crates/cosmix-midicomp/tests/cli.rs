//! End-to-end tests driving the built `cosmix-midicomp` binary, mirroring the
//! original project's CTest suite (plain / verbose / roundtrip / canonical) plus
//! a synthetic file exercising every event type the grammar supports.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_cosmix-midicomp")
}

/// This crate's manifest directory, resolved when the test *runs* rather than
/// baked in by `env!`. Cargo exports `CARGO_MANIFEST_DIR` into the test
/// process too, and that value names the tree cargo is actually running in;
/// the `env!` form records whichever tree last *compiled* the binary. The two
/// diverge whenever one `CARGO_TARGET_DIR` is shared across several git
/// worktrees of this repo: cargo writes workspace-*relative* paths into its
/// dep-info, so it resolves them against the current worktree, judges an
/// artefact built in a sibling worktree "fresh", and reruns that binary here —
/// still pointing at the other tree's fixtures. When that tree has since been
/// removed the fixture reads fail with ENOENT on a path this worktree never
/// had. Falls back to the compile-time value for a test binary invoked
/// directly, outside cargo, where the variable is absent.
fn manifest_dir() -> PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")))
}

fn fixture(name: &str) -> PathBuf {
    manifest_dir().join("tests").join(name)
}

/// Run the binary with `args`, feeding `stdin` bytes, returning stdout bytes.
fn run(args: &[&str], stdin: &[u8]) -> Vec<u8> {
    let mut child = Command::new(bin())
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn cosmix-midicomp");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin)
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "binary failed: {:?}", args);
    out.stdout
}

/// Decode a `.mid` fixture to text (no stdin).
fn decode(args: &[&str]) -> String {
    String::from_utf8(run(args, &[])).expect("utf8 text")
}

#[test]
fn plain_decode_matches_golden() {
    let got = decode(&[fixture("ex1.mid").to_str().unwrap()]);
    let want = std::fs::read_to_string(fixture("ex1-plain.txt")).unwrap();
    assert_eq!(got, want, "plain decode must match ex1-plain.txt");
}

#[test]
fn verbose_decode_matches_golden() {
    // `ex1-verbose.txt` is `-v -t`: absolute bar:beat:click time in columns.
    let got = decode(&["-v", "-t", fixture("ex1.mid").to_str().unwrap()]);
    let want = std::fs::read_to_string(fixture("ex1-verbose.txt")).unwrap();
    assert_eq!(got, want, "verbose decode must match ex1-verbose.txt");
}

#[test]
fn text_roundtrip_is_stable() {
    // decode -> compile -> decode reproduces the first decode (idempotent text).
    let a = run(&[fixture("ex1.mid").to_str().unwrap()], &[]);
    let mid = run(&["-c", "/dev/stdout"], &a); // compile a.txt -> SMF on stdout
    let b = run(&["/dev/stdin"], &mid); // decode that SMF
    assert_eq!(a, b, "text round-trip must be stable");
}

#[test]
fn smf_output_is_canonical() {
    // Once compiled, midicomp's own SMF output is byte-stable on re-compile.
    let a = run(&[fixture("ex1.mid").to_str().unwrap()], &[]);
    let c1 = run(&["-c", "/dev/stdout"], &a);
    let c1_txt = run(&["/dev/stdin"], &c1);
    let c2 = run(&["-c", "/dev/stdout"], &c1_txt);
    assert_eq!(c1, c2, "compiled SMF must be byte-stable");
}

#[test]
fn note_names_render() {
    // `-n` turns the numeric note into a symbolic name in the compact format.
    let got = decode(&["-n", fixture("ex1.mid").to_str().unwrap()]);
    assert!(
        got.contains("On ch=1 n=c4 v=70"),
        "note 48 should render as c4, got:\n{got}"
    );
}

/// A synthetic file exercising every event type, with string escapes, symbolic
/// notes, hex/bank numbers, and a comment. Compiling then decoding must be
/// idempotent (gen-1 text == gen-2 text), the SMF byte-stable, and individual
/// lines must decode to the expected canonical form.
#[test]
fn synthetic_all_events_roundtrip() {
    let src = r#"MFile 0 1 480
MTrk
0 Meta SeqName "Hi \"there\""   # a comment here
0 PrCh ch=1 p=5
0 Par ch=1 c=7 v=100
0 Pb ch=1 v=8192
0 ChPr ch=1 v=64
0 PoPr ch=1 n=60 v=10
0 KeySig 2 major
0 TimeSig 3/4 24 8
0 SysEx f0 7e 00 09 01 f7
0 SeqSpec 01 02 03
0 Meta 0x09 de ad
0 SeqNr 1
10 On ch=1 n=c#4 v=0x64
20 Off ch=1 n=61 v=0
30 Meta TrkEnd
TrkEnd
"#;

    // gen-1: compile + decode.
    let mid1 = run(&["-c", "/dev/stdout"], src.as_bytes());
    let txt1 = String::from_utf8(run(&["/dev/stdin"], &mid1)).unwrap();

    // gen-2: re-compile + re-decode must be identical (text) and byte-stable.
    let mid2 = run(&["-c", "/dev/stdout"], txt1.as_bytes());
    let txt2 = String::from_utf8(run(&["/dev/stdin"], &mid2)).unwrap();
    assert_eq!(txt1, txt2, "synthetic decode must be idempotent");
    assert_eq!(mid1, mid2, "synthetic SMF must be byte-stable");

    // Spot-check canonical decoded forms.
    let want_lines = [
        "MFile 0 1 480",
        r#"0 Meta SeqName "Hi \"there\"""#, // string escapes survive
        "0 PrCh ch=1 p=5",
        "0 Par ch=1 c=7 v=100",
        "0 Pb ch=1 v=8192", // 14-bit pitch bend
        "0 ChPr ch=1 v=64",
        "0 PoPr ch=1 n=60 v=10",
        "0 KeySig 2 major",
        "0 TimeSig 3/4 24 8",
        "0 SysEx f0 7e 00 09 01 f7", // leading F0 restored on output
        "0 SeqSpec 01 02 03",
        r#"0 Meta 0x09 "\xde\xad""#, // 0x09 (DeviceName) is a text-meta -> string form
        "0 SeqNr 1",
        "10 On ch=1 n=49 v=100", // c#4 == 49, 0x64 == 100
        "20 Off ch=1 n=61 v=0",  // a real NoteOff is preserved as Off
        "30 Meta TrkEnd",
        "TrkEnd",
    ];
    for line in want_lines {
        assert!(
            txt1.lines().any(|l| l == line),
            "expected decoded line {line:?} in:\n{txt1}"
        );
    }
}

/// Bank numbers (`$`/`'` base-8) and `0x` hex are accepted as integers.
#[test]
fn bank_and_hex_numbers() {
    // $11 == octal 00 (each digit -1) -> 0; channel 1 -> ch=1.  Use a value:
    // v='11 -> octal 00 -> 0.  Verify a hex velocity and a bank velocity.
    let src = "MFile 0 1 96\nMTrk\n0 On ch=1 n=60 v=0x7f\n0 On ch=1 n=60 v='11\nTrkEnd\n";
    let mid = run(&["-c", "/dev/stdout"], src.as_bytes());
    let txt = String::from_utf8(run(&["/dev/stdin"], &mid)).unwrap();
    assert!(txt.contains("v=127"), "0x7f -> 127:\n{txt}");
    assert!(txt.contains("v=0"), "'11 (base-8 00) -> 0:\n{txt}");
}

/// Run the compiler expecting a non-zero exit on malformed input.
fn expect_compile_failure(src: &str) {
    let mut child = Command::new(bin())
        .args(["-c", "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(src.as_bytes())
        .unwrap();
    let status = child.wait().unwrap();
    assert!(!status.success(), "expected failure for input:\n{src}");
}

#[test]
fn rejects_malformed_input() {
    // Unterminated string.
    expect_compile_failure("MFile 0 1 96\nMTrk\n0 Meta Text \"oops\nTrkEnd\n");
    // PPQN beyond the 15-bit range.
    expect_compile_failure("MFile 0 1 40000\nMTrk\nTrkEnd\n");
    // Out-of-range channel.
    expect_compile_failure("MFile 0 1 96\nMTrk\n0 On ch=99 n=60 v=64\nTrkEnd\n");
    // Note value overflow (hard lex error -> not an integer -> fails).
    expect_compile_failure("MFile 0 1 96\nMTrk\n0 On ch=1 n=99999999999999999999 v=64\nTrkEnd\n");
    // Malformed hex tail must not be silently truncated.
    expect_compile_failure("MFile 0 1 96\nMTrk\n0 SysEx f0 7g f7\nTrkEnd\n");
    // Out-of-range tempo.
    expect_compile_failure("MFile 0 1 96\nMTrk\n0 Tempo 99999999\nTrkEnd\n");
    // Unsupported MFile format.
    expect_compile_failure("MFile 5 1 96\nMTrk\nTrkEnd\n");
    // Declared track count not matching the body.
    expect_compile_failure("MFile 1 2 96\nMTrk\nTrkEnd\n");
}
