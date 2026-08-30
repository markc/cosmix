//! Example-song round-trip suite — the "byte in equals byte out" guarantee.
//!
//! `examples/songs/` holds a dozen short (~30 s) pieces across musical styles,
//! each as a **canonical pair**:
//!   * `<name>.asc` — the plain-text form *exactly as the decoder emits it*
//!   * `<name>.mid` — the Standard MIDI File it compiles to
//!
//! Because each `.mid` is produced by compiling its own canonical `.asc`, the
//! pair is a fixed point of the converter. This suite pins three properties for
//! every song, so any change that perturbs the byte-level encoding or the text
//! rendering is caught:
//!
//!   1. **decode golden** — `decode(<name>.mid) == <name>.asc`
//!   2. **byte in == byte out** — `compile(<name>.asc) == <name>.mid` (byte-exact)
//!   3. **round-trip stability** — `decode(compile(decode(mid))) == decode(mid)`
//!
//! Collectively the songs exercise every event type the grammar supports
//! (see `examples/songs/README.md` for the per-song coverage matrix).

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

fn songs_dir() -> PathBuf {
    manifest_dir().join("examples").join("songs")
}

/// A scratch path unique to this test binary (cargo-provided).
fn scratch(name: &str) -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(name)
}

/// Decode an SMF file to text (`cosmix-midicomp <file.mid>`), returning stdout.
fn decode(mid: &Path) -> Vec<u8> {
    let out = Command::new(bin()).arg(mid).output().expect("spawn decode");
    assert!(
        out.status.success(),
        "decode {} failed: {}",
        mid.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// Compile a text file to an SMF (`cosmix-midicomp -c <in.asc> <out.mid>`),
/// returning the written SMF bytes.
fn compile(asc: &Path, out_mid: &Path) -> Vec<u8> {
    let status = Command::new(bin())
        .args(["-c"])
        .arg(asc)
        .arg(out_mid)
        .status()
        .expect("spawn compile");
    assert!(status.success(), "compile {} failed", asc.display());
    std::fs::read(out_mid).expect("read compiled SMF")
}

/// Compile text bytes (via stdin) to an SMF written at `out_mid`, returning the bytes.
fn compile_stdin(text: &[u8], out_mid: &Path) -> Vec<u8> {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new(bin())
        .arg("-c")
        .arg(out_mid)
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn compile stdin");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(text)
        .expect("write stdin");
    assert!(
        child.wait().expect("wait").success(),
        "compile stdin failed"
    );
    std::fs::read(out_mid).expect("read compiled SMF")
}

/// Every `<name>.mid` in `examples/songs/` that has a matching `<name>.asc`.
fn song_pairs() -> Vec<(String, PathBuf, PathBuf)> {
    let dir = songs_dir();
    let mut pairs = Vec::new();
    let entries =
        std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("mid") {
            continue;
        }
        let stem = path.file_stem().unwrap().to_str().unwrap().to_string();
        let asc = dir.join(format!("{stem}.asc"));
        assert!(asc.exists(), "{stem}.mid has no matching {stem}.asc");
        pairs.push((stem, asc, path));
    }
    pairs.sort();
    pairs
}

#[test]
fn examples_present() {
    let pairs = song_pairs();
    assert!(
        pairs.len() >= 12,
        "expected at least a dozen example songs, found {}",
        pairs.len()
    );
}

#[test]
fn decode_matches_golden_text() {
    for (name, asc, mid) in song_pairs() {
        let got = decode(&mid);
        let want = std::fs::read(&asc).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&got),
            String::from_utf8_lossy(&want),
            "decode({name}.mid) must equal {name}.asc"
        );
    }
}

#[test]
fn byte_in_equals_byte_out() {
    for (name, asc, mid) in song_pairs() {
        let want = std::fs::read(&mid).unwrap();
        let got = compile(&asc, &scratch(&format!("{name}.bibo.mid")));
        assert_eq!(
            got,
            want,
            "compile({name}.asc) must be byte-identical to {name}.mid \
             ({} bytes produced vs {} expected)",
            got.len(),
            want.len()
        );
    }
}

#[test]
fn round_trip_is_stable() {
    for (name, _asc, mid) in song_pairs() {
        let text1 = decode(&mid);
        let remid = compile_stdin(&text1, &scratch(&format!("{name}.rt.mid")));
        let text2 = decode_bytes(&remid, &name);
        assert_eq!(
            String::from_utf8_lossy(&text1),
            String::from_utf8_lossy(&text2),
            "decode->compile->decode must be stable for {name}"
        );
    }
}

/// Decode SMF bytes (written to scratch first) — helper for the stability test.
fn decode_bytes(mid: &[u8], name: &str) -> Vec<u8> {
    let p = scratch(&format!("{name}.decbytes.mid"));
    std::fs::write(&p, mid).unwrap();
    decode(&p)
}
