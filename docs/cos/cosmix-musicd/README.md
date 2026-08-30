# cosmix-musicd

`cosmix-musicd` renders Standard MIDI Files through SoundFonts, plays rendered or live MIDI audio, manipulates SoundFont banks, and supplies a 32-channel mixer engine and Bus citizen daemon. It belongs to the `cos` daemon and substrate layer in the `bus <- mix <- cos` dependency chain: the core audio code is in `cos`, while the optional daemon uses Bus libraries and the optional mixer host uses Mix strict-data support.

## Synopsis

The package builds both the `cosmix_musicd` library and the `cosmix-musicd` binary.

The binary provides headless rendering and SoundFont tools without daemon support. Default features also provide local audio playback, live MIDI input, SF3 decoding, and SFZ conversion. The `cosmix` feature adds the Bus daemon modes.

See:

- [Command-line interface](cli.md)
- [Bus service and configuration](bus.md)

## Processing layers

The crate separates its processing paths by Cargo feature:

1. The render core loads MIDI and SoundFont data, synthesises stereo floating-point buffers, and writes WAV or FLAC output.
2. The `playback` layer sends file playback or live MIDI synthesis to the host audio device.
3. The `mixer-host` layer hosts the fixed 32-channel mixer on a real-time thread and adds session loading, revisioned controls, and telemetry.
4. The `cosmix` layer registers the `musicd` Bus citizen and exposes render, playback, property, and mixer services.

Offline rendering does not require an audio device.

## Library modules

| Module | Availability | Purpose |
|---|---|---|
| `render` | Always | Offline MIDI-to-audio rendering and WAV or FLAC writing. |
| `synth` | Always | SoundFont, MIDI, and synthesiser settings helpers. |
| `smf` | Always | Minimal Standard MIDI File parsing, scanning, track assembly, and conductor extraction. |
| `stems` | Always | Per-track rendering and in-memory orchestra-bank assembly. |
| `sf2split` | Always | Splits an SF2 bank into one SF2 image per preset. |
| `sf2merge` | Always | Merges selected single-preset SF2 images into one bank. |
| `sf2gain` | Always | Measures rendered preset loudness and applies down-only PCM gain. |
| `sf3` | `sf3` | Decodes Ogg-compressed SF3 sample data into an in-memory SF2 image. |
| `sfz` | `sfz` | Converts a supported SFZ subset and WAV or FLAC samples into SF2. |
| `mixer` | Always | Fixed 32-channel mixer, source profiles, metering, and headless simulator. |
| `play` | `playback` | SoundFont MIDI playback and the daemon playback engine. |
| `live` | `playback` | Live MIDI keyboard input through a built-in sine synthesiser. |
| `mixer_host` | `mixer-host` | Real-time mixer hosting, revisioned writes, sessions, song schedules, and export. |
| `daemon`, `fetch`, `props`, `world` | `cosmix` | Main Bus citizen, first-run SoundFont fetch, properties, and world publication. |
| `mixer_daemon` | `cosmix` | Bus mixer control and telemetry service. |

## Render API

`render::RenderOptions` selects sample rate, gain, maximum polyphony, effects, channel layout, and output format. Its defaults use 44.1 kHz, unity gain, and enabled reverb and chorus.

`render::Channels` selects:

- `Stereo`
- `Mono`, using the average of left and right
- `Left`
- `Right`

`render::RenderFormat` selects 16-bit WAV, 24-bit WAV, 32-bit floating-point WAV, or 24-bit FLAC.

The principal render functions are:

- `render_to_buffers`, which returns separate left and right buffers plus a `RenderReport`
- `render_midi_to_file`, which loads inputs and writes the selected format
- `write_render`, which writes existing buffers in the selected container
- `write_wav` and `write_flac24`, which expose the container-specific writers

`RenderReport` reports duration, frame count, sample rate, peak level, and whether integer output was clipped.

The renderer is deterministic for identical inputs and options. Floating-point WAV preserves samples above unity; integer formats clamp samples to their representable range.

## MIDI and stems

`smf::parse` reads the SMF header and raw track chunks. `smf::scan` identifies track names, sounding notes, bank and program state, and percussion use. `smf::scan_song` resolves effective bank and program identities across all tracks in time order.

`smf::conductor_only` retains tempo, time-signature, and end-of-track events while preserving their absolute timing.

`stems::render_per_track` renders each note-bearing track as a separate audio file. Each stem receives the conductor timing data without inheriting track 0 note events.

`stems::assemble_orchestra` scans the song, resolves required presets from a split library, fills gaps from a fallback bank, and merges the selected presets in memory. It declines identities that SF2 cannot represent exactly.

## SoundFont tools

`sf2split::list_presets` lists preset identities and names.

`sf2split::split_presets` copies the preset, instrument, generator, modulator, and referenced sample records into standalone SF2 images. It can filter by bank or program and retag a single extracted preset.

`sf2merge::merge_images` combines extracted presets and rejects duplicate bank and program identities.

`sf2gain::measure_dbfs` renders probe notes and reports audible RMS level. `sf2gain::scale_pcm` applies a gain no greater than unity to 16-bit or 24-bit sample data.

`sf3::maybe_decode_sf3` detects Ogg-compressed sample regions and rebuilds an uncompressed SF2 image. Ordinary SF2 input passes through unchanged.

`sfz::sfz_to_sf2` converts the supported SFZ region model into an uncompressed SF2 image and reports ignored opcodes and skipped regions.

## Mixer API

The mixer operates at 48 kHz with 128-frame blocks. It has 32 mono input strips, a stereo master, equal-power pan, trim, fader, mute, solo, and RMS, peak, peak-hold, and clip metering.

`mixer::Controls`, `ChannelControl`, and `MasterControl` hold the canonical control state.

`mixer::MixerEngine` accepts a fixed `SourceProfile` at construction. Its public control surface includes `set_controls`, `seek`, `process_block`, `process_block_audio`, clip-latch reset, and source-bank swaps.

The source profiles are:

- deterministic benchmark multitone
- preloaded stem session
- per-track MIDI synthesis

`mixer::StemBank` stores immutable source audio plus non-destructive timeline regions. `MidiSynthBank` stores prebuilt per-track synthesiser voices and frame-keyed note schedules.

`mixer::run_simulator` drives the same block processor without an audio device or Bus broker and can write raw meter frames.

The mixer meter wire types and leaf schema come from the `cosmix-mixer-schema` dependency.

## Playback API

`play::play_blocking` plays one MIDI file through a SoundFont and blocks until completion.

`play::PlaybackEngine` owns the non-sendable audio stream on a dedicated operating-system thread. Callers send play and stop commands and read lock-free `PlaybackStatus`.

`live::run` connects a MIDI input to a fixed 16-voice sine synthesiser. MIDI events cross into the audio callback through a single-producer, single-consumer ring; the callback performs no locking, allocation, or I/O.

## Features

| Feature | Default | Effect |
|---|---:|---|
| `playback` | Yes | Adds host audio output, live MIDI input, and the real-time event ring. |
| `sf3` | Yes | Adds pure-Rust Ogg Vorbis decoding for SF3 SoundFonts. |
| `sfz` | Yes | Adds FLAC decoding and the SFZ-to-SF2 converter. |
| `mixer-host` | No | Adds playback plus mixer property, session, song, hashing, serialisation, and tracing support. |
| `cosmix` | No | Adds the full Bus citizen, mixer host, daemon framework, configuration, logging, properties, async runtime, fetch, hashing, and provenance support. |

The `default` feature set is `playback`, `sf3`, and `sfz`.

With no default features, the package retains the render, MIDI, SoundFont, stems, and headless mixer core. It omits local playback, live MIDI, SF3 decoding, SFZ conversion, and Bus service code.

## Runtime behaviour

The main daemon starts even when its configured SoundFont cannot be fetched or loaded. Render and play requests report the unavailable bank until loading succeeds.

Playback requires a default output device. Rendering and the mixer simulator remain available on a headless system.

The mixer daemon uses the audio clock when a compatible device is available and a monotonic software-paced fallback otherwise. Its property snapshot reports whether real audio is active.

## Errors

Library operations return `anyhow::Result`.

Render calls fail on unreadable inputs, invalid MIDI or SoundFont data, unsupported options, synthesis errors, or output failures.

SoundFont extraction validates RIFF and hydra record geometry and fails rather than emitting an inconsistent bank.

Stem-session loading validates schema, sample rate, channel uniqueness, content hashes, mono WAV layout, and declared length before starting the real-time path.

