//! Allocation-free compositor-side hot-corner detection.

use std::time::Duration;

const MOTION_EPSILON_PX: f64 = 0.001;
const MIN_STATIONARY_CONFIRM_MS: u64 = 1;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CornerConfig {
    pub(crate) enabled: bool,
    pub(crate) deadzone_px: f64,
    pub(crate) dwell_ms: u64,
    pub(crate) velocity_max_px_s: f64,
}

impl Default for CornerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            deadzone_px: 12.0,
            dwell_ms: 200,
            velocity_max_px_s: 1_500.0,
        }
    }
}

impl CornerConfig {
    pub(crate) fn valid(self) -> bool {
        self.deadzone_px.is_finite()
            && (1.0..=256.0).contains(&self.deadzone_px)
            && self.dwell_ms <= 5_000
            && self.velocity_max_px_s.is_finite()
            && (1.0..=20_000.0).contains(&self.velocity_max_px_s)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl Corner {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::TopLeft => "tl",
            Self::TopRight => "tr",
            Self::BottomLeft => "bl",
            Self::BottomRight => "br",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CornerEvent {
    Entered { corner: Corner, dwell_ms: u64 },
    Left { corner: Corner, dwell_ms: u64 },
}

pub(crate) type CornerEvents = [Option<CornerEvent>; 2];

#[derive(Clone, Copy, Debug)]
struct Candidate {
    corner: Corner,
    entered_at_ms: u64,
    velocity_eligible_since_ms: Option<u64>,
    last_speed_px_s: Option<f64>,
}

#[derive(Clone, Copy, Debug)]
struct Engagement {
    corner: Corner,
    dwell_ms: u64,
}

#[derive(Clone, Copy, Debug)]
struct Sample {
    at_ms: u64,
    position: (f64, f64),
}

#[derive(Clone, Debug)]
pub(crate) struct CornerDetector {
    config: CornerConfig,
    size: (f64, f64),
    candidate: Option<Candidate>,
    engaged: Option<Engagement>,
    last_sample: Option<Sample>,
}

impl CornerDetector {
    pub(crate) fn new(config: CornerConfig, size: (f64, f64)) -> Self {
        Self {
            config,
            size,
            candidate: None,
            engaged: None,
            last_sample: None,
        }
    }

    pub(crate) fn next_deadline_ms(&self) -> Option<u64> {
        let candidate = self.candidate?;
        candidate.velocity_eligible_since_ms.map_or_else(
            || {
                self.last_sample.map(|sample| {
                    sample.at_ms.saturating_add(if self.config.dwell_ms == 0 {
                        MIN_STATIONARY_CONFIRM_MS
                    } else {
                        self.config.dwell_ms
                    })
                })
            },
            |since| Some(since.saturating_add(self.config.dwell_ms)),
        )
    }

    #[cfg(test)]
    pub(crate) fn candidate_position(&self) -> Option<(f64, f64)> {
        self.candidate
            .and_then(|_| self.last_sample.map(|sample| sample.position))
    }

    pub(crate) fn sample(
        &mut self,
        at_ms: u64,
        position: (f64, f64),
        attempted_motion: (f64, f64),
    ) -> CornerEvents {
        if !self.config.enabled
            || !self.config.valid()
            || !valid_size(self.size)
            || !point_inside(position, self.size)
            || self
                .last_sample
                .is_some_and(|previous| at_ms < previous.at_ms)
        {
            return self.reset();
        }

        let sample = Sample { at_ms, position };
        let speed = self.instantaneous_speed(sample);
        let corner = corner_at(position, self.size, self.config.deadzone_px);
        let mut events = [None, None];
        let mut next = 0;

        if let Some(engaged) = self.engaged {
            if corner == Some(engaged.corner) {
                self.last_sample = Some(sample);
                return events;
            }
            self.engaged = None;
            events[next] = Some(CornerEvent::Left {
                corner: engaged.corner,
                dwell_ms: engaged.dwell_ms,
            });
            next += 1;
        }

        let Some(corner) = corner else {
            self.candidate = None;
            self.last_sample = Some(sample);
            return events;
        };

        let continuing = self
            .candidate
            .is_some_and(|candidate| candidate.corner == corner);
        let mut became_velocity_eligible = false;
        if !continuing {
            self.candidate = Some(Candidate {
                corner,
                entered_at_ms: at_ms,
                velocity_eligible_since_ms: speed
                    .is_some_and(|value| value <= self.config.velocity_max_px_s)
                    .then_some(at_ms),
                last_speed_px_s: speed,
            });
        } else if let Some(speed) = speed
            && let Some(candidate) = &mut self.candidate
        {
            candidate.last_speed_px_s = Some(speed);
            if speed <= self.config.velocity_max_px_s {
                if candidate.velocity_eligible_since_ms.is_none() {
                    became_velocity_eligible = true;
                    candidate.velocity_eligible_since_ms =
                        Some(self.last_sample.map_or(at_ms, |previous| previous.at_ms));
                }
            } else {
                candidate.velocity_eligible_since_ms = None;
            }
        }

        let candidate = self.candidate.expect("corner candidate exists");
        let dwell_ms = at_ms.saturating_sub(candidate.entered_at_ms);
        let velocity_dwell_complete = candidate
            .velocity_eligible_since_ms
            .is_some_and(|since| at_ms.saturating_sub(since) >= self.config.dwell_ms);
        let outward_push = continuing
            && !became_velocity_eligible
            && candidate
                .last_speed_px_s
                .is_some_and(|value| value <= self.config.velocity_max_px_s)
            && pushes_outward(corner, attempted_motion);
        if velocity_dwell_complete || outward_push {
            self.candidate = None;
            self.engaged = Some(Engagement { corner, dwell_ms });
            events[next] = Some(CornerEvent::Entered { corner, dwell_ms });
        }
        self.last_sample = Some(sample);
        events
    }

    pub(crate) fn reset(&mut self) -> CornerEvents {
        self.candidate = None;
        self.last_sample = None;
        [
            self.engaged.take().map(|engaged| CornerEvent::Left {
                corner: engaged.corner,
                dwell_ms: engaged.dwell_ms,
            }),
            None,
        ]
    }

    pub(crate) fn reconfigure(&mut self, config: CornerConfig, size: (f64, f64)) -> CornerEvents {
        let changed = self.config != config || self.size != size;
        self.config = config;
        self.size = size;
        if changed || !config.enabled || !config.valid() || !valid_size(size) {
            self.reset()
        } else {
            [None, None]
        }
    }

    fn instantaneous_speed(&self, sample: Sample) -> Option<f64> {
        let previous = self.last_sample?;
        let elapsed_ms = sample.at_ms.checked_sub(previous.at_ms)?;
        if elapsed_ms == 0 {
            return None;
        }
        let movement = (sample.position.0 - previous.position.0)
            .hypot(sample.position.1 - previous.position.1);
        Some(if movement > MOTION_EPSILON_PX {
            movement / Duration::from_millis(elapsed_ms).as_secs_f64()
        } else {
            0.0
        })
    }
}

fn valid_size(size: (f64, f64)) -> bool {
    size.0.is_finite() && size.1.is_finite() && size.0 > 0.0 && size.1 > 0.0
}

fn point_inside(point: (f64, f64), size: (f64, f64)) -> bool {
    point.0.is_finite()
        && point.1.is_finite()
        && point.0 >= 0.0
        && point.1 >= 0.0
        && point.0 < size.0
        && point.1 < size.1
}

fn corner_at(point: (f64, f64), size: (f64, f64), deadzone: f64) -> Option<Corner> {
    let left = point.0 <= deadzone;
    let right = size.0 - point.0 <= deadzone;
    let top = point.1 <= deadzone;
    let bottom = size.1 - point.1 <= deadzone;
    let candidates = [
        (left && top, Corner::TopLeft, point.0.hypot(point.1)),
        (
            left && bottom,
            Corner::BottomLeft,
            point.0.hypot(size.1 - point.1),
        ),
        (
            right && bottom,
            Corner::BottomRight,
            (size.0 - point.0).hypot(size.1 - point.1),
        ),
        (
            right && top,
            Corner::TopRight,
            (size.0 - point.0).hypot(point.1),
        ),
    ];
    candidates
        .into_iter()
        .filter(|(inside, _, _)| *inside)
        .min_by(|left, right| left.2.total_cmp(&right.2))
        .map(|(_, corner, _)| corner)
}

fn pushes_outward(corner: Corner, motion: (f64, f64)) -> bool {
    match corner {
        Corner::TopLeft => motion.0 < 0.0 || motion.1 < 0.0,
        Corner::TopRight => motion.0 > 0.0 || motion.1 < 0.0,
        Corner::BottomLeft => motion.0 < 0.0 || motion.1 > 0.0,
        Corner::BottomRight => motion.0 > 0.0 || motion.1 > 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: (f64, f64) = (1_000.0, 800.0);

    fn detector() -> CornerDetector {
        CornerDetector::new(CornerConfig::default(), SIZE)
    }

    fn entered(events: CornerEvents) -> Option<(Corner, u64)> {
        events.into_iter().flatten().find_map(|event| match event {
            CornerEvent::Entered { corner, dwell_ms } => Some((corner, dwell_ms)),
            CornerEvent::Left { .. } => None,
        })
    }

    #[test]
    fn names_all_four_corners() {
        assert_eq!(Corner::TopLeft.name(), "tl");
        assert_eq!(Corner::TopRight.name(), "tr");
        assert_eq!(Corner::BottomLeft.name(), "bl");
        assert_eq!(Corner::BottomRight.name(), "br");
    }

    #[test]
    fn slow_stationary_dwell_enters_once_then_leaves() {
        let mut detector = detector();
        assert_eq!(detector.sample(0, (5.0, 5.0), (0.0, 0.0)), [None, None]);
        assert_eq!(detector.next_deadline_ms(), Some(200));
        assert_eq!(
            entered(detector.sample(200, (5.0, 5.0), (0.0, 0.0))),
            Some((Corner::TopLeft, 200))
        );
        assert_eq!(detector.sample(250, (5.0, 5.0), (-4.0, 0.0)), [None, None]);
        assert_eq!(
            detector.sample(300, (50.0, 50.0), (0.0, 0.0))[0],
            Some(CornerEvent::Left {
                corner: Corner::TopLeft,
                dwell_ms: 200
            })
        );
    }

    #[test]
    fn each_corner_enters() {
        for (position, corner) in [
            ((1.0, 1.0), Corner::TopLeft),
            ((999.0, 1.0), Corner::TopRight),
            ((1.0, 799.0), Corner::BottomLeft),
            ((999.0, 799.0), Corner::BottomRight),
        ] {
            let mut detector = detector();
            detector.sample(0, position, (0.0, 0.0));
            assert_eq!(
                entered(detector.sample(200, position, (0.0, 0.0))),
                Some((corner, 200))
            );
        }
    }

    #[test]
    fn fast_transit_waits_for_stationary_dwell() {
        let mut detector = detector();
        detector.sample(0, (100.0, 100.0), (0.0, 0.0));
        assert_eq!(entered(detector.sample(10, (5.0, 5.0), (0.0, 0.0))), None);
        assert_eq!(
            entered(detector.sample(210, (5.0, 5.0), (0.0, 0.0))),
            Some((Corner::TopLeft, 200))
        );
    }

    #[test]
    fn outward_push_enters_early_but_inward_push_does_not() {
        let mut outward = detector();
        outward.sample(0, (5.0, 5.0), (0.0, 0.0));
        outward.sample(20, (5.0, 5.0), (0.0, 0.0));
        assert_eq!(
            entered(outward.sample(40, (5.0, 5.0), (-3.0, 0.0))),
            Some((Corner::TopLeft, 40))
        );

        let mut inward = detector();
        inward.sample(0, (5.0, 5.0), (0.0, 0.0));
        assert_eq!(entered(inward.sample(20, (5.0, 5.0), (3.0, 3.0))), None);
    }

    #[test]
    fn reset_and_geometry_or_config_change_emit_left() {
        let mut detector = detector();
        detector.sample(0, (5.0, 5.0), (0.0, 0.0));
        detector.sample(200, (5.0, 5.0), (0.0, 0.0));
        assert!(matches!(
            detector.reset()[0],
            Some(CornerEvent::Left { .. })
        ));

        detector.sample(300, (5.0, 5.0), (0.0, 0.0));
        detector.sample(500, (5.0, 5.0), (0.0, 0.0));
        assert!(matches!(
            detector.reconfigure(CornerConfig::default(), (900.0, 800.0))[0],
            Some(CornerEvent::Left { .. })
        ));
    }

    #[test]
    fn disabled_invalid_and_non_monotonic_samples_suppress_engagement() {
        let mut config = CornerConfig {
            enabled: false,
            ..CornerConfig::default()
        };
        let mut disabled = CornerDetector::new(config, SIZE);
        assert_eq!(disabled.sample(0, (5.0, 5.0), (-1.0, 0.0)), [None, None]);
        config.enabled = true;
        config.deadzone_px = f64::NAN;
        disabled.reconfigure(config, SIZE);
        assert_eq!(disabled.sample(1, (5.0, 5.0), (-1.0, 0.0)), [None, None]);

        let mut detector = detector();
        detector.sample(10, (5.0, 5.0), (0.0, 0.0));
        assert_eq!(detector.sample(9, (5.0, 5.0), (-1.0, 0.0)), [None, None]);
    }

    #[test]
    fn zero_dwell_still_requires_a_follow_up_sample() {
        let config = CornerConfig {
            dwell_ms: 0,
            ..CornerConfig::default()
        };
        let mut detector = CornerDetector::new(config, SIZE);
        assert_eq!(entered(detector.sample(0, (5.0, 5.0), (0.0, 0.0))), None);
        assert_eq!(detector.next_deadline_ms(), Some(1));
        assert_eq!(
            entered(detector.sample(1, (5.0, 5.0), (0.0, 0.0))),
            Some((Corner::TopLeft, 1))
        );
    }
}
