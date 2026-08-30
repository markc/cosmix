//! Pure synthesis helpers over rustysynth — loading + settings.
//!
//! No audio hardware and no Cosmix runtime deps. Shared by the offline renderer
//! (`render.rs`) and the real-time file player (`play.rs`).

use std::fs::File;
use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use rustysynth::{MidiFile, SoundFont, SynthesizerSettings};

/// Default render/playback sample rate when none is requested.
pub const DEFAULT_SAMPLE_RATE: i32 = 44_100;
/// rustysynth's default voice cap (range 8..=256).
pub const DEFAULT_MAX_POLYPHONY: usize = 64;

/// Load an SF2 SoundFont into a shared, reusable handle.
pub fn load_soundfont<P: AsRef<Path>>(path: P) -> Result<Arc<SoundFont>> {
    let path = path.as_ref();
    // A `.sfz` is converted to an in-memory SF2 by the pure-Rust converter
    // (no Polyphone shell-out); the rest of the path is unchanged.
    #[cfg(feature = "sfz")]
    if path
        .extension()
        .map(|e| e.eq_ignore_ascii_case("sfz"))
        .unwrap_or(false)
    {
        let (bytes, ignored, skipped) = crate::sfz::sfz_to_sf2(path)
            .map_err(|e| anyhow!("convert SFZ {}: {e}", path.display()))?;
        if !ignored.is_empty() || skipped > 0 {
            eprintln!(
                "sfz {}: {skipped} region(s) skipped (no SF2 equivalent: release/pedal/generator/round-robin), unmodelled opcodes: [{}]",
                path.display(),
                ignored.join(", ")
            );
        }
        let sf = SoundFont::new(&mut Cursor::new(bytes))
            .map_err(|e| anyhow!("parse converted SFZ {}: {e}", path.display()))?;
        return Ok(Arc::new(sf));
    }
    let bytes =
        std::fs::read(path).map_err(|e| anyhow!("open soundfont {}: {e}", path.display()))?;
    // Transparently decode SF3 (Ogg-compressed samples) to uncompressed SF2 in
    // memory; an already-uncompressed SF2 passes through untouched. rustysynth
    // only reads uncompressed SF2, so this is what unlocks SF3 banks.
    #[cfg(feature = "sf3")]
    let bytes = crate::sf3::maybe_decode_sf3(bytes)
        .map_err(|e| anyhow!("decode SF3 {}: {e}", path.display()))?;
    let sf = SoundFont::new(&mut Cursor::new(bytes))
        .map_err(|e| anyhow!("parse soundfont {}: {e}", path.display()))?;
    Ok(Arc::new(sf))
}

/// Load an in-memory SF2 image (e.g. an auto-assembled orchestra bank) into
/// a shared handle. The bytes must already be uncompressed SF2.
pub fn load_soundfont_bytes(bytes: Vec<u8>) -> Result<Arc<SoundFont>> {
    let sf = SoundFont::new(&mut Cursor::new(bytes))
        .map_err(|e| anyhow!("parse in-memory soundfont: {e}"))?;
    Ok(Arc::new(sf))
}

/// Load a Standard MIDI File into a shared handle.
pub fn load_midi<P: AsRef<Path>>(path: P) -> Result<Arc<MidiFile>> {
    let path = path.as_ref();
    let mut f = File::open(path).map_err(|e| anyhow!("open midi {}: {e}", path.display()))?;
    let mid = MidiFile::new(&mut f).map_err(|e| anyhow!("parse midi {}: {e}", path.display()))?;
    Ok(Arc::new(mid))
}

/// Build synthesizer settings at a given sample rate. `Synthesizer::new`
/// validates the field ranges and will error on out-of-range values.
pub fn make_settings(
    sample_rate: i32,
    max_polyphony: usize,
    reverb_chorus: bool,
) -> SynthesizerSettings {
    let mut s = SynthesizerSettings::new(sample_rate);
    s.maximum_polyphony = max_polyphony;
    s.enable_reverb_and_chorus = reverb_chorus;
    s
}
