//! Adversarial + SMPTE fixtures, ported from the C `midicomp` CTest suite
//! (`security` and `smpte` modes). The corpus in `tests/fixtures/` pins inputs
//! that previously crashed the C decoder (NULL deref / OOB read / SIGFPE) or
//! triggered undefined behaviour, and an SMPTE-division round-trip.

use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_cosmix-midicomp")
}

/// Run-time manifest directory rather than the `env!`-baked one — see the
/// note on `manifest_dir` in `tests/cli.rs` for why the two differ when a
/// `CARGO_TARGET_DIR` is shared across git worktrees.
fn manifest_dir() -> PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")))
}

fn fixture(name: &str) -> PathBuf {
    manifest_dir().join("tests").join("fixtures").join(name)
}

fn scratch(name: &str) -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(name)
}

/// Run the binary, returning `(exit_code_or_none, stdout)`. A `None` code means
/// the process died from a signal — i.e. a crash, which must never happen.
fn run(args: &[&str]) -> (Option<i32>, Vec<u8>) {
    let out = Command::new(bin()).args(args).output().expect("spawn");
    (out.status.code(), out.stdout)
}

/// Assert the binary handled an input cleanly: it exited (no signal) with code
/// 0 (decoded fine) or 1 (reported a clean error) — never a crash (SIGSEGV /
/// SIGFPE / abort), which shows up as `None` or a code >= 128.
fn assert_no_crash(args: &[&str], what: &str) {
    let (code, _) = run(args);
    match code {
        Some(0) | Some(1) => {}
        other => panic!("CRASH / abnormal exit on {what}: {args:?} -> {other:?}"),
    }
}

#[test]
fn security_corpus_never_crashes() {
    // Decode path (also under -t, which exercises the prtime division guards,
    // and -v / -n for the rendering paths).
    let decode_fixtures = [
        "meta-tempo-len0.mid",
        "meta-smpte-len0.mid",
        "meta-keysig-len0.mid",
        "header-division0.mid",
        "huge-varlen.mid",
    ];
    for f in decode_fixtures {
        let path = fixture(f);
        let p = path.to_str().unwrap();
        for flags in [
            vec![p],
            vec!["-t", p],
            vec!["-v", p],
            vec!["-n", p],
            vec!["-t", "-v", p],
        ] {
            assert_no_crash(&flags, &format!("decode {f} {flags:?}"));
        }
    }

    // Compile path with out-of-range / malformed values (`-c IN.txt OUT.mid`).
    for f in [
        "compile-value-oob.txt",
        "compile-timesig-denom0.txt",
        "compile-hex-oob.txt",
    ] {
        let inp = fixture(f);
        let out = scratch(&format!("{f}.out.mid"));
        assert_no_crash(
            &["-c", inp.to_str().unwrap(), out.to_str().unwrap()],
            &format!("compile {f}"),
        );
    }
}

/// The decode-side data-loss fix: a short fixed-length meta event is tolerated
/// (zero-padded to the typed form) and never silently discards the event or the
/// rest of the track. These pin the exact recovered output.
#[test]
fn short_meta_events_are_zero_padded_not_dropped() {
    // Zero-length tempo -> `Tempo 0` (not a raw `Meta 0x51`).
    let (_, out) = run(&[fixture("meta-tempo-len0.mid").to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out);
    assert!(
        text.contains("0 Tempo 0"),
        "tempo-len0 -> `Tempo 0`:\n{text}"
    );

    // Zero-length SMPTE -> five zeroed fields.
    let (_, out) = run(&[fixture("meta-smpte-len0.mid").to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out);
    assert!(
        text.contains("0 SMPTE 0 0 0 0 0"),
        "smpte-len0 -> `SMPTE 0 0 0 0 0`:\n{text}"
    );

    // Zero-length key signature -> `KeySig 0 major`, and crucially it is emitted
    // (the old library-backed decode silently dropped it and the rest of the
    // track). The fixture's trailing bytes are themselves malformed, so the
    // process still reports a clean error exit — but the KeySig survives.
    let (code, out) = run(&[fixture("meta-keysig-len0.mid").to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out);
    assert!(
        text.contains("0 KeySig 0 major"),
        "keysig-len0 -> `KeySig 0 major` must survive:\n{text}"
    );
    assert!(
        matches!(code, Some(0) | Some(1)),
        "clean exit, got {code:?}"
    );
}

/// SMPTE-division headers (`MFile <fmt> <ntrks> -<fps> <ticks>`) compile and
/// round-trip byte-for-byte, in both directions. (A too-strict division bound
/// once rejected valid SMPTE timing.)
#[test]
fn smpte_division_round_trips() {
    let src = fixture("smpte.txt");
    let mid = scratch("smpte.mid");
    let status = Command::new(bin())
        .args(["-c"])
        .arg(&src)
        .arg(&mid)
        .status()
        .expect("compile smpte");
    assert!(status.success(), "compiling smpte.txt failed");

    let (code, out) = run(&[mid.to_str().unwrap()]);
    assert_eq!(code, Some(0), "decoding smpte.mid should be clean");
    let want = std::fs::read(&src).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out),
        String::from_utf8_lossy(&want),
        "SMPTE decode must reproduce smpte.txt"
    );

    // And the compiled SMF is byte-stable: recompiling the decoded text
    // reproduces the same bytes.
    let txt = scratch("smpte.rt.txt");
    std::fs::write(&txt, &out).unwrap();
    let remid_path = scratch("smpte.rt.mid");
    let s = Command::new(bin())
        .arg("-c")
        .arg(&txt)
        .arg(&remid_path)
        .status()
        .unwrap();
    assert!(s.success(), "recompiling decoded SMPTE text failed");
    assert_eq!(
        std::fs::read(&remid_path).unwrap(),
        std::fs::read(&mid).unwrap(),
        "SMPTE compiled SMF must be byte-stable"
    );
}
