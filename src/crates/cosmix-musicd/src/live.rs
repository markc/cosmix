//! Live MIDI keyboard → built-in **sine** synth → speakers.
//!
//! This is the preserved Phase-0 spine: pressing a MIDI key makes a sound while
//! the audio callback stays **lock-free and allocation-free**. It is intentionally
//! a plain sine synth — NOT rustysynth — kept as its own mode (`cosmix-musicd
//! live`) alongside the SoundFont render/play paths.
//!
//! Signal path (the load-bearing rule):
//!
//! ```text
//!   MIDI thread (midir) ──rtrb SPSC ring──▶ audio callback (cpal) ──▶ sine voices
//! ```
//!
//! The ring buffer is the ONLY thing crossing the thread boundary. The audio
//! callback never locks, allocates, or does I/O.

use anyhow::{Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use midir::{Ignore, MidiInput};
use rtrb::{Consumer, RingBuffer};
use std::io::{Read, stdin};

/// Max simultaneous notes. Fixed-size so the audio thread never allocates.
const POLYPHONY: usize = 16;

/// What the MIDI thread hands to the audio thread.
enum MidiEvent {
    NoteOn { note: u8, vel: u8 },
    NoteOff { note: u8 },
}

#[derive(Clone, Copy)]
struct Voice {
    note: u8,
    phase: f32,     // 0.0..1.0, one cycle
    phase_inc: f32, // cycles per sample = freq / sample_rate
    amp: f32,       // current (smoothed) amplitude
    target: f32,    // amplitude we're gliding toward (0.0 = releasing)
    active: bool,
}

impl Voice {
    const SILENT: Voice = Voice {
        note: 0,
        phase: 0.0,
        phase_inc: 0.0,
        amp: 0.0,
        target: 0.0,
        active: false,
    };
}

/// All audio-thread state. Lives inside the cpal callback after `run()`.
struct Synth {
    voices: [Voice; POLYPHONY],
    sample_rate: f32,
    rx: Consumer<MidiEvent>,
}

impl Synth {
    fn handle(&mut self, ev: MidiEvent) {
        match ev {
            MidiEvent::NoteOn { note, vel } => {
                let slot = self
                    .voices
                    .iter()
                    .position(|v| !v.active)
                    .unwrap_or_else(|| self.quietest());
                let freq = 440.0 * 2f32.powf((note as f32 - 69.0) / 12.0);
                self.voices[slot] = Voice {
                    note,
                    phase: 0.0,
                    phase_inc: freq / self.sample_rate,
                    amp: 0.0,
                    target: (vel as f32 / 127.0) * 0.2, // headroom for polyphony
                    active: true,
                };
            }
            MidiEvent::NoteOff { note } => {
                for v in self.voices.iter_mut() {
                    if v.active && v.note == note {
                        v.target = 0.0; // release; deactivates once amp reaches 0
                    }
                }
            }
        }
    }

    /// Steal the quietest voice when all are busy.
    fn quietest(&self) -> usize {
        let mut idx = 0;
        let mut min = f32::MAX;
        for (i, v) in self.voices.iter().enumerate() {
            if v.amp < min {
                min = v.amp;
                idx = i;
            }
        }
        idx
    }

    /// One mono sample. RT-safe: bounded work, no alloc / lock / I/O.
    fn next_sample(&mut self) -> f32 {
        // Drain pending MIDI. `pop` is non-blocking and allocation-free.
        while let Ok(ev) = self.rx.pop() {
            self.handle(ev);
        }
        const SMOOTH: f32 = 0.002; // amp glide per sample (~2ms) — declicks the gate
        let mut mix = 0.0;
        for v in self.voices.iter_mut() {
            if !v.active {
                continue;
            }
            if v.amp < v.target {
                v.amp = (v.amp + SMOOTH).min(v.target);
            } else if v.amp > v.target {
                v.amp = (v.amp - SMOOTH).max(v.target);
            }
            if v.target == 0.0 && v.amp <= 0.0 {
                v.active = false;
                continue;
            }
            mix += (v.phase * std::f32::consts::TAU).sin() * v.amp;
            v.phase += v.phase_inc;
            if v.phase >= 1.0 {
                v.phase -= 1.0;
            }
        }
        mix
    }
}

/// Build + return the output stream for sample type `T`. The closure owns the
/// `Synth` and is the hard-real-time path.
fn build_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut synth: Synth,
) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            for frame in data.chunks_mut(channels) {
                let value = T::from_sample(synth.next_sample());
                for sample in frame.iter_mut() {
                    *sample = value; // mono → all channels
                }
            }
        },
        |err| eprintln!("audio stream error: {err}"),
        None,
    )?;
    Ok(stream)
}

/// Raw MIDI bytes → our event. NoteOn with velocity 0 is the conventional NoteOff.
fn parse_midi(msg: &[u8]) -> Option<MidiEvent> {
    if msg.len() < 3 {
        return None;
    }
    match msg[0] & 0xF0 {
        0x90 if msg[2] > 0 => Some(MidiEvent::NoteOn {
            note: msg[1],
            vel: msg[2],
        }),
        0x90 | 0x80 => Some(MidiEvent::NoteOff { note: msg[1] }),
        _ => None,
    }
}

/// Run the live keyboard → sine synth loop until the user presses Enter.
pub fn run() -> Result<()> {
    // --- audio device / config ---
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow!("no default audio output device"))?;
    let supported = device.default_output_config()?;
    println!(
        "audio out: {} @ {} Hz, {} ch, {:?}",
        device
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or_else(|_| "?".into()),
        supported.sample_rate(),
        supported.channels(),
        supported.sample_format(),
    );
    let sample_format = supported.sample_format();
    let sample_rate = supported.sample_rate() as f32;
    let stream_config: cpal::StreamConfig = supported.into();

    // --- ring buffer: MIDI thread → audio thread ---
    let (mut tx, rx) = RingBuffer::<MidiEvent>::new(1024);
    let synth = Synth {
        voices: [Voice::SILENT; POLYPHONY],
        sample_rate,
        rx,
    };

    // --- MIDI input ---
    let mut midi_in = MidiInput::new("cosmix-musicd live")?;
    midi_in.ignore(Ignore::None);
    let ports = midi_in.ports();
    // Keep the connection alive for the program's lifetime (drop = disconnect).
    let _conn = match ports.first() {
        Some(port) => {
            println!(
                "midi in:  {}",
                midi_in.port_name(port).unwrap_or_else(|_| "?".into())
            );
            Some(
                midi_in
                    .connect(
                        port,
                        "cosmix-musicd-live",
                        move |_ts, msg, _| {
                            if let Some(ev) = parse_midi(msg) {
                                // Drop on full rather than block — RT-safe back-pressure.
                                let _ = tx.push(ev);
                            }
                        },
                        (),
                    )
                    .map_err(|e| anyhow!("MIDI connect failed: {e}"))?,
            )
        }
        None => {
            println!("midi in:  (none found — audio is up, but there's nothing to play yet)");
            None
        }
    };

    // --- build + start the audio stream ---
    let stream = match sample_format {
        cpal::SampleFormat::F32 => build_stream::<f32>(&device, &stream_config, synth)?,
        cpal::SampleFormat::I16 => build_stream::<i16>(&device, &stream_config, synth)?,
        cpal::SampleFormat::U16 => build_stream::<u16>(&device, &stream_config, synth)?,
        other => return Err(anyhow!("unsupported sample format: {other:?}")),
    };
    stream.play()?;

    println!("\nLive sine synth is up — play your MIDI keyboard. Press Enter to quit.");
    let _ = stdin().read(&mut [0u8]);
    Ok(())
}
