//! Toolkit-free data for the mixer + roll bake-off (ADR 2026-09-17 test C).
//!
//! Every arm (Bevy/CTK, iced wgpu, iced tiny-skia) links this crate unchanged
//! and draws what it returns, so a difference between arms is drawing cost and
//! never a difference in the data. Everything here is a pure function of the
//! seed and a tick counter that advances at [`TICK_HZ`]:
//!
//! - [`mixer`]: strip names, fader/pan/mute/solo state, stereo peak meters and
//!   the scripted fader drag;
//! - [`song`]: the dense song loaded through `cosmix-song` as a flat,
//!   start-sorted note list with a viewport query;
//! - [`roll`]: the piano-roll viewport, its zoom/scroll rules and the scripted
//!   scroll-and-zoom path;
//! - [`layout`]: the shared geometry both arms lay out against.

pub mod layout;
pub mod mixer;
pub mod roll;
pub mod song;

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

pub use mixer::{DragSample, MeterFrame, MeterLane, MixerFeed, StripState};
pub use roll::{GridLine, RollViewport, roll_script};
pub use song::{BenchNote, BenchSong, LoadError};

/// Strips a default run shows (plus the master strip).
pub const DEFAULT_STRIPS: usize = 64;
/// The seed a default run uses.
pub const DEFAULT_SEED: u64 = 0x5eed_1234;
/// Feed rate: meters and scripts advance this many ticks per second.
pub const TICK_HZ: u32 = 30;

/// The tick a run is at after `elapsed` wall-clock time.
pub fn tick_at(elapsed: Duration) -> u64 {
    (elapsed.as_secs_f64() * f64::from(TICK_HZ)).floor() as u64
}

/// The wall-clock interval between two ticks.
pub fn tick_interval() -> Duration {
    Duration::from_secs_f64(1.0 / f64::from(TICK_HZ))
}

/// What a run animates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mode {
    /// Nothing changes after the first frame; meters are silent.
    Idle,
    /// Every meter animates at [`TICK_HZ`].
    Meters,
    /// Meters animate and one fader follows [`MixerFeed::drag_sample`].
    Drag,
    /// Meters are silent; the roll follows [`roll_script`].
    Roll,
}

impl Mode {
    pub const ALL: [Mode; 4] = [Mode::Idle, Mode::Meters, Mode::Drag, Mode::Roll];

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Idle => "idle",
            Mode::Meters => "meters",
            Mode::Drag => "drag",
            Mode::Roll => "roll",
        }
    }

    /// Whether meters move in this mode.
    pub fn animates_meters(self) -> bool {
        matches!(self, Mode::Meters | Mode::Drag)
    }

    /// Whether anything changes from tick to tick in this mode, i.e. whether
    /// an arm has to wake at [`TICK_HZ`] at all.
    pub fn is_animated(self) -> bool {
        !matches!(self, Mode::Idle)
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Mode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Mode::ALL
            .into_iter()
            .find(|mode| mode.as_str() == s)
            .ok_or_else(|| format!("unknown mode {s:?} (idle|meters|drag|roll)"))
    }
}

/// Lowest fader value, dB (the mixer schema's silence floor).
pub const FADER_MIN_DB: f32 = -120.0;
/// Highest fader value, dB.
pub const FADER_MAX_DB: f32 = 6.0;
/// Fader taper as (travel position, dB) breakpoints. Mirrors CTK's
/// `default_fader_mapping`, so a given dB value sits at the same height in
/// every arm; the Bevy arm checks the two agree in a test.
pub const FADER_TAPER: [(f32, f32); 6] = [
    (0.0, FADER_MIN_DB),
    (0.10, -60.0),
    (0.25, -30.0),
    (0.50, -12.0),
    (0.75, 0.0),
    (1.0, FADER_MAX_DB),
];

/// Fader travel position (0 bottom, 1 top) of `db`.
pub fn fader_position(db: f32) -> f32 {
    let db = db.clamp(FADER_MIN_DB, FADER_MAX_DB);
    for pair in FADER_TAPER.windows(2) {
        let ((p0, d0), (p1, d1)) = (pair[0], pair[1]);
        if db <= d1 {
            return p0 + (db - d0) / (d1 - d0) * (p1 - p0);
        }
    }
    1.0
}

/// dB value at fader travel position `position`.
pub fn fader_db(position: f32) -> f32 {
    let position = position.clamp(0.0, 1.0);
    for pair in FADER_TAPER.windows(2) {
        let ((p0, d0), (p1, d1)) = (pair[0], pair[1]);
        if position <= p1 {
            return d0 + (position - p0) / (p1 - p0) * (d1 - d0);
        }
    }
    FADER_MAX_DB
}

/// Bottom of the meter scale, dB. Anything at or below reads empty.
pub const METER_FLOOR_DB: f32 = -60.0;
/// Top of the meter scale, dB.
pub const METER_CEIL_DB: f32 = 6.0;

/// Meter height (0..=1) of `db`; the same scale as CTK's `db_to_meter_position`.
pub fn meter_position(db: f32) -> f32 {
    if db <= METER_FLOOR_DB {
        0.0
    } else {
        ((db - METER_FLOOR_DB) / (METER_CEIL_DB - METER_FLOOR_DB)).clamp(0.0, 1.0)
    }
}

/// SplitMix64 finaliser: the only randomness source, so every value is a
/// pure function of its inputs.
pub(crate) fn mix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

pub(crate) fn hash(parts: &[u64]) -> u64 {
    parts.iter().fold(0, |acc, part| mix64(acc ^ part))
}

/// Uniform in `[0, 1)` from a hash.
pub(crate) fn unit(h: u64) -> f32 {
    (h >> 40) as f32 / (1u64 << 24) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_round_trips_through_its_name() {
        for mode in Mode::ALL {
            assert_eq!(mode.as_str().parse::<Mode>(), Ok(mode));
        }
        assert!("fast".parse::<Mode>().is_err());
        assert!(!Mode::Idle.is_animated());
        assert!(Mode::Roll.is_animated() && !Mode::Roll.animates_meters());
    }

    #[test]
    fn ticks_advance_at_thirty_hertz() {
        assert_eq!(tick_at(Duration::ZERO), 0);
        assert_eq!(tick_at(Duration::from_millis(33)), 0);
        assert_eq!(tick_at(Duration::from_millis(34)), 1);
        assert_eq!(tick_at(Duration::from_secs(2)), 60);
    }

    #[test]
    fn fader_taper_hits_its_breakpoints_and_round_trips() {
        for (position, db) in FADER_TAPER {
            assert!((fader_position(db) - position).abs() < 1e-5);
            assert!((fader_db(position) - db).abs() < 1e-3);
        }
        let mut last = -1.0;
        for step in 0..=252 {
            let db = FADER_MIN_DB + step as f32 * 0.5;
            let position = fader_position(db);
            assert!(position > last, "taper must be strictly increasing");
            assert!((fader_db(position) - db).abs() < 1e-3);
            last = position;
        }
        assert_eq!(fader_position(-500.0), 0.0);
        assert_eq!(fader_position(50.0), 1.0);
    }

    #[test]
    fn meter_scale_clamps_both_ends() {
        assert_eq!(meter_position(-90.0), 0.0);
        assert_eq!(meter_position(METER_FLOOR_DB), 0.0);
        assert_eq!(meter_position(METER_CEIL_DB), 1.0);
        assert_eq!(meter_position(20.0), 1.0);
        assert!((meter_position(0.0) - 60.0 / 66.0).abs() < 1e-6);
    }

    #[test]
    fn unit_stays_in_range() {
        for i in 0..10_000 {
            let value = unit(hash(&[i, 7]));
            assert!((0.0..1.0).contains(&value));
        }
    }
}
