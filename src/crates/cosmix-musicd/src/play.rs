//! MIDI file → SoundFont → speakers, via cpal.
//!
//! Two entry points share one render callback:
//! - [`play_blocking`] — the CLI `play` subcommand: stream a file to the end, block.
//! - [`PlaybackEngine`] — the daemon's start/stop engine. Because `cpal::Stream`
//!   is `!Send`, the stream is quarantined on a **dedicated OS thread** that owns
//!   it and never sends it anywhere; the daemon's async tasks talk to that thread
//!   over a channel and read status through lock-free atomics.
//!
//! The audio callback is allocation-free: scratch buffers are pre-sized and
//! rustysynth's `render` writes in place.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use rustysynth::{MidiFile, MidiFileSequencer, SoundFont, Synthesizer};

use crate::synth;

/// Max frames rendered per inner chunk — bounds the pre-allocated scratch.
const MAX_BLOCK: usize = 4096;

/// Lock-free playback status, readable from any thread (props / status verb).
#[derive(Default)]
pub struct PlaybackStatus {
    pub playing: AtomicBool,
    pub frames: AtomicU64,
    pub sample_rate: AtomicU32,
    pub duration_ms: AtomicU64,
}

impl PlaybackStatus {
    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }
    pub fn position_s(&self) -> f64 {
        let sr = self.sample_rate.load(Ordering::Relaxed);
        if sr == 0 {
            return 0.0;
        }
        let pos = self.frames.load(Ordering::Relaxed) as f64 / sr as f64;
        pos.min(self.duration_s())
    }
    pub fn duration_s(&self) -> f64 {
        self.duration_ms.load(Ordering::Relaxed) as f64 / 1000.0
    }
}

/// Is there a default audio output device? (Headless nodes have none.)
pub fn device_available() -> bool {
    cpal::default_host().default_output_device().is_some()
}

fn default_output() -> Result<(cpal::Device, cpal::SampleFormat, cpal::StreamConfig)> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow!("no default audio output device"))?;
    let supported = device.default_output_config()?;
    let fmt = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    Ok((device, fmt, config))
}

fn make_sequencer(
    sf: &Arc<SoundFont>,
    midi: &Arc<MidiFile>,
    sample_rate: i32,
) -> Result<MidiFileSequencer> {
    let settings = synth::make_settings(sample_rate, synth::DEFAULT_MAX_POLYPHONY, true);
    let synthesizer =
        Synthesizer::new(sf, &settings).map_err(|e| anyhow!("create synthesizer: {e}"))?;
    let mut seq = MidiFileSequencer::new(synthesizer);
    seq.play(midi, false);
    Ok(seq)
}

/// Build the hard-real-time output stream for sample type `T`.
fn build_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut sequencer: MidiFileSequencer,
    gain: f32,
    status: Arc<PlaybackStatus>,
    ended: Arc<AtomicBool>,
) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = (config.channels as usize).max(1);
    let mut scratch_l = vec![0.0f32; MAX_BLOCK];
    let mut scratch_r = vec![0.0f32; MAX_BLOCK];
    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            let frames = data.len() / channels;
            let mut done = 0;
            while done < frames {
                let n = (frames - done).min(MAX_BLOCK);
                sequencer.render(&mut scratch_l[..n], &mut scratch_r[..n]);
                for i in 0..n {
                    let l = (scratch_l[i] * gain).clamp(-1.0, 1.0);
                    let r = (scratch_r[i] * gain).clamp(-1.0, 1.0);
                    let base = (done + i) * channels;
                    let frame = &mut data[base..base + channels];
                    if channels == 1 {
                        frame[0] = T::from_sample(0.5 * (l + r));
                    } else {
                        frame[0] = T::from_sample(l);
                        frame[1] = T::from_sample(r);
                        for s in frame[2..].iter_mut() {
                            *s = T::from_sample(0.0f32);
                        }
                    }
                }
                done += n;
            }
            status.frames.fetch_add(frames as u64, Ordering::Relaxed);
            if sequencer.end_of_sequence() {
                ended.store(true, Ordering::Relaxed);
            }
        },
        |err| eprintln!("audio stream error: {err}"),
        None,
    )?;
    Ok(stream)
}

#[allow(clippy::too_many_arguments)]
fn build_render_stream(
    sample_format: cpal::SampleFormat,
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    sequencer: MidiFileSequencer,
    gain: f32,
    status: Arc<PlaybackStatus>,
    ended: Arc<AtomicBool>,
) -> Result<cpal::Stream> {
    match sample_format {
        cpal::SampleFormat::F32 => {
            build_stream::<f32>(device, config, sequencer, gain, status, ended)
        }
        cpal::SampleFormat::I16 => {
            build_stream::<i16>(device, config, sequencer, gain, status, ended)
        }
        cpal::SampleFormat::U16 => {
            build_stream::<u16>(device, config, sequencer, gain, status, ended)
        }
        other => Err(anyhow!("unsupported sample format: {other:?}")),
    }
}

/// CLI path: load + play a MIDI file to completion, then return.
pub fn play_blocking<P1: AsRef<Path>, P2: AsRef<Path>>(
    midi_path: P1,
    soundfont_path: P2,
    gain: f32,
) -> Result<()> {
    let sf = synth::load_soundfont(soundfont_path)?;
    let midi = synth::load_midi(&midi_path)?;
    let (device, fmt, config) = default_output()?;
    let sample_rate = config.sample_rate as i32;
    let duration = midi.get_length();

    let status = Arc::new(PlaybackStatus::default());
    status
        .sample_rate
        .store(sample_rate as u32, Ordering::Relaxed);
    status
        .duration_ms
        .store((duration * 1000.0) as u64, Ordering::Relaxed);
    let ended = Arc::new(AtomicBool::new(false));

    let sequencer = make_sequencer(&sf, &midi, sample_rate)?;
    let stream = build_render_stream(
        fmt,
        &device,
        &config,
        sequencer,
        gain,
        status.clone(),
        ended.clone(),
    )?;
    stream.play()?;
    println!(
        "playing {} @ {} Hz ({:.1}s) — Ctrl-C to stop",
        midi_path.as_ref().display(),
        sample_rate,
        duration
    );
    while !ended.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(250)); // let the reverb tail flush
    Ok(())
}

// ---------------------------------------------------------------------------
// Daemon engine — dedicated thread owning the (!Send) cpal::Stream.
// ---------------------------------------------------------------------------

/// What a successful `play` reports back to the caller.
#[derive(Clone, Copy, Debug)]
pub struct PlayInfo {
    pub sample_rate: i32,
    pub duration_s: f64,
}

enum EngineCmd {
    Play {
        sf: Arc<SoundFont>,
        midi: Arc<MidiFile>,
        gain: f32,
        reply: mpsc::Sender<Result<PlayInfo, String>>,
    },
    Stop {
        reply: mpsc::Sender<bool>, // was_playing
    },
    Shutdown,
}

/// The non-`Send` stream + its end flag, owned only by the engine thread.
struct ActiveStream {
    _stream: cpal::Stream,
    ended: Arc<AtomicBool>,
}

/// A handle to the playback thread. Cloneable status; commands via channel.
pub struct PlaybackEngine {
    cmd_tx: mpsc::Sender<EngineCmd>,
    status: Arc<PlaybackStatus>,
    handle: Option<JoinHandle<()>>,
}

impl PlaybackEngine {
    /// Spawn the dedicated playback thread.
    pub fn spawn() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCmd>();
        let status = Arc::new(PlaybackStatus::default());
        let status_t = status.clone();
        let handle = std::thread::Builder::new()
            .name("musicd-playback".into())
            .spawn(move || engine_loop(cmd_rx, status_t))
            .expect("spawn playback thread");
        Self {
            cmd_tx,
            status,
            handle: Some(handle),
        }
    }

    pub fn status(&self) -> &Arc<PlaybackStatus> {
        &self.status
    }

    /// Start playing a (pre-loaded) MIDI file. Stops any current playback first.
    pub fn play(&self, sf: Arc<SoundFont>, midi: Arc<MidiFile>, gain: f32) -> Result<PlayInfo> {
        let (reply, rx) = mpsc::channel();
        self.cmd_tx
            .send(EngineCmd::Play {
                sf,
                midi,
                gain,
                reply,
            })
            .map_err(|_| anyhow!("playback engine stopped"))?;
        rx.recv()
            .map_err(|_| anyhow!("playback engine dropped reply"))?
            .map_err(|e| anyhow!(e))
    }

    /// Stop playback. Returns whether something was playing.
    pub fn stop(&self) -> bool {
        let (reply, rx) = mpsc::channel();
        if self.cmd_tx.send(EngineCmd::Stop { reply }).is_err() {
            return false;
        }
        rx.recv().unwrap_or(false)
    }
}

impl Drop for PlaybackEngine {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(EngineCmd::Shutdown);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn engine_loop(cmd_rx: mpsc::Receiver<EngineCmd>, status: Arc<PlaybackStatus>) {
    let mut current: Option<ActiveStream> = None;
    loop {
        match cmd_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(EngineCmd::Play {
                sf,
                midi,
                gain,
                reply,
            }) => {
                current = None; // drop = stop any prior stream
                status.playing.store(false, Ordering::Relaxed);
                match start_stream(&sf, &midi, gain, &status) {
                    Ok((active, info)) => {
                        current = Some(active);
                        let _ = reply.send(Ok(info));
                    }
                    Err(e) => {
                        status.playing.store(false, Ordering::Relaxed);
                        let _ = reply.send(Err(e.to_string()));
                    }
                }
            }
            Ok(EngineCmd::Stop { reply }) => {
                let was = current.is_some();
                current = None;
                status.playing.store(false, Ordering::Relaxed);
                let _ = reply.send(was);
            }
            Ok(EngineCmd::Shutdown) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Auto-stop when the sequence has finished.
                if let Some(active) = &current
                    && active.ended.load(Ordering::Relaxed)
                {
                    current = None;
                    status.playing.store(false, Ordering::Relaxed);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn start_stream(
    sf: &Arc<SoundFont>,
    midi: &Arc<MidiFile>,
    gain: f32,
    status: &Arc<PlaybackStatus>,
) -> Result<(ActiveStream, PlayInfo)> {
    let (device, fmt, config) = default_output()?;
    let sample_rate = config.sample_rate as i32;
    let duration = midi.get_length();
    let sequencer = make_sequencer(sf, midi, sample_rate)?;

    status
        .sample_rate
        .store(sample_rate as u32, Ordering::Relaxed);
    status
        .duration_ms
        .store((duration * 1000.0) as u64, Ordering::Relaxed);
    status.frames.store(0, Ordering::Relaxed);
    status.playing.store(true, Ordering::Relaxed);

    let ended = Arc::new(AtomicBool::new(false));
    let stream = build_render_stream(
        fmt,
        &device,
        &config,
        sequencer,
        gain,
        status.clone(),
        ended.clone(),
    )?;
    stream.play()?;
    Ok((
        ActiveStream {
            _stream: stream,
            ended,
        },
        PlayInfo {
            sample_rate,
            duration_s: duration,
        },
    ))
}
