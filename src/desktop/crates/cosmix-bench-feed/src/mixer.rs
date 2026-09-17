//! Synthetic mixer state: per-strip controls, stereo peak meters and the
//! scripted fader drag, each a pure function of `(seed, slot, tick)`.
//!
//! Slots `0..strips` are channel strips and slot `strips` is the master.

use crate::{METER_CEIL_DB, METER_FLOOR_DB, Mode, fader_db, hash, meter_position, unit};

/// Ticks the fast peak marker holds its maximum.
pub const PEAK_TICKS: u64 = 6;
/// Ticks the slow hold marker (and the clip latch) keeps its maximum.
pub const HOLD_TICKS: u64 = 45;
/// A lane is marked clipped while its hold window contains a level above this.
pub const CLIP_DB: f32 = 0.0;
/// Initial fader values closer than this to 0 dB are generated as 0 dB.
/// CTK snaps a constructed fader within (range / 200) = 0.63 dB of its 0 dB
/// detent; generating on the far side of that band keeps every arm showing the
/// same value.
pub const FADER_DETENT_BAND_DB: f32 = 0.7;
/// One full down-and-up cycle of the scripted drag.
pub const DRAG_PERIOD_TICKS: u64 = 90;
/// Fader travel range the scripted drag sweeps.
pub const DRAG_LOW: f32 = 0.30;
pub const DRAG_HIGH: f32 = 0.95;

const NAMES: [&str; 16] = [
    "Kick", "Snare", "Hat", "Tom", "Bass", "Gtr", "Keys", "Pad", "Lead", "Vox", "Str", "Brass",
    "FX", "Perc", "Synth", "Choir",
];

/// One strip's control state.
#[derive(Clone, Debug, PartialEq)]
pub struct StripState {
    /// 1-based channel number; `None` on the master.
    pub number: Option<usize>,
    pub name: String,
    /// Fader value, dB, a multiple of 0.1.
    pub fader_db: f32,
    /// Pan, -1 (left) to 1 (right), a multiple of 1/64.
    pub pan: f32,
    pub mute: bool,
    /// Always false on the master, which has no solo.
    pub solo: bool,
}

/// One meter lane, normalised to the meter scale (0..=1).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MeterLane {
    pub level: f32,
    /// Fast peak marker (max over [`PEAK_TICKS`]).
    pub peak: f32,
    /// Slow hold marker (max over [`HOLD_TICKS`]).
    pub hold: f32,
    pub clipped: bool,
}

/// A stereo meter reading.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MeterFrame {
    pub lanes: [MeterLane; 2],
}

/// Where the scripted drag holds its fader at one tick.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DragSample {
    /// The dragged channel strip's slot.
    pub strip: usize,
    /// Fader travel position, 0 bottom to 1 top. A pointer-driven drag aims
    /// here; this is also the height the fader thumb is drawn at.
    pub position: f32,
    /// The value at `position`, rounded to the fader's 0.1 dB step.
    pub db: f32,
}

/// The seeded mixer feed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MixerFeed {
    seed: u64,
    strips: usize,
    mode: Mode,
}

impl MixerFeed {
    pub fn new(seed: u64, strips: usize, mode: Mode) -> Self {
        Self { seed, strips, mode }
    }

    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// Channel strips, not counting the master.
    pub fn strips(&self) -> usize {
        self.strips
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The master's slot.
    pub fn master_slot(&self) -> usize {
        self.strips
    }

    /// Channel strips plus the master.
    pub fn slot_count(&self) -> usize {
        self.strips + 1
    }

    pub fn is_master(&self, slot: usize) -> bool {
        slot == self.strips
    }

    /// Initial control state of `slot`.
    pub fn strip(&self, slot: usize) -> StripState {
        assert!(slot <= self.strips, "slot {slot} out of range");
        if self.is_master(slot) {
            return StripState {
                number: None,
                name: "Master".to_owned(),
                fader_db: 0.0,
                pan: 0.0,
                mute: false,
                solo: false,
            };
        }
        let h = hash(&[self.seed, slot as u64, 0x0073_7472_6970]);
        let name = format!("{} {}", NAMES[(h % NAMES.len() as u64) as usize], slot + 1);
        // -24..=+3 dB in 0.1 dB steps.
        let fader_db = ((h >> 8) % 271) as f32 / 10.0 - 24.0;
        let fader_db = if fader_db.abs() < FADER_DETENT_BAND_DB {
            0.0
        } else {
            fader_db
        };
        // -1..=1 in 1/64 steps (every such value is on CTK's 1/512 grid).
        let pan = ((h >> 20) % 129) as f32 / 64.0 - 1.0;
        StripState {
            number: Some(slot + 1),
            name,
            fader_db,
            pan,
            mute: (h >> 32).is_multiple_of(9),
            solo: (h >> 40).is_multiple_of(13),
        }
    }

    /// The meter reading of `slot` at `tick`. Silent in modes whose meters do
    /// not animate.
    pub fn meter(&self, slot: usize, tick: u64) -> MeterFrame {
        if !self.mode.animates_meters() {
            return MeterFrame::default();
        }
        let mut frame = MeterFrame::default();
        for (lane, out) in frame.lanes.iter_mut().enumerate() {
            let mut peak_db = f32::NEG_INFINITY;
            let mut hold_db = f32::NEG_INFINITY;
            let first = tick.saturating_sub(HOLD_TICKS - 1);
            for t in first..=tick {
                let db = level_db(self.seed, slot, lane, t);
                hold_db = hold_db.max(db);
                if tick - t < PEAK_TICKS {
                    peak_db = peak_db.max(db);
                }
            }
            let level_now = level_db(self.seed, slot, lane, tick);
            *out = MeterLane {
                level: meter_position(level_now),
                peak: meter_position(peak_db),
                hold: meter_position(hold_db),
                clipped: hold_db > CLIP_DB,
            };
        }
        frame
    }

    /// Every slot's meter reading at `tick`, master last.
    pub fn meters_into(&self, tick: u64, out: &mut Vec<MeterFrame>) {
        out.clear();
        out.extend((0..self.slot_count()).map(|slot| self.meter(slot, tick)));
    }

    /// The channel strip the drag mode moves.
    pub fn drag_strip(&self) -> Option<usize> {
        (self.mode == Mode::Drag && self.strips > 0).then_some(self.strips / 2)
    }

    /// The scripted drag at `tick`: a triangle sweep between [`DRAG_HIGH`] and
    /// [`DRAG_LOW`] travel, starting at the top.
    pub fn drag_sample(&self, tick: u64) -> Option<DragSample> {
        let strip = self.drag_strip()?;
        let phase = (tick % DRAG_PERIOD_TICKS) as f32 / DRAG_PERIOD_TICKS as f32;
        let triangle = if phase < 0.5 {
            phase * 2.0
        } else {
            2.0 - phase * 2.0
        };
        let position = DRAG_HIGH - triangle * (DRAG_HIGH - DRAG_LOW);
        let db = (fader_db(position) * 10.0).round() / 10.0;
        Some(DragSample {
            strip,
            position,
            db,
        })
    }

    /// The fader value of `slot` at `tick`.
    pub fn fader_db(&self, slot: usize, tick: u64) -> f32 {
        match self.drag_sample(tick) {
            Some(drag) if drag.strip == slot => drag.db,
            _ => self.strip(slot).fader_db,
        }
    }

    /// Whether the mixer's visible state differs between two ticks.
    pub fn mixer_changes_between(&self, from: u64, to: u64) -> bool {
        from != to && self.mode.animates_meters()
    }
}

/// Instantaneous level of one lane, dB, clamped to the meter scale.
fn level_db(seed: u64, slot: usize, lane: usize, tick: u64) -> f32 {
    const PERIODS: [u64; 5] = [10, 12, 15, 20, 24];
    let strip = hash(&[seed, slot as u64, 0x006d_6574_6572]);
    let base = -30.0 + 24.0 * unit(strip);
    let period = PERIODS[((strip >> 8) % PERIODS.len() as u64) as usize];
    let offset = (strip >> 16) % period;
    // A decaying hit every `period` ticks.
    let pulse = -18.0 * ((tick + offset) % period) as f32 / period as f32;
    let noise = (unit(hash(&[seed, slot as u64, lane as u64, tick])) - 0.5) * 6.0;
    let lane_trim = if lane == 1 {
        -1.5 * unit(strip >> 24)
    } else {
        0.0
    };
    // Occasional hot half-seconds, so clip latches appear.
    let burst = if hash(&[seed, slot as u64, tick / 15, 0x0068_6f74]).is_multiple_of(53) {
        9.0
    } else {
        0.0
    };
    (base + pulse + noise + lane_trim + burst).clamp(METER_FLOOR_DB - 1.0, METER_CEIL_DB)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DEFAULT_SEED, DEFAULT_STRIPS, fader_position};

    fn feed(mode: Mode) -> MixerFeed {
        MixerFeed::new(DEFAULT_SEED, DEFAULT_STRIPS, mode)
    }

    #[test]
    fn same_seed_same_state() {
        let a = feed(Mode::Meters);
        let b = feed(Mode::Meters);
        for slot in 0..a.slot_count() {
            assert_eq!(a.strip(slot), b.strip(slot));
            for tick in [0, 1, 29, 30, 1000] {
                assert_eq!(a.meter(slot, tick), b.meter(slot, tick));
            }
        }
    }

    #[test]
    fn different_seed_different_state() {
        let a = feed(Mode::Meters);
        let b = MixerFeed::new(DEFAULT_SEED + 1, DEFAULT_STRIPS, Mode::Meters);
        let strips_differ = (0..DEFAULT_STRIPS).any(|slot| a.strip(slot) != b.strip(slot));
        let meters_differ = (0..DEFAULT_STRIPS).any(|slot| a.meter(slot, 10) != b.meter(slot, 10));
        assert!(strips_differ && meters_differ);
    }

    #[test]
    fn strip_state_is_on_the_control_grids() {
        let feed = feed(Mode::Idle);
        let mut names = std::collections::HashSet::new();
        for slot in 0..DEFAULT_STRIPS {
            let strip = feed.strip(slot);
            assert_eq!(strip.number, Some(slot + 1));
            assert!(names.insert(strip.name.clone()), "names are unique");
            assert!(strip.name.len() <= 9, "{} fits the compact name box", strip.name);
            assert!((-24.0..=3.0).contains(&strip.fader_db));
            assert!(strip.fader_db == 0.0 || strip.fader_db.abs() >= FADER_DETENT_BAND_DB);
            let tenths = strip.fader_db * 10.0;
            assert!((tenths - tenths.round()).abs() < 1e-3);
            assert!((-1.0..=1.0).contains(&strip.pan));
            assert_eq!((strip.pan * 64.0).fract(), 0.0);
        }
        let master = feed.strip(feed.master_slot());
        assert_eq!(master.number, None);
        assert_eq!(master.name, "Master");
        assert!(!master.solo);
        // A 64-strip board has a mix of engaged buttons, not all or none.
        let mutes = (0..DEFAULT_STRIPS).filter(|s| feed.strip(*s).mute).count();
        let solos = (0..DEFAULT_STRIPS).filter(|s| feed.strip(*s).solo).count();
        assert!(mutes > 0 && mutes < DEFAULT_STRIPS);
        assert!(solos > 0 && solos < DEFAULT_STRIPS);
    }

    #[test]
    fn idle_and_roll_meters_are_silent_and_static() {
        for mode in [Mode::Idle, Mode::Roll] {
            let feed = feed(mode);
            for slot in 0..feed.slot_count() {
                assert_eq!(feed.meter(slot, 0), MeterFrame::default());
                assert_eq!(feed.meter(slot, 500), MeterFrame::default());
            }
            assert!(!feed.mixer_changes_between(0, 1));
            assert_eq!(feed.drag_sample(10), None);
        }
    }

    #[test]
    fn animated_meters_move_and_stay_ordered() {
        let feed = feed(Mode::Meters);
        let mut frames = Vec::new();
        let mut previous = Vec::new();
        let mut moved = 0;
        let mut clipped = 0;
        for tick in 0..300 {
            feed.meters_into(tick, &mut frames);
            assert_eq!(frames.len(), DEFAULT_STRIPS + 1);
            for frame in &frames {
                for lane in frame.lanes {
                    assert!((0.0..=1.0).contains(&lane.level));
                    assert!(lane.peak >= lane.level);
                    assert!(lane.hold >= lane.peak);
                    clipped += usize::from(lane.clipped);
                }
            }
            if !previous.is_empty() {
                moved += frames.iter().zip(&previous).filter(|(a, b)| a != b).count();
            }
            std::mem::swap(&mut frames, &mut previous);
        }
        // Nearly every meter changes on nearly every tick.
        assert!(moved > 299 * (DEFAULT_STRIPS + 1) * 9 / 10, "moved {moved}");
        assert!(clipped > 0, "some lanes clip");
        assert!(feed.mixer_changes_between(4, 5));
        assert!(!feed.mixer_changes_between(5, 5));
    }

    #[test]
    fn scripted_drag_sweeps_one_fader() {
        let feed = feed(Mode::Drag);
        let strip = feed.drag_strip().unwrap();
        assert_eq!(strip, DEFAULT_STRIPS / 2);
        let start = feed.drag_sample(0).unwrap();
        assert_eq!(start.position, DRAG_HIGH);
        let bottom = feed.drag_sample(DRAG_PERIOD_TICKS / 2).unwrap();
        assert!((bottom.position - DRAG_LOW).abs() < 1e-6);
        assert_eq!(feed.drag_sample(DRAG_PERIOD_TICKS), Some(start));
        for tick in 0..DRAG_PERIOD_TICKS {
            let sample = feed.drag_sample(tick).unwrap();
            assert_eq!(sample.strip, strip);
            assert!((DRAG_LOW..=DRAG_HIGH).contains(&sample.position));
            assert!((fader_position(sample.db) - sample.position).abs() < 0.01);
            assert_eq!(feed.fader_db(strip, tick), sample.db);
        }
        // Other faders hold their initial value.
        assert_eq!(feed.fader_db(0, 17), feed.strip(0).fader_db);
        // Consecutive ticks move the fader.
        assert_ne!(feed.drag_sample(3), feed.drag_sample(4));
    }

    #[test]
    fn drag_needs_a_strip() {
        let feed = MixerFeed::new(1, 0, Mode::Drag);
        assert_eq!(feed.drag_sample(0), None);
        assert_eq!(feed.slot_count(), 1);
        assert!(feed.is_master(0));
    }
}
