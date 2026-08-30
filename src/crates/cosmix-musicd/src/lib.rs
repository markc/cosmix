//! cosmix-musicd — MIDI → audio for the cosmix substrate.
//!
//! Three layers, gated by the Cargo feature graph (see `Cargo.toml`):
//!
//! - **render + synth** — the PURE core. rustysynth renders a Standard MIDI File
//!   through an SF2 SoundFont into stereo `f32`, written to a WAV by `hound`.
//!   No audio hardware and no C FFI; the only Cosmix touch is a pure-Rust
//!   build-time provenance stamp (`build.rs`), so the compiled artifact links no
//!   C and no Cosmix runtime code. This is what
//!   `cargo test --lib --no-default-features` exercises.
//! - **play / live** — the `playback` feature: `cpal` audio output. `play` streams
//!   a rendered MIDI file to the speakers; `live` is the preserved Phase-0 path —
//!   a MIDI keyboard driving a built-in 16-voice **sine** synth.
//! - **daemon / props / world / fetch** — the `cosmix` feature: the Bus citizen
//!   (`serve`), SPEC-12 property surface, and first-run SoundFont fetch.
//!
//! The audio-output hop (`cpal` → ALSA/PipeWire) is the one unavoidable C
//! dependency on Linux; it is quarantined behind `playback` so the render core
//! stays pure and headless-renderable everywhere.

/// The narrow 32-channel bake-off mixer engine + headless simulator (D4/D7).
/// Pure (depends only on `cosmix-mixer-schema` + std), so it builds under
/// `--no-default-features` and the `mixer-sim` subcommand runs on headless nodes.
/// MIDI 2.0 / UMP core: packet model, typed messages, MIDI 1.0 ↔ 2.0
/// translators. Pure (no FFI, no runtime deps) — builds unconditionally,
/// including under `--no-default-features`.
pub mod midi2;
pub mod mixer;
pub mod render;
pub(crate) mod riff;
/// Realtime scheduling (`SCHED_FIFO`, `mlockall`) + the RT→async eventfd wake.
/// Pure libc, no runtime deps — builds under `--no-default-features`.
/// Linux-only by design: cos is a Linux substrate, and non-Linux call sites
/// should fail loudly rather than compile a fallback with weaker semantics.
#[cfg(target_os = "linux")]
pub mod rt_sched;
pub mod sf2gain;
pub mod sf2merge;
pub mod sf2split;
#[cfg(feature = "sf3")]
pub mod sf3;
#[cfg(feature = "sfz")]
pub mod sfz;
pub mod smf;
#[allow(clippy::items_after_test_module)]
pub mod stems;
pub mod synth;

#[cfg(feature = "playback")]
pub mod live;

#[cfg(feature = "playback")]
pub mod play;

#[cfg(feature = "cosmix")]
pub mod daemon;
#[cfg(feature = "cosmix")]
pub mod fetch;
/// The real 32-channel mixer Bus daemon (`cosmix-musicd mixer-serve`): runs the
/// [`mixer::MixerEngine`] on a dedicated RT thread and exposes a writeable
/// `mixer.v1` control surface (`props.set`) + a 60 Hz meter publisher +
/// `dsp.applied` events. The DSP half of the disp-skia bake-off vertical slice.
#[cfg(feature = "cosmix")]
pub mod mixer_daemon;
#[cfg(feature = "mixer-host")]
pub mod mixer_host;
#[cfg(feature = "cosmix")]
pub mod props;
#[cfg(feature = "cosmix")]
pub mod world;

/// Crate version, surfaced via `--version` and Bus provenance.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
