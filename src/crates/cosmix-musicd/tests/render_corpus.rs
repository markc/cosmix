//! Corpus render test: real MIDI → SoundFont, asserting non-silent +
//! byte-deterministic output.
//!
//! Needs a SoundFont, supplied out-of-band so no large binary lives in the repo:
//! set `MUSICD_TEST_SF2=/path/to/bank.sf2`. Absent → the test skips (prints a
//! note) rather than failing, so CI stays green without the asset.
//!
//! Run: `MUSICD_TEST_SF2=/path/FluidR3_GM_GS.sf2 cargo test -p cosmix-musicd \
//!        --no-default-features --test render_corpus`

use std::path::PathBuf;

use cosmix_musicd::render::{RenderOptions, render_to_buffers};
use cosmix_musicd::synth;

fn test_sf2() -> Option<PathBuf> {
    std::env::var_os("MUSICD_TEST_SF2")
        .map(PathBuf::from)
        .filter(|p| p.exists())
}

fn fixture_dir() -> PathBuf {
    manifest_dir().join("tests/fixtures")
}

/// The 3 self-contained fixtures shipped in the crate (no coupling to siblings).
fn fixtures() -> Vec<PathBuf> {
    let dir = fixture_dir();
    [
        "blues-shuffle.mid",
        "classical-minuet.mid",
        "chiptune-8bit.mid",
    ]
    .iter()
    .map(|f| dir.join(f))
    .collect()
}

#[test]
fn corpus_renders_non_silent_and_deterministic() {
    let Some(sf2) = test_sf2() else {
        eprintln!("SKIP: set MUSICD_TEST_SF2=/path/to/bank.sf2 to run the render corpus test");
        return;
    };
    let sf = synth::load_soundfont(&sf2).expect("load soundfont");
    let opts = RenderOptions::default();

    for mid in fixtures() {
        let midi = synth::load_midi(&mid).unwrap_or_else(|e| panic!("load {}: {e}", mid.display()));
        let (l1, r1, report) = render_to_buffers(&sf, &midi, &opts).expect("render pass 1");

        assert!(report.frames > 0, "{}: zero frames", mid.display());
        assert!(report.peak > 0.0, "{}: silent output", mid.display());

        // Determinism: a second render must be bit-identical (no RNG anywhere).
        let (l2, r2, _) = render_to_buffers(&sf, &midi, &opts).expect("render pass 2");
        assert_eq!(l1, l2, "{}: left channel not deterministic", mid.display());
        assert_eq!(r1, r2, "{}: right channel not deterministic", mid.display());
    }
}

/// Run-time manifest directory rather than the `env!`-baked one: cargo exports
/// `CARGO_MANIFEST_DIR` into the test process, and that names the tree cargo is
/// actually running in, whereas `env!` records whichever tree last *compiled*
/// the binary. The two diverge when one `CARGO_TARGET_DIR` is shared across
/// several git worktrees of this repo — cargo writes workspace-relative paths
/// into its dep-info, so an artefact built in a sibling worktree is judged
/// fresh and rerun here, still pointing at that tree's fixtures. Falls back to
/// the compile-time value when the binary is run outside cargo.
fn manifest_dir() -> std::path::PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")))
}
