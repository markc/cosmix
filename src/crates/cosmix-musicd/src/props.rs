//! SPEC 07 §2 property surface for cosmix-musicd (L1+, plus L2/L3 via `world`).
//!
//! Paths: `config.*` (state dir, soundfont, sample rate, polyphony),
//! `lifecycle.*` (uptime, health, soundfont load state, device availability),
//! `playback.*` (current file, position, duration, counters). Built fresh per
//! `props.get` from live `AppState` + the playback engine's atomic status — no
//! async lock is held inside the `PropTree` impl.

use cosmix_props::{PropDescribe, PropPath, PropTree, PropType, PropValue, tree::build_snapshot};
use serde_json::Value as Json;

/// Snapshot of musicd's observable state.
pub struct MusicdPropsSnapshot {
    pub state_dir: String,
    pub soundfont_name: String,
    pub soundfont_url: String,
    pub sample_rate: u64,
    pub max_polyphony: u64,
    pub started_at: String,
    pub uptime_s: u64,
    pub soundfont_loaded: bool,
    pub soundfont_error: String,
    pub device_available: bool,
    pub playing: bool,
    pub current_midi: String,
    pub position_s: f64,
    pub duration_s: f64,
    pub renders_total: u64,
    pub plays_total: u64,
}

impl MusicdPropsSnapshot {
    pub fn snapshot_value(&self) -> PropValue {
        let health = if self.soundfont_loaded {
            "ok"
        } else {
            "degraded"
        };
        build_snapshot([
            (
                PropPath::new("config.state_dir").unwrap(),
                PropValue::from(self.state_dir.clone()),
            ),
            (
                PropPath::new("config.soundfont_name").unwrap(),
                PropValue::from(self.soundfont_name.clone()),
            ),
            (
                PropPath::new("config.soundfont_url").unwrap(),
                PropValue::from(self.soundfont_url.clone()),
            ),
            (
                PropPath::new("config.sample_rate").unwrap(),
                PropValue::from(self.sample_rate),
            ),
            (
                PropPath::new("config.max_polyphony").unwrap(),
                PropValue::from(self.max_polyphony),
            ),
            (
                PropPath::new("lifecycle.started_at").unwrap(),
                PropValue::from(self.started_at.clone()),
            ),
            (
                PropPath::new("lifecycle.uptime_s").unwrap(),
                PropValue::from(self.uptime_s),
            ),
            (
                PropPath::new("lifecycle.health").unwrap(),
                PropValue::from(health),
            ),
            (
                PropPath::new("lifecycle.props_level").unwrap(),
                PropValue::from("L3"),
            ),
            (
                PropPath::new("lifecycle.soundfont_loaded").unwrap(),
                PropValue::Bool(self.soundfont_loaded),
            ),
            (
                PropPath::new("lifecycle.soundfont_error").unwrap(),
                PropValue::from(self.soundfont_error.clone()),
            ),
            (
                PropPath::new("lifecycle.device_available").unwrap(),
                PropValue::Bool(self.device_available),
            ),
            (
                PropPath::new("playback.playing").unwrap(),
                PropValue::Bool(self.playing),
            ),
            (
                PropPath::new("playback.current_midi").unwrap(),
                PropValue::from(self.current_midi.clone()),
            ),
            (
                PropPath::new("playback.position_s").unwrap(),
                PropValue::from(self.position_s),
            ),
            (
                PropPath::new("playback.duration_s").unwrap(),
                PropValue::from(self.duration_s),
            ),
            (
                PropPath::new("playback.renders_total").unwrap(),
                PropValue::from(self.renders_total),
            ),
            (
                PropPath::new("playback.plays_total").unwrap(),
                PropValue::from(self.plays_total),
            ),
        ])
    }
}

impl PropTree for MusicdPropsSnapshot {
    fn snapshot(&self) -> PropValue {
        self.snapshot_value()
    }

    fn list(&self) -> Vec<PropPath> {
        all_paths()
    }

    fn describe(&self, path: &PropPath) -> Option<PropDescribe> {
        describe_path(path)
    }
}

fn all_paths() -> Vec<PropPath> {
    [
        "config.state_dir",
        "config.soundfont_name",
        "config.soundfont_url",
        "config.sample_rate",
        "config.max_polyphony",
        "lifecycle.started_at",
        "lifecycle.uptime_s",
        "lifecycle.health",
        "lifecycle.props_level",
        "lifecycle.soundfont_loaded",
        "lifecycle.soundfont_error",
        "lifecycle.device_available",
        "playback.playing",
        "playback.current_midi",
        "playback.position_s",
        "playback.duration_s",
        "playback.renders_total",
        "playback.plays_total",
    ]
    .into_iter()
    .map(|s| PropPath::new(s).unwrap())
    .collect()
}

fn describe_path(path: &PropPath) -> Option<PropDescribe> {
    use PropType::*;
    let d = |ty, desc: &str| Some(PropDescribe::leaf(path.clone(), ty, desc));
    match path.as_str() {
        "config.state_dir" => d(String, "Directory holding the SoundFont and rendered WAVs."),
        "config.soundfont_name" => d(String, "Active SoundFont filename within the state dir."),
        "config.soundfont_url" => d(String, "URL the SoundFont is fetched from on first run."),
        "config.sample_rate" => d(Number, "Default render/playback sample rate (Hz)."),
        "config.max_polyphony" => d(Number, "Maximum simultaneous voices (8..=256)."),
        "lifecycle.started_at" => Some(
            PropDescribe::leaf(path.clone(), String, "RFC 3339 timestamp of process start.")
                .with_format("rfc3339"),
        ),
        "lifecycle.uptime_s" => Some(
            PropDescribe::leaf(path.clone(), Number, "Seconds since process start.")
                .with_transient(true),
        ),
        "lifecycle.health" => d(
            String,
            "Coarse health (ok | degraded — degraded until the SoundFont loads).",
        ),
        "lifecycle.props_level" => d(String, "SPEC 07 conformance level (L0 | L1 | L2 | L3)."),
        "lifecycle.soundfont_loaded" => d(
            Bool,
            "Whether the SoundFont is fetched and parsed in memory.",
        ),
        "lifecycle.soundfont_error" => d(
            String,
            "Last SoundFont fetch/load error (empty when healthy).",
        ),
        "lifecycle.device_available" => d(
            Bool,
            "Whether a default audio output device exists (false = headless; render still works).",
        ),
        "playback.playing" => d(
            Bool,
            "Whether a MIDI file is currently playing to the speakers.",
        ),
        "playback.current_midi" => d(
            String,
            "Path of the currently-playing MIDI file (empty when idle).",
        ),
        "playback.position_s" => Some(
            PropDescribe::leaf(path.clone(), Number, "Playback position in seconds.")
                .with_transient(true),
        ),
        "playback.duration_s" => d(Number, "Duration of the current/last MIDI in seconds."),
        "playback.renders_total" => d(Number, "Count of MIDI→WAV renders since process start."),
        "playback.plays_total" => d(Number, "Count of playbacks started since process start."),
        _ => None,
    }
}

/// Parse the optional `args` JSON header into a `serde_json::Value`.
pub fn parse_args(s: Option<&str>) -> Option<Json> {
    s.and_then(|raw| serde_json::from_str(raw).ok())
}
