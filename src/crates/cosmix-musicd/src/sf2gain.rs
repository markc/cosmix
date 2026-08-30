//! Loudness probe + PCM gain for SF2 images — the `split --normalize` core.
//!
//! Different SoundFonts are mastered at different loudness, so mixing
//! per-track extracts from several banks (the whole point of
//! [`crate::sf2split`]) needs a way to bring them to a common level.
//!
//! Design choices, deliberately:
//!
//! - **Measure by rendering**, not by inspecting sample peaks: the audible
//!   level is shaped by the preset's attenuation generators, velocity curves
//!   and envelopes, so the only honest number comes from playing notes
//!   through the same synthesis model the renderer uses (rustysynth,
//!   reverb/chorus off, windowed RMS).
//! - **Correct in the sample domain**, not via `initialAttenuation`: hosts
//!   disagree on attenuation scaling (FluidSynth applies ~0.4× the spec's
//!   centibels), so a generator tweak lands differently in Ardour than in
//!   musicd. Scaling PCM is exact and identical in every SF2 host.
//! - **Down-only**: gain ≤ 1.0 can never clip; boosting is refused. The
//!   bit-depth cost of a typical ≤12 dB trim is < 2 bits — inaudible.
//!   24-bit fonts are scaled in the combined smpl+sm24 domain.

use std::io::Cursor;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use rustysynth::{SoundFont, Synthesizer, SynthesizerSettings};

use crate::riff::{Chunk, find_leaf_mut, parse_chunks, write_chunk};

const PROBE_RATE: i32 = 44_100;
const PROBE_VELOCITY: i32 = 100;
/// Hold each probe note this long and take the loudest RMS window inside it.
const HOLD_S: f32 = 0.5;
/// RMS window: long enough to smooth waveform periods, short enough that a
/// decaying instrument's attack still dominates.
const WINDOW_S: f32 = 0.05;
/// Probe keys below this are considered not covered by the preset.
const SILENCE_DBFS: f32 = -80.0;

/// Melodic probe keys: middle C outward, so narrow-range presets still get
/// hit by at least one.
const MELODIC_KEYS: [i32; 5] = [60, 48, 72, 36, 84];
/// Drum-kit probe keys: kick, snare, closed hat, low tom, crash.
const DRUM_KEYS: [i32; 5] = [36, 38, 42, 45, 49];

/// Loudness of one preset: the mean over sounding probe keys of each key's
/// loudest RMS window, in dBFS. `None` when no probe key produced sound
/// (empty or exotic preset) — callers should leave such extracts untouched.
pub fn measure_dbfs(sf2: &[u8], bank: u16, program: u16) -> Result<Option<f32>> {
    let font = SoundFont::new(&mut Cursor::new(sf2.to_vec()))
        .map_err(|e| anyhow!("parse for level probe: {e}"))?;
    let mut settings = SynthesizerSettings::new(PROBE_RATE);
    settings.enable_reverb_and_chorus = false; // dry, deterministic level
    let mut synth = Synthesizer::new(&Arc::new(font), &settings)
        .map_err(|e| anyhow!("probe synthesizer: {e}"))?;

    // Channel 9 is the percussion channel (bank 128) in the GM model
    // rustysynth implements; melodic banks select via CC0.
    let (channel, keys) = if bank == 128 {
        (9, DRUM_KEYS)
    } else {
        (0, MELODIC_KEYS)
    };
    if bank != 128 {
        synth.process_midi_message(channel, 0xB0, 0x00, bank as i32);
    }
    synth.process_midi_message(channel, 0xC0, program as i32, 0);

    let hold = (HOLD_S * PROBE_RATE as f32) as usize;
    let window = (WINDOW_S * PROBE_RATE as f32) as usize;
    let mut left = vec![0.0f32; hold];
    let mut right = vec![0.0f32; hold];
    let mut sounding: Vec<f32> = Vec::new();
    for key in keys {
        synth.note_on(channel, key, PROBE_VELOCITY);
        synth.render(&mut left, &mut right);
        synth.note_off_all(true);
        // A tail render so the killed voice can't bleed into the next probe.
        let mut tl = vec![0.0f32; window];
        let mut tr = vec![0.0f32; window];
        synth.render(&mut tl, &mut tr);

        let mut peak_rms = 0.0f32;
        for w in left.chunks(window).zip(right.chunks(window)).map(|(l, r)| {
            let e: f32 = l.iter().zip(r).map(|(a, b)| (a * a + b * b) / 2.0).sum();
            (e / l.len() as f32).sqrt()
        }) {
            if w > peak_rms {
                peak_rms = w;
            }
        }
        let dbfs = 20.0 * peak_rms.max(1e-12).log10();
        if dbfs > SILENCE_DBFS {
            sounding.push(dbfs);
        }
    }
    if sounding.is_empty() {
        return Ok(None);
    }
    Ok(Some(sounding.iter().sum::<f32>() / sounding.len() as f32))
}

/// Scale every sample in an SF2 image by `gain` (0 < gain ≤ 1.0, down-only —
/// boosting could clip). With an `sm24` chunk present the scaling happens in
/// the combined 24-bit domain and both chunks are rewritten; otherwise plain
/// 16-bit with rounding. Returns the rewritten image.
pub fn scale_pcm(sf2: &[u8], gain: f32) -> Result<Vec<u8>> {
    if !(gain > 0.0 && gain <= 1.0) {
        bail!("gain {gain} out of range (0, 1] — normalization only attenuates");
    }
    let mut roots = parse_chunks(sf2)?;
    let Some(Chunk::List { id, form, children }) = roots.first_mut() else {
        bail!("not a RIFF container");
    };
    if id != b"RIFF" || form != b"sfbk" {
        bail!("not an sfbk SoundFont");
    }
    // Take sm24 out first so the two mutable leaf borrows don't overlap.
    let sm24 = find_leaf_mut(children, b"sdta", b"sm24").map(std::mem::take);
    let smpl = find_leaf_mut(children, b"sdta", b"smpl")
        .ok_or_else(|| anyhow!("SoundFont has no sample data"))?;
    let words = smpl.len() / 2;
    let mut sm24 = sm24;
    for i in 0..words {
        let hi = i16::from_le_bytes([smpl[2 * i], smpl[2 * i + 1]]);
        let new_hi = match sm24.as_mut() {
            // 24-bit signed sample: smpl carries the 16 MSB, sm24 the extra
            // LSB. Scale in the combined domain; hi/lo split is exact.
            Some(s) if i < s.len() => {
                let v24 = ((hi as i32) << 8) | s[i] as i32;
                let scaled =
                    ((v24 as f64 * gain as f64).round() as i32).clamp(-(1 << 23), (1 << 23) - 1);
                s[i] = (scaled & 0xFF) as u8;
                (scaled >> 8) as i16
            }
            // 16-bit: round at 16-bit precision (a 24-bit shift would floor).
            _ => ((hi as f64 * gain as f64).round() as i32).clamp(i16::MIN as i32, i16::MAX as i32)
                as i16,
        };
        smpl[2 * i..2 * i + 2].copy_from_slice(&new_hi.to_le_bytes());
    }
    if let Some(s) = sm24 {
        *find_leaf_mut(children, b"sdta", b"sm24").expect("sm24 located above") = s;
    }
    let mut out = Vec::with_capacity(sf2.len());
    write_chunk(roots.first().unwrap(), &mut out)?;
    Ok(out)
}

/// dB → linear gain.
pub fn db_to_gain(db: f32) -> f32 {
    10.0f32.powf(db / 20.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_gain_roundtrip() {
        assert!((db_to_gain(-6.0206) - 0.5).abs() < 1e-4);
        assert!((db_to_gain(0.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn scale_pcm_halves_16bit_samples() {
        // Reuse the splitter's synthetic font: sample 0 is a ramp 0..7,
        // sample 1 is 100..107 — halving must round-scale every word.
        let font = crate::sf2split::tests::build_test_font();
        let scaled = scale_pcm(&font, 0.5).unwrap();
        let roots = parse_chunks(&scaled).unwrap();
        let Chunk::List { children, .. } = &roots[0] else {
            panic!()
        };
        let smpl = crate::riff::find_leaf(children, b"sdta", b"smpl").unwrap();
        let v = |i: usize| i16::from_le_bytes([smpl[2 * i], smpl[2 * i + 1]]);
        assert_eq!(v(0), 0);
        assert_eq!(v(1), 1); // round(1*0.5) = 1 (round half away from zero)
        assert_eq!(v(2), 1);
        // Sample 1 starts at word 54 (8 + 46 guard): 100 → 50.
        assert_eq!(v(54), 50);
        // Down-only contract: boosting is refused.
        assert!(scale_pcm(&font, 1.5).is_err());
        assert!(scale_pcm(&font, 0.0).is_err());
    }

    #[test]
    fn measure_probe_is_finite_or_none() {
        // The synthetic font's samples are near-silent one-shot ramps; the
        // probe must return cleanly either way, never error or NaN.
        let font = crate::sf2split::tests::build_test_font();
        let m = measure_dbfs(&font, 0, 0).unwrap();
        if let Some(db) = m {
            assert!(db.is_finite());
        }
    }
}
