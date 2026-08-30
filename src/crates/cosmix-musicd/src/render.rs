//! Pure offline render: MIDI file -> stereo `f32` -> WAV/FLAC.
//!
//! No audio device is touched, so this works headless on any node and is the
//! FFI-free, deterministic core the corpus test exercises.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use flacenc::component::BitRepr;
use flacenc::error::Verify;
use rustysynth::{MidiFile, MidiFileSequencer, SoundFont, Synthesizer};

use crate::synth;

/// Knobs for a render. `Default` is 44.1 kHz, unity gain, reverb+chorus on.
#[derive(Clone, Copy, Debug)]
pub struct RenderOptions {
    pub sample_rate: i32,
    pub gain: f32,
    pub max_polyphony: usize,
    pub reverb_chorus: bool,
    /// Which channels to write. `render_to_buffers` always produces stereo L/R
    /// (rustysynth is natively stereo); this only decides the output layout.
    pub channels: Channels,
    /// Container/sample format of the output file.
    pub format: RenderFormat,
}

/// Output channel layout. `Mono` averages L+R; `Left`/`Right` take one channel
/// verbatim (a hard-panned source in the SoundFont stays intact this way).
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Channels {
    Stereo,
    Mono,
    Left,
    Right,
}

/// Output file format. 24-bit / float give the extra headroom a mixing stage
/// wants; 16-bit WAV stays the default for backward compatibility.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum RenderFormat {
    #[value(name = "wav16")]
    Wav16,
    #[value(name = "wav24")]
    Wav24,
    #[value(name = "wavf32", alias = "wav-f32")]
    WavF32,
    #[value(name = "flac24")]
    Flac24,
}

/// Output PCM sample format used by WAV writing. Not a CLI arg (the CLI takes
/// `--format`, which folds container + depth) — so no `ValueEnum` derive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitDepth {
    Bits16,
    Bits24,
    Float32,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            sample_rate: synth::DEFAULT_SAMPLE_RATE,
            gain: 1.0,
            max_polyphony: synth::DEFAULT_MAX_POLYPHONY,
            reverb_chorus: true,
            channels: Channels::Stereo,
            format: RenderFormat::Wav16,
        }
    }
}

/// Outcome of a render — enough for a caller to judge quality without the audio.
#[derive(Clone, Copy, Debug)]
pub struct RenderReport {
    pub frames: usize,
    pub duration_s: f64,
    pub sample_rate: i32,
    /// Max `|sample * gain|` BEFORE clipping — a value > 1.0 means the output
    /// clipped and the caller should re-render with lower gain.
    pub peak: f32,
    pub clipped: bool,
}

/// Render a whole MIDI file into separate left/right buffers. Applies `gain`
/// then hard-clips to `[-1.0, 1.0]` — EXCEPT for float output, which keeps
/// overs intact (that headroom is float's whole reason to exist; clipping it
/// upstream would make `wavf32` a pointless twin of `wav24`). The report's
/// `peak` is always the pre-clip value; `clipped` is true only when samples
/// were actually clamped. Deterministic: rustysynth uses no RNG, so the same
/// inputs yield byte-identical output.
pub fn render_to_buffers(
    sound_font: &Arc<SoundFont>,
    midi: &Arc<MidiFile>,
    opts: &RenderOptions,
) -> Result<(Vec<f32>, Vec<f32>, RenderReport)> {
    let settings = synth::make_settings(opts.sample_rate, opts.max_polyphony, opts.reverb_chorus);
    let synthesizer =
        Synthesizer::new(sound_font, &settings).map_err(|e| anyhow!("create synthesizer: {e}"))?;
    let mut sequencer = MidiFileSequencer::new(synthesizer);
    sequencer.play(midi, false);

    let duration_s = midi.get_length();
    // A tail so the final note release + reverb/chorus aren't truncated.
    let tail_s = if opts.reverb_chorus { 1.0 } else { 0.25 };
    let frames = ((duration_s + tail_s) * opts.sample_rate as f64).ceil() as usize;

    let mut left = vec![0.0f32; frames];
    let mut right = vec![0.0f32; frames];
    sequencer.render(&mut left, &mut right);

    // Gain + hard-clip (int formats only), tracking the pre-clip peak across
    // both channels. Float output passes overs through unclamped.
    let clamp = opts.format != RenderFormat::WavF32;
    let mut peak = 0.0f32;
    for buf in [&mut left, &mut right] {
        for s in buf.iter_mut() {
            let v = *s * opts.gain;
            let a = v.abs();
            if a > peak {
                peak = a;
            }
            *s = if clamp { v.clamp(-1.0, 1.0) } else { v };
        }
    }

    let report = RenderReport {
        frames,
        duration_s,
        sample_rate: opts.sample_rate,
        peak,
        clipped: clamp && peak > 1.0,
    };
    Ok((left, right, report))
}

/// Load SF2 + MIDI, render, and write the requested output file.
pub fn render_midi_to_file<P1, P2, P3>(
    midi_path: P1,
    soundfont_path: P2,
    out_path: P3,
    opts: &RenderOptions,
) -> Result<RenderReport>
where
    P1: AsRef<Path>,
    P2: AsRef<Path>,
    P3: AsRef<Path>,
{
    let sf = synth::load_soundfont(soundfont_path)?;
    let midi = synth::load_midi(midi_path)?;
    let (left, right, report) = render_to_buffers(&sf, &midi, opts)?;
    write_render(
        out_path,
        &left,
        &right,
        opts.sample_rate,
        opts.channels,
        opts.format,
    )?;
    Ok(report)
}

/// Backward-compatible name for callers that already use the old API. The
/// actual container now comes from `RenderOptions::format`.
pub fn render_midi_to_wav<P1, P2, P3>(
    midi_path: P1,
    soundfont_path: P2,
    out_path: P3,
    opts: &RenderOptions,
) -> Result<RenderReport>
where
    P1: AsRef<Path>,
    P2: AsRef<Path>,
    P3: AsRef<Path>,
{
    render_midi_to_file(midi_path, soundfont_path, out_path, opts)
}

/// Write stereo L/R buffers to the selected container/sample format.
pub fn write_render<P: AsRef<Path>>(
    path: P,
    left: &[f32],
    right: &[f32],
    sample_rate: i32,
    channels: Channels,
    format: RenderFormat,
) -> Result<()> {
    match format {
        RenderFormat::Wav16 => {
            write_wav(path, left, right, sample_rate, channels, BitDepth::Bits16)
        }
        RenderFormat::Wav24 => {
            write_wav(path, left, right, sample_rate, channels, BitDepth::Bits24)
        }
        RenderFormat::WavF32 => {
            write_wav(path, left, right, sample_rate, channels, BitDepth::Float32)
        }
        RenderFormat::Flac24 => write_flac24(path, left, right, sample_rate, channels),
    }
}

/// Write stereo L/R buffers to a WAV with a chosen channel layout and PCM
/// format. `channels` selects stereo / mono-average / left / right; `bit_depth`
/// selects 16-bit int, 24-bit int (pcm_s24le), or 32-bit float. Doing this in
/// the renderer removes the need for an external ffmpeg downmix/requantize.
pub fn write_wav<P: AsRef<Path>>(
    path: P,
    left: &[f32],
    right: &[f32],
    sample_rate: i32,
    channels: Channels,
    bit_depth: BitDepth,
) -> Result<()> {
    let path = path.as_ref();
    let out_channels: u16 = if channels == Channels::Stereo { 2 } else { 1 };
    let (bits, fmt) = match bit_depth {
        BitDepth::Bits16 => (16, hound::SampleFormat::Int),
        BitDepth::Bits24 => (24, hound::SampleFormat::Int),
        BitDepth::Float32 => (32, hound::SampleFormat::Float),
    };
    let spec = hound::WavSpec {
        channels: out_channels,
        sample_rate: sample_rate as u32,
        bits_per_sample: bits,
        sample_format: fmt,
    };
    let mut w = hound::WavWriter::create(path, spec)
        .map_err(|e| anyhow!("create wav {}: {e}", path.display()))?;

    let flat = flatten_channels(left, right, channels);
    for &s in &flat {
        match bit_depth {
            BitDepth::Bits16 => w.write_sample(to_i16(s)),
            BitDepth::Bits24 => w.write_sample(to_i24(s)),
            // No clamp: float carries overs (>0 dBFS) intact — the caller
            // controlled clipping upstream in render_to_buffers.
            BitDepth::Float32 => w.write_sample(s),
        }
        .map_err(|e| anyhow!("write wav sample: {e}"))?;
    }
    w.finalize()
        .map_err(|e| anyhow!("finalize wav {}: {e}", path.display()))?;
    Ok(())
}

/// Write stereo L/R buffers to 24-bit FLAC. FLAC stores integer PCM, so float32
/// is intentionally not offered as a FLAC output variant.
pub fn write_flac24<P: AsRef<Path>>(
    path: P,
    left: &[f32],
    right: &[f32],
    sample_rate: i32,
    channels: Channels,
) -> Result<()> {
    let path = path.as_ref();
    let out_channels = if channels == Channels::Stereo { 2 } else { 1 };
    // Flatten + quantise in ONE pass straight to i32 — skips the intermediate
    // Vec<f32> copy (worth ~25 MB on a full song; the encoder's in-memory
    // stream is still the dominant transient, an accepted flacenc constraint).
    let n = left.len().min(right.len());
    let mut samples: Vec<i32> = Vec::with_capacity(n * out_channels);
    for i in 0..n {
        match channels {
            Channels::Stereo => {
                samples.push(to_i24(left[i]));
                samples.push(to_i24(right[i]));
            }
            Channels::Mono => samples.push(to_i24(0.5 * (left[i] + right[i]))),
            Channels::Left => samples.push(to_i24(left[i])),
            Channels::Right => samples.push(to_i24(right[i])),
        }
    }
    let config = flacenc::config::Encoder::default()
        .into_verified()
        .map_err(|e| anyhow!("verify flac encoder config: {e:?}"))?;
    let source =
        flacenc::source::MemSource::from_samples(&samples, out_channels, 24, sample_rate as usize);
    let mut flac_stream = flacenc::encode_with_fixed_block_size(&config, source, config.block_size)
        .map_err(|e| anyhow!("encode flac {}: {e}", path.display()))?;
    flac_stream
        .stream_info_mut()
        .set_block_sizes(config.block_size, config.block_size)
        .map_err(|e| anyhow!("set flac stream block size metadata: {e}"))?;
    let mut sink = flacenc::bitsink::ByteSink::new();
    flac_stream
        .write(&mut sink)
        .map_err(|e| anyhow!("write flac stream {}: {e}", path.display()))?;
    std::fs::write(path, sink.into_inner())
        .map_err(|e| anyhow!("write flac {}: {e}", path.display()))?;
    Ok(())
}

fn flatten_channels(left: &[f32], right: &[f32], channels: Channels) -> Vec<f32> {
    let out_channels = if channels == Channels::Stereo { 2 } else { 1 };
    let n = left.len().min(right.len());
    let mut flat = Vec::with_capacity(n * out_channels);
    for i in 0..n {
        match channels {
            Channels::Stereo => {
                flat.push(left[i]);
                flat.push(right[i]);
            }
            Channels::Mono => flat.push(0.5 * (left[i] + right[i])),
            Channels::Left => flat.push(left[i]),
            Channels::Right => flat.push(right[i]),
        }
    }
    flat
}

/// Convert a clamped `[-1.0, 1.0]` sample to 16-bit PCM (deterministic rounding).
#[inline]
fn to_i16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16
}

/// Full-scale signed 24-bit sample (±(2^23 − 1)), mirroring `to_i16`. hound
/// packs the low 24 bits of the i32 when `bits_per_sample = 24`.
fn to_i24(s: f32) -> i32 {
    const MAX24: f32 = 8_388_607.0; // 2^23 - 1
    (s.clamp(-1.0, 1.0) * MAX24).round() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_i16_is_full_scale_and_clamps() {
        assert_eq!(to_i16(0.0), 0);
        assert_eq!(to_i16(1.0), i16::MAX);
        assert_eq!(to_i16(-1.0), -i16::MAX); // symmetric ±32767
        assert_eq!(to_i16(2.0), i16::MAX); // clamps above range
        assert_eq!(to_i16(-2.0), -i16::MAX); // clamps below range
    }

    #[test]
    fn write_wav_stereo16_emits_canonical_pcm_size() {
        let path = std::env::temp_dir().join("cosmix_musicd_wav_unit_test.wav");
        let left = vec![0.0f32, 0.5, -0.5, 1.0];
        let right = vec![0.0f32, -0.5, 0.5, -1.0];
        write_wav(
            &path,
            &left,
            &right,
            44_100,
            Channels::Stereo,
            BitDepth::Bits16,
        )
        .expect("write wav");
        // 44-byte canonical header + 4 frames × 2ch × 2 bytes = 60.
        assert_eq!(std::fs::metadata(&path).expect("stat wav").len(), 60);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_wav_mono_averages_and_is_one_channel() {
        use hound::WavReader;
        let path = std::env::temp_dir().join("cosmix_musicd_wav_mono_unit_test.wav");
        let left = vec![0.0f32, 1.0, -1.0, 0.5];
        let right = vec![0.0f32, 0.0, -1.0, -0.5];
        write_wav(
            &path,
            &left,
            &right,
            44_100,
            Channels::Mono,
            BitDepth::Bits16,
        )
        .expect("write");
        assert_eq!(std::fs::metadata(&path).expect("stat").len(), 52); // 44 + 4×1×2
        let mut r = WavReader::open(&path).expect("open");
        assert_eq!(r.spec().channels, 1);
        let s: Vec<i16> = r.samples::<i16>().map(|x| x.unwrap()).collect();
        assert_eq!(s[1], to_i16(0.5)); // (1.0+0.0)/2
        assert_eq!(s[3], 0); // (0.5-0.5)/2
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_wav_left_right_pick_the_named_channel() {
        use hound::WavReader;
        let base = std::env::temp_dir();
        let left = vec![1.0f32, -1.0];
        let right = vec![-1.0f32, 1.0];
        for (ch, want0) in [(Channels::Left, i16::MAX), (Channels::Right, -i16::MAX)] {
            let path = base.join(format!("cosmix_musicd_wav_{ch:?}.wav"));
            write_wav(&path, &left, &right, 44_100, ch, BitDepth::Bits16).expect("write");
            let mut r = WavReader::open(&path).expect("open");
            assert_eq!(r.spec().channels, 1);
            let s: Vec<i16> = r.samples::<i16>().map(|x| x.unwrap()).collect();
            assert_eq!(s[0], want0);
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn write_wav_24bit_and_float32_specs() {
        use hound::WavReader;
        let base = std::env::temp_dir();
        let left = vec![0.0f32, 0.5];
        let right = vec![0.0f32, -0.5];
        // 24-bit int: 4 frames? no — 2 frames × 1ch × 3 bytes = 6, + 44 header = 50.
        let p24 = base.join("cosmix_musicd_wav_24.wav");
        write_wav(
            &p24,
            &left,
            &right,
            48_000,
            Channels::Mono,
            BitDepth::Bits24,
        )
        .expect("24");
        let r24 = WavReader::open(&p24).expect("open24");
        assert_eq!(r24.spec().bits_per_sample, 24);
        assert_eq!(r24.spec().sample_format, hound::SampleFormat::Int);
        let _ = std::fs::remove_file(&p24);
        // float32 stereo
        let pf = base.join("cosmix_musicd_wav_f32.wav");
        write_wav(
            &pf,
            &left,
            &right,
            48_000,
            Channels::Stereo,
            BitDepth::Float32,
        )
        .expect("f32");
        let rf = WavReader::open(&pf).expect("openf");
        assert_eq!(rf.spec().bits_per_sample, 32);
        assert_eq!(rf.spec().sample_format, hound::SampleFormat::Float);
        assert_eq!(rf.spec().channels, 2);
        let _ = std::fs::remove_file(&pf);
    }

    #[test]
    fn write_flac24_emits_flac_container() {
        let path = std::env::temp_dir().join("cosmix_musicd_flac24_unit_test.flac");
        let left = vec![0.0f32, 0.5, -0.5, 1.0];
        let right = vec![0.0f32, -0.5, 0.5, -1.0];
        write_flac24(&path, &left, &right, 48_000, Channels::Stereo).expect("write flac24");
        let bytes = std::fs::read(&path).expect("read flac");
        assert!(bytes.len() > 4);
        assert_eq!(&bytes[..4], b"fLaC");
        // Regression for the a7ecf41 seekability fix: STREAMINFO (the first
        // metadata block, header at bytes 4..8) must declare a FIXED block
        // size — min (bytes 8..10, BE) == max (bytes 10..12, BE) — or libFLAC
        // warns the stream may not be seekable. A short final frame is legal
        // and must NOT lower min_blocksize.
        let min_bs = u16::from_be_bytes([bytes[8], bytes[9]]);
        let max_bs = u16::from_be_bytes([bytes[10], bytes[11]]);
        assert_eq!(min_bs, max_bs, "STREAMINFO must stay fixed-block-size");
        assert!(min_bs > 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn float32_output_preserves_overs() {
        use hound::WavReader;
        // wavf32's reason to exist: samples beyond 0 dBFS survive the writer.
        let path = std::env::temp_dir().join("cosmix_musicd_wav_overs_unit_test.wav");
        let left = vec![0.0f32, 1.5];
        let right = vec![0.0f32, -1.5];
        write_wav(
            &path,
            &left,
            &right,
            48_000,
            Channels::Stereo,
            BitDepth::Float32,
        )
        .expect("write f32");
        let mut r = WavReader::open(&path).expect("open");
        let s: Vec<f32> = r.samples::<f32>().map(|x| x.unwrap()).collect();
        assert_eq!(s[2], 1.5, "positive over must survive unclamped");
        assert_eq!(s[3], -1.5, "negative over must survive unclamped");
        let _ = std::fs::remove_file(&path);
    }
}
